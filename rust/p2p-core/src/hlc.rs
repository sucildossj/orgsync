//! Hybrid logical clock.
//!
//! Every replicated cell carries an [`Hlc`] plus the id of the device that
//! wrote it. Together those form a total order across the whole organisation,
//! which is what lets last-writer-wins converge without a coordinator.
//!
//! The wall-clock component keeps the order close to real time (so "latest
//! edit wins" matches what a human expects), while the counter guarantees
//! progress even when two writes land in the same millisecond or when a
//! device's clock is skewed or running backwards.

use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// How far ahead of our own clock we will accept a remote timestamp before
/// treating it as a faulty clock. We still accept the write (dropping it would
/// break convergence) but we refuse to drag our own clock along with it.
pub const MAX_CLOCK_DRIFT_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
pub struct Hlc {
    /// Milliseconds since the Unix epoch.
    pub wall_ms: u64,
    /// Disambiguates writes inside the same millisecond.
    pub counter: u32,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc { wall_ms: 0, counter: 0 };

    pub fn new(wall_ms: u64, counter: u32) -> Self {
        Self { wall_ms, counter }
    }

    /// Encodes to a lexicographically sortable 24-char hex string, which makes
    /// the value directly comparable inside SQL as well as in Rust.
    pub fn to_hex(self) -> String {
        format!("{:016x}{:08x}", self.wall_ms, self.counter)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 24 {
            return None;
        }
        Some(Self {
            wall_ms: u64::from_str_radix(&s[0..16], 16).ok()?,
            counter: u32::from_str_radix(&s[16..24], 16).ok()?,
        })
    }
}

impl std::fmt::Display for Hlc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.wall_ms, self.counter)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A clock that can be shared across the sync tasks and the local write path.
#[derive(Debug, Clone)]
pub struct HybridClock {
    last: Arc<Mutex<Hlc>>,
}

impl Default for HybridClock {
    fn default() -> Self {
        Self::new()
    }
}

impl HybridClock {
    pub fn new() -> Self {
        Self { last: Arc::new(Mutex::new(Hlc::ZERO)) }
    }

    /// Resumes from the highest timestamp already durable on disk, so a
    /// restart can never re-issue a timestamp we have previously handed out.
    pub fn resuming_from(seen: Hlc) -> Self {
        Self { last: Arc::new(Mutex::new(seen)) }
    }

    pub fn peek(&self) -> Hlc {
        *self.last.lock().expect("hlc mutex poisoned")
    }

    /// Stamps a local write. Monotonic even if the system clock jumps backwards.
    pub fn tick(&self) -> Hlc {
        let mut last = self.last.lock().expect("hlc mutex poisoned");
        let phys = now_ms();
        let next = if phys > last.wall_ms {
            Hlc { wall_ms: phys, counter: 0 }
        } else {
            Hlc { wall_ms: last.wall_ms, counter: last.counter.saturating_add(1) }
        };
        *last = next;
        next
    }

    /// Folds a timestamp observed from a peer into our clock so that any write
    /// we make afterwards sorts after everything we have already seen.
    ///
    /// A remote timestamp implausibly far in the future is honoured for
    /// ordering but not absorbed, otherwise one device with a broken clock
    /// would poison every clock in the organisation permanently.
    pub fn observe(&self, remote: Hlc) -> Hlc {
        let mut last = self.last.lock().expect("hlc mutex poisoned");
        let phys = now_ms();

        if remote.wall_ms > phys.saturating_add(MAX_CLOCK_DRIFT_MS) {
            tracing::warn!(
                remote = %remote,
                local_ms = phys,
                "peer clock is beyond the accepted drift window; not absorbing it"
            );
            let next = if phys > last.wall_ms {
                Hlc { wall_ms: phys, counter: 0 }
            } else {
                Hlc { wall_ms: last.wall_ms, counter: last.counter.saturating_add(1) }
            };
            *last = next;
            return next;
        }

        let max_wall = phys.max(last.wall_ms).max(remote.wall_ms);
        let next = if max_wall == last.wall_ms && max_wall == remote.wall_ms {
            Hlc { wall_ms: max_wall, counter: last.counter.max(remote.counter).saturating_add(1) }
        } else if max_wall == last.wall_ms {
            Hlc { wall_ms: max_wall, counter: last.counter.saturating_add(1) }
        } else if max_wall == remote.wall_ms {
            Hlc { wall_ms: max_wall, counter: remote.counter.saturating_add(1) }
        } else {
            Hlc { wall_ms: max_wall, counter: 0 }
        };
        *last = next;
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_is_strictly_monotonic() {
        let c = HybridClock::new();
        let mut prev = c.tick();
        for _ in 0..5_000 {
            let next = c.tick();
            assert!(next > prev, "{next:?} must sort after {prev:?}");
            prev = next;
        }
    }

    #[test]
    fn observe_sorts_after_a_future_peer() {
        let c = HybridClock::new();
        let remote = Hlc { wall_ms: now_ms() + 5_000, counter: 7 };
        c.observe(remote);
        assert!(c.tick() > remote, "our next write must beat the peer's");
    }

    #[test]
    fn a_wildly_skewed_peer_does_not_poison_our_clock() {
        let c = HybridClock::new();
        let insane = Hlc { wall_ms: now_ms() + 10 * 365 * 24 * 3_600_000, counter: 0 };
        c.observe(insane);
        // We keep ticking near real time rather than a decade in the future.
        assert!(c.tick().wall_ms < now_ms() + MAX_CLOCK_DRIFT_MS);
    }

    #[test]
    fn hex_round_trips_and_sorts_like_the_struct() {
        let a = Hlc::new(1_700_000_000_123, 4);
        let b = Hlc::new(1_700_000_000_123, 5);
        let c = Hlc::new(1_700_000_000_124, 0);
        assert_eq!(Hlc::from_hex(&a.to_hex()), Some(a));
        assert!(a.to_hex() < b.to_hex() && b.to_hex() < c.to_hex());
        assert!(a < b && b < c);
    }
}
