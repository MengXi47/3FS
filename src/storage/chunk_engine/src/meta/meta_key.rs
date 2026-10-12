//! RocksDB 的 key 格式：所有表共用一個 key 空間，用第一個 byte 區分是哪張表。
//!
//! ```text
//!  prefix  key layout                                               value
//!  ------  -------------------------------------------------------  -----------------------
//!   0x01   01 | ~id[0] ~id[1] .. ~id[n-1]                            ChunkMeta (derse)
//!   0x02   02 | chunk_size (4B) | group (3B) | cluster (1B)          GroupState 32B  (MERGE)
//!   0x03   03 | size>>8 (3B) | cluster (1B) | group (3B) | index     chunk_id
//!   0x04   04 | id[..prefix_len]                                     used size i64 LE (MERGE)
//!   0x05   05                                                        prefix_len u32 LE
//!   0x06   06 | id[..prefix_len] | timestamp u64 BE | id[prefix_len..]  chunk_id or empty
//!   0x07   (retired, writing records moved to 0x09)
//!   0x08   08                                                        version u8
//!   0x09   09 | id[0] id[1] .. id[n-1]   (raw, not inverted)         ChunkMeta (writing)
//!   0x6d   'm' | ..                      (tests only)                bytes   (MERGE: concat)
//!
//!   id = chunk_id given by C++ (its first prefix_len bytes identify the chain)
//!   ~  = bitwise NOT of each byte
//!   0x02 key = 02 | GroupId as BE u64      0x03 key = 03 | Position as BE u64
//! ```
//!
//! 設計重點：
//! - RocksDB 依位元組字典序排 key。整數一律用 big-endian，讓字典序等於數值序，
//!   前綴掃描拿到的 GroupId / Position / timestamp 就是由小到大。
//! - 表前綴放在第一個 byte，每張表在 key 空間裡連成一段：可以 `seek(prefix)` 掃整張表，
//!   也可以 `delete_range([p], [p + 1])` 清整張表 (`MetaStore::remove_range_mut`)。
//! - 0x02 / 0x04 是 merge 型 key，寫入方只送增量，合併規則見 `meta/meta_merge.rs`。

use super::super::{Bytes, Error, GroupId, Position, Result};
use byteorder::{BigEndian, ByteOrder};

/// 一個完整的 RocksDB key 或 key 前綴；所有 key 都經由這裡的建構函式產生，格式集中在一處。
pub struct MetaKey(Bytes);

impl MetaKey {
    pub const CHUNK_META_KEY_PREFIX: u8 = 1;
    pub const GROUP_BITS_KEY_PREFIX: u8 = 2;
    pub const POS_TO_CHUNK_KEY_PREFIX: u8 = 3;
    pub const USED_SIZE_KEY_PREFIX: u8 = 4;
    pub const USED_SIZE_PREFIX_LEN_KEY: u8 = 5;
    pub const TIMESTAMP_KEY_PREFIX: u8 = 6;
    // 7 曾是寫入中紀錄的前綴，已停用；寫入中紀錄現在用 9 (WRITING_CHUNK_KEY_PREFIX)
    // pub const WRITING_CHUNK_KEY_PREFIX: u8 = 7;
    pub const VERSION_KEY: u8 = 8;
    pub const WRITING_CHUNK_KEY_PREFIX: u8 = 9;
    pub const TEST_KEY_PREFIX: u8 = b'm';

    fn prefix(mark: u8) -> Self {
        let mut vec = Bytes::new();
        vec.push(mark);
        Self(vec)
    }

    pub fn chunk_meta_key_prefix() -> Self {
        Self::prefix(Self::CHUNK_META_KEY_PREFIX)
    }

    /// chunk_id 每個 byte 取反後當 key。取反是一對一、長度不變，但把排序整個倒過來：
    /// 對等長的 chunk_id，RocksDB 正向迭代時 chunk_id 由大到小。
    ///
    /// ```text
    ///   chunk_id 00 05  ->  key 01 ff fa
    ///   chunk_id 00 06  ->  key 01 ff f9     (larger id -> smaller key)
    /// ```
    ///
    /// `MetaStore::query_chunks_from_iterator` 依賴這個性質：seek 到 `end` 後正向走，
    /// 就是 chunk_id 從 end 往 begin 遞減。
    pub fn chunk_meta_key(chunk_id: &[u8]) -> Self {
        let mut out = Self::chunk_meta_key_prefix();
        for num in chunk_id {
            out.0.push(!num)
        }
        out
    }

    pub fn parse_chunk_meta_key(key: &[u8]) -> Bytes {
        let mut out = Bytes::new();
        for num in &key[1..] {
            out.push(!num);
        }
        out
    }

    pub fn group_bits_key_prefix() -> Self {
        Self::prefix(Self::GROUP_BITS_KEY_PREFIX)
    }

    /// 某個 size class 所有 group 的共同前綴：只取 GroupId 大端序的前 4 bytes，也就是 chunk_size。
    ///
    /// ```text
    ///   GroupId BE :  [cs cs cs cs][g g g][c]
    ///   prefix     :  02 [cs cs cs cs]               (5 bytes)
    /// ```
    ///
    /// `ChunkAllocator::load` 用它只掃自己 size class 的 group，並依 (group, cluster)
    /// 的數值序拿到，藉此找出中間缺漏、從未配置過的 group。
    pub fn group_bits_chunk_size_prefix(group_id: GroupId) -> Self {
        let mut out = Self::group_bits_key_prefix();
        out.0.extend_from_slice(&group_id.to_be_bytes()[..4]);
        out
    }

    pub fn group_bits_key(group_id: GroupId) -> Self {
        let mut out = Self::group_bits_key_prefix();
        out.0.extend_from_slice(&group_id.to_be_bytes());
        out
    }

    pub fn parse_group_bits_key(key: &[u8]) -> Result<GroupId> {
        if key.len() == std::mem::size_of::<u8>() + std::mem::size_of::<u64>() {
            let group_id = BigEndian::read_u64(&key[1..]);
            Ok(GroupId::from(group_id))
        } else {
            Err(Error::MetaError(format!(
                "parse group bits key fail: {:?}",
                key
            )))
        }
    }

    pub fn pos_to_chunk_key_prefix() -> Self {
        Self::prefix(Self::POS_TO_CHUNK_KEY_PREFIX)
    }

    /// 某個 group 內 256 個位置的共同前綴。index 是 Position 大端序的最後一個 byte，
    /// 所以把 index = 0 的完整 key 去掉最後一個 byte (`pop`) 就得到前綴。
    ///
    /// ```text
    ///   Position BE :  [size>>8 (3B)][cluster][group (3B)][index]
    ///   full key    :  03 | s s s | c | g g g | i
    ///   prefix      :  03 | s s s | c | g g g |          (8 bytes, any index 0..=255)
    /// ```
    ///
    /// compaction (`Engine::compact_groups`) 用它列出這個 group 裡還有哪些 chunk 要搬走。
    pub fn group_to_chunks_key_prefix(group_id: GroupId) -> Self {
        let mut out = Self::pos_to_chunk_key_prefix();
        out.0
            .extend_from_slice(&Position::new(group_id, 0).to_be_bytes());
        out.0.pop();
        out
    }

    pub fn pos_to_chunk_key(pos: Position) -> Self {
        let mut out = Self::pos_to_chunk_key_prefix();
        out.0.extend_from_slice(&pos.to_be_bytes());
        out
    }

    pub fn parse_pos_to_chunk_key(key: &[u8]) -> Result<Position> {
        if key.len() == std::mem::size_of::<u8>() + std::mem::size_of::<u64>() {
            Ok(Position::from(BigEndian::read_u64(&key[1..])))
        } else {
            Err(Error::MetaError(format!(
                "parse pos to chunk key fail: {:?}",
                key
            )))
        }
    }

    pub fn used_size_key_prefix() -> Self {
        Self::prefix(Self::USED_SIZE_KEY_PREFIX)
    }

    /// 每個 prefix (chain) 一個累計容量 key，值是 i64 LE，用 merge 加減 (`MetaMergeOp`)。
    pub fn used_size_key(prefix: &[u8]) -> Self {
        let mut out = Self::used_size_key_prefix();
        out.0.extend_from_slice(prefix);
        out
    }

    /// 記錄「目前 0x04 used size 是用多長的 prefix 統計的」；與設定不同時要重算
    /// (`MetaStore::update_used_size_if_need`)。
    pub fn used_size_prefix_len_key() -> Self {
        Self::prefix(Self::USED_SIZE_PREFIX_LEN_KEY)
    }

    pub fn timestamp_key_prefix() -> Self {
        Self::prefix(Self::TIMESTAMP_KEY_PREFIX)
    }

    /// 時間範圍查詢的起點：`06 | prefix | timestamp`，後面不接 chunk_id 剩餘部分，
    /// 所以 seek 到它會落在該 prefix 中第一個 timestamp >= 給定值的 key。
    pub fn timestamp_key_filter(prefix: &[u8], timestamp: u64) -> Self {
        let mut out = Self::timestamp_key_prefix();
        out.0.extend_from_slice(prefix);
        out.0.extend_from_slice(&timestamp.to_be_bytes());
        out
    }

    /// 把 timestamp 夾在 prefix 與 chunk_id 剩餘部分之間：
    ///
    /// ```text
    ///   chunk_id = [p p | r r r r]                       (prefix_len = 2)
    ///   key      = 06 | p p | t t t t t t t t | r r r r
    ///                   \_/   \_____________/   \_____/
    ///                 prefix  timestamp (BE)    rest of chunk_id
    /// ```
    ///
    /// 同一個 prefix (chain) 的 chunk 因此依時間排序，`MetaStore::query_chunks_by_timestamp`
    /// 可以 seek 到 (prefix, begin) 再順序掃到 end。`chunk_id.len() >= prefix_len` 由呼叫端
    /// 先用 `MetaStore::check_chunk_id` 保證，否則這裡的 slice 會 panic。
    pub fn timestamp_key(timestamp: u64, chunk_id: &[u8], prefix_len: usize) -> Self {
        let mut out = Self::timestamp_key_filter(&chunk_id[..prefix_len], timestamp);
        out.0.extend_from_slice(&chunk_id[prefix_len..]);
        out
    }

    /// 從 key 拼回 (timestamp, chunk_id)。長度檢查是嚴格大於，所以 chunk_id 剩餘部分
    /// 至少要 1 byte；chunk_id 長度剛好等於 prefix_len 的 key 無法解析。
    pub fn parse_timestamp_key(key: &[u8], prefix_len: usize) -> Result<(u64, Bytes)> {
        const L: usize = std::mem::size_of::<u8>() + std::mem::size_of::<u64>();
        if key.len() > L + prefix_len {
            let mut chunk_id = Bytes::from(&key[1..1 + prefix_len]);
            let timestamp = BigEndian::read_u64(&key[1 + prefix_len..]);
            chunk_id.extend_from_slice(&key[L + prefix_len..]);
            Ok((timestamp, chunk_id))
        } else {
            Err(Error::MetaError(format!(
                "parse timestamp key fail: {:?}",
                key
            )))
        }
    }

    pub fn version_key() -> Self {
        Self::prefix(Self::VERSION_KEY)
    }

    pub fn writing_chunk_key_prefix() -> Self {
        Self::prefix(Self::WRITING_CHUNK_KEY_PREFIX)
    }

    /// 寫入中紀錄：chunk_id 原樣接在 0x09 後面 (不取反)，所以掃描時 chunk_id 由小到大。
    /// update 階段寫入、commit 的 WriteBatch 裡刪除；崩潰後殘留的就是沒提交的寫入。
    pub fn writing_chunk_key(chunk_id: &[u8]) -> Self {
        let mut out = Self::writing_chunk_key_prefix();
        out.0.extend_from_slice(chunk_id);
        out
    }

    pub fn parse_writing_chunk_key(key: &[u8]) -> Result<Bytes> {
        if key.len() > 1 {
            Ok(Bytes::from(&key[1..]))
        } else {
            Err(Error::MetaError(format!(
                "parse writing chunk key fail: {:?}",
                key
            )))
        }
    }
}

impl AsRef<[u8]> for MetaKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    // 各種 key 的建構與解析互為反函數，並驗證錯誤長度的 key 會被拒絕
    #[test]
    fn test_meta_key_create() {
        use super::super::super::*;

        let prefix = MetaKey::chunk_meta_key_prefix();
        assert_eq!(prefix.as_ref(), [MetaKey::CHUNK_META_KEY_PREFIX]);

        let meta_key = MetaKey::chunk_meta_key(&[1, 2, 3, 4]);
        assert_eq!(
            meta_key.as_ref(),
            [MetaKey::CHUNK_META_KEY_PREFIX, !1, !2, !3, !4]
        );

        let group_id = GroupId::new(CHUNK_SIZE_NORMAL, 1, 2);
        let pos = Position::new(group_id, 3);
        let pos_to_chunk_key = MetaKey::pos_to_chunk_key(pos);
        assert_eq!(pos_to_chunk_key.as_ref().len(), 1 + 8);
        let parsed_pos = MetaKey::parse_pos_to_chunk_key(pos_to_chunk_key.as_ref()).unwrap();
        assert_eq!(pos, parsed_pos);

        let group_to_chunks_key_prefix = MetaKey::group_to_chunks_key_prefix(group_id);
        assert_eq!(group_to_chunks_key_prefix.as_ref().len(), 8);

        assert!(MetaKey::parse_group_bits_key(&[]).is_err());
        assert!(MetaKey::parse_pos_to_chunk_key(&[]).is_err());

        let timestamp_key = MetaKey::timestamp_key(1024, &[1, 2, 3, 4], 2);
        let (timestamp, chunk) = MetaKey::parse_timestamp_key(&timestamp_key.0, 2).unwrap();
        assert_eq!(timestamp, 1024);
        assert_eq!(chunk, [1, 2, 3, 4].as_slice());

        MetaKey::parse_timestamp_key(&[MetaKey::TIMESTAMP_KEY_PREFIX, 0, 1, 2, 3, 4, 5, 6, 7], 0)
            .unwrap_err();

        MetaKey::parse_writing_chunk_key(MetaKey::writing_chunk_key_prefix().as_ref()).unwrap_err();
    }
}
