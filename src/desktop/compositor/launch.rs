//! Managed compositor/component launch helpers.
//!
//! Launches managed session components through `setpriv` with the authenticated
//! user's primary/supplementary groups. Process spawning mechanics stay here so
//! session state management remains in sesman while launch policy stays focused.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt as _,
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use nix::unistd::{Gid, Uid, fchown};
use tracing::{debug, info};

use crate::sesman::{
    CapturedIdentity, ComponentConfig, ComponentState, capture_identity, process_alive,
};

const IDENTITY_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);
const IDENTITY_SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Resolved identity and runtime context for launching a managed component.
pub struct LaunchContext<'a> {
    pub user: &'a str,
    pub uid: u32,
    pub gid: u32,
    pub groups: &'a [libc::gid_t],
    pub home: &'a Path,
    pub xdg_runtime_dir: &'a Path,
    pub log_dir: &'a Path,
    pub environment: &'a BTreeMap<String, String>,
}

/// Spawn one managed compositor/session component through `setpriv`.
pub fn spawn_managed_component(
    component: &ComponentConfig,
    context: &LaunchContext<'_>,
) -> Result<ComponentState> {
    let log_path = component
        .log_path
        .clone()
        .unwrap_or_else(|| context.log_dir.join(format!("{}.log", component.name)));
    let stdout = open_log(&log_path, context.uid, context.gid)?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("failed to clone log handle for {}", log_path.display()))?;

    let group_arg = context
        .groups
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut command = Command::new("/usr/bin/setpriv");
    command.args([
        "--reuid",
        &context.uid.to_string(),
        "--regid",
        &context.gid.to_string(),
        "--groups",
        &group_arg,
        "--",
        &component.command,
    ]);
    command.args(&component.args);
    command.env("XDG_RUNTIME_DIR", context.xdg_runtime_dir);
    command.env("USER", context.user);
    command.env("LOGNAME", context.user);
    command.env("HOME", context.home);
    command.envs(context.environment);
    command.envs(&component.env);
    if let Some(cwd) = &component.working_dir {
        command.current_dir(cwd);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(stdout));
    command.stderr(Stdio::from(stderr));
    // Isolate each managed component so teardown reaches subprocesses as well
    // as the recorded leader PID (for example, shell-based component commands).
    command.process_group(0);

    info!(
        "starting component {}: {} {}",
        component.name,
        component.command,
        component.args.join(" ")
    );
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn component {}", component.name))?;
    let identity_deadline = Instant::now() + IDENTITY_SETTLE_TIMEOUT;
    let pid = i32::try_from(child.id()).context("child pid did not fit i32")?;
    let identity = match settle_setpriv_identity(&mut child, pid, context.uid, identity_deadline) {
        Ok(identity) => identity,
        Err(error) => {
            terminate_and_reap(&mut child);
            return Err(error).with_context(|| {
                format!(
                    "component {} did not assume uid {} after setpriv",
                    component.name, context.uid
                )
            });
        }
    };
    let component_name = component.name.clone();
    thread::spawn(move || match child.wait() {
        Ok(status) => debug!("component {component_name} pid {pid} exited with {status}"),
        Err(error) => debug!("failed to reap component {component_name} pid {pid}: {error}"),
    });

    Ok(ComponentState {
        name: component.name.clone(),
        pid,
        command: std::iter::once(component.command.clone())
            .chain(component.args.clone())
            .collect(),
        started_at: Utc::now(),
        start_ticks: identity.start_ticks,
        boot_id: identity.boot_id,
        uid: identity.uid,
        pgid: identity.pgid,
        required: component.required,
        process_group: true,
    })
}

/// Wait for `setpriv` to exec and apply the requested credentials before
/// recording this process's persistent identity.
fn settle_setpriv_identity(
    child: &mut Child,
    pid: i32,
    expected_uid: u32,
    deadline: Instant,
) -> Result<CapturedIdentity> {
    let mut start_ticks = None;

    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to check setpriv child status")?
        {
            bail!("setpriv exited before assuming the requested identity: {status}");
        }

        let mut identity = capture_identity(pid);
        // `setpriv` changes credentials in-place, so its start time remains the
        // component's start time. Retain it from an earlier partial snapshot
        // while the remaining identity fields settle.
        start_ticks = start_ticks.or(identity.start_ticks);
        identity.start_ticks = start_ticks;
        if identity_is_settled(&identity, expected_uid, process_alive(pid)) {
            return Ok(identity);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for setpriv to assume the requested identity");
        }
        thread::sleep(IDENTITY_SETTLE_POLL_INTERVAL.min(remaining));
    }
}

fn identity_is_settled(identity: &CapturedIdentity, expected_uid: u32, alive: bool) -> bool {
    alive
        && identity.uid == Some(expected_uid)
        && identity.start_ticks.is_some()
        && identity.boot_id.is_some()
        && identity.pgid.is_some()
}

fn terminate_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn open_log(path: &Path, uid: u32, gid: u32) -> Result<File> {
    const MAX_COMPONENT_LOG_BYTES: u64 = 10 * 1024 * 1024;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("component log has no parent: {}", path.display()))?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("failed to inspect log directory {}", parent.display()))?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        anyhow::bail!("unsafe component log directory: {}", parent.display());
    }
    if parent_metadata.uid() != uid || parent_metadata.gid() != gid {
        anyhow::bail!(
            "component log directory {} is not owned by {uid}:{gid}",
            parent.display()
        );
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            anyhow::bail!("unsafe component log: {}", path.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect component log {}", path.display()));
        }
    }

    let open_new = || {
        OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
    };
    let (file, created) = match open_new() {
        Ok(file) => (file, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new()
                .append(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .with_context(|| format!("failed to open component log {}", path.display()))?;
            (file, false)
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to create component log {}", path.display()));
        }
    };

    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect component log {}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("component log is not a regular file: {}", path.display());
    }
    if created {
        fchown(&file, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
            .with_context(|| format!("failed to chown log {} to {uid}:{gid}", path.display()))?;
    } else if metadata.uid() != uid || metadata.gid() != gid {
        anyhow::bail!(
            "component log {} is not owned by {uid}:{gid}",
            path.display()
        );
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("failed to restrict component log")?;

    // Component logs are diagnostics, not an audit archive. Bound persistence
    // across restarts instead of appending indefinitely.
    if file.metadata()?.len() > MAX_COMPONENT_LOG_BYTES {
        file.set_len(0)
            .context("failed to truncate oversized component log")?;
    }

    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_identity_requires_target_uid_and_every_hardened_field() {
        let identity = CapturedIdentity {
            start_ticks: Some(1),
            boot_id: Some("boot-id".to_string()),
            uid: Some(1000),
            pgid: Some(42),
        };
        assert!(identity_is_settled(&identity, 1000, true));
        assert!(!identity_is_settled(&identity, 1001, true));
        assert!(!identity_is_settled(&identity, 1000, false));

        for identity in [
            CapturedIdentity {
                start_ticks: None,
                ..identity.clone()
            },
            CapturedIdentity {
                boot_id: None,
                ..identity.clone()
            },
            CapturedIdentity {
                pgid: None,
                ..identity.clone()
            },
        ] {
            assert!(!identity_is_settled(&identity, 1000, true));
        }
    }
}
