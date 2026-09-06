//! Clipboard orchestrator.
//!
//! The orchestrator is the server-side clipboard state machine. It receives RDP
//! CLIPRDR events, provider selection events, and file-transfer responses, then
//! coordinates format conversion, loop prevention, delayed rendering, and RDP
//! server-event emission.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::rdp::channels::clipboard::core::{
    ClipboardFormat, FormatConverter, LoopDetectionConfig, TransferConfig, TransferEngine,
    sanitize::{
        parse_file_uris, sanitize_filename_for_linux, sanitize_text_for_linux,
        sanitize_text_for_windows,
    },
};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, trace, warn};

use crate::rdp::channels::clipboard::{
    error::{ClipboardError, Result},
    sync::SyncManager,
};

/// Shared clipboard provider reference (used by multiple handlers)
type SharedClipboardProvider =
    Arc<RwLock<Option<Arc<dyn crate::rdp::channels::clipboard::provider::ClipboardProvider>>>>;

/// Pending provider request queue (format_id, mime_type, timestamp)
type PendingPortalRequests =
    Arc<RwLock<std::collections::VecDeque<(u32, String, std::time::Instant)>>>;

/// Server event sender for RDP clipboard messages
type ServerEventSender = Arc<RwLock<Option<mpsc::UnboundedSender<ironrdp_server::ServerEvent>>>>;

/// Runtime configuration for the clipboard orchestrator
///
/// This is the internal implementation config, separate from the user-facing
/// `crate::config::types::ClipboardConfig` which defines what users can configure.
/// The server maps user settings to this runtime config at startup.
#[derive(Debug, Clone)]
pub struct ClipboardOrchestratorConfig {
    /// Maximum data size in bytes
    pub max_data_size: usize,

    /// Enable image format support
    pub enable_images: bool,

    /// Enable file transfer support
    pub enable_files: bool,

    /// Enable HTML format support
    pub enable_html: bool,

    /// Enable RTF format support
    pub enable_rtf: bool,

    /// Chunk size for transfers
    pub chunk_size: usize,

    /// Transfer timeout in milliseconds
    pub timeout_ms: u64,

    /// Loop detection window in milliseconds
    pub loop_detection_window_ms: u64,

    /// Minimum milliseconds between forwarded clipboard events (rate limiting)
    /// Prevents rapid-fire D-Bus signals from overwhelming Portal. Set to 0 to disable.
    pub rate_limit_ms: u64,
}

impl Default for ClipboardOrchestratorConfig {
    fn default() -> Self {
        Self {
            max_data_size: 16 * 1024 * 1024, // 16MB
            enable_images: true,
            enable_files: true,
            enable_html: true,
            enable_rtf: true,
            chunk_size: 64 * 1024, // 64KB chunks
            timeout_ms: 5000,
            loop_detection_window_ms: 500,
            rate_limit_ms: 200, // Max 5 events/second
        }
    }
}

/// Sentinel serial for eager-fetch requests (data-control upfront provision).
/// Real compositor serials are sequential small numbers, so u32::MAX won't collide.
const EAGER_FETCH_SERIAL: u32 = u32::MAX;

/// Response callback for sending data back to RDP
pub type RdpResponseCallback = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// Clipboard events from RDP or Portal
#[derive(Clone)]
pub enum ClipboardEvent {
    /// RDP clipboard channel is ready - should re-announce Linux clipboard
    RdpReady,

    /// RDP client announced available formats
    RdpFormatList(Vec<ClipboardFormat>),

    /// RDP client requests data in specific format (with callback to send response)
    RdpDataRequest(u32, Option<RdpResponseCallback>),

    /// RDP client provides requested data
    RdpDataResponse(Vec<u8>),

    /// RDP client returned error for data request (need to cancel Portal transfer)
    RdpDataError,

    /// RDP client requests file contents (Windows wants file from Linux)
    RdpFileContentsRequest {
        stream_id: u32,
        list_index: u32,
        position: u64,
        size: u32,
        is_size_request: bool,
    },

    /// RDP client provides file contents (Linux receives file from Windows)
    RdpFileContentsResponse {
        stream_id: u32,
        data: Vec<u8>,
        is_error: bool,
    },

    /// Local clipboard provider announced available MIME types.
    /// The bool marks authoritative provider-origin events (`true`) versus
    /// echo-like events (`false`) that may be filtered when RDP owns clipboard.
    PortalFormatsAvailable(Vec<String>, bool),

    /// Local clipboard provider requests data in specific MIME type
    PortalDataRequest(String),
}

impl std::fmt::Debug for ClipboardEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RdpReady => write!(f, "RdpReady"),
            Self::RdpFormatList(formats) => write!(f, "RdpFormatList({} formats)", formats.len()),
            Self::RdpDataRequest(id, _) => write!(f, "RdpDataRequest({id})"),
            Self::RdpDataResponse(data) => write!(f, "RdpDataResponse({} bytes)", data.len()),
            Self::RdpDataError => write!(f, "RdpDataError"),
            Self::RdpFileContentsRequest {
                stream_id,
                list_index,
                size,
                is_size_request,
                ..
            } => {
                write!(
                    f,
                    "RdpFileContentsRequest(stream={stream_id}, index={list_index}, size={size}, size_req={is_size_request})"
                )
            }
            Self::RdpFileContentsResponse {
                stream_id,
                data,
                is_error,
            } => {
                write!(
                    f,
                    "RdpFileContentsResponse(stream={}, {} bytes, error={})",
                    stream_id,
                    data.len(),
                    is_error
                )
            }
            Self::PortalFormatsAvailable(mimes, force) => {
                write!(f, "PortalFormatsAvailable({mimes:?}, force={force})")
            }
            Self::PortalDataRequest(mime) => write!(f, "PortalDataRequest({mime})"),
        }
    }
}

/// Clipboard orchestrator for bidirectional clipboard synchronization.
///
/// **Role:** primary runtime coordinator for clipboard channel state.
/// **Integrates:** IronRDP channel events, clipboard provider events, and format conversion.
///
/// # Architecture
///
/// Routes clipboard events between:
/// - RDP client (via `WrdpCliprdrFactory`)
/// - Local clipboard provider (via `ClipboardProvider` trait)
pub struct ClipboardOrchestrator {
    /// Configuration
    config: ClipboardOrchestratorConfig,

    /// Format converter
    converter: Arc<FormatConverter>,

    /// Transfer engine
    transfer_engine: Arc<TransferEngine>,

    /// Synchronization manager
    sync_manager: Arc<RwLock<SyncManager>>,

    /// Event sender
    event_tx: mpsc::Sender<ClipboardEvent>,

    /// Shutdown signal for the main event processor task.
    shutdown_tx: Option<mpsc::Sender<()>>,

    /// Shutdown broadcast (for all other async tasks)
    shutdown_broadcast: Arc<tokio::sync::broadcast::Sender<()>>,

    /// Task handles (for cleanup verification)
    task_handles: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,

    /// Pending Portal SelectionTransfer requests (FIFO queue)
    /// Each entry: (serial, mime_type, request_time)
    /// Used to correlate SelectionTransfer signals with RDP FormatDataResponse in order
    pending_portal_requests:
        Arc<RwLock<std::collections::VecDeque<(u32, String, std::time::Instant)>>>,

    /// Server event sender for sending clipboard requests to IronRDP
    /// Set by WrdpCliprdrFactory after ServerEvent sender is available
    server_event_sender: Arc<RwLock<Option<mpsc::UnboundedSender<ironrdp_server::ServerEvent>>>>,

    /// Clipboard provider (trait-abstracted backend).
    clipboard_provider:
        Arc<RwLock<Option<Arc<dyn crate::rdp::channels::clipboard::provider::ClipboardProvider>>>>,

    /// File transfer state (for handling file clipboard operations)
    file_transfer_state: Arc<RwLock<FileTransferState>>,

    /// Current RDP format list from Windows (for format ID lookup)
    /// Windows registered format IDs (like FileGroupDescriptorW) vary per session,
    /// so we store the actual list to look up the correct ID when requesting data.
    current_rdp_formats: Arc<RwLock<Vec<ClipboardFormat>>>,

    /// Formats we've advertised TO Windows (for Linux → Windows data requests)
    /// When Windows requests data by format ID, we look up the format name here.
    local_advertised_formats: Arc<RwLock<Vec<ClipboardFormat>>>,

    /// Health reporter for clipboard subsystem events
    health_reporter: Option<crate::rdp::session::supervision::SessionStatusReporter>,
}

/// State for managing file transfers between Windows and Linux
#[derive(Debug)]
struct FileTransferState {
    /// Incoming files (Windows → Linux) - stream_id → file state
    incoming_files: HashMap<u32, IncomingFile>,

    /// Outgoing files (Linux → Windows) - from current clipboard
    outgoing_files: Vec<OutgoingFile>,

    /// Pending file descriptors from Windows (FileGroupDescriptorW)
    /// These describe files Windows has available for transfer
    pending_descriptors: Vec<crate::rdp::channels::clipboard::core::FileDescriptor>,

    /// Directory for downloaded files
    download_dir: PathBuf,

    /// Portal serial for current incoming transfer (to deliver URIs when complete)
    portal_serial: Option<u32>,

    /// Next stream ID to use for FileContentsRequest (incremented per request)
    next_stream_id: u32,

    /// Completed files ready for delivery (final paths after rename from temp)
    completed_files: Vec<PathBuf>,
}

/// File being received from Windows
#[derive(Debug)]
struct IncomingFile {
    final_path: PathBuf,
    total_size: Option<u64>,
    awaiting_size: bool,
    received_size: u64,
    temp_path: PathBuf,
    file_handle: File,
    /// Index in the FileGroupDescriptorW list (needed for continuation requests)
    file_index: u32,
    /// Clipboard data lock ID (needed for continuation requests)
    clip_data_id: u32,
}

impl IncomingFile {
    fn append_data(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty()
            && self
                .total_size
                .is_some_and(|size| self.received_size < size)
        {
            return Err(ClipboardError::FileIoError(
                "clipboard file response made no progress".into(),
            ));
        }
        let next_size = self
            .received_size
            .checked_add(data.len() as u64)
            .ok_or_else(|| ClipboardError::FileIoError("clipboard size overflow".into()))?;
        if self.total_size.is_some_and(|size| next_size > size) {
            return Err(ClipboardError::FileIoError(
                "clipboard response exceeds advertised file size".into(),
            ));
        }
        self.file_handle
            .write_all(data)
            .map_err(|error| ClipboardError::FileIoError(format!("File write failed: {error}")))?;
        self.received_size = next_size;
        Ok(())
    }

    fn apply_size_response(&mut self, data: &[u8]) -> Result<u64> {
        let bytes: [u8; 8] = data.try_into().map_err(|_| {
            ClipboardError::FileIoError(format!(
                "invalid clipboard file size response: expected 8 bytes, got {}",
                data.len()
            ))
        })?;
        let size = u64::from_le_bytes(bytes);
        self.total_size = Some(size);
        self.awaiting_size = false;
        Ok(size)
    }
}

/// File being sent to Windows
#[derive(Debug)]
struct OutgoingFile {
    path: PathBuf,
    size: u64,
}

impl FileTransferState {
    fn new(download_dir: PathBuf) -> Self {
        Self {
            incoming_files: HashMap::new(),
            outgoing_files: Vec::new(),
            pending_descriptors: Vec::new(),
            download_dir,
            portal_serial: None,
            next_stream_id: 1,
            completed_files: Vec::new(),
        }
    }

    fn clear_incoming(&mut self) {
        for file in self.incoming_files.drain().map(|(_, file)| file) {
            if let Err(error) = std::fs::remove_file(&file.temp_path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                warn!("failed to remove clipboard temp file: {error}");
            }
        }
        self.portal_serial = None;
        self.completed_files.clear();
    }

    fn reserve_destination(&self, filename: &str) -> PathBuf {
        let candidate = self.download_dir.join(filename);
        if !self.destination_is_taken(&candidate) {
            return candidate;
        }
        let path = Path::new(filename);
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("file");
        let extension = path.extension().and_then(|value| value.to_str());
        for index in 1u32.. {
            let name = extension.map_or_else(
                || format!("{stem} ({index})"),
                |extension| format!("{stem} ({index}).{extension}"),
            );
            let candidate = self.download_dir.join(name);
            if !self.destination_is_taken(&candidate) {
                return candidate;
            }
        }
        unreachable!("u32 destination suffix space exhausted")
    }

    fn destination_is_taken(&self, candidate: &Path) -> bool {
        candidate.exists()
            || self.completed_files.contains(&candidate.to_path_buf())
            || self
                .incoming_files
                .values()
                .any(|file| file.final_path == candidate)
    }

    fn clear_outgoing(&mut self) {
        self.outgoing_files.clear();
    }

    fn set_pending_descriptors(
        &mut self,
        descriptors: Vec<crate::rdp::channels::clipboard::core::FileDescriptor>,
    ) {
        self.pending_descriptors = descriptors;
    }

    fn allocate_stream_id(&mut self) -> u32 {
        let id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(1);
        id
    }
}

fn finalize_without_replace(temp_path: &Path, final_path: &Path) -> std::io::Result<()> {
    nix::fcntl::renameat2(
        nix::fcntl::AT_FDCWD,
        temp_path,
        nix::fcntl::AT_FDCWD,
        final_path,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

/// Look up the actual RDP format ID for a MIME type from the stored format list.
///
/// Windows registered format IDs (like FileGroupDescriptorW) vary per session,
/// so we need to look them up from the actual format list sent by Windows.
fn lookup_format_id_for_mime(formats: &[ClipboardFormat], mime_type: &str) -> Option<u32> {
    use super::format_name_to_mime;

    // For text/plain, prefer CF_UNICODETEXT (13) over CF_TEXT (1)
    // CF_UNICODETEXT is UTF-16LE (full Unicode), CF_TEXT is ANSI (limited to Windows-1252)
    if mime_type == "text/plain;charset=utf-8" || mime_type == "text/plain" {
        if formats.iter().any(|f| f.id == 13) {
            debug!("Preferring CF_UNICODETEXT (13) for full Unicode support");
            return Some(13);
        }
        // Fall back to CF_TEXT if CF_UNICODETEXT not available
        if formats.iter().any(|f| f.id == 1) {
            debug!("Using CF_TEXT (1) ANSI fallback");
            return Some(1);
        }
    }

    // For all other MIME types, use normal lookup
    for format in formats {
        // First check if this format's ID maps to the requested MIME type
        if let Some(mapped_mime) = super::lib_rdp_format_to_mime(format.id)
            && mapped_mime == mime_type
        {
            return Some(format.id);
        }

        // For registered formats, check by name
        if let Some(ref name) = format.name
            && let Some(mapped_mime) = format_name_to_mime(name)
        {
            // Direct match
            if mapped_mime == mime_type {
                debug!("Found registered clipboard format ID {}", format.id);
                return Some(format.id);
            }
            // For file formats: x-special/gnome-copied-files and text/uri-list are equivalent
            // GNOME Nautilus requests gnome-copied-files, but RDP file formats map to uri-list
            if mapped_mime == "text/uri-list" && mime_type == "x-special/gnome-copied-files" {
                debug!("Found equivalent clipboard file format ID {}", format.id);
                return Some(format.id);
            }
        }
    }

    None
}

impl std::fmt::Debug for ClipboardOrchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardOrchestrator")
            .field("config", &self.config)
            .field(
                "has_clipboard_provider",
                &self
                    .clipboard_provider
                    .try_read()
                    .map(|g| g.is_some())
                    .unwrap_or(false),
            )
            .finish_non_exhaustive()
    }
}

impl ClipboardOrchestrator {
    pub async fn new(config: ClipboardOrchestratorConfig) -> Result<Self> {
        let converter = Arc::new(FormatConverter::new());

        let transfer_config = TransferConfig {
            chunk_size: config.chunk_size,
            max_size: config.max_data_size,
            timeout_ms: config.timeout_ms,
            verify_integrity: true,
        };
        let transfer_engine = Arc::new(TransferEngine::with_config(transfer_config));

        let loop_config = LoopDetectionConfig {
            window_ms: config.loop_detection_window_ms,
            max_history: 10,
            enable_content_hashing: true,
            rate_limit_ms: if config.rate_limit_ms > 0 {
                Some(config.rate_limit_ms)
            } else {
                None
            },
        };
        let sync_manager = Arc::new(RwLock::new(SyncManager::with_config(loop_config)));

        let (event_tx, event_rx) = mpsc::channel(100);

        // Stage received files into ~/Downloads (standard user location).
        // The systemd service unit must include ReadWritePaths=%h/Downloads.
        let download_dir = std::env::var("HOME").ok().map_or_else(
            || PathBuf::from("/tmp"),
            |h| PathBuf::from(h).join("Downloads"),
        );

        let file_transfer_state = Arc::new(RwLock::new(FileTransferState::new(download_dir)));

        let (shutdown_broadcast, _) = tokio::sync::broadcast::channel(16);
        let shutdown_broadcast = Arc::new(shutdown_broadcast);

        let task_handles = Arc::new(tokio::sync::Mutex::new(Vec::new()));

        let mut manager = Self {
            config,
            converter,
            transfer_engine,
            sync_manager,
            event_tx,
            shutdown_tx: None,
            pending_portal_requests: Arc::new(RwLock::new(std::collections::VecDeque::new())),
            server_event_sender: Arc::new(RwLock::new(None)), // Set by WrdCliprdrFactory
            clipboard_provider: Arc::new(RwLock::new(None)),
            file_transfer_state,
            current_rdp_formats: Arc::new(RwLock::new(Vec::new())),
            local_advertised_formats: Arc::new(RwLock::new(Vec::new())),
            health_reporter: None,
            shutdown_broadcast: Arc::clone(&shutdown_broadcast),
            task_handles: Arc::clone(&task_handles),
        };

        manager.start_event_processor(event_rx).await;

        debug!("Clipboard manager initialized");

        Ok(manager)
    }

    pub fn event_sender(&self) -> mpsc::Sender<ClipboardEvent> {
        self.event_tx.clone()
    }

    /// Set server event sender (called by WrdpCliprdrFactory after initialization)
    pub async fn set_server_event_sender(
        &self,
        sender: mpsc::UnboundedSender<ironrdp_server::ServerEvent>,
    ) {
        *self.server_event_sender.write().await = Some(sender);
        debug!(" ServerEvent sender registered with clipboard manager");
    }
    /// Wire a health reporter so clipboard operations emit health events.
    pub fn set_health_reporter(
        &mut self,
        reporter: crate::rdp::session::supervision::SessionStatusReporter,
    ) {
        self.health_reporter = Some(reporter);
    }

    /// Set the clipboard provider (trait-abstracted backend).
    ///
    /// The provider manages its own listener tasks internally; this method
    /// subscribes to the provider's event stream and forwards events to
    /// the orchestrator's main event channel.
    pub async fn set_clipboard_provider(
        &mut self,
        provider: Arc<dyn crate::rdp::channels::clipboard::provider::ClipboardProvider>,
    ) {
        info!("Setting clipboard provider: {}", provider.name());

        *self.clipboard_provider.write().await = Some(Arc::clone(&provider));

        // Subscribe to provider events and forward to our event channel
        let mut provider_rx = provider.subscribe();
        let event_tx = self.event_tx.clone();
        let pending_requests = Arc::clone(&self.pending_portal_requests);
        let mut shutdown_rx = self.shutdown_broadcast.subscribe();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = provider_rx.recv() => {
                        match event {
                            crate::rdp::channels::clipboard::provider::ClipboardProviderEvent::SelectionChanged {
                                mime_types,
                                force,
                            } => {
                                if let Err(e) = event_tx
                                    .send(ClipboardEvent::PortalFormatsAvailable(mime_types, force))
                                    .await
                                {
                                    error!("Failed to forward SelectionChanged to orchestrator: {e}");
                                    break;
                                }
                            }
                            crate::rdp::channels::clipboard::provider::ClipboardProviderEvent::SelectionTransfer {
                                serial,
                                mime_type,
                            } => {
                                // Track in pending requests queue for FIFO correlation
                                pending_requests.write().await.push_back((
                                    serial,
                                    mime_type.clone(),
                                    std::time::Instant::now(),
                                ));

                                if let Err(e) = event_tx
                                    .send(ClipboardEvent::PortalDataRequest(mime_type))
                                    .await
                                {
                                    error!("Failed to forward SelectionTransfer to orchestrator: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Provider event forwarder received shutdown");
                        break;
                    }
                }
            }
        });

        self.task_handles.lock().await.push(handle);

        debug!("Clipboard provider event forwarder started");
    }

    /// Run a health check on the active clipboard provider.
    ///
    /// Returns Ok if provider is healthy or no provider is set.
    /// Returns Err if the provider's health check fails.
    pub async fn health_check_provider(
        &self,
    ) -> crate::rdp::channels::clipboard::error::Result<()> {
        let provider_opt = self.clipboard_provider.read().await;
        if let Some(ref provider) = *provider_opt {
            provider.health_check().await
        } else {
            Ok(())
        }
    }

    /// Start event processing loop
    async fn start_event_processor(&mut self, mut event_rx: mpsc::Receiver<ClipboardEvent>) {
        let converter = self.converter.clone();
        let sync_manager = self.sync_manager.clone();
        let transfer_engine = self.transfer_engine.clone();
        let config = self.config.clone();
        let clipboard_provider = Arc::clone(&self.clipboard_provider);
        let pending_portal_requests = Arc::clone(&self.pending_portal_requests);
        let server_event_sender = Arc::clone(&self.server_event_sender);
        let file_transfer_state = Arc::clone(&self.file_transfer_state);
        let current_rdp_formats = Arc::clone(&self.current_rdp_formats);
        let local_advertised_formats = Arc::clone(&self.local_advertised_formats);
        let health_reporter = self.health_reporter.clone();

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        self.shutdown_tx = Some(shutdown_tx);

        let handle = tokio::spawn(async move {
            let mut consecutive_errors: u32 = 0;

            loop {
                tokio::select! {
                    Some(event) = event_rx.recv() => {
                        if let Err(e) = Self::handle_event(
                            event,
                            &converter,
                            &sync_manager,
                            &transfer_engine,
                            &config,
                            &clipboard_provider,
                            &pending_portal_requests,
                            &server_event_sender,
                            &file_transfer_state,
                            &current_rdp_formats,
                            &local_advertised_formats,
                        ).await {
                            let err_msg = format!("{e}");
                            error!("Error handling clipboard event: {err_msg}");
                            consecutive_errors += 1;

                            // Session-invalid errors are fatal — report immediately
                            let is_session_invalid = err_msg.contains("session invalid")
                                || err_msg.contains("Session invalid");
                            if (is_session_invalid || consecutive_errors >= 3)
                                && let Some(ref reporter) = health_reporter
                            {
                                reporter.report_aspect_health(
                                    crate::rdp::session::supervision::SessionAspectId::Clipboard,
                                    crate::rdp::session::supervision::AspectHealth::Failed(format!(
                                        "{consecutive_errors} consecutive errors: {e}"
                                    )),
                                );
                            }
                        } else if consecutive_errors > 0 {
                            // Recovered after errors
                            if consecutive_errors >= 3
                                && let Some(ref reporter) = health_reporter
                            {
                                reporter.report_aspect_health(
                                    crate::rdp::session::supervision::SessionAspectId::Clipboard,
                                    crate::rdp::session::supervision::AspectHealth::Healthy,
                                );
                            }
                            consecutive_errors = 0;
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        debug!("Clipboard manager shutting down");
                        break;
                    }
                }
            }
        });
        self.task_handles.lock().await.push(handle);
    }

    /// Handle a clipboard event
    #[expect(
        clippy::too_many_arguments,
        reason = "orchestrator dispatches with shared state refs"
    )]
    async fn handle_event(
        event: ClipboardEvent,
        converter: &FormatConverter,
        sync_manager: &Arc<RwLock<SyncManager>>,
        transfer_engine: &TransferEngine,
        config: &ClipboardOrchestratorConfig,
        clipboard_provider: &SharedClipboardProvider,
        pending_portal_requests: &PendingPortalRequests,
        server_event_sender: &ServerEventSender,
        file_transfer_state: &Arc<RwLock<FileTransferState>>,
        current_rdp_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
        local_advertised_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
    ) -> Result<()> {
        match event {
            ClipboardEvent::RdpReady => {
                debug!(
                    "RDP clipboard channel ready - checking for pending Linux clipboard to announce"
                );
                // When RDP becomes ready, re-announce any cached Linux clipboard formats
                // This handles the case where Linux clipboard changed before RDP connected
                let advertised = local_advertised_formats.read().await;
                if !advertised.is_empty() {
                    info!(
                        "Re-announcing {} cached Linux clipboard formats to RDP",
                        advertised.len()
                    );
                    let formats_to_send = advertised.clone();
                    drop(advertised);

                    let sender_opt = server_event_sender.read().await.clone();
                    if let Some(sender) = sender_opt {
                        use ironrdp_cliprdr::backend::ClipboardMessage;

                        let rdp_formats: Vec<ironrdp_cliprdr::pdu::ClipboardFormat> =
                            formats_to_send
                                .iter()
                                .map(|f| {
                                    let name = f.name.as_ref().map(|n| {
                                        ironrdp_cliprdr::pdu::ClipboardFormatName::new(n.clone())
                                    });
                                    ironrdp_cliprdr::pdu::ClipboardFormat {
                                        id: ironrdp_cliprdr::pdu::ClipboardFormatId(f.id),
                                        name,
                                    }
                                })
                                .collect();

                        info!(
                            "Re-sending FormatList to RDP client with {} formats",
                            rdp_formats.len()
                        );
                        if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                            ClipboardMessage::SendInitiateCopy(rdp_formats),
                        )) {
                            error!("Failed to re-send FormatList: {:?}", e);
                        }
                    }
                } else {
                    debug!("No cached Linux clipboard formats to announce");
                }
                Ok(())
            }

            ClipboardEvent::RdpFormatList(formats) => {
                Self::handle_rdp_format_list(
                    formats,
                    converter,
                    sync_manager,
                    clipboard_provider,
                    current_rdp_formats,
                    server_event_sender,
                    pending_portal_requests,
                )
                .await
            }

            ClipboardEvent::RdpDataRequest(format_id, _response_callback) => {
                Self::handle_rdp_data_request(
                    format_id,
                    converter,
                    sync_manager,
                    clipboard_provider,
                    server_event_sender,
                    local_advertised_formats,
                    file_transfer_state,
                )
                .await
            }

            ClipboardEvent::RdpDataResponse(data) => {
                Self::handle_rdp_data_response(
                    data,
                    sync_manager,
                    transfer_engine,
                    clipboard_provider,
                    pending_portal_requests,
                    file_transfer_state,
                    server_event_sender,
                )
                .await
            }

            ClipboardEvent::RdpDataError => {
                Self::handle_rdp_data_error(clipboard_provider, pending_portal_requests).await
            }

            ClipboardEvent::RdpFileContentsRequest {
                stream_id,
                list_index,
                position,
                size,
                is_size_request,
            } => {
                Self::handle_rdp_file_contents_request(
                    stream_id,
                    list_index,
                    position,
                    size,
                    is_size_request,
                    server_event_sender,
                    file_transfer_state,
                )
                .await
            }

            ClipboardEvent::RdpFileContentsResponse {
                stream_id,
                data,
                is_error,
            } => {
                let result = Self::handle_rdp_file_contents_response(
                    stream_id,
                    data,
                    is_error,
                    file_transfer_state,
                    clipboard_provider,
                    server_event_sender,
                )
                .await;
                if result.is_err() {
                    // Failed size/range validation or I/O must not leave a
                    // pending portal request and partial files alive indefinitely.
                    let serial = {
                        let mut state = file_transfer_state.write().await;
                        let serial = state.portal_serial;
                        state.clear_incoming();
                        serial
                    };
                    let provider = clipboard_provider.read().await.clone();
                    if let (Some(serial), Some(provider)) = (serial, provider) {
                        let _ = provider
                            .complete_transfer(
                                serial,
                                "x-special/gnome-copied-files",
                                Vec::new(),
                                false,
                            )
                            .await;
                    }
                }
                result
            }

            ClipboardEvent::PortalFormatsAvailable(mime_types, force) => {
                Self::handle_portal_formats(
                    mime_types,
                    force,
                    converter,
                    config,
                    sync_manager,
                    server_event_sender,
                    local_advertised_formats,
                )
                .await
            }

            ClipboardEvent::PortalDataRequest(mime_type) => {
                Self::handle_portal_data_request(
                    mime_type,
                    converter,
                    sync_manager,
                    server_event_sender,
                    current_rdp_formats,
                )
                .await
            }
        }
    }

    /// Handle RDP format list announcement
    #[expect(
        clippy::too_many_arguments,
        reason = "orchestrator handler with shared state refs"
    )]
    async fn handle_rdp_format_list(
        formats: Vec<ClipboardFormat>,
        _converter: &FormatConverter,
        sync_manager: &Arc<RwLock<SyncManager>>,
        clipboard_provider: &SharedClipboardProvider,
        current_rdp_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
        server_event_sender: &ServerEventSender,
        pending_portal_requests: &PendingPortalRequests,
    ) -> Result<()> {
        debug!("RDP format list received with {} entries", formats.len());

        // Registered format IDs vary per session, store for later lookup
        {
            let mut stored_formats = current_rdp_formats.write().await;
            stored_formats.clone_from(&formats);
            debug!(
                "Stored {} RDP formats for format ID lookup",
                stored_formats.len()
            );
        }

        let should_sync = {
            let mut mgr = sync_manager.write().await;
            mgr.handle_rdp_formats(formats.clone())
        };

        if !should_sync {
            debug!("Skipping RDP format list due to loop detection");
            return Ok(());
        }

        let mime_types = super::rdp_formats_to_mimes(&formats);

        debug!("Converted RDP formats to {} MIME types", mime_types.len());

        if mime_types.is_empty() {
            debug!("Empty format list from RDP client (handshake only, no clipboard content)");
            return Ok(());
        }
        debug!("Publishing {} clipboard MIME types", mime_types.len());

        info!(
            "Preparing provider selection with {} MIME types",
            mime_types.len()
        );

        // Announce via clipboard provider. Data-control sources are synchronous:
        // publishing the MIME list before bytes arrive creates an empty source (or,
        // worse, briefly republishes bytes cached for the previous selection).
        let provider_opt = clipboard_provider.read().await;
        if let Some(ref provider) = *provider_opt {
            if provider.requires_upfront_data() {
                // Retire the previous synchronous source immediately. Keeping it
                // active while the new bytes are fetched lets local clients paste
                // data from the superseded RDP selection.
                provider.announce_formats(Vec::new()).await.map_err(|e| {
                    ClipboardError::PortalError(format!("Provider clear selection failed: {e}"))
                })?;

                let has_text = mime_types.iter().any(|m| m.starts_with("text/plain"));
                let has_cf_unicodetext = formats.iter().any(|f| f.id == 13);

                if has_text && has_cf_unicodetext {
                    info!("Data-control provider: eagerly fetching CF_UNICODETEXT from RDP client");

                    // Queue a sentinel entry so handle_rdp_data_response knows this is eager fetch.
                    // The response handler publishes the source only after caching the bytes.
                    pending_portal_requests.write().await.push_back((
                        EAGER_FETCH_SERIAL,
                        "text/plain".to_string(),
                        std::time::Instant::now(),
                    ));

                    let sender_opt = server_event_sender.read().await.clone();
                    if let Some(sender) = sender_opt {
                        use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::ClipboardFormatId};

                        if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                            ClipboardMessage::SendInitiatePaste(ClipboardFormatId(13)),
                        )) {
                            warn!("Failed to send eager fetch for CF_UNICODETEXT: {:?}", e);
                            let mut pending = pending_portal_requests.write().await;
                            pending.retain(|(s, _, _)| *s != EAGER_FETCH_SERIAL);
                        }
                    } else {
                        warn!("ServerEvent sender not available for eager fetch");
                        pending_portal_requests
                            .write()
                            .await
                            .retain(|(s, _, _)| *s != EAGER_FETCH_SERIAL);
                    }
                } else {
                    warn!(
                        "Data-control provider cannot publish this RDP selection without upfront data"
                    );
                }
            } else {
                provider
                    .announce_formats(mime_types.clone())
                    .await
                    .map_err(|e| {
                        ClipboardError::PortalError(format!(
                            "Provider announce_formats failed: {e}"
                        ))
                    })?;
                debug!(
                    "RDP clipboard formats announced via {} provider",
                    provider.name()
                );
            }
        } else {
            debug!("No clipboard provider available (normal during startup)");
        }

        Ok(())
    }

    /// Handle RDP data request (Linux → Windows paste)
    #[expect(
        clippy::too_many_arguments,
        reason = "orchestrator handler with shared state refs"
    )]
    async fn handle_rdp_data_request(
        format_id: u32,
        _converter: &FormatConverter,
        _sync_manager: &Arc<RwLock<SyncManager>>,
        clipboard_provider: &SharedClipboardProvider,
        server_event_sender: &ServerEventSender,
        local_advertised_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
        file_transfer_state: &Arc<RwLock<FileTransferState>>,
    ) -> Result<()> {
        info!(
            "RDP data request for format ID: {} (Linux → Windows paste)",
            format_id
        );

        // Normal path: read from the local clipboard provider
        let advertised = local_advertised_formats.read().await;
        let format_name = advertised
            .iter()
            .find(|f| f.id == format_id || (format_id == 0 && f.name.is_some()))
            .and_then(|f| f.name.clone());
        drop(advertised);

        if let Some(ref name) = format_name
            && name == "FileGroupDescriptorW"
        {
            debug!(
                "Windows requests FileGroupDescriptorW - sending file list from Linux clipboard"
            );
            return Self::handle_file_descriptor_request(
                clipboard_provider,
                server_event_sender,
                file_transfer_state,
            )
            .await;
        }

        let mime_type = match super::lib_rdp_format_to_mime(format_id).map(str::to_string) {
            Some(m) => m,
            None => {
                warn!("Unknown clipboard format ID {format_id}");
                Self::send_format_data_error(server_event_sender).await;
                return Ok(());
            }
        };
        debug!("Mapped clipboard format ID {format_id} to a MIME type");

        let portal_data = {
            let provider_opt = clipboard_provider.read().await;
            if let Some(ref provider) = *provider_opt {
                match provider.read_data(&mime_type).await {
                    Ok(data) => {
                        info!(
                            "Read {} clipboard bytes from {} provider",
                            data.len(),
                            provider.name()
                        );
                        data
                    }
                    Err(e) => {
                        error!("Failed to read from {} provider: {:#}", provider.name(), e);
                        Self::send_format_data_error(server_event_sender).await;
                        return Ok(());
                    }
                }
            } else {
                warn!("No clipboard provider available for RDP data request");
                Self::send_format_data_error(server_event_sender).await;
                return Ok(());
            }
        };

        let rdp_data = if format_id == 13 {
            // CF_UNICODETEXT - Convert UTF-8 to UTF-16LE with line ending conversion
            let text = String::from_utf8_lossy(&portal_data);
            // Sanitize text for Windows: LF → CRLF, remove null bytes
            let sanitized = sanitize_text_for_windows(&text);
            let utf16: Vec<u16> = sanitized.encode_utf16().collect();
            let mut bytes = Vec::with_capacity(utf16.len() * 2 + 2);
            for c in utf16 {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
            bytes.extend_from_slice(&[0, 0]); // Null terminator
            debug!(
                "Converted UTF-8 ({} bytes) to UTF-16LE ({} bytes) with CRLF line endings",
                portal_data.len(),
                bytes.len()
            );
            bytes
        } else if format_id == 8 {
            // CF_DIB - Windows wants DIB, Portal has image format
            if mime_type.starts_with("image/png") {
                trace!(" Converting PNG to DIB for Windows");
                crate::rdp::channels::clipboard::core::image::png_to_dib(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else if mime_type.starts_with("image/jpeg") {
                trace!(" Converting JPEG to DIB for Windows");
                crate::rdp::channels::clipboard::core::image::jpeg_to_dib(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else if mime_type.starts_with("image/bmp") || mime_type.starts_with("image/x-bmp") {
                trace!(" Converting BMP to DIB for Windows");
                crate::rdp::channels::clipboard::core::image::bmp_to_dib(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else {
                debug!("Unknown image MIME for DIB; passing through");
                portal_data
            }
        } else if format_id == 17 {
            // CF_DIBV5 - Windows wants DIBV5 with alpha channel support
            if mime_type.starts_with("image/png") {
                trace!(" Converting PNG to DIBV5 for Windows (with alpha)");
                crate::rdp::channels::clipboard::core::image::png_to_dibv5(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else if mime_type.starts_with("image/jpeg") {
                trace!(" Converting JPEG to DIBV5 for Windows");
                crate::rdp::channels::clipboard::core::image::jpeg_to_dibv5(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else {
                // Unsupported MIME for DIBV5, fall back to raw data
                debug!("Unknown image MIME for DIBV5; passing through");
                portal_data
            }
        } else if format_id == 0xD011 {
            // CF_PNG - Windows wants PNG
            if mime_type.starts_with("image/png") {
                debug!("PNG to PNG - pass through");
                portal_data
            } else {
                debug!("Unsupported clipboard image conversion to PNG");
                portal_data
            }
        } else {
            debug!(
                "Format {} - pass through {} bytes",
                format_id,
                portal_data.len()
            );
            portal_data
        };

        let data_len = rdp_data.len();
        debug!("Converted to RDP format: {} bytes", data_len);

        let sender_opt = server_event_sender.read().await.clone();
        if let Some(sender) = sender_opt {
            use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::FormatDataResponse};
            use ironrdp_pdu::IntoOwned;

            let response = FormatDataResponse::new_data(rdp_data);
            let owned_response = response.into_owned();

            match sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendFormatData(owned_response),
            )) {
                Err(e) => {
                    error!("Failed to send FormatDataResponse via ServerEvent: {:?}", e);
                }
                _ => {
                    info!(
                        "Sent {} bytes to RDP client for format {} (Linux → Windows)",
                        data_len, format_id
                    );
                }
            }
        } else {
            warn!("ServerEvent sender not available - cannot send clipboard data to RDP");
        }

        Ok(())
    }

    /// Handle FileGroupDescriptorW request from Windows (Linux → Windows file transfer)
    ///
    /// Reads file URIs from the local clipboard provider and converts to Windows FILEDESCRIPTORW format.
    async fn handle_file_descriptor_request(
        clipboard_provider: &SharedClipboardProvider,
        server_event_sender: &ServerEventSender,
        file_transfer_state: &Arc<RwLock<FileTransferState>>,
    ) -> Result<()> {
        // Read file URIs: prefer x-special/gnome-copied-files, fall back to text/uri-list
        let uri_data = {
            let provider_opt = clipboard_provider.read().await;
            if let Some(ref provider) = *provider_opt {
                match provider.read_data("x-special/gnome-copied-files").await {
                    Ok(data) if !data.is_empty() => {
                        info!(
                            "Read {} bytes from {} provider (x-special/gnome-copied-files)",
                            data.len(),
                            provider.name()
                        );
                        data
                    }
                    _ => match provider.read_data("text/uri-list").await {
                        Ok(data) => {
                            info!(
                                "Read {} bytes from {} provider (text/uri-list)",
                                data.len(),
                                provider.name()
                            );
                            data
                        }
                        Err(e) => {
                            error!(
                                "Failed to read file URIs from {} provider: {:#}",
                                provider.name(),
                                e
                            );
                            Self::send_format_data_error(server_event_sender).await;
                            return Ok(());
                        }
                    },
                }
            } else {
                warn!("No clipboard provider available for file descriptor request");
                Self::send_format_data_error(server_event_sender).await;
                return Ok(());
            }
        };

        let file_paths = parse_file_uris(&uri_data);

        trace!("Found {} valid clipboard file path(s)", file_paths.len());

        if file_paths.is_empty() {
            warn!("No valid file paths found in clipboard");
            Self::send_format_data_error(server_event_sender).await;
            return Ok(());
        }

        {
            let mut state = file_transfer_state.write().await;
            state.clear_outgoing();
            for path in &file_paths {
                if let Ok(metadata) = std::fs::metadata(path) {
                    state.outgoing_files.push(OutgoingFile {
                        path: path.clone(),
                        size: metadata.len(),
                    });
                }
            }
            info!(
                "Stored {} outgoing files for transfer",
                state.outgoing_files.len()
            );
        }

        let descriptor_data =
            match crate::rdp::channels::clipboard::core::build_file_group_descriptor_w(&file_paths)
            {
                Ok(data) => {
                    info!(
                        "Built FileGroupDescriptorW ({} bytes) for {} files",
                        data.len(),
                        file_paths.len()
                    );
                    data
                }
                Err(e) => {
                    error!("Failed to build FileGroupDescriptorW: {:?}", e);
                    Self::send_format_data_error(server_event_sender).await;
                    return Ok(());
                }
            };

        let sender_opt = server_event_sender.read().await.clone();
        if let Some(sender) = sender_opt {
            use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::FormatDataResponse};
            use ironrdp_pdu::IntoOwned;

            let response = FormatDataResponse::new_data(descriptor_data);
            let owned_response = response.into_owned();

            match sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendFormatData(owned_response),
            )) {
                Err(e) => {
                    error!("Failed to send FileGroupDescriptorW response: {:?}", e);
                }
                _ => {
                    debug!(" Sent FileGroupDescriptorW to Windows (Linux → Windows file transfer)");
                }
            }
        }

        Ok(())
    }

    /// Send error response for FormatDataRequest
    async fn send_format_data_error(server_event_sender: &ServerEventSender) {
        let sender_opt = server_event_sender.read().await.clone();
        if let Some(sender) = sender_opt {
            use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::FormatDataResponse};
            use ironrdp_pdu::IntoOwned;

            let response = FormatDataResponse::new_error();
            let owned_response = response.into_owned();

            match sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendFormatData(owned_response),
            )) {
                Err(e) => {
                    error!("Failed to send error FormatDataResponse: {:?}", e);
                }
                _ => {
                    debug!("Sent error FormatDataResponse to RDP client");
                }
            }
        }
    }

    /// Handle RDP data response (Windows → Linux paste completion)
    #[expect(
        clippy::too_many_arguments,
        reason = "orchestrator handler with shared state refs"
    )]
    #[expect(
        clippy::expect_used,
        reason = "provider existence verified by caller before this path"
    )]
    async fn handle_rdp_data_response(
        data: Vec<u8>,
        sync_manager: &Arc<RwLock<SyncManager>>,
        _transfer_engine: &TransferEngine,
        clipboard_provider: &SharedClipboardProvider,
        pending_portal_requests: &PendingPortalRequests,
        file_transfer_state: &Arc<RwLock<FileTransferState>>,
        server_event_sender: &ServerEventSender,
    ) -> Result<()> {
        debug!("RDP data response received: {} bytes", data.len());

        let should_transfer = sync_manager.write().await.check_content(&data, true);
        if !should_transfer {
            debug!("Skipping RDP data due to content loop detection");
            return Ok(());
        }

        let provider_opt = clipboard_provider.read().await.clone();
        if provider_opt.is_none() {
            warn!("No clipboard provider available - cannot deliver clipboard data");
            return Ok(());
        }

        // Get FIRST pending request (FIFO order)
        // IronRDP doesn't correlate requests/responses, so we use FIFO queue
        let mut pending = pending_portal_requests.write().await;
        let request_opt = pending.pop_front();
        drop(pending);

        let (serial, requested_mime, _request_time) = match request_opt {
            Some(req) => req,
            None => {
                warn!("No pending request - FormatDataResponse arrived with no matching request");
                return Ok(());
            }
        };

        info!(
            "Matched FormatDataResponse to serial {} (FIFO queue)",
            serial
        );
        debug!(
            "Received {} requested clipboard bytes from Windows",
            data.len()
        );

        // Eager fetch for data-control: provide data upfront instead of completing a transfer
        if serial == EAGER_FETCH_SERIAL {
            let provider = provider_opt.as_ref().expect("provider checked above");

            // Convert UTF-16LE from CF_UNICODETEXT to UTF-8
            if data.len() >= 2 {
                let utf16_data: Vec<u16> = data
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .take_while(|&c| c != 0)
                    .collect();

                let text = String::from_utf16_lossy(&utf16_data);
                let sanitized = sanitize_text_for_linux(&text);
                let utf8_bytes = sanitized.as_bytes().to_vec();

                info!(
                    "Eager fetch: {} UTF-16 chars → {} UTF-8 bytes for data-control source",
                    utf16_data.len(),
                    utf8_bytes.len()
                );

                // Provide under both bare and charset-qualified MIME types so the
                // compositor finds data regardless of which key it requests via `send`
                if let Err(e) = provider
                    .provide_data("text/plain", utf8_bytes.clone())
                    .await
                {
                    warn!("Failed to provide eager-fetched text to data-control: {e}");
                }
                if let Err(e) = provider
                    .provide_data("text/plain;charset=utf-8", utf8_bytes)
                    .await
                {
                    warn!("Failed to provide eager-fetched text (charset): {e}");
                }

                // Data-control sources are synchronous: some backends snapshot
                // available bytes at SetClipboard time, so updating source data
                // after the initial empty announcement is not enough. Re-publish
                // the text MIME list now that bytes are cached.
                if provider.requires_upfront_data()
                    && let Err(e) = provider
                        .announce_formats(vec![
                            "text/plain".to_string(),
                            "text/plain;charset=utf-8".to_string(),
                        ])
                        .await
                {
                    warn!("Failed to re-announce eager-fetched text to data-control: {e}");
                }
            } else {
                debug!(
                    "Eager fetch: data too small ({} bytes), skipping",
                    data.len()
                );
            }

            return Ok(());
        }

        // Special handling for file transfer formats
        if requested_mime == "text/uri-list" || requested_mime == "x-special/gnome-copied-files" {
            info!(
                "Received FileGroupDescriptorW data ({} bytes) - parsing file list",
                data.len()
            );

            match crate::rdp::channels::clipboard::core::FileDescriptor::parse_list(&data) {
                Ok(descriptors) => {
                    info!(
                        "Parsed {} file descriptor(s) from Windows",
                        descriptors.len()
                    );

                    let total_bytes: u64 = descriptors
                        .iter()
                        .filter_map(|descriptor| descriptor.size)
                        .sum();
                    debug!("Incoming clipboard files total {total_bytes} bytes");

                    // Check for duplicate file transfer (apps send both text/uri-list and
                    // x-special/gnome-copied-files for the same paste)
                    {
                        let state = file_transfer_state.read().await;
                        if !state.incoming_files.is_empty() {
                            info!(
                                "Skipping duplicate clipboard file request; transfer already has {} file(s)",
                                state.incoming_files.len()
                            );
                            drop(state);

                            if let Some(ref provider) = provider_opt {
                                let _ = provider
                                    .complete_transfer(serial, &requested_mime, vec![], false)
                                    .await;
                            }
                            return Ok(());
                        }
                    }

                    // Download files before publishing local URIs.
                    info!("Downloading remote clipboard files before URI publication");

                    let sender_opt = server_event_sender.read().await.clone();
                    let sender = match sender_opt {
                        Some(s) => s,
                        None => {
                            error!(
                                "ServerEvent sender not available - cannot request file contents"
                            );
                            if let Some(ref provider) = provider_opt {
                                let _ = provider
                                    .complete_transfer(serial, &requested_mime, vec![], false)
                                    .await;
                            }
                            return Ok(());
                        }
                    };

                    {
                        let mut state = file_transfer_state.write().await;

                        state.clear_incoming();
                        state.set_pending_descriptors(descriptors.clone());
                        state.portal_serial = Some(serial);

                        use ironrdp_cliprdr::{
                            backend::ClipboardMessage,
                            pdu::{FileContentsFlags, FileContentsRequest},
                        };

                        let clip_data_id = 1u32;
                        for (idx, desc) in descriptors.iter().enumerate() {
                            let stream_id = state.allocate_stream_id();
                            let original_name = &desc.name;
                            let filename = sanitize_filename_for_linux(original_name);
                            let final_path = state.reserve_destination(&filename);
                            let total_size = desc.size;

                            info!(
                                "Requesting clipboard file {}/{} ({} bytes, stream_id={})",
                                idx + 1,
                                descriptors.len(),
                                total_size
                                    .map_or_else(|| "unknown".to_string(), |size| size.to_string()),
                                stream_id
                            );

                            let temp_path = state
                                .download_dir
                                .join(format!(".{filename}.{stream_id}.tmp"));

                            if let Err(e) = std::fs::create_dir_all(&state.download_dir) {
                                error!("Failed to create download directory: {}", e);
                                continue;
                            }

                            let file_handle = match OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .mode(0o600)
                                .open(&temp_path)
                            {
                                Ok(f) => f,
                                Err(e) => {
                                    error!("Failed to create clipboard temp file: {e}");
                                    continue;
                                }
                            };

                            let incoming = IncomingFile {
                                final_path,
                                total_size,
                                awaiting_size: total_size.is_none() || total_size == Some(0),
                                received_size: 0,
                                temp_path,
                                file_handle,
                                file_index: idx.try_into().unwrap_or(u32::MAX),
                                clip_data_id,
                            };
                            state.incoming_files.insert(stream_id, incoming);

                            let (flags, request_size) = match total_size {
                                Some(size) if size > 0 => {
                                    (FileContentsFlags::RANGE, size.min(64 * 1024 * 1024) as u32)
                                }
                                _ => (FileContentsFlags::SIZE, 8),
                            };

                            match sender.send(ironrdp_server::ServerEvent::Clipboard(
                                ClipboardMessage::SendFileContentsRequest(FileContentsRequest {
                                    stream_id,
                                    index: idx.try_into().unwrap_or(i32::MAX),
                                    flags,
                                    position: 0,
                                    requested_size: request_size,
                                    data_id: Some(clip_data_id),
                                }),
                            )) {
                                Err(e) => {
                                    error!("Failed to send FileContentsRequest: {e:?}");
                                }
                                _ => {
                                    info!(
                                        "Sent FileContentsRequest (stream={}, {} bytes, clip_data_id={})",
                                        stream_id, request_size, clip_data_id
                                    );
                                }
                            }
                        }

                        info!(
                            "Initiated staging transfer for {} file(s), waiting for responses...",
                            state.incoming_files.len()
                        );
                    }

                    return Ok(());
                }
                Err(e) => {
                    error!("Failed to parse FileGroupDescriptorW: {:?}", e);
                    // Fall through to generic handling
                }
            }
        }

        let portal_data = if requested_mime.starts_with("image/png") {
            // Provider wants PNG, Windows sent DIB or DIBV5
            // Auto-detect format based on header size
            if data.len() >= 4 {
                let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                match header_size {
                    124 => {
                        // DIBV5 format with alpha channel
                        trace!(" Converting DIBV5 to PNG for Portal (with alpha)");
                        crate::rdp::channels::clipboard::core::image::dibv5_to_png(&data).map_err(
                            |e| {
                                error!("DIBV5 to PNG conversion failed: {}", e);
                                ClipboardError::Core(e)
                            },
                        )?
                    }
                    40 => {
                        // Standard DIB format
                        trace!(" Converting DIB to PNG for Portal");
                        crate::rdp::channels::clipboard::core::image::dib_to_png(&data).map_err(
                            |e| {
                                error!("DIB to PNG conversion failed: {}", e);
                                ClipboardError::Core(e)
                            },
                        )?
                    }
                    _ => {
                        // Unknown header size, try DIBV5 parser which handles both
                        debug!(
                            "Unknown bitmap header size {}, trying auto-detect",
                            header_size
                        );
                        crate::rdp::channels::clipboard::core::image::dibv5_to_png(&data).map_err(
                            |e| {
                                error!("Bitmap to PNG conversion failed: {}", e);
                                ClipboardError::Core(e)
                            },
                        )?
                    }
                }
            } else {
                error!(
                    "Image data too small for bitmap header: {} bytes",
                    data.len()
                );
                return Err(ClipboardError::Core(
                    crate::rdp::channels::clipboard::core::ClipboardError::ImageDecode(
                        "Data too small for bitmap".to_string(),
                    ),
                ));
            }
        } else if requested_mime.starts_with("image/jpeg") {
            // Provider wants JPEG, Windows sent DIB or DIBV5
            if data.len() >= 4 {
                let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                if header_size == 124 {
                    trace!(" Converting DIBV5 to JPEG for Portal");
                    crate::rdp::channels::clipboard::core::image::dibv5_to_jpeg(&data).map_err(
                        |e| {
                            error!("DIBV5 to JPEG conversion failed: {}", e);
                            ClipboardError::Core(e)
                        },
                    )?
                } else {
                    trace!(" Converting DIB to JPEG for Portal");
                    crate::rdp::channels::clipboard::core::image::dib_to_jpeg(&data).map_err(
                        |e| {
                            error!("DIB to JPEG conversion failed: {}", e);
                            ClipboardError::Core(e)
                        },
                    )?
                }
            } else {
                error!(
                    "Image data too small for bitmap header: {} bytes",
                    data.len()
                );
                return Err(ClipboardError::Core(
                    crate::rdp::channels::clipboard::core::ClipboardError::ImageDecode(
                        "Data too small for bitmap".to_string(),
                    ),
                ));
            }
        } else if requested_mime.starts_with("image/bmp")
            || requested_mime.starts_with("image/x-bmp")
        {
            // Provider wants BMP, Windows sent DIB
            trace!(" Converting DIB to BMP for Portal");
            crate::rdp::channels::clipboard::core::image::dib_to_bmp(&data).map_err(|e| {
                error!("DIB to BMP conversion failed: {}", e);
                ClipboardError::Core(e)
            })?
        } else if requested_mime == "text/rtf" || requested_mime == "application/rtf" {
            // RTF is plain ASCII/Latin-1 text, NOT UTF-16
            // Windows CF_RTF sends raw RTF markup as bytes
            debug!(
                "RTF format detected ({} bytes) - passing through with line ending conversion",
                data.len()
            );

            // Convert to string (lossy for any invalid UTF-8, though RTF should be ASCII)
            let text = String::from_utf8_lossy(&data);

            // Sanitize for Linux: CRLF → LF, remove null bytes
            let sanitized = sanitize_text_for_linux(&text);
            let rtf_bytes = sanitized.as_bytes().to_vec();

            debug!(
                "RTF: {} raw bytes → {} bytes after line ending conversion",
                data.len(),
                rtf_bytes.len()
            );
            if !rtf_bytes.is_empty() {
                let preview_len = rtf_bytes.len().min(80);
                debug!(
                    "RTF preview: {:?}",
                    String::from_utf8_lossy(&rtf_bytes[..preview_len])
                );
            }
            rtf_bytes
        } else if (requested_mime.starts_with("text/plain")
            || requested_mime.starts_with("text/html"))
            && data.len() >= 2
        {
            // text/plain and text/html from Windows are UTF-16LE (CF_UNICODETEXT)
            // MIME may have charset suffix like "text/plain;charset=utf-8"
            // Convert UTF-16LE to UTF-8 with line ending conversion
            let utf16_data: Vec<u16> = data
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .take_while(|&c| c != 0) // Stop at null terminator
                .collect();

            // Use lossy conversion to handle malformed UTF-16
            // This handles invalid surrogates and replaces them with U+FFFD
            let text = String::from_utf16_lossy(&utf16_data);

            // Sanitize for Linux: CRLF → LF, remove null bytes
            let sanitized = sanitize_text_for_linux(&text);
            let utf8_bytes = sanitized.as_bytes().to_vec();

            debug!(
                "Converted UTF-16 to UTF-8: {} UTF-16 chars ({} bytes) → {} UTF-8 bytes with LF line endings",
                utf16_data.len(),
                data.len(),
                utf8_bytes.len()
            );
            utf8_bytes
        } else {
            // Unknown format or too small - pass through
            debug!(
                "Unknown format or small data, using raw {} bytes",
                data.len()
            );
            data
        };

        // Deliver converted data via clipboard provider
        let provider = provider_opt.as_ref().expect("provider checked above");

        match provider
            .complete_transfer(serial, &requested_mime, portal_data, true)
            .await
        {
            Ok(()) => {
                info!(
                    "Clipboard data delivered via {} provider (serial {})",
                    provider.name(),
                    serial
                );

                // Cancel unfulfilled requests (apps send multiple MIME requests per paste)
                let mut pending = pending_portal_requests.write().await;
                let unfulfilled: Vec<(u32, String)> = pending
                    .iter()
                    .filter(|(s, _, _)| *s != serial)
                    .map(|(s, m, _)| (*s, m.clone()))
                    .collect();
                pending.clear();
                drop(pending);

                for (unfulfilled_serial, mime) in &unfulfilled {
                    if let Err(e) = provider
                        .complete_transfer(*unfulfilled_serial, mime, vec![], false)
                        .await
                    {
                        warn!("Failed to cancel serial {}: {}", unfulfilled_serial, e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to deliver clipboard data via provider: {:#}", e);
                pending_portal_requests
                    .write()
                    .await
                    .retain(|(s, _, _)| *s != serial);
            }
        }

        Ok(())
    }

    /// Provider-based data response handler
    ///
    /// Mirrors the provider-backed data response flow but handles an RDP-side
    /// error response by completing the pending transfer through the active
    /// clipboard backend.
    /// Handle RDP data error (must notify clipboard provider to prevent retry crash)
    ///
    /// This is called when the RDP client responds with FormatDataResponse(error=true),
    /// which is normal protocol behavior when the client doesn't have the requested format.
    /// Per MS-RDPECLIP, this is expected and not an error condition.
    async fn handle_rdp_data_error(
        clipboard_provider: &SharedClipboardProvider,
        pending_portal_requests: &PendingPortalRequests,
    ) -> Result<()> {
        debug!("RDP FormatDataResponse: format not available, notifying clipboard backend");

        let pending = pending_portal_requests.read().await;
        let entries: Vec<(u32, String)> = pending.iter().map(|(s, m, _)| (*s, m.clone())).collect();
        drop(pending);

        match *clipboard_provider.read().await {
            Some(ref provider) => {
                for (serial, mime_type) in &entries {
                    debug!(
                        "Notifying {} provider of transfer failure (serial {})",
                        provider.name(),
                        serial
                    );
                    if let Err(e) = provider
                        .complete_transfer(*serial, mime_type, vec![], false)
                        .await
                    {
                        warn!("Failed to notify provider of transfer failure: {:#}", e);
                    }
                }
            }
            _ => {
                warn!("No clipboard provider available to notify of transfer failure");
            }
        }

        pending_portal_requests.write().await.clear();
        Ok(())
    }

    /// Handle Portal format announcement (Linux → Windows)
    ///
    /// `force=true` from D-Bus extension overrides RDP ownership; `force=false` may be blocked.
    async fn handle_portal_formats(
        mime_types: Vec<String>,
        force: bool,
        _converter: &FormatConverter,
        config: &ClipboardOrchestratorConfig,
        sync_manager: &Arc<RwLock<SyncManager>>,
        server_event_sender: &ServerEventSender,
        local_advertised_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
    ) -> Result<()> {
        info!(
            "handle_portal_formats called with {} MIME types (force={})",
            mime_types.len(),
            force
        );

        let sync_decision = {
            let mut mgr = sync_manager.write().await;
            mgr.handle_portal_formats(mime_types.clone(), force)
        };

        match sync_decision {
            crate::rdp::channels::clipboard::sync::PortalSyncDecision::Allow => {
                // Normal Linux → Windows sync
                debug!("Sync decision: Allow - proceeding with normal sync");
            }

            crate::rdp::channels::clipboard::sync::PortalSyncDecision::Block => {
                debug!("Sync decision: Block - skipping Portal formats");
                return Ok(());
            }
        }

        let original_mime_count = mime_types.len();
        let filtered_mime_types: Vec<String> = mime_types
            .into_iter()
            .filter(|mime| {
                let mime_lower = mime.to_ascii_lowercase();
                if mime_lower.starts_with("text/html") && !config.enable_html {
                    return false;
                }
                if mime.eq_ignore_ascii_case("text/rtf") && !config.enable_rtf {
                    return false;
                }
                if mime.starts_with("image/") && !config.enable_images {
                    return false;
                }
                if mime.eq_ignore_ascii_case("text/uri-list") && !config.enable_files {
                    return false;
                }
                true
            })
            .collect();

        if filtered_mime_types.is_empty() {
            info!(
                "No portal clipboard MIME types remain after config filtering; skipping RDP announcement"
            );
            return Ok(());
        }

        if filtered_mime_types.len() != original_mime_count {
            info!(
                "Filtered portal clipboard MIME types by config: {} -> {}",
                original_mime_count,
                filtered_mime_types.len()
            );
        }

        let names = filtered_mime_types
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let rdp_formats = super::lib_mime_to_rdp_formats(&names);
        debug!(
            "Converted {} MIME types to {} RDP formats",
            filtered_mime_types.len(),
            rdp_formats.len()
        );

        let ironrdp_formats: Vec<ironrdp_cliprdr::pdu::ClipboardFormat> = rdp_formats
            .iter()
            .map(|f| ironrdp_cliprdr::pdu::ClipboardFormat {
                id: ironrdp_cliprdr::pdu::ClipboardFormatId(f.id),
                name: f
                    .name
                    .clone()
                    .map(ironrdp_cliprdr::pdu::ClipboardFormatName::new),
            })
            .collect();

        {
            let mut advertised = local_advertised_formats.write().await;
            advertised.clear();
            for fmt in &ironrdp_formats {
                advertised.push(ClipboardFormat {
                    id: fmt.id.0,
                    name: fmt.name.as_ref().map(|n| n.value().to_string()),
                });
            }
            debug!(
                "Stored {} advertised formats for data request lookup",
                advertised.len()
            );
        }

        debug!(
            "Sending clipboard format list with {} entries",
            ironrdp_formats.len()
        );

        let sender_opt = server_event_sender.read().await.clone();
        if let Some(sender) = sender_opt {
            use ironrdp_cliprdr::backend::ClipboardMessage;

            info!(
                "Sending ServerEvent::Clipboard(SendInitiateCopy) with {} formats to event loop",
                ironrdp_formats.len()
            );

            let send_result = sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendInitiateCopy(ironrdp_formats),
            ));

            match send_result {
                Ok(()) => {
                    debug!(" ServerEvent::Clipboard sent successfully to IronRDP event loop");
                    info!(
                        "   Event loop should now call cliprdr.initiate_copy() → encode FormatList PDU → send to client"
                    );
                }
                Err(e) => {
                    error!("Failed to send ServerEvent::Clipboard: {:?}", e);
                    error!("   This means the event loop channel is closed/dropped!");
                }
            }
        } else {
            warn!("ServerEvent sender not available - cannot announce formats to RDP");
        }

        Ok(())
    }

    /// Handle Portal data request (Windows → Linux paste initiation)
    ///
    /// When a Linux app pastes while RDP owns the clipboard, Portal sends
    /// SelectionTransfer which becomes PortalDataRequest. We ask the RDP
    /// client to supply the data via SendInitiatePaste.
    async fn handle_portal_data_request(
        mime_type: String,
        _converter: &FormatConverter,
        _sync_manager: &Arc<RwLock<SyncManager>>,
        server_event_sender: &ServerEventSender,
        current_rdp_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
    ) -> Result<()> {
        debug!("Portal clipboard data request received");

        // Look up the format ID from the actual Windows format list first.
        // This handles runtime-registered formats (FileGroupDescriptorW etc.)
        // whose IDs vary per session.
        let formats = current_rdp_formats.read().await;
        let format_id = if let Some(id) = lookup_format_id_for_mime(&formats, &mime_type) {
            id
        } else {
            // Fall back to static mapping for standard formats
            drop(formats);
            super::lib_mime_to_rdp_formats(&[&mime_type])
                .first()
                .map(|format| format.id)
                .ok_or_else(|| {
                    ClipboardError::Core(super::CoreClipboardError::UnsupportedFormat(
                        mime_type.clone(),
                    ))
                })?
        };

        info!("Portal needs data — requesting format {format_id} from RDP client");

        let sender_opt = server_event_sender.read().await.clone();
        if let Some(sender) = sender_opt {
            use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::ClipboardFormatId};

            if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendInitiatePaste(ClipboardFormatId(format_id)),
            )) {
                error!(
                    "Failed to send SendInitiatePaste for format {}: {:?}",
                    format_id, e
                );
            }
        } else {
            warn!(
                "ServerEvent sender not available — cannot request format {} from RDP client",
                format_id
            );
        }

        Ok(())
    }

    /// Handle RDP file contents request (Linux → Windows file transfer)
    async fn handle_rdp_file_contents_request(
        stream_id: u32,
        list_index: u32,
        position: u64,
        requested_size: u32,
        is_size_request: bool,
        server_event_sender: &ServerEventSender,
        file_transfer_state: &Arc<RwLock<FileTransferState>>,
    ) -> Result<()> {
        info!(
            "FileContentsRequest: stream={}, index={}, pos={}, size={}, size_req={}",
            stream_id, list_index, position, requested_size, is_size_request
        );

        let sender = match server_event_sender.read().await.as_ref() {
            Some(s) => s.clone(),
            None => {
                error!("ServerEvent sender not available for file transfer");
                return Err(ClipboardError::NotInitialized);
            }
        };

        let state = file_transfer_state.read().await;
        let file_info = state
            .outgoing_files
            .get(list_index as usize)
            .ok_or_else(|| {
                error!(
                    "Invalid file list index: {} (have {} files)",
                    list_index,
                    state.outgoing_files.len()
                );
                ClipboardError::InvalidState(format!("File index {list_index} not found"))
            })?;

        use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::FileContentsResponse};

        if is_size_request {
            info!("Returning clipboard file size: {} bytes", file_info.size);

            let response = FileContentsResponse::new_size_response(stream_id, file_info.size);
            info!(
                "Sending FileContentsResponse(stream={}, size={})",
                stream_id, file_info.size
            );

            if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendFileContentsResponse(response),
            )) {
                error!("Failed to send FileContentsResponse: {:?}", e);
            }
        } else {
            let path = file_info.path.clone();
            let file_size = file_info.size;
            drop(state); // Release lock before file I/O

            match Self::read_file_chunk(&path, position, requested_size) {
                Ok(data) => {
                    info!(
                        "Read {} clipboard file bytes at offset {} (file size: {})",
                        data.len(),
                        position,
                        file_size
                    );

                    let response = FileContentsResponse::new_data_response(stream_id, data.clone());
                    info!(
                        "Sending FileContentsResponse(stream={}, {} bytes)",
                        stream_id,
                        data.len()
                    );

                    if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                        ClipboardMessage::SendFileContentsResponse(response),
                    )) {
                        error!("Failed to send FileContentsResponse: {:?}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to read clipboard file: {e}");

                    let response = FileContentsResponse::new_error(stream_id);
                    info!("Sending FileContentsResponse ERROR (stream={})", stream_id);

                    if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                        ClipboardMessage::SendFileContentsResponse(response),
                    )) {
                        error!("Failed to send FileContentsResponse error: {:?}", e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Read a chunk from a file
    fn read_file_chunk(path: &PathBuf, offset: u64, size: u32) -> Result<Vec<u8>> {
        let mut file = File::open(path)
            .map_err(|e| ClipboardError::FileIoError(format!("Failed to open file: {e}")))?;

        file.seek(SeekFrom::Start(offset)).map_err(|e| {
            ClipboardError::FileIoError(format!("Failed to seek to offset {offset}: {e}"))
        })?;

        // Peer-supplied range lengths must not drive multi-gigabyte allocations.
        // Short responses are valid; clients request the remaining range later.
        let remaining = file
            .metadata()
            .map_err(|e| ClipboardError::FileIoError(format!("Failed to stat file: {e}")))?
            .len()
            .saturating_sub(offset);
        let bounded_size = u64::from(size).min(64 * 1024 * 1024).min(remaining);
        let mut buffer = vec![0u8; bounded_size as usize];
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|e| ClipboardError::FileIoError(format!("Failed to read file: {e}")))?;

        buffer.truncate(bytes_read);
        Ok(buffer)
    }

    /// Handle RDP file contents response - Linux receives file from Windows
    ///
    /// Called when Windows client provides file data chunks.
    /// For files >64MB, requests continuation chunks until complete.
    /// When all files are complete, delivers file:// URIs to Portal.
    async fn handle_rdp_file_contents_response(
        stream_id: u32,
        data: Vec<u8>,
        is_error: bool,
        file_transfer_state: &Arc<RwLock<FileTransferState>>,
        clipboard_provider: &SharedClipboardProvider,
        server_event_sender: &ServerEventSender,
    ) -> Result<()> {
        if is_error {
            warn!("FileContentsResponse ERROR: stream={}", stream_id);

            let mut state = file_transfer_state.write().await;
            if let Some(file) = state.incoming_files.remove(&stream_id) {
                info!("Cleaning up failed clipboard file transfer");
                let _ = std::fs::remove_file(&file.temp_path);
            }

            if let Some(serial) = state.portal_serial.take() {
                drop(state);
                if let Some(ref provider) = *clipboard_provider.read().await {
                    let _ = provider.complete_transfer(serial, "", vec![], false).await;
                }
            }

            return Ok(());
        }

        info!(
            "FileContentsResponse [v2]: stream={}, {} bytes",
            stream_id,
            data.len()
        );

        let mut state = file_transfer_state.write().await;

        let file = match state.incoming_files.get_mut(&stream_id) {
            Some(f) => f,
            None => {
                warn!(
                    "Received FileContentsResponse for unknown stream {}",
                    stream_id
                );
                return Ok(());
            }
        };

        if file.awaiting_size {
            let total_size = file.apply_size_response(&data)?;
            if total_size > 0 {
                let request_size = total_size.min(64 * 1024 * 1024) as u32;
                let file_index = file.file_index;
                let clip_data_id = file.clip_data_id;
                drop(state);
                if let Some(sender) = server_event_sender.read().await.as_ref() {
                    use ironrdp_cliprdr::{
                        backend::ClipboardMessage,
                        pdu::{FileContentsFlags, FileContentsRequest},
                    };
                    sender
                        .send(ironrdp_server::ServerEvent::Clipboard(
                            ClipboardMessage::SendFileContentsRequest(FileContentsRequest {
                                stream_id,
                                index: file_index.try_into().unwrap_or(i32::MAX),
                                flags: FileContentsFlags::RANGE,
                                position: 0,
                                requested_size: request_size,
                                data_id: Some(clip_data_id),
                            }),
                        ))
                        .map_err(|error| {
                            ClipboardError::InvalidState(format!(
                                "failed to request clipboard file data: {error}"
                            ))
                        })?;
                }
                return Ok(());
            }
        } else {
            file.append_data(&data)?;
        }

        let total_size = file.total_size.unwrap_or(file.received_size);
        let percent = if total_size > 0 {
            (file.received_size as f64 / total_size as f64) * 100.0
        } else {
            100.0
        };
        info!(
            "Clipboard file progress: {}/{} bytes ({:.1}%)",
            file.received_size, total_size, percent
        );

        let file_complete = file
            .total_size
            .is_some_and(|size| file.received_size >= size);

        if file_complete {
            debug!("Clipboard file transfer complete");

            file.file_handle
                .flush()
                .map_err(|e| ClipboardError::FileIoError(format!("Failed to flush file: {e}")))?;

            let temp_path = file.temp_path.clone();
            let final_path = file.final_path.clone();
            state.completed_files.push(final_path.clone());
            state.incoming_files.remove(&stream_id);

            let all_complete = state.incoming_files.is_empty();
            let portal_serial = state.portal_serial;
            let completed_files = state.completed_files.clone();
            drop(state); // Release lock before file operation

            finalize_without_replace(&temp_path, &final_path).map_err(|e| {
                error!("Failed to finalize clipboard file: {e}");
                ClipboardError::FileIoError(format!("Failed to finalize file: {e}"))
            })?;

            info!("Saved incoming clipboard file");

            if all_complete {
                debug!(
                    "All {} file(s) transferred successfully",
                    completed_files.len()
                );

                // Only encode characters problematic in URIs, NOT dots/dashes/underscores
                use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
                const FILE_URI_ENCODE: &AsciiSet = &CONTROLS
                    .add(b' ')
                    .add(b'"')
                    .add(b'#')
                    .add(b'%')
                    .add(b'<')
                    .add(b'>')
                    .add(b'?')
                    .add(b'`')
                    .add(b'{')
                    .add(b'}');
                let uris: Vec<String> = completed_files
                    .iter()
                    .map(|path| {
                        let path_str = path.to_string_lossy();
                        let encoded: String = path_str
                            .split('/')
                            .map(|component| {
                                utf8_percent_encode(component, FILE_URI_ENCODE).to_string()
                            })
                            .collect::<Vec<_>>()
                            .join("/");
                        format!("file://{encoded}")
                    })
                    .collect();

                // x-special/gnome-copied-files format: "copy\nfile:///path1\nfile:///path2\0" (null-terminated)
                let uri_list = format!("copy\n{}\0", uris.join("\n"));

                debug!(
                    "Generated URI list (gnome-copied-files format): {:?}",
                    uri_list
                );

                if let Some(serial) = portal_serial {
                    let uri_bytes = uri_list.into_bytes();

                    match *clipboard_provider.read().await {
                        Some(ref provider) => {
                            match provider
                                .complete_transfer(
                                    serial,
                                    "x-special/gnome-copied-files",
                                    uri_bytes,
                                    true,
                                )
                                .await
                            {
                                Ok(()) => {
                                    info!(
                                        "Delivered {} file URI(s) via {} provider (serial={})",
                                        completed_files.len(),
                                        provider.name(),
                                        serial
                                    );
                                }
                                Err(e) => {
                                    error!("Failed to deliver URIs via provider: {:?}", e);
                                }
                            }
                        }
                        _ => {
                            warn!("No clipboard provider available to deliver file URIs");
                        }
                    }
                }

                let mut state = file_transfer_state.write().await;
                state.completed_files.clear();
                state.portal_serial = None;
            }
        } else if let Some(total_size) = file.total_size {
            // File is NOT complete - need to request the next chunk
            let remaining = total_size - file.received_size;
            let next_chunk_size = remaining.min(64 * 1024 * 1024) as u32; // Max 64MB per request
            let position = file.received_size;
            let file_index = file.file_index;
            let clip_data_id = file.clip_data_id;
            drop(state); // Release lock before sending

            if let Some(sender) = server_event_sender.read().await.as_ref() {
                use ironrdp_cliprdr::{
                    backend::ClipboardMessage,
                    pdu::{FileContentsFlags, FileContentsRequest},
                };

                info!(
                    "Requesting next clipboard file chunk (pos={}, size={}, remaining={})",
                    position, next_chunk_size, remaining
                );

                if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                    ClipboardMessage::SendFileContentsRequest(FileContentsRequest {
                        stream_id,
                        index: file_index.try_into().unwrap_or(i32::MAX),
                        flags: FileContentsFlags::RANGE,
                        position,
                        requested_size: next_chunk_size,
                        data_id: Some(clip_data_id),
                    }),
                )) {
                    error!("Failed to send continuation FileContentsRequest: {:?}", e);
                }
            } else {
                error!("ServerEvent sender not available for chunk continuation");
            }
        }

        Ok(())
    }

    /// Clear connection-scoped clipboard ownership and transfer state.
    ///
    /// The provider and CLIPRDR event loop remain available for the next RDP
    /// connection; full task shutdown is reserved for [`Self::shutdown`].
    pub async fn clear_connection_state(&mut self) {
        if let Some(provider) = self.clipboard_provider.read().await.as_ref()
            && let Err(error) = provider.announce_formats(Vec::new()).await
        {
            warn!("Failed to clear clipboard selection on disconnect: {error}");
        }
        let mut state = self.file_transfer_state.write().await;
        state.clear_incoming();
        state.clear_outgoing();
        state.pending_descriptors.clear();
        state.portal_serial = None;
    }

    /// Shutdown the clipboard manager
    ///
    /// Sends a shutdown signal to the event loop if it's running.
    /// If the event loop hasn't been started, this is a no-op.
    /// Clear local clipboard ownership and stop clipboard tasks.
    ///
    /// Provider cleanup is best-effort so a failed compositor call does not
    /// prevent the RDP connection from shutting down.
    pub async fn shutdown(&mut self) -> Result<()> {
        info!("Clipboard orchestrator shutdown starting");

        if let Some(provider) = self.clipboard_provider.read().await.as_ref() {
            if let Err(error) = provider.announce_formats(Vec::new()).await {
                warn!("Failed to clear clipboard selection during shutdown: {error}");
            }
            provider.shutdown().await;
            info!("Clipboard provider shut down");
        }
        self.file_transfer_state.write().await.clear_incoming();

        if let Some(ref tx) = self.shutdown_tx
            && let Err(e) = tx.send(()).await
        {
            warn!("Failed to send shutdown signal to event processor: {}", e);
        }

        let _ = self.shutdown_broadcast.send(());

        let task_count = {
            let handles = self.task_handles.lock().await;
            handles.len()
        };

        if task_count > 0 {
            let timeout = tokio::time::Duration::from_secs(5);
            let mut handles = self.task_handles.lock().await;

            for handle in handles.drain(..) {
                finish_clipboard_task(handle, timeout).await;
            }
        }

        self.shutdown_tx = None;

        info!("Clipboard orchestrator shutdown complete");

        Ok(())
    }
}

async fn finish_clipboard_task(mut handle: tokio::task::JoinHandle<()>, grace: Duration) {
    match tokio::time::timeout(grace, &mut handle).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!("Clipboard task stopped with error: {error}"),
        Err(_) => {
            warn!("Clipboard task exceeded shutdown grace; aborting");
            handle.abort();
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_timeout_cancels_task_and_releases_resources() {
        struct NotifyDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for NotifyDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let (started, ready) = tokio::sync::oneshot::channel();
        let (released, mut done) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _resource = NotifyDrop(Some(released));
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        finish_clipboard_task(handle, Duration::ZERO).await;
        assert_eq!(done.try_recv(), Ok(()));
    }

    #[tokio::test]
    async fn completed_clipboard_task_shuts_down_cleanly() {
        finish_clipboard_task(tokio::spawn(async {}), Duration::from_secs(1)).await;
    }

    #[test]
    fn incoming_destinations_do_not_overwrite_or_collide() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("report.txt");
        std::fs::write(&existing, b"keep").unwrap();
        let mut state = FileTransferState::new(temp.path().to_path_buf());
        let first = state.reserve_destination("report.txt");
        assert_eq!(first.file_name().unwrap(), "report (1).txt");
        let temp_path = temp.path().join(".incoming.tmp");
        let file_handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .unwrap();
        state.incoming_files.insert(
            1,
            IncomingFile {
                final_path: first,
                total_size: Some(1),
                awaiting_size: false,
                received_size: 0,
                temp_path,
                file_handle,
                file_index: 0,
                clip_data_id: 1,
            },
        );
        let second = state.reserve_destination("report.txt");
        assert_eq!(second.file_name().unwrap(), "report (2).txt");
    }

    #[test]
    fn size_response_handles_empty_files_and_rejects_malformed_data() {
        let temp = tempfile::tempdir().unwrap();
        let temp_path = temp.path().join(".incoming.tmp");
        let file_handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .unwrap();
        let mut file = IncomingFile {
            final_path: temp.path().join("empty.txt"),
            total_size: None,
            awaiting_size: true,
            received_size: 0,
            temp_path,
            file_handle,
            file_index: 0,
            clip_data_id: 1,
        };
        assert!(file.apply_size_response(&[0; 7]).is_err());
        assert_eq!(file.apply_size_response(&0u64.to_le_bytes()).unwrap(), 0);
        assert_eq!(file.total_size, Some(0));
        assert!(!file.awaiting_size);
    }

    #[test]
    fn outgoing_chunk_is_bounded_by_remaining_file_size() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("small.txt");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            ClipboardOrchestrator::read_file_chunk(&path, 1, u32::MAX).unwrap(),
            b"bc"
        );
        assert!(
            ClipboardOrchestrator::read_file_chunk(&path, 100, u32::MAX)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn oversized_clipboard_data_is_rejected_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("incoming.tmp");
        let mut file = IncomingFile {
            final_path: temp.path().join("final.txt"),
            total_size: Some(3),
            awaiting_size: false,
            received_size: 0,
            temp_path: path.clone(),
            file_handle: OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap(),
            file_index: 0,
            clip_data_id: 1,
        };
        assert!(file.append_data(&[]).is_err());
        assert_eq!(file.received_size, 0);
        file.append_data(&[1, 2]).unwrap();
        assert!(file.append_data(&[3, 4]).is_err());
        assert_eq!(file.received_size, 2);
        assert_eq!(std::fs::read(&path).unwrap(), vec![1, 2]);
        file.append_data(&[3]).unwrap();
        assert_eq!(file.received_size, 3);
    }

    #[test]
    fn clear_incoming_removes_temporary_files() {
        let temp = tempfile::tempdir().unwrap();
        let temp_path = temp.path().join(".incoming.tmp");
        let file_handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .unwrap();
        let mut state = FileTransferState::new(temp.path().to_path_buf());
        state.incoming_files.insert(
            1,
            IncomingFile {
                final_path: temp.path().join("final.txt"),
                total_size: Some(1),
                awaiting_size: false,
                received_size: 0,
                temp_path: temp_path.clone(),
                file_handle,
                file_index: 0,
                clip_data_id: 1,
            },
        );
        state.clear_incoming();
        assert!(!temp_path.exists());
    }

    #[test]
    fn finalize_does_not_replace_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let incoming = temp.path().join("incoming.tmp");
        let existing = temp.path().join("existing.txt");
        std::fs::write(&incoming, b"new").unwrap();
        std::fs::write(&existing, b"old").unwrap();
        assert!(finalize_without_replace(&incoming, &existing).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"old");
        assert_eq!(std::fs::read(&incoming).unwrap(), b"new");
    }

    #[tokio::test]
    async fn test_clipboard_manager_creation() {
        let config = ClipboardOrchestratorConfig::default();
        let manager = ClipboardOrchestrator::new(config).await.unwrap();

        assert!(manager.event_tx.capacity() > 0);
    }

    #[tokio::test]
    async fn test_rdp_format_list_handling() {
        let config = ClipboardOrchestratorConfig::default();
        let manager = ClipboardOrchestrator::new(config).await.unwrap();

        let formats = vec![ClipboardFormat::with_name(13, "CF_UNICODETEXT")];
        let event = ClipboardEvent::RdpFormatList(formats);
        manager.event_tx.send(event).await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn test_shutdown() {
        let config = ClipboardOrchestratorConfig::default();
        let mut manager = ClipboardOrchestrator::new(config).await.unwrap();
        manager.shutdown().await.unwrap();
    }
}
