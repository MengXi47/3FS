//! chunk 元資料的業務操作層：把每個 chunk 事件翻成一個原子的 WriteBatch，並負責崩潰復原
//! 需要的寫入中紀錄、每個 chain 的容量統計與元資料格式版本。key 格式見 `meta/meta_key.rs`。
//!
//! ```text
//!   WriteBatch contents per chunk event
//!
//!   key                  add_chunk_mut       move_chunk_mut                remove_mut
//!   -------------------  ------------------  ----------------------------  --------------
//!   0x01 chunk meta      put new             put new                       delete
//!   0x03 pos -> chunk    put new pos         delete old, put new     (*)   delete pos
//!   0x02 group bits      merge acquire       merge release old,      (*)   merge release
//!                                            merge acquire new
//!   0x04 used size       merge +size         merge new - old size    (*)   merge -size
//!   0x06 timestamp       put (value = id)    put new (value empty),        delete
//!                                            delete old
//!   0x09 writing record  delete              delete                        delete
//!
//!   (*) only when old_meta.pos != new_meta.pos (copy-on-write or compaction move);
//!       an in-place append keeps the same Position and skips these keys
//! ```
//!
//! ```text
//!   writing record (0x09) and crash recovery
//!
//!   Engine::update_chunk                       Engine::commit_chunk / commit_chunks
//!     data already written at its Position       one WriteBatch: add / move / remove_mut
//!     persist_writing_chunk (put 0x09, sync)     (the batch also deletes 0x09)
//!            |                                              |
//!            +------------ crash in between ----------------+
//!                                 |
//!                                 v
//!   Engine::open
//!     1. occupy_uncommitted_positions   scan 0x09; for each record whose Position is not
//!                                       the committed one: merge acquire + put 0x03 (sync)
//!     2. Allocators::new                ChunkAllocator::load sees those bits as used
//!     3. (Engine) re-reference the positions in writing_list with abort = true
//!     4. vacate_uncommitted_positions   undo step 1: merge release + delete 0x03 (sync)
//!     later: C++ handle_uncommitted_raw_chunks -> Engine::commit_chunks
//! ```
//!
//! ```text
//!   used size per chain (0x04), prefix = chunk_id[..prefix_len]
//!
//!   MetaStore::open -> update_used_size_if_need
//!     stored prefix_len (0x05) == config.prefix_len ?  -> nothing to do
//!     otherwise: scan all 0x01, sum chunk_size by chunk_id[..prefix_len],
//!                put every 0x04 total and 0x05 = prefix_len   (one batch, sync)
//!   afterwards: every add / move / remove merges a signed delta into 0x04
//! ```

use std::{cell::RefCell, collections::HashMap, ops::DerefMut};

use super::super::*;
use byteorder::{ByteOrder, LittleEndian};
use derse::{Deserialize, DownwardBytes, Serialize};

/// `prefix_len`：C++ 傳入的 chunk_id 前幾個 byte 用來識別 chain，容量統計 (0x04) 與時間索引
/// (0x06) 都以這段前綴分組。
#[derive(Debug, Default, Clone)]
pub struct MetaStoreConfig {
    pub rocksdb: RocksDBConfig,
    pub prefix_len: usize,
}

/// 整個 engine 共用一個 (`Arc<MetaStore>`)，同時由 `Engine` 與 `Allocators` 持有。
pub struct MetaStore {
    rocksdb: RocksDB,
    config: MetaStoreConfig,
}

impl MetaStore {
    // 每條執行緒一個序列化緩衝區 (derse 的 DownwardBytes，從尾端往前寫)，
    // 避免每次序列化都配置記憶體；用法見 with_tls_bytes
    thread_local! {
        static BYTES: RefCell<DownwardBytes> = RefCell::new(DownwardBytes::with_capacity(Size::MB.into()));
    }

    /// 開啟 RocksDB 並掛上 `MetaMergeOp`；若 prefix_len 設定變了，順便重算容量統計。
    pub fn open(config: &MetaStoreConfig) -> Result<Self> {
        let rocksdb = RocksDB::open::<MetaMergeOp>(&config.rocksdb)?;

        let mut this = MetaStore {
            rocksdb,
            config: config.clone(),
        };

        this.update_used_size_if_need()?;

        Ok(this)
    }

    pub fn get_chunk_meta(&self, chunk_id: &[u8]) -> Result<Option<ChunkMeta>> {
        let chunk_meta_key = MetaKey::chunk_meta_key(chunk_id);
        let value = self.rocksdb.get(chunk_meta_key)?;

        if let Some(value) = value {
            Ok(Some(
                ChunkMeta::deserialize(value.as_ref()).map_err(Error::SerializationError)?,
            ))
        } else {
            Ok(None)
        }
    }

    /// 列出 chunk_id 落在 `[begin, end)` 的 chunk，依 chunk_id 由大到小，最多 `max_count` 筆。
    pub fn query_chunks(
        &self,
        begin: impl AsRef<[u8]>,
        end: impl AsRef<[u8]>,
        max_count: u64,
    ) -> Result<Vec<(Bytes, ChunkMeta)>> {
        let it = self.iterator();
        self.query_chunks_from_iterator(it, begin, end, max_count)
    }

    /// 同 `query_chunks`，但用呼叫端給的 iterator (也就是它建立當下的讀取視圖)。
    ///
    /// 0x01 的 key 是 chunk_id 取反，key 遞增等於 chunk_id 遞減，所以從 `end` 往 `begin` 走：
    ///
    /// ```text
    ///   chunk_id axis :   begin ................................ end
    ///   key axis      :   larger key  <------------------------  seek(~end)
    ///   iteration     :   stop when chunk_id < begin   <-------  start (end itself skipped)
    /// ```
    ///
    /// 這個順序推論以等長 chunk_id 為前提；取反後長短不一的 id 不保證完全反序。
    pub fn query_chunks_from_iterator(
        &self,
        mut it: RocksDBIterator,
        begin: impl AsRef<[u8]>,
        end: impl AsRef<[u8]>,
        max_count: u64,
    ) -> Result<Vec<(Bytes, ChunkMeta)>> {
        let mut out = Vec::<(Bytes, ChunkMeta)>::with_capacity(4096);

        // 1. seek 到第一個 key >= ~end，也就是第一個 chunk_id <= end 的 chunk；剛好等於 end 就跳過
        let end_key = MetaKey::chunk_meta_key(end.as_ref());
        it.seek(&end_key)?;

        if it.key() == Some(end_key.as_ref()) {
            it.next(); // [begin, end)
        }

        // 2. 正向走 (chunk_id 遞減)，離開 0x01 表或 chunk_id < begin 就停
        for _ in 0..max_count {
            if !it.valid() {
                break;
            }

            if it.key().unwrap()[0] != MetaKey::CHUNK_META_KEY_PREFIX {
                break;
            }

            let chunk_id = MetaKey::parse_chunk_meta_key(it.key().unwrap());
            if begin.as_ref() <= chunk_id.as_ref() {
                let chunk_meta = ChunkMeta::deserialize(it.value().unwrap())
                    .map_err(Error::SerializationError)?;
                out.push((chunk_id, chunk_meta))
            } else {
                break;
            }

            it.next();
        }

        Ok(out)
    }

    /// 列出某個 prefix (chain) 中 timestamp 落在 `[begin, end)` 的 chunk_id，依時間由舊到新。
    ///
    /// ```text
    ///   06 | prefix | ts=begin ...                        ts=end
    ///        ^ seek(timestamp_key_filter(prefix, begin))   ^ stop (timestamp >= end)
    /// ```
    ///
    /// `prefix` 長度必須等於設定的 prefix_len，這裡沒有檢查；長度不符時比對會立刻失敗，
    /// 回傳空結果。chunk_id 從 key 拆出來，不讀 value (add 與 move 寫入的 value 並不一致)。
    pub fn query_chunks_by_timestamp(
        &self,
        prefix: &[u8],
        begin: u64,
        end: u64,
        max_count: u64,
    ) -> Result<Vec<Bytes>> {
        let mut it = self.iterator();
        let mut out = Vec::<Bytes>::with_capacity(4096);

        let begin_key = MetaKey::timestamp_key_filter(prefix, begin);
        it.seek(&begin_key)?;

        // 離開 0x06 表、離開這個 prefix、或 timestamp >= end 就停
        for _ in 0..max_count {
            if !it.valid() {
                break;
            }

            let key = it.key().unwrap();
            if key[0] != MetaKey::TIMESTAMP_KEY_PREFIX {
                break;
            }
            if key.len() <= prefix.len() || &key[1..1 + self.config.prefix_len] != prefix {
                break;
            }

            let (timestamp, chunk_id) = MetaKey::parse_timestamp_key(key, self.config.prefix_len)?;
            if timestamp < end {
                out.push(chunk_id)
            } else {
                break;
            }

            it.next();
        }

        Ok(out)
    }

    #[inline(always)]
    pub fn write(&self, write_batch: rocksdb::WriteBatch, sync: bool) -> Result<()> {
        self.rocksdb.write(write_batch, sync)
    }

    // add / move / remove 都有兩個版本：`xxx` 自建 WriteBatch 並立即寫入；`xxx_mut` 只把操作
    // 加進呼叫端的 batch，讓 Engine::commit_chunks、batch_remove 把多個 chunk 合成一次原子寫入

    pub fn add_chunk(&self, chunk_id: &[u8], chunk_meta: &ChunkMeta, sync: bool) -> Result<()> {
        let mut write_batch = RocksDB::new_write_batch();
        self.add_chunk_mut(chunk_id, chunk_meta, &mut write_batch)?;
        self.write(write_batch, sync)
    }

    /// 新 chunk 提交：chunk_id 之前不存在 (Engine::commit_chunk 在 meta_cache 與 RocksDB
    /// 都查不到舊 chunk 時呼叫)。
    pub fn add_chunk_mut(
        &self,
        chunk_id: &[u8],
        chunk_meta: &ChunkMeta,
        write_batch: &mut rocksdb::WriteBatch,
    ) -> Result<()> {
        // 1. add chunk meta.
        // 用 thread-local 緩衝區序列化；WriteBatch::put 會複製 bytes，所以緩衝區之後可以重用
        let chunk_meta_key = MetaKey::chunk_meta_key(chunk_id);
        Self::with_tls_bytes(|bytes| {
            chunk_meta
                .serialize_to(bytes)
                .map_err(Error::SerializationError)?;
            write_batch.put(chunk_meta_key, &bytes[..]);
            Ok(())
        })?;

        // 2. add pos->chunk map.
        // 反查表，compaction 要靠它知道某個 group 裡有哪些 chunk
        let pos_to_chunk_key = MetaKey::pos_to_chunk_key(chunk_meta.pos);
        write_batch.put(pos_to_chunk_key, chunk_id);

        // 3. update group bits.
        // 記憶體端早在 Allocator::allocate 就設了 bit；RocksDB 端到提交這一刻才記錄佔用，
        // 所以 commit 前崩潰，這個位置在重啟後自然是空的
        let group_bits_key = MetaKey::group_bits_key(chunk_meta.pos.group_id());
        Self::with_tls_bytes(|bytes| {
            MergeState::acquire(chunk_meta.pos.index())
                .serialize_to(bytes)
                .map_err(Error::SerializationError)?;
            write_batch.merge(group_bits_key, &bytes[..]);
            Ok(())
        })?;

        // 4. update used size.
        // 以 size class 大小 (pos.chunk_size) 計，不是資料長度；這裡也會檢查 chunk_id 長度，
        // 下一步切 chunk_id[..prefix_len] 才不會 panic
        self.update_used_size(chunk_id, chunk_meta.pos.chunk_size().0 as i64, write_batch)?;

        // 5. add timestamp->chunk map.
        let timestamp_key =
            MetaKey::timestamp_key(chunk_meta.timestamp, chunk_id, self.config.prefix_len);
        write_batch.put(timestamp_key, chunk_id);

        // 6. remove writing chunk log.
        // 與上面的元資料在同一個 batch：提交生效與清掉寫入中紀錄是原子的
        self.remove_writing_chunk_mut(chunk_id, write_batch);

        Ok(())
    }

    pub fn move_chunk(
        &self,
        chunk_id: &[u8],
        old_meta: &ChunkMeta,
        new_meta: &ChunkMeta,
        sync: bool,
    ) -> Result<()> {
        let mut write_batch = RocksDB::new_write_batch();
        self.move_chunk_mut(chunk_id, old_meta, new_meta, &mut write_batch)?;
        self.write(write_batch, sync)
    }

    /// 既有 chunk 換成新版本。兩種情境：
    ///
    /// ```text
    ///   copy-on-write / compaction move     in-place append
    ///   old.pos = P1, new.pos = P2           old.pos = new.pos = P1
    ///   -> rewrite meta, move 0x03,          -> rewrite meta and 0x06 only;
    ///      release P1 / acquire P2,             group bits and used size unchanged
    ///      used size += size(P2) - size(P1)
    /// ```
    ///
    /// RocksDB 這邊 P1 立刻被釋放，但記憶體裡 P1 的 bit 要等最後一個 `Arc<Chunk>` drop
    /// 才清掉 (`ChunkAllocator::dereference`)，所以讀到一半的讀取方不受影響。
    pub fn move_chunk_mut(
        &self,
        chunk_id: &[u8],
        old_meta: &ChunkMeta,
        new_meta: &ChunkMeta,
        write_batch: &mut rocksdb::WriteBatch,
    ) -> Result<()> {
        // 1. change chunk meta.
        let chunk_meta_key = MetaKey::chunk_meta_key(chunk_id);
        Self::with_tls_bytes(|bytes| {
            new_meta
                .serialize_to(bytes)
                .map_err(Error::SerializationError)?;
            write_batch.put(chunk_meta_key, bytes.as_slice());
            Ok(())
        })?;

        // 位置沒變 (原地追加) 就跳過 2-4：同一個 bit 不需要先釋放再佔用
        if old_meta.pos != new_meta.pos {
            // 2. remove old pos->chunk map.
            let old_pos = old_meta.pos;
            let pos_to_chunk_key = MetaKey::pos_to_chunk_key(old_pos);
            write_batch.delete(pos_to_chunk_key);

            let group_bits_key = MetaKey::group_bits_key(old_pos.group_id());
            Self::with_tls_bytes(|bytes| {
                MergeState::release(old_pos.index())
                    .serialize_to(bytes)
                    .map_err(Error::SerializationError)?;
                write_batch.merge(group_bits_key, &bytes[..]);
                Ok(())
            })?;

            // 3. add new pos->chunk map.
            let pos_to_chunk_key = MetaKey::pos_to_chunk_key(new_meta.pos);
            write_batch.put(pos_to_chunk_key, chunk_id);

            let group_bits_key = MetaKey::group_bits_key(new_meta.pos.group_id());
            Self::with_tls_bytes(|bytes| {
                MergeState::acquire(new_meta.pos.index())
                    .serialize_to(bytes)
                    .map_err(Error::SerializationError)?;
                write_batch.merge(group_bits_key, &bytes[..]);
                Ok(())
            })?;

            // 4. update used size.
            // 只記差值；copy-on-write 可能換 size class (例如寫超過容量時換到更大的 chunk)
            self.update_used_size(
                chunk_id,
                new_meta.pos.chunk_size().0 as i64 - old_pos.chunk_size().0 as i64,
                write_batch,
            )?;
        }

        // 5. update timestamp->chunk map.
        // 位置沒變時不會經過 update_used_size，所以在這裡另外檢查 chunk_id 長度。
        // 先 put 新 key 再 delete 舊 key：若新舊 timestamp 相同 (Engine::move_chunk 經
        // Chunk::copy_chunk 會沿用舊 timestamp)，兩者是同一個 key，batch 內後者生效，
        // 結果是這個 chunk 的時間索引被刪掉。value 寫空，與 add_chunk_mut 寫 chunk_id 不同
        self.check_chunk_id(chunk_id)?;
        let timestamp_key =
            MetaKey::timestamp_key(new_meta.timestamp, chunk_id, self.config.prefix_len);
        write_batch.put(timestamp_key, []);
        let timestamp_key =
            MetaKey::timestamp_key(old_meta.timestamp, chunk_id, self.config.prefix_len);
        write_batch.delete(timestamp_key);

        // 6. remove writing chunk log.
        self.remove_writing_chunk_mut(chunk_id, write_batch);

        Ok(())
    }

    pub fn remove(&self, chunk_id: &[u8], chunk_meta: &ChunkMeta, sync: bool) -> Result<()> {
        let mut write_batch = RocksDB::new_write_batch();
        self.remove_mut(chunk_id, chunk_meta, &mut write_batch)?;
        self.write(write_batch, sync)
    }

    /// 刪除 chunk。`chunk_meta` 必須是目前已提交的版本：要靠它的 pos 與 timestamp
    /// 算出 0x03、0x02、0x06 要動哪些 key。
    pub fn remove_mut(
        &self,
        chunk_id: &[u8],
        chunk_meta: &ChunkMeta,
        write_batch: &mut rocksdb::WriteBatch,
    ) -> Result<()> {
        // 1. delete chunk meta.
        let chunk_meta_key = MetaKey::chunk_meta_key(chunk_id);
        write_batch.delete(chunk_meta_key);

        // 2. delete pos->chunk map.
        let pos_to_chunk_key = MetaKey::pos_to_chunk_key(chunk_meta.pos);
        write_batch.delete(pos_to_chunk_key);

        // 3. release position.
        let group_bits_key = MetaKey::group_bits_key(chunk_meta.pos.group_id());
        Self::with_tls_bytes(|bytes| {
            MergeState::release(chunk_meta.pos.index())
                .serialize_to(bytes)
                .map_err(Error::SerializationError)?;
            write_batch.merge(group_bits_key, &bytes[..]);
            Ok(())
        })?;

        // 4. update used size.
        self.update_used_size(
            chunk_id,
            -(chunk_meta.pos.chunk_size().0 as i64),
            write_batch,
        )?;

        // 5. delete timestamp->chunk map.
        let timestamp_key =
            MetaKey::timestamp_key(chunk_meta.timestamp, chunk_id, self.config.prefix_len);
        write_batch.delete(timestamp_key);

        // 6. remove writing chunk log.
        self.remove_writing_chunk_mut(chunk_id, write_batch);

        Ok(())
    }

    /// 把 group 登記成「已 fallocate、目前全空」：寫入全 0 的 bits。下次啟動時
    /// `ChunkAllocator::load` 會把它放回 allocated 預備池。由 `Allocator::do_allocate_task`
    /// 在 fallocate 成功之後呼叫，sync 寫入。
    pub fn allocate_group(&self, group_id: GroupId) -> Result<()> {
        let group_bits_key = MetaKey::group_bits_key(group_id);
        self.rocksdb
            .put(group_bits_key, GroupState::empty().as_bytes(), true)
    }

    /// 註銷 group：刪掉 bits key，下次啟動時它會落在 load 的「缺號」裡，變回 unallocated。
    /// `Allocator::do_allocate_task` 先呼叫這裡、成功後才 punch hole，所以中途崩潰只會讓空間
    /// 暫時沒回收，不會出現 key 還在、檔案卻已經打洞的狀態。
    pub fn remove_group(&self, group_id: GroupId) -> Result<()> {
        let group_bits_key = MetaKey::group_bits_key(group_id);
        self.rocksdb.delete(group_bits_key, true)
    }

    pub fn iterator(&self) -> RocksDBIterator {
        self.rocksdb.new_iterator()
    }

    /// 對 chunk_id 所屬 prefix 的容量 key 送一個有號增量 (merge，不讀舊值)。
    fn update_used_size(
        &self,
        chunk_id: &[u8],
        diff: i64,
        write_batch: &mut rocksdb::WriteBatch,
    ) -> Result<()> {
        self.check_chunk_id(chunk_id)?;
        let used_size_key = MetaKey::used_size_key(&chunk_id[..self.config.prefix_len]);
        write_batch.merge(used_size_key, diff.to_le_bytes());
        Ok(())
    }

    /// 寫入中紀錄 (0x09)：`Engine::update_chunk` 把資料寫到磁碟後呼叫，記下「這個 chunk_id
    /// 有一筆未提交的新版本，資料在 chunk_meta.pos」。sync 寫入，崩潰後一定看得到；
    /// 提交時由 add / move / remove_mut 在同一個 batch 裡刪掉。
    pub fn persist_writing_chunk(&self, chunk_id: &[u8], chunk_meta: &ChunkMeta) -> Result<()> {
        let chunk_meta_key = MetaKey::writing_chunk_key(chunk_id);
        Self::with_tls_bytes(|bytes| {
            chunk_meta
                .serialize_to(bytes)
                .map_err(Error::SerializationError)?;
            self.rocksdb.put(chunk_meta_key, &bytes[..], true)
        })
    }

    /// 單獨刪除寫入中紀錄；用在「刪除一個根本不存在的 chunk」這種沒有元資料要改的提交。
    pub fn remove_writing_chunk(&self, chunk_id: &[u8], sync: bool) -> Result<()> {
        let chunk_meta_key = MetaKey::writing_chunk_key(chunk_id);
        self.rocksdb.delete(chunk_meta_key, sync)
    }

    pub fn remove_writing_chunk_mut(&self, chunk_id: &[u8], write_batch: &mut rocksdb::WriteBatch) {
        write_batch.delete(MetaKey::writing_chunk_key(chunk_id));
    }

    /// 崩潰復原第一步 (只在 `Engine::open` 建 Allocators 之前呼叫)：讓未提交寫入用到的位置
    /// 在 RocksDB 裡暫時顯示為「已佔用」，`ChunkAllocator::load` 才不會把它們當空位配出去。
    ///
    /// 回傳 `(chunk_id, 寫入中的 meta, occupied)`；`occupied` 表示這裡有暫時佔位，之後要交給
    /// `vacate_uncommitted_positions` 撤銷。暫佔與撤銷都是冪等的 bit 操作，兩步之間再崩潰，
    /// 下次啟動重做一次即可。
    pub fn occupy_uncommitted_positions(&mut self) -> Result<Vec<(Bytes, ChunkMeta, bool)>> {
        // 1. 掃出所有寫入中紀錄。query_uncommitted_chunks 要求 prefix 長度等於設定值，
        //    這裡暫時把 prefix_len 換成 0，用空 prefix 一次掃完，再換回來
        let mut prefix_len = 0;
        std::mem::swap(&mut self.config.prefix_len, &mut prefix_len);
        let list = self.query_uncommitted_chunks(&[])?;
        std::mem::swap(&mut self.config.prefix_len, &mut prefix_len);

        // 2. 逐筆判斷是否需要暫佔
        //
        //    committed meta (0x01)      writing record (0x09)     action
        //    -------------------------  ------------------------  ---------------------------
        //    exists, same pos           (in-place append, or      none: bit already acquired
        //                                remove keeps same pos)    by the committed chunk
        //    missing, or different pos  (new chunk, or COW to a   merge acquire + put 0x03
        //                                new position)            occupied = true
        let mut uncommitted_chunks = vec![];
        let mut write_batch = RocksDB::new_write_batch();
        let mut count = 0;
        for (chunk_id, writing_meta) in list {
            let pos = writing_meta.pos;
            match self.get_chunk_meta(&chunk_id)? {
                Some(meta) if meta.pos == writing_meta.pos => {
                    uncommitted_chunks.push((chunk_id, writing_meta, false));
                }
                _ => {
                    uncommitted_chunks.push((chunk_id.clone(), writing_meta, true));

                    count += 1;
                    let pos_to_chunk_key = MetaKey::pos_to_chunk_key(pos);
                    write_batch.put(pos_to_chunk_key, chunk_id);

                    let group_bits_key = MetaKey::group_bits_key(pos.group_id());
                    Self::with_tls_bytes(|bytes| {
                        MergeState::acquire(pos.index())
                            .serialize_to(bytes)
                            .map_err(Error::SerializationError)?;
                        write_batch.merge(group_bits_key, &bytes[..]);
                        Ok(())
                    })?;
                }
            }
        }
        // 3. 一次 sync 寫入所有暫佔
        if !uncommitted_chunks.is_empty() {
            self.write(write_batch, true)?;
            tracing::info!("occupy {} positions for writing chunks", count);
        }
        Ok(uncommitted_chunks)
    }

    /// 崩潰復原最後一步：Allocators 已載入、Engine 也已在記憶體裡引用這些位置之後，
    /// 撤銷 `occupy_uncommitted_positions` 的暫佔 (merge release + 刪 0x03)。
    ///
    /// 撤銷後 RocksDB 又顯示這些位置是空的，但記憶體裡的 bit 仍是 1，且 writing_list 握著引用，
    /// 所以不會被重新配出去；等之後提交，才由 add / move_chunk_mut 正式 acquire。
    pub fn vacate_uncommitted_positions(
        &self,
        uncommitted_chunks: Vec<(Bytes, ChunkMeta, bool)>,
    ) -> Result<()> {
        let mut write_batch = RocksDB::new_write_batch();
        let mut count = 0;
        for (_, chunk_meta, occupied) in uncommitted_chunks {
            if !occupied {
                continue;
            }

            count += 1;
            let pos_to_chunk_key = MetaKey::pos_to_chunk_key(chunk_meta.pos);
            write_batch.delete(pos_to_chunk_key);

            let group_bits_key = MetaKey::group_bits_key(chunk_meta.pos.group_id());
            Self::with_tls_bytes(|bytes| {
                MergeState::release(chunk_meta.pos.index())
                    .serialize_to(bytes)
                    .map_err(Error::SerializationError)?;
                write_batch.merge(group_bits_key, &bytes[..]);
                Ok(())
            })?;
        }
        self.write(write_batch, true)?;
        tracing::info!("vacate {} positions for writing chunks", count);
        Ok(())
    }

    /// 掃 0x09 寫入中紀錄。唯一呼叫點 (`occupy_uncommitted_positions`) 傳空 prefix，
    /// 於是拿到全部紀錄。
    ///
    /// 注意：傳非空 prefix 時，這裡回傳的是「chunk_id >= prefix 的所有紀錄」直到 0x09 表結束，
    /// 而不是「以 prefix 開頭的紀錄」；chunk_id 剛好等於 prefix 的那筆還會被跳過
    /// (沿用 query_chunks 的 `[begin, end)` 寫法)。
    fn query_uncommitted_chunks(&self, prefix: &[u8]) -> Result<Vec<(Bytes, ChunkMeta)>> {
        self.check_prefix(prefix)?;

        let mut it = self.iterator();
        let mut out = Vec::<(Bytes, ChunkMeta)>::with_capacity(4096);

        let end_key = MetaKey::writing_chunk_key(prefix);
        it.seek(&end_key)?;

        if it.key() == Some(end_key.as_ref()) {
            it.next(); // [begin, end)
        }

        loop {
            if !it.valid() {
                break;
            }

            if it.key().unwrap()[0] != MetaKey::WRITING_CHUNK_KEY_PREFIX {
                break;
            }

            let chunk_id = MetaKey::parse_writing_chunk_key(it.key().unwrap())?;
            if prefix <= chunk_id.as_ref() {
                let chunk_meta = ChunkMeta::deserialize(it.value().unwrap())
                    .map_err(Error::SerializationError)?;
                out.push((chunk_id, chunk_meta))
            } else {
                break;
            }

            it.next();
        }

        Ok(out)
    }

    fn check_chunk_id(&self, chunk_id: &[u8]) -> Result<()> {
        let prefix_len = self.config.prefix_len;
        if chunk_id.len() < prefix_len {
            return Err(Error::InvalidArg(format!(
                "chunk_id.len() < prefix len: {:?}, {}",
                chunk_id, prefix_len
            )));
        }
        Ok(())
    }

    fn check_prefix(&self, prefix: &[u8]) -> Result<()> {
        let prefix_len = self.config.prefix_len;
        if prefix.len() != prefix_len {
            return Err(Error::InvalidArg(format!(
                "prefix.len() != prefix len: {:?}, {}",
                prefix, prefix_len
            )));
        }
        Ok(())
    }

    /// 某個 prefix (chain) 目前佔用的容量，單位 byte，按 size class 大小累計。
    /// key 不存在代表從沒寫過，回 0。值是 merge 累加後的 i64，這裡當 u64 讀 (正常情況下非負)。
    pub fn query_used_size(&self, prefix: &[u8]) -> Result<u64> {
        self.check_prefix(prefix)?;

        let used_size_key = MetaKey::used_size_key(prefix);
        let value = self.rocksdb.get(used_size_key)?;
        if let Some(size) = value {
            if size.len() != std::mem::size_of::<u64>() {
                Err(Error::InvalidArg(format!(
                    "invalid size length: {:?}",
                    size.as_ref()
                )))
            } else {
                Ok(LittleEndian::read_u64(size.as_ref()))
            }
        } else {
            Ok(0)
        }
    }

    /// 借用這條執行緒的序列化緩衝區執行 `f`，結束後清空並把容量縮回 1MiB，
    /// 避免偶發的大物件讓緩衝區一直佔著記憶體。
    ///
    /// `f` 裡拿到的 bytes 只在閉包內有效：呼叫端都在閉包裡把它交給 WriteBatch (會複製)
    /// 或同步的 `rocksdb.put`。不可巢狀呼叫，否則 RefCell 的 `borrow_mut` 會 panic。
    fn with_tls_bytes<F, R>(f: F) -> R
    where
        F: FnOnce(&mut DownwardBytes) -> R,
    {
        Self::BYTES.with(|v| {
            let mut bytes = v.borrow_mut();
            let result = f(bytes.deref_mut());
            bytes.clear_and_shrink_to(Size::MB.into());
            result
        })
    }

    /// prefix_len 設定與上次統計時不同，就全量重算每個 prefix 的容量 (只在 `open` 呼叫)。
    ///
    /// 舊 prefix_len 留下的 0x04 key 不會被刪除。若之後切回同一個 prefix_len，而某個 prefix
    /// 已經沒有任何 chunk，重算不會覆寫它，它的舊值會殘留下來。
    fn update_used_size_if_need(&mut self) -> Result<()> {
        // 1. 讀上次統計用的 prefix_len (0x05，u32 LE)；全新的 DB 視為 0
        let old_len = match self.rocksdb.get(MetaKey::used_size_prefix_len_key())? {
            Some(value) => {
                if value.len() != std::mem::size_of::<u32>() {
                    return Err(Error::InvalidArg(format!(
                        "invalid used size prefix length: {:?}",
                        value.as_ref()
                    )));
                }
                LittleEndian::read_u32(value.as_ref()) as usize
            }
            None => 0,
        };

        // 2. 沒變就不用重算
        let prefix_len = self.config.prefix_len;
        if old_len == prefix_len {
            return Ok(());
        }

        // 3. 全表掃 0x01，依新的 prefix 加總 size class 大小。
        //    resize 會截斷過長的 chunk_id，也會用 0 補齊過短的；prefix_len == 0 時先放一筆 0，
        //    確保就算沒有任何 chunk，空 prefix 的 key 也會被寫入
        let mut map = HashMap::<Bytes, u64>::new();
        if prefix_len == 0 {
            map.insert(Bytes::new(), 0);
        }
        let mut it = self.iterator();
        it.iterate(MetaKey::chunk_meta_key_prefix(), |key, value| {
            let mut chunk_id = MetaKey::parse_chunk_meta_key(key);
            chunk_id.resize(prefix_len, 0);
            let chunk_meta = ChunkMeta::deserialize(value).map_err(Error::SerializationError)?;
            let chunk_size = chunk_meta.pos.chunk_size().0;
            map.entry(chunk_id)
                .and_modify(|v| *v += chunk_size)
                .or_insert(chunk_size);
            Ok(())
        })?;

        // 4. 新的 prefix_len 與所有總量放在同一個 batch，sync 寫入；用 put 覆蓋，不是 merge
        let mut write_batch = RocksDB::new_write_batch();
        write_batch.put(
            MetaKey::used_size_prefix_len_key(),
            (prefix_len as u32).to_le_bytes(),
        );
        for (prefix, size) in map {
            write_batch.put(MetaKey::used_size_key(&prefix), size.to_le_bytes())
        }
        self.write(write_batch, true)
    }

    // 元資料格式版本 (0x08)。沒有 key 視為版本 0。升級由 Engine::upgrade_version 執行：
    // 0 -> 1 (V1_FIX_TIMESTAMP) 會清空整張 0x06 時間索引但不重建，之後只有新寫入的 chunk
    // 才會出現在時間索引裡
    pub const V1_FIX_TIMESTAMP: u8 = 1;
    pub const LATEST_VERSION: u8 = Self::V1_FIX_TIMESTAMP;

    pub fn get_version(&self) -> Result<u8> {
        match self.rocksdb.get(MetaKey::version_key())? {
            Some(value) if !value.is_empty() => Ok(value[0]),
            Some(value) => Err(Error::InvalidArg(format!(
                "invalid version: {:?}",
                value.as_ref()
            ))),
            None => Ok(0),
        }
    }

    pub fn set_version(&self, version: u8) -> Result<()> {
        self.rocksdb.put(MetaKey::version_key(), [version], true)
    }

    /// 用 `delete_range([prefix], [prefix + 1])` 清掉整張表。0x01 到 0x05 是配置狀態與容量的
    /// 依據，整張刪掉會讓元資料不一致，所以直接拒絕；目前唯一用途是升級時清 0x06。
    pub fn remove_range_mut(
        &self,
        prefix: u8,
        write_batch: &mut rocksdb::WriteBatch,
    ) -> Result<()> {
        if prefix == MetaKey::CHUNK_META_KEY_PREFIX
            || prefix == MetaKey::GROUP_BITS_KEY_PREFIX
            || prefix == MetaKey::POS_TO_CHUNK_KEY_PREFIX
            || prefix == MetaKey::USED_SIZE_KEY_PREFIX
            || prefix == MetaKey::USED_SIZE_PREFIX_LEN_KEY
        {
            return Err(Error::InvalidArg(format!(
                "invalid remove range: {}",
                prefix
            )));
        }

        write_batch.delete_range(&[prefix], &[prefix + 1]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // add / get / query / remove 的基本流程；remove_range_mut 拒絕核心表；空的版本值報錯
    #[test]
    fn test_meta_store_normal() {
        let dir = tempfile::tempdir().unwrap();

        let config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: dir.path().into(),
                create: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let meta_store = MetaStore::open(&config).unwrap();

        let chunk_id = "1000".as_bytes();
        let chunk_meta = meta_store.get_chunk_meta(chunk_id).unwrap();
        assert!(chunk_meta.is_none());

        let chunk_meta_in = ChunkMeta {
            chunk_ver: 1,
            ..Default::default()
        };
        meta_store
            .add_chunk(chunk_id, &chunk_meta_in, false)
            .unwrap();

        let chunk_id = "1000".as_bytes();
        let chunk_meta_out = meta_store.get_chunk_meta(chunk_id).unwrap().unwrap();
        assert_eq!(chunk_meta_in, chunk_meta_out);
        assert_eq!(meta_store.query_chunks([], "100", 10).unwrap().len(), 1);

        meta_store.remove(chunk_id, &chunk_meta_out, false).unwrap();

        let mut write_batch = RocksDB::new_write_batch();
        meta_store
            .remove_range_mut(MetaKey::CHUNK_META_KEY_PREFIX, &mut write_batch)
            .unwrap_err();

        meta_store
            .rocksdb
            .put(MetaKey::version_key(), &[], false)
            .unwrap();
        meta_store.get_version().unwrap_err();
    }

    // query_chunks 的 [begin, end) 與遞減順序；多次 add 的 merge acquire 正確累積成 group bits
    #[test]
    fn test_meta_get_set() {
        let dir = tempfile::tempdir().unwrap();

        let config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: dir.path().into(),
                create: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let meta_store = MetaStore::open(&config).unwrap();

        let group_id = GroupId::default();
        let mut chunk_meta = ChunkMeta::default();
        for i in 0..128u32 {
            chunk_meta.pos = Position::new(group_id, 2 * i as u8);
            meta_store
                .add_chunk(&i.to_be_bytes(), &chunk_meta, false)
                .unwrap();
        }

        let vec = meta_store
            .query_chunks(10u32.to_be_bytes(), 20u32.to_be_bytes(), 30)
            .unwrap();
        assert_eq!(vec.len(), 10);
        assert_eq!(vec.first().unwrap().0.as_ref(), &19u32.to_be_bytes());
        assert_eq!(vec.last().unwrap().0.as_ref(), &10u32.to_be_bytes());

        let vec = meta_store
            .query_chunks(80u32.to_be_bytes(), 100u32.to_be_bytes(), 30)
            .unwrap();
        assert_eq!(vec.len(), 20);

        let mut it = meta_store.iterator();
        let mut count = 0;
        it.iterate(MetaKey::group_bits_key_prefix(), |_key, value| {
            count += 1;
            let bits = GroupState::from(value)?;
            assert_eq!(bits.count(), 128);
            for i in 0..128 {
                assert!(bits.check(i * 2));
                assert!(!bits.check(i * 2 + 1));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 1);

        for i in 0..128u32 {
            chunk_meta.pos = Position::new(group_id, 1 + 2 * i as u8);
            meta_store
                .add_chunk(&i.to_be_bytes(), &chunk_meta, false)
                .unwrap();
        }

        let mut it = meta_store.iterator();
        let mut count = 0;
        it.iterate(MetaKey::group_bits_key_prefix(), |_key, value| {
            count += 1;
            let bits = GroupState::from(value)?;
            assert_eq!(bits.count(), 256);
            assert!(bits.is_full());
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 1);
    }

    // 不可寫的路徑開啟失敗
    #[test]
    fn test_meta_store_open_failed() {
        let config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: "/proc/test".into(),
                create: true,
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(MetaStore::open(&config).is_err());
    }

    // prefix_len = 4 時的容量增減、過短 chunk_id / prefix 被拒、損壞的 0x04 / 0x05 值報錯
    #[test]
    fn test_meta_store_update_used_size() {
        let dir = tempfile::tempdir().unwrap();

        let config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: dir.path().into(),
                create: true,
                ..Default::default()
            },
            prefix_len: 4,
        };

        let meta_store = MetaStore::open(&config).unwrap();

        let chunk_id = [0, 1, 2, 3];
        let group_id = GroupId::default();
        let chunk_meta = ChunkMeta {
            pos: Position::new(group_id, 0_u8),
            ..Default::default()
        };
        meta_store
            .add_chunk(&chunk_id[..3], &chunk_meta, false)
            .unwrap_err();
        meta_store.add_chunk(&chunk_id, &chunk_meta, false).unwrap();

        meta_store.query_used_size(&chunk_id[..3]).unwrap_err();
        assert_eq!(
            meta_store.query_used_size(&chunk_id).unwrap(),
            CHUNK_SIZE_NORMAL
        );
        assert_eq!(meta_store.query_used_size(&0u32.to_le_bytes()).unwrap(), 0);

        meta_store
            .query_chunks_by_timestamp(&0u32.to_le_bytes(), 0, u64::MAX, u64::MAX)
            .unwrap();

        meta_store.remove(&chunk_id, &chunk_meta, false).unwrap();
        assert_eq!(meta_store.query_used_size(&chunk_id).unwrap(), 0);

        let key = MetaKey::used_size_key(&chunk_id);
        meta_store.rocksdb.put(key, [], false).unwrap();
        meta_store.query_used_size(&chunk_id).unwrap_err();

        meta_store
            .rocksdb
            .put(MetaKey::used_size_prefix_len_key(), [233], false)
            .unwrap();
        drop(meta_store);
        assert!(MetaStore::open(&config).is_err());
    }

    // 切換 prefix_len (0 -> 1 -> 0) 時重算容量；時間索引的範圍查詢
    #[test]
    fn test_meta_store_update_used_size_prefix_len() {
        let dir = tempfile::tempdir().unwrap();

        let mut config = MetaStoreConfig {
            rocksdb: RocksDBConfig {
                path: dir.path().into(),
                create: true,
                ..Default::default()
            },
            prefix_len: 0,
        };

        const N: u64 = 1024;
        let start = ChunkMeta::now();
        let meta_store = MetaStore::open(&config).unwrap();
        for i in 0..N {
            let chunk_id = i.to_le_bytes();
            let id = i as u8;
            let chunk_size = if id % 2 == 0 {
                CHUNK_SIZE_NORMAL
            } else {
                CHUNK_SIZE_SMALL
            };

            let pos = Position::new(GroupId::new(chunk_size, 0, 0), id);
            let meta = ChunkMeta {
                pos,
                ..Default::default()
            };
            meta_store.add_chunk(&chunk_id, &meta, false).unwrap();
        }

        let size = meta_store.query_used_size(&[]).unwrap();
        assert_eq!(size, N / 2 * (CHUNK_SIZE_NORMAL.0 + CHUNK_SIZE_SMALL.0));

        let mut write_batch = RocksDB::new_write_batch();
        write_batch.put("m", "m");
        meta_store.write(write_batch, false).unwrap();

        let end = ChunkMeta::now();
        let vec = meta_store
            .query_chunks_by_timestamp(&[], 0, start, u64::MAX)
            .unwrap();
        assert!(vec.is_empty());
        let vec = meta_store
            .query_chunks_by_timestamp(&[], start, end + 1, u64::MAX)
            .unwrap();
        assert_eq!(vec.len(), N as usize);

        drop(meta_store);

        config.prefix_len = 1;
        let meta_store = MetaStore::open(&config).unwrap();
        for i in 0..=u8::MAX {
            let size = meta_store.query_used_size(&[i]).unwrap();
            if i % 2 == 0 {
                assert_eq!(size, N / 256 * CHUNK_SIZE_NORMAL.0);
            } else {
                assert_eq!(size, N / 256 * CHUNK_SIZE_SMALL.0);
            }
        }

        for i in 0..N {
            let chunk_id = i.to_le_bytes();
            let meta = meta_store.get_chunk_meta(&chunk_id).unwrap().unwrap();
            meta_store.remove(&chunk_id, &meta, false).unwrap();
        }
        for i in 0..=u8::MAX {
            let size = meta_store.query_used_size(&[i]).unwrap();
            assert_eq!(size, 0);
        }

        drop(meta_store);
        config.prefix_len = 0;
        let meta_store = MetaStore::open(&config).unwrap();
        let size = meta_store.query_used_size(&[]).unwrap();
        assert_eq!(size, 0);
    }
}
