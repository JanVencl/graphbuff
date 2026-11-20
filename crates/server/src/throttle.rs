//! Login throttling: failed logins lock a client out for a while, so passwords cannot be
//! guessed at the speed of the network.
//!
//! Failures are counted per client address and user name, and per client address over
//! all names (so trying many names does not help). After
//! [`LoginLimits::max_failures`] failures of a key (or three times as many for an
//! address over all names) the key is locked for [`LoginLimits::lockout`]; every failure
//! after a lockout ends doubles it, up to an hour. While a key is locked its logins are
//! refused without looking at the password, whether it is right or not. A successful
//! login clears its address-and-name key (the address's count only ages out), and a key
//! with no failure for an hour is forgotten.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How failed logins are throttled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoginLimits {
    /// Failed logins of one client address and user name before they are locked out;
    /// a client address is locked after three times as many over all names. `0` turns
    /// throttling off.
    pub max_failures: u32,
    /// The first lockout; each further one doubles, up to [`MAX_LOCKOUT`].
    pub lockout: Duration,
}

impl Default for LoginLimits {
    fn default() -> Self {
        LoginLimits { max_failures: 10, lockout: Duration::from_secs(60) }
    }
}

/// The longest lockout, and how long a key is remembered after its last failure.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(3600);

/// Keys kept at most; beyond that, keys whose lockout has ended are forgotten first.
const MAX_KEYS: usize = 1 << 16;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    addr: IpAddr,
    /// `None`: the address over all names.
    user: Option<String>,
}

#[derive(Clone, Copy)]
struct Entry {
    failures: u32,
    locked_until: Option<Instant>,
    last: Instant,
}

/// Failed-login counts, shared by HTTP and Bolt.
pub(crate) struct Throttle {
    limits: LoginLimits,
    keys: Mutex<HashMap<Key, Entry>>,
}

/// The client address of a peer such as `10.0.0.7:51234` or `[::1]:51234`; `None` if it
/// is not one (then it is not throttled).
pub(crate) fn peer_addr(peer: &str) -> Option<IpAddr> {
    peer.parse::<SocketAddr>().map(|a| a.ip()).ok().or_else(|| peer.parse().ok())
}

impl Throttle {
    pub(crate) fn new(limits: LoginLimits) -> Throttle {
        Throttle { limits, keys: Mutex::new(HashMap::new()) }
    }

    fn on(&self) -> bool {
        self.limits.max_failures > 0
    }

    fn keys(addr: IpAddr, user: &str) -> [Key; 2] {
        [Key { addr, user: Some(user.to_string()) }, Key { addr, user: None }]
    }

    /// How long logins from `addr` as `user` stay refused; `None` if they may be tried.
    pub(crate) fn locked(&self, addr: Option<IpAddr>, user: &str, now: Instant) -> Option<Duration> {
        let addr = addr.filter(|_| self.on())?;
        let keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        Self::keys(addr, user)
            .iter()
            .filter_map(|k| keys.get(k)?.locked_until)
            .filter(|&until| until > now)
            .max()
            .map(|until| until - now)
    }

    /// Records a failed login; returns how long the client is now locked out, if it is.
    pub(crate) fn failed(&self, addr: Option<IpAddr>, user: &str, now: Instant) -> Option<Duration> {
        let addr = addr.filter(|_| self.on())?;
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if keys.len() >= MAX_KEYS {
            keys.retain(|_, e| now.saturating_duration_since(e.last) < MAX_LOCKOUT);
        }
        if keys.len() >= MAX_KEYS {
            keys.retain(|_, e| e.locked_until.is_some_and(|until| until > now));
        }
        let mut longest = None;
        for key in Self::keys(addr, user) {
            let limit = if key.user.is_some() { self.limits.max_failures } else { self.limits.max_failures.saturating_mul(3) };
            let e = keys.entry(key).or_insert(Entry { failures: 0, locked_until: None, last: now });
            if now.saturating_duration_since(e.last) >= MAX_LOCKOUT {
                *e = Entry { failures: 0, locked_until: None, last: now };
            }
            e.failures = e.failures.saturating_add(1);
            e.last = now;
            if e.failures >= limit {
                let doublings = (e.failures - limit).min(16);
                let lockout = self.limits.lockout.saturating_mul(1 << doublings).min(MAX_LOCKOUT);
                e.locked_until = Some(now + lockout);
                longest = longest.max(Some(lockout));
            }
        }
        longest
    }

    /// Records a successful login: clears the failures of `addr` as `user`.
    pub(crate) fn succeeded(&self, addr: Option<IpAddr>, user: &str) {
        let Some(addr) = addr.filter(|_| self.on()) else { return };
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        keys.remove(&Key { addr, user: Some(user.to_string()) });
    }
}

/// Whole seconds to wait, at least 1 (for `Retry-After` and messages).
pub(crate) fn seconds(d: Duration) -> u64 {
    (d.as_secs() + u64::from(d.subsec_nanos() > 0)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "10.0.0.7:5000";
    const B: &str = "10.0.0.8:5000";

    fn throttle(max: u32) -> Throttle {
        Throttle::new(LoginLimits { max_failures: max, lockout: Duration::from_secs(60) })
    }

    #[test]
    fn peer_addresses() {
        assert_eq!(peer_addr("10.0.0.7:51234"), Some("10.0.0.7".parse().unwrap()));
        assert_eq!(peer_addr("[::1]:51234"), Some("::1".parse().unwrap()));
        assert_eq!(peer_addr("::1"), Some("::1".parse().unwrap()));
        assert_eq!(peer_addr("unknown"), None);
    }

    #[test]
    fn locks_after_max_failures_and_doubles() {
        let t = throttle(3);
        let now = Instant::now();
        let a = peer_addr(A);
        assert_eq!(t.failed(a, "ann", now), None);
        assert_eq!(t.failed(a, "ann", now), None);
        assert_eq!(t.locked(a, "ann", now), None);
        assert_eq!(t.failed(a, "ann", now), Some(Duration::from_secs(60)));
        assert_eq!(t.locked(a, "ann", now + Duration::from_secs(10)), Some(Duration::from_secs(50)));
        // Another address, and the same address as another user, are not locked.
        assert_eq!(t.locked(peer_addr(B), "ann", now), None);
        assert_eq!(t.locked(a, "bob", now), None);
        // After the lockout, one more failure locks for twice as long, then four times.
        let later = now + Duration::from_secs(61);
        assert_eq!(t.locked(a, "ann", later), None);
        assert_eq!(t.failed(a, "ann", later), Some(Duration::from_secs(120)));
        assert_eq!(t.failed(a, "ann", later), Some(Duration::from_secs(240)));
    }

    #[test]
    fn lockouts_stop_at_an_hour() {
        let t = throttle(1);
        let now = Instant::now();
        for _ in 0..40 {
            t.failed(peer_addr(A), "ann", now);
        }
        assert_eq!(t.locked(peer_addr(A), "ann", now), Some(MAX_LOCKOUT));
    }

    #[test]
    fn many_names_lock_the_address() {
        let t = throttle(3);
        let now = Instant::now();
        let a = peer_addr(A);
        for i in 0..8 {
            assert_eq!(t.failed(a, &format!("user{i}"), now), None, "{i}");
        }
        assert_eq!(t.failed(a, "user8", now), Some(Duration::from_secs(60)));
        assert!(t.locked(a, "anyone", now).is_some());
        assert_eq!(t.locked(peer_addr(B), "anyone", now), None);
    }

    #[test]
    fn success_clears_the_name_and_failures_age_out() {
        let t = throttle(3);
        let now = Instant::now();
        let a = peer_addr(A);
        t.failed(a, "ann", now);
        t.failed(a, "ann", now);
        t.succeeded(a, "ann");
        assert_eq!(t.failed(a, "ann", now), None);
        assert_eq!(t.failed(a, "ann", now), None);
        // An hour without failures starts the count again.
        let later = now + MAX_LOCKOUT;
        assert_eq!(t.failed(a, "ann", later), None);
        assert_eq!(t.failed(a, "ann", later), None);
        assert_eq!(t.failed(a, "ann", later), Some(Duration::from_secs(60)));
    }

    #[test]
    fn off_and_unknown_peers() {
        let t = throttle(0);
        let now = Instant::now();
        for _ in 0..100 {
            assert_eq!(t.failed(peer_addr(A), "ann", now), None);
        }
        let t = throttle(1);
        assert_eq!(t.failed(None, "ann", now), None);
        assert_eq!(t.locked(None, "ann", now), None);
    }

    #[test]
    fn whole_seconds() {
        assert_eq!(seconds(Duration::ZERO), 1);
        assert_eq!(seconds(Duration::from_millis(1)), 1);
        assert_eq!(seconds(Duration::from_secs(60)), 60);
        assert_eq!(seconds(Duration::from_millis(60_001)), 61);
    }
}
