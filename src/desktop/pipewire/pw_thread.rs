//! PipeWire thread manager.
//!
//! Owns PipeWire main-loop state on a dedicated thread and bridges async
//! runtime code to that thread via command/frame channels.
//!
//! ## Runtime model
//!
//! - dedicated thread owns MainLoop/Context/Core/Stream values
//! - callers send lifecycle commands through a bounded channel
//! - frame payloads and stream-state events are sent back through channels
//! - unsafe `Send`/`Sync` assumptions are limited to this wrapper boundary

use pipewire::properties::PropertiesBox;
use pipewire::spa::param::ParamType;
use pipewire::spa::param::format_utils;
use pipewire::spa::param::video::VideoInfoRaw;
use pipewire::spa::pod::Pod;
use pipewire::spa::utils::Direction;
use pipewire::stream::{StreamBox, StreamFlags, StreamState};
use pipewire::{context::ContextBox, main_loop::MainLoopBox};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};

use crate::desktop::pipewire::error::{PipeWireError, Result};
use crate::desktop::pipewire::format::PixelFormat;
use crate::desktop::pipewire::frame::{FrameFlags, VideoFrame};
use crate::desktop::pipewire::stream::{PwStreamState, StreamConfig, StreamStateEvent};
use std::sync::Arc as StdArc;
use std::time::SystemTime;

/// Commands sent to the PipeWire thread
pub enum PipeWireThreadCommand {
    /// Create and connect a stream to a PipeWire node
    CreateStream {
        stream_id: u32,
        node_id: u32,
        config: StreamConfig,
        /// Response channel
        response_tx: std_mpsc::SyncSender<Result<()>>,
    },

    /// Destroy a stream
    DestroyStream {
        stream_id: u32,
        response_tx: std_mpsc::SyncSender<Result<()>>,
    },

    /// Get stream state
    GetStreamState {
        stream_id: u32,
        response_tx: std_mpsc::SyncSender<Option<StreamState>>,
    },

    /// Shutdown the PipeWire thread
    Shutdown,
}

/// Stream data managed on PipeWire thread
///
/// Some fields are prepared for future functionality (metrics, stats).
#[expect(dead_code)]
struct ManagedStream {
    /// Stream ID
    id: u32,

    /// PipeWire stream (lives on PipeWire thread only)
    /// SAFETY: 'static lifetime is safe because we manually enforce drop order:
    /// streams are cleared before core is dropped in run_pipewire_main_loop().
    stream: StreamBox<'static>,

    /// Stream event listener (must be kept alive)
    _listener: pipewire::stream::StreamListener<()>,

    /// Configuration
    config: StreamConfig,

    /// Current state
    state: StreamState,

    /// Frame counter
    frame_count: u64,

    /// Frame channel for sending captured frames
    frame_tx: std_mpsc::SyncSender<VideoFrame>,
}

/// PipeWire thread manager
///
/// Manages a dedicated thread that runs the PipeWire MainLoop and handles
/// all PipeWire API operations. Communicates with async code via channels.
pub struct PipeWireThreadManager {
    /// Thread handle
    thread_handle: Option<JoinHandle<()>>,

    /// Command channel sender
    command_tx: std_mpsc::SyncSender<PipeWireThreadCommand>,

    /// Frame channel receiver
    frame_rx: std_mpsc::Receiver<VideoFrame>,

    /// Stream state event receiver (state changes from PipeWire callbacks)
    state_event_rx: std_mpsc::Receiver<StreamStateEvent>,

    /// Shutdown flag
    shutdown_tx: Option<std_mpsc::SyncSender<()>>,
}

impl PipeWireThreadManager {
    /// Create and start PipeWire thread manager
    ///
    /// # Arguments
    ///
    /// * `fd` - File descriptor from portal
    ///
    /// # Returns
    ///
    /// A new PipeWireThreadManager with running thread
    ///
    /// # Errors
    ///
    /// Returns error if thread creation fails
    pub fn new(fd: RawFd) -> Result<Self> {
        info!("Creating PipeWire thread manager for FD {}", fd);

        // Create channels for commands and frames
        // Using std::sync::mpsc (not tokio) because PipeWire thread is not async
        let (command_tx, command_rx) = std_mpsc::sync_channel::<PipeWireThreadCommand>(100);
        // Frame channel: increased from 64 to 256 to handle burst traffic
        // At 60 FPS capture / 30 FPS target = 2:1 ratio needs buffer
        let (frame_tx, frame_rx) = std_mpsc::sync_channel::<VideoFrame>(256);
        // State event channel for health monitoring (bounded to prevent unbounded growth)
        let (state_event_tx, state_event_rx) = std_mpsc::sync_channel::<StreamStateEvent>(256);
        let (shutdown_tx, shutdown_rx) = std_mpsc::sync_channel::<()>(1);

        // Spawn dedicated PipeWire thread
        let thread_handle = thread::Builder::new()
            .name("pipewire-main".to_string())
            .spawn(move || {
                run_pipewire_main_loop(fd, command_rx, frame_tx, state_event_tx, shutdown_rx);
            })
            .map_err(|e| {
                PipeWireError::InitializationFailed(format!("Thread spawn failed: {e}"))
            })?;

        info!("PipeWire thread started successfully");

        Ok(Self {
            thread_handle: Some(thread_handle),
            command_tx,
            frame_rx,
            state_event_rx,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    /// Create a direct-channel frame source (no PipeWire thread).
    ///
    /// Used when the capture backend provides frames through a direct channel
    /// instead of PipeWire (e.g., portal-generic with in-process screencopy).
    /// The frame receiver is adapted to produce `VideoFrame` objects.
    pub fn new_direct(
        raw_rx: std_mpsc::Receiver<crate::desktop::pipewire::frame::RawFrameData>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        use std::sync::Arc;
        use std::time::SystemTime;

        let (frame_tx, frame_rx) = std_mpsc::sync_channel::<VideoFrame>(256);
        let (state_event_tx, state_event_rx) = std_mpsc::sync_channel::<StreamStateEvent>(256);
        let (command_tx, command_rx) = std_mpsc::sync_channel::<PipeWireThreadCommand>(1);
        let (shutdown_tx, shutdown_rx) = std_mpsc::sync_channel(1);

        // Send initial Streaming state event
        let _ = state_event_tx.try_send(StreamStateEvent {
            stream_id: 0,
            state: PwStreamState::Streaming,
        });

        // Spawn converter thread that reads RawFrameData → VideoFrame
        let thread_handle = thread::Builder::new()
            .name("direct-frame-adapter".to_string())
            .spawn(move || {
                let mut frame_count: u64 = 0;
                info!("Direct frame adapter thread started");
                loop {
                    if shutdown_rx.try_recv().is_ok()
                        || matches!(command_rx.try_recv(), Ok(PipeWireThreadCommand::Shutdown))
                    {
                        break;
                    }

                    let raw = match raw_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(raw) => raw,
                        Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                    };

                    frame_count += 1;
                    let frame = VideoFrame {
                        frame_id: frame_count,
                        pts: frame_count * 33_333_333, // ~30fps
                        dts: frame_count * 33_333_333,
                        duration: 33_333_333,
                        width: raw.width.unwrap_or(width),
                        height: raw.height.unwrap_or(height),
                        stride: raw.stride.unwrap_or(width * 4),
                        format: raw.format.unwrap_or(PixelFormat::BGRx),
                        monitor_index: 0,
                        data: Arc::new(raw.data),
                        capture_time: SystemTime::now(),
                        damage_regions: vec![],
                        flags: crate::desktop::pipewire::frame::FrameFlags::new(),
                    };
                    if frame_tx.try_send(frame).is_err() {
                        // Channel full, drop frame
                    }
                }
                info!(
                    "Direct frame adapter thread exited after {} frames",
                    frame_count
                );
            })
            .map_err(|error| {
                PipeWireError::InitializationFailed(format!(
                    "failed to spawn direct frame adapter: {error}"
                ))
            })?;

        Ok(Self {
            thread_handle: Some(thread_handle),
            command_tx,
            frame_rx,
            state_event_rx,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    /// Send a command to the PipeWire thread
    ///
    /// # Arguments
    ///
    /// * `command` - Command to execute
    ///
    /// # Errors
    ///
    /// Returns error if command cannot be sent (thread died)
    pub fn send_command(&self, command: PipeWireThreadCommand) -> Result<()> {
        self.command_tx.send(command).map_err(|_| {
            PipeWireError::ThreadCommunicationFailed("Command send failed".to_string())
        })
    }

    /// Try to receive a frame (non-blocking)
    ///
    /// # Returns
    ///
    /// Some(VideoFrame) if a frame is available, None otherwise
    pub fn try_recv_frame(&self) -> Option<VideoFrame> {
        self.frame_rx.try_recv().ok()
    }

    /// Try to receive a stream state event (non-blocking)
    ///
    /// Returns the next state change event if one is available.
    pub fn try_recv_state_event(&self) -> Option<StreamStateEvent> {
        self.state_event_rx.try_recv().ok()
    }

    /// Drain all pending stream state events
    ///
    /// Returns all queued state change events, useful for batch processing
    /// in a frame loop. Events are ordered chronologically.
    pub fn drain_state_events(&self) -> Vec<StreamStateEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.state_event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Receive a frame (blocking with timeout)
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum time to wait for a frame
    ///
    /// # Returns
    ///
    /// Some(VideoFrame) if received within timeout, None otherwise
    pub fn recv_frame_timeout(&self, timeout: Duration) -> Option<VideoFrame> {
        self.frame_rx.recv_timeout(timeout).ok()
    }

    /// Shutdown the PipeWire thread gracefully
    pub fn shutdown(&mut self) -> Result<()> {
        if self.thread_handle.is_none() {
            return Ok(());
        }

        info!("Shutting down PipeWire thread");

        // Send shutdown command
        if let Err(e) = self.send_command(PipeWireThreadCommand::Shutdown) {
            warn!("Failed to send shutdown command: {}", e);
        }

        // Signal shutdown via dedicated channel
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        // Wait for thread to finish (with timeout)
        if let Some(handle) = self.thread_handle.take()
            && handle.join().is_err()
        {
            error!("PipeWire thread panicked during shutdown");
            return Err(PipeWireError::ThreadPanic("Thread panicked".to_string()));
        }

        info!("PipeWire thread shut down successfully");
        Ok(())
    }
}

impl Drop for PipeWireThreadManager {
    fn drop(&mut self) {
        debug!("Dropping PipeWireThreadManager");
        let _ = self.shutdown();
    }
}

/// Main loop function that runs on the dedicated PipeWire thread
///
/// This function owns all PipeWire types (MainLoop, Context, Core, Streams)
/// and processes commands from the async runtime.
fn run_pipewire_main_loop(
    fd: RawFd,
    command_rx: std_mpsc::Receiver<PipeWireThreadCommand>,
    frame_tx: std_mpsc::SyncSender<VideoFrame>,
    state_event_tx: std_mpsc::SyncSender<StreamStateEvent>,
    shutdown_rx: std_mpsc::Receiver<()>,
) {
    info!("PipeWire main loop thread started");

    // Initialize PipeWire library
    pipewire::init();

    // Create main loop
    let main_loop = match MainLoopBox::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            error!("Failed to create MainLoop: {}", e);
            return;
        }
    };

    // Create context (0.9 API: takes &Loop reference + optional properties)
    let context = match ContextBox::new(main_loop.loop_(), None) {
        Ok(ctx) => ctx,
        Err(e) => {
            error!("Failed to create Context: {}", e);
            return;
        }
    };

    // Connect core using portal FD
    info!("Connecting PipeWire Core to Portal FD {}", fd);
    // SAFETY: The FD is provided by the capture session setup path.
    // We take exclusive ownership - the FD is not used anywhere else.
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let core = match context.connect_fd(owned_fd, None) {
        Ok(c) => {
            info!("Core.connect_fd() succeeded");
            c
        }
        Err(e) => {
            error!("Failed to connect Core with FD {}: {}", fd, e);
            return;
        }
    };

    info!("PipeWire Core connected successfully to Portal FD {}", fd);
    info!("This is a PRIVATE PipeWire connection - node IDs only valid on this FD");

    // Stream storage (all streams live on this thread)
    let mut streams: HashMap<u32, ManagedStream> = HashMap::new();

    // Main event loop
    let mut loop_iterations = 0u64;
    'main: loop {
        loop_iterations += 1;

        // Log periodic heartbeat
        if loop_iterations.is_multiple_of(1000) {
            info!(
                "PipeWire main loop heartbeat: {} iterations, {} streams active",
                loop_iterations,
                streams.len()
            );
        }

        // Process all pending commands
        while let Ok(command) = command_rx.try_recv() {
            match command {
                PipeWireThreadCommand::CreateStream {
                    stream_id,
                    node_id,
                    config,
                    response_tx,
                } => {
                    info!(
                        " CreateStream command received: stream_id={}, node_id={}",
                        stream_id, node_id
                    );
                    info!(
                        "  Config: {}x{} @ {}fps, dmabuf={}, buffers={}",
                        config.width,
                        config.height,
                        config.framerate,
                        config.use_dmabuf,
                        config.buffer_count
                    );

                    let result = create_stream_on_thread(
                        stream_id,
                        node_id,
                        &core,
                        config,
                        frame_tx.clone(),
                        state_event_tx.clone(),
                    );

                    match result {
                        Ok(managed_stream) => {
                            info!("Storing stream {} in active streams map", stream_id);
                            streams.insert(stream_id, managed_stream);
                            let _ = response_tx.send(Ok(()));
                            info!(
                                " Stream {} fully created - now in streams map (total: {} streams)",
                                stream_id,
                                streams.len()
                            );
                        }
                        Err(e) => {
                            error!("Failed to create stream {}: {}", stream_id, e);
                            let _ = response_tx.send(Err(e));
                        }
                    }
                }

                PipeWireThreadCommand::DestroyStream {
                    stream_id,
                    response_tx,
                } => {
                    debug!("Destroying stream {}", stream_id);

                    if let Some(managed_stream) = streams.remove(&stream_id) {
                        drop(managed_stream);
                        let _ = response_tx.send(Ok(()));
                        info!("Stream {} destroyed", stream_id);
                    } else {
                        let _ = response_tx.send(Err(PipeWireError::StreamNotFound(stream_id)));
                    }
                }

                PipeWireThreadCommand::GetStreamState {
                    stream_id,
                    response_tx,
                } => {
                    // StreamState doesn't implement Clone, so we match and reconstruct
                    let state = streams.get(&stream_id).map(|s| match &s.state {
                        StreamState::Error(msg) => StreamState::Error(msg.clone()),
                        StreamState::Unconnected => StreamState::Unconnected,
                        StreamState::Connecting => StreamState::Connecting,
                        StreamState::Paused => StreamState::Paused,
                        StreamState::Streaming => StreamState::Streaming,
                    });
                    let _ = response_tx.send(state);
                }

                PipeWireThreadCommand::Shutdown => {
                    info!("Shutdown command received");
                    break 'main;
                }
            }
        }

        // Check for shutdown signal
        if shutdown_rx.try_recv().is_ok() {
            info!("Shutdown signal received");
            break 'main;
        }

        // Run one iteration of PipeWire main loop
        // Use non-blocking iterate (0ms timeout) to avoid frame timing jitter
        // Then sleep based on expected frame timing for efficiency
        let loop_ref = main_loop.loop_();
        let events_processed = loop_ref.iterate(Duration::from_millis(0));

        if loop_iterations.is_multiple_of(1000) {
            trace!(
                "loop.iterate() returned {} (events processed this iteration)",
                events_processed
            );
        }

        // Sleep briefly to avoid busy-looping while still maintaining low latency
        // At 60 FPS, frames arrive every ~16ms, so 5ms sleep is safe
        std::thread::sleep(Duration::from_millis(5));
    }

    // Cleanup
    info!("Cleaning up PipeWire resources");
    streams.clear();
    drop(core);
    drop(context);
    drop(main_loop);

    // SAFETY: pipewire::deinit() must be called once per pipewire::init().
    // All PipeWire resources (streams, core, context, main_loop) have been dropped.
    // This thread called init() and no other code uses this PipeWire instance.
    unsafe {
        pipewire::deinit();
    }

    info!("PipeWire thread exited");
}

/// Memory-map a file descriptor to extract buffer data
///
/// Handles both DMA-BUF and MemFd buffers by mapping the FD into process memory.
///
/// # Arguments
///
/// * `fd` - File descriptor to map
/// * `size` - Size of data to read
/// * `offset` - Offset within the mapped region
///
/// # Returns
///
/// Vec<u8> containing the pixel data, or error if mmap fails
///
/// # Safety
///
/// This uses unsafe mmap operations but is safe because:
/// - We immediately copy data and unmap
/// - FD is owned by PipeWire buffer (valid during callback)
/// - No pointer aliasing (we copy, not reference)
fn mmap_fd_buffer(fd: std::os::fd::RawFd, size: usize, offset: usize) -> Result<Vec<u8>> {
    use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};
    use std::os::fd::BorrowedFd;

    // SAFETY: sysconf has no pointer arguments. A non-positive result is an error.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = usize::try_from(page_size).map_err(|_| {
        PipeWireError::FrameExtractionFailed("could not determine system page size".to_string())
    })?;
    let map_offset = (offset / page_size) * page_size;
    let data_offset_in_map = offset - map_offset;
    let map_size = size.checked_add(data_offset_in_map).ok_or_else(|| {
        PipeWireError::FrameExtractionFailed("buffer mapping size overflow".to_string())
    })?;

    info!(
        "mmap: fd={}, size={}, offset={}, page_size={}, map_offset={}, map_size={}",
        fd, size, offset, page_size, map_offset, map_size
    );

    // Memory map the file descriptor
    // SAFETY:
    // - FD is valid (owned by PipeWire buffer during callback)
    // - We immediately copy and unmap (no lifetime issues)
    // - BorrowedFd is only used during mmap call
    let addr = unsafe {
        let borrowed_fd = BorrowedFd::borrow_raw(fd);
        mmap(
            None,
            NonZeroUsize::new(map_size).ok_or_else(|| {
                PipeWireError::FrameExtractionFailed("Invalid map size".to_string())
            })?,
            ProtFlags::PROT_READ,
            MapFlags::MAP_SHARED,
            borrowed_fd,
            map_offset as i64,
        )
        .map_err(|e| PipeWireError::FrameExtractionFailed(format!("mmap failed: {e}")))?
    };

    // Copy data from mapped region
    // SAFETY: addr is valid NonNull from successful mmap above, and:
    // - data_offset_in_map + size <= map_size (calculated correctly above)
    // - Vec has sufficient capacity allocated
    // - copy_nonoverlapping is safe with non-overlapping src/dst
    // - set_len is safe because we just wrote exactly size bytes
    let result = unsafe {
        let src_ptr = (addr.as_ptr() as *const u8).add(data_offset_in_map);
        let mut vec = Vec::with_capacity(size);
        std::ptr::copy_nonoverlapping(src_ptr, vec.as_mut_ptr(), size);
        vec.set_len(size);
        vec
    };

    // Unmap immediately after copying (no dangling pointers)
    // SAFETY: addr and map_size are from the successful mmap above.
    // We've finished reading, so unmapping is safe.
    unsafe {
        munmap(addr, map_size)
            .map_err(|e| warn!("munmap warning: {}", e))
            .ok();
    }

    info!("mmap successful: extracted {} bytes", result.len());
    Ok(result)
}

/// Create a stream on the PipeWire thread
///
/// This function performs the complete stream creation, format negotiation,
/// and callback setup as specified in TASK-P1-04.
fn create_stream_on_thread(
    stream_id: u32,
    node_id: u32,
    core: &pipewire::core::Core,
    config: StreamConfig,
    frame_tx: std_mpsc::SyncSender<VideoFrame>,
    state_event_tx: std_mpsc::SyncSender<StreamStateEvent>,
) -> Result<ManagedStream> {
    let stream_name = format!("wrdp-pw-{stream_id}");
    let node_target = node_id.to_string();

    // Build stream properties per spec
    info!("Building stream properties for stream {}", stream_id);
    let mut props = PropertiesBox::new();
    props.insert("media.type", "Video");
    props.insert("media.category", "Capture");
    props.insert("media.role", "Screen");
    props.insert("media.name", stream_name.as_str());
    props.insert("node.target", node_target.as_str());
    props.insert("stream.capture-sink", "true");

    info!("Stream properties:");
    info!(" media.type = Video");
    info!(" media.category = Capture");
    info!(" media.role = Screen");
    info!(" media.name = {}", stream_name);
    info!(" node.target = {} (Portal provided node ID)", node_target);
    info!(" stream.capture-sink = true");

    // Create the stream
    info!("Calling StreamBox::new() with properties");
    // SAFETY: We use 'static lifetime because we manually enforce that the core
    // outlives all streams (streams.clear() before drop(core) in the main loop).
    let stream: StreamBox<'static> = unsafe {
        let stream_box = StreamBox::new(core, &stream_name, props).map_err(|e| {
            PipeWireError::StreamCreationFailed(format!("StreamBox::new failed: {e}"))
        })?;
        // Transmute lifetime from '_ (tied to core borrow) to 'static.
        // SAFETY: Drop ordering is manually enforced in run_pipewire_main_loop.
        std::mem::transmute::<StreamBox<'_>, StreamBox<'static>>(stream_box)
    };

    info!("Stream::new() succeeded - stream object created");

    // Set up stream event listeners.
    let frame_tx_for_process = frame_tx.clone();
    let stream_id_for_callbacks = stream_id;

    info!(
        " Registering stream {} callbacks (state_changed, param_changed, process)",
        stream_id
    );

    // Shared negotiated resolution — updated by param_changed, read by process
    let negotiated_width = StdArc::new(AtomicU32::new(config.width));
    let negotiated_height = StdArc::new(AtomicU32::new(config.height));
    let neg_w_for_param = StdArc::clone(&negotiated_width);
    let neg_h_for_param = StdArc::clone(&negotiated_height);
    let neg_w_for_process = StdArc::clone(&negotiated_width);
    let neg_h_for_process = StdArc::clone(&negotiated_height);

    let state_tx_for_callback = state_event_tx;

    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(move |_stream, _user_data, old_state, new_state| {
            info!(
                "Stream {} state changed: {:?} -> {:?}",
                stream_id_for_callbacks, old_state, new_state
            );

            match new_state {
                StreamState::Error(ref err_msg) => {
                    error!("Stream {} entered error state: {}", stream_id_for_callbacks, err_msg);
                }
                StreamState::Streaming => {
                    info!("Stream {} is now streaming", stream_id_for_callbacks);
                }
                StreamState::Paused => {
                    debug!("Stream {} paused", stream_id_for_callbacks);
                }
                _ => {}
            }

            // Emit state event for health monitoring
            // StreamState doesn't implement Clone, so reconstruct PwStreamState manually
            let pw_state = match new_state {
                StreamState::Unconnected => PwStreamState::Unconnected,
                StreamState::Connecting => PwStreamState::Connecting,
                StreamState::Paused => PwStreamState::Paused,
                StreamState::Streaming => PwStreamState::Streaming,
                StreamState::Error(msg) => PwStreamState::Error(msg.clone()),
            };
            let event = StreamStateEvent {
                stream_id: stream_id_for_callbacks,
                state: pw_state,
            };
            // Non-blocking: drop event if channel full rather than stalling PipeWire
            let _ = state_tx_for_callback.try_send(event);
        })
        .param_changed(move |_stream, _user_data, param_id, param| {
            let Some(param) = param else { return; };
            if param_id != ParamType::Format.as_raw() { return; }

            // Validate media type before parsing video specifics
            match format_utils::parse_format(param) {
                Ok((media_type, media_subtype)) => {
                    info!(
                        "Stream {} format negotiated: type={:?} subtype={:?}",
                        stream_id_for_callbacks, media_type, media_subtype
                    );
                }
                Err(e) => {
                    warn!("Stream {} param_changed: failed to parse media type: {e}", stream_id_for_callbacks);
                    return;
                }
            }

            // Parse the actual negotiated video format from the Pod
            let mut video_info = VideoInfoRaw::new();
            if let Err(e) = video_info.parse(param) {
                warn!("Stream {} param_changed: failed to parse VideoInfoRaw: {e}", stream_id_for_callbacks);
                return;
            }

            let size = video_info.size();
            let format = video_info.format();
            info!(
                "Stream {} negotiated: {}x{} {:?}",
                stream_id_for_callbacks, size.width, size.height, format
            );

            // Update shared atomics so the process callback validates against
            // the actual compositor resolution, not the requested resolution
            neg_w_for_param.store(size.width, Ordering::Release);
            neg_h_for_param.store(size.height, Ordering::Release);
        })
        .process(move |stream, _user_data| {
            // This callback is called when a new frame buffer is available
            trace!("process() callback fired for stream {}", stream_id_for_callbacks);

            // Capture stream timing before touching buffers (RT-safe)
            // SAFETY: stream pointer is valid within this callback; pw_stream_get_time_n is RT-safe
            let stream_time = unsafe { crate::desktop::pipewire::stream::get_stream_time(stream.as_raw_ptr()) };

            if let Some(mut buffer) = stream.dequeue_buffer() {
                // Debug: dump buffer data block details
                {
                    let datas = buffer.datas_mut();
                    let n = datas.len();
                    trace!(
                        "Got buffer from stream {}: {} data blocks",
                        stream_id_for_callbacks, n
                    );
                    for (i, d) in datas.iter_mut().enumerate() {
                        let has_data = d.data().is_some();
                        let data_len = d.data().map_or(0, |s| s.len());
                        trace!(
                            "  data[{}]: type={}, fd={}, has_data={}, data_len={}, chunk_size={}",
                            i,
                            d.type_().as_raw(),
                            d.fd(),
                            has_data,
                            data_len,
                            d.chunk().size()
                        );
                    }
                }

                // Extract frame data from buffer
                if let Some(data) = buffer.datas_mut().first_mut() {
                    // Get buffer chunk info
                    let chunk = data.chunk();
                    let size = chunk.size() as usize;
                    let offset = chunk.offset() as usize;
                    let data_type = data.type_();

                    // Extract pixel data based on buffer type
                    let fd = data.fd();

                    trace!(
                        "Buffer: type={}, size={}, offset={}, fd={}",
                        data_type.as_raw(),
                        size,
                        offset,
                        fd
                    );

                    let pixel_data: Option<Vec<u8>> = match data_type {
                        // MemPtr: Direct memory access via data.data()
                        libspa::buffer::DataType::MemPtr => {
                            if let Some(mapped_data) = data.data() {
                                if offset + size <= mapped_data.len() {
                                    trace!("MemPtr buffer: copying {} bytes (offset={})", size, offset);
                                    Some(mapped_data[offset..offset + size].to_vec())
                                } else {
                                    warn!(
                                        "MemPtr buffer bounds invalid: offset={}, size={}, len={}",
                                        offset,
                                        size,
                                        mapped_data.len()
                                    );
                                    None
                                }
                            } else {
                                warn!("MemPtr buffer but data.data() returned None");
                                None
                            }
                        }

                        // MemFd: File descriptor with memory mapping
                        libspa::buffer::DataType::MemFd => {
                            if let Some(mapped_data) = data.data() {
                                if offset + size <= mapped_data.len() {
                                    trace!("MemFd buffer: copying {} bytes (offset={})", size, offset);
                                    Some(mapped_data[offset..offset + size].to_vec())
                                } else {
                                    warn!(
                                        "MemFd buffer bounds invalid: offset={}, size={}, len={}",
                                        offset,
                                        size,
                                        mapped_data.len()
                                    );
                                    None
                                }
                            } else if fd >= 0 {
                                // Fallback: manual mmap of MemFd
                                // Check for empty/skip frames (size=0 is normal PipeWire behavior)
                                if size == 0 {
                                    debug!("MemFd buffer: size=0 (empty/skip frame), ignoring");
                                    None
                                } else {
                                    trace!("MemFd buffer: using manual mmap (FD={})", fd);
                                    match mmap_fd_buffer(fd, size, offset) {
                                        Ok(data) => Some(data),
                                        Err(e) => {
                                            warn!("Failed to mmap MemFd buffer: {}", e);
                                            None
                                        }
                                    }
                                }
                            } else {
                                debug!("MemFd buffer but no valid FD (fd={})", fd);
                                None
                            }
                        }

                        // DMA-BUF: map the current chunk, copy it, and unmap it.
                        // Raw file descriptors may be reused by PipeWire with different
                        // offsets or allocation sizes, so mappings are not cached by FD.
                        libspa::buffer::DataType::DmaBuf => {
                            if fd < 0 || size == 0 {
                                None
                            } else {
                                match mmap_fd_buffer(fd, size, offset) {
                                    Ok(data) => Some(data),
                                    Err(error) => {
                                        warn!("Failed to map DMA-BUF FD={fd}: {error}");
                                        None
                                    }
                                }
                            }
                        }

                        // Unknown/Invalid type — portal source streams with
                        // ALLOC_BUFFERS may not set the buffer type field.
                        // Try data.data() as a fallback since the pixels may
                        // still be mapped and valid.
                        _ => {
                            if let Some(mapped_data) = data.data() {
                                if offset + size <= mapped_data.len() {
                                    trace!(
                                        "Buffer type unknown (raw={}), but mapped data available: {} bytes",
                                        data_type.as_raw(),
                                        size
                                    );
                                    Some(mapped_data[offset..offset + size].to_vec())
                                } else {
                                    warn!(
                                        "Buffer type unknown (raw={}), mapped data bounds invalid: offset={}, size={}, len={}",
                                        data_type.as_raw(),
                                        offset,
                                        size,
                                        mapped_data.len()
                                    );
                                    None
                                }
                            } else {
                                warn!(
                                    "Unknown buffer type: {} (raw={}), no mapped data",
                                    if data_type == libspa::buffer::DataType::Invalid {
                                        "Invalid"
                                    } else {
                                        "Unknown"
                                    },
                                    data_type.as_raw()
                                );
                                None
                            }
                        }
                    };

                    if let Some(pixel_data) = pixel_data {
                        // === BUFFER VALIDATION ===
                        // PipeWire sometimes provides zero-size or undersized buffers.
                        // These MUST be rejected early to prevent visual corruption.
                        // See: wrd-server-specs/docs/QUALITY-ISSUE-ANALYSIS-2025-12-27.md

                        let bytes_per_pixel = 4; // BGRA/BGRx = 4 bytes
                        // Use the actual negotiated resolution from param_changed,
                        // not the requested config — compositor controls output size
                        let neg_w = neg_w_for_process.load(Ordering::Acquire);
                        let neg_h = neg_h_for_process.load(Ordering::Acquire);
                        let min_expected_size = (neg_w * neg_h * bytes_per_pixel) as usize;

                        if pixel_data.is_empty() {
                            // Empty buffers are normal - GNOME portal sends them as "no change" signals
                            debug!("Skipping empty buffer (size=0) - compositor indicates no change");
                            return;
                        }

                        if pixel_data.len() < min_expected_size {
                            warn!(
                                "Rejecting undersized buffer: {} bytes < {} expected for {}×{}",
                                pixel_data.len(),
                                min_expected_size,
                                neg_w,
                                neg_h
                            );
                            return;
                        }

                        // Calculate proper stride with alignment
                        // Proper stride = width * bytes_per_pixel, aligned to 16 bytes
                        let calculated_stride = (neg_w * bytes_per_pixel).div_ceil(16) * 16;

                        // Verify our calculated stride matches buffer
                        let expected_size = calculated_stride * neg_h;
                        let actual_stride = if expected_size as usize == size {
                            calculated_stride
                        } else {
                            // Buffer size doesn't match our calculation - compute actual stride
                            // This handles cases where compositor uses different alignment
                            (size / neg_h as usize) as u32
                        };

                        // Reject frames with zero stride (indicates corrupt buffer metadata)
                        if actual_stride == 0 {
                            warn!("Rejecting buffer with zero stride - corrupt metadata");
                            return;
                        }

                        // Log stride calculation details for first few frames
                        static LOGGED_FRAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                        let frame_count = LOGGED_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                        if frame_count < 5 {
                            trace!("Buffer analysis frame {}:", frame_count);
                            trace!(
                                "  Size: {} bytes, Width: {}, Height: {} (negotiated)",
                                size, neg_w, neg_h
                            );
                            trace!("  Calculated stride: {} bytes/row (16-byte aligned)", calculated_stride);
                            trace!(" Actual stride: {} bytes/row", actual_stride);
                            trace!(" Expected buffer size: {} bytes", expected_size);
                            trace!(" Buffer type: {} (1=MemPtr, 2=MemFd, 3=DmaBuf)", data_type.as_raw());
                            trace!(
                                "  Pixel format: {:?}",
                                config.preferred_format.unwrap_or(PixelFormat::BGRx)
                            );

                            // Log stream timing from pw_stream_get_time_n
                            if let Some(ref t) = stream_time {
                                trace!(
                                    "  PW timing: ticks={}, delay={}ns, queued={}/{} buffers, pressure={:.0}%",
                                    t.ticks,
                                    t.delay_nsec(),
                                    t.queued_buffers,
                                    t.queued_buffers + t.avail_buffers,
                                    t.buffer_pressure() * 100.0
                                );
                            }
                        }

                        if actual_stride != calculated_stride {
                            warn!("Stride mismatch detected:");
                            warn!(" Calculated: {} bytes/row", calculated_stride);
                            warn!(" Actual: {} bytes/row (from buffer size)", actual_stride);
                            warn!(" This may cause horizontal line artifacts!");
                        }

                        // Create VideoFrame from extracted pixel data
                        let pts = stream_time.as_ref().map_or(0, |t| t.now_nsec as u64);
                        let frame = VideoFrame {
                            frame_id: stream_id_for_callbacks as u64,
                            pts,
                            dts: 0,
                            duration: 16_666_667, // ~60fps default
                            width: neg_w,
                            height: neg_h,
                            stride: actual_stride,
                            format: config.preferred_format.unwrap_or(PixelFormat::BGRx),
                            monitor_index: 0,
                            data: StdArc::new(pixel_data),
                            capture_time: SystemTime::now(),
                            damage_regions: Vec::new(),
                            flags: FrameFlags::new(),
                        };

                        // Send frame to async runtime
                        if let Err(e) = frame_tx_for_process.try_send(frame) {
                            warn!("Failed to send frame: {} (channel full, backpressure)", e);
                        } else {
                            debug!("Frame sent to async runtime");
                        }
                    } else {
                        debug!("Could not extract pixel data from buffer");
                    }
                } else {
                    warn!("No data in buffer for stream {}", stream_id_for_callbacks);
                }
            } else {
                debug!(
                    "No buffer available (dequeue returned None) for stream {}",
                    stream_id_for_callbacks
                );
            }
        })
        .register()
        .map_err(|e| PipeWireError::StreamCreationFailed(format!("Listener registration failed: {e}")))?;

    info!("Stream {} callbacks registered successfully", stream_id);

    // Connect stream to node with format parameters
    let param_bytes = build_stream_parameters(&config)?;

    // Convert bytes to Pod reference (Pod is a borrowed type referencing the bytes)
    let pod = Pod::from_bytes(&param_bytes).ok_or_else(|| {
        PipeWireError::FormatNegotiationFailed("Failed to parse format parameters".to_string())
    })?;

    info!(
        " Stream {} connecting with format parameters ({} bytes)",
        stream_id,
        param_bytes.len()
    );

    let mut params = [pod];

    info!(
        stream_id,
        node_id, "Connecting stream with flags: AUTOCONNECT | MAP_BUFFERS | DRIVER | RT_PROCESS"
    );

    // DRIVER flag makes this stream drive the graph clock, ensuring frames
    // are delivered at the negotiated framerate even on a static desktop.
    // Without DRIVER, ScreenCast portal streams are damage-driven: no screen
    // change = no frame, causing stalls in the RDP frame delivery pipeline.
    stream
        .connect(
            Direction::Input,
            None, // PW_ID_ANY - let PipeWire use node.target property
            StreamFlags::AUTOCONNECT
                | StreamFlags::MAP_BUFFERS
                | StreamFlags::DRIVER
                | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| PipeWireError::ConnectionFailed(format!("Stream connect failed: {e}")))?;

    info!(
        " Stream {} .connect() succeeded - connected to node {}",
        stream_id, node_id
    );

    // NOTE: PipeWire tutorial does NOT call set_active() for portal streams
    // AUTOCONNECT flag should handle activation automatically
    // Calling set_active(true) here might interfere with auto-connection
    info!("⏳ NOT calling set_active() - AUTOCONNECT flag should activate stream automatically");
    info!("Waiting for PipeWire to transition stream to Streaming state via main loop events");
    info!(
        " If you don't see 'Stream {} is now streaming' within 2 seconds, AUTOCONNECT failed",
        stream_id
    );

    Ok(ManagedStream {
        id: stream_id,
        stream,
        _listener,
        config,
        state: StreamState::Connecting, // Initial state
        frame_count: 0,
        frame_tx,
    })
}

/// Build stream parameters for format negotiation
///
/// Constructs SPA Pod parameters for video format, size, and framerate negotiation.
/// Returns raw bytes that can be converted to a Pod reference at the call site.
///
/// # Format Negotiation Strategy
///
/// We accept whatever buffer type PipeWire provides since we now support:
/// - MemPtr (type 1): Direct memory access via data.data()
/// - MemFd (type 2): Memory-mapped FD via mmap()
/// - DmaBuf (type 3): GPU buffer via mmap() with FD
///
/// We provide explicit format parameters so PipeWire can complete negotiation.
/// This enables hardware acceleration when available (DMA-BUF) while maintaining compatibility.
fn build_stream_parameters(config: &StreamConfig) -> Result<Vec<u8>> {
    use pipewire::spa;
    use pipewire::spa::pod::Value;
    use pipewire::spa::pod::serialize::PodSerializer;
    use std::io::Cursor;

    info!(
        "Building format parameters: {}x{} @ {}fps",
        config.width, config.height, config.framerate
    );

    // Build a video format object using pipewire-rs macros
    // This specifies our preferred formats, size range, and framerate range
    let format_obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        // Media type: Video
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        // Media subtype: Raw (uncompressed)
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        // Video formats we accept (in order of preference)
        // BGRx/BGRA are preferred as they're common on Linux desktops
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx, // Default/preferred
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::RGBA
        ),
        // Video size range (min to max, with our preferred size as default)
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: config.width,
                height: config.height
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        // Framerate range (0/1 to 1000/1, with our target as default)
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction {
                num: config.framerate,
                denom: 1
            },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    // Serialize the object to bytes
    let serialized = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(format_obj))
        .map_err(|e| {
            warn!("Failed to serialize format parameters: {:?}", e);
            PipeWireError::FormatNegotiationFailed(format!("Format serialization failed: {e:?}"))
        })?;

    let bytes = serialized.0.into_inner();

    info!(
        "Format parameters built successfully ({} bytes)",
        bytes.len()
    );
    debug!(
        "  Preferred format: BGRx, size: {}x{}, fps: {}",
        config.width, config.height, config.framerate
    );

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_thread_manager_creation() {
        // Cannot test without valid FD from portal
        // Full tests require integration testing with actual portal
    }

    #[test]
    fn mmap_buffer_uses_offset_relative_to_aligned_mapping() {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;

        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&vec![0x11; 4093]).unwrap();
        file.write_all(b"payload").unwrap();
        file.flush().unwrap();

        let data = mmap_fd_buffer(file.as_raw_fd(), 7, 4093).unwrap();
        assert_eq!(data, b"payload");
    }

    #[test]
    fn mmap_buffer_rejects_size_overflow() {
        let error = mmap_fd_buffer(0, usize::MAX, 1).unwrap_err();
        assert!(error.to_string().contains("overflow"));
    }

    #[test]
    fn direct_manager_shutdown_does_not_wait_for_frame_producer() {
        let (raw_tx, raw_rx) = std_mpsc::channel();
        let mut manager = PipeWireThreadManager::new_direct(raw_rx, 640, 480).unwrap();

        // Keep the producer alive long enough that the old blocking recv/join
        // implementation would visibly delay shutdown, but always release it
        // so a regression cannot hang the test process indefinitely.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(raw_tx);
        });

        let started = std::time::Instant::now();
        manager.shutdown().unwrap();
        assert!(started.elapsed() < Duration::from_millis(300));
    }
}
