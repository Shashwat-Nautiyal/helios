use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use alloy::{
    consensus::BlockHeader,
    network::{primitives::HeaderResponse, BlockResponse},
    primitives::{Address, B256, U256},
    rpc::types::{BlockTransactions,BlockId.  Filter},
};
use eyre::{eyre, Result};
use tokio::{
    select,
    sync::{mpsc::Receiver, watch, RwLock},
};
use tracing::{info, warn};

use crate::network_spec::NetworkSpec;

use super::rpc::ExecutionRpc;

#[derive(Clone)]
pub struct State<N: NetworkSpec, R: ExecutionRpc<N>> {
    inner: Arc<RwLock<Inner<N, R>>>,
}

impl<N: NetworkSpec, R: ExecutionRpc<N>> State<N, R> {
    pub fn new(
        mut block_recv: Receiver<N::BlockResponse>,
        mut finalized_block_recv: watch::Receiver<Option<N::BlockResponse>>,
        history_length: usize,
        rpc: &str,
    ) -> Self {
        let rpc = R::new(rpc).unwrap();
        let inner = Arc::new(RwLock::new(Inner::new(history_length, rpc)));
        let inner_ref = inner.clone();

        #[cfg(not(target_arch = "wasm32"))]
        let run = tokio::spawn;
        #[cfg(target_arch = "wasm32")]
        let run = wasm_bindgen_futures::spawn_local;

        run(async move {
            loop {
                select! {
                    block = block_recv.recv() => {
                        if let Some(block) = block {
                            inner_ref.write().await.push_block(block).await;
                        }
                    },
                    _ = finalized_block_recv.changed() => {
                        let block = finalized_block_recv.borrow_and_update().clone();
                        if let Some(block) = block {
                            inner_ref.write().await.push_finalized_block(block);
                        }

                    },
                }
            }
        });

        Self { inner }
    }

    pub async fn push_block(&self, block: N::BlockResponse) {
        self.inner.write().await.push_block(block).await;
    }

    // full block fetch

    pub async fn get_block(&self, tag: BlockId) -> Option<N::BlockResponse> {
        match tag {
            BlockId::Latest => self
                .inner
                .read()
                .await
                .blocks
                .last_key_value()
                .map(|entry| entry.1)
                .cloned(),
                BlockId::Finalized => self.inner.read().await.finalized_block.clone(),
                BlockId::Number(number) => self.inner.read().await.blocks.get(&number).cloned(),
        }
    }

    pub async fn get_block_by_hash(&self, hash: B256) -> Option<N::BlockResponse> {
        let inner = self.inner.read().await;
        inner
            .hashes
            .get(&hash)
            .and_then(|number| inner.blocks.get(number))
            .cloned()
    }

    pub async fn get_blocks_after(&self, tag: BlockId) -> Vec<N::BlockResponse> {
        let start_block = self.get_block(tag).await;
        if start_block.is_none() {
            return vec![];
        }
        let start_block_num = start_block.unwrap().header().number();
        let blocks = self
            .inner
            .read()
            .await
            .blocks
            .range((start_block_num + 1)..)
            .map(|(_, v)| v.clone())
            .collect();
        blocks
    }

    // transaction fetch

    pub async fn get_transaction(&self, hash: B256) -> Option<N::TransactionResponse> {
        let inner = self.inner.read().await;
        inner
            .txs
            .get(&hash)
            .and_then(|loc| {
                inner
                    .blocks
                    .get(&loc.block)
                    .and_then(|block| match &block.transactions() {
                        BlockTransactions::Full(txs) => txs.get(loc.index),
                        BlockTransactions::Hashes(_) => unreachable!(),
                        BlockTransactions::Uncle => unreachable!(),
                    })
            })
            .cloned()
    }

    pub async fn get_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: u64,
    ) -> Option<N::TransactionResponse> {
        let inner = self.inner.read().await;
        inner
            .hashes
            .get(&block_hash)
            .and_then(|number| inner.blocks.get(number))
            .and_then(|block| match &block.transactions() {
                BlockTransactions::Full(txs) => txs.get(index as usize),
                BlockTransactions::Hashes(_) => unreachable!(),
                BlockTransactions::Uncle => unreachable!(),
            })
            .cloned()
    }

    pub async fn get_transaction_by_block_and_index(
        &self,
        tag: BlockId,
        index: u64,
    ) -> Option<N::TransactionResponse> {
        let block = self.get_block(tag).await?;
        match &block.transactions() {
            BlockTransactions::Full(txs) => txs.get(index as usize).cloned(),
            BlockTransactions::Hashes(_) => unreachable!(),
            BlockTransactions::Uncle => unreachable!(),
        }
    }

    // block field fetch

    pub async fn get_state_root(&self, tag: BlockId) -> Option<B256> {
        self.get_block(tag)
            .await
            .map(|block| block.header().state_root())
    }

    pub async fn get_receipts_root(&self, tag: BlockId) -> Option<B256> {
        self.get_block(tag)
            .await
            .map(|block| block.header().receipts_root())
    }

    pub async fn get_base_fee(&self, tag: BlockId) -> Option<Option<u64>> {
        self.get_block(tag)
            .await
            .map(|block| block.header().base_fee_per_gas())
    }

    pub async fn get_coinbase(&self, tag: BlockId) -> Option<Address> {
        self.get_block(tag)
            .await
            .map(|block| block.header().beneficiary())
    }

    // filter

    pub async fn push_filter(&self, id: U256, filter: FilterType) {
        self.inner.write().await.filters.insert(id, filter);
    }

    pub async fn remove_filter(&self, id: &U256) -> bool {
        self.inner.write().await.filters.remove(id).is_some()
    }

    pub async fn get_filter(&self, id: &U256) -> Option<FilterType> {
        self.inner.read().await.filters.get(id).cloned()
    }

    // misc

    pub async fn latest_block_number(&self) -> Option<u64> {
        let inner = self.inner.read().await;
        inner.blocks.last_key_value().map(|entry| *entry.0)
    }

    pub async fn oldest_block_number(&self) -> Option<u64> {
        let inner = self.inner.read().await;
        inner.blocks.first_key_value().map(|entry| *entry.0)
    }
}

struct Inner<N: NetworkSpec, R: ExecutionRpc<N>> {
    blocks: BTreeMap<u64, N::BlockResponse>,
    finalized_block: Option<N::BlockResponse>,
    hashes: HashMap<B256, u64>,
    txs: HashMap<B256, TransactionLocation>,
    filters: HashMap<U256, FilterType>,
    history_length: usize,
    rpc: R,
}

impl<N: NetworkSpec, R: ExecutionRpc<N>> Inner<N, R> {
    pub fn new(history_length: usize, rpc: R) -> Self {
        Self {
            history_length,
            blocks: BTreeMap::default(),
            finalized_block: None,
            hashes: HashMap::default(),
            txs: HashMap::default(),
            filters: HashMap::default(),
            rpc,
        }
    }

    pub async fn push_block(&mut self, block: N::BlockResponse) {
        let block_number = block.header().number();
        if self.try_insert_tip(block) {
            let mut n = block_number;

            loop {
                if let Ok(backfilled) = self.backfill_behind(n).await {
                    if !backfilled {
                        break;
                    }
                    n -= 1;
                } else {
                    self.prune_before(n);
                    break;
                }
            }

            let link_child = self.blocks.get(&n);
            let link_parent = self.blocks.get(&(n - 1));

            if let (Some(parent), Some(child)) = (link_parent, link_child) {
                if child.header().parent_hash() != parent.header().hash() {
                    warn!("detected block reorganization");
                    self.prune_before(n);
                }
            }

            self.prune();
        }
    }

    fn try_insert_tip(&mut self, block: N::BlockResponse) -> bool {
        if let Some((num, _)) = self.blocks.last_key_value() {
            if num > &block.header().number() {
                return false;
            }
        }

        self.hashes
            .insert(block.header().hash(), block.header().number());
        block
            .transactions()
            .hashes()
            .enumerate()
            .for_each(|(i, tx)| {
                let location = TransactionLocation {
                    block: block.header().number(),
                    index: i,
                };
                self.txs.insert(tx, location);
            });

        self.blocks.insert(block.header().number(), block);
        true
    }

    fn prune(&mut self) {
        while self.blocks.len() > self.history_length {
            if let Some((number, _)) = self.blocks.first_key_value() {
                self.remove_block(*number);
            }
        }
    }

    fn prune_before(&mut self, n: u64) {
        while let Some((oldest, _)) = self.blocks.first_key_value() {
            let oldest = *oldest;
            if oldest < n {
                self.blocks.remove(&oldest);
            } else {
                break;
            }
        }
    }

    async fn backfill_behind(&mut self, n: u64) -> Result<bool> {
        if self.blocks.len() < 2 {
            return Ok(false);
        }

        if let Some(block) = self.blocks.get(&n) {
            let prev = n - 1;
            if !self.blocks.contains_key(&prev) {
                let backfilled = self.rpc.get_block(block.header().parent_hash()).await?;

                if N::is_hash_valid(&backfilled)
                    && block.header().parent_hash() == backfilled.header().hash()
                {
                    info!("backfilled: block={}", backfilled.header().number());
                    self.blocks.insert(backfilled.header().number(), backfilled);
                    Ok(true)
                } else {
                    warn!("bad block backfill");
                    Err(eyre!("bad backfill"))
                }
            } else {
                Ok(false)
            }
        } else {
            Ok(false)
        }
    }

    pub fn push_finalized_block(&mut self, block: N::BlockResponse) {
        if let Some(old_block) = self.blocks.get(&block.header().number()) {
            if old_block.header().hash() != block.header().hash() {
                self.blocks = BTreeMap::new();
            }
        }

        self.finalized_block = Some(block);
    }

    fn remove_block(&mut self, number: u64) {
        if let Some(block) = self.blocks.remove(&number) {
            self.hashes.remove(&block.header().hash());
            block.transactions().hashes().for_each(|tx| {
                self.txs.remove(&tx);
            });
        }
    }
}

struct TransactionLocation {
    block: u64,
    index: usize,
}

#[derive(Clone)]
pub enum FilterType {
    // filter content
    Logs(Box<Filter>),
    // block number when the filter was created or last queried
    NewBlock(u64),
    PendingTransactions,
}
