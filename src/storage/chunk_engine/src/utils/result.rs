//! crate 共用的錯誤型別。
//!
//! 跨 FFI 時不傳 enum 本身，`cxx.rs` 用兩條管道把錯誤交給 C++：
//! - 錯誤訊息：`to_string()` 後寫進 C++ 傳入的 `CxxString` out-param；C++ 以「字串非空」
//!   判斷失敗。`Display` 直接沿用 `Debug`，輸出形如 `ChunkMissingUpdate("...")`。
//! - 錯誤碼：只有 `update_raw_chunk` 會依變體把 3FS `StorageCode` 數值填進
//!   `UpdateReq.out_error_code`；其他介面失敗時 C++ 自行決定錯誤碼
//!   (例如 commit 失敗一律 `kChunkMetadataSetError`，見 `src/storage/store/ChunkEngine.cc`)。
//!
//! ```text
//!   Engine::update_chunk() -> Err(Error::ChunkMissingUpdate(msg))
//!         |
//!         v  cxx.rs update_raw_chunk
//!   error (CxxString)  = "ChunkMissingUpdate(\"missing update 5 > 3 + 1\")"
//!   req.out_error_code = 4007
//!         |
//!         v  C++ ChunkEngine::update: makeError(req.out_error_code, error)
//! ```

/// 錯誤分類：
/// - IO / 儲存層：`IoError` (pread/pwrite/fallocate)、`RocksDBError`、`MetaError` (RocksDB 內容
///   解析失敗、bitset 狀態不一致)、`SerializationError` (derse 序列化)。
/// - 參數：`InvalidArg`。
/// - 寫入時的 CRAQ 版本與資料檢查 (`core/engine.rs` 的 `update_chunk`)：
///   `ChecksumMismatch`、`ChainVersionMismatch`、`ChunkETagMismatch`、`ChunkAlreadyExists`、
///   `ChunkCommittedUpdate` (update_ver 已提交過)、`ChunkMissingUpdate` (中間缺了版本)。
/// - 空間：`NoSpace` (預備池空且不允許同步配置新 group)。
#[derive(Debug, PartialEq)]
pub enum Error {
    IoError(String),
    RocksDBError(String),
    MetaError(String),
    InvalidArg(String),
    SerializationError(derse::Error),
    ChecksumMismatch(String),
    ChainVersionMismatch(String),
    ChunkETagMismatch(String),
    ChunkAlreadyExists,
    ChunkCommittedUpdate(String),
    ChunkMissingUpdate(String),
    NoSpace,
}

pub type Result<T> = std::result::Result<T, Error>;

/// 委派給 `Debug`，錯誤字串會帶上變體名稱，方便在 C++ log 裡辨識。
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let error = Error::InvalidArg("invalid pos".into());
        assert_eq!(error.to_string(), r#"InvalidArg("invalid pos")"#);
    }
}
