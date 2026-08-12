//! Property tests for the job state machine.
//!
//! The exhaustive transition-table tests in `job.rs` check every single step.
//! These check what holds over arbitrary *sequences* of steps, which is where
//! the invariants that matter operationally actually live — a job cannot be
//! lost, cannot get stuck, and cannot be resurrected once it has permanently
//! failed.

use dray_core::{FailureKind, JobEvent, JobState, classify_failure, transition};
use proptest::prelude::*;

fn any_state() -> impl Strategy<Value = JobState> {
    prop::sample::select(JobState::ALL.to_vec())
}

fn any_event() -> impl Strategy<Value = JobEvent> {
    prop::sample::select(JobEvent::ALL.to_vec())
}

/// Applies events, ignoring those that are illegal in the current state —
/// which is what a correct caller does when it loses a race.
fn run(start: JobState, events: &[JobEvent]) -> JobState {
    let mut state = start;
    for &event in events {
        if let Ok(next) = transition(state, event) {
            state = next;
        }
    }
    state
}

proptest! {
    /// The spec's headline invariant: no sequence of events can produce a state
    /// outside the declared set. A state machine that can wander off the map
    /// makes "every accepted job reaches a terminal state" unprovable.
    #[test]
    fn any_event_sequence_leaves_a_legal_state(
        start in any_state(),
        events in prop::collection::vec(any_event(), 0..64),
    ) {
        let final_state = run(start, &events);
        prop_assert!(JobState::ALL.contains(&final_state));
    }

    /// Terminal means terminal. Once failed or rejected, nothing revives a job.
    /// If this broke, a job could settle on chain after being reported as
    /// permanently failed.
    #[test]
    fn terminal_states_are_absorbing(
        events in prop::collection::vec(any_event(), 0..64),
    ) {
        for start in [JobState::Failed, JobState::Rejected] {
            prop_assert_eq!(run(start, &events), start);
        }
    }

    /// Reaching a terminal state ends the story regardless of what follows.
    #[test]
    fn nothing_escapes_a_terminal_state_once_reached(
        start in any_state(),
        head in prop::collection::vec(any_event(), 0..32),
        tail in prop::collection::vec(any_event(), 0..32),
    ) {
        let midpoint = run(start, &head);
        prop_assume!(midpoint.is_terminal());
        prop_assert_eq!(run(midpoint, &tail), midpoint);
    }

    /// Every reachable state can still reach a terminal state. This is the
    /// no-stuck-jobs property: there is no trap where a job is neither
    /// finishable nor failable.
    #[test]
    fn no_reachable_state_is_a_dead_end(
        start in any_state(),
        events in prop::collection::vec(any_event(), 0..64),
    ) {
        let state = run(start, &events);

        let mut reached = std::collections::HashSet::from([state]);
        let mut frontier = vec![state];
        while let Some(current) = frontier.pop() {
            for event in JobEvent::ALL {
                if let Ok(next) = transition(current, event) {
                    if reached.insert(next) {
                        frontier.push(next);
                    }
                }
            }
        }

        prop_assert!(
            reached.iter().any(|s| s.is_terminal()),
            "{state} cannot reach a terminal state",
        );
    }

    /// A legal transition never lands where it started unless the table says so.
    /// Catches an accidental identity arm swallowing a real transition.
    #[test]
    fn self_transitions_are_only_the_declared_ones(
        state in any_state(),
        event in any_event(),
    ) {
        if let Ok(next) = transition(state, event) {
            if next == state {
                // The only legal self-transitions: a retry while queued (still
                // queued) and a retry while proved (proof is kept).
                prop_assert!(
                    (state == JobState::Queued && event == JobEvent::RetryScheduled)
                        || (state == JobState::Proved && event == JobEvent::RetryScheduled),
                    "unexpected self-transition {state} + {event}",
                );
            }
        }
    }

    /// Retry policy: a transient failure retries strictly below the budget and
    /// stops at or above it. Off-by-one here means either a lost job or an
    /// infinite retry loop.
    #[test]
    fn transient_failures_retry_exactly_up_to_the_budget(
        attempts in 0u32..100,
        max_attempts in 1u32..100,
    ) {
        let event = classify_failure(FailureKind::Transient, attempts, max_attempts);
        if attempts >= max_attempts {
            prop_assert_eq!(event, JobEvent::AttemptsExhausted);
        } else {
            prop_assert_eq!(event, JobEvent::RetryScheduled);
        }
    }

    /// A permanent failure is never retried, whatever the budget says.
    /// Retrying a malformed input is pure waste.
    #[test]
    fn permanent_failures_never_retry(
        attempts in 0u32..100,
        max_attempts in 0u32..100,
    ) {
        prop_assert_eq!(
            classify_failure(FailureKind::Permanent, attempts, max_attempts),
            JobEvent::PermanentlyFailed,
        );
    }

    /// Proving work is never silently discarded: from `Proved`, no single event
    /// sends the job back to a state that would require re-proving. It either
    /// stays proved, moves forward, or fails outright.
    #[test]
    fn a_proof_is_never_discarded_by_a_single_event(event in any_event()) {
        if let Ok(next) = transition(JobState::Proved, event) {
            prop_assert!(
                !matches!(next, JobState::Queued | JobState::Leased | JobState::Proving),
                "{event} discarded a completed proof, sending Proved -> {next}",
            );
        }
    }
}
