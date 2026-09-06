//! Wayland client connection and protocol management.
//!
//! Provides the shared Wayland runtime boundary used by portal capture,
//! clipboard, and input backends. The connection discovers/binds protocol
//! globals, maintains shared output state, and drives event-loop dispatch.

pub mod data_control;
pub mod dispatch;
pub mod ext_capture;
pub mod globals;
pub mod screencopy;

use std::{
    os::{
        unix::io::{AsFd, AsRawFd},
        unix::net::UnixStream,
    },
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use data_control::DataControlManager;
pub use data_control::{ClipboardCommand, SharedClipboardState};
pub use dispatch::{OutputInfo, WaylandState};
pub use globals::AvailableProtocols;
use nix::sys::socket::{getsockopt, sockopt};
use wayland_client::{
    Connection, EventQueue, QueueHandle,
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_output::WlOutput, wl_seat::WlSeat, wl_shm::WlShm},
};
use wayland_protocols::ext::{
    data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1,
    image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_wlr::{
    data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
};

use crate::desktop::portal::xdg_desktop::{
    error::PortalError, pipewire::PipeWireManager, types::CursorMode,
};

/// Raw pixel data from a single-frame screenshot capture.
pub struct ScreenshotData {
    /// Raw pixel data (format depends on compositor, typically BGRx/ARGB).
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// Pixel format as wl_shm format enum value.
    pub format_raw: u32,
}

impl std::fmt::Debug for ScreenshotData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenshotData")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("format_raw", &self.format_raw)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Commands sent from capture backends to the Wayland event loop thread
/// for managing screencopy frame capture.
#[non_exhaustive]
pub enum CaptureCommand {
    /// Start capturing frames from an output into a PipeWire stream.
    StartCapture {
        /// wl_output global name identifying the output.
        output_global_name: u32,
        /// PipeWire node ID of the stream to deliver frames to.
        node_id: u32,
        /// Width of the capture.
        width: u32,
        /// Height of the capture.
        height: u32,
        /// Cursor mode for the capture.
        cursor_mode: CursorMode,
    },
    /// Stop capturing frames for a PipeWire stream.
    StopCapture {
        /// PipeWire node ID of the stream to stop.
        node_id: u32,
    },
    /// Capture a single screenshot frame (one-shot, no PipeWire stream).
    CaptureScreenshot {
        /// wl_output global name identifying the output.
        output_global_name: u32,
        /// Reply channel for the captured frame data.
        reply: tokio::sync::oneshot::Sender<std::result::Result<ScreenshotData, String>>,
    },
}

impl std::fmt::Debug for CaptureCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StartCapture {
                output_global_name,
                node_id,
                width,
                height,
                ..
            } => f
                .debug_struct("StartCapture")
                .field("output_global_name", output_global_name)
                .field("node_id", node_id)
                .field("width", width)
                .field("height", height)
                .finish(),
            Self::StopCapture { node_id } => f
                .debug_struct("StopCapture")
                .field("node_id", node_id)
                .finish(),
            Self::CaptureScreenshot {
                output_global_name, ..
            } => f
                .debug_struct("CaptureScreenshot")
                .field("output_global_name", output_global_name)
                .finish(),
        }
    }
}

/// Result type for Wayland operations.
pub type Result<T> = std::result::Result<T, PortalError>;

/// Verify that a connected Unix socket's peer has the expected Linux UID.
///
/// `SO_PEERCRED` identifies the process at the other end of this already
/// connected socket, avoiding any trust decision based solely on the socket
/// path or its filesystem metadata.
pub fn verify_unix_peer_uid(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    let actual_uid = getsockopt(stream, sockopt::PeerCredentials)
        .map_err(|error| {
            PortalError::Config(format!(
                "Failed to read Unix socket peer credentials: {error}"
            ))
        })?
        .uid();

    if actual_uid != expected_uid {
        return Err(PortalError::PermissionDenied(format!(
            "Wayland socket peer UID mismatch: expected {expected_uid}, got {actual_uid}"
        )));
    }

    Ok(())
}

/// Shared Wayland client connection.
///
/// Main entry point for Wayland protocol interactions across portal backend
/// services.
///
/// `WaylandState` is mirrored into shared mutex-protected state for backend
/// readers while the non-`Send` event queue remains on its owning thread.
pub struct WaylandConnection {
    /// The underlying Wayland connection.
    connection: Connection,
    /// The global list from initial registry scan.
    globals: GlobalList,
    /// Event queue for dispatching protocol events.
    event_queue: EventQueue<WaylandState>,
    /// Queue handle for creating protocol objects.
    queue_handle: QueueHandle<WaylandState>,
    /// Shared Wayland state with all bound globals.
    state: WaylandState,
    /// Detected available protocols.
    available_protocols: AvailableProtocols,
    /// Shared state for cross-thread access (backends read this).
    shared_state: Arc<Mutex<SharedWaylandState>>,
    /// When true, force wlr-screencopy even if ext-image-copy-capture is bound.
    /// Set by the service layer when the compositor profile recommends wlr.
    force_wlr_screencopy: bool,
}

/// Shared state that is safe to read from other threads.
///
/// Updated by the event loop thread after each dispatch cycle.
/// Backends read this to get current output info, etc.
#[derive(Debug, Default)]
pub struct SharedWaylandState {
    /// Current output sources (updated after each dispatch).
    pub sources: Vec<crate::desktop::portal::xdg_desktop::types::SourceInfo>,
}

impl WaylandConnection {
    /// Connect to the Wayland compositor.
    ///
    /// Connects via `$WAYLAND_DISPLAY`, performs an initial registry roundtrip
    /// to discover globals, and detects available protocols.
    pub fn connect() -> Result<Self> {
        tracing::info!("Connecting to Wayland compositor from environment...");

        // Connect to Wayland
        let connection = Connection::connect_to_env().map_err(|e| {
            PortalError::Config(format!(
                "Failed to connect to Wayland compositor: {e}. Is WAYLAND_DISPLAY set?"
            ))
        })?;

        Self::from_connection(connection)
    }

    /// Connect to an explicit Wayland Unix socket path.
    ///
    /// This compatibility entry point does not constrain the peer UID. Managed
    /// sessions should use [`Self::connect_to_path_for_uid`] instead.
    pub fn connect_to_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::connect_to_path_inner(path.as_ref(), None)
    }

    /// Connect to an explicit Wayland Unix socket path owned by `expected_uid`.
    ///
    /// The connected socket's peer credentials are verified before its file
    /// descriptor is handed to the Wayland client library.
    pub fn connect_to_path_for_uid(path: impl AsRef<Path>, expected_uid: u32) -> Result<Self> {
        Self::connect_to_path_inner(path.as_ref(), Some(expected_uid))
    }

    fn connect_to_path_inner(path: &Path, expected_uid: Option<u32>) -> Result<Self> {
        tracing::info!("Connecting to Wayland compositor");
        let stream = UnixStream::connect(path).map_err(|e| {
            PortalError::Config(format!(
                "Failed to connect to configured Wayland socket: {e}"
            ))
        })?;
        if let Some(expected_uid) = expected_uid {
            verify_unix_peer_uid(&stream, expected_uid)?;
        }
        let connection = Connection::from_socket(stream).map_err(|e| {
            PortalError::Config(format!(
                "Failed to initialize Wayland connection from {}: {e}",
                path.display()
            ))
        })?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        // Initialize registry
        let (globals, mut event_queue) =
            registry_queue_init::<WaylandState>(&connection).map_err(|e| {
                PortalError::Config(format!("Failed to initialize Wayland registry: {e}"))
            })?;

        let queue_handle = event_queue.handle();
        let mut state = WaylandState::default();

        // Detect and bind available protocols
        let available_protocols =
            Self::detect_and_bind_globals(&globals, &queue_handle, &mut state);

        // Do an initial roundtrip to receive output info etc.
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| PortalError::Config(format!("Wayland roundtrip failed: {e}")))?;

        state.initialized = true;
        available_protocols.log_summary();

        // Build initial shared state
        let shared_state = Arc::new(Mutex::new(SharedWaylandState {
            sources: state.get_sources(),
        }));

        tracing::info!("Wayland connection established");

        Ok(Self {
            connection,
            globals,
            event_queue,
            queue_handle,
            state,
            available_protocols,
            shared_state,
            force_wlr_screencopy: false,
        })
    }

    /// Detect available protocols from globals and bind them.
    #[expect(
        clippy::too_many_lines,
        reason = "sequential global binding with per-protocol error handling is inherently verbose"
    )]
    fn detect_and_bind_globals(
        globals: &GlobalList,
        qh: &QueueHandle<WaylandState>,
        state: &mut WaylandState,
    ) -> AvailableProtocols {
        let mut protocols = AvailableProtocols::default();

        // === Input protocols ===

        // Virtual pointer manager
        match globals.bind::<ZwlrVirtualPointerManagerV1, _, _>(qh, 1..=2, ()) {
            Ok(manager) => {
                tracing::debug!("Bound zwlr_virtual_pointer_manager_v1");
                state.pointer_manager = Some(manager);
                protocols.wlr_virtual_pointer = true;
            }
            Err(e) => {
                tracing::debug!("zwlr_virtual_pointer_manager_v1 not available: {}", e);
            }
        }

        // Virtual keyboard manager
        match globals.bind::<ZwpVirtualKeyboardManagerV1, _, _>(qh, 1..=1, ()) {
            Ok(manager) => {
                tracing::debug!("Bound zwp_virtual_keyboard_manager_v1");
                state.keyboard_manager = Some(manager);
                protocols.zwp_virtual_keyboard = true;
            }
            Err(e) => {
                tracing::debug!("zwp_virtual_keyboard_manager_v1 not available: {}", e);
            }
        }

        // === Seat ===
        match globals.bind::<WlSeat, _, _>(qh, 1..=9, ()) {
            Ok(seat) => {
                tracing::debug!("Bound wl_seat");
                state.seat = Some(seat);
                protocols.seat = true;
            }
            Err(e) => {
                tracing::debug!("wl_seat not available: {}", e);
            }
        }

        // === SHM (required for screencopy/ext-capture SHM buffers) ===
        match globals.bind::<WlShm, _, _>(qh, 1..=1, ()) {
            Ok(shm) => {
                tracing::debug!("Bound wl_shm");
                state.screencopy.shm = Some(shm.clone());
                state.ext_capture.shm = Some(shm);
            }
            Err(e) => {
                tracing::debug!("wl_shm not available: {}", e);
            }
        }

        // === linux-dmabuf (preferred wlr-screencopy buffer transport) ===
        match globals.bind::<ZwpLinuxDmabufV1, _, _>(qh, 3..=4, ()) {
            Ok(dmabuf) => {
                tracing::debug!("Bound zwp_linux_dmabuf_v1");
                state.screencopy.linux_dmabuf = Some(dmabuf);
            }
            Err(e) => {
                tracing::debug!("zwp_linux_dmabuf_v1 not available: {}", e);
            }
        }

        // === Screencopy manager ===
        match globals.bind::<ZwlrScreencopyManagerV1, _, _>(qh, 1..=3, ()) {
            Ok(manager) => {
                // Track the bound version for v1/v2 vs v3 feature differences.
                // v3 adds buffer_done + linux_dmabuf events.
                let version = wayland_client::Proxy::version(&manager);
                tracing::debug!(version, "Bound zwlr_screencopy_manager_v1");
                state.screencopy.manager_version = version;
                state.screencopy.manager = Some(manager);
                protocols.wlr_screencopy = true;
            }
            Err(e) => {
                tracing::debug!("zwlr_screencopy_manager_v1 not available: {}", e);
            }
        }

        // === ext-image-copy-capture (preferred capture protocol) ===
        match globals.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(qh, 1..=1, ()) {
            Ok(manager) => {
                tracing::debug!("Bound ext_output_image_capture_source_manager_v1");
                state.ext_capture.source_manager = Some(manager);
            }
            Err(e) => {
                tracing::debug!(
                    "ext_output_image_capture_source_manager_v1 not available: {}",
                    e
                );
            }
        }

        match globals.bind::<ExtImageCopyCaptureManagerV1, _, _>(qh, 1..=1, ()) {
            Ok(manager) => {
                tracing::debug!("Bound ext_image_copy_capture_manager_v1");
                state.ext_capture.manager = Some(manager);
                protocols.ext_image_copy_capture = true;
            }
            Err(e) => {
                tracing::debug!("ext_image_copy_capture_manager_v1 not available: {}", e);
            }
        }

        // === Data control (clipboard) ===
        // Prefer ext-data-control-v1, fall back to wlr-data-control-v1.
        // Only bind one; they have identical semantics.
        let mut data_control_bound = false;

        match globals.bind::<ExtDataControlManagerV1, _, _>(qh, 1..=1, ()) {
            Ok(manager) => {
                tracing::debug!("Bound ext_data_control_manager_v1");
                state.data_control.manager = Some(DataControlManager::Ext(manager));
                protocols.ext_data_control = true;
                data_control_bound = true;
            }
            Err(e) => {
                tracing::debug!("ext_data_control_manager_v1 not available: {}", e);
            }
        }

        if !data_control_bound {
            match globals.bind::<ZwlrDataControlManagerV1, _, _>(qh, 1..=2, ()) {
                Ok(manager) => {
                    tracing::debug!("Bound zwlr_data_control_manager_v1");
                    state.data_control.manager = Some(DataControlManager::Wlr(manager));
                    protocols.wlr_data_control = true;
                    data_control_bound = true;
                }
                Err(e) => {
                    tracing::debug!("zwlr_data_control_manager_v1 not available: {}", e);
                }
            }
        }

        // Create data control device from seat (after seat is bound above)
        if data_control_bound {
            if let Some(ref seat) = state.seat {
                state.data_control.create_device(seat, qh);
            } else {
                tracing::warn!(
                    "Data control manager bound but no seat available for device creation"
                );
            }
        }

        // === Outputs and protocol detection ===
        // Scan all globals to detect protocols and bind outputs
        let contents = globals.contents();
        let mut output_names = Vec::new();

        contents.with_list(|global_list| {
            for global in global_list {
                match global.interface.as_str() {
                    "wl_output" => {
                        output_names.push(global.name);
                    }
                    "ext_image_copy_capture_manager_v1" => {
                        protocols.ext_image_copy_capture = true;
                        tracing::debug!("Found ext_image_copy_capture_manager_v1");
                    }
                    "zwlr_screencopy_manager_v1" => {
                        // Bound above via globals.bind(); flag already set there.
                        // Only set the flag here if binding failed (detection only).
                        if !protocols.wlr_screencopy {
                            tracing::debug!("Found zwlr_screencopy_manager_v1 (not bound)");
                        }
                    }
                    "ext_data_control_manager_v1" => {
                        // Already handled above via globals.bind()
                        if !protocols.ext_data_control {
                            tracing::debug!("Found ext_data_control_manager_v1 (not bound)");
                        }
                    }
                    "zwlr_data_control_manager_v1"
                        // Already handled above via globals.bind()
                        if !protocols.wlr_data_control => {
                            tracing::debug!("Found zwlr_data_control_manager_v1 (not bound)");
                        }
                    _ => {}
                }
            }
        });

        protocols.output_count = output_names.len() as u32;

        // Bind wl_output globals
        for name in &output_names {
            let info = Arc::new(Mutex::new(OutputInfo {
                global_name: *name,
                ..Default::default()
            }));

            match globals.bind::<WlOutput, _, _>(qh, 1..=4, info.clone()) {
                Ok(output) => {
                    state.outputs.push((output, info));
                }
                Err(e) => {
                    tracing::warn!("Failed to bind wl_output {}: {}", name, e);
                }
            }
        }

        protocols
    }

    /// Get detected available protocols.
    pub fn available_protocols(&self) -> &AvailableProtocols {
        &self.available_protocols
    }

    /// Get a reference to the Wayland state.
    pub fn state(&self) -> &WaylandState {
        &self.state
    }

    /// Get a mutable reference to the Wayland state.
    pub fn state_mut(&mut self) -> &mut WaylandState {
        &mut self.state
    }

    /// Force wlr-screencopy even when ext-image-copy-capture is available.
    ///
    /// Use when the compositor advertises ext-capture but it doesn't work
    /// properly (e.g., missing SHM formats in constraints).
    pub fn set_force_wlr_screencopy(&mut self, force: bool) {
        self.force_wlr_screencopy = force;
    }

    /// Set the ext-capture handshake timeout.
    ///
    /// If a capture session doesn't receive constraint events (`buffer_size`,
    /// `shm_format`, `done`) within this duration, the session is considered
    /// failed. Zero means no timeout (not recommended).
    pub fn set_ext_capture_handshake_timeout(&mut self, timeout: std::time::Duration) {
        self.state.ext_capture.handshake_timeout = timeout;
    }

    /// Get the queue handle for creating protocol objects.
    pub fn queue_handle(&self) -> &QueueHandle<WaylandState> {
        &self.queue_handle
    }

    /// Get the global list.
    pub fn globals(&self) -> &GlobalList {
        &self.globals
    }

    /// Get the raw Wayland connection.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Get the Wayland connection fd for async polling.
    pub fn fd(&self) -> std::os::unix::io::RawFd {
        self.connection.as_fd().as_raw_fd()
    }

    /// Flush pending Wayland requests.
    pub fn flush(&self) -> Result<()> {
        self.connection
            .flush()
            .map_err(|e| PortalError::Config(format!("Wayland flush error: {e}")))?;
        Ok(())
    }

    /// Dispatch pending Wayland events.
    pub fn dispatch_pending(&mut self) -> Result<usize> {
        let dispatched = self
            .event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| PortalError::Config(format!("Wayland dispatch error: {e}")))?;

        // Update shared state after dispatch
        self.update_shared_state();

        Ok(dispatched)
    }

    /// Perform a blocking roundtrip.
    pub fn roundtrip(&mut self) -> Result<usize> {
        let dispatched = self
            .event_queue
            .roundtrip(&mut self.state)
            .map_err(|e| PortalError::Config(format!("Wayland roundtrip error: {e}")))?;

        // Update shared state after roundtrip
        self.update_shared_state();

        Ok(dispatched)
    }

    /// Get the shared state handle for cross-thread access.
    pub fn shared_state(&self) -> Arc<Mutex<SharedWaylandState>> {
        Arc::clone(&self.shared_state)
    }

    /// Update the shared state from the local state.
    fn update_shared_state(&self) {
        if let Ok(mut shared) = self.shared_state.lock() {
            shared.sources = self.state.get_sources();
        }
    }

    /// Run the Wayland event loop on the current thread.
    ///
    /// Consumes the connection and runs a blocking dispatch loop. Shared
    /// state is updated each cycle, and capture/clipboard commands are
    /// processed inline.
    ///
    /// # Arguments
    ///
    /// * `stop` - Atomic flag to signal the loop to stop.
    /// * `capture_rx` - Receiver for capture commands from backends.
    ///
    /// Call this from a dedicated `std::thread::spawn` — NOT from tokio,
    /// because `EventQueue` is `!Send` and must stay on the creating thread.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "ownership transfer required for thread safety — these are moved into the spawned thread"
    )]
    pub fn run_event_loop(
        mut self,
        stop: Arc<AtomicBool>,
        capture_rx: mpsc::Receiver<CaptureCommand>,
        clipboard_rx: mpsc::Receiver<ClipboardCommand>,
    ) {
        tracing::info!("Starting Wayland event loop");

        let fd = self.connection.as_fd().as_raw_fd();

        while !stop.load(Ordering::Relaxed) {
            // Poll the Wayland fd with a 100ms timeout
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            // SAFETY: pollfd is a valid local struct, nfds=1, timeout=100ms.
            // poll() is a standard POSIX syscall with no memory safety concerns here.
            #[expect(
                unsafe_code,
                reason = "poll() is a standard POSIX syscall with a valid local pollfd struct"
            )]
            let poll_result = unsafe { libc::poll(&raw mut pollfd, 1, 100) };

            if poll_result < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::error!("Wayland event loop poll error: {}", err);
                break;
            }

            // Process capture commands from backends
            self.process_capture_commands(&capture_rx);

            // Process clipboard commands from backends
            self.process_clipboard_commands(&clipboard_rx);

            // Check for ext-capture handshake timeouts
            let timeout = self.state.ext_capture.handshake_timeout;
            let timed_out = self.state.ext_capture.check_handshake_timeouts(timeout);
            for node_id in timed_out {
                self.state.ext_capture.stop_capture(node_id);
            }

            // Read new events from the Wayland socket into the internal buffer,
            // then dispatch them to handlers. prepare_read() + read_events()
            // is required — dispatch_pending() alone only processes events
            // already in the buffer from previous reads.
            if pollfd.revents & libc::POLLIN != 0
                && let Some(guard) = self.event_queue.prepare_read()
            {
                match guard.read() {
                    Ok(_) => {}
                    // WouldBlock can occur if another thread consumed the
                    // pending data between poll() and read(). This is benign
                    // — wayland-client's own blocking_read() handles it the
                    // same way (returns Ok(0) and lets the caller retry).
                    Err(wayland_client::backend::WaylandError::Io(ref e))
                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        tracing::trace!("Wayland read returned WouldBlock, retrying");
                    }
                    Err(e) => {
                        tracing::error!("Wayland read_events error: {}", e);
                        break;
                    }
                }
            }

            match self.event_queue.dispatch_pending(&mut self.state) {
                Ok(_) => {
                    self.update_shared_state();
                }
                Err(e) => {
                    tracing::error!("Wayland dispatch error: {}", e);
                    break;
                }
            }

            // Flush outgoing requests
            match self.connection.flush() {
                Ok(()) => {}
                Err(wayland_client::backend::WaylandError::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    tracing::trace!("Wayland flush returned WouldBlock, will retry next cycle");
                }
                Err(e) => {
                    tracing::error!("Wayland flush error: {}", e);
                    break;
                }
            }
        }

        tracing::info!("Wayland event loop stopped");
    }

    /// Check if the ext-image-copy-capture protocol is available.
    fn has_ext_capture(&self) -> bool {
        self.state.ext_capture.manager.is_some() && self.state.ext_capture.source_manager.is_some()
    }

    /// Find a WlOutput matching the given global name.
    fn find_output(&self, output_global_name: u32) -> Option<WlOutput> {
        self.state.outputs.iter().find_map(|(output, info)| {
            let info = info.lock().ok()?;
            if info.global_name == output_global_name {
                Some(output.clone())
            } else {
                None
            }
        })
    }

    /// Process pending capture commands from backends.
    ///
    /// Routes to ext-image-copy-capture when available, falling back to
    /// wlr-screencopy.
    fn process_capture_commands(&mut self, capture_rx: &mpsc::Receiver<CaptureCommand>) {
        let use_ext = self.has_ext_capture() && !self.force_wlr_screencopy;

        while let Ok(cmd) = capture_rx.try_recv() {
            match cmd {
                CaptureCommand::StartCapture {
                    output_global_name,
                    node_id,
                    width: _,
                    height: _,
                    cursor_mode,
                } => {
                    let output = self.find_output(output_global_name);
                    match output {
                        Some(output) => {
                            if use_ext {
                                let paint_cursors = matches!(cursor_mode, CursorMode::Embedded);
                                self.state.ext_capture.start_capture(
                                    &self.queue_handle,
                                    output,
                                    output_global_name,
                                    node_id,
                                    paint_cursors,
                                );
                            } else {
                                self.state.screencopy.start_capture(
                                    &self.queue_handle,
                                    output,
                                    output_global_name,
                                    node_id,
                                    cursor_mode,
                                );
                            }
                        }
                        None => {
                            tracing::error!(
                                output_global_name,
                                "Cannot start capture: output not found"
                            );
                        }
                    }
                }
                CaptureCommand::StopCapture { node_id } => {
                    if use_ext {
                        self.state.ext_capture.stop_capture(node_id);
                    } else {
                        self.state.screencopy.stop_capture(node_id);
                    }
                }
                CaptureCommand::CaptureScreenshot {
                    output_global_name,
                    reply,
                } => {
                    let output = self.find_output(output_global_name);
                    match output {
                        Some(output) => {
                            if use_ext {
                                self.state.ext_capture.start_screenshot(
                                    &self.queue_handle,
                                    output,
                                    output_global_name,
                                    reply,
                                );
                            } else {
                                self.state.screencopy.start_screenshot(
                                    &self.queue_handle,
                                    output,
                                    output_global_name,
                                    reply,
                                );
                            }
                        }
                        None => {
                            let _ =
                                reply.send(Err(format!("Output not found: {output_global_name}")));
                        }
                    }
                }
            }
        }
    }

    /// Process pending clipboard commands from backends.
    fn process_clipboard_commands(&mut self, clipboard_rx: &mpsc::Receiver<ClipboardCommand>) {
        while let Ok(cmd) = clipboard_rx.try_recv() {
            match cmd {
                ClipboardCommand::SetSelection { mime_types, data } => {
                    self.state
                        .data_control
                        .set_selection(&mime_types, data, &self.queue_handle);
                }
                ClipboardCommand::UpdateSourceData { mime_type, data } => {
                    self.state.data_control.update_source_data(mime_type, data);
                }
                ClipboardCommand::ReceiveFromOffer { mime_type, fd } => {
                    self.state.data_control.receive_from_offer(&mime_type, fd);
                }
            }
        }
    }

    /// Spawn the Wayland event loop on a dedicated OS thread.
    ///
    /// Returns a stop flag, the shared Wayland state, a capture command sender,
    /// a clipboard command sender, the shared clipboard state, and the thread
    /// join handle. The connection is consumed and moved to the new thread.
    ///
    /// The `pipewire` manager is given to the event loop so it can send
    /// captured frames directly to PipeWire streams without an extra hop.
    #[expect(
        clippy::type_complexity,
        reason = "6-tuple return is the minimum needed to expose all event loop handles"
    )]
    pub fn spawn_event_loop(
        self,
        pipewire: Arc<PipeWireManager>,
    ) -> (
        Arc<AtomicBool>,
        Arc<Mutex<SharedWaylandState>>,
        mpsc::Sender<CaptureCommand>,
        mpsc::Sender<ClipboardCommand>,
        Arc<Mutex<SharedClipboardState>>,
        thread::JoinHandle<()>,
    ) {
        self.spawn_event_loop_with_frame_channel(pipewire, None)
    }

    /// Spawn the event loop with an optional direct frame channel.
    ///
    /// When `frame_tx` is provided, screencopy frames are sent through this
    /// channel instead of PipeWire. This bypasses PipeWire buffer sharing
    /// which doesn't work across separate PipeWire connections.
    #[expect(
        clippy::type_complexity,
        reason = "shared backend ownership and event-loop return contract are explicit here"
    )]
    #[expect(
        clippy::expect_used,
        reason = "internal invariant established during construction or by guarded control flow"
    )]
    pub fn spawn_event_loop_with_frame_channel(
        mut self,
        pipewire: Arc<PipeWireManager>,
        frame_tx: Option<
            std::sync::mpsc::Sender<
                crate::desktop::portal::xdg_desktop::wayland::screencopy::RawFrame,
            >,
        >,
    ) -> (
        Arc<AtomicBool>,
        Arc<Mutex<SharedWaylandState>>,
        mpsc::Sender<CaptureCommand>,
        mpsc::Sender<ClipboardCommand>,
        Arc<Mutex<SharedClipboardState>>,
        thread::JoinHandle<()>,
    ) {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let shared = self.shared_state();
        let (capture_tx, capture_rx) = mpsc::channel();
        let (clipboard_tx, clipboard_rx) = mpsc::channel();

        // Get a handle to the shared clipboard state before moving self
        let shared_clipboard = Arc::clone(&self.state.data_control.shared_state);

        // Give the PipeWire manager to the capture states so the event loop
        // can send frame data directly to PipeWire.
        self.state.screencopy.pipewire = Some(Arc::clone(&pipewire));
        self.state.ext_capture.pipewire = Some(pipewire);

        // Wire direct frame channel if provided
        self.state.screencopy.frame_tx = frame_tx;

        tracing::debug!(
            timeout_ms = self.state.ext_capture.handshake_timeout.as_millis() as u64,
            "ext-capture handshake timeout configured"
        );

        let handle = thread::Builder::new()
            .name("wayland-event-loop".to_string())
            .spawn(move || {
                self.run_event_loop(stop_clone, capture_rx, clipboard_rx);
            })
            .expect("Failed to spawn Wayland event loop thread");

        (
            stop,
            shared,
            capture_tx,
            clipboard_tx,
            shared_clipboard,
            handle,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        thread,
    };

    use nix::unistd::Uid;

    use super::verify_unix_peer_uid;

    #[test]
    fn unix_peer_uid_verification_accepts_current_uid_and_rejects_another() {
        let directory = tempfile::tempdir().expect("create temporary socket directory");
        let socket_path = directory.path().join("wayland-peer-uid.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind Unix listener");
        let client_path = socket_path.clone();
        let client = thread::spawn(move || UnixStream::connect(client_path));
        let (stream, _) = listener.accept().expect("accept Unix connection");
        let _client = client
            .join()
            .expect("Unix client thread panicked")
            .expect("connect Unix client");

        let current_uid = Uid::effective().as_raw();
        assert!(verify_unix_peer_uid(&stream, current_uid).is_ok());

        let other_uid = current_uid
            .checked_add(1)
            .unwrap_or_else(|| current_uid - 1);
        assert!(verify_unix_peer_uid(&stream, other_uid).is_err());
    }
}
