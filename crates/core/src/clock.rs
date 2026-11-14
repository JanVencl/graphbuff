//! The clocks the engine reads.
//!
//! Natively these are the ones of `std::time`. In a browser (`wasm32-unknown-unknown`)
//! `std` has no clock, so the page that instantiates the module supplies two functions in
//! its `env` imports: `graphbuff_unix_ms` (milliseconds since the Unix epoch, `Date.now()`)
//! and `graphbuff_monotonic_ms` (milliseconds on a clock that never goes back,
//! `performance.now()`).

use std::time::Duration;

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub use std::time::Instant;

/// The time since the Unix epoch; `None` if the system clock is set before it.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub fn unix_time() -> Option<Duration> {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use browser::{Instant, unix_time};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod browser {
    use super::Duration;

    #[link(wasm_import_module = "env")]
    unsafe extern "C" {
        safe fn graphbuff_unix_ms() -> f64;
        safe fn graphbuff_monotonic_ms() -> f64;
    }

    fn millis(ms: f64) -> Duration {
        Duration::try_from_secs_f64(ms / 1e3).unwrap_or_default()
    }

    pub fn unix_time() -> Option<Duration> {
        Some(millis(graphbuff_unix_ms()))
    }

    /// The part of `std::time::Instant` the engine uses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Instant(Duration);

    impl Instant {
        pub fn now() -> Instant {
            Instant(millis(graphbuff_monotonic_ms()))
        }

        pub fn elapsed(&self) -> Duration {
            Instant::now().saturating_duration_since(*self)
        }

        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.0.saturating_sub(earlier.0)
        }
    }

    impl std::ops::Add<Duration> for Instant {
        type Output = Instant;

        fn add(self, rhs: Duration) -> Instant {
            Instant(self.0.saturating_add(rhs))
        }
    }
}
