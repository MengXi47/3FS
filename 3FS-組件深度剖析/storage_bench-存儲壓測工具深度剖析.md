# `storage_bench` 存儲壓測工具深度剖析

> 對應原始碼：`benchmarks/storage_bench/StorageBench.h`（907 行）、`StorageBench.cc`（292 行）、`CMakeLists.txt`
> 同目錄夥伴：`benchmarks/fio_usrbio/`（fio 外掛，獨立 Makefile）
> 關鍵依賴：`tests/lib/UnitTestFabric.h`（單元測試骨架）
> 風格基準：[`../3FS-元資料層深度剖析.md`](../3FS-元資料層深度剖析.md)

---

## 0. 一句話總結

`storage_bench` 是一個**完全繞過 meta server 的存儲層壓測器**：它不開檔案、不解析路徑、不查 layout，而是直接用 `(chainId, chunkId, offset, length)` 四元組對 storage 節點下 `batchRead` / `batchWrite`。它最不顯而易見的設計是**繼承自單元測試骨架 `test::UnitTestFabric`**，因此同一個執行檔有兩種完全不同的形態：一種是在自己的進程裡拉起 N 個 storage server 做**自封閉基準測試**，另一種是連上真實叢集的 mgmtd 做**線上壓測**。

它揭露的關心點是明確的：作者關心的不是「檔案系統有多快」，而是「**RDMA 傳輸、磁碟 IO、批次聚合這三段各自貢獻了多少延遲**」——這從 `--benchmarkNetwork` / `--benchmarkStorage` 這兩個把對方那一段短路掉的旗標可以直接讀出來。

同時它有幾處確實的缺陷（讀取統計被記成寫入參數、TDigest 精度設定被覆蓋失效、CSV 欄位標籤與實際計算不符），本報告一併舉證。

---

## 1. `benchmarks/` 目錄全貌

```
benchmarks/
├── CMakeLists.txt                    1 行 → add_subdirectory(storage_bench)
├── storage_bench/
│   ├── CMakeLists.txt                1 行
│   ├── StorageBench.h              907 行  ← 全部實作（header-only 風格）
│   └── StorageBench.cc             292 行  ← 60 個 gflags + main()
└── fio_usrbio/                              ← 不在 CMake 建置樹裡
    ├── Makefile                     23 行
    ├── README.md                    40 行
    └── hf3fs_usrbio.cpp            286 行
```

**`benchmarks/CMakeLists.txt` 只有一行**：

```cmake
add_subdirectory(storage_bench)
```

`fio_usrbio` 沒有被納入，它有自己的 `Makefile`，需要三個外部路徑變數（`HF3FS_LIB_DIR` / `HF3FS_INCLUDE_DIR` / `FIO_SRC_DIR`）才能建。這是刻意的：fio 外掛必須 include fio 自己的 `config-host.h` 與 `fio.h`（`Makefile:11` 的 `-include config-host.h`），而 fio 的原始碼不在 3FS repo 裡。

兩個 benchmark 的分工是互補的，見 §10。

---

## 2. 它測的是哪一段路徑

```
┌─────────────────────────────────────────────────────────────────────┐
│  應用程式                                                            │
│    ↓ open("/mnt/3fs/a.bin")                                         │
│  ┌───────────────────────────────────────────────┐                  │
│  │ FUSE / USRBIO                                 │  ← fio_usrbio 測這裡
│  ├───────────────────────────────────────────────┤                  │
│  │ meta client → meta server                     │                  │
│  │   路徑解析、inode、layout、chunk id 計算       │  ← storage_bench 全部跳過
│  │   Layout::getChainOfChunk(inode, chunkIndex)  │                  │
│  ├───────────────────────────────────────────────┤                  │
│  │ ★ (chainId, chunkId, offset, length) ★        │  ← storage_bench 從這裡開始
│  ├───────────────────────────────────────────────┤                  │
│  │ StorageClient                                 │                  │
│  │   批次聚合、路由查詢、重試、traffic control     │  ← 測                │
│  ├───────────────────────────────────────────────┤                  │
│  │ RDMA / TCP 傳輸                                │  ← 測（可用旗標短路）  │
│  ├───────────────────────────────────────────────┤                  │
│  │ storage server：CRAQ 鏈式複製 + AIO 磁碟 IO     │  ← 測（可用旗標短路）  │
│  └───────────────────────────────────────────────┘                  │
└─────────────────────────────────────────────────────────────────────┘
```

**chunk id 是憑空造的，不對應任何檔案。** `generateChunkIds()`（`StorageBench.h:107-140`）自己編出 128 位元的 chunk id，前 2 bytes 是 `--chunkIdPrefix`（預設 `0xFFFF`）：

```cpp
uint64_t chunkIdPrefix64 = ((uint64_t)benchOptions_.chunkIdPrefix) << (UINT64_WIDTH - UINT16_WIDTH);
```

對照元資料層報告 §6 的 `ChunkId` 佈局（`tenent[1] + reserved[1] + inode[8] + track[2] + chunk[4]`，big-endian），`0xFFFF` 前綴落在 `tenent` 與 `reserved` 這兩個保留 byte 上——真實檔案產生的 chunk id 這兩個 byte 恆為 0。**這是刻意的隔離**：壓測資料的 key space 與真實資料完全不重疊，即使壓測工具沒清乾淨（或 `--cleanupChunks` 沒開），也絕不會覆蓋任何檔案的 chunk。這個細節說明它是設計來**在生產叢集上跑**的。

有兩種 chunk id 生成策略（`StorageBench.h:126-138`）：

```cpp
for (auto chainId : chainIds_) {
  for (size_t chunkIndex = 0; chunkIndex < benchOptions_.numChunks; chunkIndex++) {
    if (benchOptions_.sparseChunkIds) {
      uint64_t chunkIdHigh = chunkIdPrefix64 | (folly::Random::rand64(randGen_) & 0x000000FFFFFFFFFF);
      uint64_t chunkIdLow = (folly::Random::rand64(randGen_) << UINT32_WIDTH) + chunkIndex;
      chunkInfos.push_back({chainId, ChunkId(chunkIdHigh, chunkIdLow), 0});
    } else {
      chunkInfos.push_back({chainId, ChunkId(instancePrefix, chunkIndex), 0});
    }
  }
}
if (benchOptions_.randomShuffleChunkIds) std::shuffle(chunkInfos.begin(), chunkInfos.end(), generator);
```

| 模式 | 效果 | 測什麼 |
|---|---|---|
| `--sparseChunkIds=false`（預設） | 同一個 coroutine 的 chunk id 共用高 64 位，低位是連號 | chunk id 在 storage 端 metadata 索引裡**連續**，接近真實檔案的分佈（元資料層報告 §6：ChunkId 用 big-endian 就是為了讓同一 inode 的 chunk 連續） |
| `--sparseChunkIds=true` | 每個 chunk id 幾乎全隨機 | 打散 storage 端的 metadata 索引（LevelDB/RocksDB 或 chunk engine），測**索引隨機存取**的成本 |

`--randomShuffleChunkIds` 則只打亂**存取順序**而不改 id 本身。三個旗標的組合讓人可以分離「id 空間分佈」與「存取順序」這兩個變因。

`randSeed` 可指定（`StorageBench.h:112`：`randGen_.seed(benchOptions_.randSeed)`），所以**兩次執行可以產生完全相同的 chunk id 集合**——這是 `--serverMode` / `--clientMode` 分離部署（見 §4.3）以及「先寫後讀分兩次跑」能運作的前提。

---

## 3. 意外的繼承：壓測器 is-a 單元測試

`StorageBench.h:26`：

```cpp
class StorageBench : public test::UnitTestFabric {
```

`UnitTestFabric`（`tests/lib/UnitTestFabric.h:167`）是 3FS 存儲層**單元測試**的共用骨架。CMake 也直說了（`benchmarks/storage_bench/CMakeLists.txt:1`）：

```cmake
target_add_bin(storage_bench "StorageBench.cc" test-fabric-lib storage-client storage memory-common follybenchmark gmock fdb mgmtd)
```

一個 benchmark 執行檔連了 `gmock` 與 `test-fabric-lib`，並且連了整個 `storage` 靜態庫（server 端實作，不只 client）。

繼承帶來的能力：

| `UnitTestFabric` 提供 | `StorageBench` 的用法 |
|---|---|
| `setUpStorageSystem()` | `setup()` 直接呼叫（`StorageBench.h:361`），在**本進程內**拉起 N 個 `StorageServer` |
| `createStorageServer(nodeIndex)` | 間接使用 |
| `buildRepliaChainMap()` + `createRoutingInfo()` | 合成假的 chain table（見 `UnitTestFabric.h:174-190` 的註解圖） |
| `chainIds_` / `storageClient_` / `clientConfig_` / `mgmtdClientConfig_` / `client_` / `clientId_` | 全部當成員直接用 |
| `writeToChunk()` / `readFromChunk()` | **未使用**（bench 自己寫了批次版） |
| `setTargetOffline()` / `stopAndRemoveStorageServer()` | 未使用（bench 不做故障注入拓撲變更） |

`UnitTestFabric` 內建的 chain 分配演算法（`tests/lib/UnitTestFabric.h:174-190` 的註解）決定了自封閉模式的拓撲：

```
 node1 | target1001 | target1002 | target1003
 node2 | target2001 | target2002 | target2003
 node3 | target3001 | target3002 | target3003
 node4 | target4001 | target4002 | target4003
  ↓ 對角線走訪
 chain1 | target1001 | target2001 | target3001
 chain2 | target4001 | target1002 | target2002
 chain3 | target3002 | target4002 | target1003
 chain4 | target2003 | target3003 | target4003
```

**這個複用是有代價的。** `SystemSetupConfig` 的欄位（`num_chains` / `num_replicas` / `num_storage_nodes`）在自封閉模式下是「要建幾個」的**指令**，但在 cluster 模式下就變成純粹的「使用者宣稱」——`connect()` 完全不驗證它們是否與真實 routing info 一致。而 CSV 統計檔又原封不動記錄這些數字（`StorageBench.h:430-432`）。所以在 cluster 模式下，`perfstats.csv` 裡的 `#storages` / `#chains` / `#replicas` 三欄**是命令列輸入的回音，不是叢集實況**。詳見 §7.3。

---

## 4. 三種執行形態

`main()` 與 `runBenchmarks()` 用三個布林旗標切出三條路（`StorageBench.cc:23-25`）：

```cpp
DEFINE_bool(serverMode, false, "Run in server mode");
DEFINE_bool(clientMode, false, "Run in client mode");
DEFINE_bool(clusterMode, false, "Run in cluster mode (get routing info from mgmtd)");
```

### 4.1 自封閉模式（預設，三個旗標全 false）

```
                單一 storage_bench 進程
  ┌───────────────────────────────────────────────────┐
  │  StorageBench（客戶端邏輯）                        │
  │    ↓ storageClient_（RPC 實作）                    │
  │  ┌──────────┐ ┌──────────┐ ┌──────────┐          │
  │  │StorageSrv│ │StorageSrv│ │StorageSrv│  ← 同進程  │
  │  │ node 1   │ │ node 2   │ │ node 3   │          │
  │  │ LevelDB  │ │ LevelDB  │ │ LevelDB  │          │
  │  │ /tmp/... │ │ /tmp/... │ │ /tmp/... │          │
  │  └──────────┘ └──────────┘ └──────────┘          │
  │  FakeMgmtdClient（useFakeMgmtdClient=true）        │
  └───────────────────────────────────────────────────┘
```

`StorageBench.cc:145-161` 組出 `SystemSetupConfig`，注意倒數第三個參數 `true /*useFakeMgmtdClient*/`——不連真的 mgmtd，routing info 由 `UnitTestFabric` 合成。`--dataPaths` 預設是 `folly::fs::temp_directory_path()`（`StorageBench.cc:55`），`--metaStoreType` 可選 LevelDB / RocksDB / MemDB（`:28`）。

這個模式測的是「**在單機上、排除網路的情況下，storage 引擎本身能跑多快**」。`--metaStoreType=2`（MemDB）更可以把 chunk metadata 也排除，只剩 AIO 磁碟路徑。

### 4.2 叢集模式（`--clusterMode`）

```
   storage_bench 進程                       真實叢集
  ┌──────────────────────┐                 ┌──────────┐
  │ StorageBench         │───RPC──────────▶│  mgmtd   │
  │  MgmtdClientForClient│◀──routing info──│          │
  │  storageClient_      │                 └──────────┘
  │                      │                 ┌──────────┐
  │                      │───RDMA─────────▶│ storage  │
  │                      │                 │  ×N      │
  └──────────────────────┘                 └──────────┘
```

`connect()`（`StorageBench.h:142-308`）做四件事：

1. **IB 初始化**（`setupIBSock()`，`:310-352`）：解析 `--ibnetZones`（格式 `zone:subnet`）、套用 `--ibvDevices` 過濾器（預設 `mlx5_0,mlx5_1`）、設 `--defaultPKeyIndex`。注意 `ibConfig.set_allow_unknown_zone(false)`（`:340`）——**未知網路區直接失敗**，不允許蒙混。
2. **建立 mgmtd client**（`:149-192`）：關掉心跳（`set_enable_auto_heartbeat(false)`，`:151`）、開自動 refresh 與 client session 續期，三個間隔都設 3 秒。註冊的 session 描述是 `"StorageBench: {containerHostname}"`（`:187`）。與 [`migration_main`](migration_main-資料遷移工具深度剖析.md) 一樣走 `MgmtdClientForClient` + `NodeType::CLIENT` 的「外掛工具」路線。
3. **取 routing info 並選 chain**（`:196-284`）：最多重試 15 次、每次睡 1 秒。`--chainTableVersion=0` 表示取最新版（`:230-232`：`const auto iter = --tableVersions.cend();`）。
4. **選 chain 的三種方式**（`:246-269`），優先順序如下：

   | 條件 | 行為 | 行號 |
   |---|---|---|
   | `--storageNodeIds` 非空 | 選出**任一副本落在這些節點上**的所有 chain | `:246-259` |
   | 否則 `--chainIds` 非空 | 選出交集 | `:260-266` |
   | 否則 | 用整張 chain table | `:267-269` |

   第一種是最有價值的：它讓人可以「**壓測特定幾台機器**」而不必自己算出哪些 chain 經過它們。這對排查單機效能異常直接有用。

`--clusterMode` 下 `setUpStorageSystem()` **不會**被呼叫（`StorageBench.cc:224-234` 的 if/else），所以不會在本地拉起任何 server。

### 4.3 分離部署（`--serverMode` / `--clientMode`）

兩個進程分別跑在不同機器：

```
  機器 A：storage_bench --serverMode --listenPort 8000 ...
    → setup() 拉起 N 個 StorageServer
    → StorageBench.cc:244-248：while (true) { ::sleep(1); }   ← 永遠不做 IO

  機器 B：storage_bench --clientMode --storageEndpoints '1@RDMA://A:8000,...' ...
    → SystemSetupConfig 的 startStorageServer = !FLAGS_clientMode = false
      （StorageBench.cc:159）
    → 只建 client，連過去打
```

`--clientMode` 強制要求 `--storageEndpoints`（`StorageBench.cc:133-136`），格式 `nodeId@Address`（`:112-131` 解析）。

這個模式的價值：**排除 client 與 server 爭搶 CPU 的干擾**，同時仍然不需要一整套 mgmtd + FDB 部署。兩邊靠相同的 `--randSeed` 與 chunk id 參數對齊 key space。

---

## 5. 負載模型

### 5.1 併發結構

```
 testExecutor_ : folly::CPUThreadPoolExecutor(--numTestThreads)
       │
       ├── coroutine 0 ──┐
       ├── coroutine 1   │  共 --numCoroutines 個
       ├── ...           │  每個獨立擁有：
       └── coroutine N-1 ┘    · chunkInfos_[i]（自己的 chunk 集合）
                              · 一塊 aligned + RDMA-registered 記憶體
                              · 自己的 TDigest
                              · 每輪送出 batchSize 個 IO 後 co_await 整批完成
```

`StorageBench.h:722-727`（寫）與 `:757-759`（讀）：

```cpp
for (size_t instanceId = 0; instanceId < benchOptions_.numCoroutines; instanceId++) {
  writeTasks.push_back(
      batchWrite(instanceId, benchOptions_.writeBatchSize, benchOptions_.writeSize, benchOptions_.numWriteSecs)
          .scheduleOn(folly::Executor::getKeepAliveToken(testExecutor_))
          .start());
}
auto results = folly::coro::blockingWait(folly::coro::collectAllRange(std::move(writeTasks)));
```

**有效佇列深度 = `numCoroutines × batchSize`**。這兩個維度是刻意分開的：

- `numCoroutines` 增加**獨立的請求流**（每個流內部是串行的「送一批 → 等一批」）
- `batchSize` 增加**單次 RPC 裡打包的 IO 數**

前者測的是 client 的併發處理能力與 server 的多連線行為，後者測的是**批次聚合的攤提效果**。3FS 的 storage client 有 `traffic_control` 限制併發請求數，`generateChunks()` 特地用它反推批次大小（`StorageBench.h:682-684`）：

```cpp
size_t writeBatchSize =
    std::max(benchOptions_.writeBatchSize,
             clientConfig_.traffic_control().write().max_concurrent_requests() / benchOptions_.numCoroutines);
```

也就是「造測試資料」這個階段會自動把批次撐到把 traffic control 的額度用滿——因為那階段只求快，延遲數字不重要。

`--readBatchSize` / `--writeBatchSize` / `--removeBatchSize` 若為 0 則各自 fallback 到 `--batchSize`（`StorageBench.h:102-104`）。

### 5.2 寫入模式：先順序填滿，再隨機覆寫

`batchWrite`（`StorageBench.h:509-522`）：

```cpp
auto &[chainId, chunkId, chunkSize] = chunkInfos[seqChunkIndex++ % chunkInfos.size()];
size_t writeOffset = 0;
size_t writeLength = 0;

if (chunkSize < setupConfig_.chunk_size()) {
  writeOffset = chunkSize;
  writeLength = std::min(writeSize, setupConfig_.chunk_size() - writeOffset);
  chunkSize += writeLength;
  numCreatedChunks += chunkSize == setupConfig_.chunk_size();
} else {
  writeOffset = folly::Random::rand32(0, setupConfig_.chunk_size() - writeSize);
  writeLength = writeSize;
}
```

`ChunkInfo::size`（`StorageBench.h:72-76`）是**客戶端側維護的「這個 chunk 已寫到哪」游標**。行為分兩階段：

```
階段一（chunk 尚未寫滿）：
  chunk: [====已寫====|<- writeSize ->|            ]
                       ↑ writeOffset = 目前 size
  → 嚴格順序追加，模擬「檔案循序寫入」

階段二（chunk 已滿）：
  chunk: [=========================================]
              ↑ writeOffset = rand(0, chunkSize - writeSize)
  → 隨機位置覆寫，模擬「原地更新」
```

**這兩個階段測的是完全不同的東西。** 3FS 的 storage 寫入走 CRAQ 鏈式複製 + copy-on-write chunk（見存儲層深度報告）：追加寫可能落在同一個實體 chunk 的尾端，覆寫則會觸發新版本的產生與舊版本回收。混在同一個 benchmark 裡意味著結果會隨執行時間漂移——前段是追加、後段是覆寫。

實務上這由 `run()` 的階段劃分緩解：`generateChunks()` 用 `numWriteSecs=0` 呼叫 `batchWrite`，此時終止條件是 `numCreatedChunks >= chunkInfos.size()`（`StorageBench.h:504`），也就是**只跑階段一，把所有 chunk 填滿就停**。而 `runWriteBench()` 用 `numWriteSecs>0` 呼叫，跑固定時長。

**寫入緩衝區的一個細節**：`batchWrite` 只配置**一個** chunk 大小的記憶體塊（`StorageBench.h:456-457`），批次裡所有 IO 都指向 `&memoryBlock[writeOffset]`（`:529`）。所以同一批的多個 IO 會共用重疊的來源記憶體。對 RDMA 讀取來說這完全合法（唯讀），但它意味著**測不出「來源記憶體分散」造成的 cache/TLB 壓力**。讀取側則相反，每個 IO 有自己的落點（`:566` 配置 `alignedBufSize * readBatchSize`，`:625` 用 `&memoryBlock[readIndex * alignedBufSize]`）。

### 5.3 讀取模式：均勻隨機 + 可控對齊

`batchRead`（`StorageBench.h:616-629`）：

```cpp
uint64_t randChunkIndex = folly::Random::rand64(0, chunkInfos.size());
const auto &[chainId, chunkId, chunkSize] = chunkInfos[randChunkIndex];
uint32_t offset = folly::Random::rand32(0, setupConfig_.chunk_size() - benchOptions_.readSize);
uint32_t alignedOffset = ALIGN_LOWER(offset, offsetAlignment);
```

`offsetAlignment` 的預設值很值得注意（`:601-602`）：

```cpp
size_t offsetAlignment =
    benchOptions_.readOffAlignment ? benchOptions_.readOffAlignment : std::max(size_t(1), benchOptions_.readSize);
```

**不指定 `--readOffAlignment` 時，對齊粒度 = 讀取大小本身。** 這讓讀取自然落在「第 k 個 readSize 區塊」的邊界上，模擬應用程式以固定區塊讀檔的行為。顯式指定 `--readOffAlignment=1` 則能測「完全未對齊的讀取」——這對 O_DIRECT 路徑與 RDMA 的 scatter-gather 效率是有意義的變因。

`--memoryAlignment` 則控制記憶體端的對齊（`:456`、`:565` 的 `ALIGN_UPPER`），底層一律用 `folly::aligned_malloc(size, sysconf(_SC_PAGESIZE))` 保證頁對齊，`--memoryAlignment` 是在頁對齊之上再加的偏移粒度。

**兩個對齊旗標可獨立調整，這本身就是一個訊號**：作者關心 3FS 在非對齊 IO 上的退化程度。

### 5.4 RDMA 記憶體註冊

每個 coroutine 開始時註冊一次（`StorageBench.h:468-477`、`:572-577`）：

```cpp
auto regRes = storageClient_->registerIOBuffer(memoryBlock, memoryBlockSize);
if (regRes.hasError()) { co_return regRes.error().code(); }
auto ioBuffer = std::move(*regRes);
```

註冊成本（`ibv_reg_mr`，涉及頁釘選）被排除在計時之外——這是正確的建模：真實應用（USRBIO）也是啟動時註冊一次然後重複使用。

---

## 6. Debug 旗標：把三段路徑各自短路

這是整個工具**最能說明作者關心什麼**的部分。`StorageBench.h:479-485`（寫）與 `:591-597`（讀）：

```cpp
WriteOptions options;
options.set_enableChecksum(benchOptions_.verifyWriteChecksum);
options.debug().set_bypass_disk_io(benchOptions_.benchmarkNetwork);
options.debug().set_bypass_rdma_xmit(benchOptions_.benchmarkStorage);
options.debug().set_inject_random_server_error(benchOptions_.injectRandomServerError);
options.debug().set_inject_random_client_error(benchOptions_.injectRandomClientError);
options.retry().set_retry_permanent_error(benchOptions_.retryPermanentError);
```

**注意映射是交叉的**：`--benchmarkNetwork` 設 `bypass_disk_io`，`--benchmarkStorage` 設 `bypass_rdma_xmit`。語意是「要測 X，就把非 X 的部分短路掉」。

這兩個旗標會變成 RPC 上的 feature flag（`src/client/storage/StorageClientImpl.cc:651-652`）：

```cpp
if (debugOptions.bypass_disk_io()) BITFLAGS_SET(featureFlags, hf3fs::storage::FeatureFlags::BYPASS_DISKIO);
if (debugOptions.bypass_rdma_xmit()) BITFLAGS_SET(featureFlags, hf3fs::storage::FeatureFlags::BYPASS_RDMAXMIT);
```

server 端依此跳過對應階段。讀取路徑（`src/storage/service/StorageOperator.cc:157-179`）：

```cpp
if (BITFLAGS_CONTAIN(req.featureFlags, FeatureFlags::BYPASS_DISKIO)) {
  for (AioReadJobIterator it(&batch); it; it++) {
    it->result().lengthInfo = it->readIO().length;   // ← 直接偽造成功結果
    batch.finish(&*it);
  }
} else {
  co_await components_.aioReadWorker.enqueue(...);   // ← 真的下 AIO
}
...
} else if (!BITFLAGS_CONTAIN(req.featureFlags, FeatureFlags::BYPASS_RDMAXMIT)) {
  auto ibSocket = ctx.transport()->ibSocket();
  ...                                                 // ← 真的做 RDMA write
}
```

寫入路徑同理（`StorageOperator.cc:565`、`:599-604`、`:624-628`，連 `doCommit` 也吃這個旗標）。

於是得到四種可測組合：

```
                       bypass_rdma_xmit
                    false            true
                 ┌──────────────┬──────────────┐
   bypass  false │  完整路徑     │ --benchmarkStorage │
   _disk_io      │  （端到端）   │ 只有 RPC + 磁碟     │
                 ├──────────────┼──────────────┤
           true  │--benchmark   │  兩個都開      │
                 │ Network      │  只剩 RPC 框架 │
                 │ RPC + RDMA   │  的純開銷      │
                 │ 不碰磁碟      │              │
                 └──────────────┴──────────────┘
```

四個角落一減，就能算出每一段的貢獻：

- `完整 − benchmarkNetwork` ≈ 磁碟 IO 的成本
- `完整 − benchmarkStorage` ≈ RDMA 資料傳輸的成本
- `兩個都開` ≈ RPC 序列化 + 排程 + CRAQ 協定往返的固定開銷

**這就是這個工具存在的核心理由。** 光看端到端數字無法回答「延遲花在哪」；有了這兩個旗標，一次跑四組就能做出延遲分解。相較之下，`fio_usrbio` 只能測完整路徑。

另外三個 debug 旗標：

| 旗標 | 對應 client 設定 | 用途 |
|---|---|---|
| `--injectRandomServerError` | `inject_random_server_error`（`src/client/storage/StorageClient.h:164`） | 測**重試路徑在壓力下的行為**——不是測錯誤處理正確性（那是單元測試的事），而是測「重試會讓延遲尾巴變多長」 |
| `--injectRandomClientError` | `inject_random_client_error` | 同上，client 側 |
| `--retryPermanentError` | `retry().set_retry_permanent_error` | 讓永久性錯誤也重試 |
| `--ignoreIOError` | 純 bench 側（`StorageBench.h:542`、`:638`） | 跳過結果驗證迴圈；與注入錯誤搭配使用時必開，否則第一個注入的錯誤就會終止整個 coroutine |

注意 checksum 的預設值不對稱（`StorageBench.cc:15-16`）：

```cpp
DEFINE_bool(verifyReadChecksum, false, "Verify the checksum of read IOs");
DEFINE_bool(verifyWriteChecksum, true, "Verify the checksum of write IOs");
```

**寫入預設驗 checksum，讀取預設不驗。** 這反映了一個判斷：寫入的 checksum 是資料完整性的來源，不能省；讀取端的驗證是額外的 CPU 成本，而多數人跑 benchmark 是要看頻寬上限。`StorageClient.h:203, 221` 還有一層邏輯——bypass 任一段時 checksum 自動失效：

```cpp
return enabled && !debug().bypass_disk_io() && !debug().bypass_rdma_xmit();
```

`--verifyReadData` 則是更強的驗證（`StorageBench.h:462-466`、`:581-585`、`:651-665`）：寫入時把緩衝區填成 `memoryBlock[i] = i`（byte index 截斷成 u8 的遞增樣式），讀回來用 `std::mismatch` 逐 byte 比對，失敗回 `kFoundBug`。

---

## 7. 計時與統計

### 7.1 計時點：批次 RPC 的往返時間

`StorageBench.h:535-540`（寫）、`:631-636`（讀）：

```cpp
auto rpcStart = hf3fs::SteadyClock::now();
co_await storageClient_->batchWrite(writeIOs, flat::UserInfo(), options);
auto elapsedMicro = std::chrono::duration_cast<std::chrono::microseconds>(hf3fs::SteadyClock::now() - rpcStart);
elapsedMicroSecs.push_back(elapsedMicro.count());
```

**測的是「整批完成」的時間，不是單個 IO 的時間。** 這一點對解讀數字至關重要：`batchSize=32` 時報出的「P99 latency 5ms」意思是「一整批 32 個 IO 全部完成的 P99 是 5ms」，而不是任何單一 IO 的延遲。批次內部的 IO 可能被拆到不同 chain、不同節點，整批延遲由最慢的那個決定——這是**尾延遲放大**（tail amplification）效應，`batchSize` 越大這個數字越悲觀。

建構 IO 物件的時間（`createWriteIO` / `createReadIO` 迴圈）被排除在外，只計 `co_await` 的部分。這是對的：物件建構是 benchmark 自己的開銷。

延遲樣本累積在 `std::vector<double> elapsedMicroSecs`（`:487`、`:599`）裡，跑完才一次併入 TDigest。代價是記憶體與樣本數成正比（60 秒 × 每秒數千批 = 數十萬個 double，約數 MB），好處是不干擾熱路徑。

### 7.2 TDigest：以及一處失效的精度設定

宣告與初始化（`StorageBench.h:70`、`:79-80`、`:94-95`）：

```cpp
static constexpr uint32_t kTDigestMaxSize = 1000;
...
std::vector<folly::TDigest> writeLatencyDigests_;
std::vector<folly::TDigest> readLatencyDigests_;
...
writeLatencyDigests_(benchOptions_.numCoroutines, folly::TDigest(kTDigestMaxSize)),
readLatencyDigests_(benchOptions_.numCoroutines, folly::TDigest(kTDigestMaxSize)),
```

`kTDigestMaxSize = 1000` 是 TDigest 的 centroid 數上限，越大分位數越精確。但實際使用時（`StorageBench.h:556-557`、`:668-669`）：

```cpp
folly::TDigest digest;                                        // ← 預設建構
writeLatencyDigests_[instanceId] = digest.merge(elapsedMicroSecs);
```

`folly::TDigest` 的預設建構子是 `explicit TDigest(size_t maxSize = 100)`（`third_party/folly/folly/stats/TDigest.h:78-79`），而 `merge()` 是 const 成員函式，回傳的新 TDigest 沿用 `this` 的 `maxSize_`。**於是 `digest.merge(...)` 產出的是 maxSize=100 的 TDigest，直接覆蓋掉建構子裡設好的 maxSize=1000 那個物件。**

結論：`kTDigestMaxSize` 這個常數**從未生效**，實際跑的是 folly 預設的 100 個 centroid。修法是一行：`folly::TDigest digest(kTDigestMaxSize);`。

影響有多大？TDigest 的分位數誤差在極端分位（P99、P999）處與 centroid 數呈反比。100 個 centroid 對 P50/P90 幾乎沒差，對 P99 會有可觀的偏差。考慮到這是個**專門用來看尾延遲**的工具，這個 bug 是實質的。

跨 coroutine 合併用靜態版本（`:705`、`:742`、`:774`）：

```cpp
auto mergedDigest = folly::TDigest::merge(writeLatencyDigests_);
```

### 7.3 兩套輸出：日誌與 CSV，分位數集合不一致

**日誌輸出**（`printLatencyDigest`，`StorageBench.h:381-389`）：

```cpp
for (double p : {0.1, 0.2, 0.5, 0.9, 0.95, 0.99}) {
  XLOGF(WARN, "{}%: {:10.1f}us", p * 100.0, digest.estimateQuantile(p));
}
```

**CSV 輸出**（`dumpPerfStats`，`:446-448`）：

```cpp
for (double p : {0.5, 0.75, 0.9, 0.95, 0.99}) {
  fout << fmt::format(",{:.1f}", digest.estimateQuantile(p));
}
```

日誌有 P10/P20（低分位，看「最好情況」）但沒有 P75；CSV 有 P75 但沒有低分位。兩套各自演化的痕跡。

CSV 表頭（`:407-410`）21 欄，資料列（`:428-448`）16 + 5 = 21 欄，數量對得上。但有兩處欄位語意問題：

**(a) 第 8 欄的標籤與計算不符。**

表頭寫：`effective batch size (batch size / #replicas)`
實際算（`:436`）：`double(batchSize) / setupConfig_.num_storage_nodes()`

除的是**節點數**不是**副本數**。在自封閉模式且 `numStorageNodes == numReplicas` 時湊巧一致，其他情況一律錯。

**(b) 前四欄在 cluster 模式下不反映實況。**

```cpp
fout << fmt::format("{},{},{},{},...",
                    testName,
                    setupConfig_.num_storage_nodes(),   // = FLAGS_numStorageNodes，預設 1
                    setupConfig_.num_chains(),          // = FLAGS_numChains，預設 1
                    setupConfig_.num_replicas(),        // = FLAGS_numReplicas，預設 1
                    ...
```

cluster 模式下 chain 是從真實 chain table 選出來的（`chainIds_.size()` 才是真實數量，`:283` 有 log），但 CSV 記的是命令列旗標。使用者若沒手動把 `--numChains` 等對齊真實叢集，CSV 就會是誤導性的。

### 7.4 衍生指標的計算

`dumpPerfStats:419-424`：

```cpp
auto elapsedMicro = std::chrono::duration_cast<std::chrono::microseconds>(elapsedTime);
double bandwidthMBps = totalGiB * 1024.0 / (elapsedMicro.count() / 1'000'000.0);
size_t ioSize = readIO ? benchOptions_.readSize : benchOptions_.writeSize;
size_t batchSize = readIO ? benchOptions_.readBatchSize : benchOptions_.writeBatchSize;
double iops = bandwidthMBps * 1024.0 * 1024.0 / ioSize;
double qps = bandwidthMBps * 1024.0 * 1024.0 / (batchSize * ioSize);
```

IOPS 與 QPS **不是獨立測量的**，而是從頻寬反推：`IOPS = 頻寬 / IO大小`、`QPS = IOPS / 批次大小`。這在 IO 大小固定時是準確的（本 bench 的讀取確實固定 `readSize`），但寫入路徑有階段一的 `std::min(writeSize, chunkSize - writeOffset)` 截斷（`:516`），最後一筆寫入可能小於 `writeSize`——所以寫入的 IOPS 會被**低估**（分母偏大）。影響很小，僅在 `chunk_size % writeSize != 0` 時的邊界那筆。

### 7.5 一個確實的 bug：讀取統計用了寫入參數

`runWriteBench`（`StorageBench.h:745`）：

```cpp
dumpPerfStats("batch write", mergedDigest, elapsedTime, totalGiB, false /*readIO*/);
```

`runReadBench`（`StorageBench.h:777`）：

```cpp
dumpPerfStats("batch read", mergedDigest, elapsedTime, totalGiB, false /*readIO*/);
```

**兩者都傳 `false`。** 讀取這一行應該是 `true`。

後果（回頭看 §7.4 的 `dumpPerfStats:421-422`）：讀取那一列 CSV 的

- `io size (bytes)` 欄記成 `--writeSize`（預設 131072）而非 `--readSize`（預設 4096）
- `batch size` 欄記成 `--writeBatchSize` 而非 `--readBatchSize`
- 因此 `IOPS` 與 `QPS` 兩欄也一併算錯

`bandwidth (MB/s)`、`elapsed time`、以及所有延遲分位數**不受影響**（它們不依賴 `readIO` 參數）。但只要 `readSize != writeSize`（預設就是 4096 vs 131072，差 32 倍），CSV 裡的讀取 IOPS 就會**低估 32 倍**。

`printThroughput` 與 `printLatencyDigest` 走的是另一條路徑（`:770-775`），日誌輸出是對的。所以這個 bug 只影響 CSV，且只影響三欄。

---

## 8. 執行流程

`run()`（`StorageBench.h:891-899`）：

```cpp
bool run() {
  if (benchOptions_.numWriteSecs > 0)
    if (runWriteBench() != StatusCode::kOK) return false;
  if (benchOptions_.generateTestData)
    if (generateChunks() != StatusCode::kOK) return false;
  if (benchOptions_.numReadSecs > 0)
    if (runReadBench() != StatusCode::kOK) return false;
  return true;
}
```

`runBenchmarks()` 外層（`StorageBench.cc:236-259`）：

```
  bench.generateChunkIds()                  ← 一律執行，先造出 id 集合
     │
  if (--cleanupChunksBeforeBench) bench.cleanup()
     │
  if (--serverMode) → while(true) sleep(1)  ← 永遠不回來
  else              → bench.run()
     │
  if (--truncateChunks) bench.truncate()
  if (--cleanupChunks)  bench.cleanup()
     │
  bench.teardown()
```

**`run()` 的順序值得留意：寫入 benchmark 排在造資料之前。**

這不是筆誤，而是一個取捨。若同時開 `--numWriteSecs` 與 `--generateTestData`：

1. `runWriteBench()` 先跑 N 秒，過程中 `ChunkInfo::size` 從 0 開始遞增，所以這 N 秒**全部落在「階段一：順序追加」**（§5.2）——測的是**建立新 chunk**的成本，而不是覆寫既有 chunk。
2. `generateChunks()` 接手，把剩下沒填滿的 chunk 補完（終止條件是 `numCreatedChunks >= chunkInfos.size()`）。
3. `runReadBench()` 這時所有 chunk 都是滿的，隨機讀不會讀到不存在的 chunk。

如果順序反過來（先 generate 再 write bench），寫入 benchmark 就會全部落在「階段二：隨機覆寫」。兩種都合理，但作者選了前者——**寫入 benchmark 測的是「首次寫入」而非「覆寫」**。這個選擇沒有寫在任何註解裡，是從程式碼順序推出來的，也是本 benchmark 最容易被誤讀的一點。

若只想測覆寫，正確做法是分兩次執行：第一次只開 `--generateTestData`，第二次只開 `--numWriteSecs` 並關掉 `--generateTestData`（用相同 `--randSeed` 保證 chunk id 一致）。但要注意第二次執行時 `ChunkInfo::size` 是新的記憶體物件，一律從 0 開始——**客戶端不知道 chunk 已經在 server 上滿了**，所以還是會走階段一。要真正測覆寫，得先 `--truncateChunks` 之外的手段，或修改程式碼。這是這個模型的一個真實限制。

### 清理與 truncate

`cleanup()`（`StorageBench.h:782-839`）每 `removeBatchSize` 個 `RemoveChunksOp` 送一次 `removeChunks`。注意刪除範圍（`:793`）：

```cpp
removeOps.push_back(storageClient_->createRemoveOp(chainId, chunkId, ChunkId(chunkId, 1)));
```

`ChunkId(chunkId, 1)` 是「`chunkId` 加 1」的建構子，所以範圍是 `[chunkId, chunkId+1)`——**精確刪一個 chunk**。程式碼還檢查 `numChunksRemoved != 1` 並在 DBG5 等級記錄（`:809-813`），這是對「範圍刪除語意」的自我驗證。

`truncate()`（`:841-889`）把每個 chunk truncate 到 `chunk_size`。這在 3FS 的 chunk 語意下是「確保 chunk 存在且為完整大小」，可用來在讀測試前把 chunk 補齊而不必真的寫資料。

---

## 9. 監控整合

`main()`（`StorageBench.cc:268-291`）：

```cpp
int main(int argc, char **argv) {
  folly::init(&argc, &argv, true);
  hf3fs::monitor::Monitor::Config monitorConfig;

  if (FLAGS_printMetrics || FLAGS_reportMetrics) {
    if (FLAGS_printMetrics) {
      monitorConfig.reporters(0).set_type("log");
    } else if (FLAGS_reportMetrics) {
      monitorConfig.reporters(0).set_type("monitor_collector");
      monitorConfig.reporters(0).monitor_collector().set_remote_ip(FLAGS_monitorEndpoint);
      monitorConfig.set_reporters_length(1);
    }
    auto monitorResult = hf3fs::monitor::Monitor::start(monitorConfig);
    XLOGF_IF(FATAL, !monitorResult, "Failed to start monitor: {}", monitorResult.error());
  }

  bool ok = hf3fs::storage::benchmark::runBenchmarks();

  hf3fs::monitor::Monitor::stop();
  hf3fs::memory::shutdown();
  return ok ? EXIT_SUCCESS : EXIT_FAILURE;
}
```

**監控預設是關的**，兩個旗標二選一開啟：

- `--printMetrics`：所有 metric 印到日誌
- `--reportMetrics --monitorEndpoint <ip>`：推到 `monitor_collector`（見 [monitor_collector 報告](monitor_collector_main-監控收集器深度剖析.md)）

開啟後，storage client 與（自封閉模式下的）storage server 內部所有 `monitor::Recorder` 都會產出資料。這是這個工具**最強大但最少人用**的能力：`StorageOperator.cc` 裡佈滿了細粒度的 recorder（`storageAioEnqueueRecorder`、`storageWaitAioRecorder`、`storageWaitBatchRecorder`、`storageWriteWaitSemRecorder`、`storageWriteWaitPostRecorder`……）。搭配 `--reportMetrics`，可以得到比 §6 的四角落減法**細得多**的延遲分解——直接看到「等 AIO」「等 RDMA semaphore」「等 post」各花多久。

`hf3fs::memory::shutdown()`（`:288`）配合 `StorageBench.cc:6` 的 `#include "memory/common/OverrideCppNewDelete.h"`，讓 benchmark 也套用自訂記憶體配置器並在結束時輸出配置器統計。見 [記憶體配置器包裝報告](memory_allocator_wrappers-記憶體配置器包裝深度剖析.md)。

**一個未使用的依賴**：CMake 連了 `follybenchmark`（`benchmarks/storage_bench/CMakeLists.txt:1`），但 `StorageBench.cc` / `.h` 完全沒有 `BENCHMARK` 巨集、沒有 `folly::runBenchmarks()`。作者選擇了手寫 TDigest 而非 folly 的 benchmark 框架——這是對的選擇（folly benchmark 面向的是微秒級的函式層 microbenchmark，不適合多秒、多執行緒、跨網路的場景），但 CMake 沒清乾淨。

---

## 10. 對照：`fio_usrbio`

`benchmarks/fio_usrbio/` 是同目錄下的另一個 benchmark，測的是完全不同的層次。

```cpp
// benchmarks/fio_usrbio/hf3fs_usrbio.cpp:264-284
void get_ioengine(struct ioengine_ops **ioengine_ptr) {
    ioengine.name = "hf3fs_usrbio",
    ioengine.init        = hf3fs_usrbio_init;
    ioengine.queue       = hf3fs_usrbio_queue;
    ioengine.commit      = hf3fs_usrbio_commit;
    ioengine.getevents   = hf3fs_usrbio_getevents;
    ...
    ioengine.iomem_alloc = hf3fs_usrbio_alloc;   // ← 關鍵
    ioengine.iomem_free  = hf3fs_usrbio_free;
}
```

它是一個 **fio 外部 ioengine 外掛**。`iomem_alloc` 這個 hook 是整份程式碼的精髓（`:239-250`）：

```cpp
static int hf3fs_usrbio_alloc(struct thread_data *td, size_t total_mem) {
    auto &iov = static_cast<hf3fs_usrbio_data *>(td->io_ops_data)->iov;
    auto res = hf3fs_iovcreate(&iov, options->mountpoint, total_mem, 0, -1);
    if (res < 0) return res;
    td->orig_buffer = iov.base;
    return 0;
}
```

它把 fio 的整塊 IO 緩衝區**替換成 USRBIO 的共享記憶體 iov**。USRBIO 的零複製要求資料落在預先註冊的共享區裡，這個 hook 讓 fio 從一開始就在正確的記憶體上工作，不需要任何額外複製。

其餘結構是把 fio 的 queue/commit/getevents 模型映射到 USRBIO 的 ior：

- `queue`（`:122-141`）只把 io_u 收進 vector；讀寫方向改變時強制 flush（`if (io_u->ddir != sd->last_ddir)`），因為讀寫用不同的 ior（`ior_r` / `ior_w`，`:79-89`）。
- `commit`（`:143-190`）才真正 `hf3fs_prep_io` × N → `hf3fs_submit_ios` → `hf3fs_wait_for_ios`。**它是同步等待的**（`:173` 一次要求 `sd->queued` 個完成），所以 `getevents`（`:192-201`）只是把 commit 階段已經拿到的數字回報給 fio。這讓外掛實作簡單，代價是無法測「提交後不等待、繼續做別的事」的非同步模式。
- `open_file`（`:212-230`）在 `open()` 後呼叫 `hf3fs_reg_fd(f->fd, 0)`——USRBIO 要求 fd 先註冊。

`README.md:30-36` 給的用法很值得記：

```
iodepth=1024
iodepth_batch_submit=1024
iodepth_batch_complete_min=1024
iodepth_batch_complete_max=1024
```

四個參數設成同一個值，是為了讓 fio 一次攢滿 1024 個 IO 才提交、且要等全部完成——**這正好對應 `storage_bench` 的 `batchSize` 概念**。README 明說這是「to benchmark batched small I/Os」。3FS 的效能故事建立在批次聚合上，兩個 benchmark 用不同方式測同一件事。

### 兩者對照

| | `storage_bench` | `fio_usrbio` |
|---|---|---|
| 建置 | CMake（`benchmarks/CMakeLists.txt:1`） | 獨立 Makefile，需要 fio 原始碼 |
| 進入層次 | storage client（chain + chunk id） | USRBIO（檔案 + offset） |
| 經過 meta | 否 | 是 |
| 經過 FUSE 掛載 | 否 | 是（需要掛載點） |
| 需要 mgmtd | 只有 `--clusterMode` 需要 | 是（FUSE 進程需要） |
| 能自帶 server | 是（`UnitTestFabric`） | 否 |
| 延遲分解能力 | 強（bypass 旗標 + monitor） | 無 |
| 負載模型彈性 | 中（寫死的兩階段寫入 / 均勻隨機讀） | 高（fio 全套：zipf、pareto、混合讀寫、多檔案……） |
| 統計品質 | 自製 TDigest（有 §7.2 的精度 bug） | fio 的完整統計（clat/slat/lat 分離、完整分位表） |
| 適用問題 | 「storage 引擎的極限在哪」「延遲花在哪一段」 | 「應用程式實際能拿到多少」 |

**兩者是刻意分工**：`storage_bench` 回答「系統內部」的問題，`fio_usrbio` 回答「使用者能得到什麼」的問題。而 `fio_usrbio` 借用 fio 而不是自己寫，是正確的取捨——負載模型與統計是 fio 二十年累積的長處，沒必要重造。

---

## 11. 從參數面板反推作者的關心點

60 個 gflags（`StorageBench.cc:8-71`）本身就是一份「作者在乎什麼」的清單。按主題歸類：

### 高度在乎的（有多個獨立旗標）

**(1) 延遲的組成** — `--benchmarkNetwork` / `--benchmarkStorage` / `--printMetrics` / `--reportMetrics`。四個旗標全指向同一件事：把端到端延遲拆開。

**(2) 批次聚合** — `--batchSize` / `--readBatchSize` / `--writeBatchSize` / `--removeBatchSize` / `--numCoroutines`。五個旗標。批次大小甚至可以讀寫刪各自獨立設定，說明作者觀察到三者的最佳批次大小不同。

**(3) IB 網路細節** — `--ibvDevices` / `--ibnetZones` / `--defaultPKeyIndex` / `--serviceLevel`。這四個是 InfiniBand 的專業旋鈕：pkey（分割區）、SL（service level，對應 QoS 與虛擬通道）、網路分區。一般 benchmark 不會暴露這些。它們的存在說明作者實際遇過「換一個 SL 效能就不同」這類問題。

**(4) 對齊敏感度** — `--memoryAlignment` / `--readOffAlignment`。記憶體端與檔案 offset 端分開控制。

**(5) chunk id 分佈** — `--sparseChunkIds` / `--randomShuffleChunkIds` / `--chunkIdPrefix` / `--randSeed`。分離「id 空間局部性」與「存取順序」兩個變因。

**(6) 正確性驗證的分級** — `--verifyWriteChecksum`（預設開）/ `--verifyReadChecksum`（預設關）/ `--verifyReadData`（最強，逐 byte）/ `--ignoreIOError`。四個層級可選，因為驗證是有成本的，測頻寬時要能關掉。

### 明顯不在乎的（沒有對應旗標）

| 缺席的能力 | 意涵 |
|---|---|
| 讀寫混合比例 | 讀與寫是**兩個分開的階段**（`runWriteBench` 然後 `runReadBench`），不能同時跑。所以測不出「讀寫互相干擾」——而這在 CRAQ 鏈式複製下是個真問題（寫入要走完整條 chain，讀取可以從任一副本讀） |
| 存取熱點分佈 | 讀取一律均勻隨機（`folly::Random::rand64(0, chunkInfos.size())`）。沒有 zipf、沒有熱點集中。測不出快取效果 |
| 讀寫大小的分佈 | `readSize` / `writeSize` 各是單一固定值，沒有大小混合 |
| 拓撲變化 / 故障 | 繼承的 `UnitTestFabric` 提供了 `setTargetOffline()` 與 `stopAndRemoveStorageServer()`，但 bench **一次都沒用**。測不出「一個副本掛掉時的效能」 |
| 長時間穩定性 | 沒有 warmup 期、沒有分時段報告。整段跑完才出一個匯總數字，看不到效能隨時間的衰減 |
| 多檔案 / 多租戶 | `chunkIdPrefix` 的租戶欄位被拿來當隔離前綴用了 |

**這份缺席清單指向一個結論**：`storage_bench` 是**元件層的效能量測工具**，不是**系統層的工作負載模擬器**。工作負載模擬那部分外包給了 `fio_usrbio`。

---

## 12. 設計取捨總結

| 決策 | 得到什麼 | 付出什麼 |
|---|---|---|
| 繞過 meta，直接用 (chain, chunk) 定址 | 測到的是純粹的 storage 層數字，不被 meta 的延遲汙染 | 測不出真實應用路徑；chunk id 要自己造 |
| 繼承 `test::UnitTestFabric` | 同一支執行檔既能自封閉測試又能連線上叢集；免費得到 chain 拓撲生成 | 執行檔連了 gmock 與整個 server 端；`SystemSetupConfig` 的欄位在 cluster 模式下語意漂移，CSV 因此不可靠（§7.3） |
| `chunkIdPrefix=0xFFFF` 佔用 tenant/reserved byte | 與真實資料的 key space 完全隔離，可安全在生產叢集上跑 | 佔用了多租戶預留的編碼空間 |
| `bypass_disk_io` / `bypass_rdma_xmit` 作為 RPC feature flag | 四角落減法即可分解延遲；不需要改設定或重啟 server | server 端要為 debug 而在熱路徑上加分支判斷 |
| 每 coroutine 一個 TDigest，最後合併 | 熱路徑無鎖；folly TDigest 的合併在數學上正確 | 延遲樣本先進 vector 再合併，記憶體與樣本數成正比 |
| 計時單位 = 整批 RPC | 直接反映使用者感受到的批次完成延遲 | 尾延遲被批次大小放大；看不到單一 IO 的延遲 |
| IOPS/QPS 由頻寬反推 | 不需要額外計數器 | 寫入的部分批次截斷會造成低估；`readIO` 參數傳錯時全盤皆錯（§7.5） |
| 寫入先順序填滿再隨機覆寫 | 造資料與壓測共用同一段程式碼 | 兩階段測的東西不同，且無法單獨測覆寫（§8） |
| `run()` 把 write bench 排在 generate 之前 | 寫入 benchmark 測「首次寫入」 | 不符直覺，且沒有註解說明；容易被誤讀 |
| 讀寫分兩階段而非混合 | 各自的數字乾淨 | 測不出讀寫互相干擾——CRAQ 下這是真問題 |
| 讀取均勻隨機 | 排除快取，測「最壞情況」 | 測不出真實負載的快取命中率 |
| 手寫統計而非用 folly benchmark | 適合多秒、多執行緒、跨網路的場景 | 少了 folly benchmark 的成熟度；`follybenchmark` 依賴白連 |
| 監控整合預設關閉 | 不影響基準數字 | 最強的延遲分解能力（server 內部 recorder）預設用不到 |

---

## 13. 已知缺陷清單

| 位置 | 問題 | 嚴重度 |
|---|---|---|
| `StorageBench.h:777` | `dumpPerfStats("batch read", ..., false /*readIO*/)` 應為 `true`。CSV 的讀取列的 `io size` / `batch size` / `IOPS` / `QPS` 四欄錯用寫入參數；預設值下 IOPS 低估 32 倍。日誌輸出不受影響 | 高（CSV 誤導） |
| `StorageBench.h:556, 668` | `folly::TDigest digest;` 用預設 maxSize=100，覆蓋掉建構子設的 `kTDigestMaxSize=1000`（`:94-95`）。該常數從未生效，P99 精度低於設計意圖。修法：`folly::TDigest digest(kTDigestMaxSize);` | 中（尾延遲精度） |
| `StorageBench.h:407-408` vs `:436` | CSV 第 8 欄標籤寫 `batch size / #replicas`，實際除的是 `num_storage_nodes()` | 中（標籤誤導） |
| `StorageBench.h:430-432` | cluster 模式下 CSV 的 `#storages`/`#chains`/`#replicas` 是命令列旗標的回音，不是真實叢集拓撲。真實 chain 數只出現在 `:283` 的日誌裡 | 中 |
| `StorageBench.h:381-388` vs `:446-447` | 日誌與 CSV 的分位數集合不同（日誌有 P10/P20 無 P75；CSV 有 P75 無低分位） | 低 |
| `benchmarks/storage_bench/CMakeLists.txt:1` | 連了 `follybenchmark` 但完全未使用 | 低 |
| `StorageBench.h:111` | `static thread_local std::mt19937 generator;` 未 seed，所以 `--randSeed` 只控制 chunk id 生成，**不控制 `randomShuffleChunkIds` 的洗牌順序**。兩次執行的 chunk id 相同但存取順序不同 | 低（但影響 `--serverMode`/`--clientMode` 分離部署的可重現性） |
| `StorageBench.h:402-405, 412-415` | `dumpPerfStats` 在檔案開啟失敗時 `throw std::runtime_error`，但整份程式碼其他地方一律回傳錯誤碼。異常不會被接住，會直接 terminate | 低 |

---

## 14. 檔案索引

| 檔案 | 行數 | 職責 |
|---|---|---|
| `benchmarks/CMakeLists.txt` | 1 | 只 `add_subdirectory(storage_bench)`；`fio_usrbio` 不在 CMake 建置樹裡 |
| `benchmarks/storage_bench/CMakeLists.txt` | 1 | `target_add_bin(storage_bench "StorageBench.cc" test-fabric-lib storage-client storage memory-common follybenchmark gmock fdb mgmtd)`——連了測試骨架、gmock、以及未使用的 `follybenchmark` |
| `benchmarks/storage_bench/StorageBench.h` | 907 | 全部實作。`:28-67` `Options`（40 個欄位）；`:72-88` 成員（TDigest 陣列、`ChunkInfo`、原子計數器）；`:107-140` `generateChunkIds` 兩種 id 策略；`:142-308` `connect` 叢集模式（IB → mgmtd → routing info → 三種選 chain 方式 → storage client）；`:310-352` `setupIBSock`；`:354-368` `setup` 自封閉模式；`:375-389` 日誌輸出；`:391-452` `dumpPerfStats` CSV；`:454-561` `batchWrite`（兩階段寫入模型）；`:563-673` `batchRead`（均勻隨機 + 對齊控制 + 資料驗證）；`:675-709` `generateChunks`；`:711-748` `runWriteBench`；`:750-780` `runReadBench`；`:782-839` `cleanup`；`:841-889` `truncate`；`:891-899` `run` 三階段編排 |
| `benchmarks/storage_bench/StorageBench.cc` | 292 | `:8-71` 60 個 gflags；`:77-91` `stringToIntVec`；`:93-264` `runBenchmarks`（解析參數 → 組 `SystemSetupConfig` → 三種模式分派 → 清理 → teardown）；`:268-291` `main`（folly::init → 可選 monitor → 執行 → `memory::shutdown`） |
| `benchmarks/fio_usrbio/CMakeLists.txt` | — | **不存在**；用獨立 Makefile |
| `benchmarks/fio_usrbio/Makefile` | 23 | 需 `HF3FS_LIB_DIR` / `HF3FS_INCLUDE_DIR` / `FIO_SRC_DIR` 三個外部路徑；`-include config-host.h` 引入 fio 的建置設定；連 `libhf3fs_api_shared` |
| `benchmarks/fio_usrbio/README.md` | 40 | 建置與使用說明；`:30-36` 建議把 `iodepth` / `iodepth_batch_*` 四個參數設成同值以測批次小 IO |
| `benchmarks/fio_usrbio/hf3fs_usrbio.cpp` | 286 | fio 外部 ioengine。`:20-61` `mountpoint`/`ior_depth`/`ior_timeout` 三個引擎選項；`:75-97` `init` 建立讀寫兩個 ior；`:122-141` `queue` 攢批（讀寫方向切換時強制 flush）；`:143-190` `commit` prep→submit→wait（同步）；`:212-237` `open`/`close` 搭配 `hf3fs_reg_fd`；`:239-255` `iomem_alloc` **把 fio 緩衝區換成 USRBIO iov**（零複製的關鍵）；`:264-284` 引擎表註冊 |

### 相關但不屬於本組件的檔案

| 檔案 | 關係 |
|---|---|
| `tests/lib/UnitTestFabric.h` | `StorageBench` 的基底類別。`:86-101` `SystemSetupConfig` 欄位；`:174-190` chain 分配演算法註解圖；`:205` `setUpStorageSystem()` |
| `src/client/storage/StorageClient.h` | `:162-165` 四個 debug 選項定義；`:203, 221` bypass 時 checksum 自動停用 |
| `src/client/storage/StorageClientImpl.cc` | `:651-652` debug 選項 → RPC feature flag |
| `src/storage/service/StorageOperator.cc` | server 端的 bypass 實作：讀取 `:157-179`，寫入 `:565, 599-604`，commit `:624-628` |
| `src/fbs/storage/Common.h` | `:74-75` `BYPASS_DISKIO = 1` / `BYPASS_RDMAXMIT = 2` |
| `third_party/folly/folly/stats/TDigest.h` | `:78-79` 預設 `maxSize = 100`——§7.2 那個 bug 的根源 |
