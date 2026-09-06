//! Compositor capability probing and profiles.
//!
//! The compositor layer identifies the current Wayland environment, probes
//! Portal and native protocol availability, and turns the result into profiles
//! used by service advertisement, session selection, capture, and diagnostics.
//! Runtime probing behavior lives in `probing`; data shapes and profile defaults
//! are split into smaller modules and re-exported here for callers.

mod capabilities;
pub mod launch;
mod portal_caps;
mod probing;
mod profiles;
pub mod runtime;

pub use capabilities::{
    BufferType, CaptureBackend, CompositorCapabilities, CompositorType, WaylandGlobal,
};
pub use portal_caps::{CursorMode, PortalCapabilities, SourceType};
pub use probing::{
    OsRelease, detect_os_release, identify_compositor, probe_capabilities,
    probe_capabilities_with_portals,
};
pub use profiles::{CompositorProfile, Quirk};

/// Return whether the current process is running in a Wayland session.
pub fn is_wayland_session() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
}

pub fn wayland_display() -> Option<String> {
    std::env::var("WAYLAND_DISPLAY").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_wayland_session() {
        // Environment-dependent smoke check: call path must remain panic-free.
        let _ = is_wayland_session();
    }

    #[test]
    fn test_wayland_display() {
        // Environment-dependent smoke check.
        let _ = wayland_display();
    }
}
