//! EGFX frame sender for encoded graphics updates.
//!
//! Bridges encoded frame output to the IronRDP EGFX pipeline by sending frames,
//! draining DVC output, and forwarding resulting server events.

use std::sync::Arc;

// IronRDP types - used internally only
use ironrdp_dvc::{DvcMessage, encode_dvc_messages};
use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::rdp::channels::graphics::{damage::DamageRegion, egfx::channel::HandlerState};

pub(super) type SendResult<T> = anyhow::Result<T>;

/// Thin adapter from encoded frames to IronRDP's graphics pipeline.
///
/// IronRDP owns capability negotiation, surfaces, frame IDs, acknowledgement
/// windows, DVC framing, and protocol encoding. This adapter validates the
/// encoder boundary and forwards IronRDP's queued output to the server loop.
pub struct EgfxChannelSender {
    /// Handle to the GraphicsPipelineServer for sending frames
    /// Also used to query channel_id via server.channel_id()
    gfx_server: GfxServerHandle,

    /// Handler state for checking readiness (codec support, surface availability)
    handler_state: Arc<tokio::sync::RwLock<Option<HandlerState>>>,

    /// Channel for sending server events (unbounded for backpressure-free EGFX)
    event_tx: mpsc::UnboundedSender<ServerEvent>,
}

/// Send a ResetGraphics monitor layout for one primary monitor before surface creation.
pub(crate) fn resize_with_primary_monitor(
    server: &mut ironrdp_egfx::server::GraphicsPipelineServer,
    width: u16,
    height: u16,
) {
    use ironrdp_pdu::gcc::{Monitor, MonitorFlags};

    server.resize_with_monitors(
        width,
        height,
        vec![Monitor {
            left: 0,
            top: 0,
            right: width as i32 - 1,
            bottom: height as i32 - 1,
            flags: MonitorFlags::PRIMARY,
        }],
    );
}

impl EgfxChannelSender {
    pub fn new(
        gfx_server: GfxServerHandle,
        handler_state: Arc<tokio::sync::RwLock<Option<HandlerState>>>,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    ) -> Self {
        Self {
            gfx_server,
            handler_state,
            event_tx,
        }
    }

    /// Inspect the acknowledgement window before encoding or advancing damage
    /// history. Submission rechecks the window under the server lock.
    pub fn is_backpressured(&self) -> anyhow::Result<bool> {
        let server = self
            .gfx_server
            .lock()
            .map_err(|_| anyhow::anyhow!("EGFX server lock poisoned"))?;
        Ok(server.should_backpressure())
    }

    /// Flush pending EGFX server messages, such as ResetGraphics,
    /// CreateSurface, and MapSurfaceToOutput, through the DVC adapter.
    pub async fn flush_pending_server_messages(&self) -> SendResult<usize> {
        let (channel_id, dvc_messages) = {
            let mut server = self
                .gfx_server
                .lock()
                .map_err(|_| anyhow::anyhow!("EGFX server lock poisoned"))?;
            let channel_id = server
                .channel_id()
                .ok_or_else(|| anyhow::anyhow!("EGFX channel not ready"))?;
            let dvc_messages = server.drain_output();
            (channel_id, dvc_messages)
        };

        if dvc_messages.is_empty() {
            return Ok(0);
        }

        let message_count = dvc_messages.len();
        let svc_count = self.send_dvc_messages(channel_id, dvc_messages)?;

        trace!(
            "EGFX: encoded {} pending DVC messages into {} SVC messages for channel {}",
            message_count, svc_count, channel_id
        );

        Ok(message_count)
    }

    /// Whether capability negotiation has completed.
    pub async fn is_egfx_ready(&self) -> bool {
        self.handler_state
            .read()
            .await
            .as_ref()
            .is_some_and(|state| state.is_ready)
    }

    async fn ready_state(&self) -> SendResult<HandlerState> {
        let state = self
            .handler_state
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("EGFX channel not ready"))?;

        if state.is_ready {
            Ok(state)
        } else {
            Err(anyhow::anyhow!("EGFX channel not ready"))
        }
    }

    async fn avc420_state(&self) -> SendResult<HandlerState> {
        let state = self.ready_state().await?;
        if state.is_avc420_enabled {
            Ok(state)
        } else {
            Err(anyhow::anyhow!("AVC420 not supported by client"))
        }
    }

    async fn avc444_state(&self) -> SendResult<HandlerState> {
        let state = self.ready_state().await?;
        if state.is_avc444_enabled {
            Ok(state)
        } else {
            Err(anyhow::anyhow!("AVC444 not supported by client"))
        }
    }

    fn validate_frame(
        data: &[u8],
        encoded_width: u16,
        encoded_height: u16,
        display_width: u16,
        display_height: u16,
    ) -> SendResult<()> {
        if data.is_empty()
            || encoded_width == 0
            || encoded_height == 0
            || display_width == 0
            || display_height == 0
            || display_width > encoded_width
            || display_height > encoded_height
            || !encoded_width.is_multiple_of(16)
            || !encoded_height.is_multiple_of(16)
        {
            anyhow::bail!(
                "invalid frame geometry/data: encoded={encoded_width}x{encoded_height}, display={display_width}x{display_height}, bytes={}",
                data.len()
            );
        }
        Ok(())
    }

    fn surface_id(state: &HandlerState) -> SendResult<u16> {
        state
            .primary_surface_id
            .ok_or_else(|| anyhow::anyhow!("no primary EGFX surface"))
    }

    fn send_dvc_messages(
        &self,
        channel_id: u32,
        dvc_messages: Vec<DvcMessage>,
    ) -> SendResult<usize> {
        if dvc_messages.is_empty() {
            return Ok(0);
        }

        let svc_messages =
            encode_dvc_messages(channel_id, dvc_messages, ChannelFlags::SHOW_PROTOCOL)
                .map_err(|error| anyhow::anyhow!("DVC encoding failed: {error}"))?;
        let sent_count = svc_messages.len();

        self.event_tx
            .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                messages: svc_messages,
            }))
            .map_err(|_| anyhow::anyhow!("server event channel closed"))?;

        Ok(sent_count)
    }

    /// Send a Planar-encoded frame through EGFX.
    ///
    /// Planar codec (0xa) is supported by the MS Android RD Client.
    /// Used when AVC is disabled and RemoteFX is not supported by the client.
    ///
    /// The `planar_encoder` should be created once and reused across frames.
    pub async fn send_planar_frame(
        &self,
        planar_encoder: &mut ironrdp_graphics::rdp6::BitmapStreamEncoder,
        bitmap: &ironrdp_server::BitmapUpdate,
        display_width: u16,
        display_height: u16,
        timestamp_ms: u32,
    ) -> SendResult<u32> {
        let state = self.ready_state().await?;
        let surface_id = Self::surface_id(&state)?;

        // Encode to RDP6_BITMAP_STREAM (Planar codec, codec_id=0xa).
        // PipeWire delivers BGRx32 (B=byte0, G=byte1, R=byte2, X=byte3),
        // so BgrAChannels must be used for correct RGB channel mapping.
        //
        // BitmapStreamEncoder stores width/height and uses them to split the pixel
        // iterator into per-scanline RLE segments. If the stored dimensions differ
        // from the actual frame, the delta encoding uses the wrong row boundary and
        // produces striped corruption. Rebuild from actual frame dimensions on every
        // call — encoder construction is O(1) with no allocation.
        let w = bitmap.width.get() as usize;
        let h = bitmap.height.get() as usize;
        *planar_encoder = ironrdp_graphics::rdp6::BitmapStreamEncoder::new(w, h);
        let mut planar_buf = vec![0u8; w * h * 4 + 1024];
        let encoded_len = planar_encoder
            .encode_bitmap::<ironrdp_graphics::rdp6::BgrAChannels>(
                &bitmap.data,
                &mut planar_buf,
                true,
            )
            .map_err(|error| anyhow::anyhow!("Planar encode failed: {error}"))?;
        let planar_data = &planar_buf[..encoded_len];

        let _ = (
            surface_id,
            planar_data,
            display_width,
            display_height,
            timestamp_ms,
        );

        anyhow::bail!("EGFX Planar send is unavailable in the pinned IronRDP API")
    }

    /// Send an H.264 frame with specific damage regions
    ///
    /// Damage regions tell the client which areas changed, enabling partial rendering.
    /// Empty damage_regions = full frame update.
    #[expect(
        clippy::too_many_arguments,
        reason = "frame + damage regions + geometry"
    )]
    pub async fn send_frame_with_regions(
        &self,
        h264_data: &[u8],
        encoded_width: u16,
        encoded_height: u16,
        display_width: u16,
        display_height: u16,
        damage_regions: &[DamageRegion],
        timestamp_ms: u32,
    ) -> SendResult<u32> {
        Self::validate_frame(
            h264_data,
            encoded_width,
            encoded_height,
            display_width,
            display_height,
        )?;
        let state = self.avc420_state().await?;
        let surface_id = Self::surface_id(&state)?;

        // CRITICAL: When damage_regions is empty (full frame update), use encoded
        // dimensions for the region. Windows mstsc requires the AVC region to match
        // the encoded frame dimensions (16-pixel aligned), not the display dimensions.
        // The H.264 bitstream contains encoded_width×encoded_height macroblocks; the
        // region must cover the entire encoded frame or mstsc will reject/black-screen.
        //
        // For damage regions (partial updates), we still use display_width/height
        // because damage detection operates on the visible display area.
        let regions = if damage_regions.is_empty() {
            vec![Avc420Region::full_frame(encoded_width, encoded_height, 22)]
        } else {
            damage_regions_to_avc420(damage_regions, display_width, display_height)
        };

        let (frame_id, dvc_messages, channel_id) = {
            let mut server = self
                .gfx_server
                .lock()
                .map_err(|_| anyhow::anyhow!("EGFX server lock poisoned"))?;
            let channel_id = server
                .channel_id()
                .ok_or_else(|| anyhow::anyhow!("EGFX channel not ready"))?;

            let frame_id =
                server.try_send_avc420_frame(surface_id, h264_data, &regions, timestamp_ms)?;

            let messages = server.drain_output();
            (frame_id, messages, channel_id)
        };

        if !dvc_messages.is_empty() {
            self.send_dvc_messages(channel_id, dvc_messages)?;
        }

        Ok(frame_id)
    }

    /// Send an AVC444 frame with specific damage regions
    ///
    /// Similar to `send_frame_with_regions` but for AVC444 dual-stream encoding.
    ///
    /// Auxiliary stream omission mode.
    ///
    /// The `stream2_data` parameter is now Optional. When `None`, IronRDP's
    /// `send_avc444_frame` will set LC=1 (luma only), instructing the client
    /// to reuse its cached auxiliary stream for bandwidth optimization.
    #[expect(
        clippy::too_many_arguments,
        reason = "dual-stream AVC444 + damage regions + geometry"
    )]
    pub async fn send_avc444_frame_with_regions(
        &self,
        stream1_data: &[u8],
        stream2_data: Option<&[u8]>,
        encoded_width: u16,
        encoded_height: u16,
        display_width: u16,
        display_height: u16,
        damage_regions: &[DamageRegion],
        timestamp_ms: u32,
    ) -> SendResult<u32> {
        Self::validate_frame(
            stream1_data,
            encoded_width,
            encoded_height,
            display_width,
            display_height,
        )?;
        if stream2_data.is_some_and(|data| data.is_empty()) {
            anyhow::bail!("AVC444 auxiliary stream is empty");
        }
        let state = self.avc444_state().await?;
        let surface_id = Self::surface_id(&state)?;

        // Same fix as send_frame_with_regions: full-frame regions must use encoded
        // (16-aligned) dimensions so Windows mstsc sees a region that covers the
        // entire H.264 bitstream. Partial damage regions stay at display size.
        let regions = if damage_regions.is_empty() {
            vec![Avc420Region::full_frame(encoded_width, encoded_height, 22)]
        } else {
            damage_regions_to_avc420(damage_regions, display_width, display_height)
        };

        if regions.len() > 1 {
            debug!(
                "EGFX AVC444: Sending {} regions for {}×{} frame",
                regions.len(),
                display_width,
                display_height
            );
        }

        let (frame_id, dvc_messages, channel_id) = {
            let mut server = self
                .gfx_server
                .lock()
                .map_err(|_| anyhow::anyhow!("EGFX server lock poisoned"))?;
            let channel_id = server
                .channel_id()
                .ok_or_else(|| anyhow::anyhow!("EGFX channel not ready"))?;

            // Pass optional auxiliary stream to IronRDP when available.
            let frame_id = server.try_send_avc444_frame(
                surface_id,
                stream1_data,
                &regions,
                stream2_data,
                stream2_data.map(|_| regions.as_slice()),
                timestamp_ms,
            )?;

            let messages = server.drain_output();

            (frame_id, messages, channel_id)
        };

        if !dvc_messages.is_empty() {
            self.send_dvc_messages(channel_id, dvc_messages)?;
        }

        Ok(frame_id)
    }
}

/// Convert DamageRegion list to Avc420Region list
///
/// Clamps regions to display bounds and assigns QP values.
/// Avc420Region uses left/top/right/bottom (inclusive LTRB) format.
fn damage_regions_to_avc420(
    regions: &[DamageRegion],
    display_width: u16,
    display_height: u16,
) -> Vec<Avc420Region> {
    regions
        .iter()
        .filter_map(|r| {
            // Clamp to display bounds (LTRB format, inclusive)
            if r.width == 0 || r.height == 0 || display_width == 0 || display_height == 0 {
                return None;
            }
            let left = r.x.min(display_width as u32) as u16;
            let top = r.y.min(display_height as u32) as u16;
            // Right and bottom are inclusive, so subtract 1 from the exclusive bounds
            let right =
                r.x.saturating_add(r.width)
                    .min(display_width as u32)
                    .saturating_sub(1) as u16;
            let bottom =
                r.y.saturating_add(r.height)
                    .min(display_height as u32)
                    .saturating_sub(1) as u16;

            // Skip invalid regions (where right < left or bottom < top)
            if right < left || bottom < top {
                return None;
            }

            // Avc420Region fields:
            // - quantization_parameter: H.264 QP (0-51, lower = better quality)
            // - quality: 0-100 (higher = better)
            Some(Avc420Region {
                left,
                top,
                right,
                bottom,
                quantization_parameter: 22, // Good quality/bitrate balance
                quality: 100,               // Maximum quality for damage regions
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_frame_can_be_retried_after_ack_on_same_channel() {
        use ironrdp_dvc::DvcProcessor;
        use ironrdp_egfx::pdu::{
            CapabilitiesAdvertisePdu, CapabilitiesV81Flags, CapabilitySet, GfxPdu,
        };
        use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
        use std::sync::Mutex;
        struct Handler;
        impl GraphicsPipelineHandler for Handler {
            fn capabilities_advertise(&mut self, _: &CapabilitiesAdvertisePdu) {}
            fn on_ready(&mut self, _: &CapabilitySet) {}
        }
        let mut server = GraphicsPipelineServer::new(Box::new(Handler));
        server.start(7).unwrap();
        let caps = GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu::from_typed(&[
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED,
            },
        ]));
        server
            .process(7, &ironrdp_core::encode_vec(&caps).unwrap())
            .unwrap();
        let surface = server.create_surface(16, 16).unwrap();
        server.drain_output();
        server.set_max_frames_in_flight(3);
        server.clamp_max_frames_in_flight(std::num::NonZeroU32::new(1).unwrap());
        let server = Arc::new(Mutex::new(server));
        let state = Arc::new(tokio::sync::RwLock::new(Some(HandlerState {
            is_ready: true,
            is_avc420_enabled: true,
            primary_surface_id: Some(surface),
            ..HandlerState::default()
        })));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sender = EgfxChannelSender::new(Arc::clone(&server), state, tx);
        #[cfg(feature = "h264-source")]
        let mut encoder = crate::rdp::channels::graphics::egfx::h264::encoder::Avc420Encoder::new(
            crate::rdp::channels::graphics::egfx::h264::encoder::EncoderConfig {
                enable_skip_frame: false,
                ..Default::default()
            },
        )
        .unwrap();
        #[cfg(feature = "h264-source")]
        let first_payload = encoder
            .encode_bgra(&vec![0; 16 * 16 * 4], 16, 16, 1)
            .unwrap()
            .unwrap()
            .data;
        #[cfg(not(feature = "h264-source"))]
        let first_payload = vec![1];
        let first = sender
            .send_frame_with_regions(&first_payload, 16, 16, 16, 16, &[], 1)
            .await
            .unwrap();
        assert!(rx.try_recv().is_ok());
        assert!(sender.is_backpressured().unwrap());
        #[cfg(feature = "h264-source")]
        let rejected_payload = encoder
            .encode_bgra(&vec![255; 16 * 16 * 4], 16, 16, 2)
            .unwrap()
            .unwrap()
            .data;
        #[cfg(not(feature = "h264-source"))]
        let rejected_payload = vec![2];
        assert!(
            sender
                .send_frame_with_regions(&rejected_payload, 16, 16, 16, 16, &[], 2)
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err());
        let ack = GfxPdu::FrameAcknowledge(ironrdp_egfx::pdu::FrameAcknowledgePdu {
            frame_id: first,
            queue_depth: ironrdp_egfx::pdu::QueueDepth::Unavailable,
            total_frames_decoded: 1,
        });
        server
            .lock()
            .unwrap()
            .process(7, &ironrdp_core::encode_vec(&ack).unwrap())
            .unwrap();
        assert!(!sender.is_backpressured().unwrap());
        #[cfg(feature = "h264-source")]
        let recovery_payload = {
            // The rejected frame advanced encoder references. Recovery must be
            // independently decodable while retaining the same encoder instance.
            encoder.force_keyframe();
            let data = encoder
                .encode_bgra(&vec![255; 16 * 16 * 4], 16, 16, 3)
                .unwrap()
                .unwrap()
                .data;
            assert!(openh264::nal_units(&data).any(|nal| {
                let prefix = if nal.starts_with(&[0, 0, 0, 1]) { 4 } else { 3 };
                nal.get(prefix).is_some_and(|byte| byte & 0x1f == 5)
            }));
            data
        };
        #[cfg(not(feature = "h264-source"))]
        let recovery_payload = vec![3];
        let next = sender
            .send_frame_with_regions(&recovery_payload, 16, 16, 16, 16, &[], 3)
            .await
            .unwrap();
        assert_ne!(first, next);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn preflight_tracks_acknowledgement_window() {
        use ironrdp_core::encode_vec;
        use ironrdp_dvc::DvcProcessor as _;
        use ironrdp_egfx::{
            pdu::{
                CapabilitiesAdvertisePdu, CapabilitiesV81Flags, CapabilitySet, FrameAcknowledgePdu,
                GfxPdu, QueueDepth,
            },
            server::{GraphicsPipelineHandler, GraphicsPipelineServer},
        };
        struct Handler;
        impl GraphicsPipelineHandler for Handler {
            fn capabilities_advertise(&mut self, _: &CapabilitiesAdvertisePdu) {}
            fn on_ready(&mut self, _: &CapabilitySet) {}
        }
        let mut server = GraphicsPipelineServer::new(Box::new(Handler));
        server.set_max_frames_in_flight(3);
        assert_eq!(
            server.clamp_max_frames_in_flight(std::num::NonZeroU32::new(1).unwrap()),
            1
        );
        let caps = GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu::from_typed(&[
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED,
            },
        ]));
        server.process(0, &encode_vec(&caps).unwrap()).unwrap();
        let surface = server.create_surface(16, 16).unwrap();
        let handle = Arc::new(std::sync::Mutex::new(server));
        let (tx, _rx) = mpsc::unbounded_channel();
        let sender = EgfxChannelSender::new(
            Arc::clone(&handle),
            Arc::new(tokio::sync::RwLock::new(None)),
            tx,
        );
        assert!(!sender.is_backpressured().unwrap());
        let id = handle
            .lock()
            .unwrap()
            .send_avc420_frame(
                surface,
                &[0, 0, 1, 0x67],
                &[Avc420Region::full_frame(16, 16, 22)],
                0,
            )
            .unwrap();
        assert!(sender.is_backpressured().unwrap());
        let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
            frame_id: id,
            queue_depth: QueueDepth::Unavailable,
            total_frames_decoded: 1,
        });
        handle
            .lock()
            .unwrap()
            .process(0, &encode_vec(&ack).unwrap())
            .unwrap();
        assert!(!sender.is_backpressured().unwrap());
    }

    #[test]
    fn frame_validation_rejects_empty_or_inconsistent_geometry() {
        assert!(EgfxChannelSender::validate_frame(&[], 16, 16, 16, 16).is_err());
        assert!(EgfxChannelSender::validate_frame(&[1], 0, 16, 16, 16).is_err());
        assert!(EgfxChannelSender::validate_frame(&[1], 16, 16, 17, 16).is_err());
        assert!(EgfxChannelSender::validate_frame(&[1], 17, 16, 16, 16).is_err());
        assert!(EgfxChannelSender::validate_frame(&[1], 16, 31, 16, 16).is_err());
        assert!(EgfxChannelSender::validate_frame(&[1], 16, 16, 16, 16).is_ok());
    }

    #[test]
    fn empty_damage_does_not_become_a_pixel_at_the_origin() {
        for (width, height) in [(0, 10), (10, 0), (0, 0)] {
            let region = DamageRegion {
                x: 0,
                y: 0,
                width,
                height,
            };
            assert!(damage_regions_to_avc420(&[region], 1920, 1080).is_empty());
        }
    }

    #[test]
    fn damage_conversion_handles_overflowing_regions() {
        let regions = damage_regions_to_avc420(
            &[DamageRegion {
                x: u32::MAX - 1,
                y: u32::MAX - 1,
                width: 100,
                height: 100,
            }],
            1920,
            1080,
        );
        assert!(regions.is_empty());
    }
}
