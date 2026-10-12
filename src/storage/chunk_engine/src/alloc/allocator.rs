//! 單一 size class 的配置器外殼：把 `ChunkAllocator`（純狀態）包進 Mutex，
//! 並綁上該 size class 的 256 個資料檔 (`Clusters`) 與無鎖計數器。
//!
//! ```text
//!  Arc<Allocator>  (shared by every Chunk of this size class)
//!   |
//!   |-- Mutex<ChunkAllocator>   all bitset / rc / group-set mutations, short critical sections
//!   |-- Arc<AllocatorCounter>   lock-free counters, readable without the mutex
//!   `-- Clusters                pread / pwrite / fallocate, called WITHOUT the mutex
//!                               (except the inline slow path inside ChunkAllocator::allocate)
//! ```
//!
//! 每個 `Chunk` 都持有 `Arc<Allocator>`，所以 Chunk 自己就能 pread/pwrite，
//! 並在 Drop 時回頭呼叫 `dereference` 歸還位置。

use super::super::*;
use std::sync::{Arc, Mutex};

pub struct Allocator {
    allocator: Mutex<ChunkAllocator>,
    pub counter: Arc<AllocatorCounter>,
    pub clusters: Clusters,
}

impl Allocator {
    /// 啟動時呼叫：用 RocksDB iterator 重建此 size class 的配置狀態（見 `ChunkAllocator::load`）。
    pub fn load(clusters: Clusters, it: RocksDBIterator) -> Result<Arc<Allocator>> {
        let counter = Arc::new(AllocatorCounter::new(clusters.chunk_size));
        Ok(Arc::new(Self {
            allocator: Mutex::new(ChunkAllocator::load(
                it,
                counter.clone(),
                clusters.chunk_size,
            )?),
            counter,
            clusters,
        }))
    }

    /// 配一個新位置並包成 `Chunk`（rc 已是 1，meta 除了 pos 都是預設值，len = 0）。
    /// 回傳的 Chunk 若在 commit 前被 drop，位置會經由 Drop 自動歸還，不會洩漏。
    /// 收 `self: &Arc<Self>` 是為了讓新 Chunk 能 clone 一份 `Arc<Allocator>` 帶走。
    pub fn allocate(self: &Arc<Self>, allow_to_allocate: bool) -> Result<Chunk> {
        let this = self.as_ref();
        let mut allocator = this.allocator.lock().unwrap();
        allocator
            .allocate(&this.clusters, allow_to_allocate)
            .map(|pos| {
                Chunk::new(
                    ChunkMeta {
                        pos,
                        ..Default::default()
                    },
                    self.clone(),
                )
            })
    }

    /// 對既有位置建立一個新的 `Chunk` 物件並 rc + 1。用於 Chunk::clone、
    /// `Engine::get` 從 RocksDB 載入 meta、以及 `Engine::open` 恢復寫入中的 chunk。
    pub fn reference(self: &Arc<Self>, meta: ChunkMeta, first_ref: bool) -> Chunk {
        let mut allocator = self.allocator.lock().unwrap();
        allocator.reference(meta.pos, first_ref);
        Chunk::new(meta, self.clone())
    }

    /// 只由 `Chunk::drop` 呼叫；rc 歸零時記憶體 bit 被清除。
    pub fn dereference(&self, pos: Position) {
        let mut allocator = self.allocator.lock().unwrap();
        allocator.dereference(pos)
    }

    pub fn get_allocate_task(&self, min_remain: usize, max_remain: usize) -> AllocateTask {
        let mut allocator = self.allocator.lock().unwrap();
        allocator
            .group_allocator
            .get_allocate_task(min_remain, max_remain)
    }

    pub fn finish_allocate_task(&self, task: AllocateTask, succ: bool) {
        let mut allocator = self.allocator.lock().unwrap();
        allocator.group_allocator.finish_allocate_task(task, succ);
    }

    /// 背景維持預備池水位的一次動作（C++ `AllocateWorker` 經 `Allocators::allocate_groups` 呼叫）。
    ///
    /// fallocate、punch hole 與同步寫 RocksDB 都是慢操作，所以拆成三段，
    /// 只有第一、三段拿鎖，寫入路徑的 `allocate` 不會被背景 I/O 卡住：
    ///
    /// ```text
    ///   [lock]   get_allocate_task     pick group, remove it from every set
    ///   [unlock] do the slow I/O       fallocate + RocksDB put   (Allocate)
    ///                                  RocksDB delete + punch hole (Deallocate)
    ///   [lock]   finish_allocate_task  put group back by success / failure, update counter
    /// ```
    pub fn do_allocate_task(
        &self,
        min_remain: usize,
        max_remain: usize,
        meta_store: &MetaStore,
    ) -> Result<AllocateTask> {
        // 1. 鎖內決定要做什麼
        let task = self.get_allocate_task(min_remain, max_remain);

        // 2. 鎖外做 I/O。兩種順序都保證「RocksDB 有 key ⇒ 磁碟一定有空間」：
        //    Allocate 先 fallocate 再寫 key；Deallocate 先刪 key 再 punch hole。
        //    中途崩潰最多留下沒有 key 的已配置空間，重啟後被當成 unallocated 重用，不會壞資料
        let result = match task {
            AllocateTask::None => return Ok(task),
            AllocateTask::Allocate(group_id) => (|| {
                self.clusters.allocate(group_id)?;
                meta_store.allocate_group(group_id)
            })(),
            AllocateTask::Deallocate(group_id) => (|| {
                tracing::warn!("deallocate group: {:?}", group_id);
                meta_store.remove_group(group_id)?;
                self.clusters.deallocate(group_id)
            })(),
        };

        // 3. 鎖內依成敗收尾，失敗也要放回集合，否則 group 會從所有集合中消失
        self.finish_allocate_task(task, result.is_ok());

        result?;
        Ok(task)
    }

    /// compaction 的取 / 還兩段；中間的搬移 (`Engine::move_chunk`) 在鎖外由 Engine 完成。
    pub fn get_compact_task(&self, max_reserved: u64) -> Option<GroupId> {
        let mut allocator = self.allocator.lock().unwrap();
        allocator.get_compact_task(max_reserved)
    }

    pub fn finish_compact_task(&self, group_id: GroupId) {
        let mut allocator = self.allocator.lock().unwrap();
        allocator.finish_compact_task(group_id)
    }
}

impl Drop for Allocator {
    fn drop(&mut self) {
        tracing::info!("Allocator {:?} is dropping...", self.clusters);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 驗證：配了立刻 drop 的 Chunk 會歸還位置（連配 10000 次都拿到同一個位置）、
    // 多執行緒對不同 chunk 並行 pwrite/pread 正確，全部 drop 後 active / full 都清空
    #[test]
    fn test_allocator() {
        use rand::seq::SliceRandom;
        let dir = tempfile::tempdir().unwrap();

        let cluster_config = ClustersConfig {
            path: dir.path().into(),
            chunk_size: CHUNK_SIZE_NORMAL,
            create: true,
        };
        let clusters = Clusters::open(&cluster_config).unwrap();

        let meta_store_config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: dir.path().join("meta"),
                create: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let meta_store = Arc::new(MetaStore::open(&meta_store_config).unwrap());

        let allocator = Allocator::load(clusters, meta_store.iterator()).unwrap();

        for _ in 0..10000 {
            let chunk = allocator.allocate(true).unwrap();
            assert_eq!(chunk.meta().pos, Position::new(GroupId::default(), 0));
        }

        const N: usize = 1000;
        let mut chunks = vec![];
        for _ in 0..N {
            let chunk = allocator.allocate(true).unwrap();
            chunks.push(std::sync::Arc::new(chunk));
        }

        {
            let allocator = allocator.allocator.lock().unwrap();
            assert_eq!(allocator.full_groups.len(), N / 256);
            assert_eq!(allocator.active_groups.len(), 1);
            assert_eq!(
                allocator.active_groups.iter().next().unwrap().1.count() as usize,
                N % 256
            );
        }

        const T: usize = 8;
        (0..T)
            .map(|i| {
                let chunks = chunks.clone();
                std::thread::spawn(move || {
                    let mut vec = create_aligned_buf(ALIGN_SIZE);
                    vec.fill(0);
                    for chunk in chunks.iter() {
                        if chunk.meta().pos.index() as usize % T == i {
                            vec.fill(chunk.meta().pos.index());
                            chunk.pwrite(&vec[..], 0).unwrap();
                        }
                    }
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .for_each(|t| t.join().unwrap());

        chunks.shuffle(&mut rand::thread_rng());

        (0..T)
            .map(|i| {
                let chunks = chunks.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8];
                    for chunk in chunks.iter() {
                        if chunk.meta().pos.index() as usize % T == i {
                            assert!(chunk.pread(&mut buf, 0).is_ok());
                            assert_eq!(buf, [chunk.meta().pos.index(); 8]);
                        }
                    }
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .for_each(|t| t.join().unwrap());

        chunks.clear();

        {
            let allocator = allocator.allocator.lock().unwrap();
            assert!(allocator.full_groups.is_empty());
            assert!(allocator.active_groups.is_empty());
        }
    }

    // 驗證背景水位維持：預備池補到 min_remain 為止，超過 max_remain 時歸還，
    // allocated / reserved 計數隨之增減
    #[test]
    fn test_allocator_do_allocate_task() {
        let dir = tempfile::tempdir().unwrap();
        const S: Size = CHUNK_SIZE_NORMAL;

        let cluster_config = ClustersConfig {
            path: dir.path().into(),
            chunk_size: S,
            create: true,
        };
        let clusters = Clusters::open(&cluster_config).unwrap();

        let meta_store_config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: dir.path().join("meta"),
                create: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let meta_store = Arc::new(MetaStore::open(&meta_store_config).unwrap());

        let allocator = Allocator::load(clusters, meta_store.iterator()).unwrap();

        for _ in 0..4 {
            assert!(matches!(
                allocator.do_allocate_task(4, 8, &meta_store).unwrap(),
                AllocateTask::Allocate(_)
            ));
        }
        assert!(matches!(
            allocator.do_allocate_task(4, 8, &meta_store).unwrap(),
            AllocateTask::None
        ));

        let s = allocator.counter.used_size();
        assert_eq!(s.allocated_size, S * GroupState::TOTAL_BITS as u64 * 4);
        assert_eq!(s.reserved_size, S * GroupState::TOTAL_BITS as u64 * 4);

        for _ in 2..4 {
            assert!(matches!(
                allocator.do_allocate_task(1, 2, &meta_store).unwrap(),
                AllocateTask::Deallocate(_)
            ));
        }
        assert!(matches!(
            allocator.do_allocate_task(1, 2, &meta_store).unwrap(),
            AllocateTask::None
        ));

        let s = allocator.counter.used_size();
        assert_eq!(s.allocated_size, S * GroupState::TOTAL_BITS as u64 * 2);
        assert_eq!(s.reserved_size, S * GroupState::TOTAL_BITS as u64 * 2);
    }
}
