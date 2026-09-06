//! ext-data-control-v1 clipboard backend.
//!
//! Cross-thread architecture: this backend runs in the tokio async context
//! but the actual Wayland protocol objects (`ext_data_control_manager_v1`)
//! live on the dedicated Wayland event loop thread. Communication happens via:
//!
//! - **Commands out**: `ClipboardCommand` sent over an `mpsc::Sender` to the
//!   event loop for operations like `SetSelection` and `ReceiveFromOffer`.
//! - **State in**: `SharedClipboardState` (behind `Arc<Mutex<>>`) is updated
//!   by the event loop when the compositor's selection changes.
//!
//! The local cache (`local_data`, `local_mime_types`) allows immediate
//! read-back of data we set ourselves, without a roundtrip to the compositor.

use std::{
    collections::HashMap,
    io::Read,
    os::unix::io::{AsRawFd, OwnedFd},
    sync::{Arc, Mutex, mpsc},
};

use super::{ClipboardBackend, ClipboardProtocol};
use crate::desktop::portal::xdg_desktop::{
    error::{PortalError, Result},
    types::ClipboardData,
    wayland::{ClipboardCommand, SharedClipboardState},
};

/// ext-data-control clipboard backend.
///
/// Uses `ext_data_control_manager_v1` to access the compositor's clipboard.
/// The actual Wayland protocol objects live on the event loop thread — this
/// backend sends commands via a channel and reads shared state for the
/// current selection.
pub struct ExtClipboardBackend {
    /// Command sender to the Wayland event loop thread.
    clipboard_tx: mpsc::Sender<ClipboardCommand>,
    /// Shared clipboard state (updated by event loop on selection changes).
    shared_clipboard: Arc<Mutex<SharedClipboardState>>,
    /// Locally cached data for our own SetSelection (for immediate read-back).
    local_data: HashMap<String, Vec<u8>>,
    /// MIME types from our last SetSelection (for immediate read-back).
    local_mime_types: Vec<String>,
}

impl ExtClipboardBackend {
    /// Create a new ext clipboard backend.
    pub fn new(
        clipboard_tx: mpsc::Sender<ClipboardCommand>,
        shared_clipboard: Arc<Mutex<SharedClipboardState>>,
    ) -> Self {
        Self {
            clipboard_tx,
            shared_clipboard,
            local_data: HashMap::new(),
            local_mime_types: Vec::new(),
        }
    }
}

impl ClipboardBackend for ExtClipboardBackend {
    fn protocol_type(&self) -> ClipboardProtocol {
        ClipboardProtocol::ExtDataControl
    }

    fn get_clipboard(&self) -> Result<ClipboardData> {
        let shared = self.shared_clipboard.lock().map_err(|_| {
            PortalError::Wayland("Failed to lock shared clipboard state".to_string())
        })?;

        Ok(ClipboardData {
            mime_types: shared.mime_types.clone(),
            data: HashMap::new(), // Data is fetched on-demand via read_selection
        })
    }

    fn set_clipboard(&mut self, data: ClipboardData) -> Result<()> {
        tracing::debug!(
            mime_types = ?data.mime_types,
            protocol = "ext-data-control",
            "Setting clipboard"
        );

        // Cache locally for immediate read-back
        self.local_mime_types.clone_from(&data.mime_types);
        self.local_data.clone_from(&data.data);

        // Send command to event loop to create data control source
        self.clipboard_tx
            .send(ClipboardCommand::SetSelection {
                mime_types: data.mime_types,
                data: data.data,
            })
            .map_err(|e| {
                PortalError::Wayland(format!("Failed to send SetSelection command: {e}"))
            })?;

        Ok(())
    }

    fn on_selection_changed(&mut self, callback: Box<dyn Fn(Vec<String>) + Send + Sync>) {
        if let Ok(mut shared) = self.shared_clipboard.lock() {
            shared.on_change = Some(Arc::from(callback));
        }
    }

    fn read_selection(&self, mime_type: &str) -> Result<Option<Vec<u8>>> {
        // Check if we have locally cached data (from our own SetSelection)
        if let Some(data) = self.local_data.get(mime_type) {
            return Ok(Some(data.clone()));
        }

        // Find a matching MIME type from the compositor, tolerating charset differences
        // (e.g., "text/plain;charset=utf-8" vs "text/plain")
        let actual_mime = {
            let shared = self.shared_clipboard.lock().map_err(|_| {
                PortalError::Wayland("Failed to lock shared clipboard state".to_string())
            })?;
            super::find_mime_match(mime_type, &shared.mime_types).map(str::to_string)
        };

        let Some(actual_mime) = actual_mime else {
            return Ok(None);
        };

        // Create a pipe and request data from the compositor
        let (read_fd, write_fd) = create_pipe()?;

        self.clipboard_tx
            .send(ClipboardCommand::ReceiveFromOffer {
                mime_type: actual_mime,
                fd: write_fd,
            })
            .map_err(|e| {
                PortalError::Wayland(format!("Failed to send ReceiveFromOffer command: {e}"))
            })?;

        // Read data from the pipe
        read_pipe_data(read_fd)
    }

    fn update_source_data(&mut self, mime_type: &str, data: Vec<u8>) -> Result<()> {
        self.clipboard_tx
            .send(ClipboardCommand::UpdateSourceData {
                mime_type: mime_type.to_string(),
                data,
            })
            .map_err(|e| {
                PortalError::Wayland(format!("Failed to send UpdateSourceData command: {e}"))
            })?;
        Ok(())
    }

    fn write_done(&mut self, serial: u32, success: bool) -> Result<()> {
        tracing::debug!(
            serial = serial,
            success = success,
            protocol = "ext-data-control",
            "Selection write done"
        );
        Ok(())
    }
}

/// Create a Unix pipe, returning (read_fd, write_fd).
fn create_pipe() -> Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe()
        .map_err(|e| PortalError::FdPassingFailed(format!("Failed to create pipe: {e}")))
}

/// Read all data from a pipe fd with a timeout.
///
/// Uses `poll()` to avoid blocking indefinitely if the Wayland event loop
/// thread is slow to write data. This is safe to call from a tokio
/// `spawn_blocking` context or from synchronous code.
#[expect(
    unsafe_code,
    reason = "poll() requires unsafe libc call and File::from_raw_fd requires unsafe FFI"
)]
fn read_pipe_data(read_fd: OwnedFd) -> Result<Option<Vec<u8>>> {
    // Set non-blocking so reads don't hang
    let flags = nix::fcntl::fcntl(&read_fd, nix::fcntl::FcntlArg::F_GETFL)
        .map_err(|e| PortalError::Io(std::io::Error::from(e)))?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags.insert(nix::fcntl::OFlag::O_NONBLOCK);
    nix::fcntl::fcntl(&read_fd, nix::fcntl::FcntlArg::F_SETFL(oflags))
        .map_err(|e| PortalError::Io(std::io::Error::from(e)))?;

    let mut file = std::fs::File::from(read_fd);
    let poll_fd = file.as_raw_fd();

    let mut data = Vec::new();
    const MAX_SIZE: usize = 100 * 1024 * 1024; // 100MB limit
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        // Wait for data with timeout using poll()
        let mut pollfd = libc::pollfd {
            fd: poll_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(PortalError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "clipboard pipe deadline exceeded",
            )));
        }
        let timeout_ms = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        let poll_result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };

        if poll_result == 0 {
            return Err(PortalError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "clipboard pipe did not reach EOF before its deadline",
            )));
        } else if poll_result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(PortalError::Io(err));
        }

        let mut chunk = vec![0u8; 4096];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if data.len() + n > MAX_SIZE {
                    return Err(PortalError::ClipboardDataTooLarge(data.len() + n, MAX_SIZE));
                }
                data.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(PortalError::Io(e)),
        }
    }

    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_backend() -> ExtClipboardBackend {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        ExtClipboardBackend::new(tx, shared)
    }

    #[test]
    fn pipe_eof_preserves_complete_data() {
        use std::io::Write;
        let (read, write) = create_pipe().unwrap();
        let mut writer = std::fs::File::from(write);
        writer.write_all(b"clipboard").unwrap();
        drop(writer);
        assert_eq!(read_pipe_data(read).unwrap(), Some(b"clipboard".to_vec()));
    }

    #[test]
    fn pipe_timeout_rejects_partial_data() {
        use std::io::Write;
        let (read, write) = create_pipe().unwrap();
        let mut writer = std::fs::File::from(write);
        writer.write_all(b"partial").unwrap();
        let error = read_pipe_data(read).unwrap_err();
        assert!(
            matches!(error, PortalError::Io(ref e) if e.kind() == std::io::ErrorKind::TimedOut)
        );
        drop(writer);
    }

    #[test]
    fn test_protocol_type() {
        let backend = make_test_backend();
        assert_eq!(backend.protocol_type(), ClipboardProtocol::ExtDataControl);
    }

    #[test]
    fn test_get_clipboard_empty() {
        let backend = make_test_backend();
        let data = backend.get_clipboard().unwrap();
        assert!(data.mime_types.is_empty());
    }

    #[test]
    fn test_set_clipboard_caches_locally() {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let mut backend = ExtClipboardBackend::new(tx, shared);

        let data = ClipboardData {
            mime_types: vec!["text/plain".to_string()],
            data: HashMap::from([("text/plain".to_string(), b"Hello".to_vec())]),
        };

        backend.set_clipboard(data).unwrap();

        // Should be readable from local cache
        let result = backend.read_selection("text/plain").unwrap();
        assert_eq!(result.unwrap(), b"Hello");

        // Should have sent command
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, ClipboardCommand::SetSelection { .. }));
    }

    #[test]
    fn test_read_selection_unknown_mime() {
        let backend = make_test_backend();
        let result = backend.read_selection("image/png").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_write_done() {
        let mut backend = make_test_backend();
        assert!(backend.write_done(1, true).is_ok());
        assert!(backend.write_done(2, false).is_ok());
    }

    #[test]
    fn test_on_selection_changed() {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let mut backend = ExtClipboardBackend::new(tx, Arc::clone(&shared));

        let called = Arc::new(Mutex::new(false));
        let called_clone = Arc::clone(&called);
        backend.on_selection_changed(Box::new(move |_types| {
            *called_clone.lock().unwrap() = true;
        }));

        // Verify the callback is registered
        let shared = shared.lock().unwrap();
        assert!(shared.on_change.is_some());
    }
}
