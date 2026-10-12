//! 每個 size class 一份的容量計數器，無鎖 (`AtomicU64`)，讓查容量不必拿 `Allocator` 的 Mutex。
//!
//! 兩個核心數字的語意（單位都是「chunk 個數」，乘上 `chunk_size` 才是位元組）：
//!
//! ```text
//!   allocated_chunks = (# groups that own disk space) * 256
//!   reserved_chunks  = free slots inside those groups  (allocated but not used)
//!   used             = allocated_chunks - reserved_chunks
//!
//!   event                         allocated     reserved
//!   ---------------------------   ---------     --------
//!   allocate_group  (fallocate)     +256          +256
//!   deallocate_group (punch hole)   -256          -256
//!   allocate_chunk  (bit 0 -> 1)      0            -1
//!   deallocate_chunk (bit 1 -> 0)     0            +1
//! ```
//!
//! `reserved_chunks` 同時是 compaction 的觸發依據：`ChunkAllocator::get_compact_task` 在它
//! 超過 `max_reserved` 時才挑 group 來搬（代表已配置空間裡的空洞太多）。

use super::super::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct AllocatorCounter {
    pub chunk_size: Size,
    /// 擁有磁碟空間的 group 數 * 256（包含 allocated 預備池、active、full、frozen）
    pub allocated_chunks: AtomicU64,
    /// 上述 group 裡尚未使用的 chunk 槽位數
    pub reserved_chunks: AtomicU64,
    /// `position_rc` 表裡目前有幾個不同的 Position（至少被一個 Chunk 引用）
    pub position_count: AtomicU64,
    /// 所有 Position 引用計數的總和（存活中的 Chunk 物件數）
    pub position_rc: AtomicU64,
}

/// 給外部看的容量快照。`#[repr(C)]` 是因為 `cxx.rs` 的 `raw_used_size` 直接把它
/// `transmute` 成 `ffi::RawUsedSize`，兩者欄位佈局必須一致。
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct UsedSize {
    pub allocated_size: Size,
    pub reserved_size: Size,
    pub position_count: u64,
    pub position_rc: u64,
}

// 讓 `Allocators::used_size` 可以把 11 個 size class 的快照 `.sum()` 起來
impl std::iter::Sum for UsedSize {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        let mut s = UsedSize::default();
        for i in iter {
            s.allocated_size += i.allocated_size;
            s.reserved_size += i.reserved_size;
            s.position_count += i.position_count;
            s.position_rc += i.position_rc;
        }
        s
    }
}

impl AllocatorCounter {
    pub fn new(chunk_size: Size) -> Self {
        Self {
            chunk_size,
            ..Default::default()
        }
    }

    pub fn allocated_chunks(&self) -> u64 {
        self.allocated_chunks.load(Ordering::Acquire)
    }

    pub fn reserved_chunks(&self) -> u64 {
        self.reserved_chunks.load(Ordering::Acquire)
    }

    /// 四個欄位各自 load，彼此之間不是同一瞬間的一致快照，只適合監控用。
    pub fn used_size(&self) -> UsedSize {
        UsedSize {
            allocated_size: self.allocated_chunks() * self.chunk_size,
            reserved_size: self.reserved_chunks() * self.chunk_size,
            position_count: self.position_count.load(Ordering::Acquire),
            position_rc: self.position_rc.load(Ordering::Acquire),
        }
    }

    /// 啟動時由 `ChunkAllocator::load` 掃完 RocksDB 的 group bits 後一次設定。
    pub fn init(&self, allocated_count: u64, reserved_count: u64) {
        self.allocated_chunks
            .store(allocated_count, Ordering::Release);
        self.reserved_chunks
            .store(reserved_count, Ordering::Release);
    }

    /// 一個 group 拿到磁碟空間：新增 256 個槽位，而且全是空的。
    pub fn allocate_group(&self) {
        self.allocated_chunks
            .fetch_add(GroupState::TOTAL_BITS as u64, Ordering::SeqCst);
        self.reserved_chunks
            .fetch_add(GroupState::TOTAL_BITS as u64, Ordering::SeqCst);
    }

    /// 一個空 group 被 punch hole 歸還；只有完全空的 group 才會走到這裡，所以兩邊都減 256。
    pub fn deallocate_group(&self) {
        self.allocated_chunks
            .fetch_sub(GroupState::TOTAL_BITS as u64, Ordering::SeqCst);
        self.reserved_chunks
            .fetch_sub(GroupState::TOTAL_BITS as u64, Ordering::SeqCst);
    }

    pub fn allocate_chunk(&self) {
        self.reserved_chunks.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn deallocate_chunk(&self) {
        self.reserved_chunks.fetch_add(1, Ordering::SeqCst);
    }
}
