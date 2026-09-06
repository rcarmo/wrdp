//! Session manager for runtime portal session lifecycle handling.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU32, Ordering},
};

use zbus::zvariant::ObjectPath;

use super::state::{PersistMode, Session};
use crate::desktop::portal::xdg_desktop::error::{PortalError, Result};

/// Default maximum sessions per application.
const DEFAULT_MAX_SESSIONS_PER_APP: usize = 10;

/// Counter for generating unique session IDs.
static SESSION_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Configuration for the session manager.
#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    /// Maximum sessions per application.
    pub max_sessions_per_app: usize,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            max_sessions_per_app: DEFAULT_MAX_SESSIONS_PER_APP,
        }
    }
}

/// Manages portal sessions.
///
/// Owns session creation/removal, ownership validation, per-app limits,
/// and stale-session cleanup coordination.
pub struct SessionManager {
    /// Active sessions keyed by session handle path string.
    sessions: HashMap<String, Session>,
    /// Count of sessions per app for limit enforcement.
    app_session_count: HashMap<String, usize>,
    /// Configuration.
    config: SessionManagerConfig,
}

impl SessionManager {
    /// Create a new session manager with default configuration.
    pub fn new() -> Self {
        Self::with_config(SessionManagerConfig::default())
    }

    /// Create a new session manager with custom configuration.
    pub fn with_config(config: SessionManagerConfig) -> Self {
        Self {
            sessions: HashMap::new(),
            app_session_count: HashMap::new(),
            config,
        }
    }

    /// Generate a new unique session handle.
    #[expect(
        clippy::expect_used,
        reason = "internal invariant established during construction or by guarded control flow"
    )]
    pub fn generate_session_handle() -> ObjectPath<'static> {
        let id = SESSION_COUNTER.fetch_add(1, Ordering::SeqCst);
        let uuid = uuid::Uuid::new_v4();
        // D-Bus object paths can only contain alphanumeric and underscores
        // Replace hyphens in UUID with underscores
        let uuid_str = uuid.to_string().replace('-', "_");
        let path = format!("/org/freedesktop/portal/generic/session/s{id}_{uuid_str}");
        ObjectPath::try_from(path).expect("generated valid object path")
    }

    /// Create a new session.
    ///
    /// Returns a mutable reference to the created session after enforcing
    /// per-application limits and duplicate-handle checks.
    pub fn create_session(
        &mut self,
        session_handle: ObjectPath<'static>,
        sender: String,
        app_id: String,
        persist_mode: PersistMode,
    ) -> Result<&mut Session> {
        let handle_str = session_handle.to_string();

        // Check session limit
        self.enforce_session_limit(&app_id)?;

        // Check for duplicate session handle (shouldn't happen but be safe)
        if self.sessions.contains_key(&handle_str) {
            return Err(PortalError::InvalidSession(format!(
                "Session already exists: {session_handle}"
            )));
        }

        // Create the session
        let mut session = Session::new(session_handle, sender, app_id.clone());
        session.persist_mode = persist_mode;

        tracing::info!(
            persist_mode = ?persist_mode,
            "Session created"
        );

        // Update counts
        *self.app_session_count.entry(app_id).or_insert(0) += 1;

        // Insert and return
        Ok(self.sessions.entry(handle_str).or_insert(session))
    }

    /// Get a session by handle.
    pub fn get_session(&self, session_handle: &ObjectPath<'_>) -> Option<&Session> {
        self.sessions.get(session_handle.as_str())
    }

    /// Get a mutable session by handle.
    pub fn get_session_mut(&mut self, session_handle: &ObjectPath<'_>) -> Option<&mut Session> {
        self.sessions.get_mut(session_handle.as_str())
    }

    /// Validate that a session exists and belongs to the given app/sender.
    ///
    /// # Arguments
    ///
    /// * `session_handle` - The session to validate
    /// * `app_id` - Expected application ID
    /// * `sender` - Expected D-Bus sender
    ///
    /// # Returns
    ///
    /// Ok(()) if valid, Err with appropriate error otherwise.
    pub fn validate_session(
        &self,
        session_handle: &ObjectPath<'_>,
        app_id: &str,
        sender: &str,
    ) -> Result<()> {
        let session = self
            .sessions
            .get(session_handle.as_str())
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        // Check app ID matches
        if session.app_id != app_id {
            tracing::warn!(
                expected_app = %session.app_id,
                actual_app = %app_id,
                "App ID mismatch"
            );
            return Err(PortalError::PermissionDenied(format!(
                "App {} tried to access session owned by {}",
                app_id, session.app_id
            )));
        }

        // Check D-Bus sender matches
        if session.sender != sender {
            tracing::warn!(
                expected_sender = %session.sender,
                actual_sender = %sender,
                "Sender mismatch"
            );
            return Err(PortalError::PermissionDenied(format!(
                "Sender {} tried to access session owned by {}",
                sender, session.sender
            )));
        }

        Ok(())
    }

    /// Close and remove a session.
    ///
    /// This marks the session as closed and removes it from the manager.
    /// Returns the closed session for resource cleanup.
    pub fn close_session(&mut self, session_handle: &ObjectPath<'_>) -> Option<Session> {
        if let Some(mut session) = self.sessions.remove(session_handle.as_str()) {
            session.close();

            // Update counts
            if let Some(count) = self.app_session_count.get_mut(&session.app_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.app_session_count.remove(&session.app_id);
                }
            }

            tracing::info!("Session removed from manager");

            Some(session)
        } else {
            None
        }
    }

    /// Close all sessions for an application.
    ///
    /// Returns a list of closed sessions for resource cleanup.
    pub fn close_app_sessions(&mut self, app_id: &str) -> Vec<Session> {
        let handles_to_close: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.app_id == app_id)
            .map(|(h, _)| h.clone())
            .collect();

        let mut closed = Vec::new();
        for handle in handles_to_close {
            if let Some(mut session) = self.sessions.remove(&handle) {
                session.close();
                closed.push(session);
            }
        }

        // Update counts
        self.app_session_count.remove(app_id);

        if !closed.is_empty() {
            tracing::info!(count = closed.len(), "Closed all sessions for app");
        }

        closed
    }

    /// Close all sessions for a D-Bus sender (when client disconnects).
    ///
    /// Returns a list of closed sessions for resource cleanup.
    pub fn close_sender_sessions(&mut self, sender: &str) -> Vec<Session> {
        let handles_to_close: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.sender == sender)
            .map(|(h, _)| h.clone())
            .collect();

        let mut closed = Vec::new();
        for handle in handles_to_close {
            if let Some(mut session) = self.sessions.remove(&handle) {
                // Update app counts
                if let Some(count) = self.app_session_count.get_mut(&session.app_id) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.app_session_count.remove(&session.app_id);
                    }
                }
                session.close();
                closed.push(session);
            }
        }

        if !closed.is_empty() {
            tracing::info!(
                sender = %sender,
                count = closed.len(),
                "Closed all sessions for sender"
            );
        }

        closed
    }

    /// Enforce session limit for an application.
    fn enforce_session_limit(&self, app_id: &str) -> Result<()> {
        let count = self.app_session_count.get(app_id).copied().unwrap_or(0);
        if count >= self.config.max_sessions_per_app {
            tracing::warn!(
                current = count,
                max = self.config.max_sessions_per_app,
                "Session limit exceeded"
            );
            return Err(PortalError::SessionLimitExceeded(
                app_id.to_string(),
                self.config.max_sessions_per_app,
            ));
        }
        Ok(())
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get the number of sessions for an application.
    pub fn app_session_count(&self, app_id: &str) -> usize {
        self.app_session_count.get(app_id).copied().unwrap_or(0)
    }

    /// Iterate over all sessions.
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    /// Get session handles for all clipboard-enabled sessions.
    pub fn clipboard_session_handles(&self) -> Vec<ObjectPath<'static>> {
        self.sessions
            .values()
            .filter(|s| s.clipboard_enabled && s.state != super::state::SessionState::Closed)
            .map(|s| s.id.clone())
            .collect()
    }

    /// Clean up stale sessions that have been idle too long.
    ///
    /// Sessions in `Init` state for longer than `max_idle` are considered stale
    /// and removed. Started sessions idle for 2x `max_idle` are also removed.
    ///
    /// Returns the list of closed sessions for resource cleanup.
    pub fn cleanup_stale_sessions(&mut self, max_idle: std::time::Duration) -> Vec<Session> {
        use std::time::SystemTime;

        let now = SystemTime::now();
        let mut stale_handles = Vec::new();

        for (handle, session) in &self.sessions {
            let elapsed = now.duration_since(session.created_at).unwrap_or_default();

            match session.state {
                super::state::SessionState::Init => {
                    // Un-started sessions that are older than max_idle
                    if elapsed > max_idle {
                        tracing::debug!(
                            elapsed_secs = elapsed.as_secs(),
                            "Stale Init session detected"
                        );
                        stale_handles.push(handle.clone());
                    }
                }
                super::state::SessionState::Started => {
                    // Started sessions idle for 2x max_idle with no activity
                    let since_activity = now
                        .duration_since(session.last_activity)
                        .unwrap_or_default();
                    if since_activity > max_idle * 2 {
                        tracing::debug!(
                            idle_secs = since_activity.as_secs(),
                            "Stale Started session detected (no activity)"
                        );
                        stale_handles.push(handle.clone());
                    }
                }
                super::state::SessionState::Closed => {
                    // Closed sessions shouldn't be in the map, but clean up just in case
                    stale_handles.push(handle.clone());
                }
            }
        }

        let mut closed = Vec::new();
        for handle in stale_handles {
            if let Some(mut session) = self.sessions.remove(&handle) {
                if let Some(count) = self.app_session_count.get_mut(&session.app_id) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.app_session_count.remove(&session.app_id);
                    }
                }
                session.close();
                closed.push(session);
            }
        }

        closed
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let mut manager = SessionManager::new();
        let handle = SessionManager::generate_session_handle();

        let session = manager
            .create_session(
                handle.clone(),
                ":1.123".to_string(),
                "com.example.app".to_string(),
                PersistMode::None,
            )
            .unwrap();

        assert_eq!(session.id, handle);
        assert_eq!(session.sender, ":1.123");
        assert_eq!(session.app_id, "com.example.app");

        assert_eq!(manager.session_count(), 1);
        assert_eq!(manager.app_session_count("com.example.app"), 1);
    }

    #[test]
    fn test_session_validation() {
        let mut manager = SessionManager::new();
        let handle = SessionManager::generate_session_handle();

        manager
            .create_session(
                handle.clone(),
                ":1.123".to_string(),
                "com.example.app".to_string(),
                PersistMode::None,
            )
            .unwrap();

        // Valid
        assert!(
            manager
                .validate_session(&handle, "com.example.app", ":1.123")
                .is_ok()
        );

        // Wrong app
        assert!(
            manager
                .validate_session(&handle, "com.other.app", ":1.123")
                .is_err()
        );

        // Wrong sender
        assert!(
            manager
                .validate_session(&handle, "com.example.app", ":1.456")
                .is_err()
        );

        // Non-existent session
        let fake_handle = SessionManager::generate_session_handle();
        assert!(
            manager
                .validate_session(&fake_handle, "com.example.app", ":1.123")
                .is_err()
        );
    }

    #[test]
    fn test_session_limit() {
        let config = SessionManagerConfig {
            max_sessions_per_app: 2,
        };
        let mut manager = SessionManager::with_config(config);

        // Create 2 sessions (at limit)
        for _ in 0..2 {
            let handle = SessionManager::generate_session_handle();
            manager
                .create_session(
                    handle,
                    ":1.123".to_string(),
                    "com.example.app".to_string(),
                    PersistMode::None,
                )
                .unwrap();
        }

        // Third should fail
        let handle = SessionManager::generate_session_handle();
        let result = manager.create_session(
            handle,
            ":1.123".to_string(),
            "com.example.app".to_string(),
            PersistMode::None,
        );
        assert!(matches!(
            result,
            Err(PortalError::SessionLimitExceeded(_, _))
        ));

        // Different app should work
        let handle = SessionManager::generate_session_handle();
        assert!(
            manager
                .create_session(
                    handle,
                    ":1.123".to_string(),
                    "com.other.app".to_string(),
                    PersistMode::None,
                )
                .is_ok()
        );
    }

    #[test]
    fn test_close_session() {
        let mut manager = SessionManager::new();
        let handle = SessionManager::generate_session_handle();

        manager
            .create_session(
                handle.clone(),
                ":1.123".to_string(),
                "com.example.app".to_string(),
                PersistMode::None,
            )
            .unwrap();

        assert_eq!(manager.session_count(), 1);

        let closed = manager.close_session(&handle);
        assert!(closed.is_some());
        assert_eq!(manager.session_count(), 0);
        assert_eq!(manager.app_session_count("com.example.app"), 0);
    }

    #[test]
    fn test_close_app_sessions() {
        let mut manager = SessionManager::new();

        // Create sessions for different apps
        for _ in 0..3 {
            let handle = SessionManager::generate_session_handle();
            manager
                .create_session(
                    handle,
                    ":1.123".to_string(),
                    "com.example.app".to_string(),
                    PersistMode::None,
                )
                .unwrap();
        }
        for _ in 0..2 {
            let handle = SessionManager::generate_session_handle();
            manager
                .create_session(
                    handle,
                    ":1.456".to_string(),
                    "com.other.app".to_string(),
                    PersistMode::None,
                )
                .unwrap();
        }

        assert_eq!(manager.session_count(), 5);

        // Close all for one app
        let closed = manager.close_app_sessions("com.example.app");
        assert_eq!(closed.len(), 3);
        assert_eq!(manager.session_count(), 2);
        assert_eq!(manager.app_session_count("com.example.app"), 0);
        assert_eq!(manager.app_session_count("com.other.app"), 2);
    }

    #[test]
    fn test_close_sender_sessions() {
        let mut manager = SessionManager::new();

        // Create sessions for different senders
        for _ in 0..3 {
            let handle = SessionManager::generate_session_handle();
            manager
                .create_session(
                    handle,
                    ":1.123".to_string(),
                    "com.example.app".to_string(),
                    PersistMode::None,
                )
                .unwrap();
        }
        let handle = SessionManager::generate_session_handle();
        manager
            .create_session(
                handle,
                ":1.456".to_string(),
                "com.example.app".to_string(),
                PersistMode::None,
            )
            .unwrap();

        assert_eq!(manager.session_count(), 4);

        // Close all for one sender
        let closed = manager.close_sender_sessions(":1.123");
        assert_eq!(closed.len(), 3);
        assert_eq!(manager.session_count(), 1);
    }

    #[test]
    fn test_session_handle_uniqueness() {
        let handles: Vec<_> = (0..100)
            .map(|_| SessionManager::generate_session_handle())
            .collect();

        // All handles should be unique
        let unique: std::collections::HashSet<_> = handles
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(unique.len(), handles.len());
    }

    #[test]
    fn test_stale_init_session_cleanup() {
        use std::time::{Duration, SystemTime};

        let mut manager = SessionManager::new();
        let handle = SessionManager::generate_session_handle();

        let session = manager
            .create_session(
                handle.clone(),
                ":1.123".to_string(),
                "com.example.app".to_string(),
                PersistMode::None,
            )
            .unwrap();

        // Backdate the creation time to make it stale
        session.created_at = SystemTime::now() - Duration::from_secs(600);

        assert_eq!(manager.session_count(), 1);

        // Cleanup with a 5-minute max idle — the 10-minute-old session should be removed
        let stale = manager.cleanup_stale_sessions(Duration::from_secs(300));
        assert_eq!(stale.len(), 1);
        assert_eq!(manager.session_count(), 0);
    }

    #[test]
    fn test_fresh_session_not_cleaned() {
        use std::time::Duration;

        let mut manager = SessionManager::new();
        let handle = SessionManager::generate_session_handle();

        manager
            .create_session(
                handle.clone(),
                ":1.123".to_string(),
                "com.example.app".to_string(),
                PersistMode::None,
            )
            .unwrap();

        assert_eq!(manager.session_count(), 1);

        // Cleanup with a 5-minute max idle — the fresh session should NOT be removed
        let stale = manager.cleanup_stale_sessions(Duration::from_secs(300));
        assert_eq!(stale.len(), 0);
        assert_eq!(manager.session_count(), 1);
    }

    #[test]
    fn test_clipboard_session_handles() {
        let mut manager = SessionManager::new();

        // Create two sessions, enable clipboard on one
        let h1 = SessionManager::generate_session_handle();
        let h2 = SessionManager::generate_session_handle();

        manager
            .create_session(
                h1.clone(),
                ":1.1".to_string(),
                "app1".to_string(),
                PersistMode::None,
            )
            .unwrap();
        let session2 = manager
            .create_session(
                h2.clone(),
                ":1.2".to_string(),
                "app2".to_string(),
                PersistMode::None,
            )
            .unwrap();
        session2.request_clipboard().unwrap();

        let handles = manager.clipboard_session_handles();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0], h2);
    }

    #[test]
    fn test_closed_session_cleaned_from_map() {
        use std::time::Duration;

        let mut manager = SessionManager::new();
        let handle = SessionManager::generate_session_handle();

        let session = manager
            .create_session(
                handle.clone(),
                ":1.123".to_string(),
                "com.example.app".to_string(),
                PersistMode::None,
            )
            .unwrap();

        // Manually set state to Closed (simulating a race where close() didn't remove it)
        session.state = crate::desktop::portal::xdg_desktop::session::state::SessionState::Closed;

        assert_eq!(manager.session_count(), 1);

        // Cleanup should remove closed sessions regardless of age
        let stale = manager.cleanup_stale_sessions(Duration::from_secs(300));
        assert_eq!(stale.len(), 1);
        assert_eq!(manager.session_count(), 0);
    }
}
