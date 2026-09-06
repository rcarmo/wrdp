//! `RemoteDesktop` D-Bus interface implementation.
//!
//! Implements the runtime `org.freedesktop.impl.portal.RemoteDesktop` boundary,
//! including session setup, device selection, stream startup, and protocol-aware
//! input event forwarding.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use zbus::{
    interface,
    zvariant::{Fd, ObjectPath, OwnedValue, Value},
};

use super::{Response, empty_results, get_option_bool, get_option_u32};
use crate::desktop::portal::xdg_desktop::{
    error::PortalError,
    pipewire::PipeWireManager,
    services::{
        capture::CaptureBackend,
        input::{InputBackend, InputProtocol},
    },
    session::{PersistMode, SessionManager},
    types::{
        ButtonState, CursorMode, DeviceTypes, InputEvent, KeyState, KeyboardEvent, PointerEvent,
        ScrollAxis, StreamInfo, TouchEvent,
    },
    wayland::AvailableProtocols,
};

/// `RemoteDesktop` portal interface implementation.
pub struct RemoteDesktopInterface {
    /// Session manager.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Input backend for protocol-aware input injection.
    input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    /// Capture backend for combined `RemoteDesktop`+`ScreenCast` sessions.
    /// Used in `Start()` to create capture streams when sources are selected.
    capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// `PipeWire` manager for stream lifecycle.
    /// Used via `capture_backend` for creating/destroying stream resources.
    pipewire_manager: Arc<PipeWireManager>,
    /// Available protocols for capability queries.
    available_protocols: AvailableProtocols,
}

impl RemoteDesktopInterface {
    /// Create a new `RemoteDesktop` interface.
    pub fn new(
        session_manager: Arc<Mutex<SessionManager>>,
        input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        pipewire_manager: Arc<PipeWireManager>,
        available_protocols: AvailableProtocols,
    ) -> Self {
        Self {
            session_manager,
            input_backend,
            capture_backend,
            pipewire_manager,
            available_protocols,
        }
    }

    /// Extract `persist_mode` from options.
    fn get_persist_mode(options: &HashMap<String, OwnedValue>) -> PersistMode {
        get_option_u32(options, "persist_mode").map_or(PersistMode::None, PersistMode::from_dbus)
    }

    /// Extract device types from options.
    fn get_device_types(options: &HashMap<String, OwnedValue>) -> DeviceTypes {
        get_option_u32(options, "types").map_or_else(DeviceTypes::all, DeviceTypes::from_bits)
    }

    /// Build stream results for D-Bus response (combined `RemoteDesktop`+`ScreenCast`).
    ///
    /// Same format as `ScreenCast.Start` response: `streams` key with
    /// `a(ua{sv})` -- array of (`node_id`, properties) tuples.
    #[expect(
        clippy::expect_used,
        reason = "Value-to-OwnedValue conversion is infallible for tuple/array types"
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

#[interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktopInterface {
    /// Create a new `RemoteDesktop` session.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session could not be created or if the sender
    /// is missing from the D-Bus header.
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
            "CreateSession called"
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
                tracing::error!(error = %e, "CreateSession failed");
                Ok((Response::Other.to_u32(), empty_results()))
            }
        };

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;
        result
    }

    /// Select input devices for the session.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or the sender is invalid.
    #[zbus(name = "SelectDevices")]
    async fn select_devices(
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
            "SelectDevices called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
        );
        let _ = server.at(&handle, request_iface).await;

        let device_types = Self::get_device_types(&options);

        let mut manager = self.session_manager.lock().await;
        manager.validate_session(&session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        session.select_devices(device_types)?;

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;

        Ok((Response::Success.to_u32(), empty_results()))
    }

    /// Start the `RemoteDesktop` session.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session cannot be started, sources cannot be
    /// captured, or the sender is invalid.
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
        let _ = (parent_window, &options);
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            "Start called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
        );
        let _ = server.at(&handle, request_iface).await;

        let mut manager = self.session_manager.lock().await;
        manager.validate_session(&session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        let device_bits = session.device_types.to_bits();
        let sources_selected = session.sources_selected;
        let clipboard_enabled = session.clipboard_enabled;
        let sources = session.sources.clone();
        let persist_mode = session.persist_mode;

        // If sources were selected (combined RemoteDesktop+ScreenCast session),
        // create capture streams before starting the session.
        let streams = if sources_selected && !sources.is_empty() {
            let mut backend = self.capture_backend.lock().await;
            backend
                .create_capture_session(&sources, CursorMode::default())
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
        } else {
            vec![]
        };

        session.start(streams.clone())?;

        // If we created capture streams, set up stream-to-output mappings
        // for multi-monitor absolute pointer positioning.
        if !streams.is_empty() {
            let mappings: Vec<crate::desktop::portal::xdg_desktop::types::StreamOutputMapping> =
                streams
                    .iter()
                    .map(
                        |s| crate::desktop::portal::xdg_desktop::types::StreamOutputMapping {
                            stream_node_id: s.node_id,
                            x: s.position.0,
                            y: s.position.1,
                            width: s.size.0,
                            height: s.size.1,
                        },
                    )
                    .collect();

            let mut backend = self.input_backend.lock().await;
            backend.set_stream_mappings(mappings);
        }

        let mut results = HashMap::new();
        results.insert("devices".to_string(), OwnedValue::from(device_bits));

        // Include streams in response for combined RemoteDesktop+ScreenCast sessions
        if !streams.is_empty() {
            let stream_results = Self::build_stream_results(&streams);
            for (key, value) in stream_results {
                results.insert(key, value);
            }
        }

        // Include clipboard state
        if clipboard_enabled {
            results.insert("clipboard_enabled".to_string(), OwnedValue::from(true));
        }

        // Include restore_data if persist_mode is set
        if persist_mode != PersistMode::None && !sources.is_empty() {
            let output_names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();
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
            devices = device_bits,
            stream_count = streams.len(),
            clipboard = clipboard_enabled,
            "RemoteDesktop session started"
        );

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;

        Ok((Response::Success.to_u32(), results))
    }

    /// Connect to EIS for input injection.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not in a valid state for EIS
    /// connection, or if EIS context creation fails.
    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<Fd<'static>> {
        let _ = &options;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            session_handle = %session_handle,
            "ConnectToEIS called"
        );

        let mut manager = self.session_manager.lock().await;
        manager.validate_session(&session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.can_connect_to_eis() {
            return Err(PortalError::InvalidState {
                expected: "Started with devices selected, not yet connected".to_string(),
                actual: format!(
                    "state={}, devices_selected={}, uses_eis={}",
                    session.state, session.devices_selected, session.uses_eis
                ),
            }
            .into());
        }

        let device_types = session.device_types;
        let session_id = session_handle.to_string();

        let mut backend = self.input_backend.lock().await;

        match backend.protocol_type() {
            InputProtocol::Eis => {
                let fd = backend
                    .create_context(&session_id, device_types)
                    .map_err(|e| PortalError::EisCreationFailed(e.to_string()))?
                    .ok_or_else(|| {
                        PortalError::EisCreationFailed("EIS backend returned no fd".to_string())
                    })?;

                drop(backend);
                let mut manager = self.session_manager.lock().await;
                let session = manager
                    .get_session_mut(&session_handle)
                    .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

                #[expect(
                    clippy::items_after_statements,
                    reason = "static counter is logically scoped to its usage point"
                )]
                static CONTEXT_ID: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let context_id = CONTEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                session.connect_to_eis(context_id)?;

                tracing::info!(
                    context_id = context_id,
                    protocol = "EIS",
                    "Input connection established"
                );

                Ok(Fd::from(fd))
            }
            InputProtocol::WlrVirtualInput => {
                backend
                    .create_context(&session_id, device_types)
                    .map_err(|e| PortalError::EisCreationFailed(e.to_string()))?;

                drop(backend);
                let mut manager = self.session_manager.lock().await;
                let session = manager
                    .get_session_mut(&session_handle)
                    .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

                session.devices_selected = true;

                tracing::info!(
                    protocol = "wlr-virtual-input",
                    "Input context created (use Notify* methods)"
                );

                Err(zbus::fdo::Error::NotSupported(
                    "EIS not available. Use NotifyPointerMotion, NotifyKeyboardKeycode, etc. for input injection.".to_string()
                ))
            }
        }
    }

    // === Input notification methods ===

    /// Notify of pointer motion.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or pointer is not enabled.
    #[zbus(name = "NotifyPointerMotion")]
    async fn notify_pointer_motion(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        dx: f64,
        dy: f64,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            dx = dx,
            dy = dy,
            "NotifyPointerMotion"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.pointer {
            return Err(PortalError::PermissionDenied("Pointer not enabled".to_string()).into());
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Pointer(PointerEvent::Motion {
                dx,
                dy,
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of absolute pointer motion.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or pointer is not enabled.
    #[zbus(name = "NotifyPointerMotionAbsolute")]
    async fn notify_pointer_motion_absolute(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        stream: u32,
        x: f64,
        y: f64,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            stream = stream,
            x = x,
            y = y,
            "NotifyPointerMotionAbsolute"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.pointer {
            return Err(PortalError::PermissionDenied("Pointer not enabled".to_string()).into());
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Pointer(PointerEvent::MotionAbsolute {
                x,
                y,
                stream,
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of pointer button press/release.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or pointer is not enabled.
    #[expect(
        clippy::cast_sign_loss,
        reason = "D-Bus sends button as i32 but kernel button codes are u32"
    )]
    #[zbus(name = "NotifyPointerButton")]
    async fn notify_pointer_button(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        button: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            button = button,
            state = state,
            "NotifyPointerButton"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.pointer {
            return Err(PortalError::PermissionDenied("Pointer not enabled".to_string()).into());
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Pointer(PointerEvent::Button {
                button: button as u32,
                state: ButtonState::from_dbus(state),
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of pointer scroll.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or pointer is not enabled.
    #[zbus(name = "NotifyPointerAxis")]
    async fn notify_pointer_axis(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        dx: f64,
        dy: f64,
    ) -> zbus::fdo::Result<()> {
        let finish = get_option_bool(&options, "finish").unwrap_or(false);

        tracing::trace!(
            session_handle = %session_handle,
            dx = dx,
            dy = dy,
            finish = finish,
            "NotifyPointerAxis"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.pointer {
            return Err(PortalError::PermissionDenied("Pointer not enabled".to_string()).into());
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Pointer(PointerEvent::Scroll {
                dx,
                dy,
                time_usec: current_time_usec(),
            }),
        )?;

        // If finish is set, send axis_stop events to indicate the scroll gesture ended
        if finish {
            backend.inject_event(
                &session_id,
                InputEvent::Pointer(PointerEvent::ScrollStop {
                    time_usec: current_time_usec(),
                }),
            )?;
        }

        Ok(())
    }

    /// Notify of discrete pointer scroll (wheel clicks).
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or pointer is not enabled.
    #[zbus(name = "NotifyPointerAxisDiscrete")]
    async fn notify_pointer_axis_discrete(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        axis: u32,
        steps: i32,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            axis = axis,
            steps = steps,
            "NotifyPointerAxisDiscrete"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.pointer {
            return Err(PortalError::PermissionDenied("Pointer not enabled".to_string()).into());
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                axis: ScrollAxis::from_dbus(axis),
                steps,
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of keyboard key by keycode.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or keyboard is not enabled.
    #[expect(
        clippy::cast_sign_loss,
        reason = "D-Bus sends keycode as i32 but XKB keycodes are u32"
    )]
    #[zbus(name = "NotifyKeyboardKeycode")]
    async fn notify_keyboard_keycode(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        keycode: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            keycode = keycode,
            state = state,
            "NotifyKeyboardKeycode"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.keyboard {
            return Err(PortalError::PermissionDenied("Keyboard not enabled".to_string()).into());
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Keyboard(KeyboardEvent {
                keycode: keycode as u32,
                state: KeyState::from_dbus(state),
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of keyboard key by keysym.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found, keyboard is not enabled,
    /// or the keysym cannot be mapped to a keycode.
    #[expect(
        clippy::cast_sign_loss,
        reason = "D-Bus sends keysym as i32 but XKB keysyms are u32"
    )]
    #[zbus(name = "NotifyKeyboardKeysym")]
    async fn notify_keyboard_keysym(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        keysym: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            keysym = keysym,
            state = state,
            "NotifyKeyboardKeysym"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.keyboard {
            return Err(PortalError::PermissionDenied("Keyboard not enabled".to_string()).into());
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;

        // Convert keysym to keycode via the backend's XKB keymap
        let keycode = backend.keysym_to_keycode(keysym as u32).ok_or_else(|| {
            PortalError::Config(format!(
                "No keycode found for keysym 0x{keysym:04x} in current keymap"
            ))
        })?;

        backend.inject_event(
            &session_id,
            InputEvent::Keyboard(KeyboardEvent {
                keycode,
                state: KeyState::from_dbus(state),
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of touch down event.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or touchscreen is not enabled.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "D-Bus sends slot as u32 but touch id is i32; slot values are small"
    )]
    #[zbus(name = "NotifyTouchDown")]
    async fn notify_touch_down(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            stream = stream,
            slot = slot,
            x = x,
            y = y,
            "NotifyTouchDown"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.touchscreen {
            return Err(
                PortalError::PermissionDenied("Touchscreen not enabled".to_string()).into(),
            );
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Touch(TouchEvent::Down {
                id: slot as i32,
                x,
                y,
                stream,
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of touch motion event.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or touchscreen is not enabled.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "D-Bus sends slot as u32 but touch id is i32; slot values are small"
    )]
    #[zbus(name = "NotifyTouchMotion")]
    async fn notify_touch_motion(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            stream = stream,
            slot = slot,
            x = x,
            y = y,
            "NotifyTouchMotion"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.touchscreen {
            return Err(
                PortalError::PermissionDenied("Touchscreen not enabled".to_string()).into(),
            );
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Touch(TouchEvent::Motion {
                id: slot as i32,
                x,
                y,
                stream,
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    /// Notify of touch up event.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or touchscreen is not enabled.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "D-Bus sends slot as u32 but touch id is i32; slot values are small"
    )]
    #[zbus(name = "NotifyTouchUp")]
    async fn notify_touch_up(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        slot: u32,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::trace!(
            session_handle = %session_handle,
            slot = slot,
            "NotifyTouchUp"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.device_types.touchscreen {
            return Err(
                PortalError::PermissionDenied("Touchscreen not enabled".to_string()).into(),
            );
        }

        let session_id = session_handle.to_string();
        drop(manager);

        let mut backend = self.input_backend.lock().await;
        backend.inject_event(
            &session_id,
            InputEvent::Touch(TouchEvent::Up {
                id: slot as i32,
                time_usec: current_time_usec(),
            }),
        )?;

        Ok(())
    }

    // === Properties ===

    /// Available device types.
    ///
    /// Derived from available Wayland protocols:
    /// - Pointer available if wlr-virtual-pointer is bound
    /// - Keyboard available if zwp-virtual-keyboard is bound
    /// - Touchscreen: NOT advertised -- wlr-virtual-pointer does not support
    ///   real multi-touch input. Clients should not send touch events.
    #[expect(
        clippy::unused_async,
        reason = "zbus requires async for property methods"
    )]
    #[zbus(property, name = "AvailableDeviceTypes")]
    async fn available_device_types(&self) -> u32 {
        let devices = DeviceTypes {
            pointer: self.available_protocols.wlr_virtual_pointer,
            keyboard: self.available_protocols.zwp_virtual_keyboard,
            touchscreen: false,
        };
        devices.to_bits()
    }

    /// Interface version.
    #[expect(
        clippy::unused_async,
        reason = "zbus requires async for property methods"
    )]
    #[zbus(property)]
    async fn version(&self) -> u32 {
        2
    }
}

/// Get current monotonic time in microseconds.
///
/// Uses `CLOCK_MONOTONIC` per the portal API spec for input event
/// timestamps. Wall-clock time (`UNIX_EPOCH`) is incorrect because it can
/// jump backwards on NTP adjustments.
#[expect(
    clippy::cast_sign_loss,
    reason = "CLOCK_MONOTONIC tv_sec and tv_nsec are always non-negative"
)]
fn current_time_usec() -> u64 {
    use nix::time::{ClockId, clock_gettime};
    match clock_gettime(ClockId::CLOCK_MONOTONIC) {
        Ok(ts) => ts.tv_sec() as u64 * 1_000_000 + ts.tv_nsec() as u64 / 1_000,
        Err(_) => 0,
    }
}
