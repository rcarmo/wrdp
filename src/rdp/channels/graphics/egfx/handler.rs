//! IronRDP EGFX negotiation callbacks.

use crate::rdp::channels::graphics::egfx::channel::{
    EgfxCodecPolicy, HandlerState, NegotiatedEgfxMode, SharedHandlerState,
};
use ironrdp_egfx::{
    pdu::{
        CapabilitiesAdvertisePdu, CapabilitiesV10Flags, CapabilitiesV81Flags,
        CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet,
    },
    server::GraphicsPipelineHandler,
};
use std::sync::Arc;

pub struct WrdpGraphicsHandler {
    state: SharedHandlerState,
    policy: EgfxCodecPolicy,
    max_frames: u32,
}
impl WrdpGraphicsHandler {
    pub fn with_quirks(_width: u16, _height: u16, avc420_only: bool) -> Self {
        Self {
            state: Arc::new(tokio::sync::RwLock::new(None)),
            policy: if avc420_only {
                EgfxCodecPolicy::Avc420
            } else {
                EgfxCodecPolicy::Auto
            },
            max_frames: 3,
        }
    }
    pub fn with_config(
        _width: u16,
        _height: u16,
        state: SharedHandlerState,
        policy: EgfxCodecPolicy,
        max_frames: u32,
    ) -> Self {
        Self {
            state,
            policy,
            max_frames,
        }
    }
    fn publish(&self, value: Option<HandlerState>) {
        for _ in 0..50 {
            if let Ok(mut state) = self.state.try_write() {
                *state = value;
                return;
            }
            std::thread::yield_now();
        }
        tracing::warn!("EGFX negotiated state update was contended");
    }
}
impl GraphicsPipelineHandler for WrdpGraphicsHandler {
    fn capabilities_advertise(&mut self, pdu: &CapabilitiesAdvertisePdu) {
        tracing::debug!(sets = pdu.0.len(), "EGFX capabilities advertised");
    }
    fn on_ready(&mut self, cap: &CapabilitySet) {
        let support = codec_support(cap);
        let mode = select_mode(self.policy, support);
        let (avc420, avc444) = (
            mode == NegotiatedEgfxMode::Avc420,
            mode == NegotiatedEgfxMode::Avc444,
        );
        tracing::info!(
            policy = ?self.policy,
            selected = mode.name(),
            wire_avc420 = support.avc420,
            wire_avc444 = support.avc444,
            "graphics mode selected"
        );
        self.publish(Some(HandlerState {
            is_ready: true,
            negotiated_mode: Some(mode),
            is_avc420_enabled: avc420,
            is_avc444_enabled: avc444,
            requires_core_reset: false,
            // Modern Microsoft mobile clients render their own local pointer.
            // Sending an additional server bitmap/position stream produces a
            // duplicate cursor and a hotspot that diverges from injected input.
            needs_android_pointer_updates: false,
            primary_surface_id: None,
            dvc_channel_id: 0,
        }));
    }
    fn on_close(&mut self) {
        tracing::warn!(
            reason = "rdpgfx-channel-closed",
            "graphics fallback selected"
        );
        self.publish(Some(bitmap_fallback_state(true)));
    }
    fn max_frames_in_flight(&self) -> u32 {
        self.max_frames
    }
    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE,
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::SMALL_CACHE,
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
            },
        ]
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodecSupport {
    avc420: bool,
    avc444: bool,
}

fn codec_support(cap: &CapabilitySet) -> CodecSupport {
    match cap {
        CapabilitySet::V8 { .. } => disabled(),
        CapabilitySet::V8_1 { flags } => {
            let avc = flags.contains(CapabilitiesV81Flags::AVC420_ENABLED);
            CodecSupport {
                avc420: avc,
                avc444: false,
            }
        }
        CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => {
            enabled(!flags.contains(CapabilitiesV10Flags::AVC_DISABLED))
        }
        CapabilitySet::V10_1 => enabled(true),
        CapabilitySet::V10_3 { flags } => {
            enabled(!flags.contains(CapabilitiesV103Flags::AVC_DISABLED))
        }
        CapabilitySet::V10_4 { flags }
        | CapabilitySet::V10_5 { flags }
        | CapabilitySet::V10_6 { flags }
        | CapabilitySet::V10_6Err { flags } => {
            enabled(!flags.contains(CapabilitiesV104Flags::AVC_DISABLED))
        }
        CapabilitySet::V10_7 { flags } => {
            enabled(!flags.contains(CapabilitiesV107Flags::AVC_DISABLED))
        }
    }
}
const fn enabled(value: bool) -> CodecSupport {
    CodecSupport {
        avc420: value,
        avc444: value,
    }
}

const fn disabled() -> CodecSupport {
    enabled(false)
}

fn bitmap_fallback_state(requires_core_reset: bool) -> HandlerState {
    HandlerState {
        is_ready: true,
        negotiated_mode: Some(NegotiatedEgfxMode::Bitmap),
        requires_core_reset,
        needs_android_pointer_updates: false,
        ..HandlerState::default()
    }
}

fn select_mode(policy: EgfxCodecPolicy, support: CodecSupport) -> NegotiatedEgfxMode {
    match policy {
        EgfxCodecPolicy::Bitmap => NegotiatedEgfxMode::Bitmap,
        EgfxCodecPolicy::Avc444 if support.avc444 => NegotiatedEgfxMode::Avc444,
        EgfxCodecPolicy::Avc420 if support.avc420 => NegotiatedEgfxMode::Avc420,
        EgfxCodecPolicy::Auto if support.avc444 => NegotiatedEgfxMode::Avc444,
        EgfxCodecPolicy::Auto if support.avc420 => NegotiatedEgfxMode::Avc420,
        _ => NegotiatedEgfxMode::Bitmap,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v81_requires_flag() {
        assert_eq!(
            codec_support(&CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::empty()
            }),
            disabled()
        );
    }

    #[test]
    fn policy_is_intersected_with_wire_support() {
        let avc = CodecSupport {
            avc420: true,
            avc444: true,
        };
        assert_eq!(
            select_mode(EgfxCodecPolicy::Auto, avc),
            NegotiatedEgfxMode::Avc444
        );
        assert_eq!(
            select_mode(EgfxCodecPolicy::Avc420, avc),
            NegotiatedEgfxMode::Avc420
        );
        assert_eq!(
            select_mode(EgfxCodecPolicy::Bitmap, avc),
            NegotiatedEgfxMode::Bitmap
        );
        assert_eq!(
            select_mode(EgfxCodecPolicy::Avc444, disabled()),
            NegotiatedEgfxMode::Bitmap
        );
    }

    #[test]
    fn negotiation_failure_state_releases_pipeline_to_bitmap() {
        let state = bitmap_fallback_state(false);
        assert!(state.is_ready);
        assert_eq!(state.negotiated_mode, Some(NegotiatedEgfxMode::Bitmap));
        assert!(!state.is_avc420_enabled);
        assert!(!state.is_avc444_enabled);
        assert!(!state.requires_core_reset);
        assert!(!state.needs_android_pointer_updates);
        assert!(bitmap_fallback_state(true).requires_core_reset);
    }

    #[test]
    fn avc_disabled_capability_selects_bitmap() {
        let support = codec_support(&CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::AVC_DISABLED,
        });
        assert_eq!(
            select_mode(EgfxCodecPolicy::Auto, support),
            NegotiatedEgfxMode::Bitmap
        );
    }
}
