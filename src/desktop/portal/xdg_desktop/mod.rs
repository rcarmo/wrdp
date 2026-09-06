//! XDG desktop portal backend for Wayland compositors.
//!
//! Provides the runtime portal backend boundary used by wrdp. It wires D-Bus
//! interfaces, session lifecycle, Wayland protocol backends, and PipeWire stream
//! management for capture/input/clipboard/settings flows.
//!
//! Protocol selection and fallback are handled by the service backends based on
//! detected compositor capabilities and optional environment overrides.

#![warn(missing_docs)]
#![warn(clippy::all)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod dbus;
pub mod error;
pub mod pipewire;
pub mod services;
pub mod session;
pub mod types;
pub mod wayland;

// Re-export main types
use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
};

pub use error::{PortalError, Result};
// Backend traits and factories
pub use services::capture::{CaptureBackend, CaptureDetector, CapturePreference, CaptureProtocol};
pub use services::{
    clipboard::{ClipboardBackend, ClipboardPreference, ClipboardProtocol},
    input::{
        AvailableProtocols, EisBridgeBackend, EisConfig, EisSession, InputBackend,
        InputBackendConfig, InputProtocol, ProtocolDetector, WlrConfig, WlrInputBackend,
        create_input_backend,
    },
};
pub use session::{PersistMode, RestoreData, Session, SessionManager, SessionState};
use tokio::sync::Mutex;
pub use types::{
    ButtonState, ClipboardData, CursorMode, DeviceTypes, InputEvent, KeyState, KeyboardEvent,
    PointerEvent, ScrollAxis, SourceInfo, SourceType, StreamInfo, TouchEvent,
};
pub use wayland::{globals::AvailableProtocols as WaylandProtocols, screencopy::RawFrame};

/// The portal backend service.
///
/// Holds the backend trait objects for each feature domain and wires them
/// to the D-Bus interfaces. Created via [`PortalBackend::new()`] with
/// pre-constructed backends from factory functions.
pub struct PortalBackend {
    /// Session manager.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Input backend (always required for RemoteDesktop).
    input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    /// Capture backend for ScreenCast.
    capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// Clipboard backend (optional — not all compositors support data control).
    clipboard_backend: Option<Arc<Mutex<Box<dyn ClipboardBackend>>>>,
    /// PipeWire manager for screen capture streams.
    pipewire_manager: Arc<pipewire::PipeWireManager>,
    /// Available Wayland protocols (used for capability reporting).
    available_protocols: wayland::globals::AvailableProtocols,
    /// Capture command sender for one-shot screenshots.
    capture_tx: mpsc::Sender<wayland::CaptureCommand>,
    /// Shared Wayland state for monitoring output changes.
    shared_wayland_state: Option<Arc<std::sync::Mutex<wayland::SharedWaylandState>>>,
}

impl PortalBackend {
    /// Create a new portal backend with boxed backends from factory functions.
    pub fn new(
        input_backend: Box<dyn InputBackend>,
        capture_backend: Box<dyn CaptureBackend>,
        clipboard_backend: Option<Box<dyn ClipboardBackend>>,
        pipewire_manager: Arc<pipewire::PipeWireManager>,
        available_protocols: wayland::globals::AvailableProtocols,
        capture_tx: mpsc::Sender<wayland::CaptureCommand>,
    ) -> Self {
        Self {
            session_manager: Arc::new(Mutex::new(SessionManager::new())),
            input_backend: Arc::new(Mutex::new(input_backend)),
            capture_backend: Arc::new(Mutex::new(capture_backend)),
            clipboard_backend: clipboard_backend.map(|cb| Arc::new(Mutex::new(cb))),
            pipewire_manager,
            available_protocols,
            capture_tx,
            shared_wayland_state: None,
        }
    }

    /// Set the shared Wayland state for output hotplug monitoring.
    pub fn set_shared_wayland_state(
        &mut self,
        state: Arc<std::sync::Mutex<wayland::SharedWaylandState>>,
    ) {
        self.shared_wayland_state = Some(state);
    }

    /// Get a reference to the session manager.
    pub fn session_manager(&self) -> Arc<Mutex<SessionManager>> {
        Arc::clone(&self.session_manager)
    }

    /// Get a reference to the PipeWire manager.
    pub fn pipewire_manager(&self) -> Arc<pipewire::PipeWireManager> {
        Arc::clone(&self.pipewire_manager)
    }

    /// Run the D-Bus service.
    ///
    /// Registers the portal interfaces, sets up client disconnect monitoring,
    /// clipboard signal bridging, and runs until the service is stopped.
    pub async fn run(&self) -> anyhow::Result<()> {
        use dbus::{
            ClipboardInterface, ClipboardSignal, RemoteDesktopInterface, ScreenCastInterface,
            ScreenshotInterface, SettingsInterface,
        };

        // Create clipboard interface with shared pending_writes
        let clipboard_iface = ClipboardInterface::new(
            Arc::clone(&self.session_manager),
            self.clipboard_backend.clone(),
        );
        let pending_writes = clipboard_iface.pending_writes();

        let connection = zbus::connection::Builder::session()?
            .name("org.freedesktop.impl.portal.desktop.generic")?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                RemoteDesktopInterface::new(
                    Arc::clone(&self.session_manager),
                    Arc::clone(&self.input_backend),
                    Arc::clone(&self.capture_backend),
                    Arc::clone(&self.pipewire_manager),
                    self.available_protocols.clone(),
                ),
            )?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                ScreenCastInterface::new(
                    Arc::clone(&self.session_manager),
                    Arc::clone(&self.capture_backend),
                    Arc::clone(&self.pipewire_manager),
                    Arc::clone(&self.input_backend),
                ),
            )?
            .serve_at("/org/freedesktop/portal/desktop", clipboard_iface)?
            .serve_at(
                "/org/freedesktop/portal/desktop",
                ScreenshotInterface::new(
                    Arc::clone(&self.capture_backend),
                    self.capture_tx.clone(),
                ),
            )?
            .serve_at("/org/freedesktop/portal/desktop", SettingsInterface::new())?
            .build()
            .await?;

        tracing::info!(
            "Portal backend started on {}",
            connection.unique_name().map_or("unknown", |n| n.as_str())
        );

        // Set up clipboard signal bridge: Wayland on_selection_changed → D-Bus SelectionOwnerChanged
        if let Some(ref clipboard_backend) = self.clipboard_backend {
            let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel::<ClipboardSignal>();
            let session_manager = Arc::clone(&self.session_manager);
            let dbus_conn = connection.clone();

            // Register the on_selection_changed callback to send signals via channel
            {
                let mut backend = clipboard_backend.lock().await;
                backend.on_selection_changed(Box::new(move |mime_types| {
                    let _ = signal_tx.send(ClipboardSignal::SelectionOwnerChanged { mime_types });
                }));
            }

            // Spawn task to receive clipboard signals and emit D-Bus signals
            let pending_writes_clone = pending_writes;
            tokio::spawn(async move {
                Self::clipboard_signal_bridge(
                    dbus_conn,
                    session_manager,
                    signal_rx,
                    pending_writes_clone,
                )
                .await;
            });
        }

        // Spawn client disconnect monitor
        let session_manager = Arc::clone(&self.session_manager);
        let input_backend = Arc::clone(&self.input_backend);
        let capture_backend = Arc::clone(&self.capture_backend);
        let pipewire_manager = Arc::clone(&self.pipewire_manager);
        let dbus_conn = connection.clone();
        tokio::spawn(async move {
            Self::monitor_client_disconnects(
                dbus_conn,
                session_manager,
                input_backend,
                capture_backend,
                pipewire_manager,
            )
            .await;
        });

        // Spawn periodic session cleanup task
        let session_manager = Arc::clone(&self.session_manager);
        tokio::spawn(async move {
            Self::periodic_session_cleanup(session_manager).await;
        });

        // Spawn settings monitoring task (re-reads env vars periodically,
        // emits SettingChanged signals when values change)
        let settings_conn = connection.clone();
        tokio::spawn(async move {
            Self::monitor_settings_changes(settings_conn).await;
        });

        // Spawn output hotplug monitor (propagates wl_output changes to capture backends)
        if let Some(shared_wayland) = &self.shared_wayland_state {
            let shared_wayland = Arc::clone(shared_wayland);
            let capture_backend = Arc::clone(&self.capture_backend);
            tokio::spawn(async move {
                Self::monitor_output_changes(shared_wayland, capture_backend).await;
            });
        }

        // Wait forever (or until connection is dropped)
        std::future::pending::<()>().await;

        Ok(())
    }

    /// Monitor D-Bus client disconnects and clean up their sessions.
    async fn monitor_client_disconnects(
        connection: zbus::Connection,
        session_manager: Arc<Mutex<SessionManager>>,
        input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        pipewire_manager: Arc<pipewire::PipeWireManager>,
    ) {
        use futures::StreamExt;

        let dbus_proxy = match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(e) => {
                tracing::error!(
                    "Failed to create D-Bus proxy for disconnect monitoring: {}",
                    e
                );
                return;
            }
        };

        let mut name_owner_changed = match dbus_proxy.receive_name_owner_changed().await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::error!("Failed to subscribe to NameOwnerChanged: {}", e);
                return;
            }
        };

        tracing::info!("Client disconnect monitor started");

        while let Some(signal) = name_owner_changed.next().await {
            if let Ok(args) = signal.args() {
                // A client disconnected when new_owner is empty
                if args.new_owner.as_ref().map(zbus::names::UniqueName::as_str) == Some("") {
                    let disconnected_name = args.name.as_str();
                    tracing::debug!(
                        name = %disconnected_name,
                        "D-Bus client disconnected"
                    );

                    let mut manager = session_manager.lock().await;
                    let closed_sessions = manager.close_sender_sessions(disconnected_name);
                    drop(manager);

                    // Clean up resources for closed sessions
                    for session in &closed_sessions {
                        let session_id = session.id.to_string();

                        // Destroy input contexts
                        let mut input = input_backend.lock().await;
                        let _ = input.destroy_context(&session_id);
                        drop(input);

                        // Destroy capture streams
                        if !session.streams.is_empty() {
                            let stream_ids = session.stream_ids();
                            let mut capture = capture_backend.lock().await;
                            let _ = capture.destroy_capture_session(&stream_ids);
                            drop(capture);

                            // Destroy PipeWire streams
                            for node_id in stream_ids {
                                let _ = pipewire_manager.destroy_stream(node_id).await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Bridge clipboard signals from Wayland to D-Bus.
    ///
    /// Receives `ClipboardSignal` messages from the Wayland event loop
    /// (via the `on_selection_changed` callback) and emits corresponding
    /// D-Bus signals on all clipboard-enabled sessions.
    async fn clipboard_signal_bridge(
        connection: zbus::Connection,
        session_manager: Arc<Mutex<SessionManager>>,
        mut signal_rx: tokio::sync::mpsc::UnboundedReceiver<dbus::ClipboardSignal>,
        pending_writes: dbus::PendingWrites,
    ) {
        use dbus::ClipboardSignal;
        use zbus::zvariant::{OwnedValue, Value};

        tracing::info!("Clipboard signal bridge started");

        while let Some(signal) = signal_rx.recv().await {
            match signal {
                ClipboardSignal::SelectionOwnerChanged { mime_types } => {
                    let manager = session_manager.lock().await;
                    let handles = manager.clipboard_session_handles();
                    drop(manager);

                    if handles.is_empty() {
                        continue;
                    }

                    // Build the options map with mime_types and session_is_owner=false
                    // (the compositor owns the selection, not our portal client)
                    let mut options: HashMap<String, OwnedValue> = HashMap::new();

                    // Convert mime_types to OwnedValue (array of strings)
                    let mime_array: Vec<Value<'_>> =
                        mime_types.iter().map(|s| Value::from(s.as_str())).collect();
                    if let Ok(val) = OwnedValue::try_from(Value::Array(mime_array.into())) {
                        options.insert("mime_types".to_string(), val);
                    }

                    options.insert("session_is_owner".to_string(), OwnedValue::from(false));

                    // Emit signal on each clipboard-enabled session
                    let iface_ref = connection
                        .object_server()
                        .interface::<_, dbus::ClipboardInterface>("/org/freedesktop/portal/desktop")
                        .await;

                    if let Ok(iface) = iface_ref {
                        let ctx = iface.signal_emitter();
                        for handle in &handles {
                            if let Err(e) = dbus::ClipboardInterface::emit_selection_owner_changed(
                                ctx,
                                handle.clone(),
                                options.clone(),
                            )
                            .await
                            {
                                tracing::warn!(
                                    session = %handle,
                                    error = %e,
                                    "Failed to emit SelectionOwnerChanged signal"
                                );
                            }
                        }
                    }

                    tracing::debug!(
                        session_count = handles.len(),
                        mime_types = ?mime_types,
                        "Emitted SelectionOwnerChanged to clipboard sessions"
                    );
                }
                ClipboardSignal::SelectionTransfer { mime_type, serial } => {
                    let manager = session_manager.lock().await;
                    let handles = manager.clipboard_session_handles();
                    drop(manager);

                    if handles.is_empty() {
                        continue;
                    }

                    // Register a pending write entry so SelectionWrite can store data
                    if let Ok(mut writes) = pending_writes.lock() {
                        writes.insert(
                            serial,
                            dbus::PendingWriteEntry {
                                mime_type: mime_type.clone(),
                                data: None,
                            },
                        );
                    }

                    let iface_ref = connection
                        .object_server()
                        .interface::<_, dbus::ClipboardInterface>("/org/freedesktop/portal/desktop")
                        .await;

                    if let Ok(iface) = iface_ref {
                        let ctx = iface.signal_emitter();
                        for handle in &handles {
                            if let Err(e) = dbus::ClipboardInterface::emit_selection_transfer(
                                ctx,
                                handle.clone(),
                                &mime_type,
                                serial,
                            )
                            .await
                            {
                                tracing::warn!(
                                    session = %handle,
                                    error = %e,
                                    "Failed to emit SelectionTransfer signal"
                                );
                            }
                        }
                    }

                    tracing::debug!(
                        serial = serial,
                        mime_type = %mime_type,
                        session_count = handles.len(),
                        "Emitted SelectionTransfer to clipboard sessions"
                    );
                }
            }
        }

        tracing::warn!("Clipboard signal bridge stopped");
    }

    /// Monitor Wayland output changes and propagate to capture backends.
    ///
    /// Periodically checks the shared Wayland state for output changes and
    /// updates the capture backend's source list when outputs are added or removed.
    async fn monitor_output_changes(
        shared_wayland: Arc<std::sync::Mutex<wayland::SharedWaylandState>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    ) {
        use std::time::Duration;
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let mut last_source_count = 0u32;

        loop {
            interval.tick().await;

            let current_sources = {
                let Ok(state) = shared_wayland.lock() else {
                    continue;
                };
                state.sources.clone()
            };

            let count = current_sources.len() as u32;
            if count != last_source_count {
                tracing::info!(
                    old_count = last_source_count,
                    new_count = count,
                    "Output count changed, updating capture backend sources"
                );
                let mut backend = capture_backend.lock().await;
                backend.update_sources(current_sources);
                last_source_count = count;
            }
        }
    }

    /// Monitor settings for runtime changes and emit SettingChanged signals.
    ///
    /// Re-reads environment variables every 10 seconds and emits D-Bus
    /// signals for any settings that have changed. This allows companion
    /// tools to update appearance settings by modifying env vars and
    /// signaling the portal process.
    async fn monitor_settings_changes(connection: zbus::Connection) {
        use std::time::Duration;
        let mut interval = tokio::time::interval(Duration::from_secs(10));

        // Skip the first immediate tick (settings were just initialized)
        interval.tick().await;

        loop {
            interval.tick().await;

            let iface_ref = connection
                .object_server()
                .interface::<_, dbus::SettingsInterface>("/org/freedesktop/portal/desktop")
                .await;

            let Ok(iface) = iface_ref else {
                continue;
            };

            // Refresh settings and collect changes
            let changes = {
                let mut iface_mut = iface.get_mut().await;
                iface_mut.refresh_from_env()
            };

            // Emit signals for each changed setting
            if !changes.is_empty() {
                let ctx = iface.signal_emitter();
                for (namespace, key, value) in &changes {
                    tracing::info!(
                        namespace = %namespace,
                        key = %key,
                        "Setting changed, emitting SettingChanged signal"
                    );
                    if let Err(e) =
                        dbus::SettingsInterface::setting_changed(ctx, namespace, key, value.clone())
                            .await
                    {
                        tracing::warn!(
                            error = %e,
                            "Failed to emit SettingChanged signal"
                        );
                    }
                }
            }
        }
    }

    /// Periodically clean up stale sessions.
    async fn periodic_session_cleanup(session_manager: Arc<Mutex<SessionManager>>) {
        use std::time::Duration;
        let mut interval = tokio::time::interval(Duration::from_mins(1));

        loop {
            interval.tick().await;

            let mut manager = session_manager.lock().await;
            let stale = manager.cleanup_stale_sessions(Duration::from_mins(5));
            drop(manager);

            if !stale.is_empty() {
                tracing::info!(count = stale.len(), "Cleaned up stale sessions");
            }
        }
    }
}
