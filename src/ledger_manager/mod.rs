use crate::block::Content;
use crate::blockchain::BlockChain;
use crate::blockdb::BlockDatabase;
use crate::crypto::hash::{Hashable, H256};
use crate::experiment::performance_counter::PERFORMANCE_COUNTER;

use crate::transaction::{CoinId, Output, Transaction};
use crate::utxodb::UtxoDatabase;
use crate::wallet::Wallet;
use crossbeam::channel;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;

// 该结构体扮演了账本管理器的角色
pub struct LedgerManager {
    // Arc是Rust中的一个智能指针类型，可以安全的在线程之间共享数据

    // 区块链数据库，用于存储和检索区块数据
    blockdb: Arc<BlockDatabase>,
    // 区块链，将所有区块按照特定规则连接起来。每个区块都包含了一些交易数据
    chain: Arc<BlockChain>,
    // 未花费交易输出数据库，用于存储和检索所有的未花费交易输出
    utxodb: Arc<UtxoDatabase>,
    // 钱包用于管理用户的钥匙对以及创建和签名交易
    wallet: Arc<Wallet>,
}

impl LedgerManager {
    pub fn new(
        blockdb: &Arc<BlockDatabase>,
        chain: &Arc<BlockChain>,
        utxodb: &Arc<UtxoDatabase>,
        wallet: &Arc<Wallet>,
    ) -> Self {
        Self {
            blockdb: Arc::clone(&blockdb),
            chain: Arc::clone(&chain),
            utxodb: Arc::clone(&utxodb),
            wallet: Arc::clone(&wallet),
        }
    }
    // 启动一列的线程来处理区块链的交易
    pub fn start(self, buffer_size: usize, num_workers: usize) {
        // start thread that updates transaction sequence
        let blockdb = Arc::clone(&self.blockdb);
        let chain = Arc::clone(&self.chain);
        
        // 创建一个新的线程来并行处理交易序列的更新，提高了程序的性能
        // 使用了通道来在线程之间传递数据，保证了数据的安全性
        let (tx_diff_tx, tx_diff_rx) = channel::bounded(buffer_size);
        thread::spawn(move || loop {
            let tx_diff = update_transaction_sequence(&blockdb, &chain);
            tx_diff_tx.send(tx_diff).unwrap();
        });

        // start thread that dispatches jobs to utxo manager
        let _utxodb = Arc::clone(&self.utxodb);
        // Scoreboard notes the transaction ID of the coins that is being looked up, may be added,
        // or may be deleted. Before dispatching a transaction, we first check whether the input
        // and output are used by transactions being processed. If no, we will dispatch this
        // transaction. Otherwise, we will wait until this situation clears. This prevents Read
        // After Write (must ins/del then check), Write After Read (must check then ins/del), and
        // Write After Write (must ins then del) hazards. We can also do this at CoinId level, but
        // doing this at transaction hash level should be pretty sufficient.
        // 用于记录正在处理的交易的输入和输出
        // 在分发一个交易之前，会先检查这个交易的输入和输出是否
        // 被 scoreboard 中的其他交易使用。如果没有，就会分发这个交易。
        // 否则，就会等待，直到没有交易使用这个交易的输入或输出为止。
        // 这样做可以防止读后写、写后读和写后写的风险。
        let mut scoreboard: HashSet<H256> = HashSet::new();
        // Transaction coins keeps the mapping between transaction ID and the entries in the
        // scoreboard that this transaction is responsible for.
        let mut transaction_coins: HashMap<H256, Vec<H256>> = HashMap::new();
        // 有界通道，用于发送和接受交易
        let (transaction_tx, transaction_rx) = channel::bounded(buffer_size * num_workers);
        // 无界通道，用于发送和接受通知
        let (notification_tx, notification_rx) = channel::unbounded();
        // 无界通道，用于发送和接收币的差异
        let (coin_diff_tx, coin_diff_rx) = channel::unbounded();

        thread::spawn(move || {
            loop {
                // get the diff
                // 从通道获取被添加 和 被移除的 交易
                let (mut added_tx, mut removed_tx) = tx_diff_rx.recv().unwrap();

                // dispatch transactions
                // 处理被移除的交易
                for (t, h) in removed_tx.drain(..).rev() {
                    // drain the notification channel so that we mark all finished transaction as
                    // finished
                    // 对于每一个已经处理完成的交易进行删除
                    for processed in notification_rx.try_iter() {
                        let finished_coins = transaction_coins.remove(&processed).unwrap();
                        for hash in &finished_coins {
                            scoreboard.remove(&hash);
                        }
                    }

                    // collect the tx hash of all coins this tx will touch
                    // 将所有可能被修改的交易的哈希值收集起来
                    let mut touched_coin_transaction_hash: HashSet<H256> = HashSet::new();
                    touched_coin_transaction_hash.insert(h); // the transaction hash of all output coins
                    for input in &t.input {
                        touched_coin_transaction_hash.insert(input.coin.hash); // tx hash of input coin
                    }

                    // wait until we are not touching hot coins
                    // 防止冲突，使得当前处理的交易涉及的硬币没有正在被其他交易处理
                    while !scoreboard.is_disjoint(&touched_coin_transaction_hash) {
                        let processed = notification_rx.recv().unwrap();
                        let finished_coins = transaction_coins.remove(&processed).unwrap();
                        for hash in &finished_coins {
                            scoreboard.remove(&hash);
                        }
                    }

                    // mark the coins that we will be touching as hot
                    let mut touched: Vec<H256> = vec![];
                    for hash in touched_coin_transaction_hash.drain() {
                        touched.push(hash);
                        scoreboard.insert(hash);
                    }
                    transaction_coins.insert(h, touched);
                    transaction_tx.send((false, t, h)).unwrap();
                }
                for (t, h) in added_tx.drain(..) {
                    // drain the notification channel so that we mark all finished transaction as
                    // finished
                    for processed in notification_rx.try_iter() {
                        let finished_coins = transaction_coins.remove(&processed).unwrap();
                        for hash in &finished_coins {
                            scoreboard.remove(&hash);
                        }
                    }

                    // collect the tx hash of all coins this tx will touch
                    let mut touched_coin_transaction_hash: HashSet<H256> = HashSet::new();
                    touched_coin_transaction_hash.insert(h); // the transaction hash of all output coins
                    for input in &t.input {
                        touched_coin_transaction_hash.insert(input.coin.hash); // tx hash of input coin
                    }

                    // wait until we are not touching hot coins
                    while !scoreboard.is_disjoint(&touched_coin_transaction_hash) {
                        let processed = notification_rx.recv().unwrap();
                        let finished_coins = transaction_coins.remove(&processed).unwrap();
                        for hash in &finished_coins {
                            scoreboard.remove(&hash);
                        }
                    }

                    // mark the coins that we will be touching as hot
                    let mut touched: Vec<H256> = vec![];
                    for hash in touched_coin_transaction_hash.drain() {
                        touched.push(hash);
                        scoreboard.insert(hash);
                    }
                    transaction_coins.insert(h, touched);
                    transaction_tx.send((true, t, h)).unwrap();
                }
            }
        });

        // start utxo manager
        let utxo_manager = UtxoManager {
            utxodb: Arc::clone(&self.utxodb),
            transaction_chan: transaction_rx,
            coin_chan: coin_diff_tx,
            notification_chan: notification_tx,
        };
        utxo_manager.start(num_workers);

        // start thread that writes to wallet
        let wallet = Arc::clone(&self.wallet);
        thread::spawn(move || loop {
            let coin_diff = coin_diff_rx.recv().unwrap();
            wallet.apply_diff(&coin_diff.0, &coin_diff.1).unwrap();
        });
    }
}

#[derive(Clone)]
struct UtxoManager {
    utxodb: Arc<UtxoDatabase>,
    /// Channel for dispatching jobs (add/delete, transaction, hash of transaction).
    transaction_chan: channel::Receiver<(bool, Transaction, H256)>,
    /// Channel for returning added and removed coins.
    coin_chan: channel::Sender<(Vec<(CoinId, Output)>, Vec<CoinId>)>,
    /// Channel for notifying the dispatcher about the completion of processing this transaction.
    notification_chan: channel::Sender<H256>,
}

impl UtxoManager {
    fn start(self, num_workers: usize) {
        for _i in 0..num_workers {
            let cloned = self.clone();
            thread::spawn(move || {
                cloned.worker_loop();
            });
        }
    }

    fn worker_loop(&self) {
        loop {
            let (add, transaction, hash) = self.transaction_chan.recv().unwrap();
            if add {
                let diff = self.utxodb.add_transaction(&transaction, hash).unwrap();
                self.coin_chan.send(diff).unwrap();
            } else {
                let diff = self.utxodb.remove_transaction(&transaction, hash).unwrap();
                self.coin_chan.send(diff).unwrap();
            }
            self.notification_chan.send(hash).unwrap();
        }
    }
}

fn update_transaction_sequence(
    blockdb: &BlockDatabase,
    chain: &BlockChain,
) -> (Vec<(Transaction, H256)>, Vec<(Transaction, H256)>) {
    let diff = chain.update_ledger().unwrap();
    PERFORMANCE_COUNTER.record_deconfirm_transaction_blocks(diff.1.len());

    // gather the transaction diff
    let mut add: Vec<(Transaction, H256)> = vec![];
    let mut remove: Vec<(Transaction, H256)> = vec![];
    for hash in diff.0 {
        let block = blockdb.get(&hash).unwrap().unwrap();
        PERFORMANCE_COUNTER.record_confirm_transaction_block(&block);
        let content = match block.content {
            Content::Transaction(data) => data,
            _ => unreachable!(),
        };
        let mut transactions = content
            .transactions
            .iter()
            .map(|t| (t.clone(), t.hash()))
            .collect();
        // TODO: precompute the hash here. Note that although lazy-eval for tx hash, and we could have
        // just called hash() here without storing the results (the results will be cached in the struct),
        // such function call will be optimized away by LLVM. As a result, we have to manually pass the hash
        // here. The same for added transactions below. This is a very ugly hack.
        add.append(&mut transactions);
    }
    for hash in diff.1 {
        let block = blockdb.get(&hash).unwrap().unwrap();
        let content = match block.content {
            Content::Transaction(data) => data,
            _ => unreachable!(),
        };
        let mut transactions = content
            .transactions
            .iter()
            .map(|t| (t.clone(), t.hash()))
            .collect();
        remove.append(&mut transactions);
    }
    (add, remove)
}
