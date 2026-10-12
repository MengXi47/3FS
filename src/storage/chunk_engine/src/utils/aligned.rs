//! 4KiB 對齊的記憶體 buffer 與對齊判斷，服務 O_DIRECT IO。
//!
//! O_DIRECT 要求「buffer 位址、IO 長度、檔案 offset」三者都對齊邏輯區塊大小，這裡統一取
//! 4KiB。`file/cluster.rs` 的 `pread` / `pwrite` 用 `is_aligned_io` 決定走哪個 fd：
//!
//! ```text
//!   is_aligned_io(buf, offset) && is_aligned_len(buf.len())
//!        |                                   |
//!       yes                                  no
//!        v                                   v
//!   direct_fd (O_DIRECT, bypass page cache)  normal_fd (O_SYNC, via page cache)
//! ```
//!
//! `alloc/chunk.rs` 的 thread-local `BUFFER` 與全域 `ZERO` 都是 `AlignedBuffer`，讓
//! copy-on-write / 補尾端時湊出的資料能直接走 O_DIRECT。

use super::super::Size;
use std::alloc::Layout;

/// O_DIRECT 的對齊單位 (位址、長度、offset 都要是它的倍數)。
pub const ALIGN_SIZE: Size = Size::new(4096);

/// 以 `ALIGN_SIZE` 對齊配置的 heap buffer，Drop 時以相同 layout 釋放。
///
/// 內部用 `&'static mut [u8]` 指向自行 `alloc` 的記憶體；生命週期實際由本型別管理，
/// `'static` 只是為了不必在型別上帶 lifetime 參數。
pub struct AlignedBuffer(&'static mut [u8]);

impl AlignedBuffer {
    /// 配置至少 `size` bytes (向上取整到 4KiB 倍數，最小 4KiB)。
    ///
    /// 注意：記憶體是 `alloc` 而非 `alloc_zeroed`，內容未初始化；也沒有檢查 `alloc` 回傳 null。
    pub fn new(size: usize) -> Self {
        // 1. size 向上取整到 4KiB 倍數 (0 視為 1，避免零大小 layout)
        // 2. 以 4KiB 對齊配置，再包成 slice
        Self(unsafe {
            let size = std::cmp::max(size, 1).next_multiple_of(ALIGN_SIZE.into());
            let layout = Layout::from_size_align_unchecked(size, ALIGN_SIZE.into());
            let ptr = std::alloc::alloc(layout);
            std::slice::from_raw_parts_mut(ptr, size)
        })
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // slice 長度就是 new() 取整後的 size，因此能重建出與配置時相同的 layout
        unsafe {
            let layout = Layout::from_size_align_unchecked(self.0.len(), ALIGN_SIZE.into());
            std::alloc::dealloc(self.0.as_mut_ptr(), layout);
        }
    }
}

impl std::ops::Deref for AlignedBuffer {
    type Target = [u8];

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl std::ops::DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

pub fn create_aligned_buf(size: Size) -> AlignedBuffer {
    AlignedBuffer::new(size.into())
}

/// buffer 起始位址與長度都是 4KiB 倍數。
pub fn is_aligned_buf(data: &[u8]) -> bool {
    data.as_ptr() as u64 % ALIGN_SIZE.0 == 0 && data.len() as u64 % ALIGN_SIZE.0 == 0
}

pub fn is_aligned_len(len: u32) -> bool {
    len % ALIGN_SIZE.0 as u32 == 0
}

/// buffer 對齊且 chunk 內 offset 也對齊。chunk 本身在檔案中的起點
/// (`Position::offset`，chunk_size 至少 64KiB) 必然對齊 4KiB，所以不必再檢查。
pub fn is_aligned_io(data: &[u8], offset: u32) -> bool {
    is_aligned_buf(data) && is_aligned_len(offset)
}
