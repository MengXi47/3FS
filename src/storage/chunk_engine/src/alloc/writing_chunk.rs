//! 兩階段寫入的中間產物：`Engine::update_chunk` 已寫好資料、尚未 `commit_chunk` 的 chunk。
//!
//! ```text
//!  Engine::update_chunk
//!     |  data written to new / same position
//!     |  writing_list[prefix][chunk_id] = WritingHolder { chunk (clone), abort: false }
//!     |  RocksDB put 0x09 + chunk_id (writing record, sync)
//!     v
//!  WritingChunk  ---- handed to C++ (raw pointer), chain forwards the update ----
//!     |
//!     +-- Engine::commit_chunk ok -> commit_succ = true
//!     |      drop: remove the entry from writing_list          (normal end)
//!     |
//!     +-- error / C++ releases it without commit
//!            drop: keep the entry, mark holder.abort = true     (leftover)
//!
//!  leftover (abort = true) entries:
//!    - next update_chunk on the same chunk_id may replace it   (Engine::update_chunk)
//!    - chain recovery commits them via Engine::handle_uncommitted_chunks
//!    - the holder's Chunk keeps the written position alive (position_rc)
//! ```
//!
//! writing_list 以 chunk_id 前 `prefix_len` 個位元組分桶（C++ 端這段是 chain 識別），
//! 讓「列出 / 處理某條 chain 的未提交 chunk」只需看一個桶。

use crate::{Bytes, Chunk, ChunkArc, ChunkMeta};
use dashmap::DashMap;
use std::{collections::HashMap, sync::Arc};

/// writing_list 裡的一筆：持有新版本 Chunk 的一份引用，讓位置在 commit 或放棄前不被回收。
pub struct WritingHolder {
    pub chunk: Chunk,
    /// true = 寫入方已放棄（WritingChunk 未成功 commit 就被 drop），或是重啟時恢復的紀錄；
    /// 同 chunk_id 的新 update 可以覆蓋它，false 時則會被拒絕（chunk is in writing）
    pub abort: bool,
}

/// prefix (chain) -> chunk_id -> 寫入中的版本。外層 DashMap 依 prefix 分片上鎖。
pub type WritingList = DashMap<Bytes, HashMap<Bytes, WritingHolder>>;

pub struct WritingChunk {
    pub chunk_id: Bytes,
    /// 新版本；meta 已帶新的 chunk_ver / chain_ver / etag，uncommitted = true
    pub chunk: Chunk,
    /// 指回 Engine 的 writing_list，Drop 時要回頭清理
    pub list: Arc<WritingList>,
    pub prefix_len: u32,
    pub is_remove: bool,
    /// 由 Engine 在 RocksDB 提交成功後設為 true，決定 Drop 的行為
    pub commit_succ: bool,
}

impl WritingChunk {
    pub fn meta(&self) -> &ChunkMeta {
        self.chunk.meta()
    }

    pub fn set_committed(&mut self) {
        self.chunk.set_committed();
    }

    pub fn commit_succ(&mut self) {
        self.commit_succ = true;
    }
}

// RAII 收尾：不管 commit 成功、失敗或 C++ 直接釋放，writing_list 都會被正確更新。
// 找不到自己的紀錄代表不變式被破壞（同一 chunk 有兩個 WritingChunk），直接 panic。
impl Drop for WritingChunk {
    fn drop(&mut self) {
        let prefix = &self.chunk_id[..self.prefix_len as usize];
        // get_mut 會鎖住這個 prefix 的分片，直到 map 離開作用域
        if let Some(mut map) = self.list.get_mut(prefix) {
            if self.commit_succ {
                // 已提交：移除紀錄，holder 裡的 Chunk 隨之 drop（rc - 1）；
                // 新版本仍由 meta_cache 持有，所以位置不會被釋放
                if map.remove(&self.chunk_id).is_some() {
                    return;
                }
            } else if let Some(holder) = map.get_mut(&self.chunk_id) {
                // 未提交：保留紀錄（RocksDB 的 0x09 寫入紀錄也還在），只標記放棄
                holder.abort = true;
                return;
            }
        }
        panic!("chunk id {:?} is not in the writing list!", self.chunk_id);
    }
}

// commit 時用來產生放進 meta_cache 的 Arc<Chunk>；clone 會讓位置 rc + 1
impl From<&WritingChunk> for ChunkArc {
    fn from(chunk: &WritingChunk) -> Self {
        Arc::new(chunk.chunk.clone())
    }
}

impl std::fmt::Debug for WritingChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WritingChunk")
            .field("chunk_id", &self.chunk_id)
            .field("chunk", &self.chunk)
            .field("is_remove", &self.is_remove)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::*;
    use std::sync::Arc;

    // 共用情境：造一個沒有登記在 writing_list 的 WritingChunk，函式結束 drop 時必定 panic。
    // 三個測試分別涵蓋：prefix 桶不存在、桶存在但沒有此 chunk（未提交 / 已提交）
    fn test_writing_chunk_not_in_list(has_list: bool, commit_succ: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let meta_config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: path.join("meta"),
                create: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let meta_store = Arc::new(MetaStore::open(&meta_config).unwrap());
        let allocators = Allocators::new(path, true, meta_store).unwrap();
        let chunk = allocators.allocate(CHUNK_SIZE_NORMAL, true).unwrap();

        let writing_list: Arc<WritingList> = Default::default();
        if has_list {
            writing_list
                .entry(Bytes::from(b"te".as_slice()))
                .or_default();
        }
        let writing_chunk = WritingChunk {
            chunk_id: b"test".as_ref().into(),
            chunk,
            list: writing_list.clone(),
            prefix_len: 2,
            is_remove: false,
            commit_succ,
        };
        println!("{:#?}", writing_chunk);
    }

    #[test]
    #[should_panic(expected = "chunk id [116, 101, 115, 116] is not in the writing list!")]
    fn test_writing_chunk_not_in_list_1() {
        test_writing_chunk_not_in_list(false, false);
    }

    #[test]
    #[should_panic(expected = "chunk id [116, 101, 115, 116] is not in the writing list!")]
    fn test_writing_chunk_not_in_list_2() {
        test_writing_chunk_not_in_list(true, false);
    }

    #[test]
    #[should_panic(expected = "chunk id [116, 101, 115, 116] is not in the writing list!")]
    fn test_writing_chunk_not_in_list_3() {
        test_writing_chunk_not_in_list(true, true);
    }
}
