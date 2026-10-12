//! 頂層編排：`Engine` 串接 meta（RocksDB 中繼資料）與 alloc（配置器、chunk I/O），
//! 提供讀取、兩階段寫入、崩潰復原與背景維護 API；C++ 經 `cxx.rs` 呼叫。
//!
//! ```text
//!   cxx.rs (FFI)
//!       |
//!       v
//!   core::Engine ---> meta::MetaStore ---> RocksDB
//!       |
//!       +-----------> alloc::Allocators -> file::Clusters -> data files
//! ```

mod engine;

pub use engine::*;
