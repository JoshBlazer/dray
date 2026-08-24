//! Deciding what a submission failure means.
//!
//! The spec asks for one thing here: distinguish permanent failures (an invalid
//! proof) from transient ones (RPC down, nonce gap) and retry only the latter.
//! Getting it wrong is expensive in both directions — retrying a proof the
//! verifier rejects burns a nonce and gas on every attempt to be told the same
//! thing, and permanently failing a job because an RPC endpoint blinked
//! discards a valid proof that cost real CPU to produce.
//!
//! # The third answer
//!
//! There is a case that is neither, and it is the one most likely to be got
//! wrong: **the settlement already happened**. A relayer that broadcast, then
//! died before recording anything, comes back to find its own transaction
//! already mined — or another relayer's. The contract reports this as a revert
//! on `NullifierAlreadyUsed`, which looks exactly like a failure and is not
//! one. Treating it as permanent would mark a *settled* job failed.
//!
//! So classification has three outcomes, not two, and
//! [`SubmissionFailure::AlreadySettled`] is the one that keeps at-least-once
//! delivery honest.
//!
//! # Matching on error strings
//!
//! Node errors arrive as free text — there is no standard code for "nonce too
//! low". Matching on substrings is therefore unavoidable, and worth being
//! honest about: an unrecognised error is classified **transient**, because the
//! cost of retrying something permanent is bounded by the attempt budget, while
//! the cost of permanently failing something retryable is a proof thrown away.

/// What went wrong with a submission, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionFailure {
    /// The nullifier is already consumed on chain. Not a failure: the job is
    /// settled, by this relayer's earlier attempt or by another relayer.
    AlreadySettled,

    /// Worth trying again: RPC unavailable, nonce gap, underpriced replacement,
    /// or anything unrecognised.
    Transient(String),

    /// Will fail identically for ever: the verifier rejected the proof.
    Permanent(String),
}

impl SubmissionFailure {
    /// How the store should record this.
    ///
    /// # Panics
    ///
    /// Never. [`SubmissionFailure::AlreadySettled`] has no failure kind because
    /// it is not a failure; callers must handle it before asking.
    #[must_use]
    pub fn kind(&self) -> Option<dray_core::FailureKind> {
        match self {
            SubmissionFailure::AlreadySettled => None,
            SubmissionFailure::Transient(_) => Some(dray_core::FailureKind::Transient),
            SubmissionFailure::Permanent(_) => Some(dray_core::FailureKind::Permanent),
        }
    }

    /// The label used for this failure in metrics.
    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            SubmissionFailure::AlreadySettled => "already_settled",
            SubmissionFailure::Transient(_) => "transient",
            SubmissionFailure::Permanent(_) => "permanent",
        }
    }

    /// Whether the account's nonce should be re-read from the chain before the
    /// next attempt.
    ///
    /// A nonce disagreement is not fixed by waiting. If the relayer's counter
    /// has drifted from the chain's — because a transaction landed that it did
    /// not know about, or because it was restarted — every subsequent
    /// submission fails the same way until the counter is resynchronised.
    #[must_use]
    pub fn needs_nonce_resync(&self) -> bool {
        match self {
            SubmissionFailure::Transient(message) => {
                let lower = message.to_ascii_lowercase();
                NONCE_PROBLEMS.iter().any(|needle| lower.contains(needle))
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for SubmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmissionFailure::AlreadySettled => {
                f.write_str("the nullifier is already consumed on chain")
            }
            SubmissionFailure::Transient(message) | SubmissionFailure::Permanent(message) => {
                f.write_str(message)
            }
        }
    }
}

/// Errors from the settlement contract that mean the job is already done.
const ALREADY_SETTLED: &[&str] = &["nullifieralreadyused", "nullifier already used"];

/// Errors from the generated Honk verifier, and the contract's own rejection of
/// a proof it could not verify. These are properties of the proof, so every
/// attempt produces them identically.
const INVALID_PROOF: &[&str] = &[
    "sumcheckfailed",
    "valuegefieldorder",
    "shpleminifailed",
    "publicinputscountinvalid",
    "proofinvalid",
    "invalidproof",
    "invalid proof",
];

/// Nonce disagreements, which waiting does not fix.
const NONCE_PROBLEMS: &[&str] = &[
    "nonce too low",
    "nonce too high",
    "invalid nonce",
    "nonce has already been used",
    "already known",
    "replacement transaction underpriced",
];

/// Classify a node or contract error.
///
/// `message` is whatever the RPC returned, revert reason included where the
/// node decoded one.
#[must_use]
pub fn classify(message: &str) -> SubmissionFailure {
    let lower = message.to_ascii_lowercase();

    // Checked first, and deliberately: a nullifier revert is not a failure, and
    // reading it as one would mark a settled job failed.
    if ALREADY_SETTLED.iter().any(|needle| lower.contains(needle)) {
        return SubmissionFailure::AlreadySettled;
    }

    if INVALID_PROOF.iter().any(|needle| lower.contains(needle)) {
        return SubmissionFailure::Permanent(message.to_owned());
    }

    // Everything else, recognised or not. See the module documentation: the
    // asymmetry is deliberate.
    SubmissionFailure::Transient(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dray_core::FailureKind;

    /// The case that matters most. A relayer that broadcast and then died comes
    /// back to find its own transaction mined; the contract reverts on the
    /// nullifier. Reading that as a failure would mark a settled job failed.
    #[test]
    fn a_consumed_nullifier_means_settled_not_failed() {
        for message in [
            "execution reverted: custom error NullifierAlreadyUsed(bytes32)",
            "reverted: Nullifier already used",
            "NullifierAlreadyUsed",
        ] {
            assert_eq!(
                classify(message),
                SubmissionFailure::AlreadySettled,
                "{message:?} should mean the job is done, not broken"
            );
        }

        assert_eq!(
            SubmissionFailure::AlreadySettled.kind(),
            None,
            "a settled job has no failure to record"
        );
    }

    /// A proof the verifier rejects will be rejected identically for ever.
    /// Retrying costs a nonce and gas to learn nothing.
    #[test]
    fn a_rejected_proof_is_permanent() {
        for message in [
            "execution reverted: SumcheckFailed()",
            "execution reverted: ValueGeFieldOrder()",
            "execution reverted: custom error InvalidProof()",
            "execution reverted: PublicInputsCountInvalid()",
        ] {
            assert_eq!(
                classify(message).kind(),
                Some(FailureKind::Permanent),
                "{message:?} should not be retried"
            );
        }
    }

    #[test]
    fn infrastructure_problems_are_transient() {
        for message in [
            "error sending request for url (https://sepolia.base.org)",
            "connection refused",
            "504 Gateway Timeout",
            "nonce too low",
            "replacement transaction underpriced",
            "insufficient funds for gas * price + value",
            "rate limit exceeded",
        ] {
            assert_eq!(
                classify(message).kind(),
                Some(FailureKind::Transient),
                "{message:?} should be retried"
            );
        }
    }

    /// The asymmetry, stated as a test. Retrying something permanent is bounded
    /// by the attempt budget; permanently failing something retryable throws
    /// away a proof that cost real CPU.
    #[test]
    fn an_unrecognised_error_is_retried_rather_than_discarded() {
        let failure = classify("something nobody has seen before");
        assert_eq!(failure.kind(), Some(FailureKind::Transient));
    }

    /// Insufficient funds is an operator problem, not a job problem. Funding
    /// the account fixes it, and the job should still be there when it does.
    #[test]
    fn an_unfunded_relayer_does_not_fail_the_job() {
        assert_eq!(
            classify("insufficient funds for transfer").kind(),
            Some(FailureKind::Transient),
            "the proof is fine; the relayer's account is not"
        );
    }

    /// A nonce disagreement is not fixed by waiting: every subsequent
    /// submission fails identically until the counter is resynchronised.
    #[test]
    fn nonce_problems_ask_for_a_resync() {
        for message in [
            "nonce too low",
            "Nonce too high",
            "invalid nonce",
            "already known",
            "replacement transaction underpriced",
        ] {
            assert!(
                classify(message).needs_nonce_resync(),
                "{message:?} should trigger a nonce resync"
            );
        }
    }

    #[test]
    fn ordinary_failures_do_not_ask_for_a_resync() {
        assert!(!classify("connection refused").needs_nonce_resync());
        assert!(!classify("execution reverted: SumcheckFailed()").needs_nonce_resync());
        assert!(!SubmissionFailure::AlreadySettled.needs_nonce_resync());
    }

    /// Classification must not depend on how a node happens to capitalise.
    #[test]
    fn matching_ignores_case() {
        assert_eq!(
            classify("EXECUTION REVERTED: SUMCHECKFAILED()").kind(),
            Some(FailureKind::Permanent)
        );
        assert_eq!(
            classify("nullifieralreadyused"),
            SubmissionFailure::AlreadySettled
        );
    }

    /// A settled job is checked for before an invalid proof, because a message
    /// could plausibly mention both and only one of them is actionable.
    #[test]
    fn already_settled_wins_over_an_invalid_proof_reading() {
        let confusing = "execution reverted: NullifierAlreadyUsed after InvalidProof check";
        assert_eq!(classify(confusing), SubmissionFailure::AlreadySettled);
    }

    #[test]
    fn the_message_survives_classification() {
        let message = "error sending request for url (https://sepolia.base.org/): timed out";
        assert_eq!(classify(message).to_string(), message);
    }
}
