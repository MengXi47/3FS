//! chunk 大小分級（size class）常數。
//!
//! chunk_engine 支援 11 級 chunk 大小：從 64KiB 起每級乘 2，到 64MiB 為止。
//! 每一級各有一個獨立的 `Allocator` 和一個磁碟目錄 `<engine_path>/<size>/`
//! （見 `alloc/allocators.rs` 的 `Allocators::new`）。
//!
//! ```text
//!  class idx | chunk size | group size (x256) | named const
//!  ----------+------------+-------------------+-------------------
//!      0     |   64KiB    |      16MiB        | CHUNK_SIZE_SMALL
//!      1     |  128KiB    |      32MiB        |
//!      2     |  256KiB    |      64MiB        |
//!      3     |  512KiB    |     128MiB        | CHUNK_SIZE_NORMAL
//!      4     |    1MiB    |     256MiB        |
//!      5     |    2MiB    |     512MiB        |
//!      6     |    4MiB    |       1GiB        | CHUNK_SIZE_LARGE
//!      7     |    8MiB    |       2GiB        |
//!      8     |   16MiB    |       4GiB        |
//!      9     |   32MiB    |       8GiB        |
//!     10     |   64MiB    |      16GiB        | CHUNK_SIZE_ULTRA
//!
//!  class idx = log2(chunk size) - CHUNK_SIZE_SHIFT(16)
//! ```

use super::super::Size;

/// 最小的 size class（第 0 級）；`Allocators::select_by_size` 把 <= 64KiB 的資料都放這一級。
pub const CHUNK_SIZE_SMALL: Size = Size::kibibyte(64);
/// 預設 size class（第 3 級）；`GroupId::default()` 用它。
pub const CHUNK_SIZE_NORMAL: Size = Size::kibibyte(512);
/// 「一般」與「ultra」group 的分界：`Allocators::allocate_groups` 以
/// `chunk_size > CHUNK_SIZE_LARGE` 判斷是否為 ultra group，兩者由不同的水位參數維持預備池。
pub const CHUNK_SIZE_LARGE: Size = Size::mebibyte(4);
/// 最大的 size class（第 10 級）；也是 `alloc/chunk.rs` 中 thread-local 暫存 buffer 的大小，
/// 確保任何 chunk 都能整個讀進 buffer 做 copy-on-write。
pub const CHUNK_SIZE_ULTRA: Size = Size::mebibyte(64);
/// 用來把 chunk 大小換算成 class idx：`trailing_zeros(size) - CHUNK_SIZE_SHIFT`。
pub const CHUNK_SIZE_SHIFT: usize = 16; // 64KiB is 2^16
/// size class 的總數，也就是 `Allocators.vec` 的長度。
pub const CHUNK_SIZE_NUMBER: usize = 11; // from 64KiB to 64MiB
