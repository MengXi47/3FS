//! `GroupState`：一個 group 內 256 個 chunk 的配置狀態（256-bit bitset + 快取的計數）。
//!
//! ```text
//!  bits: [u64; 4]                         index = word * 64 + bit
//!
//!   word 0              word 1              word 2              word 3
//!  +-------------------+-------------------+-------------------+-------------------+
//!  | idx 0 ..... 63    | idx 64 ... 127    | idx 128 .. 191    | idx 192 .. 255    |
//!  +-------------------+-------------------+-------------------+-------------------+
//!   bit = 1 : chunk allocated      bit = 0 : free
//!
//!  level = count / 64            (used by ChunkAllocator.active_levels)
//!   count   0..63  -> level 0
//!   count  64..127 -> level 1
//!   count 128..191 -> level 2
//!   count 192..255 -> level 3
//!   count 256      -> full (moved to full_groups, never in active_levels)
//! ```
//!
//! 出現的地方：
//!
//! - 記憶體：`ChunkAllocator.active_groups` / `frozen_groups` 的 value。
//! - RocksDB：前綴 0x02 + GroupId 的 value，固定 32 bytes（`as_bytes()`）。
//!   寫入端只送 `MergeState` 增量，由 `meta/meta_merge.rs` 的 MergeOp 呼叫 `update()` 套用。

use super::super::*;
use std::num::NonZeroU64;

type Item = u64;
type Bits = [Item; 4];

/// 一個 group 的 256-bit 配置 bitset；`count` 是 1 的個數，避免每次都 popcount。
#[derive(Debug, PartialEq, Copy, Clone)]
pub struct GroupState {
    bits: Bits,
    count: u32,
}

impl GroupState {
    /// 32 bytes，也是 RocksDB 裡 value 的固定長度。
    const TOTAL_BYTES: usize = std::mem::size_of::<Bits>();
    /// 256，一個 group 的 chunk 數。
    pub const TOTAL_BITS: usize = 8 * Self::TOTAL_BYTES;
    /// 64，每個 word 的位元數。
    pub const ITEM_BITS: u8 = 8 * std::mem::size_of::<Item>() as u8;
    /// 4，word 數。
    pub const LEN: usize = Self::TOTAL_BYTES / std::mem::size_of::<Item>();
    /// active group 依使用量分成的級數；每級 256 / 4 = 64 個 chunk。
    pub const LEVELS: usize = 4;

    /// 從 RocksDB 讀出的 32 bytes 還原，並重新計算 `count`。長度不對回 `MetaError`。
    pub fn from(value: &[u8]) -> Result<Self> {
        let mut out = Self::empty();
        if value.len() != Self::TOTAL_BYTES {
            return Err(Error::MetaError(format!(
                "group state load bytes {} != {}",
                value.len(),
                Self::TOTAL_BYTES
            )));
        }
        out.as_mut_bytes().copy_from_slice(value);
        out.count = out.bits.iter().map(|b| b.count_ones()).sum();
        Ok(out)
    }

    pub const fn empty() -> Self {
        Self {
            bits: [0; Self::LEN],
            count: 0,
        }
    }

    /// 全滿。`ChunkAllocator::deallocate` 釋放 full group 中的 chunk 時，
    /// 先用它重建 bitset 再清掉一個 bit（full group 在記憶體裡不保存 bitset）。
    pub fn full() -> Self {
        Self {
            bits: [!0; Self::LEN],
            count: Self::TOTAL_BITS as u32,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn is_full(&self) -> bool {
        self.count == Self::TOTAL_BITS as u32
    }

    /// 配置最低編號的空位，回傳 index；全滿回 `None`。
    pub fn allocate(&mut self) -> Option<u8> {
        // 對每個 word 取反：0（空位）變成 1，再用 trailing_zeros 找最低的那個 1。
        // NonZeroU64::new 在 !v == 0（word 全滿）時回 None，直接跳下一個 word。
        //
        // 算例 v = 0b0111 (idx 0,1,2 used)：
        //   !v               = ...1111_1000
        //   trailing_zeros   = 3              -> idx 3 is the lowest free slot
        //   v |= 1 << 3      = 0b1111
        //   return i * 64 + 3
        for (i, v) in self.bits.iter_mut().enumerate() {
            if let Some(mark) = NonZeroU64::new(!*v) {
                let idx = mark.trailing_zeros();
                *v |= 1 << idx;
                self.count += 1;
                return Some(i as u8 * Self::ITEM_BITS + idx as u8);
            } else {
                continue;
            }
        }
        None
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// count / 64；全滿時為 4，但全滿的 group 不會留在 `active_levels`，實際只會用到 0..=3。
    pub fn level(&self) -> u32 {
        self.count() / (Self::TOTAL_BITS / Self::LEVELS) as u32
    }

    /// index 對應的 bit 是否為 1（已配置）。x = 第幾個 word，y = word 內第幾位。
    pub fn check(&self, index: u8) -> bool {
        let x = index / Self::ITEM_BITS;
        let y = index % Self::ITEM_BITS;
        self.bits[x as usize] & (1 << y) != 0
    }

    /// 釋放 index；該位原本就是 0（重複釋放）回 `MetaError`。
    pub fn deallocate(&mut self, index: u8) -> Result<()> {
        let x = index / Self::ITEM_BITS;
        let y = index % Self::ITEM_BITS;
        let mark = &mut self.bits[x as usize];
        // 已確認該位為 1，所以用 XOR 翻轉等同清除
        if *mark & (1 << y) != 0 {
            *mark ^= 1 << y;
            self.count -= 1;
            Ok(())
        } else {
            Err(Error::MetaError(format!(
                "group state deallocate fail: index {}",
                index
            )))
        }
    }

    /// 套用一個 `MergeState` 增量：先設 acquire 的位，再清 release 的位。
    ///
    /// 由 RocksDB MergeOp 的 `full_merge`（`meta/meta_merge.rs`）呼叫。
    /// 與 `deallocate` 不同，這裡不檢查重複設定或重複清除，最後直接重算 `count`。
    pub fn update(&mut self, merge_bits: &MergeState) {
        for pos in &merge_bits.acquire {
            let x = pos / Self::ITEM_BITS;
            let y = pos % Self::ITEM_BITS;
            self.bits[x as usize] |= 1 << y;
        }
        for pos in &merge_bits.release {
            let x = pos / Self::ITEM_BITS;
            let y = pos % Self::ITEM_BITS;
            self.bits[x as usize] &= !(1 << y);
        }
        self.count = self.bits.iter().map(|b| b.count_ones()).sum();
    }

    /// 以原生位元組序把 `[u64; 4]` 視為 `[u8; 32]`，用來寫進 RocksDB。
    ///
    /// 在 little-endian 機器上 index i 落在 byte i / 8 的第 i % 8 位
    /// （`types/merge_state.rs` 的測試驗證 acquire(0) 後 `as_bytes()[0] == 1`）。
    /// 因為用原生位元組序，RocksDB 裡的資料不能跨不同 endianness 的機器搬移。
    pub fn as_bytes(&self) -> &[u8; Self::TOTAL_BYTES] {
        unsafe { std::mem::transmute(&self.bits) }
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8; Self::TOTAL_BYTES] {
        unsafe { std::mem::transmute(&mut self.bits) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 依序配置 0..=255、滿後回 None；隨機釋放後重複釋放必失敗；bytes 來回轉換一致
    #[test]
    fn test_group_bits_normal() {
        use rand::seq::SliceRandom;

        let mut group_state = GroupState::empty();
        assert_eq!(group_state.count(), 0);

        for i in 0..=255 {
            assert_eq!(i, group_state.allocate().unwrap());
        }
        assert!(group_state.allocate().is_none());
        assert_eq!(group_state.count(), 256);

        let mut vec = (0..=255).collect::<Vec<u8>>();
        vec.shuffle(&mut rand::thread_rng());
        for i in vec {
            group_state.deallocate(i).unwrap();
            group_state.deallocate(i).unwrap_err();

            let j = group_state.allocate().unwrap();
            group_state.deallocate(j).unwrap();
            group_state.deallocate(j).unwrap_err();
        }
        assert_eq!(group_state.count(), 0);

        group_state.allocate().unwrap();
        group_state.allocate().unwrap();
        group_state.deallocate(0).unwrap();
        assert!(group_state.check(1));

        let bytes = group_state.as_bytes();
        let another_state = GroupState::from(bytes).unwrap();
        assert_eq!(another_state, group_state);

        assert!(GroupState::from(&bytes[1..]).is_err());
    }
}
