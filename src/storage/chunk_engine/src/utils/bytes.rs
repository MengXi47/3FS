//! crate 內通用的小型位元組串型別，主要用來裝 chunk_id、chain prefix 與 RocksDB key。

/// 長度 <= 28 bytes 時存在 inline 陣列 (不配置 heap)，超過才轉成 `Vec<u8>`。
///
/// C++ 端 (`src/storage/store/ChunkEngine.h`) 傳入的 chunk_id = 4-byte ChainId + 16-byte
/// meta ChunkId = 20 bytes，落在 inline 範圍內，所以 `meta_cache`、`writing_list` 的 key
/// 通常不需要 heap 配置。
///
/// ```text
///   chunk_id (20 bytes)
///   +-------------+----------------------------------------------+
///   | ChainId (4) | ChunkId (16): tenant|rsv|inode(8)|track|chunk |
///   +-------------+----------------------------------------------+
///   |<- prefix ->|        (prefix_len = sizeof(ChainId) = 4)
/// ```
pub type Bytes = tinyvec::TinyVec<[u8; 28]>;
