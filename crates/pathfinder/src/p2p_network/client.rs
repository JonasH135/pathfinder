//! Sync related data retrieval from other peers
//!
//! This is a temporary wrapper around proper p2p sync|propagation api that fits into
//! current sequential sync logic and will be removed when __proper__ sync algo is
//! integrated. What it does is just split methods between a "proxy" node
//! that syncs from the gateway and propagates new headers and a
//! "proper" p2p node which only syncs via p2p.

use lru::LruCache;
use p2p::{
    client::peer_agnostic,
    client::types::{BlockHeader, StateUpdateWithDefs},
    HeadRx,
};
use pathfinder_common::state_update::{ContractClassUpdate, ContractUpdate};
use pathfinder_common::{
    BlockHash, BlockId, BlockNumber, CallParam, CasmHash, ClassHash, ContractAddress,
    ContractAddressSalt, EntryPoint, Fee, SierraHash, StateCommitment, StateUpdate,
    TransactionHash, TransactionNonce, TransactionSignatureElem, TransactionVersion,
};
use starknet_gateway_client::{GatewayApi, GossipApi};
use starknet_gateway_types::reply;
use starknet_gateway_types::request::add_transaction::ContractDefinition;
use starknet_gateway_types::{error::SequencerError, reply::Block};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

pub mod v0;
pub mod v1;

/// Hybrid, as it uses either p2p or the gateway depending on role and api call
#[derive(Clone, Debug)]
pub enum HybridClient {
    /// Syncs from the feeder gateway, propagates new headers via p2p/gossipsub
    /// Proxies blockchain data to non propagating nodes via p2p
    GatewayProxy {
        p2p_client: peer_agnostic::Client,
        sequencer: starknet_gateway_client::Client,
    },
    /// Syncs from p2p network, does not propagate
    NonPropagatingP2P {
        p2p_client: peer_agnostic::Client,
        sequencer: starknet_gateway_client::Client,
        head_rx: HeadRx,
        /// We need to cache the last two fetched blocks via p2p otherwise sync logic will
        /// produce a false reorg from genesis when we loose connection to other p2p nodes.
        /// This was we can stay at the same height while we are disconnected.
        block_cache: BlockCache,
    },
}

#[derive(Clone, Debug)]
pub struct BlockCache {
    inner: Arc<Mutex<LruCache<BlockNumber, BlockCacheEntry>>>,
}

#[derive(Clone, Debug)]
pub struct BlockCacheEntry {
    pub block: Block,
    pub state_update: Option<StateUpdate>,
    pub cairo_definitions: HashMap<ClassHash, Vec<u8>>,
    pub sierra_definitions: HashMap<SierraHash, Vec<u8>>,
}

impl Default for BlockCache {
    fn default() -> Self {
        Self {
            // We only need 2 blocks: the last one and its parent
            inner: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(2).unwrap()))),
        }
    }
}

impl BlockCache {
    fn get_block(&self, number: BlockNumber) -> Option<Block> {
        let mut locked_inner = self.inner.lock().unwrap();
        locked_inner.get(&number).map(|entry| entry.block.clone())
    }

    fn clear_if_reorg(&self, header: &BlockHeader) {
        if let Some(parent_number) = header.number.get().checked_sub(1) {
            let mut locked_inner = self.inner.lock().unwrap();
            if let Some(parent) = locked_inner.get(&BlockNumber::new_or_panic(parent_number)) {
                // If there's a reorg, purge the cache or we'll be stuck
                //
                // There's a risk we'll falsely reorg to genesis if all other peers get disconnected
                // just after the cache is purged
                // TODO: consider increasing the cache and just purging the last block
                if parent.block.block_hash.0 != header.parent_hash.0 {
                    locked_inner.clear();
                }
            }
        }
    }

    fn insert_block(&self, block: Block) {
        let mut locked_inner = self.inner.lock().unwrap();
        locked_inner.put(
            block.block_number,
            BlockCacheEntry {
                block,
                state_update: None,
                cairo_definitions: Default::default(),
                sierra_definitions: Default::default(),
            },
        );
    }

    fn insert_state_update(&self, block_number: BlockNumber, state_update: StateUpdate) {
        let mut locked_inner = self.inner.lock().unwrap();
        locked_inner
            .get_mut(&block_number)
            .expect("block is already cached")
            .state_update = Some(state_update);
    }

    fn insert_cairo_definitions(
        &self,
        block_number: BlockNumber,
        definitions: HashMap<ClassHash, Vec<u8>>,
    ) {
        let mut locked_inner = self.inner.lock().unwrap();
        locked_inner
            .get_mut(&block_number)
            .expect("block is already cached")
            .cairo_definitions = definitions;
    }

    fn insert_sierra_definitions(
        &self,
        block_number: BlockNumber,
        definitions: HashMap<SierraHash, Vec<u8>>,
    ) {
        let mut locked_inner = self.inner.lock().unwrap();
        locked_inner
            .get_mut(&block_number)
            .expect("block is already cached")
            .sierra_definitions = definitions;
    }
}

impl HybridClient {
    pub fn new(
        i_am_proxy: bool,
        p2p_client: peer_agnostic::Client,
        sequencer: starknet_gateway_client::Client,
        head_rx: HeadRx,
    ) -> Self {
        if i_am_proxy {
            Self::GatewayProxy {
                p2p_client,
                sequencer,
            }
        } else {
            Self::NonPropagatingP2P {
                p2p_client,
                sequencer,
                head_rx,
                block_cache: Default::default(),
            }
        }
    }

    fn as_sequencer(&self) -> &starknet_gateway_client::Client {
        match self {
            HybridClient::GatewayProxy { sequencer, .. } => sequencer,
            HybridClient::NonPropagatingP2P { sequencer, .. } => sequencer,
        }
    }
}

/// A hacky temporary way to wrap p2p related errors
mod error {
    use starknet_gateway_types::error::{
        KnownStarknetErrorCode, SequencerError, StarknetError, StarknetErrorCode,
    };

    pub fn block_not_found(message: impl ToString) -> SequencerError {
        SequencerError::StarknetError(StarknetError {
            code: StarknetErrorCode::Known(KnownStarknetErrorCode::BlockNotFound),
            message: message.to_string(),
        })
    }

    pub fn class_not_found(message: impl ToString) -> SequencerError {
        SequencerError::StarknetError(StarknetError {
            code: StarknetErrorCode::Known(KnownStarknetErrorCode::UndeclaredClass),
            message: message.to_string(),
        })
    }
}

#[async_trait::async_trait]
impl GatewayApi for HybridClient {
    async fn block(&self, block: BlockId) -> Result<reply::MaybePendingBlock, SequencerError> {
        use error::block_not_found;

        match self {
            HybridClient::GatewayProxy { sequencer, .. } => sequencer.block(block).await,
            HybridClient::NonPropagatingP2P {
                p2p_client,
                block_cache,
                ..
            } => match block {
                BlockId::Number(n) => {
                    if let Some(block) = block_cache.get_block(n) {
                        tracing::trace!(number=%n, "HybridClient: using cached block");
                        return Ok(block.into());
                    }

                    let mut headers = p2p_client
                        .block_headers(n, 1)
                        .await
                        .map_err(block_not_found)?;

                    if headers.len() != 1 {
                        return Err(block_not_found(format!(
                            "Headers len for block {n} is {}, expected 1",
                            headers.len()
                        )));
                    }

                    let header = headers.swap_remove(0);

                    todo!("use v1");
                }
                BlockId::Latest => {
                    unreachable!("GatewayApi.head() is used in sync and sync status instead")
                }
                BlockId::Hash(_) => unreachable!("not used in sync"),
                BlockId::Pending => {
                    unreachable!("pending should be disabled when p2p is enabled")
                }
            },
        }
    }

    async fn block_without_retry(
        &self,
        block: BlockId,
    ) -> Result<reply::MaybePendingBlock, SequencerError> {
        match self {
            HybridClient::GatewayProxy { sequencer, .. } => {
                sequencer.block_without_retry(block).await
            }
            HybridClient::NonPropagatingP2P { .. } => {
                unreachable!("used for gas price and not in sync")
            }
        }
    }

    async fn class_by_hash(&self, class_hash: ClassHash) -> Result<bytes::Bytes, SequencerError> {
        use error::class_not_found;

        match self {
            HybridClient::GatewayProxy { sequencer, .. } => {
                sequencer.class_by_hash(class_hash).await
            }
            HybridClient::NonPropagatingP2P { p2p_client, .. } => {
                todo!("use v1");
            }
        }
    }

    async fn pending_class_by_hash(
        &self,
        class_hash: ClassHash,
    ) -> Result<bytes::Bytes, SequencerError> {
        use error::class_not_found;

        match self {
            HybridClient::GatewayProxy { sequencer, .. } => {
                sequencer.pending_class_by_hash(class_hash).await
            }
            HybridClient::NonPropagatingP2P { p2p_client, .. } => {
                todo!("use v1");
            }
        }
    }

    async fn transaction(
        &self,
        transaction_hash: TransactionHash,
    ) -> Result<reply::TransactionStatus, SequencerError> {
        self.as_sequencer().transaction(transaction_hash).await
    }

    async fn state_update(&self, block: BlockId) -> Result<StateUpdate, SequencerError> {
        use error::{block_not_found, class_not_found};

        match self {
            HybridClient::GatewayProxy { sequencer, .. } => sequencer.state_update(block).await,
            HybridClient::NonPropagatingP2P { p2p_client, .. } => match block {
                BlockId::Hash(hash) => {
                    let mut state_updates = p2p_client
                        .state_updates(hash, 1)
                        .await
                        .map_err(block_not_found)?;

                    if state_updates.len() != 1 {
                        return Err(block_not_found(format!(
                            "State updates len is {}, expected 1",
                            state_updates.len()
                        )));
                    }

                    let StateUpdateWithDefs {
                        block_hash,
                        state_update,
                        classes,
                    } = state_updates.swap_remove(0);

                    if block_hash != hash {
                        return Err(block_not_found("Block hash mismatch"));
                    }

                    #[derive(serde::Deserialize)]
                    #[serde(untagged)]
                    enum SierraOrCairo<'a> {
                        Sierra {
                            #[allow(unused)]
                            #[serde(borrow)]
                            sierra_program: &'a serde_json::value::RawValue,
                        },
                        Cairo {
                            #[allow(unused)]
                            #[serde(borrow)]
                            program: &'a serde_json::value::RawValue,
                        },
                    }

                    let mut declared_cairo_classes = HashSet::new();
                    let mut declared_sierra_classes = HashMap::new();

                    for class in classes {
                        let definition = zstd::decode_all(class.definition.as_slice())
                            .map_err(|_| class_not_found("zstd failed"))?;

                        match serde_json::from_slice::<SierraOrCairo<'_>>(definition.as_slice()) {
                            Ok(SierraOrCairo::Sierra { .. }) => {
                                // We don't verify the class hash
                                declared_sierra_classes
                                    .insert(SierraHash(class.hash.0), CasmHash::ZERO);
                            }
                            Ok(SierraOrCairo::Cairo { .. }) => {
                                declared_cairo_classes.insert(class.hash);
                            }
                            Err(_) => {
                                return Err(class_not_found("Invalid class definition"));
                            }
                        }
                    }

                    Ok(StateUpdate {
                        block_hash: hash,
                        // We don't verify the state commitment so both commitments are 0
                        parent_state_commitment: StateCommitment::default(),
                        state_commitment: StateCommitment::default(),
                        contract_updates: state_update
                            .contract_updates
                            .into_iter()
                            .map(|(k, v)| {
                                (
                                    k,
                                    ContractUpdate {
                                        storage: v.storage,
                                        // It does not matter if we mark class updates as "deploy" or "replace"
                                        // as the way those updates are inserted into our storage is "deploy/replace-agnostic"
                                        class: v.class.map(ContractClassUpdate::Deploy),
                                        nonce: v.nonce,
                                    },
                                )
                            })
                            .collect(),
                        system_contract_updates: state_update.system_contract_updates,
                        declared_cairo_classes,
                        declared_sierra_classes,
                    })
                }
                _ => unreachable!("not used in sync"),
            },
        }
    }

    async fn eth_contract_addresses(&self) -> Result<reply::EthContractAddresses, SequencerError> {
        self.as_sequencer().eth_contract_addresses().await
    }

    #[allow(clippy::too_many_arguments)]
    async fn add_invoke_transaction(
        &self,
        version: TransactionVersion,
        max_fee: Fee,
        signature: Vec<TransactionSignatureElem>,
        nonce: Option<TransactionNonce>,
        contract_address: ContractAddress,
        entry_point_selector: Option<EntryPoint>,
        calldata: Vec<CallParam>,
    ) -> Result<reply::add_transaction::InvokeResponse, SequencerError> {
        self.as_sequencer()
            .add_invoke_transaction(
                version,
                max_fee,
                signature,
                nonce,
                contract_address,
                entry_point_selector,
                calldata,
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn add_declare_transaction(
        &self,
        version: TransactionVersion,
        max_fee: Fee,
        signature: Vec<TransactionSignatureElem>,
        nonce: TransactionNonce,
        contract_definition: ContractDefinition,
        sender_address: ContractAddress,
        compiled_class_hash: Option<CasmHash>,
        token: Option<String>,
    ) -> Result<reply::add_transaction::DeclareResponse, SequencerError> {
        self.as_sequencer()
            .add_declare_transaction(
                version,
                max_fee,
                signature,
                nonce,
                contract_definition,
                sender_address,
                compiled_class_hash,
                token,
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn add_deploy_account(
        &self,
        version: TransactionVersion,
        max_fee: Fee,
        signature: Vec<TransactionSignatureElem>,
        nonce: TransactionNonce,
        contract_address_salt: ContractAddressSalt,
        class_hash: ClassHash,
        calldata: Vec<CallParam>,
    ) -> Result<reply::add_transaction::DeployAccountResponse, SequencerError> {
        self.as_sequencer()
            .add_deploy_account(
                version,
                max_fee,
                signature,
                nonce,
                contract_address_salt,
                class_hash,
                calldata,
            )
            .await
    }

    /// This is a **temporary** measure to keep the sync logic unchanged
    ///
    /// TODO remove me when sync is changed to use the high level (ie. peer unaware) p2p API
    async fn head(&self) -> Result<(BlockNumber, BlockHash), SequencerError> {
        match self {
            HybridClient::GatewayProxy { sequencer, .. } => sequencer.head().await,
            HybridClient::NonPropagatingP2P { head_rx, .. } => {
                let head = *head_rx.borrow();
                tracing::trace!(?head, "HybridClient::head");
                head.ok_or(error::block_not_found(
                    "Haven't received any gossiped head yet",
                ))
            }
        }
    }
}

#[async_trait::async_trait]
impl GossipApi for HybridClient {
    async fn propagate_head(&self, block_number: BlockNumber, block_hash: BlockHash) {
        use p2p_proto_v1::common::{BlockId, Hash};
        match self {
            HybridClient::GatewayProxy { p2p_client, .. } => {
                match p2p_client
                    .propagate_new_head(BlockId {
                        number: block_number.get(),
                        hash: Hash(block_hash.0),
                    })
                    .await
                {
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "Propagating head failed"),
                }
            }
            HybridClient::NonPropagatingP2P { .. } => {
                // This is why it's called non-propagating
            }
        }
    }
}
