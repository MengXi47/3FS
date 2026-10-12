//! 檔案層：把 `GroupId` / `Position` 翻成「哪個檔案、哪個 offset」並執行實際 I/O。
//!
//! 每個 size class 一個 `Clusters`，底下 256 個 `Cluster`（檔案 `00`..`FF`），
//! 每個 `Cluster` 同時開兩個 fd。
//!
//! ```text
//!  Allocators.vec[i] -> Allocator -> Clusters  (one per size class)
//!                                       |
//!            +--------------------------+---------------------------+
//!            |                          |                           |
//!       Cluster 00                 Cluster 01      ...         Cluster FF
//!       <dir>/00                   <dir>/01                    <dir>/FF
//!       normal_fd (O_SYNC)
//!       direct_fd (O_DIRECT, or O_SYNC on ZFS)
//!
//!  <dir> = <engine_path>/<chunk size>   e.g. /data/target1/512KiB
//! ```
//!
//! - `cluster`：單一檔案的 open / fallocate / pread / pwrite。
//! - `clusters`：256 個檔案的集合，依 `cluster()` 欄位分派。
//! - `fs_type`：偵測檔案系統，決定能不能用 O_DIRECT。

mod cluster;
mod clusters;
mod fs_type;

pub use cluster::*;
pub use clusters::*;
pub use fs_type::*;
