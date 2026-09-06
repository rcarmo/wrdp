//! Request interface implementation.
//!
//! Provides cancellation handling for in-progress portal operations and
//! session-associated cleanup in the runtime portal backend.

use std::sync::Arc;

use tokio::sync::Mutex;
use zbus::interface;

use crate::desktop::portal::xdg_desktop::session::SessionManager;

/// Request interface for portal operations.
///
/// Long-running portal calls expose a Request object so callers can
/// signal cancellation through `Close`.
pub struct RequestInterface {
    /// Session manager for cleanup on cancel.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Handle of the session this request is for (if any).
    session_handle: Option<String>,
}

impl RequestInterface {
    /// Create a new request interface.
    pub fn new(session_manager: Arc<Mutex<SessionManager>>) -> Self {
        Self {
            session_manager,
            session_handle: None,
        }
    }

    /// Create a standalone request interface (no session association).
    ///
    /// Used for operations that do not bind to a session handle.
    pub fn standalone() -> Self {
        Self {
            session_manager: Arc::new(Mutex::new(SessionManager::new())),
            session_handle: None,
        }
    }

    /// Create a new request interface for a specific session.
    pub fn for_session(
        session_manager: Arc<Mutex<SessionManager>>,
        session_handle: String,
    ) -> Self {
        Self {
            session_manager,
            session_handle: Some(session_handle),
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestInterface {
    /// Close the request.
    ///
    /// This cancels any in-progress operation and cleans up resources.
    async fn close(&self) {
        tracing::debug!("Request.Close called");

        // If this request is associated with a session, close it
        if let Some(session_handle) = &self.session_handle {
            let mut manager = self.session_manager.lock().await;
            if let Ok(handle) = zbus::zvariant::ObjectPath::try_from(session_handle.as_str())
                && manager.close_session(&handle).is_some()
            {
                tracing::info!("Session closed via Request.Close");
            }
        }
    }
}
