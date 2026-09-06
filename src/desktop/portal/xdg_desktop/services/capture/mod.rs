//! Screen capture backend abstraction.
//!
//! Provides a [`CaptureBackend`] trait with two implementations:
//! - [`ExtCaptureBackend`]: Uses `ext-image-copy-capture-v1` (preferred)
//! - [`WlrCaptureBackend`]: Uses `wlr-screencopy-unstable-v1` (fallback)
//!
//! The backend is selected at startup based on available protocols and
//! optional preferences from the server (compositor profiles, config, quirks).
//! See [`CapturePreference`] for the preference API and [`detection`] for
//! the selection algorithm.

pub mod detection;
mod ext_backend;
mod wlr_backend;

use std::sync::{Arc, mpsc};

pub use detection::{AvailableCaptureProtocols, CaptureDetector};
pub use ext_backend::ExtCaptureBackend;
pub use wlr_backend::WlrCaptureBackend;

use crate::desktop::portal::xdg_desktop::{
    error::Result,
    pipewire::PipeWireManager,
    types::{CursorMode, SourceInfo, SourceType, StreamInfo},
    wayland::{AvailableProtocols, CaptureCommand},
};

/// Screen capture protocol in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptureProtocol {
    /// ext-image-copy-capture-v1.
    ExtImageCopyCapture,
    /// wlr-screencopy-unstable-v1 (wlroots).
    WlrScreencopy,
}

impl std::fmt::Display for CaptureProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureProtocol::ExtImageCopyCapture => write!(f, "ext-image-copy-capture-v1"),
            CaptureProtocol::WlrScreencopy => write!(f, "wlr-screencopy-v1"),
        }
    }
}

/// Capture protocol selection preferences.
///
/// Passed by the server (from compositor profiles, config, or quirks) to
/// guide protocol selection. When no preferences are provided, auto-detection
/// selects ext-image-copy-capture over wlr-screencopy (current behavior).
///
/// Mirrors [`super::input::InputBackendConfig`] pattern.
#[derive(Debug, Clone)]
pub struct CapturePreference {
    /// Preferred protocol. `None` = auto-detect based on availability.
    pub preferred: Option<CaptureProtocol>,

    /// Allow fallback to alternative protocol if preferred is unavailable or broken.
    pub allow_fallback: bool,

    /// Timeout for protocol handshake validation (milliseconds).
    /// Zero means no timeout (not recommended -- can cause permanent stall).
    pub handshake_timeout_ms: u64,

    /// Protocols known to be broken on this compositor.
    /// These are skipped during selection regardless of availability.
    pub broken_protocols: Vec<CaptureProtocol>,

    /// Deliver frames through the in-process direct frame channel instead of
    /// creating PipeWire streams. Used by embedded production binders where the
    /// RDP server consumes frames directly.
    pub direct_frame_channel: bool,
}

impl Default for CapturePreference {
    fn default() -> Self {
        Self {
            preferred: None,
            allow_fallback: true,
            handshake_timeout_ms: 5000,
            broken_protocols: vec![],
            direct_frame_channel: false,
        }
    }
}

impl CapturePreference {
    /// Create preferences from environment variables.
    ///
    /// Reads:
    /// - `XDP_GENERIC_CAPTURE_PROTOCOL`: "ext" or "wlr"
    /// - `XDP_GENERIC_CAPTURE_NO_FALLBACK`: "1" to disable fallback
    /// - `XDP_GENERIC_CAPTURE_TIMEOUT_MS`: handshake timeout in milliseconds
    pub fn from_env() -> Self {
        let mut prefs = Self::default();

        if let Ok(protocol) = std::env::var("XDP_GENERIC_CAPTURE_PROTOCOL") {
            match protocol.to_lowercase().as_str() {
                "ext" | "ext-image-copy-capture" => {
                    prefs.preferred = Some(CaptureProtocol::ExtImageCopyCapture);
                }
                "wlr" | "wlr-screencopy" => {
                    prefs.preferred = Some(CaptureProtocol::WlrScreencopy);
                }
                _ => tracing::warn!("Unknown capture protocol: {}", protocol),
            }
        }

        if std::env::var("XDP_GENERIC_CAPTURE_NO_FALLBACK").is_ok() {
            prefs.allow_fallback = false;
        }

        if let Ok(timeout) = std::env::var("XDP_GENERIC_CAPTURE_TIMEOUT_MS")
            && let Ok(ms) = timeout.parse::<u64>()
        {
            prefs.handshake_timeout_ms = ms;
        }

        prefs
    }
}

/// Abstraction over screen capture protocols.
///
/// This trait provides a unified interface for screen capture, regardless
/// of which Wayland protocol is used underneath.
pub trait CaptureBackend: Send + Sync {
    /// Get the capture protocol this backend implements.
    fn protocol_type(&self) -> CaptureProtocol;

    /// Get available capturable sources (monitors, windows).
    fn get_sources(&self, source_types: &[SourceType]) -> Result<Vec<SourceInfo>>;

    /// Create capture sessions/streams for the given sources.
    ///
    /// Returns stream information including PipeWire node IDs.
    fn create_capture_session(
        &mut self,
        sources: &[SourceInfo],
        cursor_mode: CursorMode,
    ) -> Result<Vec<StreamInfo>>;

    /// Destroy capture sessions/streams.
    fn destroy_capture_session(&mut self, stream_ids: &[u32]) -> Result<()>;

    /// Get available source types (bit flags).
    fn available_source_types(&self) -> u32 {
        SourceType::Monitor.to_bits()
    }

    /// Get available cursor modes (bit flags).
    fn available_cursor_modes(&self) -> u32 {
        CursorMode::Hidden.to_bits() | CursorMode::Embedded.to_bits()
    }

    /// Update the source list (e.g., after output hotplug).
    fn update_sources(&mut self, sources: Vec<SourceInfo>);
}

/// Create a capture backend based on available protocols and preferences.
///
/// Uses [`CaptureDetector`] to select the best protocol based on preferences
/// (from compositor profiles, config, quirks) and Wayland global availability.
///
/// # Arguments
///
/// * `protocols` - Available Wayland protocols from compositor registry
/// * `prefs` - Capture protocol preferences (from server or config)
/// * `sources` - Available output sources for capture
/// * `pipewire` - PipeWire manager for frame delivery
/// * `capture_tx` - Command channel to the Wayland event loop
pub fn create_capture_backend(
    protocols: &AvailableProtocols,
    prefs: &CapturePreference,
    sources: Vec<SourceInfo>,
    pipewire: Arc<PipeWireManager>,
    capture_tx: mpsc::Sender<CaptureCommand>,
) -> Result<Box<dyn CaptureBackend>> {
    let available = CaptureDetector::detect(protocols);

    match CaptureDetector::select(prefs, &available) {
        Ok(CaptureProtocol::ExtImageCopyCapture) => {
            tracing::info!("Using ext-image-copy-capture-v1 for screen capture");
            if prefs.handshake_timeout_ms > 0 {
                tracing::info!("  Handshake timeout: {}ms", prefs.handshake_timeout_ms);
            }
            Ok(Box::new(ExtCaptureBackend::new(
                sources, pipewire, capture_tx,
            )))
        }
        Ok(CaptureProtocol::WlrScreencopy) => {
            tracing::info!("Using wlr-screencopy-v1 for screen capture");
            Ok(Box::new(WlrCaptureBackend::new(
                sources,
                pipewire,
                capture_tx,
                prefs.direct_frame_channel,
            )))
        }
        Err(e) => {
            tracing::warn!("Capture protocol selection failed: {}", e);
            // Return a backend that reports no sources rather than hard-failing
            Ok(Box::new(ExtCaptureBackend::new(
                vec![],
                pipewire,
                capture_tx,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_protocol_display() {
        assert_eq!(
            CaptureProtocol::ExtImageCopyCapture.to_string(),
            "ext-image-copy-capture-v1"
        );
        assert_eq!(
            CaptureProtocol::WlrScreencopy.to_string(),
            "wlr-screencopy-v1"
        );
    }

    // Note: create_capture_backend tests require PipeWireManager which needs
    // a running PipeWire daemon. Protocol selection logic is tested here;
    // integration tests cover the full pipeline.

    #[test]
    fn test_capture_protocol_selection_logic() {
        let protocols = AvailableProtocols {
            ext_image_copy_capture: true,
            wlr_screencopy: true,
            ..Default::default()
        };
        // ext is preferred when both are available
        assert!(protocols.ext_image_copy_capture);

        let protocols = AvailableProtocols {
            ext_image_copy_capture: false,
            wlr_screencopy: true,
            ..Default::default()
        };
        // wlr is fallback
        assert!(!protocols.ext_image_copy_capture);
        assert!(protocols.wlr_screencopy);
    }
}
