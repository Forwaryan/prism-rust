use crate::crypto::hash::{Hashable, H256};
use crate::transaction::{CoinId, Input, Transaction};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;

/// transactions storage
#[derive(Debug)]
pub struct MemoryPool {
    /// Number of transactions
    num_transactions: u64,
    /// Maximum number that the memory pool can hold
    max_transactions: u64,
    /// Counter for storage index
    counter: u64,
    /// By-hash storage
    /// 通过交易的哈希可以查找对应的条目
    by_hash: HashMap<H256, Entry>,
    /// Transactions by previous output, formatted as Input
    /// 通过输入可以查找对应的交易
    by_input: HashMap<Input, H256>,
    /// Storage for order by storage index, it is equivalent to FIFO
    /// 通过内存池中的索引可以查找对应的交易
    by_storage_index: BTreeMap<u64, H256>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// Transaction
    pub transaction: Transaction,
    /// counter of the tx
    /// 交易在内存池中的索引(顺序编号)
    storage_index: u64,
}

impl MemoryPool {
    pub fn new(size_limit: u64) -> Self {
        Self {
            num_transactions: 0,
            max_transactions: size_limit,
            counter: 0,
            by_hash: HashMap::new(),
            by_input: HashMap::new(),
            by_storage_index: BTreeMap::new(),
        }
    }

    
    /// Insert a tx into memory pool. The input of it will also be recorded.
    /// 将一个交易插入到内存池中
    pub fn insert(&mut self, tx: Transaction) {
        if self.num_transactions > self.max_transactions {
            return;
        }
        // assumes no duplicates nor double spends
        let hash = tx.hash();
        let entry = Entry {
            transaction: tx,
            storage_index: self.counter,
        };
        self.counter += 1;

        // associate all inputs with this transaction
        for input in &entry.transaction.input {
            self.by_input.insert(input.clone(), hash);
        }

        // add to btree
        self.by_storage_index.insert(entry.storage_index, hash);

        // add to hashmap
        self.by_hash.insert(hash, entry);

        self.num_transactions += 1;
    }

    pub fn get(&self, h: &H256) -> Option<&Entry> {
        let entry = self.by_hash.get(h)?;
        Some(entry)
    }

    /// Check whether a tx hash is in memory pool
    /// When adding tx into mempool, should check this.
    pub fn contains(&self, h: &H256) -> bool {
        self.by_hash.contains_key(h)
    }

    /// Check whether the input of a tx is already recorded. If so, this tx is a double spend.
    /// When adding tx into mempool, should check this.
    pub fn is_double_spend(&self, inputs: &[Input]) -> bool {
        inputs.iter().any(|input| self.by_input.contains_key(input))
    }

    // 用于移除内存池中的一个指定交易，并返回这个交易的条目
    fn remove_and_get(&mut self, hash: &H256) -> Option<Entry> {
        let entry = self.by_hash.remove(hash)?;
        for input in &entry.transaction.input {
            self.by_input.remove(&input);
        }
        self.by_storage_index.remove(&entry.storage_index);
        self.num_transactions -= 1;
        Some(entry)
    }

    /// Remove a tx by its hash, also remove its recorded inputs
    /// 用于移除内存池中的一个指定交易
    pub fn remove_by_hash(&mut self, hash: &H256) {
        self.remove_and_get(hash);
    }

    /// Remove potential tx that use this input.
    /// This function runs recursively, so it may remove more transactions.
    /// 根据输入删除交易，如果这个交易的输出被别的交易引用，也会被删除
    pub fn remove_by_input(&mut self, prevout: &Input) {
        //use a deque to recursively remove, in case there are multi level dependency between txs.
        // 可能会存在多级依赖关系，所以使用双端队列进行递归删除
        let mut queue: VecDeque<Input> = VecDeque::new();
        queue.push_back(prevout.clone());

        while let Some(prevout) = queue.pop_front() {
            if let Some(entry_hash) = self.by_input.get(&prevout) {
                let entry_hash = *entry_hash;
                let entry = self.remove_and_get(&entry_hash).unwrap();
                for (index, output) in entry.transaction.output.iter().enumerate() {
                    queue.push_back(Input {
                        coin: CoinId {
                            hash: entry_hash,
                            index: index as u32,
                        },
                        value: output.value,
                        owner: output.recipient,
                    });
                }
            }
        }
    }

    /// get n transaction by fifo
    /// 获取交易池的前k个交易
    pub fn get_transactions(&self, n: u32) -> Vec<Transaction> {
        self.by_storage_index
            .values()
            .take(n as usize)
            .map(|hash| self.get(hash).unwrap().transaction.clone())
            .collect()
    }

    /// get size/length
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }
}

#[cfg(test)]
pub mod tests {}
