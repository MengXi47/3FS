//! chunk_engine 的編排核心：`Engine` 把 MetaStore（RocksDB 中繼資料）、Allocators（記憶體配置器）、
//! meta_cache（chunk_id -> `Arc<Chunk>`）與 writing_list（寫入中 chunk）串成對外 API；
//! C++ 端透過 `cxx.rs` 的 FFI 呼叫這裡的方法。
//!
//! ```text
//!          C++ storage (UpdateJob / BatchReadJob / AllocateWorker / TargetMap)
//!                                   |  cxx.rs
//!                                   v
//! +------------------------------- Engine --------------------------------+
//! |                                                                       |
//! |  read API          write API (2-phase)          maintenance API       |
//! |  get / batch_get   update_chunk -> commit_chunk  allocate_groups       |
//! |  query_*           commit_chunks                 compact_groups        |
//! |                    handle_uncommitted_chunks     set_allow_to_allocate |
//! |       |                   |          |                 |              |
//! |       v                   v          v                 v              |
//! |  +-----------+    +--------------+  +-----------+  +--------------+   |
//! |  | meta_cache|    | writing_list |  | MetaStore |  |  Allocators  |   |
//! |  | id->Arc   |    | prefix->id-> |  | (RocksDB) |  | 11 size cls  |   |
//! |  | <Chunk>   |    | WritingHolder|  |           |  | -> Clusters  |   |
//! |  +-----------+    +--------------+  +-----------+  +--------------+   |
//! +-----------------------------------------------------------------------+
//! ```
//!
//! 一次寫入的生命週期（CRAQ 兩階段：先寫資料、等 chain ack、再切換中繼資料）：
//!
//! ```text
//!  update_chunk                                   commit_chunk
//!  ------------                                   ------------
//!  verify crc / versions / etag                   lock meta_cache entry(id)
//!  write data to disk (COW / append / new)        WriteBatch: meta, pos map,
//!  writing_list[prefix][id] = holder                group bits, used size,
//!  RocksDB put 0x09+id (sync)  ---- ack ---->       timestamp, del 0x09
//!  return WritingChunk                            meta_cache[id] = Arc<new>
//!   (data invisible to readers)                   unlock; drop -> leave list
//!                                                  (data visible to readers)
//! ```
//!
//! 鎖的層級：meta_cache 是 `LockMap`，`entry_by_ref` 會持有該 key 的鎖直到 entry 被 drop，
//! 同一執行緒對同一 key 重入會死鎖；批次操作一律依排序後的 chunk_id 取鎖避免互鎖。
//! writing_list 是 `DashMap`，只在插入／移除 holder 的短時間內持有分片鎖。

use super::super::*;
use lockmap::{EntryByRef, LockMap};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Deref;
use std::path::PathBuf;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 開啟 Engine 的設定；C++ 端經 `cxx.rs` 的 `create` 建構，測試用 toml 反序列化。
#[derive(Debug, Default, Deserialize)]
pub struct EngineConfig {
    /// 根目錄：底下有 `meta/`（RocksDB）與每個 size class 一個資料目錄（`64KiB/` ...）。
    pub path: PathBuf,
    /// true 時建立不存在的目錄與檔案；false 時缺檔即開啟失敗。
    pub create: bool,
    /// chunk_id 前幾個位元組視為分組前綴；C++ 端填 `sizeof(ChainId)`，即「依 chain 分組」。
    pub prefix_len: usize,
}

/// 單顆盤（一個 storage target）的 chunk 儲存引擎。
///
/// 所有欄位都是 `Arc` 或 `Copy`，`Clone` 只是多一個 handle，指向同一份狀態。
/// 注意：`Drop` 會呼叫 `stop_and_join`，任何一個 clone 被 drop 都會停掉共用的 workers。
#[derive(Clone)]
pub struct Engine {
    /// RocksDB 中繼資料（chunk meta、group bits、pos->chunk、used size、timestamp、writing log）。
    pub meta_store: Arc<MetaStore>,
    /// 11 個 size class（64KiB..64MiB）各一個 `Allocator`，負責位置配置與資料檔 I/O。
    pub allocators: Allocators,
    /// chunk_id -> 已提交版本的 `Arc<Chunk>`；讀取方拿到的就是這個 Arc。
    ///
    /// 不是可淘汰的快取：其中的 `Chunk` 持有位置引用計數，entry 被移除（或被替換）時若計數
    /// 歸零，該位置會在記憶體裡被釋放。所以只有刪除／搬移／覆寫時才會移除或替換 entry，
    /// 被存取過的 chunk 會一直留在這裡。
    pub meta_cache: Arc<LockMap<Bytes, ChunkArc>>,
    /// Rust 端背景執行緒；只有 `start_allocate_workers` 會填入（bench 與測試用），
    /// 正式環境由 C++ 的 `AllocateWorker` 直接呼叫 `allocate_groups` / `compact_groups`。
    pub workers: Arc<Mutex<Vec<Worker>>>,
    /// 是否允許為新資料擴張磁碟用量；C++ `CheckWorker` 在磁碟用量超過門檻時設為 false。
    /// false 時預配置水位降為 0、寫入路徑不准同步 fallocate（池子空了就回 `NoSpace`）。
    pub allow_to_allocate: Arc<AtomicBool>,
    pub metrics: Arc<Metrics>,
    /// 見 `EngineConfig::prefix_len`。
    pub prefix_len: usize,
    /// prefix(chain) -> chunk_id -> 寫入中（update 完、尚未 commit）的 chunk。
    ///
    /// 同一個 chunk 同時最多一筆寫入中的更新；`abort = true` 表示上一筆已放棄
    /// （WritingChunk 未成功 commit 就被 drop，或崩潰後從 RocksDB 復原），可被新更新取代。
    pub writing_list: Arc<WritingList>,
}

impl Engine {
    /// 開啟引擎並從上次崩潰中復原寫入中的 chunk。
    ///
    /// ```text
    ///  RocksDB 0x09 (writing log)          memory
    ///  --------------------------          ------
    ///  1. occupy: temporarily set bits  ->
    ///     of uncommitted positions
    ///  2. Allocators::new loads 0x02    ->  GroupState has those bits set
    ///  3.                                   writing_list[id] = holder{abort:true}
    ///                                       (holder keeps position rc >= 1)
    ///  4. vacate: clear the temporary   ->  bits stay set in memory only
    ///     bits in RocksDB
    ///  later: C++ TargetMap -> handle_uncommitted_chunks commits them
    /// ```
    pub fn open(config: &EngineConfig) -> Result<Self> {
        // 1. 開 RocksDB（<path>/meta）
        let meta_config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: config.path.join("meta"),
                create: config.create,
                ..Default::default()
            },
            prefix_len: config.prefix_len,
        };

        let mut meta_store = MetaStore::open(&meta_config)?;
        // 2. 先在 RocksDB 暫時佔住寫入中 chunk 的位置，必須早於 Allocators::new：
        //    這些位置還沒 commit，group bits 裡是 0；不先佔住，載入後會被當成空位再配出去，
        //    下面 allocator.reference 對「bit 必須已設」的 assert 也會失敗
        let uncommitted_chunks = meta_store.occupy_uncommitted_positions()?;
        let meta_store = Arc::new(meta_store);
        // 3. 每個 size class 掃一次 0x02 group bits，重建記憶體裡的配置狀態
        let allocators = Allocators::new(&config.path, config.create, meta_store.clone())?;
        // 預留 1M 個 entry、256 個分片；cache 只增不淘汰（見 Engine::meta_cache）
        let meta_cache = Arc::new(LockMap::with_capacity_and_shard_amount(1 << 20, 256));

        let engine = Self {
            meta_store: meta_store.clone(),
            allocators,
            meta_cache,
            workers: Default::default(),
            allow_to_allocate: Arc::new(AtomicBool::new(true)),
            metrics: Default::default(),
            prefix_len: config.prefix_len,
            writing_list: Default::default(),
        };

        if !uncommitted_chunks.is_empty() {
            // 4. 把寫入中的 chunk 放回 writing_list，標成 abort：
            //    - 新的 update 可以直接取代它
            //    - handle_uncommitted_chunks 會把它提交
            // resume writing chunks in memory.
            for (chunk_id, meta, _) in &uncommitted_chunks {
                // 先載入已提交版本（若有）進 meta_cache；原地追加時兩者共用同一個位置
                let old_chunk = engine.get(chunk_id)?;

                let prefix: Bytes = chunk_id[..engine.prefix_len].into();
                let mut writing_list = engine.writing_list.entry(prefix).or_default();
                let allocator = engine.allocators.select_by_pos(meta.pos)?;
                // 沒有已提交版本時，這必定是該位置的第一個引用（first_ref 會 assert rc == 1）
                let chunk = allocator.reference(meta.clone(), old_chunk.is_none());
                writing_list.insert(chunk_id.clone(), WritingHolder { chunk, abort: true });
            }
            // 5. 撤銷步驟 2 暫設的 bits；之後這些位置只靠記憶體引用計數保住，
            //    再崩潰一次會從 0x09 紀錄重走一遍同樣的流程
            meta_store.vacate_uncommitted_positions(uncommitted_chunks)?;
        }

        // 6. 中繼資料格式升級
        engine.upgrade_version()?;

        Ok(engine)
    }

    pub fn used_size(&self) -> UsedSize {
        self.allocators.used_size()
    }

    /// 維持 size <= 4MiB 各 size class 的空 group 預備池在 `[min_remain, max_remain]`：
    /// 不足就 fallocate 新 group，過多就 punch hole 歸還；每個 size class 最多做 `batch_size` 次。
    /// 回傳完成的配置／回收次數。正式環境由 C++ `AllocateWorker` 週期呼叫。
    ///
    /// ```text
    ///  allocated pool size:   0 ... min_remain ....... max_remain ... n
    ///  action:              <-- Allocate -->  (none)  <-- Deallocate -->
    /// ```
    pub fn allocate_groups(
        &self,
        min_remain: usize,
        max_remain: usize,
        batch_size: usize,
    ) -> usize {
        self.allocators.allocate_groups(
            // 禁止擴張時下限歸 0：不再預配置新 group，但超過上限的仍會回收
            if self.allow_to_allocate.load(Ordering::Acquire) {
                min_remain
            } else {
                0
            },
            max_remain,
            batch_size,
            false,
        )
    }

    /// 同 `allocate_groups`，但只處理 size > 4MiB（8MiB..64MiB）的 size class。
    /// 這些 group 很大（64MiB 的 group 是 16GiB），所以由 C++ 給一組獨立、較低的水位。
    pub fn allocate_ultra_groups(
        &self,
        min_remain: usize,
        max_remain: usize,
        batch_size: usize,
    ) -> usize {
        self.allocators.allocate_groups(
            if self.allow_to_allocate.load(Ordering::Acquire) {
                min_remain
            } else {
                0
            },
            max_remain,
            batch_size,
            true,
        )
    }

    /// 碎片整理：對「部分使用的 group 裡空閒 chunk 數」超過 `max_reserved` 的 size class，
    /// 挑一個最空的 group，把裡面的 chunk 全部搬走，讓整個 group 能回到預備池。
    /// 回傳成功整理完的 group 數。
    ///
    /// ```text
    ///  active (emptiest) --freeze--> frozen          (no new allocation from it)
    ///        |
    ///        |  scan RocksDB 0x03 + group prefix  -> chunk ids in this group
    ///        |  move_chunk(id) for each           -> copy to another group
    ///        v
    ///  last chunk freed -> group returns to allocated pool
    ///  finish_compact_task: still frozen? -> back to active
    /// ```
    pub fn compact_groups(&self, max_reserved: u64) -> usize {
        // 1. 每個 size class 最多挑一個 group 並凍結
        //    （名稱雖叫 allocate_tasks，回傳的是 compact 目標）
        let group_ids = self.allocators.get_allocate_tasks(max_reserved);
        if group_ids.is_empty() {
            return 0;
        }

        let mut finish = 0usize;
        for group_id in group_ids {
            // 2. 由 pos->chunk 反查表列出該 group 的 chunk，逐一搬移；
            //    move_chunk 回 Err 時 iterate 立即停止，這個 group 算失敗
            let mut it = self.meta_store.iterator();
            let result = it.iterate(
                MetaKey::group_to_chunks_key_prefix(group_id),
                |_, chunk_id| self.move_chunk(chunk_id).map(|_| ()),
            );
            match result {
                Ok(_) => finish += 1,
                Err(e) => tracing::error!("compact group {:?} failed: {:?}", group_id, e),
            }
            // 3. 解凍：group 若還有 chunk（有讀取方仍持有舊位置，或搬移失敗）就放回 active；
            //    已經清空的 group 早在最後一個位置釋放時就離開 frozen
            self.allocators.finish_compact_task(group_id);
        }

        finish
    }

    /// 啟動 `num` 條 Rust 背景執行緒維持預備池（水位固定 1..2、每輪 2 個）。
    /// 只有 `bin/bench.rs` 與測試使用；正式環境的對應角色是 C++ `AllocateWorker`。
    pub fn start_allocate_workers(&self, num: usize) {
        let workers = (0..num)
            .map(|i| {
                let allocators = self.allocators.clone();
                WorkerBuilder::default()
                    .name(format!("Allocate{}", i))
                    .spawn(move || {
                        // 有做事就馬上再跑一輪，沒事就睡 100ms
                        let finish = allocators.allocate_groups(1, 2, 2, false);
                        if finish != 0 {
                            WorkerState::Continue
                        } else {
                            WorkerState::Wait(std::time::Duration::from_millis(100))
                        }
                    })
            })
            .collect::<Vec<_>>();

        self.workers.lock().unwrap().extend(workers);
    }

    pub fn stop_and_join(&self) {
        let mut workers = self.workers.lock().unwrap();
        for mut worker in workers.drain(..) {
            worker.stop_and_join();
        }
    }

    /// 見 `Engine::allow_to_allocate`；C++ `CheckWorker` 依磁碟用量切換。
    pub fn set_allow_to_allocate(&self, val: bool) {
        self.allow_to_allocate.store(val, Ordering::Release)
    }

    /// Intentionally leaks Arc pointers to avoid shutdown overhead.
    ///
    /// # Safety
    ///
    /// This function **must only be called** when the process is guaranteed to exit immediately afterwards.
    /// Calling it in any other context (e.g., during a restart, in a test) will cause permanent memory leaks
    /// of the Arc-managed data. This is an internal method intended to be used exactly once in the process exit path.
    pub unsafe fn speed_up_quit(&self) {
        // 用意：meta_cache 可能有上百萬個 Arc<Chunk>，正常解構時每一個的 Drop 都要拿 allocator
        // 的 Mutex 做 dereference，行程結束前白白耗時。把這兩個 Arc 各洩漏一份，引用計數就永遠
        // 不會歸零，整張表跳過解構，交給 OS 回收。C++ 在 Components.cc 收尾時依
        // speed_up_quit 設定（預設 true）呼叫
        // There is a memory leak and should only be called when the process exits.
        let _ = Arc::into_raw(self.meta_cache.clone());
        let _ = Arc::into_raw(self.writing_list.clone());
    }

    /// 取得 chunk 已提交的版本；寫入中（未 commit）的版本不可見。
    ///
    /// 回傳的 `Arc<Chunk>` 保證位置在 drop 前不會被回收，即使同時被覆寫或刪除。
    /// 會短暫持有該 chunk_id 的 entry 鎖，所以正在 commit 同一個 chunk 時會等待。
    ///
    /// ```text
    ///  entry_by_ref(id) --lock-->  hit  : clone Arc
    ///                              miss : RocksDB 0x01 -> ChunkMeta
    ///                                     allocator.reference(pos)  rc = 1
    ///                                     insert Arc into cache
    ///  drop entry       --unlock
    /// ```
    pub fn get(&self, chunk_id: &[u8]) -> Result<Option<ChunkArc>> {
        let mut entry = self.meta_cache.entry_by_ref(chunk_id);
        self.get_with_entry(chunk_id, &mut entry)
    }

    /// 在呼叫端已持有的 entry 鎖下查 cache，未命中就從 RocksDB 載入並放進 cache。
    /// 供 commit 系列在「鎖住 entry -> 讀舊值 -> 寫 RocksDB -> 換新值」整段使用。
    fn get_with_entry(
        &self,
        chunk_id: &[u8],
        entry: &mut EntryByRef<Bytes, [u8], ChunkArc>,
    ) -> Result<Option<ChunkArc>> {
        match entry.get() {
            Some(chunk) => Ok(Some(chunk.clone())),
            None => {
                let meta = self.meta_store.get_chunk_meta(chunk_id)?;
                if let Some(mut meta) = meta {
                    // 舊資料可能沒有 etag，補一個由 checksum 推得的預設值
                    meta.set_default_etag_if_need();
                    let allocator = self.allocators.select_by_pos(meta.pos)?;
                    // cache 未命中代表沒有人持有這個位置，first_ref 會 assert rc == 1
                    let chunk = Arc::new(allocator.reference(meta, true));
                    entry.insert(chunk.clone());
                    Ok(Some(chunk))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// 一次取多個 chunk，回傳的是同一個時間點的一致快照。
    pub fn batch_get(&self, chunk_ids: &BTreeSet<Bytes>) -> Result<BTreeMap<Bytes, ChunkArc>> {
        let mut chunks = BTreeMap::<Bytes, ChunkArc>::new();

        // 依 BTreeSet 的排序逐一上鎖並把 entry 留在 entries 裡，全部取完才一起釋放：
        // 期間這些 chunk 都無法被 commit。排序取鎖與 commit_chunks / batch_remove 一致，避免互鎖
        let mut entries = Vec::with_capacity(chunk_ids.len());
        for chunk_id in chunk_ids {
            let mut entry = self.meta_cache.entry_by_ref(chunk_id);
            if let Some(chunk) = self.get_with_entry(chunk_id, &mut entry)? {
                chunks.insert(chunk_id.clone(), chunk);
            }
            entries.push(entry);
        }

        Ok(chunks)
    }

    /// 把 chunk 複製到同 size class 的新位置並切換中繼資料（compaction 用）。
    /// 回傳 `None` 表示 chunk 不存在，或搬移期間被別人改掉、這次搬移作廢。
    ///
    /// 樂觀並行：複製資料時不持有鎖，最後才鎖 entry 檢查 cache 裡還是不是同一個 Arc。
    ///
    /// ```text
    ///  get(id) -> old Arc ......... copy data to new pos (no lock) .........
    ///                                                      lock entry(id)
    ///                         cache[id] still == old Arc ?
    ///                           yes -> RocksDB move (sync) ; cache[id] = new
    ///                           no  -> drop new (its position is freed)
    ///                                                      unlock
    /// ```
    ///
    /// 讀碼觀察：`copy_chunk` 沿用舊 meta 的 timestamp，`MetaStore::move_chunk_mut` 對同一個
    /// timestamp key 先 put 再 delete，同一 WriteBatch 內結果是刪除，搬移後該 chunk 會從
    /// 0x06 時間索引消失；etag 也被清成空值，而這裡直接放進 cache 的新 Arc 不會經過
    /// `set_default_etag_if_need`。
    pub fn move_chunk(&self, chunk_id: &[u8]) -> Result<Option<ChunkArc>> {
        // 1. get chunk and keep reference.
        let old_chunk = match self.get(chunk_id)? {
            Some(chunk) => chunk,
            None => return Ok(None),
        };

        // 2. allocate new chunk.
        // copy_chunk 在同 size class 配新位置、以 allow_to_allocate = true 呼叫，
        // 不受 Engine::allow_to_allocate 限制；複製長度是 len 向上對齊 4KiB
        let new_chunk = old_chunk.copy_chunk()?;

        // 3. replace chunk.
        let mut entry = self.meta_cache.entry_by_ref(chunk_id);
        match entry.get() {
            // ptr_eq 而非比較 meta：只要中途有任何 commit，cache 裡就會換成另一個 Arc
            Some(chunk) if Arc::ptr_eq(chunk, &old_chunk) => {
                self.meta_store
                    .move_chunk(chunk_id, old_chunk.meta(), new_chunk.meta(), true)?;
                let new_chunk = Arc::new(new_chunk);
                // 舊 Arc 被換出 cache；等本函式的 old_chunk 與所有讀取方都 drop 後，舊位置才釋放
                entry.insert(new_chunk.clone());
                Ok(Some(new_chunk))
            }
            _ => Ok(None), // chunk is updated or deleted by other thread.
        }
    }

    /// 便利 API：一次完成 update + commit（不走 chain、不 sync）。
    /// `update_ver` 為 0，版本自動遞增；C++ 正式路徑不用這個，而是分開呼叫兩階段。
    pub fn write(
        &self,
        chunk_id: &[u8],
        data: &[u8],
        offset: u32,
        checksum: u32,
    ) -> Result<ChunkArc> {
        self.update(
            chunk_id,
            &mut UpdateReq {
                checksum,
                length: data.len() as _,
                offset,
                data: data.as_ptr() as u64,
                ..Default::default()
            },
        )
    }

    pub fn remove(&self, chunk_id: &[u8]) -> Result<ChunkArc> {
        self.update(
            chunk_id,
            &mut UpdateReq {
                is_remove: true,
                ..Default::default()
            },
        )
    }

    /// 把 chunk 長度設為 `length`：縮短只改 len 與 checksum，加長則補 0。
    /// truncate 以 `offset` 欄位傳遞目標長度、`length` 欄位為 0。
    pub fn truncate(&self, chunk_id: &[u8], length: u32) -> Result<ChunkArc> {
        self.update(
            chunk_id,
            &mut UpdateReq {
                is_truncate: true,
                offset: length,
                ..Default::default()
            },
        )
    }

    /// update_chunk 後立刻 commit_chunk（sync = false）；write / remove / truncate 與測試共用。
    fn update(&self, chunk_id: &[u8], req: &mut UpdateReq) -> Result<ChunkArc> {
        let chunk = self.update_chunk(chunk_id, req)?;
        self.commit_chunk(chunk, false)
    }

    /// 兩階段寫入的第一階段：驗證、把資料寫到磁碟、登記為寫入中，回傳 `WritingChunk`。
    ///
    /// 此時新版本對 `get` 不可見；呼叫端（C++ UpdateJob）等 chain 後繼 ack 後再
    /// `commit_chunk`。`WritingChunk` 若沒成功 commit 就被 drop，writing_list 中的 holder
    /// 會被標成 abort。`req` 的 `out_*` 欄位是回給 C++ 的結果，錯誤路徑也會填。
    ///
    /// ```text
    ///  1. verify crc32c(data)
    ///  2. old = get(id)                    -> out_commit_ver / out_chain_ver / out_checksum
    ///  3. chain_ver >= old.chain_ver ?
    ///     new_ver:  syncing      -> update_ver            (accept as is)
    ///               update_ver>0 -> must be old_ver + 1   (else Committed / Missing)
    ///               update_ver=0 -> old_ver + 1           (auto increment)
    ///     expected_tag == old.etag ?                      (conditional update)
    ///  4. write strategy:
    ///     +-------------------------------+---------------------------------------+
    ///     | old exists, is_remove         | clone old (same position)             |
    ///     | old exists, syncing           | copy-on-write to new position         |
    ///     |   or overwrite (off < len)    |                                       |
    ///     |   or off+len > capacity       |                                       |
    ///     | old exists, otherwise         | in-place safe_write on same position  |
    ///     |   (append / truncate)         |   (only bytes after old len change)   |
    ///     | old missing                   | allocate by off+len, safe_write       |
    ///     +-------------------------------+---------------------------------------+
    ///  5. update meta (versions, etag, uncommitted = true)
    ///  6. writing_list[prefix][id] = holder     (reject if a live update exists)
    ///  7. RocksDB put 0x09 + id -> meta  (sync)  -- crash-recoverable from here on
    ///  8. return WritingChunk (RAII guard of the writing_list entry)
    /// ```
    pub fn update_chunk(&self, chunk_id: &[u8], req: &mut UpdateReq) -> Result<WritingChunk> {
        // prefix_len 個位元組之後會被切出來當 writing_list / used size 的分組 key
        if chunk_id.len() < self.prefix_len {
            return Err(Error::InvalidArg(format!(
                "chunk id {:?} is too short for prefix {}",
                chunk_id, self.prefix_len
            )));
        }

        // 1. prepare update info.
        let data = if req.length != 0 {
            // SAFETY: req.data pointer must be valid and remain live for the duration of this function.
            // The slice is consumed synchronously and immediately within safe_write/copy_on_write calls
            // (lines 386-394, 398-404, 418-424). If writes become asynchronous in the future,
            // this would require refactoring to take ownership of the data (e.g., via Vec<u8>).
            let data =
                unsafe { std::slice::from_raw_parts(req.data as *const _, req.length as usize) };
            let checksum = crc32c::crc32c(data);
            // without_checksum：呼叫端沒算 checksum，以本地算出的為準；
            // 否則必須與傳入值一致，擋下網路或記憶體中的資料損毀
            if req.without_checksum {
                req.checksum = checksum;
            } else if checksum != req.checksum {
                return Err(Error::ChecksumMismatch(format!(
                    "invalid checksum local {:08x} != remote {:08x}",
                    checksum, req.checksum
                )));
            }
            data
        } else {
            &[]
        };

        // 2. get old chunk.
        // 只取已提交版本，entry 鎖在 get 回傳時就釋放；同一 chunk 的並行更新由下面的
        // writing_list 擋掉，而不是靠這把鎖
        let old_chunk = self.get(chunk_id)?;
        let etag = match &old_chunk {
            Some(chunk) => {
                // create_new：只允許建立新 chunk，已存在就失敗
                if req.create_new {
                    return Err(Error::ChunkAlreadyExists);
                }
                let meta = chunk.meta();
                req.out_commit_ver = meta.chunk_ver;
                req.out_chain_ver = meta.chain_ver;
                req.out_checksum = meta.checksum;
                meta.etag.as_slice()
            }
            None => {
                // 刪除不存在的 chunk 要能成功（冪等）：把「目前版本」假裝成 update_ver - 1，
                // 讓步驟 3 的「必須剛好 +1」檢查通過
                if req.is_remove && req.update_ver > 0 {
                    req.out_commit_ver = req.update_ver - 1;
                } else {
                    req.out_commit_ver = 0;
                }
                req.out_chain_ver = req.chain_ver;
                req.out_checksum = 0;
                req.out_non_existent = true;
                &[]
            }
        };

        // 3. check version.
        // chain_ver：chain 成員變更時遞增；拒絕來自舊 chain 版本的請求
        if req.chain_ver < req.out_chain_ver {
            return Err(Error::ChainVersionMismatch(format!(
                "req.chain_ver {} < meta.chain_ver {}",
                req.chain_ver, req.out_chain_ver
            )));
        }

        // chunk_ver：
        //   - syncing（resync 時由前驅送來整份 chunk）：直接採用對方版本，可以跳號
        //   - 一般更新：必須剛好是目前版本 + 1；<= 代表重送已提交的更新，> +1 代表中間漏了一筆
        //   - update_ver == 0：本地 API（write/remove/truncate）自動遞增
        let new_chunk_ver = if req.is_syncing {
            req.update_ver
        } else if req.update_ver > 0 {
            if req.update_ver <= req.out_commit_ver {
                return Err(Error::ChunkCommittedUpdate(format!(
                    "committed update {} <= {}",
                    req.update_ver, req.out_commit_ver
                )));
            } else if req.update_ver > req.out_commit_ver + 1 {
                return Err(Error::ChunkMissingUpdate(format!(
                    "missing update {} > {} + 1",
                    req.update_ver, req.out_commit_ver
                )));
            }
            req.update_ver
        } else {
            req.out_commit_ver + 1
        };

        // 條件更新：呼叫端指定 expected_tag 時，必須與目前 etag 相同才執行
        if !req.expected_tag.is_empty() && req.expected_tag != etag {
            return Err(Error::ChunkETagMismatch(format!(
                "expected {:?} != real {:?}",
                req.expected_tag, etag
            )));
        }

        // 4. do update.
        // 核心不變式：已提交範圍的內容絕不原地改變。讀取方可能正握著舊 Arc 讀舊位置，
        // 所以會動到舊 len 以內資料的寫入一律換位置（COW），只有「舊 len 之後」才能原地寫
        // （safe_write 為了對齊可能重寫最後一個 4KiB 區塊，但寫回的是讀出來的相同內容）
        let mut new_chunk = match old_chunk {
            // 刪除：沿用舊位置（Chunk::clone 讓位置引用計數 +1），真正釋放在 commit 之後
            Some(old_chunk) if req.is_remove => old_chunk.as_ref().clone(),
            // COW：整份 sync、覆寫既有範圍、或超出目前 size class 容量
            Some(old_chunk)
                if req.is_syncing
                    || (req.length > 0 && req.offset < old_chunk.meta().len)
                    || req.offset + req.length > old_chunk.capacity() =>
            {
                // 依新長度選 size class 配新位置；需要時先讀回舊資料再合併寫入
                old_chunk.copy_on_write(
                    data,
                    req.offset,
                    req.checksum,
                    req.is_syncing,
                    self.allow_to_allocate.load(Ordering::Acquire),
                    &self.allocators,
                    &self.metrics,
                )?
            }
            // 原地：追加在舊 len 之後且不超過容量、truncate、或零長度寫入。
            // new_chunk 與舊版共用同一個 Position；舊 Arc 的 meta.len 不變，讀取方看不到新尾巴
            Some(old_chunk) => {
                let mut new_chunk = old_chunk.as_ref().clone();
                new_chunk.safe_write(
                    data,
                    req.offset,
                    req.checksum,
                    req.is_truncate,
                    &self.metrics,
                )?;
                new_chunk
            }
            // 新 chunk：依 offset + length 選 size class。
            // 注意刪除不存在的 chunk 也會走到這裡（長度 0 -> 64KiB class），配到的位置
            // 只是暫用，commit 時只清 writing log，回傳的 Arc drop 後位置即釋放
            None => {
                let begin = std::time::Instant::now();
                let mut new_chunk = self.allocators.allocate(
                    Size::from(req.offset + req.length),
                    self.allow_to_allocate.load(Ordering::Acquire),
                )?;
                let latency = std::time::Instant::now().duration_since(begin).as_micros() as _;
                self.metrics.allocate_times.fetch_add(1, Ordering::AcqRel);
                self.metrics
                    .allocate_latency
                    .fetch_add(latency, Ordering::AcqRel);
                new_chunk.safe_write(
                    data,
                    req.offset,
                    req.checksum,
                    req.is_truncate,
                    &self.metrics,
                )?;
                new_chunk
            }
        };

        // 5. 填回結果並更新 meta：版本、etag（desired_tag 或隨機）、uncommitted = true
        req.out_commit_ver = new_chunk_ver;
        req.out_chain_ver = req.chain_ver;
        req.out_checksum = new_chunk.meta().checksum;
        new_chunk.update_meta(req.deref());

        // 6. 登記到 writing_list；只在這個區塊內持有該 prefix 的 DashMap 分片鎖
        let prefix: Bytes = chunk_id[..self.prefix_len].into();
        let mut writing_list = self.writing_list.entry(prefix).or_default();
        match writing_list.entry(chunk_id.into()) {
            // 前一筆已放棄（未 commit 就 drop、或崩潰復原而來）：直接取代，
            // 舊 holder 的 Chunk 被 drop，其位置引用隨之釋放
            std::collections::hash_map::Entry::Occupied(mut occupied_entry)
                if occupied_entry.get().abort =>
            {
                occupied_entry.insert(WritingHolder {
                    chunk: new_chunk.clone(),
                    abort: false,
                });
            }
            // 還有一筆活著的寫入中更新：同一 chunk 不允許兩筆並行。
            // 此時剛寫好的 new_chunk 會隨函式返回被 drop，它的位置（若是新配的）也一併釋放
            std::collections::hash_map::Entry::Occupied(occupied_entry) => {
                return Err(Error::InvalidArg(format!(
                    "chunk {:?} is in writing ({:?})",
                    chunk_id,
                    occupied_entry.get().chunk.meta(),
                )));
            }
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(WritingHolder {
                    chunk: new_chunk.clone(),
                    abort: false,
                });
            }
        }
        drop(writing_list);

        // 7. 寫 0x09 寫入中紀錄（sync）。這之後崩潰，Engine::open 會把它復原回 writing_list；
        //    這之前崩潰，資料只寫在 RocksDB 不認得的位置（或舊 len 之後），等同沒寫過。
        //    注意：若這裡失敗，上面插入的 holder 仍是 abort = false（WritingChunk 尚未建立，
        //    沒有 Drop 能把它標成 abort），之後同一 chunk 的更新會被拒，
        //    直到 handle_uncommitted_chunks 處理該 prefix
        self.meta_store
            .persist_writing_chunk(chunk_id, new_chunk.meta())?;

        // 8. WritingChunk 是 RAII：commit 成功後 drop 會把自己移出 writing_list，
        //    否則把 holder 標成 abort（見 alloc/writing_chunk.rs）
        Ok(WritingChunk {
            chunk_id: chunk_id.into(),
            chunk: new_chunk,
            list: self.writing_list.clone(),
            prefix_len: self.prefix_len as _,
            is_remove: req.is_remove,
            commit_succ: false,
        })
    }

    /// 兩階段寫入的第二階段：原子地切換中繼資料，讓新版本對讀取方可見。
    ///
    /// 「讀舊值 -> 寫 RocksDB -> 換 cache」整段都在該 chunk_id 的 entry 鎖內完成，
    /// 所以同一 chunk 的切換是序列化的，讀取方不會看到 RocksDB 與 cache 不一致的中間態。
    /// `sync` 決定 RocksDB 寫入是否 fsync WAL。
    ///
    /// ```text
    ///  lock entry(id) ; old = cache or RocksDB
    ///  +------------------ one WriteBatch (atomic) ------------------+
    ///  | old exists, pos changed : put  0x01 meta                    |
    ///  |                           del  0x03 old_pos ; put 0x03 new  |
    ///  |                           merge 0x02 release old, acquire new|
    ///  |                           merge 0x04 size delta             |
    ///  | old exists, same pos    : put  0x01 meta (append in place)  |
    ///  | old missing             : put 0x01, put 0x03, merge 0x02/0x04|
    ///  | always                  : timestamp 0x06 update, del 0x09   |
    ///  +-------------------------------------------------------------+
    ///  cache[id] = Arc<new>   (old Arc leaves cache; its position is freed
    ///                          after the last reader drops it)
    ///  unlock ; drop WritingChunk -> removed from writing_list
    /// ```
    pub fn commit_chunk(&self, mut chunk: WritingChunk, sync: bool) -> Result<ChunkArc> {
        // uncommitted = false；回傳給呼叫端的 Arc 是另一個指向同一位置的 Chunk
        chunk.set_committed();
        let chunk_id = &chunk.chunk_id;
        let new_chunk: ChunkArc = (&chunk).into();

        if chunk.is_remove {
            let mut entry = self.meta_cache.entry_by_ref(chunk_id);
            match self.get_with_entry(chunk_id, &mut entry)? {
                // 一個 WriteBatch 刪 0x01/0x03/0x06/0x09、release 0x02 bit、扣 0x04 用量
                Some(old_chunk) => {
                    self.meta_store
                        .remove(&chunk.chunk_id, old_chunk.meta(), sync)?
                }
                // 本來就不存在：只需清掉 update_chunk 寫的 0x09 紀錄
                None => self.meta_store.remove_writing_chunk(chunk_id, sync)?,
            }
            // cache 釋出舊 Arc；呼叫端拿到的 new_chunk 與讀取方的 Arc 都 drop 後，
            // 位置才在記憶體釋放
            entry.remove();
            drop(entry);
            chunk.commit_succ();
            return Ok(new_chunk);
        }

        // update rocksdb under lock protection.
        // 舊值在鎖內重新讀取，而不是用 update_chunk 當時看到的版本：中間可能被
        // compaction 的 move_chunk 換過位置，這裡以 cache 的現值計算要 release 的舊位置
        let mut entry = self.meta_cache.entry_by_ref(chunk_id);
        match self.get_with_entry(chunk_id, &mut entry)? {
            Some(old_chunk) => {
                self.meta_store
                    .move_chunk(chunk_id, old_chunk.meta(), new_chunk.meta(), sync)?;
            }
            None => {
                self.meta_store
                    .add_chunk(chunk_id, new_chunk.meta(), sync)?;
            }
        }
        // RocksDB 寫入成功後才換 cache；上面任何一步失敗都會提早返回，cache 保持舊值，
        // chunk 以 commit_succ = false 被 drop，writing_list 中的 holder 轉為 abort
        entry.insert(new_chunk.clone());
        drop(entry);

        // 標記成功後，函式結束時 WritingChunk 的 Drop 會把自己移出 writing_list
        chunk.commit_succ();
        Ok(new_chunk)
    }

    /// `commit_chunk` 的批次版：所有 chunk 的中繼資料變更合進同一個 WriteBatch，一次原子寫入。
    ///
    /// ```text
    ///  1. reject duplicated ids
    ///  2. lock entries in sorted id order (BTreeSet) -> no deadlock with other batches
    ///  3. build one WriteBatch: remove_mut / move_chunk_mut / add_chunk_mut per chunk
    ///  4. RocksDB write (atomic)
    ///  5. update cache entries ; mark commit_succ
    ///  6. drop entries (unlock) ; drop chunks -> removed from writing_list
    /// ```
    pub fn commit_chunks(&self, mut chunks: Vec<WritingChunk>, sync: bool) -> Result<()> {
        // 1. 同一批不能有重複的 chunk_id（同一 entry 鎖兩次會死鎖，也無法定義先後）
        let chunk_ids = chunks
            .iter()
            .map(|c| c.chunk_id.clone())
            .collect::<BTreeSet<_>>();
        if chunk_ids.len() != chunks.len() {
            return Err(Error::InvalidArg("same chunk id in the batch!".into()));
        }

        // 2. 依排序後的 chunk_id 逐一上鎖並預載舊值；鎖一直持有到函式結束
        // lock it.
        let mut entries = HashMap::with_capacity(chunk_ids.len());
        for chunk_id in &chunk_ids {
            let mut entry = self.meta_cache.entry_by_ref(chunk_id);
            self.get_with_entry(chunk_id, &mut entry)?;
            entries.insert(chunk_id.clone(), entry);
        }

        // 3. 組 WriteBatch；此時還沒寫入，任何錯誤返回都不會改到 RocksDB 或 cache
        let mut write_batch = RocksDB::new_write_batch();
        for chunk in &mut chunks {
            chunk.set_committed();
            if chunk.is_remove {
                match entries.get_mut(&chunk.chunk_id).unwrap().get() {
                    Some(old_chunk) => self.meta_store.remove_mut(
                        &chunk.chunk_id,
                        old_chunk.meta(),
                        &mut write_batch,
                    )?,
                    // 例外：刪除不存在的 chunk 時，0x09 紀錄是立即單獨刪除，不在 WriteBatch 內
                    None => self
                        .meta_store
                        .remove_writing_chunk(&chunk.chunk_id, sync)?,
                }
            } else {
                match entries.get_mut(&chunk.chunk_id).unwrap().get() {
                    Some(old_chunk) => self.meta_store.move_chunk_mut(
                        &chunk.chunk_id,
                        old_chunk.meta(),
                        chunk.meta(),
                        &mut write_batch,
                    )?,
                    None => self.meta_store.add_chunk_mut(
                        &chunk.chunk_id,
                        chunk.meta(),
                        &mut write_batch,
                    )?,
                }
            }
        }
        // 4. 一次原子寫入
        self.meta_store.write(write_batch, sync)?;

        // 5. RocksDB 成功後才更新 cache；仍在 entry 鎖內
        for chunk in &mut chunks {
            chunk.commit_succ();
            if chunk.is_remove {
                entries.get_mut(&chunk.chunk_id).unwrap().remove();
            } else {
                entries
                    .get_mut(&chunk.chunk_id)
                    .unwrap()
                    .insert(chunk.deref().into());
            }
        }
        // 6. 返回時先 drop entries（解鎖），再 drop chunks（各自移出 writing_list）
        Ok(())
    }

    /// 列出某個 prefix（chain）目前寫入中的 chunk；C++ `StorageTarget::uncommitted` 使用。
    pub fn query_uncommitted_chunks(&self, prefix: &[u8]) -> Result<Vec<(Bytes, ChunkMeta)>> {
        // or_default 會順手為不存在的 prefix 插入一個空 map
        let writing_list = self.writing_list.entry(prefix.into()).or_default();
        let mut uncommitted_chunks = Vec::with_capacity(writing_list.len());
        for (chunk_id, holder) in writing_list.iter() {
            uncommitted_chunks.push((chunk_id.clone(), holder.chunk.meta().clone()));
        }
        Ok(uncommitted_chunks)
    }

    /// 把某個 chain 所有寫入中的 chunk 以新的 `chain_ver` 強制提交，回傳被提交的清單。
    ///
    /// 與 CRAQ 兩階段的關係：正常情況下 update_chunk 之後由 chain ack 觸發 commit；
    /// 當 chain 成員或狀態變更（C++ `TargetMap` 對 SERVING / SYNCING / WAITING 的 target 呼叫
    /// `StorageTarget::resetUncommitted`），那些 ack 可能永遠不會來，就由這裡收尾，
    /// 包括崩潰後在 `Engine::open` 復原、標成 abort 的 chunk。
    ///
    /// ```text
    ///  writing_list[prefix] : { id1: holder, id2: holder(abort), ... }
    ///        | set chain_ver on each holder, build WritingChunk (is_remove = false)
    ///        v
    ///  commit_chunks(all, sync = true)  -> visible, removed from writing_list
    /// ```
    ///
    /// 注意 `is_remove` 一律為 false（holder 沒有記錄原本是否為刪除），
    /// 寫入中的刪除在這裡會被當成一般更新提交：chunk 會被保留；若原本就不存在，
    /// 提交的是 update_chunk 為它配的空 chunk。
    pub fn handle_uncommitted_chunks(
        &self,
        prefix: &[u8],
        chain_ver: u32,
    ) -> Result<Vec<(Bytes, ChunkMeta)>> {
        let mut writing_list = self.writing_list.entry(prefix.into()).or_default();

        let mut chunks = Vec::with_capacity(writing_list.len());
        let mut uncommitted_chunks = Vec::with_capacity(writing_list.len());
        // commit it!
        for (chunk_id, holder) in writing_list.iter_mut() {
            holder.chunk.set_chain_ver(chain_ver);
            let chunk = holder.chunk.clone();
            chunks.push(WritingChunk {
                chunk_id: chunk_id.clone(),
                chunk,
                list: self.writing_list.clone(),
                prefix_len: self.prefix_len as _,
                is_remove: false,
                commit_succ: false,
            });
            uncommitted_chunks.push((chunk_id.clone(), holder.chunk.meta().clone()));
        }
        // 必須先放掉 DashMap 分片鎖：commit 成功後 WritingChunk::drop 要回頭改同一個 map
        drop(writing_list);
        self.commit_chunks(chunks, true)?;
        Ok(uncommitted_chunks)
    }

    /// 列出 chunk_id 落在 `[begin, end)` 的已提交 chunk，最多 `max_count` 筆，
    /// 依 chunk_id 由大到小。
    /// 直接讀 RocksDB 0x01，不經 meta_cache；`end` 為空表示沒有上界。
    pub fn query_chunks(
        &self,
        begin: impl AsRef<[u8]>,
        end: impl AsRef<[u8]>,
        max_count: u64,
    ) -> Result<Vec<(Bytes, ChunkMeta)>> {
        let mut chunks = self.meta_store.query_chunks(begin, end, max_count)?;
        for (_, meta) in &mut chunks {
            meta.set_default_etag_if_need();
        }
        Ok(chunks)
    }

    // returns an unordered list of chunks (including chunks being written).
    /// 列出 prefix（chain）下所有 chunk；同一 chunk 若有寫入中版本，以寫入中版本為準。
    pub fn query_all_chunks(&self, prefix: &[u8]) -> Result<Vec<(Bytes, ChunkMeta)>> {
        // 在持有該 prefix 的 DashMap 分片鎖時，同時拍下 writing_list 快照並建立 RocksDB iterator。
        // commit 是先寫 RocksDB、後在 WritingChunk::drop 移出 writing_list（需要這把鎖），
        // 所以任何 chunk 至少會出現在兩份快照之一，不會兩邊都漏掉
        let (mut writing_list, it) = match self.writing_list.entry(prefix.into()) {
            dashmap::Entry::Occupied(occupied_entry) => {
                let mut map = HashMap::new();
                for (chunk_id, holder) in occupied_entry.get() {
                    map.insert(chunk_id.clone(), holder.chunk.meta().clone());
                }
                (map, self.meta_store.iterator())
            }
            dashmap::Entry::Vacant(_) => (Default::default(), self.meta_store.iterator()),
        };

        // begin = end = prefix 的技巧：從 ~prefix 開始往後掃，停在第一個 chunk_id < prefix 處，
        // 恰好涵蓋所有以 prefix 開頭的 chunk_id（end 不含，只排除 chunk_id 恰等於 prefix 者）
        let mut chunks =
            self.meta_store
                .query_chunks_from_iterator(it, prefix, prefix, u64::MAX)?;
        // 以寫入中版本覆蓋已提交版本；剩下的是只存在於 writing_list 的新 chunk，補在後面
        for (chunk_id, meta) in &mut chunks {
            meta.set_default_etag_if_need();
            if let Some(writing) = writing_list.remove(chunk_id) {
                *meta = writing;
            }
        }
        chunks.extend(writing_list);
        Ok(chunks)
    }

    /// 依最後更新時間（微秒）查 prefix 下的 chunk：先用 0x06 時間索引找出 chunk_id，
    /// 再逐一 `get` 取目前的已提交 meta（索引查到但已刪除的會被略過）。
    pub fn query_chunks_by_timestamp(
        &self,
        prefix: &[u8],
        begin: u64,
        end: u64,
        max_count: u64,
    ) -> Result<Vec<(Bytes, ChunkMeta)>> {
        let chunk_ids = self
            .meta_store
            .query_chunks_by_timestamp(prefix, begin, end, max_count)?;
        let mut chunks = Vec::with_capacity(chunk_ids.len());
        for chunk_id in chunk_ids {
            if let Some(chunk) = self.get(&chunk_id)? {
                let mut meta = chunk.meta().clone();
                meta.set_default_etag_if_need();
                chunks.push((chunk_id, meta));
            }
        }
        Ok(chunks)
    }

    /// 刪除 chunk_id 落在 `[begin, end)` 的已提交 chunk（最多 `max_count` 個），回傳處理的數量。
    /// 不經兩階段寫入、不檢查版本，直接改中繼資料；每 4096 個一個 WriteBatch（sync）。
    ///
    /// ```text
    ///  ids = query_chunks(begin, end)
    ///  for each batch of 4096:
    ///      sort ids -> lock entries in order
    ///      WriteBatch += remove_mut(id) for chunks still present
    ///      RocksDB write (sync)
    ///      remove cache entries -> positions freed after last reader drops
    /// ```
    pub fn batch_remove(
        &self,
        begin: impl AsRef<[u8]>,
        end: impl AsRef<[u8]>,
        max_count: u64,
    ) -> Result<u64> {
        // 1. 先從 RocksDB 列出要刪的 id（此時不持鎖，之後逐批鎖住再確認是否仍存在）
        let mut chunk_ids = self
            .meta_store
            .query_chunks(begin, end, max_count)?
            .into_iter()
            .map(|(chunk_id, _)| chunk_id)
            .collect::<Vec<Bytes>>();

        // 2. 分批處理，限制單一 WriteBatch 大小與同時持有的 entry 鎖數量
        const BATCH_SIZE: usize = 4096;
        for batch in chunk_ids.chunks_mut(BATCH_SIZE) {
            batch.sort(); // acquire locks in sequence to avoid deadlocks.

            let mut write_batch = RocksDB::new_write_batch();
            let mut entries = Vec::with_capacity(batch.len());
            for chunk_id in batch.iter() {
                let mut entry = self.meta_cache.entry_by_ref(chunk_id);
                match self.get_with_entry(chunk_id, &mut entry)? {
                    Some(chunk) => {
                        self.meta_store
                            .remove_mut(chunk_id, chunk.meta(), &mut write_batch)?;
                    }
                    None => {
                        // The chunk has been deleted by another thread. Still count it as a successfully deleted one.
                    }
                }
                entries.push(entry);
            }

            self.meta_store.write(write_batch, true)?;

            // RocksDB 成功後才清 cache；entry 逐一 drop 時解鎖
            for mut entry in entries {
                entry.remove();
            }
        }

        Ok(chunk_ids.len() as _)
    }

    /// 中繼資料格式升級，`open` 時呼叫。目前只有 v0 -> v1：
    /// 刪除整段 0x06 時間索引（舊格式有誤），不重建；之後 chunk 被寫入時才重新寫入新格式的索引。
    pub fn upgrade_version(&self) -> Result<()> {
        let version = self.meta_store.get_version()?;
        let mut new_version = version;

        if new_version < MetaStore::V1_FIX_TIMESTAMP {
            // fix timestamp format.
            tracing::info!(
                "upgrade version: {} -> {}",
                new_version,
                MetaStore::V1_FIX_TIMESTAMP
            );
            let mut write_batch = RocksDB::new_write_batch();
            self.meta_store
                .remove_range_mut(MetaKey::TIMESTAMP_KEY_PREFIX, &mut write_batch)?;
            self.meta_store.write(write_batch, true)?;
            new_version = MetaStore::V1_FIX_TIMESTAMP;
        }

        if version < new_version {
            self.meta_store.set_version(new_version)?;
            tracing::info!("upgrade version {} -> {} successful!", version, new_version);
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // 每個 clone 被 drop 都會執行；workers 是共用的，第一次就會全部停掉並清空
        self.stop_and_join();
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // 跨多次 open 驗證寫入、讀回、move_chunk、COW 與原地追加的位置變化，以及引用計數何時釋放位置
    #[test]
    fn test_engine_normal() {
        let dir = tempfile::tempdir().unwrap();

        let chunk_id = "chunk01".as_bytes();
        let bytes = "hello world!".as_bytes();

        let mut config: EngineConfig = toml::from_str(&format!(
            r#"
                path = {:?}
                create = true
                prefix_len = 0
            "#,
            dir.path()
        ))
        .unwrap();

        {
            let engine = Engine::open(&config).unwrap();
            let _ = format!("{:?}", engine);
            assert!(engine.get(chunk_id).unwrap().is_none());
            assert_eq!(engine.used_size(), UsedSize::default());
            assert!(engine.move_chunk(chunk_id).is_ok());

            let allocator = engine.allocators.select_by_size(CHUNK_SIZE_NORMAL).unwrap();
            for _ in 0..1000 {
                let chunk = allocator.allocate(true).unwrap();
                assert_eq!(chunk.meta().pos, Position::default());
            }

            let s = CHUNK_SIZE_NORMAL * GroupId::COUNT;
            assert_eq!(engine.used_size().allocated_size, s);
            assert_eq!(engine.used_size().reserved_size, s);

            // SAFETY: This is an exceptional case for testing the function itself.
            // In normal usage, speed_up_quit() should ONLY be called immediately before process exit.
            // This test intentionally violates that contract to verify the function's behavior,
            // but this will cause memory leaks that persist for the remainder of the test process.
            unsafe { engine.speed_up_quit(); }
        }

        {
            let engine = Engine::open(&config).unwrap();
            assert!(engine.get(chunk_id).unwrap().is_none());
            assert_eq!(engine.used_size(), UsedSize::default());
            assert!(engine.write(chunk_id, bytes, 0, 0).is_err());

            let checksum = crc32c::crc32c(bytes);
            let chunk = engine.write(chunk_id, bytes, 0, checksum).unwrap();
            assert_eq!(chunk.meta().pos.chunk_size(), CHUNK_SIZE_SMALL);
            assert_eq!(chunk.meta().pos.group(), 0);
            assert_eq!(chunk.meta().pos.cluster(), 0);
            assert_eq!(chunk.meta().chunk_ver, 1);

            let s = CHUNK_SIZE_SMALL * GroupId::COUNT;
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL);

            let chunk = engine.get(chunk_id).unwrap().unwrap();
            assert_eq!(chunk.meta().chunk_ver, 1);
            assert_eq!(chunk.meta().len, 12);

            let mut buf = [0u8; 12];
            assert!(chunk.pread(&mut buf, 0).is_ok());
            assert_eq!(buf, bytes);
            assert_eq!(chunk.meta().checksum, crc32c::crc32c(&buf));

            let fd_and_offset = chunk.fd_and_offset();
            assert_ne!(fd_and_offset.fd, 0);
            assert_eq!(fd_and_offset.offset, chunk.meta().pos.offset());
        }

        {
            config.create = false;
            let engine = Engine::open(&config).unwrap();
            let chunk0 = engine.get(chunk_id).unwrap().unwrap();

            let s = CHUNK_SIZE_SMALL * GroupId::COUNT;
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL);

            let mut buf = [0u8; 12];
            assert!(chunk0.pread(&mut buf, 0).is_ok());
            assert_eq!(buf, bytes);
            assert_eq!(chunk0.meta().pos.chunk_size(), CHUNK_SIZE_SMALL);
            assert_eq!(chunk0.meta().pos.group(), 0);
            assert_eq!(chunk0.meta().pos.cluster(), 0);
            assert_eq!(chunk0.meta().checksum, crc32c::crc32c(&buf));

            let chunk1 = engine.move_chunk(chunk_id).unwrap().unwrap();
            let mut buf = [0u8; 12];
            assert!(chunk1.pread(&mut buf, 0).is_ok());
            assert_eq!(buf, bytes);
            assert_eq!(chunk1.meta().pos.chunk_size(), CHUNK_SIZE_SMALL);
            assert_eq!(chunk1.meta().pos.index(), 1);
            assert_eq!(chunk1.meta().checksum, crc32c::crc32c(&buf));
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL * 2);

            let allocator = engine.allocators.select_by_size(CHUNK_SIZE_SMALL).unwrap();
            assert_eq!(
                allocator.allocate(true).unwrap().meta().pos,
                Position::new(GroupId::new(CHUNK_SIZE_SMALL, 0, 0), 2)
            );
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL * 2);

            drop(chunk0);
            assert_eq!(
                allocator.allocate(true).unwrap().meta().pos,
                Position::new(GroupId::new(CHUNK_SIZE_SMALL, 0, 0), 0)
            );
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL);

            drop(chunk1); // still in cache.
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL);

            let chunk2 = engine
                .write(chunk_id, bytes, 12, crc32c::crc32c(bytes))
                .unwrap();
            assert_eq!(chunk2.meta().len, 24);
            assert_eq!(chunk2.meta().pos.index(), 1);
            assert_eq!(chunk2.meta().chunk_ver, 2);
            let mut buf = [0u8; 12];
            chunk2.pread(&mut buf, 12).unwrap();
            assert_eq!(buf, bytes);
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL);

            let empty = [0u8; 12];
            let chunk3 = engine
                .write(chunk_id, &empty, 0, crc32c::crc32c(&empty))
                .unwrap();
            assert_eq!(chunk3.meta().len, 24);
            assert_eq!(chunk3.meta().pos.index(), 0);
            assert_eq!(chunk3.meta().chunk_ver, 3);
            let mut buf = [0u8; 12];
            chunk3.pread(&mut buf, 0).unwrap();
            assert_eq!(buf, [0u8; 12]);
            chunk3.pread(&mut buf, 12).unwrap();
            assert_eq!(buf, bytes);
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL * 2);

            engine.remove(chunk_id).unwrap();
            engine.remove(chunk_id).unwrap();
            assert!(engine.get(chunk_id).unwrap().is_none());
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL * 2);

            let mut buf = [0u8; 12];
            chunk3.pread(&mut buf, 0).unwrap();
            assert_eq!(buf, [0u8; 12]);
            chunk3.pread(&mut buf, 12).unwrap();
            assert_eq!(buf, bytes);

            drop(chunk3);
            assert_eq!(engine.used_size().reserved_size, s - CHUNK_SIZE_SMALL);

            drop(chunk2);
            assert_eq!(engine.used_size().reserved_size, s);
        }

        {
            let engine = Engine::open(&config).unwrap();
            let s = CHUNK_SIZE_SMALL * GroupId::COUNT;
            assert_eq!(engine.used_size().reserved_size, s);

            engine.start_allocate_workers(2);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // query_chunks 依 chunk_id 由大到小列出、size class 依長度選擇，以及 batch_remove 全部刪除
    #[test]
    fn test_engine_list_chunks() {
        let dir = tempfile::tempdir().unwrap();

        // 1. prepare.
        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        // 2. write chunks.
        let engine = Engine::open(&config).unwrap();
        const N: usize = 512;
        for i in 1..=N {
            let data = vec![i as u8; i * 1024];
            let checksum = crc32c::crc32c(&data);
            let chunk = engine
                .write(&i.to_be_bytes(), &data, i as u32 * 512, checksum)
                .unwrap();
            assert_eq!(chunk.meta().chunk_ver, 1);
        }

        // 3. list all chunks.
        drop(engine);
        let engine = Engine::open(&config).unwrap();
        let chunks = engine.query_chunks([], [], u64::MAX).unwrap();
        assert_eq!(chunks.len(), N);
        let mut buffer = create_aligned_buf(CHUNK_SIZE_LARGE);
        for (i, (chunk_id, chunk_meta)) in chunks.iter().enumerate() {
            let i = N - i; // reversed order.
            assert_eq!(&i.to_be_bytes(), &chunk_id[..]);
            let offset = i * 512;
            let data_len = i * 1024;
            let total = Size::from(offset + data_len);
            assert_eq!(chunk_meta.len, total);
            assert_eq!(
                chunk_meta.pos.chunk_size(),
                std::cmp::max(total.next_power_of_two(), CHUNK_SIZE_SMALL)
            );

            let chunk = engine.get(chunk_id).unwrap().unwrap();
            assert_eq!(chunk.meta(), chunk_meta);

            chunk.pread(&mut buffer[..total.into()], 0).unwrap();
            assert!(buffer[..offset].iter().all(|v| *v == 0));
            assert!(buffer[offset..total.into()].iter().all(|v| *v == i as u8));
        }

        let key = vec![1u8; 1 << 20];
        engine.write(&key, &[], 0, 0).unwrap();

        let count = engine.batch_remove([], [], u64::MAX).unwrap();
        assert_eq!(count, 1 + N as u64);
        for i in 1..=N {
            assert!(engine.get(&i.to_be_bytes()).unwrap().is_none());
        }
        let count = engine.batch_remove([], [], u64::MAX).unwrap();
        assert_eq!(count, 0);

        let chunks = engine.query_chunks([], [], u64::MAX).unwrap();
        assert!(chunks.is_empty());
    }

    // 重開後資料、checksum、刪除狀態與時間索引都能從 RocksDB 正確還原
    #[test]
    fn test_engine_load() {
        let dir = tempfile::tempdir().unwrap();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();

        // 1. prepare.
        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        // 2. write chunks.
        let engine = Engine::open(&config).unwrap();

        let mut data = create_aligned_buf(CHUNK_SIZE_SMALL);
        for i in 0..512u32 {
            data.fill(i as u8);
            let checksum = crc32c::crc32c(&data);
            let chunk = engine.write(&i.to_le_bytes(), &data, 0, checksum).unwrap();
            assert_eq!(chunk.meta().chunk_ver, 1);
        }

        let vec = engine.query_chunks([], [], 10000).unwrap();
        assert_eq!(vec.len(), 512);

        for i in 0..256u32 {
            engine.remove(&i.to_le_bytes()).unwrap();
        }

        let vec = engine.query_chunks([], [], 10000).unwrap();
        assert_eq!(vec.len(), 256);

        assert_eq!(engine.allocate_groups(0, 0, 8), 1);

        drop(engine);
        let engine = Engine::open(&config).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut buf = create_aligned_buf(CHUNK_SIZE_SMALL);
        for i in 0..512u32 {
            let chunk = engine.get(&i.to_le_bytes()).unwrap();
            if i < 256 {
                assert!(chunk.is_none());
            } else {
                let chunk = chunk.unwrap();
                chunk.pread(&mut buf, 0).unwrap();
                assert!(buf.iter().all(|&v| v == i as u8));
                assert_eq!(
                    chunk.meta().checksum,
                    crc32c::crc32c(&[i as u8; CHUNK_SIZE_SMALL.0 as _])
                );

                data.fill(!i as u8);
                let checksum = crc32c::crc32c(&data);
                let chunk = engine.write(&i.to_le_bytes(), &data, 0, checksum).unwrap();
                assert_eq!(chunk.meta().chunk_ver, 2);
            }
        }

        drop(engine);
        let engine = Engine::open(&config).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        for i in 0..512u32 {
            let chunk = engine.get(&i.to_le_bytes()).unwrap();
            if i < 256 {
                assert!(chunk.is_none());
            } else {
                let chunk = chunk.unwrap();
                chunk.pread(&mut buf, 0).unwrap();
                assert!(buf.iter().all(|&v| v == !i as u8));
                assert_eq!(
                    chunk.meta().checksum,
                    crc32c::crc32c(&[!i as u8; CHUNK_SIZE_SMALL.0 as _])
                );
            }
        }

        let vec = engine.query_chunks([], [], 10000).unwrap();
        assert_eq!(vec.len(), 256);

        let vec = engine
            .query_chunks_by_timestamp(&[], 0, u64::MAX, 10000)
            .unwrap();
        assert_eq!(vec.len(), 256);
    }

    // compact_groups 在背景讀取持續進行時搬移 chunk，讀到的資料始終正確，空出的 group 可回收
    #[test]
    fn test_engine_compact() {
        let dir = tempfile::tempdir().unwrap();

        // 1. prepare.
        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        // 2. write chunks.
        let engine = Engine::open(&config).unwrap();

        let mut data = create_aligned_buf(CHUNK_SIZE_SMALL);
        for i in 0..512u32 {
            data.fill(i as u8);
            let checksum = crc32c::crc32c(&data);
            let chunk = engine.write(&i.to_le_bytes(), &data, 0, checksum).unwrap();
            assert_eq!(chunk.meta().chunk_ver, 1);
        }

        engine.remove(&255u32.to_le_bytes()).unwrap();
        let chunk1 = engine.allocators.allocate(CHUNK_SIZE_SMALL, true);
        engine.remove(&511u32.to_le_bytes()).unwrap();
        let chunk2 = engine.allocators.allocate(CHUNK_SIZE_SMALL, true);

        // 3. remove some chunks.
        for i in 0..512u32 {
            if i % 2 == 0 {
                engine.remove(&i.to_le_bytes()).unwrap();
            }
        }
        assert_eq!(engine.used_size().allocated_size, CHUNK_SIZE_SMALL * 512);
        assert_eq!(engine.used_size().reserved_size, CHUNK_SIZE_SMALL * 256);
        assert_eq!(engine.allocate_groups(0, 0, 16), 0);

        // 4. start a bg read thread.
        let stop = Arc::new(AtomicBool::default());
        let stop_clone = stop.clone();
        let engine_clone = engine.clone();
        let thread = std::thread::spawn(move || {
            let engine = engine_clone;
            let stop = stop_clone;
            while !stop.load(Ordering::Acquire) {
                let mut buf = create_aligned_buf(CHUNK_SIZE_SMALL);
                for i in 0..512u32 {
                    let chunk = engine.get(&i.to_le_bytes()).unwrap();
                    if i % 2 == 0 || i == 255 || i == 511 {
                        assert!(chunk.is_none());
                    } else {
                        let chunk = chunk.unwrap();
                        chunk.pread(&mut buf, 0).unwrap();
                        assert!(buf.iter().all(|&v| v == i as u8));
                        assert_eq!(
                            chunk.meta().checksum,
                            crc32c::crc32c(&[i as u8; CHUNK_SIZE_SMALL.0 as _])
                        );
                    }
                }
            }
        });

        // 5. compact.
        assert_eq!(engine.compact_groups(10000), 0);
        assert_eq!(engine.compact_groups(0), 1);
        assert_eq!(engine.used_size().allocated_size, CHUNK_SIZE_SMALL * 512);
        assert_eq!(engine.used_size().reserved_size, CHUNK_SIZE_SMALL * 256);

        // 6. deallocate previous group.
        drop(chunk1);
        drop(chunk2);
        assert_eq!(engine.compact_groups(0), 1);
        assert_eq!(engine.allocate_groups(0, 0, 16), 1);
        assert_eq!(engine.used_size().allocated_size, CHUNK_SIZE_SMALL * 256);
        assert_eq!(engine.used_size().reserved_size, CHUNK_SIZE_SMALL * 2);
        assert_eq!(engine.compact_groups(1), 1);

        // 7. stop read chunks.
        stop.store(true, Ordering::SeqCst);
        thread.join().unwrap();
    }

    // 加長補 0、截短重算 checksum，以及 update_ver / chain_ver / is_syncing 的版本檢查規則
    #[test]
    fn test_engine_truncate() {
        let dir = tempfile::tempdir().unwrap();

        // 1. prepare.
        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        // 2. write chunks.
        let engine = Engine::open(&config).unwrap();
        let mut data = create_aligned_buf(CHUNK_SIZE_SMALL);
        for i in 0..512u32 {
            data.fill(i as u8);
            let checksum = crc32c::crc32c(&data);
            let chunk = engine.write(&i.to_le_bytes(), &data, 0, checksum).unwrap();
            assert_eq!(chunk.meta().chunk_ver, 1);
        }

        // 3. extend chunks.
        for i in 0..512u32 {
            let length = i * 131;
            let chunk = engine.write(&i.to_le_bytes(), &[], length, 0).unwrap();
            assert_eq!(chunk.meta().chunk_ver, 2);
            assert_eq!(
                chunk.meta().len,
                std::cmp::max(length, CHUNK_SIZE_SMALL.into())
            );
        }

        // 4. truncate and check.
        let mut buf = create_aligned_buf(CHUNK_SIZE_NORMAL);
        for i in 0..512u32 {
            let length = i * 137;
            let chunk = engine.truncate(&i.to_le_bytes(), length).unwrap();
            assert_eq!(chunk.meta().chunk_ver, 3);
            assert_eq!(chunk.meta().len, length);
            chunk
                .pread(
                    &mut buf[..length.next_multiple_of(ALIGN_SIZE.into()) as usize],
                    0,
                )
                .unwrap();

            let bound = std::cmp::min(length, CHUNK_SIZE_SMALL.into()) as usize;
            assert!(buf[..bound].iter().all(|v| *v == i as u8));
            assert!(buf[bound..length as usize].iter().all(|v| *v == 0));
            assert_eq!(
                chunk.meta().checksum,
                crc32c::crc32c(&buf[..length as usize])
            );
        }

        // 4. check version.
        let chunk_id = &511u32.to_le_bytes();
        let mut req = UpdateReq {
            update_ver: 4,
            ..Default::default()
        };
        let chunk = engine.update(chunk_id, &mut req).unwrap();
        assert_eq!(req.out_chain_ver, 0);
        assert_eq!(req.out_commit_ver, 4);
        assert_eq!(req.out_checksum, chunk.meta().checksum);

        engine.update(chunk_id, &mut req).unwrap_err();
        assert_eq!(req.out_chain_ver, 0);
        assert_eq!(req.out_commit_ver, 4);
        assert_eq!(req.out_checksum, chunk.meta().checksum);

        req.update_ver = 6;
        engine.update(chunk_id, &mut req).unwrap_err();
        assert_eq!(req.out_chain_ver, 0);
        assert_eq!(req.out_commit_ver, 4);
        assert_eq!(req.out_checksum, chunk.meta().checksum);

        req.is_syncing = true;
        engine.update(chunk_id, &mut req).unwrap();
        req.is_syncing = false;
        assert_eq!(req.out_chain_ver, 0);
        assert_eq!(req.out_commit_ver, 6);
        assert_eq!(req.out_checksum, 0);
        assert_eq!(req.length, 0);

        req.chain_ver = 1;
        req.update_ver = 7;
        let chunk = engine.update(chunk_id, &mut req).unwrap();
        assert_eq!(req.out_chain_ver, 1);
        assert_eq!(req.out_commit_ver, 7);
        assert_eq!(req.out_checksum, chunk.meta().checksum);

        req.chain_ver = 0;
        req.update_ver = 8;
        engine.update(chunk_id, &mut req).unwrap_err();
        assert_eq!(req.out_chain_ver, 1);
        assert_eq!(req.out_commit_ver, 7);
        assert_eq!(req.out_checksum, chunk.meta().checksum);

        assert_eq!(
            format!("{:?}", chunk.as_ref()),
            format!("{:?}", chunk.meta())
        );
    }

    // 追加後 checksum 正確合併，without_checksum、expected_tag 與 create_new 的行為
    #[test]
    fn test_engine_checksum() {
        let dir = tempfile::tempdir().unwrap();

        // 1. prepare.
        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        // 2. write chunks.
        let engine = Engine::open(&config).unwrap();

        let data = b"etc";
        let checksum = crc32c::crc32c(data);
        engine.write(b"chunk", data, 0, checksum).unwrap();

        let data = b"zzz";
        let checksum = crc32c::crc32c(data);
        let chunk = engine.write(b"chunk", data, 3, checksum).unwrap();

        let mut buf = [0u8; 6];
        chunk.pread(&mut buf, 0).unwrap();
        assert_eq!(crc32c::crc32c(&buf), chunk.meta().checksum);

        // 3. write without checksum
        engine
            .update(
                b"chunk",
                &mut UpdateReq {
                    without_checksum: true,
                    length: data.len() as _,
                    offset: 6,
                    data: data.as_ptr() as u64,
                    expected_tag: unsafe {
                        std::mem::transmute::<&[u8], &[u8]>(chunk.meta().etag.as_slice())
                    },
                    desired_tag: b"desired_tag",
                    ..Default::default()
                },
            )
            .unwrap();

        let err = engine
            .update(
                b"chunk",
                &mut UpdateReq {
                    without_checksum: true,
                    length: data.len() as _,
                    offset: 6,
                    data: data.as_ptr() as u64,
                    expected_tag: b"mismatch",
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::ChunkETagMismatch(_)));

        let err = engine
            .update(
                b"chunk",
                &mut UpdateReq {
                    without_checksum: true,
                    length: data.len() as _,
                    offset: 6,
                    data: data.as_ptr() as u64,
                    create_new: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::ChunkAlreadyExists));
    }

    // 禁止擴張後不再預配置 group，預備池空時寫入回 NoSpace
    #[test]
    fn test_set_allow_to_allocate() {
        let dir = tempfile::tempdir().unwrap();

        // 1. prepare.
        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        // 2. write chunks.
        let engine = Engine::open(&config).unwrap();
        assert_eq!(engine.allocate_ultra_groups(0, 1, 1), 0);

        engine.set_allow_to_allocate(false);
        assert_eq!(engine.allocate_groups(1, 1, 1), 0);
        assert_eq!(engine.allocate_ultra_groups(0, 1, 1), 0);

        let data = b"etc";
        let checksum = crc32c::crc32c(data);
        let err = engine.write(b"chunk", data, 0, checksum).unwrap_err();
        assert!(matches!(err, Error::NoSpace));
    }

    // 重開後以 syncing 刪除既有 chunk，out_* 回報的是刪除前的狀態
    #[test]
    fn test_remove_update() {
        let dir = tempfile::tempdir().unwrap();

        // 1. prepare.
        let data = b"etc";
        let checksum = crc32c::crc32c(data);
        {
            let config = EngineConfig {
                path: dir.path().into(),
                create: true,
                ..Default::default()
            };
            let engine = Engine::open(&config).unwrap();

            engine.write(b"chunk", data, 0, checksum).unwrap();
        }

        // 2. remove.
        {
            let config = EngineConfig {
                path: dir.path().into(),
                create: false,
                ..Default::default()
            };
            let engine = Engine::open(&config).unwrap();
            let mut req = UpdateReq {
                is_remove: true,
                is_syncing: true,
                ..Default::default()
            };
            engine.update(b"chunk", &mut req).unwrap();
            assert!(!req.out_non_existent);
            assert_eq!(req.out_checksum, checksum);
        }
    }

    // 寫入中版本只在 query_all_chunks 可見、同一 chunk 不能並行更新、commit_chunks 批次提交
    #[test]
    fn test_engine_writing_list() {
        let dir = tempfile::tempdir().unwrap();

        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };
        let engine = Engine::open(&config).unwrap();

        let chunks = engine.query_all_chunks(&[]).unwrap();
        assert!(chunks.is_empty());

        let chunk_id = "chunk01".as_bytes();
        let bytes = "hello world!".as_bytes();
        let mut req = UpdateReq {
            length: bytes.len() as _,
            data: bytes.as_ptr() as _,
            without_checksum: true,
            ..Default::default()
        };
        let mut chunk = engine.update_chunk(chunk_id, &mut req).unwrap();
        assert!(!engine
            .writing_list
            .entry(Default::default())
            .or_default()
            .is_empty());

        let err = engine.update_chunk(chunk_id, &mut req).unwrap_err();
        assert!(matches!(err, Error::InvalidArg(_)));

        let chunks = engine.query_chunks([], [], u64::MAX).unwrap();
        assert!(chunks.is_empty());

        let chunks = engine.query_all_chunks(&[]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0].1, chunk.meta());

        chunk.chunk.set_chain_ver(3);
        let mut meta = chunk.meta().clone();
        engine.commit_chunks(vec![chunk], false).unwrap();
        assert!(engine
            .writing_list
            .entry(Default::default())
            .or_default()
            .is_empty());

        let chunks = engine.query_chunks([], [], u64::MAX).unwrap();
        assert_eq!(chunks.len(), 1);
        meta.uncommitted = false;
        assert_eq!(chunks[0].1, meta);

        let mut writing_chunks = vec![];
        let remove = engine
            .update_chunk(
                b"remove",
                &mut UpdateReq {
                    is_remove: true,
                    ..Default::default()
                },
            )
            .unwrap();
        writing_chunks.push(remove);

        let new_chunk = engine
            .update_chunk(
                b"new_chunk",
                &mut UpdateReq {
                    length: bytes.len() as _,
                    data: bytes.as_ptr() as _,
                    without_checksum: true,
                    ..Default::default()
                },
            )
            .unwrap();
        writing_chunks.push(new_chunk);

        let update = engine
            .update_chunk(
                chunk_id,
                &mut UpdateReq {
                    offset: bytes.len() as _,
                    length: bytes.len() as _,
                    data: bytes.as_ptr() as _,
                    without_checksum: true,
                    chain_ver: 3,
                    ..Default::default()
                },
            )
            .unwrap();
        writing_chunks.push(update);
        assert_eq!(
            engine
                .writing_list
                .entry(Default::default())
                .or_default()
                .len(),
            3
        );

        assert_eq!(engine.query_chunks([], [], u64::MAX).unwrap().len(), 1);
        let chunks = engine.query_all_chunks(&[]).unwrap();
        assert_eq!(chunks.len(), 3);
        for (id, meta) in chunks {
            if id == chunk_id {
                assert_eq!(meta.len as usize, bytes.len() * 2);
            } else if id.as_slice() == b"new_chunk" {
                assert_eq!(meta.len as usize, bytes.len());
            } else {
                assert_eq!(id.as_slice(), b"remove");
            }
        }

        engine.commit_chunks(writing_chunks, false).unwrap();
        assert!(engine
            .writing_list
            .entry(Default::default())
            .or_default()
            .is_empty());
    }

    // 未 commit 的新 chunk 在重開後復原回 writing_list，並由 handle_uncommitted_chunks 提交
    #[test]
    fn test_engine_persist_writing_list() {
        let dir = tempfile::tempdir().unwrap();

        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };
        let engine = Engine::open(&config).unwrap();

        let chunk_id = "chunk01".as_bytes();
        let bytes = "hello world!".as_bytes();
        let mut req = UpdateReq {
            length: bytes.len() as _,
            data: bytes.as_ptr() as _,
            without_checksum: true,
            ..Default::default()
        };
        let chunk = engine.update_chunk(chunk_id, &mut req).unwrap();
        assert!(!engine
            .writing_list
            .entry(Default::default())
            .or_default()
            .is_empty());

        let mut chunks = engine.query_all_chunks(&[]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0].1, chunk.meta());
        assert!(chunk.meta().uncommitted);

        let uncommitted_chunks = engine.query_uncommitted_chunks(&[]).unwrap();
        assert_eq!(uncommitted_chunks.len(), 1);
        assert_eq!(chunks, uncommitted_chunks);

        drop(chunk);
        drop(engine);

        let engine = Engine::open(&config).unwrap();
        let uncommitted_chunks = engine.query_uncommitted_chunks(&[]).unwrap();
        assert_eq!(uncommitted_chunks.len(), 1);
        assert_eq!(chunks, uncommitted_chunks);

        assert!(engine.get(chunk_id).unwrap().is_none());
        let chain_ver = 233;
        let uncommitted_chunks = engine.handle_uncommitted_chunks(&[], chain_ver).unwrap();
        assert_eq!(uncommitted_chunks.len(), 1);
        for chunk in &mut chunks {
            chunk.1.chain_ver = chain_ver;
        }
        assert_eq!(chunks, uncommitted_chunks);

        let chunk = engine.get(chunk_id).unwrap().unwrap();
        assert!(!chunk.meta().uncommitted);
        assert_eq!(chunk.meta().chain_ver, chain_ver);
    }

    // 對已存在 chunk 的原地追加多次放棄後重開，只復原最後一筆，提交後長度正確
    #[test]
    fn test_engine_persist_writing_list_2() {
        let dir = tempfile::tempdir().unwrap();

        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };
        let engine = Engine::open(&config).unwrap();

        let chunk_id = "chunk01".as_bytes();
        let bytes = "hello world!".as_bytes();
        let mut req = UpdateReq {
            length: bytes.len() as _,
            data: bytes.as_ptr() as _,
            without_checksum: true,
            ..Default::default()
        };
        let _ = engine.update(chunk_id, &mut req).unwrap();
        assert!(engine
            .writing_list
            .entry(Default::default())
            .or_default()
            .is_empty());

        let mut req = UpdateReq {
            length: bytes.len() as _,
            data: bytes.as_ptr() as _,
            offset: bytes.len() as _,
            without_checksum: true,
            ..Default::default()
        };
        let chunk = engine.update_chunk(chunk_id, &mut req).unwrap();
        assert!(!engine
            .writing_list
            .entry(Default::default())
            .or_default()
            .is_empty());

        let chunks = engine.query_all_chunks(&[]).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(&chunks[0].1, chunk.meta());
        assert!(chunk.meta().uncommitted);

        let uncommitted_chunks = engine.query_uncommitted_chunks(&[]).unwrap();
        assert_eq!(uncommitted_chunks.len(), 1);
        assert_eq!(chunks, uncommitted_chunks);

        drop(chunk);

        let mut req = UpdateReq {
            length: bytes.len() as _,
            data: bytes.as_ptr() as _,
            offset: bytes.len() as _,
            without_checksum: true,
            ..Default::default()
        };
        let chunk = engine.update_chunk(chunk_id, &mut req).unwrap();
        drop(chunk);

        let mut req = UpdateReq {
            length: bytes.len() as _,
            data: bytes.as_ptr() as _,
            offset: bytes.len() as _,
            without_checksum: true,
            ..Default::default()
        };
        let chunk = engine.update_chunk(chunk_id, &mut req).unwrap();
        drop(chunk);

        drop(engine);

        let engine = Engine::open(&config).unwrap();
        let chunks = engine.query_uncommitted_chunks(&[]).unwrap();
        assert_eq!(chunks.len(), 1);

        engine.handle_uncommitted_chunks(&[], 2).unwrap();
        let chunk = engine.get(chunk_id).unwrap().unwrap();
        assert_eq!(chunk.meta().len as usize, bytes.len() * 2);
    }

    // 一條執行緒反覆寫入／刪除、另一條反覆 get，不得 panic 或死鎖
    #[test]
    fn test_engine_concurrent_update_and_get() {
        let dir = tempfile::tempdir().unwrap();

        let config = EngineConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };
        let engine = Engine::open(&config).unwrap();

        let chunk_id = "chunk01".as_bytes();
        let bytes = "hello world!".as_bytes();
        let checksum = crc32c::crc32c(bytes);

        let start = std::time::Instant::now();
        let duration = std::time::Duration::from_secs(2);
        let engine_clone = engine.clone();
        let commit_thread = std::thread::spawn(move || {
            while std::time::Instant::now().duration_since(start) < duration {
                engine.write(chunk_id, bytes, 0, checksum).unwrap();
                engine.remove(chunk_id).unwrap();
            }
        });

        let get_thread = std::thread::spawn(move || {
            while std::time::Instant::now().duration_since(start) < duration {
                let _ = engine_clone.get(chunk_id).unwrap();
            }
        });

        let _ = get_thread.join().unwrap();
        let _ = commit_thread.join().unwrap();
    }
}
