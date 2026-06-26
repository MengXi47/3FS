# 3FS 存儲層深度剖析：CRAQ · Chunk · Target · 切片 · 資料回復

> 直接讀程式碼產出。範圍涵蓋 `src/fbs/meta`、`src/fbs/mgmtd`、`src/fbs/storage`、`src/storage/*`（service / store / sync / aio / worker / chunk_engine）、`src/client/storage`、`src/fuse/PioV`、`src/mgmtd`。所有引用為 `檔案:行號`，可點擊。
>
> 閱讀順序建議：先看 **第 1 部分的「資料分佈表」**（資料如何分到系統），再依寫入 / 狀態機 / 復原逐章深入。

---

## 目錄

0. [五個核心名詞](#0-五個核心名詞)
1. [資料如何分到系統中（核心表格）](#1-資料如何分到系統中核心表格)
2. [實體層級：Node → Target → Disk → Chunk Engine](#2-實體層級node--target--disk--chunk-engine)
3. [CRAQ 鏈式複製：寫入、提交、讀取](#3-craq-鏈式複製寫入提交讀取)
4. [Target 狀態機與 Chain 管理（mgmtd）](#4-target-狀態機與-chain-管理mgmtd)
5. [資料回復（Resync）](#5-資料回復resync)
6. [端到端資料流總結](#6-端到端資料流總結)
7. [附錄：參數、列舉、檔案索引](#7-附錄)

---

## 0. 五個核心名詞

| 名詞 | 英文 | 一句話定義 | 程式碼型別 |
|------|------|-----------|-----------|
| **節點** | Node | 一台實體 storage 伺服器 | `NodeId`(u64) → `NodeInfo` |
| **目標** | Target | 一個節點上「一顆磁碟的一個副本分片」，是複製的最小單位 | `TargetId`(u64) → `TargetInfo` |
| **鏈** | Chain | 一組 target 組成的 CRAQ 複製鏈（HEAD→TAIL），同一份資料的多個副本 | `ChainId`(u32) → `ChainInfo` |
| **鏈表** | ChainTable | 一組 chain 的有序列表，檔案 layout 透過它的索引選 chain | `ChainTableId`(u32) → `ChainTable` |
| **塊** | Chunk | 檔案被切成的固定大小資料單元，是 IO 與儲存的最小單位 | `ChunkId`(16B) |

階層關係：

```
                  叢集 RoutingInfo（mgmtd 維護，client/meta pull）
                  ┌───────────────────────────────────────────┐
                  │ nodes:       NodeId  → NodeInfo            │
                  │ targets:     TargetId→ TargetInfo          │
                  │ chains:      ChainId → ChainInfo(targets[])│
                  │ chainTables: ChainTableId → {ver→ChainTable}│
                  └───────────────────────────────────────────┘

 File(inode)
   └─ 切成多個 Chunk（chunkSize）
        └─ 每個 Chunk 依 stripe 落在一條 Chain
             └─ Chain = [Target_HEAD, …, Target_TAIL]   (CRAQ 順序，預設 3 副本)
                  └─ 每個 Target 在某 Node 的某顆 Disk 上
                       └─ Disk 上的 chunk engine 把 chunk 存進固定大小 data file 的某 position
```

---

## 1. 資料如何分到系統中（核心表格）

這是整份報告的核心。一個應用對檔案 `(inode, offset, length)` 的存取，經過 **12 個層級** 才落到某顆實體磁碟的某個位元組偏移。下表逐層拆解「每一層用什麼識別、由什麼決定、在哪段程式碼」。

### 1.1 完整分佈鏈（12 層）

| # | 層級 | 識別子 / 公式 | 由什麼決定 | 程式碼位置 |
|---|------|--------------|-----------|-----------|
| 1 | **檔案** | `InodeId`（u64） | meta 建檔時分配 | `fbs/meta/Schema.h:361` |
| 2 | **檔案位元組** | `offset`（u64） | 應用 read/write 座標 | — |
| 3 | **chunk 序號** | `chunkIndex = offset / chunkSize` | `Layout.chunkSize`（**必為 2 的冪**） | `Schema.cc:67` `getChunkId` |
| 4 | **chunk 鍵** | `ChunkId` = 16B 大端 `[tenant1│rsvd1│inode8│track2│chunk4]` | `inode + track + chunkIndex` | `Schema.h:177-226` |
| 5 | **stripe 槽位** | `stripe = chunkIndex % stripeSize` | `Layout.stripeSize`（檔案橫跨幾條 chain） | `Schema.cc:195` `getChainOfChunk` |
| 6 | **chain 索引** | `chainIndex = chainIndexList[stripe]` | `Layout.chains`（`ChainRange` 或 `ChainList`） | `Schema.cc:160-197` |
| 7 | **chain 引用** | `ChainRef{tableId, tableVersion, chainIndex}` | `Layout.tableId / tableVersion` | `fbs/mgmtd/ChainRef.h:14` |
| 8 | **chain ID** | `ChainId = chainTable.chains[(chainIndex-1) % N]` | `RoutingInfo.getChainId(ref)`（**1-based + 取模環繞**） | `fbs/mgmtd/RoutingInfo.cc:22-35` |
| 9 | **chain 資訊** | `ChainInfo{chainVersion, targets[], preferredTargetOrder}` | mgmtd 維護（CRAQ 順序） | `fbs/mgmtd/ChainInfo.h:6` |
| 10 | **target** | `TargetInfo{nodeId, diskIndex, publicState, localState, chainId}` | mgmtd 依心跳指派 | `fbs/mgmtd/TargetInfo.h:8` |
| 11 | **node** | `NodeInfo{...服務位址}` | 叢集拓樸 | `fbs/mgmtd/NodeInfo.h` |
| 12 | **實體位置** | `disk[diskIndex]` → chunk engine → 檔案 `cluster/group/index`，偏移 `chunkSize×(group×256+index)` | chunk engine 分配 | `chunk_engine/types/position.rs:40` |

> **一句話總結**：`offset` 除以 chunk 大小決定「第幾塊」（垂直切片，第 3 層）；塊號對 stripe 取模決定「分散到第幾條鏈」（水平條帶化，第 5 層）；鏈索引經 chain table 解析成全域鏈 ID（第 8 層）；鏈內第一個 SERVING target 是 HEAD、最後一個是 TAIL（第 9 層）；每個 target 釘在某 node 的某 disk（第 10-12 層）。

### 1.2 第 3-4 層：切片成 Chunk

`File::getChunkId`（`Schema.cc:62-73`）：

```cpp
auto chunk = offset / layout.chunkSize;      // 第幾塊
return ChunkId(id, 0, chunk);                // track 在此固定為 0
```

**`ChunkId` 的 16-byte 大端佈局**（`Schema.h:177-198`）：

```
 byte:  0        1        2 ─────── 9        10 ── 11      12 ─── 15
      ┌────────┬────────┬──────────────────┬────────────┬──────────────┐
      │tenant  │reserved│   inode id (64)   │ track (16) │  chunk (32)  │
      │ =0x00  │ =0x00  │   big-endian      │ big-endian │  big-endian  │
      └────────┴────────┴──────────────────┴────────────┴──────────────┘
```

**為何用大端**：保持字典序 = 數值序，使「同一 inode 的所有 chunk」在 KV（RocksDB）中連續排列，利於 range 查詢與整檔刪除。`ChunkId::range(inode)`（`Schema.h:208-215`）正是用 `[ChunkId(inode,0,0), ChunkId(inode,1,0))` 框出一個 inode 的全部 chunk —— 這也是 meta 查詢檔案長度時對每條 chain 查「最後一個 chunk」的依據（`FileOperation.cc:84`）。

### 1.3 第 5-6 層：條帶化（Stripe）—— 跨節點平行的關鍵

`Layout::getChainOfChunk`（`Schema.cc:192-197`）：

```cpp
auto stripe = chunkIndex % stripeSize;            // round-robin
return ChainRef{tableId, tableVersion, chains[stripe]};
```

- **`chunkSize`**（垂直）：一個 chunk 多大。
- **`stripeSize`**（水平）：連續 chunk 分散到幾條 chain。相鄰 chunk 落在不同 chain → 不同 node → **平行頻寬**。

`Layout.chains` 是個 variant（`Schema.h:144`），兩種模式：

| 模式 | 內容 | chainIndexList 產生方式 | 程式碼 |
|------|------|------------------------|--------|
| `ChainRange` | `baseIndex + seed` | `chains[i] = baseIndex + i`（i=0..stripeSize-1），可選 `STD_SHUFFLE_MT19937` 用 seed 打散 | `Schema.cc:160-181` |
| `ChainList` | 明確的 `chainIndexes[]` | 直接使用該列表（size 必須 == stripeSize） | `Schema.cc:188` |

`ChainRange` 是預設（省空間，不必存整個列表）；shuffle 的用意是**避免所有檔案都從同一條 chain 起始造成熱點**（`Schema.cc:117-127` `newChainRange` 用 `find_safe_seed`）。

**track 機制**（多軌檔案，目前保留）：`getChainId` 計算時用 `chunkIndex + track * 7`（`Schema.cc:83`，7 是質數 `TRACK_OFFSET_FOR_CHAIN`），讓不同 track 從不同 chain 集合起始；但 `getChunkId` 的 track 欄位仍寫 0，track 只影響選鏈、不影響 chunk 鍵。

### 1.4 第 7-8 層：ChainRef → ChainId 解析

檔案 layout 不直接存全域 `ChainId`，而是存「**chain table 內的相對索引**」（`ChainRef`），好處是 chain table 可以增減 chain 而不必改寫每個檔案的 layout。解析在 `RoutingInfo::getChainId`（`RoutingInfo.cc:22-35`）：

```cpp
auto [tid, tv, index] = ref.decode();
if (tid == 0 && tv == 0) return ChainId(index);   // 特例：直接把 index 當 ChainId（ChainList 直連）
const auto *table = getChainTable(tid, tv);        // tv==0 表示取最新版
if (index == 0) return std::nullopt;               // chainIndex 從 1 起算
index = (index - 1) % table->chains.size();        // 1-based → 0-based，且對鏈數取模環繞
return table->chains[index];
```

兩個要點：
1. **1-based**：`chainIndex` 從 1 起算（layout 驗證明確拒絕 index 0，`Schema.cc:142,151`）。
2. **取模環繞**：meta 可以用「超過實際 chain 數」的索引，自動 wrap 分散到各 chain，達到負載均衡。

`ChainTable`（`fbs/mgmtd/ChainTable.h`）本質就是「一組有序 `ChainId` 的列表」+ 版本；多版本以 `std::map<ChainTableVersion, ChainTable>` 保留（`RoutingInfo.h:37`），舊檔案可繼續用舊版 stripe 映射。

### 1.5 第 9-12 層：Chain → Target → Node → Disk

```cpp
// ChainInfo（fbs/mgmtd/ChainInfo.h:6）
chainId, chainVersion, targets: vector<ChainTargetInfo>, preferredTargetOrder
// ChainTargetInfo（ChainTargetInfo.h:8）= { targetId, publicState }
// TargetInfo（TargetInfo.h:8）= { targetId, publicState, localState, chainId, nodeId?, diskIndex?, usedSize }
```

`targets` 向量的**索引順序即 CRAQ 物理順序**：index 0 = HEAD（收寫入），最後一個 SERVING = TAIL（強一致讀點）。`TargetInfo.nodeId + diskIndex` 把副本釘到「哪台機器的哪顆盤」。

### 1.6 具體範例

假設一個檔案：`chunkSize = 512KiB`、`stripeSize = 4`、layout 用 `ChainRange{baseIndex=10, NO_SHUFFLE}`，chain table #2（最新版）的 `chains = [c0, c1, …]`（共 100 條），讀 `offset = 1.3 MiB`、`length = 8KiB`：

```
offset 1.3MiB = 1363148 B
chunkIndex   = 1363148 / 524288        = 2
ChunkId      = (inode, track=0, chunk=2)
stripe       = 2 % 4                    = 2
chainIndex   = chainIndexList[2] = baseIndex+2 = 12     (NO_SHUFFLE)
ChainRef     = {table=2, ver=最新, index=12}
ChainId      = chainTable[2].chains[(12-1) % 100] = chains[11]
ChainInfo    = chains[11] → targets = [T_a(HEAD), T_b, T_c(TAIL)]
讀取         → LoadBalance 選 T_a/T_b/T_c 任一 serving 副本
              → TargetInfo{nodeId=N5, diskIndex=3}
              → N5 的第 3 顆盤 chunk engine 讀 ChunkId 對應 position
```

同檔案的 chunk 0,1,2,3 會分別落在 stripe 0,1,2,3（4 條不同 chain，多半在不同 node），chunk 4 又回到 stripe 0 —— 這就是條帶化平行。

---

## 2. 實體層級：Node → Target → Disk → Chunk Engine

### 2.1 Storage 端 Target 的兩層表示

storage node 本機對 target 有**兩層**物件：

| 層 | 型別 | 角色 | 程式碼 |
|----|------|------|--------|
| 路由/狀態視圖 | `Target`（`fbs/storage/Common.h:685`） | 持 `isHead/isTail/vChainId/localState/publicState/successor`，供路由判斷 | `service/TargetMap.cc` |
| 實體 store handle | `StorageTarget`（`store/StorageTarget.h:20`） | 封裝磁碟上的 `PhysicalConfig`、chunk 引擎、鎖 | `store/StorageTarget.cc` |

`Target.storageTarget` 指向 `StorageTarget`。`TargetMap` 用 **`atomic_shared_ptr` 快照 + copy-on-write**（`TargetMap.cc:370-382`）：讀路徑完全無鎖（`snapshot()`），寫路徑 clone → 改 → `compare_exchange`。

### 2.2 兩條儲存引擎路徑

`StorageTarget` 的每個操作都看 `useChunkEngine()`（`StorageTarget.h:162`，即 `only_chunk_engine`）二選一：

- **新路徑**：Rust `chunk_engine::Engine`（`ChunkEngine::*`，cxx 橋接）
- **舊路徑**：C++ `ChunkStore` + `ChunkReplica::*`

多 target 由 `StorageTargets`（`store/StorageTargets.h`）管理，並收集所有 chunk 檔的 `fds_` 供 io_uring 註冊。

### 2.3 Chunk Engine 物理佈局（Rust）

chunk engine（`src/storage/chunk_engine/`，Rust crate v0.1.11，cxx 橋接）把「邏輯 chunk」映射到「實體檔案偏移」，五層階層：

```
Allocators (11 種 chunk size)               alloc/allocators.rs:6
└─ Allocator (每種 size 一個 + Mutex)        alloc/allocator.rs:4
   ├─ ChunkAllocator (記憶體空間狀態機)       alloc/chunk_allocator.rs:7
   └─ Clusters (每種 size 256 個檔案)         file/clusters.rs:18
      └─ Cluster (單一實體檔, 2 個 fd)        file/cluster.rs:11
         └─ Group (連續 256 個 chunk 槽)      types/group_state.rs:14
            └─ Position (單一 chunk 槽)        types/position.rs:7
```

**11 種固定大小**（`types/constants.rs`）：64KiB 起每次 ×2 到 64MiB（`CHUNK_SIZE_NUMBER=11`）。寫入需求向上取 2 的冪選級別（`allocators.rs:57` `select_by_size`）。

**Position 是打包的 u64**（`position.rs:9`）：`[chunk_size>>8 (24)│cluster(8)│group(24)│index(8)]`，與 `GroupId` 可零成本互轉。實體位元組偏移 = `chunk_size × (group×256 + index)`（`position.rs:40`）。

**空間用 `fallocate` 管理**（`cluster.rs:43`）：分配 group = `fallocate(0)` 佔空間；回收 = `fallocate(PUNCH_HOLE|KEEP_SIZE)` 打洞釋放實體 block 但保留檔案大小（檔案只增不縮）。

### 2.4 COW（Copy-On-Write）—— 配合 CRAQ pending/commit

寫入分 update(pending) 與 commit 兩階段；commit 前舊的已提交版本必須仍可讀。`Engine::update_chunk`（`engine.rs:386-434`）依情況選路徑：

| 情況 | 路徑 | 說明 |
|------|------|------|
| 刪除 | clone meta，不寫資料 | — |
| **覆寫既有資料**（`offset < len`）/ 同步 / 需更大 chunk | **COW 到新 Position**（`copy_on_write`，`chunk.rs:89`） | 舊 Position 保留到無讀者 |
| 追加到已提交長度之後（`offset >= len`） | 就地寫（`safe_write`），clone 共用同一 Position（refcount+1） | 安全，已提交讀者看不到新位元組 |
| 全新 chunk | 分配新 Position | — |

舊 Position 靠 **refcount**（`chunk_allocator.rs:133`）保護：所有讀者 drop 後 refcount 歸零才真正釋放槽位。這是 COW 安全的基石。

### 2.5 Metadata 存 RocksDB

chunk metadata 全存 RocksDB（`meta/meta_store.rs`），7 種 1-byte 前綴 key（`meta/meta_key.rs:7`）：

| 前綴 | 用途 |
|------|------|
| 1 | chunk_id（**反相**）→ `ChunkMeta`（主表，反相使迭代為降序） |
| 2 | group_id → 256-bit bitmap（用 merge operator 無鎖累加） |
| 3 | position → chunk_id（反向表，compaction 用） |
| 4 | chain 前綴 → used size（merge 累加） |
| 6 | timestamp 次級索引 |
| 8 | schema 版本 |
| 9 | 未提交寫入 WAL（崩潰復原用） |

`ChunkMeta`（`types/chunk_meta.rs:6`）：`pos, chain_ver, chunk_ver, len, checksum, timestamp, last_request_id, last_client_*, etag, uncommitted`。

**崩潰復原**（`engine.rs:42-70`）：開機掃 WAL（前綴 9），把未 commit 的 position 先佔住避免被當空閒重分配，重建記憶體 `WritingHolder`，交由 CRAQ 決定 commit 或丟棄。

### 2.6 資料完整性：CRC32C

全程 CRC32C（硬體加速）。寫入時比對 client checksum（`engine.rs:304-320`，不符拒絕）；增量維護用 `crc32c_combine`/`crc32c_append` 避免整段重算（`chunk.rs:176-281`）。引擎保證 `meta.checksum` 恆等於落盤資料的 CRC。

> **細節**：C++ 層儲存的是 checksum 的**補數**（`~checksum`），引擎內部用正規 CRC32C，邊界互轉（`ChunkEngine.cc:39,65`）。

---

## 3. CRAQ 鏈式複製：寫入、提交、讀取

**CRAQ**（Chain Replication with Apportioned Queries）核心：寫入沿鏈 HEAD→TAIL 流動，commit 由 TAIL→HEAD 回溯；每個 chunk 同時維護「已提交版本」與「待提交(pending/dirty)版本」，讓讀取可分散到任一副本（apportioned queries）。

### 3.1 三版本 + 狀態（pending vs committed 的核心）

`ChunkMetadata`（`Common.h:652`）攜帶三個版本與一個狀態：

```cpp
commitVer  // 已提交版本
updateVer  // 最新（可能未提交）版本
chainVer   // 鏈拓樸版本（成員變更時遞增）
chunkState // COMMIT(0) / DIRTY(1) / CLEAN(2)
```

| 狀態 | 版本關係 | 含義 |
|------|----------|------|
| `COMMIT` | `commitVer == updateVer` | 完全提交，乾淨 |
| `CLEAN` | `updateVer > commitVer` | 資料已寫完但**未提交**（pending，可被讀 uncommitted） |
| `DIRTY` | 寫入進行中 | 落盤途中崩潰會不一致，需 resync |

**不變式**：`updateVer >= commitVer`，且 pending 至多領先一個版本（`updateVer <= commitVer + 1`）。

### 3.2 寫入完整流程（函式呼叫鏈）

```
[client write RPC]
StorageOperator::write                       StorageOperator.cc:233
└─ ReliableUpdate::update                     ReliableUpdate.cc:16   (channel 鎖 + 冪等去重)
   └─ StorageOperator::handleUpdate           StorageOperator.cc:333 (CRAQ 編排核心)
      ├─ lockChunk                            StorageOperator.cc:371 (per-chunk FIFO 序列化)
      ├─ doUpdate ── 伺服器主動 RDMA Read 拉資料 StorageOperator.cc:521
      │    └─ UpdateWorker → ChunkReplica::update  (寫 pending：updateVer+1, state=CLEAN)
      ├─ forwardWithRetry → 後繼節點           ReliableForwarding.cc:33 (阻塞直到後繼整個完成)
      │    └─ doForward → messenger.update(successor) → (遞迴)後繼 handleUpdate…
      ├─ 校驗 checksum（本地 vs 後繼）           StorageOperator.cc:480
      └─ doCommit                             StorageOperator.cc:616
           └─ ChunkReplica::commit            (commitVer→updateVer, state=COMMIT)
```

**關鍵架構事實**：`forwardWithRetry` 是**阻塞呼叫**，要等後繼整個 `handleUpdate`（含後繼自己的 commit）回來才返回。遞迴展開後：**TAIL 最先 commit，commit/ACK 由 TAIL 沿鏈回溯到 HEAD**。不需要獨立的 commit 廣播協議。

### 3.3 逐跳 RDMA Read（資料流 HEAD→TAIL）

3FS 的寫資料是**伺服器主動 RDMA Read**（非 client push）。`doUpdate`（`StorageOperator.cc:551-567`）：

```cpp
auto allocateResult = buffer.tryAllocate(updateIO.rdmabuf.size());   // 本地落地緩衝
remoteBuf = allocateResult->toRemoteBuf();        // 暴露給「下一節點」來 RDMA Read
auto readBatch = ibSocket->rdmaReadBatch();
readBatch.add(updateIO.rdmabuf, std::move(*allocateResult));  // 從「前驅」rdmabuf 拉
co_await readBatch.post();                          // 發出 RDMA READ
```

資料逐跳拉：`client → HEAD → node2 → … → TAIL`，每節點從上一節點的註冊 buffer RDMA Read。小寫入可用 `SEND_DATA_INLINE` 內嵌 RPC 省去 RDMA。

### 3.4 版本遞增與檢查（全鏈一致性）

`ChunkReplica::update`（`ChunkReplica.cc:211-239`）分三種來源：

```cpp
if (options.isSyncing) {              // (a) 同步補資料：直接採用來源版本
  meta.updateVer = writeIO.updateVer;  meta.commitVer = updateVer-1;
} else if (writeIO.updateVer > 0) {  // (b) 後繼：版本由 HEAD 指定，必須精確匹配
  if (updateVer <= commitVer)      return kChunkCommittedUpdate;   // 已提交（冪等命中）
  if (updateVer <= meta.updateVer) return kChunkStaleUpdate;       // 過期重複
  if (updateVer >  meta.updateVer+1) return kChunkMissingUpdate;   // 跳號漏更新
  meta.updateVer = writeIO.updateVer;
} else {                             // (c) HEAD 收 client 寫（updateVer==0）：自行 +1
  meta.updateVer += 1;
  if (meta.updateVer > meta.commitVer+1) return kChunkAdvanceUpdate;  // 不可領先 >1
}
```

- **HEAD** 決定版本號（`updateVer==0` → `+1`，並回填進 `req.payload.updateVer` 下傳，`StorageOperator.cc:409`）。
- **後繼**必須收到「正好下一個版本」，藉 4 種版本碼檢測 漏/重/已提交/越界 → **全鏈版本號嚴格一致**。

### 3.5 Commit：pending → committed

`ChunkReplica::commit`（`ChunkReplica.cc:397-467`）核心：

```cpp
if (commitIO.commitVer > meta.updateVer)   return kChunkVersionMismatch;
if (meta.chunkState == ChunkState::DIRTY)  return kChunkNotClean;     // 寫到一半不能 commit
if (meta.commitVer < commitIO.commitVer)   meta.commitVer = commitIO.commitVer;  // 推進
if (meta.commitVer == meta.updateVer) {    // pending 追平 → 轉 COMMIT
  meta.chunkState = ChunkState::COMMIT;
  meta.chainVer = job.commitChainVer();
}
```

`commitVer` 推到等於 `updateVer` 的瞬間，chunk 從 `CLEAN`(pending) 轉 `COMMIT`(已提交)。

### 3.6 讀取路徑（Apportioned Queries）

讀取入口 `StorageOperator::batchRead`（`StorageOperator.cc:82`）。一致性檢查在 `ChunkReplica::aioPrepareRead`（`ChunkReplica.cc:55-66`）：

```cpp
if (commitVer != updateVer && !readUncommitted)
  return kChunkNotCommit;   // 存在 pending 又不允許讀未提交 → 拒絕（改問 TAIL）
```

**讀寫路由非對稱**（client 端策略，`TargetSelection.cc`）：

| 操作 | 預設策略 | 選哪個 target | 理由 |
|------|---------|--------------|------|
| **讀** `batchRead` | `LoadBalance`（`StorageClientImpl.cc:1647`） | serving 副本中累積 IO 最少者 | CRAQ：已提交資料任一副本都能服務 → 讀分散 |
| **寫** `batchWrite` | `HeadTarget`（`StorageClientImpl.cc:1805`） | chain 頭 | 寫必須從 HEAD 沿鏈 forward |
| query/remove/truncate | `HeadTarget` | chain 頭 | 變更/強一致 |

讀取執行（`BatchReadJob`）：io_uring/libaio 用 **fixed file + fixed buffer**（`AioStatus.cc:230`）從 disk 讀進 server 端註冊 buffer，再**單邊 RDMA Write 直寫 client 的 rdmabuf**（`StorageOperator.cc:187-218`）。4096 對齊由 head/tail padding 處理，回寫前 `subrange(headLength, length)` 去 padding。**client 收到 RPC 回應時資料已在自己 buffer**（零拷貝）。

### 3.7 一致性：雙層鎖 + per-disk 串行

| 鎖 | 位置 | 作用 |
|----|------|------|
| **channel 鎖**（非阻塞 tryLock） | `ReliableUpdate.cc:56` | 同一 client-channel 串行 + 冪等重試 |
| **chunk 鎖**（阻塞 FIFO） | `StorageOperator.cc:371` | 同一 chunk 跨 client 互斥（涵蓋 doUpdate+forward+commit 全程） |

chunk 鎖底層是 `CoLockManager`（per-key `std::queue<Baton>` FIFO）。落盤層 `UpdateWorker` 依 `diskIndex` 分佇列（**每碟一條，同碟串行、跨碟並行**）。

### 3.8 ReliableForwarding 可靠性

`forwardWithRetry`（`ReliableForwarding.cc:33`）用指數退避（首 100ms / 最大 1s / 總 60s），**每次重試重新解析路由**（後繼可能換）。`kNoSuccessorTarget`（TAIL）不算錯誤。資料損毀自保：後繼回 `kChecksumMismatch` 時本地重算 buffer checksum，若發現本地已損毀直接 `SIGUSR2` 自殺（`ReliableForwarding.cc:264`），避免壞資料傳播。

---

## 4. Target 狀態機與 Chain 管理（mgmtd）

### 4.1 兩個正交狀態維度

| 維度 | 列舉 | 由誰決定 | 值 |
|------|------|---------|-----|
| **LocalTargetState** | `MgmtdTypes.h:21` | **storage 自報**（事實層） | UPTODATE / ONLINE / OFFLINE |
| **PublicTargetState** | `MgmtdTypes.h:10` | **mgmtd 裁決**（決策層） | SERVING / LASTSRV / SYNCING / WAITING / OFFLINE |

- `ONLINE`：已上線可服務但資料未必最新；`UPTODATE`：已追平 HEAD。
- `SERVING`：正常服務參與鏈；`SYNCING`：正從前驅同步；`WAITING`：上線但暫無同步來源；`LASTSRV`：所有 SERVING 都掛時保留「最後服務者」（資料最全，整鏈等它）。

### 4.2 狀態轉換規則（`generateNewChain`，`mgmtd/service/updateChain.cc:25-104`）

mgmtd 每 1s 跑 `generateNewChain`，依各 target 的 localState + 鏈內是否已有 SERVING/SYNCING 重算 publicState：

| 舊 Public | Local | 條件 | → 新 Public |
|-----------|-------|------|-------------|
| SERVING | ONLINE/UPTODATE | — | **SERVING** |
| SERVING | OFFLINE | 尚無 LASTSRV（第一個掛的） | **LASTSRV** |
| SERVING | OFFLINE | 已有 LASTSRV | **OFFLINE** |
| LASTSRV | ONLINE/UPTODATE | 還沒有 SERVING | **SERVING** |
| LASTSRV | * | 已有 SERVING | **OFFLINE** |
| SYNCING | UPTODATE | — | **SERVING**（同步完成畢業） |
| SYNCING | ONLINE | 有 SERVING | **SYNCING**（繼續） |
| SYNCING | ONLINE | 無 SERVING | **WAITING** |
| WAITING/OFFLINE | ONLINE | 有 SERVING 且**無其他 SYNCING** | **SYNCING**（開始同步） |
| * | OFFLINE | — | **OFFLINE** |

**三個不變量**：① 一條鏈同時最多一個 SYNCING；② 有 SERVING 就清掉所有 LASTSRV；③ 漸進式（恢復路徑 `OFFLINE→WAITING→SYNCING→SERVING`，每步需一輪新心跳）。

重組後鏈順序恆為 `[SERVING…][LASTSRV][SYNCING][WAITING…][OFFLINE…]`（`updateChain.cc:98`），SYNCING 永遠排在 SERVING 之後（符合 CRAQ「新副本從尾端追資料」）。

### 4.3 RoutingInfo 生成與三層版本

```
storage 心跳上報 localState
   → RoutingInfo::localUpdateTargets（更新 target.localState + ts）
   → MgmtdChainsUpdater（每 1s）掃 ts 變動的 target → generateNewChain
   → appendChangedChains：oldTargets != newTargets 才 chainVersion++，寫 FDB + 套記憶體
   → MgmtdRoutingInfoVersionUpdater（每 5s）批次 routingInfoVersion++
   → client getRoutingInfo(version) pull（版本相同回 nullopt 省頻寬）
```

三層版本：`chainVersion`(單鏈 u32) < `chainTableVersion`(鏈表，多版本保留) < `routingInfoVersion`(全域 u64，單調遞增)。所有寫入先寫 FDB 後改記憶體，保證崩潰後單調。

### 4.4 失效偵測（心跳 60s）

`MgmtdHeartbeatChecker`（每 10s 檢查，門檻 `heartbeat_fail_interval=60s`）只把失聯 target 的 `localState` 打成 `OFFLINE` 並更新 `ts`，**不直接改 publicState**——交由下一輪 `generateNewChain` 處理（職責分離）。

完整失效時間線：

```
target 失聯 → 最多 60s 後 mgmtd 標 localState=OFFLINE
            → 最多再 1s 後 generateNewChain 降級（HEAD 失效則先變 LASTSRV）並重排鏈
            → 最多再 5s 後 routingInfoVersion++
            → client 下次 pull 拿到新拓樸
```

另有 **NewBorn 出生保護**（新建鏈/mgmtd 重啟後 2min 內不重組，`MgmtdConfig.h:17`）與 **stale 心跳過濾**（storage 上報的 chainVersion 比 mgmtd 舊則忽略，防狀態機被往回拉）。

---

## 5. 資料回復（Resync）

### 5.1 方向性（最關鍵前提）

鏈式複製只有單向 `successor`、**無 predecessor**。所以 resync 是：

> **由「失效副本的前驅（一個 SERVING 副本）」主動把資料 PUSH 給「正在 SYNCING 的後繼副本」。**

每個 storage node 跑一個 `ResyncWorker`，只在「自己某 target 的直接後繼是 SYNCING」時動作（自己當資料源）。SYNCING 那方被動：回應 `syncStart`(交 metadata)、透過正常 update RPC 收被轉發的寫入、回應 `syncDone`(標自己 UPTODATE)。

### 5.2 完整流程（syncStart#8 → 比對 → 傳輸 → syncDone#9）

RPC 方法號（`fbs/storage/Service.h:15-16`）：`syncStart=8`、`syncDone=9`。全程在 `ResyncWorker::handleSync`（`ResyncWorker.cc:101-387`）：

```
ResyncWorker 主迴圈（每 500ms 掃 syncingChains，30s 去抖）   ResyncWorker.cc:66
└─ handleSync：
   A. 取本地(前驅)target + 後繼位址                          :121
   B. syncStart(#8) → 取回後繼全量 metadata (remoteMetas)    :162
      └─ server: StorageOperator::syncStart（後繼須 SYNCING+ONLINE，交 getAllMetadata）:1007
   C. 讀本地(前驅)全量 metadata (localMetas)，再 re-check 鏈版本 :183
   D. 比對差異 → writeList / removeList                       :199 (見 5.3)
   E. 批次 forward（先 remove 後 write，每批 16，並發 64）     :305
      └─ 每批前重取最新路由，任一失敗整個中止
   F. syncDone(#9) → 後繼 localState = UPTODATE              :356
      └─ server: syncDone → TargetMap::syncReceiveDone（:111）→ mgmtd 下輪升回 SERVING
```

### 5.3 差異比對（五維決策樹，`ResyncWorker.cc:203-292`）

比對「前驅 localMetas」vs「後繼 remoteMetas」，用 `chainVer + updateVer + commitVer + chunkState + checksum` 五維交叉判斷：

| 條件 | 判定 | 動作 |
|------|------|------|
| 後繼有、前驅無 | 後繼多餘 | **removeList** |
| 前驅 chainVer 較高 | 後繼落後 | **forward write** |
| 後繼 chunk 未提交 | 需覆蓋 | **forward write** |
| 前驅有、後繼無 | 後繼缺失 | **writeList** |
| 完全一致 | — | skip |
| **前驅落後於後繼且已提交** | 不該發生 | **FATAL → 強制下線後繼 + 中止** |
| checksum 不符且非當前鏈寫入 | 疑損毀 | **FATAL → 中止** |

> 註：resync 用的是 `syncStart`(#8) 取後繼 metadata，而非 `getAllChunkMetadata`(#13，那是給 CLI 查詢用)。

### 5.4 Full-Chunk-Replace（chunk 級增量 + chunk 內全量）

- **chunk 級增量**：只補有版本/checksum 差異的 chunk（相同的跳過）。
- **chunk 內全量替換**：每個要傳的 chunk **整塊讀出、整塊覆寫**（offset=0、整個 chunkSize），不做 byte-range 增量。

整塊讀實作在轉發層 `ReliableForwarding::doForward`（`ReliableForwarding.cc:158-213`）：偵測後繼 SYNCING 時 `readForSyncing=true`，讀整塊（`readUncommitted=true` 拿最新內容）後轉發；小塊走 inline 省 RDMA。接收端 `ChunkReplica::update` 的 `isSyncing` 分支（`ChunkReplica.cc:211`）直接採用前驅版本（繞過單調遞增檢查）、整塊覆蓋。

`full_sync_level` 開關：`NONE`(預設，只補差異) / `HEAVY`(強制重傳所有 chunk，懷疑損毀時用)。

### 5.5 Resync 期間的寫入一致性

SYNCING 後繼同時承受兩股流量（線上即時寫入 + 背景補洞），靠四個機制保證一致：

1. **前驅 per-chunk 鎖序列化**：線上寫入(`handleUpdate`)與 resync(`forward`)對前驅同一 chunk 加**同一把鎖**（`ResyncWorker.cc:398` + `StorageOperator.cc:371`），兩股流量互斥不交錯。
2. **鎖後 re-check 競態**：`forward` 取鎖後重查 chunk 現狀，處理「比對快照→實際轉發」間 chunk 已變化（write/remove 競態，`ResyncWorker.cc:404-424`）。
3. **後繼即寫即提交**：後繼通常是 tail，同步寫走三段式 write→forward(無後繼,成功)→commit，不留未提交尾巴。
4. **全程鏈版本重檢**：handleSync 與每批都反覆 `getByChainId`，鏈拓樸變動即中止。

線上部分寫（`length != chunkSize`）到達 SYNCING 後繼的前驅時，前驅會**自動整塊讀後轉發完整 chunk**（確保後繼即使缺前綴也得到完整內容）。

### 5.6 CheckWorker（巡檢，非完整性掃描）

`CheckWorker`（每 100ms）做磁碟健康 + target 重新上線 + 心跳/容量上報，**不做 chunk 級完整性掃描**（那由寫入 checksum 與 resync checksum 比對負責）。六項工作中與復原最相關：

- **重載 OFFLINE target**（`CheckWorker.cc:124`）：磁碟修好後重新 `loadTarget` → 本地 ONLINE → mgmtd 才能轉 SYNCING → 觸發 resync。這是「失效節點復原」的入口。
- **磁碟探測**（每 3s 寫 `.hf3fs_check`）：失敗即 `offlineTargets` 下線該碟所有 target。
- **觸發心跳**（每 1s）：把 localState（含 resync 完成的 UPTODATE）回報 mgmtd，是 resync 畢業的管道。

---

## 6. 端到端資料流總結

### 6.1 寫一個檔案（client write）

```
1. client：File::getChunkId/getChainId 把 (inode,off,len) 切成多個 (ChunkId, ChainId) 的 WriteIO（PioV）
2. 路由：每個 WriteIO 選 HeadTarget → 送到該 chain 的 HEAD target 所在 node
3. HEAD：handleUpdate → 鎖 chunk → RDMA Read 從 client 拉資料 → 本地寫 pending(updateVer+1,CLEAN)
4. HEAD → forward 給後繼（阻塞）→ 後繼 RDMA Read 從 HEAD 拉 → 寫 pending → 再 forward…
5. TAIL：無後繼 → 本地 commit(COMMIT) → 回溯
6. 中間/HEAD：後繼返回後校驗 checksum 一致 → 本地 commit
7. HEAD 回 client 成功
```

### 6.2 讀一個檔案（client read）

```
1. client：切成多個 ReadIO（ChunkId, ChainId, chunk 內 offset/len），帶自己 rdmabuf 描述子
2. 路由：每個 ReadIO 選 LoadBalance（任一 serving 副本）→ 分組按 node 批次平行送
3. storage：batchRead → 檢查 commitVer==updateVer（pending 且不允許 uncommitted 則拒）
4. io_uring/libaio fixed buffer 從 disk 讀進 server 註冊 buffer
5. 單邊 RDMA Write 直寫 client 的 rdmabuf（零拷貝）
6. client 收到 RPC 回應時資料已在 buffer；失敗則 failover 換副本重試
```

### 6.3 一個 target 失效到復原

```
1. 磁碟故障 / 節點離線 → 心跳停
2. ≤60s：mgmtd 標 localState=OFFLINE
3. ≤1s：generateNewChain 把它從 SERVING 降級（若是 HEAD→先 LASTSRV）；剩餘副本繼續服務
4. 磁碟修復重掛 → CheckWorker loadTarget → localState=ONLINE
5. mgmtd 看到「有 SERVING + 無其他 SYNCING + ONLINE」→ 標 SYNCING
6. 前驅 ResyncWorker：syncStart 取後繼 metadata → 五維比對 → 整塊 push 差異 chunk → syncDone
7. 後繼 localState=UPTODATE → mgmtd 下輪 SYNCING→SERVING（畢業，重新加入服務）
```

---

## 7. 附錄

### 7.1 關鍵參數

| 參數 | 預設 | 出處 |
|------|------|------|
| chunk size 級別 | 64KiB ~ 64MiB（11 種，×2 遞增） | `chunk_engine/types/constants.rs` |
| chunk engine cluster 數/級別 | 256 | `file/clusters.rs:18` |
| group 槽數 | 256 | `types/group_state.rs:14` |
| AIO 對齊 | 4096 B | `Common.h:80` |
| 心跳超時 | 60s | `MgmtdConfig.h:16` |
| chain 重組間隔 | 1s | `MgmtdConfig.h:22` |
| routingInfoVersion 提升 | 5s | `MgmtdConfig.h:24` |
| NewBorn 出生保護 | 2min | `MgmtdConfig.h:17` |
| resync 掃描/去抖 | 500ms / 30s | `ResyncWorker.cc:66` |
| resync 批次/並發 | 16 / 64 | `ResyncWorker.h:31,36` |
| 讀批次 | 128 筆 / 4MB | `StorageClientImpl` |
| failover 門檻 | 1 次失敗 | `StorageClient.h:366` |

### 7.2 狀態列舉

```
PublicTargetState  : INVALID / SERVING / LASTSRV / SYNCING / WAITING / OFFLINE   (MgmtdTypes.h:10)
LocalTargetState   : INVALID / UPTODATE / ONLINE / OFFLINE                        (MgmtdTypes.h:21)
ChunkState         : COMMIT(0) / DIRTY(1) / CLEAN(2)                              (storage/Common.h:60)
UpdateType         : WRITE / REMOVE / TRUNCATE / EXTEND / COMMIT                  (storage/Common.h:51)
```

### 7.3 ID 型別

| 型別 | 底層 | 出處 |
|------|------|------|
| `InodeId` | u64 | `fbs/meta` |
| `ChunkId` | 16 byte | `Schema.h:177`（meta）/ `Common.h:82`（storage，string 包裝） |
| `TargetId` | u64 | `MgmtdTypes.h:44` |
| `ChainId` | u32 | `MgmtdTypes.h:55` |
| `ChainVersion` | u32 | `MgmtdTypes.h:51` |
| `ChainTableId` / `ChainTableVersion` | u32 | `MgmtdTypes.h:53-54` |
| `RoutingInfoVersion` | u64 | `MgmtdTypes.h:46` |

### 7.4 檔案索引

| 主題 | 檔案 |
|------|------|
| 檔案切片（chunk/chain 計算） | `fbs/meta/Schema.cc`（`getChunkId`/`getChainOfChunk`）、`fuse/PioV.cc` |
| ChunkId / Layout / File | `fbs/meta/Schema.h` |
| Chain/Target/Node 型別 | `fbs/mgmtd/{ChainRef,ChainInfo,ChainTargetInfo,TargetInfo,ChainTable,RoutingInfo,MgmtdTypes,NodeInfo}.h` |
| RoutingInfo 解析 | `fbs/mgmtd/RoutingInfo.cc`（`getChainId`） |
| **CRAQ 寫入/提交** | `storage/service/StorageOperator.cc`、`ReliableUpdate.cc`、`ReliableForwarding.cc`、`store/ChunkReplica.cc` |
| **讀取** | `storage/service/StorageOperator.cc`(batchRead)、`aio/{BatchReadJob,AioReadWorker,AioStatus}.cc` |
| client 切片/路由 | `client/storage/{StorageClientImpl,TargetSelection}.cc` |
| storage target 狀態 | `storage/service/TargetMap.cc`、`store/StorageTarget.cc` |
| **chunk engine（物理儲存）** | `storage/chunk_engine/src/{core/engine,alloc/*,meta/*,types/*,file/*,cxx}.rs`、`store/ChunkEngine.{h,cc}` |
| **mgmtd 狀態機** | `mgmtd/service/updateChain.cc`、`MgmtdData.cc`、`background/Mgmtd*Checker.cc` |
| **resync** | `storage/sync/ResyncWorker.cc`、`worker/CheckWorker.cc` |
| RPC 結構 | `fbs/storage/Common.h`、`fbs/storage/Service.h` |

---

## 一頁總結

1. **資料分佈是 12 層映射**：`inode+offset → chunkIndex → ChunkId(16B) → stripe → chainIndex → ChainRef → ChainId → ChainInfo → Target → Node → Disk → chunk engine position`。`chunkSize`(2 的冪)決定垂直切塊、`stripeSize`決定水平條帶化（跨 node 平行）、chainIndex 經 chain table 1-based 取模環繞解析成全域 ChainId。
2. **Chunk 實體儲存**：11 種 64KiB–64MiB 固定大小級別，每級 256 cluster 檔，檔內切 group(256 槽)，Position 是打包 u64；用 `fallocate`/punch-hole 管理空間；metadata 存 RocksDB(7 種前綴 key)；COW 配合 CRAQ pending/commit，舊版本靠 refcount 保留。
3. **CRAQ**：寫入 HEAD→TAIL（逐跳 RDMA Read 拉資料），commit TAIL→HEAD 回溯（靠 forward 阻塞語意）；三版本(commitVer/updateVer/chainVer)+ChunkState(COMMIT/DIRTY/CLEAN)實現 pending/committed；讀分散到任一 serving 副本(LoadBalance)、寫到 HEAD；雙層鎖+per-disk 串行保證一致。
4. **Target 狀態機**：storage 報 localState(事實)，mgmtd 算 publicState(決策)，`generateNewChain` 是純函式狀態機；恢復路徑 `OFFLINE→WAITING→SYNCING→SERVING` 漸進、一鏈一次一個 SYNCING；HEAD 失效先變 LASTSRV(資料最全保護位)。
5. **Resync**：前驅 SERVING 主動 PUSH 給後繼 SYNCING；syncStart(#8)取 metadata → 五維(chainVer/updateVer/commitVer/state/checksum)比對 → chunk 級增量+chunk 內整塊替換 → syncDone(#9)畢業；靠前驅 per-chunk 鎖序列化線上寫入與背景補洞。
