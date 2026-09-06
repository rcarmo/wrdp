use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::Mutex,
    time::{Duration, Instant},
};
#[derive(Clone, Copy)]
struct Failure {
    count: u8,
    at: Instant,
}
pub(super) struct PeerGuard {
    current: Mutex<IpAddr>,
    failures: Mutex<HashMap<IpAddr, Failure>>,
}
impl PeerGuard {
    pub(super) fn new() -> Self {
        Self {
            current: Mutex::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            failures: Mutex::new(HashMap::new()),
        }
    }
    pub(super) fn set_peer(&self, ip: IpAddr) {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ip;
    }
    pub(super) fn peer(&self) -> IpAddr {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(super) fn blocked_for(&self, ip: IpAddr) -> Option<Duration> {
        let entries = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = entries.get(&ip)?;
        let lock = match e.count {
            0..=3 => return None,
            4 => Duration::from_secs(5),
            5 => Duration::from_secs(15),
            _ => Duration::from_secs(60),
        };
        lock.checked_sub(e.at.elapsed())
    }
    pub(super) fn failure(&self, ip: IpAddr) {
        let mut entries = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = entries.entry(ip).or_insert(Failure {
            count: 0,
            at: Instant::now(),
        });
        e.count = e.count.saturating_add(1);
        e.at = Instant::now();
    }
    pub(super) fn success(&self, ip: IpAddr) {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&ip);
    }
    pub(super) fn prune(&self) {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, e| e.at.elapsed() < Duration::from_secs(120));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_escalate_and_success_resets_only_that_peer() {
        let guard = PeerGuard::new();
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let other = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        for _ in 0..3 {
            guard.failure(peer);
            assert!(guard.blocked_for(peer).is_none());
        }
        for seconds in [5, 15, 60] {
            guard.failure(peer);
            let remaining = guard.blocked_for(peer).unwrap();
            assert!(remaining <= Duration::from_secs(seconds));
            assert!(remaining > Duration::from_secs(seconds - 1));
        }
        guard.failure(other);
        guard.success(peer);
        assert!(guard.blocked_for(peer).is_none());
        assert!(guard.failures.lock().unwrap().contains_key(&other));
    }

    #[test]
    fn expired_lock_and_stale_entries_are_released() {
        let guard = PeerGuard::new();
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let recent = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        guard.failures.lock().unwrap().insert(
            peer,
            Failure {
                count: 6,
                at: Instant::now() - Duration::from_secs(121),
            },
        );
        guard.failure(recent);
        assert!(guard.blocked_for(peer).is_none());
        guard.prune();
        let entries = guard.failures.lock().unwrap();
        assert!(!entries.contains_key(&peer));
        assert!(entries.contains_key(&recent));
    }
}
