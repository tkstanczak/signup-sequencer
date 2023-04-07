//! Functionality for interacting with smart contracts deployed on chain.
mod abi;

use std::sync::Arc;

use anyhow::anyhow;
use clap::Parser;
use ethers::{
    providers::Middleware,
    types::{Address, U256},
};
use semaphore::Field;
use tracing::{error, info, instrument};

use self::abi::BatchingContract as ContractAbi;
use crate::{
    ethereum::{write::TransactionId, Ethereum, ReadProvider},
    prover::{
        batch_insertion::{Identity, Prover, ProverMap},
        proof::Proof,
    },
};

/// Configuration options for the component responsible for interacting with the
/// contract.
#[derive(Clone, Debug, PartialEq, Eq, Parser)]
#[group(skip)]
pub struct Options {
    /// The address of the identity manager contract.
    #[clap(long, env)]
    pub identity_manager_address: Address,

    /// The depth of the tree that the contract is working with. This needs to
    /// agree with the verifier in the deployed contract, and also with
    /// `semaphore-mtb`.
    #[clap(long, env, default_value = "10")]
    pub tree_depth: usize,

    /// Initial value of the Merkle tree leaves. Defaults to the initial value
    /// used in the identity manager contract.
    #[clap(
        long,
        env,
        default_value = "0000000000000000000000000000000000000000000000000000000000000000"
    )]
    pub initial_leaf_value: Field,
}

/// A structure representing the interface to the batch-based identity manager
/// contract.
#[derive(Debug)]
pub struct IdentityManager {
    ethereum:           Ethereum,
    prover_map:         Arc<ProverMap>,
    abi:                ContractAbi<ReadProvider>,
    initial_leaf_value: Field,
    tree_depth:         usize,
}

impl IdentityManager {
    #[instrument(level = "debug", skip_all)]
    pub async fn new(
        options: Options,
        ethereum: Ethereum,
        prover_map_internal: ProverMap,
    ) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        // Check that there is code deployed at the target address.
        let address = options.identity_manager_address;
        let code = ethereum.provider().get_code(address, None).await?;
        if code.as_ref().is_empty() {
            error!(
                ?address,
                "No contract code is deployed at the provided address."
            );
        }

        // Connect to the running batching contract.
        let abi = ContractAbi::new(
            options.identity_manager_address,
            ethereum.provider().clone(),
        );

        let owner = abi.owner().call().await?;
        if owner != ethereum.address() {
            error!(?owner, signer = ?ethereum.address(), "Signer is not the owner of the identity manager contract.");
            panic!("Cannot currently continue in read-only mode.")
        }
        info!(
            ?address,
            ?owner,
            "Connected to the WorldID Identity Manager"
        );

        let initial_leaf_value = options.initial_leaf_value;
        let tree_depth = options.tree_depth;
        let prover_map = Arc::new(prover_map_internal);

        let identity_manager = Self {
            ethereum,
            prover_map,
            abi,
            initial_leaf_value,
            tree_depth,
        };

        Ok(identity_manager)
    }

    #[must_use]
    pub const fn tree_depth(&self) -> usize {
        self.tree_depth
    }

    #[must_use]
    pub fn max_batch_size(&self) -> usize {
        self.prover_map.max_batch_size()
    }

    #[must_use]
    pub const fn initial_leaf_value(&self) -> Field {
        self.initial_leaf_value
    }

    /// Validates that merkle proofs are of the correct length against tree
    /// depth
    pub fn validate_merkle_proofs(&self, identity_commitments: &[Identity]) -> anyhow::Result<()> {
        for id in identity_commitments {
            if id.merkle_proof.len() != self.tree_depth {
                return Err(anyhow!(format!(
                    "Length of merkle proof ({len}) did not match tree depth ({depth})",
                    len = id.merkle_proof.len(),
                    depth = self.tree_depth
                )));
            }
        }

        Ok(())
    }

    pub fn get_suitable_prover(&self, num_identities: usize) -> anyhow::Result<&Prover> {
        let prover = self
            .prover_map
            .get(num_identities)
            .ok_or_else(|| anyhow!("No available prover for batch size: {num_identities}"))?;

        Ok(prover)
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn prepare_proof(
        prover: &Prover,
        start_index: usize,
        pre_root: U256,
        post_root: U256,
        identity_commitments: &[Identity],
    ) -> anyhow::Result<Proof> {
        let batch_size = identity_commitments.len();

        let actual_start_index: u32 = start_index.try_into()?;

        info!(
            "Sending {} identities to prover of batch size {}",
            batch_size,
            prover.batch_size()
        );

        let proof_data: Proof = prover
            .generate_proof(
                actual_start_index,
                pre_root,
                post_root,
                identity_commitments,
            )
            .await?;

        Ok(proof_data)
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn register_identities(
        &self,
        start_index: usize,
        pre_root: U256,
        post_root: U256,
        identity_commitments: Vec<Identity>,
        proof_data: Proof,
    ) -> anyhow::Result<TransactionId> {
        let actual_start_index: u32 = start_index.try_into()?;

        let proof_points_array: [U256; 8] = proof_data.into();
        let identities = identity_commitments
            .iter()
            .map(|id| id.commitment)
            .collect();

        // We want to send the transaction through our ethereum provider rather than
        // directly now. To that end, we create it, and then send it later, waiting for
        // it to complete.
        let register_identities_transaction = self
            .abi
            .register_identities(
                proof_points_array,
                pre_root,
                actual_start_index,
                identities,
                post_root,
            )
            .tx;

        self.ethereum
            .send_transaction(register_identities_transaction, true)
            .await
            .map_err(|tx_err| anyhow!("{}", tx_err.to_string()))
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn mine_identities(&self, transaction_id: TransactionId) -> anyhow::Result<()> {
        self.ethereum.mine_transaction(transaction_id).await?;
        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn fetch_pending_identities(&self) -> anyhow::Result<Vec<TransactionId>> {
        let pending_identities = self.ethereum.fetch_pending_transactions().await?;

        Ok(pending_identities)
    }

    /// Waits until all the pending transactions have been mined or failed
    #[instrument(level = "debug", skip_all)]
    pub async fn await_clean_slate(&self) -> anyhow::Result<()> {
        // Await for all pending transactions
        let pending_identities = self.fetch_pending_identities().await?;

        for pending_identity_tx in pending_identities {
            // Ignores the result of each transaction - we only care about a clean slate in
            // terms of pending transactions
            drop(self.mine_identities(pending_identity_tx).await);
        }

        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn assert_latest_root(&self, root: Field) -> anyhow::Result<()> {
        let latest_root = self.abi.latest_root().call().await?;
        let processed_root: U256 = root.into();
        if processed_root == latest_root {
            Ok(())
        } else {
            Err(anyhow::Error::msg(format!("{root} is not latest root.",)))
        }
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn latest_root(&self) -> anyhow::Result<U256> {
        let latest_root = self.abi.latest_root().call().await?;

        Ok(latest_root)
    }
}

/// A type for an identity manager object that can be sent across threads.
pub type SharedIdentityManager = Arc<IdentityManager>;
