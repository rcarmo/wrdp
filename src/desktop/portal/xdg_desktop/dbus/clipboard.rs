//! Clipboard D-Bus interface implementation.
//!
//! Implements the runtime clipboard portal interface boundary.

use std::{
    collections::HashMap,
    os::unix::io::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use tokio::sync::Mutex;
use zbus::{
    interface,
    object_server::SignalEmitter,
    zvariant::{Fd, ObjectPath, OwnedValue},
};

use crate::desktop::portal::xdg_desktop::{
    error::PortalError, services::clipboard::ClipboardBackend, session::SessionManager,
    types::ClipboardData,
};

/// Serial counter for clipboard transfers.
///
/// Each transfer gets a unique serial so write completion can be correlated
/// with the initiating transfer event.
static CLIPBOARD_SERIAL: AtomicU32 = AtomicU32::new(1);

/// Signals emitted from Wayland handling into D-Bus.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClipboardSignal {
    /// The compositor's selection owner changed.
    SelectionOwnerChanged {
        /// MIME types offered by the new selection.
        mime_types: Vec<String>,
    },
    /// The compositor (or another client) is requesting clipboard data
    /// in a specific MIME type. Emitted from the Wayland `send` event
    /// on our data source.
    SelectionTransfer {
        /// MIME type requested.
        mime_type: String,
        /// Unique serial for this transfer (client uses in `SelectionWrite`).
        serial: u32,
    },
}

/// Pending clipboard write data, keyed by serial.
///
/// Stores write payloads until `SelectionWriteDone` finalizes transfer handling.
pub type PendingWrites = Arc<std::sync::Mutex<HashMap<u32, PendingWriteEntry>>>;

/// Entry in the pending writes map.
#[derive(Debug, Clone)]
pub struct PendingWriteEntry {
    /// MIME type for this transfer.
    pub mime_type: String,
    /// Data written by the client (populated after `SelectionWriteDone`).
    pub data: Option<Vec<u8>>,
}

/// Clipboard portal interface implementation.
pub struct ClipboardInterface {
    /// Session manager.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Clipboard backend for clipboard operations.
    clipboard_backend: Option<Arc<Mutex<Box<dyn ClipboardBackend>>>>,
    /// Pending clipboard writes keyed by serial.
    pending_writes: PendingWrites,
}

impl ClipboardInterface {
    /// Create a new Clipboard interface with a clipboard backend.
    pub fn new(
        session_manager: Arc<Mutex<SessionManager>>,
        clipboard_backend: Option<Arc<Mutex<Box<dyn ClipboardBackend>>>>,
    ) -> Self {
        Self {
            session_manager,
            clipboard_backend,
            pending_writes: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Get the pending writes map (for sharing with the signal bridge task).
    #[must_use]
    pub fn pending_writes(&self) -> PendingWrites {
        Arc::clone(&self.pending_writes)
    }

    /// Emit a `SelectionOwnerChanged` D-Bus signal.
    ///
    /// This is a public wrapper around the zbus-generated signal method,
    /// allowing the clipboard signal bridge to emit signals from outside
    /// the interface implementation.
    ///
    /// # Errors
    ///
    /// Returns a `zbus::Error` if the D-Bus signal emission fails.
    pub async fn emit_selection_owner_changed(
        signal_emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()> {
        Self::selection_owner_changed(signal_emitter, session_handle, options).await
    }

    /// Emit a `SelectionTransfer` D-Bus signal.
    ///
    /// Public wrapper for the zbus-generated signal method.
    ///
    /// # Errors
    ///
    /// Returns a `zbus::Error` if the D-Bus signal emission fails.
    pub async fn emit_selection_transfer(
        signal_emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()> {
        Self::selection_transfer(signal_emitter, session_handle, mime_type, serial).await
    }

    /// Get MIME types from options.
    fn get_mime_types(options: &HashMap<String, OwnedValue>) -> Vec<String> {
        options
            .get("mime_types")
            .and_then(|v| {
                use zbus::zvariant::Value;
                let value: &Value<'_> = v.downcast_ref().ok()?;
                if let Value::Array(arr) = value {
                    let strings: Vec<String> = arr
                        .iter()
                        .filter_map(|v| {
                            if let Value::Str(s) = v {
                                Some(s.to_string())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !strings.is_empty() {
                        return Some(strings);
                    }
                }
                None
            })
            .unwrap_or_default()
    }
}

#[interface(name = "org.freedesktop.impl.portal.Clipboard")]
impl ClipboardInterface {
    /// Request clipboard access for a session.
    #[zbus(name = "RequestClipboard")]
    async fn request_clipboard(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        tracing::debug!(
            session_handle = %session_handle,
            "RequestClipboard called"
        );

        let mut manager = self.session_manager.lock().await;
        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        session.request_clipboard()?;

        tracing::info!("Clipboard enabled for session");

        Ok(())
    }

    /// Set the clipboard selection.
    #[zbus(name = "SetSelection")]
    async fn set_selection(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        tracing::debug!(
            session_handle = %session_handle,
            "SetSelection called"
        );

        let mime_types = Self::get_mime_types(&options);

        if mime_types.is_empty() {
            tracing::warn!("SetSelection called with no MIME types");
        }

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.clipboard_enabled {
            return Err(PortalError::ClipboardNotEnabled.into());
        }

        drop(manager);

        // Set clipboard via backend
        if let Some(ref backend) = self.clipboard_backend {
            let mut backend = backend.lock().await;
            backend
                .set_clipboard(ClipboardData {
                    mime_types: mime_types.clone(),
                    data: HashMap::new(),
                })
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        }

        tracing::debug!(
            mime_types = ?mime_types,
            "Selection set"
        );

        Ok(())
    }

    /// Get a file descriptor to write clipboard data.
    #[zbus(name = "SelectionWrite")]
    async fn selection_write(
        &self,
        session_handle: ObjectPath<'_>,
        serial: u32,
    ) -> zbus::fdo::Result<Fd<'static>> {
        tracing::debug!(
            session_handle = %session_handle,
            serial = serial,
            "SelectionWrite called"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.clipboard_enabled {
            return Err(PortalError::ClipboardNotEnabled.into());
        }

        let (read_fd, write_fd) = create_pipe()?;

        let pending_writes = Arc::clone(&self.pending_writes);
        tokio::spawn(async move {
            if let Err(e) = handle_selection_write(read_fd, serial, &pending_writes) {
                tracing::error!(
                    serial = serial,
                    error = %e,
                    "Failed to handle selection write"
                );
            }
        });

        Ok(Fd::from(write_fd))
    }

    /// Get a file descriptor to read clipboard data.
    #[zbus(name = "SelectionRead")]
    async fn selection_read(
        &self,
        session_handle: ObjectPath<'_>,
        mime_type: &str,
    ) -> zbus::fdo::Result<Fd<'static>> {
        tracing::debug!(
            session_handle = %session_handle,
            mime_type = %mime_type,
            "SelectionRead called"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.clipboard_enabled {
            return Err(PortalError::ClipboardNotEnabled.into());
        }

        drop(manager);

        // Read from clipboard backend
        let data = if let Some(ref backend) = self.clipboard_backend {
            let backend = backend.lock().await;
            let clipboard = backend
                .get_clipboard()
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

            if !clipboard.mime_types.contains(&mime_type.to_string()) {
                return Err(PortalError::UnsupportedMimeType(mime_type.to_string()).into());
            }

            clipboard.data.get(mime_type).cloned()
        } else {
            return Err(PortalError::ClipboardNotEnabled.into());
        };

        let (read_fd, write_fd) = create_pipe()?;

        if let Some(data) = data {
            tokio::spawn(async move {
                use std::io::Write;
                let mut file = owned_fd_to_file(write_fd);
                let _ = file.write_all(&data);
            });
        }

        Ok(Fd::from(read_fd))
    }

    /// Notify that a clipboard write operation has completed.
    ///
    /// Called by the client after finishing writing data through a
    /// `SelectionWrite` pipe. The serial matches the value from the
    /// `SelectionTransfer` signal.
    #[zbus(name = "SelectionWriteDone")]
    async fn selection_write_done(
        &self,
        session_handle: ObjectPath<'_>,
        serial: u32,
        success: bool,
    ) -> zbus::fdo::Result<()> {
        tracing::debug!(
            session_handle = %session_handle,
            serial = serial,
            success = success,
            "SelectionWriteDone called"
        );

        let manager = self.session_manager.lock().await;
        let session = manager
            .get_session(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.clipboard_enabled {
            return Err(PortalError::ClipboardNotEnabled.into());
        }

        drop(manager);

        // Extract the written data from pending writes
        let entry = if let Ok(mut writes) = self.pending_writes.lock() {
            writes.remove(&serial)
        } else {
            None
        };

        // If write was successful and we have data, update the clipboard backend
        if success
            && let (Some(entry), Some(backend)) = (&entry, &self.clipboard_backend)
            && let Some(ref data) = entry.data
        {
            let mut backend = backend.lock().await;
            // Update the backend's source data for this MIME type
            let clipboard = backend
                .get_clipboard()
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
            let mut new_data: HashMap<String, Vec<u8>> = HashMap::new();
            new_data.insert(entry.mime_type.clone(), data.clone());
            backend
                .set_clipboard(ClipboardData {
                    mime_types: clipboard.mime_types,
                    data: new_data,
                })
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        }

        // Notify the clipboard backend
        if let Some(ref backend) = self.clipboard_backend {
            let mut backend = backend.lock().await;
            backend
                .write_done(serial, success)
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        }

        Ok(())
    }

    // === Signals ===

    /// Signal: Selection owner changed.
    #[zbus(signal, name = "SelectionOwnerChanged")]
    async fn selection_owner_changed(
        signal_emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// Signal: Selection transfer requested.
    #[zbus(signal, name = "SelectionTransfer")]
    async fn selection_transfer(
        signal_emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        mime_type: &str,
        serial: u32,
    ) -> zbus::Result<()>;

    // === Properties ===

    /// Interface version.
    #[zbus(property)]
    #[expect(clippy::unused_async, reason = "zbus interface requires async")]
    async fn version(&self) -> u32 {
        1
    }
}

/// Create a Unix pipe.
#[expect(
    unsafe_code,
    reason = "transmuting raw fds from nix::pipe() into OwnedFd"
)]
fn create_pipe() -> Result<(OwnedFd, OwnedFd), PortalError> {
    use nix::unistd::pipe;

    let (read, write) =
        pipe().map_err(|e| PortalError::FdPassingFailed(format!("Failed to create pipe: {e}")))?;

    // SAFETY: The raw fds from nix::pipe() are valid and open.
    let read_fd = unsafe { OwnedFd::from_raw_fd(read.as_raw_fd()) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(write.as_raw_fd()) };

    std::mem::forget(read);
    std::mem::forget(write);

    Ok((read_fd, write_fd))
}

/// Convert an `OwnedFd` to a File, taking ownership.
#[expect(unsafe_code, reason = "transmuting OwnedFd into File via raw fd")]
fn owned_fd_to_file(fd: OwnedFd) -> std::fs::File {
    let file = unsafe { std::fs::File::from_raw_fd(fd.as_raw_fd()) };
    std::mem::forget(fd);
    file
}

/// Handle data written to selection write pipe.
///
/// Reads all data from the pipe and stores it in `pending_writes`
/// keyed by serial, so it can be forwarded to the compositor when
/// `SelectionWriteDone` is called.
fn handle_selection_write(
    read_fd: OwnedFd,
    serial: u32,
    pending_writes: &PendingWrites,
) -> Result<(), PortalError> {
    use std::io::Read;

    const MAX_SIZE: usize = 100 * 1024 * 1024;

    let mut file = owned_fd_to_file(read_fd);
    let mut data = Vec::new();

    loop {
        let mut chunk = vec![0u8; 4096];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if data.len() + n > MAX_SIZE {
                    return Err(PortalError::ClipboardDataTooLarge(data.len() + n, MAX_SIZE));
                }
                data.extend_from_slice(&chunk[..n]);
            }
            Err(e) => {
                return Err(PortalError::Io(e));
            }
        }
    }

    tracing::debug!(
        serial = serial,
        size = data.len(),
        "Received clipboard data via SelectionWrite"
    );

    // Store the data so SelectionWriteDone can forward it to the clipboard backend
    if let Ok(mut writes) = pending_writes.lock() {
        if let Some(entry) = writes.get_mut(&serial) {
            entry.data = Some(data);
        } else {
            tracing::warn!(
                serial = serial,
                "SelectionWrite data received but no pending transfer for this serial"
            );
        }
    }

    Ok(())
}

/// Generate a new clipboard serial.
pub fn next_clipboard_serial() -> u32 {
    CLIPBOARD_SERIAL.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
#[expect(
    unsafe_code,
    reason = "tests create OwnedFd from raw fds for pipe testing"
)]
mod tests {
    use super::*;

    #[test]
    fn test_next_clipboard_serial_increments() {
        let s1 = next_clipboard_serial();
        let s2 = next_clipboard_serial();
        assert!(s2 > s1);
    }

    #[test]
    fn test_pending_write_entry() {
        let entry = PendingWriteEntry {
            mime_type: "text/plain".to_string(),
            data: None,
        };
        assert_eq!(entry.mime_type, "text/plain");
        assert!(entry.data.is_none());

        let entry_with_data = PendingWriteEntry {
            mime_type: "text/html".to_string(),
            data: Some(b"<b>hello</b>".to_vec()),
        };
        assert_eq!(entry_with_data.data.unwrap(), b"<b>hello</b>");
    }

    #[test]
    fn test_pending_writes_map() {
        let writes: PendingWrites = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // Insert a pending transfer
        {
            let mut map = writes.lock().unwrap();
            map.insert(
                42,
                PendingWriteEntry {
                    mime_type: "text/plain".to_string(),
                    data: None,
                },
            );
        }

        // Simulate data arriving
        {
            let mut map = writes.lock().unwrap();
            if let Some(entry) = map.get_mut(&42) {
                entry.data = Some(b"hello world".to_vec());
            }
        }

        // Verify
        {
            let map = writes.lock().unwrap();
            let entry = map.get(&42).unwrap();
            assert_eq!(entry.mime_type, "text/plain");
            assert_eq!(entry.data.as_ref().unwrap(), b"hello world");
        }
    }

    #[test]
    fn test_handle_selection_write_stores_data() {
        use std::{
            io::Write,
            os::unix::io::{AsRawFd, FromRawFd},
        };

        let writes: PendingWrites = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let serial = 99;

        // Pre-register the pending transfer
        writes.lock().unwrap().insert(
            serial,
            PendingWriteEntry {
                mime_type: "text/plain".to_string(),
                data: None,
            },
        );

        // Create a pipe
        let (read_nix, write_nix) = nix::unistd::pipe().unwrap();

        let read_fd = unsafe { OwnedFd::from_raw_fd(read_nix.as_raw_fd()) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(write_nix.as_raw_fd()) };
        std::mem::forget(read_nix);
        std::mem::forget(write_nix);

        // Write data and close the write end
        {
            let mut file = owned_fd_to_file(write_fd);
            file.write_all(b"clipboard data").unwrap();
            // file dropped, closing write end
        }

        handle_selection_write(read_fd, serial, &writes).unwrap();

        // Verify data was stored
        let map = writes.lock().unwrap();
        let entry = map.get(&serial).unwrap();
        assert_eq!(entry.data.as_ref().unwrap(), b"clipboard data");
    }

    #[test]
    fn test_handle_selection_write_no_pending_entry() {
        use std::os::unix::io::{AsRawFd, FromRawFd};

        let writes: PendingWrites = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let serial = 123;

        // No pre-registered pending transfer — data should be silently lost
        let (read_nix, write_nix) = nix::unistd::pipe().unwrap();
        let read_fd = unsafe { OwnedFd::from_raw_fd(read_nix.as_raw_fd()) };
        std::mem::forget(read_nix);

        // Close write end immediately so read gets EOF
        drop(write_nix);

        handle_selection_write(read_fd, serial, &writes).unwrap();

        // Verify map is still empty
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn test_clipboard_signal_debug() {
        let signal = ClipboardSignal::SelectionOwnerChanged {
            mime_types: vec!["text/plain".to_string()],
        };
        let debug = format!("{signal:?}");
        assert!(debug.contains("SelectionOwnerChanged"));
        assert!(debug.contains("text/plain"));
    }

    #[test]
    fn test_get_mime_types_empty_options() {
        let options: HashMap<String, OwnedValue> = HashMap::new();
        assert!(ClipboardInterface::get_mime_types(&options).is_empty());
    }

    #[test]
    fn test_get_mime_types_no_mime_types_key() {
        let mut options: HashMap<String, OwnedValue> = HashMap::new();
        options.insert("other_key".to_string(), OwnedValue::from(42u32));
        assert!(ClipboardInterface::get_mime_types(&options).is_empty());
    }
}
