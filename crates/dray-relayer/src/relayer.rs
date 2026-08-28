//! The submission loop: take a proof, get it on chain, keep it there.
//!
//! # Why "settled" is not the end
//!
//! Every other queue in Dray ends when work succeeds. This one does not.
//! Confirming a transaction to N blocks makes a reorg unlikely, not impossible,
//! and on an L2 the chain is not final until it is proven on L1 (ADR-010). So a
//! settled job is watched for a while afterwards, and a settlement that
//! disappears returns the job to `proved` for resubmission — the proof is still
//! perfectly valid, only its place on the chain was lost.
//!
//! Reorgs are detected by asking the contract whether the nullifier is still
//! consumed, not by tracking block hashes. That is the question that actually
//! matters: if the nullifier is free again, the settlement is gone, whatever
//! the block structure did. It also stays correct when a transaction is
//! re-mined into a different block, which is not a reorg needing any action.
//!
//! # Resuming is the normal case, not the exception
//!
//! A relayer can die between broadcasting and recording, and at-least-once
//! delivery means it will. So taking a job begins by asking what already
//! happened to it: an unconfirmed settlement of its own is resumed rather than
//! resubmitted, and a nullifier already consumed means the job is done. A
//! relayer that assumed a fresh start would pay a second time to have the
//! contract reject it.

use std::{sync::Arc, time::Duration};

use alloy::primitives::B256;
use dray_store::{Job, Receipt, Store};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    chain::{Chain, MinedTransaction},
    failure::{self, SubmissionFailure},
    gas::{Bump, Fees, GasPolicy},
    nonce::NonceManager,
};

/// How an attempt to settle one job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Confirmed to the required depth and recorded.
    Settled(Uuid),
    /// The nullifier was already consumed. The job is settled; this relayer did
    /// not do it, or did it before dying.
    AlreadySettled(Uuid),
    /// The attempt failed and the result was recorded.
    Failed {
        id: Uuid,
        kind: dray_core::FailureKind,
        retry_in: Option<Duration>,
    },
    /// The lease was lost mid-attempt. Nothing was recorded, deliberately.
    LeaseLost(Uuid),
    /// Shutdown arrived first; the lease was released.
    Abandoned(Uuid),
}

impl Outcome {
    #[must_use]
    pub fn job_id(&self) -> Uuid {
        match self {
            Outcome::Settled(id)
            | Outcome::AlreadySettled(id)
            | Outcome::Failed { id, .. }
            | Outcome::LeaseLost(id)
            | Outcome::Abandoned(id) => *id,
        }
    }

    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            Outcome::Settled(_) => "settled",
            Outcome::AlreadySettled(_) => "already_settled",
            Outcome::Failed { .. } => "failed",
            Outcome::LeaseLost(_) => "lease_lost",
            Outcome::Abandoned(_) => "abandoned",
        }
    }
}

/// Everything about a relayer that is not the store or the chain.
#[derive(Debug, Clone)]
pub struct RelayerConfig {
    pub relayer_id: String,
    /// Blocks a settlement must be buried under before it is recorded.
    pub confirmations: u64,
    /// How long to wait for a broadcast transaction before replacing it at a
    /// higher price.
    pub stuck_after: Duration,
    /// How long to keep watching a settled job for a reorg.
    pub reorg_watch_window: Duration,
    pub lease_ttl: Duration,
    pub heartbeat_interval: Duration,
    pub poll_interval: Duration,
    pub confirm_poll_interval: Duration,
    pub reap_interval: Duration,
    pub shutdown_grace: Duration,
    pub gas: GasPolicy,
    pub backoff: dray_worker_backoff::Backoff,
}

/// Re-exported so a relayer and a worker share one backoff implementation
/// rather than two that drift.
pub mod dray_worker_backoff {
    pub use dray_worker::backoff::Backoff;
}

impl RelayerConfig {
    /// Defaults for Base Sepolia (ADR-010): roughly 2-second blocks.
    ///
    /// Five confirmations is about ten seconds — short enough to be worth
    /// waiting for, deep enough that an ordinary sequencer hiccup does not
    /// unwind it. The reorg watch runs for far longer than that, because the
    /// depth that makes a reorg unlikely is not the depth that makes it
    /// impossible.
    #[must_use]
    pub fn new(relayer_id: impl Into<String>) -> Self {
        Self {
            relayer_id: relayer_id.into(),
            confirmations: 5,
            // Several blocks. Replacing sooner mostly races the chain and pays
            // more for a transaction that was about to be mined anyway.
            stuck_after: Duration::from_secs(30),
            reorg_watch_window: Duration::from_secs(600),
            lease_ttl: Duration::from_secs(300),
            heartbeat_interval: Duration::from_secs(30),
            poll_interval: Duration::from_millis(500),
            confirm_poll_interval: Duration::from_secs(2),
            reap_interval: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(60),
            gas: GasPolicy::default(),
            backoff: dray_worker_backoff::Backoff::default(),
        }
    }
}

/// A relayer: leases proved jobs, settles them, and watches what it settled.
#[derive(Debug)]
pub struct Relayer {
    store: Store,
    chain: Chain,
    config: RelayerConfig,
    nonce: Arc<NonceManager>,
}

impl Relayer {
    #[must_use]
    pub fn new(store: Store, chain: Chain, config: RelayerConfig) -> Self {
        Self {
            store,
            chain,
            config,
            nonce: Arc::new(NonceManager::new()),
        }
    }

    #[must_use]
    pub fn relayer_id(&self) -> &str {
        &self.config.relayer_id
    }

    /// Check the things that would make every submission fail, once, at
    /// start-up.
    ///
    /// An unauthorised or unfunded relayer produces a stream of identical
    /// failures that look like job problems. Saying so once is far better than
    /// letting an operator infer it from the failure rate.
    ///
    /// # Errors
    ///
    /// Returns a description of what is wrong.
    pub async fn preflight(&self) -> Result<(), String> {
        if !self
            .chain
            .is_authorised()
            .await
            .map_err(|e| format!("could not check authorisation: {e}"))?
        {
            return Err(format!(
                "{} is not an authorised relayer on {}; the owner must call \
                 setRelayer for it",
                self.chain.address(),
                self.chain.settlement()
            ));
        }

        let balance = self
            .chain
            .balance()
            .await
            .map_err(|e| format!("could not read the balance: {e}"))?;
        if balance.is_zero() {
            return Err(format!(
                "{} has no funds on chain {}; every submission would fail",
                self.chain.address(),
                self.chain.chain_id()
            ));
        }

        tracing::info!(
            relayer = %self.config.relayer_id,
            address = %self.chain.address(),
            chain_id = self.chain.chain_id(),
            settlement = %self.chain.settlement(),
            "relayer authorised and funded"
        );
        Ok(())
    }

    /// Run until `shutdown` fires, returning every attempt made.
    pub async fn run(&self, mut shutdown: Shutdown) -> Vec<Outcome> {
        let mut outcomes = Vec::new();

        let _reaper = TaskGuard::spawn(reap_loop(
            self.store.clone(),
            self.config.relayer_id.clone(),
            self.config.reap_interval,
        ));

        let _reorgs = TaskGuard::spawn(reorg_watch(
            self.store.clone(),
            self.chain.clone(),
            self.config.relayer_id.clone(),
            self.config.reorg_watch_window,
            self.config.confirm_poll_interval,
        ));

        loop {
            if shutdown.is_requested() {
                break;
            }

            // Deliberately *not* raced against shutdown. Leasing commits a
            // database transaction, so a cancelled future can leave the job
            // leased with nobody working on it — stuck in `submitting` until
            // its lease expires, which is exactly the delay graceful shutdown
            // exists to avoid. Take the lease, then decide.
            let leased = self
                .store
                .lease_next_proved(&self.config.relayer_id, self.config.lease_ttl)
                .await;

            match leased {
                Ok(Some(job)) => {
                    let id = job.id;

                    if shutdown.is_requested() {
                        tracing::info!(job = %id, "shutting down; handing the job straight back");
                        if let Err(err) =
                            self.store.release_lease(id, &self.config.relayer_id).await
                        {
                            tracing::warn!(job = %id, error = %err, "could not release the lease");
                        }
                        break;
                    }

                    let outcome = self.attempt(job, &mut shutdown).await;
                    tracing::info!(job = %id, outcome = ?outcome, "settlement attempt finished");
                    outcomes.push(outcome);
                }
                Ok(None) => {
                    tokio::select! {
                        biased;
                        () = shutdown.requested() => break,
                        () = tokio::time::sleep(self.config.poll_interval) => {}
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "could not lease a proof; will retry");
                    tokio::select! {
                        biased;
                        () = shutdown.requested() => break,
                        () = tokio::time::sleep(self.config.poll_interval) => {}
                    }
                }
            }
        }

        tracing::info!(
            relayer = %self.config.relayer_id,
            attempts = outcomes.len(),
            "relayer stopped"
        );
        outcomes
    }

    /// Settle one leased job.
    async fn attempt(&self, job: Job, shutdown: &mut Shutdown) -> Outcome {
        let id = job.id;

        let mut heartbeat = TaskGuard::spawn(heartbeat(
            self.store.clone(),
            id,
            self.config.relayer_id.clone(),
            self.config.lease_ttl,
            self.config.heartbeat_interval,
        ));

        let work = self.settle(&job);
        tokio::pin!(work);

        tokio::select! {
            biased;

            _ = heartbeat.handle() => {
                tracing::warn!(job = %id, "lease lost while settling; abandoning the attempt");
                Outcome::LeaseLost(id)
            }

            outcome = &mut work => outcome,

            () = shutdown.requested() => {
                match tokio::time::timeout(self.config.shutdown_grace, &mut work).await {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        tracing::info!(job = %id, "releasing the lease on shutdown");
                        if let Err(err) =
                            self.store.release_lease(id, &self.config.relayer_id).await
                        {
                            tracing::warn!(job = %id, error = %err, "could not release the lease");
                        }
                        Outcome::Abandoned(id)
                    }
                }
            }
        }
    }

    /// Broadcast, track, and record. The body of one settlement.
    async fn settle(&self, job: &Job) -> Outcome {
        let id = job.id;

        let (Some(proof), Some(public_inputs_bytes)) =
            (job.proof.as_ref(), job.public_inputs.as_ref())
        else {
            // The schema forbids this, so reaching it means the schema was
            // bypassed. Permanent: nothing about retrying supplies a proof.
            return self
                .record_failure(
                    id,
                    &SubmissionFailure::Permanent(
                        "job is proved but carries no proof or public inputs".to_owned(),
                    ),
                )
                .await;
        };

        let Some(public_inputs) = crate::chain::public_inputs_from_bytes(public_inputs_bytes)
        else {
            return self
                .record_failure(
                    id,
                    &SubmissionFailure::Permanent(format!(
                        "public inputs are {} bytes, not a whole number of field elements",
                        public_inputs_bytes.len()
                    )),
                )
                .await;
        };

        let Some(nullifier) = public_inputs.last().copied() else {
            return self
                .record_failure(
                    id,
                    &SubmissionFailure::Permanent("no public inputs to settle".to_owned()),
                )
                .await;
        };

        // Resuming is the normal case. An unconfirmed settlement of this
        // relayer's own is tracked rather than replaced, because resubmitting
        // would spend a second nonce racing the first.
        match self.store.latest_settlement(id).await {
            Ok(Some(existing)) if existing.reorged_at.is_none() => {
                if let Some(tx_hash) = to_b256(&existing.tx_hash) {
                    tracing::info!(job = %id, tx = %tx_hash, "resuming an earlier broadcast");

                    // Recover the nonce and price from the transaction itself,
                    // so a resumed broadcast can still be replaced if it is
                    // stuck. Without this a relayer restarted mid-submission
                    // would wait on it for ever, unable to bump it and unable
                    // to safely re-nonce around it.
                    let resumable = match self.chain.transaction_terms(tx_hash).await {
                        Ok(Some((nonce, fees))) => {
                            let circuit = crate::chain::circuit_id(&job.circuit_id);
                            let gas_limit = self
                                .chain
                                .estimate_settle_gas(circuit, proof, &public_inputs)
                                .await
                                .map_or(DEFAULT_GAS_LIMIT, GasPolicy::gas_limit);

                            Some((
                                Replaceable {
                                    nonce,
                                    circuit,
                                    proof,
                                    public_inputs: &public_inputs,
                                    gas_limit,
                                },
                                fees,
                            ))
                        }
                        // The node has forgotten it, so there is nothing to
                        // replace; the tracker will notice it never mines.
                        Ok(None) => None,
                        Err(err) => {
                            tracing::warn!(
                                job = %id,
                                error = %err,
                                "could not read the resumed transaction's terms"
                            );
                            None
                        }
                    };

                    return self.track(id, tx_hash, nullifier, resumable).await;
                }
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(job = %id, error = %err, "could not read prior settlements");
            }
        }

        // Someone already settled this — possibly this relayer, before dying
        // without writing anything down.
        match self.chain.nullifier_used(nullifier).await {
            Ok(true) => return self.mark_settled_elsewhere(id).await,
            Ok(false) => {}
            Err(err) => {
                return self
                    .record_failure(id, &failure::classify(&err.to_string()))
                    .await;
            }
        }

        let circuit = crate::chain::circuit_id(&job.circuit_id);

        // Estimating first is a pre-flight: a proof the verifier rejects fails
        // here, before a nonce has been spent on it.
        let gas_limit = match self
            .chain
            .estimate_settle_gas(circuit, proof, &public_inputs)
            .await
        {
            Ok(estimate) => GasPolicy::gas_limit(estimate),
            Err(err) => {
                let classified = failure::classify(&err.to_string());
                if classified == SubmissionFailure::AlreadySettled {
                    return self.mark_settled_elsewhere(id).await;
                }
                return self.record_failure(id, &classified).await;
            }
        };

        let fees = match self.chain.fee_inputs().await {
            Ok((base, tip)) => self.config.gas.initial(base, tip),
            Err(err) => {
                return self
                    .record_failure(id, &failure::classify(&err.to_string()))
                    .await;
            }
        };

        let (tx_hash, nonce) = match self
            .broadcast(circuit, proof, &public_inputs, fees, gas_limit)
            .await
        {
            Ok(sent) => sent,
            Err(classified) => {
                if classified == SubmissionFailure::AlreadySettled {
                    return self.mark_settled_elsewhere(id).await;
                }
                return self.record_failure(id, &classified).await;
            }
        };

        // Written before confirmation, and deliberately: a relayer that died
        // here would otherwise leave a transaction in flight that nothing knew
        // about, and its successor would pay to submit a second one.
        if let Err(err) = self
            .store
            .record_submission(id, tx_hash.as_slice(), nullifier.as_slice())
            .await
        {
            tracing::error!(job = %id, tx = %tx_hash, error = %err, "broadcast but not recorded");
        }

        let replaceable = Replaceable {
            nonce,
            circuit,
            proof,
            public_inputs: &public_inputs,
            gas_limit,
        };
        self.track(id, tx_hash, nullifier, Some((replaceable, fees)))
            .await
    }

    /// Send one transaction, holding the nonce lock across the broadcast.
    async fn broadcast(
        &self,
        circuit: B256,
        proof: &[u8],
        public_inputs: &[B256],
        fees: Fees,
        gas_limit: u64,
    ) -> Result<(B256, u64), SubmissionFailure> {
        let mut guard = self.nonce.begin().await;

        let nonce = match guard.nonce() {
            Some(nonce) => nonce,
            None => {
                let from_chain = self
                    .chain
                    .pending_nonce()
                    .await
                    .map_err(|e| failure::classify(&e.to_string()))?;
                guard.synchronise(from_chain);
                from_chain
            }
        };

        match self
            .chain
            .submit(circuit, proof, public_inputs, nonce, fees, gas_limit)
            .await
        {
            Ok(tx_hash) => {
                // Only now. A nonce advanced for a broadcast that never reached
                // the node leaves a gap that blocks every later transaction.
                guard.consumed();
                Ok((tx_hash, nonce))
            }
            Err(err) => {
                let classified = failure::classify(&err.to_string());
                if classified.needs_nonce_resync() {
                    tracing::warn!(
                        error = %err,
                        "the chain disagrees about the nonce; re-reading it"
                    );
                    guard.invalidate();
                }
                Err(classified)
            }
        }
    }

    /// Wait for a broadcast transaction to be confirmed to depth.
    ///
    /// `replaceable` carries what a replacement would need; `None` when
    /// resuming a broadcast from a previous process, which cannot be replaced
    /// because its fees are not known here.
    async fn track(
        &self,
        id: Uuid,
        first_hash: B256,
        nullifier: B256,
        replaceable: Option<(Replaceable<'_>, Fees)>,
    ) -> Outcome {
        let mut waiting_since = tokio::time::Instant::now();
        let mut tx_hash = first_hash;
        let mut fees = replaceable.map(|(_, fees)| fees);
        let mut call = replaceable.map(|(call, _)| call);

        loop {
            tokio::time::sleep(self.config.confirm_poll_interval).await;

            let mined = match self.chain.mined(tx_hash).await {
                Ok(mined) => mined,
                Err(err) => {
                    tracing::warn!(job = %id, error = %err, "could not check the transaction");
                    continue;
                }
            };

            match mined {
                Some(mined) if !mined.succeeded => {
                    return self.explain_revert(id, nullifier).await;
                }

                Some(mined) => match self.confirm(id, tx_hash, &mined).await {
                    Confirmation::Settled => return Outcome::Settled(id),
                    Confirmation::Orphaned => {
                        tracing::warn!(
                            job = %id,
                            block = mined.block_number,
                            "the block holding this settlement is no longer canonical"
                        );
                        return self
                            .record_failure(
                                id,
                                &SubmissionFailure::Transient(
                                    "settlement block was reorged out before confirmation"
                                        .to_owned(),
                                ),
                            )
                            .await;
                    }
                    Confirmation::Waiting => {}
                    Confirmation::Failed(err) => {
                        tracing::error!(job = %id, error = %err, "could not record settlement");
                        return Outcome::LeaseLost(id);
                    }
                },

                // Not mined. Either still pending, or waiting behind a gap.
                None => {
                    if waiting_since.elapsed() < self.config.stuck_after {
                        continue;
                    }

                    let Some(pending) = call else {
                        // A resumed broadcast whose price and calldata are not
                        // known here. Keep waiting rather than guess: sending a
                        // different transaction under the original's nonce
                        // would race it rather than replace it.
                        continue;
                    };
                    let Some(current) = fees else { continue };

                    // Before paying more, check the transaction *can* be mined.
                    // A transaction whose nonce is ahead of the account's is
                    // not underpriced, it is unreachable — it waits for a
                    // transaction that may never exist, and bumping buys
                    // nothing at any price. This is the nonce gap the spec asks
                    // about, and it happens for real: a reorg can un-mine
                    // transactions and move an account's nonce backwards.
                    match self.chain.pending_nonce().await {
                        Ok(chain_nonce) if chain_nonce < pending.nonce => {
                            tracing::warn!(
                                job = %id,
                                ours = pending.nonce,
                                chain = chain_nonce,
                                "nonce gap: this transaction cannot be mined at that nonce; \
                                 re-reading and resubmitting"
                            );

                            self.nonce.begin().await.invalidate();

                            match self
                                .broadcast(
                                    pending.circuit,
                                    pending.proof,
                                    pending.public_inputs,
                                    current,
                                    pending.gas_limit,
                                )
                                .await
                            {
                                Ok((replacement, nonce)) => {
                                    // The same settlement, carried by a
                                    // different transaction — not a second one.
                                    if let Err(err) = self
                                        .store
                                        .replace_submission_tx(
                                            id,
                                            tx_hash.as_slice(),
                                            replacement.as_slice(),
                                        )
                                        .await
                                    {
                                        tracing::error!(
                                            job = %id,
                                            error = %err,
                                            "resubmitted but could not record the new hash"
                                        );
                                    }
                                    tx_hash = replacement;
                                    call = Some(Replaceable { nonce, ..pending });
                                    waiting_since = tokio::time::Instant::now();
                                }
                                Err(err) => {
                                    tracing::warn!(job = %id, error = %err, "resubmission refused");
                                }
                            }
                            continue;
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(job = %id, error = %err, "could not read the nonce");
                        }
                    }

                    match self.config.gas.bump(current) {
                        Bump::To(higher) => {
                            tracing::info!(
                                job = %id,
                                nonce = pending.nonce,
                                from = current.max_fee_per_gas,
                                to = higher.max_fee_per_gas,
                                "transaction is stuck; replacing it at a higher price"
                            );

                            // The *same* nonce and the same calldata: this
                            // replaces the stuck transaction rather than
                            // queueing a second settlement behind it.
                            match self
                                .chain
                                .submit(
                                    pending.circuit,
                                    pending.proof,
                                    pending.public_inputs,
                                    pending.nonce,
                                    higher,
                                    pending.gas_limit,
                                )
                                .await
                            {
                                Ok(replacement) => {
                                    // Without this the settlement row keeps
                                    // naming the transaction that was replaced,
                                    // and confirmation — which matches on the
                                    // hash — would silently update nothing.
                                    if let Err(err) = self
                                        .store
                                        .replace_submission_tx(
                                            id,
                                            tx_hash.as_slice(),
                                            replacement.as_slice(),
                                        )
                                        .await
                                    {
                                        tracing::error!(
                                            job = %id,
                                            error = %err,
                                            "replaced but could not record the new hash"
                                        );
                                    }
                                    tx_hash = replacement;
                                    fees = Some(higher);
                                    waiting_since = tokio::time::Instant::now();
                                }
                                Err(err) => {
                                    tracing::warn!(job = %id, error = %err, "replacement refused");
                                }
                            }
                        }
                        Bump::AtCeiling => {
                            tracing::warn!(
                                job = %id,
                                "transaction is stuck at the gas ceiling; waiting for the \
                                 market to fall back"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Decide whether a mined transaction is buried deeply enough to record.
    async fn confirm(&self, id: Uuid, tx_hash: B256, mined: &MinedTransaction) -> Confirmation {
        let head = match self.chain.block_number().await {
            Ok(head) => head,
            Err(_) => return Confirmation::Waiting,
        };

        let depth = head.saturating_sub(mined.block_number).saturating_add(1);
        if depth < self.config.confirmations {
            return Confirmation::Waiting;
        }

        // Depth alone is not enough. A receipt keeps naming its block after
        // that block has been orphaned, so a settlement can look deeply buried
        // and not exist.
        match self
            .chain
            .block_still_canonical(mined.block_number, mined.block_hash)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Confirmation::Orphaned,
            Err(_) => return Confirmation::Waiting,
        }

        let receipt = Receipt {
            block_number: i64::try_from(mined.block_number).unwrap_or(i64::MAX),
            confirmations: i32::try_from(depth).unwrap_or(i32::MAX),
            gas_used: i64::try_from(mined.gas_used).ok(),
            effective_gas_price: Some(mined.effective_gas_price.to_string()),
        };

        match self
            .store
            .confirm_settlement(id, &self.config.relayer_id, tx_hash.as_slice(), &receipt)
            .await
        {
            Ok(_) => {
                tracing::info!(
                    job = %id,
                    tx = %tx_hash,
                    block = mined.block_number,
                    gas_used = mined.gas_used,
                    "settled"
                );
                Confirmation::Settled
            }
            Err(err) => Confirmation::Failed(err.to_string()),
        }
    }

    /// A mined transaction that reverted. Work out which kind.
    async fn explain_revert(&self, id: Uuid, nullifier: B256) -> Outcome {
        // The revert reason is not in the receipt, but the question that
        // matters can be asked directly: is the nullifier spent? If it is, this
        // settled — by an earlier attempt or another relayer — and the revert
        // is the contract correctly refusing a duplicate.
        match self.chain.nullifier_used(nullifier).await {
            Ok(true) => self.mark_settled_elsewhere(id).await,
            Ok(false) => {
                // Mined, reverted, and the nullifier is untouched: the verifier
                // rejected the proof. Retrying spends another nonce to be told
                // the same thing.
                self.record_failure(
                    id,
                    &SubmissionFailure::Permanent(
                        "the settlement transaction reverted and the nullifier was not \
                         consumed, so the verifier rejected the proof"
                            .to_owned(),
                    ),
                )
                .await
            }
            Err(err) => {
                self.record_failure(id, &failure::classify(&err.to_string()))
                    .await
            }
        }
    }

    async fn mark_settled_elsewhere(&self, id: Uuid) -> Outcome {
        match self
            .store
            .mark_settled_elsewhere(
                id,
                &self.config.relayer_id,
                "the nullifier was already consumed on chain",
            )
            .await
        {
            Ok(_) => {
                tracing::info!(job = %id, "already settled on chain");
                Outcome::AlreadySettled(id)
            }
            Err(err) => {
                tracing::error!(job = %id, error = %err, "could not mark an existing settlement");
                Outcome::LeaseLost(id)
            }
        }
    }

    async fn record_failure(&self, id: Uuid, failure: &SubmissionFailure) -> Outcome {
        let Some(kind) = failure.kind() else {
            return self.mark_settled_elsewhere(id).await;
        };

        let retry_in = match kind {
            dray_core::FailureKind::Permanent => None,
            dray_core::FailureKind::Transient => {
                let attempt = self.store.job(id).await.ok().flatten().map_or(1, |job| {
                    u32::try_from(job.submission_attempts.max(1)).unwrap_or(1)
                });
                Some(self.config.backoff.delay_random(attempt))
            }
        };

        tracing::warn!(
            job = %id,
            kind = ?kind,
            retry_in = ?retry_in,
            error = %failure,
            "settlement attempt failed"
        );

        if let Err(err) = self
            .store
            .record_submission_failure(
                id,
                &self.config.relayer_id,
                kind,
                &failure.to_string(),
                retry_in,
            )
            .await
        {
            tracing::error!(job = %id, error = %err, "could not record the failure");
            return Outcome::LeaseLost(id);
        }

        Outcome::Failed { id, kind, retry_in }
    }
}

/// Everything needed to re-broadcast a stuck transaction.
///
/// A replacement is the *same call* at the same nonce with a higher price. It
/// carries the calldata because rebuilding it from anything less would risk
/// sending a different transaction under a nonce the original still holds —
/// which is not a replacement, it is a second settlement racing the first.
#[derive(Debug, Clone, Copy)]
struct Replaceable<'a> {
    nonce: u64,
    circuit: B256,
    proof: &'a [u8],
    public_inputs: &'a [B256],
    gas_limit: u64,
}

/// Gas limit assumed when an estimate is unavailable on the resume path.
///
/// Generous: a settlement measured at ~3.8M gas, rounded well up. Unused gas is
/// refunded, and the alternative — abandoning a resumed transaction because the
/// estimator was momentarily unreachable — costs far more.
const DEFAULT_GAS_LIMIT: u64 = 6_000_000;

enum Confirmation {
    Settled,
    Waiting,
    Orphaned,
    Failed(String),
}

fn to_b256(bytes: &[u8]) -> Option<B256> {
    (bytes.len() == 32).then(|| B256::from_slice(bytes))
}

/// Renew a lease until it is refused.
async fn heartbeat(store: Store, id: Uuid, relayer_id: String, ttl: Duration, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;

    loop {
        ticker.tick().await;
        match store.renew_lease(id, &relayer_id, ttl).await {
            Ok(true) => tracing::debug!(job = %id, "settlement lease renewed"),
            Ok(false) => return,
            Err(err) => tracing::warn!(job = %id, error = %err, "lease renewal failed; retrying"),
        }
    }
}

/// Return expired leases to their queues.
async fn reap_loop(store: Store, relayer_id: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);

    loop {
        ticker.tick().await;
        match store.reap_expired_leases(&relayer_id).await {
            Ok(reaped) if !reaped.is_empty() => {
                tracing::info!(
                    count = reaped.len(),
                    "returned expired leases to their queues"
                );
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "reaping failed; will retry"),
        }
    }
}

/// Watch settled jobs for reorgs.
///
/// The question asked is whether the nullifier is still consumed, because that
/// is what a settlement *is* as far as this system is concerned. A settlement
/// re-mined into a different block is still a settlement; one whose nullifier
/// is free again is not, whatever the blocks did.
async fn reorg_watch(
    store: Store,
    chain: Chain,
    relayer_id: String,
    window: Duration,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);

    loop {
        ticker.tick().await;

        let watching = match store.settlements_to_watch(window).await {
            Ok(watching) => watching,
            Err(err) => {
                tracing::warn!(error = %err, "could not list settlements to watch");
                continue;
            }
        };

        for (id, tx_hash, nullifier_bytes) in watching {
            let Some(nullifier) = to_b256(&nullifier_bytes) else {
                continue;
            };

            match chain.nullifier_used(nullifier).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        job = %id,
                        "the nullifier is no longer consumed on chain; a reorg unwound this \
                         settlement"
                    );
                    match store.record_reorg(id, &relayer_id, &tx_hash).await {
                        Ok(_) => tracing::info!(job = %id, "returned to the submit queue"),
                        Err(err) => {
                            tracing::error!(job = %id, error = %err, "could not record the reorg");
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(job = %id, error = %err, "could not check the nullifier");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shutdown and task ownership
// ---------------------------------------------------------------------------

/// A spawned task aborted when its guard drops.
///
/// The same reasoning as in the worker: a detached heartbeat outliving its
/// attempt renews a lease for work that has stopped, so the job never expires,
/// is never reaped, and is never retried.
#[derive(Debug)]
struct TaskGuard<T>(tokio::task::JoinHandle<T>);

impl<T: Send + 'static> TaskGuard<T> {
    fn spawn(future: impl std::future::Future<Output = T> + Send + 'static) -> Self {
        Self(tokio::spawn(future))
    }

    fn handle(&mut self) -> &mut tokio::task::JoinHandle<T> {
        &mut self.0
    }
}

impl<T> Drop for TaskGuard<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Tells relayers to stop taking new work.
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

/// Fires a [`Shutdown`].
#[derive(Debug, Clone)]
pub struct ShutdownHandle(watch::Sender<bool>);

/// Create a linked handle and signal.
#[must_use]
pub fn shutdown() -> (ShutdownHandle, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownHandle(tx), Shutdown(rx))
}

impl ShutdownHandle {
    pub fn trigger(&self) {
        let _ = self.0.send(true);
    }
}

impl Shutdown {
    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once shutdown is requested. A dropped handle counts, so a
    /// relayer cannot hang on the one path meant to stop it.
    pub async fn requested(&mut self) {
        loop {
            if *self.0.borrow_and_update() {
                return;
            }
            if self.0.changed().await.is_err() {
                return;
            }
        }
    }
}
