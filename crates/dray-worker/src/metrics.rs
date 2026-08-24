//! Prometheus metrics, in the text exposition format.
//!
//! Written by hand rather than pulled from a crate. The set of instruments a
//! worker needs is small and fixed, the exposition format is a few lines of
//! text, and a metrics client is a dependency that runs in every process and
//! whose failure modes matter more than what it measures. The tests below
//! assert the output against the format's actual rules, which is the part a
//! library would otherwise be trusted for.
//!
//! # What is measured, and why each one earns its place
//!
//! | Metric | The question it answers |
//! |---|---|
//! | Queue depth | Is the pool keeping up? |
//! | Oldest lease age | Is a worker stuck holding a job? |
//! | Proving duration | Has proving got slower, and is the wall clock still generous? |
//! | Peak memory | Is the address-space ceiling still generous, or about to start failing healthy jobs? |
//! | Timeouts and OOMs, separately | Which bound is being hit — they have opposite fixes |
//! | Attempts per job | Is work succeeding first time, or churning? |
//!
//! Timeouts and out-of-memory kills are counted separately and deliberately.
//! Aggregated into one "resource failure" number they are indistinguishable,
//! and the response to each is the opposite of the other: a timeout usually
//! means give it longer or more CPU, an OOM means give it more memory or make
//! the job smaller.

use std::{
    fmt::Write as _,
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    time::Duration,
};

/// A monotonically increasing count.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A value that can go up and down.
#[derive(Debug, Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub fn set(&self, value: i64) {
        self.0.store(value, Ordering::Relaxed);
    }

    #[must_use]
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A cumulative histogram over fixed bucket boundaries.
///
/// Buckets are cumulative, as Prometheus requires: the count for `le="2"`
/// includes everything in `le="1"`. Getting that wrong produces quantiles that
/// look plausible and are wrong, which is worse than no histogram at all.
#[derive(Debug)]
pub struct Histogram {
    bounds: &'static [f64],
    /// One counter per bound, plus one for `+Inf`.
    counts: Vec<AtomicU64>,
    sum: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    #[must_use]
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            // Stored as milli-units in an integer, because f64 has no atomic
            // and a lock here would be contended by every worker thread.
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, value: f64) {
        let index = self
            .bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(self.bounds.len());

        if let Some(bucket) = self.counts.get(index) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum
            .fetch_add((value.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn sum(&self) -> f64 {
        self.sum.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Cumulative counts, one per bound then `+Inf`.
    fn cumulative(&self) -> Vec<u64> {
        let mut running = 0;
        self.counts
            .iter()
            .map(|bucket| {
                running += bucket.load(Ordering::Relaxed);
                running
            })
            .collect()
    }
}

/// Proving takes roughly 2.5 s, bounded at 120 s. The buckets cluster around
/// the normal case and then spread out, so a slow-down is visible long before
/// it starts tripping the wall clock.
const DURATION_BUCKETS: &[f64] = &[0.5, 1.0, 2.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// Measured peak is ~42 MB for `bb` and ~75 MB for `nargo`, against a 4 GiB
/// ceiling. The upper buckets exist to show the margin being eaten, not because
/// anything is expected up there.
const MEMORY_BUCKETS_KB: &[f64] = &[
    32_768.0,
    65_536.0,
    131_072.0,
    262_144.0,
    524_288.0,
    1_048_576.0,
    2_097_152.0,
    4_194_304.0,
];

/// The default budget is three attempts, so anything past that is a job that
/// keeps coming back.
const ATTEMPT_BUCKETS: &[f64] = &[1.0, 2.0, 3.0, 5.0, 10.0];

/// Failure reasons, fixed at compile time.
///
/// A fixed set rather than an open map: Prometheus label values that grow
/// without bound turn a metric into a memory leak, and the reasons are exactly
/// the variants of `BoundedError` and `ProveError` anyway.
const FAILURE_REASONS: &[&str] = &[
    "timeout",
    "oom",
    "cpu_exhausted",
    "killed",
    "nonzero_exit",
    "spawn_failed",
    "bad_inputs",
    "unknown_circuit",
    "scratch_failed",
    "missing_artefact",
];

const OUTCOMES: &[&str] = &["proved", "failed", "lease_lost", "abandoned"];

/// Everything a worker reports.
#[derive(Debug)]
pub struct Metrics {
    worker_id: String,

    outcomes: Vec<(&'static str, Counter)>,
    failures: Vec<(&'static str, Counter)>,

    pub proving_duration: Histogram,
    pub peak_memory_kb: Histogram,
    pub attempts: Histogram,

    pub queue_depth: Gauge,
    pub oldest_lease_age: Gauge,
    pub leases_reaped: Counter,
}

impl Metrics {
    #[must_use]
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            outcomes: OUTCOMES
                .iter()
                .map(|name| (*name, Counter::default()))
                .collect(),
            failures: FAILURE_REASONS
                .iter()
                .map(|name| (*name, Counter::default()))
                .collect(),
            proving_duration: Histogram::new(DURATION_BUCKETS),
            peak_memory_kb: Histogram::new(MEMORY_BUCKETS_KB),
            attempts: Histogram::new(ATTEMPT_BUCKETS),
            queue_depth: Gauge::default(),
            oldest_lease_age: Gauge::default(),
            leases_reaped: Counter::default(),
        }
    }

    /// Count an attempt's outcome. Unknown names are ignored rather than
    /// registered, keeping the label set bounded.
    pub fn record_outcome(&self, outcome: &str) {
        if let Some((_, counter)) = self.outcomes.iter().find(|(name, _)| *name == outcome) {
            counter.increment();
        }
    }

    /// Count a failure by reason.
    pub fn record_failure(&self, reason: &str) {
        if let Some((_, counter)) = self.failures.iter().find(|(name, _)| *name == reason) {
            counter.increment();
        } else {
            debug_assert!(false, "unregistered failure reason: {reason}");
        }
    }

    pub fn record_proof(&self, duration: Duration, peak_memory_kb: Option<u64>, attempts: i32) {
        self.proving_duration.observe(duration.as_secs_f64());
        if let Some(kb) = peak_memory_kb {
            self.peak_memory_kb.observe(kb as f64);
        }
        self.attempts.observe(f64::from(attempts.max(0)));
    }

    /// Render the Prometheus text exposition format.
    #[must_use]
    pub fn render(&self) -> String {
        let worker = escape_label(&self.worker_id);
        let mut out = String::with_capacity(2048);

        out.push_str("# HELP dray_worker_attempts_total Attempts finished, by outcome.\n");
        out.push_str("# TYPE dray_worker_attempts_total counter\n");
        for (outcome, counter) in &self.outcomes {
            let _ = writeln!(
                out,
                "dray_worker_attempts_total{{worker=\"{worker}\",outcome=\"{outcome}\"}} {}",
                counter.get()
            );
        }

        out.push_str("# HELP dray_worker_failures_total Failed attempts, by reason.\n");
        out.push_str("# TYPE dray_worker_failures_total counter\n");
        for (reason, counter) in &self.failures {
            let _ = writeln!(
                out,
                "dray_worker_failures_total{{worker=\"{worker}\",reason=\"{reason}\"}} {}",
                counter.get()
            );
        }

        render_histogram(
            &mut out,
            "dray_worker_proving_duration_seconds",
            "How long a successful proof took.",
            &worker,
            &self.proving_duration,
        );
        render_histogram(
            &mut out,
            "dray_worker_peak_memory_kilobytes",
            "Peak resident set size of a proving subprocess.",
            &worker,
            &self.peak_memory_kb,
        );
        render_histogram(
            &mut out,
            "dray_worker_attempts_per_job",
            "How many attempts a job needed before it succeeded.",
            &worker,
            &self.attempts,
        );

        out.push_str("# HELP dray_queue_depth Jobs waiting to be leased.\n");
        out.push_str("# TYPE dray_queue_depth gauge\n");
        let _ = writeln!(
            out,
            "dray_queue_depth{{worker=\"{worker}\"}} {}",
            self.queue_depth.get()
        );

        out.push_str(
            "# HELP dray_oldest_lease_age_seconds Age of the longest-held lease, or 0 if none.\n",
        );
        out.push_str("# TYPE dray_oldest_lease_age_seconds gauge\n");
        let _ = writeln!(
            out,
            "dray_oldest_lease_age_seconds{{worker=\"{worker}\"}} {}",
            self.oldest_lease_age.get()
        );

        out.push_str("# HELP dray_leases_reaped_total Expired leases returned to the queue.\n");
        out.push_str("# TYPE dray_leases_reaped_total counter\n");
        let _ = writeln!(
            out,
            "dray_leases_reaped_total{{worker=\"{worker}\"}} {}",
            self.leases_reaped.get()
        );

        out
    }
}

fn render_histogram(out: &mut String, name: &str, help: &str, worker: &str, histogram: &Histogram) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");

    let cumulative = histogram.cumulative();
    for (index, bound) in histogram.bounds.iter().enumerate() {
        let _ = writeln!(
            out,
            "{name}_bucket{{worker=\"{worker}\",le=\"{bound}\"}} {}",
            cumulative.get(index).copied().unwrap_or(0)
        );
    }
    let _ = writeln!(
        out,
        "{name}_bucket{{worker=\"{worker}\",le=\"+Inf\"}} {}",
        histogram.count()
    );
    let _ = writeln!(out, "{name}_sum{{worker=\"{worker}\"}} {}", histogram.sum());
    let _ = writeln!(
        out,
        "{name}_count{{worker=\"{worker}\"}} {}",
        histogram.count()
    );
}

/// Escape a label value per the exposition format.
///
/// The worker id can come from the environment, so an unescaped quote in it
/// would produce a scrape body Prometheus rejects — and the failure would look
/// like the worker being down rather than like a bad name.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_counts() {
        let counter = Counter::default();
        assert_eq!(counter.get(), 0);
        counter.increment();
        counter.add(4);
        assert_eq!(counter.get(), 5);
    }

    #[test]
    fn a_gauge_goes_both_ways() {
        let gauge = Gauge::default();
        gauge.set(10);
        assert_eq!(gauge.get(), 10);
        gauge.set(-3);
        assert_eq!(gauge.get(), -3);
    }

    /// Prometheus buckets are cumulative. Reporting per-bucket counts instead
    /// produces quantiles that look reasonable and are wrong.
    #[test]
    fn histogram_buckets_are_cumulative() {
        let histogram = Histogram::new(&[1.0, 2.0, 5.0]);
        histogram.observe(0.5);
        histogram.observe(1.5);
        histogram.observe(4.0);
        histogram.observe(100.0);

        assert_eq!(histogram.cumulative(), vec![1, 2, 3, 4]);
        assert_eq!(histogram.count(), 4);
    }

    #[test]
    fn a_value_exactly_on_a_boundary_falls_in_that_bucket() {
        // `le` means less than *or equal*.
        let histogram = Histogram::new(&[1.0, 2.0]);
        histogram.observe(1.0);
        assert_eq!(histogram.cumulative()[0], 1, "1.0 belongs in le=1");
    }

    #[test]
    fn a_value_above_every_bound_only_counts_in_inf() {
        let histogram = Histogram::new(&[1.0, 2.0]);
        histogram.observe(500.0);

        let cumulative = histogram.cumulative();
        assert_eq!(cumulative[0], 0);
        assert_eq!(cumulative[1], 0);
        assert_eq!(histogram.count(), 1);
    }

    #[test]
    fn the_histogram_sum_survives_the_integer_encoding() {
        let histogram = Histogram::new(&[1.0, 10.0]);
        histogram.observe(2.5);
        histogram.observe(0.125);
        assert!(
            (histogram.sum() - 2.625).abs() < 0.001,
            "sum was {}",
            histogram.sum()
        );
    }

    // ---- exposition format -------------------------------------------------

    /// Every metric must be declared before it is used, every TYPE line must
    /// match a HELP line, and every sample line must belong to something
    /// declared. A scrape body that breaks these is rejected wholesale, so the
    /// symptom is "the worker has no metrics" rather than "one metric is off".
    #[test]
    fn the_exposition_is_well_formed() {
        let metrics = Metrics::new("worker-1");
        metrics.record_outcome("proved");
        metrics.record_failure("timeout");
        metrics.record_proof(Duration::from_millis(2500), Some(43_008), 1);
        metrics.queue_depth.set(7);

        let rendered = metrics.render();

        let mut declared_help = std::collections::HashSet::new();
        let mut declared_type = std::collections::HashSet::new();

        for line in rendered.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest.split_once(' ').expect("HELP needs a description");
                assert!(!help.trim().is_empty(), "empty help for {name}");
                assert!(
                    declared_help.insert(name.to_owned()),
                    "{name} declared twice"
                );
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("TYPE needs a kind");
                assert!(
                    ["counter", "gauge", "histogram"].contains(&kind),
                    "unknown metric type {kind}"
                );
                assert!(declared_type.insert(name.to_owned()), "{name} typed twice");
            } else {
                assert!(!line.starts_with('#'), "unknown comment line: {line}");
                let (series, value) = line.rsplit_once(' ').expect("a sample needs a value");
                value
                    .parse::<f64>()
                    .unwrap_or_else(|_| panic!("{value:?} is not a number, in: {line}"));

                let base = series.split('{').next().expect("a metric name");
                let root = base
                    .trim_end_matches("_bucket")
                    .trim_end_matches("_sum")
                    .trim_end_matches("_count");
                assert!(
                    declared_help.contains(base) || declared_help.contains(root),
                    "sample {base} was never declared"
                );
            }
        }

        assert_eq!(
            declared_help, declared_type,
            "HELP and TYPE declarations disagree"
        );
    }

    #[test]
    fn outcomes_and_failures_appear_with_their_labels() {
        let metrics = Metrics::new("worker-1");
        metrics.record_outcome("proved");
        metrics.record_outcome("proved");
        metrics.record_failure("oom");

        let rendered = metrics.render();
        assert!(
            rendered
                .contains("dray_worker_attempts_total{worker=\"worker-1\",outcome=\"proved\"} 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("dray_worker_failures_total{worker=\"worker-1\",reason=\"oom\"} 1"),
            "{rendered}"
        );
    }

    /// Timeouts and OOMs must stay separable. Their remedies are opposites —
    /// more time versus more memory — so a single "resource failure" counter
    /// would point an operator in the wrong direction half the time.
    #[test]
    fn timeouts_and_out_of_memory_are_counted_separately() {
        let metrics = Metrics::new("w");
        metrics.record_failure("timeout");
        metrics.record_failure("timeout");
        metrics.record_failure("oom");

        let rendered = metrics.render();
        assert!(rendered.contains("reason=\"timeout\"} 2"), "{rendered}");
        assert!(rendered.contains("reason=\"oom\"} 1"), "{rendered}");
    }

    /// Every label value a metric can carry must be present from the start,
    /// even at zero. A counter that only appears after its first occurrence
    /// makes `rate()` undefined over the window in which the problem began.
    #[test]
    fn every_label_is_present_before_anything_happens() {
        let rendered = Metrics::new("w").render();

        for reason in FAILURE_REASONS {
            assert!(
                rendered.contains(&format!("reason=\"{reason}\"}} 0")),
                "{reason} missing from a fresh registry"
            );
        }
        for outcome in OUTCOMES {
            assert!(
                rendered.contains(&format!("outcome=\"{outcome}\"}} 0")),
                "{outcome} missing from a fresh registry"
            );
        }
    }

    /// The worker id comes from the environment, so it can contain anything.
    /// An unescaped quote makes the whole scrape unparseable, which reads as
    /// "the worker is down".
    #[test]
    fn a_hostile_worker_id_cannot_break_the_exposition() {
        let metrics = Metrics::new("bad\"id\\with\nnewline");
        let rendered = metrics.render();

        for line in rendered.lines() {
            assert!(
                !line.is_empty(),
                "an escaped newline leaked a blank line into the body"
            );
        }

        // The quote, the backslash, and the newline must all arrive as
        // two-character escapes rather than as themselves.
        assert!(
            rendered.contains(r#"bad\"id"#),
            "quote not escaped: {rendered}"
        );
        assert!(
            rendered.contains(r"\\with"),
            "backslash not escaped: {rendered}"
        );
        assert!(
            rendered.contains(r"\nnewline"),
            "newline not escaped: {rendered}"
        );

        // Every label value must still be closed on the line it opened on.
        for line in rendered.lines().filter(|l| l.contains("worker=")) {
            let quotes = line.matches('"').count() - line.matches(r#"\""#).count();
            assert!(
                // `% 2` rather than `is_multiple_of`, which postdates the MSRV.
                quotes % 2 == 0,
                "unbalanced quotes, so the line is unparseable: {line}"
            );
        }
    }

    #[test]
    fn an_unknown_outcome_is_ignored_rather_than_registered() {
        let metrics = Metrics::new("w");
        metrics.record_outcome("something_new");

        let rendered = metrics.render();
        assert!(
            !rendered.contains("something_new"),
            "an unbounded label set would leak memory: {rendered}"
        );
    }

    #[test]
    fn a_proof_records_duration_memory_and_attempts() {
        let metrics = Metrics::new("w");
        metrics.record_proof(Duration::from_millis(2500), Some(43_008), 2);

        assert_eq!(metrics.proving_duration.count(), 1);
        assert_eq!(metrics.peak_memory_kb.count(), 1);
        assert_eq!(metrics.attempts.count(), 1);

        let rendered = metrics.render();
        assert!(
            rendered.contains("dray_worker_proving_duration_seconds_sum{worker=\"w\"} 2.5"),
            "{rendered}"
        );
    }

    /// Memory is optional — `/usr/bin/time` may not be available — and a
    /// missing measurement must not be recorded as zero, which would drag the
    /// distribution down and hide a real ceiling problem.
    #[test]
    fn a_missing_memory_measurement_is_not_recorded_as_zero() {
        let metrics = Metrics::new("w");
        metrics.record_proof(Duration::from_secs(1), None, 1);

        assert_eq!(metrics.peak_memory_kb.count(), 0);
        assert_eq!(metrics.proving_duration.count(), 1);
    }

    #[test]
    fn the_declared_buckets_bracket_the_measured_costs() {
        // Phase 1 measured ~2.5s and ~42MB; nargo peaks near 75MB. A histogram
        // whose buckets all sit above or below the normal case says nothing.
        assert!(
            DURATION_BUCKETS.contains(&2.5),
            "the measured proving time should be a bucket boundary"
        );
        // A normal proof peaks near 75 MB (nargo) and 43 MB (bb). That has to
        // land *inside* the range: in the first bucket it would be
        // indistinguishable from a trivial job, and in `+Inf` it would be
        // indistinguishable from a runaway one.
        const MEASURED_PEAK_KB: f64 = 76_800.0;
        assert!(
            MEMORY_BUCKETS_KB
                .first()
                .is_some_and(|first| *first < MEASURED_PEAK_KB),
            "no bucket below the measured peak, so a normal proof is unresolvable"
        );
        assert!(
            MEMORY_BUCKETS_KB
                .last()
                .is_some_and(|last| *last > MEASURED_PEAK_KB * 4.0),
            "no headroom above the measured peak to show the margin being eaten"
        );
        assert_eq!(
            *ATTEMPT_BUCKETS.first().expect("buckets"),
            1.0,
            "first-attempt success is the number that matters most"
        );
    }
}
