//! chunk 層級的配置器（單一 size class）：在 group 的 256-bit bitset 裡配置 / 釋放 chunk，
//! 並用 `position_rc` 追蹤每個位置還被幾個 `Chunk` 物件引用。
//!
//! 所有方法都是 `&mut self`，外層由 `Allocator` 的 `Mutex<ChunkAllocator>` 保護。
//!
//! group 在這一層的狀態機（GroupAllocator 管的是 unallocated / allocated 兩態）：
//!
//! ```text
//!                       allocate(): first chunk
//!  GroupAllocator  ---------------------------------+
//!  (allocated pool)                                 v
//!        ^                       +----------------------------------------+
//!        |   last chunk freed    |  active_groups  (GroupState in memory) |
//!        +---------------------- |   active_levels[0]: count   1..63      |
//!        |                       |   active_levels[1]: count  64..127     |
//!        |                       |   active_levels[2]: count 128..191     |
//!        |                       |   active_levels[3]: count 192..255     |
//!        |                       +---+-------------------------+----------+
//!        |          count == 256     |                         ^  one chunk freed
//!        |                           v                         |
//!        |                       +-------------------------------+
//!        |                       |  full_groups (no state kept)  |
//!        |                       +-------------------------------+
//!        |
//!        |   get_compact_task(): move emptiest active group -> frozen
//!        |                       +-------------------------------+
//!        +---------------------- |  frozen_groups (no new alloc) | --finish--> active
//!          all chunks moved out  +-------------------------------+   (if not emptied)
//! ```
//!
//! 挑選方向刻意相反：
//! - 配置從 level 3 往 0 找（最滿的先填）：新資料集中在少數 group，稀疏的 group 不再長大，
//!   比較有機會自然清空並回到預備池。
//! - compaction 從 level 0 往 3 找（最空的先搬）：要搬的 chunk 最少就能騰出整個 group。
//!
//! 釋放時序（記憶體 bit 與 RocksDB bit 是兩條獨立路徑）：
//!
//! ```text
//!  RocksDB bit : cleared at commit time by MetaStore (move_chunk / remove), in a WriteBatch
//!  memory bit  : cleared only when position_rc drops to 0, i.e. the last Chunk is dropped
//!
//!  Chunk::clone() -> Allocator::reference()   -> position_rc[pos] += 1
//!  Chunk::drop()  -> Allocator::dereference() -> position_rc[pos] -= 1
//!                                                  == 0 -> deallocate(pos): clear memory bit
//! ```
//!
//! 「rc 歸零就能釋放」成立的前提：`Engine.meta_cache` 不做淘汰，已載入的有效 chunk
//! 永遠被 cache 持有一個 `Arc`，所以 rc 歸零只會發生在該位置已被新版本取代或已刪除之後。

use super::super::*;
use std::collections::hash_map::Entry;

use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct ChunkAllocator {
    /// 256 個 chunk 全用完的 group；不存 GroupState（必然全 1），省記憶體
    pub full_groups: ShardsSet<GroupId>,
    /// 部分使用的 group 與其 bitset，是配置新 chunk 的來源
    pub active_groups: ShardsMap<GroupId, GroupState>,
    /// `active_groups` 依使用量分桶的索引（`GroupState::level()` = count / 64），
    /// 用來 O(1) 找到最滿或最空的 group
    pub(super) active_levels: [ShardsSet<GroupId>; GroupState::LEVELS],
    /// compaction 進行中的 group：仍可釋放 chunk，但不再從中配置新 chunk
    pub(super) frozen_groups: ShardsMap<GroupId, GroupState>,
    pub(super) group_allocator: GroupAllocator,
    /// 每個位置目前被幾個 `Chunk` 物件引用；不在表中的位置 rc 視為 0
    pub(super) position_rc: ShardsMap<Position, u32>,
    pub(super) counter: Arc<AllocatorCounter>,
}

impl ChunkAllocator {
    /// 只給測試用的空白配置器（GroupAllocator 的 next_group_id 固定是 512KiB 的 group 0）。
    pub fn with_chunk_size(chunk_size: Size) -> Self {
        let counter = Arc::new(AllocatorCounter::new(chunk_size));
        Self {
            full_groups: Default::default(),
            active_groups: Default::default(),
            active_levels: Default::default(),
            frozen_groups: Default::default(),
            group_allocator: GroupAllocator::init(counter.clone()),
            position_rc: Default::default(),
            counter,
        }
    }

    /// 啟動時從 RocksDB 的 group bits（key 前綴 0x02 + 此 size class 的 chunk_size）重建
    /// 全部記憶體狀態。
    ///
    /// ```text
    ///  keys are big-endian GroupId, so iteration order == numeric order:
    ///
    ///  current -> g0   g1   [gap]  g4   g5 ...
    ///             |    |     |     |
    ///             |    |     |     +-- bits classify: empty / full / partial
    ///             |    |     +-------- g2, g3 missing -> unallocated_groups
    ///             ...
    ///  after loop: next_group_id = last seen + 1
    ///
    ///  bits empty   -> allocated_groups (pool)   allocated += 256, reserved += 256
    ///  bits full    -> full_groups               allocated += 256
    ///  bits partial -> active_groups + level     allocated += 256, reserved += free
    /// ```
    pub fn load(
        mut it: RocksDBIterator,
        counter: Arc<AllocatorCounter>,
        chunk_size: Size,
    ) -> Result<Self> {
        // 1. 準備空集合；frozen 一定是空的，compaction 狀態不持久化
        let mut full_groups = ShardsSet::with_capacity(4096);
        let mut active_groups = ShardsMap::with_capacity(4096);
        let frozen_groups = ShardsMap::with_capacity(4096);
        let mut active_levels = std::array::from_fn(|_| ShardsSet::with_capacity(4096));

        let mut allocated_groups = ShardsSet::with_capacity(4096);
        let mut unallocated_groups = ShardsSet::with_capacity(4096);
        let mut current = GroupId::new(chunk_size, 0, 0);

        let mut allocated_count: u64 = 0;
        let mut reserved_count: u64 = 0;

        // 2. 掃描此 size class 的所有 group bits
        let prefix = MetaKey::group_bits_chunk_size_prefix(current);
        it.iterate(prefix, |key, value| {
            let group_id = MetaKey::parse_group_bits_key(key)?;
            let group_state = GroupState::from(value)?;

            // big-endian key 保證遞增；倒退代表 key 編碼或資料損毀
            assert!(
                current <= group_id,
                "current {current:?} > next {group_id:?}"
            );
            // 2a. 中間跳過的 id 沒有 key：從未配置或已 punch hole 歸還，都算 unallocated
            while current < group_id {
                unallocated_groups.insert(current);
                current.next();
            }
            current.next();

            // 2b. 有 key 就代表有磁碟空間，依 bitset 分類
            allocated_count += GroupState::TOTAL_BITS as u64;
            if group_state.is_empty() {
                allocated_groups.insert(group_id);
                reserved_count += GroupState::TOTAL_BITS as u64;
            } else if group_state.is_full() {
                full_groups.insert(group_id);
            } else {
                reserved_count += GroupState::TOTAL_BITS as u64 - group_state.count() as u64;
                active_levels[group_state.level() as usize].insert(group_id);
                active_groups.insert(group_id, group_state);
            }

            Ok(())
        })?;

        // 3. 設定計數並組裝；position_rc 從空開始，之後由 Engine::get 載入 chunk 時逐一 reference
        counter.init(allocated_count, reserved_count);
        let chunk_allocator = ChunkAllocator {
            full_groups,
            active_groups,
            active_levels,
            frozen_groups,
            counter: counter.clone(),
            group_allocator: GroupAllocator {
                allocated_groups,
                unallocated_groups,
                next_group_id: current,
                counter,
            },
            position_rc: ShardsMap::with_capacity(1 << 20),
        };

        Ok(chunk_allocator)
    }

    /// 配置一個 chunk 位置並建立第一個引用（rc = 1）。純記憶體操作，不寫 RocksDB；
    /// 持久化要等 `Engine::commit_chunk` 的 WriteBatch 帶上 merge(acquire)。
    ///
    /// ```text
    ///  active_groups non-empty?
    ///    yes -> scan level 3 -> 2 -> 1 -> 0, take any group in the first non-empty level
    ///             set bit (trailing_zeros of !bits)
    ///             count == 256        -> move to full_groups
    ///             crossed 64 boundary -> move to level + 1
    ///    no  -> GroupAllocator::allocate() (pool, or inline fallocate)
    ///             new GroupState, set bit 0, put into its level
    /// ```
    pub fn allocate(&mut self, clusters: &Clusters, allow_to_allocate: bool) -> Result<Position> {
        // 1. fast path：從最滿的 level 開始找部分使用的 group
        if !self.active_groups.is_empty() {
            for level in (0..GroupState::LEVELS).rev() {
                let set = &mut self.active_levels[level];
                if let Some(&group_id) = set.iter().next() {
                    let state = self.active_groups.get_mut(&group_id).unwrap();
                    // 在 active 的 group 必有空位，所以 unwrap 安全
                    let index = state.allocate().unwrap();
                    // 一次只多 1 個 chunk，level 最多上升一級；滿了優先處理
                    if state.is_full() {
                        self.full_groups.insert(group_id);
                        self.active_groups.remove(&group_id);
                        set.remove(&group_id);
                    } else if state.level() != level as u32 {
                        set.remove(&group_id);
                        self.active_levels[level + 1].insert(group_id);
                    }
                    let pos = Position::new(group_id, index);
                    self.reference(pos, true);
                    self.counter.allocate_chunk();
                    return Ok(pos);
                }
            }
        }

        // 2. 沒有部分使用的 group：向 GroupAllocator 要一個空 group
        //    （counter 的 allocate_group 在 group 拿到空間時已加過，這裡只扣掉一個槽位）
        let group_id = self.group_allocator.allocate(clusters, allow_to_allocate)?;
        self.counter.allocate_chunk();
        let state = match self.active_groups.entry(group_id) {
            Entry::Occupied(_) => panic!("should not be active groups: {:?}", group_id),
            Entry::Vacant(entry) => entry.insert(GroupState::empty()),
        };
        let index = state.allocate().unwrap();
        self.active_levels[state.level() as usize].insert(group_id);
        let pos = Position::new(group_id, index);
        self.reference(pos, true);
        Ok(pos)
    }

    /// 位置的引用計數 +1。由 `Allocator::reference`（Chunk::clone、Engine::get 載入、
    /// 啟動時恢復 writing chunk）與本檔的 `allocate` 呼叫。
    ///
    /// `first_ref = true` 表示呼叫端宣稱「這是此位置的第一個 Chunk」，會斷言 rc == 1，
    /// 用來抓出同一位置被兩個 chunk 同時佔用的錯誤。
    pub fn reference(&mut self, pos: Position, first_ref: bool) {
        // 1. 不變式檢查：被引用的位置在記憶體 bitset 裡必須已是 1（或整個 group 已滿）
        let group_id = pos.group_id();
        if let Some(state) = self.active_groups.get_mut(&group_id) {
            assert!(state.check(pos.index()), "ref pos failed: {:?}", pos);
        } else if let Some(state) = self.frozen_groups.get_mut(&group_id) {
            assert!(state.check(pos.index()), "ref pos failed: {:?}", pos);
        } else {
            assert!(self.full_groups.contains(&group_id));
        }

        // 2. rc += 1；第一次出現的位置同時讓 position_count + 1
        let rc = match self.position_rc.entry(pos) {
            Entry::Occupied(mut occupied_entry) => {
                let rc = occupied_entry.get_mut();
                *rc += 1;
                *rc
            }
            Entry::Vacant(vacant_entry) => {
                self.counter.position_count.fetch_add(1, Ordering::AcqRel);
                vacant_entry.insert(1);
                1
            }
        };
        self.counter.position_rc.fetch_add(1, Ordering::AcqRel);

        if first_ref {
            assert!(rc == 1, "should be first ref to pos {:?}, rc {}", pos, rc);
        }
    }

    /// 位置的引用計數 -1（由 `Chunk::drop` 經 `Allocator::dereference` 呼叫）。
    /// 歸零時清掉記憶體 bit，此位置才可能被重新配置出去。
    pub fn dereference(&mut self, pos: Position) {
        self.counter.position_rc.fetch_sub(1, Ordering::AcqRel);
        let count = self.position_rc.get_mut(&pos).unwrap();
        *count -= 1;
        if *count == 0 {
            self.counter.position_count.fetch_sub(1, Ordering::AcqRel);
            self.position_rc.remove(&pos);
            self.deallocate(pos);
        }
    }

    /// 清掉記憶體 bitset 中的一個 bit，並依 group 所在集合調整狀態。只改記憶體，
    /// RocksDB 的 bit 早在 commit 時就已由 MetaStore 清掉（或從未設過）。
    ///
    /// ```text
    ///  group is in      after clearing the bit
    ///  -------------    ------------------------------------------------------
    ///  active           empty           -> GroupAllocator pool (keep disk space)
    ///                   level dropped   -> move to level - 1
    ///  frozen           empty           -> GroupAllocator pool (compaction done)
    ///  full             255 used        -> becomes active again at level 3
    ///  none of above    unreachable!    (double free or unknown position)
    /// ```
    pub fn deallocate(&mut self, pos: Position) {
        let group_id = pos.group_id();
        if let Some(state) = self.active_groups.get_mut(&group_id) {
            // 先記下舊 level，清 bit 後 level 可能下降
            let level = state.level();
            state.deallocate(pos.index()).unwrap();
            if state.is_empty() {
                self.active_groups.remove(&group_id);
                self.active_levels[level as usize].remove(&group_id);
                self.group_allocator.deallocate(group_id);
            } else if state.level() != level {
                self.active_levels[level as usize].remove(&group_id);
                self.active_levels[level as usize - 1].insert(group_id);
            }
        } else if let Some(state) = self.frozen_groups.get_mut(&group_id) {
            // frozen 不在 active_levels 裡，不用維護 level；清空就代表 compaction 成功騰出整個 group
            state.deallocate(pos.index()).unwrap();
            if state.is_empty() {
                self.frozen_groups.remove(&group_id);
                self.group_allocator.deallocate(group_id);
            }
        } else if self.full_groups.contains(&group_id) {
            // full 不存 bitset，從全 1 重建後清掉這一個 bit
            let mut state = GroupState::full();
            state.deallocate(pos.index()).unwrap();
            self.active_levels[state.level() as usize].insert(group_id);
            self.active_groups.insert(group_id, state);
            self.full_groups.remove(&group_id);
        } else {
            unreachable!(
                "deallocate position failed! not found this position: {:?}",
                pos
            );
        }
        self.counter.deallocate_chunk();
    }

    /// 挑一個 group 做 compaction：已配置空間裡的空槽位（reserved_chunks）超過門檻才動作，
    /// 從 level 0（最空）開始挑，移到 frozen 後交給 `Engine::compact_groups` 逐一 move_chunk。
    /// 注意 `max_reserved` 與 reserved_chunks 比較的單位是 chunk 個數。
    pub fn get_compact_task(&mut self, max_reserved: u64) -> Option<GroupId> {
        let reserved = self.counter.reserved_chunks();
        if reserved <= max_reserved {
            return None;
        }

        for set in &mut self.active_levels {
            if let Some(&group_id) = set.iter().next() {
                set.remove(&group_id);
                let state = self.active_groups.remove(&group_id).unwrap();
                self.frozen_groups.insert(group_id, state);
                return Some(group_id);
            }
        }

        None
    }

    /// compaction 結束：group 若還在 frozen（有 chunk 沒搬走、或舊 Chunk 仍被讀取方持有），
    /// 放回 active；若已被 `deallocate` 清空移走，代表成功騰出整個 group。
    pub fn finish_compact_task(&mut self, group_id: GroupId) {
        if let Some(state) = self.frozen_groups.remove(&group_id) {
            self.active_levels[state.level() as usize].insert(group_id);
            self.active_groups.insert(group_id, state);
            tracing::info!("finish compact task and move back {:?}", group_id);
        } else {
            tracing::info!("finish compact task successful!");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 驗證同一 group 內依序配置、跨過 64 的邊界時從 level 0 升到 level 1、
    // 計數器的 allocated / reserved，以及配滿 256 個後移入 full_groups
    #[test]
    fn test_chunk_allocator() {
        let dir = tempfile::tempdir().unwrap();

        let config = ClustersConfig {
            path: dir.path().into(),
            chunk_size: CHUNK_SIZE_NORMAL,
            create: true,
        };

        let clusters = Clusters::open(&config).unwrap();
        let mut chunk_allocator = ChunkAllocator::with_chunk_size(CHUNK_SIZE_NORMAL);
        assert!(chunk_allocator.active_groups.is_empty());
        assert!(chunk_allocator
            .active_levels
            .iter()
            .all(|set| set.is_empty()));
        assert!(chunk_allocator.full_groups.is_empty());

        let one_level_count = GroupState::TOTAL_BITS / GroupState::LEVELS;
        for i in 0..(one_level_count - 1) {
            let pos = chunk_allocator.allocate(&clusters, true).unwrap();
            assert_eq!(pos, Position::new(GroupId::default(), i as _));
        }
        assert_eq!(chunk_allocator.active_groups.len(), 1);
        assert_eq!(chunk_allocator.active_levels[0].len(), 1);

        let pos = chunk_allocator.allocate(&clusters, true).unwrap();
        assert_eq!(
            pos,
            Position::new(GroupId::default(), one_level_count as u8 - 1)
        );
        assert_eq!(chunk_allocator.active_groups.len(), 1);
        assert_eq!(chunk_allocator.active_levels[0].len(), 0);
        assert_eq!(chunk_allocator.active_levels[1].len(), 1);

        let used_size = chunk_allocator.counter.used_size();
        assert_eq!(
            used_size.allocated_size,
            CHUNK_SIZE_NORMAL * GroupState::TOTAL_BITS
        );
        assert_eq!(
            used_size.reserved_size,
            CHUNK_SIZE_NORMAL * (GroupState::TOTAL_BITS - one_level_count)
        );

        for i in one_level_count..GroupState::TOTAL_BITS {
            let pos = chunk_allocator.allocate(&clusters, true).unwrap();
            assert_eq!(pos, Position::new(GroupId::default(), i as _));
        }
        assert!(chunk_allocator.active_groups.is_empty());
        assert!(chunk_allocator
            .active_levels
            .iter()
            .all(|set| set.is_empty()));
        assert_eq!(chunk_allocator.full_groups.len(), 1);
    }

    // 釋放一個從未配置的位置必須 panic（unreachable!）
    #[test]
    #[should_panic(expected = "not found this position")]
    fn test_chunk_invalid_deallocate() {
        let mut allocator = ChunkAllocator::with_chunk_size(CHUNK_SIZE_NORMAL);
        allocator.deallocate(Position::default());
    }
}
