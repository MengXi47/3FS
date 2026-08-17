# 3FS `storage_main`（存儲服務）深度剖析

> 對應原始碼：`src/storage/`（進入點 `src/storage/storage.cpp`）
> RPC 契約：`src/fbs/storage/Service.h`、`src/fbs/storage/Common.h`
> 對側：`src/client/storage/`（客戶端）、`src/common/net/ib/`（RDMA）
> 設定：`configs/storage_main.toml`

---

## 0. 一句話總結

`storage_main` 是 3FS 裡唯一真正碰硬碟的行程。它把「一台機器上的 N 顆 NVMe」抽象成 N×M 個 **storage target**，每個 target 是某條 CRAQ 複製鏈上的一個副本；它自己**不做任何全域決策**——鏈的成員、順序、誰是 HEAD/TAIL 全部由 mgmtd 下發的 routing info 決定，storage 只負責在收到的那一版 routing info 下把三個版本號（`updateVer` / `commitVer` / `chainVer`）維持在不變式內。整條寫路徑上**沒有任何一跳自己搬過資料**：每一跳都用單邊 RDMA Read 從前驅的已註冊記憶體把資料拉過來，commit 則沿著 RPC 的回傳路徑從 TAIL 一路倒著蓋回 HEAD。讀路徑同樣是零拷貝：io_uring fixed buffer 把資料讀進註冊好的 RDMA buffer，再用單邊 RDMA Write 直接寫進客戶端的記憶體。

---

## 1. 這個 binary 是什麼 / 啟動流程

### 1.1 從 `main` 到 serve

`src/storage/storage.cpp` 只有 8 行：

```cpp
int main(int argc, char *argv[]) {
  using namespace hf3fs;
  return TwoPhaseApplication<storage::StorageServer>().run(argc, argv);
}
```

`TwoPhaseApplication`（`src/common/app/TwoPhaseApplication.h`）的「兩階段」指的是**設定的兩階段**，不是啟動的兩階段：

```
階段一（Launcher）                          階段二（Server）
────────────────────                        ────────────────
parseFlags(--app_cfg/--launcher_cfg)
     │
launcher_->init()
  ├─ appConfig_.init()      節點 ID
  ├─ launcherConfig_.init() cluster_id、mgmtd 位址、ib_devices
  └─ net::IBManager::start(ib_devices)   ← RDMA 裝置在這裡就緒
     │
loadAppInfo()  ─── 連 mgmtd 補齊 tags ──→ flat::AppInfo
loadConfigTemplate() ── 從 mgmtd 拉 STORAGE 的設定模板
     │
     └──────────────→ app_detail::initConfig(config_, ...)
                             │
                      initServer(): new StorageServer(config.server()) → setup()
                             │
                      startServer(): fetcher_->startServer(server, appInfo)
                             │
                      StorageServer::start(appInfo, netClient, mgmtdClient)
                             │
                      net::Server::start() ─ beforeStart() ─ group->start() ─ afterStart()
```

關鍵在 `ServerLauncher::init()`（`src/core/app/ServerLauncher.h:41`）：**IB 裝置在讀設定之前就初始化完成**，因為 `StorageOperator` 的建構子要走訪 `net::IBDevice::all()` 來為每張 HCA 各建一組 semaphore（`src/storage/service/StorageOperator.h:51-54`）。設定模板本身是從 mgmtd 拉的，也就是說 storage 節點的設定是叢集集中管理、可熱推的。

`RemoteConfigFetcher` 被特化為 `core::launcher::ServerMgmtdClientFetcher`（`src/storage/service/StorageServer.h:31`），它的 `startServer()` 會把已建好的 `net::Client` 與 `MgmtdClient` 交棒給 server：

```cpp
hf3fs::Result<Void> StorageServer::start(const flat::AppInfo &info,
                                         std::unique_ptr<::hf3fs::net::Client> client,
                                         std::shared_ptr<::hf3fs::client::MgmtdClient> mgmtdClient) {
  components_.netClient = std::move(client);
  components_.mgmtdClient = std::make_unique<hf3fs::client::MgmtdClientForServer>(std::move(mgmtdClient));
  return net::Server::start(info);
}
```
（`src/storage/service/StorageServer.cc:52-58`）

這個交棒很重要：**啟動期就已經與 mgmtd 通過話**（為了拿設定模板），所以不需要再建第二條連線；`Components::waitRoutingInfo()` 裡的 `if (!netClient)` / `if (mgmtdClient.load() == nullptr)` 兩個分支（`src/storage/service/Components.cc:73-83`）只有在單元測試直接構造 `Components` 時才會走到。

### 1.2 `beforeStart()`：真正的組裝點

```cpp
Result<Void> StorageServer::beforeStart() {
  RETURN_AND_LOG_ON_ERROR(addSerdeService(std::make_unique<StorageService>(components_.storageOperator), true));
  RETURN_AND_LOG_ON_ERROR(addSerdeService(std::make_unique<core::CoreService>()));
  groups().front()->setCoroutinesPoolGetter([this](const serde::MessagePacket<> &packet) -> DynamicCoroutinesPool & {
    switch (packet.serviceId) {
      case StorageSerde<>::kServiceID:
        return components_.getCoroutinesPool(packet.methodId);
      default:
        return components_.defaultPool;
    }
  });
  RETURN_AND_LOG_ON_ERROR(components_.start(appInfo(), tpg()));
  return Void{};
}
```
（`src/storage/service/StorageServer.cc:26-39`）

`addSerdeService(..., true)` 的第二個參數把 `StorageSerde` 綁到 group 0（RDMA），`CoreService` 走 group 1（TCP）。`setCoroutinesPoolGetter` 是 3FS 在網路層留的一個鉤子：**按 RPC method id 把請求分派到不同的協程池**，避免讀請求被寫請求餓死（見 §4.2）。

### 1.3 `Components::start()`：15 個步驟的順序有講究

`src/storage/service/Components.cc:39-69`：

| # | 步驟 | 為什麼是這個位置 |
|---|---|---|
| 1 | `rdmabufPool.init(procThreadPool)` | 要在 aioReadWorker 之前——後者需要 `iovecs()` 去註冊 io_uring fixed buffer |
| 2-5 | `readPool/updatePool/syncPool/defaultPool.start()` | 協程池 |
| 6 | `messenger.start()` | 轉發用的 client（`forward_client` 設定段） |
| 7 | `reliableForwarding.init()` | 目前是空實作 |
| 8 | `storageTargets.load(procThreadPool)` | 掛載所有磁碟、建 chunk engine、載入每個 target |
| 9 | `aioReadWorker.start(storageTargets.fds(), rdmabufPool.iovecs())` | **必須在 8 與 1 之後**：fd 表與 buffer 表都要先齊備才能註冊 |
| 10-13 | `dumpWorker` / `allocateWorker` / `punchHoleWorker` / `syncMetaKvWorker` | 背景維護 |
| 14 | `waitRoutingInfo(...)` | **啟動屏障**，見 §1.4 |
| 15 | `resyncWorker.start()`、`checkWorker.start()` | 必須在拿到 routing info 之後 |
| 16 | `storageOperator.init(targetPaths().size())` | `UpdateWorker` 每顆盤一條佇列，所以要知道盤數 |

第 9 步的耦合是整份程式碼裡最緊的一處：`AioReadWorker::start(fds, iovecs)` 把 fd 陣列與 iovec 陣列一次性註冊進每個工作執行緒各自的 io_uring（`src/storage/aio/AioStatus.cc:194-209`）。註冊之後 fd 表與 buffer 表就**不能再變**——這也解釋了為什麼 `GlobalFileStore::collect()` 是一次性把所有 fd 掃出來編號（`src/storage/store/GlobalFileStore.cc:51-60`），以及為什麼盤故障時只是把 target 標記 offline 而不關 fd。

### 1.4 「等待舊實例的 target 全部下線」啟動屏障

`Components::waitRoutingInfo()`（`src/storage/service/Components.cc:71-137`）在正式服務前有一段死迴圈：

```cpp
// 2. wait target offline.
auto currentMap = targetMap.snapshot();
updateHeartbeatPayload(*currentMap, true);          // 先宣告「我全部 OFFLINE」
folly::coro::blockingWait(mgmtdClient.load()->start(&executor));
for (auto sleep = 0;; ++sleep) {
  if (sleep) { XLOGF(WARNING, "Waiting for target offline in routing info..."); std::this_thread::sleep_for(1000_ms); }
  folly::coro::blockingWait(mgmtdClient.load()->heartbeat());
  auto copy = currentMap->clone();
  auto refreshResult = folly::coro::blockingWait(mgmtdClient.load()->refreshRoutingInfo(false));
  ...
  for (auto &[targetId, target] : copy->getTargets()) {
    if (target.publicState == flat::PublicTargetState::SERVING ||
        target.publicState == flat::PublicTargetState::SYNCING ||
        target.publicState == flat::PublicTargetState::WAITING) { needWaiting = true; break; }
  }
  if (!needWaiting) break;
}
```

語意是：**新起的行程必須先把自己所有 target 報成 OFFLINE，並且等到 mgmtd 的 routing info 也認同這件事，才敢開始服務。** 這是防「程序重啟後舊 routing info 還把我當 SERVING，於是客戶端把寫送過來，但我本地資料可能落後」的競態。代價是重啟會多等一到數個心跳週期（`auto_heartbeat_interval = '10s'`）。

注意 `updateHeartbeatPayload(map, offline=true)` 只是把**上報內容**改成 OFFLINE，本地 `targetMap` 裡 target 的 `localState` 仍是 `ONLINE`（`addStorageTarget` 設的，`src/storage/service/TargetMap.cc:86`）。真正的狀態遷移由 mgmtd 回饋的 `publicState` 驅動（§7）。

### 1.5 兩個監聽埠

`src/storage/service/Components.h:30-43`：

```cpp
CONFIG_OBJ(base, net::Server::Config, [](net::Server::Config &c) {
  c.set_groups_length(2);
  c.groups(0).listener().set_listen_port(8000);
  c.groups(0).set_network_type(net::Address::RDMA);
  c.groups(0).set_services({"StorageSerde"});

  c.groups(1).set_network_type(net::Address::TCP);
  c.groups(1).listener().set_listen_port(9000);
  c.groups(1).set_use_independent_thread_pool(true);
  c.groups(1).set_services({"Core"});

  c.thread_pool().set_num_io_threads(32);
  c.thread_pool().set_num_proc_threads(32);
});
```

| 埠 | 傳輸 | 服務 | 執行緒池 |
|---|---|---|---|
| 8000 | RDMA | `StorageSerde`（serviceId = 3） | 共用 `thread_pool`（32 io + 32 proc） |
| 9000 | TCP | `Core`（設定推送、狀態查詢） | `independent_thread_pool`（2+2） |

Core 服務走**獨立執行緒池**是刻意的：資料面被打爆時，管理面（推設定、查狀態、下線 target）還要能進得來。這也是 admin_cli 能在存儲節點忙到冒煙時仍然 `list-targets` 的原因。

### 1.6 關機路徑

`Components::stopAndJoin()`（`src/storage/service/Components.cc:141-206`）的順序是啟動的逆序，但多了三件事：

1. **`beforeStop()` 先豎旗**：`reliableUpdate.beforeStop()` 與 `reliableForwarding.beforeStop()` 只是把 `stopped_ = true`，之後所有新進來的 update 直接回 `RPCCode::kRequestRefused`（`src/storage/service/ReliableUpdate.cc:22-26`）。這一步在 `net::Server::stopAndJoin()` 停監聽之前，所以是「先拒絕、再關門」。
2. **主動上報 OFFLINE**：`updateHeartbeatPayload(*targetMap.snapshot(), true)` 後同步發一次心跳，讓 mgmtd 立刻知道，不用等心跳逾時。
3. **平行釋放 target**：把所有 `StorageTarget` 丟到 executor 上平行 `release()`（= 設 `released_ = true` 然後 `sync()`），主執行緒每 100ms 輪詢一次計數。`release()` 失敗會打 `CRITICAL`——因為那代表 meta KV 沒落盤，下次啟動要靠 WAL 恢復。

最後 `config.speed_up_quit()`（預設 true）會對每個 chunk engine 呼叫 `speed_up_quit()`（`src/storage/service/Components.cc:200-204`）——Rust 側的「別做乾淨關閉，直接放棄」開關，用來避免 RocksDB 關閉時的長尾。

---

## 2. 整體分層架構

```
                          ┌──────────────────────────────────────────────┐
  client / 前驅 storage ──▶│ StorageService  (serde 薄殼，只記錄佇列延遲)  │  fbs/storage/Service.h
                          └───────────────────┬──────────────────────────┘
                                              ▼
                          ┌──────────────────────────────────────────────┐
                          │ StorageOperator   14 個 RPC 的業務主體         │
                          │  batchRead / write / update / truncate / ...  │
                          └───┬──────────────┬──────────────┬────────────┘
              讀              │              │ 寫            │ 查詢/管理
        ┌─────────────────────▼──┐   ┌───────▼────────────┐ │
        │ BatchReadJob           │   │ ReliableUpdate     │ │  ← per-(client,chain,channel) 去重快取
        │ AioReadJobIterator     │   └───────┬────────────┘ │
        └─────────┬──────────────┘           ▼              │
                  ▼                  ┌────────────────────┐ │
        ┌────────────────────┐       │ handleUpdate       │ │  ← chunk 鎖 / 本地寫 / 轉發 / commit
        │ AioReadWorker ×32  │       └───┬────────────┬───┘ │
        │  AioStatus(libaio) │           │            ▼     │
        │  IoUringStatus     │           │   ┌──────────────────────┐
        └─────────┬──────────┘           │   │ ReliableForwarding   │ ──RPC──▶ 後繼 storage
                  │                      ▼   └──────────────────────┘
                  │            ┌────────────────────┐
                  │            │ UpdateWorker ×32   │  每顆盤一條 BoundedQueue
                  │            └─────────┬──────────┘
                  ▼                      ▼
        ┌──────────────────────────────────────────────────────────────┐
        │ StorageTarget   （一個 target = 一顆盤上的一個目錄）            │
        │   useChunkEngine() ? ─── 是 ──▶ ChunkEngine  ──cxx橋接──▶ Rust │
        │                    └── 否 ──▶ ChunkReplica ─▶ ChunkStore      │
        └──────────────────────────────────────────────────────────────┘
                                              │
              ┌───────────────────────────────┼──────────────────────────┐
              ▼                               ▼                          ▼
      ┌──────────────┐              ┌──────────────────┐        ┌────────────────┐
      │ ChunkMetaStore│             │ ChunkFileStore   │        │ GlobalFileStore│
      │ LevelDB/RocksDB│            │ 每 chunkSize 256 │        │ path → (normal │
      │ 分配/回收/打洞  │             │ 個物理檔          │        │  fd, direct fd)│
      └──────────────┘              └──────────────────┘        └────────────────┘
```

一條重要的分界線：**`StorageTarget` 以上完全不知道底層是舊的 `ChunkStore` 還是新的 Rust chunk engine**。所有分支都收斂在 `StorageTarget` 的十來個 `if (useChunkEngine())`（`src/storage/store/StorageTarget.h:84-162`、`src/storage/store/StorageTarget.cc:289-439`）。判定依據是 target 的 `target.toml` 裡的 `only_chunk_engine` 欄位（`src/storage/store/PhysicalConfig.h:27`），一旦建立就不可變。

---

## 3. 核心資料模型

### 3.1 `ChunkId`：不透明位元組串，但排序有語意

`src/fbs/storage/Common.h:82-110`。`ChunkId` 內部就是 `std::string`，但有兩個特殊建構子：

```cpp
ChunkId(uint64_t high, uint64_t low);              // 128-bit，big-endian 打包
ChunkId(const ChunkId &baseChunkId, uint64_t chunkIndex);  // 在最後 8 bytes 上加 chunkIndex
```

`src/fbs/storage/Common.cc:10-34` 顯示兩者都做 `__builtin_bswap64`，也就是 **chunk id 用 big-endian 儲存**。理由在 `GlobalKey::fromFileOffset()`（`src/fbs/storage/Common.cc:98-107`）：

```cpp
size_t chunkIndex = fileOffset / chunkSize;
ChunkId chunkId(baseChunkId, chunkIndex);
return {chainIds[chunkIndex], chunkId};
```

同一個檔案的連號 chunk 在位元組序上必須相鄰，`queryLastChunk`（找檔案最後一個 chunk）與 `removeChunks`（刪一整段）才能用 range scan 完成。這與元資料層 inode 刻意用 little-endian 打散熱點是**相反的取捨**——storage 這邊要的是局部性，因為 chunk 早已被 chain 分散到不同機器上了。

兩個 range 輔助函式對應 §6 的「點查優化」：

```cpp
ChunkId ChunkId::nextChunkId() const;            // 位元組串 +1（進位）
ChunkId ChunkId::rangeEndForCurrentChunk() const;// 尾巴補一個 '\0'
```

### 3.2 三個版本號與 `ChunkState` 的不變式

這是整個 CRAQ 實作的核心。`src/fbs/storage/Common.h:60-64`、`src/fbs/storage/Common.h:652-677`：

| 欄位 | 型別 | 語意 |
|---|---|---|
| `updateVer` | `ChunkVer`(u32) | 這個副本**已經寫下**的最新版本 |
| `commitVer` | `ChunkVer`(u32) | 這個副本**已經確認全鏈都寫下**的版本 |
| `chainVer` | `ChainVer`(u32) | 最後一次修改時所在的鏈版本（來自 mgmtd） |
| `chunkState` | enum | `COMMIT`(0) / `DIRTY`(1) / `CLEAN`(2) |

不變式（由 `ChunkReplica::update` / `ChunkReplica::commit` 共同維持）：

```
  commitVer ≤ updateVer ≤ commitVer + 1

  chunkState == COMMIT  ⟺  commitVer == updateVer 且已落盤
  chunkState == DIRTY   ⟹  正在寫，資料頁可能只寫了一半（不可讀）
  chunkState == CLEAN   ⟹  資料已寫完，但尚未收到 commit（updateVer = commitVer + 1）

  讀取條件：commitVer == updateVer，否則回 kChunkNotCommit
            （除非 featureFlags 帶 ALLOW_READ_UNCOMMITTED）
```

狀態機（單一 chunk，非 syncing 路徑）：

```
                writeIO 到達，版本檢查通過
   ┌────────┐  meta.updateVer += 1        ┌───────┐
   │ COMMIT │ ─────────────────────────▶  │ DIRTY │   ← 先落 meta 再寫資料
   └────────┘  chunkState = DIRTY         └───┬───┘      (ChunkReplica.cc:241-249)
        ▲                                     │ pwrite 成功 + checksum 更新
        │                                     ▼
        │                                 ┌───────┐
        │  commit 到達                     │ CLEAN │   ← 資料完整但未 commit
        │  commitVer = commitIO.commitVer └───┬───┘      (ChunkReplica.cc:304-307)
        │  若 commitVer == updateVer          │
        └─────────────────────────────────────┘
                                          (ChunkReplica.cc:451-454)
```

「**先把 meta 標成 DIRTY 落盤，再寫資料**」這個順序是崩潰安全的關鍵：如果在 pwrite 中途掉電，重啟後看到 `DIRTY` 就知道這個 chunk 的資料頁不可信，必須靠 resync 重新拉。反過來如果先寫資料再改 meta，掉電後 meta 還顯示舊版本但資料已經是新的，就會讀到「舊版本號 + 新資料」的幽靈狀態。

`skipPersist` 是這條路徑上的一個效能例外（`src/storage/store/ChunkReplica.cc:246-249`）：

```cpp
const bool isAppendWrite = writeIO.offset == meta.size;
const bool skipPersist = (writeIO.isWrite() && isAppendWrite) || writeIO.isTruncate() || writeIO.isExtend();
auto setResult = needCreateChunk ? store.createChunk(...) : store.set(chunkId, chunkInfo, !skipPersist);
```

**純追加寫（`offset == meta.size`）不需要把 DIRTY meta 落盤**——因為追加寫只會碰到 `[meta.size, meta.size+length)` 這段從來沒被讀過的空間，即使掉電，舊的 `meta.size` 仍然指向一段完整正確的資料。這省掉了順序寫場景下一半的 KV 寫入。但 `force_persist`（預設 `true`，`configs/storage_main.toml:460`）可以把這個優化關掉。

### 3.3 `ChunkMetadata` 完整欄位

`src/fbs/storage/Common.h:652-677`，註解明說「The order of members has been adjusted for smaller size」：

| 欄位 | 用途 |
|---|---|
| `commitVer` / `updateVer` / `chainVer` | §3.2 |
| `size` | chunk 目前的有效長度（≤ chunkSize） |
| `chunkState` | `COMMIT`/`DIRTY`/`CLEAN` |
| `recycleState` | `NORMAL` / `REMOVAL_IN_PROGRESS` / `REMOVAL_IN_RETRYING` |
| `checksumType` / `checksumValue` | §5.8 |
| `lastNodeId` / `lastRequestId` / `lastClientUuid` | 最後一次修改的來源，純除錯用 |
| `innerOffset` / `innerFileId{chunkSize, chunkIdx}` | 舊引擎的物理位置：哪個檔、檔內偏移 |
| `timestamp` | 最後修改時間 |

`recycleState` 是刪除的兩階段標記：`ChunkReplica::update` 收到 REMOVE 時只把它設成 `REMOVAL_IN_PROGRESS`（`src/storage/store/ChunkReplica.cc:274-279`），真正的 `store.remove()` 要等到 commit 階段（`src/storage/store/ChunkReplica.cc:458`）。中間這段時間 chunk 對查詢是「不存在」的——`processQueryResults` 會跳過所有非 `NORMAL` 的 chunk（`src/storage/service/StorageOperator.cc:681-701`）。這讓刪除也遵守「先鏈上達成共識、再真的動手」的 CRAQ 語意。

### 3.4 `Target`：本地視角的鏈成員

`src/fbs/storage/Common.h:685-711`：

```cpp
struct Target {
  std::shared_ptr<StorageTarget> storageTarget;   // 非 serde 欄位
  std::weak_ptr<bool> weakStorageTarget;          // 非 serde 欄位
  SERDE_STRUCT_FIELD(targetId, TargetId{});
  SERDE_STRUCT_FIELD(path, Path{});
  SERDE_STRUCT_FIELD(diskError, false);
  SERDE_STRUCT_FIELD(lowSpace, false);
  SERDE_STRUCT_FIELD(rejectCreateChunk, false);
  SERDE_STRUCT_FIELD(isHead, false);
  SERDE_STRUCT_FIELD(isTail, false);
  SERDE_STRUCT_FIELD(vChainId, VersionedChainId{});
  SERDE_STRUCT_FIELD(localState, flat::LocalTargetState::INVALID);
  SERDE_STRUCT_FIELD(publicState, flat::PublicTargetState::INVALID);
  SERDE_STRUCT_FIELD(successor, std::optional<Successor>{});
  SERDE_STRUCT_FIELD(diskIndex, uint32_t{});
  SERDE_STRUCT_FIELD(chainId, ChainId{});
  SERDE_STRUCT_FIELD(offlineUponUserRequest, false);
  SERDE_STRUCT_FIELD(useChunkEngine, false);
};
```

`weakStorageTarget` 是個巧妙的設計：當 target 被判定 offline 時，`storageTarget` 這個 `shared_ptr` 被置 nullptr，但 `weakStorageTarget = storageTarget->aliveWeakPtr()` 保留一個對 `StorageTarget::alive_`（一個 `shared_ptr<bool>`）的弱引用（`src/storage/service/TargetMap.cc:214-218`）。之後：

- `CheckWorker` 靠 `weakStorageTarget.expired()` 判斷「還有沒有 in-flight 請求握著這個 target」，只有完全過期才敢重新 `loadTarget()`（`src/storage/worker/CheckWorker.cc:131-143`）。
- `removeTarget` RPC 同樣用它來拒絕「還在用」的刪除（`src/storage/service/StorageOperator.cc:1126-1130`）。

也就是說：**用一個 `weak_ptr<bool>` 當成「引用計數歸零」的探針**，不需要額外的 refcount 欄位。

### 3.5 `MessageTag` / `UpdateChannel`：去重的鑰匙

```cpp
struct UpdateChannel { ChannelId id; ChannelSeqNum seqnum; };          // u16 + u64
struct MessageTag { ClientId clientId; RequestId requestId; UpdateChannel channel; };
```
（`src/fbs/storage/Common.h:270-287`）

客戶端的 `UpdateChannelAllocator`（`src/client/storage/UpdateChannelAllocator.cc`）維護一個 channel id 的堆疊，每次寫請求借一個 id、配一個全域遞增的 seqnum，請求完成後歸還 id。`ChannelId{0}` 是保留的無效值。

storage 端的 `ReliableUpdate` 用 `(clientId, chainId, channelId) → 最後一次結果` 建一張表（`src/storage/service/ReliableUpdate.h:53-58`）：

```cpp
struct ReqResult {
  ChannelSeqNum channelSeqnum;
  RequestId requestId;
  IOResult updateResult;
  ChunkVer succUpdateVer;
  uint32_t generationId;
};
using ClientMap = std::unordered_map<ClientId, std::shared_ptr<ClientStatus>>;
Shards<ClientMap, 1024> shards_;
```

一個 channel 上同時只能有一個 in-flight 請求（客戶端保證），因此**只要記住每個 channel 的最後一個 seqnum 就能做冪等**——不需要為每個請求 id 建表。這把去重表的大小從 O(in-flight 請求數) 壓到 O(客戶端數 × 鏈數 × channel 數)，且天然帶淘汰語意。

---

## 4. RPC 服務面

### 4.1 完整方法表

`src/fbs/storage/Service.h:8-23`，`serviceId = 3`：

| # | 方法 | 請求 / 回應 | 實作 | 誰呼叫 | 協程池 |
|---|---|---|---|---|---|
| 1 | `batchRead` | `BatchReadReq` / `BatchReadRsp` | `StorageOperator.cc:82` | client | `readPool` |
| 2 | `write` | `WriteReq` / `WriteRsp` | `StorageOperator.cc:233` | client → HEAD | `updatePool` |
| 3 | `update` | `UpdateReq` / `UpdateRsp` | `StorageOperator.cc:284` | **前驅 storage** | `updatePool` |
| 5 | `queryLastChunk` | `QueryLastChunkReq` / `Rsp` | `StorageOperator.cc:863` | meta（算檔案長度） | `defaultPool` |
| 6 | `truncateChunks` | `TruncateChunksReq` / `Rsp` | `StorageOperator.cc:923` | meta | `defaultPool` |
| 7 | `removeChunks` | `RemoveChunksReq` / `Rsp` | `StorageOperator.cc:941` | meta（刪檔 GC） | `defaultPool` |
| 8 | `syncStart` | `SyncStartReq` / `TargetSyncInfo` | `StorageOperator.cc:1007` | **前驅 storage** | `syncPool` |
| 9 | `syncDone` | `SyncDoneReq` / `SyncDoneRsp` | `StorageOperator.cc:1052` | **前驅 storage** | `defaultPool` |
| 10 | `spaceInfo` | `SpaceInfoReq` / `Rsp` | `StorageOperator.cc:1065` | admin_cli / mgmtd | `defaultPool` |
| 11 | `createTarget` | `CreateTargetReq` / `Rsp` | `StorageOperator.cc:1075` | admin_cli | `defaultPool` |
| 12 | `queryChunk` | `QueryChunkReq` / `Rsp` | `StorageOperator.cc:1156` | admin_cli（除錯） | `defaultPool` |
| 13 | `getAllChunkMetadata` | `GetAllChunkMetadataReq` / `Rsp` | `StorageOperator.cc:1175` | admin_cli / 校驗工具 | `syncPool` |
| 16 | `offlineTarget` | `OfflineTargetReq` / `Rsp` | `StorageOperator.cc:1084` | admin_cli / **後繼 storage** | `defaultPool` |
| 17 | `removeTarget` | `RemoveTargetReq` / `Rsp` | `StorageOperator.cc:1103` | admin_cli | `defaultPool` |

編號 4、14、15 缺號——這是 serde 協議的向前相容設計：**方法編號一旦分配就永不重用**，刪掉的方法留下空洞，避免舊客戶端的請求被新方法誤接。

幾個值得注意的點：

- **`write` 與 `update` 是兩個方法而不是一個。** `write` 只能打在 HEAD 上（`handleUpdate` 開頭就檢查 `req.options.fromClient && !target->isHead`，`src/storage/service/StorageOperator.cc:338-341`），`update` 則是鏈內轉發用。分成兩個方法讓「誰能寫」這件事在 RPC 層就分得開，也讓 `truncateChunks` / `removeChunks` 這種 server 內部合成的操作可以直接復用 `update` 的骨架。
- **`syncStart`(#8) / `syncDone`(#9) 是純內部方法**，由 resync 的**前驅**呼叫**後繼**。`syncStart` 要求對方的 `publicState == SYNCING` 且 `localState == ONLINE`（`src/storage/service/StorageOperator.cc:1021-1031`），`getAllChunkMetadata`(#13) 則要求 `SERVING` + `UPTODATE`——同一件事（列出所有 chunk meta）在兩個不同的狀態前提下用兩個方法，避免誤用。
- **`offlineTarget` 也會被後繼 storage 呼叫。** `ResyncWorker` 在比對出「不可能發生」的狀態時，會直接對後繼發 `offlineTarget(force=true)` 把它踢下線（`src/storage/sync/ResyncWorker.cc:277-287`）。

### 4.2 三層佇列與協程池分流

`StorageService` 本身極薄，每個方法只做兩件事：記一個佇列延遲、包一個 `ServiceRequestContext` 然後轉呼 `StorageOperator`。真正的分流在 `Components::getCoroutinesPool()`（`src/storage/service/Components.h:80-92`）：

```cpp
inline DynamicCoroutinesPool &getCoroutinesPool(uint16_t methodId) {
  if (LIKELY(config.use_coroutines_pool_read()) && methodId == StorageSerde<>::batchReadMethodId) return readPool;
  if (LIKELY(config.use_coroutines_pool_update()) &&
      (methodId == StorageSerde<>::writeMethodId || methodId == StorageSerde<>::updateMethodId)) return updatePool;
  if (methodId == StorageSerde<>::syncStartMethodId || methodId == StorageSerde<>::getAllChunkMetadataMethodId)
    return syncPool;
  return defaultPool;
}
```

四個池各 64 協程 / 8 執行緒 / 1024 佇列（`configs/storage_main.toml:297-315`）。分流的動機很直接：

- `syncStart` / `getAllChunkMetadata` 會**列舉一個 target 的全部 chunk meta**，是重量級長操作。如果與讀寫共池，一次 resync 就能把讀延遲打爆。
- `batchRead` 與 `write/update` 分池，是因為兩者的阻塞點完全不同（前者卡在 AIO + RDMA Write，後者卡在 RDMA Read + 轉發 RPC），混在一起會互相放大尾延遲。

對應的三個延遲指標分別是 `storage.read.queue_latency` / `storage.update.queue_latency` / `storage.default.queue_latency`（`src/storage/service/StorageService.cc:8-10`），量的是**封包從進網路層到協程開始執行的間隔**，是判斷「是不是被協程池排隊卡住」的第一手證據。

---

## 5. 寫入路徑：CRAQ 的完整實作

### 5.1 全景時序圖

以一條三副本鏈 `HEAD → MID → TAIL` 為例，客戶端寫一個 chunk：

```
client            HEAD                    MID                     TAIL
  │                │                       │                        │
  │─ write(#2) ───▶│                       │                        │
  │  payload.rdmabuf = client 的註冊記憶體    │                        │
  │                │                        │                       │
  │                ├─ targetMap.getByChainId(vChainId) 校驗鏈版本     │
  │                ├─ ReliableUpdate: 查 (client,chain,channel) 去重表 │
  │                ├─ lockChunk(chunkId)  ← 同一 chunk 序列化          │
  │                │                                                 │
  │◀── RDMA READ ──┤  doUpdate: 從 client 的 buffer 拉資料到本地       │
  │                ├─ updateWorker → ChunkReplica::update             │
  │                │    meta.updateVer += 1;  state = DIRTY           │
  │                │    pwrite;  checksum;  state = CLEAN             │
  │                │                                                 │
  │                ├─ forward: update(#3), payload.rdmabuf = HEAD 本地 buffer 的 remote 描述子
  │                │──────────────────────▶│                         │
  │                │                       ├─ lockChunk / doUpdate    │
  │                │                       │◀── RDMA READ ───────────┤（MID 從 HEAD 拉）
  │                │                       │  ※ 圖示方向：MID 主動 read HEAD
  │                │                       ├─ 本地寫入，updateVer 帶著走
  │                │                       │                          │
  │                │                       ├─ forward: update(#3) ───▶│
  │                │                       │                          ├─ lockChunk / doUpdate
  │                │                       │                          │◀ RDMA READ（TAIL 從 MID 拉）
  │                │                       │                          ├─ 本地寫入
  │                │                       │                          │
  │                │                       │                          ├─ successor == nullopt
  │                │                       │                          │  → kNoSuccessorTarget
  │                │                       │                          │  → commitIO.commitChainVer
  │                │                       │                          │     = 本地 chainVer
  │                │                       │                          ├─ doCommit  ← commit 從這裡開始
  │                │                       │◀── UpdateRsp{commitVer} ─┤
  │                │                       ├─ commitIO.commitVer = rsp.commitVer
  │                │                       ├─ doCommit                │
  │                │◀── UpdateRsp{commitVer}┤                         │
  │                ├─ commitIO.commitVer = rsp.commitVer              │
  │                ├─ 校驗 commitVer == 本地 updateVer（不等就 DFATAL） │
  │                ├─ doCommit                                        │
  │◀── WriteRsp ───┤                        │                         │
```

兩個結構性事實：

1. **資料流與控制流方向相反。** 控制流是 HEAD→TAIL 的 RPC 串接；資料流是每一跳**主動去讀前一跳**的 RDMA Read。沒有任何一跳把資料「推」出去，因此發送端不需要為每個下游各準備一份緩衝。
2. **commit 是 RPC 的回傳值，不是另一輪 RPC。** TAIL 因為找不到 successor 而率先 commit，然後每一跳在收到後繼的回應之後才 commit 自己。這就是 CRAQ「TAIL 是權威」的實作方式——**不需要額外的 commit 廣播**。

### 5.2 `handleUpdate()`：一次寫入的七個階段

`src/storage/service/StorageOperator.cc:333-519` 是整份程式碼最核心的函式。

**階段 1：前置校驗**（`:337-356`）

```cpp
if (UNLIKELY(req.options.fromClient && !target->isHead)) { ... kRoutingError }
if (UNLIKELY(req.options.fromClient && config_.read_only())) { ... kReadOnlyMode }
if (UNLIKELY(req.payload.key.chunkId.data().empty())) { ... kInvalidArg }
if (req.options.isSyncing && (req.payload.isTruncate() || req.payload.isExtend())) {
  auto msg = fmt::format("reject truncate/extend request from syncing client: {}", req);
  XLOG(ERR, msg);
  co_return makeError(StatusCode::kInvalidArg, std::move(msg));
}
```

最後一條是近期修正（commit `e2cef82`）補上的。原因見 §8.5：resync 路徑上所有操作都會被前驅改寫成「整塊 WRITE」，所以一個標著 `isSyncing` 的 truncate/extend 請求本身就是 bug 的證據，直接拒絕比默默處理安全。

**階段 2：鎖 chunk，鎖後重取 target**（`:368-387`）

```cpp
folly::coro::Baton baton;
auto lockGuard = target->storageTarget->lockChunk(baton, req.payload.key.chunkId, fmt::to_string(req.tag));
if (!lockGuard.locked()) { co_await lockGuard.lock(); }
...
// re-check chain version after acquiring the lock.
auto targetResult = components_.targetMap.getByChainId(req.payload.key.vChainId);
if (UNLIKELY(!targetResult)) co_return makeError(std::move(targetResult.error()));
target = std::move(*targetResult);
```

`lockChunk` 用的是 `CoLockManager<>`（`src/common/utils/CoLockManager.h`）——一個以 key 分片、每 key 一條 FIFO 佇列的協程鎖。等待者掛在自己的 `folly::coro::Baton` 上，前一個持有者釋放時 post 隊首。`currentTag()` 會回報目前持有者的 tag（也就是 `MessageTag` 的字串形式），所以日誌裡能直接看到「我在等哪個請求」。

**鎖後重取 target 是必要的**：`targetMap` 是無鎖的 `atomic_shared_ptr` 快照，等鎖期間 routing info 完全可能已經翻版。若不重取，就會拿著舊的 `chainVer` 去寫，破壞 §5.7 的鏈版本不變式。

**階段 3：本地寫**（`:389-441`）

```cpp
ChunkEngineUpdateJob chunkEngineJob{};
auto buffer = components_.rdmabufPool.get();
net::RDMARemoteBuf remoteBuf;
auto updateResult = co_await doUpdate(requestCtx, req.payload, req.options, req.featureFlags,
                                      target->storageTarget, ibSocket, buffer, remoteBuf, chunkEngineJob,
                                      !(req.options.fromClient && target->rejectCreateChunk));
```

`buffer` 與 `remoteBuf` 都宣告在 `handleUpdate` 的棧上而非 `doUpdate` 內部——因為**後繼還要來 RDMA Read 這塊記憶體**，它的生命週期必須覆蓋整個轉發過程。`BufferPool::Buffer` 的解構子才把 slot 還給池子（`src/storage/service/BufferPool.cc:96-100`）。

最後一個參數是空間保護：磁碟使用率超過 `disk_reject_create_chunk_threshold`（0.98）時 `target->rejectCreateChunk` 為真，此時**來自客戶端的寫不允許新建 chunk**（回 `kNoSpace`），但覆蓋既有 chunk 仍可進行，且鏈內轉發（`fromClient == false`）與 resync 不受限制。這樣盤快滿時副本仍能追上，不會因為空間不足而讓鏈卡在 SYNCING。

**階段 4：五種寫入結果的分類處置**（`:406-439`）

```cpp
uint32_t code = updateResult.lengthInfo ? 0 : updateResult.lengthInfo.error().code();
if (code == 0) {
  if (req.payload.updateVer == 0) req.payload.updateVer = updateResult.updateVer;  // HEAD 定版
  else if (UNLIKELY(req.payload.updateVer != updateResult.updateVer)) { ... DFATAL, kChunkVersionMismatch }
} else if (code == StorageCode::kChunkMissingUpdate)   { XLOG(DFATAL); co_return updateResult; }
  else if (code == StorageCode::kChunkCommittedUpdate) { /* 當作成功 */ ... co_return updateResult; }
  else if (code == StorageCode::kChunkStaleUpdate)     { /* 當作成功，繼續往下走 */ ... }
  else if (code == StorageCode::kChunkAdvanceUpdate)   { XLOG(DFATAL); co_return updateResult; }
  else { XLOGF(CRITICAL, ...); co_return updateResult; }
```

| 錯誤碼 | 觸發條件（`ChunkReplica.cc:216-238`） | 處置 | 為什麼 |
|---|---|---|---|
| — | 成功 | `updateVer` 定版並沿鏈傳遞 | HEAD 分配版本，下游必須照抄 |
| `kChunkCommittedUpdate` (4008) | `writeIO.updateVer ≤ meta.commitVer` | **視為成功**並直接回 | 這個版本早就 commit 了，是重試的回音 |
| `kChunkStaleUpdate` (4006) | `writeIO.updateVer ≤ meta.updateVer` | **視為成功**，但**繼續往下轉發** | 本地已有，但下游可能還沒有 |
| `kChunkMissingUpdate` (4007) | `writeIO.updateVer > meta.updateVer + 1` | `DFATAL` 並阻塞 | 中間漏了一版，鏈已不一致，寧可停 |
| `kChunkAdvanceUpdate` (4012) | HEAD 自增後 `updateVer > commitVer + 1` | `DFATAL` | 上一次寫還沒 commit 就又寫，違反不變式 |

`kChunkStaleUpdate` 與 `kChunkCommittedUpdate` 的不同處置是這段最精細的地方：**「我已經 commit 了」意味著整條鏈當時都同意過，可以直接回；「我只是寫過了」不代表下游寫過，必須繼續轉發**。這正是重試安全的分界線。

**階段 5：轉發**（`:443-490`），見 §5.5。

**階段 6：checksum 交叉驗證**（`:469-490`）

```cpp
if (forwardResult.lengthInfo) {
  if (commitIO.isRemove && (forwardResult.checksum.type == ChecksumType::NONE ||
                            updateResult.checksum.type == ChecksumType::NONE || commitIO.isSyncing)) {
    XLOGF(INFO, "Remove op local checksum {} not equal ...");   // 已知可接受的情況
  } else if (forwardResult.checksum != updateResult.checksum) {
    XLOG_IF(DFATAL, !requestCtx.debugFlags.faultInjectionEnabled(), msg);
    co_return makeError(StorageClientCode::kChecksumMismatch, std::move(msg));
  }
}
```

**每一跳都會把自己算出的 chunk checksum 與後繼回報的 checksum 比對。** 這是一道非常強的端到端護欄：只要鏈上任一副本的資料與其他人不同，寫入當下就會被抓到，而不是等到將來讀取或校驗。REMOVE 是唯一的例外——刪除時一端可能已經 `chunk not found`（checksum 為 NONE），這是已知的良性不一致。

**階段 7：commit**（`:492-518`）

```cpp
auto commitResult = co_await doCommit(requestCtx, commitIO, req.options, chunkEngineJob,
                                      req.featureFlags, target->storageTarget);
code = ...;
if (LIKELY(code == 0)) { }
else if (code == StorageCode::kChunkStaleCommit) { commitResult.commitVer = updateResult.updateVer; }
else { co_return commitResult; }
commitResult.lengthInfo = updateResult.lengthInfo;   // 長度與 checksum 用本地寫入的結果
commitResult.checksum   = updateResult.checksum;
```

`kChunkStaleCommit`（`meta.commitVer ≥ commitIO.commitVer`，`ChunkReplica.cc:440-449`）同樣視為成功——重試導致的重複 commit 是冪等的。

### 5.3 `ReliableUpdate`：channel 級的冪等層

`src/storage/service/ReliableUpdate.cc:16-127`。它包在 `handleUpdate` 外面，做四件事：

**(1) 拒絕 `ChannelId{0}`**（`:29-36`）。無 channel 的請求無法去重，直接 `kFoundBug`。唯一的例外是 `updateType == REMOVE && channel.id == 0`——這種請求會繞過 `ReliableUpdate` 直接走 `handleUpdate`（`src/storage/service/StorageOperator.cc:310-314`、`:836-840`），因為 `removeChunks` 的內部合成請求本來就是冪等的（刪不存在的 chunk 回成功）。

**(2) 取 channel 鎖，拿不到直接回錯**（`:53-62`）：

```cpp
auto lock = target->storageTarget->tryLockChannel(baton, fmt::format("{}:{}", clientId, req.tag.channel.id));
if (!lock.locked()) {
  reliableUpdateWaited.addSample(1);
  XLOGF(ERR, "Channel is locked, need retry, tag: {}, req: {}", req.tag, req);
  co_return makeError(StorageCode::kChannelIsLocked);
}
```

注意這裡用 `tryLock` 而不是 `lock`——**寧可讓客戶端重試，也不在 server 端排隊**。因為同一 channel 上出現兩個並發請求本身就是客戶端違約（或是舊請求的重複投遞），排隊只會把問題藏起來。

**(3) 三分支的快取判定**（`:65-110`）：

```
req.seqnum <  cached.seqnum                     → kDuplicateUpdate（過期的重試，直接丟）
req.seqnum == cached.seqnum && generationId 相同 →
     ├ requestId 不同                → kFoundBug（同一 seqnum 被兩個請求用了）
     ├ cached 成功                   → 回放快取結果
     └ cached 失敗 且 req.updateVer==0 且非 chunk engine 且 succUpdateVer!=0
                                     → 撿回上次分配的 updateVer（見下）
req.seqnum >  cached.seqnum                     → 執行新任務
```

`generationId` 是每個 `StorageTarget` 物件建構時從全域原子計數器取的序號（`src/storage/store/StorageTarget.cc:66`、`gGenerationId`）。target 被 offline 又 reload 之後 generationId 會變，**快取結果隨之全部失效**——因為 reload 期間可能發生過 resync，舊的版本號結論不再成立。用一個 u32 就把「快取跨 target 生命週期失效」這件事解決掉，不需要清表。

「撿回 updateVer」那條分支（`:105-109`）處理的是一個很細的競態：HEAD 上一次嘗試在**分配了 updateVer 之後**失敗（例如轉發逾時），客戶端重試時 `updateVer` 仍是 0，如果讓它重新自增就會跳號變成 `kChunkAdvanceUpdate`。所以把上次分配的版本撿回來復用。chunk engine 路徑不需要這招（它自己管版本），所以條件裡排除了。

**(4) 快取長度修正**（`:87-94`）：

```cpp
if (*updateResult.lengthInfo != req.payload.length && !req.payload.isExtend()) {
  updateResult.lengthInfo = req.payload.length;
  XLOGF(WARN, "Cached length info {} not equal to write size in request {} ...");
}
```

因為 EXTEND 回傳的是 chunk 的最終長度而非寫入長度，其餘操作回傳的都應該等於請求長度。

`cleanUpExpiredClients()`（`:129-156`）由 `CheckWorker` 每 60 秒觸發一次，拿 mgmtd 的活躍 client 清單去對照，把不在清單裡且超過 `expired_clients_timeout`（1h）的條目刪掉。預設是**關閉**的（`clean_up_expired_clients = false`）——保守起見，寧可讓表長一點。

### 5.4 `doUpdate()`：把資料拉進來

`src/storage/service/StorageOperator.cc:521-614`。三條互斥的取資料路徑：

```cpp
if (BITFLAGS_CONTAIN(featureFlags, FeatureFlags::SEND_DATA_INLINE)) {
  if (updateIO.inlinebuf.data.size() != updateIO.length) { ... kFoundBug }
  job.state().data = updateIO.inlinebuf.data.data();          // (a) 資料就在 RPC 封包裡
} else if (updateIO.isWrite()) {
  auto allocateResult = buffer.tryAllocate(updateIO.rdmabuf.size());
  if (UNLIKELY(!allocateResult)) allocateResult = co_await buffer.allocate(updateIO.rdmabuf.size());
  job.state().data = allocateResult->ptr();
  remoteBuf = allocateResult->toRemoteBuf();                  // ← 給後繼來讀的描述子
  if (!BITFLAGS_CONTAIN(featureFlags, FeatureFlags::BYPASS_RDMAXMIT)) {
    auto readBatch = ibSocket->rdmaReadBatch();
    auto batchAddResult = readBatch.add(updateIO.rdmabuf, std::move(*allocateResult));
    ... SemaphoreGuard guard(concurrentRdmaReadSemaphore_[dev]); co_await guard.coWait();
    auto postResult = co_await readBatch.post();              // (b) 單邊 RDMA Read
  }
}
// (c) TRUNCATE / EXTEND / REMOVE 不需要資料
```

`tryAllocate` 先試無等待版本（只吃 `semaphore_.try_wait()`），失敗才走 `co_await allocate()`（`src/storage/service/BufferPool.cc:102-141`）。這個「先試後等」模式在讀路徑也一樣（`StorageOperator.cc:143-146`）。

`remoteBuf = allocateResult->toRemoteBuf()` 是整條鏈的接力棒：本地 buffer 的 `(addr, rkey, length)` 三元組會被塞進轉發給後繼的 `UpdateReq.payload.rdmabuf`（`src/storage/service/ReliableForwarding.cc:148`），後繼就用它發起自己的 RDMA Read。

寫完之後有一段故障隔離：

```cpp
auto code = job.result().lengthInfo.error().code();
if (code == StorageCode::kChunkWriteFailed || code == StorageCode::kChunkMetadataSetError) {
  components_.targetMap.offlineTargets(target->path().parent_path());
}
```
（`src/storage/service/StorageOperator.cc:608-612`）

**寫盤失敗或 meta 寫失敗 ⟹ 立刻把整顆盤上的所有 target 下線。** 注意粒度是 `parent_path()`，也就是磁碟掛載點而非單個 target——因為這兩種錯誤幾乎必然是硬體或檔案系統層面的，同一顆盤上其他 target 不可能倖免。

### 5.5 `ReliableForwarding`：轉發、重試與 syncing 特化

**`forwardWithRetry()`**（`src/storage/service/ReliableForwarding.cc:33-104`）是一個指數退避重試迴圈：

```cpp
ExponentialBackoffRetry retry(config_.retry_first_wait().asMs(),    // 100ms
                              config_.retry_max_wait().asMs(),      // 1s
                              config_.retry_total_time().asMs());   // 60s
for (uint32_t retryCount = 0; !stopped_; ++retryCount) {
  auto waitTime = retry.getWaitTime();
  auto targetResult = components_.targetMap.getByChainId(req.payload.key.vChainId, allowOutdatedChainVer);
  target = std::move(*targetResult);                       // ← 每輪都重取
  auto ioResult = co_await forward(req, retryCount, rdmabuf, chunkEngineJob, target, commitIO, waitTime);
  if (LIKELY(bool(ioResult.lengthInfo))) { co_return ioResult; }
  else if (ioResult.lengthInfo.error().code() == StorageCode::kNoSuccessorTarget) { co_return ioResult; }
  ...
  if (waitTime.count() == 0) { XLOGF_IF(DFATAL, ...); co_return ioResult; }    // 重試預算耗盡
  else if (code != RPCCode::kTimeout) {
    for (auto elapsed = 0ms; elapsed < waitTime && !stopped_; elapsed += 100ms) {
      target = ...getByChainId(...);
      if (!target->successor.has_value()) break;           // ← 後繼被摘掉了，不用再等
      co_await folly::coro::sleep(std::min(checkInterval, waitTime - elapsed));
    }
  }
}
```

三個細節：

- **重試預算 60 秒**。這是一個很長的窗口，設計意圖是「撐過 mgmtd 完成一次鏈重組」。期間客戶端的請求就掛在這裡，而它持有的 chunk 鎖也一直不放——所以同一個 chunk 上的其他寫入也會跟著等。這是刻意的：寧可阻塞，不可分歧。
- **睡眠期間每 100ms 重查一次 target**，一旦 mgmtd 把故障的後繼摘掉（`successor == nullopt`），立刻跳出重試迴圈，下一輪 `forward()` 就會回 `kNoSuccessorTarget`（表示「我現在是 TAIL 了」）並直接 commit。這把「等 mgmtd 修好鏈」的反應時間從 `retry_max_wait` 壓到 100ms。
- **`code == RPCCode::kTimeout` 時不睡**——RPC 本身已經等過了 `waitTime`，再睡一次是浪費。

**`forward()`**（`:106-136`）處理 TAIL 判定與鏈版本回退檢查：

```cpp
if (!target->successor.has_value()) {
  commitIO.commitChainVer = target->vChainId.chainVer;    // 我是 TAIL，用本地鏈版本
  co_return makeError(StorageCode::kNoSuccessorTarget);
}
auto ioResult = co_await doForward(...);
if (ioResult.lengthInfo) {
  commitIO.commitVer      = ioResult.commitVer;
  commitIO.commitChainVer = ioResult.commitChainVer;      // 用後繼的鏈版本
  if (ioResult.commitChainVer > target->vChainId.chainVer) {
    ... co_return makeError(StorageCode::kChainVersionMismatch, ...);   // 對方比我新，重試
  }
}
```

**commit 用的是「後繼回報的 chainVer」而不是本地的。** 因為 CRAQ 的權威在 TAIL，TAIL 用哪一版鏈接受了這次寫，整條鏈就必須記錄那一版。若後繼的版本反而比自己高，說明本地 routing info 落後，必須重試而不能硬幹。

**`doForward()`**（`:138-280`）是 syncing 特化的所在：

```cpp
isSyncing = target.successor->targetInfo.publicState == hf3fs::flat::PublicTargetState::SYNCING;
if (isSyncing) { updateReq.options.isSyncing = true; updateReq.options.commitChainVer = target.vChainId.chainVer; }

bool readForSyncing = isSyncing && !req.payload.isRemove() &&
                      (req.options.isSyncing || req.payload.isTruncate() || req.payload.isExtend() ||
                       (req.payload.isWrite() && req.payload.length != req.payload.chunkSize));
```

當後繼正在 SYNCING（資料還不完整）時，**部分寫必須被升級成整塊寫**——因為後繼上那個 chunk 可能根本不存在，或內容是舊的，只把新寫的那幾 KB 送過去會產生一個縫合怪。所以前驅會：

1. 從本地讀出**整個 chunk**（`payload.length = req.payload.chunkSize`，`readUncommitted = true`），走的是與客戶端讀完全相同的 `BatchReadJob` + `aioReadWorker` 路徑（`:176-191`）；
2. 把請求改寫成 `offset=0, length=實際長度, updateType=WRITE`，checksum 換成整塊的 checksum（`:204-208`）；
3. 若長度小於 `max_inline_forward_bytes`，直接內嵌進 RPC 封包（`:210-213`），省掉一次 RDMA 往返。

`batch.setRecalculateChecksum()`（`:181`）讓這次讀在完成時**重新算一遍 checksum 並與 meta 裡記的比對**（`src/storage/aio/BatchReadJob.cc:43-54`），不一致就直接 `CRITICAL` + `kChecksumMismatch`。resync 是把資料複製到新副本的最後關口，在這裡加一道全量校驗的性價比最高。

`chunkEngineJob.chunk()` 存在時（`:185-187`）會把 chunk engine 已經取得的 `Chunk` 指標直接交給讀 job，避免再查一次——因為此時本地寫剛完成，那個 `WritingChunk` 就握著最新的資料位置。

**commit `e2cef82` 的修正**：舊條件是 `req.payload.isWriteTruncateExtend() && isSyncing && (req.options.isSyncing || req.payload.length != req.payload.chunkSize)`。問題在於 TRUNCATE/EXTEND 的 `length` 語意是「目標長度」而不是「寫入長度」，當它剛好等於 `chunkSize` 時舊條件會判定「這是整塊寫，不需要重讀」，於是把一個 truncate 原封不動轉給正在 syncing 的後繼——後繼上那個 chunk 可能不存在，truncate 就變成無效操作，資料永久缺失。新條件把 truncate/extend 無條件納入重讀，並同時在 `handleUpdate` 加上「syncing 請求不得是 truncate/extend」的斷言，兩邊夾擊。

最後是一道自我保護（`:264-277`）：

```cpp
if (errorCode == StorageCode::kChecksumMismatch) {
  auto realChecksum = ChecksumInfo::create(reqChecksum.type, (const uint8_t *)updateReq.payload.rdmabuf.addr(),
                                           updateReq.payload.length);
  if (reqChecksum != realChecksum) {
    XLOGF(DFATAL, "local rdma buffer is corrupted local {} != client {}, req: {}, kill self...", ...);
    ApplicationBase::handleSignal(SIGUSR2);
  }
}
```

後繼回報 checksum 不符時，前驅**回頭重算自己 RDMA buffer 裡的內容**。如果連自己的 buffer 都對不上請求宣稱的 checksum，那就是本機記憶體損毀（RDMA 寫壞、DRAM 位翻轉），此時**自殺**是唯一負責任的選擇——繼續服務只會把壞資料寫進更多副本。

### 5.6 `doCommit()` 與 commit 的回溯

`src/storage/service/StorageOperator.cc:616-636` 只是把 `CommitIO` 包成 `UpdateJob` 丟給 `updateWorker_`。真正的邏輯在 `ChunkReplica::commit()`（`src/storage/store/ChunkReplica.cc:397-467`）：

```cpp
if (commitIO.isRemove && !getResult && getResult.error().code() == StorageCode::kChunkMetadataNotFound) {
  result.commitVer = result.updateVer = commitIO.commitVer;   // 刪已不存在的 chunk = 成功
  return 0;
}
if (job.commitChainVer() < meta.chainVer)   { reportFatalEvent(); DFATAL; return kChainVersionMismatch; }
if (commitIO.commitVer > meta.updateVer)    { reportFatalEvent(); DFATAL; return kChunkVersionMismatch; }

if (commitIO.isForce)                       { meta.chunkState = ChunkState::CLEAN; meta.commitVer = commitIO.commitVer; }
else if (meta.chunkState == ChunkState::DIRTY) { return kChunkNotClean; }      // 資料沒寫完，不能 commit
else if (meta.commitVer < commitIO.commitVer)  { meta.commitVer = commitIO.commitVer; }
else                                           { return kChunkStaleCommit; }

if (meta.commitVer == meta.updateVer) { meta.chunkState = ChunkState::COMMIT; meta.chainVer = job.commitChainVer(); }
...
auto metaResult = meta.readyToRemove() ? store.remove(chunkId, chunkInfo) : store.set(chunkId, chunkInfo);
```

三個要點：

1. **`commitVer > updateVer` 是致命錯誤。** 這代表後繼 commit 了一個本地根本沒寫過的版本——鏈的狀態已經無法解釋，`reportFatalEvent()` 打上 `storage.fatal` 計數並 `DFATAL`。
2. **`chainVer` 只在 `commitVer == updateVer` 時才推進。** 也就是說 chunk 的 `chainVer` 記錄的是「最後一次完整達成共識時所在的鏈版本」，中間那些 CLEAN 狀態不會污染它。§8.4 的 resync 五維比對完全建立在這個性質上。
3. **`isForce` 只有一個使用者**：`ChunkStore::resetUncommitted()`（`src/storage/store/ChunkStore.cc:179-214`）。當一個 target 從 `LASTSRV` 恢復成 SERVING/SYNCING 時（`src/storage/service/TargetMap.cc:272-279`），它是全鏈唯一的倖存者，那些 CLEAN 但沒 commit 的 chunk 不可能再從別人那裡得到確認，只能自行 commit。這是「最後一個活著的副本說了算」。

### 5.7 三個版本號的不變式全表

跨節點的不變式（設鏈為 `T₁ → T₂ → … → Tₙ`）：

```
(I1)  ∀i:  Tᵢ.commitVer ≤ Tᵢ.updateVer ≤ Tᵢ.commitVer + 1
(I2)  ∀i<j: Tᵢ.updateVer ≥ Tⱼ.updateVer          寫入沿鏈單向傳播
(I3)  ∀i<j: Tᵢ.commitVer ≤ Tⱼ.commitVer          commit 沿鏈反向傳播
(I4)  Tₙ.commitVer == Tₙ.updateVer 恆成立         TAIL 收到寫就立即 commit
(I5)  讀取任何 Tᵢ 若 commitVer == updateVer，讀到的一定是全鏈已確認的版本
```

(I5) 就是 CRAQ 相對於原始 chain replication 的核心優勢：**可以讀任意副本，不必都打 TAIL**。`ChunkReplica::aioPrepareRead` 的檢查（`src/storage/store/ChunkReplica.cc:61-66`）正是 (I5) 的直接編碼：

```cpp
if (UNLIKELY(result.commitVer != result.updateVer && !state.readUncommitted)) {
  storageReadUncommitted.addSample(1);
  return makeError(StorageCode::kChunkNotCommit, std::move(msg));
}
```

當副本處於「有 pending 寫入」的狀態（`commitVer != updateVer`），這個副本**直接拒絕讀**，由客戶端去換一個副本重試。CRAQ 論文裡「向 TAIL 查詢真實版本」的做法在這裡被簡化成了「拒絕 + 重試」——省掉一次跨機器往返，代價是寫入密集時讀的重試率上升。`storage.chunk_read.uncommitted` 這個計數器就是在量這件事。

### 5.8 checksum：算在哪、存哪、驗在哪

`ChecksumInfo`（`src/fbs/storage/Common.h:113-201`）支援 `NONE` / `CRC32C` / `CRC32`，以 1MB 為單位分段計算（`kChunkSize = 1_MB`）。

**全鏈路的六個檢查點：**

| # | 位置 | 檢查什麼 |
|---|---|---|
| 1 | 客戶端寫入前 | 算出 `UpdateIO.checksum` 一併送出 |
| 2 | `ChunkReplica::update`（`ChunkReplica.cc:193-207`） | 從 RDMA buffer 重算，與請求宣稱的比對；不符 → `reportFatalEvent()` + `DFATAL` |
| 3 | `ChunkReplica::updateChecksum`（`:319-394`） | 更新 chunk 的整塊 checksum |
| 4 | `handleUpdate` 階段 6（`StorageOperator.cc:480-487`） | 與後繼回報的比對 |
| 5 | `doForward` 失敗回溯（`ReliableForwarding.cc:264-277`） | 懷疑本機記憶體損毀時自殺 |
| 6 | `AioReadJob::setResult`（`BatchReadJob.cc:24-54`） | 讀出的資料算 checksum 回給客戶端；resync 讀還會與 meta 比對 |

**`updateChecksum` 的四條路徑**（`src/storage/store/ChunkReplica.cc:334-390`），對應四個計數器：

```cpp
if (writeIO.checksum.type == ChecksumType::NONE || meta.size == 0) {
  meta.checksumValue = 0;                                   // storage.chunk_update.checksum_none
} else if (writeIO.offset == 0 && writeIO.length == meta.size) {
  meta.checksumValue = writeIO.checksum.value;              // storage.chunk_update.checksum_reuse   整塊覆蓋，直接用
} else if (writeIO.checksum.type == chunkChecksum.type && combineChecksum) {
  chunkChecksum.combine(writeIO.checksum, writeIO.length);  // storage.chunk_update.checksum_combine 追加寫，數學合併
  meta.checksumValue = chunkChecksum.value;
} else {
  auto prefixChecksum = chunkInfo.view.checksum(type, writeIO.offset, 0, meta);       // 讀前綴
  auto suffixChecksum = chunkInfo.view.checksum(type, suffixLength, suffixStart, meta); // 讀後綴
  prefixChecksum->combine(writeIO.checksum, writeIO.length);
  prefixChecksum->combine(*suffixChecksum, suffixLength);
  meta.checksumValue = prefixChecksum->value;               // storage.chunk_update.checksum_read_chunk
}
```

`combineChecksum = chunkSizeBeforeWrite > 0 && isAppendWrite`。**只有純追加寫可以用 `crc32c_combine` 免讀合併**；覆蓋寫必須把 chunk 的前綴與後綴都重讀一遍再拼。這是 3FS 一直把「追加寫」當成一等公民優化的又一處體現（前面還有 `skipPersist`）。`storage.chunk_update.checksum_read_chunk` 這個計數器如果很高，代表工作負載有大量隨機覆蓋寫，讀放大會很嚴重。

TRUNCATE/EXTEND 的處理很特別（`:328-332`）：把 `writeIO` 偽造成「在新的 `meta.size` 位置寫 0 個 byte」，然後走同一套邏輯——結果就是「讀前綴 [0, newSize)、後綴為空」，等於重算整塊。

**為什麼 C++ 端存補數。** `ChecksumInfo::create` 以 `~0U` 為起始值連續呼叫 `folly::crc32c(data, len, prev)`，folly 的這個 API 回傳的是**可續算的中間狀態**而非最終值，標準 CRC32C 等於它的補數。3FS 選擇把中間狀態直接存進 `checksumValue`，好處是分段計算與 `combine` 都不需要來回反轉；代價是所有對外呈現的地方都要補一次 `~`：

```cpp
// 格式化輸出（src/fbs/storage/Common.h:769）
return fmt::format_to(ctx.out(), "{}#{:08X}", magic_enum::enum_name(checksum.type), ~checksum.value);

// 送進 Rust chunk engine（src/storage/store/ChunkEngine.cc:41-42）
if (updateIO.checksum.type == ChecksumType::CRC32C) req.checksum = ~updateIO.checksum.value;

// 從 Rust 拿回來（src/storage/store/ChunkEngine.cc:66、ChunkEngine.h:22）
result.checksum = ChecksumInfo{ChecksumType::CRC32C, ~req.out_checksum};
out.checksumValue = ~in.checksum;
```

Rust 側用的是標準的 `crc32c` crate 語意（存最終值），所以橋接處**每一次跨語言傳遞 checksum 都要取一次補數**。這是 §10 那張橋接表裡最容易寫錯的一格。

---

## 6. 讀路徑：對齊、AIO、單邊 RDMA Write

### 6.1 `batchRead()` 六個階段

`src/storage/service/StorageOperator.cc:82-231`。一個 `BatchReadReq` 帶一個 `vector<ReadIO>`，每個 `ReadIO` 是 `{offset, length, GlobalKey{vChainId, chunkId}, rdmabuf}`——`rdmabuf` 是**客戶端已註冊記憶體的遠端描述子**。

```
① 取 target 快照，逐 IO 校驗
     snapshot = targetMap.snapshot()          ← 整批共用一個快照，保證批內鏈版本一致
     for each ReadIO:
         target = snapshot->getByChainId(key.vChainId, batch_read_ignore_chain_version)
         require target->upToDate()                          ← 只有 UPTODATE 才可讀
         require readIO.length ≤ readIO.rdmabuf.size()
         it->state().storageTarget = target->storageTarget.get()   ← 裸指標，見下
         累計 totalLength / headLength / tailLength

② 配置本地 RDMA buffer
     buffer = rdmabufPool.get()
     for each job: job.state().localbuf = buffer.tryAllocate(job.alignedLength()) 或 co_await allocate()
                   job.state().bufferIndex = buffer.index()   ← io_uring fixed buffer 的索引

③ 分片投遞給 AIO worker
     for (start = 0; start < batchSize; start += batch_read_job_split_size /* 1024 */)
         co_await aioReadWorker.enqueue(AioReadJobIterator(&batch, start, splitSize))

④ 等整批完成
     co_await batch.complete()                 ← finishedCount_ == jobs_.size() 時 post baton

⑤ 回傳資料（三選一）
     SEND_DATA_INLINE  → batch.copyToRespBuffer(rsp.inlinebuf.data)   memcpy 進回應封包
     BYPASS_RDMAXMIT   → 什麼都不做（壓測用）
     預設               → ctx.writeTransmission() 組批 → 取 semaphore → post()  單邊 RDMA Write

⑥ 回應
```

**階段 ① 存的是裸指標 `StorageTarget*`。** 這是安全的，因為 `snapshot`（一個 `shared_ptr<const TargetMap>`）在整個 `batchRead` 協程的棧上活著，而 `TargetMap` 持有 `shared_ptr<StorageTarget>`。批次共用一個快照還有第二個好處：**同一批請求裡不會出現一半用舊鏈版本、一半用新鏈版本的情況**。

`batch_read_ignore_chain_version`（預設 false）是一個逃生開關：開啟後讀取不校驗 `chainVer`，用於鏈版本頻繁跳動時保住讀可用性。

### 6.2 4KB 對齊與 head/tail 裁切

`O_DIRECT` 要求偏移、長度、緩衝區位址三者都 4KB 對齊。`AioReadJob` 的建構子把這件事編碼成兩個欄位（`src/storage/aio/BatchReadJob.cc:16-22`）：

```cpp
state_.headLength = readIO_.offset % kAIOAlignSize;
state_.tailLength = (kAIOAlignSize - (readIO_.offset + readIO_.length) % kAIOAlignSize) % kAIOAlignSize;
```

```
       readIO.offset                    readIO.offset + length
            │                                    │
  ──────────┼────────────────────────────────────┼──────────
  │  head   │        使用者要的資料                │  tail   │
  └─────────┴────────────────────────────────────┴─────────┘
  ▲                                                        ▲
  alignedOffset() = offset - headLength          alignedLength() = length + head + tail
  （4KB 對齊）                                    （4KB 的整數倍）
```

於是：

- **配的 buffer 是 `alignedLength()`**，讀進去的也是 `alignedLength()`（`state.readLength = job.alignedLength()`，`ChunkReplica.cc:69`）；
- **回給客戶端的是 `localbuf.subrange(headLength, length)`**（`BatchReadJob.cc:80`、`:102`），把多讀的頭尾切掉；
- `alignBuffer()`（`src/storage/service/BufferPool.cc:17-25`）保證每次 `tryAllocate` 之後 `current_` 的起始位址仍是 4KB 對齊的，做法是「切完之後把剩餘部分往前推到下一個對齊邊界」。

三個計數器 `storage.aio_align.total_head_length` / `total_tail_length` / `total_length` 專門量這個放大。若 head+tail 佔 total 的比例很高，代表客戶端在做大量小的非對齊讀，適合開 `SEND_DATA_INLINE`。

### 6.3 AIO 執行引擎：libaio 與 io_uring 雙實作

`src/storage/aio/AioStatus.h` 定義了共同介面 `IoStatus`：

```cpp
class IoStatus {
  bool hasUnfinishedBatchReadJob() const { return iterator_; }
  void setAioReadJobIterator(AioReadJobIterator it) { iterator_ = it; }
  bool availableToSubmit() const { return inflight_ < maxEvents_; }
  virtual void collect() = 0;                    // 準備 SQE / iocb
  virtual void submit() = 0;                     // 送出
  virtual void reap(uint32_t minCompleteIn) = 0; // 收割
};
```

工作執行緒的主迴圈（`src/storage/aio/AioReadWorker.cc:60-94`）：

```cpp
while (true) {
  auto it = queue_.dequeue();                       // BoundedQueue，容量 4096
  if (it.isNull()) return Void{};                   // nullptr iterator = 停止訊號
  batchReadInQueueRecorder.addSample(RelativeTime::now() - it.startTime());
  IoStatus &status = config_.useIoUring() ? static_cast<IoStatus &>(ioUringStatus) : aioStatus;
  status.setAioReadJobIterator(it);
  do {
    status.collect();
    status.submit();
    while (status.inflight()) { status.reap(config_.min_complete()); }
  } while (status.hasUnfinishedBatchReadJob());
}
```

一個很重要的性質：**每個執行緒同時只處理一個 `AioReadJobIterator`，且會把它做完才回頭撈下一個**。`collect()` 受 `availableToSubmit()`（`inflight_ < max_events` = 512）限制，所以一個超過 512 個 IO 的分片會被拆成多輪 collect/submit/reap——外層的 `do...while(hasUnfinishedBatchReadJob())` 就是為了這個。深度控制因此是**兩層**的：`batch_read_job_split_size`(1024) 決定一次 enqueue 多少個 IO，`max_events`(512) 決定同時在飛多少個。

`ioengine` 支援 `libaio` / `io_uring` / `random` 三種取值（`src/storage/aio/AioReadWorker.h:20-49`），`random` 會**逐批隨機選一個引擎**——這是灰度與 A/B 對照用的，兩套 metrics（`storage.io_submit` / `storage.io_getevents`）共用，可以直接比較。預設是 `libaio`（`configs/storage_main.toml:66`），但 `enable_io_uring = true` 讓 io_uring 的資源仍然被初始化（fd 與 buffer 都註冊了），可以熱切。

**io_uring 路徑的兩處固定化**（`src/storage/aio/AioStatus.cc:214-243`）：

```cpp
::io_uring_prep_read_fixed(sqe,
                           state.fdIndex.value_or(state.readFd),   // 註冊過的 fd 索引，或裸 fd
                           state.localbuf.ptr(),
                           state.readLength,
                           state.readOffset,
                           state.bufferIndex);                     // 註冊過的 buffer 索引
if (state.fdIndex) { sqe->flags |= IOSQE_FIXED_FILE; }
```

`fdIndex` 來自 `GlobalFileStore::collect()` 的全域編號（`src/storage/store/GlobalFileStore.cc:51-60`），`bufferIndex` 來自 `BufferPool` 的註冊索引。兩者都是啟動時一次註冊，之後每次 IO 省掉核心的 fd 查表與記憶體釘選。注意 `fdIndex` 是 `std::optional`——chunk engine 路徑拿到的 fd 來自 Rust 側的 `fd_and_offset()`（`src/storage/store/ChunkEngine.h:67-70`），**不在註冊表裡**，所以退回用裸 fd。

**libaio 路徑的錯誤處理更細**（`src/storage/aio/AioStatus.cc:108-151`）：`io_submit` 回 `-EAGAIN` 就重試、回 `-EBADF` 就把那一個 job 標失敗然後跳過繼續、其他錯誤才把整批標失敗。io_uring 的 `submit()` 只有一個全成或全敗的分支（`:245-260`）。

**收割與長度計算**（`src/storage/aio/AioStatus.cc:27-55`）：

```cpp
auto length = std::min(std::min(std::max(0l, res - job->state().headLength), int64_t(job->readIO().length)),
                       std::max(0l, int64_t(job->state().chunkLen) - job->readIO().offset));
```

三重裁切：扣掉 head、不超過請求長度、不超過 chunk 的實際有效長度（`state.chunkLen = meta.size`）。第三項是「讀超過 chunk 尾巴」的處理——回傳短讀而不是報錯，語意上等同於 POSIX 的 `pread` 讀到 EOF。

### 6.4 讀完成時的三件事

`AioReadJob::setResult()`（`src/storage/aio/BatchReadJob.cc:24-63`）：

```cpp
// (1) 算 checksum
if (checksumType == ChecksumType::NONE)                       result_.checksum = {NONE, 0U};
else if (checksumType == state_.chunkChecksum.type
         && readIO_.offset == 0 && *lengthInfo == state_.chunkLen)
                                                              result_.checksum = state_.chunkChecksum;  // 整塊讀，直接用 meta 裡的
else  result_.checksum = ChecksumInfo::create(checksumType, dataBuf.ptr(), dataBuf.size());

// (2) 重新檢查版本
auto result = state_.storageTarget->aioFinishRead(*this);
if (UNLIKELY(!result)) lengthInfo = makeError(std::move(result.error()));

// (3) resync 專用：全量校驗
if (batch_.recalculateChecksum() && readIO_.offset == 0 && *lengthInfo == state_.chunkLen) {
  auto realChecksum = ChecksumInfo::create(state_.chunkChecksum.type, state_.localbuf.ptr(), *lengthInfo);
  if (UNLIKELY(realChecksum != state_.chunkChecksum)) { aioChecksumMismatch.addSample(1); ... kChecksumMismatch }
}
```

第 (2) 步 `aioFinishRead` 是**撕裂讀偵測**（`src/storage/store/ChunkReplica.cc:79-100`）：

```cpp
if (UNLIKELY(result.updateVer != meta.updateVer)) {
  storageReadUncommitted.addSample(1);
  return makeError(StorageCode::kChunkNotCommit, std::move(msg));
}
```

讀之前記下 `updateVer`，讀完之後再看一次——如果變了，說明讀的過程中有並發寫入把資料改掉了，這次讀出來的可能是新舊混合的位元組。**因為讀路徑刻意不拿 chunk 鎖**（拿了就會被寫阻塞，完全放棄了 CRAQ 的讀擴展性），所以用「樂觀讀 + 版本再驗證」來取代互斥。

chunk engine 路徑則完全跳過這一步（`src/storage/store/StorageTarget.cc:299-304`）：

```cpp
Result<Void> StorageTarget::aioFinishRead(AioReadJob &job) {
  if (job.state().chunkEngineJob.has_chunk()) { return Void{}; }
  return ChunkReplica::aioFinishRead(chunkStore_, job);
}
```

因為新引擎是 copy-on-write 的：`get_raw_chunk()` 回來的是一個 `Arc<Chunk>` 快照，寫入會分配新的 position 而不是原地覆蓋，讀者手上的那份永遠不會被改。**COW 讓「撕裂讀」這個問題在新引擎裡不存在**，代價是寫放大與空間佔用（也就是 `AllocateWorker` 要不停 `compact_groups` 的原因）。

### 6.5 回填客戶端：批次 RDMA Write + 每卡限流

`src/storage/service/StorageOperator.cc:176-226`：

```cpp
auto writeBatch = ctx.writeTransmission();           // IBV_WR_RDMA_WRITE 的批次
batch.addBufferToBatch(writeBatch);                  // 把每個成功的 job 的 (remote, local) 對加進去

auto rdmaSemaphoreIter = concurrentRdmaWriteSemaphore_.find(ibSocket->device()->id());
if (ctx.packet().controlRDMA() && RDMATransmissionReqTimeout != 0_ms && applyTransmissionBeforeGettingSemaphore) {
  co_await writeBatch.applyTransmission(RDMATransmissionReqTimeout);   // 向客戶端申請傳輸額度
}
SemaphoreGuard guard(rdmaSemaphoreIter->second);
co_await guard.coWait();                                               // 本機該張卡的併發額度
if (ctx.packet().controlRDMA() && RDMATransmissionReqTimeout != 0_ms && !applyTransmissionBeforeGettingSemaphore) {
  co_await writeBatch.applyTransmission(RDMATransmissionReqTimeout);
}
auto postResult = co_await writeBatch.post();
```

這裡有**兩層獨立的流控**，而且順序是可設定的：

- `applyTransmission` 是**客戶端側**的許可（`enable_rdma_control` / `max_concurrent_transmission`），防止多個 storage 節點同時往同一個客戶端灌爆它的 QP；
- `concurrentRdmaWriteSemaphore_` 是**本機該張 HCA** 的併發上限（`max_concurrent_rdma_writes = 256`），防止單張卡的 WQ 被塞爆。

`apply_transmission_before_getting_semaphore`（預設 true）決定先搶哪個。先申請客戶端額度再搶本機額度，可以避免「握著本機額度乾等客戶端」——本機額度是稀缺資源（每卡 256），握著它睡覺會拖垮整台機器的吞吐。

`hf3fs::Semaphore` 支援**熱調整**（`changeUsableTokens`，`src/common/utils/Semaphore.h:45-53`）：它其實是一個 `maxTokens = 4096` 的固定 semaphore，靠「預先 wait 掉多餘的 token」來模擬較小的容量。改設定時 `StorageOperator` 註冊的 callback 會對每張卡的 semaphore 調整可用數（`src/storage/service/StorageOperator.h:56-63`），不需要重建物件。

`post()` 失敗時**把整批 job 的結果都改成錯誤**（`:219-222`），因為無法區分哪幾個 WR 成功了。

### 6.6 `FeatureFlags`：四個旁路開關

`src/fbs/storage/Common.h:72-78`：

| flag | 值 | 效果 |
|---|---|---|
| `BYPASS_DISKIO` | 1 | 讀：直接把 `lengthInfo` 設成請求長度（`StorageOperator.cc:157-161`）；寫：直接 `job.setResult(length)`（`:599-600`）。純網路壓測 |
| `BYPASS_RDMAXMIT` | 2 | 跳過 RDMA 傳輸。純磁碟壓測 |
| `SEND_DATA_INLINE` | 4 | 資料內嵌在 RPC 封包裡（`BatchReadRsp.inlinebuf` / `UpdateIO.inlinebuf`），小 IO 省掉 RDMA 往返 |
| `ALLOW_READ_UNCOMMITTED` | 8 | 讀時跳過 `commitVer == updateVer` 檢查 |

前兩個組合起來可以把讀寫路徑上的任何一段單獨隔離出來測量，這是 3FS 效能調優時定位瓶頸的主要手段。`SEND_DATA_INLINE` 在 resync 路徑上被自動使用（`max_inline_forward_bytes`，§5.5）。

---

## 7. target 狀態機與心跳

### 7.1 兩組狀態：誰說了算

```
   mgmtd 的視角                        storage 本地的視角
   flat::PublicTargetState             flat::LocalTargetState
   ──────────────────────              ──────────────────────
   INVALID  = 0                        INVALID  = 0
   SERVING  = 1   在鏈中正常服務        UPTODATE = 1   本地資料已追平，可讀
   LASTSRV  = 2   全鏈只剩它            ONLINE   = 2   本地已載入，但資料可能落後
   SYNCING  = 4   正在被前驅追資料      OFFLINE  = 4   不可用
   WAITING  = 8   等待進入鏈
   OFFLINE  = 16
```

**方向是不對稱的**：`publicState` 由 mgmtd 決定並透過 routing info 下發；`localState` 由 storage 自己維護並透過心跳上報。兩者透過 `TargetMap::updateLocalState()` 這個純函式耦合（`src/storage/service/TargetMap.cc:329-352`）：

```cpp
if (localState == UPTODATE && (publicState == OFFLINE || publicState == LASTSRV || publicState == WAITING)) {
  XLOGF(CRITICAL, "move to offline state (shutdown), ...");
  return OFFLINE;                                   // ← 降級
} else if (localState == ONLINE && publicState == SERVING) {
  XLOGF(INFO, "move to up-to-date state, ...");
  return UPTODATE;                                  // ← 升級
}
return localState;                                  // 其餘一律不動
```

完整的本地狀態機：

```
                    loadTarget() / addStorageTarget()
                              │
                              ▼
                         ┌────────┐
      ┌─────────────────▶│ ONLINE │◀──────── CheckWorker 偵測 offline 且
      │                  └───┬────┘          weakStorageTarget 已過期 → reload
      │                      │
      │   publicState == SERVING（mgmtd 認為我是好的）
      │                      ▼
      │                 ┌──────────┐
      │                 │ UPTODATE │  ← 唯一可以服務讀的狀態
      │                 └────┬─────┘
      │                      │ publicState ∈ {OFFLINE, LASTSRV, WAITING}
      │                      │   或 磁碟錯誤 / 唯讀 / sync 失敗
      │                      │   或 offlineTarget RPC
      │                      ▼
      │                 ┌─────────┐
      └── reload ───────│ OFFLINE │
                        └─────────┘
                             │ diskError == true 或 offlineUponUserRequest == true
                             ▼  （unrecoverableOffline()）
                     永不自動 reload，只能 removeTarget 後重建
```

`syncReceiveDone()`（`src/storage/service/TargetMap.cc:111-122`）是 `ONLINE → UPTODATE` 的另一條路徑：後繼收到前驅的 `syncDone`(#9) RPC 時直接升級，不必等下一輪 routing info。

**為什麼 `LASTSRV` 會讓 UPTODATE 降級成 OFFLINE？** `LASTSRV` 表示「這條鏈只剩你一個副本還活著」。此時 mgmtd 已經無法保證資料完整性（沒有第二份可比對），storage 主動下線是保守選擇——但注意這個判定只在**本地已經是 UPTODATE** 時觸發，如果本地是 ONLINE（例如剛重啟），`LASTSRV` 不會讓它下線，反而會在 `updateRouting` 尾巴觸發 `resetUncommitted()`（見 §5.6 第 3 點）。

### 7.2 `updateRouting()`：每次都是全量重建

`src/storage/service/TargetMap.cc:124-283` 是 storage 消化 routing info 的唯一入口。它**不做增量 diff，而是每次全量重算**：

```cpp
// 1. reset current state.
routingInfoVersion_ = routingInfo->routingInfoVersion;
chainToTarget_.clear();
syncingChains_.clear();
robin_hood::unordered_set<TargetId> headTargets, tailTargets, lastSrvTargets;   // 記下舊值，只為了印 log
for (auto &[targetId, target] : targets_) {
  if (target.isHead)  headTargets.insert(target.targetId);
  if (target.isTail)  tailTargets.insert(target.targetId);
  if (target.publicState == LASTSRV) lastSrvTargets.insert(target.targetId);
  target.isHead = false; target.isTail = false;
  target.vChainId = VersionedChainId{};
  target.publicState = INVALID;
  target.successor = std::nullopt;
}
```

注意被清掉的是 `isHead/isTail/vChainId/publicState/successor`，**`localState` 與 `storageTarget` 被保留**——前者是本地權威，後者是資源。三個 `unordered_set` 只用來在狀態翻轉時印一行 WARNING（`:256-269`），是純觀測用途。

接著逐條鏈掃描（`:170-270`）：

```cpp
for (auto &[id, chain] : routingInfo->chains) {
  auto it = std::find_if(chain.targets.begin(), chain.targets.end(), [&](const flat::ChainTargetInfo &ti) {
    return bool(getMutableTarget(ti.targetId));               // 找到「屬於我這台機器」的那個 target
  });
  if (it == chain.targets.end()) continue;
  ...
  bool targetIsServing = (publicState == SERVING || publicState == SYNCING);
  target->isHead = (targetIsServing && it == chain.targets.begin());
  target->vChainId = VersionedChainId{chain.chainId, chain.chainVersion};
  ...
  // 6. update successor：往後找「第一個」SERVING 或 SYNCING 的 target
  while (targetIsServing && ++it != chain.targets.end()) {
    if (publicState == SERVING)      target->successor = Successor{{}, *targetInfo};
    else if (publicState == SYNCING) { target->successor = ...; syncingChains_.push_back({chainId, chainVersion}); }
    if (target->successor) { ...填 nodeInfo... }
    break;                                                     // ← 只看下一個，不繼續往後找
  }
  target->isTail = (targetIsServing && !target->successor.has_value());
}
```

**`while` 迴圈裡有個無條件的 `break`**，所以它其實只執行一次——只看**緊鄰的下一個** target。如果下一個是 `WAITING` 或 `OFFLINE`，`successor` 就留空，本節點直接變成 TAIL。這意味著：**鏈上出現一個非 serving 的成員時，鏈就在那裡被截斷了，後面的成員全部收不到寫入**。這是刻意的——CRAQ 要求嚴格的順序傳播，不能跳過中間節點。

`setChainId` 的惰性寫入（`:195-204`）值得一提：target 剛被 `createTarget` 建立時可能沒有 chainId，第一次在 routing info 裡看到自己屬於哪條鏈時才寫進 `target.toml`（`src/storage/store/StorageTarget.cc:259-286`，用 tmp 檔 + rename 保證原子性）。之後如果 routing 說的鏈與本地記錄的不符，直接回 `kRoutingError` 拒絕整次更新——**寧可停用整台機器的 routing 更新，也不接受 target 換鏈**。

### 7.3 `AtomicallyTargetMap`：copy-on-write + CAS

`src/storage/service/TargetMap.cc:370-382`：

```cpp
Result<Void> AtomicallyTargetMap::updateTargetMap(auto &&updateFunc) {
  auto lock = std::unique_lock(mutex_);          // 寫者互斥
  auto map = snapshot();
  while (true) {
    auto newMap = map->clone();                  // 整張表複製
    RETURN_AND_LOG_ON_ERROR(updateFunc(newMap));
    if (targetMap_.compare_exchange_strong(map, std::move(newMap))) break;
  }
  updateCallback_(*snapshot());                  // 觸發心跳 payload 更新
  return Void{};
}
```

讀者完全無鎖：`snapshot()` 只是 `folly::atomic_shared_ptr::load()`。寫者已經被 `mutex_` 序列化了，CAS 迴圈理論上不會失敗——留著是為了防禦有人繞過 `updateTargetMap` 直接改。

**每次修改整張表（含所有 target）都要 clone 一次**，成本 O(target 數)。但 target 數是每機幾十到幾百的量級，而修改的頻率是「routing info 變更」+「磁碟狀態變更」，秒級以下，所以完全划算。換來的是讀路徑上零同步開銷——這在每秒幾十萬次讀的場景下是壓倒性的。

`updateCallback_` 被設成 `updateHeartbeatPayload`（`src/storage/service/Components.cc:125`），因此**任何 target 狀態變化都會立刻更新待上報的心跳內容並把 `triggerHeartbeatFlag` 加一**。`CheckWorker` 每秒檢查一次這個旗標，有值就主動觸發一次心跳（`src/storage/worker/CheckWorker.cc:236-239`、`Components.cc:236-240`），把狀態變更的通知延遲從 10 秒（`auto_heartbeat_interval`）壓到 1 秒。

### 7.4 心跳上報什麼

`Components::updateHeartbeatPayload()`（`src/storage/service/Components.cc:242-261`）：

```cpp
flat::StorageHeartbeatInfo heartbeat;
for (auto &[targetId, target] : targetMap.getTargets()) {
  flat::LocalTargetInfo targetInfo;
  targetInfo.targetId   = targetId;
  targetInfo.localState = offline ? OFFLINE : target.localState;
  targetInfo.diskIndex  = target.diskIndex;
  targetInfo.lowSpace   = target.lowSpace;
  targetStateRecorder.set(uint32_t(target.localState), tag);       // storage.target_state 指標
  if (targetInfo.localState != OFFLINE) {
    targetInfo.usedSize    = target.storageTarget->usedSize();
    targetInfo.chainVersion = target.vChainId.chainVer;
  }
  heartbeat.targets.push_back(targetInfo);
}
```

五個欄位：`targetId`、`localState`、`diskIndex`、`lowSpace`、以及非 OFFLINE 時的 `usedSize` + `chainVersion`。

- `diskIndex` 讓 mgmtd 知道哪些 target 共用一顆盤，排鏈時可以避免把同一條鏈的兩個副本放在同一顆盤上；
- `lowSpace` 讓 mgmtd 在配置新 chunk 時避開快滿的盤；
- **上報 `chainVersion` 是防降級的關鍵**：mgmtd 據此判斷這個 target 認知的鏈版本是否已經跟上，沒跟上就不會把它升成 SERVING。

### 7.5 target 的下線觸發點總表

| 觸發點 | 程式位置 | 設定的旗標 | 可自動恢復？ |
|---|---|---|---|
| 寫盤失敗 / meta 寫失敗 | `StorageOperator.cc:608-612` → `offlineTargets(parent_path)` | `diskError = true` | ✗ |
| `boost::filesystem::space()` 失敗 | `CheckWorker.cc:159-163` | `diskError = true` | ✗ |
| 檔案系統唯讀（寫 `.hf3fs_check` 失敗） | `CheckWorker.cc:170-176` | `diskError = true` | ✗ |
| meta KV `sync()` 失敗 | `SyncMetaKvWorker.cc:69-75` | `diskError = true` | ✗ |
| `offlineTarget` RPC | `TargetMap.cc:294-303` | `offlineUponUserRequest = true` | ✗ |
| routing info 說 publicState 惡化 | `TargetMap.cc:205`（`updateLocalState`） | 無額外旗標 | ✓ |

前五種都設了 `diskError` 或 `offlineUponUserRequest`，也就是 `unrecoverableOffline()` 為真（`src/fbs/storage/Common.h:709`），`CheckWorker` 的自動重載邏輯會直接跳過（`src/storage/worker/CheckWorker.cc:131-133`）。只有最後一種（純粹因為 routing info 而下線的）能被自動重新載入。

這個設計把「軟性下線」與「硬體故障」分得很清楚：前者是叢集調度的正常流轉，後者需要人介入（換盤、`remove-target`、`create-target`）。

---

## 8. resync：前驅主動 PUSH 給後繼

`src/storage/sync/ResyncWorker.cc`（460 行）是全檔案裡邏輯密度最高的一個。

### 8.1 誰發起、什麼時候發起

**發起者永遠是前驅，不是需要資料的那一方。** `ResyncWorker::loop()`（`:66-99`）每 500ms 掃一次：

```cpp
auto syncingChains = components_.targetMap.snapshot()->syncingChains();
syncingRemainingTargetsCount.set(syncingChains.size());
std::shuffle(syncingChains.begin(), syncingChains.end(), std::mt19937{std::random_device{}()});
for (auto &vChainId : syncingChains) {
  bool succ = shards_.withLock([vChainId](SyncingChainIds &m) {
    auto &status = m[vChainId.chainId];
    if (!status.isSyncing && RelativeTime::now() - status.lastSyncingTime > 30_s) { status.isSyncing = true; return true; }
    return false;
  }, vChainId.chainId);
  if (succ) pool_.enqueueSync(vChainId);
}
```

`syncingChains_` 是 `updateRouting()` 填的：**當本節點的 target 的緊鄰後繼處於 `SYNCING` 狀態時，這條鏈就進清單**（`src/storage/service/TargetMap.cc:231-234`）。所以「誰要同步」這件事完全由 mgmtd 的鏈編排決定，storage 只是執行。

兩層節流：`isSyncing` 旗標防同一條鏈並發同步；`lastSyncingTime > 30_s` 讓失敗的同步至少隔 30 秒才重試。`std::shuffle` 是為了避免多個 storage 節點同時重啟後以相同順序衝擊同一批後繼。

協程池 `pool_`（`num_threads = 16`、`coroutines_num = 64`）與 `batchConcurrencyLimiter_`（`max_concurrency = 64`）疊加，構成 resync 的總體限流。

### 8.2 完整協定

```
前驅（SERVING）                                     後繼（SYNCING）
     │                                                    │
     │─── syncStart(#8){vChainId} ───────────────────────▶│
     │                                                    ├ 檢查 publicState == SYNCING
     │                                                    ├ 檢查 localState  == ONLINE
     │                                                    ├ getAllMetadata() 列舉全部 chunk meta
     │                                                    ├ 再查一次 chain version（防中途翻版）
     │◀── TargetSyncInfo{ vector<ChunkMeta> } ────────────┤
     │                                                    │
     ├ getAllMetadataMap(localMetas)  列舉本地全部 chunk    │
     ├ 再查一次 chain version                              │
     │                                                    │
     ├ 五維比對 → writeList / removeList                    │
     │                                                    │
     ├ 逐批（batch_size=16）發 removeList：                 │
     │   forward() → lockChunk → reliableForwarding ─────▶│  update(#3){REMOVE, isSyncing=true}
     │                                                    │
     ├ 逐批發 writeList：                                   │
     │   forward() → lockChunk → reliableForwarding ─────▶│  update(#3){WRITE, isSyncing=true}
     │      ↑ doForward 內部把它改寫成「整塊 chunk 寫」       │  （資料由後繼 RDMA Read 前驅的 buffer）
     │                                                    │
     │─── syncDone(#9){vChainId} ────────────────────────▶│
     │                                                    ├ targetMap.syncReceiveDone()
     │                                                    └ localState: ONLINE → UPTODATE
```

注意 `syncStart` 的回應是**後繼把自己有什麼全部列出來**，比對是在**前驅**做的。這個方向的選擇有道理：前驅是資料的權威來源，讓它決定「該送什麼」比讓後繼猜「該要什麼」更不容易出錯；而且後繼此時可能狀態很糟（剛換過盤、資料半殘），不該讓它做決策。

`syncStart` 與 `getAllMetadataMap` 兩次列舉之後都各有一次「再查 chain version」（`src/storage/service/StorageOperator.cc:1040-1046`、`src/storage/sync/ResyncWorker.cc:190-198`）。因為列舉可能耗時很久（幾十萬個 chunk），期間 routing info 完全可能翻版，翻了就整輪作廢。

### 8.3 兩張表的正規化

前驅拿到 `remoteMetas`（vector）與 `localMetas`（`unordered_map<ChunkId, ChunkMetadata>`）。比對迴圈（`:203-275`）用 `SCOPE_EXIT { localMetas.erase(it); }` 邊比邊刪，**跑完之後 `localMetas` 裡剩下的就是「本地有、遠端沒有」的 chunk**，全部進 writeList（`:289-292`，計入 `syncingRemoteMissCount`）。

`remoteMeta` 在 `localMetas` 裡找不到 ⟹ 進 removeList（`:205-209`）：**遠端有、本地沒有的必須刪掉**。這是 resync 為什麼能收斂的關鍵——它不是「補齊差異」，而是「讓後繼的內容嚴格等於前驅」。

### 8.4 五維比對：`needForward` 的判定樹

`src/storage/sync/ResyncWorker.cc:222-269`，按順序判定：

```cpp
bool needForward = true;
if (meta.chainVer > remoteMeta.chainVer) {
  ++currentSyncingRemoteChainVersionLowCount;                       // ① 遠端鏈版本落後 → 送
} else if (remoteMeta.updateVer != remoteMeta.commitVer || remoteMeta.chunkState != ChunkState::COMMIT) {
  XLOGF(WARNING, "chain {} remote uncommitted {}", ...);            // ② 遠端未 commit → 送
  ++currentSyncingRemoteUncommittedCount;
} else if (meta.chainVer < remoteMeta.chainVer) {
  if (meta.chunkState == ChunkState::COMMIT) {
    XLOGF(DFATAL, "chain {} remote chain version high, ...");       // ③ 遠端比我新且我已 COMMIT → 不可能
    hasFatalEvents = true; break;
  } else { needForward = false; ++currentSyncingLocalUncommittedCount; XLOGF(CRITICAL, ...); }
} else if (meta.updateVer != remoteMeta.commitVer) {
  if (meta.chainVer != vChainId.chainVer && meta.chunkState == ChunkState::COMMIT) {
    XLOGF(DFATAL, "chain {} commit version mismatch, ...");         // ④ 版本不符且是舊鏈的已 commit 資料 → 不可能
    hasFatalEvents = true; break;
  } else { needForward = false; ++currentSyncingCurrentChainIsWritingCount; XLOGF(CRITICAL, ...); }
} else if (heavyFullSync) {
  ++currentSyncingRemoteFullSyncHeavyCount;                         // ⑤ 強制全量同步 → 送
} else if (meta.checksum() != remoteMeta.checksum) {
  if (meta.chainVer != vChainId.chainVer) {
    XLOGF(DFATAL, "chain {} checksum not equal, ...");              // ⑥ checksum 不符且是舊鏈資料 → 不可能
    ++currentSyncingRemoteFullSyncLightCount; hasFatalEvents = true; break;
  } else { needForward = false; ++currentSyncingCurrentChainIsWritingCount; XLOGF(CRITICAL, ...); }
} else {
  needForward = false;                                              // ⑦ 完全一致 → 跳過
}
```

整理成表：

| 維度 | 條件 | 結論 | 計數器 |
|---|---|---|---|
| ① chainVer | 本地 > 遠端 | **送** | `storage.syncing.chain_version_low` |
| ② 遠端 commit 狀態 | 遠端 `updateVer != commitVer` 或非 COMMIT | **送** | `storage.syncing.remote_uncommitted` |
| ③ chainVer | 本地 < 遠端，且本地已 COMMIT | **FATAL**，中止並踢掉後繼 | `storage.syncing.chain_version_high` |
| ③' | 本地 < 遠端，但本地未 COMMIT | 跳過（本地才是落後的那個） | `storage.syncing.local_uncommitted` |
| ④ 版本值 | `本地 updateVer != 遠端 commitVer`，且本地是**舊鏈**的已 COMMIT 資料 | **FATAL** | `storage.syncing.commit_version_mismatch` |
| ④' | 同上，但本地是**當前鏈**或未 COMMIT | 跳過（正在寫，稍後自然收斂） | `storage.syncing.current_chain_is_writing` |
| ⑤ 強制 | `full_sync_level == HEAVY` | **送** | `storage.syncing.full_sync_heavy` |
| ⑥ checksum | 不相等，且本地是**舊鏈**資料 | **FATAL** | `storage.syncing.full_sync_light` |
| ⑥' | 不相等，但本地是**當前鏈**資料 | 跳過（正在寫） | `storage.syncing.current_chain_is_writing` |
| ⑦ | 全部相同 | 跳過 | `storage.syncing.skip_count` |

判定樹的骨架是同一個問句反覆出現：**「`meta.chainVer != vChainId.chainVer`？」**

- 若 `meta.chainVer == vChainId.chainVer`，表示這個 chunk 在**當前鏈版本下**剛被寫過或正在寫，任何不一致都是「正在飛的寫入」造成的暫時現象，等它落地就好，跳過；
- 若 `meta.chainVer != vChainId.chainVer` 且 `chunkState == COMMIT`，表示這是**上一個鏈版本下已經達成共識的資料**，它與遠端的任何不一致都無法用併發解釋，只能是資料損毀或狀態機被破壞——`DFATAL`。

`hasFatalEvents` 的處置非常激烈（`:277-287`）：

```cpp
OfflineTargetReq req;
req.targetId = target->successor->targetInfo.targetId;
req.force = true;
CO_RETURN_AND_LOG_ON_ERROR(co_await components_.messenger.offlineTarget(*addrResult, req, &options));
```

**前驅直接對後繼發 `offlineTarget(force=true)`，把它踢下線。** 這是唯一一處 storage 之間有「權力關係」的地方。理由是：既然後繼的狀態已經無法用任何合法的執行序列解釋，繼續同步只會把污染擴大，不如讓它徹底離線，等人工介入或重建。

`full_sync_level` / `full_sync_chains` 是運維逃生口：懷疑某條鏈的資料有問題時，設成 `HEAVY` 就會忽略所有 checksum 相等的判定，把整條鏈的每一個 chunk 都重送一遍。

### 8.5 `forward()`：與線上寫入的互斥

`src/storage/sync/ResyncWorker.cc:389-458`。resync 的每一個 chunk 都要與線上寫入互斥，做法是**拿同一把 chunk 鎖**：

```cpp
folly::coro::Baton baton;
auto lockGuard = target->storageTarget->lockChunk(baton, chunkId, "sync");
if (!lockGuard.locked()) {
  XLOGF(WARNING, "target {} chunk {} wait lock, current tag: {}", *target, chunkId, lockGuard.currentTag());
  co_await lockGuard.lock();
}
```

tag 用字串 `"sync"`，所以日誌裡一眼能看出「線上寫在等 resync」還是反過來。

拿到鎖之後**重查一次 chunk 的當前狀態**（`:404-424`），處理兩種在等鎖期間發生的變化：

```cpp
auto chunkResult = target->storageTarget->queryChunk(chunkId);
if (chunkResult) {
  if (updateType == REMOVE && chunkResult->recycleState == RecycleState::NORMAL) {
    XLOGF(WARNING, "target {} chunk {} has been updated, skip remove", *target, chunkId);
    syncingSkipRemoveAfterUpdate.addSample(1);  co_return Void{};        // 本來要刪，但它又被寫回來了
  }
  chunkSize = chunkResult->innerFileId.chunkSize;                        // 用最新的 chunk size
} else if (chunkResult.error().code() == kChunkMetadataNotFound) {
  if (updateType == WRITE) {
    XLOGF(WARNING, "target {} chunk {} has been removed, skip updated", *target, chunkId);
    syncingSkipUpdateAfterRemove.addSample(1);  co_return Void{};        // 本來要送，但它被刪了
  }
}
```

比對是在**沒有鎖**的情況下用兩份快照做的（不可能對整個 target 上鎖），所以決策做完到真正執行之間必然有窗口。這兩個 skip 分支就是在窗口關閉時（拿到鎖後）做最後校正，對應 `storage.syncing.skip_remove_after_update` 與 `storage.syncing.skip_update_after_remove` 兩個計數器。

然後借一個 channel、組一個假的 `UpdateReq`：

```cpp
req.payload.updateVer = ChunkVer{1};              // ← 固定填 1
req.options.isSyncing = true;
req.options.commitChainVer = target->vChainId.chainVer;
req.payload.checksum.type = ChecksumType::CRC32C;
req.tag.clientId = clientId;                      // ← 由 vChainId + targetId 拼出來的偽 ClientId
```

**`updateVer` 填 1 是因為它會被 `doForward` 覆寫。** syncing 路徑上 `doForward` 會從本地讀出整塊 chunk 並把讀到的 `updateVer` 填進去（`src/storage/service/ReliableForwarding.cc:200`）；後繼的 `ChunkReplica::update` 在 `isSyncing` 分支直接照抄（`src/storage/store/ChunkReplica.cc:211-215`）：

```cpp
if (options.isSyncing) {
  meta.updateVer = writeIO.updateVer;
  meta.commitVer = ChunkVer{writeIO.updateVer - 1};    // ← 刻意設成 updateVer - 1
  meta.recycleState = RecycleState::NORMAL;
}
```

**同步寫入完全繞過版本檢查**（不做 stale/missing/advance 判定），因為它就是要「用前驅的版本強行覆蓋」。`commitVer = updateVer - 1` 讓 chunk 進入 CLEAN 狀態，之後由 commit 階段補上——維持了 §5.7 的 (I1)。

`clientId` 是偽造的（`:130-133`）：

```cpp
ClientId clientId{};
static_assert(sizeof(ClientId::uuid) == sizeof(VersionedChainId) + sizeof(TargetId));
*reinterpret_cast<VersionedChainId *>(clientId.uuid.data) = vChainId;
*reinterpret_cast<TargetId *>(clientId.uuid.data + sizeof(VersionedChainId)) = targetId;
```

把 `(chainId, chainVer, targetId)` 塞進 16 byte 的 UUID。這讓 `ReliableUpdate` 的去重表天然按「哪一輪同步」分區：**鏈版本一變，clientId 就變，舊的去重記錄自動失效**，不會有跨輪次的誤命中。同時 `static_assert` 把「三個欄位加起來剛好 16 byte」這件事釘在編譯期。

`forwardWithRetry(..., allowOutdatedChainVer = false)`（`:453`）是 resync 與線上寫的另一處差異：線上寫允許用稍舊的鏈版本重試（因為客戶端在等），resync 不允許——鏈版本一變就直接放棄這一輪，等下一輪重來。

---

## 9. 磁碟、target 與 chunk 空間管理

### 9.1 一台機器上的目錄佈局

```
配置 target_paths = ["/storage/data0", "/storage/data1", ...]     ← 每個元素是一顆盤的掛載點
                          │
                    diskIndex = 0, 1, 2, ...                      ← 就是陣列下標

/storage/data0/                                    ← 掛載點
├── .hf3fs_check                                   ← CheckWorker 每 3 秒寫一次，測唯讀
├── engine/                                        ← 新 chunk engine 的資料（整顆盤共用一個）
│   └── ...（RocksDB meta + 大檔案 cluster）
├── 101000100000000/                               ← 一個 target，目錄名 = targetId
│   ├── target.toml                                ← PhysicalConfig
│   ├── meta/                                      ← LevelDB / RocksDB（chunk metadata）
│   ├── 512KB/  00 01 02 ... FF                    ← 256 個物理檔（physical_file_count）
│   ├── 1MB/    00 01 02 ... FF
│   ├── 2MB/    ...
│   ├── 4MB/
│   ├── 16MB/
│   └── 64MB/
└── 101000200000000/
    └── ...
```

映射關係由三處程式碼確立：

```cpp
// 建立時：diskIndex = idx / targetNumPerPath   （StorageTargets.cc:103）
auto diskIndex = idx / targetNumPerPath;
targetConfig.path = targetPaths[diskIndex] / std::to_string(targetId);

// 載入時：由父目錄反查（StorageTargets.cc:224-232）
auto diskPath = targetPath.parent_path();
if (UNLIKELY(!pathToDiskIndex_.contains(diskPath))) { ... kStorageInitFailed }
auto diskIndex = pathToDiskIndex_[diskPath];

// 物理檔路徑（ChunkFileStore.cc:111）
Path filePath = path_ / Size::toString(fileId.chunkSize) / fmt::format("{:02X}", fileId.chunkIdx);
```

`loadTarget` 還會交叉驗證目錄名與 `target.toml` 裡的 targetId 一致（`src/storage/store/StorageTargets.cc:240-244`），以及 `target.toml` 裡的 `path` 與實際路徑一致（`src/storage/store/StorageTarget.cc:165-169`）——防止有人 `mv` 了目錄。

**磁碟 UUID 綁定**（`src/storage/store/StorageTarget.cc:39-56`、`:171-183`）是最強的一道防呆：

```cpp
Result<std::string> getDeviceUUID(const Path &path) {
  struct stat st;  ::stat(path.c_str(), &st);
  auto getDeviceUUIDResult = SysResource::fileSystemUUID();
  if (!getDeviceUUIDResult->count(st.st_dev)) { ... kStorageUUIDMismatch }
  return getDeviceUUIDResult->at(st.st_dev);
}
// load 時：
if (targetConfig_.block_device_uuid != *getDeviceUUIDResult) { ... kStorageUUIDMismatch }
```

target 建立時把所在區塊裝置的檔案系統 UUID 寫進 `target.toml`，之後每次載入都比對。**換盤、換插槽、掛載點漂移都會被立刻抓到**——因為 3FS 的 target 是有身分的，一顆盤上的資料不能被當成另一顆盤的。`allow_disk_without_uuid` 是給沒有 UUID 的環境（容器 overlayfs、測試）用的逃生口。

### 9.2 `GlobalFileStore`：每個檔案兩個 fd

`src/storage/store/GlobalFileStore.cc:11-49`：

```cpp
{ auto flags = O_RDWR | O_SYNC;   file.normal_ = ::open(...); }   // 有 page cache，但 O_SYNC
{ auto flags = O_RDWR | O_DIRECT; file.direct_ = ::open(...); }   // 繞過 page cache
```

**同一個檔案同時開兩個 fd**，用途分工：

| fd | 用在哪 | 為什麼 |
|---|---|---|
| `direct_` | 所有 AIO/io_uring 讀（`state.readFd = chunkInfo.view.directFD()`）；對齊的寫；`fallocate`（打洞、預配） | 讀路徑要零拷貝進 RDMA buffer，必須 `O_DIRECT` |
| `normal_` | 非對齊的寫 | `O_DIRECT` 要求偏移/長度/位址三對齊，非對齊寫只能退回 buffered + `O_SYNC` |

選擇邏輯在 `ChunkFileView::write()`（`src/storage/store/ChunkFileView.cc:55-61`）：

```cpp
int fd = normal_;
if (size % kAIOAlignSize == 0 && offset % kAIOAlignSize == 0 && reinterpret_cast<uint64_t>(buf) % kAIOAlignSize == 0) {
  fd = direct_;
  storageWriteDirect.addSample(1);
}
```

`storage.pwrite.direct` 這個計數器除以 `storage.pwrite` 的總次數，就是 direct 寫的比例——這是判斷客戶端 IO 模式是否「對齊友善」的直接指標。

`ChunkFileView::write()` 還有一個容易忽略的行為（`:63-87`）：**寫失敗會指數退避重試最多 30 秒**（`ExponentialBackoffRetry retry(100_ms, 5_s, 30_s)`），而不是立刻報錯。理由是 NVMe 偶發的 `EIO` 常常是瞬時的（韌體 GC、佇列滿），重試 30 秒能吃掉大部分抖動；真的失敗才回 `kChunkWriteFailed`，觸發整顆盤下線。

`tlsCache_`（`src/storage/store/ChunkFileStore.h:62`）是一層 thread-local 的 `ChunkFileId → FileDescriptor*` 快取，避開 `GlobalFileStore` 的 256 路分片鎖。因為 fd 一旦開啟就不會關閉（直到 `clear()`），指標永遠有效。

### 9.3 `ChunkMetaStore` 的 key 空間

`src/storage/store/ChunkMetaStore.cc:30-55` 用一個 `MetaKeyType : uint8_t` 當第一個 byte：

| type | 值 | key 格式 | value | 用途 |
|---|---|---|---|---|
| `METADATA` | 0 | `0x00` + **~chunkId** | serde(ChunkMetadata) | chunk 主表 |
| `FREECHUNK` | 1 | — | — | **已廢棄** |
| `FILESIZE` | 2 | serde(ChunkFileId) + `0x02` | u64 | 每個物理檔已配置到多大 |
| `LASTSERVE` | 3 | — | — | **已廢棄** |
| `CREATEDSIZE` | 4 | `0x04` | u64 | 累計建立位元組數 |
| `REMOVEDSIZE` | 5 | `0x05` | u64 | 累計刪除位元組數 |
| `UNCOMMITTED` | 6 | `0x06` + ~chunkId | 空 | 未 commit 的 chunk 索引 |
| `UNRECYCLED` | 7 | — | — | **已廢棄** |
| `SYNCDUMMY` | 8 | `0x08` | 空 | `sync()` 用的假寫入 |
| `REMOVED` | 12 | `chunkSize` + **BE(microsecond)** + pos + `0x0C` | 空 | 已刪待回收（按時間排序） |
| `RECYCLED` | 13 | `chunkSize` + pos + `0x0D` | 空 | 已回收可重用 |
| `CREATED` | 14 | `chunkSize` + pos + `0x0E` | 空 | 已預配尚未使用 |
| `HOLE` | 15 | — | — | 只有計數 |
| `*COUNT` | 16-21 | `chunkSize` + type | u64 | 六個計數器 |
| `ALLOCATEINDEX` | 22 | `chunkSize` + `0x16` | u32 | 輪到哪個物理檔預配 |
| `ALLOCATESTART` | 23 | `chunkSize` + `0x17` | u64 | 起始點（見 §9.5） |
| `VERSION` | 24 | `chunkSize` + `0x18` | u32 | schema 版本，目前 1 |
| `MAX` | 0xFF | `0xFF` | sentinel | 哨兵 |

三個編碼技巧：

**(a) chunkId 存反相位元。** `ChunkKey` 對每個 byte 做 `~ch`（`src/storage/store/ChunkMetaStore.cc:101-106`）：

```cpp
static void reverseBits(std::string_view view, std::string &out) {
  for (auto ch : view) { out.push_back(~ch); }
}
```

於是 KV 裡的字典序等於 chunkId 的**反向**字典序。`ChunkStore::queryChunks()` 因此可以從 `range.end` 開始 seek，用**前向迭代**掃出「從大到小」的結果（`src/storage/store/ChunkStore.cc:120-147`）：

```cpp
auto it = metaStore_.iterator(chunkIdRange.end.data());
for (; it->valid() && chunkIds.size() < maxNum; it->next()) {
  if (chunkId == chunkIdRange.end) continue;      // [begin, end)
  if (chunkId < chunkIdRange.begin) break;
  chunkIds.emplace_back(chunkId, *metadata);
}
```

`ChunkStore::queryChunks` 的註解直接寫著「the chunk ids in result are in reverse lexicographical order」。為什麼要倒序？因為最主要的使用者是 `queryLastChunk`——**找檔案的最後一個 chunk**。倒序掃第一個就是答案。LevelDB/RocksDB 的反向迭代器（`Prev()`）比正向慢很多，用 key 編碼把它轉成正向掃描是純賺。

**(b) sentinel 雙哨兵。** `create()` 時在 `kMinKey`(`0x00 0x00`) 與 `kMaxKey`(`0xFF 0x00`) 各寫一份 `"3fs-vnext storage target {target_id}"`（`:189-192`），`load()` 時兩頭都驗（`:215-218`）：

```cpp
Result<Void> ChunkMetaStore::checkSentinel(std::string_view key) {
  auto result = kv_->get(key);
  if (*result != sentinel_) { reportFatalEvent(); XLOG(DFATAL, msg); return makeError(kMetaStoreOpenFailed, msg); }
}
```

一頭一尾各放一個，是為了同時偵測「開錯 DB」與「DB 被截斷」。sentinel 內容含 targetId，所以連「開到同一台機器上另一個 target 的 DB」都會被抓到。

**(c) `RemovedKey` 的時間戳用 big-endian。** `SERDE_STRUCT_FIELD(microsecond, serde::BigEndian<uint64_t>{})`（`:160`）——因為 `REMOVED` 這一區要按**刪除時間由早到晚**掃描（`recycleRemovedChunks` 每次只回收最舊的一批），big-endian 讓時間戳的字典序等於數值序。這與 §3.1 的 chunkId 是同一個動機。

### 9.4 chunk 空間的三級管線

一個 chunk 的物理空間在四個狀態之間流轉：

```
       allocateChunks()                 createChunk()
   ┌──────────────────────┐         ┌────────────────────┐
   │ fallocate 一大段空間  │────────▶│  CREATED（已預配）  │──────┐
   │  allocate_size=256MB │         └────────────────────┘      │
   └──────────────────────┘                                     │  用掉
              ▲                                                 ▼
              │ createdChunks 見底                        ┌────────────┐
              │                                           │  使用中     │
              │                                           └─────┬──────┘
   ┌──────────┴───────────┐                                     │ remove()
   │  RECYCLED（可重用）   │◀────────────────────────┐           ▼
   └──────────┬───────────┘   recycleRemovedChunks() │    ┌──────────────┐
              │ createChunk 優先取這裡                 └────│   REMOVED     │
              │                                            │（按時間排序） │
              └────────────────────────────────────────    └──────┬───────┘
                                                                  │ punchHoleRemovedChunks()
                                                                  ▼
                                                        FALLOC_FL_PUNCH_HOLE
                                                        空間還給檔案系統
```

**`createChunk()` 的優先序**（`src/storage/store/ChunkMetaStore.cc:427-535`）：先用 RECYCLED，沒有才用 CREATED，兩者都沒有就同步觸發預配。同步/非同步的切換很精巧：

```cpp
if (needRecycleRemovedChunks(state)) {
  if (!state.recycling.exchange(true)) { executor.add([this, &state] { recycleRemovedChunks(state); }); }  // 非同步
  if (state.recycledChunks.empty()) {                                    // 但真的沒貨了
    lock.unlock();
    auto recycleLock = std::unique_lock(state.recycleMutex);             // 換一把鎖同步做
    lock.lock();
    if (state.recycledChunks.empty()) { RETURN_AND_LOG_ON_ERROR(recycleRemovedChunks(state, true)); }
  }
}
...
if (state.createdChunks.size() * state.chunkSize <= config_.allocate_size() / 2) {
  if (!state.allocating.exchange(true)) { executor.add([this, &state] { allocateChunks(state); }); }        // 水位低於一半就預配
}
if (state.createdChunks.empty()) { ...同樣的 unlock/取 allocateMutex/lock/重檢查... }
```

**水位低於 `allocate_size / 2`（128MB）就非同步補充，真的見底才同步阻塞。** `executor` 是 `UpdateWorker` 的 `bgExecutors_`（8 執行緒，名字叫 `"Recycle"`，`src/storage/update/UpdateWorker.h:23-24`），與寫入執行緒分離。

三把鎖的分工（`src/storage/store/ChunkMetaStore.h:110-112`）：`createMutex` 保護兩個 vector 與計數器、`recycleMutex` 序列化回收、`allocateMutex` 序列化預配。`lock.unlock() → 取另一把 → lock.lock() → 重新檢查` 這個模式出現兩次，是標準的雙重檢查鎖：不能握著 `createMutex` 去做慢操作（會擋住所有並發的 createChunk），但放掉之後必須重新確認條件仍成立。

**`needRecycleRemovedChunks()` 的兩條觸發線**（`:917-933`）：

```cpp
if (state.recycling) return false;
if (state.recycledChunks.size() >= config_.recycle_batch_size() * 0.6) return false;   // 存貨還夠
const bool doRecycle =
    recycledAndHoleCount + config_.recycle_batch_size() <= removedCount ||             // 積壓夠一批（256 個）
    (recycledAndHoleCount < removedCount &&
     state.oldestRemovedTimestamp.load() + config_.removed_chunk_force_recycled_time() < UtcClock::now());
                                                                                       // 或最舊的等超過 1 小時
```

「積壓夠一批」是吞吐優化（批次 KV 寫），「等超過 1 小時」是延遲保底（避免低負載時空間長期不回收）。

**打洞是獨立的第三級**（`punchHoleRemovedChunks`，`:1009-1053`），由 `PunchHoleWorker` 每 10 秒驅動，只處理**刪除超過 `removed_chunk_expiration_time`（3 天）**的 chunk：

```cpp
auto expirationUs = UtcClock::now().toMicroseconds() - config_.removed_chunk_expiration_time().asUs().count();
if (emergencyRecycling_) { expirationUs = UtcClock::now().toMicroseconds(); }    // 緊急模式：全部立刻打洞
```

為什麼刪除後要等 3 天才真的把空間還給檔案系統？因為 RECYCLED 的空間是**可以原地重用**的（不需要 `fallocate`，寫入時直接覆蓋），而打洞之後再寫就要重新配置，還會製造檔案系統層的碎片。3 天的窗口讓「刪了又建」的工作負載能命中重用路徑。

`emergencyRecycling_` 由 `CheckWorker` 在磁碟使用率超過 `emergency_recycling_ratio`（0.95）時打開（`src/storage/worker/CheckWorker.cc:213`），此時放棄重用優化，立刻把所有空間吐回去。

### 9.5 `startingPoint`：一個 schema 遷移的痕跡

`loadAllocateState()`（`src/storage/store/ChunkMetaStore.cc:694-718`）：

```cpp
result = kv_->get(allocateStartKey);
if (LIKELY(bool(result))) { ...直接讀... }
else if (result.error().code() == StatusCode::kKVStoreNotFound) {
  uint64_t fileSize{};  // 讀 0 號物理檔的大小
  ...
  auto allocateSize = config_.allocate_size();
  state.startingPoint = (fileSize + allocateSize - 1) / allocateSize * allocateSize;
  RETURN_AND_LOG_ON_ERROR(kv_->put(allocateStartKey, serde::serializeBytes(state.startingPoint.load())));
}
```

`startingPoint` 標記「新版分配器接管的起點」。它的用途在 `remove()`（`:389-403`）：

```cpp
const bool doPunchHole = meta.innerOffset < state.startingPoint.load();
if (doPunchHole) {
  RETURN_AND_LOG_ON_ERROR(fileStore_.punchHole(meta.innerFileId, meta.innerOffset));   // 舊區域：直接打洞
} else {
  ...插入 RemovedKey，走正常回收流程...                                                 // 新區域：進回收管線
}
```

**起點以下的 chunk 是舊版分配器留下的，刪除時直接打洞不進回收池**；起點以上才走新的三級管線。這樣新舊兩套分配策略可以在同一個 target 上共存，不需要停機遷移。`kTargetVersion = 1` 的升級邏輯（`:720-770`）也只在 `startingPoint > 0`（即確實有舊資料）時才去重掃 REMOVED/RECYCLED 修正計數。

### 9.6 空間統計的三個數字

`unusedSize()`（`src/storage/store/ChunkMetaStore.cc:565-585`）：

```cpp
const int64_t reservedCount   = max(0, createdCount - usedCount) + max(0, recycledCount - reusedCount);
const int64_t unrecycledCount = max(0, removedCount - recycledCount - holeCount);
reservedSize   += chunkSize * reservedCount;      // 已預配但沒人用 → 可以馬上用
unrecycledSize += chunkSize * unrecycledCount;    // 已刪但還沒回收 → 遲早能用
```

`usedSize()` 則是 `createdSize_ - removedSize_`（`:71`）。三者的關係反映在 `spaceInfos()` 回報的 `free`（`src/storage/store/StorageTargets.cc:287`）：

```cpp
info.free = spaceInfo.free + diskUnusedSize[info.path] + usedSize.reserved_size;
//          ↑ 檔案系統實際空閒   ↑ 舊引擎的 reserved+unrecycled   ↑ 新引擎的 reserved
```

也就是說 storage 對外宣稱的「空閒空間」= 檔案系統空閒 + 自己佔著但隨時能還的。這讓 mgmtd 的容量規劃不會被「預配了但沒用」誤導。`spaceInfos()` 有 5 秒快取（`space_info_cache_timeout`），`SpaceInfoReq.force` 可以強制刷新。

<!--PART5-->




---

## 10. 背景 worker 群（`src/storage/worker/`）

storage server 除了處理 RPC，還跑五個獨立的背景執行緒。它們**形態高度一致**，讀懂一個就懂全部：

```cpp
class XxxWorker {
  Result<Void> start();
  Result<Void> stopAndJoin();
 private:
  void loop();                        // 專屬執行緒的主迴圈
  std::atomic<bool> stopping_ = false;
  std::atomic<bool> started_ = false;
  std::atomic<bool> stopped_ = false;
};
```

三個原子旗標的分工：`stopping_` 是「請你停」的請求、`started_` / `stopped_` 是實際狀態。`stopAndJoin` 的等待迴圈也是同一個模板（以 `PunchHoleWorker.cc:29-31` 為例）：

```cpp
for (int i = 0; started_ && !stopped_; ++i) {
  XLOGF_IF(INFO, i % 5 == 0, "Waiting for PunchHoleWorker@{}::loop stop...", fmt::ptr(this));
  std::this_thread::sleep_for(100_ms);
}
```

每 500ms 印一次等待訊息（`i % 5` 配 100ms 睡眠），帶上 `this` 指標以便多實例時區分。這種「輪詢 + 週期性日誌」而非條件變數的寫法，是為了讓**停不下來的 worker 在日誌裡留下痕跡**——直接 join 會靜默地卡住。

各 worker 的主迴圈統一用 `cond_.wait_for(lock, 100_ms, [&]{ return stopping_.load(); })` 睡眠（`AllocateWorker.cc:39`），這樣停止請求能立刻喚醒，不必等滿一個週期。

### 10.1 `AllocateWorker`：預配置空間

`worker/AllocateWorker.h:17-21`：

| 設定項 | 預設 | 意義 |
|---|---|---|
| `min_remain_groups` | 4 | 一般 group 的低水位 |
| `max_remain_groups` | 8 | 一般 group 的高水位 |
| `min_remain_ultra_groups` | 0 | 大 chunk（>4MiB）group 的低水位 |
| `max_remain_ultra_groups` | 4 | 大 chunk group 的高水位 |
| `max_reserved_chunks` | 1 GB | compact 的保留上限 |

迴圈每 100ms 做三件事（`AllocateWorker.cc:49-51`）：

```cpp
engine->allocate_groups(minRemainGroups, maxRemainGroups, 128);
engine->allocate_ultra_groups(minRemainUltraGroups, maxRemainUltraGroups, 32);
engine->compact_groups(maxReserved);
```

**雙水位預配置**：低於 `min` 就補到 `max`。這讓寫入路徑上的 `allocate` 幾乎永遠命中已備好的 group，不必在請求路徑上做 `fallocate`（那是要碰檔案系統的慢操作）。

一般 group 與 ultra group（>4MiB）分開管理，且 ultra 的低水位預設是 **0**——大 chunk 不預配置，用到才配。因為大 chunk 佔空間，預配置的浪費遠大於省下的延遲；而且大 IO 本身耗時較長，多一次 `fallocate` 的相對成本可忽略。

第三個參數（128 / 32）是單次批量上限，同樣是「大的配少一點」。

`compact_groups` 回收碎片化的 group，`max_reserved_chunks` 限制它保留多少空閒 chunk 不還給檔案系統——留一些避免反覆 allocate/free 抖動。

### 10.2 `CheckWorker`：磁碟健康與空間水位

`worker/CheckWorker.h:18-21`：

| 設定項 | 預設 | 意義 |
|---|---|---|
| `update_target_size_interval` | 10 s | 更新 target 大小的週期 |
| `emergency_recycling_ratio` | 0.95 | 超過此使用率啟動緊急回收 |
| `disk_low_space_threshold` | 0.96 | 標記磁碟空間不足 |
| `disk_reject_create_chunk_threshold` | 0.98 | **拒絕建立新 chunk** |

三個門檻構成漸進式的空間保護：

```
使用率  0.95 ────────► 緊急回收（主動 punch hole 釋放空間）
        0.96 ────────► 標記 low space（回報給 mgmtd）
        0.98 ────────► 拒絕 create chunk（寫入開始失敗）
        1.00 ────────► 磁碟滿
```

`start()` 接受 `targetPaths` 與 `manufacturers`（`:27`）——後者是磁碟廠商字串，用於針對不同型號套用不同的健康檢查邏輯。

### 10.3 `PunchHoleWorker`：空間回收

沒有 `Config` 類別，用固定節奏。`PunchHoleWorker.cc:49-78` 的迴圈：

```cpp
// 1. recycle all targets.
for (auto &[targetId, target] : targetMap->getTargets()) { ... }
...
for (auto &weakTarget : targets) {
  for (auto i = 0u; i < 128u && !stopping_; ++i) {
    auto result = target->punchHole();
    if (...) XLOGF(ERR, "recycle target {} failed: {}", target->path(), result.error());
  }
}
```

每個 target 每輪最多 punch 128 次，且**每次迴圈都檢查 `stopping_`**——`punch_hole` 是同步的檔案系統操作，一次做太多會讓停機卡住好幾秒。128 這個上限是在「回收效率」與「停機響應性」之間取的平衡。

先收集 `weakTarget` 再處理（而非直接在 map 迭代中操作），避免長時間持有 target map 的鎖。

### 10.4 `SyncMetaKvWorker`：定期落盤 chunk metadata

`worker/SyncMetaKvWorker.h:17` 只有一個設定：`sync_meta_kv_interval`，預設 1 分鐘。

`SyncMetaKvWorker.cc:58-62`：

```cpp
void SyncMetaKvWorker::syncAllMetaKVs() {
  auto snapshot = components_.targetMap.snapshot();
  for (auto &[targetId, target] : snapshot->getTargets()) {
    if (target.localState == flat::LocalTargetState::OFFLINE || target.storageTarget == nullptr) continue;
    ...
```

把各 target 的 metadata KV（RocksDB）強制 sync 到磁碟。跳過 OFFLINE 與空 target。

這是**耐久性與效能的取捨點**：平時寫入不強制 fsync metadata（靠 RocksDB 的 WAL），每分鐘統一 sync 一次。斷電最多損失一分鐘的 metadata 更新——但這不會導致資料錯誤，因為 chunk 資料本身與 CRAQ 的版本號可以在重啟後從 RocksDB WAL 恢復並與鄰居比對修正（見 §8 resync）。

用 `targetMap.snapshot()` 而非直接迭代，是這份程式碼一貫的無鎖讀取手法。

### 10.5 `DumpWorker`：狀態轉存與自動 profiling

`worker/DumpWorker.h:18-20`：

| 設定項 | 預設 | 意義 |
|---|---|---|
| `dump_root_path` | 空 | 轉存目錄，空則不啟用 |
| `dump_interval` | 1 天 | 轉存週期 |
| `high_cpu_usage_threshold` | 100 | **CPU 核心數門檻**（注意單位是核心數不是百分比） |

它做兩件不相干的事。第一是每天把所有 target 的狀態轉存一次（`DumpWorker.cc:83-86`）。

第二件更有意思——**CPU 用量觸發的自動效能剖析**（`DumpWorker.cc:3, 18, 68-80`）：

```cpp
#include <gperftools/profiler.h>
...
monitor::ValueRecorder cpuCores{"storage.sys.cpu_cores", std::nullopt, true};
...
cpuCores.set(cores);
if (!profiler && cores >= config_.high_cpu_usage_threshold()) {
  profiler = true;
  ...
  profilerStart(config_.dump_root_path());
}
...
if (profiler && RelativeTime::now() - lastProfilerTime >= 1_min) {
  ...
  profiler = false;
}
```

當進程佔用的 CPU 核心數達到門檻（預設 100 核），**自動啟動 gperftools profiler 一分鐘後自動停止**。

這是「現場證據自動保全」的設計：CPU 尖峰往往轉瞬即逝，等人接到告警登入機器時現場已經沒了。與其事後重現，不如在異常發生的當下自動抓一份 profile 落地。門檻設在 100 核意味著這是為大規格機器準備的——只有真正失控的忙碌才會觸發，不會在正常負載下反覆產生 profile 檔案。

`cpuCores` 同時被記成一個 `ValueRecorder` 指標，所以監控上也看得到這條曲線（見 `monitor_collector_main` 報告）。

---

## 11. AIO 層與 RDMA 緩衝池

### 11.1 兩種 IO 引擎並存

`aio/AioReadWorker.h:20-34` 定義了可切換的 IO 引擎：

```cpp
enum class IoEngine {
  libaio,
  io_uring,
  random,     // 逐批隨機選一個引擎（見 §6.3）
};

class Config : public ConfigBase<Config> {
  CONFIG_ITEM(num_threads, 32ul);
  CONFIG_ITEM(queue_size, 4096u);
  CONFIG_ITEM(max_events, 512u);
  CONFIG_ITEM(enable_io_uring, true);
  CONFIG_HOT_UPDATED_ITEM(min_complete, 128u);
  CONFIG_HOT_UPDATED_ITEM(wait_all_inflight, false);      // deprecated.
  CONFIG_HOT_UPDATED_ITEM(inflight_control_offset, 128);  // deprecated.
  CONFIG_HOT_UPDATED_ITEM(ioengine, IoEngine::libaio);
};
```

值得注意的是 **`ioengine` 預設是 `libaio` 而非 `io_uring`**，儘管 `enable_io_uring` 預設為 true（那只是「允許」，不是「使用」）。兩者的互動由 `useIoUring()` 決定（`src/storage/aio/AioReadWorker.h:37-49`）——它是查詢函式而非驗證器，**不會拒絕任何設定值**：

```cpp
inline bool useIoUring() const {
  if (!enable_io_uring()) { return false; }        // ← 靜默退回 libaio
  switch (ioengine()) {
    case IoEngine::io_uring: return true;
    case IoEngine::libaio:   return false;
    case IoEngine::random:   return folly::Random::rand32() & 1;
  }
}
```

所以 `enable_io_uring=false` 搭配 `ioengine='io_uring'` 是**合法設定，只是靜默降級**，不會有任何錯誤或警告提示這個組合自相矛盾。

保留 libaio 作為預設，反映了 io_uring 在不同核心版本上的行為差異——它是可熱更新的設定項，代表可以在線上逐台切換觀察。兩個標記 `// deprecated.` 的設定項則是演進留下的痕跡。

`aio/AioStatus.h:12-70` 用虛介面統一兩者：

```cpp
class IoStatus {
  virtual void submit() = 0;
  virtual void reap(uint32_t minCompleteIn) = 0;
};

class AioStatus : public IoStatus {        // libaio
  std::vector<struct iocb> iocbs_;
  std::vector<struct iocb *> availables_;
  std::vector<struct io_event> events_;
};

class IoUringStatus : public IoStatus {    // io_uring
  Result<Void> init(uint32_t maxEvents, const std::vector<int> &fds, const std::vector<struct iovec> &iovecs);
  struct io_uring ring_{};
  std::vector<AioReadJob *> submittingJobs_;
};
```

介面只有 `submit` / `reap` 兩個動詞——提交與收割，這是所有非同步 IO 引擎的共同抽象。

**關鍵差異在 `init` 的簽章**：`IoUringStatus::init` 接受 `fds` 與 `iovecs`（`AioStatus.h:60`），而 `AioReadWorker::start` 也是（`AioReadWorker.h:61`）：

```cpp
Result<Void> start(const std::vector<int> &fds, const std::vector<struct iovec> &iovecs);
```

這兩份陣列就是 io_uring 的 **fixed files 與 fixed buffers 註冊**。預先把所有 chunk 檔案的 fd 與所有 RDMA 緩衝區註冊給核心，之後每次 IO 只傳索引而非 fd／指標——省下每次 IO 的 fd 查表與記憶體頁鎖定。這是 io_uring 相對 libaio 的主要優勢來源，libaio 沒有對應機制。

`min_complete` 預設 128：每次 `reap` 至少等 128 個完成才返回。這是**吞吐優先**的設定——批次收割攤薄系統呼叫成本，代價是單一請求的延遲會被拖到同批最慢者。

### 11.2 `BufferPool`：雙尺寸 RDMA 緩衝池

`service/BufferPool.h:17-27`：

```cpp
struct ... {
  uint32_t registerIndex;
  net::RDMABuf buffer;
};

class BufferPool {
  class Config : public ConfigBase<Config> {
    CONFIG_ITEM(rdmabuf_size, 4_MB);
    CONFIG_ITEM(rdmabuf_count, 1024u);
    CONFIG_ITEM(big_rdmabuf_size, 64_MB);
    CONFIG_ITEM(big_rdmabuf_count, 64u);
  };
  auto &iovecs() const { return iovecs_; }
  std::vector<net::RDMABuf> buffers_;
  std::vector<struct iovec> iovecs_;
};
```

兩級尺寸：

| 級別 | 單塊大小 | 數量 | 總量 |
|---|---|---|---|
| 一般 | 4 MiB | 1024 | 4 GiB |
| 大 | 64 MiB | 64 | 4 GiB |

共 8 GiB 常駐記憶體，全部在啟動時 `ibv_reg_mr` 註冊（這就是 `deploy/systemd/storage_main.service:8` 需要 `LimitMEMLOCK=infinity` 的原因）。

`registerIndex` 欄位是連接 RDMA 與 io_uring 的橋樑——**同一塊記憶體同時是 RDMA 的註冊區與 io_uring 的 fixed buffer**。`iovecs()` 回傳的陣列正是餵給 `AioReadWorker::start` 的那份。於是讀取路徑成為：

```
io_uring 直接讀進 fixed buffer（DMA，零拷貝）
        │  同一塊記憶體
        ▼
RDMA Write 直接把它推給 client（零拷貝）
```

全程資料**沒有經過任何一次 memcpy**。`Buffer::index()`（`:53`）取的 `indices_.back().registerIndex` 就是在兩套機制之間傳遞的那個索引。

雙尺寸的理由是碎片與浪費的權衡：小 IO 用 4MiB 塊避免浪費，大 IO（例如 resync 整塊搬移）用 64MiB 塊避免拆分成十幾個請求。

---

## 12. chunk engine 的 C++ 側橋接（`store/ChunkEngine.h`）

Rust 引擎的內部設計見 `chunk_engine` 報告，此處只講 C++ 這一側怎麼用它。

### 12.1 型別轉換

`store/ChunkEngine.h:30-31` 是所有字串參數的統一入口：

```cpp
static rust::Slice<const uint8_t> toSlice(const std::string &key) {
  return rust::Slice<const uint8_t>{(const uint8_t *)key.data(), key.size()};
}
```

`rust::Slice` 是 cxx 提供的「指標 + 長度」對，跨語言傳遞時 **Rust 側不會複製**——這正是 `chunk_engine` 報告 §13.1 指出的信任邊界：`std::string` 的生命週期必須涵蓋整個呼叫。

`ChainId` 的傳遞更直接（`:139`、`:159`）：

```cpp
rust::Slice<const uint8_t> prefix{(const uint8_t *)&chainId, sizeof(chainId)};
```

直接把 `ChainId` 的記憶體表示當成 byte 前綴用於 RocksDB 的範圍查詢。這隱含要求 `ChainId` 的位元組序必須與 Rust 側的 key 排序一致。

### 12.2 版本欄位的映射

`ChunkEngine.h:15-16` 與 `:61-63` 出現同一個模式：

```cpp
out.commitVer = ChunkVer{in.chunk_ver};
out.updateVer = ChunkVer{in.chunk_ver};
```

**Rust 側只有一個 `chunk_ver`，`copyMeta` 把 C++ 的 `commitVer` 與 `updateVer` 都填成它。** 但這只是 `copyMeta` 單獨使用時的行為，**不代表引擎不持久化未提交狀態**——`ChunkMeta` 有一個會落盤的 `uncommitted: bool` 欄位（`src/storage/chunk_engine/src/types/chunk_meta.rs:18`），並經 bridge 以 `chunk_uncommitted(i)` 暴露。

resync 依賴的兩條路徑會讀它並還原出 CLEAN 狀態（`src/storage/store/ChunkEngine.h:221-224`，`getAllMetadataMap` 於 `:247-250` 同理）：

```cpp
if (chunks->chunk_uncommitted(i)) {
  out.commitVer = ChunkVer{out.commitVer - 1};
  out.chunkState = ChunkState::CLEAN;
}
```

也就是說 pending 狀態（`updateVer > commitVer`）**是落盤的**，重啟後可以還原——這正是 §8 的 resync 能在重啟後正確比對五個維度的前提。

`:63` 額外映射了 `commitChainVer = ChainVer{meta.chain_ver}`，這是 resync 時五維比對的欄位之一（見 §8）。

### 12.3 主要操作

| 方法 | 對應 Rust | 說明 |
|---|---|---|
| `get_raw_chunk(key, error)`（`:47`、`:86`） | `Arc::into_raw` | 取得 chunk 的裸指標，**必須配對 `release_raw_chunk`**（`:98`） |
| `update(engine, job)`（`:75`） | — | 寫入，回傳新版本號 |
| `commit(engine, job, sync)`（`:77`） | `Box::from_raw` 取回所有權 | 提交；`sync` 控制是否強制落盤 |
| `query_raw_chunks(begin, end, max, error)`（`:119`） | 範圍查詢 | 供掃描與遷移使用，`maxNumChunkIdsToProcess` 限制單次回傳量 |
| `queryUncommittedChunks(engine, chainId)`（`:138-142`） | 前綴查詢 | 重啟後找出未提交的寫入 |
| `resetUncommittedChunks(engine, chainId, chainVer)`（`:158-162`） | `handle_uncommitted_raw_chunks` | 把該 chain 所有未提交寫入的 `chain_ver` 蓋成傳入值，然後**全部 commit** |

最後兩個方法是**崩潰復原的核心**：storage 重啟後，先用 `queryUncommittedChunks` 查出該 chain 所有未提交的 chunk，再用 `resetUncommittedChunks` 處理它們。

要注意 Rust 側**沒有任何丟棄分支**（`src/storage/chunk_engine/src/core/engine.rs:600-616`）：

```rust
for (chunk_id, holder) in writing_list.iter_mut() {
    holder.chunk.set_chain_ver(chain_ver);   // ← 無條件蓋上新的 chain_ver
    ...
}
self.commit_chunks(chunks, true)?;           // ← 然後全部提交
```

它把所有未提交寫入一律**蓋上當前 chainVer 並提交**，不做「這筆該不該保留」的判定。要不要接受這些資料，是由上層的 CRAQ 協定決定的——提交後它們帶著新的 `chainVer`，resync 時會與前驅比對五個維度（§8），對不上的由前驅 PUSH 正確內容覆蓋。**判定權在鏈的協定層，不在儲存引擎。**

`get_raw_chunk` / `release_raw_chunk` 的配對由 C++ 的 RAII 守衛保證，一旦漏掉就是 §12 所述的槽位永久洩漏——`chunk_engine` 報告 §13.1 對這個失敗模式有完整分析。

---

## 13. 檔案索引

### `src/storage/`

| 檔案 | 職責 |
|---|---|
| `storage.cpp` | binary 進入點 |
| `CMakeLists.txt` | `target_add_bin(storage_main "storage.cpp" storage jemalloc)` |

### `service/`

| 檔案 | 職責 |
|---|---|
| `StorageServer.cc/h` | 服務容器：掛載磁碟、載入 target、向 mgmtd 註冊、啟動所有 worker |
| `StorageService.cc/h` | serde 服務定義，RPC 方法表 |
| `StorageOperator.cc/h` | **核心**：所有 RPC 的實作入口（read / write / commit / sync / query…） |
| `Components.cc/h` | 全部子組件的持有者，貫穿各層的依賴注入容器 |
| `TargetMap.cc/h` | target 的無鎖快照表（`snapshot()` 是全檔最常見的呼叫） |
| `ReliableUpdate.cc/h` | 寫入的可靠性包裝：重試與冪等 |
| `ReliableForwarding.cc/h` | CRAQ 逐跳轉發的可靠性包裝（`forwardWithRetry` 的實作處） |
| `BufferPool.cc/h` | 雙尺寸 RDMA 緩衝池，同時是 io_uring 的 fixed buffer 來源（§11.2） |

### `store/`

| 檔案 | 職責 |
|---|---|
| `StorageTargets.cc/h` | 全部 target 的容器：磁碟掛載、target 建立與載入 |
| `StorageTarget.cc/h` | 單一 target：對應一顆盤上的一個儲存單元 |
| `ChunkStore.cc/h` | chunk 存取的統一門面 |
| `ChunkEngine.cc/h` | **Rust chunk engine 的 cxx 橋接層**（§12） |
| `ChunkFileStore.cc/h` | 舊版檔案式 chunk 儲存（chunk engine 之前的實作） |
| `ChunkFileView.cc/h` | chunk 檔案的視圖抽象，封裝 offset 計算 |
| `ChunkMetaStore.cc/h` | chunk metadata 的持久化 |
| `ChunkMetadata.cc/h` | metadata 資料結構：三版本 + state + checksum |
| `ChunkReplica.cc/h` | 副本層邏輯：CRAQ 狀態判定 |
| `GlobalFileStore.cc/h` | 全域檔案控制代碼管理（fd 快取） |
| `PhysicalConfig.h` | 實體佈局設定：目錄結構、chunk 大小清單 |

### `update/`

| 檔案 | 職責 |
|---|---|
| `UpdateJob.h` | 寫入任務的資料結構，持有 RDMA 接收緩衝區與 `WritingChunk` 指標 |
| `UpdateWorker.cc/h` | 寫入任務的執行者 |

### `aio/`

| 檔案 | 職責 |
|---|---|
| `AioReadWorker.cc/h` | 非同步讀取 worker，32 執行緒、4096 佇列、可切換 libaio／io_uring（§11.1） |
| `AioStatus.cc/h` | `IoStatus` 虛介面 + `AioStatus`（libaio）+ `IoUringStatus`（io_uring）兩個實作 |
| `BatchReadJob.cc/h` | 批次讀取任務；持有 chunk 裸指標並以 RAII 保證 `release_raw_chunk` |

### `sync/`

| 檔案 | 職責 |
|---|---|
| `ResyncWorker.cc/h` | resync 執行者：前驅主動 PUSH 給後繼的完整協定（§8） |

### `worker/`

| 檔案 | 職責 |
|---|---|
| `AllocateWorker.cc/h` | 雙水位預配置 group，一般與 ultra（>4MiB）分開管理（§10.1） |
| `CheckWorker.cc/h` | 磁碟健康檢查與三段式空間水位保護（0.95／0.96／0.98）（§10.2） |
| `PunchHoleWorker.cc/h` | 空間回收，每 target 每輪上限 128 次且逐次檢查停止旗標（§10.3） |
| `SyncMetaKvWorker.cc/h` | 每分鐘強制 sync 所有 target 的 metadata KV（§10.4） |
| `DumpWorker.cc/h` | 每日狀態轉存 + **CPU 超標時自動啟動 gperftools profiler 一分鐘**（§10.5） |
