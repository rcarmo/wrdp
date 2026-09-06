//! Direct desktop capability probes.

use super::{
    capabilities::{CompositorCapabilities, CompositorType, WaylandGlobal},
    portal_caps::PortalCapabilities,
};
use anyhow::Result;
use std::fs;

pub async fn probe_capabilities() -> Result<CompositorCapabilities> {
    probe_capabilities_with_portals(true).await
}
pub async fn probe_capabilities_with_portals(use_portal: bool) -> Result<CompositorCapabilities> {
    let portal = if use_portal {
        PortalCapabilities::probe().await.unwrap_or_default()
    } else {
        PortalCapabilities::default()
    };
    let caps = CompositorCapabilities::new(
        identify_compositor(),
        portal,
        wayland_globals().unwrap_or_default(),
    );
    caps.log_summary();
    Ok(caps)
}

pub fn identify_compositor() -> CompositorType {
    let value = std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .unwrap_or_default();
    identity_hint(&value)
}
fn identity_hint(value: &str) -> CompositorType {
    let v = value.to_ascii_lowercase();
    if v.contains("gnome") {
        CompositorType::Gnome { version: None }
    } else if v.contains("plasma") || v.contains("kde") {
        CompositorType::Kde { version: None }
    } else if v.contains("sway") {
        CompositorType::Sway { version: None }
    } else if v.contains("hyprland") {
        CompositorType::Hyprland { version: None }
    } else if v.contains("weston") {
        CompositorType::Weston
    } else if v.contains("cosmic") {
        CompositorType::Cosmic
    } else if v.contains("wrdp-compositor") {
        CompositorType::Wlroots {
            name: "wrdp-compositor".into(),
        }
    } else {
        CompositorType::Unknown {
            session_info: (!value.is_empty()).then(|| value.into()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct OsRelease {
    pub id: String,
    pub version_id: String,
    pub name: String,
    pub pretty_name: String,
    pub id_like: Vec<String>,
}
impl OsRelease {
    pub fn is_rhel_family(&self) -> bool {
        self.id == "rhel" || self.id_like.iter().any(|v| v == "rhel")
    }
    pub fn is_rhel9(&self) -> bool {
        self.id == "rhel" && self.major_version() == Some(9)
    }
    pub fn is_rhel8(&self) -> bool {
        self.id == "rhel" && self.major_version() == Some(8)
    }
    pub fn major_version(&self) -> Option<u32> {
        self.version_id.split('.').next()?.parse().ok()
    }
}
pub fn detect_os_release() -> Option<OsRelease> {
    let text = fs::read_to_string("/run/host/os-release")
        .or_else(|_| fs::read_to_string("/etc/os-release"))
        .or_else(|_| fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    let mut release = OsRelease::default();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches(['"', '\'']);
        match key {
            "ID" => release.id = value.to_ascii_lowercase(),
            "VERSION_ID" => release.version_id = value.into(),
            "NAME" => release.name = value.into(),
            "PRETTY_NAME" => release.pretty_name = value.into(),
            "ID_LIKE" => {
                release.id_like = value
                    .split_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect();
            }
            _ => {}
        }
    }
    (!release.id.is_empty()).then_some(release)
}

#[cfg(feature = "wayland")]
fn wayland_globals() -> Result<Vec<WaylandGlobal>> {
    use wayland_client::{
        Connection, Dispatch, QueueHandle,
        globals::{GlobalListContents, registry_queue_init},
        protocol::wl_registry,
    };
    struct State;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let connection = Connection::connect_to_env().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let (globals, _) =
        registry_queue_init::<State>(&connection).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(globals.contents().with_list(|list| {
        list.iter()
            .map(|g| WaylandGlobal::new(g.interface.clone(), g.version, g.name))
            .collect()
    }))
}
#[cfg(not(feature = "wayland"))]
fn wayland_globals() -> Result<Vec<WaylandGlobal>> {
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hints_are_diagnostic_only() {
        assert!(matches!(
            identity_hint("GNOME"),
            CompositorType::Gnome { .. }
        ));
        assert!(matches!(identity_hint(""), CompositorType::Unknown { .. }));
    }
}
