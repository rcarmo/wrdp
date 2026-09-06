//! Wayland state and `Dispatch` implementations.
//!
//! Centralizes protocol event handling for the runtime Wayland connection so
//! backend services consume shared state instead of managing separate clients.

use std::sync::{Arc, Mutex};

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::GlobalListContents,
    protocol::{
        wl_buffer::WlBuffer, wl_output::WlOutput, wl_registry::WlRegistry, wl_seat::WlSeat,
        wl_shm::WlShm, wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::ext::{
    data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
        ext_data_control_source_v1::{self, ExtDataControlSourceV1},
    },
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::{
    data_control::v1::client::{
        zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
        zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
        zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
    },
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};

use super::{
    data_control::DataControlState, ext_capture::ExtCaptureState, screencopy::ScreencopyState,
};
use crate::desktop::portal::xdg_desktop::types::SourceInfo;

/// Output information collected from wl_output events.
#[derive(Debug, Clone, Default)]
pub struct OutputInfo {
    /// Global name from the registry.
    pub global_name: u32,
    /// Output name (from wl_output.name event, wayland 4+).
    pub name: Option<String>,
    /// Output description (from wl_output.description event).
    pub description: Option<String>,
    /// Physical width in mm.
    pub physical_width: i32,
    /// Physical height in mm.
    pub physical_height: i32,
    /// Current mode width in pixels.
    pub width: u32,
    /// Current mode height in pixels.
    pub height: u32,
    /// Current mode refresh rate in millihertz.
    pub refresh: u32,
    /// Position x.
    pub x: i32,
    /// Position y.
    pub y: i32,
    /// Whether this output info has received a 'done' event.
    pub done: bool,
}

impl OutputInfo {
    /// Convert to a SourceInfo for the portal.
    pub fn to_source_info(&self) -> SourceInfo {
        SourceInfo {
            id: self.global_name,
            name: self
                .name
                .clone()
                .unwrap_or_else(|| format!("output-{}", self.global_name)),
            description: self.description.clone().unwrap_or_default(),
            width: self.width,
            height: self.height,
            refresh_rate: self.refresh,
            source_type: crate::desktop::portal::xdg_desktop::types::SourceType::Monitor,
        }
    }
}

/// Shared Wayland state holding bound globals and protocol objects.
///
/// This is the central mutable state used by dispatch handlers for capture,
/// data-control, and input protocol events.
#[derive(Default)]
pub struct WaylandState {
    // === Input managers ===
    /// Virtual pointer manager (wlr protocol).
    pub pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    /// Virtual keyboard manager (misc protocol).
    pub keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,

    // === Core ===
    /// Seat global.
    pub seat: Option<WlSeat>,
    /// Known outputs with their info.
    pub outputs: Vec<(WlOutput, Arc<Mutex<OutputInfo>>)>,

    // === Screen capture ===
    /// wlr-screencopy frame capture state.
    pub screencopy: ScreencopyState,
    /// ext-image-copy-capture state (preferred protocol).
    pub ext_capture: ExtCaptureState,

    // === Clipboard ===
    /// Data control clipboard state (ext or wlr protocol).
    pub data_control: DataControlState,

    // === Initialization ===
    /// Whether initial roundtrip is complete.
    pub initialized: bool,
}

impl WaylandState {
    /// Get all completed output infos as SourceInfo.
    pub fn get_sources(&self) -> Vec<SourceInfo> {
        self.outputs
            .iter()
            .filter_map(|(_, info)| {
                let info = info.lock().ok()?;
                if info.done && info.width > 0 && info.height > 0 {
                    Some(info.to_source_info())
                } else {
                    None
                }
            })
            .collect()
    }
}

// === Dispatch implementations ===

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;

        match event {
            Event::Global {
                name,
                interface,
                version,
            } => {
                // Handle new wl_output globals (output hotplug)
                if interface == "wl_output" && state.initialized {
                    tracing::info!(name, version, "New wl_output global detected (hotplug)");
                    let info = Arc::new(Mutex::new(OutputInfo {
                        global_name: name,
                        ..Default::default()
                    }));
                    let bind_version = version.min(4);
                    let output: WlOutput = registry.bind(name, bind_version, qh, info.clone());
                    state.outputs.push((output, info));
                }
            }
            Event::GlobalRemove { name } => {
                // Handle removed wl_output globals (output unplug)
                let idx = state
                    .outputs
                    .iter()
                    .position(|(_, info)| info.lock().is_ok_and(|i| i.global_name == name));
                if let Some(idx) = idx {
                    let (output, info) = state.outputs.remove(idx);
                    let output_name = info
                        .lock()
                        .ok()
                        .and_then(|i| i.name.clone())
                        .unwrap_or_else(|| format!("output-{name}"));
                    tracing::info!(
                        name,
                        output = %output_name,
                        "wl_output global removed (unplug)"
                    );
                    output.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_seat::Event;
        match event {
            Event::Capabilities { capabilities } => {
                tracing::debug!("Seat capabilities: {:?}", capabilities);
            }
            Event::Name { name } => {
                tracing::debug!("Seat name: {}", name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, Arc<Mutex<OutputInfo>>> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        data: &Arc<Mutex<OutputInfo>>,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output::Event;
        if let Ok(mut info) = data.lock() {
            match event {
                Event::Geometry {
                    x,
                    y,
                    physical_width,
                    physical_height,
                    ..
                } => {
                    info.x = x;
                    info.y = y;
                    info.physical_width = physical_width;
                    info.physical_height = physical_height;
                }
                Event::Mode {
                    flags,
                    width,
                    height,
                    refresh,
                } => {
                    use wayland_client::{WEnum, protocol::wl_output::Mode};
                    // Only use current mode
                    if let WEnum::Value(f) = flags
                        && f.contains(Mode::Current)
                    {
                        info.width = width as u32;
                        info.height = height as u32;
                        info.refresh = refresh as u32;
                    }
                }
                Event::Name { name } => {
                    info.name = Some(name);
                }
                Event::Description { description } => {
                    info.description = Some(description);
                }
                Event::Done => {
                    info.done = true;
                    tracing::debug!(
                        "Output {} done: {}x{}@{} ({})",
                        info.global_name,
                        info.width,
                        info.height,
                        info.refresh,
                        info.name.as_deref().unwrap_or("unnamed"),
                    );
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events
    }
}

// === SHM buffer protocol Dispatch impls ===

impl Dispatch<WlShm, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        event: <WlShm as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_shm::Event;
        if let Event::Format { format } = event {
            tracing::trace!("wl_shm supports format: {:?}", format);
        }
    }
}

impl Dispatch<WlShmPool, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events
    }
}

impl Dispatch<WlBuffer, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        event: <WlBuffer as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_buffer::Event;
        if let Event::Release = event {
            // Buffer released by compositor — we track this via frame events instead
            tracing::trace!("wl_buffer released");
        }
    }
}

// === wlr-screencopy Dispatch impls ===

impl Dispatch<ZwlrScreencopyManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Manager has no events
    }
}

/// Dispatch for screencopy frame events.
///
/// User data is the PipeWire `node_id` identifying which active capture this
/// frame belongs to; events are routed into [`ScreencopyState`].
impl Dispatch<ZwlrScreencopyFrameV1, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as wayland_client::Proxy>::Event,
        data: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let node_id = *data;

        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => match format {
                wayland_client::WEnum::Value(f) => {
                    let info = super::screencopy::BufferFormatInfo {
                        format: f,
                        format_raw: f as u32,
                        width,
                        height,
                        stride,
                    };
                    state.screencopy.on_frame_buffer(node_id, info, qh);
                }
                wayland_client::WEnum::Unknown(_) => {
                    tracing::warn!(node_id, "Unknown SHM format in screencopy buffer event");
                }
            },

            zwlr_screencopy_frame_v1::Event::Flags { .. } => {
                // Flags indicate Y-invert etc. We handle this in PipeWire format if needed.
                tracing::trace!(node_id, "Screencopy frame flags received");
            }

            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.screencopy.on_frame_ready(node_id, qh);
            }

            zwlr_screencopy_frame_v1::Event::Failed => {
                state.screencopy.on_frame_failed(node_id, qh);
            }

            zwlr_screencopy_frame_v1::Event::Damage { .. } => {
                // Damage info — we capture the full frame each time for simplicity
                tracing::trace!(node_id, "Screencopy frame damage received");
            }

            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                state.screencopy.on_frame_linux_dmabuf(
                    node_id,
                    super::screencopy::DmabufFormatInfo {
                        format,
                        width,
                        height,
                    },
                );
            }

            zwlr_screencopy_frame_v1::Event::BufferDone => {
                // v3: all buffer format events sent, allocate and copy
                state.screencopy.on_frame_buffer_done(node_id, qh);
            }

            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        _event: <ZwpLinuxDmabufV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxBufferParamsV1,
        _event: <ZwpLinuxBufferParamsV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// === wlr-data-control Dispatch impls ===

impl Dispatch<ZwlrDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Manager has no events
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.data_control.on_data_offer_wlr(id);
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if id.is_some() {
                    state.data_control.on_selection();
                } else {
                    state.data_control.on_selection_cleared();
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                state.data_control.on_device_finished();
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {
                // We only handle the regular clipboard, not primary selection
                tracing::trace!("wlr data control primary selection event (ignored)");
            }
            _ => {}
        }
    }

    // DataOffer event creates a child ZwlrDataControlOfferV1 object;
    // without this the default panics in wayland-client's event_queue.rs
    wayland_client::event_created_child!(WaylandState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlSourceV1,
        event: <ZwlrDataControlSourceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.data_control.on_source_send(&mime_type, fd);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.data_control.on_source_cancelled();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        event: <ZwlrDataControlOfferV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.data_control.on_offer_mime_type(mime_type);
        }
    }
}

// === ext-image-copy-capture Dispatch impls ===

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtOutputImageCaptureSourceManagerV1,
        _event: <ExtOutputImageCaptureSourceManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Manager has no events
    }
}

impl Dispatch<ExtImageCaptureSourceV1, u32> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: <ExtImageCaptureSourceV1 as wayland_client::Proxy>::Event,
        _data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Source is an opaque handle; no events
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: <ExtImageCopyCaptureManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Manager has no events
    }
}

/// Dispatch for ext-image-copy-capture session events.
///
/// User data is the PipeWire node_id (`u32`) identifying which active capture
/// this session belongs to. Events route through to [`ExtCaptureState`] methods.
impl Dispatch<ExtImageCopyCaptureSessionV1, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as wayland_client::Proxy>::Event,
        data: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let node_id = *data;

        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state
                    .ext_capture
                    .on_session_buffer_size(node_id, width, height);
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                state
                    .ext_capture
                    .on_session_shm_format(node_id, format.into());
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { .. } => {
                tracing::trace!(
                    node_id,
                    "ext capture session dmabuf device (ignored, using SHM)"
                );
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { .. } => {
                tracing::trace!(
                    node_id,
                    "ext capture session dmabuf format (ignored, using SHM)"
                );
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                state.ext_capture.on_session_done(node_id, qh);
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.ext_capture.on_session_stopped(node_id);
            }
            _ => {}
        }
    }
}

/// Dispatch for ext-image-copy-capture frame events.
///
/// User data is the PipeWire node_id (`u32`) for routing.
impl Dispatch<ExtImageCopyCaptureFrameV1, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as wayland_client::Proxy>::Event,
        data: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let node_id = *data;

        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { .. } => {
                tracing::trace!(node_id, "ext frame transform received");
            }
            ext_image_copy_capture_frame_v1::Event::Damage { .. } => {
                tracing::trace!(node_id, "ext frame damage received");
            }
            ext_image_copy_capture_frame_v1::Event::PresentationTime { .. } => {
                tracing::trace!(node_id, "ext frame presentation time received");
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.ext_capture.on_frame_ready(node_id, qh);
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                let reason_val = match reason {
                    wayland_client::WEnum::Value(r) => r as u32,
                    wayland_client::WEnum::Unknown(_) => 0,
                };
                state.ext_capture.on_frame_failed(node_id, reason_val, qh);
            }
            _ => {}
        }
    }
}

// === ext-data-control Dispatch impls ===

impl Dispatch<ExtDataControlManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Manager has no events
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.data_control.on_data_offer_ext(id);
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                if id.is_some() {
                    state.data_control.on_selection();
                } else {
                    state.data_control.on_selection_cleared();
                }
            }
            ext_data_control_device_v1::Event::Finished => {
                state.data_control.on_device_finished();
            }
            ext_data_control_device_v1::Event::PrimarySelection { .. } => {
                tracing::trace!("ext data control primary selection event (ignored)");
            }
            _ => {}
        }
    }

    // DataOffer event creates a child ExtDataControlOfferV1 object;
    // without this the default panics in wayland-client's event_queue.rs
    wayland_client::event_created_child!(WaylandState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlSourceV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlSourceV1,
        event: <ExtDataControlSourceV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.data_control.on_source_send(&mime_type, fd);
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.data_control.on_source_cancelled();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlOfferV1,
        event: <ExtDataControlOfferV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.data_control.on_offer_mime_type(mime_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wayland_state_default() {
        let state = WaylandState::default();
        assert!(state.pointer_manager.is_none());
        assert!(state.keyboard_manager.is_none());
        assert!(state.seat.is_none());
        assert!(state.outputs.is_empty());
        assert!(!state.initialized);
    }

    #[test]
    fn test_output_info_to_source_info() {
        let info = OutputInfo {
            global_name: 1,
            name: Some("eDP-1".to_string()),
            description: Some("Built-in Display".to_string()),
            width: 1920,
            height: 1080,
            refresh: 60000,
            done: true,
            ..Default::default()
        };

        let source = info.to_source_info();
        assert_eq!(source.id, 1);
        assert_eq!(source.name, "eDP-1");
        assert_eq!(source.description, "Built-in Display");
        assert_eq!(source.width, 1920);
        assert_eq!(source.height, 1080);
        assert_eq!(source.refresh_rate, 60000);
    }

    #[test]
    fn test_get_sources_filters_incomplete() {
        let state = WaylandState::default();
        // No outputs, no sources
        assert!(state.get_sources().is_empty());
    }
}
