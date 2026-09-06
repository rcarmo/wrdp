//! `ScreenCast` D-Bus interface implementation.
//!
//! Implements the runtime capture-session boundary for portal screencast flows.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use zbus::{
    interface,
    zvariant::{self, ObjectPath, OwnedValue, Value},
};

use super::{Response, empty_results, get_option_bool, get_option_u32};
use crate::desktop::portal::xdg_desktop::{
    error::PortalError,
    pipewire::PipeWireManager,
    services::{capture::CaptureBackend, input::InputBackend},
    session::{PersistMode, RestoreData, SessionManager},
    types::{CursorMode, SourceInfo, SourceType, StreamInfo},
};

/// `ScreenCast` portal interface implementation.
pub struct ScreenCastInterface {
    /// Session manager.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Capture backend for screen capture operations.
    capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// `PipeWire` manager for stream lifecycle (used by `OpenPipeWireRemote`).
    pipewire_manager: Arc<PipeWireManager>,
    /// Input backend for session cleanup.
    input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
}

impl ScreenCastInterface {
    /// Create a new `ScreenCast` interface with a capture backend.
    pub fn new(
        session_manager: Arc<Mutex<SessionManager>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        pipewire_manager: Arc<PipeWireManager>,
        input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    ) -> Self {
        Self {
            session_manager,
            capture_backend,
            pipewire_manager,
            input_backend,
        }
    }

    /// Extract `persist_mode` from options.
    fn get_persist_mode(options: &HashMap<String, OwnedValue>) -> PersistMode {
        get_option_u32(options, "persist_mode").map_or(PersistMode::None, PersistMode::from_dbus)
    }

    /// Extract source types from options.
    fn get_source_types(options: &HashMap<String, OwnedValue>) -> Vec<SourceType> {
        get_option_u32(options, "types")
            .map_or_else(|| vec![SourceType::Monitor], SourceType::from_bits)
    }

    /// Extract cursor mode from options.
    fn get_cursor_mode(options: &HashMap<String, OwnedValue>) -> CursorMode {
        get_option_u32(options, "cursor_mode")
            .map_or_else(CursorMode::default, CursorMode::from_bits)
    }

    /// Extract multiple sources flag.
    fn get_multiple(options: &HashMap<String, OwnedValue>) -> bool {
        get_option_bool(options, "multiple").unwrap_or(false)
    }

    /// Try to parse `restore_data` from D-Bus options.
    ///
    /// The `restore_data` option is a `(suv)` tuple: (vendor, version, data).
    /// We only accept vendor `"generic"` and version `1`. The data variant
    /// contains a string array of output names.
    fn parse_restore_data(options: &HashMap<String, OwnedValue>) -> Option<RestoreData> {
        let rd = options.get("restore_data")?;
        // Try to decode the (suv) structure
        let value: &Value<'_> = rd.downcast_ref().ok()?;
        if let Value::Structure(s) = value {
            let fields = s.fields();
            if fields.len() >= 3 {
                let vendor: &str = fields[0].downcast_ref().ok()?;
                if vendor != "generic" {
                    tracing::debug!(vendor, "Unknown restore_data vendor, ignoring");
                    return None;
                }
                let version = u32::try_from(&fields[1]).ok()?;
                if version != 1 {
                    tracing::debug!(version, "Unknown restore_data version, ignoring");
                    return None;
                }
                // Data variant: try to extract as array of strings
                if let Value::Value(inner) = &fields[2]
                    && let Value::Array(arr) = inner.as_ref()
                {
                    let names: Vec<String> = arr
                        .iter()
                        .filter_map(|v| {
                            if let Value::Str(s) = v {
                                Some(s.to_string())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !names.is_empty() {
                        return Some(RestoreData {
                            vendor: vendor.to_string(),
                            version,
                            output_names: names,
                        });
                    }
                }
            }
        }
        None
    }

    /// Build stream results for D-Bus response.
    ///
    /// Includes `ScreenCast` v5 properties: `source_type` (from actual source),
    /// `mapping_id` (persistent output identifier).
    #[expect(
        clippy::expect_used,
        reason = "infallible zvariant Value-to-OwnedValue conversions"
    )]
    fn build_stream_results(streams: &[StreamInfo]) -> HashMap<String, OwnedValue> {
        let stream_data: Vec<(u32, HashMap<String, OwnedValue>)> = streams
            .iter()
            .map(|s| {
                let mut props: HashMap<String, OwnedValue> = HashMap::new();
                props.insert(
                    "position".to_string(),
                    OwnedValue::try_from(Value::from((s.position.0, s.position.1)))
                        .expect("tuple Value converts to OwnedValue"),
                );
                props.insert(
                    "size".to_string(),
                    OwnedValue::try_from(Value::from((s.size.0, s.size.1)))
                        .expect("tuple Value converts to OwnedValue"),
                );
                props.insert(
                    "source_type".to_string(),
                    OwnedValue::from(s.source_type.to_bits()),
                );
                if let Some(ref mapping_id) = s.mapping_id
                    && let Ok(val) = OwnedValue::try_from(Value::from(mapping_id.as_str()))
                {
                    props.insert("mapping_id".to_string(), val);
                }
                (s.node_id, props)
            })
            .collect();

        let mut results = HashMap::new();
        results.insert(
            "streams".to_string(),
            OwnedValue::try_from(Value::from(stream_data))
                .expect("stream data Value converts to OwnedValue"),
        );
        results
    }
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastInterface {
    /// Create a new `ScreenCast` session.
    #[zbus(name = "CreateSession")]
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            sender = %sender,
            "ScreenCast.CreateSession called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::new(Arc::clone(&self.session_manager));
        let _ = server.at(&handle, request_iface).await;

        let persist_mode = Self::get_persist_mode(&options);

        let mut manager = self.session_manager.lock().await;

        let result = match manager.create_session(
            session_handle.to_owned(),
            sender,
            app_id.to_string(),
            persist_mode,
        ) {
            Ok(_session) => {
                // Register a Session D-Bus object at the session handle path
                let session_iface = super::SessionInterface::new(
                    Arc::clone(&self.session_manager),
                    session_handle.to_owned(),
                    Arc::clone(&self.input_backend),
                    Arc::clone(&self.capture_backend),
                    Arc::clone(&self.pipewire_manager),
                );
                if let Err(e) = server.at(&session_handle, session_iface).await {
                    tracing::warn!(
                        session_handle = %session_handle,
                        error = %e,
                        "Failed to register Session D-Bus object"
                    );
                }

                let mut results = HashMap::new();
                results.insert(
                    "session_handle".to_string(),
                    OwnedValue::from(session_handle.to_owned()),
                );
                Ok((Response::Success.to_u32(), results))
            }
            Err(e) => {
                tracing::error!(error = %e, "ScreenCast.CreateSession failed");
                Ok((Response::Other.to_u32(), empty_results()))
            }
        };

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;
        result
    }

    /// Select sources for capture.
    #[zbus(name = "SelectSources")]
    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            "SelectSources called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
        );
        let _ = server.at(&handle, request_iface).await;

        let source_types = Self::get_source_types(&options);
        let multiple = Self::get_multiple(&options);
        let restore_data = Self::parse_restore_data(&options);
        let persist_mode = Self::get_persist_mode(&options);

        let mut manager = self.session_manager.lock().await;
        manager.validate_session(&session_handle, app_id, &sender)?;

        // Get available sources from capture backend
        let backend = self.capture_backend.lock().await;
        let sources = backend
            .get_sources(&source_types)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        if sources.is_empty() {
            tracing::warn!("No sources available for capture");
            return Ok((Response::Other.to_u32(), empty_results()));
        }

        drop(backend);

        // Try to restore saved selection from restore_data
        let selected_sources = if let Some(ref rd) = restore_data {
            let restored: Vec<SourceInfo> = rd
                .output_names
                .iter()
                .filter_map(|name| sources.iter().find(|s| s.name == *name).cloned())
                .collect();

            if restored.is_empty() {
                tracing::debug!(
                    "Restore data output names don't match current outputs, using picker"
                );
                select_sources_with_picker(&sources, multiple)
            } else {
                tracing::info!(
                    count = restored.len(),
                    names = ?rd.output_names,
                    "Restored sources from persist data"
                );
                restored
            }
        } else {
            select_sources_with_picker(&sources, multiple)
        };

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        session.persist_mode = persist_mode;
        session.restore_data = restore_data;
        session.select_sources(selected_sources)?;

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;

        Ok((Response::Success.to_u32(), empty_results()))
    }

    /// Start the `ScreenCast` session.
    #[zbus(name = "Start")]
    #[expect(
        clippy::too_many_arguments,
        reason = "D-Bus method signature requires all parameters"
    )]
    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = parent_window;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            "ScreenCast.Start called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
        );
        let _ = server.at(&handle, request_iface).await;

        let cursor_mode = Self::get_cursor_mode(&options);

        let mut manager = self.session_manager.lock().await;
        manager.validate_session(&session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.sources_selected {
            return Err(PortalError::InvalidState {
                expected: "Sources selected".to_string(),
                actual: "No sources selected".to_string(),
            }
            .into());
        }

        let sources = session.sources.clone();
        let persist_mode = session.persist_mode;

        // Create capture streams via capture backend
        let mut backend = self.capture_backend.lock().await;
        let streams = backend
            .create_capture_session(&sources, cursor_mode)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        drop(backend);

        // Start the session with streams
        session.start(streams.clone())?;

        let mut results = Self::build_stream_results(&streams);

        // If persist_mode is set, generate and return restore_data
        if persist_mode != PersistMode::None {
            let output_names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();
            // Build restore_data as (suv): ("generic", 1, variant(as))
            let names_value = Value::from(output_names);
            let rd_tuple = Value::from(("generic", 1u32, names_value));
            if let Ok(rd_owned) = OwnedValue::try_from(rd_tuple) {
                results.insert("restore_data".to_string(), rd_owned);
            }
            results.insert(
                "persist_mode".to_string(),
                OwnedValue::from(persist_mode.to_dbus()),
            );
        }

        tracing::info!(
            stream_count = streams.len(),
            persist = ?persist_mode,
            "ScreenCast session started"
        );

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;

        Ok((Response::Success.to_u32(), results))
    }

    /// Open a `PipeWire` remote for the `ScreenCast` session.
    ///
    /// Returns a file descriptor that the client can use to connect to `PipeWire`
    /// and access active screen-capture stream nodes through the runtime
    /// PipeWire graph.
    #[zbus(name = "OpenPipeWireRemote")]
    async fn open_pipe_wire_remote(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<zvariant::OwnedFd> {
        let _ = &options;
        tracing::debug!(
            session_handle = %session_handle,
            "OpenPipeWireRemote called"
        );

        // Validate the session exists and has been started
        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.is_started() {
            return Err(PortalError::InvalidState {
                expected: "Session started with streams".to_string(),
                actual: "Session not started".to_string(),
            }
            .into());
        }

        drop(manager);

        // Get a PipeWire remote fd from the manager
        let fd = self
            .pipewire_manager
            .open_remote()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        tracing::info!(
            session_handle = %session_handle,
            "PipeWire remote fd opened for client"
        );

        Ok(zvariant::OwnedFd::from(fd))
    }

    // === Properties ===

    /// Available source types.
    #[zbus(property, name = "AvailableSourceTypes")]
    async fn available_source_types(&self) -> u32 {
        let backend = self.capture_backend.lock().await;
        backend.available_source_types()
    }

    /// Available cursor modes.
    #[zbus(property, name = "AvailableCursorModes")]
    async fn available_cursor_modes(&self) -> u32 {
        let backend = self.capture_backend.lock().await;
        backend.available_cursor_modes()
    }

    /// Interface version.
    #[zbus(property)]
    #[expect(clippy::unused_async, reason = "zbus interface requires async")]
    async fn version(&self) -> u32 {
        5
    }
}

/// Select sources using a configured picker tool, or auto-select on fallback.
///
/// # Configuration
///
/// - `XDP_GENERIC_SOURCE_PICKER` — Path to a source picker tool.
///   The tool receives source names (one per line) on stdin and should write
///   the selected source name(s) to stdout (one per line).
///
/// - If the tool exits with non-zero status, the selection is cancelled.
/// - If no picker is configured, auto-selects the first source.
///
/// # Multi-select
///
/// When `multiple` is true, the picker may return multiple selections.
/// When false, only the first selection is used.
fn select_sources_with_picker(sources: &[SourceInfo], multiple: bool) -> Vec<SourceInfo> {
    // Check for a configured picker tool
    if let Ok(picker_cmd) = std::env::var("XDP_GENERIC_SOURCE_PICKER") {
        match run_source_picker(&picker_cmd, sources) {
            Ok(selected) => {
                if selected.is_empty() {
                    tracing::info!("Source picker returned no selections, using auto-select");
                } else {
                    let result = if multiple {
                        selected
                    } else {
                        selected.into_iter().take(1).collect()
                    };
                    tracing::info!(
                        count = result.len(),
                        sources = ?result.iter().map(|s| &s.name).collect::<Vec<_>>(),
                        "Sources selected via picker"
                    );
                    return result;
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    picker = %picker_cmd,
                    "Source picker failed, falling back to auto-select"
                );
            }
        }
    }

    // Auto-select: first source (default behavior)
    let selected = sources.first().cloned().into_iter().collect::<Vec<_>>();
    tracing::debug!(
        sources = ?selected.iter().map(|s| &s.name).collect::<Vec<_>>(),
        "Auto-selected sources"
    );
    selected
}

/// Run the configured source picker tool.
///
/// Writes available source names to stdin and reads selected
/// source name(s) from stdout.
fn run_source_picker(picker_cmd: &str, sources: &[SourceInfo]) -> Result<Vec<SourceInfo>, String> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    // Build the input: one source per line as "name\tdescription\tWxH"
    let input: String = sources
        .iter()
        .map(|s| format!("{}\t{}\t{}x{}", s.name, s.description, s.width, s.height))
        .collect::<Vec<_>>()
        .join("\n");

    // Parse the command (support basic shell-like splitting)
    let parts: Vec<&str> = picker_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Err("Empty picker command".to_string());
    }

    let mut cmd = Command::new(parts[0]);
    for arg in &parts[1..] {
        cmd.arg(arg);
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn picker: {e}"))?;

    // Write source list to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Picker process error: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "Picker exited with status {}",
            output.status.code().unwrap_or(-1)
        ));
    }

    // Parse selected source names from stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    let selected_names: Vec<&str> = stdout
        .lines()
        .map(|line| {
            // Support tab-separated format (take first column = name)
            line.split('\t').next().unwrap_or(line).trim()
        })
        .filter(|name| !name.is_empty())
        .collect();

    // Match selected names to source objects
    let selected: Vec<SourceInfo> = selected_names
        .iter()
        .filter_map(|name| sources.iter().find(|s| s.name == *name).cloned())
        .collect();

    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::portal::xdg_desktop::types::SourceInfo;

    fn test_sources() -> Vec<SourceInfo> {
        vec![
            SourceInfo {
                id: 1,
                name: "eDP-1".to_string(),
                description: "Built-in Display".to_string(),
                width: 1920,
                height: 1080,
                refresh_rate: 60000,
                source_type: SourceType::Monitor,
            },
            SourceInfo {
                id: 2,
                name: "HDMI-A-1".to_string(),
                description: "External Monitor".to_string(),
                width: 2560,
                height: 1440,
                refresh_rate: 60000,
                source_type: SourceType::Monitor,
            },
        ]
    }

    #[test]
    fn test_auto_select_first_source() {
        let sources = test_sources();
        let selected = select_sources_with_picker(&sources, false);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "eDP-1");
    }

    #[test]
    fn test_auto_select_empty_sources() {
        let selected = select_sources_with_picker(&[], false);
        assert!(selected.is_empty());
    }

    #[test]
    fn test_picker_with_echo() {
        // Use echo as a trivial picker that outputs its argument
        let sources = test_sources();
        let selected = run_source_picker("echo HDMI-A-1", &sources);
        assert!(selected.is_ok());
        let selected = selected.unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "HDMI-A-1");
    }

    #[test]
    fn test_picker_nonexistent_tool() {
        let sources = test_sources();
        let result = run_source_picker("/nonexistent/picker/tool", &sources);
        assert!(result.is_err());
    }

    #[test]
    fn test_picker_failing_tool() {
        let sources = test_sources();
        let result = run_source_picker("false", &sources);
        assert!(result.is_err());
    }
}
