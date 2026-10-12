//! `Chunk`：一個 chunk 版本在記憶體中的代表 = `ChunkMeta`（含磁碟位置）+ 所屬 `Allocator`。
//!
//! 它同時是「位置的引用」：Chunk 物件存在一天，它的 Position 就不會被重新配置。
//!
//! ```text
//!   Chunk::clone()  ==  Allocator::reference(pos)    rc += 1   (same position, not a data copy)
//!   drop(Chunk)     ==  Allocator::dereference(pos)  rc -= 1   (0 -> memory bit cleared)
//!
//!   holders of a Chunk:
//!     Engine.meta_cache     Arc<Chunk> of the committed version
//!     WritingHolder         new version while update is in flight
//!     C++ readers           Arc<Chunk> via get_raw_chunk until release_raw_chunk
//! ```
//!
//! 寫入策略由 `Engine::update_chunk` 決定，資料怎麼落盤在本檔：
//!
//! ```text
//!   overwrite existing range / syncing / exceeds capacity
//!       -> copy_on_write(): allocate NEW position, write merged data there
//!   pure append within capacity, or truncate
//!       -> clone() (same position) + safe_write(): only touch bytes beyond old len
//!   compaction
//!       -> copy_chunk(): allocate new position in the SAME size class, copy bytes
//! ```
//!
//! 已提交的資料範圍 `[0, len)` 永遠不被原地修改，讀取方握著舊 Chunk 時讀到的內容不會變。
//!
//! 所有 I/O 都盡量湊成 4KiB 對齊，讓 `Cluster` 走 O_DIRECT 的 `direct_fd`。

use super::super::*;
use lazy_static::lazy_static;
use rand::Rng;
use std::cell::RefCell;

use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct Chunk {
    /// 這個版本的中繼資料；`meta.pos` 決定資料在哪個檔案的哪個 offset
    meta: ChunkMeta,
    /// 所屬 size class 的配置器，用來做 I/O 與在 Drop 時歸還位置
    allocator: Arc<Allocator>,
}

pub type ChunkArc = Arc<Chunk>;

// 64MiB 全 0 且 4KiB 對齊的共用 buffer，給 safe_write 對齊路徑補洞（offset > len）時
// 直接用 O_DIRECT 寫出去；唯讀，所以全行程共用一份
lazy_static! {
    static ref ZERO: AlignedBuffer = {
        let mut vec = create_aligned_buf(CHUNK_SIZE_ULTRA);
        vec.fill(0);
        vec
    };
}

impl Chunk {
    // 每個執行緒一塊 64MiB（最大 chunk）、4KiB 對齊的暫存區：讀舊資料、拼接新資料、
    // 補齊尾端 block 都在這裡做，避免每次寫入都配置大塊對齊記憶體。
    // buffer 的 index 直接對應 chunk 內的 offset（vec[x] == chunk 第 x 個位元組）。
    // 代價：每條會寫入的執行緒常駐 64MiB；內容會殘留上一次使用的資料
    thread_local! {
        static BUFFER: RefCell<AlignedBuffer> = RefCell::new(create_aligned_buf(CHUNK_SIZE_ULTRA));
    }

    /// 只包裝，不增加引用計數；呼叫端（`Allocator::allocate` / `reference`）必須已經 rc + 1，
    /// 否則 Drop 時的 dereference 會讓計數對不上。
    pub fn new(meta: ChunkMeta, allocator: Arc<Allocator>) -> Self {
        Self { meta, allocator }
    }

    pub fn meta(&self) -> &ChunkMeta {
        &self.meta
    }

    /// 這個位置最多能放多少位元組，等於所屬 size class 的 chunk size。
    pub fn capacity(&self) -> u32 {
        self.meta.pos.chunk_size().into()
    }

    /// 寫完資料後套上這次更新的版本資訊，並標記為 uncommitted（等 commit 才改回 false）。
    /// `req.out_commit_ver` 此時已由 `Engine::update_chunk` 填成新的 chunk 版本號。
    pub fn update_meta(&mut self, req: &UpdateReq) {
        self.meta.chunk_ver = req.out_commit_ver;
        self.meta.chain_ver = req.chain_ver;
        self.meta.last_request_id = req.last_request_id;
        self.meta.last_client_low = req.last_client_low;
        self.meta.last_client_high = req.last_client_high;
        // 沒指定 etag 就給一個隨機 u64 的十六進位字串，確保每個版本 etag 不同
        if req.desired_tag.is_empty() {
            let r: u64 = rand::thread_rng().gen();
            self.meta.etag = ETag::from(format!("{:X}", r).as_bytes());
        } else {
            self.meta.etag = req.desired_tag.into();
        }
        self.meta.uncommitted = true;
        self.meta.timestamp = ChunkMeta::now();
    }

    pub fn set_chain_ver(&mut self, chain_ver: u32) {
        self.meta.chain_ver = chain_ver;
    }

    pub fn set_committed(&mut self) {
        self.meta.uncommitted = false;
    }

    /// compaction 用（`Engine::move_chunk`）：在同一個 size class 配新位置，原樣複製資料與 meta。
    /// 永遠允許 slow path fallocate（傳 true），不受 allow_to_allocate 限制。
    pub fn copy_chunk(&self) -> Result<Chunk> {
        // 1. allocate new chunk.
        let mut new_chunk = self.allocator.allocate(true)?;

        // 2. copy meta.
        //    只換掉 pos；etag 是 TinyVec 不是 Copy，`..self.meta` 無法從 &self 搬走它，
        //    所以另給空值。空 etag 寫進 RocksDB 後，下次 Engine::get 從 RocksDB 載入時
        //    set_default_etag_if_need 會改用 checksum 的十六進位當 etag
        new_chunk.meta = ChunkMeta {
            pos: new_chunk.meta.pos,
            etag: Default::default(),
            ..self.meta
        };

        // 3. copy data.
        //    長度向上取整到 4KiB，讀寫都走 O_DIRECT；多出的尾巴是舊位置上的殘值，
        //    落在 len 之外不會被讀到
        Self::BUFFER.with(|v| {
            let mut vec = v.borrow_mut();
            let len = self.meta.len.next_multiple_of(ALIGN_SIZE.into());
            let buf = &mut vec[..len as usize]; // aligned.
            self.pread(buf, 0)?;
            new_chunk.pwrite(buf, 0)?;
            Result::Ok(())
        })?;

        Ok(new_chunk)
    }

    /// 寫到新位置：舊版本完全不動，產生一個 rc = 1 的新 Chunk（尚未 update_meta、尚未提交）。
    ///
    /// ```text
    ///  new_len   = max(old.len, offset + data.len)     (syncing: offset + data.len)
    ///  size class chosen by new_len -> may move to a bigger (or smaller) class
    ///
    ///  skip_read = is_syncing || (offset == 0 && data covers whole old chunk)
    ///
    ///  skip_read && aligned(data, offset)
    ///      pwrite(data) directly at offset                   checksum = client's (reuse)
    ///  otherwise
    ///      BUFFER[0 .. old.len]      <- pread old (unless skip_read)
    ///      BUFFER[old.len .. offset] <- 0 (hole)
    ///      BUFFER[offset .. +len]    <- data
    ///      pwrite BUFFER[0 .. align_up(new_len)] at 0
    ///      checksum = client's if skip_read, else crc32c(BUFFER[0 .. new_len])
    /// ```
    ///
    /// skip_read 且未對齊時 BUFFER 不會先讀舊資料，`[0, offset)` 是殘值；
    /// 這條路徑因此假設 skip_read 時 data 從 offset 0 寫起。
    pub fn copy_on_write(
        &self,
        data: &[u8],
        offset: u32,
        checksum: u32,
        is_syncing: bool,
        allow_to_allocate: bool,
        allocators: &Allocators,
        metrics: &Metrics,
    ) -> Result<Chunk> {
        // 1. allocate new chunk.
        //    依新長度挑 size class，所以 copy-on-write 也是 chunk 換 size class 的唯一途徑
        let new_len = std::cmp::max(self.meta.len, offset + data.len() as u32);
        let begin = std::time::Instant::now();
        let mut new_chunk = allocators.allocate(Size::from(new_len), allow_to_allocate)?;
        let begin2 = std::time::Instant::now();
        let latency = begin2.duration_since(begin).as_micros() as _;
        metrics.allocate_times.fetch_add(1, Ordering::AcqRel);
        metrics
            .allocate_latency
            .fetch_add(latency, Ordering::AcqRel);
        metrics.copy_on_write_times.fetch_add(1, Ordering::AcqRel);

        // 2. write data.
        //    skip_read：新資料會完全取代舊內容（resync 整塊覆蓋，或從 0 覆寫到超過舊長度），
        //    不必讀舊資料，client 帶來的 checksum 也就是整塊的 checksum
        let skip_read = is_syncing || (offset == 0 && data.len() >= self.meta.len as usize);
        let checksum = Self::BUFFER.with(|v| {
            let mut vec = v.borrow_mut();
            if !skip_read {
                // aligned read.
                // 2a. 部分覆寫：先把整個舊 chunk 讀進 BUFFER，再在上面蓋新資料
                let len = self.meta.len.next_multiple_of(ALIGN_SIZE.into());
                let begin = std::time::Instant::now();
                self.pread(&mut vec[..len as usize], 0)?;
                let latency = std::time::Instant::now().duration_since(begin).as_micros() as _;
                metrics
                    .copy_on_write_read_times
                    .fetch_add(1, Ordering::AcqRel);
                metrics
                    .copy_on_write_read_bytes
                    .fetch_add(len as _, Ordering::AcqRel);
                metrics
                    .copy_on_write_read_latency
                    .fetch_add(latency, Ordering::AcqRel);
            }

            // aligned write.
            // 2b. 整塊取代且呼叫端 buffer 已對齊：零拷貝，直接 O_DIRECT 寫出
            if skip_read && is_aligned_io(data, offset) {
                let begin = std::time::Instant::now();
                new_chunk.pwrite(data, offset)?;
                let latency = std::time::Instant::now().duration_since(begin).as_micros() as _;
                metrics.pwrite_times.fetch_add(1, Ordering::AcqRel);
                metrics.pwrite_latency.fetch_add(latency, Ordering::AcqRel);
            } else {
                // 2c. 其餘情況在 BUFFER 拼出完整新內容：舊資料 + 0 填洞 + 新資料，
                //     再從 offset 0 一次寫出對齊後的長度
                if self.meta.len < offset {
                    vec[self.meta.len as usize..offset as usize].fill(0);
                }
                vec[offset as usize..][..data.len()].copy_from_slice(data);
                let len = new_len.next_multiple_of(ALIGN_SIZE.into());
                let begin = std::time::Instant::now();
                new_chunk.pwrite(&vec[..len as usize], 0)?;
                let latency = std::time::Instant::now().duration_since(begin).as_micros() as _;
                metrics.pwrite_times.fetch_add(1, Ordering::AcqRel);
                metrics.pwrite_latency.fetch_add(latency, Ordering::AcqRel);
            };

            // 2d. checksum：整塊取代就沿用 client 的，否則對拼好的內容重算
            Result::Ok(if skip_read {
                metrics.checksum_reuse.fetch_add(1, Ordering::AcqRel);
                checksum
            } else {
                metrics.checksum_recalculate.fetch_add(1, Ordering::AcqRel);
                crc32c::crc32c(&vec[..new_len as usize])
            })
        })?;
        let latency = std::time::Instant::now().duration_since(begin2).as_micros() as _;
        metrics
            .copy_on_write_latency
            .fetch_add(latency, Ordering::AcqRel);

        // 3. copy meta.
        //    只設 len 與 checksum；版本、etag 等由呼叫端接著用 update_meta 填。
        //    syncing 以來源端的長度為準（可能比本地舊版本短），一般寫入只會變長
        new_chunk.meta.len = if is_syncing {
            offset + data.len() as u32
        } else {
            new_len
        };
        new_chunk.meta.checksum = checksum;

        Ok(new_chunk)
    }

    /// 原地寫（同一個 Position）：只允許縮短長度或在 `len` 之後追加，絕不改動 `[0, len)`，
    /// 所以正在讀舊版本（舊 len）的人不受影響。`self` 是舊 Chunk 的 clone，改的是新版本 meta。
    ///
    /// 呼叫端保證 offset >= len（不是覆寫）且 offset + data.len <= capacity，
    /// 否則 `Engine::update_chunk` 會改走 copy_on_write。
    ///
    /// ```text
    ///  A. truncate && offset < len               (shorten)
    ///       pread [0, align_up(offset)), len = offset, checksum = crc32c(recompute)
    ///       no disk write: bytes beyond len are simply ignored
    ///
    ///  B. len, offset aligned && data aligned    (direct path, zero copy)
    ///       offset > len : pwrite ZERO[len, offset)    checksum = crc32c_append(zeros)
    ///       data         : pwrite data at offset       checksum = crc32c_combine(old, client)
    ///
    ///  C. unaligned && grows                     (indirect path via BUFFER)
    ///       start = align_down(len)
    ///       BUFFER[start, start+4K) <- pread tail block   (only if len not aligned)
    ///       BUFFER[len, offset)     <- 0
    ///       BUFFER[offset, +len)    <- data
    ///       pwrite BUFFER[start, align_up(new_len)) at start
    ///       checksum = crc32c_append(old, BUFFER[len, new_len))
    ///
    ///  D. otherwise                              data must be empty, nothing to do
    /// ```
    ///
    /// 路徑 C 會重寫 `[start, len)` 這段已提交的位元組，但寫回的是剛讀出的同一份內容。
    pub fn safe_write(
        &mut self,
        data: &[u8],
        offset: u32,
        checksum: u32,
        truncate: bool,
        metrics: &Metrics,
    ) -> Result<()> {
        // A. 縮短：truncate 時 offset 就是新長度。只需重算 [0, offset) 的 checksum
        if truncate && offset < self.meta.len {
            metrics
                .safe_write_truncate_shorten
                .fetch_add(1, Ordering::AcqRel);
            metrics.checksum_recalculate.fetch_add(1, Ordering::AcqRel);
            return Self::BUFFER.with(|v| {
                // aligned read.
                let mut vec = v.borrow_mut();
                let len = offset.next_multiple_of(ALIGN_SIZE.into());
                self.pread(&mut vec[..len as usize], 0)?;
                self.meta.len = offset;
                self.meta.checksum = crc32c::crc32c(&vec[..offset as usize]);
                Result::Ok(())
            });
        }

        // B. 全對齊：舊尾端、寫入點、資料 buffer（位址與長度）都對齊 4KiB，可直接 O_DIRECT
        if is_aligned_len(self.meta.len)
            && is_aligned_len(offset)
            && (data.is_empty() || is_aligned_buf(data))
        {
            // already aligned.
            // B1. 有洞（含 truncate 延長）：用共用的 ZERO buffer 補 0，checksum 接上這段 0
            if offset > self.meta.len {
                let padding = (offset - self.meta.len) as usize;
                let begin = std::time::Instant::now();
                self.pwrite(&ZERO[..padding], self.meta.len)?;
                let latency = std::time::Instant::now().duration_since(begin).as_micros() as _;
                metrics.pwrite_times.fetch_add(1, Ordering::AcqRel);
                metrics.pwrite_latency.fetch_add(latency, Ordering::AcqRel);
                self.meta.len = offset;
                self.meta.checksum = crc32c::crc32c_append(self.meta.checksum, &ZERO[..padding]);
                metrics
                    .safe_write_truncate_extend
                    .fetch_add(1, Ordering::AcqRel);
                metrics.checksum_combine.fetch_add(1, Ordering::AcqRel);
            }

            // B2. 追加資料：此時 len 已補到 offset。資料直接寫出，
            //     checksum 用 crc32c_combine 把舊 checksum 與 client 給的資料 checksum 接起來，
            //     不必讀回任何資料
            if !data.is_empty() {
                assert!(offset == self.meta.len);
                let begin = std::time::Instant::now();
                self.pwrite(data, offset)?;
                let latency = std::time::Instant::now().duration_since(begin).as_micros() as u64;
                metrics.pwrite_times.fetch_add(1, Ordering::AcqRel);
                metrics.pwrite_latency.fetch_add(latency, Ordering::AcqRel);
                self.meta.len = offset + data.len() as u32;
                self.meta.checksum =
                    crc32c::crc32c_combine(self.meta.checksum, checksum, data.len());
                metrics
                    .safe_write_direct_append
                    .fetch_add(1, Ordering::AcqRel);
                metrics.checksum_combine.fetch_add(1, Ordering::AcqRel);
            }
        } else if self.meta.len < offset + data.len() as u32 {
            // copy to buffer and write.
            // C. 未對齊但會變長：在 BUFFER 拼出「最後一個舊 block + 0 + 新資料」，
            //    從舊尾端所在 block 的起點開始寫，讓整次寫入仍是 4KiB 對齊
            assert!(self.meta.len <= offset);
            Self::BUFFER.with(|v| {
                let mut vec = v.borrow_mut();
                // C1. 舊尾端向下對齊；不在 block 邊界上就要先把那個 block 讀回來
                let start = self.meta.len & !(ALIGN_SIZE.0 as u32 - 1);
                if start != self.meta.len {
                    metrics
                        .safe_write_read_tail_times
                        .fetch_add(1, Ordering::AcqRel);
                    metrics
                        .safe_write_read_tail_bytes
                        .fetch_add(ALIGN_SIZE.0, Ordering::AcqRel);
                    self.pread(&mut vec[start as usize..][..ALIGN_SIZE.into()], start)?;
                }
                // C2. 填洞與放入新資料
                if self.meta.len < offset {
                    metrics
                        .safe_write_truncate_extend
                        .fetch_add(1, Ordering::AcqRel);
                    vec[self.meta.len as usize..offset as usize].fill(0);
                }
                vec[offset as usize..][..data.len()].copy_from_slice(data);
                // C3. 寫出 [start, align_up(new_len))，再對新增的 [len, new_len) 增量算 checksum
                //     （包含補的 0；這條路徑不使用 client 給的 checksum）
                let new_len = offset as usize + data.len();
                let begin = std::time::Instant::now();
                self.pwrite(
                    &vec[start as usize..new_len.next_multiple_of(ALIGN_SIZE.into())],
                    start,
                )?;
                let latency = std::time::Instant::now().duration_since(begin).as_micros() as _;
                metrics.pwrite_times.fetch_add(1, Ordering::AcqRel);
                metrics.pwrite_latency.fetch_add(latency, Ordering::AcqRel);
                self.meta.checksum = crc32c::crc32c_append(
                    self.meta.checksum,
                    &vec[self.meta.len as usize..new_len],
                );
                metrics
                    .safe_write_indirect_append
                    .fetch_add(1, Ordering::AcqRel);
                metrics.checksum_combine.fetch_add(1, Ordering::AcqRel);
                self.meta.len = new_len as u32;
                Result::Ok(())
            })?;
        } else {
            // D. 不會變長又不是縮短：只可能是空寫入（例如 truncate 到相同長度）
            assert!(data.is_empty());
        }
        Ok(())
    }

    /// 讀這個版本的資料；offset 是 chunk 內的位移。是否走 O_DIRECT 由 `Cluster::pread`
    /// 依 buffer 與 offset 是否對齊決定。
    pub fn pread(&self, buf: &mut [u8], offset: u32) -> Result<()> {
        self.allocator.clusters.pread(self.meta.pos, buf, offset)
    }

    pub(super) fn pwrite(&self, buf: &[u8], offset: u32) -> Result<()> {
        self.allocator.clusters.pwrite(self.meta.pos, buf, offset)
    }

    /// 給 C++ 讀取路徑：回傳 cluster 檔案的 direct_fd 與此 chunk 的起始位元組 offset，
    /// C++ 自己發 AIO 讀；讀完前必須一直持有這個 Chunk，位置才不會被回收。
    pub fn fd_and_offset(&self) -> FdAndOffset {
        self.allocator.clusters.fd_and_offset(self.meta.pos)
    }
}

// clone 不是複製資料，而是對同一位置再建立一個引用（rc + 1）
impl Clone for Chunk {
    fn clone(&self) -> Self {
        self.allocator.reference(self.meta.clone(), false)
    }
}

// 最後一個引用消失時，記憶體 bitset 中的位置被釋放（見 `ChunkAllocator::dereference`）
impl Drop for Chunk {
    fn drop(&mut self) {
        self.allocator.dereference(self.meta.pos);
    }
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.meta, f)
    }
}
