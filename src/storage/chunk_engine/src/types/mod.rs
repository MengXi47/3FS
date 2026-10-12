//! 基礎型別層：整個 crate 共用的位址、狀態與中繼資料型別。
//!
//! 這一層不依賴 `alloc/`、`meta/`、`file/`、`core/`，只依賴 `utils/`（`Size`、`Error`）。
//!
//! ```text
//!   GroupId (u64)  ----- + index (u8) ----->  Position (u64)
//!   (size, group, cluster)                    (size, cluster, group, index)
//!        |                                          |
//!        | key of                                   | field of
//!        v                                          v
//!   GroupState (256-bit bitset)               ChunkMeta (pos, versions, len, crc, ...)
//!        ^                                          |
//!        | applied by RocksDB MergeOp               | stored in RocksDB 0x01 / 0x09
//!        |                                          v
//!   MergeState {acquire, release}             returned to C++ via cxx.rs (RawMeta)
//! ```
//!
//! - `constants`：11 級 chunk 大小。
//! - `group_id` / `position`：把多個欄位 bit-pack 成一個 u64 的磁碟位址。
//! - `group_state`：一個 group 內 256 個 chunk 的配置 bitset。
//! - `merge_state`：對 bitset 的增量操作，用於 RocksDB merge。
//! - `chunk_meta`：單一 chunk 的中繼資料。

mod chunk_meta;
mod constants;
mod group_id;
mod group_state;
mod merge_state;
mod position;

pub use chunk_meta::*;
pub use constants::*;
pub use group_id::*;
pub use group_state::*;
pub use merge_state::*;
pub use position::*;
