//! 11 個 size class 配置器的集合，是 `Engine` 唯一直接面對的配置入口。
//!
//! ```text
//!  vec index :   0      1       2       3     4     5     6      7      8      9      10
//!  chunk size: 64KiB 128KiB  256KiB  512KiB 1MiB  2MiB  4MiB   8MiB  16MiB  32MiB  64MiB
//!                |                                          |      |                   |
//!                +-------------- normal groups -------------+      +-- ultra groups ---+
//!                                (<= CHUNK_SIZE_LARGE)               (> 4MiB)
//!
//!  index = log2(chunk_size) - CHUNK_SIZE_SHIFT(16)
//!
//!  on disk:  <engine_path>/<chunk_size as string>/00 .. FF   e.g. <path>/512KiB/3A
//! ```
//!
//! normal 與 ultra 分開維持預備池，因為 ultra 的一個 group 很大（64MiB * 256 = 16GiB），
//! C++ `AllocateWorker` 對兩者用不同的 min / max 水位（ultra 預設 min = 0，不預先配置）。

use super::super::*;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub struct Allocators {
    /// 依 chunk size 由小到大排列，index 對應關係見檔頭
    pub vec: [Arc<Allocator>; CHUNK_SIZE_NUMBER],
    /// 背景 Allocate / Deallocate task 要寫 group bits，所以持有一份
    meta_store: Arc<MetaStore>,
}

impl Allocators {
    /// 開啟（或建立）11 個 size class 的資料目錄，並各自從 RocksDB 重建配置狀態。
    pub fn new(path: &Path, create: bool, meta_store: Arc<MetaStore>) -> Result<Self> {
        let mut allocators = vec![];
        for i in 0..CHUNK_SIZE_NUMBER {
            let chunk_size = CHUNK_SIZE_SMALL * (1 << i);
            let allocator = Self::create(path, create, &meta_store, chunk_size)?;
            allocators.push(allocator);
        }

        // Vec 轉固定長度陣列；寫死 11 個 index，CHUNK_SIZE_NUMBER 改了這裡也得跟著改
        Ok(Self {
            vec: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map(|i| allocators[i].clone()),
            meta_store,
        })
    }

    fn create(
        path: &Path,
        create: bool,
        meta_store: &Arc<MetaStore>,
        chunk_size: Size,
    ) -> Result<Arc<Allocator>> {
        // 目錄名用 Size 的 Display，例如 "64KiB"、"512KiB"、"64MiB"
        let cluster_config = ClustersConfig {
            path: path.join(chunk_size.to_string()),
            chunk_size,
            create,
        };
        let clusters = Clusters::open(&cluster_config)?;
        let allocator = Allocator::load(clusters, meta_store.iterator())?;
        tracing::info!("Allocator {:?} is created...", allocator.clusters);
        Result::Ok(allocator)
    }

    /// 依既有位置找回它所屬的配置器（位置裡編了 chunk_size）。
    /// 用於從 RocksDB 載入 meta 後建立 Chunk、以及 compaction 收尾。
    pub fn select_by_pos(&self, pos: Position) -> Result<&Arc<Allocator>> {
        let chunk_size = pos.chunk_size();
        if chunk_size.is_power_of_two()
            && CHUNK_SIZE_SMALL <= chunk_size
            && chunk_size <= CHUNK_SIZE_ULTRA
        {
            Ok(&self.vec[chunk_size.trailing_zeros() as usize - CHUNK_SIZE_SHIFT])
        } else {
            Err(Error::InvalidArg(format!(
                "select allocator invalid pos: {pos:?}"
            )))
        }
    }

    /// 依資料長度挑 size class：取 >= size 的最小 2 冪，下限 64KiB、上限 64MiB。
    /// 例：100KiB -> 128KiB、512KiB -> 512KiB、512KiB + 1 -> 1MiB。
    pub fn select_by_size(&self, size: Size) -> Result<&Arc<Allocator>> {
        if size <= CHUNK_SIZE_SMALL {
            Ok(&self.vec[0])
        } else if size <= CHUNK_SIZE_ULTRA {
            Ok(&self.vec[size.next_power_of_two().trailing_zeros() as usize - CHUNK_SIZE_SHIFT])
        } else {
            Err(Error::InvalidArg(format!(
                "select allocator invalid size: {size:?}"
            )))
        }
    }

    pub fn allocate(&self, size: Size, allow_to_allocate: bool) -> Result<Chunk> {
        let allocator = self.select_by_size(size)?;
        allocator.allocate(allow_to_allocate)
    }

    /// 背景水位維持的入口（`Engine::allocate_groups` / `allocate_ultra_groups` 呼叫）。
    /// 對每個符合 normal / ultra 篩選的 size class，最多做 `batch_size` 次
    /// `do_allocate_task`；遇到 None（水位已達標）或錯誤就換下一個 size class。
    /// 回傳實際完成的 Allocate + Deallocate 次數。
    pub fn allocate_groups(
        &self,
        min_remain: usize,
        max_remain: usize,
        batch_size: usize,
        allocate_ultra_groups: bool,
    ) -> usize {
        let mut finish = 0usize;
        for allocator in &self.vec {
            let is_ultra = allocator.clusters.chunk_size > CHUNK_SIZE_LARGE;
            if is_ultra != allocate_ultra_groups {
                continue;
            }
            for _ in 0..batch_size {
                match allocator.do_allocate_task(min_remain, max_remain, &self.meta_store) {
                    Ok(AllocateTask::None) => break,
                    Ok(_) => {
                        finish += 1;
                        continue;
                    }
                    Err(_) => break,
                }
            }
        }
        finish
    }

    pub fn used_size(&self) -> UsedSize {
        self.vec
            .iter()
            .map(|allocator| allocator.counter.used_size())
            .sum()
    }

    /// 名稱雖叫 allocate，實際是取 compaction task：每個 size class 最多挑一個 group 凍結，
    /// 由 `Engine::compact_groups` 搬完後呼叫 `finish_compact_task`。
    /// TinyVec 內嵌 3 個，超過才落到 heap（最多 11 個）。
    pub fn get_allocate_tasks(&self, max_reserved: u64) -> tinyvec::TinyVec<[GroupId; 3]> {
        self.vec
            .iter()
            .filter_map(|allocator| allocator.get_compact_task(max_reserved))
            .collect()
    }

    pub fn finish_compact_task(&self, group_id: GroupId) {
        self.select_by_pos(Position::new(group_id, 0))
            .unwrap()
            .finish_compact_task(group_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 驗證 select_by_pos / select_by_size 的 size class 對應與上下限檢查
    #[test]
    fn test_allocators() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let meta_config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: path.join("meta"),
                create: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let meta_store = Arc::new(MetaStore::open(&meta_config).unwrap());
        let allocators = Allocators::new(path, true, meta_store).unwrap();

        assert_eq!(
            allocators
                .select_by_pos(Position::new(GroupId::new(CHUNK_SIZE_NORMAL, 0, 0), 0))
                .unwrap()
                .clusters
                .chunk_size,
            CHUNK_SIZE_NORMAL
        );

        assert_eq!(
            allocators
                .select_by_size(CHUNK_SIZE_SMALL)
                .unwrap()
                .clusters
                .chunk_size,
            CHUNK_SIZE_SMALL
        );

        assert_eq!(
            allocators
                .select_by_size(CHUNK_SIZE_SMALL + 1)
                .unwrap()
                .clusters
                .chunk_size,
            CHUNK_SIZE_SMALL * 2,
        );

        assert_eq!(
            allocators
                .select_by_size(CHUNK_SIZE_NORMAL)
                .unwrap()
                .clusters
                .chunk_size,
            CHUNK_SIZE_NORMAL
        );

        assert_eq!(
            allocators
                .select_by_size(CHUNK_SIZE_NORMAL + 1)
                .unwrap()
                .clusters
                .chunk_size,
            CHUNK_SIZE_NORMAL * 2,
        );

        let used_size = allocators.used_size();
        assert_eq!(used_size.allocated_size, 0);
        assert_eq!(used_size.reserved_size, 0);

        assert!(allocators
            .select_by_pos(Position::new(GroupId::new(CHUNK_SIZE_ULTRA, 0, 0), 0))
            .is_ok());
        assert!(allocators
            .select_by_pos(Position::new(GroupId::new(Size::gibibyte(1), 0, 0), 0))
            .is_err());
        assert!(allocators.select_by_size(Size::gibibyte(1)).is_err());
    }
}
