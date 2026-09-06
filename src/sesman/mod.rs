//! wrdp session manager (`wrdp-sesman`).
//!
//! This module owns the lifecycle of the per-user headless compositor stack
//! launched for wrdp sessions.
//!
//! The production single-port daemon owns the public RDP listener and binds
//! authenticated connections to these per-user display stacks. `sesman` must
//! not open listener sockets in the production path.
//!
//! Responsibilities here are limited to session registry, reuse/reconnect,
//! process health checks, and teardown for managed compositor components.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use nix::{
    fcntl::{Flock, FlockArg},
    sys::signal::Signal,
    unistd::{Gid, Uid, fchown},
};
use tracing::{debug, info, warn};

use crate::desktop::compositor::{
    launch::{LaunchContext, spawn_managed_component},
    runtime::{
        MANAGED_COMPOSITOR_COMMAND, MANAGED_COMPOSITOR_COMPONENT, MANAGED_WAYLAND_LOCK,
        MANAGED_WAYLAND_SOCKET, MANAGED_WRDP_COMPOSITOR_CONFIG_DIR,
    },
};

const STATE_VERSION: u32 = 2;
const DEFAULT_SESSION_NAME: &str = "default";
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const DEFAULT_START_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_STOP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

mod cleanup;
mod identity;
mod process;
mod state;

use identity::{gid_for_user, home_dir_for_user, supplementary_groups_for_user, uid_for_user};
pub(crate) use process::{CapturedIdentity, capture_identity, process_alive};
use process::{
    MatchPurpose, ProcessSignature, process_group_alive, process_matches, signal_process,
};

pub use state::{
    ClientInfo, ComponentConfig, ComponentState, EnsureOptions, EnsureResult, ReadinessCheck,
    SesmanConfig, SessionHealth, SessionSize, SessionState, SessionStatus,
};

/// wrdp session manager.
pub struct SessionManager {
    config: SesmanConfig,
}

impl SessionManager {
    pub fn new(config: SesmanConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &SesmanConfig {
        &self.config
    }

    /// Ensure a usable session exists, reusing healthy processes when possible.
    pub fn ensure(&self, options: EnsureOptions) -> Result<EnsureResult> {
        self.prepare_directories()?;
        let _lock = FileLock::acquire(&self.config.lock_path())?;

        let mut status = self.status()?;
        if options.force_restart {
            info!(
                "force restart requested for session {}",
                self.config.session_name
            );
            self.stop_locked()?;
            status = self.status()?;
        }

        if self.status_can_serve(&status) {
            let mut state = status
                .state
                .clone()
                .ok_or_else(|| anyhow!("usable status without state"))?;
            self.update_reconnect_state(&mut state, &options);
            self.write_state(&state)?;
            return Ok(EnsureResult {
                reused_existing: true,
                status: self.status()?,
            });
        }

        if matches!(status.health, SessionHealth::Degraded | SessionHealth::Dead) {
            warn!(
                "stopping stale/degraded session before restart: {:?}",
                status.dead_components
            );
            self.stop_locked()?;
        }

        self.cleanup_runtime_paths()?;
        let mut state = SessionState::new(&self.config);
        self.update_reconnect_state(&mut state, &options);

        let start_result = (|| -> Result<()> {
            for component in &self.config.components {
                let component_state = self.spawn_component(component)?;
                state.components.push(component_state);
                state.mark_updated();
                self.write_state(&state)?;
                self.wait_until_ready(component, &state)?;
                if component.startup_delay_ms > 0 {
                    thread::sleep(Duration::from_millis(component.startup_delay_ms));
                }
            }
            Ok(())
        })();

        if let Err(start_error) = start_result {
            // Use the in-memory state: persistence itself may have failed after
            // spawning a component, in which case re-reading the registry would
            // lose the only PID/process-group record available for cleanup.
            if let Err(cleanup_error) = self.stop_state(&state) {
                return Err(start_error).context(format!(
                    "session startup failed and partial-session cleanup also failed: {cleanup_error:#}"
                ));
            }
            return Err(start_error).context("session startup failed; partial session was stopped");
        }

        Ok(EnsureResult {
            reused_existing: false,
            status: self.status()?,
        })
    }

    /// Return current persisted session status.
    pub fn status(&self) -> Result<SessionStatus> {
        let state_path = self.config.state_path();
        let state = match self.read_state_file(&state_path) {
            Ok(content) => Some(
                serde_json::from_str::<SessionState>(&content)
                    .with_context(|| format!("failed to parse {}", state_path.display()))?,
            ),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(e) => return Err(e),
        };

        let Some(ref session_state) = state else {
            return Ok(SessionStatus {
                health: SessionHealth::Missing,
                state_path,
                state,
                dead_components: Vec::new(),
            });
        };

        let mut dead_components = Vec::new();
        let mut alive_components = 0usize;

        if !self.state_belongs_to_config(session_state) {
            dead_components.push("session-registry".to_string());
        } else {
            for configured in &self.config.components {
                let expected_command: Vec<String> = std::iter::once(configured.command.clone())
                    .chain(configured.args.clone())
                    .collect();
                let Some(component) = session_state
                    .components
                    .iter()
                    .find(|state| state.name == configured.name)
                else {
                    if configured.required {
                        dead_components.push(configured.name.clone());
                    }
                    continue;
                };

                let process_matches_state = component.command == expected_command
                    && process_matches(&component_signature(component), MatchPurpose::Status);
                let ready = process_matches_state
                    && match &configured.readiness {
                        ReadinessCheck::None | ReadinessCheck::ProcessAlive => true,
                        ReadinessCheck::UnixSocket { path } => self.unix_socket_ready(path),
                    };
                if ready {
                    alive_components += 1;
                } else {
                    dead_components.push(component.name.clone());
                }
            }

            // A component removed from configuration must not be silently reused
            // forever. Degrade the registry so ensure() tears the old stack down.
            for persisted in &session_state.components {
                if !self
                    .config
                    .components
                    .iter()
                    .any(|configured| configured.name == persisted.name)
                {
                    dead_components.push(persisted.name.clone());
                }
            }
        }

        let health = if dead_components.is_empty() {
            SessionHealth::Healthy
        } else if alive_components == 0 {
            SessionHealth::Dead
        } else {
            SessionHealth::Degraded
        };

        Ok(SessionStatus {
            health,
            state_path,
            state,
            dead_components,
        })
    }

    /// Stop the persisted session and remove the registry entry.
    pub fn stop(&self) -> Result<SessionStatus> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        self.stop_locked()?;
        self.status()
    }

    /// Mark the single client owned by the production daemon as connected.
    ///
    /// The production listener serves one connection at a time. Assigning one
    /// rather than incrementing also reconciles stale counts left by a daemon
    /// crash, and deferring this call until binding succeeds prevents leaks on
    /// failed or cancelled handler setup.
    pub fn bind_single_client(&self) -> Result<SessionStatus> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        let status = self.status()?;
        if !self.status_can_serve(&status) {
            bail!("cannot bind client to {:?} session", status.health);
        }
        let mut state = status
            .state
            .ok_or_else(|| anyhow!("healthy status without state"))?;
        state.active_clients = 1;
        state.last_disconnected_at = None;
        state.idle_deadline_at = None;
        state.mark_updated();
        self.write_state(&state)?;
        self.status()
    }

    /// Mark the single client owned by the production daemon as disconnected.
    /// Assigning zero reconciles counts left by a prior daemon crash.
    pub fn unbind_single_client(&self) -> Result<SessionStatus> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        let status = self.status()?;
        let Some(mut state) = status.state else {
            return Ok(status);
        };
        let now = Utc::now();
        state.active_clients = 0;
        state.last_disconnected_at = Some(now);
        state.idle_deadline_at = self.idle_deadline_after(now);
        state.mark_updated();
        self.write_state(&state)?;
        self.status()
    }

    /// Mark one RDP client as disconnected.
    pub fn disconnect_client(&self) -> Result<SessionStatus> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        let status = self.status()?;
        let Some(mut state) = status.state else {
            return Ok(status);
        };
        state.active_clients = state.active_clients.saturating_sub(1);
        if state.active_clients == 0 {
            let now = Utc::now();
            state.last_disconnected_at = Some(now);
            state.idle_deadline_at = self.idle_deadline_after(now);
        }
        state.mark_updated();
        self.write_state(&state)?;
        self.status()
    }

    /// Stop an idle persisted session if it has exceeded the configured idle window.
    /// Returns `Ok(true)` when a session was stopped.
    pub fn cleanup_idle(&self) -> Result<bool> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        self.cleanup_idle_locked()
    }

    /// Reconcile persisted state after the owning daemon restarted.
    ///
    /// A daemon crash can leave `active_clients` above zero with no live RDP
    /// connection, and (for a legacy or partially written registry) without a
    /// recorded disconnect time or idle deadline. Under the registry lock this
    /// zeroes any stale client count, records a disconnect time and idle
    /// deadline when missing, persists the reconciled state, and then runs idle
    /// cleanup inline via [`Self::cleanup_idle_locked`] so an already-overdue
    /// session is stopped without releasing and re-acquiring the lock.
    pub fn reconcile_after_daemon_restart(&self) -> Result<SessionStatus> {
        let _lock = FileLock::acquire(&self.config.lock_path())?;
        let status = self.status()?;
        let Some(mut state) = status.state else {
            return Ok(status);
        };
        let now = Utc::now();
        let mut changed = false;
        if state.active_clients > 0 {
            state.active_clients = 0;
            changed = true;
        }
        if state.active_clients == 0 && state.last_disconnected_at.is_none() {
            state.last_disconnected_at = Some(now);
            changed = true;
        }
        if state.active_clients == 0
            && state.idle_deadline_at.is_none()
            && self.config.idle_timeout_ms != 0
        {
            let base = state.last_disconnected_at.unwrap_or(now);
            state.idle_deadline_at = self.idle_deadline_after(base);
            changed = true;
        }
        if changed {
            state.mark_updated();
            self.write_state(&state)?;
        }
        self.cleanup_idle_locked()?;
        self.status()
    }

    /// Idle-cleanup body shared by [`Self::cleanup_idle`] and
    /// [`Self::reconcile_after_daemon_restart`]. The registry lock must already
    /// be held by the caller; this method never acquires it, avoiding a
    /// double-lock during restart reconciliation.
    fn cleanup_idle_locked(&self) -> Result<bool> {
        let status = self.status()?;
        if status.health == SessionHealth::Missing {
            return Ok(false);
        }
        let Some(state) = status.state else {
            return Ok(false);
        };
        if state.active_clients > 0 {
            return Ok(false);
        }
        let Some(deadline) = self.effective_idle_deadline(&state) else {
            return Ok(false);
        };
        if Utc::now() < deadline {
            return Ok(false);
        }

        info!(
            "stopping idle session {} for user {} past idle deadline {deadline}",
            state.name, state.user
        );
        self.stop_locked()?;
        Ok(true)
    }

    /// Compute the idle deadline for a disconnect happening at `base`, or `None`
    /// when idle recovery is disabled (`idle_timeout_ms == 0`).
    fn idle_deadline_after(&self, base: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.config.idle_timeout_ms == 0 {
            return None;
        }
        Some(base + chrono::Duration::milliseconds(self.config.idle_timeout_ms as i64))
    }

    /// Resolve the effective idle deadline for a session. A persisted deadline
    /// is authoritative; a legacy registry without one derives a deadline from
    /// `last_disconnected_at` plus the configured idle timeout. Returns `None`
    /// when idle recovery is disabled or no disconnect time is available.
    fn effective_idle_deadline(&self, state: &SessionState) -> Option<DateTime<Utc>> {
        if let Some(deadline) = state.idle_deadline_at {
            return Some(deadline);
        }
        if self.config.idle_timeout_ms == 0 {
            return None;
        }
        self.idle_deadline_after(state.last_disconnected_at?)
    }

    fn stop_locked(&self) -> Result<()> {
        let status = self.status()?;
        let Some(state) = status.state else {
            return Ok(());
        };
        self.stop_state(&state)
    }

    fn stop_state(&self, state: &SessionState) -> Result<()> {
        // Authorize every target exactly once while its leader identity is still
        // observable. A process group can remain alive after the leader exits,
        // but it is safe to keep checking that group only after authenticating
        // the leader and proving that the persisted PGID is the group we signal.
        let state_is_owned = self.state_belongs_to_config(state);
        let authorized: Vec<bool> = state
            .components
            .iter()
            .map(|component| {
                state_is_owned
                    && process_matches(&component_signature(component), MatchPurpose::Signal)
                    && (!component.process_group || component.pgid == Some(component.pid))
            })
            .collect();
        let target_alive = |index: usize, component: &ComponentState| {
            authorized[index]
                && if component.process_group {
                    process_group_alive(component.pid)
                } else {
                    process_matches(&component_signature(component), MatchPurpose::Signal)
                }
        };
        let all_targets_stopped = || {
            state
                .components
                .iter()
                .enumerate()
                .all(|(index, component)| !target_alive(index, component))
        };

        for (index, component) in state.components.iter().enumerate().rev() {
            if target_alive(index, component) {
                debug!("SIGTERM {} pid {}", component.name, component.pid);
                if let Err(error) =
                    signal_process(component.pid, component.process_group, Signal::SIGTERM)
                    && target_alive(index, component)
                {
                    return Err(error);
                }
            }
        }

        let term_deadline = Instant::now() + Duration::from_millis(self.config.stop_timeout_ms);
        while Instant::now() < term_deadline && !all_targets_stopped() {
            thread::sleep(Duration::from_millis(100));
        }

        for (index, component) in state.components.iter().enumerate().rev() {
            if target_alive(index, component) {
                warn!("SIGKILL {} pid {}", component.name, component.pid);
                if let Err(error) =
                    signal_process(component.pid, component.process_group, Signal::SIGKILL)
                    && target_alive(index, component)
                {
                    return Err(error);
                }
            }
        }

        // SIGKILL delivery is asynchronous. Keep the authenticated registry if
        // any target survives so a later operator/retry can inspect and stop it.
        let kill_deadline = Instant::now() + Duration::from_millis(self.config.stop_timeout_ms);
        while Instant::now() < kill_deadline && !all_targets_stopped() {
            thread::sleep(Duration::from_millis(100));
        }
        let survivors: Vec<String> = state
            .components
            .iter()
            .enumerate()
            .filter(|(index, component)| target_alive(*index, component))
            .map(|(_, component)| component.name.clone())
            .collect();
        if !survivors.is_empty() {
            bail!(
                "managed targets survived SIGKILL; retaining session state: {}",
                survivors.join(", ")
            );
        }

        // Required cleanup happens before deleting the registry. If cleanup
        // fails, retain state rather than claiming teardown completed.
        self.cleanup_runtime_paths()?;
        self.remove_state_file(&self.config.state_path())
    }

    fn state_belongs_to_config(&self, state: &SessionState) -> bool {
        let expected_uid = uid_for_user(&self.config.user).ok();
        let expected_gid = gid_for_user(&self.config.user).ok();
        state.version == STATE_VERSION
            && state.name == self.config.session_name
            && state.user == self.config.user
            && state.xdg_runtime_dir == self.config.xdg_runtime_dir
            && state.boot_id.as_deref() == process::read_boot_id().as_deref()
            && state.uid == expected_uid
            && state.gid == expected_gid
    }

    fn status_can_serve(&self, status: &SessionStatus) -> bool {
        status.health == SessionHealth::Healthy
            || (status.health == SessionHealth::Degraded
                && status.dead_components.iter().all(|name| {
                    self.config
                        .components
                        .iter()
                        .find(|component| component.name == *name)
                        .is_some_and(|component| !component.required)
                }))
    }

    fn update_reconnect_state(&self, state: &mut SessionState, options: &EnsureOptions) {
        if let Some(size) = options.requested_size {
            state.requested_size = Some(size);
        }
        if let Some(peer) = options.client_peer.clone() {
            state.last_client = Some(ClientInfo {
                peer,
                connected_at: Utc::now(),
                requested_size: options.requested_size,
            });
        }
        if options.client_connected {
            state.active_clients = state.active_clients.saturating_add(1);
            state.last_disconnected_at = None;
            state.idle_deadline_at = None;
        }
        state.mark_updated();
    }

    /// Create and validate the directories which contain trusted sesman state
    /// and user-controlled runtime artifacts.  Never chmod/chown a path until
    /// it has been checked with `symlink_metadata`; ownership changes happen on
    /// an opened directory descriptor.
    fn prepare_directories(&self) -> Result<()> {
        let target_uid = uid_for_user(&self.config.user)?;
        let target_gid = gid_for_user(&self.config.user)?;
        let effective_uid = Uid::effective().as_raw();
        let effective_gid = Gid::effective().as_raw();

        if self.is_production_runtime(target_uid) {
            let runtime_root = PathBuf::from(format!("/run/user/{target_uid}"));
            let metadata = fs::symlink_metadata(&runtime_root).with_context(|| {
                format!(
                    "production runtime root {} is unavailable",
                    runtime_root.display()
                )
            })?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != target_uid
            {
                bail!(
                    "unsafe production runtime root {}; expected directory owned by {target_uid}",
                    runtime_root.display()
                );
            }
        }

        ensure_secure_directory(
            &self.config.state_dir,
            effective_uid,
            effective_gid,
            true,
            "sesman state directory",
        )?;
        ensure_secure_directory(
            &self.config.xdg_runtime_dir,
            target_uid,
            target_gid,
            true,
            "user runtime directory",
        )?;
        ensure_secure_directory(
            &self.config.log_dir,
            target_uid,
            target_gid,
            true,
            "component log directory",
        )
    }

    fn is_production_runtime(&self, target_uid: u32) -> bool {
        self.config.xdg_runtime_dir == format!("/run/user/{target_uid}/wrdp")
    }

    fn is_production_registry(&self) -> bool {
        uid_for_user(&self.config.user)
            .is_ok_and(|uid| self.config.state_dir == format!("/run/wrdp/sesman/{uid}"))
    }

    fn spawn_component(&self, component: &ComponentConfig) -> Result<ComponentState> {
        let uid = uid_for_user(&self.config.user)?;
        let gid = gid_for_user(&self.config.user)?;
        let groups = supplementary_groups_for_user(&self.config.user, gid)?;
        let home = home_dir_for_user(&self.config.user)?;
        spawn_managed_component(
            component,
            &LaunchContext {
                user: &self.config.user,
                uid,
                gid,
                groups: &groups,
                home: &home,
                xdg_runtime_dir: &self.config.xdg_runtime_dir,
                log_dir: &self.config.log_dir,
                environment: &self.config.environment,
            },
        )
    }

    fn wait_until_ready(&self, component: &ComponentConfig, state: &SessionState) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.config.start_timeout_ms);
        loop {
            let alive = state
                .components
                .iter()
                .find(|candidate| candidate.name == component.name)
                .is_some_and(|candidate| {
                    process_matches(&component_signature(candidate), MatchPurpose::Status)
                });
            if !alive {
                if component.required {
                    bail!("component {} exited before becoming ready", component.name);
                }
                warn!(
                    "optional component {} exited before becoming ready; continuing",
                    component.name
                );
                return Ok(());
            }

            match &component.readiness {
                ReadinessCheck::None | ReadinessCheck::ProcessAlive => return Ok(()),
                ReadinessCheck::UnixSocket { path } if self.unix_socket_ready(path) => {
                    return Ok(());
                }
                ReadinessCheck::UnixSocket { .. } => {}
            }

            if Instant::now() >= deadline {
                if component.required {
                    bail!(
                        "timed out waiting for component {} readiness",
                        component.name
                    );
                }
                warn!(
                    "optional component {} did not become ready; continuing",
                    component.name
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn unix_socket_ready(&self, path: &Path) -> bool {
        let Ok(target_uid) = uid_for_user(&self.config.user) else {
            return false;
        };
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return false;
        };
        !metadata.file_type().is_symlink()
            && metadata.file_type().is_socket()
            && metadata.uid() == target_uid
            && UnixStream::connect(path).is_ok()
    }

    fn cleanup_runtime_paths(&self) -> Result<()> {
        cleanup::cleanup_runtime_paths(&self.config.cleanup_paths, &self.config.cleanup_globs)
    }

    fn read_state_file(&self, path: &Path) -> Result<String> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("unsafe sesman state file: {}", path.display());
        }
        if self.is_production_registry() && metadata.uid() != Uid::effective().as_raw() {
            bail!("untrusted production sesman state file: {}", path.display());
        }

        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect opened state file {}", path.display()))?;
        if !metadata.is_file()
            || (self.is_production_registry() && metadata.uid() != Uid::effective().as_raw())
        {
            bail!("untrusted sesman state file: {}", path.display());
        }
        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(content)
    }

    fn remove_state_file(&self, path: &Path) -> Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("unsafe sesman state file: {}", path.display());
        }
        if self.is_production_registry() && metadata.uid() != Uid::effective().as_raw() {
            bail!("untrusted production sesman state file: {}", path.display());
        }
        fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
    }

    fn write_state(&self, state: &SessionState) -> Result<()> {
        let path = self.config.state_path();
        let tmp_path = path.with_extension("state.json.tmp");
        let data = serde_json::to_vec_pretty(state).context("failed to serialize session state")?;
        let mut tmp_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp_path)
            .context("failed to open temporary sesman state file")?;
        tmp_file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("failed to restrict temporary sesman state file")?;
        tmp_file
            .write_all(&data)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .context("failed to sync temporary sesman state file")?;
        drop(tmp_file);
        fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                tmp_path.display()
            )
        })?;
        Ok(())
    }
}

/// An advisory, process-lifetime registry lock.  The lock file is deliberately
/// retained: `flock(2)` releases it automatically when this descriptor closes,
/// so there is no stale-file recovery race or unlink window.
struct FileLock {
    _file: Flock<File>,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("failed to open lock {}", path.display()))?;
        let file = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, error)| {
            anyhow!("session is already locked: {}: {error}", path.display())
        })?;
        Ok(Self { _file: file })
    }
}

fn component_signature(component: &ComponentState) -> ProcessSignature<'_> {
    ProcessSignature {
        pid: component.pid,
        command: &component.command,
        started_at: component.started_at,
        start_ticks: component.start_ticks,
        boot_id: component.boot_id.as_deref(),
        uid: component.uid,
        pgid: component.pgid,
    }
}

/// Securely create a directory hierarchy and validate the final directory.
/// Ancestors are never chmodded/chowned; only the leaf is managed by sesman.
fn ensure_secure_directory(
    path: &Path,
    uid: u32,
    gid: u32,
    chown_created: bool,
    description: &str,
) -> Result<()> {
    if !path.is_absolute() {
        bail!("{description} must be absolute: {}", path.display());
    }

    let mut current = PathBuf::from("/");
    let mut created_leaf = false;
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => current.push(name),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                bail!("unsafe {description} path: {}", path.display());
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("unsafe {description}: {}", current.display());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {
                        if current == path {
                            created_leaf = true;
                        }
                    }
                    // Another concurrent session setup created this component.
                    // Revalidate it below before accepting it.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to create {description} {}", current.display())
                        });
                    }
                }
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!("failed to inspect {description} {}", current.display())
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("unsafe {description}: {}", current.display());
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect {description} {}", current.display())
                });
            }
        }
    }

    // Open the already checked leaf without following a replacement symlink;
    // permissions and ownership are then changed through this descriptor.
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open {description} {}", path.display()))?;
    let metadata = directory
        .metadata()
        .with_context(|| format!("failed to inspect opened {description} {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("unsafe {description}: {}", path.display());
    }

    if created_leaf && chown_created {
        fchown(
            &directory,
            Some(Uid::from_raw(uid)),
            Some(Gid::from_raw(gid)),
        )
        .with_context(|| format!("failed to chown {description} {}", path.display()))?;
    }
    let metadata = directory
        .metadata()
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    if metadata.uid() != uid || metadata.gid() != gid {
        bail!(
            "{description} {} is not owned by {uid}:{gid}",
            path.display()
        );
    }
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to restrict {description} {}", path.display()))?;
    Ok(())
}

/// Root of the root-owned production session registry tree.
const PRODUCTION_REGISTRY_ROOT: &str = "/run/wrdp/sesman";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProductionSessionScanOperation {
    ReconcileAfterDaemonRestart,
    CleanupIdle,
}

impl ProductionSessionScanOperation {
    fn name(self) -> &'static str {
        match self {
            Self::ReconcileAfterDaemonRestart => "restart reconciliation",
            Self::CleanupIdle => "idle cleanup",
        }
    }

    fn applies_startup_reconciliation(self) -> bool {
        matches!(self, Self::ReconcileAfterDaemonRestart)
    }

    fn apply(self, manager: &SessionManager) -> Result<()> {
        if self.applies_startup_reconciliation() {
            manager.reconcile_after_daemon_restart()?;
        } else {
            // Periodic cleanup must not alter the active connection count. Only
            // startup reconciliation can recover that count after a daemon exit.
            let _ = manager.cleanup_idle()?;
        }
        Ok(())
    }
}

/// Reconcile every production per-user session after a daemon restart.
pub fn reconcile_production_sessions() -> Result<()> {
    scan_production_sessions(ProductionSessionScanOperation::ReconcileAfterDaemonRestart)
}

/// Clean up expired idle production sessions without changing connection state.
pub fn cleanup_production_sessions() -> Result<()> {
    scan_production_sessions(ProductionSessionScanOperation::CleanupIdle)
}

/// Scan root-owned production registries and apply one lifecycle operation.
///
/// Nothing under the user-writable `/run/user` tree is trusted. The registry
/// root, each numeric-UID directory, and each `*.state.json` file must be
/// root-owned; symlinks are refused (`O_NOFOLLOW`). State is deserialized only
/// to learn its user, and the manager's re-derived state path must exactly
/// match the scanned path. Errors for individual entries are logged and do not
/// prevent processing other users.
fn scan_production_sessions(operation: ProductionSessionScanOperation) -> Result<()> {
    let root = Path::new(PRODUCTION_REGISTRY_ROOT);
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", root.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != 0 {
        bail!(
            "production registry root {} is not a root-owned directory",
            root.display()
        );
    }

    let entries =
        fs::read_dir(root).with_context(|| format!("failed to scan {}", root.display()))?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(
                    "skipping unreadable production registry entry in {} during {}: {error:#}",
                    root.display(),
                    operation.name()
                );
                continue;
            }
        };
        let dir_path = entry.path();
        if let Err(error) = scan_production_registry_dir(&dir_path, operation) {
            warn!(
                "skipping production session registry {} during {}: {error:#}",
                dir_path.display(),
                operation.name()
            );
        }
    }
    Ok(())
}

/// Scan one numeric-UID directory under the production registry root.
fn scan_production_registry_dir(
    dir_path: &Path,
    operation: ProductionSessionScanOperation,
) -> Result<()> {
    let Some(dir_uid) = dir_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<u32>().ok())
    else {
        // Only per-UID directories are production registries; ignore the rest.
        return Ok(());
    };
    let metadata = fs::symlink_metadata(dir_path)
        .with_context(|| format!("failed to inspect {}", dir_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != 0 {
        bail!(
            "registry {} is not a root-owned directory",
            dir_path.display()
        );
    }
    let entries =
        fs::read_dir(dir_path).with_context(|| format!("failed to scan {}", dir_path.display()))?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(
                    "skipping unreadable production registry entry in {} during {}: {error:#}",
                    dir_path.display(),
                    operation.name()
                );
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Err(error) = scan_production_state_file(&path, dir_uid, operation) {
            warn!(
                "skipping production session registry {} during {}: {error:#}",
                path.display(),
                operation.name()
            );
        }
    }
    Ok(())
}

/// Validate one root-owned state file and apply the requested lifecycle operation.
fn scan_production_state_file(
    path: &Path,
    dir_uid: u32,
    operation: ProductionSessionScanOperation,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.uid() != 0 {
        bail!(
            "state file {} is not a root-owned regular file",
            path.display()
        );
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("failed to inspect opened state file {}", path.display()))?;
    if !opened.is_file() || opened.uid() != 0 {
        bail!(
            "state file {} is not a root-owned regular file",
            path.display()
        );
    }
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("failed to read {}", path.display()))?;

    // Deserialize only to learn the owning user. Every other trusted path is
    // re-derived from the user's own manager config below, never from the file.
    let state: SessionState = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let user = state.user;
    let resolved_uid =
        uid_for_user(&user).with_context(|| format!("failed to resolve session user {user}"))?;
    if resolved_uid != dir_uid {
        bail!(
            "registry {} names user {user} (uid {resolved_uid}) but lives under uid {dir_uid}",
            path.display()
        );
    }
    let config = SesmanConfig::for_user(&user)?;
    if config.state_path() != path {
        bail!(
            "registry {} does not match configured state path {} for user {user}",
            path.display(),
            config.state_path().display()
        );
    }
    operation.apply(&SessionManager::new(config))
}

fn default_true() -> bool {
    true
}

fn default_session_name() -> String {
    DEFAULT_SESSION_NAME.to_string()
}

fn default_start_timeout_ms() -> u64 {
    DEFAULT_START_TIMEOUT_MS
}

fn default_stop_timeout_ms() -> u64 {
    DEFAULT_STOP_TIMEOUT_MS
}

fn default_idle_timeout_ms() -> u64 {
    DEFAULT_IDLE_TIMEOUT_MS
}

fn default_size() -> SessionSize {
    SessionSize::default()
}

fn default_user() -> String {
    let uid = nix::unistd::Uid::effective();
    nix::unistd::User::from_uid(uid)
        .ok()
        .flatten()
        .map_or_else(String::new, |user| user.name)
}

fn default_xdg_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || PathBuf::from(format!("/run/user/{}/wrdp", nix::unistd::Uid::effective())),
        PathBuf::from,
    )
}

fn default_state_dir() -> PathBuf {
    default_xdg_runtime_dir().join("sesman")
}

fn default_log_dir() -> PathBuf {
    default_xdg_runtime_dir().join("logs")
}

fn default_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "XDG_CURRENT_DESKTOP".to_string(),
            "wrdp-compositor".to_string(),
        ),
        (
            "XDG_SESSION_DESKTOP".to_string(),
            "wrdp-compositor".to_string(),
        ),
        // wrdp starts wrdp-compositor as a headless per-user compositor, not as a
        // seat/DRM compositor. Keep the headless backend, but render through EGL
        // on the configured render node instead of forcing the CPU-only Pixman
        // renderer. The render node does not require logind/TTY ownership.
        ("WLR_BACKENDS".to_string(), "headless".to_string()),
        ("WLR_LIBINPUT_NO_DEVICES".to_string(), "1".to_string()),
        (
            "WLR_RENDERER".to_string(),
            std::env::var("WRDP_WLR_RENDERER").unwrap_or_else(|_| "gles2".to_string()),
        ),
        (
            "WLR_RENDER_DRM_DEVICE".to_string(),
            std::env::var("WRDP_RENDER_DRM_DEVICE")
                .unwrap_or_else(|_| "/dev/dri/renderD128".to_string()),
        ),
        ("WLR_NO_HARDWARE_CURSORS".to_string(), "1".to_string()),
        ("LIBVA_DRIVER_NAME".to_string(), "iHD".to_string()),
        ("RUST_BACKTRACE".to_string(), "1".to_string()),
        (
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin".to_string(),
        ),
    ])
}

fn default_cleanup_paths() -> Vec<PathBuf> {
    let runtime = default_xdg_runtime_dir();
    vec![
        runtime.join(MANAGED_WAYLAND_SOCKET),
        runtime.join(MANAGED_WAYLAND_LOCK),
    ]
}

fn default_cleanup_globs() -> Vec<String> {
    Vec::new()
}

fn default_components() -> Vec<ComponentConfig> {
    let runtime = default_xdg_runtime_dir();
    vec![ComponentConfig {
        name: MANAGED_COMPOSITOR_COMPONENT.to_string(),
        command: MANAGED_COMPOSITOR_COMMAND.to_string(),
        args: vec![
            "--config-dir".to_string(),
            MANAGED_WRDP_COMPOSITOR_CONFIG_DIR.to_string(),
        ],
        env: BTreeMap::new(),
        working_dir: None,
        log_path: None,
        readiness: ReadinessCheck::UnixSocket {
            path: runtime.join(MANAGED_WAYLAND_SOCKET),
        },
        required: true,
        startup_delay_ms: 250,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn default_paths_are_scoped_to_the_effective_user() {
        let config = SesmanConfig::default();
        let expected_root = default_xdg_runtime_dir();
        assert_eq!(config.xdg_runtime_dir, expected_root);
        assert_eq!(config.state_dir, expected_root.join("sesman"));
        assert_eq!(config.log_dir, expected_root.join("logs"));
        assert_eq!(
            config.cleanup_paths,
            vec![
                expected_root.join(MANAGED_WAYLAND_SOCKET),
                expected_root.join(MANAGED_WAYLAND_LOCK),
            ]
        );
        assert!(config.cleanup_globs.is_empty());
    }

    #[test]
    fn production_session_scan_cleanup_does_not_reconcile_client_counts() {
        assert!(
            ProductionSessionScanOperation::ReconcileAfterDaemonRestart
                .applies_startup_reconciliation()
        );
        assert!(!ProductionSessionScanOperation::CleanupIdle.applies_startup_reconciliation());
        assert_eq!(
            ProductionSessionScanOperation::CleanupIdle.name(),
            "idle cleanup"
        );
    }

    #[test]
    fn default_config_matches_managed_wrdp_compositor_stack() {
        let config = SesmanConfig::default();
        assert_eq!(config.components.len(), 1);
        assert_eq!(config.components[0].name, MANAGED_COMPOSITOR_COMPONENT);
        assert_eq!(config.components[0].command, MANAGED_COMPOSITOR_COMMAND);
        assert!(
            config.components[0]
                .args
                .iter()
                .any(|arg| arg == "--config-dir")
        );
        assert!(
            config.components[0]
                .args
                .iter()
                .any(|arg| arg == MANAGED_WRDP_COMPOSITOR_CONFIG_DIR)
        );
        assert!(matches!(
            config.components[0].readiness,
            ReadinessCheck::UnixSocket { ref path } if path.ends_with("wayland-0")
        ));
    }

    #[test]
    fn supplementary_group_helper_includes_primary_group() {
        let output = Command::new("id").arg("-un").output().expect("id -un");
        assert!(output.status.success());
        let user = String::from_utf8(output.stdout)
            .expect("id -un output should be UTF-8")
            .trim()
            .to_string();
        let primary_gid = gid_for_user(&user).expect("current user gid should resolve");
        let groups = supplementary_groups_for_user(&user, primary_gid)
            .expect("current user groups should resolve");
        assert!(groups.contains(&primary_gid));
    }

    #[test]
    fn user_identity_helpers_resolve_current_account() {
        let output = Command::new("id").arg("-un").output().expect("id -un");
        assert!(output.status.success());
        let user = String::from_utf8(output.stdout)
            .expect("id -un output should be UTF-8")
            .trim()
            .to_string();
        uid_for_user(&user).expect("uid should resolve");
        gid_for_user(&user).expect("gid should resolve");
        assert!(
            home_dir_for_user(&user)
                .expect("home should resolve")
                .is_absolute()
        );
    }

    #[test]
    fn per_user_config_uses_wrdp_runtime_layout_without_listener_socket() {
        let user_output = Command::new("id").arg("-un").output().expect("id -un");
        assert!(user_output.status.success());
        let user = String::from_utf8(user_output.stdout)
            .expect("id -un output should be UTF-8")
            .trim()
            .to_string();
        let uid_output = Command::new("id")
            .args(["-u", &user])
            .output()
            .expect("id -u should run for current user");
        if !uid_output.status.success() {
            return;
        }
        let uid = String::from_utf8(uid_output.stdout)
            .expect("uid should be UTF-8")
            .trim()
            .to_string();

        let config = SesmanConfig::for_user(&user).expect("current user should resolve");
        assert_eq!(config.user, user);
        assert_eq!(
            config.xdg_runtime_dir,
            PathBuf::from(format!("/run/user/{uid}/wrdp"))
        );
        assert_eq!(
            config.state_dir,
            PathBuf::from(format!("/run/wrdp/sesman/{uid}"))
        );
        assert_eq!(config.log_dir, config.xdg_runtime_dir.join("logs"));
        assert!(config.components.iter().all(|component| {
            !component.command.contains("rdp-server")
                && !component.command.contains("wrdp-server")
                && component.args.iter().all(|arg| !arg.contains(":339"))
        }));
        assert!(
            config
                .components
                .iter()
                .any(|component| component.command == MANAGED_COMPOSITOR_COMMAND)
        );
    }

    #[test]
    fn degraded_status_is_usable_only_for_optional_failures() {
        let mut config = SesmanConfig::default();
        config.components.push(ComponentConfig {
            name: "optional".to_string(),
            command: "/bin/false".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            working_dir: None,
            log_path: None,
            readiness: ReadinessCheck::ProcessAlive,
            required: false,
            startup_delay_ms: 0,
        });
        let manager = SessionManager::new(config.clone());
        let mut status = SessionStatus {
            health: SessionHealth::Degraded,
            state_path: config.state_path(),
            state: None,
            dead_components: vec!["optional".to_string()],
        };
        assert!(manager.status_can_serve(&status));

        status.dead_components = vec![MANAGED_COMPOSITOR_COMPONENT.to_string()];
        assert!(!manager.status_can_serve(&status));
        status.dead_components = vec!["unknown".to_string()];
        assert!(!manager.status_can_serve(&status));
    }

    #[test]
    fn incomplete_registry_is_not_reported_healthy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        manager
            .write_state(&SessionState::new(&config))
            .expect("write incomplete state");

        let status = manager.status().expect("status");
        assert_eq!(status.health, SessionHealth::Dead);
        assert_eq!(status.dead_components, vec![MANAGED_COMPOSITOR_COMPONENT]);
    }

    #[test]
    fn unbind_single_client_reconciles_stale_count() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.active_clients = 7;
        manager.write_state(&state).expect("write state");

        let status = manager.unbind_single_client().expect("unbind");
        let state = status.state.expect("state remains after unbind");
        assert_eq!(state.active_clients, 0);
        assert!(state.last_disconnected_at.is_some());
        assert!(state.idle_deadline_at.is_some());
    }

    #[test]
    fn bind_clears_idle_deadline() {
        let temp = tempfile::tempdir().expect("tempdir");
        let current_user = Command::new("id").arg("-un").output().expect("id -un");
        assert!(current_user.status.success());
        let user = String::from_utf8(current_user.stdout)
            .expect("id output utf8")
            .trim()
            .to_string();

        let mut child = Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep component");

        let mut config = SesmanConfig::default();
        config.user = user;
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.components = vec![ComponentConfig {
            name: MANAGED_COMPOSITOR_COMPONENT.to_string(),
            command: "/bin/sleep".to_string(),
            args: vec!["60".to_string()],
            env: BTreeMap::new(),
            working_dir: None,
            log_path: None,
            readiness: ReadinessCheck::ProcessAlive,
            required: true,
            startup_delay_ms: 0,
        }];
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.active_clients = 0;
        state.last_disconnected_at = Some(Utc::now());
        state.idle_deadline_at = Some(Utc::now());
        let pid = i32::try_from(child.id()).expect("pid fits i32");
        let identity = capture_identity(pid);
        state.components.push(ComponentState {
            name: MANAGED_COMPOSITOR_COMPONENT.to_string(),
            pid,
            command: vec!["/bin/sleep".to_string(), "60".to_string()],
            started_at: Utc::now(),
            start_ticks: identity.start_ticks,
            boot_id: identity.boot_id,
            uid: identity.uid,
            pgid: identity.pgid,
            required: true,
            process_group: false,
        });
        manager.write_state(&state).expect("write state");

        let status = manager.bind_single_client().expect("bind");
        let state = status.state.expect("state after bind");
        assert_eq!(state.active_clients, 1);
        assert!(state.last_disconnected_at.is_none());
        assert!(state.idle_deadline_at.is_none());

        manager.stop().expect("cleanup spawned component");
        let _ = child.try_wait();
    }

    #[test]
    fn disconnect_marks_idle_when_last_client_leaves() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.active_clients = 1;
        manager.write_state(&state).expect("write state");

        let status = manager.disconnect_client().expect("disconnect");
        let state = status.state.expect("state remains after disconnect");
        assert_eq!(state.active_clients, 0);
        assert!(state.last_disconnected_at.is_some());
        assert!(state.idle_deadline_at.is_some());
    }

    #[test]
    fn reconcile_after_daemon_restart_zeroes_stale_clients_and_sets_deadline() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.idle_timeout_ms = 60_000;
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        // A crashed daemon left an active client with no disconnect record.
        state.active_clients = 3;
        state.last_disconnected_at = None;
        state.idle_deadline_at = None;
        manager.write_state(&state).expect("write state");

        let status = manager.reconcile_after_daemon_restart().expect("reconcile");
        let state = status.state.expect("state remains after reconcile");
        assert_eq!(state.active_clients, 0);
        assert!(state.last_disconnected_at.is_some());
        assert!(state.idle_deadline_at.is_some());
    }

    #[test]
    fn reconcile_after_daemon_restart_stops_overdue_idle_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.idle_timeout_ms = 1;
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.active_clients = 0;
        state.last_disconnected_at = Some(Utc::now() - chrono::Duration::milliseconds(50));
        state.idle_deadline_at = Some(Utc::now() - chrono::Duration::milliseconds(49));
        manager.write_state(&state).expect("write state");

        let status = manager.reconcile_after_daemon_restart().expect("reconcile");
        assert_eq!(status.health, SessionHealth::Missing);
    }

    #[test]
    fn disconnect_decrements_active_clients_without_marking_idle_until_zero() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.active_clients = 2;
        manager.write_state(&state).expect("write state");

        let status = manager.disconnect_client().expect("first disconnect");
        let state = status.state.expect("state remains after disconnect");
        assert_eq!(state.active_clients, 1);
        assert!(state.last_disconnected_at.is_none());

        let status = manager.disconnect_client().expect("second disconnect");
        let state = status.state.expect("state remains after disconnect");
        assert_eq!(state.active_clients, 0);
        assert!(state.last_disconnected_at.is_some());
    }

    #[test]
    fn ensure_reuses_healthy_session_for_reconnect() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut child = Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep component");

        let current_user = Command::new("id").arg("-un").output().expect("id -un");
        assert!(current_user.status.success());
        let user = String::from_utf8(current_user.stdout)
            .expect("id output utf8")
            .trim()
            .to_string();

        let mut config = SesmanConfig::default();
        config.user = user;
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.cleanup_paths = vec![config.xdg_runtime_dir.join(MANAGED_WAYLAND_SOCKET)];
        config.components = vec![ComponentConfig {
            name: MANAGED_COMPOSITOR_COMPONENT.to_string(),
            command: "/bin/sleep".to_string(),
            args: vec!["60".to_string()],
            env: BTreeMap::new(),
            working_dir: None,
            log_path: None,
            readiness: ReadinessCheck::ProcessAlive,
            required: true,
            startup_delay_ms: 0,
        }];
        fs::create_dir_all(&config.state_dir).expect("state dir");
        fs::create_dir_all(&config.log_dir).expect("log dir");
        fs::create_dir_all(&config.xdg_runtime_dir).expect("runtime dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.active_clients = 1;
        let pid = i32::try_from(child.id()).expect("pid fits i32");
        let identity = capture_identity(pid);
        state.components.push(ComponentState {
            name: MANAGED_COMPOSITOR_COMPONENT.to_string(),
            pid,
            command: vec!["/bin/sleep".to_string(), "60".to_string()],
            started_at: Utc::now(),
            start_ticks: identity.start_ticks,
            boot_id: identity.boot_id,
            uid: identity.uid,
            pgid: identity.pgid,
            required: true,
            process_group: false,
        });
        manager.write_state(&state).expect("write state");

        let result = manager
            .ensure(EnsureOptions {
                client_connected: true,
                client_peer: Some("127.0.0.1:3389".to_string()),
                ..EnsureOptions::default()
            })
            .expect("ensure should reuse healthy state");
        assert!(result.reused_existing);
        let state = result.status.state.expect("state after ensure");
        assert_eq!(state.active_clients, 2);
        assert!(state.last_disconnected_at.is_none());
        assert_eq!(state.components.len(), 1);
        assert_eq!(state.components[0].pid, i32::try_from(child.id()).unwrap());

        manager.stop().expect("cleanup spawned component");
        let _ = child.try_wait();
    }

    #[test]
    fn stop_terminates_managed_component_process_group() {
        if !nix::unistd::Uid::effective().is_root() {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let current_user = Command::new("id").arg("-un").output().expect("id -un");
        assert!(current_user.status.success());
        let user = String::from_utf8(current_user.stdout)
            .expect("id output utf8")
            .trim()
            .to_string();
        let child_pid_path = temp.path().join("child.pid");

        let mut config = SesmanConfig::default();
        config.user = user;
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.stop_timeout_ms = 1_000;
        config.components = vec![ComponentConfig {
            name: "shell-tree".to_string(),
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("sleep 60 & echo $! > {}; wait", child_pid_path.display()),
            ],
            env: BTreeMap::new(),
            working_dir: None,
            log_path: None,
            readiness: ReadinessCheck::ProcessAlive,
            required: true,
            startup_delay_ms: 50,
        }];

        let manager = SessionManager::new(config);
        let ensured = manager
            .ensure(EnsureOptions::default())
            .expect("start process tree");
        let leader_pid = ensured.status.state.expect("state").components[0].pid;
        let child_pid = fs::read_to_string(&child_pid_path)
            .expect("child pid file")
            .trim()
            .parse::<i32>()
            .expect("child pid");

        manager.stop().expect("stop process tree");
        assert!(!process::process_alive(leader_pid));
        assert!(!process::process_alive(child_pid));
    }

    #[test]
    fn stop_does_not_signal_reused_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut child = Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep component");

        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.components = vec![ComponentConfig {
            name: "sleep".to_string(),
            command: "/bin/sleep".to_string(),
            args: vec!["60".to_string()],
            env: BTreeMap::new(),
            working_dir: None,
            log_path: None,
            readiness: ReadinessCheck::ProcessAlive,
            required: true,
            startup_delay_ms: 0,
        }];
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.components.push(ComponentState {
            name: "sleep".to_string(),
            pid: i32::try_from(child.id()).expect("pid fits i32"),
            command: vec!["/bin/sleep".to_string(), "60".to_string()],
            started_at: Utc::now() - chrono::Duration::minutes(1),
            start_ticks: None,
            boot_id: None,
            uid: None,
            pgid: None,
            required: true,
            process_group: false,
        });
        manager.write_state(&state).expect("write state");

        assert_eq!(
            manager.status().expect("status").health,
            SessionHealth::Dead
        );
        manager.stop().expect("remove stale registry");
        assert!(child.try_wait().expect("inspect sleep").is_none());

        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");
    }

    #[test]
    fn ensure_restarts_dead_compositor_state() {
        if !nix::unistd::Uid::effective().is_root() {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let current_user = Command::new("id").arg("-un").output().expect("id -un");
        assert!(current_user.status.success());
        let user = String::from_utf8(current_user.stdout)
            .expect("id output utf8")
            .trim()
            .to_string();

        let mut config = SesmanConfig::default();
        config.user = user;
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.cleanup_paths = vec![config.xdg_runtime_dir.join(MANAGED_WAYLAND_SOCKET)];
        config.start_timeout_ms = 2_000;
        config.stop_timeout_ms = 500;
        config.components = vec![ComponentConfig {
            name: MANAGED_COMPOSITOR_COMPONENT.to_string(),
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                r#"touch "$XDG_RUNTIME_DIR/wayland-0"; sleep 60"#.to_string(),
            ],
            env: BTreeMap::new(),
            working_dir: None,
            log_path: Some(temp.path().join("logs/wrdp-compositor.log")),
            readiness: ReadinessCheck::ProcessAlive,
            required: true,
            startup_delay_ms: 0,
        }];
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut dead_state = SessionState::new(&config);
        dead_state.components.push(ComponentState {
            name: MANAGED_COMPOSITOR_COMPONENT.to_string(),
            pid: 999_999,
            command: vec!["/bin/false".to_string()],
            started_at: Utc::now(),
            start_ticks: None,
            boot_id: None,
            uid: None,
            pgid: None,
            required: true,
            process_group: false,
        });
        manager.write_state(&dead_state).expect("write dead state");
        assert_eq!(
            manager.status().expect("status").health,
            SessionHealth::Dead
        );

        let result = manager
            .ensure(EnsureOptions {
                client_connected: true,
                ..EnsureOptions::default()
            })
            .expect("ensure should restart dead component");
        assert!(!result.reused_existing);
        assert_eq!(result.status.health, SessionHealth::Healthy);
        let state = result.status.state.expect("state after restart");
        assert_eq!(state.active_clients, 1);
        assert_eq!(state.components.len(), 1);
        assert_ne!(state.components[0].pid, 999_999);
        manager.stop().expect("cleanup restarted component");
    }

    #[test]
    fn cleanup_idle_stops_expired_idle_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = SesmanConfig::default();
        config.state_dir = temp.path().join("state");
        config.log_dir = temp.path().join("logs");
        config.xdg_runtime_dir = temp.path().join("runtime");
        config.idle_timeout_ms = 1;
        fs::create_dir_all(&config.state_dir).expect("state dir");

        let manager = SessionManager::new(config.clone());
        let mut state = SessionState::new(&config);
        state.active_clients = 0;
        state.last_disconnected_at = Some(Utc::now() - chrono::Duration::milliseconds(50));
        manager.write_state(&state).expect("write state");

        assert!(manager.cleanup_idle().expect("cleanup idle"));
        assert_eq!(
            manager.status().expect("status").health,
            SessionHealth::Missing
        );
    }

    #[test]
    fn file_lock_blocks_concurrent_registry_access() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("default.lock");
        let first = FileLock::acquire(&lock_path).expect("first lock should succeed");
        let second = FileLock::acquire(&lock_path);
        assert!(
            second.is_err(),
            "second lock should fail while first is held"
        );
        drop(first);
        FileLock::acquire(&lock_path).expect("lock should be reusable after drop");
    }

    #[test]
    fn ini_config_rejects_relative_runtime_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("sesman.ini");
        std::fs::write(
            &path,
            "[globals]\nuser=alice\nxdg_runtime_dir=relative\nstate_dir=state\nlog_dir=logs\n",
        )
        .expect("write config");
        let error = SesmanConfig::load(Some(&path)).unwrap_err().to_string();
        assert!(error.contains("must be absolute"), "{error}");
    }

    #[test]
    fn ini_config_can_override_components() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("sesman.ini");
        std::fs::write(
            &path,
            r#"
[globals]
session_name=test
user=alice
xdg_runtime_dir=/tmp/runtime-alice
state_dir=/tmp/state
log_dir=/tmp/logs

[component:dummy]
command=/bin/sleep
args=[60]
readiness=process_alive
"#,
        )
        .expect("write config");

        let config = SesmanConfig::load(Some(&path)).expect("test INI should parse");
        assert_eq!(config.session_name, "test");
        assert_eq!(config.components.len(), 1);
        assert_eq!(config.components[0].command, "/bin/sleep");
    }
}
