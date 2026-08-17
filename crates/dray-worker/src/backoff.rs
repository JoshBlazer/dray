//! How long to wait before retrying a failed attempt.
//!
//! # Why jitter is not optional
//!
//! Retries exist because failures are often shared. The database restarts, a
//! proving host runs out of memory, a dependency times out — and every worker
//! that was mid-attempt fails at roughly the same instant. Without jitter they
//! then all wait the same interval and retry at the same instant, reproducing
//! the load spike that caused the failure, and doing it again on the next
//! attempt with a longer period and no less synchrony.
//!
//! So the delay here is drawn uniformly from `[base/2, base]` — "full-ish"
//! jitter, keeping a guaranteed minimum wait while spreading the herd across a
//! window that widens with each attempt. A recovering database sees arrivals
//! smeared over seconds rather than a thundering herd.
//!
//! # Why the randomness is injected
//!
//! [`Backoff::delay`] takes the random draw as an argument rather than calling
//! a generator. That keeps the schedule a pure function, so the tests below can
//! assert exact bounds at both extremes of the draw instead of sampling and
//! hoping. [`Backoff::delay_random`] supplies the draw in production.

use std::time::Duration;

/// An exponential retry schedule with jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay used for the first retry, before any doubling.
    pub base: Duration,
    /// Ceiling on the pre-jitter delay. Without one, a job with a high attempt
    /// limit would eventually schedule a retry days out, which is
    /// indistinguishable from losing it.
    pub max: Duration,
}

impl Default for Backoff {
    /// Tuned against measured proving cost: a proof takes roughly 2.5 s, so a
    /// first retry at one second is short enough to recover from a blip
    /// promptly without meaningfully competing with useful work.
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The delay before attempt number `attempt`, given a jitter draw in
    /// `[0.0, 1.0]`.
    ///
    /// `attempt` counts attempts already made, so the delay after the first
    /// failure is `attempt == 1` and receives `base`. An `attempt` of zero is
    /// treated as one rather than rejected: a caller that has failed cannot
    /// meaningfully have made no attempts, and returning a zero delay would
    /// turn that confusion into a spin.
    ///
    /// The draw is clamped rather than trusted, so a caller that hands over a
    /// value outside the unit interval cannot produce a negative or
    /// unboundedly long delay.
    #[must_use]
    pub fn delay(self, attempt: u32, jitter: f64) -> Duration {
        let attempt = attempt.max(1);

        // Saturating rather than wrapping: attempt 64 must not fold back round
        // to a delay of `base`, which would silently disable the ceiling.
        let doublings = attempt - 1;
        let scaled = self
            .base
            .saturating_mul(1_u32.checked_shl(doublings).unwrap_or(u32::MAX));
        let capped = scaled.min(self.max);

        // Uniform over [capped/2, capped].
        //
        // NaN is handled before the clamp, not by it: `f64::clamp` *propagates*
        // NaN rather than pinning it to a bound, and `Duration::mul_f64` panics
        // on a NaN factor. A worker that panicked while scheduling a retry
        // would fail far more jobs than whatever caused the retry.
        let draw = if jitter.is_nan() {
            0.0
        } else {
            jitter.clamp(0.0, 1.0)
        };
        let factor = 0.5 + draw / 2.0;
        capped.mul_f64(factor)
    }

    /// The delay before attempt number `attempt`, drawing the jitter from the
    /// thread-local generator.
    #[must_use]
    pub fn delay_random(self, attempt: u32) -> Duration {
        self.delay(attempt, random_unit())
    }
}

/// A uniform draw in `[0, 1)`.
///
/// Backoff jitter does not need a cryptographic generator, and pulling one in
/// for it would mean carrying a dependency whose failure modes matter far more
/// elsewhere. This is a SplitMix64 step seeded per thread from the clock and
/// the thread identity, which is ample for spreading retries and is not used
/// for anything else.
fn random_unit() -> f64 {
    use std::cell::Cell;

    thread_local! {
        static STATE: Cell<u64> = Cell::new(seed());
    }

    fn seed() -> u64 {
        use std::hash::{BuildHasher, RandomState};
        // `RandomState` is randomly seeded per process, and hashing the thread
        // id separates threads within it.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos().into());
        let hashed = RandomState::new().hash_one(std::thread::current().id());
        // Never zero: SplitMix64 is fine with a zero state, but seeding every
        // thread identically would defeat the point.
        hashed ^ nanos ^ 0x9e37_79b9_7f4a_7c15
    }

    STATE.with(|state| {
        let mut z = state.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        state.set(z);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        // Top 53 bits give a double in [0, 1) with no rounding to exactly 1.0.
        ((z >> 11) as f64) / ((1_u64 << 53) as f64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Backoff = Backoff {
        base: Duration::from_secs(1),
        max: Duration::from_secs(60),
    };

    #[test]
    fn the_delay_doubles_with_each_attempt() {
        // Jitter at its maximum, so the un-jittered schedule is visible.
        assert_eq!(B.delay(1, 1.0), Duration::from_secs(1));
        assert_eq!(B.delay(2, 1.0), Duration::from_secs(2));
        assert_eq!(B.delay(3, 1.0), Duration::from_secs(4));
        assert_eq!(B.delay(4, 1.0), Duration::from_secs(8));
    }

    #[test]
    fn jitter_spans_half_the_delay_to_all_of_it() {
        assert_eq!(B.delay(3, 0.0), Duration::from_secs(2));
        assert_eq!(B.delay(3, 0.5), Duration::from_secs(3));
        assert_eq!(B.delay(3, 1.0), Duration::from_secs(4));
    }

    /// The whole point of jitter: two workers failing at the same instant must
    /// not retry at the same instant.
    #[test]
    fn the_lowest_and_highest_draws_are_far_apart() {
        let earliest = B.delay(5, 0.0);
        let latest = B.delay(5, 1.0);
        assert_eq!(latest, earliest * 2, "jitter window should span 2x");
        assert!(
            latest - earliest >= Duration::from_secs(8),
            "attempt 5 should smear retries over at least 8s, got {:?}",
            latest - earliest
        );
    }

    #[test]
    fn the_delay_is_capped() {
        // Attempt 20 would be 2^19 seconds — about six days — uncapped.
        assert_eq!(B.delay(20, 1.0), B.max);
        assert_eq!(B.delay(20, 0.0), B.max / 2);
    }

    /// A shift of 32 or more is undefined in the naive formulation and would
    /// panic in debug or wrap in release. `max_attempts` is operator-supplied,
    /// so a large one must not be able to take a worker down.
    #[test]
    fn an_absurd_attempt_number_still_returns_the_cap() {
        for attempt in [31, 32, 33, 64, 1000, u32::MAX] {
            assert_eq!(
                B.delay(attempt, 1.0),
                B.max,
                "attempt {attempt} should be capped, not wrapped"
            );
        }
    }

    #[test]
    fn attempt_zero_is_treated_as_the_first_attempt() {
        // Never zero, which would busy-loop.
        assert_eq!(B.delay(0, 1.0), B.delay(1, 1.0));
        assert!(B.delay(0, 0.0) > Duration::ZERO);
    }

    #[test]
    fn a_draw_outside_the_unit_interval_is_clamped() {
        assert_eq!(B.delay(3, -5.0), B.delay(3, 0.0));
        assert_eq!(B.delay(3, 5.0), B.delay(3, 1.0));
        assert_eq!(B.delay(3, f64::NAN), B.delay(3, 0.0), "NaN must not escape");
    }

    #[test]
    fn every_delay_is_positive() {
        for attempt in 0..40 {
            for jitter in [0.0, 0.25, 0.5, 0.75, 1.0] {
                assert!(
                    B.delay(attempt, jitter) > Duration::ZERO,
                    "attempt {attempt} jitter {jitter} produced a zero delay"
                );
            }
        }
    }

    /// The generator has to actually vary, and vary *evenly*, or "jitter" is
    /// decoration.
    ///
    /// Uniformity is checked by decile occupancy rather than by counting
    /// distinct values. Distinctness is a birthday-paradox measurement, not a
    /// uniformity one: 1000 draws bucketed into 1000 buckets collide down to
    /// about 632 distinct buckets even when the generator is perfect, so a
    /// threshold set by intuition rather than by that number fails against
    /// working code.
    #[test]
    fn the_random_draw_is_spread_over_the_unit_interval() {
        const DRAWS: usize = 10_000;
        let draws: Vec<f64> = (0..DRAWS).map(|_| random_unit()).collect();

        assert!(
            draws.iter().all(|d| (0.0..1.0).contains(d)),
            "draw escaped [0, 1)"
        );

        // Every decile populated, none by more than double its share. A
        // constant, a low-period cycle, or a generator stuck in one half of the
        // range all fail this.
        let mut deciles = [0_usize; 10];
        for draw in &draws {
            deciles[((draw * 10.0) as usize).min(9)] += 1;
        }
        let expected = DRAWS / 10;
        for (index, &count) in deciles.iter().enumerate() {
            assert!(
                count > expected / 2 && count < expected * 2,
                "decile {index} holds {count} of {DRAWS} draws, expected about {expected}: {deciles:?}"
            );
        }

        let mean = draws.iter().sum::<f64>() / DRAWS as f64;
        assert!(
            (0.45..0.55).contains(&mean),
            "mean {mean} is too far from 0.5 to be uniform"
        );
    }

    /// Two threads failing together must not draw the same sequence — that is
    /// the exact scenario jitter exists for, and per-thread state seeded
    /// identically would reproduce the herd it is meant to break up.
    #[test]
    fn separate_threads_draw_different_sequences() {
        fn sequence() -> Vec<u64> {
            (0..32).map(|_| random_unit().to_bits()).collect()
        }

        let left = std::thread::spawn(sequence);
        let right = std::thread::spawn(sequence);
        let (left, right) = (
            left.join().expect("thread panicked"),
            right.join().expect("thread panicked"),
        );

        assert_ne!(left, right, "two threads produced identical jitter");
    }

    #[test]
    fn random_delays_stay_within_the_declared_window() {
        for _ in 0..1000 {
            let delay = B.delay_random(4);
            assert!(
                delay >= Duration::from_secs(4) && delay <= Duration::from_secs(8),
                "attempt 4 delay {delay:?} escaped [4s, 8s]"
            );
        }
    }
}
