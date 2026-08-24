//! Talking to the chain.
//!
//! A deliberately small surface: read a nonce, read fees, estimate, broadcast,
//! fetch a receipt, ask whether a nullifier is spent. Everything policy-shaped
//! lives in [`crate::gas`], [`crate::nonce`], and [`crate::failure`], which are
//! pure and therefore testable without a node.
//!
//! # Broadcasting is not settling
//!
//! [`Chain::submit`] returns as soon as the node accepts the transaction. That
//! is not a settlement and must never be recorded as one: the transaction may
//! be replaced, dropped from the mempool, or mined into a block that a reorg
//! later discards. Confirmation is a separate question, asked repeatedly by the
//! tracker, and is why the job stays `submitting` until it has been answered to
//! depth.

use alloy::{
    network::{EthereumWallet, TransactionBuilder},
    primitives::{Address, B256, Bytes, FixedBytes, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
    sol_types::SolCall,
};

use crate::gas::Fees;

sol! {
    #[sol(rpc)]
    interface IDraySettlement {
        function settle(bytes32 circuitId, bytes proof, bytes32[] publicInputs) external;
        function wouldSettle(bytes32 circuitId, bytes proof, bytes32[] publicInputs)
            external view returns (bool);
        function nullifierUsed(bytes32 nullifier) external view returns (bool);
        function isRelayer(address who) external view returns (bool);
    }
}

/// Anything that went wrong talking to the chain.
///
/// Deliberately not classified here. [`crate::failure::classify`] decides what
/// a message means, so that the decision is in one place and testable without a
/// node.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("could not reach the chain: {0}")]
    Rpc(String),

    #[error("the relayer key is unusable: {0}")]
    Key(String),

    #[error("transaction {tx_hash} reverted: {reason}")]
    Reverted { tx_hash: String, reason: String },
}

/// What the chain reported about a mined transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedTransaction {
    pub block_number: u64,
    pub block_hash: B256,
    /// False when the transaction was mined but reverted. A reverted settlement
    /// is *mined* — it consumed its nonce and cost gas — so it must not be
    /// treated as still pending and resubmitted under the same nonce.
    pub succeeded: bool,
    pub gas_used: u64,
    pub effective_gas_price: u128,
}

/// A connection to one chain, submitting from one account.
#[derive(Clone)]
pub struct Chain {
    provider: DynProvider,
    settlement: Address,
    address: Address,
    chain_id: u64,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("address", &self.address)
            .field("settlement", &self.settlement)
            .field("chain_id", &self.chain_id)
            .finish_non_exhaustive()
    }
}

impl Chain {
    /// Connect, and learn the chain id from the node rather than being told it.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError`] if the key is malformed or the node is
    /// unreachable.
    pub async fn connect(
        rpc_url: &str,
        private_key: &str,
        settlement: Address,
    ) -> Result<Self, ChainError> {
        let signer: PrivateKeySigner = private_key
            .trim()
            .trim_start_matches("0x")
            .parse()
            .map_err(|e| ChainError::Key(format!("{e}")))?;
        let address = signer.address();

        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect(rpc_url)
            .await
            .map_err(|e| ChainError::Rpc(format!("connecting to {rpc_url}: {e}")))?
            .erased();

        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))?;

        Ok(Self {
            provider,
            settlement,
            address,
            chain_id,
        })
    }

    #[must_use]
    pub fn address(&self) -> Address {
        self.address
    }

    #[must_use]
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    #[must_use]
    pub fn settlement(&self) -> Address {
        self.settlement
    }

    /// The current head.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn block_number(&self) -> Result<u64, ChainError> {
        self.provider
            .get_block_number()
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))
    }

    /// The account's next nonce, counting transactions this relayer has already
    /// broadcast but that are not yet mined.
    ///
    /// Pending rather than latest, deliberately: a transaction broadcast
    /// moments ago is pending, and starting from the mined count would reuse
    /// its nonce and silently replace it.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn pending_nonce(&self) -> Result<u64, ChainError> {
        self.provider
            .get_transaction_count(self.address)
            .pending()
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))
    }

    /// The current base fee and a suggested tip.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn fee_inputs(&self) -> Result<(u128, u128), ChainError> {
        let estimate = self
            .provider
            .estimate_eip1559_fees()
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))?;

        // `max_fee_per_gas` from the node already folds in headroom over the
        // base fee; the base fee itself is what the policy wants to reason
        // about, so it is recovered here.
        let base = estimate
            .max_fee_per_gas
            .saturating_sub(estimate.max_priority_fee_per_gas);

        Ok((base, estimate.max_priority_fee_per_gas))
    }

    /// Build the settlement call.
    fn settle_request(
        &self,
        circuit_id: B256,
        proof: &[u8],
        public_inputs: &[B256],
    ) -> TransactionRequest {
        let call = IDraySettlement::settleCall {
            circuitId: circuit_id,
            proof: Bytes::copy_from_slice(proof),
            publicInputs: public_inputs.to_vec(),
        };

        TransactionRequest::default()
            .with_to(self.settlement)
            .with_from(self.address)
            .with_input(call.abi_encode())
    }

    /// Estimate the gas a settlement would use.
    ///
    /// Also serves as a pre-flight: a proof the verifier rejects fails here,
    /// before a nonce has been spent on it.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures, including reverts surfaced by the estimator.
    pub async fn estimate_settle_gas(
        &self,
        circuit_id: B256,
        proof: &[u8],
        public_inputs: &[B256],
    ) -> Result<u64, ChainError> {
        self.provider
            .estimate_gas(self.settle_request(circuit_id, proof, public_inputs))
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))
    }

    /// Broadcast a settlement at an exact nonce and price.
    ///
    /// Every field is supplied rather than filled in by the provider. Letting a
    /// filler choose the nonce would defeat [`crate::nonce`] entirely, and
    /// letting it choose the fee would defeat the ceiling in [`crate::gas`].
    ///
    /// Returns as soon as the node accepts the transaction — which is *not* a
    /// settlement. See the module documentation.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures, including rejection by the node.
    pub async fn submit(
        &self,
        circuit_id: B256,
        proof: &[u8],
        public_inputs: &[B256],
        nonce: u64,
        fees: Fees,
        gas_limit: u64,
    ) -> Result<B256, ChainError> {
        let request = self
            .settle_request(circuit_id, proof, public_inputs)
            .with_nonce(nonce)
            .with_chain_id(self.chain_id)
            .with_gas_limit(gas_limit)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);

        let pending = self
            .provider
            .send_transaction(request)
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))?;

        Ok(*pending.tx_hash())
    }

    /// Look up a transaction, if it has been mined.
    ///
    /// `Ok(None)` means still pending — or dropped, which is indistinguishable
    /// from the outside and is why the tracker also watches for the nonce
    /// moving past it.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn mined(&self, tx_hash: B256) -> Result<Option<MinedTransaction>, ChainError> {
        let receipt = self
            .provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))?;

        let Some(receipt) = receipt else {
            return Ok(None);
        };
        let (Some(block_number), Some(block_hash)) = (receipt.block_number, receipt.block_hash)
        else {
            // A receipt without a block is a node reporting a transaction it
            // has accepted but not placed. Not mined yet.
            return Ok(None);
        };

        Ok(Some(MinedTransaction {
            block_number,
            block_hash,
            succeeded: receipt.status(),
            gas_used: receipt.gas_used,
            effective_gas_price: receipt.effective_gas_price,
        }))
    }

    /// Whether a nullifier has already been consumed on chain.
    ///
    /// The authoritative answer to "has this already settled?", used when a
    /// submission reverts and when a relayer restarts holding a job it may
    /// already have submitted.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn nullifier_used(&self, nullifier: B256) -> Result<bool, ChainError> {
        let contract = IDraySettlement::new(self.settlement, &self.provider);
        contract
            .nullifierUsed(nullifier)
            .call()
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))
    }

    /// Whether this relayer is authorised by the settlement contract.
    ///
    /// Checked at start-up. An unauthorised relayer's every submission reverts,
    /// and discovering that from the failure rate is worse than being told once.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn is_authorised(&self) -> Result<bool, ChainError> {
        let contract = IDraySettlement::new(self.settlement, &self.provider);
        contract
            .isRelayer(self.address)
            .call()
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))
    }

    /// The account's balance, for the start-up check and the funding metric.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn balance(&self) -> Result<U256, ChainError> {
        self.provider
            .get_balance(self.address)
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))
    }

    /// Whether `block_hash` is still the hash of `block_number` on the
    /// canonical chain.
    ///
    /// This is how a reorg is detected. A transaction's receipt keeps reporting
    /// the block it was mined into even after that block has been orphaned, so
    /// trusting the receipt alone would report a settlement that no longer
    /// exists.
    ///
    /// # Errors
    ///
    /// Propagates RPC failures.
    pub async fn block_still_canonical(
        &self,
        block_number: u64,
        block_hash: B256,
    ) -> Result<bool, ChainError> {
        let block = self
            .provider
            .get_block_by_number(block_number.into())
            .await
            .map_err(|e| ChainError::Rpc(format!("{e}")))?;

        Ok(block.is_some_and(|block| block.header.hash == block_hash))
    }
}

/// The circuit identifier the settlement contract dispatches on.
///
/// `keccak256("dray.circuit." || id)`, matching `Deploy.s.sol` and
/// `SettleProof.s.sol`. Computed here rather than stored, so the API, the
/// scripts, and the relayer cannot drift apart.
#[must_use]
pub fn circuit_id(circuit: &str) -> B256 {
    use alloy::primitives::keccak256;
    keccak256([b"dray.circuit.", circuit.as_bytes()].concat())
}

/// Split a public input vector into 32-byte field elements.
///
/// Returns `None` if it is not a whole number of them, rather than padding or
/// truncating — a malformed vector means the proof and the inputs disagree, and
/// submitting it would spend a nonce to be rejected.
#[must_use]
pub fn public_inputs_from_bytes(bytes: &[u8]) -> Option<Vec<B256>> {
    if bytes.is_empty() || bytes.len() % 32 != 0 {
        return None;
    }

    Some(
        bytes
            .chunks_exact(32)
            .map(|chunk| {
                let mut word = [0_u8; 32];
                word.copy_from_slice(chunk);
                FixedBytes(word)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identifier must match what `Deploy.s.sol` registered, or every
    /// settlement reverts with `UnknownCircuit`.
    #[test]
    fn the_circuit_id_matches_the_deployment_scripts() {
        // keccak256("dray.circuit.membership"), as computed by Solidity's
        // keccak256(abi.encodePacked("dray.circuit.", "membership")).
        let membership = circuit_id("membership");
        let range = circuit_id("range_proof");

        assert_ne!(membership, range, "circuits must not collide");
        assert_eq!(
            membership,
            alloy::primitives::keccak256(b"dray.circuit.membership"),
            "abi.encodePacked concatenates; the id must be the same bytes"
        );
    }

    #[test]
    fn public_inputs_split_into_field_elements() {
        let mut bytes = vec![0xAA_u8; 32];
        bytes.extend_from_slice(&[0xBB_u8; 32]);

        let inputs = public_inputs_from_bytes(&bytes).expect("two whole elements");
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0], FixedBytes([0xAA; 32]));
        assert_eq!(inputs[1], FixedBytes([0xBB; 32]));
    }

    /// Padding or truncating a ragged vector would spend a nonce to have the
    /// verifier reject inputs that do not match the proof.
    #[test]
    fn a_ragged_public_input_vector_is_refused() {
        assert!(public_inputs_from_bytes(&[]).is_none(), "empty");
        assert!(public_inputs_from_bytes(&[0; 31]).is_none(), "short");
        assert!(public_inputs_from_bytes(&[0; 40]).is_none(), "ragged");
        assert!(public_inputs_from_bytes(&[0; 64]).is_some());
    }

    /// The nullifier is the last public input (ADR-008), and the relayer has to
    /// find it in the same place the contract will.
    #[test]
    fn the_last_element_is_the_nullifier() {
        let mut bytes = vec![0x11_u8; 32];
        bytes.extend_from_slice(&[0x22_u8; 32]);
        bytes.extend_from_slice(&[0x33_u8; 32]);

        let inputs = public_inputs_from_bytes(&bytes).expect("three elements");
        assert_eq!(
            inputs.last().copied(),
            Some(FixedBytes([0x33; 32])),
            "range_proof publishes (min, max, nullifier)"
        );
    }
}
