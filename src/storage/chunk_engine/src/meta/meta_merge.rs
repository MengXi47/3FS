//! RocksDB merge operator 的業務規則：依 key 的第一個 byte 決定怎麼合併。
//!
//! 寫入方只送「增量」，不必先讀舊值再寫回，所以並發的 WriteBatch 同時改同一個 key
//! 也不會互相覆蓋，而且寫入路徑省掉一次讀。
//!
//! ```text
//!   key prefix        operand                     full_merge(base, ops)       partial(ops)
//!   ----------------  --------------------------  --------------------------  -------------
//!   0x02 group bits   MergeState{acquire,release} base bits (or empty)        fold ops into
//!                     (derse)                      |= acquire, &= ~release    one MergeState
//!   0x04 used size    i64 LE delta                base + sum(ops)             sum(ops)
//!   'm'  tests only   bytes                       base ++ op1 ++ op2 ..       None (keep ops)
//!   other             -                           None -> merge error         None
//! ```
//!
//! MergeState 的折疊 (`MergeState::merge`, `types/merge_state.rs`)：同一個 index 以最後一個
//! 運算元為準，acquire 與 release 兩個集合永遠互斥。
//!
//! ```text
//!   ops (oldest -> newest):  acquire{3}   release{3}   acquire{7}
//!   folded:                  acquire{7}, release{3}
//!   apply to base bits:      bit3 = 0, bit7 = 1, others unchanged
//! ```

use byteorder::{ByteOrder, LittleEndian};
use derse::{DownwardBytes, Serialize};

use super::super::{GroupState, MergeState, MetaKey};

/// 掛在 `RocksDB::open::<MetaMergeOp>` 上的 merge 規則；沒有狀態，只是一組關聯函式。
pub struct MetaMergeOp;

impl super::MergeOp for MetaMergeOp {
    /// 有基底值 (或確定沒有基底值) 時的最終合併。任何運算元格式錯誤都回 None，
    /// 讓 RocksDB 把這次讀取視為錯誤，而不是悄悄算出錯的結果。
    fn full_merge<'a>(
        key: &[u8],
        value: Option<&[u8]>,
        operands: impl Iterator<Item = &'a [u8]>,
    ) -> Option<Vec<u8>> {
        match key[0] {
            MetaKey::GROUP_BITS_KEY_PREFIX => {
                // 1. 先把所有運算元折疊成一個 MergeState (後來的運算元蓋掉先前同 index 的)
                let mut merge_bits = MergeState::empty();
                for op in operands {
                    merge_bits.merge(&MergeState::from(op).ok()?);
                }

                // 2. 套到基底 bitset；沒有基底值時從全空開始。group 被 slow path 配置時
                //    不會先 put 空的 bits (見 GroupAllocator::allocate)，第一次 merge 就走這裡
                let mut bits = if let Some(group_bits) = value {
                    GroupState::from(group_bits).ok()?
                } else {
                    GroupState::empty()
                };
                bits.update(&merge_bits);
                Some(Vec::from(bits.as_bytes()))
            }
            MetaKey::USED_SIZE_KEY_PREFIX => {
                // 運算元是 chunk 大小的增減 (add 為正、remove 為負、move 為新舊大小差)
                let mut total = 0i64;
                for op in operands {
                    if op.len() != std::mem::size_of_val(&total) {
                        return None;
                    }
                    total += LittleEndian::read_i64(op);
                }
                // 基底值可能是 update_used_size_if_need 用 put 寫入的 u64 LE；
                // 非負時與 i64 LE 的位元完全相同，所以直接當 i64 讀
                if let Some(value) = value {
                    if value.len() != std::mem::size_of_val(&total) {
                        return None;
                    }
                    total += LittleEndian::read_i64(value);
                }
                let mut vec = Vec::with_capacity(std::mem::size_of_val(&total));
                vec.extend_from_slice(&total.to_le_bytes());
                Some(vec)
            }
            // 只給 meta/rocksdb.rs 的測試用：key 以 'm' 開頭時把 value 與運算元依序串接
            MetaKey::TEST_KEY_PREFIX => {
                let mut out = Vec::<u8>::new();
                if let Some(value) = value {
                    out.extend_from_slice(value);
                }
                for op in operands {
                    out.extend_from_slice(op);
                }
                Some(out)
            }
            // 不認得的前綴：回 None，RocksDB 讀這個 key 時回報合併失敗
            _ => None,
        }
    }

    /// flush / compaction 時在還沒遇到基底值的情況下預先合併運算元，減少堆積的運算元數量。
    /// 結果必須仍是同格式的「運算元」，因為之後還要再交給 full_merge。
    fn partial_merge<'a>(key: &[u8], operands: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
        match key[0] {
            MetaKey::GROUP_BITS_KEY_PREFIX => {
                // 折疊成一個 MergeState 再序列化回去，仍是合法的 0x02 運算元
                let mut merge_bits = MergeState::empty();
                for op in operands {
                    merge_bits.merge(&MergeState::from(op).ok()?);
                }

                if let Ok(bytes) = merge_bits.serialize::<DownwardBytes>() {
                    Some(Vec::from(bytes.as_slice()))
                } else {
                    None
                }
            }
            MetaKey::USED_SIZE_KEY_PREFIX => {
                let mut total = 0i64;
                for op in operands {
                    if op.len() != std::mem::size_of_val(&total) {
                        return None;
                    }
                    total += LittleEndian::read_i64(op);
                }
                let mut vec = Vec::with_capacity(std::mem::size_of_val(&total));
                vec.extend_from_slice(&total.to_le_bytes());
                Some(vec)
            }
            // 其他前綴 (包含測試用的 'm') 不做預先合併，RocksDB 保留原運算元
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    // 不認得的前綴，partial_merge 回 None
    #[test]
    fn test_meta_merge_op() {
        let slice = [233u8].as_slice();
        assert_eq!(
            MetaMergeOp::partial_merge(&[233], vec![slice].into_iter()),
            None
        );
    }

    // used size 的 partial / full merge 都是加總；長度不是 8 bytes 的運算元回 None
    #[test]
    fn test_used_size_merge() {
        let mut ops = Vec::new();
        for i in 0..10 {
            let mut vec = Vec::with_capacity(std::mem::size_of::<i64>());
            vec.extend_from_slice(&(i as i64).to_le_bytes());
            ops.push(vec);
        }

        let merged = MetaMergeOp::partial_merge(
            &[MetaKey::USED_SIZE_KEY_PREFIX],
            ops.iter().map(|v| v.as_slice()),
        )
        .unwrap();
        assert_eq!(LittleEndian::read_i64(&merged), (0..10).sum::<i64>());

        // test full merge.
        let mut ops = Vec::new();
        for i in 0..10 {
            let mut vec = Vec::with_capacity(std::mem::size_of::<i64>());
            vec.extend_from_slice(&(i as i64).to_le_bytes());
            ops.push(vec);
        }

        let value = 10i64;
        let merged = MetaMergeOp::full_merge(
            &[MetaKey::USED_SIZE_KEY_PREFIX],
            Some(value.to_le_bytes().as_slice()),
            ops.iter().map(|v| v.as_slice()),
        )
        .unwrap();
        assert_eq!(
            LittleEndian::read_i64(&merged),
            (0..10).sum::<i64>() + value
        );

        // test invalid ops.
        let invalid_ops = [vec![1, 2, 3]];
        assert_eq!(
            MetaMergeOp::partial_merge(
                &[MetaKey::USED_SIZE_KEY_PREFIX],
                invalid_ops.iter().map(|v| v.as_slice()),
            ),
            None
        );
    }
}
