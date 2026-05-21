//! Injectable time source.
//!
//! Core logic that needs "now" takes a [`Clock`] so tests are deterministic and never sleep on
//! the wall clock (PLAN §1, §5). Production wires [`SystemClock`]; tests wire [`TestClock`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. Used as the timestamp on a
/// [`crate::history::HistoryTurn`] so history is serializable and clock-independent.
pub type UnixMillis = u64;

/// A source of the current time. `Send + Sync` so it can be shared across the actor's tasks.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current time in milliseconds since the Unix epoch.
    fn now_ms(&self) -> UnixMillis;
}

/// Wall-clock implementation backed by [`SystemTime`].
#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> UnixMillis {
        // Before the epoch is not representable here; clamp to 0 rather than panic.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

/// Deterministic clock for tests. Time only advances when the test advances it.
///
/// Cloning shares the same underlying counter, so a clone handed to the code under test and the
/// handle the test holds stay in lock-step.
#[derive(Debug, Clone, Default)]
pub struct TestClock {
    now: Arc<AtomicU64>,
}

impl TestClock {
    /// Create a clock reading `start_ms`.
    #[must_use]
    pub fn new(start_ms: UnixMillis) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(start_ms)),
        }
    }

    /// Move time forward by `delta_ms` and return the new value.
    pub fn advance(&self, delta_ms: u64) -> UnixMillis {
        self.now.fetch_add(delta_ms, Ordering::SeqCst) + delta_ms
    }

    /// Set the absolute current time.
    pub fn set(&self, now_ms: UnixMillis) {
        self.now.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> UnixMillis {
        self.now.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_advances_only_when_told() {
        let c = TestClock::new(1_000);
        assert_eq!(c.now_ms(), 1_000);
        assert_eq!(c.advance(500), 1_500);
        assert_eq!(c.now_ms(), 1_500);
        c.set(42);
        assert_eq!(c.now_ms(), 42);
    }

    #[test]
    fn test_clock_clone_shares_counter() {
        let a = TestClock::new(0);
        let b = a.clone();
        a.advance(10);
        assert_eq!(b.now_ms(), 10);
    }

    #[test]
    fn system_clock_is_after_2020() {
        // 2020-01-01T00:00:00Z in ms — a sanity floor, not a tight assertion.
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }
}
