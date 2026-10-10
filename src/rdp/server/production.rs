//! Production RDP listener and per-user connection binding.

#[cfg(feature = "portal-generic")]
use crate::rdp::session::supervision::{SessionStatusEvent, SessionSupervisor};

use std::{
    fs,
    future::Future,
    io,
    net::SocketAddr,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use ironrdp_pdu::rdp::capability_sets::server_codecs_capabilities;
use ironrdp_server::{
    BoundConnection, ConnectionBinder, Credentials, DesktopSize, DisplayUpdate, KeyboardEvent,
    MouseEvent, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler,
};
use nix::sys::socket::{getsockopt, sockopt};
use tokio::sync::{Mutex, Notify, RwLock, broadcast};
use tracing::{info, warn};

trait PeerAwareValidator: Send + Sync {
    fn set_peer_ip(&self, ip: std::net::IpAddr);
    fn prune_stale_entries(&self);
}

impl PeerAwareValidator for crate::security::PamValidator {
    fn set_peer_ip(&self, ip: std::net::IpAddr) {
        self.set_peer_ip(ip);
    }
    fn prune_stale_entries(&self) {
        self.prune_stale_entries();
    }
}

impl PeerAwareValidator for crate::security::StaticPasswordValidator {
    fn set_peer_ip(&self, ip: std::net::IpAddr) {
        self.set_peer_ip(ip);
    }
    fn prune_stale_entries(&self) {
        self.prune_stale_entries();
    }
}

use crate::{
    config::Config,
    rdp::{
        channels::{
            audio::{AudioTarget, WrdpSoundFactory},
            clipboard::{ClipboardOrchestrator, ClipboardOrchestratorConfig, WrdpCliprdrFactory},
        },
        server::{
            DisplayChannelHandler, EgfxChannelFactory, InputChannelHandler,
            display_handler::ManagedCompositorControl,
        },
        session::supervision::{OverallStatus, SessionStatus, SessionStatusSubscriber},
    },
};

fn effective_codec_policy(
    configured: crate::rdp::server::EgfxCodecPolicy,
    hardware_enabled: bool,
) -> crate::rdp::server::EgfxCodecPolicy {
    if configured == crate::rdp::server::EgfxCodecPolicy::Auto && hardware_enabled {
        crate::rdp::server::EgfxCodecPolicy::Avc420
    } else {
        configured
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProductionChannelPolicy {
    clipboard: bool,
    input: bool,
}

fn production_channel_policy(config: &Config) -> ProductionChannelPolicy {
    ProductionChannelPolicy {
        clipboard: config.clipboard.enabled && !config.server.view_only,
        input: !config.server.view_only,
    }
}

const PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACCEPT_RETRY_DELAY: Duration = Duration::from_secs(1);
const SUPERVISION_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const PRODUCTION_IDLE_CLEANUP_INTERVAL: Duration = Duration::from_mins(1);
const PRODUCTION_DEFAULT_DESKTOP_SIZE: DesktopSize = DesktopSize {
    width: 1920,
    height: 1080,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionTimeoutPhase {
    PreAuth,
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectedConnectionTimeout {
    deadline: tokio::time::Instant,
    phase: ConnectionTimeoutPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionCancellation {
    Timeout(ConnectionTimeoutPhase),
    SessionInvalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellationDecision {
    Cancel,
    DeferUntilAdmissionFinishes,
}

fn cancellation_decision(
    admission_started: bool,
    admission_finished: bool,
) -> CancellationDecision {
    if admission_started && !admission_finished {
        CancellationDecision::DeferUntilAdmissionFinishes
    } else {
        CancellationDecision::Cancel
    }
}

struct AdmissionFinishedGuard {
    finished: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl AdmissionFinishedGuard {
    fn new(finished: Arc<AtomicBool>, notify: Arc<Notify>) -> Self {
        Self { finished, notify }
    }
}

impl Drop for AdmissionFinishedGuard {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        self.notify.notify_one();
    }
}

fn selected_connection_timeout(
    started: tokio::time::Instant,
    admission_started: bool,
    session_timeout: Option<Duration>,
) -> Option<SelectedConnectionTimeout> {
    let session = session_timeout.map(|timeout| SelectedConnectionTimeout {
        deadline: started + timeout,
        phase: ConnectionTimeoutPhase::Session,
    });
    if admission_started {
        return session;
    }

    let pre_auth = SelectedConnectionTimeout {
        deadline: started + PRE_AUTH_TIMEOUT,
        phase: ConnectionTimeoutPhase::PreAuth,
    };
    match session {
        Some(session) if session.deadline <= pre_auth.deadline => Some(session),
        _ => Some(pre_auth),
    }
}

#[expect(
    clippy::expect_used,
    reason = "internal invariant established during construction or by guarded control flow"
)]
async fn run_with_connection_timeouts<F>(
    connection: F,
    admission_started: Arc<AtomicBool>,
    admission_started_notify: Arc<Notify>,
    admission_finished: Arc<AtomicBool>,
    admission_finished_notify: Arc<Notify>,
    connection_invalid: Arc<Notify>,
    session_timeout: Option<Duration>,
) -> std::result::Result<F::Output, ConnectionCancellation>
where
    F: Future,
{
    let started = tokio::time::Instant::now();
    let mut pending_cancellation = None;
    tokio::pin!(connection);

    loop {
        let admitted = admission_started.load(Ordering::Acquire);
        let finished = admission_finished.load(Ordering::Acquire);

        if let Some(cancellation) = pending_cancellation
            && cancellation_decision(admitted, finished) == CancellationDecision::Cancel
        {
            // Poll once after the binder's guard completed. This lets the same
            // connection future consume the binder result before cancellation,
            // so any committed resources are visible to common cleanup.
            tokio::select! {
                biased;
                result = &mut connection => return Ok(result),
                () = std::future::ready(()) => return Err(cancellation),
            }
        }

        let timeout = pending_cancellation
            .is_none()
            .then(|| selected_connection_timeout(started, admitted, session_timeout))
            .flatten();
        let timeout_wait = async {
            match timeout {
                Some(timeout) => tokio::time::sleep_until(timeout.deadline).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            biased;
            result = &mut connection => return Ok(result),
            () = admission_started_notify.notified(), if !admitted => {}
            () = admission_finished_notify.notified(),
                if pending_cancellation.is_some() && !finished => {}
            () = connection_invalid.notified(), if pending_cancellation.is_none() => {
                let cancellation = ConnectionCancellation::SessionInvalid;
                if cancellation_decision(
                    admission_started.load(Ordering::Acquire),
                    admission_finished.load(Ordering::Acquire),
                ) == CancellationDecision::DeferUntilAdmissionFinishes
                {
                    pending_cancellation = Some(cancellation);
                } else {
                    return Err(cancellation);
                }
            }
            () = timeout_wait, if timeout.is_some() => {
                let cancellation = ConnectionCancellation::Timeout(
                    timeout.expect("timeout branch requires a deadline").phase,
                );
                if cancellation_decision(
                    admission_started.load(Ordering::Acquire),
                    admission_finished.load(Ordering::Acquire),
                ) == CancellationDecision::DeferUntilAdmissionFinishes
                {
                    pending_cancellation = Some(cancellation);
                } else {
                    return Err(cancellation);
                }
            }
        }
    }
}

fn is_recoverable_accept_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
    ) {
        return true;
    }

    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

fn accept_retry_delay(consecutive_errors: u32) -> Duration {
    let shift = consecutive_errors.saturating_sub(1).min(7);
    Duration::from_millis((10_u64 << shift).min(MAX_ACCEPT_RETRY_DELAY.as_millis() as u64))
}

pub async fn run(config: Config) -> Result<()> {
    info!("starting wrdp production single-daemon listener");

    tokio::task::spawn_blocking(crate::sesman::reconcile_production_sessions)
        .await
        .context("production sesman reconciliation task panicked")??;
    info!("production sesman restart reconciliation complete");
    spawn_production_idle_cleanup();

    let tls_config = crate::security::TlsConfig::from_files_with_options(
        &config.security.cert_path,
        &config.security.key_path,
        config.security.require_tls_13,
    )
    .context("failed to load TLS certificates")?;
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config.server_config());

    let listen_addr: SocketAddr = config
        .server
        .listen_addr
        .parse()
        .context("invalid server.listen_addr")?;
    // Keep RemoteFX advertised as the compatible bitmap codec baseline. Some
    // Microsoft mobile clients only open the rdpgfx dynamic channel when the
    // server also advertises a graphics codec in the core capability exchange.
    // EGFX/AVC remains the preferred transport once the DVC is negotiated.
    let codecs = server_codecs_capabilities(&["remotefx"])
        .map_err(|e| anyhow::anyhow!("failed to create bitmap codec capabilities: {e}"))?;

    let active_bound_user = Arc::new(Mutex::new(None));
    let active_connection = Arc::new(Mutex::new(None));
    let active_connection_cancel: Arc<StdMutex<Option<Arc<Notify>>>> =
        Arc::new(StdMutex::new(None));
    let fatal_shutdown_error: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
    let audio_target = AudioTarget::default();

    let configured_codec_policy = crate::rdp::server::EgfxCodecPolicy::parse(&config.egfx.codec)
        .context("invalid egfx.codec")?;
    // The VA-API implementation produces AVC420. Selecting AVC444 under
    // `auto` would negotiate an incompatible sender and then require a software
    // OpenH264 module that may not be installed. Keep auto aligned with the
    // enabled hardware encoder; operators can still explicitly request AVC444.
    let codec_policy =
        effective_codec_policy(configured_codec_policy, config.hardware_encoding.enabled);
    let (gfx_factory, gfx_server_handle, gfx_handler_state) =
        if config.egfx.enabled && codec_policy != crate::rdp::server::EgfxCodecPolicy::Bitmap {
            let compression_mode = match config.egfx.zgfx_compression.to_lowercase().as_str() {
                "auto" => ironrdp_graphics::zgfx::CompressionMode::Auto,
                "always" => ironrdp_graphics::zgfx::CompressionMode::Always,
                _ => ironrdp_graphics::zgfx::CompressionMode::Never,
            };
            let gfx_width = std::env::var("WRDP_DEFAULT_WIDTH")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1920);
            let gfx_height = std::env::var("WRDP_DEFAULT_HEIGHT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1080);
            let gfx_factory = EgfxChannelFactory::with_config(
                gfx_width,
                gfx_height,
                false,
                config.egfx.max_frames_in_flight,
                compression_mode,
            )
            .with_codec_policy(codec_policy);
            let gfx_handler_state = gfx_factory.handler_state();
            let gfx_server_handle = gfx_factory.server_handle();
            (
                Some(Box::new(gfx_factory) as Box<dyn ironrdp_server::GfxServerFactory>),
                Some(gfx_server_handle),
                Some(gfx_handler_state),
            )
        } else {
            (None, None, None)
        };

    let channel_policy = production_channel_policy(&config);
    let clipboard_manager = if channel_policy.clipboard {
        let all_allowed = config.clipboard.allowed_types.is_empty();
        let has_type = |patterns: &[&str]| {
            all_allowed
                || config
                    .clipboard
                    .allowed_types
                    .iter()
                    .any(|t| patterns.iter().any(|p| t.contains(p)))
        };
        let clipboard_config = ClipboardOrchestratorConfig {
            max_data_size: config.clipboard.max_size,
            enable_images: has_type(&["image/"]),
            enable_files: has_type(&["uri-list", "file", "x-special"]),
            enable_html: has_type(&["text/html"]),
            enable_rtf: has_type(&["rtf"]),
            rate_limit_ms: config.clipboard.rate_limit_ms,
            ..ClipboardOrchestratorConfig::default()
        };
        match ClipboardOrchestrator::new(clipboard_config).await {
            Ok(manager) => Some(Arc::new(Mutex::new(manager))),
            Err(e) => {
                warn!(
                    "production clipboard initialization failed, continuing without CLIPRDR: {e:#}"
                );
                None
            }
        }
    } else {
        None
    };
    let cliprdr_factory = clipboard_manager.as_ref().map(|mgr| {
        Box::new(WrdpCliprdrFactory::new(Arc::clone(mgr)))
            as Box<dyn ironrdp_server::CliprdrServerFactory>
    });

    let mut rdp_server = RdpServer::builder()
        .with_addr(listen_addr)
        .with_tls(tls_acceptor)
        .with_no_input()
        .with_display_handler(ProductionPlaceholderDisplay)
        .with_bitmap_codecs(codecs)
        // Preserve the desktop requested during initial capability exchange.
        // Without this, a later correction emits an avoidable Deactivate-All
        // cycle before the first graphics surface is usable.
        .with_honor_client_desktop_size(Some(PRODUCTION_DEFAULT_DESKTOP_SIZE))
        .with_cliprdr_factory(cliprdr_factory)
        .with_gfx_factory(gfx_factory)
        .with_sound_factory(Some(Box::new(WrdpSoundFactory::new(audio_target.clone()))))
        .build();

    let mut peer_validator: Option<Arc<dyn PeerAwareValidator>> = None;
    if config.security.auth_method == "pam" {
        let validator = Arc::new(crate::security::PamValidator::new_with_allowed_username(
            Some("wrdp".to_string()),
            config.security.allowed_username.clone(),
        ));
        rdp_server.set_credential_validator(Some(validator.clone()));
        peer_validator = Some(validator.clone());
        info!("production PAM credential validator attached (service=wrdp)");
    }

    match config.security.auth_method.as_str() {
        "pam" => {}
        "password" => {
            let validator = Arc::new(crate::security::StaticPasswordValidator::new_hashes(
                config.security.password_credentials.clone(),
            )?);
            rdp_server.set_credential_validator(Some(validator.clone()));
            peer_validator = Some(validator);
            info!("production static password credential validator attached");
        }
        "none" => {
            anyhow::bail!(
                "auth_method=none is not supported by the production wrdp daemon; use pam or password"
            );
        }
        other => {
            anyhow::bail!("unsupported auth_method in production wrdp mode: {other}");
        }
    }

    let admission_started = Arc::new(AtomicBool::new(false));
    let admission_started_notify = Arc::new(Notify::new());
    let admission_finished = Arc::new(AtomicBool::new(false));
    let admission_finished_notify = Arc::new(Notify::new());
    rdp_server.set_connection_binder(Some(Arc::new(ProductionSesmanBinder {
        active_bound_user: Arc::clone(&active_bound_user),
        active_connection: Arc::clone(&active_connection),
        active_connection_cancel: Arc::clone(&active_connection_cancel),
        fatal_shutdown_error: Arc::clone(&fatal_shutdown_error),
        audio_target: audio_target.clone(),
        config: Arc::new(config.clone()),
        server_event_tx: rdp_server.event_sender().clone(),
        clipboard_manager: clipboard_manager.clone(),
        gfx_server_handle,
        gfx_handler_state,
        admission_started: Arc::clone(&admission_started),
        admission_started_notify: Arc::clone(&admission_started_notify),
        admission_finished: Arc::clone(&admission_finished),
        admission_finished_notify: Arc::clone(&admission_finished_notify),
    })));

    let session_timeout = (config.server.session_timeout != 0)
        .then(|| Duration::from_secs(config.server.session_timeout));
    let listener = production_listener(listen_addr)?;
    info!(
        "wrdp production listener ready on {}",
        listener.local_addr()?
    );

    let mut consecutive_accept_errors = 0_u32;
    loop {
        let (stream, peer) = loop {
            match listener.accept().await {
                Ok(accepted) => {
                    consecutive_accept_errors = 0;
                    break accepted;
                }
                Err(error) if is_recoverable_accept_error(&error) => {
                    consecutive_accept_errors = consecutive_accept_errors.saturating_add(1);
                    let delay = accept_retry_delay(consecutive_accept_errors);
                    warn!(
                        error = %error,
                        retry_delay_ms = delay.as_millis(),
                        "recoverable RDP listener accept error; retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error).context("failed to accept RDP client"),
            }
        };

        info!("accepted RDP client connection");
        if let Some(validator) = peer_validator.as_ref() {
            validator.set_peer_ip(peer.ip());
            validator.prune_stale_entries();
        }

        admission_finished.store(false, Ordering::Release);
        admission_started.store(false, Ordering::Release);
        let connection_invalid = Arc::new(Notify::new());
        *active_connection_cancel
            .lock()
            .map_err(|_| anyhow::anyhow!("active connection cancellation lock poisoned"))? =
            Some(Arc::clone(&connection_invalid));

        let connection = rdp_server.run_connection(stream);
        let result = match Box::pin(run_with_connection_timeouts(
            connection,
            Arc::clone(&admission_started),
            Arc::clone(&admission_started_notify),
            Arc::clone(&admission_finished),
            Arc::clone(&admission_finished_notify),
            Arc::clone(&connection_invalid),
            session_timeout,
        ))
        .await
        {
            Ok(result) => result.map_err(anyhow::Error::from),
            Err(ConnectionCancellation::Timeout(ConnectionTimeoutPhase::PreAuth)) => Err(
                anyhow::anyhow!("RDP pre-authentication timed out after 30 seconds"),
            ),
            Err(ConnectionCancellation::Timeout(ConnectionTimeoutPhase::Session)) => {
                Err(anyhow::anyhow!(
                    "RDP session timed out after {} seconds",
                    config.server.session_timeout
                ))
            }
            Err(ConnectionCancellation::SessionInvalid) => {
                Err(anyhow::anyhow!("production session became invalid"))
            }
        };
        {
            let mut active_cancel = active_connection_cancel
                .lock()
                .map_err(|_| anyhow::anyhow!("active connection cancellation lock poisoned"))?;
            if active_cancel
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &connection_invalid))
            {
                *active_cancel = None;
            }
        }
        if let Err(e) = &result {
            warn!("RDP client connection ended with error: {e:#}");
        } else {
            info!("RDP client connection ended cleanly");
        }
        if let Some(error) = fatal_shutdown_error
            .lock()
            .map_err(|_| anyhow::anyhow!("fatal shutdown error latch poisoned"))?
            .take()
        {
            anyhow::bail!("fatal production connection shutdown failure: {error}");
        }

        // Keep cleanup common to clean disconnects, protocol failures, and
        // connection futures dropped by timeout or invalid-session cancellation.
        if let Some(connection) = active_connection.lock().await.take() {
            connection
                .shutdown()
                .await
                .context("failed to shut down production connection resources")?;
        }
        audio_target.clear();
        if let Some(manager) = clipboard_manager.as_ref() {
            manager.lock().await.clear_connection_state().await;
        }
        if let Some(user) = active_bound_user.lock().await.take() {
            record_production_disconnect(user).await;
        }
    }
}

/// Run daemon-wide cleanup independently of individual RDP connections.
fn spawn_production_idle_cleanup() {
    tokio::spawn(async {
        loop {
            tokio::time::sleep(PRODUCTION_IDLE_CLEANUP_INTERVAL).await;
            match tokio::task::spawn_blocking(crate::sesman::cleanup_production_sessions).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!("periodic production idle cleanup failed: {error:#}"),
                Err(error) => warn!("periodic production idle cleanup task panicked: {error:#}"),
            }
        }
    });
}

fn production_listener(listen_addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    if let Some(listener) = adopt_systemd_listener(listen_addr)? {
        return Ok(listener);
    }
    let std_listener = std::net::TcpListener::bind(listen_addr)
        .with_context(|| format!("failed to bind {listen_addr}"))?;
    std_listener
        .set_nonblocking(true)
        .context("failed to set listener nonblocking")?;
    tokio::net::TcpListener::from_std(std_listener).context("failed to create tokio listener")
}

fn adopt_systemd_listener(expected_addr: SocketAddr) -> Result<Option<tokio::net::TcpListener>> {
    let mut listenfd = listenfd::ListenFd::from_env();
    let Some(std_listener) = listenfd
        .take_tcp_listener(0)
        .context("failed to inspect systemd socket activation fd")?
    else {
        return Ok(None);
    };

    validate_systemd_listener_accepts_connections(&std_listener)?;
    std_listener
        .set_nonblocking(true)
        .context("failed to set systemd listener nonblocking")?;
    let actual_addr = std_listener
        .local_addr()
        .context("systemd fd is not a TCP listener")?;
    validate_systemd_listener_addr(expected_addr, actual_addr)?;
    info!("adopted systemd listener {}", actual_addr);
    tokio::net::TcpListener::from_std(std_listener)
        .map(Some)
        .context("failed to adopt systemd listener")
}

fn validate_systemd_listener_accepts_connections<F: std::os::fd::AsFd>(socket: &F) -> Result<()> {
    if !getsockopt(socket, sockopt::AcceptConn)
        .context("failed to query SO_ACCEPTCONN on systemd socket activation fd")?
    {
        anyhow::bail!("systemd socket activation fd is not accepting connections");
    }
    Ok(())
}

fn validate_systemd_listener_addr(expected: SocketAddr, actual: SocketAddr) -> Result<()> {
    if actual != expected {
        anyhow::bail!(
            "systemd listener address mismatch: expected exact configured address {expected}, got {actual} (address and IP family must match)"
        );
    }
    Ok(())
}

async fn record_production_disconnect(user: String) {
    match tokio::task::spawn_blocking(move || {
        let config = crate::sesman::SesmanConfig::for_user(&user)?;
        let idle_timeout_ms = config.idle_timeout_ms;
        let manager = crate::sesman::SessionManager::new(config);
        let status = manager.unbind_single_client()?;
        let stopped_idle = manager.cleanup_idle()?;
        Ok::<_, anyhow::Error>((user, status, stopped_idle, idle_timeout_ms))
    })
    .await
    {
        Ok(Ok((user, status, stopped_idle, idle_timeout_ms))) => {
            info!(
                "recorded wrdp sesman disconnect for user '{}' (health={:?}, stopped_idle={})",
                user, status.health, stopped_idle
            );
            if !stopped_idle && idle_timeout_ms > 0 {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(idle_timeout_ms)).await;
                    let cleanup_user = user.clone();
                    match tokio::task::spawn_blocking(move || {
                        let config = crate::sesman::SesmanConfig::for_user(&cleanup_user)?;
                        crate::sesman::SessionManager::new(config).cleanup_idle()
                    })
                    .await
                    {
                        Ok(Ok(true)) => info!("stopped idle wrdp user session"),
                        Ok(Ok(false)) => {}
                        Ok(Err(e)) => warn!("failed delayed idle user-session cleanup: {e:#}"),
                        Err(e) => {
                            warn!("delayed idle user-session cleanup task panicked: {e:#}");
                        }
                    }
                });
            }
        }
        Ok(Err(e)) => warn!("failed to record wrdp sesman disconnect: {e:#}"),
        Err(e) => warn!("wrdp sesman disconnect task panicked: {e:#}"),
    }
}

/// Roll back the session state after `ensure` succeeded but RDP binding did not.
///
/// A reused session remains available for its configured idle period.  A newly
/// created session has no successful client owner and is stopped immediately.
async fn rollback_ensured_production_session(user: String, reused_existing: bool) {
    if reused_existing {
        match tokio::task::spawn_blocking(move || {
            let config = crate::sesman::SesmanConfig::for_user(&user)?;
            let idle_timeout_ms = config.idle_timeout_ms;
            let status = crate::sesman::SessionManager::new(config).unbind_single_client()?;
            Ok::<_, anyhow::Error>((user, status, idle_timeout_ms))
        })
        .await
        {
            Ok(Ok((user, status, idle_timeout_ms))) => {
                info!(
                    "left reused production session idle after failed RDP bind (health={:?})",
                    status.health
                );
                if idle_timeout_ms > 0 {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(idle_timeout_ms)).await;
                        let cleanup_user = user.clone();
                        match tokio::task::spawn_blocking(move || {
                            let config = crate::sesman::SesmanConfig::for_user(&cleanup_user)?;
                            crate::sesman::SessionManager::new(config).cleanup_idle()
                        })
                        .await
                        {
                            Ok(Ok(true)) => {
                                info!("stopped idle reused production session after failed bind");
                            }
                            Ok(Ok(false)) => {}
                            Ok(Err(error)) => warn!(
                                "failed delayed cleanup of reused production session: {error:#}"
                            ),
                            Err(error) => warn!(
                                "delayed reused production-session cleanup task panicked: {error:#}"
                            ),
                        }
                    });
                }
            }
            Ok(Err(error)) => warn!("failed to mark reused production session idle: {error:#}"),
            Err(error) => warn!("reused production-session rollback task panicked: {error:#}"),
        }
        return;
    }

    match tokio::task::spawn_blocking(move || {
        let config = crate::sesman::SesmanConfig::for_user(&user)?;
        crate::sesman::SessionManager::new(config).stop()
    })
    .await
    {
        Ok(Ok(status)) => info!(
            "stopped newly created production session after failed RDP bind (health={:?})",
            status.health
        ),
        Ok(Err(error)) => warn!("failed to stop newly created production session: {error:#}"),
        Err(error) => warn!("new production-session rollback task panicked: {error:#}"),
    }
}

#[cfg_attr(
    not(feature = "portal-generic"),
    expect(
        dead_code,
        reason = "retained shared API when the runtime backend feature is disabled"
    )
)]
async fn watch_production_session_health(
    mut subscriber: SessionStatusSubscriber,
    connection_invalid: Arc<Notify>,
) {
    while subscriber.changed().await.is_ok() {
        let status = subscriber.current();
        let Some(reason) = invalid_session_reason(&status) else {
            continue;
        };

        warn!(%reason, "production session became invalid; disconnecting RDP client");
        connection_invalid.notify_one();
        break;
    }
}

fn invalid_session_reason(status: &SessionStatus) -> Option<String> {
    (status.overall == OverallStatus::Invalid).then(|| {
        format!(
            "production session invalid: session={}, graphics={}, input={}, clipboard={}",
            status.session, status.graphics, status.input, status.clipboard
        )
    })
}

fn retain_fatal_shutdown_error(latch: &StdMutex<Option<String>>, error: &anyhow::Error) {
    let mut latch = latch
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if latch.is_none() {
        *latch = Some(format!("{error:#}"));
    }
}

async fn await_supervision_task(name: &'static str, mut handle: tokio::task::JoinHandle<()>) {
    match tokio::time::timeout(SUPERVISION_TASK_SHUTDOWN_TIMEOUT, &mut handle).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.is_cancelled() => {}
        Ok(Err(error)) => warn!(task = name, %error, "production supervision task failed"),
        Err(_) => {
            warn!(
                task = name,
                "production supervision task did not stop; aborting"
            );
            handle.abort();
            let _ = handle.await;
        }
    }
}

struct ProductionConnectionResources {
    display: Arc<DisplayChannelHandler>,
    session_handle: Arc<dyn crate::rdp::session::SessionHandle>,
    connection_invalid: Arc<Notify>,
    supervision_shutdown: broadcast::Sender<()>,
    monitor_handle: tokio::task::JoinHandle<()>,
    watcher_handle: tokio::task::JoinHandle<()>,
}

impl ProductionConnectionResources {
    async fn shutdown(self) -> Result<()> {
        let Self {
            display,
            session_handle,
            connection_invalid: _connection_invalid,
            supervision_shutdown,
            monitor_handle,
            watcher_handle,
        } = self;

        // The session backend reports SessionClosed during intentional teardown.
        // Cancel the invalid-health watcher first; its generation token must not
        // request cancellation after this connection starts shutting down.
        watcher_handle.abort();
        display.release_connection_resources().await;
        let shutdown_result = session_handle.shutdown().await;
        let _ = supervision_shutdown.send(());

        tokio::join!(
            await_supervision_task("monitor", monitor_handle),
            await_supervision_task("watcher", watcher_handle),
        );

        shutdown_result.context("session backend shutdown failed")
    }
}

struct ProductionSesmanBinder {
    active_bound_user: Arc<Mutex<Option<String>>>,
    active_connection: Arc<Mutex<Option<ProductionConnectionResources>>>,
    active_connection_cancel: Arc<StdMutex<Option<Arc<Notify>>>>,
    fatal_shutdown_error: Arc<StdMutex<Option<String>>>,
    audio_target: AudioTarget,
    config: Arc<Config>,
    server_event_tx: tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>,
    clipboard_manager: Option<Arc<Mutex<ClipboardOrchestrator>>>,
    gfx_server_handle: Option<Arc<RwLock<Option<ironrdp_server::GfxServerHandle>>>>,
    gfx_handler_state: Option<Arc<RwLock<Option<crate::rdp::server::HandlerState>>>>,
    admission_started: Arc<AtomicBool>,
    admission_started_notify: Arc<Notify>,
    admission_finished: Arc<AtomicBool>,
    admission_finished_notify: Arc<Notify>,
}

#[async_trait::async_trait]
impl ConnectionBinder for ProductionSesmanBinder {
    async fn bind_connection(
        &self,
        credentials: &Credentials,
        desktop_size: DesktopSize,
    ) -> ironrdp_server::ServerResult<BoundConnection> {
        self.bind_connection_inner(credentials, desktop_size)
            .await
            .map_err(|error| {
                ironrdp_server::ServerError::new(
                    "session binding",
                    ironrdp_server::ServerErrorKind::Custom,
                )
                .with_source(std::io::Error::other(error))
            })
    }
}

impl ProductionSesmanBinder {
    async fn bind_connection_inner(
        &self,
        credentials: &Credentials,
        desktop_size: DesktopSize,
    ) -> Result<BoundConnection> {
        self.admission_finished.store(false, Ordering::Release);
        self.admission_started.store(true, Ordering::Release);
        let _admission_finished_guard = AdmissionFinishedGuard::new(
            Arc::clone(&self.admission_finished),
            Arc::clone(&self.admission_finished_notify),
        );
        self.admission_started_notify.notify_one();

        let connection_invalid = self
            .active_connection_cancel
            .lock()
            .map_err(|_| anyhow::anyhow!("active connection cancellation lock poisoned"))?
            .clone()
            .context("no cancellation generation for active RDP connection")?;

        // The initial desktop size is part of session creation, not a later
        // display-control request.  Reject it before `ensure`: after a managed
        // compositor is started the binder cannot renegotiate RDP capabilities.
        let desktop_size = DisplayChannelHandler::validate_geometry_policy(
            &self.config,
            desktop_size,
            Some(PRODUCTION_DEFAULT_DESKTOP_SIZE),
        )
        .context("initial RDP desktop geometry violates production policy")?;

        let username = normalize_local_account_name(&credentials.username);
        let active_bound_user = Arc::clone(&self.active_bound_user);
        let active_connection = Arc::clone(&self.active_connection);
        let audio_target = self.audio_target.clone();
        let fatal_shutdown_error = Arc::clone(&self.fatal_shutdown_error);
        let config = Arc::clone(&self.config);
        let server_event_tx = self.server_event_tx.clone();
        let clipboard_manager = self.clipboard_manager.clone();
        let gfx_server_handle = self.gfx_server_handle.clone();
        let gfx_handler_state = self.gfx_handler_state.clone();

        let ensured_user = username.clone();
        let ensure_result = tokio::task::spawn_blocking(move || {
            let config = crate::sesman::SesmanConfig::for_user(&ensured_user)?;
            let manager = crate::sesman::SessionManager::new(config);
            manager.ensure(crate::sesman::EnsureOptions {
                force_restart: false,
                requested_size: Some(crate::sesman::SessionSize {
                    width: u32::from(desktop_size.width),
                    height: u32::from(desktop_size.height),
                }),
                client_peer: None,
                client_connected: false,
            })
        })
        .await
        .context("sesman ensure task panicked")??;
        let reused_existing = ensure_result.reused_existing;

        let runtime_dir = match ensure_result.status.state.as_ref() {
            Some(state) => state.xdg_runtime_dir.clone(),
            None => {
                rollback_ensured_production_session(username.clone(), reused_existing).await;
                anyhow::bail!("sesman ensure completed without persisted session state");
            }
        };
        let wayland_socket = runtime_dir.join("wayland-0");

        let identity_user = username.clone();
        let target_user =
            match tokio::task::spawn_blocking(move || resolve_target_user_identity(&identity_user))
                .await
            {
                Ok(Ok(identity)) => identity,
                Ok(Err(error)) => {
                    rollback_ensured_production_session(username.clone(), reused_existing).await;
                    return Err(error).context("failed to resolve managed session identity");
                }
                Err(error) => {
                    rollback_ensured_production_session(username.clone(), reused_existing).await;
                    return Err(error).context("managed session identity task panicked");
                }
            };

        // A reused compositor can retain an earlier mode.  Apply and verify the
        // negotiated geometry before capture is created so capture, input, and
        // RDP all use the same size.
        if let Err(error) =
            resize_production_compositor(&wayland_socket, desktop_size, &target_user).await
        {
            rollback_ensured_production_session(username.clone(), reused_existing).await;
            return Err(error)
                .context("failed to apply negotiated desktop size to managed compositor");
        }

        let (bound, resources) = match build_production_portal_generic_connection(
            &username,
            wayland_socket,
            &target_user,
            config,
            server_event_tx,
            connection_invalid,
            clipboard_manager,
            gfx_server_handle,
            gfx_handler_state,
            desktop_size,
            Arc::clone(&fatal_shutdown_error),
        )
        .await
        {
            Ok(bound) => bound,
            Err(error) => {
                // The builder explicitly shuts down its portal session before
                // returning an error, including any partial local resources.
                rollback_ensured_production_session(username.clone(), reused_existing).await;
                return Err(error).with_context(|| {
                    format!("failed to bind production RDP handlers for user '{username}'")
                });
            }
        };

        let bound_user = username.clone();
        let bind_result = tokio::task::spawn_blocking(move || {
            let config = crate::sesman::SesmanConfig::for_user(&bound_user)?;
            crate::sesman::SessionManager::new(config).bind_single_client()
        })
        .await;
        match bind_result {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                let bind_error = error.context("failed to record bound production client");
                if let Err(shutdown_error) = resources.shutdown().await {
                    let shutdown_error = shutdown_error.context(format!(
                        "production resource shutdown failed before rollback after client-bind failure: {bind_error:#}"
                    ));
                    retain_fatal_shutdown_error(&fatal_shutdown_error, &shutdown_error);
                    return Err(shutdown_error);
                }
                rollback_ensured_production_session(username.clone(), reused_existing).await;
                return Err(bind_error);
            }
            Err(error) => {
                let bind_error =
                    anyhow::Error::new(error).context("sesman client-bind task panicked");
                if let Err(shutdown_error) = resources.shutdown().await {
                    let shutdown_error = shutdown_error.context(format!(
                        "production resource shutdown failed before rollback after client-bind failure: {bind_error:#}"
                    ));
                    retain_fatal_shutdown_error(&fatal_shutdown_error, &shutdown_error);
                    return Err(shutdown_error);
                }
                rollback_ensured_production_session(username.clone(), reused_existing).await;
                return Err(bind_error);
            }
        }

        audio_target.set_runtime_dir(runtime_dir);
        *active_bound_user.lock().await = Some(username.clone());
        *active_connection.lock().await = Some(resources);
        info!(
            "post-auth production sesman bind for user '{}' complete (reused_existing={}, health={:?})",
            username, reused_existing, ensure_result.status.health
        );

        Ok(bound)
    }
}

#[derive(Debug, Clone)]
struct TargetUserIdentity {
    uid: u32,
    gid: u32,
    groups: String,
}

#[cfg_attr(
    not(feature = "portal-generic"),
    expect(
        dead_code,
        reason = "retained shared API when the runtime backend feature is disabled"
    )
)]
fn managed_compositor_control(
    socket: PathBuf,
    target_user: &TargetUserIdentity,
) -> ManagedCompositorControl {
    let output = std::env::var("WRDP_HEADLESS_OUTPUT").unwrap_or_else(|_| "HEADLESS-1".to_string());
    managed_compositor_control_with_output(socket, target_user, output)
}

fn managed_compositor_control_with_output(
    socket: PathBuf,
    target_user: &TargetUserIdentity,
    output: String,
) -> ManagedCompositorControl {
    ManagedCompositorControl::new(
        socket,
        target_user.uid.to_string(),
        target_user.gid.to_string(),
        target_user.groups.clone(),
        output,
    )
}

fn resolve_target_user_identity(username: &str) -> Result<TargetUserIdentity> {
    let user = nix::unistd::User::from_name(username)
        .context("failed to resolve managed session account")?
        .with_context(|| format!("unknown managed session account: {username}"))?;
    let uid = user.uid.as_raw();
    let gid = user.gid.as_raw();

    // Keep this consistent with sesman's component launcher: setpriv receives
    // explicit numeric supplementary groups instead of inheriting root's groups.
    let output = std::process::Command::new("/usr/bin/id")
        .args(["-G", username])
        .output()
        .with_context(|| format!("failed to resolve supplementary groups for {username}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to resolve supplementary groups for {username}: id -G exited {}",
            output.status
        );
    }
    let mut groups = output
        .stdout
        .split(u8::is_ascii_whitespace)
        .filter(|group| !group.is_empty())
        .map(|group| {
            std::str::from_utf8(group)
                .context("id -G output was not UTF-8")?
                .parse::<u32>()
                .context("id -G returned a nonnumeric group")
        })
        .collect::<Result<Vec<_>>>()?;
    if !groups.contains(&gid) {
        groups.push(gid);
    }
    groups.sort_unstable();
    groups.dedup();
    if groups.is_empty() {
        anyhow::bail!("no supplementary groups resolved for {username}");
    }

    Ok(TargetUserIdentity {
        uid,
        gid,
        groups: groups
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
    })
}

/// Validate the runtime directory hierarchy that contains the managed Wayland
/// socket, before the socket file itself is validated.
///
/// Each directory component from the fixed base `/run/user/<uid>` down to (and
/// including) the socket's parent directory must be a real (non-symlink)
/// directory, owned by `expected_uid`, with mode `0700`. Rejecting a component
/// that is a symlink, group/world-accessible, or owned by another principal
/// narrows the TOCTOU window in which a user-writable path could be redirected
/// to another socket between validation and use.
///
/// Residual risk: std cannot perform an atomic `openat(2)`-based traversal, nor
/// bind/connect on a validated directory descriptor, so a sufficiently
/// privileged local attacker able to swap a component between this check and the
/// eventual `connect(2)` is not fully excluded. Component-wise validation of the
/// fixed `/run/user/<uid>/wrdp` prefix is the strongest guarantee available
/// without dropping to raw syscalls.
fn validate_runtime_directory_hierarchy(wayland_socket: &Path, expected_uid: u32) -> Result<()> {
    let base = PathBuf::from(format!("/run/user/{expected_uid}"));
    let parent = wayland_socket
        .parent()
        .context("managed compositor socket has no runtime directory")?;
    if !parent.starts_with(&base) {
        anyhow::bail!(
            "managed Wayland socket {} is not under the expected runtime base {}",
            wayland_socket.display(),
            base.display()
        );
    }

    // Ordered directory components to validate: the runtime base and every
    // intermediate directory down to the socket's parent (at least
    // `/run/user/<uid>/wrdp`).
    let mut dirs = vec![base.clone()];
    let mut current = base.clone();
    if let Ok(rest) = parent.strip_prefix(&base) {
        for component in rest.components() {
            current = current.join(component);
            dirs.push(current.clone());
        }
    }

    for dir in dirs {
        let metadata = fs::symlink_metadata(&dir)
            .with_context(|| format!("failed to inspect runtime directory {}", dir.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_dir()
            || metadata.uid() != expected_uid
            || metadata.mode() & 0o777 != 0o700
        {
            anyhow::bail!(
                "unsafe runtime directory {} (expected a non-symlink directory owned by uid {expected_uid} with mode 0700)",
                dir.display()
            );
        }
    }
    Ok(())
}

fn validate_managed_wayland_socket(wayland_socket: &Path, expected_uid: u32) -> Result<()> {
    // Validate the containing runtime directory hierarchy first, so the final
    // socket check happens on an already-vetted path prefix.
    validate_runtime_directory_hierarchy(wayland_socket, expected_uid)?;
    let metadata = fs::symlink_metadata(wayland_socket).with_context(|| {
        format!(
            "failed to inspect managed Wayland socket {}",
            wayland_socket.display()
        )
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != expected_uid
    {
        anyhow::bail!(
            "unsafe managed Wayland socket {} (expected a non-symlink socket owned by uid {expected_uid})",
            wayland_socket.display()
        );
    }
    Ok(())
}

async fn run_wlr_randr_as_target(
    wayland_socket: &Path,
    target_user: &TargetUserIdentity,
    arguments: &[&str],
) -> Result<std::process::Output> {
    // Validate immediately before each client invocation.  The user-writable
    // runtime directory must not redirect privileged daemon work to another
    // socket between session validation and compositor control.
    validate_managed_wayland_socket(wayland_socket, target_user.uid)?;
    let runtime_dir = wayland_socket
        .parent()
        .context("managed compositor socket has no runtime directory")?;
    let display = wayland_socket
        .file_name()
        .context("managed compositor socket has no display name")?;
    let uid = target_user.uid.to_string();
    let gid = target_user.gid.to_string();
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("/usr/bin/setpriv")
            .args([
                "--reuid",
                uid.as_str(),
                "--regid",
                gid.as_str(),
                "--groups",
                target_user.groups.as_str(),
                "--",
                "/usr/bin/wlr-randr",
            ])
            .args(arguments)
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("WAYLAND_DISPLAY", display)
            // Ensure the child is reaped if this future is cancelled or the
            // 5s bound elapses instead of leaking a wlr-randr process.
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("wlr-randr timed out after 5s")?
    .context("failed to execute wlr-randr through setpriv")?;
    if !output.status.success() {
        anyhow::bail!(
            "wlr-randr exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn wlr_randr_reports_current_mode(output: &[u8], size: DesktopSize) -> bool {
    let mode = format!("{}x{}", size.width, size.height);
    String::from_utf8_lossy(output).lines().any(|line| {
        let line = line.trim();
        line.split_whitespace().next() == Some(mode.as_str())
            && line.contains(" px")
            && line.contains("(current)")
    })
}

async fn resize_production_compositor(
    wayland_socket: &Path,
    size: DesktopSize,
    target_user: &TargetUserIdentity,
) -> Result<()> {
    let output_name =
        std::env::var("WRDP_HEADLESS_OUTPUT").unwrap_or_else(|_| "HEADLESS-1".to_string());
    let mode = format!("{}x{}", size.width, size.height);
    run_wlr_randr_as_target(
        wayland_socket,
        target_user,
        &[
            "--output",
            output_name.as_str(),
            "--custom-mode",
            mode.as_str(),
        ],
    )
    .await?;

    // wlr-randr accepting --custom-mode does not prove the compositor selected
    // it.  Query the named output as the session user and fail closed unless
    // that exact mode is currently active.
    let query = run_wlr_randr_as_target(
        wayland_socket,
        target_user,
        &["--output", output_name.as_str()],
    )
    .await?;
    if !wlr_randr_reports_current_mode(&query.stdout, size) {
        anyhow::bail!(
            "managed compositor output {output_name} did not realize requested mode {mode}; query output: {}",
            String::from_utf8_lossy(&query.stdout).trim()
        );
    }
    info!(
        width = size.width,
        height = size.height,
        "managed compositor output matches negotiated desktop"
    );
    Ok(())
}

#[cfg_attr(
    all(not(feature = "portal-generic"), not(test)),
    expect(
        dead_code,
        reason = "retained shared API when the runtime backend feature is disabled"
    )
)]
fn prime_direct_frame_receiver(
    source: std::sync::mpsc::Receiver<crate::desktop::pipewire::frame::RawFrameData>,
    expected_size: DesktopSize,
    timeout: Duration,
) -> Result<std::sync::mpsc::Receiver<crate::desktop::pipewire::frame::RawFrameData>> {
    let deadline = Instant::now() + timeout;
    let first = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("timed out waiting for a correctly-sized managed-session frame")?;
        let frame = source
            .recv_timeout(remaining)
            .context("timed out waiting for a correctly-sized managed-session frame")?;
        if frame.width == Some(u32::from(expected_size.width))
            && frame.height == Some(u32::from(expected_size.height))
        {
            break frame;
        }
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("primed-frame-forwarder".into())
        .spawn(move || {
            if sender.send(first).is_err() {
                return;
            }
            for frame in source {
                if sender.send(frame).is_err() {
                    break;
                }
            }
        })
        .context("failed to spawn primed frame forwarder")?;
    Ok(receiver)
}

#[expect(
    clippy::too_many_arguments,
    reason = "explicit protocol and pixel-plane parameters preserve existing call contracts"
)]
async fn build_production_portal_generic_connection(
    username: &str,
    wayland_socket: PathBuf,
    target_user: &TargetUserIdentity,
    config: Arc<Config>,
    server_event_tx: tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>,
    connection_invalid: Arc<Notify>,
    clipboard_manager: Option<Arc<Mutex<ClipboardOrchestrator>>>,
    gfx_server_handle: Option<Arc<RwLock<Option<ironrdp_server::GfxServerHandle>>>>,
    gfx_handler_state: Option<Arc<RwLock<Option<crate::rdp::server::HandlerState>>>>,
    desktop_size: DesktopSize,
    fatal_shutdown_error: Arc<StdMutex<Option<String>>>,
) -> Result<(BoundConnection, ProductionConnectionResources)> {
    #[cfg(not(feature = "portal-generic"))]
    {
        let _ = (
            username,
            wayland_socket,
            target_user,
            config,
            server_event_tx,
            connection_invalid,
            clipboard_manager,
            gfx_server_handle,
            gfx_handler_state,
            desktop_size,
            fatal_shutdown_error,
        );
        anyhow::bail!("production wrdp requires the portal-generic feature");
    }

    #[cfg(feature = "portal-generic")]
    {
        let portal_settings = crate::config::PortalStartupSettings::from_process(&config)?;
        let channel_policy = production_channel_policy(&config);
        validate_managed_wayland_socket(&wayland_socket, target_user.uid).with_context(|| {
            format!(
                "sesman reported an unsafe managed Wayland socket for '{username}': {}",
                wayland_socket.display()
            )
        })?;
        let session_handle =
            crate::rdp::session::backends::PortalSessionBackend::create_session_for_wayland_socket_with_policy_for_uid(
                wayland_socket.clone(),
                portal_settings,
                channel_policy.input,
                channel_policy.clipboard,
                target_user.uid,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to attach portal-generic to {}",
                    wayland_socket.display()
                )
            })?;

        let result: Result<(BoundConnection, ProductionConnectionResources)> = async {
            let streams = session_handle.streams();
            if streams.is_empty() {
                anyhow::bail!("portal-generic produced no capture streams for '{username}'");
            }

            let stream_info: Vec<crate::portal::StreamInfo> = streams
                .iter()
                .map(|s| crate::portal::StreamInfo {
                    node_id: s.node_id,
                    position: (s.position_x, s.position_y),
                    size: (s.width, s.height),
                    source_type: crate::portal::SourceType::Monitor,
                })
                .collect();
            let initial_size = stream_info
                .first()
                .map_or((1920, 1080), |s| (s.size.0 as u16, s.size.1 as u16));

            let raw_rx = session_handle.direct_frame_receiver()?;
            let raw_rx = tokio::task::spawn_blocking(move || {
                prime_direct_frame_receiver(raw_rx, desktop_size, Duration::from_secs(2))
            })
            .await
            .context("first-frame priming task panicked")??;
            info!("Managed-session capture primed before RDP activation");

            let display_handler = Arc::new(
                DisplayChannelHandler::new_direct(
                    desktop_size.width,
                    desktop_size.height,
                    raw_rx,
                    stream_info.clone(),
                    Some(managed_compositor_control(
                        wayland_socket.clone(),
                        target_user,
                    )),
                    None,
                    gfx_server_handle,
                    gfx_handler_state,
                    Arc::clone(&config),
                    production_service_registry(),
                )
                .await
                .context("failed to create production display handler")?,
            );
            display_handler
                .set_server_event_sender(server_event_tx.clone())
                .await;

            let channel_policy = production_channel_policy(&config);
            let (bound_input, input_handler): (
                Box<dyn RdpServerInputHandler>,
                Option<InputChannelHandler>,
            ) = if channel_policy.input {
                let primary_stream_id = stream_info.first().map_or(0, |s| s.node_id);
                let min_stream_x = stream_info.iter().map(|s| s.position.0).min().unwrap_or(0);
                let min_stream_y = stream_info.iter().map(|s| s.position.1).min().unwrap_or(0);
                let monitors: Vec<crate::rdp::channels::input::MonitorInfo> = stream_info
                    .iter()
                    .enumerate()
                    .map(|(idx, stream)| {
                        // Display encoding crops the capture to the negotiated desktop.
                        // Input must target that same visible crop rather than scaling
                        // coordinates into the uncropped capture dimensions.
                        let visible_width = if idx == 0 {
                            u32::from(desktop_size.width).min(stream.size.0)
                        } else {
                            stream.size.0
                        };
                        let visible_height = if idx == 0 {
                            u32::from(desktop_size.height).min(stream.size.1)
                        } else {
                            stream.size.1
                        };
                        crate::rdp::channels::input::MonitorInfo {
                            id: idx as u32,
                            name: format!("{username} monitor {idx}"),
                            x: stream.position.0,
                            y: stream.position.1,
                            width: visible_width,
                            height: visible_height,
                            dpi: 96.0,
                            scale_factor: 1.0,
                            stream_x: (i64::from(stream.position.0) - i64::from(min_stream_x)) as u32,
                            stream_y: (i64::from(stream.position.1) - i64::from(min_stream_y)) as u32,
                            stream_width: visible_width,
                            stream_height: visible_height,
                            is_primary: idx == 0,
                        }
                    })
                    .collect();

                let clipboard_provider: Option<
                    Arc<dyn crate::rdp::channels::clipboard::ClipboardProvider>,
                > = match clipboard_manager.as_ref() {
                    Some(manager) => match session_handle.clipboard_source() {
                        crate::rdp::session::backend::ClipboardSource::DataControl(ref backend) => {
                            let provider = Arc::new(
                                crate::rdp::channels::clipboard::providers::DataControlClipboardProvider::new(
                                    Arc::clone(backend),
                                ),
                            );
                            manager
                                .lock()
                                .await
                                .set_clipboard_provider(provider.clone())
                                .await;
                            display_handler
                                .set_clipboard_manager(Arc::clone(manager))
                                .await;
                            info!(
                                "production portal-generic clipboard provider attached for '{username}'"
                            );
                            Some(provider)
                        }
                        _ => {
                            warn!(
                                "production portal-generic session for '{username}' did not expose data-control clipboard"
                            );
                            None
                        }
                    },
                    None => None,
                };

                let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
                let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
                let input_handler = InputChannelHandler::new(
                    session_handle.clone(),
                    monitors,
                    primary_stream_id,
                    input_tx,
                    Some(display_handler.get_update_sender()),
                    Some(display_handler.get_gfx_handler_state()),
                    input_rx,
                    shutdown_rx,
                    &config.input.keyboard_layout,
                    config.input.cjk_paste_fallback,
                    clipboard_provider,
                )
                .context("failed to create production input handler")?;
                display_handler
                    .set_input_handler(Arc::new(input_handler.clone()))
                    .await;
                let health_input_handler = input_handler.clone();
                (
                    Box::new(ProductionBoundInput {
                        inner: input_handler,
                        _shutdown_tx: shutdown_tx,
                    }),
                    Some(health_input_handler),
                )
            } else {
                debug_assert!(!channel_policy.clipboard);
                debug_assert!(clipboard_manager.is_none());
                info!(
                    "production view-only policy active for '{username}'; input and clipboard are detached"
                );
                (Box::new(ProductionViewOnlyInput), None)
            };

            let (supervision_shutdown, _) = broadcast::channel(1);
            let (monitor, reporter, subscriber) =
                SessionSupervisor::new(supervision_shutdown.subscribe());
            session_handle.set_health_reporter(reporter.clone());
            display_handler.set_health_reporter(reporter.clone()).await;
            if let Some(input_handler) = input_handler.as_ref() {
                input_handler.set_health_reporter(reporter.clone());
            }
            if let Some(manager) = clipboard_manager.as_ref() {
                manager
                    .lock()
                    .await
                    .set_health_reporter(reporter.clone());
            }
            if !channel_policy.input {
                reporter.report(SessionStatusEvent::SubsystemNotAvailable {
                    subsystem: "input".to_string(),
                });
            }
            if !channel_policy.clipboard {
                reporter.report(SessionStatusEvent::SubsystemNotAvailable {
                    subsystem: "clipboard".to_string(),
                });
            }

            let monitor_handle = tokio::spawn(monitor.run());
            let watcher_handle = tokio::spawn(watch_production_session_health(
                subscriber,
                Arc::clone(&connection_invalid),
            ));

            Arc::clone(&display_handler).start_pipeline();

            info!(
                "production wrdp attached user '{}' to {} with {} stream(s), capture={}x{}, negotiated={}x{}",
                username,
                wayland_socket.display(),
                stream_info.len(),
                initial_size.0,
                initial_size.1,
                desktop_size.width,
                desktop_size.height,
            );

            let bound = BoundConnection {
                display: Box::new((*display_handler).clone()),
                input: bound_input,
            };
            Ok((
                bound,
                ProductionConnectionResources {
                    display: display_handler,
                    session_handle: Arc::clone(&session_handle),
                    connection_invalid,
                    supervision_shutdown,
                    monitor_handle,
                    watcher_handle,
                },
            ))
        }
        .await;

        match result {
            Ok(connection) => Ok(connection),
            Err(error) => match session_handle.shutdown().await {
                Ok(()) => Err(error),
                Err(shutdown_error) => {
                    let shutdown_error = shutdown_error.context(format!(
                        "portal-generic shutdown failed after production connection builder error: {error:#}"
                    ));
                    retain_fatal_shutdown_error(&fatal_shutdown_error, &shutdown_error);
                    Err(shutdown_error)
                }
            },
        }
    }
}

#[cfg_attr(
    not(feature = "portal-generic"),
    expect(
        dead_code,
        reason = "retained shared API when the runtime backend feature is disabled"
    )
)]
fn production_service_registry() -> Arc<crate::services::RuntimeCapabilities> {
    let capabilities = crate::desktop::compositor::CompositorCapabilities::new(
        crate::desktop::compositor::CompositorType::Wlroots {
            name: "wrdp-compositor".to_string(),
        },
        crate::desktop::compositor::PortalCapabilities::default(),
        Vec::new(),
    );
    Arc::new(crate::services::RuntimeCapabilities::from_compositor(
        &capabilities,
    ))
}

#[cfg_attr(
    all(not(feature = "portal-generic"), not(test)),
    expect(
        dead_code,
        reason = "retained shared API when the runtime backend feature is disabled"
    )
)]
struct ProductionViewOnlyInput;

impl RdpServerInputHandler for ProductionViewOnlyInput {
    fn keyboard(&mut self, _event: KeyboardEvent) {}

    fn mouse(&mut self, _event: MouseEvent) {}
}

#[cfg_attr(
    not(feature = "portal-generic"),
    expect(
        dead_code,
        reason = "retained shared API when the runtime backend feature is disabled"
    )
)]
struct ProductionBoundInput {
    inner: InputChannelHandler,
    _shutdown_tx: broadcast::Sender<()>,
}

impl RdpServerInputHandler for ProductionBoundInput {
    fn keyboard(&mut self, event: KeyboardEvent) {
        self.inner.keyboard(event);
    }

    fn mouse(&mut self, event: MouseEvent) {
        self.inner.mouse(event);
    }
}

fn normalize_local_account_name(username: &str) -> String {
    let without_domain = username
        .rsplit_once('\\')
        .map_or(username, |(_, user)| user);
    without_domain
        .split_once('@')
        .map_or(without_domain, |(user, _)| user)
        .to_string()
}

#[derive(Clone)]
struct ProductionPlaceholderDisplay;

#[async_trait]
impl RdpServerDisplay for ProductionPlaceholderDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: 1920,
            height: 1080,
        }
    }

    async fn updates(&mut self) -> ironrdp_server::ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(ProductionNoopUpdates))
    }
}

struct ProductionNoopUpdates;

#[async_trait]
impl RdpServerDisplayUpdates for ProductionNoopUpdates {
    async fn next_update(&mut self) -> ironrdp_server::ServerResult<Option<DisplayUpdate>> {
        std::future::pending::<()>().await;
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdp::server::EgfxCodecPolicy;

    fn raw_frame(
        width: u32,
        height: u32,
        data: Vec<u8>,
    ) -> crate::desktop::pipewire::frame::RawFrameData {
        crate::desktop::pipewire::frame::RawFrameData {
            data,
            width: Some(width),
            height: Some(height),
            stride: Some(width * 4),
            format: None,
        }
    }

    #[test]
    fn invalid_health_watcher_only_builds_reason_for_invalid_status() {
        let healthy = SessionStatus::default();
        assert!(invalid_session_reason(&healthy).is_none());

        let invalid = SessionStatus {
            overall: OverallStatus::Invalid,
            ..SessionStatus::default()
        };
        assert!(
            invalid_session_reason(&invalid)
                .unwrap()
                .starts_with("production session invalid:")
        );
    }

    #[test]
    fn cancellation_is_deferred_only_while_admission_is_running() {
        assert_eq!(
            cancellation_decision(false, false),
            CancellationDecision::Cancel
        );
        assert_eq!(
            cancellation_decision(true, false),
            CancellationDecision::DeferUntilAdmissionFinishes
        );
        assert_eq!(
            cancellation_decision(true, true),
            CancellationDecision::Cancel
        );
    }

    #[tokio::test]
    async fn admission_finished_guard_sets_flag_and_notifies() {
        let finished = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let notified = notify.notified();

        {
            let _guard = AdmissionFinishedGuard::new(Arc::clone(&finished), Arc::clone(&notify));
            assert!(!finished.load(Ordering::Acquire));
        }

        assert!(finished.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_millis(10), notified)
            .await
            .expect("guard did not notify admission completion");
    }

    #[test]
    fn first_matching_frame_is_preserved_when_capture_is_primed() {
        let (sender, source) = std::sync::mpsc::channel();
        sender.send(raw_frame(2, 1, vec![9, 9, 9, 9])).unwrap();
        sender.send(raw_frame(1, 1, vec![1, 2, 3, 4])).unwrap();
        drop(sender);

        let receiver = prime_direct_frame_receiver(
            source,
            DesktopSize {
                width: 1,
                height: 1,
            },
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(receiver.recv().unwrap().data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn first_frame_priming_times_out_cleanly() {
        let (_sender, source) = std::sync::mpsc::channel();
        assert!(
            prime_direct_frame_receiver(
                source,
                DesktopSize {
                    width: 1,
                    height: 1,
                },
                Duration::from_millis(1),
            )
            .is_err()
        );
    }

    #[test]
    fn managed_compositor_control_propagates_target_identity_and_output() {
        let identity = TargetUserIdentity {
            uid: 1001,
            gid: 1002,
            groups: "1002,1003".to_string(),
        };
        let socket = PathBuf::from("/run/user/1001/wrdp/wayland-0");

        assert_eq!(
            managed_compositor_control_with_output(
                socket.clone(),
                &identity,
                "HEADLESS-2".to_string(),
            ),
            ManagedCompositorControl::new(
                socket,
                "1001".to_string(),
                "1002".to_string(),
                "1002,1003".to_string(),
                "HEADLESS-2".to_string(),
            )
        );
    }

    #[test]
    fn initial_geometry_policy_is_fail_closed() {
        let mut config = Config::default();
        let default = PRODUCTION_DEFAULT_DESKTOP_SIZE;
        assert_eq!(
            DisplayChannelHandler::validate_geometry_policy(&config, default, Some(default))
                .unwrap(),
            default
        );

        config.display.allow_resize = false;
        assert!(
            DisplayChannelHandler::validate_geometry_policy(
                &config,
                DesktopSize {
                    width: 1280,
                    height: 720,
                },
                Some(default),
            )
            .is_err()
        );

        config.display.allow_resize = true;
        config.display.allowed_resolutions = vec!["1280x720".to_string()];
        assert!(
            DisplayChannelHandler::validate_geometry_policy(
                &config,
                DesktopSize {
                    width: 0,
                    height: 720,
                },
                Some(default),
            )
            .is_err()
        );
        assert!(
            DisplayChannelHandler::validate_geometry_policy(
                &config,
                DesktopSize {
                    width: 3840,
                    height: 2401,
                },
                Some(default),
            )
            .is_err()
        );
        assert!(
            DisplayChannelHandler::validate_geometry_policy(
                &config,
                DesktopSize {
                    width: 1920,
                    height: 1080,
                },
                Some(default),
            )
            .is_err()
        );
        assert!(
            DisplayChannelHandler::validate_geometry_policy(
                &config,
                DesktopSize {
                    width: 1280,
                    height: 720,
                },
                Some(default),
            )
            .is_ok()
        );
    }

    #[test]
    fn hardware_auto_policy_uses_avc420() {
        assert_eq!(
            effective_codec_policy(EgfxCodecPolicy::Auto, true),
            EgfxCodecPolicy::Avc420
        );
        assert_eq!(
            effective_codec_policy(EgfxCodecPolicy::Auto, false),
            EgfxCodecPolicy::Auto
        );
        assert_eq!(
            effective_codec_policy(EgfxCodecPolicy::Avc444, true),
            EgfxCodecPolicy::Avc444
        );
    }

    #[test]
    fn view_only_policy_detaches_input_and_clipboard() {
        let mut config = Config::default();
        config.clipboard.enabled = true;
        config.server.view_only = true;

        assert_eq!(
            production_channel_policy(&config),
            ProductionChannelPolicy {
                clipboard: false,
                input: false,
            }
        );

        config.server.view_only = false;
        assert_eq!(
            production_channel_policy(&config),
            ProductionChannelPolicy {
                clipboard: true,
                input: true,
            }
        );
    }

    #[test]
    fn runtime_hierarchy_rejects_path_outside_runtime_base() {
        // A socket outside /run/user/<uid> must be rejected before any
        // filesystem inspection of the (attacker-chosen) prefix.
        let socket = Path::new("/tmp/evil/wayland-0");
        assert!(validate_runtime_directory_hierarchy(socket, 1000).is_err());
    }

    #[test]
    fn runtime_hierarchy_rejects_nonexistent_runtime_base() {
        // A path under the expected base but whose components do not exist must
        // fail closed rather than silently pass validation.
        let socket = PathBuf::from("/run/user/424242/wrdp/wayland-0");
        assert!(validate_runtime_directory_hierarchy(&socket, 424242).is_err());
    }

    #[test]
    fn view_only_input_implements_pinned_ironrdp_noop_trait() {
        fn assert_input_handler<T: RdpServerInputHandler>() {}
        assert_input_handler::<ProductionViewOnlyInput>();

        let mut input = ProductionViewOnlyInput;
        input.keyboard(KeyboardEvent::Pressed {
            code: 30,
            extended: false,
        });
        input.mouse(MouseEvent::Move { x: 10, y: 20 });
    }

    #[test]
    fn systemd_listener_address_must_match_exactly() {
        let expected: SocketAddr = "127.0.0.1:3389".parse().unwrap();
        assert!(validate_systemd_listener_addr(expected, expected).is_ok());

        let wildcard: SocketAddr = "0.0.0.0:3389".parse().unwrap();
        let wrong_family: SocketAddr = "[::1]:3389".parse().unwrap();
        assert!(validate_systemd_listener_addr(expected, wildcard).is_err());
        assert!(validate_systemd_listener_addr(expected, wrong_family).is_err());
    }

    #[test]
    fn systemd_listener_must_accept_connections() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(validate_systemd_listener_accepts_connections(&listener).is_ok());

        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        assert!(validate_systemd_listener_accepts_connections(&stream).is_err());
    }

    #[test]
    fn timeout_selection_stops_pre_auth_only_until_binding_begins() {
        let started = tokio::time::Instant::now();
        assert_eq!(
            selected_connection_timeout(started, false, None),
            Some(SelectedConnectionTimeout {
                deadline: started + PRE_AUTH_TIMEOUT,
                phase: ConnectionTimeoutPhase::PreAuth,
            })
        );
        assert_eq!(selected_connection_timeout(started, true, None), None);

        let short_session = Duration::from_secs(10);
        assert_eq!(
            selected_connection_timeout(started, false, Some(short_session)),
            Some(SelectedConnectionTimeout {
                deadline: started + short_session,
                phase: ConnectionTimeoutPhase::Session,
            })
        );

        let long_session = Duration::from_secs(90);
        assert_eq!(
            selected_connection_timeout(started, true, Some(long_session)),
            Some(SelectedConnectionTimeout {
                deadline: started + long_session,
                phase: ConnectionTimeoutPhase::Session,
            })
        );
    }

    #[test]
    fn accept_retry_policy_classifies_and_caps_recoverable_errors() {
        assert!(is_recoverable_accept_error(&io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "aborted",
        )));
        assert!(is_recoverable_accept_error(&io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(!is_recoverable_accept_error(&io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid listener",
        )));

        assert_eq!(accept_retry_delay(1), Duration::from_millis(10));
        assert_eq!(accept_retry_delay(u32::MAX), MAX_ACCEPT_RETRY_DELAY);
    }
}
