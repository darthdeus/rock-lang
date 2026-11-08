//! The shared monotonic clock: `std::time::Instant` on native targets and a
//! zero clock on wasm.

#[cfg(not(target_family = "wasm"))]
pub use native_instant::Instant;

// Clippy follows re-exports, so wrap the native clock to keep the ban local.
#[cfg(not(target_family = "wasm"))]
#[allow(clippy::disallowed_types)]
mod native_instant {
    use std::time::Duration;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(std::time::Instant);

    impl Instant {
        pub fn now() -> Self {
            Self(std::time::Instant::now())
        }
        pub fn elapsed(&self) -> Duration {
            self.0.elapsed()
        }
        pub fn duration_since(&self, earlier: Self) -> Duration {
            self.0.duration_since(earlier.0)
        }
        pub fn saturating_duration_since(&self, earlier: Self) -> Duration {
            self.0.saturating_duration_since(earlier.0)
        }
    }

    impl std::ops::Sub for Instant {
        type Output = Duration;
        fn sub(self, earlier: Self) -> Duration {
            self.0 - earlier.0
        }
    }

    impl std::ops::Add<Duration> for Instant {
        type Output = Self;
        fn add(self, duration: Duration) -> Self {
            Self(self.0 + duration)
        }
    }
}
#[cfg(target_family = "wasm")]
pub use wasm_instant::Instant;

// Wasm has no host clock contract yet; zero timings avoid panics and JS imports.
#[cfg(target_family = "wasm")]
mod wasm_instant {
    use std::time::Duration;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant;

    impl Instant {
        pub fn now() -> Self {
            Self
        }
        pub fn elapsed(&self) -> Duration {
            Duration::ZERO
        }
        pub fn duration_since(&self, _: Self) -> Duration {
            Duration::ZERO
        }
        pub fn saturating_duration_since(&self, _: Self) -> Duration {
            Duration::ZERO
        }
    }

    impl std::ops::Sub for Instant {
        type Output = Duration;
        fn sub(self, _: Self) -> Duration {
            Duration::ZERO
        }
    }

    impl std::ops::Add<Duration> for Instant {
        type Output = Self;
        fn add(self, _: Duration) -> Self {
            self
        }
    }
}
