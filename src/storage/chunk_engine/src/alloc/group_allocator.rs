//! group 層級的配置器：管理「哪些 group 有磁碟空間」，是 `ChunkAllocator` 的下層。
//!
//! 一個 group = 某個 cluster 檔案裡連續 256 個 chunk 的空間，是 fallocate / punch hole 的單位。
//! `GroupAllocator` 只關心「完全空」的 group；一旦有 chunk 被配出去，group 就移交給
//! `ChunkAllocator` 的 active / full 集合管理，全部釋放後再交還這裡。
//!
//! ```text
//!                 get_unallocated_group_id()
//!   next_group_id ---------------------------+
//!   (never used, plus_one each time)         |
//!                                            v
//!   +----------------------+   Allocate task: fallocate     +----------------------+
//!   |  unallocated_groups  | -----------------------------> |   allocated_groups   |
//!   |  (no disk space)     | <----------------------------- |  (fallocated, empty) |
//!   +----------------------+   Deallocate task: punch hole  +----------+-----------+
//!              |                                                       |  ^
//!              | allocate() slow path:                   allocate()    |  | deallocate()
//!              | pool empty -> fallocate inline          (fast path)   |  | (last chunk freed)
//!              v                                                       v  |
//!          +-----------------------------------------------------------------------+
//!          |          ChunkAllocator: active_groups / full_groups / frozen          |
//!          +-----------------------------------------------------------------------+
//! ```
//!
//! 背景水位維持：C++ `AllocateWorker` 週期呼叫 `Engine::allocate_groups`，最後走到
//! `Allocator::do_allocate_task`，以 get / do / finish 三段讓 allocated 池維持在
//! `[min_remain, max_remain]` 之間，寫入路徑因此幾乎都能走 fast path（不碰檔案系統）。

use std::sync::Arc;

use super::super::*;

pub struct GroupAllocator {
    /// 已 fallocate、裡面一個 chunk 都沒用的 group（預備池）
    pub(super) allocated_groups: ShardsSet<GroupId>,
    /// 曾經用過、目前已 punch hole 歸還的 group id，優先於 `next_group_id` 重用
    pub(super) unallocated_groups: ShardsSet<GroupId>,
    /// 從未用過的最小 group id；`plus_one` 先換 cluster 再換 group，所以新 group 輪流落在
    /// 256 個檔案上（見 `types/group_id.rs`）
    pub(super) next_group_id: GroupId,
    pub(super) counter: Arc<AllocatorCounter>,
}

/// 背景維持預備池水位的工作單。`get_allocate_task` 產生時就把 group 從所有集合中取出，
/// 所以在 do 階段（鎖外）沒有人能碰到這個 group；`finish_allocate_task` 再依成敗放回。
#[derive(Debug, Clone, Copy)]
pub enum AllocateTask {
    None,
    /// 預備池太少：替這個 group fallocate 並在 RocksDB 寫入空的 group bits
    Allocate(GroupId),
    /// 預備池太多：刪除 RocksDB 的 group bits 並 punch hole 歸還空間
    Deallocate(GroupId),
}

impl GroupAllocator {
    /// 只給測試用的空白初始化；`next_group_id` 用 `GroupId::default()`（512KiB）。
    /// 正式路徑走 `ChunkAllocator::load`，會依 size class 正確設定 `next_group_id`。
    pub fn init(counter: Arc<AllocatorCounter>) -> Self {
        Self {
            allocated_groups: Default::default(),
            unallocated_groups: Default::default(),
            next_group_id: Default::default(),
            counter,
        }
    }

    /// 交出一個空 group 給 `ChunkAllocator` 開始配 chunk。
    ///
    /// 呼叫端持有 `Allocator` 的 Mutex。slow path 會在鎖內同步 fallocate，
    /// 這正是背景 `AllocateWorker` 要事先把預備池補滿的原因。
    pub fn allocate(&mut self, clusters: &Clusters, allow_to_allocate: bool) -> Result<GroupId> {
        // fast path：預備池有現成的空 group，純記憶體操作。
        // ShardsSet 是 64 個 HashSet，iter().next() 拿到的是任意一個，不保證 id 最小
        if let Some(&group_id) = self.allocated_groups.iter().next() {
            self.allocated_groups.remove(&group_id);
            Ok(group_id)
        } else if allow_to_allocate {
            // slow path：池子空了，當場 fallocate。
            // 這裡只配磁碟空間、不寫 RocksDB 的 group bits；第一個 chunk commit 時
            // MetaStore 的 merge(acquire) 會在 key 不存在時以空 GroupState 為底建立它。
            // 若 commit 前崩潰，重啟後這個 group 沒有 key，會被視為 unallocated，
            // 之後再 fallocate 一次即可（fallocate 對已配置的範圍是冪等的）
            let group_id = self.get_unallocated_group_id();
            tracing::info!("allocate group slow path {:?}", group_id);
            let result = clusters.allocate(group_id);
            if let Err(err) = result {
                self.unallocated_groups.insert(group_id);
                return Err(err);
            }
            self.counter.allocate_group();
            Ok(group_id)
        } else {
            // C++ 端關閉了 allow_to_allocate（例如磁碟快滿），只准用預備池
            Err(Error::NoSpace)
        }
    }

    /// group 的最後一個 chunk 被釋放時由 `ChunkAllocator::deallocate` 呼叫：
    /// 空間仍保留（不 punch hole），直接回到預備池；多出來的由背景 Deallocate task 歸還。
    pub fn deallocate(&mut self, group_id: GroupId) {
        self.allocated_groups.insert(group_id);
    }

    /// 優先重用已歸還的 group id，沒有才往後開新的；讓檔案不會無限長大。
    fn get_unallocated_group_id(&mut self) -> GroupId {
        if let Some(&group_id) = self.unallocated_groups.iter().next() {
            self.unallocated_groups.remove(&group_id);
            group_id
        } else {
            let group_id = self.next_group_id;
            self.next_group_id = self.next_group_id.plus_one();
            group_id
        }
    }

    /// 三段式的第一段（鎖內）：依預備池大小決定要補、要還、還是不動，
    /// 並把目標 group 先從集合中摘掉，避免 do 階段在鎖外時被別人配走。
    ///
    /// ```text
    ///   allocated_groups.len()
    ///      < min_remain          -> Allocate(take id from unallocated / next_group_id)
    ///      > max_remain          -> Deallocate(take one out of allocated_groups)
    ///      in [min, max]         -> None
    /// ```
    pub fn get_allocate_task(&mut self, min_remain: usize, max_remain: usize) -> AllocateTask {
        if self.allocated_groups.len() < min_remain {
            AllocateTask::Allocate(self.get_unallocated_group_id())
        } else if self.allocated_groups.len() > max_remain {
            let group_id = *self.allocated_groups.iter().next().unwrap();
            self.allocated_groups.remove(&group_id);
            AllocateTask::Deallocate(group_id)
        } else {
            AllocateTask::None
        }
    }

    /// 三段式的第三段（鎖內）：依 do 階段成敗把 group 放回正確的集合，並更新計數。
    ///
    /// ```text
    ///   task         succ    -> goes to             counter
    ///   Allocate     true       allocated_groups    allocate_group   (+256 / +256)
    ///   Deallocate   true       unallocated_groups  deallocate_group (-256 / -256)
    ///   Allocate     false      unallocated_groups  (unchanged, retry later)
    ///   Deallocate   false      allocated_groups    (unchanged, still has disk space)
    /// ```
    pub fn finish_allocate_task(&mut self, task: AllocateTask, succ: bool) {
        // 每個分支回傳 HashSet::insert 的 bool，整個 match 的值以 `;` 丟棄，只為了型別一致
        match (task, succ) {
            (AllocateTask::Allocate(group_id), true) => {
                self.counter.allocate_group();
                self.allocated_groups.insert(group_id)
            }
            (AllocateTask::Deallocate(group_id), true) => {
                self.counter.deallocate_group();
                self.unallocated_groups.insert(group_id)
            }
            (AllocateTask::Allocate(group_id), false) => self.unallocated_groups.insert(group_id),
            (AllocateTask::Deallocate(group_id), false) => self.allocated_groups.insert(group_id),
            _ => false,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 驗證 fast / slow path：池空時依序開新 group，歸還的 group 會被優先重用，
    // allow_to_allocate=false 且池空時回 NoSpace
    #[test]
    fn test_group_allocator() {
        let dir = tempfile::tempdir().unwrap();

        let config = ClustersConfig {
            path: dir.path().into(),
            chunk_size: CHUNK_SIZE_NORMAL,
            create: true,
        };

        let clusters = Clusters::open(&config).unwrap();
        let counter = Arc::new(AllocatorCounter::new(CHUNK_SIZE_NORMAL));
        let mut group_allocator = GroupAllocator::init(counter);

        let group_id_1 = group_allocator.allocate(&clusters, true).unwrap();
        assert_eq!(group_id_1, GroupId::default());
        assert_eq!(group_allocator.next_group_id, group_id_1.plus_one());
        assert!(group_allocator.allocated_groups.is_empty());
        assert!(group_allocator.unallocated_groups.is_empty());

        let group_id_2 = group_allocator.allocate(&clusters, true).unwrap();
        assert_eq!(group_id_1.plus_one(), group_id_2);
        assert_eq!(group_allocator.next_group_id, group_id_2.plus_one());
        assert!(group_allocator.allocated_groups.is_empty());
        assert!(group_allocator.unallocated_groups.is_empty());

        group_allocator.deallocate(group_id_1);
        assert_eq!(group_allocator.next_group_id, group_id_2.plus_one());
        assert_eq!(group_allocator.allocated_groups.len(), 1);
        assert!(group_allocator.unallocated_groups.is_empty());

        let group_id_3 = group_allocator.allocate(&clusters, true).unwrap();
        assert_eq!(group_id_1, group_id_3);
        assert_eq!(group_allocator.next_group_id, group_id_2.plus_one());
        assert!(group_allocator.allocated_groups.is_empty());
        assert!(group_allocator.unallocated_groups.is_empty());

        group_allocator.allocate(&clusters, false).unwrap_err();
    }

    // 驗證 get / finish 兩段：失敗的 Allocate 會進 unallocated 並被下一次重用，
    // 失敗的 Deallocate 會放回 allocated，next_group_id 只在真的開新 id 時前進
    #[test]
    fn test_group_allocator_task() {
        let counter = Arc::new(AllocatorCounter::new(CHUNK_SIZE_NORMAL));
        let mut group_allocator = GroupAllocator::init(counter);
        assert!(group_allocator.allocated_groups.is_empty());
        assert!(group_allocator.unallocated_groups.is_empty());
        assert_eq!(group_allocator.next_group_id.cluster(), 0);

        let task = group_allocator.get_allocate_task(2, 4);
        assert!(matches!(task, AllocateTask::Allocate(_)));
        assert!(group_allocator.allocated_groups.is_empty());
        assert!(group_allocator.unallocated_groups.is_empty());
        assert_eq!(group_allocator.next_group_id.cluster(), 1);

        group_allocator.finish_allocate_task(task, false);
        let task = group_allocator.get_allocate_task(2, 4);
        assert!(matches!(task, AllocateTask::Allocate(_)));
        assert!(group_allocator.allocated_groups.is_empty());
        assert!(group_allocator.unallocated_groups.is_empty());
        assert_eq!(group_allocator.next_group_id.cluster(), 1);

        group_allocator.finish_allocate_task(task, true);
        assert_eq!(group_allocator.allocated_groups.len(), 1);
        assert_eq!(group_allocator.unallocated_groups.len(), 0);
        assert_eq!(group_allocator.next_group_id.cluster(), 1);

        let task = group_allocator.get_allocate_task(2, 4);
        assert!(matches!(task, AllocateTask::Allocate(_)));
        group_allocator.finish_allocate_task(task, true);
        assert_eq!(group_allocator.allocated_groups.len(), 2);
        assert_eq!(group_allocator.unallocated_groups.len(), 0);
        assert_eq!(group_allocator.next_group_id.cluster(), 2);

        let task = group_allocator.get_allocate_task(2, 4);
        assert!(matches!(task, AllocateTask::None));
        group_allocator.finish_allocate_task(task, true);
        assert_eq!(group_allocator.allocated_groups.len(), 2);
        assert_eq!(group_allocator.unallocated_groups.len(), 0);
        assert_eq!(group_allocator.next_group_id.cluster(), 2);

        let task = group_allocator.get_allocate_task(3, 4);
        assert!(matches!(task, AllocateTask::Allocate(_)));
        group_allocator.finish_allocate_task(task, false);
        assert_eq!(group_allocator.allocated_groups.len(), 2);
        assert_eq!(group_allocator.unallocated_groups.len(), 1);
        assert_eq!(group_allocator.next_group_id.cluster(), 3);

        let task = group_allocator.get_allocate_task(1, 1);
        assert!(matches!(task, AllocateTask::Deallocate(_)));
        group_allocator.finish_allocate_task(task, false);
        assert_eq!(group_allocator.allocated_groups.len(), 2);
        assert_eq!(group_allocator.unallocated_groups.len(), 1);
        assert_eq!(group_allocator.next_group_id.cluster(), 3);
    }
}
