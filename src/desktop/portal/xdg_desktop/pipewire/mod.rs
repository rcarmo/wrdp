//! PipeWire thread manager for portal capture streams.
//!
//! Provides [`PipeWireManager`], which owns the dedicated PipeWire loop thread
//! and bridges async runtime calls into that thread via command dispatch.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────┐     pipewire::channel::Sender   ┌──────────────────────┐
//! │  Tokio async world      │ ──────────────────────────────> │  PipeWire OS thread  │
//! │  (D-Bus handlers,       │                                 │  - MainLoop          │
//! │   capture backends)     │ <────────────────────────────── │  - Context           │
//! │                         │     oneshot reply channels       │  - Core              │
//! └─────────────────────────┘                                 │  - Streams HashMap   │
//!                                                             └──────────────────────┘
//! ```

pub mod stream;

use std::{
    collections::HashMap,
    os::unix::io::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

pub use stream::StreamConfig;
use tokio::sync::oneshot;

use crate::desktop::portal::xdg_desktop::error::PortalError;

/// Commands sent from async world to the PipeWire thread.
#[non_exhaustive]
pub enum PipeWireCommand {
    /// Create a new video source stream.
    CreateStream {
        /// Configuration for the stream.
        config: StreamConfig,
        /// Reply channel for the resulting PipeWire node ID.
        reply: oneshot::Sender<Result<u32, PortalError>>,
    },
    /// Destroy a stream by its node ID.
    DestroyStream {
        /// PipeWire node ID of the stream to destroy.
        node_id: u32,
        /// Reply channel.
        reply: oneshot::Sender<Result<(), PortalError>>,
    },
    /// Queue a frame buffer into a stream.
    QueueBuffer {
        /// PipeWire node ID of the target stream.
        node_id: u32,
        /// Raw pixel data.
        data: Vec<u8>,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Row stride in bytes.
        stride: u32,
        /// Pixel format (SPA format enum value).
        format: u32,
    },
    /// Get a restricted PipeWire connection fd for a client.
    OpenRemote {
        /// Reply channel returning an OwnedFd.
        reply: oneshot::Sender<Result<OwnedFd, PortalError>>,
    },
    /// Shut down the PipeWire thread.
    Shutdown,
}

// Manual Debug impl since oneshot::Sender doesn't implement Debug
impl std::fmt::Debug for PipeWireCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateStream { config, .. } => f
                .debug_struct("CreateStream")
                .field("config", config)
                .finish(),
            Self::DestroyStream { node_id, .. } => f
                .debug_struct("DestroyStream")
                .field("node_id", node_id)
                .finish(),
            Self::QueueBuffer {
                node_id,
                width,
                height,
                stride,
                format,
                ..
            } => f
                .debug_struct("QueueBuffer")
                .field("node_id", node_id)
                .field("width", width)
                .field("height", height)
                .field("stride", stride)
                .field("format", format)
                .finish(),
            Self::OpenRemote { .. } => f.debug_struct("OpenRemote").finish(),
            Self::Shutdown => write!(f, "Shutdown"),
        }
    }
}

/// Manages the PipeWire thread and provides an async command interface.
///
/// The manager spawns a dedicated OS thread running the PipeWire main loop.
/// All PipeWire operations are dispatched as commands to that thread via
/// `pipewire::channel`, which integrates directly with the PipeWire loop's
/// event system for zero-latency wakeups.
pub struct PipeWireManager {
    /// Command sender to the PipeWire thread.
    command_tx: mpsc::Sender<PipeWireCommand>,
    /// Whether the manager is running.
    running: Arc<AtomicBool>,
    /// Thread join handle.
    thread_handle: Option<thread::JoinHandle<()>>,
}

impl PipeWireManager {
    /// Create an inert manager for direct-frame mode.
    ///
    /// No PipeWire thread is started; `queue_buffer()` traffic is intentionally
    /// ignored because direct-frame paths bypass this manager entirely.
    pub fn disabled() -> Self {
        let (command_tx, _command_rx) = mpsc::channel::<PipeWireCommand>();
        Self {
            command_tx,
            running: Arc::new(AtomicBool::new(false)),
            thread_handle: None,
        }
    }

    /// Start the PipeWire manager on a dedicated thread.
    ///
    /// Initializes PipeWire, creates the main loop, context, and core,
    /// then enters the event loop. Commands are processed via a
    /// `pipewire::channel` receiver attached to the main loop.
    pub fn start() -> Result<Self, PortalError> {
        let (command_tx, command_rx) = mpsc::channel::<PipeWireCommand>();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let thread_handle = thread::Builder::new()
            .name("pipewire-main".to_string())
            .spawn(move || {
                if let Err(e) = Self::run_thread(&command_rx, &running_clone) {
                    tracing::error!("PipeWire thread exited with error: {}", e);
                }
            })
            .map_err(|e| PortalError::PipeWire(format!("Failed to spawn PipeWire thread: {e}")))?;

        tracing::info!("PipeWire manager started");

        Ok(Self {
            command_tx,
            running,
            thread_handle: Some(thread_handle),
        })
    }

    /// Run the PipeWire event loop on the dedicated thread.
    ///
    /// Uses a manual iterate loop with `std::sync::mpsc` for commands.
    /// The API surface in use relies on lifetime-bound loop/context types,
    /// so commands are polled each iteration instead of moved into callbacks.
    #[expect(
        clippy::too_many_lines,
        reason = "PipeWire setup is inherently sequential"
    )]
    fn run_thread(
        command_rx: &mpsc::Receiver<PipeWireCommand>,
        running: &Arc<AtomicBool>,
    ) -> Result<(), PortalError> {
        let mainloop = pipewire::main_loop::MainLoopBox::new(None)
            .map_err(|e| PortalError::PipeWire(format!("Failed to create main loop: {e}")))?;

        let context = pipewire::context::ContextBox::new(mainloop.loop_(), None)
            .map_err(|e| PortalError::PipeWire(format!("Failed to create context: {e}")))?;

        let core = context.connect(None).map_err(|e| {
            PortalError::PipeWire(format!("Failed to connect to PipeWire daemon: {e}"))
        })?;

        tracing::info!("Connected to PipeWire daemon");

        let mut streams: HashMap<u32, stream::PipeWireVideoStream> = HashMap::new();

        // Manual event loop: process PipeWire events + poll commands
        while running.load(Ordering::Relaxed) {
            // Process all pending commands
            while let Ok(command) = command_rx.try_recv() {
                match command {
                    PipeWireCommand::CreateStream { config, reply } => {
                        let result = stream::PipeWireVideoStream::create(&core, &config);
                        match result {
                            Ok(mut pw_stream) => {
                                // node_id() returns SPA_ID_INVALID right after connect()
                                // because node assignment is async. Run main loop iterations
                                // to let PipeWire process the connection and assign a real ID.
                                let mut node_id = pw_stream.node_id();
                                if node_id == u32::MAX {
                                    for attempt in 0..50 {
                                        mainloop.loop_().iterate(Duration::from_millis(10));
                                        node_id = pw_stream.refresh_node_id();
                                        if node_id != u32::MAX {
                                            tracing::debug!(
                                                node_id,
                                                attempts = attempt + 1,
                                                "Node ID assigned after main loop iterations"
                                            );
                                            break;
                                        }
                                    }
                                    if node_id == u32::MAX {
                                        tracing::error!(
                                            "PipeWire stream node ID still invalid after 500ms"
                                        );
                                        let _ = reply.send(Err(PortalError::PipeWire(
                                            "Stream node ID not assigned (SPA_ID_INVALID)"
                                                .to_string(),
                                        )));
                                        continue;
                                    }
                                }
                                tracing::info!(
                                    node_id = node_id,
                                    width = config.width,
                                    height = config.height,
                                    "PipeWire stream created"
                                );
                                streams.insert(node_id, pw_stream);
                                let _ = reply.send(Ok(node_id));
                            }
                            Err(e) => {
                                tracing::error!("Failed to create PipeWire stream: {}", e);
                                let _ = reply.send(Err(e));
                            }
                        }
                    }

                    PipeWireCommand::DestroyStream { node_id, reply } => {
                        if let Some(pw_stream) = streams.remove(&node_id) {
                            pw_stream.disconnect();
                            tracing::info!(node_id = node_id, "PipeWire stream destroyed");
                            let _ = reply.send(Ok(()));
                        } else {
                            let _ = reply.send(Err(PortalError::PipeWire(format!(
                                "Stream not found: {node_id}"
                            ))));
                        }
                    }

                    PipeWireCommand::QueueBuffer {
                        node_id,
                        data,
                        width,
                        height,
                        stride,
                        format,
                    } => {
                        if let Some(pw_stream) = streams.get_mut(&node_id) {
                            if let Err(e) =
                                pw_stream.queue_frame(&data, width, height, stride, format)
                            {
                                tracing::warn!(
                                    node_id = node_id,
                                    error = %e,
                                    "Failed to queue frame"
                                );
                            }
                        } else {
                            tracing::trace!(node_id = node_id, "QueueBuffer for unknown stream");
                        }
                    }

                    PipeWireCommand::OpenRemote { reply } => {
                        match Self::create_remote_fd(&context) {
                            Ok(fd) => {
                                let _ = reply.send(Ok(fd));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }

                    PipeWireCommand::Shutdown => {
                        tracing::info!("PipeWire thread shutting down");
                        running.store(false, Ordering::Relaxed);
                    }
                }
            }

            // Run one PipeWire main loop iteration (process stream events)
            mainloop.loop_().iterate(Duration::from_millis(10));
        }

        // Clean up all streams before dropping core/context
        for (node_id, pw_stream) in streams.drain() {
            pw_stream.disconnect();
            tracing::debug!(node_id = node_id, "Cleaned up PipeWire stream on shutdown");
        }

        tracing::info!("PipeWire thread exited cleanly");
        Ok(())
    }

    /// Create a PipeWire remote connection fd for a client.
    ///
    /// This creates a new context connection that the portal client can use
    /// to access the portal's PipeWire stream nodes.
    fn create_remote_fd(
        context: &pipewire::context::ContextBox<'_>,
    ) -> Result<OwnedFd, PortalError> {
        // Create a socket pair — one end for the new PipeWire connection,
        // the other returned to the client.
        let (server_socket, client_socket) = {
            use std::os::unix::net::UnixStream;
            UnixStream::pair()
                .map_err(|e| PortalError::PipeWire(format!("Failed to create socket pair: {e}")))?
        };

        // Convert to OwnedFd via Into<OwnedFd>
        let server_fd: OwnedFd = server_socket.into();
        let client_fd: OwnedFd = client_socket.into();

        // Connect using the server end of the socket pair.
        // The client gets the other end to talk to PipeWire through.
        // We keep _core alive as long as we need the connection, but drop it after
        // the fd is connected — the client side keeps the connection alive.
        let _core = context
            .connect_fd(server_fd, None)
            .map_err(|e| PortalError::PipeWire(format!("Failed to connect fd to PipeWire: {e}")))?;

        Ok(client_fd)
    }

    /// Create a new video source stream.
    ///
    /// Returns the PipeWire node ID for the created stream.
    pub async fn create_stream(&self, config: StreamConfig) -> Result<u32, PortalError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(PipeWireCommand::CreateStream {
                config,
                reply: reply_tx,
            })
            .map_err(|_| PortalError::PipeWire("PipeWire thread not running".to_string()))?;

        reply_rx
            .await
            .map_err(|_| PortalError::PipeWire("PipeWire thread dropped reply".to_string()))?
    }

    /// Destroy a stream by node ID.
    pub async fn destroy_stream(&self, node_id: u32) -> Result<(), PortalError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(PipeWireCommand::DestroyStream {
                node_id,
                reply: reply_tx,
            })
            .map_err(|_| PortalError::PipeWire("PipeWire thread not running".to_string()))?;

        reply_rx
            .await
            .map_err(|_| PortalError::PipeWire("PipeWire thread dropped reply".to_string()))?
    }

    /// Queue a frame buffer into a stream (fire-and-forget).
    pub fn queue_buffer(
        &self,
        node_id: u32,
        data: Vec<u8>,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) {
        let _ = self.command_tx.send(PipeWireCommand::QueueBuffer {
            node_id,
            data,
            width,
            height,
            stride,
            format,
        });
    }

    /// Get a PipeWire remote connection fd for a client.
    pub async fn open_remote(&self) -> Result<OwnedFd, PortalError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(PipeWireCommand::OpenRemote { reply: reply_tx })
            .map_err(|_| PortalError::PipeWire("PipeWire thread not running".to_string()))?;

        reply_rx
            .await
            .map_err(|_| PortalError::PipeWire("PipeWire thread dropped reply".to_string()))?
    }

    /// Check if the PipeWire manager is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Shut down the PipeWire manager.
    pub fn shutdown(&self) {
        let _ = self.command_tx.send(PipeWireCommand::Shutdown);
    }
}

impl Drop for PipeWireManager {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipewire_command_debug() {
        let config = StreamConfig {
            source_id: 1,
            width: 1920,
            height: 1080,
            framerate: 30,
        };
        let (tx, _rx) = oneshot::channel();
        let cmd = PipeWireCommand::CreateStream { config, reply: tx };
        let debug = format!("{cmd:?}");
        assert!(debug.contains("CreateStream"));
        assert!(debug.contains("1920"));
    }

    #[test]
    fn test_stream_config() {
        let config = StreamConfig {
            source_id: 1,
            width: 2560,
            height: 1440,
            framerate: 60,
        };
        assert_eq!(config.source_id, 1);
        assert_eq!(config.width, 2560);
        assert_eq!(config.height, 1440);
        assert_eq!(config.framerate, 60);
    }

    #[test]
    fn test_command_shutdown_debug() {
        let cmd = PipeWireCommand::Shutdown;
        assert_eq!(format!("{cmd:?}"), "Shutdown");
    }
}
