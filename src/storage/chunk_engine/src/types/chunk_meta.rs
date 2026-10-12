//! `ChunkMeta`：單一 chunk 的中繼資料。
//!
//! 同一個結構有三種存在形式：
//!
//! ```text
//!   memory   : Chunk { meta: ChunkMeta, .. }  in Engine.meta_cache / writing_list
//!   RocksDB  : 0x01 + ~chunk_id -> ChunkMeta  (committed, derse-encoded)
//!              0x09 + chunk_id  -> ChunkMeta  (writing record, uncommitted = true)
//!   C++      : &ChunkMeta transmuted to &ffi::RawMeta (cxx.rs)
//!
//!  #[repr(C)] layout, first 9 fields shared with ffi::RawMeta:
//!  +-----+-----------+-----------+-----+----------+-----------+-----------------+
//!  | pos | chain_ver | chunk_ver | len | checksum | timestamp | last_request_id |
//!  | u64 |    u32    |    u32    | u32 |   u32    |    u64    |       u64       |
//!  +-----+-----------+-----------+-----+----------+-----------+-----------------+
//!  | last_client_low u64 | last_client_high u64 |  etag (TinyVec) | uncommitted  |
//!  +---------------------+----------------------+-----------------+--------------+
//!   ^------------------- visible to C++ as RawMeta ----^   ^-- exposed via raw_etag()
//!                                                           /  uncommitted()
//! ```
//!
//! 欄位順序不可更動：C++ 端靠 transmute 讀前 9 個欄位，derse 也依欄位順序編碼。

use super::super::*;

/// chunk 的版本標籤。內嵌 14 bytes，超過才配置 heap
/// （checksum 的 8 位十六進位放得下，隨機 u64 的十六進位最長 16 字元則會溢出到 heap）。
pub type ETag = tinyvec::TinyVec<[u8; 14]>;

/// 單一 chunk 的中繼資料；佈局與三種存在形式見檔頭圖。
#[derive(derse::Serialize, derse::Deserialize, Clone, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ChunkMeta {
    /// 資料在磁碟上的位址；`pos.chunk_size()` 就是這個 chunk 的容量上限。
    pub pos: Position,
    /// 寫入時的 chain 版本（CRAQ chain 的 configuration 版本）。
    /// `Engine::update_chunk` 拒絕 `req.chain_ver` 小於此值的請求。
    pub chain_ver: u32,
    /// 已提交的更新版本號。一般更新要求 `update_ver == chunk_ver + 1`，
    /// 否則回 `ChunkCommittedUpdate` / `ChunkMissingUpdate`；syncing 時直接採用 `update_ver`。
    pub chunk_ver: u32,
    /// 有效資料長度（bytes），<= `pos.chunk_size()`。
    pub len: u32,
    /// `[0, len)` 範圍資料的 crc32c。追加寫時用 `crc32c_combine` / `crc32c_append` 增量更新。
    pub checksum: u32,
    /// 最後一次更新的時間，UNIX epoch 起算的微秒；也是 RocksDB 前綴 0x06 時間索引的一部分。
    pub timestamp: u64,
    /// 最後一次更新此 chunk 的 request id；C++ 由 `requestCtx().tag.requestId` 填入。
    pub last_request_id: u64,
    /// 最後一次更新的 client uuid 低 64 位；C++ 端與 high 合成 `lastClientUuid`。
    pub last_client_low: u64,
    /// 最後一次更新的 client uuid 高 64 位。
    pub last_client_high: u64,
    /// 版本標籤：更新時取 `req.desired_tag`，沒給就用隨機 u64 的十六進位（`Chunk::update_meta`）。
    /// `req.expected_tag` 非空時必須與它相等，否則回 `ChunkETagMismatch`（條件式更新）。
    pub etag: ETag,
    /// true 表示這份 meta 屬於「已寫資料、尚未 commit」的版本。
    /// `Chunk::update_meta` 設為 true，`commit_chunk` 前 `set_committed` 改回 false，
    /// 所以前綴 0x01 存的都是 false，前綴 0x09 的寫入中紀錄才是 true。
    pub uncommitted: bool,
}

impl ChunkMeta {
    /// 目前時間，UNIX epoch 起算的微秒。
    pub fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as _
    }

    /// etag 為空時改用 checksum 的大寫十六進位補上。
    ///
    /// `Engine::get_with_entry` 從 RocksDB 載入 meta 時呼叫，讓沒有 etag 的舊資料
    /// 仍有一個可比對的標籤。
    pub fn set_default_etag_if_need(&mut self) {
        if self.etag.is_empty() {
            self.etag = ETag::from(format!("{:X}", self.checksum).as_bytes());
        }
    }
}

/// `pos` 預設為 1GiB chunk size 的位置。1GiB 不是合法的 size class，只是佔位值：
/// `Allocator::allocate` 建立新 chunk 時一定會用 `ChunkMeta { pos, ..Default::default() }`
/// 覆寫它。
impl Default for ChunkMeta {
    fn default() -> Self {
        Self {
            pos: Position::new(GroupId::new(Size::GB, 0, 0), 0),
            chain_ver: 0,
            chunk_ver: 0,
            len: 0,
            checksum: 0,
            timestamp: Self::now(),
            last_request_id: 0,
            last_client_low: 0,
            last_client_high: 0,
            etag: Default::default(),
            uncommitted: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use derse::{Deserialize, Serialize};

    // derse 編碼的位元組內容固定且可還原；開頭的 63 恰等於其後內容的總長度
    // （pos 8 + 4 個 u32 16 + 4 個 u64 32 + etag 1+5 + bool 1），pos 以 little-endian 寫出
    #[test]
    fn test_chunk_meta_seralization() {
        let ser = ChunkMeta {
            pos: Position::new(GroupId::default(), 88),
            chain_ver: 1,
            chunk_ver: 1,
            len: 2,
            timestamp: 0,
            etag: ETag::from(b"hello".as_slice()),
            ..Default::default()
        };

        let bytes: derse::DownwardBytes = ser.serialize().unwrap();
        assert_eq!(
            bytes.as_slice(),
            &[
                63, 88, 0, 0, 0, 0, 0, 8, 0, 1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 5, b'h', b'e', b'l', b'l', b'o', 0,
            ]
        );

        let der = ChunkMeta::deserialize(&bytes[..]).unwrap();
        assert_eq!(ser, der);
    }
}
