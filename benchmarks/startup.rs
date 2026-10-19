//! Benchmarks the startup of an undaemonized (standalone) polymodo launcher.
//!
//! Each run opens the launcher window, which quits by itself once it has fully loaded (first
//! frame rendered, all desktop entries found and all icons loaded) and a test query has been
//! rendered. It then prints its timings (see `src/timing.rs`), and every metric becomes a
//! benchmark here. This needs a running Wayland session, and the window will briefly appear.
//!
//! ```sh
//! cargo bench --bench startup                          # all metrics
//! cargo bench --bench startup -- fully_loaded          # a single metric
//! cargo bench --bench startup -- --save-baseline before
//! cargo bench --bench startup -- --baseline before     # compare against a saved baseline
//! ```
//!
//! Set `POLYMODO_BENCH_BIN` to benchmark another polymodo binary instead of this crate's.
//!
//! The machine's load affects the absolute numbers a lot; compare runs made back to back.

use criterion::{Criterion, criterion_group, criterion_main};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

/// The metrics reported by polymodo in bench mode, all in milliseconds.
/// Milestones are measured from process start; the others are total time spent in a hot path.
const METRICS: &[&str] = &[
    // milestones
    "first_frame",
    "entries_scanned",
    "fully_loaded",
    "final_frame",
    // latency of a search query, from setting it to its results being rendered
    "query_frame",
    // time spent in hot paths
    "new_entries",
    "search_update",
    "icon_lookup",
    "icon_read",
    "icon_decode",
];

/// How many samples to measure for each metric. Since every run of polymodo yields a sample of
/// every metric, this is also (about) the total number of runs.
const SAMPLES: usize = 20;
/// Bail out if, somehow, criterion asks for far more runs than expected.
const MAX_RUNS: u32 = 100;

/// Runs polymodo, and hands out the metrics of each run.
///
/// Every run measures all metrics at once, so the samples of a run are queued up for the
/// benchmarks of the other metrics, instead of running polymodo again for each of them.
struct Runner {
    binary: String,
    samples: HashMap<&'static str, VecDeque<Duration>>,
    runs: u32,
}

impl Runner {
    fn new() -> Self {
        let binary = std::env::var("POLYMODO_BENCH_BIN")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_polymodo").to_string());

        Self {
            binary,
            samples: METRICS.iter().map(|&m| (m, VecDeque::new())).collect(),
            runs: 0,
        }
    }

    /// Run polymodo once, queueing up the reported metrics.
    fn run(&mut self) {
        self.runs += 1;
        assert!(self.runs <= MAX_RUNS, "ran polymodo more than {MAX_RUNS} times");

        let output = Command::new(&self.binary)
            .arg("--standalone")
            .env("POLYMODO_BENCH", "1")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", self.binary));

        let stdout = String::from_utf8_lossy(&output.stdout);
        let report = stdout
            .lines()
            .find_map(|line| line.strip_prefix("POLYMODO_BENCH "))
            .unwrap_or_else(|| {
                panic!(
                    "polymodo ({}) didn't report any timings; is a Wayland session running?",
                    output.status
                )
            });

        // fields are `key=value`, or `key=total/count`; values are in milliseconds
        for (key, value) in report.split_whitespace().filter_map(|f| f.split_once('=')) {
            let Some(samples) = self.samples.get_mut(key) else {
                continue; // not a duration (e.g. a count of entries)
            };
            let millis = value.split('/').next().and_then(|v| v.parse::<f64>().ok());
            let millis = millis.unwrap_or_else(|| panic!("bad value for {key}: {value}"));

            samples.push_back(Duration::from_secs_f64(millis / 1000.0));
        }
    }

    /// A sample of a metric, without consuming it.
    fn peek(&mut self, metric: &str) -> Duration {
        let sample = self.next(metric);
        self.samples.get_mut(metric).unwrap().push_front(sample);
        sample
    }

    /// The next sample of a metric, running polymodo if none are queued up.
    fn next(&mut self, metric: &str) -> Duration {
        loop {
            if let Some(sample) = self.samples.get_mut(metric).and_then(VecDeque::pop_front) {
                return sample;
            }
            self.run();
        }
    }
}

fn startup(c: &mut Criterion) {
    let runner = Rc::new(RefCell::new(Runner::new()));

    let mut group = c.benchmark_group("startup");
    // Criterion decides how many iterations to run based on how long they take on the wall clock,
    // which is meaningless here: a run takes much longer than any metric it reports, and samples
    // queued up from an earlier run are handed out instantly. So force exactly one iteration
    // (= one sample) per measurement, with a warm-up of a single iteration.
    // (Criterion warns that it's "unable to complete" the samples in time; that's expected.)
    group
        .sample_size(SAMPLES)
        .sampling_mode(criterion::SamplingMode::Flat)
        .warm_up_time(Duration::from_nanos(1))
        .measurement_time(Duration::from_nanos(1));

    for &metric in METRICS {
        if runner.borrow_mut().peek(metric).is_zero() {
            eprintln!("skipping {metric}: polymodo reports no time spent in it");
            continue;
        }

        let runner = runner.clone();
        group.bench_function(metric, move |b| {
            b.iter_custom(|iters| (0..iters).map(|_| runner.borrow_mut().next(metric)).sum());
        });
    }

    group.finish();
    eprintln!("ran polymodo {} times", runner.borrow().runs);
}

criterion_group!(benches, startup);
criterion_main!(benches);
