//! C++ 與 Rust 的 FFI 邊界 (cxx bridge，namespace `hf3fs::chunk_engine`)。
//!
//! 這個檔案沒有業務邏輯，只把 `Engine` 的 Rust API 包成 C++ 可以呼叫的形式：
//! 1. `Result` 轉成「CxxString out-param + 回傳 null / 0 / 空集合」。
//! 2. `Arc` / `Box` 轉成 raw pointer 交給 C++ 持有，再由 C++ 呼叫對應函式歸還。
//! 3. `ChunkMeta` / `UsedSize` / `Metrics` 轉成 C++ 看得懂的 shared struct (`RawMeta` 等)。
//!
//! 呼叫關係 (C++ 端在 `src/storage/`)：
//!
//! ```text
//!   C++ caller                         ffi fn                          Rust
//!   ---------------------------------  ------------------------------  -----------------------
//!   StorageTargets::init               create                          Engine::open
//!   ChunkEngine::update (UpdateJob)    update_raw_chunk                Engine::update_chunk
//!   ChunkEngine::commit                commit_raw_chunk                Engine::commit_chunk
//!   ~ChunkEngineUpdateJob (abort)      release_writing_chunk           drop WritingChunk
//!   ChunkEngine::aioPrepareRead        get_raw_chunk                   Engine::get
//!     / queryChunk
//!   AIO read                           Chunk::fd_and_offset            Clusters::fd_and_offset
//!   read done                          release_raw_chunk               drop Arc<Chunk>
//!   ChunkEngine::queryUncommitted...   query_uncommitted_raw_chunks    Engine::query_uncommitted
//!   ChunkEngine::resetUncommitted...   handle_uncommitted_raw_chunks   Engine::handle_uncommitted
//!   ChunkEngine::queryChunks           query_raw_chunks                Engine::query_chunks
//!   ChunkEngine::getAllMetadata*       query_all_raw_chunks            Engine::query_all_chunks
//!   ChunkEngine::removeAllChunks       raw_batch_remove                Engine::batch_remove
//!   ChunkEngine::chainUsedSize         query_raw_used_size             MetaStore::query_used_size
//!   AllocateWorker                     allocate_groups                 Engine::allocate_groups
//!                                      allocate_ultra_groups / compact Engine::allocate_ultra_...
//!   CheckWorker                        raw_used_size / get_metrics     Engine::used_size / metrics
//!                                      set_allow_to_allocate           Engine::set_allow_to_...
//! ```
//!
//! raw pointer 的所有權 (誰產生、誰釋放)：
//!
//! ```text
//!   pointer              produced by            released by
//!   -------------------  ---------------------  ---------------------------------------------
//!   *mut Engine          create (Box::into_raw) C++ rust::Box<Engine>::from_raw -> Box drop
//!   *const Chunk         get_raw_chunk(s)       release_raw_chunk (Arc::from_raw -> rc - 1)
//!                        (Arc::into_raw)
//!   *mut WritingChunk    update_raw_chunk       commit_raw_chunk(s)  (consumed, even on error)
//!                        (Box::into_raw)        or release_writing_chunk (abort path)
//!   *const Chunk         WritingChunk::         never released: borrowed view into the
//!                          raw_chunk            WritingChunk, valid only while it lives
//!   *mut LogGuard        init_log               not released through FFI
//!   Box<RawChunks>       query_* / handle_*     C++ rust::Box destructor
//! ```
//!
//! 錯誤慣例：每個可失敗的函式都收一個 `Pin<&mut CxxString>`，失敗時 `push_str` 錯誤訊息，
//! C++ 以「字串非空」判斷失敗。`update_raw_chunk` 另外把錯誤種類轉成 3FS `StorageCode`
//! 填進 `UpdateReq.out_error_code`。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::*;
pub use ::cxx::CxxString;

/// 開啟 (或建立) 一個引擎。成功回傳 `Box::into_raw` 的指標，C++ 端
/// (`src/storage/store/StorageTargets.cc`) 立刻用 `rust::Box<Engine>::from_raw` 接手所有權；
/// 失敗回傳 null 並把錯誤寫進 `error`。
fn create(path: &str, create: bool, prefix_len: usize, error: Pin<&mut CxxString>) -> *mut Engine {
    let config = EngineConfig {
        path: PathBuf::from(path),
        create,
        prefix_len,
    };
    match Engine::open(&config) {
        Ok(engine) => Box::into_raw(Box::new(engine)),
        Err(e) => {
            error.push_str(&e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// 接收 `Box<Engine>` 後什麼都不做，離開函式時 drop (觸發 `Engine::drop` -> `stop_and_join`)。
/// 目前 C++ 端不呼叫它，而是靠 `rust::Box` 解構子釋放。
fn release(_engine: Box<Engine>) {}

/// 持有 `tracing_appender` 背景寫檔執行緒的 guard；guard 被 drop 時會把緩衝中的 log 刷出。
/// 欄位從不讀取，只是為了延長 guard 的生命週期，所以標 `dead_code`。
#[allow(dead_code)]
struct LogGuard(tracing_appender::non_blocking::WorkerGuard);

/// 把 crate 內的 `tracing` log 導向 `path` 的滾動檔案 (單檔 500MiB、保留 20 個、INFO 以上)。
///
/// 回傳的 `LogGuard` 要一直持有到程式結束，否則非同步寫檔執行緒會停止。
/// `tracing_subscriber::fmt().init()` 設定的是全域 subscriber，同一個行程重複呼叫會 panic。
/// 本 repo 的 C++ 端目前沒有呼叫這個函式。
fn init_log(path: &str, error: Pin<&mut CxxString>) -> *mut LogGuard {
    // 1. 建立滾動檔案 appender
    // 2. 包成 non-blocking writer (背景執行緒寫檔)，安裝為全域 subscriber
    // 3. 把 guard 交給 C++ 持有
    match rolling_file::BasicRollingFileAppender::new(
        path,
        rolling_file::RollingConditionBasic::new().max_size(Size::mebibyte(500).into()),
        20,
    ) {
        Ok(file_appender) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_writer(non_blocking)
                .with_ansi(false)
                .init();
            Box::into_raw(Box::new(LogGuard(guard)))
        }
        Err(e) => {
            error.push_str(&e.to_string());
            std::ptr::null_mut()
        }
    }
}

// 以下三組 impl 是 ffi 裡 `extern "Rust"` 宣告的方法實作 (Chunk / WritingChunk / Engine)。

impl Chunk {
    /// 把 `&ChunkMeta` 重新解讀成 `&RawMeta` (零拷貝)。
    ///
    /// ```text
    ///   ChunkMeta (#[repr(C)])               RawMeta (cxx shared struct, repr(C))
    ///   +------------------------------+     +------------------------------+
    ///   | pos              u64         | ==  | pos              u64         |
    ///   | chain_ver/chunk_ver/len/csum | ==  | chain_ver/chunk_ver/len/csum |
    ///   | timestamp        u64         | ==  | timestamp        u64         |
    ///   | last_request_id  u64         | ==  | last_request_id  u64         |
    ///   | last_client_low/high u64     | ==  | last_client_low/high u64     |
    ///   +------------------------------+     +------------------------------+
    ///   | etag (TinyVec), uncommitted  |      (not visible to C++ via RawMeta;
    ///   +------------------------------+       exposed by raw_etag / uncommitted)
    /// ```
    ///
    /// `RawMeta` 是 `ChunkMeta` 前段欄位的「前綴視圖」，兩者欄位順序與型別逐一相同。
    /// 檔尾只用 `const_assert_eq!` 檢查兩者 align 相同 (size 本來就不同)，欄位順序的一致性
    /// 沒有編譯期檢查，修改 `types/chunk_meta.rs` 時要手動同步。
    fn raw_meta(&self) -> &ffi::RawMeta {
        unsafe { std::mem::transmute(self.meta()) }
    }

    fn raw_etag(&self) -> &[u8] {
        &self.meta().etag
    }

    fn uncommitted(&self) -> bool {
        self.meta().uncommitted
    }
}

impl WritingChunk {
    fn raw_meta(&self) -> &ffi::RawMeta {
        self.chunk.raw_meta()
    }

    fn raw_etag(&self) -> &[u8] {
        self.chunk.raw_etag()
    }

    fn uncommitted(&self) -> bool {
        self.chunk.uncommitted()
    }

    /// 回傳內部 `Chunk` 的借用指標，**沒有增加引用計數**。C++ 只能在 WritingChunk 還活著
    /// 時使用，且不可對它呼叫 `release_raw_chunk`。
    fn raw_chunk(&self) -> *const Chunk {
        &self.chunk
    }

    /// C++ `ChunkEngine::commit` 在 commit 前呼叫，用 `job.commitChainVer()` 覆寫
    /// update 階段記下的 chain_ver，再讀 `raw_meta()` 回報結果。
    fn set_chain_ver(&mut self, chain_ver: u32) {
        self.chunk.set_chain_ver(chain_ver);
    }
}

impl Engine {
    /// `UsedSize` 與 `RawUsedSize` 都是 `repr(C)` 的 4 個 u64 (`Size` 也是 u64 newtype)，
    /// 檔尾有 size / align 的編譯期檢查，因此可以按值 transmute。
    fn raw_used_size(&self) -> ffi::RawUsedSize {
        unsafe { std::mem::transmute(self.used_size()) }
    }

    /// 查單一 chunk。回傳值三種情況：
    /// - 找到：`Arc::into_raw` 交出一份強引用，C++ 用完必須 `release_raw_chunk` 歸還，
    ///   否則該位置的引用計數永遠不歸零、空間不會回收。
    /// - 不存在：null，且 `error` 被清空 (C++ 以此區分「不存在」與「出錯」)。
    /// - 出錯：null，`error` 有訊息。
    fn get_raw_chunk(&self, chunk_id: &[u8], error: Pin<&mut CxxString>) -> *const Chunk {
        match self.get(chunk_id) {
            Ok(None) => {
                error.clear();
                std::ptr::null()
            }
            Ok(Some(c)) => {
                error.clear();
                Arc::into_raw(c)
            }
            Err(e) => {
                error.push_str(&e.to_string());
                std::ptr::null()
            }
        }
    }

    /// 批次版 `get_raw_chunk`：結果寫回每個 `GetReq.chunk_ptr` (找到為 Arc raw ptr，否則 null)，
    /// 每個非 null 指標都要各自 `release_raw_chunk`。整批出錯時 `chunk_ptr` 維持原值不動。
    /// 本 repo 的 C++ 端目前沒有呼叫。
    fn get_raw_chunks(&self, reqs: &mut [GetReq], error: Pin<&mut CxxString>) {
        // 1. 收集並去重 chunk_id (BTreeSet 排序，batch_get 依序對各 key 加鎖)
        // 2. 批次查詢後，依原本 reqs 的順序把結果填回去
        let chunk_ids = reqs
            .iter()
            .map(|r| Bytes::from(r.chunk_id))
            .collect::<BTreeSet<_>>();
        match self.batch_get(&chunk_ids) {
            Ok(chunks) => {
                for req in reqs {
                    match chunks.get(req.chunk_id) {
                        Some(c) => req.chunk_ptr = Arc::into_raw(c.clone()),
                        None => req.chunk_ptr = std::ptr::null_mut(),
                    }
                }
                error.clear();
            }
            Err(e) => {
                error.push_str(&e.to_string());
            }
        }
    }

    /// 歸還 `get_raw_chunk(s)` 交出的強引用：`Arc::from_raw` 取回後立即 drop，rc 減一；
    /// 若這是最後一個引用，`Chunk::drop` 會把位置還給 allocator。
    ///
    /// # Safety
    /// `chunk` 必須是 null，或是 `get_raw_chunk(s)` 回傳且尚未歸還過的指標；重複歸還會造成
    /// double free。不可傳入 `WritingChunk::raw_chunk` 的借用指標。
    unsafe fn release_raw_chunk(&self, chunk: *const Chunk) {
        if !chunk.is_null() {
            Arc::from_raw(chunk);
        }
    }

    /// 放棄一個尚未 commit 的寫入 (C++ `ChunkEngineUpdateJob` 解構時呼叫)。
    /// drop `WritingChunk` 時因 `commit_succ == false`，會把 `writing_list` 中對應項目標成
    /// `abort = true`，之後同一 chunk 的新 update 可以覆蓋它 (見 `alloc/writing_chunk.rs`)。
    ///
    /// # Safety
    /// `chunk` 必須是 null，或是 `update_raw_chunk` 回傳、尚未被 commit 或 release 的指標。
    unsafe fn release_writing_chunk(&self, chunk: *mut WritingChunk) {
        if !chunk.is_null() {
            let _ = Box::from_raw(chunk);
        }
    }

    /// 兩階段寫入的第一階段：寫資料、留 writing 紀錄，回傳 `Box::into_raw` 的 WritingChunk。
    ///
    /// ```text
    ///   C++ UpdateReq (in fields) --> Engine::update_chunk --> Ok(WritingChunk)
    ///                                        |                   |
    ///             req.out_* filled  <--------+                   v  Box::into_raw
    ///             (also on error:                         *mut WritingChunk
    ///              current versions)                      -> commit_raw_chunk
    ///                                                     or release_writing_chunk
    /// ```
    ///
    /// 失敗時回傳 null、寫 `error`，並依錯誤種類填 `req.out_error_code` (3FS `StorageCode`)。
    /// 注意 `update_chunk` 在版本檢查前就先填好 `out_commit_ver` 等欄位，所以即使失敗，
    /// C++ 也拿得到 chunk 目前的已提交版本。
    fn update_raw_chunk(
        &self,
        chunk_id: &[u8],
        mut req: Pin<&mut ffi::UpdateReq>,
        error: Pin<&mut CxxString>,
    ) -> *mut WritingChunk {
        match self.update_chunk(chunk_id, &mut req) {
            // 所有權交給 C++ (UpdateJob 內的 ChunkEngineUpdateJob 持有)
            Ok(chunk) => Box::into_raw(Box::new(chunk)),
            Err(e) => {
                error.push_str(&e.to_string());
                // 數值必須與 C++ 的 StorageCode / StatusCode 定義一致，右側註解是對應的名稱
                req.out_error_code = match e {
                    Error::IoError(_) => 4011,              // ChunkWriteFailed
                    Error::RocksDBError(_) => 4003,         // ChunkMetadataSetError
                    Error::MetaError(_) => 4002,            // ChunkMetadataGetError
                    Error::InvalidArg(_) => 3,              // InvalidArg
                    Error::SerializationError(_) => 4002,   // ChunkMetadataGetError
                    Error::ChecksumMismatch(_) => 4080,     // ChecksumMismatch
                    Error::ChainVersionMismatch(_) => 4081, // ChainVersionMismatch
                    Error::ChunkETagMismatch(_) => 4083,    // ChunkETagMismatch
                    Error::ChunkAlreadyExists => 4084,      // ChunkAlreadyExists
                    Error::ChunkCommittedUpdate(_) => 4008, // ChunkCommittedUpdate
                    Error::ChunkMissingUpdate(_) => 4007,   // ChunkMissingUpdate
                    Error::NoSpace => 7021,                 // NoSpace
                };
                std::ptr::null_mut()
            }
        }
    }

    /// 兩階段寫入的第二階段：以一個 RocksDB WriteBatch 切換 meta，再更新 meta_cache。
    /// `sync` 決定這次 WriteBatch 是否 fsync WAL。
    ///
    /// **無論成功或失敗，指標都會被消耗** (`Box::from_raw` 取回所有權)；C++ 呼叫後立刻
    /// `chunkEngineJob().release()` 放掉指標，避免解構時再 `release_writing_chunk`。
    /// 失敗時 WritingChunk 在 `commit_chunk` 內被 drop，writing_list 對應項目標成 abort。
    ///
    /// # Safety
    /// `new_chunk` 必須是 `update_raw_chunk` 回傳、尚未 commit / release 的非 null 指標。
    unsafe fn commit_raw_chunk(
        &self,
        new_chunk: *mut WritingChunk,
        sync: bool,
        error: Pin<&mut CxxString>,
    ) {
        let new_chunk = Box::from_raw(new_chunk);
        match self.commit_chunk(*new_chunk, sync) {
            Ok(_) => (),
            Err(e) => error.push_str(&e.to_string()),
        }
    }

    /// 批次 commit：多個 WritingChunk 合進同一個 WriteBatch。所有指標同樣一律被消耗。
    /// 本 repo 的 C++ 端目前沒有呼叫。
    ///
    /// # Safety
    /// `reqs` 內每個指標都必須是 `update_raw_chunk` 回傳、尚未 commit / release 的非 null 指標，
    /// 且彼此不重複。
    unsafe fn commit_raw_chunks(
        &self,
        reqs: &[*mut WritingChunk],
        sync: bool,
        error: Pin<&mut CxxString>,
    ) {
        let chunks = reqs.iter().map(|c| *Box::from_raw(*c)).collect::<Vec<_>>();
        match self.commit_chunks(chunks, sync) {
            Ok(_) => (),
            Err(e) => error.push_str(&e.to_string()),
        }
    }

    // 以下 query_* 都回傳 Box<RawChunks> (所有權交給 C++ 的 rust::Box)；失敗時回傳空集合並寫
    // error，C++ 必須先看 error 才能分辨「失敗」與「查無資料」。

    /// 依 chunk_id 範圍 [begin, end) 查 meta，最多 `max_count` 筆
    /// (C++ `ChunkEngine::queryChunks`)。
    fn query_raw_chunks(
        &self,
        begin: &[u8],
        end: &[u8],
        max_count: u64,
        error: Pin<&mut CxxString>,
    ) -> Box<RawChunks> {
        match self.query_chunks(begin, end, max_count) {
            Ok(vec) => Box::new(RawChunks { vec }),
            Err(e) => {
                error.push_str(&e.to_string());
                Default::default()
            }
        }
    }

    /// 查某個 prefix (chain) 下的全部 chunk (C++ `ChunkEngine::getAllMetadata*`)。
    fn query_all_raw_chunks(&self, prefix: &[u8], error: Pin<&mut CxxString>) -> Box<RawChunks> {
        match self.query_all_chunks(prefix) {
            Ok(vec) => Box::new(RawChunks { vec }),
            Err(e) => {
                error.push_str(&e.to_string());
                Default::default()
            }
        }
    }

    /// 依時間戳範圍查某個 prefix 下的 chunk (走 RocksDB 前綴 0x06 索引)。
    /// 本 repo 的 C++ 端目前沒有呼叫。
    fn query_raw_chunks_by_timestamp(
        &self,
        prefix: &[u8],
        begin: u64,
        end: u64,
        max_count: u64,
        error: Pin<&mut CxxString>,
    ) -> Box<RawChunks> {
        match self.query_chunks_by_timestamp(prefix, begin, end, max_count) {
            Ok(vec) => Box::new(RawChunks { vec }),
            Err(e) => {
                error.push_str(&e.to_string());
                Default::default()
            }
        }
    }

    /// 刪除範圍內最多 `max_count` 個 chunk，回傳實際刪除數；失敗回 0 並寫 error。
    /// C++ `ChunkEngine::removeAllChunks` 以 begin == end == ChainId 呼叫 (刪整條 chain)。
    fn raw_batch_remove(
        &self,
        begin: &[u8],
        end: &[u8],
        max_count: u64,
        error: Pin<&mut CxxString>,
    ) -> u64 {
        match self.batch_remove(begin, end, max_count) {
            Ok(cnt) => cnt,
            Err(e) => {
                error.push_str(&e.to_string());
                0
            }
        }
    }

    /// 某個 prefix (chain) 已用的容量 (bytes)，來自 RocksDB 前綴 0x04 的 merge 累計值。
    fn query_raw_used_size(&self, prefix: &[u8], error: Pin<&mut CxxString>) -> u64 {
        match self.meta_store.query_used_size(prefix) {
            Ok(size) => size,
            Err(e) => {
                error.push_str(&e.to_string());
                0
            }
        }
    }

    /// 讀取並歸零寫入路徑的計數器 (C++ `CheckWorker` 定期呼叫)。
    ///
    /// ```text
    ///   Metrics (AtomicU64, cumulative)  --swap(0)-->  ffi::Metrics (delta since last call)
    ///     *_times / *_bytes              -> copied as-is
    ///     *_latency (sum of micros)      -> divided by *_times => average micros
    /// ```
    ///
    /// 每個欄位各自 `swap`，不是原子快照；並發寫入時 times 與 latency 可能差一兩筆。
    fn get_metrics(&self) -> ffi::Metrics {
        let metrics = self.metrics.as_ref();
        let copy_on_write_times = metrics.copy_on_write_times.swap(0, Ordering::AcqRel);
        let copy_on_write_latency = metrics.copy_on_write_latency.swap(0, Ordering::AcqRel);
        let copy_on_write_read_times = metrics.copy_on_write_read_times.swap(0, Ordering::AcqRel);
        let copy_on_write_read_latency =
            metrics.copy_on_write_read_latency.swap(0, Ordering::AcqRel);
        let allocate_total_latency = metrics.allocate_latency.swap(0, Ordering::AcqRel);
        let allocate_total_times = metrics.allocate_times.swap(0, Ordering::AcqRel);
        let pwrite_total_latency = metrics.pwrite_latency.swap(0, Ordering::AcqRel);
        let pwrite_total_times = metrics.pwrite_times.swap(0, Ordering::AcqRel);
        // 平均延遲 = 總和 / 次數；用 max(1, times) 避免除以 0
        ffi::Metrics {
            copy_on_write_times,
            copy_on_write_latency: copy_on_write_latency / std::cmp::max(1, copy_on_write_times),
            copy_on_write_read_bytes: metrics.copy_on_write_read_bytes.swap(0, Ordering::AcqRel),
            copy_on_write_read_times,
            copy_on_write_read_latency: copy_on_write_read_latency
                / std::cmp::max(1, copy_on_write_read_times),
            checksum_reuse: metrics.checksum_reuse.swap(0, Ordering::AcqRel),
            checksum_combine: metrics.checksum_combine.swap(0, Ordering::AcqRel),
            checksum_recalculate: metrics.checksum_recalculate.swap(0, Ordering::AcqRel),
            safe_write_direct_append: metrics.safe_write_direct_append.swap(0, Ordering::AcqRel),
            safe_write_indirect_append: metrics
                .safe_write_indirect_append
                .swap(0, Ordering::AcqRel),
            safe_write_truncate_shorten: metrics
                .safe_write_truncate_shorten
                .swap(0, Ordering::AcqRel),
            safe_write_truncate_extend: metrics
                .safe_write_truncate_extend
                .swap(0, Ordering::AcqRel),
            safe_write_read_tail_times: metrics
                .safe_write_read_tail_times
                .swap(0, Ordering::AcqRel),
            safe_write_read_tail_bytes: metrics
                .safe_write_read_tail_bytes
                .swap(0, Ordering::AcqRel),
            allocate_latency: allocate_total_latency / std::cmp::max(1, allocate_total_times),
            allocate_times: allocate_total_times,
            pwrite_latency: pwrite_total_latency / std::cmp::max(1, pwrite_total_times),
            pwrite_times: pwrite_total_times,
        }
    }

    /// 列出某條 chain 在記憶體 `writing_list` 中尚未 commit 的 chunk
    /// (C++ `ChunkEngine::queryUncommittedChunks`)。
    fn query_uncommitted_raw_chunks(
        &self,
        prefix: &[u8],
        error: Pin<&mut CxxString>,
    ) -> Box<RawChunks> {
        match self.query_uncommitted_chunks(prefix) {
            Ok(chunks) => Box::new(RawChunks { vec: chunks }),
            Err(e) => {
                error.push_str(&e.to_string());
                Default::default()
            }
        }
    }

    /// 把某條 chain 所有未 commit 的 chunk 以 `chain_ver` 強制 commit，回傳被處理的清單
    /// (C++ `ChunkEngine::resetUncommittedChunks`，chain 恢復時使用；重啟後由
    /// `Engine::open` 復原進 writing_list 的項目也靠這裡收尾)。
    fn handle_uncommitted_raw_chunks(
        &self,
        prefix: &[u8],
        chain_ver: u32,
        error: Pin<&mut CxxString>,
    ) -> Box<RawChunks> {
        match self.handle_uncommitted_chunks(prefix, chain_ver) {
            Ok(chunks) => Box::new(RawChunks { vec: chunks }),
            Err(e) => {
                error.push_str(&e.to_string());
                Box::default()
            }
        }
    }
}

/// 查詢結果的容器，以 opaque type 交給 C++，C++ 只能用 `len()` 與索引存取函式逐筆讀取。
/// 回傳的 `&[u8]` / `&RawMeta` 都借用自這個 Box，C++ 必須在 Box 釋放前用完。
#[derive(Default)]
struct RawChunks {
    vec: Vec<(Bytes, ChunkMeta)>,
}

// 索引超出範圍時 Vec 會 panic，跨 FFI 的 panic 會讓行程 abort；C++ 端以 len() 控制範圍。
impl RawChunks {
    fn len(&self) -> usize {
        self.vec.len()
    }

    fn chunk_id(&self, pos: usize) -> &[u8] {
        self.vec[pos].0.as_ref()
    }

    fn chunk_meta(&self, pos: usize) -> &ffi::RawMeta {
        // 與 Chunk::raw_meta 相同：RawMeta 是 ChunkMeta 前段欄位的前綴視圖
        unsafe { std::mem::transmute(&self.vec[pos].1) }
    }

    fn chunk_etag(&self, pos: usize) -> &[u8] {
        &self.vec[pos].1.etag
    }

    fn chunk_uncommitted(&self, pos: usize) -> bool {
        self.vec[pos].1.uncommitted
    }
}

// cxx bridge 定義：shared struct 兩邊都看得到欄位 (C++ 端產生同名 struct)；
// `extern "Rust"` 區塊裡的 type 對 C++ 是 opaque，只能透過列出的方法操作。
// 注意：本區塊內刻意只用 `//`，不用 `///`，避免 doc 被 cxx 帶進產生的 C++ header。
#[::cxx::bridge(namespace = "hf3fs::chunk_engine")]
pub mod ffi {
    // 一次 update 的請求與回覆，C++ `ChunkEngine::update` 填入、Rust `update_chunk` 讀取並回填。
    //
    //   field                          dir   meaning
    //   -----------------------------  ----  ----------------------------------------------
    //   without_checksum               in    true: skip verifying `checksum`, compute locally
    //   is_truncate                    in    set chunk length to `offset` (shrink, or
    //                                        zero-extend if longer)
    //   is_remove                      in    delete the chunk
    //   is_syncing                     in    resync write from predecessor: always COW,
    //                                        version taken from `update_ver` as-is
    //   update_ver                     in    CRAQ version of this update; 0 = old + 1
    //                                        else must be exactly committed_ver + 1
    //   chain_ver                      in    chain version; must be >= stored chain_ver
    //   checksum                       in    crc32c of data (verified unless without_checksum)
    //   length / offset                in    write range inside the chunk
    //   data                           in    raw pointer (as u64) to `length` bytes
    //   last_request_id / client_*     in    copied into ChunkMeta (last writer's request)
    //   expected_tag                   in    if non-empty, must equal current etag
    //   desired_tag                    in    etag to set; empty = random hex
    //   create_new                     in    fail with ChunkAlreadyExists if chunk exists
    //   out_non_existent               out   chunk did not exist before this update
    //   out_error_code                 out   3FS StorageCode, set only on error
    //   out_commit_ver / out_chain_ver out   new versions (on early error: current ones)
    //   out_checksum                   out   checksum of the new content (on early error:
    //                                        current checksum, 0 if chunk not exist)
    #[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
    struct UpdateReq {
        without_checksum: bool,
        is_truncate: bool,
        is_remove: bool,
        is_syncing: bool,
        update_ver: u32,
        chain_ver: u32,
        checksum: u32,
        length: u32,
        offset: u32,
        // SAFETY: This u64 field represents a raw pointer to data of size `length`.
        // The pointer must remain valid for the entire duration of update_chunk().
        // Currently safe because data is consumed synchronously within the function.
        data: u64,
        last_request_id: u64,
        last_client_low: u64,
        last_client_high: u64,
        expected_tag: &'static [u8],
        desired_tag: &'static [u8],
        create_new: bool,

        out_non_existent: bool,
        out_error_code: u16,
        out_commit_ver: u32,
        out_chain_ver: u32,
        out_checksum: u32,
    }

    // get_raw_chunks 的一筆請求：chunk_id 由 C++ 填入 (in)，chunk_ptr 由 Rust 回填 (out，
    // 找到為 Arc raw ptr，需 release_raw_chunk；找不到為 null)。
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct GetReq<'a> {
        chunk_id: &'a [u8],
        chunk_ptr: *const Chunk,
    }

    // ChunkMeta 的 C++ 視圖 (只含固定大小的前段欄位，etag / uncommitted 另有方法取得)。
    // 欄位順序必須與 types/chunk_meta.rs 的 ChunkMeta 一致，raw_meta() 靠 transmute 轉換。
    //   pos        Position (u64): chunk_size | cluster | group | index
    //   chain_ver / chunk_ver: CRAQ chain 版本與 chunk 版本
    //   len        資料長度 (bytes)；checksum = crc32c(data[..len])
    //   timestamp  最後更新時間 (UNIX epoch microseconds)
    #[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
    struct RawMeta {
        pos: u64,
        chain_ver: u32,
        chunk_ver: u32,
        len: u32,
        checksum: u32,
        timestamp: u64,
        last_request_id: u64,
        last_client_low: u64,
        last_client_high: u64,
    }

    // 整個引擎 (11 個 size class 加總) 的空間統計，對應 alloc/allocator_counter.rs 的 UsedSize：
    //   allocated_size  已 fallocate 的 group 總容量 (bytes)
    //   reserved_size   已 fallocate 但尚未配給 chunk 的空位容量 (bytes)
    //   position_count  記憶體中被引用的不同 Position 數
    //   position_rc     所有 Position 的引用計數總和
    #[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
    struct RawUsedSize {
        allocated_size: u64,
        reserved_size: u64,
        position_count: u64,
        position_rc: u64,
    }

    // C++ 發 AIO 讀所需的位置：fd 是該 cluster 檔的 direct_fd (O_DIRECT，ZFS 上為 O_SYNC)，
    // offset 是 chunk 在檔案內的起點 (Position::offset)。fd 由 Rust 持有，C++ 不可關閉。
    #[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
    struct FdAndOffset {
        fd: i32,
        offset: u64,
    }

    // get_metrics 的回傳值：距上次呼叫的增量。*_times / *_bytes 為次數與位元組數，
    // *_latency 為平均延遲 (microseconds)。欄位語意見 alloc/metrics.rs。
    #[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Metrics {
        pub copy_on_write_times: u64,
        pub copy_on_write_latency: u64,
        pub copy_on_write_read_bytes: u64,
        pub copy_on_write_read_times: u64,
        pub copy_on_write_read_latency: u64,

        pub checksum_reuse: u64,
        pub checksum_combine: u64,
        pub checksum_recalculate: u64,

        pub safe_write_direct_append: u64,
        pub safe_write_indirect_append: u64,
        pub safe_write_truncate_shorten: u64,
        pub safe_write_truncate_extend: u64,
        pub safe_write_read_tail_times: u64,
        pub safe_write_read_tail_bytes: u64,

        pub allocate_times: u64,
        pub allocate_latency: u64,
        pub pwrite_times: u64,
        pub pwrite_latency: u64,
    }

    // Engine：生命週期、背景配置、讀寫兩階段、查詢與統計。
    extern "Rust" {
        type Engine;
        fn create(
            path: &str,
            create: bool,
            prefix_len: usize,
            error: Pin<&mut CxxString>,
        ) -> *mut Engine;
        fn release(engine: Box<Engine>);

        // 空間管理 (AllocateWorker / CheckWorker)
        fn raw_used_size(&self) -> RawUsedSize;
        fn allocate_groups(&self, min_remain: usize, max_remain: usize, batch_size: usize)
            -> usize;
        fn allocate_ultra_groups(
            &self,
            min_remain: usize,
            max_remain: usize,
            batch_size: usize,
        ) -> usize;
        fn compact_groups(&self, max_reserved: u64) -> usize;

        fn set_allow_to_allocate(&self, val: bool);
        // 行程結束前呼叫，刻意洩漏 meta_cache / writing_list 以跳過大量解構
        unsafe fn speed_up_quit(&self);

        // 讀取路徑：取得 / 歸還 chunk 引用
        fn get_raw_chunk(&self, chunk_id: &[u8], error: Pin<&mut CxxString>) -> *const Chunk;
        fn get_raw_chunks(&self, reqs: &mut [GetReq], error: Pin<&mut CxxString>);
        unsafe fn release_raw_chunk(&self, chunk: *const Chunk);
        unsafe fn release_writing_chunk(&self, chunk: *mut WritingChunk);

        // 寫入路徑：update (第一階段) -> commit (第二階段)
        fn update_raw_chunk(
            &self,
            chunk_id: &[u8],
            req: Pin<&mut UpdateReq>,
            error: Pin<&mut CxxString>,
        ) -> *mut WritingChunk;

        unsafe fn commit_raw_chunk(
            &self,
            new_chunk: *mut WritingChunk,
            sync: bool,
            error: Pin<&mut CxxString>,
        );

        unsafe fn commit_raw_chunks(
            &self,
            reqs: &[*mut WritingChunk],
            sync: bool,
            error: Pin<&mut CxxString>,
        );

        // 查詢 / 批次刪除 / 統計 / 未提交寫入的處理
        fn query_raw_chunks(
            &self,
            begin: &[u8],
            end: &[u8],
            max_count: u64,
            error: Pin<&mut CxxString>,
        ) -> Box<RawChunks>;

        fn query_all_raw_chunks(&self, prefix: &[u8], error: Pin<&mut CxxString>)
            -> Box<RawChunks>;

        fn query_raw_chunks_by_timestamp(
            &self,
            prefix: &[u8],
            begin: u64,
            end: u64,
            max_count: u64,
            error: Pin<&mut CxxString>,
        ) -> Box<RawChunks>;

        fn raw_batch_remove(
            &self,
            begin: &[u8],
            end: &[u8],
            max_count: u64,
            error: Pin<&mut CxxString>,
        ) -> u64;

        fn query_raw_used_size(&self, prefix: &[u8], error: Pin<&mut CxxString>) -> u64;

        fn get_metrics(&self) -> Metrics;

        fn query_uncommitted_raw_chunks(
            &self,
            prefix: &[u8],
            error: Pin<&mut CxxString>,
        ) -> Box<RawChunks>;

        fn handle_uncommitted_raw_chunks(
            &self,
            prefix: &[u8],
            chain_ver: u32,
            error: Pin<&mut CxxString>,
        ) -> Box<RawChunks>;
    }

    // 日誌初始化 (LogGuard 需持有到行程結束)
    extern "Rust" {
        type LogGuard;
        fn init_log(path: &str, error: Pin<&mut CxxString>) -> *mut LogGuard;
    }

    // 已提交的 chunk (get_raw_chunk 回傳)：讀 meta、取得 AIO 用的 fd 與 offset
    extern "Rust" {
        type Chunk;
        fn raw_meta(&self) -> &RawMeta;
        fn raw_etag(&self) -> &[u8];
        fn uncommitted(&self) -> bool;
        fn fd_and_offset(&self) -> FdAndOffset;
    }

    // 寫入中的 chunk (update_raw_chunk 回傳，等待 commit)
    extern "Rust" {
        type WritingChunk;
        fn raw_meta(&self) -> &RawMeta;
        fn raw_etag(&self) -> &[u8];
        fn uncommitted(&self) -> bool;
        fn raw_chunk(&self) -> *const Chunk;
        fn set_chain_ver(&mut self, chain_ver: u32);
    }

    // 查詢結果集合，以索引逐筆讀取
    extern "Rust" {
        type RawChunks;
        fn len(&self) -> usize;
        fn chunk_id(&self, pos: usize) -> &[u8];
        fn chunk_meta(&self, pos: usize) -> &RawMeta;
        fn chunk_etag(&self, pos: usize) -> &[u8];
        fn chunk_uncommitted(&self, pos: usize) -> bool;
    }
}

// 編譯期檢查上面幾個 transmute 的前提：
// - ChunkMeta -> RawMeta：只檢查 align (RawMeta 是前綴視圖，size 本來就比 ChunkMeta 小)
// - UsedSize -> RawUsedSize：size 與 align 都要相同 (按值 transmute)
// - Metrics (alloc/metrics.rs, AtomicU64) vs ffi::Metrics (u64)：欄位數一致的粗略檢查；
//   實際轉換在 get_metrics 逐欄位進行，不是 transmute
static_assertions::const_assert_eq!(
    std::mem::align_of::<ChunkMeta>(),
    std::mem::align_of::<ffi::RawMeta>()
);
static_assertions::const_assert_eq!(
    std::mem::size_of::<UsedSize>(),
    std::mem::size_of::<ffi::RawUsedSize>()
);
static_assertions::const_assert_eq!(
    std::mem::align_of::<UsedSize>(),
    std::mem::align_of::<ffi::RawUsedSize>()
);
static_assertions::const_assert_eq!(
    std::mem::size_of::<Metrics>(),
    std::mem::size_of::<ffi::Metrics>()
);
static_assertions::const_assert_eq!(
    std::mem::align_of::<Metrics>(),
    std::mem::align_of::<ffi::Metrics>()
);
