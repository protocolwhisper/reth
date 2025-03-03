//! Provides everything related to `eth_` namespace
//!
//! The entire implementation of the namespace is quite large, hence it is divided across several
//! files.

use crate::eth::{
    api::pending_block::{PendingBlock, PendingBlockEnv, PendingBlockEnvOrigin},
    cache::EthStateCache,
    error::{EthApiError, EthResult},
    gas_oracle::GasPriceOracle,
    signer::EthSigner,
};
use async_trait::async_trait;
use reth_interfaces::Result;
use reth_network_api::NetworkInfo;
use reth_primitives::{
    Address, BlockId, BlockNumberOrTag, ChainInfo, SealedBlock, H256, U256, U64,
};
use reth_provider::{
    BlockReaderIdExt, ChainSpecProvider, EvmEnvProvider, StateProviderBox, StateProviderFactory,
};
use reth_rpc_types::{SyncInfo, SyncStatus};
use reth_tasks::{TaskSpawner, TokioTaskExecutor};
use reth_transaction_pool::TransactionPool;
use revm_primitives::{BlockEnv, CfgEnv};
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, Mutex};

mod block;
mod call;
mod fees;
mod pending_block;
mod server;
mod sign;
mod state;
mod transactions;

use crate::TracingCallPool;
pub use transactions::{EthTransactions, TransactionSource};

/// `Eth` API trait.
///
/// Defines core functionality of the `eth` API implementation.
#[async_trait]
pub trait EthApiSpec: EthTransactions + Send + Sync {
    /// Returns the current ethereum protocol version.
    async fn protocol_version(&self) -> Result<U64>;

    /// Returns the chain id
    fn chain_id(&self) -> U64;

    /// Returns provider chain info
    fn chain_info(&self) -> Result<ChainInfo>;

    /// Returns a list of addresses owned by provider.
    fn accounts(&self) -> Vec<Address>;

    /// Returns `true` if the network is undergoing sync.
    fn is_syncing(&self) -> bool;

    /// Returns the [SyncStatus] of the network
    fn sync_status(&self) -> Result<SyncStatus>;
}

/// `Eth` API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
/// These are implemented two-fold: Core functionality is implemented as [EthApiSpec]
/// trait. Additionally, the required server implementations (e.g. [`reth_rpc_api::EthApiServer`])
/// are implemented separately in submodules. The rpc handler implementation can then delegate to
/// the main impls. This way [`EthApi`] is not limited to [`jsonrpsee`] and can be used standalone
/// or in other network handlers (for example ipc).
pub struct EthApi<Provider, Pool, Network> {
    /// All nested fields bundled together.
    inner: Arc<EthApiInner<Provider, Pool, Network>>,
}

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Provider: BlockReaderIdExt + ChainSpecProvider,
{
    /// Creates a new, shareable instance using the default tokio task spawner.
    pub fn new(
        provider: Provider,
        pool: Pool,
        network: Network,
        eth_cache: EthStateCache,
        gas_oracle: GasPriceOracle<Provider>,
        gas_cap: impl Into<GasCap>,
        tracing_call_pool: TracingCallPool,
    ) -> Self {
        Self::with_spawner(
            provider,
            pool,
            network,
            eth_cache,
            gas_oracle,
            gas_cap.into().into(),
            Box::<TokioTaskExecutor>::default(),
            tracing_call_pool,
        )
    }

    /// Creates a new, shareable instance.
    #[allow(clippy::too_many_arguments)]
    pub fn with_spawner(
        provider: Provider,
        pool: Pool,
        network: Network,
        eth_cache: EthStateCache,
        gas_oracle: GasPriceOracle<Provider>,
        gas_cap: u64,
        task_spawner: Box<dyn TaskSpawner>,
        tracing_call_pool: TracingCallPool,
    ) -> Self {
        // get the block number of the latest block
        let latest_block = provider
            .header_by_number_or_tag(BlockNumberOrTag::Latest)
            .ok()
            .flatten()
            .map(|header| header.number)
            .unwrap_or_default();

        let inner = EthApiInner {
            provider,
            pool,
            network,
            signers: Default::default(),
            eth_cache,
            gas_oracle,
            gas_cap,
            starting_block: U256::from(latest_block),
            task_spawner,
            pending_block: Default::default(),
            tracing_call_pool,
        };
        Self { inner: Arc::new(inner) }
    }

    /// Executes the future on a new blocking task.
    ///
    /// This accepts a closure that creates a new future using a clone of this type and spawns the
    /// future onto a new task that is allowed to block.
    pub(crate) async fn on_blocking_task<C, F, R>(&self, c: C) -> EthResult<R>
    where
        C: FnOnce(Self) -> F,
        F: Future<Output = EthResult<R>> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let this = self.clone();
        let f = c(this);
        self.inner.task_spawner.spawn_blocking(Box::pin(async move {
            let res = f.await;
            let _ = tx.send(res);
        }));
        rx.await.map_err(|_| EthApiError::InternalEthError)?
    }

    /// Returns the state cache frontend
    pub(crate) fn cache(&self) -> &EthStateCache {
        &self.inner.eth_cache
    }

    /// Returns the gas oracle frontend
    pub(crate) fn gas_oracle(&self) -> &GasPriceOracle<Provider> {
        &self.inner.gas_oracle
    }

    /// Returns the configured gas limit cap for `eth_call` and tracing related calls
    pub fn gas_cap(&self) -> u64 {
        self.inner.gas_cap
    }

    /// Returns the inner `Provider`
    pub fn provider(&self) -> &Provider {
        &self.inner.provider
    }

    /// Returns the inner `Network`
    pub fn network(&self) -> &Network {
        &self.inner.network
    }

    /// Returns the inner `Pool`
    pub fn pool(&self) -> &Pool {
        &self.inner.pool
    }
}

// === State access helpers ===

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
{
    /// Returns the state at the given [BlockId] enum.
    pub fn state_at_block_id(&self, at: BlockId) -> EthResult<StateProviderBox<'_>> {
        Ok(self.provider().state_by_block_id(at)?)
    }

    /// Returns the state at the given [BlockId] enum or the latest.
    ///
    /// Convenience function to interprets `None` as `BlockId::Number(BlockNumberOrTag::Latest)`
    pub fn state_at_block_id_or_latest(
        &self,
        block_id: Option<BlockId>,
    ) -> EthResult<StateProviderBox<'_>> {
        if let Some(block_id) = block_id {
            self.state_at_block_id(block_id)
        } else {
            Ok(self.latest_state()?)
        }
    }

    /// Returns the state at the given block number
    pub fn state_at_hash(&self, block_hash: H256) -> Result<StateProviderBox<'_>> {
        self.provider().history_by_block_hash(block_hash)
    }

    /// Returns the _latest_ state
    pub fn latest_state(&self) -> Result<StateProviderBox<'_>> {
        self.provider().latest()
    }
}

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Pool: TransactionPool + Clone + 'static,
    Network: NetworkInfo + Send + Sync + 'static,
{
    /// Configures the [CfgEnv] and [BlockEnv] for the pending block
    ///
    /// If no pending block is available, this will derive it from the `latest` block
    pub(crate) fn pending_block_env_and_cfg(&self) -> EthResult<PendingBlockEnv> {
        let origin = if let Some(pending) = self.provider().pending_block()? {
            PendingBlockEnvOrigin::ActualPending(pending)
        } else {
            // no pending block from the CL yet, so we use the latest block and modify the env
            // values that we can
            let mut latest =
                self.provider().latest_header()?.ok_or_else(|| EthApiError::UnknownBlockNumber)?;

            // child block
            latest.number += 1;
            // assumed child block is in the next slot
            latest.timestamp += 12;
            // base fee of the child block
            let chain_spec = self.provider().chain_spec();
            latest.base_fee_per_gas = latest.next_block_base_fee(chain_spec.base_fee_params);

            PendingBlockEnvOrigin::DerivedFromLatest(latest)
        };

        let mut cfg = CfgEnv::default();
        let mut block_env = BlockEnv::default();
        self.provider().fill_block_env_with_header(&mut block_env, origin.header())?;
        self.provider().fill_cfg_env_with_header(&mut cfg, origin.header())?;

        Ok(PendingBlockEnv { cfg, block_env, origin })
    }

    /// Returns the locally built pending block
    pub(crate) async fn local_pending_block(&self) -> EthResult<Option<SealedBlock>> {
        let pending = self.pending_block_env_and_cfg()?;
        if pending.origin.is_actual_pending() {
            return Ok(pending.origin.into_actual_pending())
        }

        // no pending block from the CL yet, so we need to build it ourselves via txpool
        self.on_blocking_task(|this| async move {
            let mut lock = this.inner.pending_block.lock().await;
            let now = Instant::now();

            // check if the block is still good
            if let Some(pending_block) = lock.as_ref() {
                // this is guaranteed to be the `latest` header
                if pending.block_env.number.to::<u64>() == pending_block.block.number &&
                    pending.origin.header().hash == pending_block.block.parent_hash &&
                    now <= pending_block.expires_at
                {
                    return Ok(Some(pending_block.block.clone()))
                }
            }

            // if we're currently syncing, we're unable to build a pending block
            if this.network().is_syncing() {
                return Ok(None)
            }

            // we rebuild the block
            let pending_block = match pending.build_block(this.provider(), this.pool()) {
                Ok(block) => block,
                Err(err) => {
                    tracing::debug!(target: "rpc", "Failed to build pending block: {:?}", err);
                    return Ok(None)
                }
            };

            let now = Instant::now();
            *lock = Some(PendingBlock {
                block: pending_block.clone(),
                expires_at: now + Duration::from_secs(3),
            });

            Ok(Some(pending_block))
        })
        .await
    }
}

impl<Provider, Pool, Events> std::fmt::Debug for EthApi<Provider, Pool, Events> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EthApi").finish_non_exhaustive()
    }
}

impl<Provider, Pool, Events> Clone for EthApi<Provider, Pool, Events> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

#[async_trait]
impl<Provider, Pool, Network> EthApiSpec for EthApi<Provider, Pool, Network>
where
    Pool: TransactionPool + Clone + 'static,
    Provider:
        BlockReaderIdExt + ChainSpecProvider + StateProviderFactory + EvmEnvProvider + 'static,
    Network: NetworkInfo + 'static,
{
    /// Returns the current ethereum protocol version.
    ///
    /// Note: This returns an `U64`, since this should return as hex string.
    async fn protocol_version(&self) -> Result<U64> {
        let status = self.network().network_status().await?;
        Ok(U64::from(status.protocol_version))
    }

    /// Returns the chain id
    fn chain_id(&self) -> U64 {
        U64::from(self.network().chain_id())
    }

    /// Returns the current info for the chain
    fn chain_info(&self) -> Result<ChainInfo> {
        self.provider().chain_info()
    }

    fn accounts(&self) -> Vec<Address> {
        self.inner.signers.iter().flat_map(|s| s.accounts()).collect()
    }

    fn is_syncing(&self) -> bool {
        self.network().is_syncing()
    }

    /// Returns the [SyncStatus] of the network
    fn sync_status(&self) -> Result<SyncStatus> {
        let status = if self.is_syncing() {
            let current_block = U256::from(
                self.provider().chain_info().map(|info| info.best_number).unwrap_or_default(),
            );
            SyncStatus::Info(SyncInfo {
                starting_block: self.inner.starting_block,
                current_block,
                highest_block: current_block,
                warp_chunks_amount: None,
                warp_chunks_processed: None,
            })
        } else {
            SyncStatus::None
        };
        Ok(status)
    }
}

/// The default gas limit for eth_call and adjacent calls.
///
/// This is different from the default to regular 30M block gas limit
/// [ETHEREUM_BLOCK_GAS_LIMIT](reth_primitives::constants::ETHEREUM_BLOCK_GAS_LIMIT) to allow for
/// more complex calls.
pub const RPC_DEFAULT_GAS_CAP: GasCap = GasCap(50_000_000);

/// The wrapper type for gas limit
#[derive(Debug, Clone, Copy)]
pub struct GasCap(u64);

impl Default for GasCap {
    fn default() -> Self {
        RPC_DEFAULT_GAS_CAP
    }
}

impl From<u64> for GasCap {
    fn from(gas_cap: u64) -> Self {
        Self(gas_cap)
    }
}

impl From<GasCap> for u64 {
    fn from(gas_cap: GasCap) -> Self {
        gas_cap.0
    }
}

/// Container type `EthApi`
struct EthApiInner<Provider, Pool, Network> {
    /// The transaction pool.
    pool: Pool,
    /// The provider that can interact with the chain.
    provider: Provider,
    /// An interface to interact with the network
    network: Network,
    /// All configured Signers
    signers: Vec<Box<dyn EthSigner>>,
    /// The async cache frontend for eth related data
    eth_cache: EthStateCache,
    /// The async gas oracle frontend for gas price suggestions
    gas_oracle: GasPriceOracle<Provider>,
    /// Maximum gas limit for `eth_call` and call tracing RPC methods.
    gas_cap: u64,
    /// The block number at which the node started
    starting_block: U256,
    /// The type that can spawn tasks which would otherwise block.
    task_spawner: Box<dyn TaskSpawner>,
    /// Cached pending block if any
    pending_block: Mutex<Option<PendingBlock>>,
    /// A pool dedicated to tracing calls
    tracing_call_pool: TracingCallPool,
}
