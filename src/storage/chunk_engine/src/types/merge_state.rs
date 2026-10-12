//! `MergeState`：對 `GroupState` bitset 的增量操作，作為 RocksDB merge operand。
//!
//! 寫入路徑不讀舊的 group bits，只對前綴 0x02 的 key 送出「設哪些 bit、清哪些 bit」，
//! 由 RocksDB 在 compaction 或讀取時合併（`meta/meta_merge.rs`）：
//!
//! ```text
//!  WriteBatch (meta_store.rs)            RocksDB MergeOp (meta_merge.rs)
//!  --------------------------            -------------------------------
//!  merge(0x02+gid, acquire{3})  --+
//!  merge(0x02+gid, release{3})    +-->  partial_merge: fold operands with merge()
//!  merge(0x02+gid, acquire{5})  --+          -> MergeState{acquire:{5}, release:{3}}
//!                                       full_merge: GroupState::update(base, merged)
//!                                           -> set bit 5, clear bit 3
//! ```
//!
//! 合併規則：**後到的操作覆蓋先前對同一 index 的操作**。
//! 因此「依序合併再一次套用」與「逐一套用」結果相同，RocksDB 可以任意分批做 partial merge。

use std::collections::HashSet;

use super::super::*;
use derse::Deserialize;

/// 一組 bit 增量；同一個 index 不會同時出現在兩個集合（`merge` 維持此性質）。
#[derive(Clone, Debug, Default, derse::Deserialize, derse::Serialize, PartialEq)]
pub struct MergeState {
    /// 要設為 1（已配置）的 index。
    pub acquire: HashSet<u8>,
    /// 要清為 0（釋放）的 index。
    pub release: HashSet<u8>,
}

impl MergeState {
    pub fn empty() -> Self {
        Self::default()
    }

    /// 從 RocksDB operand 的 derse bytes 還原。
    pub fn from(value: &[u8]) -> Result<Self> {
        Self::deserialize(value).map_err(Error::SerializationError)
    }

    /// 單一 index 的配置操作；`MetaStore::add_chunk_mut` / `move_chunk_mut` 使用。
    pub fn acquire(pos: u8) -> Self {
        let mut b = Self::empty();
        b.acquire.insert(pos);
        b
    }

    /// 單一 index 的釋放操作；`MetaStore::remove_mut` / `move_chunk_mut` 使用。
    pub fn release(pos: u8) -> Self {
        let mut b = Self::empty();
        b.release.insert(pos);
        b
    }

    /// 把較晚的 `right` 疊到 self 上，right 對同一 index 的操作覆蓋 self 的。
    ///
    /// ```text
    ///  self  : acquire{0}            right : release{0}
    ///  result: acquire{}  release{0}        (later release wins)
    ///
    ///  self  : release{7}            right : acquire{7}
    ///  result: acquire{7} release{}         (later acquire wins)
    /// ```
    pub fn merge(&mut self, right: &Self) {
        for pos in &right.acquire {
            self.acquire.insert(*pos);
            self.release.remove(pos);
        }
        for pos in &right.release {
            self.acquire.remove(pos);
            self.release.insert(*pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 單獨套用 acquire / release 的效果；acquire 後 merge release 等於只有 release；
    // 累積 256 個 acquire 得到 full，再累積 256 個 release 套回 full 得到 empty
    #[test]
    fn test_merge_bits() {
        fn group_bits_apply(mut bits: GroupState, merge_bits: &MergeState) -> GroupState {
            bits.update(merge_bits);
            bits
        }

        let state = GroupState::empty();

        assert_eq!(group_bits_apply(state, &MergeState::empty()), state);

        let acquire_first_bit = MergeState::acquire(0);
        let state_after_acquire = group_bits_apply(state, &acquire_first_bit);
        assert_eq!(state_after_acquire.as_bytes()[0], 1);
        assert_eq!(state_after_acquire.as_bytes()[1..], state.as_bytes()[1..]);

        let release_first_bit = MergeState::release(0);
        let state_after_release = group_bits_apply(state_after_acquire, &release_first_bit);
        assert_eq!(state_after_release, state);

        let mut merge_bits = acquire_first_bit;
        merge_bits.merge(&release_first_bit);
        assert_eq!(merge_bits, release_first_bit);
        assert_eq!(state, group_bits_apply(state, &merge_bits));

        let mut merge_bits = MergeState::empty();
        for i in 0..=255 {
            merge_bits.merge(&MergeState::acquire(i));
        }
        let full_state = group_bits_apply(state, &merge_bits);
        assert!(full_state.is_full());

        for i in 0..=255 {
            merge_bits.merge(&MergeState::release(i));
        }
        let empty_state = group_bits_apply(full_state, &merge_bits);
        assert_eq!(empty_state, state);

        assert!(MergeState::from(&[]).is_err());
    }
}
