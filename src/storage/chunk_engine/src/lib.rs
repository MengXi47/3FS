//! chunk_engine：3FS storage 服務的單盤 chunk 儲存引擎，以 Rust 實作、透過 cxx 給 C++ 呼叫。
//!
//! 負責「一顆盤上的 chunk 怎麼配置、怎麼寫、怎麼讀、崩潰後怎麼復原」；CRAQ、chain、副本等
//! 分散式邏輯都在 C++ (`src/storage/`)，這個 crate 完全不碰。C++ 對設定中的每個
//! `target_paths` 項目 (通常一顆盤) 開一個 `Engine`，目錄為 `<path>/engine`
//! (`src/storage/store/StorageTargets.cc`)；該路徑下的多個 target 共用這個引擎，
//! 以 chunk_id 前 `prefix_len = sizeof(ChainId)` bytes 區分 chain。
//!
//! 設計核心是兩個元件：
//! - **Allocator** (`alloc/`)：純記憶體的空間配置，group = 256 個 chunk 的 bitset。
//! - **MetaStore** (`meta/`)：RocksDB，持久化 chunk meta 與配置事件；同一次更新的所有 key
//!   放進一個 WriteBatch，原子生效。
//!
//! ```text
//!              +-------------------------------------+
//!   layer 4    | core/engine.rs        cxx.rs (FFI)  |   orchestration / C++ boundary
//!              +-------------------------------------+
//!                  |                 |
//!              +---v---------+   +---v-------------------------+
//!   layer 3    | alloc/      |   | meta/                       |
//!              |  Allocators |   |  MetaStore  (meta_store.rs) |
//!              |  Allocator  |   |  MetaKey    (meta_key.rs)   |
//!              |  ChunkAlloc |   |  MergeOp    (meta_merge.rs) |
//!              |  GroupAlloc |   |  RocksDB    (rocksdb.rs)    |
//!              |  Chunk      |   +-----------------------------+
//!              |  WritingChk |
//!              +------+------+
//!                     |
//!              +------v------+
//!   layer 2    | file/       |   Clusters -> Cluster (pread/pwrite/fallocate)
//!              +-------------+
//!   layer 1    | types/      |   GroupId, Position, ChunkMeta, GroupState, MergeState
//!   layer 0    | utils/      |   Size, Bytes, AlignedBuffer, ShardsMap/Set, Worker, Error
//!              +-------------+
//! ```
//!
//! 磁碟佈局：
//!
//! ```text
//!   <engine_path>/
//!    |-- meta/            RocksDB (all metadata)
//!    |-- 64KiB/00..FF     size class 0: 256 cluster files
//!    |-- 128KiB/00..FF
//!    |   ...
//!    `-- 64MiB/00..FF     size class 10
//! ```
//!
//! 寫入是兩階段 (配合 CRAQ)：`update_chunk` 寫資料並留下 writing 紀錄 -> chain 後繼 ack ->
//! `commit_chunk` 以一個 WriteBatch 切換 meta。讀取只回傳 `Arc<Chunk>` (fd + offset)，
//! 實際 IO 由 C++ AIO 執行；`Arc` 引用計數保證讀取期間位置不被回收。
//!
//! 所有子模組都以 `pub use xxx::*` 攤平到 crate 根，因此其他檔案用 `use super::super::*;`
//! 或 `use crate::*;` 就能取得全部型別。

mod alloc;
mod core;
mod cxx;
mod file;
mod meta;
mod types;
mod utils;

pub use alloc::*;
pub use core::*;
pub use cxx::{
    ffi::{FdAndOffset, GetReq, UpdateReq},
    CxxString,
};
pub use file::*;
pub use meta::*;
pub use types::*;
pub use utils::*;
