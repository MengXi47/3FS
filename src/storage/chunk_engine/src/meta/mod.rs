//! 元資料層：chunk engine 的所有持久化狀態都存在同一個 RocksDB 實例 (`<engine_path>/meta/`)。
//! cluster 資料檔只放 chunk 內容；「chunk 在哪個位置、哪些位置被佔用、每個 chain 用了多少空間、
//! 哪些寫入還沒提交」全部記在這裡。
//!
//! ```text
//!                  core/engine.rs (Engine)
//!                          |
//!     get / add / move / remove chunk, writing record, recovery
//!                          v
//!   +-----------------------------------------------+
//!   | meta_store.rs   MetaStore                     |  business ops:
//!   |   - one WriteBatch per chunk event            |  atomic multi-key update,
//!   |   - occupy / vacate uncommitted positions     |  crash recovery,
//!   |   - used size per prefix, version             |  used size
//!   +-----------+-----------------------+-----------+
//!               | key layout            | merge semantics
//!               v                       v
//!   +-------------------+     +---------------------+
//!   | meta_key.rs       |     | meta_merge.rs       |
//!   | MetaKey           |     | MetaMergeOp         |
//!   +-------------------+     +----------+----------+
//!                                        | impl MergeOp
//!                                        v
//!   +-----------------------------------------------+
//!   | rocksdb.rs   RocksDB / RocksDBIterator        |  thin wrapper
//!   +-----------------------------------------------+
//!                          |
//!                          v
//!                rust-rocksdb (librocksdb)
//! ```
//!
//! alloc/ 也直接用到這一層：`Allocators::create` 把 `MetaStore::iterator` 交給
//! `ChunkAllocator::load` 掃 group bits 重建記憶體狀態；`Allocator::do_allocate_task`
//! 呼叫 `MetaStore::allocate_group` / `remove_group` 登記或註銷整個 group。

mod meta_key;
mod meta_merge;
mod meta_store;
mod rocksdb;

pub use meta_key::*;
pub use meta_merge::*;
pub use meta_store::*;
pub use rocksdb::*;
