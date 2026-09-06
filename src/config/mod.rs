//! Configuration loading, merging, and validation.
//!
//! Runtime policy enters the server through INI files, environment variables,
//! and CLI arguments. Keeping merge order and validation in one module prevents
//! transport/session/channel code from inventing local defaults that drift from
//! operator-facing configuration.
//!
//! Defaults here should document the protocol or deployment reason for the
//! value. Avoid adding new knobs speculatively; prefer a concrete caller and a
//! validation rule before expanding the configuration surface.
#![expect(unsafe_code, reason = "getuid() for root detection")]

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Check if running inside a Flatpak sandbox
pub fn is_flatpak() -> bool {
    // Check for FLATPAK_ID env var (set by Flatpak runtime)
    std::env::var("FLATPAK_ID").is_ok()
        // Also check for /.flatpak-info which exists in all Flatpak sandboxes
        || std::path::Path::new("/.flatpak-info").exists()
}

pub fn get_cert_config_dir() -> PathBuf {
    if is_flatpak() {
        // Flatpak: use XDG paths which are mapped to ~/.var/app/<app-id>/
        if let Some(config_dir) = dirs::config_dir() {
            return config_dir;
        }
        // Fallback for Flatpak (shouldn't happen but be safe)
        PathBuf::from("/app/config")
    } else {
        // Native: prefer user config if not root, otherwise /etc/
        let uid = unsafe { libc::getuid() };
        if uid == 0 {
            // Running as root - use system directory
            PathBuf::from("/etc/wrdp")
        } else {
            // Running as user - use XDG config
            dirs::config_dir().map_or_else(|| PathBuf::from("/etc/wrdp"), |d| d.join("wrdp"))
        }
    }
}

/// Resolve log directory, enforcing sandbox containment in Flatpak.
///
/// In Flatpak mode the configured log_dir is ignored — logs always go to
/// the sandbox data directory. In native mode the configured path is used,
/// falling back to XDG_DATA_HOME/wrdp/logs.
pub fn resolve_log_dir(configured: &Option<PathBuf>) -> PathBuf {
    if is_flatpak() {
        // Sandbox: XDG_DATA_HOME is ~/.var/app/<app-id>/data in Flatpak
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("/app/data"))
            .join("logs")
    } else {
        configured.clone().unwrap_or_else(|| {
            dirs::data_dir().map_or_else(|| PathBuf::from("/tmp/wrdp"), |d| d.join("wrdp/logs"))
        })
    }
}

pub fn default_cert_path() -> PathBuf {
    get_cert_config_dir().join("cert.pem")
}

pub fn default_key_path() -> PathBuf {
    get_cert_config_dir().join("key.pem")
}

mod portal_startup;
pub mod types;

pub use portal_startup::PortalStartupSettings;

// Use types from types.rs
// Re-export types needed by other modules
pub use types::HardwareEncodingConfig;
use types::{
    CaptureProtocolConfig, ClipboardConfig, DamageTrackingConfig, DisplayConfig, EgfxConfig,
    InputConfig, LoggingConfig, PerformanceConfig, SecurityConfig, ServerConfig, VideoConfig,
};

/// Current config file version. Bumped when breaking changes require migration.
const CURRENT_CONFIG_VERSION: u32 = 1;

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Config file format version (for migration support)
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    /// Server configuration
    #[serde(default)]
    pub server: ServerConfig,
    /// Security configuration
    #[serde(default)]
    pub security: SecurityConfig,
    /// Video configuration
    #[serde(default)]
    pub video: VideoConfig,
    /// Capture protocol configuration (portal-generic backend)
    #[serde(default)]
    pub capture: CaptureProtocolConfig,
    /// Input configuration
    #[serde(default)]
    pub input: InputConfig,
    /// Clipboard configuration
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    /// Performance configuration
    #[serde(default)]
    pub performance: PerformanceConfig,
    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,
    /// EGFX configuration
    #[serde(default)]
    pub egfx: EgfxConfig,
    /// Damage tracking configuration
    #[serde(default)]
    pub damage_tracking: DamageTrackingConfig,
    /// Hardware encoding configuration
    #[serde(default)]
    pub hardware_encoding: HardwareEncodingConfig,
    /// Display control configuration
    #[serde(default)]
    pub display: DisplayConfig,
}

fn default_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
}

impl Config {
    /// Load INI configuration, apply documented environment overrides, and validate.
    pub fn load(path: &str) -> Result<Self> {
        let table = match std::fs::read_to_string(path) {
            Ok(text) => parse_ini_config(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        let mut config: Self = toml::Value::Table(table)
            .try_into()
            .context("deserialize configuration")?;
        apply_environment(&mut config)?;
        config.config_version = CURRENT_CONFIG_VERSION;
        config.validate()?;
        Ok(config)
    }

    /// Create default configuration
    pub fn default_config() -> Result<Self> {
        Ok(Config::default())
    }

    /// Generate an INI-style config file with all defaults.
    pub fn generate_default_ini() -> Result<String> {
        let config = Config::default();
        let value = toml::Value::try_from(&config).context("Failed to serialize default config")?;
        let table = value
            .as_table()
            .context("Default config did not serialize to a table")?;
        Ok(render_ini_config(table))
    }

    /// Reject configuration that cannot be applied safely at startup.
    pub fn validate(&self) -> Result<()> {
        self.server
            .listen_addr
            .parse::<SocketAddr>()
            .context("Invalid listen address")?;
        if self.server.max_connections != 1 {
            anyhow::bail!(
                "server.max_connections must be 1 because production wrdp uses a serial single-client model (got {})",
                self.server.max_connections
            );
        }
        require_file(&self.security.cert_path, "Certificate")?;
        require_file(&self.security.key_path, "Private key")?;
        require_choice(
            "auth_method",
            &self.security.auth_method,
            &["pam", "password"],
        )?;
        require_choice(
            "cursor mode",
            &self.video.cursor_mode,
            &["embedded", "metadata", "hidden"],
        )?;
        require_choice(
            "ZGFX compression mode",
            &self.egfx.zgfx_compression,
            &["never", "auto", "always"],
        )?;
        require_choice(
            "EGFX codec",
            &self.egfx.codec,
            &["avc420", "avc444", "auto", "bitmap"],
        )?;
        require_choice(
            "damage tracking method",
            &self.damage_tracking.method,
            &["pipewire", "diff", "hybrid"],
        )?;
        require_choice(
            "quality preset",
            &self.hardware_encoding.quality_preset,
            &["speed", "balanced", "quality"],
        )?;

        if self.security.auth_method == "password" {
            if self.security.password_credentials.is_empty() {
                anyhow::bail!("auth_method=password requires security.password_credentials");
            }
            for (username, encoded) in &self.security.password_credentials {
                crate::security::validate_username(username).with_context(|| {
                    format!("Invalid password credential username '{username}'")
                })?;
                let hash = argon2::password_hash::PasswordHash::new(encoded).map_err(|error| {
                    anyhow::anyhow!("Invalid password hash for user '{username}': {error}")
                })?;
                if hash.algorithm.as_str() != "argon2id" {
                    anyhow::bail!("Password hash for user '{username}' must use Argon2id");
                }
            }
        }

        if !(self.egfx.qp_min..=self.egfx.qp_max).contains(&self.egfx.qp_default) {
            anyhow::bail!(
                "EGFX QP must satisfy min ({}) <= default ({}) <= max ({})",
                self.egfx.qp_min,
                self.egfx.qp_default,
                self.egfx.qp_max
            );
        }
        Ok(())
    }

    /// Override config with CLI arguments
    pub fn with_overrides(mut self, listen: Option<String>, port: Option<u16>) -> Self {
        if let Some(listen_addr) = listen {
            let port = port.unwrap_or_else(|| {
                self.server
                    .listen_addr
                    .parse::<SocketAddr>()
                    .map_or(3389, |addr| addr.port())
            });
            self.server.listen_addr = format!("{listen_addr}:{port}");
        } else if let Some(port) = port
            && let Ok(mut addr) = self.server.listen_addr.parse::<SocketAddr>()
        {
            addr.set_port(port);
            self.server.listen_addr = addr.to_string();
        }

        self
    }
}

fn require_file(path: &std::path::Path, label: &str) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        anyhow::bail!("{label} not found: {}", path.display())
    }
}

fn require_choice(label: &str, value: &str, accepted: &[&str]) -> Result<()> {
    if accepted.contains(&value) {
        Ok(())
    } else {
        anyhow::bail!(
            "Invalid {label}: {value} (expected {})",
            accepted.join(", ")
        )
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CURRENT_CONFIG_VERSION,
            server: ServerConfig::default(),
            security: SecurityConfig::default(),
            video: VideoConfig::default(),
            capture: CaptureProtocolConfig::default(),
            input: InputConfig::default(),
            clipboard: ClipboardConfig::default(),
            performance: PerformanceConfig::default(),
            logging: LoggingConfig::default(),
            egfx: EgfxConfig::default(),
            damage_tracking: DamageTrackingConfig::default(),
            hardware_encoding: HardwareEncodingConfig::default(),
            display: DisplayConfig::default(),
        }
    }
}

/// Parse an INI-style wrdp config file into the internal config table.
///
/// The file format intentionally follows xrdp/sesman-style conventions:
/// section headers (`[server]`), `key=value` assignments, and `#`/`;` comments.
/// Nested structs use dotted section names such as `[performance.adaptive_fps]`.
/// `[globals]` maps to top-level config keys like `config_version`.
fn apply_environment(config: &mut Config) -> Result<()> {
    if let Ok(value) = std::env::var("WRDP_SERVER__LISTEN_ADDR") {
        config.server.listen_addr = value;
    }
    if let Ok(value) = std::env::var("WRDP_EGFX__ENABLED") {
        config.egfx.enabled = value
            .parse()
            .context("WRDP_EGFX__ENABLED must be boolean")?;
    }
    if let Ok(value) = std::env::var("WRDP_VIDEO__TARGET_FPS") {
        config.video.target_fps = value
            .parse()
            .context("WRDP_VIDEO__TARGET_FPS must be an integer")?;
    }
    Ok(())
}

fn parse_ini_config(content: &str) -> Result<toml::Table> {
    let mut parser = configparser::ini::Ini::new();
    let sections = parser
        .read(content.to_string())
        .map_err(|error| anyhow::anyhow!("invalid INI: {error}"))?;
    let mut root = toml::Table::new();
    for (section, fields) in sections {
        let path =
            if section.eq_ignore_ascii_case("default") || section.eq_ignore_ascii_case("globals") {
                Vec::new()
            } else {
                section.split('.').map(str::to_ascii_lowercase).collect()
            };
        let target = nested_table(&mut root, &path)?;
        for (key, value) in fields {
            if let Some(value) = value {
                target.insert(key.to_ascii_lowercase(), infer_config_value(&value));
            }
        }
    }
    Ok(root)
}

fn nested_table<'a>(root: &'a mut toml::Table, path: &[String]) -> Result<&'a mut toml::Table> {
    let mut table = root;
    for part in path {
        table = table
            .entry(part)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .with_context(|| format!("configuration section {part} conflicts with a value"))?;
    }
    Ok(table)
}

fn infer_config_value(s: &str) -> toml::Value {
    let s = s.trim();
    if s == "[]" {
        return toml::Value::Array(Vec::new());
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        let values = inner
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(infer_config_value)
            .collect();
        return toml::Value::Array(values);
    }
    if let Some(unquoted) = unquote_ini_value(s) {
        return toml::Value::String(unquoted);
    }
    if let Ok(b) = s.parse::<bool>() {
        return toml::Value::Boolean(b);
    }
    if let Ok(i) = s.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return toml::Value::Float(f);
    }
    toml::Value::String(s.to_string())
}

fn unquote_ini_value(s: &str) -> Option<String> {
    if s.len() < 2 {
        return None;
    }
    if let Some(inner) = s.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        return Some(
            inner
                .replace(r"\\", r"\")
                .replace(r#"\""#, "\"")
                .replace(r"\n", "\n"),
        );
    }
    s.strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .map(str::to_string)
}

fn render_ini_config(table: &toml::Table) -> String {
    let mut out = String::new();

    if let Some(value) = table.get("config_version") {
        out.push_str("[globals]\n");
        out.push_str("config_version=");
        out.push_str(&format_ini_value(value));
        out.push_str("\n\n");
    }

    for (key, value) in table {
        if key == "config_version" {
            continue;
        }
        render_ini_value(&mut out, key, value);
    }

    out
}

fn render_ini_value(out: &mut String, section: &str, value: &toml::Value) {
    match value {
        toml::Value::Table(table) => {
            let scalar_entries: Vec<_> = table
                .iter()
                .filter(|(_, value)| !matches!(value, toml::Value::Table(_)))
                .collect();
            if !scalar_entries.is_empty() {
                out.push('[');
                out.push_str(section);
                out.push_str("]\n");
                for (key, value) in scalar_entries {
                    out.push_str(key);
                    out.push('=');
                    out.push_str(&format_ini_value(value));
                    out.push('\n');
                }
                out.push('\n');
            }

            for (key, nested) in table
                .iter()
                .filter(|(_, value)| matches!(value, toml::Value::Table(_)))
            {
                render_ini_value(out, &format!("{section}.{key}"), nested);
            }
        }
        scalar => {
            out.push('[');
            out.push_str(section);
            out.push_str("]\nvalue=");
            out.push_str(&format_ini_value(scalar));
            out.push_str("\n\n");
        }
    }
}

fn format_ini_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => {
            let s = f.to_string();
            if s.contains('.') { s } else { format!("{s}.0") }
        }
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Array(values) => {
            let inner = values
                .iter()
                .map(format_ini_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        }
        toml::Value::Table(_) => String::new(),
        toml::Value::Datetime(dt) => dt.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default_config().unwrap();
        assert_eq!(config.server.listen_addr, "0.0.0.0:3389");
        assert_eq!(config.server.max_connections, 1);
        assert!(config.server.use_portals);
        assert_eq!(config.video.target_fps, 30);
    }

    #[test]
    fn test_config_validation_invalid_address() {
        let mut config = Config::default_config().unwrap();
        config.server.listen_addr = "invalid".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn config_validation_rejects_non_serial_connection_limit() {
        let mut config = Config::default_config().unwrap();
        config.server.max_connections = 2;

        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("server.max_connections must be 1"),
            "{error}"
        );
        assert!(error.contains("serial single-client"), "{error}");
    }

    #[test]
    fn test_config_validation_invalid_cursor_mode() {
        let mut config = Config::default_config().unwrap();
        config.video.cursor_mode = "invalid_mode".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_password_auth_requires_static_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, "test cert").unwrap();
        std::fs::write(&key_path, "test key").unwrap();

        let mut config = Config::default_config().unwrap();
        config.security.cert_path = cert_path;
        config.security.key_path = key_path;
        config.security.auth_method = "password".to_string();

        assert!(config.validate().is_err());

        config.security.password_credentials.insert(
            "rdpuser".to_string(),
            crate::security::hash_static_password("secret").unwrap(),
        );

        assert!(config.validate().is_ok());
    }

    #[test]
    fn password_auth_rejects_non_argon2id_phc_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, "test cert").unwrap();
        std::fs::write(&key_path, "test key").unwrap();

        let mut config = Config::default_config().unwrap();
        config.security.cert_path = cert_path;
        config.security.key_path = key_path;
        config.security.auth_method = "password".to_string();
        let argon2i = crate::security::hash_static_password("secret")
            .unwrap()
            .replacen("$argon2id$", "$argon2i$", 1);
        config
            .security
            .password_credentials
            .insert("alice".to_string(), argon2i);

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("must use Argon2id"), "{error}");
    }

    #[test]
    fn test_password_auth_accepts_multiple_static_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, "test cert").unwrap();
        std::fs::write(&key_path, "test key").unwrap();

        let mut config = Config::default_config().unwrap();
        config.security.cert_path = cert_path;
        config.security.key_path = key_path;
        config.security.auth_method = "password".to_string();
        config.security.password_credentials.insert(
            "alice".to_string(),
            crate::security::hash_static_password("alice-secret").unwrap(),
        );
        config.security.password_credentials.insert(
            "bob".to_string(),
            crate::security::hash_static_password("bob-secret").unwrap(),
        );

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_with_overrides_without_cli_port_preserves_listen_addr() {
        let config = Config::default_config().unwrap();
        let config = config.with_overrides(None, None);

        assert_eq!(config.server.listen_addr, "0.0.0.0:3389");
    }

    #[test]
    fn test_with_overrides_without_cli_port_preserves_ipv6_listen_addr() {
        let mut config = Config::default_config().unwrap();
        config.server.listen_addr = "[::1]:3389".to_string();

        let config = config.with_overrides(None, None);

        assert_eq!(config.server.listen_addr, "[::1]:3389");
    }

    #[test]
    fn test_with_overrides_cli_port_updates_existing_host_only() {
        let config = Config::default_config().unwrap();
        let config = config.with_overrides(None, Some(3389));

        assert_eq!(config.server.listen_addr, "0.0.0.0:3389");
    }

    #[test]
    fn test_infer_config_value_bool() {
        assert_eq!(infer_config_value("true"), toml::Value::Boolean(true));
        assert_eq!(infer_config_value("false"), toml::Value::Boolean(false));
    }

    #[test]
    fn test_infer_config_value_integer() {
        assert_eq!(infer_config_value("42"), toml::Value::Integer(42));
        assert_eq!(infer_config_value("0"), toml::Value::Integer(0));
        assert_eq!(infer_config_value("-1"), toml::Value::Integer(-1));
    }

    #[test]
    fn test_infer_config_value_float() {
        assert_eq!(infer_config_value("3.14"), toml::Value::Float(3.14));
    }

    #[test]
    fn test_infer_config_value_string() {
        assert_eq!(
            infer_config_value("hello"),
            toml::Value::String("hello".into())
        );
        assert_eq!(
            infer_config_value("0.0.0.0:3389"),
            toml::Value::String("0.0.0.0:3389".into())
        );
    }

    #[test]
    fn test_infer_config_value_ordering() {
        // "0" parses as integer, not bool or string
        assert_eq!(infer_config_value("0"), toml::Value::Integer(0));
        // "1" parses as integer, not bool
        assert_eq!(infer_config_value("1"), toml::Value::Integer(1));
        // "1.0" parses as float, not string
        assert_eq!(infer_config_value("1.0"), toml::Value::Float(1.0));
        // IP:port stays string (colon prevents numeric parse)
        assert!(matches!(
            infer_config_value("127.0.0.1:3389"),
            toml::Value::String(_)
        ));
    }
}
