//! Process lifecycle helpers used by sesman runtime supervision.
//!
//! These helpers implement the low-level checks/signals/ownership operations
//! used while tracking managed compositor components in per-user sessions.

use std::fs;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};

pub(crate) fn process_alive(pid: i32) -> bool {
    if pid <= 0 || kill(Pid::from_raw(pid), None).is_err() {
        return false;
    }

    // `kill(pid, 0)` succeeds for zombies, but a managed compositor component
    // in `/proc` state `Z` is considered dead for sesman lifecycle decisions.
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(after_comm) = stat.rsplit_once(") ").map(|(_, rest)| rest) else {
        return false;
    };
    !after_comm.starts_with('Z')
}

/// Why a caller is authenticating a persisted PID.
///
/// Signalling is destructive: sending SIGTERM/SIGKILL to a PID that Linux reused
/// after a crash or reboot would kill an unrelated process. Status reporting is
/// observational, so it may fall back to weaker evidence and simply flag a
/// component as possibly-stale instead of refusing outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchPurpose {
    /// Read-only health classification. Legacy registry entries missing the
    /// hardened identity fields may still be matched using kernel start ticks or
    /// the recorded start timestamp; the result can be stale but never signals.
    Status,
    /// Authorization for a destructive signal. Any missing hardened identity
    /// field fails closed so sesman never signals a process it cannot fully
    /// authenticate against the recorded spawn identity.
    Signal,
}

/// The persisted/expected identity of a managed process a caller wants to
/// authenticate against the live `/proc` state of `pid`.
///
/// A plain `kill(pid, 0)` is not sufficient for persisted state: after a crash
/// or reboot Linux may reuse the PID, and sesman must never reuse or signal that
/// unrelated process. Callers build this from a persisted registry record and
/// hand it to [`process_matches`]. Fields recorded as `None` come from legacy
/// registry entries written before that field existed.
#[derive(Debug, Clone)]
pub(crate) struct ProcessSignature<'a> {
    /// PID recorded at spawn time.
    pub pid: i32,
    /// Recorded argv (command plus args). Empty never matches.
    pub command: &'a [String],
    /// Wall-clock spawn time; only used as a legacy fallback for status.
    pub started_at: DateTime<Utc>,
    /// Exact kernel start-time ticks, stable across argv/exec changes.
    pub start_ticks: Option<u64>,
    /// Boot ID captured at spawn. A mismatch means the machine rebooted and any
    /// PID match is coincidental.
    pub boot_id: Option<&'a str>,
    /// Real UID observed via `/proc/<pid>/status` at spawn.
    pub uid: Option<u32>,
    /// Process-group id observed via `/proc/<pid>/stat` at spawn.
    pub pgid: Option<i32>,
}

impl ProcessSignature<'_> {
    /// True when every hardened identity field was persisted. Legacy entries
    /// lacking any of these cannot be authenticated for signalling.
    fn is_fully_hardened(&self) -> bool {
        self.start_ticks.is_some()
            && self.boot_id.is_some()
            && self.uid.is_some()
            && self.pgid.is_some()
    }
}

/// Check that a live PID is still the process instance recorded by sesman.
///
/// Present hardened fields (boot id, uid, pgid, start ticks) must all match the
/// live process. For [`MatchPurpose::Signal`] any missing hardened field fails
/// closed. For [`MatchPurpose::Status`] a legacy entry falls back to kernel
/// start ticks and finally to the recorded start timestamp.
pub(crate) fn process_matches(signature: &ProcessSignature<'_>, purpose: MatchPurpose) -> bool {
    if !process_alive(signature.pid) || signature.command.is_empty() {
        return false;
    }

    // Signalling must never target a process we cannot fully authenticate.
    if purpose == MatchPurpose::Signal && !signature.is_fully_hardened() {
        return false;
    }

    if let Some(expected_boot_id) = signature.boot_id
        && read_boot_id().as_deref() != Some(expected_boot_id)
    {
        return false;
    }

    if let Some(expected_uid) = signature.uid {
        match process_status_uids(signature.pid) {
            // Accept either credential: a managed component may legitimately
            // drop privileges, changing effective while retaining real (or vice
            // versa). A reused PID owned by another account matches neither.
            Some((real_uid, effective_uid))
                if real_uid == expected_uid || effective_uid == expected_uid => {}
            _ => return false,
        }
    }

    if let Some(expected_pgid) = signature.pgid
        && process_pgid(signature.pid) != Some(expected_pgid)
    {
        return false;
    }

    if let Some(expected_start_ticks) = signature.start_ticks {
        return process_start_ticks(signature.pid) == Some(expected_start_ticks);
    }

    // Only reachable for status of a fully legacy entry (no start ticks).
    process_started_at(signature.pid).is_some_and(|actual| {
        (actual.timestamp_millis() - signature.started_at.timestamp_millis()).abs() <= 5_000
    })
}

/// Snapshot of a freshly spawned process identity for persistence.
///
/// Callers record these alongside the PID so a later [`process_matches`] can
/// authenticate the live process against exactly what was spawned.
#[derive(Debug, Clone)]
pub(crate) struct CapturedIdentity {
    pub start_ticks: Option<u64>,
    pub boot_id: Option<String>,
    pub uid: Option<u32>,
    pub pgid: Option<i32>,
}

/// Capture the hardened identity of `pid` immediately after spawn.
pub(crate) fn capture_identity(pid: i32) -> CapturedIdentity {
    CapturedIdentity {
        start_ticks: process_start_ticks(pid),
        boot_id: read_boot_id(),
        uid: process_status_uids(pid).map(|(real_uid, _)| real_uid),
        pgid: process_pgid(pid),
    }
}

/// Read the kernel boot ID (`/proc/sys/kernel/random/boot_id`).
///
/// The value is regenerated on every boot, so it distinguishes a persisted PID
/// from a same-numbered PID that appeared after a reboot.
pub(crate) fn read_boot_id() -> Option<String> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()?
        .trim()
        .to_string();
    if boot_id.is_empty() {
        return None;
    }
    Some(boot_id)
}

/// Read the real and effective UID of `pid` from `/proc/<pid>/status`.
///
/// Returns `(real_uid, effective_uid)`. Prefer `/proc/<pid>/status` over the
/// `stat` credential fields because it is the documented, stable interface for
/// process credentials.
pub(crate) fn process_status_uids(pid: i32) -> Option<(u32, u32)> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid_line = status.lines().find_map(|line| line.strip_prefix("Uid:"))?;
    let mut fields = uid_line.split_whitespace();
    let real_uid = fields.next()?.parse::<u32>().ok()?;
    let effective_uid = fields.next()?.parse::<u32>().ok()?;
    Some((real_uid, effective_uid))
}

/// Read the process-group id (pgrp) of `pid` from `/proc/<pid>/stat`.
///
/// The parser splits after the final `") "` so a `comm` value containing spaces
/// or parentheses cannot shift field offsets. pgrp is field 5, i.e. the third
/// whitespace token after `comm` (state, ppid, pgrp).
pub(crate) fn process_pgid(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().nth(2)?.parse::<i32>().ok()
}

pub(crate) fn process_start_ticks(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    // The text after comm starts at field 3; starttime is field 22.
    after_comm.split_whitespace().nth(19)?.parse::<u64>().ok()
}

pub(super) fn process_started_at(pid: i32) -> Option<DateTime<Utc>> {
    let start_ticks = process_start_ticks(pid)?;
    let boot_seconds = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .parse::<u64>()
        .ok()?;
    let ticks_per_second = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK).ok()??;
    if ticks_per_second <= 0 {
        return None;
    }
    let ticks_per_second = u64::try_from(ticks_per_second).ok()?;
    let seconds = boot_seconds.checked_add(start_ticks / ticks_per_second)?;
    let nanos = (start_ticks % ticks_per_second)
        .checked_mul(1_000_000_000)?
        .checked_div(ticks_per_second)?;
    DateTime::from_timestamp(i64::try_from(seconds).ok()?, u32::try_from(nanos).ok()?)
}

pub(super) fn process_group_alive(pid: i32) -> bool {
    pid > 0 && kill(Pid::from_raw(-pid), None).is_ok()
}

pub(super) fn signal_process(pid: i32, process_group: bool, signal: Signal) -> Result<()> {
    let target = if process_group { -pid } else { pid };
    kill(Pid::from_raw(target), Some(signal)).with_context(|| {
        if process_group {
            format!("failed to signal process group {pid}")
        } else {
            format!("failed to signal pid {pid}")
        }
    })
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    /// Build a signature for the current test process using its live identity.
    fn current_signature<'a>(
        command: &'a [String],
        boot_id: &'a str,
        uid: u32,
        pgid: i32,
        start_ticks: u64,
    ) -> ProcessSignature<'a> {
        ProcessSignature {
            pid: i32::try_from(std::process::id()).expect("pid fits i32"),
            command,
            started_at: Utc::now(),
            start_ticks: Some(start_ticks),
            boot_id: Some(boot_id),
            uid: Some(uid),
            pgid: Some(pgid),
        }
    }

    #[test]
    fn current_process_identity_matches_itself() {
        let pid = i32::try_from(std::process::id()).expect("pid fits i32");
        let start_ticks = process_start_ticks(pid).expect("start ticks");
        let boot_id = read_boot_id().expect("boot id");
        let (real_uid, _) = process_status_uids(pid).expect("proc uids");
        let pgid = process_pgid(pid).expect("pgid");
        let command = vec!["test-harness".to_string()];

        let signature = current_signature(&command, &boot_id, real_uid, pgid, start_ticks);
        assert!(process_matches(&signature, MatchPurpose::Signal));
        assert!(process_matches(&signature, MatchPurpose::Status));
    }

    #[test]
    fn identity_rejects_wrong_uid() {
        let pid = i32::try_from(std::process::id()).expect("pid fits i32");
        let start_ticks = process_start_ticks(pid).expect("start ticks");
        let boot_id = read_boot_id().expect("boot id");
        let (real_uid, _) = process_status_uids(pid).expect("proc uids");
        let pgid = process_pgid(pid).expect("pgid");
        let command = vec!["test-harness".to_string()];

        let signature = current_signature(
            &command,
            &boot_id,
            real_uid.wrapping_add(1),
            pgid,
            start_ticks,
        );
        assert!(!process_matches(&signature, MatchPurpose::Signal));
        assert!(!process_matches(&signature, MatchPurpose::Status));
    }

    #[test]
    fn identity_rejects_wrong_boot_id() {
        let pid = i32::try_from(std::process::id()).expect("pid fits i32");
        let start_ticks = process_start_ticks(pid).expect("start ticks");
        let (real_uid, _) = process_status_uids(pid).expect("proc uids");
        let pgid = process_pgid(pid).expect("pgid");
        let command = vec!["test-harness".to_string()];

        // A different boot id models the machine having rebooted since spawn.
        let signature = current_signature(
            &command,
            "00000000-0000-0000-0000-000000000000",
            real_uid,
            pgid,
            start_ticks,
        );
        assert!(!process_matches(&signature, MatchPurpose::Signal));
        assert!(!process_matches(&signature, MatchPurpose::Status));
    }

    #[test]
    fn identity_rejects_wrong_pgid() {
        let pid = i32::try_from(std::process::id()).expect("pid fits i32");
        let start_ticks = process_start_ticks(pid).expect("start ticks");
        let boot_id = read_boot_id().expect("boot id");
        let (real_uid, _) = process_status_uids(pid).expect("proc uids");
        let pgid = process_pgid(pid).expect("pgid");
        let command = vec!["test-harness".to_string()];

        let signature = current_signature(
            &command,
            &boot_id,
            real_uid,
            pgid.wrapping_add(1),
            start_ticks,
        );
        assert!(!process_matches(&signature, MatchPurpose::Signal));
        assert!(!process_matches(&signature, MatchPurpose::Status));
    }

    #[test]
    fn legacy_identity_fails_closed_for_signalling_but_reports_for_status() {
        let mut child = Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = i32::try_from(child.id()).expect("pid fits i32");
        let command = vec!["/bin/sleep".to_string(), "60".to_string()];
        let start_ticks = process_start_ticks(pid).expect("start ticks");

        // Legacy registry entry: kernel start ticks only, no hardened fields.
        let legacy = ProcessSignature {
            pid,
            command: &command,
            started_at: Utc::now(),
            start_ticks: Some(start_ticks),
            boot_id: None,
            uid: None,
            pgid: None,
        };
        // Fails closed for signalling: cannot prove PID was not reused.
        assert!(!process_matches(&legacy, MatchPurpose::Signal));
        // Status may still report it as live via start ticks.
        assert!(process_matches(&legacy, MatchPurpose::Status));

        // Wrong start ticks are rejected even for status.
        let wrong_ticks = ProcessSignature {
            start_ticks: Some(start_ticks.saturating_add(1)),
            ..legacy.clone()
        };
        assert!(!process_matches(&wrong_ticks, MatchPurpose::Status));

        // Fully legacy entry (no start ticks) falls back to start timestamp for
        // status and still fails closed for signalling.
        let no_ticks = ProcessSignature {
            start_ticks: None,
            ..legacy.clone()
        };
        assert!(process_matches(&no_ticks, MatchPurpose::Status));
        assert!(!process_matches(&no_ticks, MatchPurpose::Signal));
        let stale = ProcessSignature {
            start_ticks: None,
            started_at: Utc::now() - chrono::Duration::minutes(1),
            ..legacy.clone()
        };
        assert!(!process_matches(&stale, MatchPurpose::Status));

        child.kill().expect("kill sleep");
        child.wait().expect("reap sleep");
    }
}
