//! Session backend interfaces for managed wrdp runtime sessions.
//!
//! Defines the common traits used by runtime backends that provide capture,
//! input injection, health reporting, and clipboard wiring.

#[cfg(feature = "portal-generic")]
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::rdp::session::supervision::SessionStatusReporter;

pub type DirectFrameReceiver =
    std::sync::mpsc::Receiver<crate::desktop::pipewire::frame::RawFrameData>;

/// Describes how a backend provides clipboard support.
///
/// Each backend returns one of these variants from `clipboard_source()`,
/// telling the server what clipboard path is available without exposing
/// backend internals to server code.
pub enum ClipboardSource {
    /// Wayland data-control protocol (portal-generic backend).
    /// Clipboard is handled via ext-data-control-v1 or wlr-data-control-v1.
    #[cfg(feature = "portal-generic")]
    DataControl(
        Arc<std::sync::Mutex<Box<dyn crate::desktop::portal::xdg_desktop::ClipboardBackend>>>,
    ),

    /// No clipboard support from this backend.
    None,
}

/// Common session handle trait for managed compositor runtime backends.
#[async_trait]
pub trait SessionHandle: Send + Sync {
    fn direct_frame_receiver(&self) -> Result<DirectFrameReceiver>;

    fn streams(&self) -> Vec<StreamInfo>;

    // === Input Injection Methods ===

    async fn notify_keyboard_keycode(&self, keycode: i32, pressed: bool) -> Result<()>;

    async fn notify_keyboard_keysym(&self, keysym: i32, pressed: bool) -> Result<()> {
        let _ = (keysym, pressed);
        anyhow::bail!("Keyboard keysym injection is not available for this session backend")
    }

    async fn notify_pointer_motion_absolute(&self, stream_id: u32, x: f64, y: f64) -> Result<()>;

    async fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()>;

    async fn notify_pointer_axis(&self, dx: f64, dy: f64) -> Result<()>;

    // === Health Integration ===

    /// Wire a health reporter into this session handle.
    ///
    /// Called once after session creation. The reporter is used to notify the
    /// health monitor of session lifecycle events (closed, invalidated, errors).
    /// Default: no-op for backends that don't support health reporting.
    fn set_health_reporter(&self, _reporter: SessionStatusReporter) {}

    /// Provide stream info from a non-local video source.
    fn set_streams(&self, _streams: Vec<StreamInfo>) {}

    // === Deterministic Teardown ===

    /// Deterministically tear down this session's resources.
    ///
    /// Idempotent: repeated calls are no-ops after the first successful call.
    /// Backends that own compositor resources should destroy input contexts and
    /// capture sessions/streams, signal and join their background loops without
    /// blocking the async runtime, and report the session closed exactly once.
    ///
    /// Default: no-op for backends that don't need explicit teardown. `Drop`
    /// remains a best-effort stop path and must not duplicate the closed report.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // === Clipboard Support ===

    /// Describes how this backend provides clipboard functionality.
    ///
    /// The server uses this to wire the correct clipboard provider without
    /// needing to know backend-specific details.
    fn clipboard_source(&self) -> ClipboardSource;
}

/// Stream information shared across runtime backends.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub position_x: i32,
    pub position_y: i32,
}

impl StreamInfo {
    pub fn new(node_id: u32, width: u32, height: u32, position_x: i32, position_y: i32) -> Self {
        Self {
            node_id,
            width,
            height,
            position_x,
            position_y,
        }
    }
}
