//! `GroupId`：用一個 u64 表示「哪種 chunk 大小、哪個 cluster 檔案、檔案內第幾個 group」。
//!
//! group 是磁碟空間的配置單位（一次 fallocate / punch hole 256 個 chunk），
//! 它的 id 同時是 RocksDB 前綴 0x02（group bits）的 key。
//!
//! ```text
//!  GroupId (u64)
//!   63                    32 31                      8 7        0
//!  +------------------------+-------------------------+----------+
//!  |   chunk_size (32bit)   |      group (24bit)      | cluster  |
//!  +------------------------+-------------------------+----------+
//!
//!  file   = <engine_path>/<chunk_size>/<cluster as %02X>
//!  offset = group * 256 * chunk_size        (see offset())
//!  size   = 256 * chunk_size                (see size())
//! ```
//!
//! 位元佈局本身就帶有行為：
//!
//! 1. 數值排序（derive `Ord`）＝ RocksDB big-endian key 的字典序，啟動時可以依序掃描補洞。
//! 2. cluster 在最低位，`plus_one()` 先換檔案再換 group，新 group 自動輪流分散到 256 個檔案。
//! 3. 一個遮罩加一次乘法就得到 group 在檔案內的位元組偏移。

use super::super::*;

/// 一個 group 的位址；欄位佈局見檔頭圖。
///
/// 欄位是 `pub u64`，任何人都能用 `GroupId(x)` 造出任意值，型別本身不做驗證。
/// `Ord` 直接比較 u64，順序為 chunk_size -> group -> cluster；
/// `ChunkAllocator::load`（`alloc/chunk_allocator.rs`）用 `current < group_id`
/// 找出 RocksDB 裡缺少的 group，依賴的正是這個順序與 big-endian key 順序一致。
#[derive(Copy, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct GroupId(pub u64);

/// 預設為 512KiB、cluster 0、group 0。
///
/// `GroupAllocator::init` 用它初始化 `next_group_id`；正式路徑走 `ChunkAllocator::load`，
/// 會改用 `GroupId::new(chunk_size, 0, 0)`，所以寫死 512KiB 只影響測試。
impl Default for GroupId {
    fn default() -> Self {
        GroupId::new(CHUNK_SIZE_NORMAL, 0, 0)
    }
}

impl GroupId {
    // 32bit chunk size + 24bit group + 8bit cluster
    /// cluster 佔的位元數；`Position` 中 index 也佔 8 位，沿用同一個值。
    const SHIFT: u32 = 8;
    /// 256。身兼兩義：每個 size class 有 256 個 cluster 檔案，每個 group 有 256 個 chunk
    /// （對應 `GroupState` 的 256 bit）。
    pub const COUNT: u32 = (1 << Self::SHIFT);

    /// 把三個欄位 pack 成 u64。
    ///
    /// 不做範圍檢查：`group` 必須 < 2^24，否則 `group << 8` 在 u32 上會靜默丟掉高位；
    /// `chunk_size` 必須 < 2^32（最大 64MiB，不成問題）。
    pub const fn new(chunk_size: Size, cluster: u8, group: u32) -> Self {
        // 運算子優先序：as > << > |，等同
        //   hi = chunk_size << 32
        //   lo = ((group << 8) | cluster) as u32 -> zero-extend to u64
        //   id = hi | lo
        //
        // 算例 new(512KiB, cluster=3, group=5)：
        //   chunk_size = 524288 = 0x0008_0000
        //   hi = 0x0008_0000_0000_0000
        //   lo = (5 << 8) | 3 = 0x0000_0503
        //   id = 0x0008_0000_0000_0503
        Self(chunk_size.0 << 32 | (group << Self::SHIFT | cluster as u32) as u64)
    }

    /// 右移 32 位，低位的 group / cluster 自然被推掉。
    pub const fn chunk_size(&self) -> Size {
        Size::new(self.0 >> 32)
    }

    /// 轉成 u8 只留最低 8 位。
    pub const fn cluster(&self) -> u8 {
        self.0 as u8
    }

    /// 先轉 u32 砍掉 chunk_size，再右移 8 位推掉 cluster。
    pub const fn group(&self) -> u32 {
        (self.0 as u32) >> Self::SHIFT
    }

    /// group 在其 cluster 檔案內的起始位元組偏移，供 `Cluster::fallocate` 使用。
    ///
    /// cluster 不參與計算：它決定開哪個檔案，不決定檔案內的位置。
    pub fn offset(&self) -> Size {
        // 關鍵在 MARKS 是在 u32 上取反後才轉 u64：
        //
        //   GroupId::COUNT - 1   = 0x0000_00FF            (u32)
        //   !(...)               = 0xFFFF_FF00            (u32)
        //   as u64               = 0x0000_0000_FFFF_FF00  (zero-extended)
        //
        // 一個遮罩同時清掉高 32 位的 chunk_size（零擴展）與低 8 位的 cluster，
        // 只剩 group << 8 = group * 256。
        //
        // 算例 (512KiB, cluster=3, group=5)：
        //   self.0 & MARKS = 0x500 = 1280
        //   offset = 512KiB * 1280 = 640MiB = 5 groups * 128MiB
        const MARKS: u64 = !(GroupId::COUNT - 1) as u64;
        self.chunk_size() * (self.0 & MARKS)
    }

    /// group 大小 = chunk_size * 256，也是 fallocate 的長度
    /// （512KiB -> 128MiB，64MiB -> 16GiB）。
    pub fn size(&self) -> Size {
        self.chunk_size() * GroupId::COUNT as u64
    }

    /// 下一個 group id；`GroupAllocator::get_unallocated_group_id` 用它產生新 group。
    ///
    /// 因為 cluster 在最低位，加 1 先換檔案、滿 256 才進位到 group：
    ///
    /// ```text
    ///  (g=0,c=0) -> (g=0,c=1) -> ... -> (g=0,c=255) -> carry -> (g=1,c=0) -> ...
    ///
    ///  cluster file:  00   01   02  ...  FF   00   01  ...
    ///  group:          0    0    0        0    1    1
    /// ```
    ///
    /// 結果是新 group 輪流落在 256 個檔案上，所有檔案一起慢慢長大。
    /// group 超過 2^24 時進位會溢進 chunk_size，沒有防呆，但實務上碰不到。
    pub fn plus_one(&self) -> Self {
        Self(self.0 + 1)
    }

    /// 原地版的 `plus_one`；`ChunkAllocator::load` 掃描時用它推進游標。
    pub fn next(&mut self) {
        self.0 += 1
    }

    /// 取出原始 u64。和 `Deref` 重複，但它是 `const fn`，
    /// 讓同為 `const fn` 的 `Position::new` 能呼叫（trait 方法不能在 const 環境用）。
    pub const fn inner(&self) -> u64 {
        self.0
    }
}

/// RocksDB 讀出的 u64 轉回 GroupId（例如 `MetaKey::parse_group_bits_key`）。
impl From<u64> for GroupId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<GroupId> for u64 {
    fn from(val: GroupId) -> Self {
        val.0
    }
}

/// 讓 `group_id.to_be_bytes()` 直接呼叫 u64 的方法；`meta/meta_key.rs` 組 key 時使用：
///
/// ```text
///  group_bits_key               = 0x02 | 00 08 00 00 | 00 00 05 03
///                                        ^^^^^^^^^^^   ^^^^^^^^^^^
///                                        chunk_size    group | cluster
///  group_bits_chunk_size_prefix = 0x02 | 00 08 00 00   (to_be_bytes()[..4])
///                                 -> prefix scan of one whole size class
/// ```
impl std::ops::Deref for GroupId {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// 例：`GroupId { chunk_size: 512KiB, cluster: 3, group: 5 }`；chunk_size 走 `Size` 的 Display。
impl std::fmt::Debug for GroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GroupId {{ chunk_size: {}, cluster: {}, group: {} }}",
            self.chunk_size(),
            self.cluster(),
            self.group(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 連續 plus_one 256000 次：cluster 255 時歸 0 並進位 group，其餘只遞增 cluster
    #[test]
    fn test_group_id_next() {
        let mut group_id = GroupId::default();

        for _ in 0..1000 {
            for i in 0..=255 {
                let next = group_id.plus_one();
                if i == 255 {
                    assert_eq!(group_id.chunk_size(), next.chunk_size());
                    assert_eq!(0, next.cluster());
                    assert_eq!(group_id.group() + 1, next.group());
                } else {
                    assert_eq!(group_id.chunk_size(), next.chunk_size());
                    assert_eq!(group_id.cluster() + 1, next.cluster());
                    assert_eq!(group_id.group(), next.group());
                }
                group_id = next;
            }
        }

        let value = u64::from(group_id);
        assert_eq!(value, group_id.0);
    }
}
