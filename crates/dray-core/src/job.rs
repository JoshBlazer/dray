//! The job state machine.
//!
//! Deliberately pure. `transition` is a total function over (state, event) with
//! no I/O, no clock, and no randomness, so the entire transition table can be
//! tested exhaustively — including every illegal pair. Persistence applies the
//! result inside a Postgres transaction; that is `dray-store`'s problem, not
//! this module's.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Where a job is in its life.
///
/// The lifecycle is the one in `DRAY_BUILD_SPEC.md` §4.2:
///
/// ```text
/// queued ──► leased ──► proving ──► proved ──► submitting ──► settled
///    │          │          │           │            │
///    │          └──────────┴───────────┘            │
///    │              (lease expiry → queued)         │
///    │                                              │
///    └──► rejected (validation)      failed ◄───────┘ (permanent)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted and durable, waiting for a worker.
    Queued,
    /// A worker holds a lease but has not started proving.
    Leased,
    /// A proving subprocess is running.
    Proving,
    /// A valid proof exists and is waiting for the relayer.
    Proved,
    /// A transaction has been sent and is awaiting confirmation.
    Submitting,
    /// Confirmed on chain to the required depth.
    Settled,
    /// Permanently failed. Terminal.
    Failed,
    /// Rejected before any work was attempted. Terminal.
    Rejected,
}

impl JobState {
    /// Every state, for exhaustive iteration in tests and in the operator CLI.
    pub const ALL: [JobState; 8] = [
        JobState::Queued,
        JobState::Leased,
        JobState::Proving,
        JobState::Proved,
        JobState::Submitting,
        JobState::Settled,
        JobState::Failed,
        JobState::Rejected,
    ];

    /// Whether no further transition is possible.
    ///
    /// `Settled` is deliberately **not** terminal. A chain reorganisation can
    /// undo a settlement, which sends the job back to `Proved` for
    /// resubmission. Treating `Settled` as final is the kind of assumption that
    /// looks harmless until a reorg happens.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, JobState::Failed | JobState::Rejected)
    }

    /// Whether a worker or relayer is expected to be actively holding this job.
    #[must_use]
    pub fn is_in_flight(self) -> bool {
        matches!(
            self,
            JobState::Leased | JobState::Proving | JobState::Submitting
        )
    }

    /// Stable lowercase name. This is the on-the-wire and in-database
    /// representation; changing it is a breaking change.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Leased => "leased",
            JobState::Proving => "proving",
            JobState::Proved => "proved",
            JobState::Submitting => "submitting",
            JobState::Settled => "settled",
            JobState::Failed => "failed",
            JobState::Rejected => "rejected",
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for JobState {
    type Err = UnknownJobState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        JobState::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownJobState(s.to_owned()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown job state: {0}")]
pub struct UnknownJobState(pub String);

/// Something that happened to a job.
///
/// Events carry no data. Anything a transition would need to *decide* an
/// outcome is resolved before the event is constructed — see
/// [`classify_failure`] — which is what keeps `transition` a pure lookup rather
/// than a policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEvent {
    /// A worker acquired a lease.
    Leased,
    /// The worker began running the proving subprocess.
    ProvingStarted,
    /// Proof generation succeeded.
    ProofSucceeded,
    /// The relayer sent a transaction.
    SubmissionStarted,
    /// The transaction confirmed to the required depth.
    SettlementConfirmed,
    /// A lease expired without renewal — the worker died or stalled.
    LeaseExpired,
    /// A transient failure with attempts still remaining.
    RetryScheduled,
    /// A transient failure with no attempts remaining.
    AttemptsExhausted,
    /// A failure that retrying cannot fix.
    PermanentlyFailed,
    /// Validation rejected the job after it was persisted.
    ValidationRejected,
    /// A reorg removed a settlement that had been confirmed.
    Reorged,
}

impl JobEvent {
    pub const ALL: [JobEvent; 11] = [
        JobEvent::Leased,
        JobEvent::ProvingStarted,
        JobEvent::ProofSucceeded,
        JobEvent::SubmissionStarted,
        JobEvent::SettlementConfirmed,
        JobEvent::LeaseExpired,
        JobEvent::RetryScheduled,
        JobEvent::AttemptsExhausted,
        JobEvent::PermanentlyFailed,
        JobEvent::ValidationRejected,
        JobEvent::Reorged,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            JobEvent::Leased => "leased",
            JobEvent::ProvingStarted => "proving_started",
            JobEvent::ProofSucceeded => "proof_succeeded",
            JobEvent::SubmissionStarted => "submission_started",
            JobEvent::SettlementConfirmed => "settlement_confirmed",
            JobEvent::LeaseExpired => "lease_expired",
            JobEvent::RetryScheduled => "retry_scheduled",
            JobEvent::AttemptsExhausted => "attempts_exhausted",
            JobEvent::PermanentlyFailed => "permanently_failed",
            JobEvent::ValidationRejected => "validation_rejected",
            JobEvent::Reorged => "reorged",
        }
    }
}

impl fmt::Display for JobEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A transition that the state machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("illegal transition: {event} is not valid in state {state}")]
pub struct IllegalTransition {
    pub state: JobState,
    pub event: JobEvent,
}

/// Apply an event to a state.
///
/// # Errors
///
/// Returns [`IllegalTransition`] if the event has no meaning in that state.
/// Callers must treat this as a bug or a lost race, never as something to
/// paper over — it means two actors disagreed about who owned the job.
pub fn transition(state: JobState, event: JobEvent) -> Result<JobState, IllegalTransition> {
    use JobEvent as E;
    use JobState as S;

    let next = match (state, event) {
        // The forward path.
        (S::Queued, E::Leased) => S::Leased,
        (S::Leased, E::ProvingStarted) => S::Proving,
        (S::Proving, E::ProofSucceeded) => S::Proved,
        (S::Proved, E::SubmissionStarted) => S::Submitting,
        (S::Submitting, E::SettlementConfirmed) => S::Settled,

        // A lease can only expire while it is held. Returning to `Queued`
        // rather than failing is what gives at-least-once delivery without
        // leader election: a worker dying is an ordinary event.
        (S::Leased | S::Proving, E::LeaseExpired) => S::Queued,

        // A retryable failure before a proof exists means redoing the proof.
        (S::Queued | S::Leased | S::Proving, E::RetryScheduled) => S::Queued,

        // A retryable failure *after* a proof exists must not discard it.
        // Re-proving costs seconds of CPU; resubmitting costs a nonce. A
        // transient RPC error is not a reason to throw away valid work.
        (S::Proved | S::Submitting, E::RetryScheduled) => S::Proved,

        // Exhausted or permanent failures are terminal from anywhere the job
        // is still live.
        (
            S::Queued | S::Leased | S::Proving | S::Proved | S::Submitting,
            E::AttemptsExhausted | E::PermanentlyFailed,
        ) => S::Failed,

        // Validation rejects only before work begins; once a worker has taken
        // the job, a validation problem is a permanent failure instead.
        (S::Queued, E::ValidationRejected) => S::Rejected,

        // A reorg unwinds a settlement. The proof is still valid, so the job
        // goes back to `Proved` and the relayer resubmits.
        (S::Settled, E::Reorged) => S::Proved,

        _ => return Err(IllegalTransition { state, event }),
    };

    Ok(next)
}

/// Whether `event` is legal in `state`, without computing the result.
#[must_use]
pub fn can_transition(state: JobState, event: JobEvent) -> bool {
    transition(state, event).is_ok()
}

/// How a failed attempt should be reported to the state machine.
///
/// Separating this from [`transition`] keeps retry *policy* out of the state
/// machine, so the transition table stays a pure lookup and the policy can be
/// tested on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Might succeed if tried again: RPC timeout, lease lost, worker OOM.
    Transient,
    /// Will never succeed: malformed inputs, unsatisfiable circuit, a proof the
    /// verifier rejects.
    Permanent,
}

/// Choose the event for a failed attempt.
///
/// `attempts` is the number of attempts already made, including the one that
/// just failed.
#[must_use]
pub fn classify_failure(kind: FailureKind, attempts: u32, max_attempts: u32) -> JobEvent {
    match kind {
        FailureKind::Permanent => JobEvent::PermanentlyFailed,
        FailureKind::Transient if attempts >= max_attempts => JobEvent::AttemptsExhausted,
        FailureKind::Transient => JobEvent::RetryScheduled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use JobEvent as E;
    use JobState as S;

    /// The complete set of legal transitions.
    ///
    /// Written out longhand rather than derived from `transition`, so that the
    /// test disagrees with the implementation when either changes. A table
    /// generated from the code under test would agree with it by construction
    /// and prove nothing.
    const LEGAL: &[(JobState, JobEvent, JobState)] = &[
        (S::Queued, E::Leased, S::Leased),
        (S::Queued, E::RetryScheduled, S::Queued),
        (S::Queued, E::AttemptsExhausted, S::Failed),
        (S::Queued, E::PermanentlyFailed, S::Failed),
        (S::Queued, E::ValidationRejected, S::Rejected),
        (S::Leased, E::ProvingStarted, S::Proving),
        (S::Leased, E::LeaseExpired, S::Queued),
        (S::Leased, E::RetryScheduled, S::Queued),
        (S::Leased, E::AttemptsExhausted, S::Failed),
        (S::Leased, E::PermanentlyFailed, S::Failed),
        (S::Proving, E::ProofSucceeded, S::Proved),
        (S::Proving, E::LeaseExpired, S::Queued),
        (S::Proving, E::RetryScheduled, S::Queued),
        (S::Proving, E::AttemptsExhausted, S::Failed),
        (S::Proving, E::PermanentlyFailed, S::Failed),
        (S::Proved, E::SubmissionStarted, S::Submitting),
        (S::Proved, E::RetryScheduled, S::Proved),
        (S::Proved, E::AttemptsExhausted, S::Failed),
        (S::Proved, E::PermanentlyFailed, S::Failed),
        (S::Submitting, E::SettlementConfirmed, S::Settled),
        (S::Submitting, E::RetryScheduled, S::Proved),
        (S::Submitting, E::AttemptsExhausted, S::Failed),
        (S::Submitting, E::PermanentlyFailed, S::Failed),
        (S::Settled, E::Reorged, S::Proved),
    ];

    #[test]
    fn every_legal_transition_produces_the_expected_state() {
        for &(state, event, expected) in LEGAL {
            assert_eq!(
                transition(state, event),
                Ok(expected),
                "{state} + {event} should give {expected}"
            );
        }
    }

    /// The other half, and the half that matters: every pair *not* in the table
    /// must be rejected. This is what stops a lost race from silently
    /// corrupting a job's state.
    #[test]
    fn every_other_pair_is_illegal() {
        for state in JobState::ALL {
            for event in JobEvent::ALL {
                let legal = LEGAL.iter().any(|&(s, e, _)| s == state && e == event);
                if legal {
                    continue;
                }
                assert_eq!(
                    transition(state, event),
                    Err(IllegalTransition { state, event }),
                    "{state} + {event} should be illegal"
                );
            }
        }
    }

    #[test]
    fn the_transition_table_is_exhaustively_covered() {
        // 8 states x 11 events. If either enum grows, this figure changes and
        // the test fails, forcing the new pairs to be considered rather than
        // silently defaulting to illegal.
        let total = JobState::ALL.len() * JobEvent::ALL.len();
        assert_eq!(total, 88, "state or event set changed; review the table");
        assert_eq!(
            LEGAL.len(),
            24,
            "legal transition count changed; review the table"
        );
    }

    #[test]
    fn terminal_states_accept_nothing() {
        for state in [S::Failed, S::Rejected] {
            assert!(state.is_terminal());
            for event in JobEvent::ALL {
                assert!(
                    transition(state, event).is_err(),
                    "{state} accepted {event}"
                );
            }
        }
    }

    #[test]
    fn settled_is_not_terminal_because_of_reorgs() {
        assert!(!S::Settled.is_terminal());
        assert_eq!(transition(S::Settled, E::Reorged), Ok(S::Proved));
    }

    /// The expensive-work-preservation rule, stated as a test so it cannot be
    /// "simplified" away later.
    #[test]
    fn a_retry_after_proving_does_not_discard_the_proof() {
        assert_eq!(transition(S::Submitting, E::RetryScheduled), Ok(S::Proved));
        assert_eq!(transition(S::Proved, E::RetryScheduled), Ok(S::Proved));

        // Whereas before a proof exists, a retry does go back to the queue.
        assert_eq!(transition(S::Proving, E::RetryScheduled), Ok(S::Queued));
        assert_eq!(transition(S::Leased, E::RetryScheduled), Ok(S::Queued));
    }

    #[test]
    fn a_lease_can_only_expire_while_it_is_held() {
        for state in JobState::ALL {
            let expected = state.is_in_flight() && state != S::Submitting;
            assert_eq!(
                can_transition(state, E::LeaseExpired),
                expected,
                "lease expiry in {state}"
            );
        }
    }

    #[test]
    fn validation_rejects_only_before_work_starts() {
        assert_eq!(
            transition(S::Queued, E::ValidationRejected),
            Ok(S::Rejected)
        );
        for state in JobState::ALL.into_iter().filter(|&s| s != S::Queued) {
            assert!(transition(state, E::ValidationRejected).is_err(), "{state}");
        }
    }

    #[test]
    fn failure_classification_respects_the_attempt_budget() {
        assert_eq!(
            classify_failure(FailureKind::Transient, 1, 3),
            E::RetryScheduled
        );
        assert_eq!(
            classify_failure(FailureKind::Transient, 2, 3),
            E::RetryScheduled
        );
        assert_eq!(
            classify_failure(FailureKind::Transient, 3, 3),
            E::AttemptsExhausted
        );
        assert_eq!(
            classify_failure(FailureKind::Transient, 4, 3),
            E::AttemptsExhausted
        );

        // A permanent failure ignores the budget entirely. Retrying a malformed
        // input three times is three times the work for the same answer.
        for attempts in 0..5 {
            assert_eq!(
                classify_failure(FailureKind::Permanent, attempts, 3),
                E::PermanentlyFailed
            );
        }
    }

    #[test]
    fn state_names_round_trip() {
        for state in JobState::ALL {
            assert_eq!(state.as_str().parse(), Ok(state));
        }
        assert!("not_a_state".parse::<JobState>().is_err());
    }

    #[test]
    fn state_names_are_unique() {
        let mut names: Vec<_> = JobState::ALL.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "two states share a name");
    }

    #[test]
    fn every_state_is_reachable_from_queued() {
        // A state nothing can reach is dead code in the schema.
        let mut reached = std::collections::HashSet::from([S::Queued]);
        loop {
            let mut grew = false;
            for state in reached.clone() {
                for event in JobEvent::ALL {
                    if let Ok(next) = transition(state, event) {
                        grew |= reached.insert(next);
                    }
                }
            }
            if !grew {
                break;
            }
        }
        for state in JobState::ALL {
            assert!(
                reached.contains(&state),
                "{state} is unreachable from queued"
            );
        }
    }

    #[test]
    fn every_live_state_can_reach_a_terminal_state() {
        // Otherwise a job could get stuck forever, which breaks the spec's
        // first invariant: every accepted job reaches a terminal state.
        for start in JobState::ALL {
            let mut reached = std::collections::HashSet::from([start]);
            loop {
                let mut grew = false;
                for state in reached.clone() {
                    for event in JobEvent::ALL {
                        if let Ok(next) = transition(state, event) {
                            grew |= reached.insert(next);
                        }
                    }
                }
                if !grew {
                    break;
                }
            }
            assert!(
                reached.iter().any(|s| s.is_terminal()),
                "{start} cannot reach a terminal state"
            );
        }
    }
}
