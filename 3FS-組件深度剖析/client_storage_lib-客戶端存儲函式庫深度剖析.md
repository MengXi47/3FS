# 3FS 客戶端存儲函式庫（`src/client/storage/`）深度剖析

> 對象：`src/client/storage/` 全部 13 個原始檔（4432 行程式碼 + 1 行 CMakeLists）
> 邊界：`src/fbs/storage/`（RPC 型別與服務定義）、`src/client/mgmtd/RoutingInfo.h`（路由快照）、`src/common/net/`（RDMA/serde 傳輸層）僅讀懂介面
> 對側視角請參閱 `storage_main-存儲服務深度剖析.md`；本篇是同一條 IO 路徑的客戶端半邊

---

## 0. 一句話總結

`storage-client` 是一個**無狀態、以 chunk 為單位、批次化 + 協程化**的 RPC 客戶端函式庫：它接受使用者已經算好的 `(chainId, chunkId, chunk 內 offset, length)`，用 mgmtd 推送的路由快照把每個操作解析成一個具體的 `(targetId, nodeId)`，按 **nodeId** 分組聚合成批次 RPC，經 RDMA 送給 storage 服務，並在上面套一層**指數退避重試 + target 級 failover + channel 級去重**的可靠性外殼。

它**不**做的事同樣重要：它不知道 inode、不知道檔案 offset、不做 chunk 切分定址、不維護任何 chunk 快取、不管 CRAQ 的版本推進——那些分別是呼叫端（`src/fuse/PioV.cc`、`src/meta/`）與服務端的責任。

---

## 1. 邊界：這個函式庫的輸入從哪來

### 1.1 「從 `(inode, offset, length)` 算到 chunk」不在這個目錄裡

任務描述裡問「怎麼從 `(inode, offset, length)` 算到 chunk」——答案是：**這一步發生在呼叫端，`storage-client` 從來沒看過 inode**。

證據在 `src/fuse/PioV.cc:98-120`：

```cpp
Result<Void> PioV::chunkIo(
    const meta::Inode &inode, uint16_t track, off_t off, size_t len,
    std::function<void(storage::ChainId, storage::ChunkId, uint32_t, uint32_t, uint32_t)> &&consumeChunk) {
  const auto &f = inode.asFile();
  auto chunkSize = f.layout.chunkSize;
  auto chunkOff = off % chunkSize;
  ...
    auto chain = f.getChainId(inode, opOff, *routingInfo_, track);
    auto fchunk = f.getChunkId(inode.id, opOff);
    auto chunk = storage::ChunkId(*fchunk);
```

`FileLayout::getChainId()` / `getChunkId()` 屬於 `src/fbs/meta/`，切分迴圈屬於 FUSE 層。切好之後才呼叫：

```cpp
rios_.emplace_back(storageClient_.createReadIO(chain, chunk, chunkOff, chunkLen,
                                               (uint8_t *)buf + bufOff, &memh,
                                               reinterpret_cast<void *>(idx)));
```
（`src/fuse/PioV.cc:42-48`）

所以本函式庫的定址單位是：**「第 `chainId` 條複製鏈上、id 為 `chunkId` 的那個 chunk，的第 `offset` 到 `offset+length` 個位元組」**。`ChunkId` 對它而言是一個不透明的位元組串（`hf3fs::storage::ChunkId`，`src/fbs/storage/Common.h:82-110`），只有比較與排序語意。

唯一一個跟「檔案 offset」沾邊的函式是 `GlobalKey::fromFileOffset()`（`src/fbs/storage/Common.cc:98-107`），但它在 `fbs` 層，且在整個 `src/client/storage/` 裡**沒有任何呼叫點**。

### 1.2 已知的呼叫端

| 呼叫端 | 用途 | 入口 |
|---|---|---|
| `src/fuse/PioV.cc` | FUSE / USRBIO 的批次讀寫 | `createReadIO` / `createWriteIO` + `batchRead` / `batchWrite` |
| `src/lib/common/Shm.cc:103` | USRBIO 共享記憶體分段註冊 | `registerIOBuffer` |
| `src/client/meta/MetaClient.cc` | meta 客戶端持有 storage client | `truncateChunks` / `queryLastChunk` |
| `src/meta/store/` | meta 服務端（truncate / GC） | `queryLastChunk` / `removeChunks` / `truncateChunks` |
| `src/client/cli/admin/*` | `admin_cli` 的 `query-chunk` / `fill-zero` / `remove-chunks` / `read-bench` | 全部 API |
| `tests/`、`benchmarks/` | 測試與壓測 | 全部 API |

`StorageClient.h:537` 有一句原始碼註解明確標示了這條分界：

```cpp
// the following interfaces are assumed to be used at server-side (e.g. in meta service)
```

也就是 `queryLastChunk` / `removeChunks` / `truncateChunks` / `querySpaceInfo` / `createTarget` / `offlineTarget` / `removeTarget` / `queryChunk` / `getAllChunkMetadata` 這九個，作者預期由服務端（meta）或管理工具呼叫，而不是資料面客戶端。

---

## 2. 對外 API 全表

### 2.1 IO 物件模型

整個函式庫的物件關係是一個扁平的兩層繼承 + 三個獨立 op：

```
folly::MoveOnly
   │
   ├── IOBuffer                    ← 已註冊的 RDMA 記憶體控制代碼（delete 即反註冊）
   │      const net::RDMABuf rdmabuf
   │
   ├── IOBase                      ← 讀寫共用欄位
   │      RoutingTarget routingTarget   ← 動態解析結果 + UpdateChannel
   │      ChunkId chunkId
   │      const uint32_t offset / length / chunkSize
   │      uint8_t *const data           ← 使用者緩衝區內的位址
   │      IOBuffer *const buffer        ← data 必須落在這個 buffer 內
   │      void *const userCtx           ← 使用者透傳，函式庫完全不碰
   │      IOResult result
   │      │
   │      ├── ReadIO       + RequestId requestId; std::vector<ReadIO> splittedIOs
   │      └── WriteIO      + const RequestId requestId; ChecksumInfo checksum
   │
   ├── QueryLastChunkOp    RoutingTarget + ChunkIdRange range + QueryLastChunkResult
   ├── RemoveChunksOp      RoutingTarget + ChunkIdRange range + RemoveChunksResult
   └── TruncateChunkOp     RoutingTarget + chunkId/chunkLen/chunkSize/onlyExtendChunk + IOResult
```

五個 op 型別都提供同一組「鴨子型別」成員函式，這是整個實作能用同一套模板處理它們的關鍵（`StorageClient.h:88-95, 241-247, 273-279, 313-319`）：

| 成員 | 語意 |
|---|---|
| `status()` / `statusCode()` | 從 `result` 取出狀態 |
| `resultLen()` | 成功時的資料長度（query/remove/truncate 一律回 0） |
| `dataLen()` | 這個 op 的資料位元組數，用於批次位元組上限（query/remove/truncate 回 0） |
| `chunkRange()` | 回 `ChunkIdRange`；單 chunk op 回 `{chunkId, chunkId, 1}` |
| `numProcessedChunks()` | 成功時處理了幾個 chunk |
| `resetResult()` | 重試前清空結果 |

`RoutingTarget` 的解構子有一個洩漏偵測（`StorageClient.h:26-32`）：

```cpp
~RoutingTarget() {
  XLOGF_IF(DFATAL, channel.id != ChannelId{0},
           "Leaked update channel, routing target: {}, stack trace: {}",
           *this, folly::symbolizer::getStackTraceStr());
}
```

也就是說：**任何 op 銷毀時若還握著 update channel，debug build 會直接 DFATAL 並印堆疊**。這是 §7.2 channel 生命週期管理的最後一道防線。

### 2.2 工廠方法（都在基底類別，非 virtual dispatch 的實質差異）

| 方法 | 簽章要點 | requestId 來源 | 定義 |
|---|---|---|---|
| `createReadIO` | `(chainId, chunkId, offset, length, data, buffer, userCtx)` | **不配**（送出時由 batch 統一配） | `StorageClient.cc:48-56` |
| `createWriteIO` | `(chainId, chunkId, offset, length, chunkSize, data, buffer, userCtx)` | `nextRequestId_.fetch_add(1)` | `StorageClient.cc:58-68` |
| `createQueryOp` | `(chainId, chunkIdBegin, chunkIdEnd, maxNumChunkIdsToProcess=1, userCtx)` | **不配** | `StorageClient.cc:70-76` |
| `createRemoveOp` | 同上 | `nextRequestId_.fetch_add(1)` | `StorageClient.cc:78-85` |
| `createTruncateOp` | `(chainId, chunkId, chunkLen, chunkSize, onlyExtendChunk=false, userCtx)` | `nextRequestId_.fetch_add(1)` | `StorageClient.cc:87-95` |
| `registerIOBuffer` | `(uint8_t *buf, size_t len) -> Result<IOBuffer>` | — | `StorageClient.cc:97-110` |

**requestId 的兩種配置時機**是一個真實的不對稱：

- **讀 / query**：`requestId` 是 `ReadIO::requestId` / `QueryLastChunkOp::requestId`（非 const），在 `buildBatchRequest()` 裡**整批共用一個** `RequestId`，並回寫到每個 op（`StorageClientImpl.cc:684-696`、`:738-742`）。
- **寫 / remove / truncate**：`requestId` 是 `const`，在 `create*Op()` 當下就從 `nextRequestId_` 取，**每個 op 一個**，並在 `buildBatchRequest()` 裡逐 op 塞進各自的 `MessageTag`（`StorageClientImpl.cc:773`、`:799`）。

這個差異與去重機制直接相關：更新類操作的 `MessageTag` 必須逐 op 唯一，因為服務端會用 `(clientId, requestId)` 做一致性校驗（見 §7.3）。

### 2.3 八個非同步操作 + 五個管理操作

全部宣告在 `StorageClient.h:519-564`，全部是 `virtual ... = 0`（`StorageClient` 是純介面）。

| 方法 | 簽章 | 語意 | 批次分組 | 目標選取預設 | 重試 |
|---|---|---|---|---|---|
| `batchRead` | `(span<ReadIO>, UserInfo, ReadOptions, vector<ReadIO*>* failedIOs)` | 批次讀 | per-node | `LoadBalance` | ✅ |
| `read` | `(ReadIO&, UserInfo, ReadOptions)` | 單筆讀 | — | `LoadBalance` | ✅ |
| `batchWrite` | `(span<WriteIO>, UserInfo, WriteOptions, vector<WriteIO*>*)` | 批次寫 | per-node，**節點內序列送** | `HeadTarget` | ✅ |
| `write` | `(WriteIO&, UserInfo, WriteOptions)` | 單筆寫 | — | `HeadTarget` | ✅ |
| `queryLastChunk` | `(span<QueryLastChunkOp>, UserInfo, ReadOptions, vector<...>*)` | 查範圍內字典序最大的 chunk + 總長度/總數 | per-node | `HeadTarget` | ✅ |
| `removeChunks` | `(span<RemoveChunksOp>, UserInfo, WriteOptions, vector<...>*)` | 刪除 `[begin, end)` 範圍內的 chunk | per-node | `HeadTarget` | ✅ |
| `truncateChunks` | `(span<TruncateChunkOp>, UserInfo, WriteOptions, vector<...>*)` | 截斷/擴展單 chunk，不存在就建立 | per-node | `HeadTarget` | ✅ |
| `querySpaceInfo` | `(NodeId) -> SpaceInfoRsp` | 查某節點磁碟空間 | 單發 | 直接指定 nodeId | ❌ |
| `createTarget` | `(NodeId, CreateTargetReq) -> CreateTargetRsp` | 建 target | 單發 | 直接指定 nodeId | ❌ |
| `offlineTarget` | `(NodeId, OfflineTargetReq)` | 下線 target | 單發 | 直接指定 nodeId | ❌ |
| `removeTarget` | `(NodeId, RemoveTargetReq)` | 移除 target | 單發 | 直接指定 nodeId | ❌ |
| `queryChunk` | `(QueryChunkReq) -> vector<Result<QueryChunkRsp>>` | 對鏈上**每個** target 各發一次，回傳 N 份結果 | 序列走訪 | 走 `chainInfo.targets` 全部 | ❌ |
| `getAllChunkMetadata` | `(ChainId, TargetId) -> ChunkMetaVector` | 取單一 target 的全量 chunk metadata | 單發 | 校驗 target 在鏈上 | ❌ |

三個語意要點，都有原始碼註解直接支持：

**（1）`removeChunks` 的 `numChunksRemoved` 是下界，不是準確值。** `StorageClient.h:488-494`：

> Note that the `numChunksRemoved' in `RemoveChunksResult' might be less or equal to the number of chunks actually removed by storage service if the request fails and is automatically retried until it succeeds.

原因見 §7.3：remove 每次重試都會換新的 channel seqnum，所以服務端會重新掃描並重新計數，而客戶端只保留最後一次的計數。

**（2）`queryLastChunk` / `removeChunks` 的 `moreChunksInRange`** 表示範圍內的 chunk 數超過 `maxNumChunkIdsToProcess`，需要呼叫端分頁再打一次（`StorageClient.h:476-499`）。

**（3）`truncateChunks` 的結果需要呼叫端自己驗**。`StorageClient.h:501-508`：

> The truncated/extended chunk size is returned as `lengthInfo' in the IO result; the user should check if the chunk size is expected.

`TruncateChunkOp::result` 的欄位註解也重複了一次（`StorageClient.h:329`）：`// result.lengthInfo == chunkLen if the op succeeds`。

**`failedIOs` / `failedOps` 出參的語意**：這是一個**選填**的收集器。若傳 `nullptr`，實作會在內部建一個區域 vector 頂替（例：`StorageClientImpl.cc:1571-1572`），因此**不傳並不會關掉失敗收集，只是拿不到清單**。所有五個批次方法的回傳值都是 `co_return Void{}`——**函式本身幾乎不會回錯**，錯誤全部落在每個 op 自己的 `result` 上。呼叫端必須逐 op 檢查，這是最容易誤用的一點。

### 2.4 錯誤碼全表

`StorageClientCode` 定義在 `src/common/utils/StatusCodeDetails.h:220-243`：

| 碼 | 名稱 | 客戶端如何分類 | 產生點（客戶端側） |
|---|---|---|---|
| 7000 | `kInitFailed` | — | 未在本目錄使用 |
| 7001 | `kMemoryError` | **永久** | `registerIOBuffer` 註冊失敗（`StorageClient.cc:108`） |
| 7002 | `kInvalidArg` | **永久** | 緩衝區校驗、chunk 範圍校驗、chunkLen>chunkSize、target 不在鏈上 |
| 7003 | `kNotInitialized` | 中性（尚未有結果） | `IOResult::lengthInfo` 的預設值（`Common.h:222`） |
| 7004 | `kRoutingError` | 可重試 | 找不到 chain/node/target、node 型別不對、routingInfo 為 null |
| 7005 | `kNotAvailable` | 可重試 | 鏈上無可用 target、`ManualMode` 索引越界、target 非 SERVING |
| 7006 | `kCommError` | **暫時不可用**（記 failover） | RPC `kSendFailed`/`kRequestRefused` 轉換而來 |
| 7007 | `kChunkNotFound` | **永久** | 服務端 `kChunkMetadataNotFound` 轉換而來 |
| 7008 | `kTimeout` | **暫時不可用**（記 failover） | RPC `kTimeout` 轉換；重試放棄時強制覆寫 |
| 7009 | `kBadConfig` | **永久** | 未在本目錄使用 |
| 7010 | `kRemoteIOError` | **暫時不可用**（記 failover） | 服務端 `kChunkReadFailed`；故障注入 |
| 7011 | `kServerError` | 可重試 | 所有未特別對應的服務端錯誤的兜底 |
| 7012 | `kResourceBusy` | 可重試 | **channel 配不到**；服務端 `kChannelIsLocked` |
| 7013 | `kDuplicateUpdate` | — | **收到即 DFATAL「[BUG]」**（見 §7.3） |
| 7014 | `kRoutingVersionMismatch` | **快速重試** | 送出前路由已過期；服務端 `kChainVersionMismatch` |
| 7015 | `kChecksumMismatch` | **永久** | 本地驗 checksum 失敗；服務端同名錯誤 |
| 7016 | `kNoRDMAInterface` | 可重試 | 目標節點沒有任何 RDMA 端點（`StorageClientImpl.cc:886`） |
| 7017 | `kProtocolMismatch` | **永久** | RPC `kVerifyRequestFailed`/`kVerifyResponseFailed` |
| 7018 | `kRequestCanceled` | **永久** | `RequestInfo::get()->canceled()`（`StorageClientImpl.cc:838-841`） |
| 7019 | `kReadOnlyServer` | **永久** | 服務端 `kReadOnlyMode` |
| 7020 | `kChunkNotCommit` | **快速重試** | 服務端同名錯誤（CRAQ 髒讀） |
| 7021 | `kNoSpace` | **永久** | 服務端錯誤轉換 |
| 7999 | `kFoundBug` | **永久** | 回應數量對不上 op 數量；inline buffer 長度對不上 |

服務端錯誤碼到客戶端錯誤碼的轉換表在 `src/common/utils/StatusCodeConversion.cc:5-38`，並有一個兜底規則：

```cpp
if (StatusCode::typeOf(status.code()) == StatusCodeType::StorageClient) {
  return status;                                        // 已經是客戶端碼，原樣保留
} else {
  return status.convert(StorageClientCode::kServerError);  // 其餘一律 kServerError
}
```

### 2.5 三層設定

```
StorageClient::Config                     ← 程序級，建構時綁定
├── net_client              : net::Client::Config      （讀路徑用）
├── net_client_for_updates  : net::Client::Config      （更新路徑用，需開關啟用）
├── retry                   : RetryConfig
│     init_wait_time = 10_s / max_wait_time = 30_s / max_retry_time = 60_s
│     max_failures_before_failover = 1
├── traffic_control         : TrafficControlConfig
│     read  : HotLoadOperationConcurrency   ← 熱更新
│     write : OperationConcurrency          ← 不可熱更新
│     query : HotLoadOperationConcurrency   ← 熱更新
│     remove: OperationConcurrency          ← 不可熱更新
│     truncate: OperationConcurrency        ← 不可熱更新
├── implementation_type     = RPC | InMem   （非熱更新）
├── chunk_checksum_type     = CRC32C
├── create_net_client_for_updates = false
├── check_overlapping_read_buffers  = true   ← 熱更新
├── check_overlapping_write_buffers = false  ← 熱更新
├── max_inline_read_bytes  = 0               ← 熱更新
├── max_inline_write_bytes = 0               ← 熱更新
└── max_read_io_bytes      = 0               ← 熱更新（0 = 不切分）

IoOptions（每次呼叫傳入，非設定物件的一部分）
├── read  : ReadOptions  { debug, retry, targetSelection, enableChecksum=false, allowReadUncommitted=false }
└── write : WriteOptions { debug, retry, targetSelection, enableChecksum=true }
```

`OperationConcurrency` 與 `HotLoadOperationConcurrency` 兩個類別**欄位完全相同**（`max_batch_size=128`、`max_batch_bytes=4_MB`、`max_concurrent_requests=32`、`max_concurrent_requests_per_server=8`、`random_shuffle_requests=true`、`process_batches_in_parallel=true`），唯一差別是前者用 `CONFIG_ITEM`（`StorageClient.h:379-384`）、後者用 `CONFIG_HOT_UPDATED_ITEM`（`StorageClient.h:388-393`）。讀與 query 走熱更新版，寫/刪/截斷走非熱更新版。

`RetryOptions` 的三個時間欄位預設 `Duration::zero()`，並在原始碼註解裡標明 `// if set to zero, use the value from client config`（`StorageClient.h:182-184`）。合併邏輯在 `RetryConfig::mergeWith()`（`StorageClient.h:370-375`），每次呼叫時執行 `config_.retry().mergeWith(options.retry())`。

`WriteOptions::targetSelection` 上有一行註解 `// for test only`（`StorageClient.h:210`）——寫入的目標選取模式不是給生產環境調的。

**checksum 的啟用邏輯有一個 debug/release 分岔**（`StorageClient.h:196-204, 214-222`）：

```cpp
bool verifyChecksum() const {
#ifndef NDEBUG
  bool enabled = true;          // debug build：無視 enableChecksum，一律開
#else
  bool enabled = enableChecksum();
#endif
  return enabled && !debug().bypass_disk_io() && !debug().bypass_rdma_xmit();
}
```

讀的 `enableChecksum` 預設 `false`，寫的預設 `true`。也就是 release build 預設**寫算 checksum、讀不驗 checksum**。兩個 bypass 開關都會把 checksum 一併關掉（因為此時資料根本沒經過磁碟/網路，驗了必然不符）。

---

## 3. 組件全景

```
                    ┌──────────────────────────────────────────────────────┐
   使用者程式碼       │  createReadIO / createWriteIO / createQueryOp / ...   │
   (FUSE, meta,      │  registerIOBuffer                                     │
    admin_cli)       │  batchRead / batchWrite / queryLastChunk / ...        │
                    └───────────────────────┬──────────────────────────────┘
                                            │  StorageClient（純虛介面 + 工廠）
                                            │  StorageClient.h / .cc
                      ┌─────────────────────┴─────────────────────┐
                      │                                           │
          ┌───────────▼──────────┐                    ┌───────────▼───────────┐
          │  StorageClientImpl   │                    │  StorageClientInMem   │
          │  (ImplementationType │                    │  (ImplementationType  │
          │        ::RPC)        │                    │       ::InMem)        │
          │  2560 行，真正的實作   │                    │  395 行，測試替身       │
          └───────────┬──────────┘                    └───────────────────────┘
                      │
     ┌────────────────┼──────────────────┬───────────────────┬─────────────────┐
     │                │                  │                   │                 │
┌────▼──────┐ ┌───────▼────────┐ ┌───────▼────────┐ ┌────────▼──────┐ ┌────────▼────────┐
│ Routing   │ │ TargetSelection│ │ UpdateChannel  │ │ Concurrency   │ │ StorageMessenger│
│ 快照      │ │ Strategy ×6    │ │ Allocator      │ │ Limit ×5      │ │ ×2              │
│           │ │                │ │                │ │               │ │                 │
│ atomic_   │ │ LoadBalance    │ │ stack<Channel  │ │ 全域 Semaphore │ │ messenger_      │
│ shared_ptr│ │ RoundRobin     │ │       Id>      │ │ + per-node    │ │ (讀/查詢)        │
│ <Routing  │ │ RandomTarget   │ │ + 全域遞增      │ │   Semaphore   │ │ messengerFor    │
│  Info>    │ │ TailTarget     │ │   seqnum       │ │               │ │  Updates_(更新)  │
│           │ │ HeadTarget     │ │                │ │               │ │                 │
│ mgmtd     │ │ ManualMode     │ │                │ │               │ │ net::Client     │
│ listener  │ │                │ │                │ │               │ │  + serde RPC    │
└───────────┘ └────────────────┘ └────────────────┘ └───────────────┘ └────────┬────────┘
                                                                                │
                                                                    RDMA (serde StorageSerde)
                                                                                │
                                                                     ┌──────────▼──────────┐
                                                                     │  storage service    │
                                                                     │  (storage_main)     │
                                                                     └─────────────────────┘

                    ┌───────────────────────────────────────────┐
                    │  StorageClientBlobImpl.h（80 行，孤兒標頭） │
                    │  無 .cc、無任何引用、且無法編譯（見 §15）    │
                    └───────────────────────────────────────────┘
```

**兩個 messenger 的角色分工**（`StorageClientImpl.h:131-133`）：

```cpp
StorageMessenger &getStorageMessengerForUpdates() {
  return config_.create_net_client_for_updates() ? messengerForUpdates_ : messenger_;
}
```

`create_net_client_for_updates` 預設 `false`（`StorageClient.h:419`），所以**預設情況下讀寫共用同一個 `net::Client`**（同一組執行緒池、同一組連線池）。開啟後，更新類操作（write / removeChunks / truncateChunks）走獨立的第二個 `net::Client`，讀與 query 仍走 `messenger_`。`messengerForUpdates_` 只有在開關啟用時才會 `start()` 與 `stopAndJoin()`（`StorageClientImpl.cc:1433-1440`、`:1484-1487`）。

---

## 4. `StorageMessenger`：薄到只剩型別的 RPC 轉接層

`StorageMessenger.h`（97 行）+ `StorageMessenger.cc`（223 行）是整個目錄裡最沒有邏輯的兩個檔案，但它的形狀本身就是設計。

它持有一個 `hf3fs::net::Client client_`（`StorageMessenger.h:94`），並把 `StorageSerde` 的 14 個方法（`src/fbs/storage/Service.h:8-23`）中的 **14 個全部**包成同型別的協程函式。所有 14 個實作長得一模一樣，都轉呼叫同一個模板（`StorageMessenger.cc:14-36`）：

```cpp
template <typename Req, typename Rsp, auto rpcMethod>
CoTryTask<Rsp> callSerdeRpcMethod(hf3fs::net::Client &client,
                                  const hf3fs::net::Address &address,
                                  const Req &request,
                                  const net::UserRequestOptions *options,
                                  serde::Timestamp *timestamp) {
  auto clientCtx = client.serdeCtx(address);
  auto packedRsp = co_await rpcMethod(clientCtx, request, options, timestamp);
  if (!packedRsp) {
    auto timeout = options && options->timeout ? options->timeout.value() : client.options()->timeout;
    XLOGF(ERR, "RPC communication error: {}, request: {}, timeout: {}, peer address: {}",
          packedRsp.error(), fmt::ptr(&request), timeout, address);
    co_return makeError(packedRsp.error());
  }
  co_return packedRsp;
}
```

它做的事只有三件：建 `serde::ClientContext`、呼叫、失敗時多印一行帶 `address` 與 `timeout` 的錯誤日誌。**沒有重試、沒有連線管理、沒有負載平衡、沒有錯誤碼轉換**——那些分別落在 `net::Client`（連線池、send retry）與 `StorageClientImpl`（重試、錯誤轉換）。

### 4.1 一個容易忽略的事實：`syncStart` / `syncDone` 有實作但客戶端不用

`StorageMessenger` 提供了 `syncStart()`（`:58-61`）與 `syncDone()`（`:63-66`），對應 CRAQ 的 resync 協定。但在 `StorageClientImpl.cc` 全檔中，這兩個方法**沒有任何呼叫點**——resync 的發起方是 storage 服務端自己（`src/storage/sync/ResyncWorker.cc`），它複用了同一個 `StorageMessenger` 類別去跟後繼節點通訊。

同理，`update()`（`:33-36`）也沒有客戶端呼叫點：客戶端寫入走 `write()`（`StorageSerde` method id 2），鏈內轉發才走 `update()`（method id 3）。**`StorageMessenger` 是「客戶端 + 服務端轉發器」的共用元件，不是純客戶端元件。**

### 4.2 `StorageSerde` 方法號的空洞

`src/fbs/storage/Service.h` 的方法號是 1,2,3,**5**,6,7,8,9,10,11,12,13,**16**,17——缺 4、14、15。序號一旦分配就不能重用（serde 的線上相容性），所以這三個洞是歷史上被移除的方法留下的痕跡。

---

## 5. 路由解析：從 `chainId` 到 `(targetId, nodeId)`

### 5.1 路由快照的取得與更新

`StorageClientImpl` 持有 `folly::atomic_shared_ptr<hf3fs::client::RoutingInfo const> currentRoutingInfo_`（`StorageClientImpl.h:230`）。

啟動時（`StorageClientImpl.cc:1442-1459`）：

```cpp
auto routingInfo = mgmtdClient_.getRoutingInfo();
if (routingInfo == nullptr || routingInfo->raw() == nullptr) {
  XLOGF(CRITICAL, "Failed to get the first routing info");
  return makeError(StorageClientCode::kRoutingError);          // ← 拿不到路由就啟動失敗
}
setCurrentRoutingInfo(routingInfo);

bool addListenerOK = mgmtdClient_.addRoutingInfoListener(fmt::to_string(clientId_),
    [this](auto &&routingInfo) { setCurrentRoutingInfo(std::forward<decltype(routingInfo)>(routingInfo)); });
```

監聽器的名字用 `clientId_` 的字串形式，所以同一個 mgmtd client 上可以掛多個 storage client。`stop()` 對稱地移除監聽器，移除失敗會 DFATAL（`StorageClientImpl.cc:1475-1479`）。

`setCurrentRoutingInfo()`（`StorageClientImpl.cc:1494-1559`）除了 `store()` 之外，還做了一輪**純日誌用途的差異比對**：對每條「副本數 > 1」的鏈（`:1518` 明確跳過單副本鏈），比對新舊 `chainVersion` 並印出變化；對新版本裡所有非 SERVING 的 target 印 WARN。這一段沒有任何控制流效果，純粹是可觀測性。

### 5.2 三個 getter 的校驗鏈

`getChainInfo` / `getNodeInfo` / `getTargetInfo`（`StorageClientImpl.cc:331-416`）做的不只是查表，每個都額外校驗「查出來的東西的 id 是否等於查詢的 id」——這是在防路由表自身不一致：

```cpp
if (chainInfo->chainId != chainId) { ... return makeError(kRoutingError); }
if (nodeInfo->app.nodeId != nodeId) { ... return makeError(kRoutingError); }
if (nodeInfo->type != hf3fs::flat::NodeType::STORAGE) { ... return makeError(kRoutingError); }
if (targetInfo->targetId != targetId) { ... return makeError(kRoutingError); }
if (targetInfo->publicState != hf3fs::flat::PublicTargetState::SERVING) { ... return makeError(kNotAvailable); }
```

注意最後一條回的是 `kNotAvailable` 而不是 `kRoutingError`——「target 存在但不在服務」與「路由表壞了」是兩種語意。

### 5.3 `selectServingTargets()`：鏈上可用 target 的裁切

`StorageClientImpl.cc:418-487`。這個函式把 `flat::ChainInfo.targets`（有序，index 0 是 HEAD）轉成 `vector<SlimTargetInfo>`，過程中有**三種不同的終止/跳過行為**，區分得很清楚：

```cpp
for (const auto &target : chainInfo.targets) {
  if (target.publicState != PublicTargetState::SERVING) {
    XLOGF(DBG5, "Found the first non-serving target ...");
    break;                          // ← 遇到第一個非 SERVING 就「截斷後綴」
  }
  auto targetInfo = getTargetInfo(routingInfo, target.targetId);
  if (!targetInfo) return makeError(targetInfo.error().code());   // ← 整條鏈解析失敗
  if (!targetInfo->nodeId) {
    XLOGF(WARN, "Host node id of target ... is unknown ...");
    break;                          // ← 同樣截斷後綴
  }
  auto nodeInfo = getNodeInfo(routingInfo, *targetInfo->nodeId);
  if (!nodeInfo) return makeError(nodeInfo.error().code());
  if (!nodeSelector(*nodeInfo)) {
    XLOGF(DBG5, "Target ... is skipped since its host ... is filtered out");
    continue;                       // ← traffic zone 過濾是「跳過單個」，不截斷
  }
  servingTargets.push_back({TargetId(targetInfo->targetId), NodeId(*targetInfo->nodeId)});
}
```

**「非 SERVING 就 `break`」而不是 `continue`** 是一個結構性選擇：CRAQ 鏈是有序的，`servingTargets` 保留原順序，所以 `front()` 一定是 HEAD、`back()` 是「最後一個連續 SERVING 的 target」。若中間有一個 target 掉線就 `continue` 跳過，`back()` 就不再是真正的 TAIL 了，`TailTargetStrategy` 會選到錯的節點。原始碼日誌訊息用的字眼是 "the first non-serving target"，與這個解讀一致。

**traffic zone 過濾用 `continue`**，因此它只影響「哪些節點可讀」，不影響鏈的前綴語意。`TargetSelectionOptions` 的註解直接說明了兩者的交互（`TargetSelection.h:41-42`）：

> if mode = Tail/Head, but tail/head is not in the specified traffic zone, the read could fail
> if mode = LB/RR/Random, only storage targets hosted in the specified traffic zone are selected

`selectNodeByTrafficZone()`（`src/fbs/mgmtd/NodeInfo.h:54-69`）把設定字串按 `, ` 分割成多個 zone，比對節點 tag 中 key 為 `kTrafficZoneTagKey` 的值；**空字串代表不過濾**。

### 5.4 `selectRoutingTargetForOps()`：每批一次的鏈資訊快取

`StorageClientImpl.cc:489-631` 是路由解析的主函式。它在整批 op 上跑一遍，用兩個以 `chainId` 為 key 的 map 做**批內去重快取**（`slimChains` 與 `chainInfos`，`:514-515`），所以同一條鏈上的 100 個 op 只解析一次鏈資訊。

`TargetSelectionStrategy` 是**每批建一個新實例**——`TargetSelection.h:48` 的註解寫得很明白：

```cpp
// An instance of each TargetSelectionStrategy implementation is created for each batch read.
```

**failover 過濾只在 `selectAnyTarget()` 為 true 時生效**（`:547-578`）：

```cpp
} else if (targetSelectionStrategy->selectAnyTarget()) {
  std::vector<SlimTargetInfo> reachableTargets;
  for (const auto &[targetId, nodeId] : *servingTargets) {
    auto targetOnChain = std::make_tuple(targetId, chainInfo->chainId, chainInfo->chainVersion);
    auto iter = requestCtx.numFailures.find(targetOnChain);
    if (iter != requestCtx.numFailures.end() &&
        iter->second >= requestCtx.clientConfig.retry().max_failures_before_failover()) {
      // 跳過這個 target
    } else {
      reachableTargets.push_back({targetId, nodeId});
    }
  }
  if (reachableTargets.empty()) {
    XLOGF(DBG3, "All serving targets on the chain not reachable: {}", *chainInfo);
    // ← 全部不可達時「不套用」過濾，退回原始 servingTargets
  } else {
    servingTargets->swap(reachableTargets);
  }
}
```

三個關鍵條件：

1. **`selectAnyTarget()` 只有 LoadBalance / RoundRobin / RandomTarget 回 true**（`TargetSelection.cc:43, 67, 87`）。Head / Tail / Manual 三種策略的基底實作回 `false`（`TargetSelection.h:59`）。因此**預設走 `HeadTarget` 的寫入、query、remove、truncate 完全沒有 target 級 failover**——它們只能反覆重試同一個 HEAD。這與 CRAQ 的約束一致：服務端在 `src/storage/service/StorageOperator.cc:338-341` 明確拒絕非 HEAD 收到的客戶端更新（`kRoutingError`）。
2. **失敗計數的 key 是 `(targetId, chainId, chainVer)` 三元組**（`using TargetOnChain = std::tuple<TargetId, ChainId, ChainVer>`，`StorageClientImpl.cc:67`）。鏈版本一變，舊的失敗計數就自動失效。
3. **計數存在 `ClientRequestContext::numFailures`（`:125`），生命週期只有一次 user call**。跨 `batchRead` 呼叫不累積，所以這是「本次呼叫內的 failover」，不是全域黑名單。

`targetSelectionStrategy->reset()` 的觸發條件（`:587-589`）值得注意：

```cpp
if (slimChain.servingTargets.size() < chainInfo->targets.size()) {
  targetSelectionStrategy->reset();
}
```

**只要這條鏈上有任何 target 被裁掉（非 SERVING / 不在 zone / 被 failover 跳過），就呼叫 `reset()`。** 而 `LoadBalanceStrategy::reset()` 與 `RoundRobinStrategy::reset()` 清的是**程序級全域 map**（見 §6.1），不是這一批的狀態。

---

## 6. `TargetSelection`：六種副本選取策略

`TargetSelection.h`（71 行）定義介面與設定，`TargetSelection.cc`（152 行）實作六個策略 + 一個工廠。

```cpp
enum TargetSelectionMode {
  Default = 0, LoadBalance, RoundRobin, RandomTarget, TailTarget, HeadTarget, ManualMode, EndOfMode
};
```

`Default` 沒有對應的策略類別——`TargetSelectionStrategy::create()` 的 `default:` 分支回 `nullptr`（`TargetSelection.cc:147-148`）。`Default` 的解析發生在**每個操作的 `*WithoutRetry()` 進入點**，各自替換成具體模式：

| 操作 | `Default` 替換為 | 位置 |
|---|---|---|
| `batchRead` / `read` | `LoadBalance` | `StorageClientImpl.cc:1648-1650` |
| `batchWrite` / `write` | `HeadTarget` | `StorageClientImpl.cc:1806-1808` |
| `queryLastChunk` | `HeadTarget` | `StorageClientImpl.cc:2068-2070` |
| `removeChunks` | `HeadTarget` | `StorageClientImpl.cc:2171-2173` |
| `truncateChunks` | `HeadTarget` | `StorageClientImpl.cc:2302-2304` |

若使用者顯式設了非 `Default` 的模式，就直接沿用——**包括寫入**。所以理論上可以強迫寫入去 TAIL，只是服務端會拒絕（前述 `kRoutingError`）；`WriteOptions::targetSelection` 上的 `// for test only` 註解正對應這件事。

### 6.1 兩個程序級全域狀態

```cpp
namespace {
folly::AtomicUnorderedInsertMap<NodeId, folly::MutableAtom<uint64_t>> numAccumIOs{32_KB};
folly::AtomicUnorderedInsertMap<ChainId, folly::MutableAtom<uint64_t>> nextTargetIndex{1_MB};
}  // namespace
```
（`TargetSelection.cc:6-11`）

這兩張表是**匿名 namespace 裡的檔案級靜態變數**，容量在建構時固定（32768 個節點、1048576 條鏈）。它們**跨所有 `StorageClient` 實例、跨所有批次共享**。這意味著：

- 同一個程序裡兩個獨立的 storage client（例如 FUSE 同時持有讀寫兩套）會互相影響 round-robin 的指標與 load-balance 的累積計數；
- `reset()`（§5.4 提到的觸發條件）會把**整張全域表**歸零，而不只是當前鏈：

```cpp
void reset() override { for (auto &[k, v] : numAccumIOs) { v.data = 0; } }        // LoadBalance
void reset() override { for (auto &[k, v] : nextTargetIndex) { v.data = 0; } }    // RoundRobin
```

也就是說：**一條鏈退化（哪怕只是一個 target 不在 traffic zone 內），就會清掉整個程序的負載平衡歷史**。這是純粹的事實陳述——原始碼沒有註解說明為何要這樣做。

### 6.2 `LoadBalanceStrategy`：兩級比較

`TargetSelection.cc:14-53`：

```cpp
Result<SlimTargetInfo> selectTarget(const SlimChainInfo &chain) override {
  uint32_t targetIndex = folly::Random::rand32(0, chain.servingTargets.size());
  SlimTargetInfo preferredTarget = chain.servingTargets[targetIndex];      // ① 隨機起點
  auto [preferredIt, succ] = numAccumIOs.emplace(preferredTarget.nodeId, 0);

  for (const auto &target : chain.servingTargets) {
    if (numIOs[target.nodeId] < numIOs[preferredTarget.nodeId]) {          // ② 主鍵：批內計數
      preferredTarget = target;
    } else if (target.nodeId != preferredTarget.nodeId &&
               numIOs[target.nodeId] == numIOs[preferredTarget.nodeId]) {
      auto [currentIt, succ1] = numAccumIOs.emplace(target.nodeId, 0);
      if (currentIt->second.data < preferredIt->second.data) {             // ③ 次鍵：全域累積計數
        preferredTarget = target;
        std::memcpy(&preferredIt, &currentIt, sizeof(preferredIt));
      }
    }
  }
  numIOs[preferredTarget.nodeId]++;      // 批內計數 +1
  preferredIt->second.data++;            // 全域計數 +1
  return preferredTarget;
}
```

三層決策：**隨機起點 → 本批次內 IO 數最少 → 平手時比全程序累積 IO 數最少**。`numIOs` 是 `std::unordered_map<NodeId, uint64_t>`，是策略實例的成員（`TargetSelection.cc:52`），生命週期只有一批；`numAccumIOs` 是全域的。

第 32 行 `std::memcpy(&preferredIt, &currentIt, sizeof(preferredIt))` 用 memcpy 賦值迭代器，而非 `preferredIt = currentIt`。這是實作事實，原始碼沒有註解說明原因。

`kNodeIdKeyedMapExpectedNumElements = 1000`（`TargetSelection.h:67`）是 `numIOs` 的預留桶數。

### 6.3 其餘五種策略

| 策略 | 實作 | `selectAnyTarget()` | `reset()` |
|---|---|---|---|
| `RoundRobinStrategy` | `nextTargetIndex[chainId]++ % servingTargets.size()`（`TargetSelection.cc:61-65`） | `true` | 清全域 `nextTargetIndex` |
| `RandomTargetStrategy` | `folly::Random::rand32(0, size)`（`:82-85`） | `true` | 無 |
| `TailTargetStrategy` | `servingTargets.back()`（`:96`） | `false` | 無 |
| `HeadTargetStrategy` | `servingTargets.front()`（`:105`） | `false` | 無 |
| `ManualModeStrategy` | `servingTargets[options_.targetIndex()]`，越界回 `kNotAvailable` + CRITICAL 日誌（`:114-124`） | `false` | 無 |

`ManualModeStrategy` 上方的註解寫的是 `// Always select the head target`（`TargetSelection.cc:108`）——這是複製 `HeadTargetStrategy` 時留下的錯誤註解，實際行為是按使用者指定的索引選。

`TailTargetStrategy` / `HeadTargetStrategy` **不檢查 `servingTargets` 是否為空**，但呼叫端在 `selectRoutingTargetForOps` 裡已經先擋掉了空的情況（`StorageClientImpl.cc:597-606`，設 `kNotAvailable` 後 `continue`）。

---

## 7. 寫路徑

### 7.1 全景時序（對接 `storage_main` 深度剖析 §5.1）

```
 使用者                    StorageClientImpl                      storage service (HEAD)
   │                              │                                        │
   │─ createWriteIO() ───────────▶│  requestId = nextRequestId_++          │
   │   (chain, chunk, off, len,   │  WriteIO{ routingTarget{chainId},      │
   │    chunkSize, data, buffer)  │           chunkId, offset, length,     │
   │                              │           chunkSize, data, buffer }    │
   │                              │                                        │
   │─ batchWrite(span<WriteIO>) ─▶│                                        │
   │                              │                                        │
   │                    ┌─────────┴─────────┐                              │
   │                    │ ① validateWrite   │  offset+length > chunkSize?  │
   │                    │   DataRange       │  → kInvalidArg（逐 op）       │
   │                    │   + validateData  │  buffer 為 null / data 不在   │
   │                    │     Range         │  buffer 內 → 整批 kInvalidArg │
   │                    └─────────┬─────────┘                              │
   │                              │                                        │
   │                    ┌─────────┴─────────┐  ← sendOpsWithRetry 迴圈開始   │
   │                    │ ② 每輪重試：       │     requestTimeout =          │
   │                    │   resetResult()   │        backoff.getWaitTime()  │
   │                    └─────────┬─────────┘                              │
   │                              │                                        │
   │                    ┌─────────┴─────────┐                              │
   │                    │ ③ selectRouting   │  routingInfo 快照            │
   │                    │   TargetForOps    │  HeadTarget 策略 → HEAD      │
   │                    │                   │  無 failover（見 §5.4）       │
   │                    └─────────┬─────────┘                              │
   │                              │                                        │
   │                    ┌─────────┴─────────┐                              │
   │                    │ ④ groupOpsByNodeId│  按 nodeId 分組              │
   │                    │   max_batch_size  │  128 ops / 4MB               │
   │                    │   random_shuffle  │  批次順序打散                 │
   │                    └─────────┬─────────┘                              │
   │                              │                                        │
   │                 ┌────────────┴────────────┐  processBatches(parallel) │
   │                 │ 每個 batch 一個協程：     │                          │
   │                 │  ⑤ perServerSemaphore   │  ≤8 併發/節點             │
   │                 │  ⑥ concurrencySemaphore │  ≤32 全域併發             │
   │                 │  ⑦ isLatestRoutingInfo  │  過期 → kRoutingVersion   │
   │                 │                         │           Mismatch        │
   │                 │  ⑧ allocateChannelsFor  │  配不到 → kResourceBusy   │
   │                 │       Ops(reallocate=F) │                           │
   │                 │  ⑨ sendWriteRequests    │                           │
   │                 │       Sequentially      │  ← 節點內「一次一個」        │
   │                 └────────────┬────────────┘                           │
   │                              │                                        │
   │                              │  for each writeIO (序列)：              │
   │                              │    checksum = CRC32C(data, length)     │
   │                              │    payload.rdmabuf = buffer.subrange()  │
   │                              │                     .toRemoteBuf()     │
   │                              │─── WriteReq{payload, tag{clientId,   ──▶│
   │                              │       requestId, channel{id,seqnum}}} │
   │                              │                                        ├─ ReliableUpdate 去重
   │                              │                                        ├─ lockChunk
   │                              │◀────── RDMA READ 拉資料 ────────────────┤
   │                              │                                        ├─ 鏈內轉發 → commit
   │                              │◀── WriteRsp{IOResult} ─────────────────┤
   │                              │                                        │
   │                              │  成功 → releaseChannelsForOp           │
   │                              │  失敗 → break（本批剩餘 IO 不送）        │
   │                              │                                        │
   │                    ┌─────────┴─────────┐                              │
   │                    │ ⑩ 分類每個 op 的   │  永久錯誤 → 放 channel、不重試 │
   │                    │   狀態，決定重試    │  暫時不可用 → 記 numFailures  │
   │                    └─────────┬─────────┘  快速重試錯誤 → 縮短等待       │
   │                              │                                        │
   │◀─ co_return Void{} ──────────┤  collectFailedOps + reportNumFailedOps │
   │   （逐 op 檢查 result）        │                                        │
```

### 7.2 為什麼寫必須走 HEAD

三份互相印證的證據：

1. **客戶端預設**：`batchWriteWithoutRetry` 把 `Default` 換成 `HeadTarget`（`StorageClientImpl.cc:1806-1808`）。
2. **服務端拒絕**：`src/storage/service/StorageOperator.cc:338-341`
   ```cpp
   if (UNLIKELY(req.options.fromClient && !target->isHead)) {
     XLOGF(ERR, "non-head node receive a client update request");
     co_return makeError(StorageClientCode::kRoutingError, "non-head node receive a client update request");
   }
   ```
3. **`selectServingTargets` 的前綴語意**：`servingTargets.front()` 必定是 `chainInfo.targets[0]`，因為遇到第一個非 SERVING 就 `break`（§5.3）。若 HEAD 本身非 SERVING，`servingTargets` 為空，寫入直接拿 `kNotAvailable`。

**推論**：HEAD 故障時，寫入沒有任何客戶端側的補救手段——只能等 mgmtd 推送新的鏈版本（把新的 HEAD 排到最前面），然後靠重試迴圈重新解析路由。這也解釋了為什麼 `max_retry_time` 預設高達 60 秒。

### 7.3 `UpdateChannelAllocator`：channel 是什麼、為什麼需要

`UpdateChannelAllocator.h`（36 行）+ `.cc`（81 行）。標頭上的註解直接說明了用途：

```cpp
/*
  Allocate a channel id, which is used to de-duplicate updates at storage service,
  for example, write, truncate, remove etc.
*/
```

**資料結構**（`UpdateChannelAllocator.h:29-34`）：

```cpp
size_t numChannels_;
std::mutex availableChannelMutex_;
std::stack<ChannelId> availableChannelIds_;      // 可用 id 池（LIFO）
std::atomic_uint64_t nextSeqNum_ = 1;            // 全域單調遞增序號
```

`ChannelId` 是 `uint16_t`（`src/fbs/storage/Common.h:30`），所以 `kMaxNumChannels = (1 << 16) - 1 = 65535`（`UpdateChannelAllocator.h:17-18`）。id `0` 保留作「未配置」哨兵值——建構子從 `numChannels_` 倒數推到 `1`，永遠不會 push 0（`UpdateChannelAllocator.cc:15-18`）。

池大小來自 `TrafficControlConfig::max_concurrent_updates()`（`StorageClient.h:404-408`）：

```cpp
size_t max_concurrent_updates() const {
  return write().max_concurrent_requests()    * write().max_batch_size()      // 32 × 128 = 4096
       + remove().max_concurrent_requests()   * remove().max_batch_size()     // 32 × 128 = 4096
       + truncate().max_concurrent_requests() * truncate().max_batch_size();  // 32 × 128 = 4096
}                                                                            // = 12288
```

也就是「所有可能同時在飛的更新 op 的上限」。`StorageClient::create()` 在建立前先檢查這個數不超過 65535，超過就直接回 `nullptr` 並印 CRITICAL（`StorageClient.cc:22-28`）——這是唯一一個會讓 client 建立失敗的設定校驗。

**`allocate()` 的雙模式**（`UpdateChannelAllocator.cc:39-63`）：

```cpp
bool UpdateChannelAllocator::allocate(UpdateChannel &channel, uint32_t slots) {
  if (channel.id != ChannelId{0}) {
    channel.seqnum = ChannelSeqNum{nextSeqNum_.fetch_add(slots)};   // ← 已有 id：只換序號
    XLOGF(DBG7, "Reallocated a channel {}#{}", channel, slots);
    return true;
  }
  { std::scoped_lock lock(availableChannelMutex_);
    if (availableChannelIds_.empty()) { XLOGF(WARN, "No available channel, ..."); return false; }
    channel.id = availableChannelIds_.top();
    availableChannelIds_.pop(); }
  channel.seqnum = ChannelSeqNum{nextSeqNum_.fetch_add(slots)};
  num_update_channels_inuse.addSample(1);
  return true;
}
```

**服務端如何使用 channel**（`src/storage/service/ReliableUpdate.cc:38-72`）：

```cpp
auto key = std::pair<ChainId, ChannelId>(req.payload.key.vChainId.chainId, req.tag.channel.id);
auto &reqResult = clientStatus->channelMap[key];               // 以 (clientId, chainId, channelId) 為 key
...
if (req.tag.channel.seqnum < reqResult->channelSeqnum) {
  co_return makeError(StorageClientCode::kDuplicateUpdate);    // 序號變小 → 判定重複
}
if (req.tag.channel.seqnum == reqResult->channelSeqnum && generationId 相同) {
  if (req.tag.requestId != reqResult->requestId) { ... kFoundBug }
  ... co_return updateResult;                                  // 序號相同 → 回快取結果
}
// 序號更大 → 真的執行
```

於是三件事被鎖死在一起：

**（a）`nextSeqNum_` 必須全域單調遞增，不能 per-channel。** 因為 channel id 會被回收再配給另一個 op。若序號是每個 channel 各自從頭數，一個被回收再配出的 channel 會拿到比服務端記憶中更小的序號，直接被判 `kDuplicateUpdate`。全域遞增保證「同一個 channel id 的下一次使用，序號一定更大」。這也正是客戶端把 `kDuplicateUpdate` 視為 **[BUG]** 的原因（`StorageClientImpl.cc:1356-1362`、`:1950-1956` 兩處 DFATAL）——正常情況下客戶端不可能送出倒退的序號。

**（b）`slots` 參數對應服務端的序號消耗。** `allocateChannelsForOps` 巨集傳的是 `op->chunkRange().maxNumChunkIdsToProcess`（`StorageClientImpl.cc:299`）。對 write / truncate 而言 `chunkRange()` 回 `{chunkId, chunkId, 1}`，所以 `slots = 1`；對 remove / query 而言是使用者指定的範圍上限。服務端在 `StorageOperator::removeChunks` 裡**每刪一個 chunk 就把序號 +1**：

```cpp
removeOp.tag.channel.seqnum++;  // increment the sequence number for next remove
```
（`src/storage/service/StorageOperator.cc:974`）

所以客戶端一次預留 `maxNumChunkIdsToProcess` 個連續序號，正好覆蓋服務端展開範圍時要消耗的區間。

**（c）三種更新操作的 `reallocate` 旗標不同：**

| 操作 | `reallocate` | 位置 | 重試時的效果 |
|---|---|---|---|
| `batchWrite` | `false` | `StorageClientImpl.cc:1836` | 沿用同一 `(id, seqnum)` → 服務端回**快取結果**（冪等） |
| `truncateChunks` | `false` | `StorageClientImpl.cc:2333` | 同上（冪等） |
| `removeChunks` | `true` | `StorageClientImpl.cc:2202` | 每次重試都 `fetch_add(slots)` → 服務端**重新執行** |

remove 之所以是 `true`，與 §2.3 那條「`numChunksRemoved` 可能小於實際刪除數」的註解互為表裡：範圍刪除是一個掃描過程，重試時前一輪已刪的 chunk 已經不在了，服務端必須重新掃描剩下的，因此不能吃快取結果。

**channel 的釋放時機**共四處：

| 時機 | 位置 |
|---|---|
| op 成功（批次路徑） | `StorageClientImpl.cc:1377`（`sendBatchRequest` 內） |
| op 成功（寫入序列路徑） | `StorageClientImpl.cc:1969`（`sendWriteRequestsSequentially` 內） |
| 判定為永久錯誤、不再重試 | `StorageClientImpl.cc:1232-1233` |
| 重試超時、放棄 | `StorageClientImpl.cc:1175-1180` |

配置失敗時的部分回滾在 `allocateChannelsForOps` 巨集內（`StorageClientImpl.cc:324-326`）：用 `goto fail` 跳到 `releaseChannelsForOps(chanAllocator, opsWithChan)` 把本輪已配出的全部還回去，然後回 `false`。

最後 `reportNumFailedOps` 巨集在收尾時再檢查一次（`StorageClientImpl.cc:256-263`）：任何失敗的 op 若還握著 channel，DFATAL。

### 7.4 `sendWriteRequestsSequentially`：為什麼寫入不併發

`StorageClientImpl.cc:1927-1990`。同一個節點的一批 `WriteIO` 是**序列**送的，而且**第一個失敗就 `break`**：

```cpp
for (auto &writeIO : writeIOs) {
  co_await sendWriteRequest(requestCtx, writeIO, *nodeInfo, userInfo, options);
  ...
  if (writeIO->statusCode() == StatusCode::kOK) {
    totalDataLen += writeIO->resultLen();
    totalNumChunksUpdated += writeIO->numProcessedChunks();
    releaseChannelsForOp(chanAllocator_, writeIO);
  } else {
    firstError = writeIO->statusCode();
    break;                              // ← 本批剩下的 IO 一個都不送
  }
}
```

被 `break` 略過的 IO 保持 `kNotInitialized`（`IOResult` 的預設值），在重試迴圈的分類階段（`:1234`）因為 `statusCode() != kOK` 被放進 `remainingOps`，下一輪重試。它們的 channel 也一直握著沒放——這正是 §7.3 冪等機制想要的：下一輪用同樣的 `(id, seqnum)` 重送。

這與讀路徑形成鮮明對比：讀是一個 `BatchReadReq` 帶 `vector<ReadIO>` 一次送完，寫是**每個 chunk 一個 `WriteReq`**（`StorageSerde::write` 的請求型別 `WriteReq` 只有單一 `payload`，`src/fbs/storage/Common.h:374-382`）。所以「批次寫」在協定層面根本不存在批次——客戶端的 batch 只是分組與限流單位。

### 7.5 `sendWriteRequest`：一次寫的請求組裝

`StorageClientImpl.cc:1860-1925`：

```cpp
size_t offset = writeIO->data - writeIO->buffer->data();      // data 在註冊區內的位移
auto iobuf = writeIO->buffer->subrange(offset, writeIO->length);
...
if (options.verifyChecksum()) {
  writeIO->checksum = ChecksumInfo::create(config_.chunk_checksum_type(), writeIO->data, writeIO->length);
}
hf3fs::storage::MessageTag tag{clientId_, requestId, writeIO->routingTarget.channel};
hf3fs::storage::UpdateIO payload{writeIO->offset, writeIO->length, writeIO->chunkSize,
                                 key, iobuf.toRemoteBuf(),
                                 hf3fs::storage::ChunkVer(0) /*updateVer*/,
                                 UpdateType::WRITE, writeIO->checksum};
if (writeIO->length <= requestCtx.clientConfig.max_inline_write_bytes()) {
  payload.inlinebuf.data.assign(writeIO->data, writeIO->data + writeIO->length);
  BITFLAGS_SET(featureFlags, FeatureFlags::SEND_DATA_INLINE);
}
```

四個要點：

1. **`updateVer` 客戶端一律填 0**。版本推進完全由服務端決定，客戶端不參與 CRAQ 的版本協商。
2. **`updateType` 一律是 `UpdateType::WRITE`**。`UpdateType` 的其他值（`REMOVE` / `TRUNCATE` / `EXTEND` / `COMMIT`，`Common.h:51-58`）由服務端在展開 `removeChunks` / `truncateChunks` 時自己填。
3. **checksum 在客戶端算，隨請求上行**。`ChecksumInfo::create` 以 1MB 為單位分段做 CRC（`Common.h:118`，`kChunkSize = 1_MB`）。
4. **inline 判定是 `<=`**（寫）而讀是 `<`（`StorageClientImpl.cc:706`：`if (requestedBytes < max_inline_read_bytes())`）。兩個預設值都是 0，所以讀路徑的 inline 永遠不會觸發（`requestedBytes < 0` 恆假），而寫路徑在 `length == 0` 時會觸發（`0 <= 0` 為真）。這個不對稱是實作事實。

故障注入點（`StorageClientImpl.cc:1879-1883`）刻意只對前半段資料算 checksum，模擬客戶端算錯：

```cpp
writeIO->checksum = FAULT_INJECTION_POINT(
    requestCtx.debugFlags.injectClientError(),
    ChecksumInfo::create(config_.chunk_checksum_type(), writeIO->data, std::max(writeIO->length / 2, 1U)),
    ChecksumInfo::create(config_.chunk_checksum_type(), writeIO->data, writeIO->length));
```

`FAULT_INJECTION_POINT` 在 `NDEBUG` 下退化為直接取第三個參數（`src/fbs/storage/Common.h:42-47`），所以 release build 沒有任何開銷。

---

## 8. 讀路徑

### 8.1 `batchRead` 的四段

```
batchRead(span<ReadIO>)                                   StorageClientImpl.cc:1563-1575
  └─ ClientRequestContext(MethodType::batchRead, ...)
  └─ createVectorOfPtrsFromOps  → vector<ReadIO*>
  └─ batchReadWithRetry ─────────────────────────────────  :1577-1640
       ├─ validateDataRange(readIOs, check_overlapping_read_buffers)
       ├─ [可選] splitReadIOs(*this, validIOs, max_read_io_bytes)
       ├─ sendOpsWithRetry<ReadIO>(...)  ← 重試外殼
       │     └─ batchReadWithoutRetry ──────────────────  :1642-1746
       │          ├─ TargetSelectionOptions → LoadBalance
       │          ├─ selectRoutingTargetForOps
       │          ├─ groupOpsByNodeId(read.max_batch_size, read.max_batch_bytes, shuffle)
       │          └─ processBatches(parallel) → 每批一個協程：
       │               ├─ perServerSemaphore.coWait()      ≤8/節點
       │               ├─ concurrencySemaphore.coWait()    ≤32 全域
       │               ├─ isLatestRoutingInfo?             否 → kRoutingVersionMismatch
       │               ├─ buildBatchRequest<ReadIO, BatchReadReq>
       │               ├─ sendBatchRequest → StorageMessenger::batchRead
       │               ├─ [SEND_DATA_INLINE] memcpy 回使用者緩衝
       │               └─ [verifyChecksum] 逐 IO 重算 CRC 並比對
       ├─ [可選] 把 splittedIOs 的結果合併回 parentIO
       └─ collectFailedOps + reportNumFailedOps
```

### 8.2 `validateDataRange`：三項校驗，一票否決

`StorageClientImpl.cc:889-951`。三個檢查：

```cpp
if (io->buffer == nullptr || io->data == nullptr)         → kInvalidArg
if (!io->buffer->contains(io->data, io->length))          → kInvalidArg
if (checkOverlappingBuffers && lastIO && io->data < lastIO->dataEnd())  → kInvalidArg
```

**三個檢查任何一個失敗，都是 `setErrorCodeOfOps(sortedIOs, kInvalidArg)` + `return {}`——整批全滅。** 不是只標記出問題的那一個。

`buffer->contains()` 最終落到 `RDMABuf::contains()`（`src/common/net/ib/RDMABuf.h:162`）：

```cpp
bool contains(const uint8_t *data, uint32_t len) const { return ptr() <= data && data + len <= ptr() + capacity(); }
```

重疊檢查需要先按 `data` 指標排序（`:893-895`）：

```cpp
if (checkOverlappingBuffers) {
  std::sort(begin(sortedIOs), end(sortedIOs), [](const IO *a, const IO *b) { return a->data < b->data; });
}
```

**副作用**：開啟 `check_overlapping_read_buffers`（預設 `true`）時，回傳的 `validIOs` 是**按緩衝區位址排序過的**，不是使用者傳入的順序。這會影響後續 `groupOpsByNodeId` 的分批切點。關閉檢查時（`check_overlapping_write_buffers` 預設 `false`）則保留原順序。

`validateWriteDataRange`（`:994-1015`）在呼叫 `validateDataRange` 前多做一項**逐 op**的 chunk 邊界檢查：

```cpp
if (io->chunkSize == 0 || io->offset + io->length > io->chunkSize) {
  setErrorCodeOfOp(io, kInvalidArg);       // ← 只標記這一個
} else {
  validIOs.push_back(io);
}
```

所以寫入的邊界檢查是逐個的，緩衝區檢查是整批的。

### 8.3 `splitReadIOs`：大 IO 切分

`StorageClientImpl.cc:953-992`，只在 `config_.max_read_io_bytes() > 0` 時啟用（預設 0，即**預設不切分**）。

```cpp
for (uint32_t offset = parentIO->offset, length = parentIO->length; length > 0;) {
  uint32_t ioEnd = ALIGN_LOWER(offset, maxIOLen) + maxIOLen;
  uint32_t ioLen = std::min(ioEnd - offset, length);
  parentIO->splittedIOs.push_back(client.createReadIO(parentIO->routingTarget.chainId,
                                                      parentIO->chunkId, offset, ioLen,
                                                      parentIO->data + (offset - parentIO->offset),
                                                      parentIO->buffer));
  offset += ioLen;  length -= ioLen;
}
for (auto &splittedIO : parentIO->splittedIOs) splittedIOs.push_back(&splittedIO);
```

切點對齊到 `maxIOLen` 的倍數（`ALIGN_LOWER(offset, maxIOLen) + maxIOLen`），所以第一片可能短於 `maxIOLen`，後面的都是整齊的 `maxIOLen`。

**指標安全性**：子 IO 存在 `parentIO->splittedIOs`（一個 `std::vector<ReadIO>`），`push_back` 會導致重新配置。程式碼在**迴圈全部結束之後**才統一取指標（`:988`），因此不會有懸空指標。

`:961` 的 `reserve((parentIO->chunkSize + maxIOLen - 1) / maxIOLen)` 用的是 `chunkSize`，但 `ReadIO` 的建構子把 `chunkSize` 固定填 0（`StorageClient.h:121`：`IOBase(chainId, chunkId, offset, length, 0 /*chunkSize*/, ...)`），所以這個 `reserve` 實際上總是 `reserve(0)`。這是實作事實；由於指標在迴圈後才取，它不影響正確性，只影響配置次數。

**結果合併**（`StorageClientImpl.cc:1607-1634`）：

```cpp
if (!bool(splittedIO.result.lengthInfo)) {
  parentIO->result = splittedIO.result;    // 任何一片失敗 → parent 取該片的錯誤，break
  break;
} else if (ioIndex == 0) {
  parentIO->result = splittedIO.result;    // 第一片：整包接管，含 requestId / routingTarget
  parentIO->requestId = splittedIO.requestId;
  parentIO->routingTarget = splittedIO.routingTarget;
} else {
  parentIO->result.lengthInfo = *parentIO->result.lengthInfo + *splittedIO.result.lengthInfo;
  parentIO->result.checksum.combine(splittedIO.result.checksum, *splittedIO.result.lengthInfo);
}
```

checksum 用 `folly::crc32c_combine` / `crc32_combine` 拼接（`src/fbs/storage/Common.h:179-198`），所以切分後的 CRC 仍能組回整段的 CRC。注意 `parentIO->routingTarget` 只取第一片的——各片可能被 LoadBalance 選到**不同的 target**，parent 記錄的只是第一片的落點。

### 8.4 `buildBatchRequest<ReadIO, BatchReadReq>`：RDMA 描述子的產生

`StorageClientImpl.cc:670-721`。核心兩行：

```cpp
size_t offset = op->data - op->buffer->data();
auto iobuf = op->buffer->subrange(offset, op->length);
...
payloads.push_back({op->offset, op->length, std::move(key), iobuf.toRemoteBuf()});
```

`op->offset` 是 **chunk 內的偏移**，`offset`（區域變數）是 **使用者資料在註冊緩衝區內的偏移**——兩個完全不同的概念，共用了相似的名字。

`toRemoteBuf()`（`src/common/net/ib/RDMABuf.h:220-227`）把本地 `RDMABuf` 轉成可序列化的 `RDMARemoteBuf`，內容是 `{addr, length, rkeys[kMaxDeviceCnt]}`——**每張 HCA 一組 `{rkey, devId}`**。服務端拿到後用自己那張卡的 `devId` 查出對應的 rkey（`RDMABuf.h:64-71` 的 `getRkey`），然後對這段記憶體做單邊 RDMA Write（讀路徑）或 RDMA Read（寫路徑）。

**整批共用一個 `RequestId`**：

```cpp
hf3fs::storage::RequestId requestId(nextRequestId.fetch_add(1));
hf3fs::storage::MessageTag tag{clientId, requestId};    // ← 注意：沒有 channel
for (auto &op : ops) { ...; op->requestId = requestId; ... }
```

讀請求的 `MessageTag` **不帶 channel**（用了兩參數建構子，`Common.h:283-286`，channel 預設為 `UpdateChannel{}` 即 id=0）。讀是唯讀操作，不需要去重。

FeatureFlags 的組裝（`:704-712`）：

```cpp
uint32_t featureFlags = buildFeatureFlagsFromOptions(options.debug());   // BYPASS_DISKIO / BYPASS_RDMAXMIT
if (requestedBytes < requestCtx.clientConfig.max_inline_read_bytes()) BITFLAGS_SET(featureFlags, SEND_DATA_INLINE);
if (options.allowReadUncommitted()) BITFLAGS_SET(featureFlags, ALLOW_READ_UNCOMMITTED);
```

`allowReadUncommitted` 對應 CRAQ 的髒讀：允許讀到尚未 commit 的版本，避免 `kChunkNotCommit`。預設 `false`。

### 8.5 `sendBatchRequest`：通用的批次回應處理

`StorageClientImpl.cc:1302-1391`，被 read / query / remove / truncate 四條路徑共用（write 不走這裡，走 §7.4）。

```cpp
auto &results = (*response).results;
if (results.size() != ops.size()) {
  XLOGF(DFATAL, "[BUG] Unexpected length of results: {}, expected value: {}", results.size(), ops.size());
  setErrorCodeOfOps(ops, StorageClientCode::kFoundBug);
  co_return makeError(StorageClientCode::kFoundBug);
}
for (uint32_t opIdx = 0; opIdx < results.size(); opIdx++) {
  ... setResultOfOp(ops[opIdx], results[opIdx]);
  if (ops[opIdx]->statusCode() == StatusCode::kOK) {
    totalDataLen += ops[opIdx]->resultLen();
    totalNumChunks += ops[opIdx]->numProcessedChunks();
    releaseChannelsForOp(chanAllocator_, ops[opIdx]);
  }
}
```

**回應與請求靠陣列下標對應，沒有任何 id 校驗**。這是一個強契約：服務端必須按請求順序回傳等長的結果陣列。長度對不上就是 `kFoundBug`（7999）。

`if constexpr (requires { results[opIdx].statusCode; })` 這個 concept 判斷（`:1344`）用來區分兩類結果型別：`QueryLastChunkResult` / `RemoveChunksResult` 有 `statusCode` 欄位，`IOResult` 只有 `lengthInfo`。同樣的模式在 `sendOpsWithRetry`（`:1226-1230`）與 `buildBatchRequest` 的特化裡都出現。

### 8.6 inline 回填與 checksum 驗證

`StorageClientImpl.cc:1697-1737`。inline 模式下，資料在回應封包的 `inlinebuf` 裡，客戶端逐 IO memcpy 回使用者緩衝：

```cpp
if (response->inlinebuf.data.size() != totalDataLen) {
  XLOGF(DFATAL, "[BUG] Inline buffer size {} not equal to total data size {} of {} read IOs", ...);
  setErrorCodeOfOps(batchIOs, StorageClientCode::kFoundBug);
  co_return false;
}
auto inlinebuf = &response->inlinebuf.data[0];
for (auto readIO : batchIOs) {
  std::memcpy(readIO->data, inlinebuf, readIO->resultLen());
  inlinebuf += readIO->resultLen();
}
```

checksum 驗證（`:1720-1737`）在 inline 回填**之後**，所以兩種傳輸模式共用同一段驗證邏輯：

```cpp
if (readIO->result.lengthInfo && *readIO->result.lengthInfo > 0) {
  auto checksum = ChecksumInfo::create(readIO->result.checksum.type, readIO->data, *readIO->result.lengthInfo);
  if (checksum != readIO->result.checksum) {
    XLOGF_IF(DFATAL, !requestCtx.debugFlags.faultInjectionEnabled(),
             "Local checksum {} not equal to checksum {} generated by server, routing target: {}", ...);
    setErrorCodeOfOp(readIO, StorageClientCode::kChecksumMismatch);
  }
}
```

用的 checksum 型別是**服務端回傳的** `readIO->result.checksum.type`，不是本地設定的 `chunk_checksum_type`。而且 `kChecksumMismatch` 被歸類為**永久錯誤**（`:148`），不重試——資料損毀重試也沒用。`XLOGF_IF(DFATAL, ...)` 的條件排除了故障注入模式，避免注入測試把程序打掛。

---

## 9. 批次聚合：`groupOpsByNodeId`

`StorageClientImpl.cc:1029-1122`。這是「多個 chunk 請求如何合併成一次 RPC」的答案。

### 9.1 分組維度是 nodeId，不是 targetId

```cpp
opsGroupedByNode[op->routingTarget.targetInfo.nodeId].push_back(op);
opsGroupBytes[op->routingTarget.targetInfo.nodeId] += op->dataLen();
```

**一個 storage 節點上可能有多個 target（多顆盤），它們會被合併進同一個批次。** 這也是服務端 `BatchReadReq` 裡每個 `ReadIO` 都要自帶完整 `GlobalKey{vChainId, chunkId}` 的原因——同一個請求裡的 IO 可能落在不同的鏈、不同的 target 上。

### 9.2 `calcAvgSize`：均勻切分而非貪心填滿

```cpp
static auto calcAvgSize = [](size_t total, size_t max) {
  if (total == 0) return max;
  size_t n = (total + max - 1) / max;      // 需要幾個批次
  size_t avg = (total + n - 1) / n;        // 每批平均多少
  return std::min(avg, max);
};
```

切分條件（`:1072-1073`）：

```cpp
if ((batchOps.size() >= avgBatchSize && batchOps.size() + remainingOps > maxBatchSize) ||
    (batchBytes + op->dataLen() > avgBatchBytes && batchBytes + remainingBytes > maxBatchBytes)) {
  batches.emplace_back(nodeId, std::move(batchOps));
  batchOps.clear();  batchBytes = 0;  numBatches++;
}
```

具體效果：130 個 op、`maxBatchSize = 128` 時，`n = 2`、`avg = 65`，最終切成 **65 + 65** 兩批，而不是「128 + 2」。第二個合取項 `batchOps.size() + remainingOps > maxBatchSize` 保證「剩下的全部塞得進一個批次時就不再切」。

### 9.3 query / remove / truncate 的位元組上限被寫死為 `SIZE_MAX`

| 操作 | `maxBatchSize` 來源 | `maxBatchBytes` 來源 | 位置 |
|---|---|---|---|
| `batchRead` | `read().max_batch_size()` | `read().max_batch_bytes()` | `:1659-1660` |
| `batchWrite` | `write().max_batch_size()` | `write().max_batch_bytes()` | `:1817-1818` |
| `queryLastChunk` | `query().max_batch_size()` | **`SIZE_MAX`** | `:2079-2080` |
| `removeChunks` | `remove().max_batch_size()` | **`SIZE_MAX`** | `:2182-2183` |
| `truncateChunks` | `truncate().max_batch_size()` | **`SIZE_MAX`** | `:2313-2314` |

也就是 `traffic_control.query/remove/truncate.max_batch_bytes` 這三個設定項**在程式碼裡沒有作用**——它們的 `dataLen()` 本來就恆為 0（`QueryLastChunkOp::dataLen()` 等都回 0，`StorageClient.h:244, 276, 316`），所以即使傳入真實設定也不會觸發切分；寫死 `SIZE_MAX` 只是把這件事講明白。

### 9.4 已持有 channel 的 op 被插到批次最前面

```cpp
if (UNLIKELY(!opsWithChannelId.empty())) {
  for (auto &[nodeId, opsGroup] : opsWithChannelId) {
    XLOGF(DBG3, "Move {} ops with channel ids to the front of batches, ...", ...);
    batches.emplace(batches.begin(), nodeId, std::move(opsGroup));
  }
}
```
（`:1109-1119`）

這些 op 是**上一輪重試留下的**（成功或永久失敗都會釋放 channel，只有待重試的才保留）。它們有三個特殊待遇：

1. **不參與 `maxBatchSize` / `maxBatchBytes` 切分**——一個節點一批，不論多少個 op；
2. **不被 `random_shuffle_requests` 打散**（shuffle 在 `:1102-1105`，發生在插入之前）；
3. **不計入 `requestCtx.userCallBytes`**（它們走的是 `opsWithChannelId` 分支，`:1043`，沒有累加 `opsGroupBytes`）。

`UNLIKELY` 提示說明作者預期這是重試路徑上的少數情況。

### 9.5 一個位元組統計上的細節

```cpp
for (const auto &[nodeId, opsGroup] : opsGroupedByNode) {
  size_t batchBytes = 0;
  ...
  for (...) { ... batchBytes += op->dataLen(); ... }        // 每次切分時 batchBytes 歸零
  if (!batchOps.empty()) { batches.emplace_back(nodeId, batchOps); numBatches++; }
  requests_per_server.addSample(numBatches, requestCtx.requestTagSet);
  requestCtx.userCallBytes += batchBytes;                    // ← 只累加最後一批的位元組數
}
```
（`:1059-1095`）

`batchBytes` 在每次切分時被歸零（`:1077`），迴圈結束後只剩**最後一個批次**的位元組數。因此當某個節點的 op 被切成多批時，`userCallBytes` 會少計前面幾批。這個變數只用於 `bytes_per_user_call` 與 `user_call_bw` 兩個監控指標（`:1098`、`StorageClientImpl.cc:95-96`），不影響控制流。

---

## 10. 併發模型

### 10.1 三層限流

```
                    ┌─────────────────────────────────────────────┐
   使用者呼叫層      │  無限制。使用者可以同時發起任意多個 batchRead  │
                    │  （只由 concurrent_user_calls 指標觀測）      │
                    └──────────────────┬──────────────────────────┘
                                       │
                    ┌──────────────────▼──────────────────────────┐
   全域併發請求數     │  OperationConcurrencyLimit::                 │
                    │    concurrencySemaphore_                     │
                    │  每種操作各一個（read/write/query/remove/     │
                    │  truncate），預設 32                          │
                    │  hf3fs::Semaphore(usable=32, max=4096)       │
                    └──────────────────┬──────────────────────────┘
                                       │
                    ┌──────────────────▼──────────────────────────┐
   每節點併發請求數   │  perServerSemaphore_[nodeId]                 │
                    │  unordered_map + shared_mutex 惰性建立        │
                    │  預設 8                                      │
                    └─────────────────────────────────────────────┘
```

**取號順序固定是「先 per-server、後全域」**（例：`StorageClientImpl.cc:1667-1671`）：

```cpp
SemaphoreGuard perServerReq(*readConcurrencyLimit_.getPerServerSemaphore(nodeId));
co_await perServerReq.coWait();
SemaphoreGuard concurrentReq(readConcurrencyLimit_.getConcurrencySemaphore());
co_await concurrentReq.coWait();
```

五條路徑（read / write / query / remove / truncate）全部照這個順序。`SemaphoreGuard` 是 RAII（`src/common/utils/SemaphoreGuard.h:14-17`），協程結束時自動 `signal()`；`coWait()` 先 `try_wait()`，失敗才掛起（`:29-34`）。

`hf3fs::Semaphore` 的 `maxTokens` 預設是 **4096**（`src/common/utils/Semaphore.h:13`），而建構時只放 `usableTokens` 個令牌進去、其餘 `maxTokens - usableTokens` 個當場 `wait()` 掉（`:22`）。因此 `max_concurrent_requests` 熱更新時只能在 `[0, 4096]` 內調整。

### 10.2 熱更新的兩種等級

`HotLoadOperationConcurrencyLimit`（`StorageClientImpl.h:198-222`）在建構時掛一個 config callback：

```cpp
onConfigUpdated_(config_.addCallbackGuard([this]() { updateUsableTokens(); }))
...
void updateUsableTokens() {
  concurrencySemaphore_.changeUsableTokens(config_.max_concurrent_requests());
  std::shared_lock rlock(perServerSemaphoreMutex_);
  for (auto &[_, semaphore] : perServerSemaphore_) {
    semaphore->changeUsableTokens(config_.max_concurrent_requests_per_server());
  }
}
```

`Semaphore::changeUsableTokens()`（`src/common/utils/Semaphore.h:46-54`）**調小時會阻塞式地 `wait()` 掉多餘的令牌**——如果當前令牌都在使用中，這個呼叫會卡住直到有人歸還。這是設定回呼路徑上的一個潛在阻塞點。

`readConcurrencyLimit_` 與 `queryConcurrencyLimit_` 是熱更新版，`write` / `remove` / `truncate` 三個是基底版（`StorageClientImpl.h:232-236`），設定改了不生效。

### 10.3 per-server semaphore 的惰性建立

`OperationConcurrencyLimit::getPerServerSemaphore()`（`StorageClientImpl.h:169-188`）是標準的雙檢查鎖：先 `shared_lock` 查，miss 後 `unique_lock` 再查一次、然後建立。**建立後永不移除**——節點下線後它的 semaphore 仍留在 map 裡。

`HotLoadOperationConcurrencyLimit` 覆寫了這個方法（`:205-207`），把 `initTokens` 從建構時固定的 `maxConcurrentRequestsPerServer_` 換成當前設定值，這樣熱更新後**新出現的節點**也拿到新的令牌數。

### 10.4 批次的併發執行

`processBatches`（`StorageClientImpl.cc:1124-1141`）：

```cpp
template <typename Op, typename Ops = std::vector<Op *>>
CoTask<void> processBatches(const std::vector<std::pair<NodeId, Ops>> &batches, auto &&func, bool parallel) {
  std::vector<CoTask<bool>> tasks;
  if (parallel) tasks.reserve(batches.size());
  for (size_t index = 0; index < batches.size(); index++) {
    const auto &[nodeId, ops] = batches[index];
    if (!ops.empty()) {
      if (parallel) tasks.push_back(func(index, nodeId, ops));
      else co_await func(index, nodeId, ops);
    }
  }
  if (parallel) co_await folly::coro::collectAllRange(std::move(tasks));
}
```

`process_batches_in_parallel` 預設 `true`，所有批次的協程一起交給 `collectAllRange`，實際併發度由 §10.1 的兩層 semaphore 決定。`collectAllRange` 會等全部完成——**沒有部分失敗提前退出的機制**。

注意 `func` 的回傳型別是 `CoTask<bool>`，但這個 `bool` 從頭到尾**沒有被讀取**：`collectAllRange` 的結果被丟棄，序列模式下 `co_await func(...)` 的值也被丟棄。真正的成功/失敗訊號寫在每個 op 的 `result` 上。

### 10.5 併發模型總結

| 維度 | 機制 |
|---|---|
| 協程 | folly coroutines（`CoTryTask` / `CoTask`），全程無阻塞式等待 |
| 節點間 | 批次協程併發（`collectAllRange`） |
| 節點內（讀/查/刪/截斷） | 一個 RPC 帶多個 op，服務端併發 |
| 節點內（寫） | **序列**，第一個失敗即 `break`（§7.4） |
| 限流 | 每操作型別一個全域 semaphore（32）+ 每節點 semaphore（8） |
| 共享狀態的同步 | `folly::atomic_shared_ptr`（路由）、`std::shared_mutex`（semaphore map）、`std::mutex`（channel 池）、`AtomicUnorderedInsertMap`（LB/RR 全域計數） |
| 取消 | `RequestInfo::get()->canceled()`，只在 `callMessengerMethod` 進入時檢查一次（`:838-841`） |

---

## 11. 重試與錯誤分類

### 11.1 三個分類函式

`StorageClientImpl.cc:137-174`：

```cpp
static bool isPermanentError(status_code_t statusCode) {
  switch (statusCode) {
    case kMemoryError: case kInvalidArg: case kChunkNotFound: case kBadConfig:
    case kProtocolMismatch: case kRequestCanceled: case kReadOnlyServer:
    case kNoSpace: case kFoundBug: case kChecksumMismatch:
      return true;
  }
  return false;
}
static bool isTemporarilyUnavailable(status_code_t statusCode) {
  switch (statusCode) { case kCommError: case kTimeout: case kRemoteIOError: return true; }
  return false;
}
static bool isFastRetryError(status_code_t statusCode) {
  switch (statusCode) { case kRoutingVersionMismatch: case kChunkNotCommit: return true; }
  return false;
}
```

三者**不是分割**，而是三個獨立的謂詞，用在三個不同的地方：

| 謂詞 | 用途 |
|---|---|
| `isPermanentError` | 決定「不再重試 + 立刻放 channel」（`:1232-1233`） |
| `isTemporarilyUnavailable` | 決定「這個 target 記一次失敗，可能觸發 failover」（`:1237-1240`） |
| `isFastRetryError` | 決定「這輪的等待時間砍半」（`:1242`）；也用於抑制 WARN 日誌（`:182`、`:195`） |

**沒被任何一個謂詞命中的錯誤**（`kRoutingError`、`kNotAvailable`、`kServerError`、`kResourceBusy`、`kNoRDMAInterface`）走預設路徑：重試、但不記 target 失敗、不縮短等待。

`retry_permanent_error` 選項（`StorageClient.h:185`，預設 `false`）可以關掉永久錯誤的短路，讓所有錯誤都進重試迴圈。

### 11.2 `sendOpsWithRetry` 的完整迴圈

`StorageClientImpl.cc:1143-1300`。

```
ExponentialBackoffRetry backoff(init_wait_time, max_wait_time, max_retry_time)
pendingOps = ops;  for op in pendingOps: op->resetResult()

while (true):
  requestTimeout = backoff.getWaitTime()
  if requestTimeout == 0:                                  ← 退避器判定「時間用完了」
      for op in pendingOps:
          releaseChannelsForOp(op)
          if op.statusCode() == kNotInitialized: op.result = kTimeout
      goto exit

  requestCtx.retryCount   = retryCount
  requestCtx.requestTimeout = requestTimeout               ← ★ 退避等待時間 == RPC 逾時
  requestCtx.initDebugFlags()

  t0 = now();  co_await sendOps(pendingOps);  latency = now() - t0
  waitTime = max(0, requestTimeout - latency)              ← ★ 「補足」到一個完整的退避週期

  for op in pendingOps:
      op.result = convertToStorageClientCode(op.result)    ← 服務端碼 → 客戶端碼
      if !retry_permanent_error && isPermanentError(op): releaseChannelsForOp(op)
      elif op.statusCode() != kOK:
          remainingOps.push_back(op)
          if isTemporarilyUnavailable(op): failedTargets.insert((targetId, chainId, chainVer))
          if isFastRetryError(op): waitTime = min(waitTime, init_wait_time / 2)

  for t in failedTargets: requestCtx.numFailures[t]++      ← 供下一輪 selectRoutingTargetForOps 用

  if remainingOps.empty(): goto exit
  pendingOps.swap(remainingOps);  ++retryCount
  if waitTime > 0: co_await folly::coro::sleep(waitTime)
```

### 11.3 一個容易誤解的設計：退避時間 == RPC 逾時

`requestTimeout = backoffRetry.getWaitTime()`（`:1159`）——**`ExponentialBackoffRetry` 產出的「等待時間」被直接當成本輪 RPC 的逾時**（`:1201`，然後在 `callMessengerMethod` 裡設進 `net::UserRequestOptions::timeout`，`:835`）。

然後 `waitTime = max(0, requestTimeout - requestLatency)`（`:1207`）——**真正的 sleep 只補足「逾時值減去實際耗時」的差額**。若請求跑滿逾時才失敗，就不再 sleep，直接重試；若請求很快就失敗（例如立刻拿到 `kNotAvailable`），就 sleep 到湊滿一個退避週期。

以預設值（`init=10s`、`max_wait=30s`、`max_retry=60s`）與「每次請求都跑滿逾時」的情境推演 `ExponentialBackoffRetry::getWaitTime()`（`src/common/utils/ExponentialBackoffRetry.h:21-32`）：

```
輪次   elapsed 進入時   nextWaitTime_   回傳（=RPC 逾時）      sleep      備註
 #0       0 ms            10 s           min(10, 60-0)=10 s     0 s      nextWaitTime_ → 20 s
 #1      10 s             20 s           min(20, 60-10)=20 s    0 s      nextWaitTime_ → 30 s（撞 max_wait 上限）
 #2      30 s             30 s           min(30, 60-30)=30 s    0 s      nextWaitTime_ → 30 s
 #3      60 s              —             60+10 > 60 → 0 s        —       放棄
```

也就是預設設定下**最多送出 3 次請求、總耗時約 60 秒**。若請求是瞬間失敗的，輪次會更多、但總時間仍被 `max_retry_time` 卡在 60 秒。

給棄時的收尾（`:1175-1180`）值得注意：

```cpp
for (const auto &op : pendingOps) {
  releaseChannelsForOp(chanAllocator, op);
  if (op->statusCode() == StorageClientCode::kNotInitialized) {
    op->result = StorageClientCode::kTimeout;
  }
}
```

只有**從頭到尾沒拿到任何結果**（仍是 `kNotInitialized`）的 op 才被改寫成 `kTimeout`；已經有具體錯誤碼的 op 保留原碼。

### 11.4 路由過期的偵測：`isLatestRoutingInfo`

`StorageClientImpl.cc:633-645` 是一個巨集（因為要在 `StorageClientImpl` 的成員函式上下文裡呼叫 `getCurrentRoutingInfo()`）：

```cpp
#define isLatestRoutingInfo(routingInfo, ops)                                                     \
  [&]() -> bool {                                                                                 \
    auto currentRoutingInfo = getCurrentRoutingInfo();                                            \
    bool isLatestVersion = routingInfo->raw()->routingInfoVersion ==                              \
                           currentRoutingInfo->raw()->routingInfoVersion;                         \
    if (isLatestVersion) return true;                                                             \
    for (const auto op : ops) {                                                                   \
      auto chainId = op->routingTarget.chainId;                                                   \
      auto chainVer = op->routingTarget.chainVer;                                                 \
      auto currentChainInfo = getChainInfo(currentRoutingInfo, hf3fs::flat::ChainId(chainId));    \
      if (!currentChainInfo || currentChainInfo->chainVersion != chainVer) return false;          \
    }                                                                                             \
    return true;                                                                                  \
  }()
```

**兩級判定**：路由版本號相同就直接放行；版本號變了，再逐 op 檢查「這條鏈的版本有沒有變」——**只要這批 op 涉及的鏈都沒變，就當作沒過期**。這避免了「叢集裡某條無關的鏈變了，導致所有 in-flight 請求全部作廢」。

檢查點在**取到 semaphore 之後、組請求之前**（例：`:1673-1676`），也就是「排隊等待期間路由可能已經變了」的那個視窗。不通過就整批標 `kRoutingVersionMismatch`——而這個碼正是 `isFastRetryError`（`:168`），所以會走「等待時間砍半」的快速重試路徑，下一輪用新路由重新解析。

這條路徑是**唯一**主動偵測路由過期的地方；路由的**更新**則完全被動，靠 mgmtd 的 listener 推送。

---

## 12. RDMA 緩衝區管理

### 12.1 誰配置、誰註冊、誰釋放

```
使用者程式碼                       StorageClient                    net::RDMABuf / IBDevice
     │                                  │                                 │
     ├─ 自己 malloc / mmap / shm ───────┤                                 │
     │  （函式庫不配置任何資料緩衝區）      │                                 │
     │                                  │                                 │
     ├─ registerIOBuffer(buf, len) ────▶│                                 │
     │                                  ├─ RDMABuf::createFromUserBuffer ▶│
     │                                  │                                 ├─ for dev in IBDevice::all():
     │                                  │                                 │     mr = dev->regMemory(ptr, cap, flags)
     │                                  │                                 │     mrs_[dev->id()] = mr
     │                                  │  失敗 → kMemoryError             │  （任一張卡失敗 → 整體失敗）
     │◀── Result<IOBuffer> ─────────────┤                                 │
     │                                  │                                 │
     ├─ createReadIO(..., data, &buf)   │  data 必須落在 buf 範圍內          │
     ├─ batchRead(...)                  │                                 │
     │                                  ├─ buf->subrange(data - buf->data(), length)
     │                                  ├─ .toRemoteBuf()  →  {addr, len, rkeys[]}
     │                                  │      隨 serde 請求上行             │
     │                                  │                                 │
     ├─ ~IOBuffer()（使用者刪除） ────────┤                                 │
     │                                  │                                 ├─ ~RDMABuf::Inner:
     │                                  │                                 │     for dev: dev->deregMemory(mr)
     │                                  │                                 │     userBuffer_ → 不釋放記憶體本身
```

**函式庫從不配置資料緩衝區。** `registerIOBuffer` 是唯一的入口（`StorageClient.cc:97-110`）：

```cpp
Result<IOBuffer> StorageClient::registerIOBuffer(uint8_t *buf, size_t len) {
  monitor::ScopedLatencyWriter latencyWriter(iobuf_reg_latency);
  iobuf_reg_size.addSample(len);
  auto rdmabuf = hf3fs::net::RDMABuf::createFromUserBuffer(buf, len);
  if (rdmabuf.valid()) { iobuf_reg_success_ops.addSample(1); return IOBuffer{rdmabuf}; }
  else { iobuf_reg_failed_ops.addSample(1); return makeError(StorageClientCode::kMemoryError); }
}
```

註解寫得很清楚（`StorageClient.h:516`）：`// delete the returned IOBuffer object to deregister the buffer`。

**多 HCA 註冊**：`RDMABuf::Inner::registerMemory()`（`src/common/net/ib/RDMABuf.cc:107-127`）對**每一張** IB 裝置都做一次 `ibv_reg_mr`，任何一張失敗就整體回 `-1`。生成的 `RDMARemoteBuf` 攜帶一個 `std::array<Rkey, kMaxDeviceCnt>`，每個元素是 `{rkey, devId}`；服務端按自己的 `devId` 查表取 rkey。**這是「client 註冊一次、任意一張卡的 storage 節點都能直接存取」的實作基礎。**

**去註冊**：`IOBuffer` 持有 `const net::RDMABuf rdmabuf`（`StorageClient.h:60`），`RDMABuf` 內部是 `shared_ptr<Inner>`。`~Inner()`（`RDMABuf.cc:50-63`）對所有卡 `deregMemory`，然後檢查 `userBuffer_` 旗標——**使用者傳進來的記憶體不會被 `deallocate`**，只反註冊。

**`IOBuffer` 是 `folly::MoveOnly`**，所以不會意外複製導致重複反註冊；`registerIOBuffer` 回傳的是值，呼叫端通常 `std::move` 進 `shared_ptr`（如 `src/lib/common/Shm.cc:114`）。

### 12.2 USRBIO 的分段註冊

`src/lib/common/Shm.cc:100-116` 展示了大塊共享記憶體的處理方式：

```cpp
for (size_t i = 0; i < memhs_.size(); ++i) {
  auto res = sc.registerIOBuffer(bufStart + blockSize * i, std::min(size - blockSize * i, blockSize));
  ...
  memhs_[i].store(std::make_shared<storage::client::IOBuffer>(std::move(*res)));
}
```

一整片 shm 被切成 `blockSize` 大小的段，**每段各自註冊成一個 `IOBuffer`**。這也解釋了 `IOBase` 為什麼要同時帶 `data`（實際位址）與 `buffer`（所屬註冊段）兩個欄位——一個位址對應哪個 MR 不是唯一的，必須由呼叫端指明。

### 12.3 緩衝區與 IO 的三重契約

`StorageClient.h:448-451`（`createReadIO` 的註解）把契約寫死了：

> The memory pointed by `data' should be large enough to store the data, fall in the range of the registered `buffer' and does not overlap with other IOs in the same batch (can be disable by setting `check_overlapping_read_buffers').

三條分別由 §8.2 的三個檢查對應。第三條「同批內不重疊」的理由與 RDMA 語意直接相關：服務端對每個 `ReadIO` 的 `rdmabuf` 做獨立的單邊 RDMA Write，若兩個 IO 的目標區間重疊，寫入順序未定義。

---

## 13. 可觀測性

### 13.1 指標全表

`StorageClientImpl.cc:30-63` + `StorageClient.cc:10-13` + `UpdateChannelAllocator.cc:9-10`，共 30 個。所有指標都帶 `requestTagSet`（tag key = `instance`，value = `MethodType` 的名字），部分額外帶 `userTagSet`（多一個 tag `uid`）。

| 類別 | 指標名 | 型別 | 記錄點 |
|---|---|---|---|
| 併發 | `storage_client.concurrent_user_calls` | Distribution | `ClientRequestContext` 建構/解構 |
| 併發 | `storage_client.num_pending_ops` | Distribution | 同上 |
| 併發 | `storage_client.inflight_requests` | Distribution | `callMessengerMethod` 進出 |
| 扇出 | `storage_client.accessed_servers_per_user_call` | Distribution | `groupOpsByNodeId` |
| 扇出 | `storage_client.requests_per_user_call` | Distribution | 同上 |
| 扇出 | `storage_client.requests_per_server` | Distribution | 同上，逐節點 |
| 扇出 | `storage_client.ops_per_user_call` | Distribution | 同上 |
| 扇出 | `storage_client.ops_per_request` | Distribution | `buildBatchRequest` × 4 特化 + `sendWriteRequest` |
| 資料量 | `storage_client.bytes_per_operation` | Distribution | `buildBatchRequest<ReadIO>` / `sendWriteRequest` |
| 資料量 | `storage_client.bytes_per_request` | Distribution | 同上 |
| 資料量 | `storage_client.bytes_per_user_call` | Distribution | `groupOpsByNodeId` |
| 資料量 | `storage_client.data_payload_bytes` | Count(reset) | `sendBatchRequest` / `sendWriteRequestsSequentially` |
| 資料量 | `storage_client.data_payload_bytes_per_user` | Count(reset) | 同上 |
| 頻寬 | `storage_client.user_call_bw` | Distribution | `~ClientRequestContext` |
| 頻寬 | `storage_client.request_bw` | Distribution | `sendBatchRequest` / `sendWriteRequestsSequentially` |
| 延遲 | `storage_client.overall_latency` | Latency | `ClientRequestContext` 全生命週期 |
| 延遲 | `storage_client.waiting_time` | Latency | `logWaitingTime()`：取到 semaphore、通過路由檢查、配到 channel **之後**，發 RPC **之前** |
| 延遲 | `storage_client.request_latency` | Latency | `callMessengerMethod` |
| 延遲 | `storage_client.inflight_time` | Latency | serde `Timestamp::inflightLatency()` |
| 延遲 | `storage_client.server_latency` | Latency | serde `Timestamp::serverLatency()` |
| 延遲 | `storage_client.network_latency` | Latency | serde `Timestamp::networkLatency()` |
| 計數 | `storage_client.num_processed_chunks` (+`_per_user`) | Count(reset) | 成功的 op 累加 `numProcessedChunks()` |
| 計數 | `storage_client.num_completed_ops` (+`_per_user`) | Count(reset) | `collectFailedOps` |
| 計數 | `storage_client.num_failed_ops` (+`_per_user`) | Count(reset) | `reportNumFailedOps` |
| 計數 | `storage_client.num_retried_ops` (+`_per_user`) | Count(reset) | `sendOpsWithRetry` 每輪 |
| 計數 | `storage_client.num_error_codes` | Count(reset) | `reportNumFailedOps`，額外帶 `statusCode` tag |
| Channel | `storage_client.num_update_channels.inuse` | Count(不 reset) | `allocate` / `release` |
| Channel | `storage_client.num_update_channels.total` | Count(不 reset) | 建構子 |
| Buffer | `storage_client.iobuf_reg.{success_ops,failed_ops,latency,size}` | ×4 | `registerIOBuffer` |

三個 `std::unordered_map<MethodType, std::atomic_int64_t>`（`StorageClientImpl.cc:26-28`）在 `start()` 時用 `magic_enum::enum_values<MethodType>()` 預先填滿所有 key（`:1420-1424`）——之後全部用 `.at()` 存取，不會插入，所以在多執行緒下讀寫是安全的。**注意這三個 map 是檔案級靜態變數，跨所有 `StorageClientImpl` 實例共享。**

### 13.2 `ClientRequestContext`：一次 user call 的全部上下文

`StorageClientImpl.cc:69-133`。除了指標之外，它還攜帶三件控制流資訊：

| 欄位 | 用途 |
|---|---|
| `userCallId` | 全域遞增，所有日誌都印 `usercall: #{}`，用於串起一次呼叫的全部日誌 |
| `retryCount` | 寫進 `BatchReadReq::retryCount` 等欄位，服務端可據此識別重試 |
| `requestTimeout` | 每輪重試由 `sendOpsWithRetry` 設定，傳到 `net::UserRequestOptions` |
| `debugFlags` | 每輪重試 `initDebugFlags()` 重新擲骰，決定注入點 |
| `numFailures` | target failover 的計數表（§5.4） |
| `userCallBytes` | 頻寬統計 |

`DebugOptions::toDebugFlags()`（`StorageClient.h:169-178`）每次呼叫都重新產生一個隨機的 `numOfInjectPtsBeforeFail`：

```cpp
DebugFlags toDebugFlags() const {
#ifndef NDEBUG
  return DebugFlags{.injectRandomServerError = inject_random_server_error(),
                    .injectRandomClientError = inject_random_client_error(),
                    .numOfInjectPtsBeforeFail = (uint16_t)folly::Random::rand32(1, max_num_of_injection_points() + 1)};
#else
  return DebugFlags{};       // ← release build 一律空
#endif
}
```

`DebugFlags::isFailPoint()`（`src/fbs/storage/Common.h:296-300`）是一個倒數計數器：每經過一個注入點就減 1，減到 1 時觸發失敗。所以「第 N 個注入點失敗」，N 每輪重試重新隨機。

### 13.3 日誌的五個巨集

`StorageClientImpl.cc:178-327` 定義了七個巨集，其中五個與日誌/錯誤處理相關：

| 巨集 | 行為 |
|---|---|
| `setResultOfOp(op, res)` | 設結果 + 非 OK 且非 fast-retry 時印 WARN |
| `setErrorCodeOfOp(op, errorCode)` | 同上（實作完全相同，只是參數語意不同） |
| `setErrorCodeOfOps(ops, errorCode)` | 對整個容器套用上者 |
| `collectFailedOps(ops, failedOps, requestCtx)` | 逐 op 印 DBG5（`kChunkNotFound` 不印）、收集失敗者、印總結 ERR、記 `num_completed_ops` |
| `reportNumFailedOps(failedOps, requestCtx)` | 按錯誤碼分組記 `num_error_codes`、檢查 channel 洩漏、記 `num_failed_ops` |

`setResultOfOp` 與 `setErrorCodeOfOp` 的函式體逐字相同（`:178-202`）——差別僅在呼叫慣例（前者傳完整 result 物件，後者傳錯誤碼，靠 `IOResult(uint32_t statusCode)` 的隱式建構子，`src/fbs/storage/Common.h:233-234`）。

`isFastRetryError` 在日誌條件裡的作用是**降噪**：`kRoutingVersionMismatch` 與 `kChunkNotCommit` 在鏈變更或 CRAQ 髒讀時是常態，不該刷 WARN。

---

## 14. `StorageClientInMem`：不只是測試替身

### 14.1 定位

`StorageClientInMem.h`（111 行）+ `.cc`（395 行）。透過 `Config::implementation_type = InMem` 選用（`StorageClient.cc:34-36`），與 `StorageClientImpl` 並列在同一個工廠裡。

**它的用途是「讓不關心 storage 的上層測試能跑起來」，而不是「模擬 storage 的行為」。** 三處實際使用：

| 使用點 | 目的 |
|---|---|
| `src/meta/service/MockMeta.h:79` | Mock meta 服務內建一個記憶體 storage |
| `tests/client/TestMetaClient.cc:149` | 測 MetaClient 時不需要真的 storage 叢集 |
| `tests/meta/components/TestGcManager.cc:304` | 用 `injectErrorOnChain` 注入鏈級錯誤，測 GC 的錯誤處理 |
| `tests/storage/client/TestStorageClientInterface.cc:757` | 作為 `TestStorageClientInterface` 的一個參數化實例（`InMemClient`），與 `RpcClient` 跑同一組介面測試 |

第四項最關鍵：**它與真實實作共用同一套介面測試**，所以「介面契約」的部分是被強制對齊的。但測試裡也明確跳過了它做不到的事（`tests/storage/client/TestStorageClientInterface.cc:358`）：

```cpp
if (setupConfig_.client_impl_type() == StorageClient::ImplementationType::InMem) return;
```

### 14.2 資料模型

```cpp
struct ChunkData { std::vector<uint8_t> content; uint32_t capacity; uint32_t version; };
struct Chain     { folly::coro::Mutex mutex; std::map<ChunkId, ChunkData> chunks; Result<Void> error = Void{}; };
folly::Synchronized<std::unordered_map<ChainId, Chain>, std::mutex> chains_;
```
（`StorageClientInMem.h:80-108`）

**以 `ChainId` 為單位、而不是以 target 為單位**——整個副本鏈退化成一份資料。`std::map<ChunkId, ...>` 的有序性支撐了 `queryLastChunk` 與 `removeChunks` 的範圍掃描。

`getChain()`（`:92-96`）先鎖外層 `std::mutex` 拿到 `Chain&`，再 `co_await chain.mutex.co_scoped_lock()`。`GET_CHAIN` 巨集（`StorageClientInMem.cc:23-28`）在拿鎖後立刻檢查注入的錯誤：

```cpp
#define GET_CHAIN(chainId)                                                     \
  auto [chain, guard] = co_await getChain(chainId);                            \
  if (chain->error.hasError()) {                                               \
    XLOGF(WARN, "Inject error {} on chain {}", chain->error.error(), chainId); \
    co_return makeError(chain->error.error().code(), "fault injection");       \
  }
```

### 14.3 與真實實作的行為落差（逐項）

| 面向 | `StorageClientImpl` | `StorageClientInMem` |
|---|---|---|
| **路由** | 解析 chain → serving targets → 選 target → nodeId | **完全忽略**。只用 `routingTarget.chainId` 當 map key |
| **`start()` / `stop()`** | 啟 messenger、抓路由、掛 listener | 用基底類別的空實作（`StorageClient.h:443-446`），什麼都不做 |
| **RDMA buffer** | `data` 必須落在註冊的 `buffer` 內，否則 `kInvalidArg` | **從不檢查 `buffer`**，直接 `memcpy(readIO.data, ...)` |
| **重疊緩衝區檢查** | `validateDataRange` | 無 |
| **重試 / 退避** | `sendOpsWithRetry` 完整迴圈 | 無。一次就是一次 |
| **併發限流** | 兩層 semaphore | 無 |
| **批次聚合** | `groupOpsByNodeId` | 無。`batchWrite` 逐個呼叫 `write()` |
| **update channel** | 完整的配置/釋放/去重 | **完全不涉及**。`RemoveChunksOp` 等的 `channel.id` 始終為 0 |
| **checksum** | 讀驗證、寫計算 | 無。`getAllChunkMetadata` 回 `ChecksumInfo{}` |
| **`failedIOs` 收集** | 由 `collectFailedOps` 統一處理 | **不一致**：`batchRead` 會 push（`:65`）、`batchWrite` 會 push（`:91-93`）、`truncateChunks` 會 push（`:317, 330`）、但 **`removeChunks` 的 `failedOps` 被 `boost::ignore_unused` 掉**（`:279`）、`queryLastChunk` 也沒有 push |
| **`read()` 的 failedIOs** | 有 | `read()` 轉呼叫 `batchRead(..., nullptr)`（`:100`），失敗不收集 |
| **`querySpaceInfo`** | 真的發 RPC | **回硬編碼的假資料**：`/disk1` 10GB/2GB/1GB `intel`、`/disk2` 10GB/8GB/8GB `micron`（`:346-354`） |
| **`createTarget` / `offlineTarget` / `removeTarget`** | 發 RPC | 回空的 `Rsp{}`，**永遠成功** |
| **`queryChunk`** | 對鏈上每個 target 各發一次 | 回**空 vector**（`:368-370`） |
| **版本語意** | `commitVer` / `updateVer` 由 CRAQ 推進，可能不等 | `IOResult{len, ChunkVer(version), ChunkVer(version)}`——**兩個版本永遠相等** |
| **`ChunkState`** | 服務端維護 COMMIT/DIRTY/CLEAN | `getAllChunkMetadata` 一律回 `ChunkState::COMMIT` |
| **`chainVer`** | 真實鏈版本 | `getAllChunkMetadata` 回 `ChainVer{}`（零值） |
| **寫入順序** | 依 `groupOpsByNodeId` 的結果 | **刻意 `randomShuffle`**（`:33-42, 89`），逼上層不能依賴批內順序 |

**兩處刻意加入的「非確定性」**是這個實作最有意思的地方：

1. `randomShuffle()` 用在 `batchWrite` / `queryLastChunk` / `removeChunks` / `truncateChunks`——**打散處理順序**，強迫上層測試不能假設批內順序。
2. `processQueryResults()`（`:162-230`）把 `maxNumResultsPerQuery` **寫死為 3**：
   ```cpp
   const uint32_t maxNumResultsPerQuery = 3U;
   ```
   即使使用者要求一次處理 1000 個 chunk，它也會拆成一次 3 個的多輪查詢。這模擬了服務端的分頁行為，讓 `moreChunksInRange` 與分頁邏輯在測試裡真的被走到。

`processQueryResults` 的分頁判定與服務端的同名函式（`src/storage/service/StorageOperator.cc`）結構一致：多查一個（`numChunksToProcess + 1`）來判斷是否還有更多，最後 `moreChunksInRange = numQueryResults > numChunksToProcess`。

`FAULT_INJECTION()` 與 `FAULT_INJECTION_SET_FACTOR(ops.size())`（`:166-169, 238, 281`）接的是 `src/common/utils/FaultInjection.h` 的全域注入框架，與 `StorageClientImpl` 用的 `DebugFlags` 是**兩套獨立的機制**。

**結論：`StorageClientInMem` 是「介面層級的功能替身」，不是「行為層級的模擬器」。** 它保證上層程式碼能編譯、能跑通功能流程，但任何依賴路由、重試、去重、RDMA、版本語意的行為在它上面都測不出來。

---

## 15. `StorageClientBlobImpl`：無法編譯的孤兒標頭

`StorageClientBlobImpl.h` 是 80 行的純宣告，**沒有對應的 `.cc`**。三項硬證據說明它是未接線的殘留：

**證據一：全 repo 只有它自己提到自己。**

```
$ grep -rn "StorageClientBlobImpl" . --exclude-dir=.git --exclude-dir=third_party
src/client/storage/StorageClientBlobImpl.h:17:class StorageClientBlobImpl : public StorageClient {
src/client/storage/StorageClientBlobImpl.h:19:  StorageClientBlobImpl(const ClientId &clientId, ...);
src/client/storage/StorageClientBlobImpl.h:21:  ~StorageClientBlobImpl() override;
```

沒有任何 `#include`、沒有任何工廠分支（`StorageClient::create()` 只認 `RPC` 與 `InMem`，`StorageClient.cc:32-36`）、`ImplementationType` 列舉也只有兩個值（`StorageClient.h:340-343`）。

**證據二：它宣告了兩個不存在的 override。**

```cpp
CoTryTask<void> listChunks(std::span<ListChunksOp> ops, ...) override;    // :62-65
CoTryTask<void> batchCopy(std::span<CopyIO> copyIOs, ...) override;       // :67-70
```

`ListChunksOp`、`CopyIO`、`listChunks`、`batchCopy` **在整個 repo 裡除了這個檔案之外沒有任何定義或使用**：

```
$ grep -rn "ListChunksOp\|CopyIO\|listChunks\|batchCopy" src tests
src/client/storage/StorageClientBlobImpl.h:62,65,67,70   （全部四個命中都在同一個檔案）
```

型別不存在 + `override` 指定了基底類別沒有的虛擬函式 = **這個標頭一旦被 `#include` 就編譯失敗**。這是它從未被引用的最強證據。

**證據三：它缺少三個基底類別的純虛擬函式。**

`StorageClient` 有 14 個 `= 0` 的虛擬函式（`StorageClient.h:441, 519, 524, 529, 533, 539, 544, 549, 554, 556, 558, 560, 562, 564`）。`StorageClientBlobImpl` 只宣告了其中 12 個，**漏掉 `offlineTarget`（`StorageClient.h:558`）與 `removeTarget`（`:560`）**——對照 `StorageClientBlobImpl.h:72-78` 可見它從 `createTarget` 直接跳到 `queryChunk`。因此即使前兩個問題被修掉，它仍是抽象類別、無法實例化。

**推斷**（明確標示為推斷）：從命名（Blob）與多出來的 `listChunks` / `batchCopy` 介面看，這曾經是一個「以 blob 儲存為後端的第三種實作」的介面草稿，隨著 `StorageClient` 介面演進（`offlineTarget` / `removeTarget` 是後加的）而失去同步，最終被遺留下來。原始碼裡**沒有任何註解或 TODO 支持這個推斷**，以上只是從介面差異推得的最合理解讀。

**它應該被刪除。** 它不在任何編譯單元裡（`CMakeLists.txt` 只有一行 `target_add_lib(storage-client storage-fbs common mgmtd-client)`，靠 glob 收檔，但標頭不參與編譯），所以刪除它零風險。

---

## 16. 與 `storage_main` 服務端的對接矩陣

| 客戶端動作 | RPC 方法（method id） | 服務端入口 | 客戶端關鍵欄位 | 服務端回應 |
|---|---|---|---|---|
| `batchRead` | `batchRead` (1) | `StorageOperator::batchRead` | `payloads[].rdmabuf`（單邊 RDMA Write 的目標）、`featureFlags`、`checksumType` | `results[]` 等長 + 可選 `inlinebuf` |
| `batchWrite` / `write` | `write` (2) | `ReliableUpdate` → `StorageOperator::handleUpdate` | `payload.rdmabuf`（服務端 RDMA Read 的來源）、`tag.channel`、`payload.checksum` | `result: IOResult` |
| （鏈內轉發，非客戶端） | `update` (3) | 同上 | `options.fromClient = false` | — |
| `queryLastChunk` | `queryLastChunk` (5) | `StorageOperator::queryLastChunk` | `payloads[].chunkIdRange` | `results[]` |
| `truncateChunks` | `truncateChunks` (6) | `StorageOperator::truncateChunks` | 逐 op `tag.channel`（`slots=1`） | `results[]: IOResult` |
| `removeChunks` | `removeChunks` (7) | `StorageOperator::removeChunks` | 逐 op `tag.channel`（`slots=maxNumChunkIdsToProcess`） | `results[]` |
| （resync，非客戶端） | `syncStart` (8) / `syncDone` (9) | `ResyncWorker` 用同一個 `StorageMessenger` | — | — |
| `querySpaceInfo` | `spaceInfo` (10) | — | 空 `SpaceInfoReq` | `spaceInfos[]` |
| `createTarget` | `createTarget` (11) | — | `CreateTargetReq` | 空 |
| `queryChunk` | `queryChunk` (12) | — | 對鏈上每個 target 各發一次 | 每個 target 一份 `QueryChunkRsp` |
| `getAllChunkMetadata` | `getAllChunkMetadata` (13) | — | `targetId` | `chunkMetaVec` |
| `offlineTarget` | `offlineTarget` (16) | — | `OfflineTargetReq` | 空 |
| `removeTarget` | `removeTarget` (17) | — | `RemoveTargetReq` | 空 |

### 16.1 四個必須雙邊一致的協定不變式

1. **回應陣列與請求陣列等長、同序**。客戶端靠下標配對，長度不符即 `kFoundBug`（`StorageClientImpl.cc:1334-1338`）。
2. **channel seqnum 單調遞增**。客戶端保證全域遞增（`nextSeqNum_`），服務端據此判 `kDuplicateUpdate` / 回快取 / 執行（`ReliableUpdate.cc:65-72`）。任一邊違約，另一邊都會 DFATAL。
3. **range 操作的 seqnum 預留量**。客戶端預留 `maxNumChunkIdsToProcess` 個，服務端每處理一個 chunk 消耗一個（`StorageOperator.cc:974`）。
4. **`rdmabuf` 的 rkey 陣列覆蓋所有 HCA**。客戶端在 `registerIOBuffer` 時對所有卡註冊，服務端按自己的 `devId` 查（`RDMABuf.h:64-71`）。

### 16.2 只走 RDMA，不 fallback 到 TCP

`callMessengerMethod`（`StorageClientImpl.cc:843-886`）：

```cpp
for (const auto &serviceGroup : nodeInfo.app.serviceGroups) {
  for (const auto &address : serviceGroup.endpoints) {
    if (address.type == net::Address::Type::RDMA) {
      ... co_return response;                       // 找到第一個 RDMA 端點就用，用完就 return
    }
  }
}
XLOGF(DBG1, "No RDMA interface found on node: {:?}", nodeInfo);
co_return makeError(StorageClientCode::kNoRDMAInterface);
```

**節點若沒有標記為 RDMA 型別的端點，storage client 就無法與它通訊**——不會退回 TCP 端點。`net::Client::Config::force_use_tcp`（`src/common/net/Client.h:30`）確實存在，但它的作用是在 `serdeCtx()` 裡把位址轉成 TCP（`Client.h:56`：`config_.force_use_tcp() ? serverAddr.tcp() : serverAddr`），**前提仍是節點必須先宣告一個 RDMA 型別的端點讓上面的迴圈找得到**。

另外注意：迴圈找到第一個 RDMA 端點就發送並回傳，**多端點不會輪詢或重試**。

---

## 17. 值得注意的行為細節與落差

以下全部是從程式碼直接讀出的事實，不含對動機的臆測。

**（1）`queryLastChunk` 把 `opsPtrs` 而不是 `validOps` 送進重試迴圈。**

`StorageClientImpl.cc:2042-2053`：

```cpp
auto validOps = validateChunkIdRange(opsPtrs);
if (validOps.empty()) { XLOGF(ERR, "Empty list of ops: all the {} query ops are invalid", opsPtrs.size()); goto exit; }
co_await sendOpsWithRetry<QueryLastChunkOp>(requestCtx, chanAllocator_, opsPtrs, sendOps, ...);
//                                                                     ^^^^^^^^
```

對照 `removeChunks`（`:2152-2156`）與 `truncateChunks`（`:2283-2287`）傳的都是 `validOps`。由於 `sendOpsWithRetry` 開頭會對所有 op 做 `resetResult()`（`:1156`），`validateChunkIdRange` 剛設好的 `kInvalidArg` 會被清掉，無效的 op 會照樣被送出並重試。`validOps` 在這個函式裡只被用來判斷「是否全部無效」。

**（2）三個 `max_batch_bytes` 設定項無作用**（§9.3）。

**（3）`userCallBytes` 只累計每個節點最後一批的位元組數**（§9.5）。

**（4）inline 判定的 `<` / `<=` 不對稱**（§7.5 第 4 點）。

**（5）讀路徑的緩衝區校驗是「一票否決」，寫路徑的 chunk 邊界校驗是逐 op**（§8.2）。

**（6）開啟重疊檢查會改變 IO 的處理順序**（§8.2 的 `std::sort`）。

**（7）`splitReadIOs` 的 `reserve` 恆為 0**，因為 `ReadIO::chunkSize` 永遠是 0（§8.3）。

**（8）`ManualModeStrategy` 上方的註解是錯的**（`TargetSelection.cc:108` 寫 `// Always select the head target`）。

**（9）`LoadBalance` / `RoundRobin` 的計數表是程序級全域的，且一條鏈退化就會清空整張表**（§6.1）。

**（10）`processBatches` 的 `CoTask<bool>` 回傳值從未被讀取**（§10.4）。

**（11）`setResultOfOp` 與 `setErrorCodeOfOp` 兩個巨集的函式體逐字相同**（`StorageClientImpl.cc:178-202`）。

**（12）`StorageMessenger` 的 `update` / `syncStart` / `syncDone` 三個方法在客戶端沒有呼叫點**——它們服務的是 storage 服務端的轉發與 resync 路徑（§4.1）。

**（13）`StorageClientInMem` 的 `failedOps` 處理在五個方法之間不一致**（§14.3）。

**（14）`StorageClientBlobImpl.h` 無法編譯**（§15）。

---

## 18. 檔案索引

| # | 檔案 | 行數 | 職責 | 關鍵內容 |
|---|---|---|---|---|
| 1 | `src/client/storage/StorageClient.h` | 593 | **對外介面與全部設定**。定義 `StorageClient` 純虛基底、五個 op 型別、`IOBuffer`、`RoutingTarget`、三層 Config、`ReadOptions`/`WriteOptions`/`DebugOptions`/`RetryOptions` | `MethodType`（13 個）、`RetryConfig`（10s/30s/60s/1）、`OperationConcurrency`（128/4MB/32/8）、`max_concurrent_updates()`、13 個純虛擬函式、`RoutingTarget` 的 channel 洩漏 DFATAL |
| 2 | `src/client/storage/StorageClient.cc` | 112 | **工廠與工廠方法的基底實作**。`create()` 按 `implementation_type` 分派；五個 `create*Op()`；`registerIOBuffer()` | channel 池大小校驗（超過 65535 直接回 `nullptr`）、`nextRequestId_` 的兩種配置時機、RDMA 記憶體註冊的四個指標 |
| 3 | `src/client/storage/StorageClientImpl.h` | 239 | **RPC 實作的類別宣告**。私有方法分層（`*WithRetry` / `*WithoutRetry` / `sendBatchRequest` / `callMessengerMethod`）；兩個限流器類別 | `OperationConcurrencyLimit`（雙檢查鎖的 per-server semaphore map）、`HotLoadOperationConcurrencyLimit`（config callback → `changeUsableTokens`）、五個限流器成員、`getStorageMessengerForUpdates()` |
| 4 | `src/client/storage/StorageClientImpl.cc` | 2560 | **核心實作**。指標定義、`ClientRequestContext`、錯誤分類、七個巨集、路由解析、批次分組、重試迴圈、五條操作路徑、五個管理操作 | `isPermanentError`/`isTemporarilyUnavailable`/`isFastRetryError`、`selectRoutingTargetForOps`、`groupOpsByNodeId`、`sendOpsWithRetry`、`sendBatchRequest`、`sendWriteRequestsSequentially`、`isLatestRoutingInfo` 巨集、`allocateChannelsForOps` 巨集 |
| 5 | `src/client/storage/StorageMessenger.h` | 97 | **RPC 轉接層介面**。把 `StorageSerde` 的 14 個方法包成統一簽章的協程函式 | 統一簽章 `(Address, Req, UserRequestOptions*, serde::Timestamp*)`；持有一個 `net::Client` |
| 6 | `src/client/storage/StorageMessenger.cc` | 223 | **RPC 轉接層實作**。14 個方法全部轉呼叫同一個模板 | `callSerdeRpcMethod<Req, Rsp, rpcMethod>`：建 `serdeCtx` → `co_await` → 失敗多印一行帶 address/timeout 的 ERR |
| 7 | `src/client/storage/TargetSelection.h` | 71 | **副本選取的介面與設定**。`SlimTargetInfo` / `SlimChainInfo` / `TargetSelectionMode`（7 值）/ `TargetSelectionOptions` / `TargetSelectionStrategy` 基底 | `selectAnyTarget()` 決定是否參與 failover 過濾；「每批建一個策略實例」的註解；traffic zone 與 Head/Tail 互動的註解 |
| 8 | `src/client/storage/TargetSelection.cc` | 152 | **六種選取策略 + 工廠**。LoadBalance / RoundRobin / RandomTarget / Tail / Head / Manual | 兩個程序級全域 `AtomicUnorderedInsertMap`（32K / 1M）；LoadBalance 的「隨機起點 + 批內計數 + 全域計數」三級決策；`reset()` 清全域表 |
| 9 | `src/client/storage/UpdateChannelAllocator.h` | 36 | **更新去重 channel 的配置器介面**。`kChannelIdNumBits=16`、`kMaxNumChannels=65535` | `stack<ChannelId>` + 全域 `atomic nextSeqNum_`；標頭註解直接說明「用於服務端去重」 |
| 10 | `src/client/storage/UpdateChannelAllocator.cc` | 81 | **配置器實作**。`allocate(channel, slots)` 雙模式、`release()`、解構時的洩漏檢查 | id 0 保留為哨兵；`slots` 對應服務端範圍操作消耗的序號數；解構時檢查所有 channel 都已歸還（重複 id / 數量不符都 DFATAL） |
| 11 | `src/client/storage/StorageClientInMem.h` | 111 | **記憶體版實作的宣告**。`ChunkData` / `Chain` / `chains_`；額外的 `injectErrorOnChain` | 以 `ChainId` 為單位存資料（不區分副本）；`folly::Synchronized` + `folly::coro::Mutex` 兩層鎖；`gtest_prod.h` 說明它是測試導向 |
| 12 | `src/client/storage/StorageClientInMem.cc` | 395 | **記憶體版實作**。13 個介面方法 + `processQueryResults` 分頁模擬 | `GET_CHAIN` 巨集（拿鎖 + 檢查注入錯誤）、刻意的 `randomShuffle`、寫死的 `maxNumResultsPerQuery=3`、`querySpaceInfo` 的假資料、`createTarget` 等永遠成功 |
| 13 | `src/client/storage/StorageClientBlobImpl.h` | 80 | **孤兒標頭**。宣告一個從未實作、從未引用、且無法編譯的第三種實作 | 引用不存在的 `ListChunksOp` / `CopyIO`；`override` 了基底類別沒有的 `listChunks` / `batchCopy`；漏掉 `offlineTarget` / `removeTarget` 兩個純虛擬函式 |
| — | `src/client/storage/CMakeLists.txt` | 1 | 建置定義 | `target_add_lib(storage-client storage-fbs common mgmtd-client)` |

**依賴方向**：

```
StorageClient.h ◀─── StorageClientImpl.h ◀─── StorageClientImpl.cc
      ▲                    │                        │
      │                    ├── StorageMessenger.h ──┴── StorageMessenger.cc
      │                    └── UpdateChannelAllocator.h ── .cc
      │
      ├── TargetSelection.h ── TargetSelection.cc
      ├── UpdateChannelAllocator.h
      │
      ◀─── StorageClientInMem.h ◀─── StorageClientInMem.cc
      ◀─── StorageClientBlobImpl.h    （無 .cc，無引用）

外部依賴：
  fbs/storage/Common.h        RPC 型別、ChunkId、IOResult、FeatureFlags、DebugFlags
  fbs/storage/Service.h       StorageSerde 服務定義（14 個方法）
  client/mgmtd/ICommonMgmtdClient.h  路由快照 + listener
  client/mgmtd/RoutingInfo.h  getChain / getNode / getTarget
  common/net/Client.h         連線池、執行緒池、serde context
  common/net/ib/RDMABuf.h     記憶體註冊、rkey、remote buf 描述子
  common/utils/Semaphore.h    可變容量 semaphore
  common/utils/ExponentialBackoffRetry.h  退避器
  common/utils/StatusCodeConversion.h     服務端碼 → 客戶端碼
  common/monitor/Recorder.h   30 個指標
```

---

## 附錄 A：一次 `batchRead` 的完整呼叫鏈（含行號）

```
StorageClientImpl::batchRead                                        StorageClientImpl.cc:1563
 └─ ClientRequestContext ctor                                                        :73
 └─ createVectorOfPtrsFromOps<ReadIO>                                              :1017
 └─ batchReadWithRetry                                                             :1577
     ├─ validateDataRange<ReadIO>                                                   :889
     │   ├─ std::sort by data ptr（若 check_overlapping_read_buffers）                :894
     │   ├─ buffer/data null 檢查                                                    :905
     │   ├─ buffer->contains(data, length)                                           :916
     │   └─ 重疊檢查                                                                  :931
     ├─ splitReadIOs（若 max_read_io_bytes > 0）                                      :953
     └─ sendOpsWithRetry<ReadIO>                                                    :1143
         ├─ ExponentialBackoffRetry ctor                                            :1149
         ├─ [每輪] backoffRetry.getWaitTime() → requestTimeout                       :1159
         ├─ [每輪] batchReadWithoutRetry                                            :1642
         │   ├─ Default → LoadBalance                                               :1648
         │   ├─ getCurrentRoutingInfo()                                             :1652
         │   ├─ selectRoutingTargetForOps<ReadIO>                                    :489
         │   │   ├─ getChainInfo                                                     :331
         │   │   ├─ selectServingTargets                                             :418
         │   │   │   └─ getTargetInfo / getNodeInfo / selectNodeByTrafficZone        :385/:355
         │   │   ├─ failover 過濾（僅 selectAnyTarget()）                              :547
         │   │   ├─ strategy->reset()（若鏈退化）                                      :587
         │   │   └─ strategy->selectTarget(slimChain)               TargetSelection.cc:20
         │   ├─ groupOpsByNodeId<ReadIO>                                            :1029
         │   │   ├─ 已有 channel 的 op 分流                                          :1041
         │   │   ├─ calcAvgSize + 均勻切批                                           :1050
         │   │   ├─ random_shuffle                                                  :1102
         │   │   └─ 已有 channel 的批次插到最前                                        :1109
         │   └─ processBatches<ReadIO>(parallel)                                    :1124
         │       └─ [每批] sendReq lambda                                           :1665
         │           ├─ perServerSemaphore.coWait()                                 :1667
         │           ├─ concurrencySemaphore.coWait()                               :1670
         │           ├─ isLatestRoutingInfo                                          :633
         │           ├─ requestCtx.logWaitingTime()                                  :101
         │           ├─ buildBatchRequest<ReadIO, BatchReadReq>                      :670
         │           │   ├─ buffer->subrange(...).toRemoteBuf()                      :691
         │           │   └─ FeatureFlags 組裝                                        :704
         │           ├─ sendBatchRequest<..., &StorageMessenger::batchRead>         :1302
         │           │   ├─ getNodeInfo                                             :1309
         │           │   ├─ callMessengerMethod                                      :823
         │           │   │   ├─ RequestInfo::canceled() 檢查                          :838
         │           │   │   ├─ 找 RDMA 端點                                          :843
         │           │   │   └─ StorageMessenger::batchRead        StorageMessenger.cc:38
         │           │   │       └─ callSerdeRpcMethod                              :14
         │           │   ├─ results.size() != ops.size() → kFoundBug                :1334
         │           │   ├─ setResultOfOp 逐 op                                     :1354
         │           │   └─ releaseChannelsForOp（成功者）                            :1377
         │           ├─ SEND_DATA_INLINE 回填                                       :1697
         │           └─ verifyChecksum                                              :1720
         ├─ [每輪] 錯誤分類 + numFailures 累加                                        :1225
         └─ [每輪] co_await folly::coro::sleep(waitTime)                            :1294
     ├─ 合併 splittedIOs 結果                                                        :1607
     ├─ collectFailedOps                                                             :211
     └─ reportNumFailedOps                                                           :250
```

## 附錄 B：預設設定速查

| 設定項 | 預設值 | 熱更新 | 來源 |
|---|---|---|---|
| `implementation_type` | `RPC` | ❌ | `StorageClient.h:417` |
| `chunk_checksum_type` | `CRC32C` | ❌ | `StorageClient.h:418` |
| `create_net_client_for_updates` | `false` | ❌ | `StorageClient.h:419` |
| `check_overlapping_read_buffers` | `true` | ✅ | `StorageClient.h:420` |
| `check_overlapping_write_buffers` | `false` | ✅ | `StorageClient.h:421` |
| `max_inline_read_bytes` | `0` | ✅ | `StorageClient.h:422` |
| `max_inline_write_bytes` | `0` | ✅ | `StorageClient.h:423` |
| `max_read_io_bytes` | `0`（不切分） | ✅ | `StorageClient.h:424` |
| `retry.init_wait_time` | `10s` | ✅ | `StorageClient.h:363` |
| `retry.max_wait_time` | `30s` | ✅ | `StorageClient.h:364` |
| `retry.max_retry_time` | `60s` | ✅ | `StorageClient.h:365` |
| `retry.max_failures_before_failover` | `1` | ✅ | `StorageClient.h:366` |
| `traffic_control.*.max_batch_size` | `128` | read/query ✅ | `StorageClient.h:379, 388` |
| `traffic_control.*.max_batch_bytes` | `4MB` | read/query ✅ | `StorageClient.h:380, 389` |
| `traffic_control.*.max_concurrent_requests` | `32` | read/query ✅ | `StorageClient.h:381, 390` |
| `traffic_control.*.max_concurrent_requests_per_server` | `8` | read/query ✅ | `StorageClient.h:382, 391` |
| `traffic_control.*.random_shuffle_requests` | `true` | ✅ | `StorageClient.h:383, 392` |
| `traffic_control.*.process_batches_in_parallel` | `true` | ✅ | `StorageClient.h:384, 393` |
| `ReadOptions.enableChecksum` | `false` | ✅ | `StorageClient.h:192` |
| `ReadOptions.allowReadUncommitted` | `false` | ✅ | `StorageClient.h:193` |
| `WriteOptions.enableChecksum` | `true` | ✅ | `StorageClient.h:211` |
| `targetSelection.mode` | `Default` | ✅ | `TargetSelection.h:43` |
| `targetSelection.targetIndex` | `0` | ✅ | `TargetSelection.h:44` |
| `targetSelection.trafficZone` | `""`（不過濾） | ✅ | `TargetSelection.h:45` |
| `net_client.default_timeout` | `1s`（被 `requestTimeout` 覆寫） | ✅ | `configs/hf3fs_fuse_main.toml:237` |
| `net_client.default_send_retry_times` | `1`（被 `options.sendRetryTimes=1` 覆寫） | ✅ | `StorageClientImpl.cc:836` |
| 衍生：`max_concurrent_updates()` | `12288` | — | `StorageClient.h:404-408` |
| 衍生：`kMaxNumChannels` | `65535` | — | `UpdateChannelAllocator.h:18` |
| 衍生：`Semaphore::maxTokens` | `4096` | — | `src/common/utils/Semaphore.h:13` |
