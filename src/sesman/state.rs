//! Persisted sesman state and configuration schema for managed wrdp sessions.
//!
//! This module defines the on-disk registry/config structures used by session
//! lifecycle code to recover, reuse, and supervise per-user compositor stacks.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

use crate::desktop::compositor::runtime::{MANAGED_COMPOSITOR_COMPONENT, MANAGED_WAYLAND_SOCKET};

use super::{
    DEFAULT_HEIGHT, DEFAULT_WIDTH, STATE_VERSION, default_cleanup_globs, default_cleanup_paths,
    default_components, default_environment, default_idle_timeout_ms, default_log_dir,
    default_session_name, default_size, default_start_timeout_ms, default_state_dir,
    default_stop_timeout_ms, default_true, default_user, default_xdg_runtime_dir, gid_for_user,
    uid_for_user,
};

/// Requested client geometry tracked by sesman.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSize {
    pub width: u32,
    pub height: u32,
}

impl Default for SessionSize {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }
}

/// Client metadata persisted for reconnect decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    pub peer: String,
    pub connected_at: DateTime<Utc>,
    pub requested_size: Option<SessionSize>,
}

/// A managed process component: wrdp-compositor or site-specific extras.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub log_path: Option<PathBuf>,
    #[serde(default)]
    pub readiness: ReadinessCheck,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub startup_delay_ms: u64,
}

/// Readiness signal for a component.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReadinessCheck {
    /// Process is considered ready once it is still alive after spawn.
    #[default]
    ProcessAlive,
    /// Wait for a Unix-domain Wayland socket path.
    UnixSocket { path: PathBuf },
    /// Do not wait for this component.
    None,
}

/// Sesman runtime configuration model loaded from defaults or INI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SesmanConfig {
    #[serde(default = "default_session_name")]
    pub session_name: String,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default = "default_xdg_runtime_dir")]
    pub xdg_runtime_dir: PathBuf,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,
    #[serde(default = "default_size")]
    pub default_size: SessionSize,
    #[serde(default = "default_start_timeout_ms")]
    pub start_timeout_ms: u64,
    #[serde(default = "default_stop_timeout_ms")]
    pub stop_timeout_ms: u64,
    /// How long to keep a disconnected healthy display stack alive.
    /// 0 disables automatic idle cleanup.
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub cleanup_paths: Vec<PathBuf>,
    #[serde(default)]
    pub cleanup_globs: Vec<String>,
    #[serde(default = "default_components")]
    pub components: Vec<ComponentConfig>,
}

impl Default for SesmanConfig {
    fn default() -> Self {
        Self {
            session_name: default_session_name(),
            user: default_user(),
            xdg_runtime_dir: default_xdg_runtime_dir(),
            state_dir: default_state_dir(),
            log_dir: default_log_dir(),
            default_size: default_size(),
            start_timeout_ms: default_start_timeout_ms(),
            stop_timeout_ms: default_stop_timeout_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
            environment: default_environment(),
            cleanup_paths: default_cleanup_paths(),
            cleanup_globs: default_cleanup_globs(),
            components: default_components(),
        }
    }
}

impl SesmanConfig {
    /// Build production per-user defaults for the authenticated account.
    ///
    /// This layout is used by the single public RDP daemon after PAM/static
    /// authentication. It separates a root-owned lifecycle registry from the
    /// user's runtime tree:
    ///
    /// * `xdg_runtime_dir` = `/run/user/<uid>/wrdp` (user-owned runtime, sockets)
    /// * `state_dir` = `/run/wrdp/sesman/<uid>` (root-owned lifecycle registry)
    /// * `log_dir` = `/run/user/<uid>/wrdp/logs` (user-owned component logs)
    ///
    /// Keeping the registry out of the user runtime tree stops the target user
    /// from tampering with the PID/identity records sesman signals against. It
    /// does not include an RDP listener socket.
    pub fn for_user(user: &str) -> Result<Self> {
        let uid = uid_for_user(user)?;
        let runtime = PathBuf::from(format!("/run/user/{uid}/wrdp"));
        let mut config = Self::default();
        config.user = user.to_string();
        config.xdg_runtime_dir = runtime.clone();
        // Root-owned lifecycle registry, outside the user-writable runtime tree.
        config.state_dir = PathBuf::from(format!("/run/wrdp/sesman/{uid}"));
        config.log_dir = runtime.join("logs");
        config.cleanup_paths = vec![
            runtime.join(MANAGED_WAYLAND_SOCKET),
            runtime.join("wayland-0.lock"),
        ];
        config.cleanup_globs = Vec::new();

        for component in &mut config.components {
            component.log_path = Some(config.log_dir.join(format!("{}.log", component.name)));
            if component.name == MANAGED_COMPOSITOR_COMPONENT {
                component.readiness = ReadinessCheck::UnixSocket {
                    path: runtime.join(MANAGED_WAYLAND_SOCKET),
                };
            }
        }

        config.validate()?;
        Ok(config)
    }

    /// Load sesman INI config, or return managed compositor defaults if absent.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };

        match fs::read_to_string(path) {
            Ok(content) => {
                let mut config = parse_sesman_ini(&content)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                config.apply_runtime_defaults();
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Generate a sesman-style INI config with default values.
    pub fn generate_default_ini() -> String {
        render_sesman_ini(&Self::default())
    }

    pub(super) fn apply_runtime_defaults(&mut self) {
        if self.environment.is_empty() {
            self.environment = default_environment();
        }
        if self.cleanup_paths.is_empty() {
            self.cleanup_paths = default_cleanup_paths();
        }
        if self.cleanup_globs.is_empty() {
            self.cleanup_globs = default_cleanup_globs();
        }
        if self.components.is_empty() {
            self.components = default_components();
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.session_name.is_empty()
            || self.session_name == "."
            || self.session_name == ".."
            || !self
                .session_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            bail!("session_name must be a single safe filename component");
        }
        if self.user.trim().is_empty() {
            bail!("user could not be resolved; set user in wrdp-sesman.ini");
        }
        crate::auth::validate_username(&self.user).context("invalid sesman user")?;
        if !self.xdg_runtime_dir.is_absolute()
            || !self.state_dir.is_absolute()
            || !self.log_dir.is_absolute()
        {
            bail!("sesman runtime, state, and log paths must be absolute");
        }
        if self.components.is_empty() {
            bail!("at least one component is required");
        }
        let mut component_names = std::collections::BTreeSet::new();
        for component in &self.components {
            if component.name.trim().is_empty() {
                bail!("component name cannot be empty");
            }
            if !component_names.insert(component.name.as_str()) {
                bail!("duplicate component name: {}", component.name);
            }
            if component.command.trim().is_empty() {
                bail!("component {} has empty command", component.name);
            }
        }
        Ok(())
    }

    pub(super) fn state_path(&self) -> PathBuf {
        self.state_dir
            .join(format!("{}.state.json", self.session_name))
    }

    pub(super) fn lock_path(&self) -> PathBuf {
        self.state_dir.join(format!("{}.lock", self.session_name))
    }
}

/// PID record for one managed session component.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentState {
    pub name: String,
    pub pid: i32,
    pub command: Vec<String>,
    pub started_at: DateTime<Utc>,
    /// Exact kernel start-time ticks for PID-reuse-safe identity. `None` keeps
    /// registry files written by older versions readable.
    #[serde(default)]
    pub start_ticks: Option<u64>,
    /// Kernel boot ID (`/proc/sys/kernel/random/boot_id`) captured at spawn.
    /// A mismatch means the host rebooted, so any PID match is coincidental.
    /// `None` for legacy entries; such entries fail closed for signalling.
    #[serde(default)]
    pub boot_id: Option<String>,
    /// Real UID observed via `/proc/<pid>/status` at spawn. Guards against a
    /// reused PID now owned by another account. `None` for legacy entries.
    #[serde(default)]
    pub uid: Option<u32>,
    /// Process-group id observed via `/proc/<pid>/stat` at spawn, used to
    /// authenticate group-wide teardown. `None` for legacy entries.
    #[serde(default)]
    pub pgid: Option<i32>,
    pub required: bool,
    /// New managed components are process-group leaders so teardown also stops
    /// children they spawn. Older registry entries default to PID-only signals.
    #[serde(default)]
    pub process_group: bool,
}

/// Persistent session registry entry written to `*.state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    pub id: Uuid,
    pub name: String,
    pub user: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub xdg_runtime_dir: PathBuf,
    /// Kernel boot ID recorded when the session registry was created. Lets
    /// callers detect a host reboot before trusting any persisted PID identity.
    /// `None` for legacy registry files.
    #[serde(default)]
    pub boot_id: Option<String>,
    /// Resolved UID of the owning account at session creation. `None` for legacy
    /// registry files.
    #[serde(default)]
    pub uid: Option<u32>,
    /// Resolved primary GID of the owning account at session creation. `None`
    /// for legacy registry files.
    #[serde(default)]
    pub gid: Option<u32>,
    pub default_size: SessionSize,
    pub requested_size: Option<SessionSize>,
    pub last_client: Option<ClientInfo>,
    #[serde(default)]
    pub active_clients: u32,
    #[serde(default)]
    pub last_disconnected_at: Option<DateTime<Utc>>,
    /// Absolute instant after which an idle (no active clients) session is
    /// eligible for automatic cleanup. Set when the last client disconnects and
    /// cleared on bind. `None` disables idle recovery for this session (for
    /// example when the configured idle timeout is zero). Legacy registry files
    /// default to `None`; idle recovery then derives a deadline from
    /// `last_disconnected_at` and the configured idle timeout.
    #[serde(default)]
    pub idle_deadline_at: Option<DateTime<Utc>>,
    pub components: Vec<ComponentState>,
}

impl SessionState {
    pub(super) fn new(config: &SesmanConfig) -> Self {
        let now = Utc::now();
        Self {
            version: STATE_VERSION,
            id: Uuid::new_v4(),
            name: config.session_name.clone(),
            user: config.user.clone(),
            created_at: now,
            updated_at: now,
            xdg_runtime_dir: config.xdg_runtime_dir.clone(),
            // Capture boot ID and resolved account ids so later health/signal
            // checks can fail closed on reboot or account changes. Identity
            // resolution is best-effort here; callers persist per-component
            // identity captured at spawn for authoritative signalling.
            boot_id: super::process::read_boot_id(),
            uid: uid_for_user(&config.user).ok(),
            gid: gid_for_user(&config.user).ok(),
            default_size: config.default_size,
            requested_size: None,
            last_client: None,
            active_clients: 0,
            last_disconnected_at: None,
            idle_deadline_at: None,
            components: Vec::new(),
        }
    }

    pub(super) fn mark_updated(&mut self) {
        self.updated_at = Utc::now();
    }
}

/// Health classification for the persisted session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionHealth {
    Missing,
    Healthy,
    Degraded,
    Dead,
}

/// Session status snapshot returned to runtime callers/automation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub health: SessionHealth,
    pub state_path: PathBuf,
    pub state: Option<SessionState>,
    pub dead_components: Vec<String>,
}

/// Result of an ensure/start operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsureResult {
    pub reused_existing: bool,
    pub status: SessionStatus,
}

/// Options passed when an RDP client asks for a session.
#[derive(Debug, Clone, Default)]
pub struct EnsureOptions {
    pub force_restart: bool,
    pub requested_size: Option<SessionSize>,
    pub client_peer: Option<String>,
    /// Mark this ensure as an active RDP client attachment.
    pub client_connected: bool,
}
fn parse_sesman_ini(content: &str) -> Result<SesmanConfig> {
    let mut config = SesmanConfig::default();
    let mut section = String::from("globals");
    let mut components: BTreeMap<String, ComponentConfig> = BTreeMap::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let line_number = idx + 1;
        let line = strip_ini_comment(raw_line).trim().to_string();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_lowercase();
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            bail!("invalid sesman config line {line_number}: expected key=value");
        };
        let key = key.trim().to_lowercase();
        let value = unquote_ini_scalar(value.trim());

        match section.as_str() {
            "globals" => apply_sesman_global(&mut config, &key, &value, line_number)?,
            "default_size" => match key.as_str() {
                "width" => config.default_size.width = parse_ini_value(&value, line_number, &key)?,
                "height" => {
                    config.default_size.height = parse_ini_value(&value, line_number, &key)?
                }
                _ => bail!("unknown default_size key '{key}' at line {line_number}"),
            },
            "environment" => {
                config.environment.insert(key.to_uppercase(), value);
            }
            "cleanup" => match key.as_str() {
                "paths" => {
                    config.cleanup_paths = parse_ini_list(&value)
                        .into_iter()
                        .map(PathBuf::from)
                        .collect();
                }
                "globs" => config.cleanup_globs = parse_ini_list(&value),
                _ => bail!("unknown cleanup key '{key}' at line {line_number}"),
            },
            section if section.starts_with("component:") => {
                let name = section.trim_start_matches("component:").to_string();
                let component = components
                    .entry(name.clone())
                    .or_insert_with(|| empty_component(name));
                apply_component_key(component, &key, &value, line_number)?;
            }
            section if section.starts_with("component_env:") => {
                let name = section.trim_start_matches("component_env:").to_string();
                let component = components
                    .entry(name.clone())
                    .or_insert_with(|| empty_component(name));
                component.env.insert(key.to_uppercase(), value);
            }
            _ => bail!("unknown sesman config section '[{section}]' at line {line_number}"),
        }
    }

    if !components.is_empty() {
        config.components = components.into_values().collect();
    }

    Ok(config)
}

fn empty_component(name: String) -> ComponentConfig {
    ComponentConfig {
        name,
        command: String::new(),
        args: Vec::new(),
        env: BTreeMap::new(),
        working_dir: None,
        log_path: None,
        readiness: ReadinessCheck::ProcessAlive,
        required: true,
        startup_delay_ms: 0,
    }
}

fn apply_sesman_global(
    config: &mut SesmanConfig,
    key: &str,
    value: &str,
    line_number: usize,
) -> Result<()> {
    match key {
        "session_name" => config.session_name = value.to_string(),
        "user" => config.user = value.to_string(),
        "xdg_runtime_dir" => config.xdg_runtime_dir = PathBuf::from(value),
        "state_dir" => config.state_dir = PathBuf::from(value),
        "log_dir" => config.log_dir = PathBuf::from(value),
        "start_timeout_ms" => config.start_timeout_ms = parse_ini_value(value, line_number, key)?,
        "stop_timeout_ms" => config.stop_timeout_ms = parse_ini_value(value, line_number, key)?,
        "idle_timeout_ms" => config.idle_timeout_ms = parse_ini_value(value, line_number, key)?,
        _ => bail!("unknown globals key '{key}' at line {line_number}"),
    }
    Ok(())
}

fn apply_component_key(
    component: &mut ComponentConfig,
    key: &str,
    value: &str,
    line_number: usize,
) -> Result<()> {
    match key {
        "command" => component.command = value.to_string(),
        "args" => component.args = parse_ini_list(value),
        "working_dir" => component.working_dir = nonempty_path(value),
        "log_path" => component.log_path = nonempty_path(value),
        "readiness" => {
            component.readiness = match value {
                "process_alive" => ReadinessCheck::ProcessAlive,
                "none" => ReadinessCheck::None,
                "unix_socket" => ReadinessCheck::UnixSocket {
                    path: PathBuf::new(),
                },
                _ => bail!("invalid readiness '{value}' at line {line_number}"),
            };
        }
        "readiness_path" => {
            component.readiness = ReadinessCheck::UnixSocket {
                path: PathBuf::from(value),
            };
        }
        "required" => component.required = parse_ini_value(value, line_number, key)?,
        "startup_delay_ms" => {
            component.startup_delay_ms = parse_ini_value(value, line_number, key)?
        }
        _ => bail!("unknown component key '{key}' at line {line_number}"),
    }
    Ok(())
}

fn render_sesman_ini(config: &SesmanConfig) -> String {
    let mut out = String::new();
    out.push_str("[globals]\n");
    out.push_str(&format!("session_name={}\n", config.session_name));
    out.push_str(&format!("user={}\n", config.user));
    out.push_str(&format!(
        "xdg_runtime_dir={}\n",
        config.xdg_runtime_dir.display()
    ));
    out.push_str(&format!("state_dir={}\n", config.state_dir.display()));
    out.push_str(&format!("log_dir={}\n", config.log_dir.display()));
    out.push_str(&format!("start_timeout_ms={}\n", config.start_timeout_ms));
    out.push_str(&format!("stop_timeout_ms={}\n", config.stop_timeout_ms));
    out.push_str(&format!("idle_timeout_ms={}\n\n", config.idle_timeout_ms));

    out.push_str("[default_size]\n");
    out.push_str(&format!("width={}\n", config.default_size.width));
    out.push_str(&format!("height={}\n\n", config.default_size.height));

    out.push_str("[environment]\n");
    for (key, value) in &config.environment {
        out.push_str(&format!("{key}={}\n", format_ini_scalar(value)));
    }
    out.push('\n');

    out.push_str("[cleanup]\n");
    out.push_str(&format!(
        "paths={}\n",
        format_ini_list(config.cleanup_paths.iter().map(|p| p.display().to_string()))
    ));
    out.push_str(&format!(
        "globs={}\n\n",
        format_ini_list(config.cleanup_globs.iter().cloned())
    ));

    for component in &config.components {
        out.push_str(&format!("[component:{}]\n", component.name));
        out.push_str(&format!("command={}\n", component.command));
        out.push_str(&format!(
            "args={}\n",
            format_ini_list(component.args.iter().cloned())
        ));
        if let Some(path) = &component.working_dir {
            out.push_str(&format!("working_dir={}\n", path.display()));
        }
        if let Some(path) = &component.log_path {
            out.push_str(&format!("log_path={}\n", path.display()));
        }
        match &component.readiness {
            ReadinessCheck::ProcessAlive => out.push_str("readiness=process_alive\n"),
            ReadinessCheck::None => out.push_str("readiness=none\n"),
            ReadinessCheck::UnixSocket { path } => {
                out.push_str("readiness=unix_socket\n");
                out.push_str(&format!("readiness_path={}\n", path.display()));
            }
        }
        out.push_str(&format!("required={}\n", component.required));
        out.push_str(&format!(
            "startup_delay_ms={}\n\n",
            component.startup_delay_ms
        ));

        if !component.env.is_empty() {
            out.push_str(&format!("[component_env:{}]\n", component.name));
            for (key, value) in &component.env {
                out.push_str(&format!("{key}={}\n", format_ini_scalar(value)));
            }
            out.push('\n');
        }
    }

    out
}

fn strip_ini_comment(line: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' | ';' if !in_single && !in_double => {
                if idx == 0 || line[..idx].ends_with(char::is_whitespace) {
                    return line[..idx].to_string();
                }
            }
            _ => {}
        }
    }
    line.to_string()
}

fn unquote_ini_scalar(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
        .to_string()
}

fn parse_ini_value<T>(value: &str, line_number: usize, key: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|e| anyhow::anyhow!("invalid value for '{key}' at line {line_number}: {e}"))
}

fn parse_ini_list(value: &str) -> Vec<String> {
    let inner = value
        .trim()
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(unquote_ini_scalar)
        .collect()
}

fn nonempty_path(value: &str) -> Option<PathBuf> {
    if value.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn format_ini_list(values: impl IntoIterator<Item = String>) -> String {
    format!(
        "[{}]",
        values
            .into_iter()
            .map(|value| format_ini_scalar(&value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn format_ini_scalar(value: &str) -> String {
    if value.is_empty()
        || value.contains(',')
        || value.contains('#')
        || value.contains(';')
        || value.chars().any(char::is_whitespace)
    {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod path_validation_tests {
    use super::*;

    #[test]
    fn session_name_cannot_escape_registry_directory() {
        for name in [
            "",
            ".",
            "..",
            "../other",
            "/tmp/other",
            "a/b",
            "a\\b",
            "a\n",
        ] {
            let config = SesmanConfig {
                session_name: name.into(),
                ..SesmanConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "accepted unsafe session name {name:?}"
            );
        }
    }

    #[test]
    fn ordinary_session_names_remain_supported() {
        for name in ["wrdp", "session-1", "user.desktop_2"] {
            let config = SesmanConfig {
                session_name: name.into(),
                ..SesmanConfig::default()
            };
            assert_eq!(
                config.state_path().parent(),
                Some(config.state_dir.as_path())
            );
            // Default account may be unresolved in an isolated build; the name
            // check must not be the reason validation fails.
            if let Err(error) = config.validate() {
                assert!(!error.to_string().contains("session_name"));
            }
        }
    }
}
