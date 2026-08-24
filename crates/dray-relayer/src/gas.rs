//! Gas pricing: what to offer, and what to do when a transaction sticks.
//!
//! # Why a transaction sticks, and why bumping is not optional
//!
//! A transaction is broadcast with a fee it is willing to pay. If the market
//! moves above that, no block will include it, and it sits in the mempool
//! indefinitely holding a nonce. Every later transaction from the same account
//! is blocked behind it — so one underpriced settlement stalls *every*
//! settlement that relayer will ever make, not just its own.
//!
//! The fix is to replace it: same nonce, higher fee. Nodes accept a replacement
//! only if it raises the fee by a clear margin — geth requires **10% on both**
//! `maxFeePerGas` and `maxPriorityFeePerGas`, and rejects anything less with
//! "replacement transaction underpriced". A bump that raises only one of them
//! is a common and quietly useless mistake: it looks like a bump and is
//! refused like a no-op.
//!
//! # Why a ceiling is not optional either
//!
//! Bumping without a bound is an open-ended promise to pay whatever the chain
//! asks. During congestion that is how a relayer empties its account into a
//! single transaction. So bumping stops at a ceiling, and a transaction that
//! cannot be priced under it is left to the confirmation tracker — it may still
//! mine when the market falls back, and it has not cost anything extra to wait.

/// EIP-1559 fees for one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fees {
    /// Total willing to pay per unit of gas, base fee included.
    pub max_fee_per_gas: u128,
    /// The part offered to the block proposer.
    pub max_priority_fee_per_gas: u128,
}

/// The outcome of asking for a higher price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bump {
    /// Replace the transaction at these fees.
    To(Fees),
    /// The ceiling is reached. Do not replace; wait, or give up.
    AtCeiling,
}

/// How this relayer prices transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasPolicy {
    /// Hard ceiling on `max_fee_per_gas`. Never exceeded, by any path.
    pub max_fee_cap: u128,
    /// Ceiling on the priority fee, separately — a tip is a bid for ordering,
    /// and there is a point past which paying more buys nothing.
    pub max_priority_fee_cap: u128,
    /// Percentage increase per bump. Must clear the node's replacement rule.
    pub bump_percent: u32,
    /// Multiple of the base fee to offer initially, as a percentage.
    ///
    /// The base fee can rise 12.5% per block, so an offer with no headroom is
    /// stale almost immediately. 200% covers roughly six consecutive full
    /// blocks — and costs nothing when the market is calm, because the base fee
    /// is what actually gets charged and the rest is refunded.
    pub base_fee_headroom_percent: u32,
}

/// The minimum increase a node will accept for a replacement transaction.
///
/// geth's rule, and the de facto standard. Bumping by less produces
/// "replacement transaction underpriced" — the old transaction stays stuck and
/// the bump has achieved nothing.
pub const MINIMUM_REPLACEMENT_BUMP_PERCENT: u32 = 10;

impl Default for GasPolicy {
    /// Tuned for Base Sepolia (ADR-010), where fees are a tiny fraction of
    /// Ethereum's.
    ///
    /// The caps are deliberately generous in absolute terms and still trivial
    /// in cost: 5 gwei against a settlement that costs a few hundred thousand
    /// gas is fractions of a cent. A ceiling set so tight that ordinary
    /// congestion trips it would stall settlement to save nothing.
    fn default() -> Self {
        Self {
            max_fee_cap: 5_000_000_000,          // 5 gwei
            max_priority_fee_cap: 1_000_000_000, // 1 gwei
            // Comfortably above the node's 10% floor. Bumping by exactly the
            // minimum risks rejection on any rounding disagreement, and the
            // cost of overshooting is refunded anyway.
            bump_percent: 25,
            base_fee_headroom_percent: 200,
        }
    }
}

impl GasPolicy {
    /// The fees to offer for a first attempt.
    ///
    /// `base_fee` and `suggested_priority_fee` come from the chain. Both are
    /// clamped to the caps, so a policy can never be talked into exceeding its
    /// own ceiling by a node reporting an extreme number.
    #[must_use]
    pub fn initial(self, base_fee: u128, suggested_priority_fee: u128) -> Fees {
        let priority = suggested_priority_fee.min(self.max_priority_fee_cap);

        let headroom = percent_of(base_fee, self.base_fee_headroom_percent);
        // `max_fee` must cover the base fee *and* the tip, or the transaction is
        // invalid rather than merely underpriced.
        let max_fee = headroom.saturating_add(priority).min(self.max_fee_cap);

        Fees {
            max_fee_per_gas: max_fee,
            // A tip larger than the total is rejected outright by the node.
            max_priority_fee_per_gas: priority.min(max_fee),
        }
    }

    /// The fees for a replacement transaction, given what the stuck one offered.
    ///
    /// Both fees rise together. Raising only `max_fee_per_gas` is the classic
    /// broken bump: nodes compare *both* against the replacement rule, so it is
    /// refused and the original stays stuck.
    #[must_use]
    pub fn bump(self, previous: Fees) -> Bump {
        if previous.max_fee_per_gas >= self.max_fee_cap {
            return Bump::AtCeiling;
        }

        let raise = |value: u128| {
            value
                .saturating_add(percent_of(value, self.bump_percent))
                // A zero fee would never grow by a percentage, so give it
                // somewhere to start.
                .max(value.saturating_add(1))
        };

        let max_fee = raise(previous.max_fee_per_gas).min(self.max_fee_cap);
        let priority = raise(previous.max_priority_fee_per_gas)
            .min(self.max_priority_fee_cap)
            .min(max_fee);

        // Clamping to the ceiling can leave the rise below what a node will
        // accept as a replacement. Offering it anyway wastes a round trip to be
        // told "underpriced"; reporting the ceiling is the honest answer.
        if !clears_replacement_rule(previous.max_fee_per_gas, max_fee) {
            return Bump::AtCeiling;
        }

        Bump::To(Fees {
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: priority,
        })
    }

    /// Gas limit to send, given an estimate.
    ///
    /// Estimates are made against current state and can be short by the time
    /// the transaction is mined — a nullifier written between estimate and
    /// inclusion changes the storage cost. Running out of gas still consumes
    /// everything supplied, so the headroom is cheap insurance: unused gas is
    /// not charged.
    #[must_use]
    pub fn gas_limit(estimate: u64) -> u64 {
        estimate.saturating_add(estimate / 5).max(21_000)
    }
}

/// Whether raising `from` to `to` clears the node's replacement threshold.
#[must_use]
pub fn clears_replacement_rule(from: u128, to: u128) -> bool {
    let required = from.saturating_add(percent_of(from, MINIMUM_REPLACEMENT_BUMP_PERCENT));
    to > from && to >= required
}

/// `value * percent / 100`, saturating rather than wrapping.
fn percent_of(value: u128, percent: u32) -> u128 {
    value
        .saturating_mul(u128::from(percent))
        .checked_div(100)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GWEI: u128 = 1_000_000_000;

    fn policy() -> GasPolicy {
        GasPolicy::default()
    }

    #[test]
    fn an_initial_offer_leaves_room_for_the_base_fee_to_rise() {
        let fees = policy().initial(GWEI, GWEI / 10);

        assert!(
            fees.max_fee_per_gas > GWEI,
            "an offer at exactly the base fee is stale within one block: {fees:?}"
        );
        assert_eq!(fees.max_priority_fee_per_gas, GWEI / 10);
    }

    /// A tip larger than the total is not merely expensive, it is invalid.
    #[test]
    fn the_priority_fee_never_exceeds_the_total() {
        let tight = GasPolicy {
            max_fee_cap: 100,
            max_priority_fee_cap: 10_000,
            ..policy()
        };
        let fees = tight.initial(1_000_000, 10_000);

        assert!(
            fees.max_priority_fee_per_gas <= fees.max_fee_per_gas,
            "{fees:?}"
        );
        assert_eq!(fees.max_fee_per_gas, 100, "the cap still holds");
    }

    #[test]
    fn an_initial_offer_is_capped() {
        let fees = policy().initial(1_000 * GWEI, 500 * GWEI);

        assert_eq!(fees.max_fee_per_gas, policy().max_fee_cap);
        assert!(fees.max_priority_fee_per_gas <= policy().max_priority_fee_cap);
    }

    /// The mistake this test exists to catch: raising only `max_fee_per_gas`.
    /// Nodes check both, so a half-bump is refused and the transaction stays
    /// stuck while appearing to have been dealt with.
    #[test]
    fn a_bump_raises_both_fees() {
        let previous = Fees {
            max_fee_per_gas: GWEI,
            max_priority_fee_per_gas: GWEI / 10,
        };

        let Bump::To(bumped) = policy().bump(previous) else {
            panic!("should have bumped, nowhere near the ceiling");
        };

        assert!(
            bumped.max_fee_per_gas > previous.max_fee_per_gas,
            "total not raised"
        );
        assert!(
            bumped.max_priority_fee_per_gas > previous.max_priority_fee_per_gas,
            "priority fee not raised — the node will refuse this replacement"
        );
    }

    #[test]
    fn a_bump_clears_the_nodes_replacement_threshold() {
        let previous = Fees {
            max_fee_per_gas: GWEI,
            max_priority_fee_per_gas: GWEI / 10,
        };

        let Bump::To(bumped) = policy().bump(previous) else {
            panic!("should have bumped");
        };

        assert!(
            clears_replacement_rule(previous.max_fee_per_gas, bumped.max_fee_per_gas),
            "{previous:?} -> {bumped:?} would be rejected as underpriced"
        );
        assert!(clears_replacement_rule(
            previous.max_priority_fee_per_gas,
            bumped.max_priority_fee_per_gas
        ));
    }

    /// Bumping without a bound is an open-ended promise to pay whatever the
    /// chain asks.
    #[test]
    fn bumping_stops_at_the_ceiling_and_never_exceeds_it() {
        let mut fees = policy().initial(GWEI, GWEI / 10);
        let mut bumps = 0;

        while let Bump::To(next) = policy().bump(fees) {
            assert!(
                next.max_fee_per_gas <= policy().max_fee_cap,
                "bump {bumps} exceeded the ceiling: {next:?}"
            );
            assert!(next.max_priority_fee_per_gas <= policy().max_priority_fee_cap);
            assert!(
                next.max_fee_per_gas > fees.max_fee_per_gas,
                "a bump that does not raise the fee is an infinite loop"
            );
            fees = next;
            bumps += 1;
            assert!(bumps < 100, "bumping should converge on the ceiling");
        }

        assert!(bumps > 0, "the first bump should have been possible");
    }

    #[test]
    fn a_transaction_already_at_the_ceiling_cannot_be_bumped() {
        let at_cap = Fees {
            max_fee_per_gas: policy().max_fee_cap,
            max_priority_fee_per_gas: policy().max_priority_fee_cap,
        };
        assert_eq!(policy().bump(at_cap), Bump::AtCeiling);
    }

    /// A bump clamped by the ceiling can land below the node's threshold.
    /// Sending it anyway buys a round trip and an "underpriced" error; saying
    /// so is the honest answer.
    #[test]
    fn a_bump_that_the_ceiling_would_make_useless_reports_the_ceiling() {
        let policy = GasPolicy {
            max_fee_cap: 105,
            bump_percent: 50,
            ..policy()
        };
        let previous = Fees {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
        };

        // 100 -> 150 clamps to 105, which is only a 5% rise: below the 10% the
        // node requires.
        assert_eq!(
            policy.bump(previous),
            Bump::AtCeiling,
            "a bump the node would reject must not be offered"
        );
    }

    #[test]
    fn the_replacement_rule_needs_a_full_ten_percent() {
        assert!(!clears_replacement_rule(100, 100), "no change");
        assert!(!clears_replacement_rule(100, 109), "9% is not enough");
        assert!(clears_replacement_rule(100, 110), "10% exactly");
        assert!(clears_replacement_rule(100, 200));
        assert!(!clears_replacement_rule(100, 50), "downward is not a bump");
    }

    /// Zero fees appear on development chains. A percentage of zero is zero, so
    /// a naive bump would loop for ever offering the same price.
    #[test]
    fn a_zero_fee_can_still_be_bumped() {
        let zero = Fees {
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
        };

        let Bump::To(bumped) = policy().bump(zero) else {
            panic!("zero is not the ceiling");
        };
        assert!(
            bumped.max_fee_per_gas > 0,
            "a percentage of zero is zero; the bump must not stall"
        );
    }

    #[test]
    fn arithmetic_saturates_rather_than_overflowing() {
        let huge = GasPolicy {
            max_fee_cap: u128::MAX,
            max_priority_fee_cap: u128::MAX,
            bump_percent: u32::MAX,
            base_fee_headroom_percent: u32::MAX,
        };

        let fees = huge.initial(u128::MAX, u128::MAX);
        assert!(fees.max_priority_fee_per_gas <= fees.max_fee_per_gas);

        // Must not panic.
        let _ = huge.bump(fees);
    }

    #[test]
    fn the_gas_limit_carries_headroom_over_the_estimate() {
        let limit = GasPolicy::gas_limit(300_000);
        assert!(
            limit > 300_000,
            "an estimate made against current state can be short by inclusion"
        );
        assert!(limit < 400_000, "but not extravagantly: {limit}");
    }

    #[test]
    fn the_gas_limit_never_falls_below_the_intrinsic_cost() {
        assert!(GasPolicy::gas_limit(0) >= 21_000);
        assert_eq!(GasPolicy::gas_limit(u64::MAX), u64::MAX);
    }
}
