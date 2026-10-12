//! 空間配置子系統：決定每個 chunk 落在磁碟的哪個位置 (`Position`)，並管理位置的生命週期。
//!
//! 只處理記憶體狀態與檔案空間 (fallocate)，持久化交給 `meta/`，兩者由 `core/engine.rs` 串起來。
//!
//! ```text
//!  Engine
//!    |
//!    v
//!  Allocators            one per engine, 11 size classes (64KiB .. 64MiB)
//!    |  select_by_size / select_by_pos
//!    v
//!  Arc<Allocator>        one per size class
//!    |-- Clusters        256 data files of this size class   (file/)
//!    |-- AllocatorCounter  allocated / reserved chunk counters
//!    `-- Mutex<ChunkAllocator>
//!          |-- active_groups / active_levels[4] / full_groups / frozen_groups
//!          |-- position_rc   (Position -> ref count)
//!          `-- GroupAllocator
//!                |-- allocated_groups    (fallocated, empty pool)
//!                |-- unallocated_groups  (no disk space)
//!                `-- next_group_id
//!
//!  Chunk         = ChunkMeta + Arc<Allocator>; Clone/Drop drive position_rc
//!  WritingChunk  = Chunk being updated but not yet committed (two-phase write)
//! ```
//!
//! 閱讀順序建議：`group_allocator.rs` → `chunk_allocator.rs` → `allocator.rs` →
//! `allocators.rs` → `chunk.rs` → `writing_chunk.rs`。

mod allocator;
mod allocator_counter;
mod allocators;
mod chunk;
mod chunk_allocator;
mod group_allocator;
mod metrics;
mod writing_chunk;

pub use allocator::*;
pub use allocator_counter::*;
pub use allocators::*;
pub use chunk::*;
pub use chunk_allocator::*;
pub use group_allocator::*;
pub use metrics::*;
pub use writing_chunk::*;
