//! Deployment-context detection for managed wrdp runtime sessions.
//!
//! These helpers classify host execution context (native, Flatpak, systemd
//! user/system, non-systemd init) so service translation and diagnostics can
//! report realistic capability expectations.
#![expect(unsafe_code, reason = "libc::getuid for systemd linger detection")]

use std::{path::Path, process::Command};

use tracing::{debug, info};

/// Deployment context used by service translation and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeploymentContext {
    /// Native host package/runtime context.
    Native,
    /// Flatpak sandbox context.
    Flatpak,
    /// systemd user-service context.
    SystemdUser {
        /// loginctl enable-linger active
        linger_enabled: bool,
    },
    /// systemd system-service context.
    SystemdSystem,
    /// Non-systemd init context (OpenRC/runit/etc.).
    InitD,
}

impl DeploymentContext {
    /// Stable identifier used by text and JSON diagnostics.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Flatpak => "flatpak",
            Self::SystemdUser { .. } => "systemd-user",
            Self::SystemdSystem => "systemd-system",
            Self::InitD => "init",
        }
    }

    pub const fn linger_enabled(self) -> Option<bool> {
        match self {
            Self::SystemdUser { linger_enabled } => Some(linger_enabled),
            _ => None,
        }
    }
}

impl std::fmt::Display for DeploymentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

pub fn detect_deployment_context() -> DeploymentContext {
    debug!("Detecting deployment context...");

    if is_flatpak_deployment() {
        info!("Detected Flatpak deployment");
        return DeploymentContext::Flatpak;
    }

    if is_systemd_invocation() {
        if has_user_runtime_env() {
            let linger_enabled = check_linger_enabled();
            info!("Detected systemd user service (linger: {})", linger_enabled);
            return DeploymentContext::SystemdUser { linger_enabled };
        }

        info!("Detected systemd system service");
        return DeploymentContext::SystemdSystem;
    }

    if systemd_runtime_available() {
        info!("Detected native systemd package");
        DeploymentContext::Native
    } else {
        info!("Detected non-systemd init");
        DeploymentContext::InitD
    }
}

fn is_flatpak_deployment() -> bool {
    Path::new("/.flatpak-info").exists()
}

fn is_systemd_invocation() -> bool {
    std::env::var("INVOCATION_ID").is_ok()
}

fn has_user_runtime_env() -> bool {
    std::env::var("USER").is_ok() && std::env::var("XDG_RUNTIME_DIR").is_ok()
}

fn systemd_runtime_available() -> bool {
    Path::new("/run/systemd/system").exists()
}

fn check_linger_enabled() -> bool {
    let uid = unsafe { libc::getuid() };
    let linger_path = format!("/var/lib/systemd/linger/{uid}");

    if Path::new(&linger_path).exists() {
        return true;
    }

    Command::new("loginctl")
        .args(["show-user", &uid.to_string(), "-p", "Linger"])
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .eq("Linger=yes")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_stable_and_linger_is_structured() {
        let cases = [
            (DeploymentContext::Native, "native", None),
            (DeploymentContext::Flatpak, "flatpak", None),
            (
                DeploymentContext::SystemdUser {
                    linger_enabled: true,
                },
                "systemd-user",
                Some(true),
            ),
            (DeploymentContext::SystemdSystem, "systemd-system", None),
            (DeploymentContext::InitD, "init", None),
        ];
        for (context, id, linger) in cases {
            assert_eq!(context.id(), id);
            assert_eq!(context.to_string(), id);
            assert_eq!(context.linger_enabled(), linger);
        }
    }
}
