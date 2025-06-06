//! Command for debugging block building.
use alloy_consensus::BlockHeader;
use alloy_eips::{
    eip2718::Encodable2718,
    eip4844::{env_settings::EnvKzgSettings, BlobTransactionSidecar},
};
use alloy_primitives::{Address, Bytes, B256};
use alloy_rlp::Decodable;
use alloy_rpc_types::engine::{BlobsBundleV1, PayloadAttributes};
use clap::Parser;
use eyre::Context;
use reth_basic_payload_builder::{BuildArguments, BuildOutcome, PayloadBuilder, PayloadConfig};
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_cli_runner::CliContext;
use reth_consensus::{Consensus, FullConsensus};
use reth_errors::{ConsensusError, RethResult};
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_execution_types::ExecutionOutcome;
use reth_fs_util as fs;
use reth_node_api::{BlockTy, EngineApiMessageVersion, PayloadBuilderAttributes};
use reth_node_ethereum::{consensus::EthBeaconConsensus, EthEvmConfig};
use reth_primitives_traits::{Block as _, SealedBlock, SealedHeader, SignedTransaction};
use reth_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    BlockHashReader, BlockReader, BlockWriter, ChainSpecProvider, ProviderFactory,
    StageCheckpointReader, StateProviderFactory,
};
use reth_revm::{cached::CachedReads, cancelled::CancelOnDrop, database::StateProviderDatabase};
use reth_stages::StageId;
use reth_transaction_pool::{
    blobstore::InMemoryBlobStore, BlobStore, EthPooledTransaction, PoolConfig, TransactionOrigin,
    TransactionPool, TransactionValidationTaskExecutor,
};
use reth_trie::StateRoot;
use reth_trie_db::DatabaseStateRoot;
use std::{path::PathBuf, str::FromStr, sync::Arc};
use tracing::*;

/// `reth debug build-block` command
/// This debug routine requires that the node is positioned at the block before the target.
/// The script will then parse the block and attempt to build a similar one.
#[derive(Debug, Parser)]
pub struct Command<C: ChainSpecParser> {
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    #[arg(long)]
    parent_beacon_block_root: Option<B256>,

    #[arg(long)]
    prev_randao: B256,

    #[arg(long)]
    timestamp: u64,

    #[arg(long)]
    suggested_fee_recipient: Address,

    /// Array of transactions.
    /// NOTE: 4844 transactions must be provided in the same order as they appear in the blobs
    /// bundle.
    #[arg(long, value_delimiter = ',')]
    transactions: Vec<String>,

    /// Path to the file that contains a corresponding blobs bundle.
    #[arg(long)]
    blobs_bundle_path: Option<PathBuf>,
}

impl<C: ChainSpecParser<ChainSpec = ChainSpec>> Command<C> {
    /// Fetches the best block from the database.
    ///
    /// If the database is empty, returns the genesis block.
    fn lookup_best_block<N: ProviderNodeTypes<ChainSpec = C::ChainSpec>>(
        &self,
        factory: ProviderFactory<N>,
    ) -> RethResult<Arc<SealedBlock<BlockTy<N>>>> {
        let provider = factory.provider()?;

        let best_number =
            provider.get_stage_checkpoint(StageId::Finish)?.unwrap_or_default().block_number;
        let best_hash = provider
            .block_hash(best_number)?
            .expect("the hash for the latest block is missing, database is corrupt");

        Ok(Arc::new(
            provider
                .block(best_number.into())?
                .expect("the header for the latest block is missing, database is corrupt")
                .seal_unchecked(best_hash),
        ))
    }

    /// Returns the default KZG settings
    const fn kzg_settings(&self) -> eyre::Result<EnvKzgSettings> {
        Ok(EnvKzgSettings::Default)
    }

    /// Execute `debug in-memory-merkle` command
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = EthPrimitives>>(
        self,
        ctx: CliContext,
    ) -> eyre::Result<()> {
        let Environment { provider_factory, .. } = self.env.init::<N>(AccessRights::RW)?;

        let consensus: Arc<dyn FullConsensus<EthPrimitives, Error = ConsensusError>> =
            Arc::new(EthBeaconConsensus::new(provider_factory.chain_spec()));

        // fetch the best block from the database
        let best_block = self
            .lookup_best_block(provider_factory.clone())
            .wrap_err("the head block is missing")?;

        let blockchain_db = BlockchainProvider::new(provider_factory.clone())?;
        let blob_store = InMemoryBlobStore::default();

        let validator = TransactionValidationTaskExecutor::eth_builder(blockchain_db.clone())
            .with_head_timestamp(best_block.timestamp)
            .kzg_settings(self.kzg_settings()?)
            .with_additional_tasks(1)
            .build_with_tasks(ctx.task_executor.clone(), blob_store.clone());

        let transaction_pool = reth_transaction_pool::Pool::eth_pool(
            validator,
            blob_store.clone(),
            PoolConfig::default(),
        );
        info!(target: "reth::cli", "Transaction pool initialized");

        let mut blobs_bundle = self
            .blobs_bundle_path
            .map(|path| -> eyre::Result<BlobsBundleV1> {
                let contents = fs::read_to_string(&path)
                    .wrap_err(format!("could not read {}", path.display()))?;
                serde_json::from_str(&contents).wrap_err("failed to deserialize blobs bundle")
            })
            .transpose()?;

        for tx_bytes in &self.transactions {
            debug!(target: "reth::cli", bytes = ?tx_bytes, "Decoding transaction");
            let transaction = TransactionSigned::decode(&mut &Bytes::from_str(tx_bytes)?[..])?
                .try_clone_into_recovered()
                .map_err(|e| eyre::eyre!("failed to recover tx: {e}"))?;

            let encoded_length = match transaction.inner() {
                TransactionSigned::Eip4844(tx) => {
                    let blobs_bundle = blobs_bundle.as_mut().ok_or_else(|| {
                        eyre::eyre!("encountered a blob tx. `--blobs-bundle-path` must be provided")
                    })?;

                    let sidecar: BlobTransactionSidecar =
                        blobs_bundle.pop_sidecar(tx.tx().blob_versioned_hashes.len());

                    let pooled = transaction
                        .clone()
                        .into_inner()
                        .try_into_pooled_eip4844(sidecar.clone())
                        .expect("should not fail to convert blob tx if it is already eip4844");
                    let encoded_length = pooled.encode_2718_len();

                    // insert the blob into the store
                    blob_store.insert(*transaction.tx_hash(), sidecar)?;

                    encoded_length
                }
                _ => transaction.encode_2718_len(),
            };

            debug!(target: "reth::cli", ?transaction, "Adding transaction to the pool");
            transaction_pool
                .add_transaction(
                    TransactionOrigin::External,
                    EthPooledTransaction::new(transaction, encoded_length),
                )
                .await?;
        }

        let payload_attrs = PayloadAttributes {
            parent_beacon_block_root: self.parent_beacon_block_root,
            prev_randao: self.prev_randao,
            timestamp: self.timestamp,
            suggested_fee_recipient: self.suggested_fee_recipient,
            // Set empty withdrawals vector if Shanghai is active, None otherwise
            withdrawals: provider_factory
                .chain_spec()
                .is_shanghai_active_at_timestamp(self.timestamp)
                .then(Vec::new),
        };
        let payload_config = PayloadConfig::new(
            Arc::new(SealedHeader::new(best_block.header().clone(), best_block.hash())),
            reth_payload_builder::EthPayloadBuilderAttributes::try_new(
                best_block.hash(),
                payload_attrs,
                EngineApiMessageVersion::default() as u8,
            )?,
        );

        let args = BuildArguments::new(
            CachedReads::default(),
            payload_config,
            CancelOnDrop::default(),
            None,
        );

        let payload_builder = reth_ethereum_payload_builder::EthereumPayloadBuilder::new(
            blockchain_db.clone(),
            transaction_pool,
            EthEvmConfig::new(provider_factory.chain_spec()),
            EthereumBuilderConfig::new(),
        );

        match payload_builder.try_build(args)? {
            BuildOutcome::Better { payload, .. } => {
                let block = payload.block();
                debug!(target: "reth::cli", ?block, "Built new payload");

                consensus.validate_header(block.sealed_header())?;
                consensus.validate_block_pre_execution(block)?;

                let block_with_senders = block.clone().try_recover().unwrap();

                let state_provider = blockchain_db.latest()?;
                let db = StateProviderDatabase::new(&state_provider);
                let evm_config = EthEvmConfig::ethereum(provider_factory.chain_spec());
                let executor = evm_config.batch_executor(db);

                let block_execution_output = executor.execute(&block_with_senders)?;
                let execution_outcome =
                    ExecutionOutcome::from((block_execution_output, block.number));
                debug!(target: "reth::cli", ?execution_outcome, "Executed block");

                let hashed_post_state = state_provider.hashed_post_state(execution_outcome.state());
                let (state_root, trie_updates) = StateRoot::overlay_root_with_updates(
                    provider_factory.provider()?.tx_ref(),
                    hashed_post_state.clone(),
                )?;

                if state_root != block_with_senders.state_root() {
                    eyre::bail!(
                        "state root mismatch. expected: {}. got: {}",
                        block_with_senders.state_root,
                        state_root
                    );
                }

                // Attempt to insert new block without committing
                let provider_rw = provider_factory.provider_rw()?;
                provider_rw.append_blocks_with_state(
                    Vec::from([block_with_senders]),
                    &execution_outcome,
                    hashed_post_state.into_sorted(),
                    trie_updates,
                )?;
                info!(target: "reth::cli", "Successfully appended built block");
            }
            _ => unreachable!("other outcomes are unreachable"),
        };

        Ok(())
    }
}

impl<C: ChainSpecParser> Command<C> {
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.env.chain)
    }
}
