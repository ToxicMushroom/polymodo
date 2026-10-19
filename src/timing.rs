//! Startup timing instrumentation.
//!
//! Spans and milestones are logged at `debug` level under the `polymodo::timing` target, e.g.
//! `RUST_LOG=polymodo::timing=debug polymodo --standalone`.
//!
//! Setting `POLYMODO_BENCH=1` makes the launcher quit as soon as it has fully loaded
//! (first frame rendered, all entries found and all icons loaded), printing a one-line summary of
//! the milestones to stdout. See `benchmarks/startup.rs` (`cargo bench --bench startup`).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();

/// Record the start of the process. Should be called as early as possible in `main`.
pub fn init() {
    START.get_or_init(Instant::now);
}

/// Time elapsed since [init] was called.
pub fn since_start() -> Duration {
    START.get_or_init(Instant::now).elapsed()
}

/// Whether bench mode (`POLYMODO_BENCH`) is enabled.
pub fn bench_mode() -> bool {
    static BENCH: OnceLock<bool> = OnceLock::new();
    *BENCH.get_or_init(|| std::env::var_os("POLYMODO_BENCH").is_some_and(|v| v != "0"))
}

/// Log that a milestone has been reached, along with the time since process start.
pub fn milestone(name: &'static str) {
    tracing::debug!(target: "polymodo::timing", "{name}: reached at {:.2?}", since_start());
}

/// A timed region of code; logs its duration when dropped.
#[must_use = "a span measures until it is dropped"]
pub struct Span {
    name: &'static str,
    start: Instant,
}

impl Drop for Span {
    fn drop(&mut self) {
        tracing::debug!(
            target: "polymodo::timing",
            "{}: took {:.2?} (ended at {:.2?})",
            self.name,
            self.start.elapsed(),
            since_start()
        );
    }
}

/// Start a timed span. It ends when the returned value is dropped.
#[inline]
pub fn span(name: &'static str) -> Span {
    Span {
        name,
        start: Instant::now(),
    }
}

/// Time a closure.
#[inline]
pub fn time<R>(name: &'static str, f: impl FnOnce() -> R) -> R {
    let _span = span(name);
    f()
}

/// Accumulates time (and the number of measurements) spent in a region that runs many times.
pub struct Accumulator {
    nanos: std::sync::atomic::AtomicU64,
    count: std::sync::atomic::AtomicU64,
}

impl Accumulator {
    pub const fn new() -> Self {
        Self {
            nanos: std::sync::atomic::AtomicU64::new(0),
            count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Time a closure, adding its duration to this accumulator.
    #[inline]
    pub fn time<R>(&self, f: impl FnOnce() -> R) -> R {
        let start = Instant::now();
        let r = f();
        self.add(start.elapsed());
        r
    }

    /// Add a single measurement.
    #[inline]
    pub fn add(&self, duration: Duration) {
        use std::sync::atomic::Ordering::Relaxed;

        self.nanos.fetch_add(duration.as_nanos() as u64, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    pub fn total(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn count(&self) -> u64 {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }
}
