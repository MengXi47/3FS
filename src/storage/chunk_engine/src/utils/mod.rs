//! 最底層的共用工具 (layer 0)，不依賴 crate 內其他模組；彼此之間只有 `aligned.rs` 用到 `Size`。
//!
//! ```text
//!   aligned.rs     AlignedBuffer / ALIGN_SIZE (4KiB)   <- O_DIRECT IO buffers
//!   bytes.rs       Bytes = TinyVec<[u8; 28]>           <- chunk_id / prefix keys
//!   result.rs      Error / Result                      <- crate-wide error type
//!   shards_map.rs  ShardsMap<K, V> (64 shards default) <- allocator maps
//!   shards_set.rs  ShardsSet<K>    (64 shards default) <- allocator group sets
//!   size.rs        Size(u64) + KiB/MiB Display         <- every size / offset
//!   worker.rs      Worker / WorkerBuilder              <- background threads
//! ```

mod aligned;
mod bytes;
mod result;
mod shards_map;
mod shards_set;
mod size;
mod worker;

pub use aligned::*;
pub use bytes::*;
pub use result::*;
pub use shards_map::*;
pub use shards_set::*;
pub use size::*;
pub use worker::*;
