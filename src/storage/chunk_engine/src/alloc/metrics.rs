//! 寫入路徑的效能計數器，全部是 `AtomicU64`，由 `Engine` 持有一份 `Arc<Metrics>`。
//!
//! 累加點集中在 `alloc/chunk.rs` 的 `copy_on_write` / `safe_write` 與 `core/engine.rs` 的
//! `update_chunk`。C++ 透過 `cxx.rs` 的 `get_metrics` 讀取時會用 `swap(0)` 取值並歸零，
//! 所以 C++ 每次拿到的是「距上次讀取的增量」，不是累計值。
//!
//! ```text
//!  Chunk::copy_on_write / Chunk::safe_write / Engine::update_chunk
//!          |  fetch_add
//!          v
//!  +-----------------+     get_metrics(): swap(0)      +----------------+
//!  | Metrics (Atomic)| ------------------------------> | ffi::Metrics   |
//!  +-----------------+     (delta since last read)     | (C++ snapshot) |
//!                                                      +----------------+
//! ```
//!
//! 所有 `*_latency` 欄位單位都是微秒（`as_micros()`），且是總和，平均值要除以對應的 `*_times`。

use std::sync::atomic::AtomicU64;

#[derive(Debug, Default)]
#[repr(C)]
pub struct Metrics {
    /// 走 copy-on-write 路徑的次數（覆寫既有範圍、syncing、或超出容量）
    pub copy_on_write_times: AtomicU64,
    /// copy-on-write 從配置完成到寫完新 chunk 的總耗時（微秒）
    pub copy_on_write_latency: AtomicU64,
    /// copy-on-write 為了合併舊資料而讀舊 chunk 的位元組數（已對齊 4KiB）
    pub copy_on_write_read_bytes: AtomicU64,
    pub copy_on_write_read_times: AtomicU64,
    pub copy_on_write_read_latency: AtomicU64,

    // checksum 的三種得出方式：
    //   reuse       = 整塊覆寫，直接沿用 client 帶來的 checksum
    //   combine     = 追加寫，用 crc32c_combine / crc32c_append 增量合併
    //   recalculate = 讀回整塊資料重算
    pub checksum_reuse: AtomicU64,
    pub checksum_combine: AtomicU64,
    pub checksum_recalculate: AtomicU64,

    /// safe_write：長度、offset、buffer 全對齊，資料直接 pwrite（走 O_DIRECT）
    pub safe_write_direct_append: AtomicU64,
    /// safe_write：未對齊，先拷到 thread-local BUFFER 補齊尾巴再寫
    pub safe_write_indirect_append: AtomicU64,
    /// safe_write：truncate 到比目前短的長度（只改 meta，重算 checksum）
    pub safe_write_truncate_shorten: AtomicU64,
    /// safe_write：寫入 offset 超過目前長度，中間空洞補 0
    pub safe_write_truncate_extend: AtomicU64,
    /// safe_write 非對齊追加時，為了補齊最後一個 4KiB block 而讀回尾巴的次數與位元組數
    pub safe_write_read_tail_times: AtomicU64,
    pub safe_write_read_tail_bytes: AtomicU64,

    /// 向 Allocators 要新 chunk 的次數與總耗時（含可能的同步 fallocate slow path）
    pub allocate_times: AtomicU64,
    pub allocate_latency: AtomicU64,
    /// 實際 pwrite 的次數與總耗時
    pub pwrite_times: AtomicU64,
    pub pwrite_latency: AtomicU64,
}
