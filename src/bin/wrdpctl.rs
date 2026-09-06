//! `wrdpctl` administration CLI for production wrdp session operations.

use std::{
    fs,
    io::{BufRead, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;
use wrdp::sesman::{ComponentState, SesmanConfig, SessionManager, SessionState, SessionStatus};

#[derive(Debug, Parser)]
#[command(name = "wrdpctl")]
#[command(version, about = "Administer wrdp sessions and diagnostics", long_about = None)]
struct Args {
    /// Emit JSON output for automation.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List discovered wrdp sessions.
    Sessions,
    /// Inspect one authenticated user's managed wrdp session.
    Inspect { user: String },
    /// Print the tail of one authenticated user's sesman logs.
    Tail {
        user: String,
        #[arg(short = 'n', long, default_value_t = 80)]
        lines: usize,
    },
    /// Stop one authenticated user's managed display stack.
    Stop { user: String },
    /// Stop one user's managed display stack and clean stale runtime state.
    Cleanup { user: String },
    /// Run production listener/session diagnostics.
    Doctor,
}

#[derive(Debug, Serialize)]
struct DiscoveredSession {
    user: String,
    uid: u32,
    state_path: PathBuf,
    status: SessionStatus,
}

#[derive(Debug, Serialize)]
struct RuntimeDirReport {
    path: PathBuf,
    exists: bool,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ProcessReport {
    name: String,
    pid: i32,
    alive: bool,
    command: Vec<String>,
    children: Vec<ProcessReport>,
}

#[derive(Debug, Serialize)]
struct RenderNodeReport {
    path: PathBuf,
    uid: u32,
    gid: u32,
    mode: u32,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    listeners: Vec<String>,
    sessions: Vec<DiscoveredSession>,
    runtime_dirs: Vec<RuntimeDirReport>,
    process_trees: Vec<ProcessReport>,
    render_nodes: Vec<RenderNodeReport>,
    warnings: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Command::Sessions => print_output(args.json, &discover_sessions()?),
        Command::Inspect { user } => print_output(args.json, &manager_for_user(&user)?.status()?),
        Command::Tail { user, lines } => tail_user_logs(&user, lines),
        Command::Stop { user } => print_output(args.json, &manager_for_user(&user)?.stop()?),
        Command::Cleanup { user } => print_output(args.json, &manager_for_user(&user)?.stop()?),
        Command::Doctor => print_output(args.json, &doctor()?),
    }
}

fn user_uid(user: &str) -> Result<u32> {
    nix::unistd::User::from_name(user)
        .context("failed to resolve session user")?
        .map(|entry| entry.uid.as_raw())
        .with_context(|| format!("unknown session user: {user}"))
}

fn manager_for_user(user: &str) -> Result<SessionManager> {
    Ok(SessionManager::new(SesmanConfig::for_user(user)?))
}

fn discover_sessions() -> Result<Vec<DiscoveredSession>> {
    discover_sessions_in(Path::new("/run/wrdp/sesman"))
}

/// Discover sessions under a root-owned registry root.
///
/// Only the root-owned production registry `/run/wrdp/sesman/<uid>` tree is
/// trusted; the legacy `/run/user/<uid>/wrdp/sesman` location is ignored. Each
/// numeric-UID directory and its `*.state.json` files must be regular,
/// non-symlink, and owned by the effective UID running this tool (root in
/// production), and the user named by the state must resolve back to the
/// directory's UID. The registry root is a parameter so the scan can be
/// exercised against a temporary tree in tests.
fn discover_sessions_in(registry_root: &Path) -> Result<Vec<DiscoveredSession>> {
    let mut sessions = Vec::new();
    let euid = nix::unistd::Uid::effective().as_raw();
    let Ok(entries) = fs::read_dir(registry_root) else {
        return Ok(sessions);
    };

    for entry in entries.flatten() {
        let Some(uid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let dir_path = entry.path();
        let Ok(dir_metadata) = fs::symlink_metadata(&dir_path) else {
            continue;
        };
        if dir_metadata.file_type().is_symlink()
            || !dir_metadata.is_dir()
            || dir_metadata.uid() != euid
        {
            continue;
        }
        let Ok(state_entries) = fs::read_dir(&dir_path) else {
            continue;
        };
        for state_entry in state_entries.flatten() {
            let path = state_entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("failed to stat {}", path.display()))?;
            if metadata.file_type().is_symlink()
                || !metadata.file_type().is_file()
                || metadata.uid() != euid
            {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<SessionState>(&bytes) else {
                continue;
            };
            let Ok(expected_uid) = user_uid(&state.user) else {
                continue;
            };
            if expected_uid != uid {
                continue;
            }
            let manager = manager_for_user(&state.user)?;
            sessions.push(DiscoveredSession {
                user: state.user,
                uid,
                state_path: path,
                status: manager.status()?,
            });
        }
    }

    sessions.sort_by(|a, b| a.user.cmp(&b.user));
    Ok(sessions)
}

fn tail_user_logs(user: &str, lines: usize) -> Result<()> {
    let config = SesmanConfig::for_user(user)?;
    let mut paths = vec![config.log_dir.join("compositor.log")];
    paths.retain(|path| path.exists());
    if paths.is_empty() {
        bail!(
            "no wrdp sesman logs found for user {user} under {}",
            config.log_dir.display()
        );
    }

    for path in paths {
        println!("==> {} <==", path.display());
        for line in tail_file(&path, lines)? {
            println!("{line}");
        }
    }
    Ok(())
}

fn tail_file(path: &Path, lines: usize) -> Result<Vec<String>> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let len = file.metadata()?.len();
    let read_from = len.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(read_from))?;
    let reader = std::io::BufReader::new(file);
    let mut collected: Vec<String> = reader.lines().collect::<std::io::Result<_>>()?;
    if collected.len() > lines {
        collected.drain(0..collected.len() - lines);
    }
    Ok(collected)
}

fn doctor() -> Result<DoctorReport> {
    let listeners = listener_lines()?;
    let sessions = discover_sessions()?;
    let runtime_dirs = runtime_dir_reports(&sessions);
    let process_trees = process_tree_reports(&sessions);
    let render_nodes = render_node_reports();
    let mut warnings = Vec::new();

    let public_3389 = listeners
        .iter()
        .filter(|line| line.contains(":3389"))
        .count();

    if public_3389 == 0 {
        warnings.push("no public 3389 listener detected".to_string());
    }
    if public_3389 > 1 {
        warnings.push(format!("multiple 3389 listeners detected: {public_3389}"));
    }

    for session in &sessions {
        if session.status.health != wrdp::sesman::SessionHealth::Healthy {
            warnings.push(format!(
                "session for user {} is {:?}",
                session.user, session.status.health
            ));
        }
        if let Some(state) = &session.status.state
            && state.active_clients == 0
            && state.last_disconnected_at.is_some()
        {
            warnings.push(format!(
                "session for user {} is idle/disconnected",
                session.user
            ));
        }
    }
    if render_nodes.is_empty() {
        warnings.push(
            "no /dev/dri/renderD* nodes found; hardware acceleration may be unavailable"
                .to_string(),
        );
    }

    Ok(DoctorReport {
        listeners,
        sessions,
        runtime_dirs,
        process_trees,
        render_nodes,
        warnings,
    })
}

fn runtime_dir_reports(sessions: &[DiscoveredSession]) -> Vec<RuntimeDirReport> {
    let mut reports = Vec::new();
    for session in sessions {
        if let Some(state) = &session.status.state {
            for path in [
                state.xdg_runtime_dir.clone(),
                state.xdg_runtime_dir.join("sesman"),
                state.xdg_runtime_dir.join("logs"),
            ] {
                reports.push(runtime_dir_report(path));
            }
        }
    }
    reports
}

fn runtime_dir_report(path: PathBuf) -> RuntimeDirReport {
    match fs::metadata(&path) {
        Ok(metadata) => RuntimeDirReport {
            path,
            exists: true,
            uid: Some(metadata.uid()),
            gid: Some(metadata.gid()),
            mode: Some(metadata.mode() & 0o7777),
        },
        Err(_) => RuntimeDirReport {
            path,
            exists: false,
            uid: None,
            gid: None,
            mode: None,
        },
    }
}

fn process_tree_reports(sessions: &[DiscoveredSession]) -> Vec<ProcessReport> {
    let mut reports = Vec::new();
    for session in sessions {
        let Some(state) = &session.status.state else {
            continue;
        };
        for component in &state.components {
            reports.push(process_report(component));
        }
    }
    reports
}

fn process_report(component: &ComponentState) -> ProcessReport {
    ProcessReport {
        name: component.name.clone(),
        pid: component.pid,
        alive: process_alive(component.pid),
        command: command_line(component.pid).unwrap_or_else(|| component.command.clone()),
        children: child_pids(component.pid)
            .into_iter()
            .map(|pid| ProcessReport {
                name: proc_comm(pid).unwrap_or_else(|| "unknown".to_string()),
                pid,
                alive: process_alive(pid),
                command: command_line(pid).unwrap_or_default(),
                children: child_pids(pid)
                    .into_iter()
                    .map(|child_pid| ProcessReport {
                        name: proc_comm(child_pid).unwrap_or_else(|| "unknown".to_string()),
                        pid: child_pid,
                        alive: process_alive(child_pid),
                        command: command_line(child_pid).unwrap_or_default(),
                        children: Vec::new(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn process_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn child_pids(ppid: i32) -> Vec<i32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if proc_ppid(pid) == Some(ppid) {
            children.push(pid);
        }
    }
    children.sort_unstable();
    children
}

fn proc_ppid(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(") ")?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn proc_comm(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn command_line(pid: i32) -> Option<Vec<String>> {
    let data = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let parts: Vec<String> = data
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

fn render_node_reports() -> Vec<RenderNodeReport> {
    let Ok(entries) = fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("renderD") {
            continue;
        }
        if let Ok(metadata) = entry.metadata() {
            reports.push(RenderNodeReport {
                path,
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode() & 0o7777,
            });
        }
    }
    reports.sort_by(|a, b| a.path.cmp(&b.path));
    reports
}

fn listener_lines() -> Result<Vec<String>> {
    let output = ProcessCommand::new("ss")
        .args(["-ltnp"])
        .output()
        .context("failed to run ss -ltnp")?;
    if !output.status.success() {
        bail!("ss -ltnp exited {}", output.status);
    }
    let stdout = String::from_utf8(output.stdout).context("ss output was not UTF-8")?;
    Ok(stdout.lines().map(str::to_string).collect())
}

fn print_output<T>(json: bool, value: &T) -> Result<()>
where
    T: Serialize + std::fmt::Debug,
{
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{value:#?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_ignores_non_numeric_and_unresolvable_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        // A non-numeric directory is not a per-UID registry and is ignored.
        fs::create_dir(root.join("not-a-uid")).expect("create non-numeric dir");
        // A numeric directory owned by the test process, but the state names an
        // unresolvable user (and is not a complete SessionState), so the entry
        // is skipped rather than reported.
        let uid_dir = root.join("4000000000");
        fs::create_dir(&uid_dir).expect("create uid dir");
        fs::write(
            uid_dir.join("default.state.json"),
            r#"{"user":"wrdp-nonexistent-user-xyz"}"#,
        )
        .expect("write state");

        let sessions = discover_sessions_in(root).expect("discover");
        assert!(sessions.is_empty());
    }
}
