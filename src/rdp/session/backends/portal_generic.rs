//! Embedded portal-generic backend: native capture + input + clipboard.
//!
//! Uses the in-process portal-generic backend to provide screen capture,
//! input injection, and clipboard support for the managed compositor runtime
//! without requiring a separate portal daemon process.
//!
//! # Protocols Used
//!
//! - **Capture**: ext-image-copy-capture-v1 or wlr-screencopy-v1
//! - **Input**: wlr-virtual-pointer + zwp-virtual-keyboard (or EIS bridge)
//! - **Clipboard**: ext-data-control-v1 or wlr-data-control-v1
//!
//! # Architecture
//!
//! ```text
//! PortalSessionBackend
//!   ├─> WaylandConnection (global registry scan)
//!   ├─> direct frame channel from screencopy capture
//!   └─> PortalGenericSessionHandle
//!       ├─> CaptureBackend → compositor frames
//!       ├─> InputBackend → virtual keyboard/pointer injection
//!       └─> ClipboardBackend → data-control read/write
//! ```
//!
//! # Runtime characteristics
//!
//! - Provides video capture, input injection, and clipboard support in one backend
//! - Keeps capture/input/clipboard wiring local to the managed compositor session
//!
//! # Limitations
//!
//! - Not Flatpak-compatible (requires direct Wayland socket access)
//! - PipeWire is not required for the managed compositor's direct-frame capture path

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::desktop::portal::xdg_desktop::{
    CaptureProtocol, InputBackend, InputEvent, KeyState, KeyboardEvent, PointerEvent,
    pipewire::PipeWireManager,
    services::{
        capture::create_capture_backend, clipboard::create_clipboard_backend,
        input::create_input_backend,
    },
    types::{CursorMode, DeviceTypes, SourceType},
    wayland::WaylandConnection,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::atomic::Ordering;
use tracing::{debug, info, warn};

use crate::{
    rdp::session::backend::{DirectFrameReceiver, SessionHandle, StreamInfo},
    rdp::session::supervision::{SessionStatusEvent, SessionStatusReporter},
};

/// Session backend using embedded portal-generic services.
///
/// Connects directly to the Wayland compositor as a client and provides
/// video capture, input injection, and clipboard via native protocols.
pub struct PortalSessionBackend;

const NATIVE_THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns `true` only on the first call for a given flag, `false` afterwards.
///
/// Used to make [`SessionHandle::shutdown`] idempotent and to keep `Drop` from
/// duplicating the session-closed report once shutdown has already run.
fn begin_shutdown_once(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

/// Reports a terminal session health event to supervision at most once.
///
/// If supervision is not attached yet, the first terminal event is latched so
/// it can be replayed as soon as a reporter is installed. The pending-event
/// mutex also closes the race between observing no reporter and attaching one.
fn report_health_once(
    reporter: &Arc<std::sync::RwLock<Option<SessionStatusReporter>>>,
    pending_event: &Arc<Mutex<Option<SessionStatusEvent>>>,
    reported: &AtomicBool,
    event: SessionStatusEvent,
) {
    let report = {
        // Always acquire pending_event before reporter; installation uses the
        // same order so an event cannot be stranded after reporter attachment.
        let mut pending_event = pending_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reported.load(Ordering::Acquire) {
            None
        } else if let Some(reporter) = reporter
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            reported
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
                .then_some((reporter, event))
        } else {
            if pending_event.is_none() {
                *pending_event = Some(event);
            }
            None
        }
    };

    if let Some((reporter, event)) = report {
        reporter.report(event);
    }
}

/// Installs supervision and immediately replays a terminal event that arrived
/// before the reporter was available.
fn install_health_reporter(
    health_reporter: &Arc<std::sync::RwLock<Option<SessionStatusReporter>>>,
    pending_event: &Arc<Mutex<Option<SessionStatusEvent>>>,
    reported: &AtomicBool,
    reporter: SessionStatusReporter,
) {
    let event = {
        // Serialize attachment with report_health_once before publishing the
        // reporter. This makes draining the pending latch atomic with install.
        let mut pending_event = pending_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *health_reporter
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reporter.clone());

        pending_event.take().filter(|_| {
            reported
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
    };

    if let Some(event) = event {
        reporter.report(event);
    }
}

/// Rolls back partially constructed session resources when setup fails before
/// ownership can be transferred to the session handle.
///
/// On drop (while armed) it destroys any already-created input context and
/// capture sessions/streams, signals the Wayland event loop to stop, and joins
/// it. Because construction runs on a blocking thread, joining here is safe.
#[expect(
    clippy::type_complexity,
    reason = "explicit shared backend ownership contract"
)]
struct ConstructionRollback {
    stop: Arc<AtomicBool>,
    event_loop: Option<std::thread::JoinHandle<()>>,
    input: Option<(Arc<Mutex<Box<dyn InputBackend>>>, String)>,
    capture: Option<(
        Arc<Mutex<Box<dyn crate::desktop::portal::xdg_desktop::CaptureBackend>>>,
        Vec<u32>,
    )>,
    armed: bool,
}

impl ConstructionRollback {
    fn new(stop: Arc<AtomicBool>, event_loop: std::thread::JoinHandle<()>) -> Self {
        Self {
            stop,
            event_loop: Some(event_loop),
            input: None,
            capture: None,
            armed: true,
        }
    }

    fn set_input(&mut self, backend: Arc<Mutex<Box<dyn InputBackend>>>, session_id: String) {
        self.input = Some((backend, session_id));
    }

    fn set_capture(
        &mut self,
        backend: Arc<Mutex<Box<dyn crate::desktop::portal::xdg_desktop::CaptureBackend>>>,
        stream_ids: Vec<u32>,
    ) {
        self.capture = Some((backend, stream_ids));
    }

    /// Disarm the guard and return ownership of the event-loop join handle to
    /// the successfully constructed session handle.
    #[expect(
        clippy::expect_used,
        reason = "internal invariant established during construction or by guarded control flow"
    )]
    fn disarm(&mut self) -> std::thread::JoinHandle<()> {
        self.armed = false;
        self.event_loop
            .take()
            .expect("event-loop join handle already taken")
    }
}

impl Drop for ConstructionRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some((backend, session_id)) = self.input.take()
            && let Ok(mut backend) = backend.lock()
            && let Err(error) = backend.destroy_context(&session_id)
        {
            warn!("portal-generic: rollback destroy input context failed: {error}");
        }
        if let Some((backend, stream_ids)) = self.capture.take()
            && !stream_ids.is_empty()
            && let Ok(mut backend) = backend.lock()
            && let Err(error) = backend.destroy_capture_session(&stream_ids)
        {
            warn!("portal-generic: rollback destroy capture session failed: {error}");
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.event_loop.take() {
            let _ = handle.join();
        }
    }
}

fn prepare_wayland<F>(
    connect: F,
    capture: &crate::desktop::portal::xdg_desktop::services::capture::CapturePreference,
) -> Result<(
    WaylandConnection,
    crate::desktop::portal::xdg_desktop::wayland::AvailableProtocols,
    Vec<crate::desktop::portal::xdg_desktop::types::SourceInfo>,
)>
where
    F: FnOnce() -> crate::desktop::portal::xdg_desktop::Result<WaylandConnection>,
{
    let mut connection = connect().context("connect to managed Wayland display")?;
    if capture.handshake_timeout_ms != 0 {
        connection.set_ext_capture_handshake_timeout(std::time::Duration::from_millis(
            capture.handshake_timeout_ms,
        ));
    }
    let avoid_ext = capture.preferred == Some(CaptureProtocol::WlrScreencopy)
        || capture
            .broken_protocols
            .contains(&CaptureProtocol::ExtImageCopyCapture);
    connection.set_force_wlr_screencopy(avoid_ext);
    let protocols = connection.available_protocols().clone();
    let sources = connection.state().get_sources();
    Ok((connection, protocols, sources))
}

fn attach_input(
    mut settings: crate::desktop::portal::xdg_desktop::services::input::InputBackendConfig,
    protocols: &crate::desktop::portal::xdg_desktop::wayland::AvailableProtocols,
    socket: Option<PathBuf>,
    expected_wayland_peer_uid: Option<u32>,
) -> Result<(String, Box<dyn InputBackend>)> {
    settings.wlr.wayland_socket_path = socket;
    // This config is also passed unchanged to the EIS bridge, whose compositor
    // side is a WlrInputBackend.
    settings.wlr.expected_wayland_peer_uid = expected_wayland_peer_uid;
    let mut backend = create_input_backend(&settings, protocols)
        .map_err(|error| anyhow::anyhow!("create input backend: {error}"))?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    backend
        .create_context(
            &id,
            DeviceTypes {
                keyboard: true,
                pointer: true,
                touchscreen: false,
            },
        )
        .map_err(|error| anyhow::anyhow!("create input context: {error}"))?;
    Ok((id, backend))
}

fn start_monitor_capture(
    settings: crate::desktop::portal::xdg_desktop::services::capture::CapturePreference,
    protocols: &crate::desktop::portal::xdg_desktop::wayland::AvailableProtocols,
    sources: Vec<crate::desktop::portal::xdg_desktop::types::SourceInfo>,
    pipewire: Arc<PipeWireManager>,
    commands: std::sync::mpsc::Sender<crate::desktop::portal::xdg_desktop::wayland::CaptureCommand>,
) -> Result<(
    Box<dyn crate::desktop::portal::xdg_desktop::CaptureBackend>,
    Vec<StreamInfo>,
)> {
    let mut backend = create_capture_backend(protocols, &settings, sources, pipewire, commands)
        .map_err(|error| anyhow::anyhow!("create capture backend: {error}"))?;
    let monitors = backend
        .get_sources(&[SourceType::Monitor])
        .map_err(|error| anyhow::anyhow!("enumerate capture monitors: {error}"))?;
    let streams = backend
        .create_capture_session(&monitors, CursorMode::Hidden)
        .map_err(|error| anyhow::anyhow!("start monitor capture: {error}"))?
        .into_iter()
        .map(|stream| {
            StreamInfo::new(
                stream.node_id,
                stream.size.0,
                stream.size.1,
                stream.position.0,
                stream.position.1,
            )
        })
        .collect();
    Ok((backend, streams))
}

impl PortalSessionBackend {
    /// Attach to a managed Wayland socket with full input + clipboard support.
    ///
    /// Retained for compatibility; delegates to the policy-aware constructor
    /// with both input and clipboard enabled.
    pub async fn create_session_for_wayland_socket(
        path: PathBuf,
        settings: crate::config::PortalStartupSettings,
    ) -> Result<Arc<dyn SessionHandle>> {
        Self::create_session_for_wayland_socket_with_policy(path, settings, true, true).await
    }

    /// Attach to a managed Wayland socket, constructing only the subsystems
    /// permitted by policy.
    ///
    /// When `enable_input` is `false` no input backend or input context is
    /// created, and injection methods report the subsystem as unavailable.
    /// When `enable_clipboard` is `false` no clipboard backend is created.
    /// Screen capture is always retained so view-only sessions still stream.
    pub async fn create_session_for_wayland_socket_with_policy(
        path: PathBuf,
        settings: crate::config::PortalStartupSettings,
        enable_input: bool,
        enable_clipboard: bool,
    ) -> Result<Arc<dyn SessionHandle>> {
        Self::create_session_for_wayland_socket_with_policy_and_peer_uid(
            path,
            settings,
            enable_input,
            enable_clipboard,
            None,
        )
        .await
    }

    /// Attach to a managed Wayland socket and require the compositor peer to
    /// have `expected_wayland_peer_uid` on each Wayland connection.
    pub async fn create_session_for_wayland_socket_with_policy_for_uid(
        path: PathBuf,
        settings: crate::config::PortalStartupSettings,
        enable_input: bool,
        enable_clipboard: bool,
        expected_wayland_peer_uid: u32,
    ) -> Result<Arc<dyn SessionHandle>> {
        Self::create_session_for_wayland_socket_with_policy_and_peer_uid(
            path,
            settings,
            enable_input,
            enable_clipboard,
            Some(expected_wayland_peer_uid),
        )
        .await
    }

    async fn create_session_for_wayland_socket_with_policy_and_peer_uid(
        path: PathBuf,
        settings: crate::config::PortalStartupSettings,
        enable_input: bool,
        enable_clipboard: bool,
        expected_wayland_peer_uid: Option<u32>,
    ) -> Result<Arc<dyn SessionHandle>> {
        let explicit_wayland_socket = path.clone();
        Self::create_session_with_wayland(
            move || match expected_wayland_peer_uid {
                Some(expected_uid) => {
                    WaylandConnection::connect_to_path_for_uid(&path, expected_uid)
                }
                None => WaylandConnection::connect_to_path(&path),
            },
            Some(explicit_wayland_socket),
            settings,
            enable_input,
            enable_clipboard,
            expected_wayland_peer_uid,
        )
        .await
    }

    async fn create_session_with_wayland<F>(
        connect_wayland: F,
        explicit_wayland_socket: Option<PathBuf>,
        settings: crate::config::PortalStartupSettings,
        enable_input: bool,
        enable_clipboard: bool,
        expected_wayland_peer_uid: Option<u32>,
    ) -> Result<Arc<dyn SessionHandle>>
    where
        F: FnOnce() -> crate::desktop::portal::xdg_desktop::Result<WaylandConnection>
            + Send
            + 'static,
    {
        info!(
            enable_input,
            enable_clipboard, "portal-generic: Creating session with embedded portal backend"
        );

        // All Wayland and PipeWire setup is synchronous; run on blocking thread
        let handle = tokio::task::spawn_blocking(move || -> Result<_> {
            let (wayland, protocols, sources) =
                prepare_wayland(connect_wayland, &settings.capture)?;
            let pipewire = Arc::new(PipeWireManager::disabled());
            let (raw_frame_tx, raw_frame_rx) = std::sync::mpsc::channel();
            let (stop, _, capture_tx, clipboard_tx, shared_clipboard, event_loop) = wayland
                .spawn_event_loop_with_frame_channel(Arc::clone(&pipewire), Some(raw_frame_tx));
            let mut rollback = ConstructionRollback::new(Arc::clone(&stop), event_loop);

            // Only build the input subsystem when policy permits it. View-only
            // sessions skip input backend/context creation entirely.
            let input = if enable_input {
                let (session_id, input_backend) = attach_input(
                    settings.input,
                    &protocols,
                    explicit_wayland_socket,
                    expected_wayland_peer_uid,
                )?;
                let input_backend = Arc::new(Mutex::new(input_backend));
                rollback.set_input(Arc::clone(&input_backend), session_id.clone());
                Some(PortalInput {
                    session_id,
                    backend: input_backend,
                })
            } else {
                info!("portal-generic: input disabled by policy; skipping input backend");
                None
            };

            let (capture_backend, streams) = start_monitor_capture(
                settings.capture,
                &protocols,
                sources,
                Arc::clone(&pipewire),
                capture_tx,
            )?;
            let capture_backend = Arc::new(Mutex::new(capture_backend));
            let capture_stream_ids: Vec<u32> =
                streams.iter().map(|stream| stream.node_id).collect();
            rollback.set_capture(Arc::clone(&capture_backend), capture_stream_ids.clone());

            // Only build the clipboard subsystem when policy permits it.
            let clipboard_backend = if enable_clipboard {
                create_clipboard_backend(
                    &protocols,
                    &settings.clipboard,
                    clipboard_tx,
                    shared_clipboard,
                )
                .map(|backend| Arc::new(Mutex::new(backend)))
            } else {
                info!("portal-generic: clipboard disabled by policy; skipping clipboard backend");
                None
            };

            let event_loop = rollback.disarm();
            Ok(PortalGenericSessionHandle {
                input,
                first_pointer_event: std::sync::atomic::AtomicBool::new(false),
                capture_backend,
                capture_stream_ids,
                clipboard_backend,
                _pipewire_manager: pipewire,
                streams: std::sync::Mutex::new(streams),
                frame_rx: std::sync::Mutex::new(Some(raw_frame_rx)),
                wayland_stop: stop,
                event_loop: std::sync::Mutex::new(Some(event_loop)),
                bridge_stop: Arc::new(AtomicBool::new(false)),
                bridge_handle: std::sync::Mutex::new(None),
                shutdown_done: AtomicBool::new(false),
                shutdown_lock: tokio::sync::Mutex::new(()),
                shutdown_failure: std::sync::Mutex::new(None),
                health_reporter: Arc::new(std::sync::RwLock::new(None)),
                pending_health_event: Arc::new(std::sync::Mutex::new(None)),
                health_reported: Arc::new(AtomicBool::new(false)),
            })
        })
        .await
        .context("portal-generic: Setup task panicked")??;

        Ok(Arc::new(handle))
    }
}

/// Optional input subsystem for a portal-generic session.
///
/// Present only when policy enables input injection. View-only sessions carry
/// `None`, and every injection path reports the subsystem as unavailable.
struct PortalInput {
    session_id: String,
    backend: Arc<Mutex<Box<dyn InputBackend>>>,
}

/// Session handle for the embedded portal-generic backend.
///
/// Bridges portal-generic backend traits to the `SessionHandle` interface
/// consumed by the RDP server session layer.
struct PortalGenericSessionHandle {
    /// Input backend + context id; `None` for view-only sessions.
    input: Option<PortalInput>,
    first_pointer_event: std::sync::atomic::AtomicBool,
    capture_backend: Arc<Mutex<Box<dyn crate::desktop::portal::xdg_desktop::CaptureBackend>>>,
    /// Capture stream/node IDs created at construction, used for teardown.
    capture_stream_ids: Vec<u32>,
    clipboard_backend:
        Option<Arc<Mutex<Box<dyn crate::desktop::portal::xdg_desktop::ClipboardBackend>>>>,
    _pipewire_manager: Arc<PipeWireManager>,
    streams: std::sync::Mutex<Vec<StreamInfo>>,
    /// Direct frame channel receiver (taken once by the display handler).
    frame_rx: std::sync::Mutex<
        Option<std::sync::mpsc::Receiver<crate::desktop::portal::xdg_desktop::RawFrame>>,
    >,
    wayland_stop: Arc<AtomicBool>,
    /// Wayland event-loop thread join handle, taken and joined by `shutdown`.
    event_loop: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Stop flag for the raw-frame bridge thread (checked in its recv loop).
    bridge_stop: Arc<AtomicBool>,
    /// Raw-frame bridge thread join handle, taken and joined by `shutdown`.
    bridge_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set once teardown has begun; guarantees idempotent teardown.
    shutdown_done: AtomicBool,
    /// Serializes shutdown callers while teardown awaits native thread joins.
    shutdown_lock: tokio::sync::Mutex<()>,
    /// Retains a terminal shutdown error for any later idempotent caller.
    shutdown_failure: std::sync::Mutex<Option<String>>,
    /// Health reporter shared with the raw-frame bridge. It can be attached
    /// after the bridge thread has already started, so it lives behind a
    /// shared lock rather than a move-once handle.
    health_reporter: Arc<std::sync::RwLock<Option<SessionStatusReporter>>>,
    /// First terminal event observed before supervision is attached.
    pending_health_event: Arc<std::sync::Mutex<Option<SessionStatusEvent>>>,
    /// One-shot guard coordinating the single closed/invalidated report across
    /// the shutdown, drop, and raw-frame bridge paths.
    health_reported: Arc<AtomicBool>,
}

impl Drop for PortalGenericSessionHandle {
    fn drop(&mut self) {
        // Best-effort stop only: signal both background loops but do not join
        // (joining belongs to `shutdown`, which runs on a blocking thread).
        self.wayland_stop.store(true, Ordering::Relaxed);
        self.bridge_stop.store(true, Ordering::Relaxed);
        debug!("portal-generic: Wayland event loop stop signaled");
        // Report closed exactly once across the shutdown/drop/bridge paths.
        report_health_once(
            &self.health_reporter,
            &self.pending_health_event,
            &self.health_reported,
            SessionStatusEvent::SessionClosed {
                reason: "portal-generic session dropped".into(),
            },
        );
    }
}

/// Get current time in microseconds for event timestamps.
fn current_time_usec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn normalize_absolute_pointer_position(
    streams: &[StreamInfo],
    stream_id: u32,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let stream = streams
        .iter()
        .find(|stream| stream.node_id == stream_id)
        .or_else(|| streams.first());

    let Some(stream) = stream else {
        return (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
    };

    let width = f64::from(stream.width.max(1));
    let height = f64::from(stream.height.max(1));
    ((x / width).clamp(0.0, 1.0), (y / height).clamp(0.0, 1.0))
}

/// Error returned by injection paths when the session has no input backend
/// (view-only policy). Kept as a named helper so the message stays consistent
/// and is unit-testable.
fn input_unavailable_error(operation: &str) -> anyhow::Error {
    anyhow::anyhow!("{operation}: input injection is not available for this view-only session")
}

impl PortalGenericSessionHandle {
    fn inject(&self, event: InputEvent, operation: &str) -> Result<()> {
        let input = self
            .input
            .as_ref()
            .ok_or_else(|| input_unavailable_error(operation))?;
        input
            .backend
            .lock()
            .map_err(|_| anyhow::anyhow!("input backend lock poisoned"))?
            .inject_event(&input.session_id, event)
            .map_err(|error| anyhow::anyhow!("{operation}: {error}"))
    }
}

#[async_trait]
impl SessionHandle for PortalGenericSessionHandle {
    fn set_health_reporter(&self, reporter: SessionStatusReporter) {
        install_health_reporter(
            &self.health_reporter,
            &self.pending_health_event,
            &self.health_reported,
            reporter,
        );
    }

    fn direct_frame_receiver(&self) -> Result<DirectFrameReceiver> {
        // Use direct frame channel — PipeWire buffer sharing doesn't work
        // across separate connections (the buffer data pointer is NULL on the
        // consumer side because the source's ALLOC_BUFFERS creates MemPtr
        // buffers that can't be shared across address spaces).
        let raw_rx = self
            .frame_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();

        let Some(raw_rx) = raw_rx else {
            anyhow::bail!("portal-generic direct frame channel was already taken");
        };

        info!("portal-generic: Using direct frame channel (bypassing PipeWire)");

        // Bridge RawFrame (portal crate) -> RawFrameData (pipewire crate).
        // Keep one output frame plus one replaceable pending frame. A plain
        // bounded channel's `try_send` would discard each *new* frame while
        // retaining stale queued frames, which makes latency grow during a
        // stall. Here newer capture replaces the pending frame instead.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let bridge_stop = Arc::clone(&self.bridge_stop);
        let health_reporter = Arc::clone(&self.health_reporter);
        let pending_health_event = Arc::clone(&self.pending_health_event);
        let health_reported = Arc::clone(&self.health_reported);
        let handle = std::thread::Builder::new()
            .name("raw-frame-bridge".into())
            .spawn(move || {
                let mut pending = None;
                let mut dropped: u64 = 0;
                loop {
                    if bridge_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Some(frame) = pending.take() {
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            Err(std::sync::mpsc::TrySendError::Full(frame)) => {
                                pending = Some(frame);
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                        }
                    }

                    match raw_rx.recv_timeout(std::time::Duration::from_millis(8)) {
                        Ok(raw) => {
                            let converted = crate::desktop::pipewire::frame::RawFrameData {
                                data: raw.data,
                                width: Some(raw.width),
                                height: Some(raw.height),
                                stride: Some(raw.stride),
                                format: None,
                            };
                            if pending.replace(converted).is_some() {
                                dropped = dropped.saturating_add(1);
                                if dropped == 1 || dropped.is_multiple_of(300) {
                                    warn!(
                                        "portal-generic: replaced {dropped} stale direct frames to keep latency low"
                                    );
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            // Never block on the output channel during teardown:
                            // attempt a single non-blocking flush of the pending
                            // frame and exit regardless of the result.
                            if let Some(frame) = pending.take() {
                                let _ = tx.try_send(frame);
                            }
                            // A disconnect while no stop was requested means the
                            // compositor/capture side vanished. Invalidate the
                            // session exactly once so supervision drives it
                            // Invalid. Intentional shutdown sets `bridge_stop`
                            // before the channel closes, so it is skipped here.
                            if !bridge_stop.load(Ordering::Relaxed) {
                                warn!(
                                    "portal-generic: raw frame channel disconnected unexpectedly; invalidating session"
                                );
                                report_health_once(
                                    &health_reporter,
                                    &pending_health_event,
                                    &health_reported,
                                    SessionStatusEvent::SessionInvalidated {
                                        reason: "portal-generic raw frame channel disconnected"
                                            .into(),
                                    },
                                );
                            }
                            break;
                        }
                    }
                }
                info!("portal-generic: raw-frame-bridge thread exited");
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn raw-frame-bridge thread: {e}"))?;

        *self
            .bridge_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

        Ok(rx)
    }

    fn streams(&self) -> Vec<StreamInfo> {
        self.streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_streams(&self, streams: Vec<StreamInfo>) {
        if let Some(input) = self.input.as_ref()
            && let Ok(mut backend) = input.backend.lock()
        {
            backend.set_stream_mappings(
                streams
                    .iter()
                    .map(
                        |stream| crate::desktop::portal::xdg_desktop::types::StreamOutputMapping {
                            stream_node_id: stream.node_id,
                            x: stream.position_x,
                            y: stream.position_y,
                            width: stream.width,
                            height: stream.height,
                        },
                    )
                    .collect(),
            );
        }
        *self
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = streams;
    }

    async fn notify_keyboard_keycode(&self, keycode: i32, pressed: bool) -> Result<()> {
        let event = InputEvent::Keyboard(KeyboardEvent {
            keycode: keycode as u32,
            state: if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            time_usec: current_time_usec(),
        });

        self.inject(event, "keyboard injection")
    }

    async fn notify_pointer_motion_absolute(&self, stream_id: u32, x: f64, y: f64) -> Result<()> {
        // The embedded portal path follows the RemoteDesktop portal contract:
        // absolute pointer coordinates are normalized 0.0–1.0 within the selected
        // PipeWire stream. wrdp's RDP coordinate transformer produces pixel
        // coordinates in stream space, so normalize them here before injecting
        // into the embedded backend. Without this, wlr_virtual_pointer receives
        // huge absolute values and the pointer appears inert/off-screen.
        let streams = self
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (x, y) = normalize_absolute_pointer_position(&streams, stream_id, x, y);
        if !self
            .first_pointer_event
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            info!(
                stream_id,
                normalized_x = x,
                normalized_y = y,
                "First normalized pointer event"
            );
        }

        let event = InputEvent::Pointer(PointerEvent::MotionAbsolute {
            x,
            y,
            stream: stream_id,
            time_usec: current_time_usec(),
        });

        self.inject(event, "absolute pointer injection")
    }

    async fn notify_pointer_button(&self, button: i32, pressed: bool) -> Result<()> {
        let event = InputEvent::Pointer(PointerEvent::Button {
            button: button as u32,
            state: if pressed {
                crate::desktop::portal::xdg_desktop::ButtonState::Pressed
            } else {
                crate::desktop::portal::xdg_desktop::ButtonState::Released
            },
            time_usec: current_time_usec(),
        });

        self.inject(event, "pointer button injection")
    }

    async fn notify_pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
        let event = InputEvent::Pointer(PointerEvent::Scroll {
            dx,
            dy,
            time_usec: current_time_usec(),
        });

        self.inject(event, "pointer scroll injection")
    }

    async fn shutdown(&self) -> Result<()> {
        let _shutdown_guard = self.shutdown_lock.lock().await;

        // Idempotent after success, while retaining a prior terminal failure.
        if !begin_shutdown_once(&self.shutdown_done) {
            if let Some(error) = self
                .shutdown_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                anyhow::bail!("{error}");
            }
            return Ok(());
        }

        let mut failures = Vec::new();

        // Destroy the input context for this session, if input was enabled.
        if let Some(input) = self.input.as_ref() {
            match input.backend.lock() {
                Ok(mut backend) => {
                    if let Err(error) = backend.destroy_context(&input.session_id) {
                        failures.push(format!("destroy input context: {error}"));
                    }
                }
                Err(_) => failures.push("input backend lock poisoned".to_string()),
            }
        }

        // Destroy the capture sessions/streams created at construction.
        if !self.capture_stream_ids.is_empty() {
            match self.capture_backend.lock() {
                Ok(mut backend) => {
                    if let Err(error) = backend.destroy_capture_session(&self.capture_stream_ids) {
                        failures.push(format!("destroy capture session: {error}"));
                    }
                }
                Err(_) => failures.push("capture backend lock poisoned".to_string()),
            }
        }

        // Signal the event loop and raw-frame bridge to stop.
        self.wayland_stop.store(true, Ordering::Relaxed);
        self.bridge_stop.store(true, Ordering::Relaxed);

        // Both native loops cooperatively poll their stop flags (Wayland at
        // roughly 100ms, bridge at 8ms). Join off-runtime, but bound the await:
        // a timeout is fatal so production exits rather than detaching a live
        // compositor loop and continuing to serve another connection.
        let event_loop = self
            .event_loop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let bridge_handle = self
            .bridge_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Use a standalone coordinator rather than Tokio's blocking pool. If a
        // native loop violates its cooperative-stop contract, production returns
        // a fatal error and process exit is not held open by a blocked Tokio
        // worker. On success the completion channel proves both joins finished.
        let (join_tx, join_rx) = tokio::sync::oneshot::channel();
        match std::thread::Builder::new()
            .name("portal-join-coordinator".into())
            .spawn(move || {
                let mut panics = Vec::new();
                if let Some(handle) = event_loop
                    && handle.join().is_err()
                {
                    panics.push("Wayland event-loop thread panicked");
                }
                if let Some(handle) = bridge_handle
                    && handle.join().is_err()
                {
                    panics.push("raw-frame bridge thread panicked");
                }
                let _ = join_tx.send(panics);
            }) {
            Ok(_coordinator) => {
                match tokio::time::timeout(NATIVE_THREAD_JOIN_TIMEOUT, join_rx).await {
                    Ok(Ok(panics)) => failures.extend(panics.into_iter().map(str::to_string)),
                    Ok(Err(recv_error)) => {
                        failures.push(format!(
                            "native join coordinator exited without a result: {recv_error}"
                        ));
                    }
                    Err(_) => {
                        failures.push(format!(
                            "native threads did not stop within {NATIVE_THREAD_JOIN_TIMEOUT:?}"
                        ));
                    }
                }
            }
            Err(error) => {
                failures.push(format!("failed to spawn native join coordinator: {error}"));
            }
        }

        // Report the session closed exactly once across shutdown/drop/bridge,
        // including teardown failures (which are returned to production).
        report_health_once(
            &self.health_reporter,
            &self.pending_health_event,
            &self.health_reported,
            SessionStatusEvent::SessionClosed {
                reason: "portal-generic session shutdown".into(),
            },
        );

        if failures.is_empty() {
            debug!("portal-generic: session shutdown complete");
            Ok(())
        } else {
            let error = format!(
                "portal-generic session shutdown failed: {}",
                failures.join("; ")
            );
            *self
                .shutdown_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
            anyhow::bail!(error)
        }
    }

    fn clipboard_source(&self) -> crate::rdp::session::backend::ClipboardSource {
        match self.clipboard_backend.as_ref() {
            Some(backend) => {
                crate::rdp::session::backend::ClipboardSource::DataControl(Arc::clone(backend))
            }
            None => crate::rdp::session::backend::ClipboardSource::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_current_time_usec() {
        let time = current_time_usec();
        assert!(time > 0);
    }

    #[test]
    fn begin_shutdown_once_is_idempotent() {
        let flag = AtomicBool::new(false);
        // First call wins and returns true; subsequent calls are no-ops.
        assert!(begin_shutdown_once(&flag));
        assert!(!begin_shutdown_once(&flag));
        assert!(!begin_shutdown_once(&flag));
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn one_pixel_is_not_misclassified_as_normalized() {
        let streams = vec![StreamInfo::new(7, 1920, 1080, 0, 0)];
        let (x, y) = normalize_absolute_pointer_position(&streams, 7, 1.0, 1.0);
        assert!((x - 1.0 / 1920.0).abs() < f64::EPSILON);
        assert!((y - 1.0 / 1080.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_normalize_absolute_pointer_position_from_pixels() {
        let streams = vec![StreamInfo::new(7, 1920, 1080, 0, 0)];

        let (x, y) = normalize_absolute_pointer_position(&streams, 7, 960.0, 540.0);

        assert!((x - 0.5).abs() < f64::EPSILON);
        assert!((y - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_normalize_absolute_pointer_position_clamps_without_streams() {
        let (x, y) = normalize_absolute_pointer_position(&[], 7, 2.0, -1.0);

        assert_eq!((x, y), (1.0, 0.0));
    }

    #[test]
    fn input_unavailable_error_mentions_view_only() {
        let error = input_unavailable_error("keyboard injection");
        let message = error.to_string();
        assert!(message.contains("keyboard injection"));
        assert!(message.contains("not available"));
        assert!(message.contains("view-only"));
    }

    #[tokio::test]
    async fn early_invalidation_replays_after_reporter_attachment() {
        use crate::rdp::session::supervision::{OverallStatus, SessionSupervisor};

        let health_reporter: Arc<std::sync::RwLock<Option<SessionStatusReporter>>> =
            Arc::new(std::sync::RwLock::new(None));
        let pending_event = Arc::new(Mutex::new(None));
        let reported = AtomicBool::new(false);

        report_health_once(
            &health_reporter,
            &pending_event,
            &reported,
            SessionStatusEvent::SessionInvalidated {
                reason: "early disconnect".into(),
            },
        );
        assert!(!reported.load(Ordering::Acquire));
        assert!(pending_event.lock().unwrap().is_some());

        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        let (monitor, reporter, subscriber) = SessionSupervisor::new(shutdown_tx.subscribe());
        let monitor_handle = tokio::spawn(monitor.run());
        install_health_reporter(&health_reporter, &pending_event, &reported, reporter);

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(reported.load(Ordering::Acquire));
        assert!(pending_event.lock().unwrap().is_none());
        assert_eq!(subscriber.current().overall, OverallStatus::Invalid);

        let _ = shutdown_tx.send(());
        let _ = monitor_handle.await;
    }

    #[tokio::test]
    async fn unexpected_disconnect_reports_invalidated_once() {
        use crate::rdp::session::supervision::{OverallStatus, SessionSupervisor};

        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        let (monitor, reporter, subscriber) = SessionSupervisor::new(shutdown_tx.subscribe());
        let monitor_handle = tokio::spawn(monitor.run());

        let reporter = Arc::new(std::sync::RwLock::new(Some(reporter)));
        let pending_event = Arc::new(Mutex::new(None));
        let reported = Arc::new(AtomicBool::new(false));

        // Simulate the raw-frame bridge observing an unexpected disconnect while
        // no stop was requested.
        report_health_once(
            &reporter,
            &pending_event,
            &reported,
            SessionStatusEvent::SessionInvalidated {
                reason: "portal-generic raw frame channel disconnected".into(),
            },
        );
        assert!(reported.load(Ordering::Relaxed));

        // A later shutdown/drop must not double-report.
        report_health_once(
            &reporter,
            &pending_event,
            &reported,
            SessionStatusEvent::SessionClosed {
                reason: "portal-generic session shutdown".into(),
            },
        );

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let state = subscriber.current();
        assert_eq!(state.overall, OverallStatus::Invalid);
        assert!(!subscriber.is_session_valid());

        let _ = shutdown_tx.send(());
        let _ = monitor_handle.await;
    }
}
