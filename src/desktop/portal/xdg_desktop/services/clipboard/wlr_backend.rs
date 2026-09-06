//! wlr-data-control-v1 clipboard backend.
//!
//! Uses the wlroots `zwlr_data_control_manager_v1` protocol for clipboard
//! access. This is the fallback when ext-data-control is not available.
//! Communicates with the Wayland event loop thread via a command channel.

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

/// wlr-data-control clipboard backend.
///
/// Uses `zwlr_data_control_manager_v1` to access the compositor's clipboard.
/// Nearly identical semantics to ext-data-control. The actual protocol
/// objects live on the event loop thread.
pub struct WlrClipboardBackend {
    /// Command sender to the Wayland event loop thread.
    clipboard_tx: mpsc::Sender<ClipboardCommand>,
    /// Shared clipboard state (updated by event loop on selection changes).
    shared_clipboard: Arc<Mutex<SharedClipboardState>>,
    /// Locally cached data for our own SetSelection (for immediate read-back).
    local_data: HashMap<String, Vec<u8>>,
    /// MIME types from our last SetSelection (for immediate read-back).
    local_mime_types: Vec<String>,
}

impl WlrClipboardBackend {
    /// Create a new wlr clipboard backend.
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

impl ClipboardBackend for WlrClipboardBackend {
    fn protocol_type(&self) -> ClipboardProtocol {
        ClipboardProtocol::WlrDataControl
    }

    fn get_clipboard(&self) -> Result<ClipboardData> {
        let shared = self.shared_clipboard.lock().map_err(|_| {
            PortalError::Wayland("Failed to lock shared clipboard state".to_string())
        })?;

        Ok(ClipboardData {
            mime_types: shared.mime_types.clone(),
            data: HashMap::new(),
        })
    }

    fn set_clipboard(&mut self, data: ClipboardData) -> Result<()> {
        tracing::debug!(
            mime_types = ?data.mime_types,
            protocol = "wlr-data-control",
            "Setting clipboard"
        );

        self.local_mime_types.clone_from(&data.mime_types);
        self.local_data.clone_from(&data.data);

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
        // Check local cache first
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

        // Create pipe and request from compositor
        let (read_fd, write_fd) = create_pipe()?;

        self.clipboard_tx
            .send(ClipboardCommand::ReceiveFromOffer {
                mime_type: actual_mime,
                fd: write_fd,
            })
            .map_err(|e| {
                PortalError::Wayland(format!("Failed to send ReceiveFromOffer command: {e}"))
            })?;

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
            protocol = "wlr-data-control",
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
/// thread is slow to write data.
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
        let poll_result = unsafe { libc::poll(&raw mut pollfd, 1, timeout_ms) };

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

    fn make_test_backend() -> WlrClipboardBackend {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        WlrClipboardBackend::new(tx, shared)
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
        assert_eq!(backend.protocol_type(), ClipboardProtocol::WlrDataControl);
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
        let mut backend = WlrClipboardBackend::new(tx, shared);

        let data = ClipboardData {
            mime_types: vec!["text/html".to_string()],
            data: HashMap::from([("text/html".to_string(), b"<b>Hi</b>".to_vec())]),
        };

        backend.set_clipboard(data).unwrap();

        let result = backend.read_selection("text/html").unwrap();
        assert_eq!(result.unwrap(), b"<b>Hi</b>");

        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, ClipboardCommand::SetSelection { .. }));
    }

    #[test]
    fn test_write_done() {
        let mut backend = make_test_backend();
        assert!(backend.write_done(1, true).is_ok());
        assert!(backend.write_done(2, false).is_ok());
    }
}
