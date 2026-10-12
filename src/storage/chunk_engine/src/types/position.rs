//! `Position`：單一 chunk 在磁碟上的完整位址（u64），也是 `ChunkMeta.pos`。
//!
//! 它由 `GroupId` 加上 8 位的 index 組成，但重新排列了欄位：
//! cluster 從最低 8 位搬到 32-39 位，空出最低 8 位放 index。
//!
//! ```text
//!  GroupId (u64)
//!   63                    32 31                      8 7        0
//!  +------------------------+-------------------------+----------+
//!  |   chunk_size (32bit)   |      group (24bit)      | cluster  |
//!  +------------------------+-------------------------+----------+
//!                                                          |
//!                     Position::new(group_id, index)       | moved up
//!                                                          v
//!  Position (u64)
//!   63              40 39      32 31                      8 7        0
//!  +------------------+----------+-------------------------+----------+
//!  | chunk_size >> 8  | cluster  |      group (24bit)      |  index   |
//!  |     (24bit)      |  (8bit)  |                         |  (8bit)  |
//!  +------------------+----------+-------------------------+----------+
//!                                 |<---- low 32 bits = group*256+index ---->|
//!
//!  file   = <engine_path>/<chunk_size>/<cluster as %02X>
//!  offset = chunk_size * (low 32 bits)        (see offset())
//! ```
//!
//! 為什麼要搬 cluster：
//!
//! - 低 32 位剛好是「檔案內第幾個 chunk」，`offset()` 只要一次乘法。
//! - index 在最後一個位元組，`MetaKey::group_to_chunks_key_prefix` 把 big-endian
//!   bytes 去掉最後一個位元組，就得到「同一個 group 的 256 個 chunk」的共同前綴
//!   （RocksDB 前綴 0x03，compaction 用它列出 group 內的 chunk）。
//!
//! 代價是 chunk_size 只剩 24 位（存 chunk_size >> 8），chunk_size 必須是 256 的倍數；
//! 最小 64KiB，不構成限制。

use super::super::*;

use derse::{Deserialize, Deserializer, Serialize, Serializer};

/// chunk 的磁碟位址；佈局見檔頭圖。
///
/// `#[repr(C)]`：`ChunkMeta` 也是 `repr(C)`，`cxx.rs` 會把 `&ChunkMeta` 直接 transmute 成
/// `&ffi::RawMeta` 交給 C++，其中 `pos` 對應 `RawMeta.pos: u64`。
#[derive(Copy, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
#[repr(C)]
pub struct Position(pub u64);

impl Position {
    /// group 欄位右移的位數（低 8 位是 index）。
    const SHIFT: u32 = 8;

    // 24bit chunk size + 8bit cluster + 24bit group + 8bit zero
    /// 由 group id 與 group 內的 index（0..=255）組出 chunk 位址。
    pub const fn new(group_id: GroupId, index: u8) -> Self {
        // 三步：
        //   1. group_id & CLEAN       清掉最低 8 位的 cluster
        //   2. | index                index 放進最低 8 位
        //   3. | cluster << 32        cluster 疊進 32-39 位
        //
        // 第 3 步能直接 OR，是因為 chunk_size 至少 64KiB 且是 256 的倍數，
        // 它在 GroupId 裡的最低 8 位（也就是 32-39 位）必為 0。
        //
        // 算例 GroupId(512KiB, cluster=3, group=5) = 0x0008_0000_0000_0503, index=7：
        //   1. 0x0008_0000_0000_0503 & 0xFFFF_FFFF_FFFF_FF00 = 0x0008_0000_0000_0500
        //   2. | 0x07                                         = 0x0008_0000_0000_0507
        //   3. | 0x03 << 32                                   = 0x0008_0003_0000_0507
        const CLEAN: u64 = !((GroupId::COUNT - 1) as u64);
        Self(group_id.inner() & CLEAN | index as u64 | (group_id.cluster() as u64) << 32)
    }

    /// 反向還原所屬的 group id：把 cluster 從 32-39 位搬回最低 8 位。
    pub fn group_id(&self) -> GroupId {
        // MARKS = 0xFF
        // CLEAN = !(0x0000_00FF_0000_00FF) = 0xFFFF_FF00_FFFF_FF00
        //         同時清掉 index（0-7 位）與 cluster（32-39 位）
        //
        // 算例 0x0008_0003_0000_0507：
        //   & CLEAN     = 0x0008_0000_0000_0500
        //   | cluster 3 = 0x0008_0000_0000_0503   == GroupId(512KiB, 3, 5)
        const MARKS: u64 = (GroupId::COUNT - 1) as u64;
        const CLEAN: u64 = !(MARKS | MARKS << 32);
        GroupId::from(self.0 & CLEAN | self.cluster() as u64)
    }

    /// 先右移 40 位丟掉 cluster 和更低的欄位，再左移 8 位補回 chunk_size 原本為 0 的低 8 位。
    pub fn chunk_size(&self) -> Size {
        // 0x0008_0003_0000_0507 >> 40 = 0x0800
        //                       << 8  = 0x8_0000 = 512KiB
        Size::new(self.0 >> 40 << 8)
    }

    pub fn cluster(&self) -> u8 {
        (self.0 >> 32) as u8
    }

    pub fn group(&self) -> u32 {
        (self.0 as u32) >> Self::SHIFT
    }

    /// chunk 在 group 內的序號，也是它在 `GroupState` bitset 中的 bit 位置。
    pub fn index(&self) -> u8 {
        self.0 as u8
    }

    /// chunk 在其 cluster 檔案內的起始位元組偏移。
    ///
    /// `Cluster::pread` / `pwrite` 與交給 C++ 的 `Clusters::fd_and_offset` 都用它。
    pub fn offset(&self) -> Size {
        // 低 32 位 = group * 256 + index = 檔案內第幾個 chunk
        //
        // 算例 0x0008_0003_0000_0507：
        //   low 32 bits = 0x507 = 1287 = 5 * 256 + 7
        //   offset      = 512KiB * 1287 = 640MiB + 3.5MiB
        self.chunk_size() * self.0 as u32 as u64
    }
}

/// 512KiB、cluster 0、group 0、index 0。
impl Default for Position {
    fn default() -> Self {
        Position::new(GroupId::default(), 0)
    }
}

/// RocksDB 讀出的 u64 轉回 Position（例如 `MetaKey::parse_pos_to_chunk_key`）。
impl From<u64> for Position {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// 讓 `pos.to_be_bytes()` 可直接呼叫；`meta/meta_key.rs` 組前綴 0x03 的 key 時使用：
///
/// ```text
///  pos_to_chunk_key          = 0x03 | 00 08 00 | 03 | 00 00 05 | 07
///                                     size>>8   cl   group      idx
///  group_to_chunks_key_prefix= 0x03 | 00 08 00 | 03 | 00 00 05        (last byte popped)
/// ```
///
/// 注意 big-endian 排序是 chunk_size -> cluster -> group -> index，和 `GroupId` 的順序不同。
impl std::ops::Deref for Position {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Position {{ chunk_size: {}, cluster: {}, group: {}, index: {} }}",
            self.chunk_size(),
            self.cluster(),
            self.group(),
            self.index(),
        )
    }
}

/// derse 序列化時就是一個 u64，作為 `ChunkMeta` 的第一個欄位寫進 RocksDB。
impl Serialize for Position {
    fn serialize_to<T: Serializer>(&self, serializer: &mut T) -> derse::Result<()> {
        self.0.serialize_to(serializer)
    }
}

impl<'a> Deserialize<'a> for Position {
    fn deserialize_from<T: Deserializer<'a>>(buf: &mut T) -> derse::Result<Self> {
        Ok(Self(u64::deserialize_from(buf)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // GroupId -> Position -> 各欄位與 group_id() 來回轉換一致，Debug 格式正確
    #[test]
    fn test_group_id_and_position() {
        let group_id = GroupId::new(64 * Size::KB, 23, 233);
        assert_eq!(group_id.chunk_size(), 64 * Size::KB);
        assert_eq!(group_id.cluster(), 23);
        assert_eq!(group_id.group(), 233);
        assert_eq!(
            format!("{:?}", group_id),
            "GroupId { chunk_size: 64KiB, cluster: 23, group: 233 }"
        );

        let position = Position::new(group_id, 223);
        assert_eq!(position.chunk_size(), 64 * Size::KB);
        assert_eq!(position.cluster(), 23);
        assert_eq!(position.group(), 233);
        assert_eq!(position.index(), 223);
        assert_eq!(position.group_id(), group_id);
        assert_eq!(position.to_be_bytes().len(), 8);
        assert_eq!(
            format!("{:?}", position),
            "Position { chunk_size: 64KiB, cluster: 23, group: 233, index: 223 }"
        );
    }
}
