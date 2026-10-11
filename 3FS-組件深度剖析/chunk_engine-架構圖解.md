# chunk_engine 架構圖解

以圖為主的導讀版，搭配原始碼閱讀用；完整的設計取捨與缺陷分析見 [chunk_engine-Rust本地儲存引擎深度剖析.md](chunk_engine-Rust本地儲存引擎深度剖析.md)。

- 原始碼：`src/storage/chunk_engine/`（約 7400 行 Rust），下文路徑皆相對於 `src/storage/chunk_engine/src/`
- 圖內一律英文，說明在圖下方

> **README 與程式碼的出入**（以程式碼為準）
>
> - README 寫 chunk 大小只有 64KB / 512KB / 4MB 三種，程式碼是 11 級（`types/constants.rs:3-8`，64KiB–64MiB）。
> - README 寫有 `allocate_thread` / `compact_thread`，正式環境實際由 C++ 的 `src/storage/worker/AllocateWorker.cc:49-51` 驅動；Rust 自己的 `start_allocate_workers` 只在測試與 bench 使用。

---

## 1. 在 3FS 中的位置

```
+-------------------------------------------------------------+
|                  C++ storage service                        |
|  UpdateJob / BatchReadJob / CheckWorker / AllocateWorker    |
|                         |                                   |
|              store/ChunkEngine.{h,cc}  (C++ wrapper)        |
+-------------------------|-----------------------------------+
                          |  cxx bridge (src/cxx.rs)
                          |  get_raw_chunk / update_raw_chunk /
                          |  commit_raw_chunk / allocate_groups ...
+-------------------------v-----------------------------------+
|                 Rust: chunk_engine::Engine                  |
|                                                             |
|   +-------------+    +--------------+    +--------------+   |
|   |  MetaCache  |    |  Allocators  |    |  MetaStore   |   |
|   | (in-memory) |    | (in-memory)  |    |  (RocksDB)   |   |
|   +-------------+    +------+-------+    +------+-------+   |
+-----------------------------|-------------------|-----------+
                              v                   v
                    <path>/<size>/00..FF     <path>/meta/
                    (raw data files)         (RocksDB dir)
```

chunk_engine 只管「**單顆盤上的 chunk 怎麼存**」：CRAQ、chain、副本這些分散式邏輯全在 C++ 層，它完全不碰。

C++ 與它的唯一介面是 `cxx.rs` 的 FFI 橋接。讀取時 Rust 只交出 `fd + offset`（`alloc/chunk.rs:291` 的 `fd_and_offset()`），真正的 AIO 讀由 C++ 自己對 `direct_fd` 發（`src/storage/store/ChunkEngine.h:67`）。

---

## 2. 模組分層

```
              +-------------------------------------+
  layer 4     | core/engine.rs        cxx.rs (FFI)  |   orchestration
              +-------------------------------------+
                  |                 |
              +---v---------+   +---v-------------------------+
  layer 3     | alloc/      |   | meta/                       |
              |  Allocators |   |  MetaStore  (meta_store.rs) |
              |  Allocator  |   |  MetaKey    (meta_key.rs)   |
              |  ChunkAlloc |   |  MergeOp    (meta_merge.rs) |
              |  GroupAlloc |   |  RocksDB    (rocksdb.rs)    |
              |  Chunk      |   +-----------------------------+
              |  WritingChk |
              +------+------+
                     |
              +------v------+
  layer 2     | file/       |   Clusters -> Cluster (pread/pwrite/fallocate)
              +-------------+
  layer 1     | types/      |   GroupId, Position, ChunkMeta, GroupState, MergeState
  layer 0     | utils/      |   Size, Bytes, AlignedBuffer, ShardsMap/Set, Worker
              +-------------+
```

依賴方向由上往下：`types/` 不依賴任何其他模組，`file/` 只認得 `GroupId` 與 `Position`。

`alloc/` 與 `meta/` 幾乎互不呼叫，由 `core/engine.rs` 負責串接。唯一的例外是 `Allocator::do_allocate_task`（`alloc/allocator.rs:63`）會呼叫 `meta_store.allocate_group`。

---

## 3. 磁碟佈局

```
<engine_path>/
 |-- meta/                      <- RocksDB (all metadata)
 |-- 64KiB/                     <- size class 0
 |    |-- 00                    <- cluster file 0
 |    |-- 01
 |    |   ...
 |    `-- FF                    <- cluster file 255
 |-- 128KiB/                    <- size class 1
 |-- 256KiB/
 |   ...
 `-- 64MiB/                     <- size class 10

 11 size classes : 64KiB * 2^i  (i = 0..10)
 256 files per class, each file opened twice:
     normal_fd : O_SYNC     (unaligned IO)
     direct_fd : O_DIRECT   (4KiB-aligned IO, also handed to C++)
```

```
  one cluster file (chunk_size = S)

  offset 0
  +--------- group 0 ---------+--------- group 1 ---------+---- ...
  | c0 | c1 | c2 | ... | c255 | c0 | c1 | ... | c255     |
  +---------------------------+---------------------------+---- ...
  |<------ 256 * S ---------->|

  group = unit of fallocate / punch-hole
  chunk = unit of allocation (1 bit in GroupState)
```

**size class 的選擇**：寫入時取「≥ 資料長度的最小 2 冪」，見 `alloc/allocators.rs:57` 的 `select_by_size`。

**group 是磁碟空間單位，chunk 只是 bit**：

- group 一次涵蓋 256 個 chunk。預配置用 `fallocate`，回收用 `FALLOC_FL_PUNCH_HOLE`（`file/cluster.rs:43`）。
- chunk 的配置與釋放只翻記憶體和 RocksDB 裡 bitset 的一個 bit，不碰檔案系統。

**兩個 fd 的分工**（`file/cluster.rs:17-40`、`:63`）：

- offset 與長度都對齊 4KiB 時走 `direct_fd`（O_DIRECT），否則走 `normal_fd`（O_SYNC）。
- ZFS 不支援 O_DIRECT，兩個 fd 都改成 O_SYNC（`file/fs_type.rs`）。

---

## 4. 定址：GroupId 與 Position 的位元佈局

```
GroupId (u64)
 63                    32 31                      8 7        0
+------------------------+-------------------------+----------+
|   chunk_size (32bit)   |      group (24bit)      | cluster  |
+------------------------+-------------------------+----------+
                                                     ^ lowest bits
   GroupId + 1  ==> next cluster first, then next group
   => consecutive groups are spread round-robin over 256 files


Position (u64)
 63              40 39      32 31                      8 7        0
+------------------+----------+-------------------------+----------+
| chunk_size >> 8  | cluster  |      group (24bit)      |  index   |
|     (24bit)      |  (8bit)  |                         |  (8bit)  |
+------------------+----------+-------------------------+----------+

   file   = <size_dir>/<cluster as hex>
   offset = chunk_size * (group * 256 + index)
          = chunk_size * (low 32 bits of Position)
```

這是整個 crate 最關鍵的一張圖，對應 `types/group_id.rs:17`、`types/position.rs:13`。

**GroupId 把 cluster 放在最低位**：`next_group_id.plus_one()` 會先換檔案、再換 group，所以新配置的 group 會依序輪流落在 256 個檔案上，I/O 自然分散。

**Position 把 cluster 搬到 32–39 位**：這樣低 32 位剛好等於「group × 256 + index」，`offset()`（`types/position.rs:40`）只需一次乘法。代價是 chunk_size 只剩 24 位，必須是 256 的倍數；最小 64KiB，不構成限制。

`Position` 就是 chunk 在磁碟上的完整位址，也會寫進 `ChunkMeta.pos`（`types/chunk_meta.rs`）。

---

## 5. 配置器：group 的生命週期

```
                     AllocateWorker: get_allocate_task()
                     (allocated < min_remain)
   +---------------+   fallocate + put empty bits   +-----------------+
   |  unallocated  | -----------------------------> |    allocated    |
   |   (no disk)   | <----------------------------- | (empty, on disk)|
   +---------------+   punch hole + delete key      +--------+--------+
          ^           (allocated > max_remain)               |
          |                                                  | first chunk allocated
          | (slow path: allocate() falls through             v
          |  directly if pool is empty)           +---------------------+
          |                                       |   active_groups     |
          |       last chunk freed                |  level 0: 1..63     |
          |  +----------------------------------- |  level 1: 64..127   |
          |  |                                    |  level 2: 128..191  |
          |  v                                    |  level 3: 192..255  |
   back to allocated                              +----+-----------+----+
                                         all 256 used  |           ^ one freed
                                                       v           |
                                                  +----------------+---+
                                                  |    full_groups     |
                                                  +--------------------+

   compaction:  active --(get_compact_task)--> frozen --(finish)--> active
                (no new allocations from frozen groups)
```

狀態分散在 `ChunkAllocator`（`alloc/chunk_allocator.rs`）與 `GroupAllocator`（`alloc/group_allocator.rs`）兩個結構：

| 集合 | 意義 |
|---|---|
| unallocated | 還沒 fallocate 的 group id |
| allocated | 已 fallocate、一個 chunk 都沒用的預備池 |
| active | 部分使用，依使用量分 4 個 level（每 64 個 chunk 一級） |
| full | 256 個 chunk 全滿 |
| frozen | compaction 期間凍結，不再從它配置新 chunk |

**配置與 compaction 的挑選方向相反**：

- 配置（`alloc/chunk_allocator.rs:97`）從 level 3 往 level 0 找，優先填最滿的 group，讓資料集中。
- compaction（`alloc/chunk_allocator.rs:207`）從 level 0 往上找，挑最空的 group 把 chunk 搬走，好讓整個 group 歸還。

**group 內找空位**：對 4 個 u64 取反後做 `trailing_zeros`（`types/group_state.rs:56`）。

**預備池水位**：由 C++ `AllocateWorker` 維持在 `min_remain_groups=4` 到 `max_remain_groups=8` 之間（`src/storage/worker/AllocateWorker.h:17-18`），所以寫入路徑幾乎都能從記憶體直接拿到 group。池子空了的時候，寫入路徑會自己同步 fallocate，log 中標為 "slow path"（`alloc/group_allocator.rs:29`）。

---

## 6. 引用計數：位置何時真正釋放

```
   meta_cache: LockMap<chunk_id, Arc<Chunk>>
                               |
                               v
   +------------------------------------------+
   | Chunk { meta: ChunkMeta, allocator: Arc }|
   +------------------------------------------+
      Chunk::clone()  -> allocator.reference(pos)   rc += 1
      Chunk::drop()   -> allocator.dereference(pos) rc -= 1
                                            |
                                            v  rc == 0
                         ChunkAllocator::deallocate(pos)
                         (clear bit in memory GroupState)

   position_rc: ShardsMap<Position, u32>

   Timeline of an overwrite (COW):
   ------------------------------------------------------------------
   reader  : get() -> Arc<Chunk@P1> ......... reading P1 ......... drop
   writer  :          alloc P2, write, commit(meta -> P2)
   RocksDB :                                   bit(P1)=0, bit(P2)=1
   memory  :                                   cache -> P2
   P1 rc   :   2              2                  1                   0 -> freed
```

相關程式碼：`alloc/chunk_allocator.rs:133`（reference）、`:162`（dereference）、`:173`（deallocate），以及 `alloc/chunk.rs` 結尾的 `Clone` / `Drop`。

**RocksDB 與記憶體是兩條獨立的釋放路徑**：

- commit 時，RocksDB 裡舊位置的 bit 立即清除。
- 記憶體裡的 bit 要等**最後一個 `Arc<Chunk>` 被 drop** 才清除。

所以讀取方只要握著 `Arc<Chunk>`，即使同時有人覆寫或刪除這個 chunk，舊位置也不會被重新配給別人，讀到的資料不會被覆蓋。讀與寫因此完全不用互相上鎖。

崩潰發生在 commit 之前的話，記憶體裡配出去的 bit 根本沒寫進 RocksDB，重啟後自然不存在。

---

## 7. MetaStore：RocksDB 的 key 空間

```
 prefix | key                                  | value                 | write op
 -------+--------------------------------------+-----------------------+---------
   1    | 0x01 + ~chunk_id (bitwise NOT)       | ChunkMeta (derse)     | put/del
   2    | 0x02 + GroupId (BE u64)              | GroupState (32 bytes) | MERGE
   3    | 0x03 + Position (BE u64)             | chunk_id              | put/del
   4    | 0x04 + chunk_id[..prefix_len]        | used size (i64 LE)    | MERGE
   5    | 0x05                                 | prefix_len            | put
   6    | 0x06 + prefix + timestamp(BE) + rest | chunk_id / empty      | put/del
   8    | 0x08                                 | version               | put
   9    | 0x09 + chunk_id                      | ChunkMeta (writing)   | put/del

 MergeOp for prefix 2:
   operands = MergeState{acquire:{idx..}, release:{idx..}}
   full_merge: GroupState.bits |= acquire; &= ~release
```

key 定義在 `meta/meta_key.rs`，merge 規則在 `meta/meta_merge.rs:9`。

| 前綴 | 用途 |
|---|---|
| 1 | 主表 `chunk_id → ChunkMeta`。chunk_id 逐位元組取反（`!num`）後才當 key，讓 RocksDB 正向迭代時 chunk_id 由大到小 |
| 2 | 每個 group 的 bitset。啟動時 `ChunkAllocator::load`（`alloc/chunk_allocator.rs:31`）掃這段就能重建記憶體狀態 |
| 3 | 位置反查 chunk。compaction 用 `group_to_chunks_key_prefix` 列出某個 group 裡的所有 chunk |
| 4 | 每個 chain 的已用容量。C++ 傳入的 chunk_id 前 `prefix_len` 個位元組就是 chain 識別 |
| 6 | 依時間戳查詢，給 `query_chunks_by_timestamp` 用 |
| 9 | 寫入中紀錄（write-ahead），見第 8、10 節 |

前綴 2 和 4 用 RocksDB **MergeOp**：寫入方只送出增量（設哪幾個 bit／清哪幾個 bit、容量加減多少），不必先讀出舊值。多個 WriteBatch 同時改同一個 group 也不會互相覆蓋。

---

## 8. 寫入路徑：update → commit 兩階段

```
 C++ UpdateJob
     |
     |  update_raw_chunk(chunk_id, UpdateReq)
     v
+------------------------------- Engine::update_chunk ------------------------------+
| 1. crc32c(data) == req.checksum ?                                                  |
| 2. old = get(chunk_id)            (meta_cache or RocksDB)                          |
| 3. version checks                                                                   |
|      chain_ver  >= old.chain_ver                                                    |
|      update_ver == old.chunk_ver + 1     (else Committed / Missing update)          |
|      expected_tag == old.etag                                                       |
| 4. choose write strategy                                                            |
|                                                                                     |
|      is_remove ------------------------> new = old.clone()      (same Position)     |
|                                                                                     |
|      syncing || overwrite (off < len)                                               |
|      || exceeds capacity --------------> COPY-ON-WRITE                              |
|                                           alloc P2 (size class by new len)         |
|                                           read old (aligned) + merge data          |
|                                           pwrite P2                                 |
|                                                                                     |
|      pure append within capacity ------> IN-PLACE APPEND (safe_write)               |
|                                           new = old.clone()      (same Position)   |
|                                           pwrite after old.len only                 |
|                                           crc32c_combine(old, new)                 |
|                                                                                     |
|      not exist ------------------------> alloc P, safe_write                        |
|                                                                                     |
| 5. writing_list[prefix][chunk_id] = new        (reject if already writing)         |
| 6. RocksDB put  0x09+chunk_id -> new meta      (sync = true)                        |
+--------------------------------------------------+---------------------------------+
                                                   | returns WritingChunk
                                                   v
     ...  C++ forwards to successor in chain, waits for ack  ...
                                                   |
     commit_raw_chunk(WritingChunk, sync)          v
+------------------------------- Engine::commit_chunk ------------------------------+
| lock meta_cache entry(chunk_id)                                                   |
| WriteBatch (atomic):                                                              |
|    put   0x01 chunk_meta                                                          |
|    del   0x03 old_pos      put 0x03 new_pos       (only if pos changed)           |
|    merge 0x02 release old  merge 0x02 acquire new (only if pos changed)           |
|    merge 0x04 +/- size                                                            |
|    del/put 0x06 timestamp                                                         |
|    del   0x09 writing record                                                      |
| meta_cache[chunk_id] = Arc<new>                                                   |
| unlock;  WritingChunk::drop -> remove from writing_list                           |
+-----------------------------------------------------------------------------------+
```

相關程式碼：`core/engine.rs:295`（update_chunk）、`:481`（commit_chunk）、`:520`（commit_chunks），`meta/meta_store.rs:149`（add_chunk_mut）、`:205`（move_chunk_mut）、`:279`（remove_mut）、`:348`（persist_writing_chunk）。

**為什麼分兩階段：配合 CRAQ**

1. `update_chunk` 先把資料寫到磁碟，並留下前綴 9 的寫入中紀錄。這時 meta_cache 仍指向舊版本，所以新資料**對讀取方不可見**。
2. C++ 等 chain 上的後繼節點回 ack，才呼叫 `commit_chunk`。
3. `commit_chunk` 用一個 WriteBatch 原子地切換所有中繼資料，最後才更新 meta_cache。

**三種寫法**：

| 情境 | 做法 | 程式碼 |
|---|---|---|
| 純追加，且不超過容量 | **原地寫**在同一個 Position：讀取方只會讀到舊 `len`，所以安全；checksum 用 `crc32c_combine` 增量合併，不重讀舊資料 | `alloc/chunk.rs:176` `safe_write` |
| 覆寫已有範圍 | 一律 copy-on-write，換到新位置寫，絕不原地修改已提交的資料（這正是第 6 節引用計數存在的原因） | `alloc/chunk.rs:89` `copy_on_write` |
| 超過容量 | 一樣 copy-on-write，同時換到更大的 size class | 同上 |

**同一 chunk 同時只能有一筆寫入中的更新**：`writing_list` 裡已有該 chunk 時，第二筆直接被拒（錯誤訊息 `chunk is in writing`）。

批次版的 `commit_chunks` 會把多個 chunk 合進同一個 WriteBatch。

---

## 9. 讀取路徑

```
 C++ BatchReadJob
     | get_raw_chunk(chunk_id)
     v
 Engine::get
     |
     +--> meta_cache.entry_by_ref(chunk_id)    (per-key lock, LockMap 256 shards)
            |
            +-- hit  --> clone Arc<Chunk>
            |
            +-- miss --> MetaStore.get_chunk_meta (0x01)
                         allocator.reference(meta)   rc += 1
                         insert into cache
     |
     v
 Arc<Chunk>  --raw ptr-->  C++
     |
     | chunk->fd_and_offset()  => { direct_fd of cluster, pos.offset() }
     v
 C++ AIO (io_uring / libaio) reads directly from disk
     |
     v
 release_raw_chunk(ptr)  ->  Arc drop  ->  rc -= 1
```

相關程式碼：`core/engine.rs:184`（get）、`:189`（get_with_entry）、`file/clusters.rs:57`（fd_and_offset）。

讀取路徑上 Rust **不碰資料本身**，只負責兩件事：

1. 把 chunk_id 翻成「哪個 fd、哪個 offset」。
2. 用 `Arc` 保證 C++ 讀完之前，這個位置不會被回收。

真正的 I/O 由 C++ 的 AIO 對 `direct_fd` 發。

---

## 10. 崩潰復原（Engine::open）

```
 Engine::open
   |
   | 1. MetaStore::open
   | 2. occupy_uncommitted_positions()
   |      scan 0x09 (writing records)
   |      for each: if 0x01 meta.pos != writing.pos
   |                   temporarily merge-acquire its bit + put 0x03
   |      (so Allocators won't hand out that position again)
   |
   | 3. Allocators::new  -> for each size class
   |      ChunkAllocator::load: scan 0x02 group bits
   |         missing key  -> unallocated
   |         empty bits   -> allocated (pool)
   |         full bits    -> full_groups
   |         partial      -> active_groups[level]
   |
   | 4. put each writing record into writing_list { abort: true }
   |      allocator.reference(pos)       (keeps position alive in memory)
   |
   | 5. vacate_uncommitted_positions()   (undo the temporary bits in RocksDB)
   v
 later, C++ calls handle_uncommitted_raw_chunks(prefix, chain_ver)
   -> commit all writing chunks of that chain
```

相關程式碼：`core/engine.rs:31`（open）、`meta/meta_store.rs:367`（occupy）、`:407`（vacate）、`core/engine.rs:592`（handle_uncommitted_chunks）。

**前綴 9 的紀錄**是「資料已落盤、但中繼資料還沒切換」的 chunk。

**要先暫時佔位的原因**：這些位置在 RocksDB 的 group bits 裡是 0，直接載入的話，配置器會把它們當空位再配出去。所以 `open` 的順序是：

1. 暫時把這些 bit 設成 1，讓 `ChunkAllocator::load` 視為已用。
2. 在記憶體裡用引用計數持有這些位置。
3. 撤銷剛才暫設的 bit。

之後由 C++ 的 chain 恢復流程呼叫 `handle_uncommitted_raw_chunks`（`src/storage/store/ChunkEngine.h:162`），決定把它們提交出去。

---

## 11. 背景工作（由 C++ 驅動）

```
 C++ AllocateWorker loop (src/storage/worker/AllocateWorker.cc)
   |
   +-- engine.allocate_groups(min=4, max=8, batch=128)        size <= 4MiB
   |      per size class: keep "allocated" pool in [min, max]
   |        < min : Allocate(gid)   -> fallocate + put empty 0x02
   |        > max : Deallocate(gid) -> delete 0x02 + punch hole
   |
   +-- engine.allocate_ultra_groups(min=0, max=4, batch=32)   size > 4MiB
   |
   `-- engine.compact_groups(max_reserved)
          per size class: if free chunks in partially used groups > max_reserved
             pick emptiest active group -> frozen
             scan 0x03 prefix of this group
             for each chunk: move_chunk (copy to new pos, swap meta)
             finish_compact_task -> back to active (or already freed)
```

相關程式碼：`alloc/allocators.rs:74`（allocate_groups）、`alloc/group_allocator.rs:63`（get_allocate_task）、`core/engine.rs:117`（compact_groups）、`core/engine.rs:226`（move_chunk）。

`compact_groups` 的門檻 `max_reserved_chunks` 預設為 `1_GB`（`src/storage/worker/AllocateWorker.h:21`）。但它被拿去跟 **chunk 個數**比較，不是位元組數，所以照預設值，compaction 幾乎不會觸發。此點為讀碼推斷，未實測。

---

## 12. 鎖與並發

```
 +--------------------------+-------------------------+----------------------------+
 | structure                | lock                    | protects                   |
 +--------------------------+-------------------------+----------------------------+
 | Allocator.allocator      | Mutex (per size class)  | bitsets, group sets, rc map|
 | Engine.meta_cache        | LockMap per-key entry   | chunk_id -> Arc<Chunk>     |
 | Engine.writing_list      | DashMap (per prefix)    | in-flight updates          |
 | RocksDB                  | internal                | WriteBatch atomicity       |
 +--------------------------+-------------------------+----------------------------+
```

**meta_cache 的 entry 鎖**：commit 期間全程持有，涵蓋「寫 RocksDB → 更新 cache」整段。所以同一個 chunk_id 的中繼資料切換是序列化的，不同 chunk_id 之間完全平行。

**配置器的 Mutex**：每個 size class 一把，臨界區只翻 bit 和操作集合，很短。

---

## 建議閱讀順序

1. `types/group_id.rs`、`types/position.rs`：對著第 4 節的圖把位元運算算一遍
2. `types/group_state.rs`、`types/merge_state.rs`：bitset 與 merge 的語意
3. `file/cluster.rs`、`file/clusters.rs`：雙 fd、fallocate
4. `alloc/chunk_allocator.rs`、`alloc/group_allocator.rs`：對照第 5、6 節
5. `meta/meta_key.rs`、`meta/meta_store.rs`：對照第 7 節的 key 表
6. `alloc/chunk.rs`：`safe_write` 與 `copy_on_write` 的分支
7. `core/engine.rs`：`open`、`update_chunk`、`commit_chunk`、`move_chunk`
