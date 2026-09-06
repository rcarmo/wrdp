//! Settings D-Bus interface implementation.
//!
//! Implements the runtime settings boundary used to expose appearance
//! preferences to portal clients.
//!
//! # Configuration
//!
//! Settings are read from environment variables with local defaults:
//!
//! - `XDP_GENERIC_COLOR_SCHEME` — `0` (default), `1` (dark), `2` (light)
//! - `XDP_GENERIC_ACCENT_COLOR` — `r,g,b` as floats (e.g., `0.21,0.52,0.89`)
//! - `XDP_GENERIC_CONTRAST` — `0` (default), `1` (high)
//! - `XDP_GENERIC_REDUCED_MOTION` — `0` (normal), `1` (reduced)
//! - Falls back to detecting dark mode from `GTK_THEME` (if it contains "dark")

use std::collections::HashMap;

use zbus::{
    interface,
    object_server::SignalEmitter,
    zvariant::{OwnedValue, Value},
};

/// The freedesktop appearance namespace.
const NS_APPEARANCE: &str = "org.freedesktop.appearance";

/// Settings portal interface implementation.
pub struct SettingsInterface {
    /// Cached settings organized by namespace → key → value.
    settings: HashMap<String, HashMap<String, OwnedValue>>,
}

impl Default for SettingsInterface {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsInterface {
    /// Create a new Settings interface with values read from the environment.
    pub fn new() -> Self {
        let mut settings = HashMap::new();
        settings.insert(NS_APPEARANCE.to_string(), Self::read_appearance_settings());
        Self { settings }
    }

    /// Read appearance settings from environment variables and defaults.
    fn read_appearance_settings() -> HashMap<String, OwnedValue> {
        let mut appearance = HashMap::new();

        // color-scheme: 0=default, 1=dark, 2=light
        let color_scheme = Self::detect_color_scheme();
        appearance.insert("color-scheme".to_string(), OwnedValue::from(color_scheme));

        // accent-color: (r, g, b) as doubles
        let (r, g, b) = Self::detect_accent_color();
        if let Ok(val) = OwnedValue::try_from(Value::from((r, g, b))) {
            appearance.insert("accent-color".to_string(), val);
        }

        // contrast: 0=default, 1=high
        let contrast = Self::detect_contrast();
        appearance.insert("contrast".to_string(), OwnedValue::from(contrast));

        // reduced-motion: 0=normal, 1=reduced
        let reduced_motion = Self::detect_reduced_motion();
        appearance.insert(
            "reduced-motion".to_string(),
            OwnedValue::from(reduced_motion),
        );

        appearance
    }

    /// Detect color scheme preference.
    ///
    /// Priority:
    /// 1. `XDP_GENERIC_COLOR_SCHEME` environment variable
    /// 2. `GTK_THEME` containing "dark" (case-insensitive)
    /// 3. Default (0 = no preference)
    fn detect_color_scheme() -> u32 {
        // Explicit override
        if let Ok(val) = std::env::var("XDP_GENERIC_COLOR_SCHEME")
            && let Ok(n) = val.parse::<u32>()
            && n <= 2
        {
            return n;
        }

        // Detect from GTK_THEME
        if let Ok(theme) = std::env::var("GTK_THEME")
            && theme.to_lowercase().contains("dark")
        {
            return 1; // prefer dark
        }

        0 // no preference
    }

    /// Detect accent color.
    ///
    /// Reads from `XDP_GENERIC_ACCENT_COLOR` as "r,g,b" floats, defaults
    /// to a neutral blue (GNOME/COSMIC default).
    fn detect_accent_color() -> (f64, f64, f64) {
        if let Ok(val) = std::env::var("XDP_GENERIC_ACCENT_COLOR") {
            let parts: Vec<&str> = val.split(',').collect();
            if parts.len() == 3
                && let (Ok(r), Ok(g), Ok(b)) = (
                    parts[0].trim().parse::<f64>(),
                    parts[1].trim().parse::<f64>(),
                    parts[2].trim().parse::<f64>(),
                )
            {
                return (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0));
            }
        }

        // Default: GNOME blue accent
        (0.21, 0.52, 0.89)
    }

    /// Detect contrast preference.
    fn detect_contrast() -> u32 {
        if let Ok(val) = std::env::var("XDP_GENERIC_CONTRAST")
            && let Ok(n) = val.parse::<u32>()
            && n <= 1
        {
            return n;
        }
        0 // default contrast
    }

    /// Detect reduced-motion preference.
    ///
    /// Returns 0 (normal animations) or 1 (prefer reduced motion).
    fn detect_reduced_motion() -> u32 {
        if let Ok(val) = std::env::var("XDP_GENERIC_REDUCED_MOTION")
            && let Ok(n) = val.parse::<u32>()
            && n <= 1
        {
            return n;
        }
        0 // normal animations
    }

    /// Check if a namespace pattern matches a full namespace name.
    ///
    /// Patterns are prefix-matched at dot boundaries:
    /// - `"org.freedesktop.appearance"` matches exactly
    /// - `"org.freedesktop"` matches `"org.freedesktop.appearance"` (prefix + dot)
    /// - `"org.freedesktop"` does NOT match `"org.freedesktopXYZ"` (no dot boundary)
    /// - Empty string matches everything
    fn namespace_matches(pattern: &str, full_ns: &str) -> bool {
        if pattern.is_empty() {
            return true;
        }
        if full_ns == pattern {
            return true;
        }
        // Prefix match: pattern must be followed by a dot in the full namespace
        full_ns.starts_with(pattern) && full_ns.as_bytes().get(pattern.len()) == Some(&b'.')
    }

    /// Update a setting value and return the replaced value if changed.
    ///
    /// The caller should emit `SettingChanged` after this returns `Some`.
    pub fn update_setting(
        &mut self,
        namespace: &str,
        key: &str,
        value: OwnedValue,
    ) -> Option<OwnedValue> {
        let ns = self.settings.entry(namespace.to_string()).or_default();
        ns.insert(key.to_string(), value)
    }

    /// Re-read settings from environment and return any that changed.
    ///
    /// Returns a list of `(namespace, key, new_value)` tuples for each
    /// setting that differs from the currently cached value.
    pub fn refresh_from_env(&mut self) -> Vec<(String, String, OwnedValue)> {
        let fresh = Self::read_appearance_settings();
        let mut changes = Vec::new();

        let current = self.settings.entry(NS_APPEARANCE.to_string()).or_default();
        for (key, new_val) in &fresh {
            let changed = match current.get(key) {
                Some(old_val) => old_val != new_val,
                None => true,
            };
            if changed {
                changes.push((NS_APPEARANCE.to_string(), key.clone(), new_val.clone()));
                current.insert(key.clone(), new_val.clone());
            }
        }

        changes
    }
}

#[interface(name = "org.freedesktop.impl.portal.Settings")]
impl SettingsInterface {
    /// Read a single setting value.
    ///
    /// Returns the value for the given namespace and key, or an error if
    /// the setting is not known.
    #[zbus(name = "Read")]
    async fn read(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        tracing::debug!(namespace = namespace, key = key, "Settings.Read called");

        let value = self
            .settings
            .get(namespace)
            .and_then(|ns| ns.get(key))
            .cloned()
            .ok_or_else(|| {
                zbus::fdo::Error::Failed(format!("Unknown setting: {namespace}/{key}"))
            })?;

        Ok(value)
    }

    /// Read all settings for the requested namespaces.
    ///
    /// Returns a map of namespace → key → value for all known settings
    /// in the requested namespaces. If the namespace list is empty,
    /// returns all known settings. Namespace patterns are matched at
    /// dot boundaries.
    #[zbus(name = "ReadAll")]
    async fn read_all(
        &self,
        namespaces: Vec<&str>,
    ) -> zbus::fdo::Result<HashMap<String, HashMap<String, OwnedValue>>> {
        tracing::debug!(
            namespaces = ?namespaces,
            "Settings.ReadAll called"
        );

        if namespaces.is_empty() {
            // Return everything
            return Ok(self.settings.clone());
        }

        let mut result = HashMap::new();
        for pattern in namespaces {
            for (full_ns, values) in &self.settings {
                if Self::namespace_matches(pattern, full_ns) {
                    result.insert(full_ns.clone(), values.clone());
                }
            }
        }

        Ok(result)
    }

    // === Signals ===

    /// Emitted when a setting value changes.
    #[zbus(signal, name = "SettingChanged")]
    pub async fn setting_changed(
        signal_emitter: &SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: OwnedValue,
    ) -> zbus::Result<()>;

    // === Properties ===

    /// Interface version.
    #[zbus(property)]
    async fn version(&self) -> u32 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_interface_creation() {
        let iface = SettingsInterface::new();
        assert!(iface.settings.contains_key(NS_APPEARANCE));

        let appearance = &iface.settings[NS_APPEARANCE];
        assert!(appearance.contains_key("color-scheme"));
        assert!(appearance.contains_key("accent-color"));
        assert!(appearance.contains_key("contrast"));
        assert!(appearance.contains_key("reduced-motion"));
    }

    #[test]
    fn test_detect_color_scheme_default() {
        // Without any env vars set, should return 0 (no preference)
        // Note: this test may be affected by the test runner's environment
        let scheme = SettingsInterface::detect_color_scheme();
        assert!(scheme <= 2);
    }

    #[test]
    fn test_detect_accent_color_default() {
        let (r, g, b) = SettingsInterface::detect_accent_color();
        // Should be in valid range
        assert!((0.0..=1.0).contains(&r));
        assert!((0.0..=1.0).contains(&g));
        assert!((0.0..=1.0).contains(&b));
    }

    #[test]
    fn test_detect_contrast_default() {
        let contrast = SettingsInterface::detect_contrast();
        assert!(contrast <= 1);
    }

    #[test]
    fn test_detect_reduced_motion_default() {
        let motion = SettingsInterface::detect_reduced_motion();
        assert!(motion <= 1);
    }

    #[test]
    fn test_appearance_settings_complete() {
        let appearance = SettingsInterface::read_appearance_settings();
        assert!(appearance.contains_key("color-scheme"));
        assert!(appearance.contains_key("accent-color"));
        assert!(appearance.contains_key("contrast"));
        assert!(appearance.contains_key("reduced-motion"));
    }

    #[test]
    fn test_namespace_matches_exact() {
        assert!(SettingsInterface::namespace_matches(
            "org.freedesktop.appearance",
            "org.freedesktop.appearance"
        ));
    }

    #[test]
    fn test_namespace_matches_prefix() {
        assert!(SettingsInterface::namespace_matches(
            "org.freedesktop",
            "org.freedesktop.appearance"
        ));
    }

    #[test]
    fn test_namespace_matches_no_false_prefix() {
        // "org.freedesktop" should NOT match "org.freedesktopXYZ"
        assert!(!SettingsInterface::namespace_matches(
            "org.freedesktop",
            "org.freedesktopXYZ"
        ));
    }

    #[test]
    fn test_namespace_matches_empty_matches_all() {
        assert!(SettingsInterface::namespace_matches(
            "",
            "org.freedesktop.appearance"
        ));
    }

    #[test]
    fn test_update_setting() {
        let mut iface = SettingsInterface::new();
        let old = iface.update_setting(NS_APPEARANCE, "color-scheme", OwnedValue::from(1u32));
        assert!(old.is_some());

        // Verify the update took effect
        let val = iface.settings[NS_APPEARANCE].get("color-scheme").unwrap();
        let scheme: u32 = val.try_into().unwrap();
        assert_eq!(scheme, 1);
    }

    #[test]
    fn test_update_setting_new_namespace() {
        let mut iface = SettingsInterface::new();
        let old = iface.update_setting("com.custom", "my-key", OwnedValue::from(42u32));
        assert!(old.is_none());

        assert!(iface.settings.contains_key("com.custom"));
    }
}
