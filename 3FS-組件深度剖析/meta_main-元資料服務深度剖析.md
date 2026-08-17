# meta_main：元資料服務（Metadata Server）深度剖析

> 對應原始碼：`src/meta/`（全部 56 個檔案）
> 進入點：`src/meta/meta.cpp` — CMake 目標定義於 `src/meta/CMakeLists.txt:2`
> 邊界：`src/fbs/meta/`（RPC 契約）、`src/core/app/`（啟動骨架）、`src/common/kv/`（交易介面）、`src/client/mgmtd/`（路由）
> 設定：`configs/meta_main.toml`、`configs/meta_main_app.toml`、`configs/meta_main_launcher.toml`

---

## 0. 一句話總結

`meta_main` 是一個**完全無狀態的 POSIX 命名空間翻譯器**：它把每一個 RPC 請求翻譯成一個 FoundationDB 交易，交易提交成功就回應成功，失敗就按 FDB 的錯誤分類重試。它自己不持有任何權威狀態——路由來自 mgmtd、資料長度來自 storage、一致性來自 FDB——唯一「有狀態」的東西是**進程內的批次合併佇列**（把打在同一個 inode 上的併發請求併成一個交易）以及為了讓這個合併有效而存在的 **Distributor**（決定哪個 inode 該由哪台 meta server 處理）。所有不能在單一交易裡完成的事情（刪大目錄、回收 chunk、清殭屍 session）都被推給背景組件慢慢做。

---

## 1. 與《3FS 元資料層深度剖析》的分工

專案根目錄已有 `3FS-元資料層深度剖析.md`。兩份報告的切分軸是**「資料長什麼樣」對「程式怎麼跑」**：

| 面向 | 元資料層報告 | 本報告 |
|---|---|---|
| 三張邏輯表的 key 佈局、endianness 取捨 | §2、§3（權威） | 引用，不重述 |
| InodeId 64-bit 位址空間規劃 | §4（權威） | 只講分配器的執行期行為 |
| `Layout` / `ChainRange` / `ChunkId` 編碼 | §6（權威） | 只講 ChainAllocator 何時填它 |
| `VersionedLength` 的合併語意 | §7（權威） | §14 講**誰在什麼時機呼叫它**、跨交易重試怎麼保持一致 |
| `DirEntry` 的 `dirAcl` / `uuid` / `gcInfo` 三個反正規化欄位 | §8（權威） | §9、§10 講它們在執行路徑上被誰讀寫 |
| GC 垃圾桶的 key 排序技巧 | §9（權威） | §13.1 講 GcManager 的任務調度、故障復原、孤兒目錄 |
| 冪等記錄的 key 佈局 | §10（權威） | §10 講**哪些操作需要冪等、由誰觸發、與交易 reset 的互動** |
| Distributor 的 HRW 雜湊 | §11（權威） | §11 講成員表的維護迴圈、心跳、下線、與交易衝突集的耦合 |
| BatchContext 的讀取去重 | §12（權威） | §7.2 講它在 batchStatByPath 路徑上的實際作用 |
| snapshot 讀 vs 一般讀的 assert 守護 | §13（權威） | §16 講衝突集在每個操作裡被顯式加入的位置 |
| serde 的 append-only 演進規則 | §14（權威） | 引用 |

**建議閱讀順序**：先讀元資料層報告 §2–§8 建立「資料長什麼樣」的心智模型，再讀本報告；本報告在需要時會標註「詳見元資料層報告 §X」。

---

## 2. 進程生命週期：從 `main` 到 `serve`

### 2.1 八行的 main

`src/meta/meta.cpp` 全文只有 8 行：

```cpp
#include "common/app/TwoPhaseApplication.h"
#include "memory/common/OverrideCppNewDelete.h"
#include "meta/service/MetaServer.h"

int main(int argc, char *argv[]) {
  using namespace hf3fs;
  return TwoPhaseApplication<meta::server::MetaServer>().run(argc, argv);
}
```

三件事：(1) 覆寫全域 `new`/`delete` 以掛上 jemalloc 統計（`OverrideCppNewDelete.h`，CMake 也顯式連結 `jemalloc`，`src/meta/CMakeLists.txt:2`）；(2) 把 `MetaServer` 這個型別餵給通用的 `TwoPhaseApplication` 模板；(3) 回傳。所有的啟動流程都由模板驅動。

### 2.2 TwoPhaseApplication 的「兩階段」

`src/common/app/TwoPhaseApplication.h:35-70` 是骨架。「兩階段」指的是**先用本地最小設定把 launcher 拉起來，再用 launcher 從 mgmtd 拉取真正的服務設定**：

```
階段一（本地）                                    階段二（遠端）
─────────────────────────────                    ──────────────────────────────
parseFlags(--app_cfg/--launcher_cfg)
        │
        ▼
launcher_->init()                                 
  ├─ appConfig_.init(meta_main_app.toml)          # node_id、allow_empty_node_id
  ├─ launcherConfig_.init(meta_main_launcher.toml)# cluster_id、mgmtd 位址、IB 裝置
  └─ net::IBManager::start(ib_devices)            # RDMA 裝置初始化（FATAL if fail）
        │
        ▼
loadAppInfo()  ──────────────────────────────────▶ 向 mgmtd 補齊 AppInfo（nodeId/hostname/…）
        │
        ▼
initConfig(config_, …, launcher_->loadConfigTemplate())
                   └──────────────────────────────▶ 從 mgmtd 拉 META 型別的設定模板
        │                                            （即 meta_main.toml 的內容）
        ▼
initCommonComponents(log / monitor / memory)
        │
        ▼
initServer():  new MetaServer(config.server()); server_->setup()
        │
        ▼
startServer(): launcher_->startServer(*server_, appInfo_)
        │
        ▼
launcher_.reset()      ← launcher 用完即丟
```

`TwoPhaseApplication::initApplication()` 全程用 `XLOGF_IF(FATAL, …)`，任一步失敗就直接 abort——meta server 沒有「降級啟動」模式。設定推送方面：`configPushable()` 回傳 `FLAGS_cfg.empty() && !FLAGS_use_local_cfg`，也就是**只有在沒指定本地設定檔時，才接受 mgmtd 熱推設定**。

`ServerLauncher`（`src/core/app/ServerLauncher.h:34-67`）的 `RemoteConfigFetcher` 對 meta 而言是 `core::launcher::ServerMgmtdClientFetcher`（`MetaServer.h:42`）；它會建立一個臨時 mgmtd client，抓 `NodeType::META` 的設定模板，然後在 `startServer` 時把真正的 mgmtd client 交棒給 server。

### 2.3 MetaServer::beforeStart：真正的組裝現場

`net::Server::start()`（`src/common/net/Server.cc:35-46`）的順序是 `beforeStart() → 各 group->start() → afterStart()`。meta 把所有初始化都塞在 `beforeStart()`（`src/meta/service/MetaServer.cc:25-81`），也就是說**服務端口在所有依賴就緒之後才開始 listen**：

```
MetaServer::beforeStart()                                   src/meta/service/MetaServer.cc
 │
 ├─(1) backgroundClient_ = net::Client(config.background_client())      :26-29
 │        └─ 專供 forward 與 mgmtd 心跳使用，與服務端口的 IO 執行緒隔離
 │
 ├─(2) mgmtdClient_ = MgmtdClientForServer(clusterId, RealStubFactory)  :30-36
 │        ├─ setAppInfoForHeartbeat(appInfo())                          :38
 │        ├─ setConfigListener(ApplicationBase::updateConfig)           :39   ← 熱更新入口
 │        ├─ updateHeartbeatPayload(flat::MetaHeartbeatInfo{})          :40
 │        ├─ start(&tpg().bgThreadPool().randomPick())                  :41
 │        └─ refreshRoutingInfo(force=false)  ── 失敗即 FATAL           :42-43
 │
 ├─(3) kvEngine_ = HybridKvEngine::from(kv_engine, use_memkv, fdb)      :46-48
 │        └─ 實際上就是 FDB client（use_memkv 已標 deprecated）
 │
 ├─(4) storageClient = StorageClient::create(ClientId::random(host), …) :50-53
 │        └─ meta 需要它來 queryChunks（查長度）與 removeChunks（GC）
 │
 ├─(5) metaOperator_ = MetaOperator(config.meta(), nodeId, kvEngine,
 │                                  mgmtdClient, storageClient,
 │                                  Forward(...))                       :57-63
 │        └─ nodeId 為 0 直接 FATAL（:56）——Distributor 依賴它
 │
 ├─(6) addSerdeService(MetaSerdeService(*metaOperator_), strict=true)   :64
 │     addSerdeService(core::CoreService())                             :65
 │        └─ strict=true 表示找不到名為 "MetaSerde" 的 group 就報錯，
 │           而不是默默塞進第一個 group
 │
 ├─(7) metaOperator_->init(rootLayout)                                  :68-76
 │        └─ rootLayout 只有 use_memkv 時才設；正式部署由 admin_cli
 │           的 init-cluster 建立 root/gcRoot，這裡只跑 GcManager::init()
 │
 └─(8) metaOperator_->start(tpg().bgThreadPool())                       :78
          └─ 啟動所有背景組件（見 §13）
```

停機路徑對稱且分成兩段（`MetaServer.cc:83-97`）：

```
Server::stopAndJoin()
 ├─ beforeStop():  metaOperator_->beforeStop()   ← stop_ = true; Distributor 從成員表移除自己
 │                 mgmtdClient_->stop()
 ├─ 各 group->stopAndJoin()                      ← 停止接受新請求
 ├─ afterStop():   metaOperator_->afterStop()    ← 停 bgRunner / gc / session / fileHelper，關 trace log
 │                 backgroundClient_->stopAndJoin()
 └─ tpg_.stopAndJoin()
```

**設計要點**：`beforeStop()` 先把自己從 Distributor 成員表拿掉（`MetaOperator.cc:232-239` → `Distributor::stopAndJoin(true)` → `update(exit=true)`），**再**停止服務端口。這中間有個短窗口：其他 server 已經知道「我不負責這些 inode 了」，但我還在接請求。這時我收到的 close/sync 會發現 `checkOnServer` 回 false，回 `kBusy` 讓 client 重試——安全但會有短暫抖動。反過來如果先關端口再改成員表，就會有請求被轉發到已死的節點。3FS 選了前者。

### 2.4 服務端點與執行緒池

`MetaServer::Config`（`src/meta/service/MetaServer.h:48-57`）預設兩個 service group：

| group | 預設埠 | toml 實際值 | 網路型別 | 服務 | 執行緒池 |
|---|---|---|---|---|---|
| 0 | 8000 | 8001 | RDMA | `MetaSerde` | 共用 `tpg_` |
| 1 | 9000 | 9001 | TCP | `Core` | `use_independent_thread_pool = true` |

（`configs/meta_main.toml` 的 `[[server.base.groups]]` 兩段。）

**為什麼 Core 服務要獨立執行緒池**：`Core` 是管理面（`getConfig` / `hotUpdateConfig` / `shutdown` / `renderConfig`，見 `src/fbs/core/service/CoreServiceDef.h`）。如果和資料面共用執行緒池，一旦 meta 被大量請求打滿（`max_processing_requests_num = 4096`），管理員連 `hotUpdateConfig` 把限流打開都做不到。獨立池 + TCP（不依賴 RDMA 健康）是一道逃生門。

`ThreadPoolGroup` 有三個 `CPUExecutorGroup`（`src/common/net/ThreadPoolGroup.h:34-36`）：`proc`（處理請求）、`io`（網路收發）、`bg`（背景任務）。meta 的背景組件全部跑在 `bgThreadPool()` 上（`MetaServer.cc:78` 傳入 `tpg().bgThreadPool()`），與請求處理隔離。預設每池 2 執行緒（toml `num_proc_threads = 2` 等）。

---

## 3. RPC 契約：完整方法表

`MetaSerde` 服務 ID 為 **4**（`src/fbs/meta/Service.h:709`，`SERDE_SERVICE(MetaSerde, 4)`）。方法 ID 是 serde 層的路由鍵，**永遠不能重排或複用**（序列化格式是 positional 的，詳見元資料層報告 §14）。

| ID | 方法 | 請求 | 回應 | 服務端實作 | 交易性質 |
|---:|---|---|---|---|---|
| 1 | `statFs` | `StatFsReq` | `StatFsRsp` | `store/ops/StatFs.cc:13` | 唯讀，**不碰 FDB**（讀 FileHelper 快取） |
| 2 | `stat` | `StatReq` | `StatRsp` | `store/ops/Stat.cc:31` | 唯讀 |
| 3 | `create` | `CreateReq` | `CreateRsp` | `store/ops/Open.cc:304`（tryOpen）+ `ops/BatchOperation.cc:446` | 讀寫，可批次 |
| 4 | `mkdirs` | `MkdirsReq` | `MkdirsRsp` | `store/ops/Mkdirs.cc:19` | 讀寫 |
| 5 | `symlink` | `SymlinkReq` | `SymlinkRsp` | `store/ops/Symlink.cc:16` | 讀寫 |
| 6 | `hardLink` | `HardLinkReq` | `HardLinkRsp` | `store/ops/HardLink.cc:22` | 讀寫 |
| 7 | `remove` | `RemoveReq` | `RemoveRsp` | `store/ops/Remove.cc:44` | 讀寫，**冪等** |
| 8 | `open` | `OpenReq` | `OpenRsp` | `store/ops/Open.cc:300` | 唯讀或讀寫（動態判定） |
| 9 | `sync` | `SyncReq` | `SyncRsp` | `store/ops/BatchOperation.cc:189` | 讀寫，**必批次** |
| 10 | `close` | `CloseReq` | `CloseRsp` | `store/ops/BatchOperation.cc:229` | 讀寫，**必批次** |
| 11 | `rename` | `RenameReq` | `RenameRsp` | `store/ops/Rename.cc:50` | 讀寫，可冪等 |
| 12 | `list` | `ListReq` | `ListRsp` | `store/ops/List.cc:24` | 唯讀 |
| 13 | `truncate` | `TruncateReq` | `TruncateRsp` | **已廢棄**，`MetaOperator.cc:314-317` | — |
| 14 | `getRealPath` | `GetRealPathReq` | `GetRealPathRsp` | `store/ops/GetRealPath.cc:46` | 唯讀 |
| 15 | `setAttr` | `SetAttrReq` | `SetAttrRsp` | `store/ops/SetAttr.cc:23` 或 `BatchOperation.cc:305` | 讀寫，**依請求形式二選一** |
| 16 | `pruneSession` | `PruneSessionReq` | `PruneSessionRsp` | `store/ops/PruneSession.cc:26` | 讀寫 |
| 17 | `dropUserCache` | `DropUserCacheReq` | `DropUserCacheRsp` | `MetaOperator.cc:425-432` | **不開交易** |
| 18 | `authenticate` | `AuthReq` | `AuthRsp` | `MetaOperator.cc:279-282` | **不開交易** |
| 19 | `lockDirectory` | `LockDirectoryReq` | `LockDirectoryRsp` | `store/ops/LockDirectory.cc:15` | 讀寫 |
| 20 | `batchStat` | `BatchStatReq` | `BatchStatRsp` | `store/ops/Stat.cc:68` | 唯讀 |
| 21 | `batchStatByPath` | `BatchStatByPathReq` | `BatchStatByPathRsp` | `store/ops/Stat.cc:68`（同模板） | 唯讀 |
| 50 | `testRpc` | `TestRpcReq` | `TestRpcRsp` | `store/MetaStore.cc:84` | 唯讀，空實作（壓測用） |

方法 ID 21（`batchStatByPath`）之後直接跳到 50（`testRpc`），中間 22–49 未使用（`src/fbs/meta/Service.h:728-741`）。原始碼沒有註解說明這個跳號的用意。

### 3.1 Service wrapper 是純轉發

`MetaSerdeService`（`src/meta/service/MetaSerdeService.h:14-39`）用一個宏把 22 個方法全部展開成同一件事：

```cpp
#define META_SERVICE_METHOD(NAME, REQ, RESP) \
  CoTryTask<RESP> NAME(serde::CallContext &, const REQ &req) { return meta_.NAME(req); }
```

`serde::CallContext` 被完全忽略。這代表**服務層沒有任何邏輯**——沒有限流、沒有記錄、沒有鑑權，全部下沉到 `MetaOperator`。好處是 RPC 層與業務層零耦合（`MockMeta.h:59` 可以用同一個 `MetaSerdeService` 包裝任意 `MetaOperator` 做單測）。

### 3.2 所有請求共享的 `ReqBase`

`src/fbs/meta/Service.h:37-63`：

```cpp
struct ReqBase {
  SERDE_STRUCT_FIELD(user, UserInfo{});          // uid / gid / groups / token
  SERDE_STRUCT_FIELD(client, ClientId{Uuid::zero()});  // 客戶端身分（含 uuid）
  SERDE_STRUCT_FIELD(forward, flat::NodeId(0));  // 非 0 = 這是被轉發來的
  SERDE_STRUCT_FIELD(uuid, Uuid::zero());        // 請求 uuid（冪等用）
};
```

四個欄位對應四個機制：`user` → 權限檢查（§15）；`client` → 目錄鎖 / session 歸屬；`forward` → 防止轉發成環（§12）；`uuid` → 冪等（§10）。

`uuid` 只在需要冪等的請求建構子裡才填 `Uuid::random()`：`CreateReq`（:158）、`MkdirsReq`（:229）、`RemoveReq`（:329）、`RenameReq`（:470）。`OpenReq`、`SyncReq`、`CloseReq`、`StatReq` 等**不填**，因為它們天然冪等或不需要。

---

## 4. 請求處理的三條路徑

`MetaOperator` 是唯一的分派點。每個 RPC 走且僅走以下三條路徑之一：

```
                        RPC 進入 MetaOperator::xxx(req)
                                   │
                    ┌──────────────┼──────────────────────┐
                    ▼              ▼                      ▼
             ①  runOp()      ②  runInBatch()        ③  forward_->forward()
        （單請求單交易）    （合併同 inode 的請求）    （轉給別台 meta）
                    │              │                      │
                    ▼              ▼                      ▼
          OperationDriver     BatchedOp + Batch 佇列   RPC 到目標節點
          .run(txn, retry)    → OperationDriver         （目標節點走 ①/②）
```

### 4.1 路徑① `runOp` — 一個請求一個交易

`src/meta/service/MetaOperator.cc:70-87`：

```cpp
template <typename Func, typename Arg>
auto MetaOperator::runOp(Func &&func, Arg &&arg) -> CoTryTask<...RspT> {
  auto deadline = std::optional<SteadyTime>();
  if constexpr (std::is_base_of_v<ReqBase, std::remove_reference_t<Arg>>) {
    CO_RETURN_ON_ERROR(arg.valid());                       // 參數校驗
    if (config_.operation_timeout() != 0_s)
      deadline = SteadyClock::now() + config_.operation_timeout();  // 預設 5s
  }
  auto txn = kvEngine_->createReadWriteTransaction();
  auto op = ((*metaStore_).*func)(std::forward<Arg>(arg));  // 造一個 IOperation
  auto driver = OperationDriver(*op, arg, deadline);
  co_return co_await driver.run(std::move(txn), createRetryConfig(),
                                config_.readonly(), config_.grv_cache());
}
```

三個細節：

1. **`arg.valid()` 在建交易之前**。無效請求不消耗 FDB 資源。但注意 `Operation::run()` 裡還有一次 `CHECK_REQUEST(req_)`（`store/Operation.h:35-42`）——因為 `runInBatch` 路徑的校驗時機不同，且 `tryOpen` 會就地修改 `req`（§7.4），所以重複校驗是必要的。
2. **`deadline` 是「整個操作」的，不是單次交易的**。`operation_timeout = 5s` 與 FDB 交易本身的 5 秒上限對齊：一個操作最多重試到 5 秒，超過就回 `MetaCode::kOperationTimeout`。
3. **總是建 ReadWrite 交易**，即使操作是唯讀。唯讀性由 `IOperation::isReadOnly()` 表達，`OperationDriver` 再據此決定要不要 `commit()`、要不要開 GRV cache。

`MetaStore::xxx(req)` 回傳的是 `std::unique_ptr<IOperation<Rsp>>`——每個請求 new 一個 operation 物件。這看起來浪費，但關鍵在於 **operation 物件承載跨重試的狀態**：`OpenOp::prevCreatedInodeId_`、`BatchedOp::versionstamp_/currLength_/nextLength_`、`Operation::events_` 都必須在交易重試之間存活。

### 4.2 路徑② `runInBatch` — 兩槽流水線

只有四種請求走這條路：`sync`、`close`、`create`（by parent inode）、`setAttr`（by inode，無 path）。

`src/meta/service/MetaOperator.cc:115-132`：

```cpp
template <typename Req, typename Rsp>
CoTryTask<Rsp> MetaOperator::runInBatch(InodeId inodeId, Req req) {
  CO_RETURN_ON_ERROR(req.valid());
  auto deadline = ...;
  OperationRecorder::Guard guard(OperationRecorder::server(), MetaSerde<>::getRpcName(req), req.user.uid);
  BatchedOp::Waiter<Req, Rsp> waiter(std::move(req));
  auto op = addBatchReq(inodeId, waiter);   // 可能回 nullptr
  co_await waiter.baton;                    // ← 等「輪到我」或「結果好了」
  if (op) {
    co_await runBatch(inodeId, std::move(op), deadline);   // 我是這批的 owner
  }
  auto result = waiter.getResult();
  guard.finish(result);
  co_return result;
}
```

`addBatchReq`（`MetaOperator.h:146-180`）的三分支決定了「兩槽流水線」結構：

```
batches_ : Shards<std::map<InodeId, Batch>, 63>       ← 63 個分片，各自一把 std::mutex
                                                         分片鍵 = inodeId 的 hash

Batch { BatchedOp *next; folly::coro::Baton *nextBaton; }

┌──────────────────────────────────────────────────────────────────────┐
│  情況 A：map 裡沒有這個 inodeId  → try_emplace 插入                   │
│    new BatchedOp; op->add(waiter); waiter.baton.post();  ← 立刻放行    │
│    return op;   （呼叫端成為 owner，馬上去跑 runBatch）               │
├──────────────────────────────────────────────────────────────────────┤
│  情況 B：有 Batch 但 next == nullptr  （前一批正在跑）                │
│    new BatchedOp; op->add(waiter); batch.setNext(op.get(), &baton);  │
│    return op;   （呼叫端成為「下一批」owner，但 baton 尚未 post，     │
│                   要等前一批 runBatch 結束呼叫 wakeupNext()）         │
├──────────────────────────────────────────────────────────────────────┤
│  情況 C：有 Batch 且 next != nullptr  （下一批正在累積）              │
│    if (next->numReqs() >= max_batch_operations)  → 回 kBusy 並 post   │
│    else next->add(waiter);                                            │
│    return nullptr;   （呼叫端純等待，結果由 BatchedOp::finish 填）    │
└──────────────────────────────────────────────────────────────────────┘
```

也就是說，對每個 inode **同時最多只有一個交易在跑，加上一個正在累積的批次**。這是刻意的：多個交易同時改同一個 inode key 必定互相衝突，FDB 會讓它們反覆重試，吞吐反而更低。用一個進程內佇列把它們序列化，再把序列化後的請求**合併成一次讀 + 一次寫**，是把「衝突」轉成「批次」。

`runBatch` 結束時（`MetaOperator.cc:103-111`）：

```cpp
batches_.withLock([&](auto &map) {
  auto iter = map.find(op->inodeId_);
  XLOGF_IF(FATAL, iter == map.end(), "shouldn't happen");
  if (!iter->second.wakeupNext()) {  // 有下一批 → 叫醒它
    map.erase(iter);                 // 沒有 → 從 map 移除，回到情況 A
  }
}, op->inodeId_);
```

**觀察**：`max_batch_operations = 4096`（`base/Config.h:83`）。超過就回 `MetaCode::kBusy`（可重試錯誤），並且每累積到 1024 的倍數就 WARN 一次（`MetaOperator.h:171-173`）。這是對「一萬個 client 同時 close 同一個檔案」這種場景的防禦——寧可讓一部分 client 退避重試，也不要造出一個 100 MB 的 FDB 交易。

`Shards<…, 63>` 的分片數是 63 而非 2 的冪——`src/common/utils/Shards.h:14` 用的是 `% N` 取模而非位元遮罩，所以分片數不必是 2 的冪。**63 是合數（7×9），不是質數**；原始碼裡沒有任何註解說明為什麼選這個數字。分片鍵是 `folly::hash::hash_combine_generic` 的結果，不受 InodeId 在 FDB key 裡的 LE 佈局（元資料層報告 §3.1）影響。

### 4.3 路徑③ `forward` — 轉發

見 §12。

### 4.4 分派決策表

| RPC | 路徑 | 分派鍵 | 是否鑑權（`AUTHENTICATE`） |
|---|---|---|---|
| `authenticate` | 直接 | — | 是 |
| `statFs` / `stat` / `batchStat` / `batchStatByPath` / `getRealPath` / `open` / `mkdirs` / `symlink` / `remove` / `rename` / `list` / `hardLink` / `lockDirectory` | ① `runOp` | — | 是 |
| `pruneSession` / `testRpc` | ① `runOp` | — | **否** |
| `dropUserCache` | 直接（改記憶體快取） | — | 否 |
| `sync` | ②/③ | `req.inode` | **否**（`MetaOperator.cc:320` 明確註解 "don't auth user for sync"） |
| `close` | ②/③ | `req.inode` | **否**（`:331` "don't auth user here"） |
| `create` | 先 ① 試 `tryOpen`，失敗再 ②/③ | `req.path.parent` | 是 |
| `setAttr` | 有 path → ①；無 path → ②/③ | `req.path.parent` | 是 |
| `truncate` | 直接回 `kNotImplemented` | — | 否 |

**`close`/`sync` 不鑑權是刻意的**。這兩個操作由 client 在檔案關閉時發出，也會由 `SessionManager` 在 client 掉線後代發（`components/SessionManager.cc:189-215` 的 `CloseTask` 偽造一個 `CloseReq`）。若要求鑑權，代發的請求會因為拿不到 token 而失敗，殭屍 session 就永遠清不掉。權限已經在 `open` 時檢查過，`close` 只是把 session 移除並回寫長度，不擴大權限。

---

## 5. OperationDriver：交易驅動器

`src/meta/store/Operation.h:148-263`。所有操作最終都經過它。

### 5.1 主迴圈

```cpp
CoTryTask<Rsp> OperationDriver::run(txn, retryConfig, readonly, enableGrvCache) {
  config.retryMaybeCommitted = operation_.retryMaybeCommitted();   // :160
  kv::FDBRetryStrategy strategy(config);
  CO_RETURN_ON_ERROR(strategy.init(txn.get()));                    // 設 FDB_TR_OPTION_MAX_RETRY_DELAY

  OperationRecorder::Guard recorder(...);

  if (readonly && !operation_.isReadOnly())                        // :166-168
    co_return makeError(StatusCode::kReadOnlyMode, ...);

  if (operation_.isReadOnly() && enableGrvCache && 是 FDBTransaction)
    fdbTxn->setOption(FDB_TR_OPTION_USE_GRV_CACHE, {});            // :170-174

  Result<Rsp> result = makeError(MetaCode::kOperationTimeout);
  auto duplicate = false;
  while (true) {
    if (deadline_ && *deadline_ <= SteadyClock::now()) break;      // :180-183
    result = co_await runAndCommit(*txn, operation_, duplicate);   // :185
    if (ErrorHandling::success(result)) break;                     // :186
    operation_.retry(result.error());                              // :191  ← 清空事件、重置 waiter
    auto retry = co_await strategy.onError(txn.get(), result.error());
    if (retry.hasError()) { result = makeError(retry.error()); break; }
    recorder.retry()++;
  }

  if (result.hasError() && result.error().code() == StatusCode::kOK) {  // :200-203
    XLOGF(DFATAL, ...); result = makeError(MetaCode::kFoundBug);
  }
  recorder.finish(result, duplicate);
  operation_.finish(result);                                       // :206  ← 這時才寫事件日誌
  co_return result;
}
```

四個值得注意的設計：

**(a) 「成功」的定義比「沒有錯誤」寬。** `ErrorHandling::success()`（`src/fbs/meta/Utils.h:40-74`）把 `kNotFound`、`kExists`、`kNoPermission`、`kNotEmpty`、`kNotDirectory`、`kIsDirectory`、`kNotFile`、`kTooManySymlinks`、`kInvalidFileLayout`、`kNameTooLong`、`kMoreChunksToRemove` 以及 `kInvalidArg`、`kAuthenticationFail` 全部視為「操作成功執行了，只是結果是個預期內的錯誤」。這些**不會觸發重試**——重試 `stat` 一個不存在的檔案沒有意義。這條線也用在監控上：`recorder.finish(result)` 只把非 success 的錯誤記成 failure。

**(b) `retryMaybeCommitted` 是每個操作自己決定的。** `IOperation::retryMaybeCommitted()` 預設 `true`（`store/MetaStore.h:54`）。FDB 的 `commit_unknown_result` 代表「交易可能已提交也可能沒有」，重試它需要操作本身是冪等的。3FS 的做法是**讓大部分操作靠結果指紋達成天然冪等**（rename/create/symlink/hardLink/mkdirs 檢查 `DirEntry::uuid`；open 檢查 `prevCreatedInodeId_`），所以可以放心重試。全 codebase 沒有覆寫這個方法回 `false` 的操作——但介面留著，是給未來不冪等的操作用的。

**(c) `deadline` 檢查在迴圈開頭。** 意味著一次已經開始的交易不會被中途打斷，最壞情況是 `deadline + 一次交易時間`。

**(d) `operation_.finish(result)` 在最後才呼叫。** `Operation::finish`（`store/Operation.h:57-69`）只在**沒有錯誤**時才把 `events_` 寫進事件日誌、把 `traces_` 寫進 parquet trace log。而 `Operation::retry`（:55）會 `clearEvents()`。合起來保證：**事件日誌只記錄真正提交成功的操作，重試過程中產生的事件不會重複輸出**。這是把「至少一次的執行」轉成「恰好一次的稽核記錄」。

### 5.2 `runAndCommit`：冪等與非冪等的兩條分支

`src/meta/store/Operation.h:221-250`：

```cpp
Uuid clientId, requestId;
auto readonly = handler.isReadOnly();
auto idem = !readonly && operation_.needIdempotent(clientId, requestId);
if (idem) {
  OperationRecorder::server().addIdempotentCount();
  IDEMPOTENT_CHECK();                       // 讀 IDEM 記錄，命中就直接回舊結果
  auto result = co_await handler(txn);
  if (result) {                             // 成功
    CO_RETURN_ON_ERROR(co_await Idempotent::store(txn, clientId, requestId, result));
    CO_RETURN_ON_ERROR(co_await txn.commit());
  } else if (ErrorHandling::success(result) || !ErrorHandling::retryable(result.error())) {
    // 這是最終結果（預期內的錯誤，或不可重試的錯誤）
    txn.reset();                            // ← 丟棄本次交易的所有修改
    IDEMPOTENT_CHECK();                     // ← reset 後要重新檢查一次
    CO_RETURN_ON_ERROR(co_await Idempotent::store(txn, clientId, requestId, result));
    CO_RETURN_ON_ERROR(co_await txn.commit());
  }
  co_return result;
} else {
  auto result = co_await handler(txn);
  if (!result.hasError() && !readonly) CO_RETURN_ON_ERROR(co_await txn.commit());
  co_return result;
}
```

**最精妙的是 `txn.reset()` 那一段。** 場景：`rm -rf /a/b` 第一次執行成功，回應在網路上丟了；client 重試。第二次執行時 `/a/b` 已經不存在，`RemoveOp` 回 `kNotFound`。如果就這樣回給 client，`rm` 會報「找不到檔案」——但使用者的意圖明明已經達成了。

有了這段邏輯：第二次執行前 `IDEMPOTENT_CHECK()` 會讀到第一次存的 `IDEM` 記錄，直接回傳第一次的成功結果。而**如果第一次連 IDEM 記錄都沒寫成功**（例如第一次是在 `handler` 執行後、`commit` 之前掛掉），第二次就會真的執行並得到 `kNotFound`——這時 `ErrorHandling::success(kNotFound)` 為 true，走 `txn.reset()` 分支：丟棄本次的（空的）修改，只把「這個請求的結果是 kNotFound」寫進 IDEM 表並提交。第三次重試就會直接讀到這個記錄。

**為什麼 reset 後要再 `IDEMPOTENT_CHECK()` 一次**：`txn.reset()` 會清掉讀版本，交易會拿一個新的讀版本重新開始。在這個新版本下，可能另一個併發的重試已經寫進了 IDEM 記錄。不重查就會覆蓋別人的結果（雖然結果應該相同，但這是 defensive 的）。

**`duplicate` 旗標**穿過整條路徑到 `recorder.finish(result, duplicate)`，讓監控能區分「真的執行了」與「命中冪等記錄」。

---

## 6. 路徑解析：PathResolveOp

`src/meta/store/PathResolve.h` / `.cc`。這是幾乎所有操作的第一步，也是唯讀開銷的大頭。

### 6.1 核心不變式：全程 snapshot 讀

類註解（`PathResolve.h:22-27`）：

```
Note: PathResolveOp always use snapshotLoad, so it won't add any key into read conflict set.
User should add keys into read conflict set manually if needed.
```

這是整個 meta server 正確性的樞紐。原因：解析 `/a/b/c/d/e` 要讀 5 層 dentry + 若干 inode。如果全部進讀衝突集，任何人在 `/a` 底下建個檔案都會讓你的交易衝突。代價是**解析結果可能過期**——所以每個寫操作都必須在決定要寫什麼之後，**顯式**把關鍵 key 加回衝突集。這件事在 debug build 靠 `assert(!snapshotLoaded_)` 守護（元資料層報告 §13）。

### 6.2 解析狀態機

`PathResolveOp::pathRange`（`PathResolve.cc:214-282`）是主迴圈：

```
                    begin, end (Path 迭代器)
                            │
              ┌─────────────▼──────────────┐
              │  begin == end ? kNotFound  │
              └─────────────┬──────────────┘
                            │
              ┌─────────────▼──────────────────────────┐
              │  *begin == "/" ?                       │
              │    parent = InodeId::root(); ++begin   │
              │    若 ++begin == end：特例，回 root     │
              └─────────────┬──────────────────────────┘
                            │
        ┌───────────────────▼───────────────────────────────────┐
        │  while (begin != end):                                │
        │    ┌── *begin 是 "." ? ──▶ ++begin，continue          │
        │    │                       （最後一段是 "." 則特殊處理）│
        │    ├── pathComponent(parent, *begin)                  │
        │    │     ├─ loadAndCheckParentAcl  ← EXEC 權限檢查     │
        │    │     ├─ ".."  → 讀 parent inode 拿 asDirectory().parent │
        │    │     └─ 一般  → DirEntry::snapshotLoad(parent, name)   │
        │    ├── dirEntry 不存在 ──▶ 立刻回傳（begin 停在缺失處） │
        │    ├── ++begin == end  ──▶ 回傳（這是最後一段）        │
        │    ├── 是 symlink      ──▶ symlink() 展開，深度+1      │
        │    ├── 是 file         ──▶ kNotDirectory              │
        │    └── parent = 該 dirEntry；continue                  │
        └───────────────────────────────────────────────────────┘
```

**「回傳時 `begin` 停在第一個缺失的路徑段」是關鍵的介面設計**。`PathResolveOp::path()`（`:160-179`）在此之上加一層：若剛好只缺最後一段，回傳成功（`dirEntry` 為 `nullopt`），中間缺就回 `kNotFound`。而 `mkdirs` 用 `pathRange()`（保留 `missing` 路徑）就能一口氣建出 `mkdir -p` 需要的所有中間目錄（`Mkdirs.cc:32-52`）。同一個解析函式服務兩種語意，靠的是把「缺失位置」也當成回傳值的一部分。

### 6.3 ResolveResult 的三態 parent

```cpp
struct ResolveResult {
  std::variant<std::pair<InodeId, Acl>, Inode, DirEntry> parent;
  std::optional<DirEntry> dirEntry;
};
```

三種狀態代表「我對父目錄知道多少」，由 `loadParentAcl`（`PathResolve.cc:37-73`）決定：

| 狀態 | 何時產生 | 額外資訊 |
|---|---|---|
| `DirEntry` | 上一層解析剛好走過這個目錄的 dentry | 有 `dirAcl`，但沒有 `Directory::layout` / `parent` / `lock` |
| `pair<InodeId, Acl>` | AclCache 命中 | **只有 ACL**，其他一概不知 |
| `Inode` | cache 未命中，只好 `Inode::snapshotLoad` | 全部資訊 |

呼叫端按需升級：只要檢查權限就 `getParentAcl()`（零成本）；需要 `layout` 或 `checkLock` 就 `getParentInode(txn)`（`PathResolve.h:41-50`，可能觸發一次額外讀取）。

例如 `MkdirsOp` 必須拿完整 inode（要繼承 `layout` 與 `iflags`，`Mkdirs.cc:54-63`），而 `StatOp` 完全不需要（`Stat.cc:44-46`）。

### 6.4 AclCache 的三處寫入與一處失效

`AclCache`（`components/AclCache.h`）是 32 分片的 `folly::EvictingCacheMap<InodeId, {SteadyTime, Acl}>`，容量在 `MetaStore` 建構時定為 2M 筆（`store/MetaStore.h:90`，`aclCache_(2 << 20)`）。

**TTL 帶隨機抖動**（`AclCache.h:46`）：

```cpp
auto deadline = cached->timestamp + ttl * folly::Random::randDouble(0.8, 1.0);
```

注意抖動是 **0.8~1.0 倍**（只縮短，不延長）——保證快取絕不會活過設定的 `acl_cache_time`（預設 15s），同時把大量同時寫入的條目的失效時間打散，避免快取雪崩。

寫入點：`loadParentAcl` 在拿到 `DirEntry`（:48-50）或 `Inode`（:61-63）時各寫一次。**失效點只有一個**：`SetAttrOp::finish`（`ops/SetAttr.cc:84-89`）在 `uid/gid/perm/iflags` 任一改變且操作成功時呼叫 `aclCache().invalid(id)`。

**這裡有個明確的不一致窗口**：AclCache 是**進程本地**的，`SetAttrOp::finish` 只失效**本機**的快取。其他 meta server 上的快取要等 TTL 過期。所以 `chmod` 在多 meta 部署下最多有 15 秒的傳播延遲。`PathResolve.cc:293-296` 的 TODO 註解坦承了這個取捨：

```cpp
// todo: For each directory, we need load it's Inode to check permission,
// this adds performance overhead to path resolution.
// A simple way to mitigate this is cache Inode permission information,
// if we can tolerate chmod doesn't make effect for several seconds.
```

而且 `BatchedOp::setAttr`（`BatchOperation.cc:305-341`）這條批次路徑**完全沒有失效 AclCache**——因為批次的 setAttr 是 by-inodeId 的，通常來自 client 的 `fchmod`，走的是 `runInBatch`。這是一個實際存在的（雖然影響有限的）不對稱。

### 6.5 環路與深度防護

- **符號連結**：兩個獨立計數器。`depth_` 限制巢狀深度（`max_symlink_depth = 4`），`symlinkCnt_` 限制單次解析中展開的總次數（`max_symlink_count = 10`），任一超限回 `kTooManySymlinks`（`PathResolve.cc:341-348`）。兩個維度都要限，是因為 `a→b→c→d` 是深度 4，而 `/x/y/z` 每段都是 symlink 是深度 1、次數 3。
- **目錄環**：`Inode::loadAncestors`（`store/Inode.cc:196-224`）用 `std::set<InodeId>` 偵測重複祖先，發現就回 `kInconsistent, "directory tree contains loop"`。這是往上爬（`rename` 檢查、`getRealPath`）時的防護。
- **`max_directory_depth = 64`**（`base/Config.h:70`）：**這個設定項在整個 codebase 裡從未被讀取**。是遺留的死設定。實際的深度限制來自 `Path` 本身與 FDB 交易 5 秒上限。
- **root 的 parent 是自己**（`MetaStore.cc:48-51` 的註解：「root Inode's parent is itself, this simplify path resolution: eg /../../../a -> /a」）。`gcRoot` 同理。`loadAncestors` 的終止條件就是 `parent == id`（`Inode.cc:216-218`）。

### 6.6 監控埋點

`PathResolveOp` 的解構子（`PathResolve.cc:87-92`）上報 `meta_server.path_components` 與按 uid 分組的版本。這讓維運能看出「某個使用者的路徑特別深」——路徑深度直接決定 FDB 讀取次數，是延遲的主要來源。`pathComponents_` 不計 `"."`（`:306`）。

---

## 7. 逐操作執行路徑

本節逐一拆解每個操作：開幾次交易、交易裡讀寫哪些 key、衝突集怎麼加、錯誤怎麼分類。

符號約定：
- `[S]` = snapshot 讀（不進衝突集）
- `[R]` = 一般讀（進衝突集）
- `[C]` = 顯式 `addReadConflict`
- `[W]` = `set`
- `[D]` = `clear`

### 7.1 `stat` — 最單純的唯讀路徑

`store/ops/Stat.cc:31-65`。交易數：**1**（唯讀，不 commit）。

```
StatOp::run(txn)
 └─ resolve(txn, user).inode(path, flags, checkRefCnt = !allow_stat_deleted_inodes)
      ├─ path.path 為空 或 (empty && AT_EMPTY_PATH)  → [S] INOD:{path.parent}
      └─ 否則 → dirEntry(path, flags)  ──▶ 逐層 [S] DENT / [S] INOD（ACL）
                 └─ entry->snapshotLoadInode(txn) → [S] INOD:{entry.id}
```

`checkRefCnt` 的語意（`PathResolve.cc:106-109`）：若 `inode.nlink == 0` 則回 `kNotFound`。預設 `allow_stat_deleted_inodes = true`，也就是**允許 stat 已刪除的 inode**——因為刪除是把 dentry 搬進垃圾桶、inode 的 `nlink` 減到 0 但實體還在（元資料層報告 §9），而 FUSE client 可能還持有 fd。

`snapshotLoadInode` 會做兩道一致性檢查（`DirEntry.cc:179-204`）：
- `checkInodeExists`：dentry 存在但 inode 不見了 → `kInconsistent`（CRITICAL 級日誌）。`.` / `..` 這種合成 dentry 例外，回 `kNotFound`。
- `checkInodeType`：dentry 記的 type 與 inode 實際 type 不符 → `kInconsistent`。

這兩個檢查是**元資料損壞的主要偵測點**。它們不修復，只報告。

### 7.2 `batchStat` / `batchStatByPath` — 一個交易多次讀

`store/ops/Stat.cc:67-142`，兩者共用 `BatchStatOp<Req, Rsp>` 模板。交易數：**1**。

```cpp
size_t concurrent = byPath ? batch_stat_by_path_concurrent(4) : batch_stat_concurrent(8);
auto exec = co_await folly::coro::co_current_executor;
[[maybe_unused]] auto guard = createBatchContext();     // ← byPath 才建 BatchContext
while (iter != vector().end()) {
  std::vector<folly::SemiFuture<TaskResultType>> tasks;
  while (iter != end && tasks.size() < concurrent) {
    tasks.push_back(byPath ? resolve(txn, *iter).scheduleOn(exec).start()
                           : Inode::snapshotLoad(txn, *iter).scheduleOn(exec).start());
    iter++;
  }
  auto results = co_await folly::coro::collectAllRange(std::move(tasks));
  ...
}
```

三個設計差異：

| | `batchStat` | `batchStatByPath` |
|---|---|---|
| 輸入 | `vector<InodeId>` | `vector<PathAt>` |
| 回應元素型別 | `optional<Inode>`（不存在 = nullopt） | `Result<Inode>`（每條路徑各自帶錯誤） |
| 併發度 | 8 | 4 |
| BatchContext | **不建** | **建**（`createBatchContext()` 回 `folly::Unit{}` vs `BatchContext::create()`） |
| 單項失敗 | 整批失敗 | `ErrorHandling::success` 的錯誤逐項保留 |

**為什麼只有 byPath 版本建 BatchContext**：`batchStat` 是純 inode 點查，一批裡沒有重複 key（有也是 client 的問題），去重無意義。`batchStatByPath` 則不同——`/data/train/a.bin` 與 `/data/train/b.bin` 共享 `/`、`/data`、`/data/train` 三層祖先，去重能把 3N 次讀降到 3+N 次。BatchContext 掛在 `folly::RequestContext` 上（`store/BatchContext.h:63-73`），透過 `folly::ShallowCopyRequestContextScopeGuard` 讓 `scheduleOn(exec)` 出去的子協程自動繼承（詳見元資料層報告 §12）。

**併發度差異的原因**：byPath 每項要做完整路徑解析（多次讀），byId 每項只有一次讀。同樣的 FDB 併發預算下，byPath 的併發度要調低。

`byPath` 版本的錯誤處理（`Stat.cc:125-133`）：

```cpp
if (result.hasError() && (!byPath || !ErrorHandling::success(result))) {
  CO_RETURN_ERROR(result);      // 只有「非預期錯誤」才讓整批失敗
}
if constexpr (byPath) inodes.push_back(result);   // 預期內錯誤（如 kNotFound）逐項保留
```

也就是 `batchStatByPath` 對「這條路徑不存在」是容忍的，對「FDB 交易衝突」則整批重試。這正好對應 `readdirplus` / 批次預取的語意。

### 7.3 `list` — 一次 range scan

`store/ops/List.cc:32-60`。交易數：**1**。

```
ListOp::run
 ├─ resolve(...).inode(path, AT_SYMLINK_FOLLOW, checkRefCnt=true)
 ├─ 檢查 isDirectory() → kNotDirectory
 ├─ 檢查 acl.checkPermission(user, READ) → kNoPermission
 └─ DirEntryList::snapshotLoad(txn, inode.id, prev, limit, status, batch_stat_concurrent)
      ├─ [S] getRange( DENT:{id}{prev} , prefixListEndKey(DENT:{id}) , limit )
      └─ if (status) 併發 [S] INOD:{每個 entry.id}   ← readdirplus
```

注意 `list` 檢查的是**目錄 inode 的 READ 權限**（不是 EXEC）。EXEC 已經在路徑解析時對每一層檢查過了。`prev` 是游標（entry name），`limit <= 0` 時取 `list_default_limit = 128`。

`DirEntryList::snapshotLoad` 的 range 邊界（`DirEntry.cc:276-288`）：`beginKey = packKey(parent, prev)`、`endKey = prefixListEndKey(packKey(parent, ""))`，兩端都 `inclusive = false`。所以 `prev` 本身被排除，天然形成「上次回傳的最後一個之後」的游標語意。

`loadInodes` 的併發載入用 `batch_stat_concurrent`（預設 8）分批（`DirEntry.cc:254-271`）。**這裡任一 inode 載入失敗就整個 list 失敗**——與 `batchStatByPath` 的容忍策略不同。因為 dentry 存在而 inode 不存在是 `kInconsistent`，不該吞掉。

### 7.4 `create` — 三段式，最複雜的路徑

`create` 的完整路徑橫跨兩個檔案與兩條分派路徑。

```
MetaOperator::create(req)                             MetaOperator.cc:341-369
 │
 ├─ AUTHENTICATE(req.user); req.valid()
 │
 ├─ if (req.path.path->has_parent_path())        ← 帶路徑的 create
 │    │
 │    ├─【交易 1】runOp(&MetaStore::tryOpen, req)      → OpenOp<CreateReq, CreateRsp>
 │    │     ├─ 若檔案已存在 → 完整走 open 語意，直接回傳 ✓
 │    │     └─ 若不存在 → OpenOp 就地把 req.path 改寫成
 │    │            PathAt(resolveResult->getParentId(), filename)     Open.cc:62-65
 │    │            並回 kNotFound
 │    │
 │    ├─ if (result.hasValue() || req.path.path->has_parent_path()) return result;
 │    │       └─ 第二個條件：若 req.path 沒被改寫（表示連父目錄都沒解析出來），
 │    │          直接把錯誤回給 client
 │    │
 │    └─ 若被改寫成功 → req 現在是 by-parent-inode 形式，繼續往下
 │
 ├─ node = distributor_->getServer(req.path.parent)
 │
 ├─ node == self →【交易 2】runInBatch<CreateReq, CreateRsp>(parentId, req)
 └─ node != self → forward_->forward<CreateReq, CreateRsp>(node, req)
```

**「先試 open 再 create」是整個 meta server 最重要的效能設計。**

理由：訓練場景下絕大多數 `open(O_CREAT)` 打在**已存在**的檔案上（重複跑同一份資料集）。若直接走 create 路徑，就得先按 parent inode 排隊進批次佇列，而**同一個目錄下的所有檔案共用一個批次槽**——目錄成為序列化瓶頸。先用一個唯讀交易試 open：命中就完全不排隊、可以無限併發；沒命中才付出排隊成本。

**`req` 被就地修改是有意的副作用**。`MetaStore::tryOpen(CreateReq &req)` 與 `MetaStore::open(OpenReq &req)` 的參數是**非 const 引用**（`store/MetaStore.h:114-116`），而其他所有 `MetaStore::xxx(const XxxReq &)` 都是 const。這個型別上的不對稱就是在標示「這兩個會改 req」。`MetaOperator.cc:353-357` 還加了一道 `req.valid()` 的事後檢查，不通過就 `XLOG(DFATAL)` + `kFoundBug`。

#### 7.4.1 批次 create 的內部

`BatchedOp::create(txn, inode)`（`BatchOperation.cc:343-397`）處理**同一個父目錄下的一批 create**：

```cpp
if (!inode.isDirectory()) { 全部回 kNotDirectory; }

folly::Synchronized<uint32_t> chainAllocCounter(inode.asDirectory().chainAllocCounter);

if (creates_.size() == 1) { 單條路徑，直接處理 }

// 多條：先按檔名分組（同名的 create 要一起處理）
std::multimap<std::string, WaiterRef<CreateReq, CreateRsp>> map;
for (auto &waiter : creates_) map.insert({name->string(), waiter});

// 每組一個協程，最多 8 組併發
for (auto begin = map.begin(), end = std::next(begin); begin != map.end(); begin = end) {
  while (end != map.end() && end->first == begin->first) end++;    // 找出同名區間
  tasks.push_back(create(txn, inode, chainAllocCounter, ibegin, iend).scheduleOn(exec).start());
  if (tasks.size() >= 8 || end == map.end()) { co_await collectAllRange(...); }
}
dirty |= SetAttr::update(inode.asDirectory().chainAllocCounter, *chainAllocCounter.rlock());
```

**用 `multimap` 按檔名分組是必要的**：兩個 client 同時 `create("/d/x")`，若各自獨立處理，兩個都會發現 dentry 不存在、各自分配 inodeId、各自 `set` 同一把 dentry key——後寫的贏，先寫的 client 拿到一個永遠不會被任何 dentry 指向的孤兒 inode。分組後（`BatchOperation.cc:399-444`）：

```cpp
auto entry = co_await DirEntry::snapshotLoad(txn, inodeId_, name);   // [S] 查一次
if (entry->has_value()) {
  auto inode = co_await entry->value().snapshotLoadInode(txn);
  co_return (co_await openExists(txn, *inode, **entry, begin, end)).then(...);  // 全組走 open
}
for (auto iter = begin; iter != end; iter++) {
  auto result = co_await create(txn, parent, chainAllocCounter, waiter.req);
  if (成功) {
    waiter.result = CreateRsp(inode, false);  waiter.newFile = true;
    // 這一組剩下的人全部改走 openExists，共用剛建好的 inode
    co_return (co_await openExists(txn, inode, entry, std::next(iter), end)).then(...);
  }
}
```

也就是**組內第一個成功建檔的人建檔，其餘的人「打開」它**。`newFile` 旗標讓事件日誌只為真正建檔者記一筆 Create 事件（`BatchOperation.cc:636-645`）。

#### 7.4.2 單次 create 的完整 KV 操作

`BatchedOp::create(txn, parent, chainAllocCounter, req)`（`BatchOperation.cc:446-522`）：

```
 1. req.valid()、req.path.validForCreate()
 2. parent.nlink == 0 → kNotFound（父目錄已刪）
 3. parentAcl.checkPermission(user, WRITE) → kNoPermission
 4. parent.asDirectory().checkLock(req.client) → kNoLock
 5. layout = req.layout ?: parent.asDirectory().layout
 6. layout 非空 → chainAlloc().checkLayoutValid(layout)      ← 查 mgmtd routing，不碰 FDB
    layout 為空 → chainAlloc().allocateChainsForLayout(layout[, chainAllocCounter])
                    └─ parent.acl.iflags & FS_CHAIN_ALLOCATION_FL 決定用目錄私有計數器
                       還是進程全域 round-robin（見 §13.4）
 7. newChunkEngine = config.enable_new_chunk_engine() || (parent.acl.iflags & FS_NEW_CHUNK_ENGINE)
 8. inodeId = allocateInodeId(txn, newChunkEngine)
      └─ [R] INOD:{newId}  ← inodeId_check_unique 時做一次防禦性讀取（Operation.h:102-112）
 9. entry = DirEntry::newFile(parentId, name, inodeId);  entry.uuid = req.uuid
10. inode = Inode::newFile(inodeId, Acl(uid, gid, perm & ALLPERMS), layout, now())
11. dynamic_stripe && req.dynStripe → inode.asFile().dynStripe = min(dynamic_stripe_initial, stripeSize)
12. parentAcl.perm & S_ISGID → inode.acl.gid = parentAcl.gid   （BSD 語意）
13. [C] INOD:{parentId}       ← 防止父目錄被併發刪除
14. [C] DENT:{parentId}{name} ← 防止併發建立同名項
15. [W] DENT:{parentId}{name}
16. [W] INOD:{inodeId}
17. req.session && 非唯讀 → [W] INOS:{inodeId}{sessionId}
```

**步驟 13/14 的兩個 `addReadConflict` 是這整套 snapshot-讀架構的核心補償。** 路徑解析用 snapshot 讀了父目錄的 dentry 與 inode，什麼都沒進衝突集。若此時另一個交易刪掉了父目錄，我們的 create 仍會提交成功，造出一個掛在已刪目錄下的孤兒檔案。顯式把兩把 key 加進衝突集後，任何對它們的併發寫入都會讓本交易衝突重試。

註解也寫得很白（`BatchOperation.cc:506-510`）：
```cpp
// NOTE: add parent inode and dirEntry into read conflict set.
// add parent inode into read conflict set to prevent parent is removed concurrently
// add directory entry into read conflict set to prevent concurrent create
```
這段註解在 `Open.cc:220-225`、`Mkdirs.cc:66-70`、`Symlink.cc:58-62`、`HardLink.cc:88-92` 幾乎逐字重複——**這是一個被複製了五次的關鍵不變式**，也說明它容易被遺漏。

**`allocateInodeId` 的防禦性讀取**（`store/Operation.h:94-118`）值得注意：分配器理論上保證唯一，但仍然預設會 `[R] INOD:{newId}` 確認沒撞到。`inodeId_check_unique = true`、`inodeId_abort_on_duplicate = false`——撞到就 `DFATAL` + 回 `kInodeIdAllocFailed`（可重試）。這多花一次 FDB 讀取換取「絕不覆蓋既有 inode」。而且這次讀取是 `Inode::load`（非 snapshot），**會進衝突集**——所以如果有人在提交前寫了這個 inodeId，本交易會衝突。

### 7.5 `open` — 動態唯讀性

`store/ops/Open.cc:30-298`，`OpenOp<Req, Rsp>` 模板同時服務 `open` 與 `create` 的 tryOpen 階段。交易數：**1**。

```cpp
bool isReadOnly() final {
  return !req_.session.has_value() && req_.flags.accessType() == AccessType::READ
         && !req_.flags.contains(O_TRUNC) && !req_.flags.contains(O_CREAT);
}
```

**唯讀性是每個請求動態判定的**，不是型別決定的。純讀開檔完全不寫 FDB（`OperationDriver` 就不會 commit，也能開 GRV cache）。這是最常見的路徑，值得這個複雜度。

`BEGIN_WRITE()` 宏（`Open.cc:18-24`）在需要寫入時把 `IReadOnlyTransaction&` 動態轉成 `IReadWriteTransaction&`，並先斷言 `!isReadOnly()`：

```cpp
#define BEGIN_WRITE()                                                    \
  if (this->isReadOnly()) {                                              \
    auto msg = fmt::format("Op {}{} shouldn't be readonly!", ...);       \
    XLOG(DFATAL, msg);                                                   \
    co_return makeError(MetaCode::kFoundBug, std::move(msg));            \
  }                                                                      \
  auto &rwTxn = dynamic_cast<IReadWriteTransaction &>(txn);
```

`openExistsFile` 的完整判定順序（`Open.cc:108-171`）：

```
 1. O_DIRECTORY  → kNotDirectory
 2. 非唯讀 && (iflags & FS_IMMUTABLE_FL) → kNoPermission
 3. acl.checkPermission(user, flags.accessType())
 4. 唯讀 && hasHole() && check_file_hole → kFileHasHole
 5. O_TRUNC && 有 entry → replaceExistsFile()   ← 見 7.5.1
 6. 非唯讀 && user.uid != acl.uid && (perm & 07000) → 清 SUID/SGID/sticky，dirty = true
 7. 有 session && 非唯讀 → [W] INOS:{inodeId}{sessionId}
    並且若 !req.dynStripe 而 inode 有 dynStripe → 設為 0（關閉動態 stripe），dirty = true
 8. dirty → [C] INOD:{id}; [W] INOD:{id}
 9. 回 Rsp(inode, otrunc)
```

步驟 6 是 POSIX 語意：非擁有者寫入檔案要清掉 set-uid/set-gid 位（`static_assert(sbits == 07000)` 把假設編譯期化）。

步驟 7 的 dynStripe 邏輯：若 client 不支援動態 stripe（`req.dynStripe == false`）卻打開一個處於動態 stripe 狀態的檔案，就**永久關閉**該檔的動態 stripe（設為 0）。因為舊 client 不會在寫超出 dynStripe 範圍時呼叫 `extendStripe`，繼續留著會導致查長度時漏掉 chunk。這是向後相容的降級。

#### 7.5.1 `O_TRUNC` 的「換 inode」優化

`replaceExistsFile`（`Open.cc:173-210`）：

```cpp
if (!otrunc_replace_file() || inode.nlink != 1 ||
    inode.asFile().length < otrunc_replace_file_threshold())  return false;
if (co_await FileSession::checkExists(txn, inode.id))         return false;   // 有人開著

// 把舊 inode 整個丟進垃圾桶
CO_RETURN_ON_ERROR(co_await gcManager().removeEntry(txn, entry, old, GcInfo{uid, entry.name}));
// 分配新 inodeId，繼承舊的 acl 與 layout
auto inodeId = co_await allocateInodeId(txn, false);
entry = DirEntry::newFile(entry.parent, entry.name, *inodeId);
inode = Inode::newFile(*inodeId, inode.acl, inode.asFile().layout, now());
CO_RETURN_ON_ERROR(co_await createInodeAndEntry(txn, entry, inode, old));
CO_RETURN_ON_ERROR(co_await createSession(txn, inode, req_.flags));
```

**這是把 `O_TRUNC` 從 O(檔案大小) 變成 O(1) 的關鍵。** 傳統做法是刪掉所有 chunk，一個 1 TB 的檔案要刪兩百萬個 chunk，client 得等很久（`TruncateRsp::finished` 那套分批介面就是為此設計的，現已廢棄）。換成「舊 inode 丟垃圾桶 + 建新 inode」後，`O_TRUNC` 只是三次 KV 寫入，chunk 由 GcManager 在背景慢慢刪。

四個前置條件缺一不可：
- `nlink == 1`：有 hard link 的話換 inode 會讓其他連結指向舊內容——語意錯誤。
- `length >= 1GB`（`otrunc_replace_file_threshold`）：小檔直接刪 chunk 更划算，換 inode 會浪費一個 inodeId 並產生 GC 任務。
- **沒有任何 session**：若別的 client 正開著這個檔案在寫，換掉 inode 會讓它的寫入落到一個已進垃圾桶的 inode 上。`FileSession::checkExists` 是 `IReadWriteTransaction` 版本，會在**沒找到 session 時**把整個 `INOS:{inodeId}` 範圍加進讀衝突集（`store/FileSession.cc:158-170`）：

```cpp
if (!exists) {
  auto prefix = SessionByInode::prefixOf(inodeId);
  auto end = kv::TransactionHelper::prefixListEndKey(prefix);
  CO_RETURN_ON_ERROR(co_await txn.addReadConflictRange(kv::TransactionHelper::keyAfter(prefix), end));
}
```

  **只在「不存在」時加範圍衝突**，是因為只有「不存在」這個結論需要被保護——若併發有人建了 session，我們必須衝突重試。若已經存在，我們直接放棄替換，結論不受後續變化影響。這是精準的衝突集控制。

- `prevCreatedInodeId_` 記錄本次交易建立的 inode（`Open.cc:229`），下次重試時 `openExists` 開頭會檢查（`:77-83`）：若 `prevCreatedInodeId_ == inode.id`，代表這個 inode 是我上一次嘗試建的（交易可能提交成功但回應丟了），直接回成功。這是交易重試場景下的**進程內冪等**，不需要 IDEM 表。

### 7.6 `close` / `sync` — 批次合併的主場

兩者都必走 `runInBatch`，分派鍵為 `req.inode`。交易數：**每批 1 次**（外加可能的 storage 查詢，見 §14）。

`BatchedOp::run(txn)`（`BatchOperation.cc:57-105`）是批次交易的主體：

```
 1. distributor().checkOnServer(txn, inodeId_)                       ← 見 §11
      ├─ [R] "\xff/metadataVersion"（進衝突集！）
      ├─ 若不是我負責 → kBusy（可重試）
      └─ 回傳 (ok, versionstamp)
 2. [S] INOD:{inodeId_}   ← 注意是 snapshot 讀
 3. 若是檔案：長度一致性 sanity check
      if (versionstamp != versionstamp_) { currLength_ = 當前長度; nextLength_ = nullopt; versionstamp_ = versionstamp; }
      if (currLength_ != 當前長度 && nextLength_ != 當前長度) → DFATAL + kBusy
 4. syncAndClose(txn, inode)  → dirty1     ← 合併所有 sync/close，移除 session，回寫長度
 5. setAttr(txn, inode)       → dirty2
 6. create(txn, inode)        → dirty3
 7. if (dirty1||dirty2||dirty3) { [C] INOD:{id}; [W] INOD:{id}; }
 8. 回 inode
```

**步驟 3 的 sanity check 是分散式正確性的自檢**。註解說明得很清楚（`BatchOperation.cc:69`）：「if we hold the lock, and versionstamp not changed file length shouldn't changed」。邏輯是：Distributor 保證這個 inode 只由我處理，只要成員表版本沒變，就不該有別人改它的長度。如果發現長度變了，代表有另一台 meta server 也認為自己負責這個 inode——這是嚴重的分裂。處理方式是 `XLOGF(DFATAL, ...)` + 回 `kBusy` 讓它重試（在 release build 下不會 abort，優雅降級）。

`versionstamp_` / `currLength_` / `nextLength_` 三個成員存活於交易重試之間。當成員表版本變了（`versionstamp != versionstamp_`），就重置基準值——因為成員關係變了，之前的「只有我能改」假設不再成立。

`syncAndClose`（`BatchOperation.cc:107-187`）的合併邏輯：

```
hintLength = VersionedLength{0, 0}            ← 初始值，會被 mergeHint 逐步抬高
updateLength = false;  truncate = false;  dirty = false

for waiter in syncs_:                                          BatchOperation.cc:189-227
    ├─ 校驗：req.inode 必須 == inodeId_（否則 DFATAL + kFoundBug）
    ├─ 校驗：(updateLength||truncated||lengthHint) 但不是檔案 → kNotFile
    ├─ 校驗：lengthHint->truncateVer > 當前 truncateVer → DFATAL + kFoundBug
    ├─ dirty |= SetAttr::update(atime, req.atime, granularity, cmp=true)
    ├─ dirty |= SetAttr::update(mtime, req.mtime, granularity, cmp=true)
    ├─ req.truncated → dirty |= update(ctime, ...)
    ├─ updateLength |= req.updateLength
    ├─ req.updateLength → hintLength = VersionedLength::mergeHint(hintLength, req.lengthHint)
    └─ truncate |= req.truncated

for waiter in closes_:                                         BatchOperation.cc:229-258
    ├─ 同樣的校驗與 atime/mtime 合併
    ├─ updateLength |= req.updateLength;  合併 hint
    └─ req.session → sessions.push_back(FileSession::create(inode.id, *req.session))

if (truncate) { hintLength = nullopt; updateLength = true; }   ← truncate 時忽略所有 hint

for session in sessions: [D] INOS:{inodeId}{sessionId}

if (!updateLength) return dirty;
newLength = queryLength(inode, hintLength, truncate)            ← 可能打 storage
if (newLength != 當前) {
    XLOGF_IF(FATAL, newLength 倒退, ...)                        ← 長度絕不允許倒退
    update(mtime, now, cmp=true)
    if (truncateVer 變了) update(ctime, now, cmp=true)
    inode.asFile().setVersionedLength(newLength);  dirty = true
}
```

**`SetAttr::update(v, nv, resolution, cmp)` 的 `cmp` 參數**（`ops/SetAttr.h:144-158`）：`cmp=true` 表示「只有新值比舊值大才更新」。sync/close 路徑全部用 `cmp=true`——因為批次裡的多個請求來自不同 client、帶著不同的時間戳，取 max 才是正確的合併。而 `SetAttr::apply` 的 utimes 路徑用 `cmp=false`（`SetAttr.h:126-127`），因為 `utimes()` 系統呼叫的語意就是**強制設定**，允許把時間往回調。同一個函式靠一個布林參數服務兩種相反語意。

`castGranularity(resolution)` 把時間截斷到 `time_granularity = 1s`。這是**降低寫入量的關鍵**：同一秒內的多次 sync 若時間戳相同，`update` 回 false，`dirty` 保持 false，整個交易就不用寫 inode。

**Waiter 的結果填寫時機**：注意 `sync()` / `close()` 在遇到單項錯誤時是 `waiter.get().result = makeError(...)` 而**不是**讓整批失敗（`BatchOperation.cc:120-132`）。也就是**批次內一項無效不影響其他項**。但若是交易級錯誤（`CO_RETURN_ON_TXN_ERROR` 宏，`BatchOperation.cc:42-48`）則整批回滾重試。這個宏只在 `create` 路徑用：

```cpp
#define CO_RETURN_ON_TXN_ERROR(result)                                                    \
  if (_r.hasError() && StatusCode::typeOf(_r.error().code()) == StatusCodeType::Transaction) \
    CO_RETURN_ERROR(_r);
```

`BatchedOp::retry`（`BatchOperation.cc:607-621`）在每次重試前把**所有** waiter 的 `result` 重置為 `nullopt`——因為上一次嘗試填的結果可能基於已失效的讀取。

### 7.7 `setAttr` — 兩條路徑

`MetaOperator::setAttr`（`MetaOperator.cc:401-414`）按請求形式二選一：

```cpp
if (req.path.path) co_return co_await runOp(&MetaStore::setAttr, req);   // 帶路徑 → 單獨交易
auto node = distributor_->getServer(req.path.parent);                    // 純 inodeId → 批次
```

**為什麼帶路徑就不批次**：批次的分派鍵必須是 inodeId，而帶路徑的請求要先解析才知道目標 inode——解析本身就是一次交易級的工作，沒法在批次組裝階段做。而 `fchmod(fd)` / `futimens(fd)` 這類 by-fd 的呼叫直接帶 inodeId，可以批次。

`SetAttr::check`（`ops/SetAttr.h:27-107`）是完整的 POSIX 權限規則實作：

| 檢查項 | 規則 | 對應 man page |
|---|---|---|
| tree root | `isTreeRoot() && (perm||uid||gid)` → `kNoPermission` | 3FS 自訂 |
| `FS_CHAIN_ALLOCATION_FL` | 從 0→1 需 `iflags_chain_allocation = true` | 3FS 自訂 |
| `FS_NEW_CHUNK_ENGINE` | 從 0→1 需 `iflags_chunk_engine = true` | 3FS 自訂 |
| 其他 iflags | root，或（擁有者 && 只動 `FS_HUGE_FILE_FL`[+`FS_IMMUTABLE_FL` if `allow_owner_change_immutable`]） | `chattr` |
| `perm` | root 或擁有者 | `chmod(2)` / CAP_FOWNER |
| `uid` | **僅 root** | `chown(2)` / CAP_CHOWN |
| `gid` | root，或（擁有者 && `user.inGroup(新 gid)`） | `chown(2)` |
| `atime`/`mtime` = NOW | 有寫權限 ‖ 擁有者 ‖ root | `utimensat(2)` |
| `atime`/`mtime` = 具體值 | 擁有者 ‖ root | `utimensat(2)` |
| `layout` | 必須是目錄 + WRITE 權限 | 3FS 自訂 |
| `dynStripe` | 必須是檔案 | 3FS 自訂 |

註解直接引用了 man page 原文（`SetAttr.h:53-56`、`:58-61`、`:69-78`），把規範來源固定在程式碼裡。

`SETATTR_TIME_NOW` 用 `UtcTime(0)` 表示 `UTIME_NOW`（`SetAttr.h:75` 註解）。

**目錄 ACL 改變時的雙寫**（`ops/SetAttr.cc:57-73` 與 `BatchOperation.cc:322-338`，兩處各實作一次）：

```cpp
if (inode.isDirectory() && inode.acl != oldAcl && inode.id != InodeId::root()) {
  // 需要同步更新 dentry 裡反正規化的 dirAcl
  if (!entry.has_value() || entry->name == "." || entry->name == "..")
    entry = co_await inode.snapshotLoadDirEntry(txn);    // 反查自己的 dentry
  entry->dirAcl = inode.acl;
  [C] DENT:{parent}{name};  [W] DENT:{parent}{name};
}
```

`Inode::snapshotLoadDirEntry`（`store/Inode.cc:157-194`）優先用 `Directory::name` 直接點查；若 `name` 為空（舊資料），就退化成**掃整個父目錄找 id 相符的 entry**——O(目錄大小)。`SetAttrOp` 與 `BatchedOp::setAttr` 都會順手把空的 `name` 補回去（`SetAttr.cc:64-67`、`BatchOperation.cc:332-334`），把這個退化路徑逐步消除。

`extendStripe` 的成長邏輯（`SetAttr.h:129-139`）：

```cpp
auto growth = std::max(2u, stripeGrowth);
auto dynStripe = inode.asFile().dynStripe;
while (dynStripe < std::min(req.dynStripe, layout.stripeSize))
  dynStripe = std::min(dynStripe * growth, layout.stripeSize);
```

倍增而非設成請求值，是為了讓 `extendStripe` 的呼叫次數是 O(log)：client 每寫超出當前 dynStripe 就呼叫一次，倍增後很快就到達 `stripeSize` 上限。

### 7.8 `mkdirs` — 一個交易建整條路徑

`store/ops/Mkdirs.cc:27-128`。交易數：**1**。

```
 1. resolve(...).pathRange(req.path)      ← 回傳 ResolveRangeResult{parent, dirEntry, missing}
 2. missing 為空 → 目標已存在
      ├─ dirEntry->uuid == req.uuid → 冪等命中，回傳現有 inode（:39-45）
      └─ 否則 kExists
 3. missing 長度 > 1 且 !recursive → kNotFound
 4. parent = getParentInode(txn)   ← 需要完整 inode（要 layout 與 iflags）
 5. parent.acl.checkPermission(WRITE); parent.asDirectory().checkLock(client)
 6. layout = req.layout ?: parent.asDirectory().layout
 7. acl = Acl(uid, gid, perm & ALLPERMS, parent.acl.iflags & FS_FL_INHERITABLE)
 8. parent.acl.perm & S_ISGID → acl.gid = parent.acl.gid; acl.perm |= S_ISGID
 9. [C] INOD:{parentId};  [C] DENT:{parentId}{missing 第一段}
10. for 每個 missing 段:
      ├─ 檔名不能是 "." / ".." → kInvalidArg
      ├─ inodeId = allocateInodeId(txn, false)      ← 目錄永遠用舊 chunk engine
      ├─ chainAlloc().checkLayoutValid(layout)
      ├─ [W] DENT:{parentId}{name}  （entry.uuid = req.uuid）
      ├─ [W] INOD:{inodeId}
      └─ parentId = inodeId
```

**衝突集只加在第一段**（步驟 9）。後面每一段的父目錄都是本交易剛建立的，不可能被併發修改——FDB 的交易隔離已經保證了。這是精準的衝突集控制，避免無謂的衝突。

**`entry.uuid = req.uuid` 加在每一段**（`:99`），所以 `mkdir -p /a/b/c` 建出的三個 dentry 都帶同一個 uuid。重試時 `pathRange` 找到 `/a/b/c` 已存在且 uuid 相符，就直接回成功。

**`FS_FL_INHERITABLE` 只包含兩個旗標**（`FS_CHAIN_ALLOCATION_FL` 與 `FS_NEW_CHUNK_ENGINE`，元資料層報告 §5.2）。`FS_IMMUTABLE_FL` 不繼承——否則在 immutable 目錄下建的子目錄也會 immutable，無法管理。

`FAULT_INJECTION_SET_FACTOR(std::distance(curr, end))`（`:86`）：故障注入的機率與要建的目錄層數成正比，讓測試更容易命中「建到一半失敗」的場景。同樣的手法用在 `PathResolve.cc:241`（按剩餘路徑段數）與 `Inode.cc:199`（固定 4）。

### 7.9 `symlink` — 最簡單的寫操作

`store/ops/Symlink.cc:24-84`。交易數：**1**。

```
 1. resolve(...).path(req.path, AT_SYMLINK_NOFOLLOW)
 2. dirEntry 已存在 → uuid 相符則冪等回傳，否則 kExists
 3. getParentInode → checkPermission(WRITE) → checkLock(client)
 4. inodeId = allocateInodeId(txn, false)
 5. [C] INOD:{parentId};  [C] DENT:{parentId}{name}
 6. [W] DENT:{parentId}{name};  [W] INOD:{inodeId}
```

`Inode::newSymlink`（`store/Inode.h:43-46`）權限寫死 0777 且註解說明「permission of symlink is never used, and won't changed」——符合 Linux 語意（symlink 本身沒有權限，解析時檢查目標）。

### 7.10 `hardLink` — 唯一會改 nlink 的正向操作

`store/ops/HardLink.cc:30-121`。交易數：**1**。

```
 1. req.newPath.validForCreate()
 2. resolve(...).path(req.newPath, req.flags)
      └─ dirEntry 已存在 → uuid 相符則冪等，否則 kExists
 3. resolve(...).inode(req.oldPath, req.flags, checkRefCnt=true)   ← 目標必須 nlink > 0
 4. getParentInode → checkPermission(WRITE) → checkLock(client)
 5. target.acl.iflags & FS_IMMUTABLE_FL → kNoPermission
 6. 目標是目錄 → kIsDirectory                                     ← 禁止硬連結目錄
 7. inode.nlink == uint16_max → kNoPermission                      ← 溢位保護
 8. [C] INOD:{parentId};  [C] DENT:{parentId}{name};  [W] DENT:{parentId}{name}
 9. inode.nlink++;  update(ctime, now, cmp=true)
10. [C] INOD:{target.id};  [W] INOD:{target.id}                    ← read-modify-write 必須加衝突
```

**步驟 10 的 `addIntoReadConflict` 是必須的**，註解說明「add link count of inode, add inode into read conflict set since this is a read modify write」（`:95`）。`nlink++` 是典型的 read-modify-write：若兩個併發 hardLink 都讀到 nlink=1、都寫 nlink=2，就丟了一次計數。加進衝突集後，後提交者必定衝突重試。

`nlink` 是 `uint16_t`（元資料層報告 §5），上限 65535。步驟 7 直接拒絕而不是回捲——回捲會導致 inode 被提前回收。

### 7.11 `rename` — 見 §8（獨立章節）

### 7.12 `remove` — 兩種模式

`store/ops/Remove.cc:64-192`。交易數：**1**（遞迴刪除的實際回收由 GcManager 在後續多個交易中完成）。

```
 1. needIdempotent(): req.checkUuid() 且 (req.recursive || idempotent_remove)  → 大多數情況為 true
 2. 解析：有 path → resolve().path(AT_SYMLINK_NOFOLLOW)     ← remove 不跟隨 symlink
          無 path → resolve().byDirectoryInodeId(parent)     ← by-inode 刪除
 3. dirEntry 不存在 → kNotFound
 4. dirEntry->id.isTreeRoot() → kNoPermission（不准刪 root / gcRoot）
 5. req.inodeId 有值且不符 → kNotFound       ← client 帶 inodeId 做 ABA 防護
 6. parent.acl.checkPermission(WRITE); parent.asDirectory().checkLock(client)
 7. req.checkType → AT_REMOVEDIR 與實際型別必須匹配（rmdir vs unlink）
 8. [S] INOD:{entry.id}
 9. sticky bit：parent.perm & S_ISVTX 且 user 不是 parent 擁有者、不是 root、不是檔案擁有者 → kNoPermission
10. inode.iflags & FS_IMMUTABLE_FL → kNoPermission
11. 若是目錄：
      ├─ DirEntryList::checkEmpty(txn, entry.id)      ← [R] getRange limit=1（進衝突集！）
      ├─ 空 → 直接刪：[C] DENT; [C] INOD; [D] DENT; [D] INOD; 回傳 ✓
      ├─ 非空且 !recursive → kNotEmpty
      ├─ inode.acl.checkRecursiveRmPerm(user, recursive_remove_check_owner)
      ├─ recursive_remove_perm_check (1024) → DirEntryList::recursiveCheckRmPerm(...)  ← best-effort
      └─ loadAncestors 拼出 gcInfo.origPath
12. gcManager().removeEntry(txn, entry, inode, gcInfo)     ← 見 §13.1
```

**步驟 11 的 `checkEmpty` 用 `getRange`（非 snapshot）**（`DirEntry.cc:316-325`），會把整個範圍加進讀衝突集。這是必須的：若判定為空之後、提交之前有人在裡面建了檔案，直接刪掉目錄 inode 就會留下孤兒。用一般讀讓 FDB 自動處理。

**`recursiveCheckRmPerm` 是誠實的 best-effort**（`store/DirEntry.h:125-181`）。註解坦承：

```
For recursive remove and move to the trash, permission checks are required.
However, because the directory may be very large, we may not able to check permissions
for entire directory tree. This method is best effort.
```

實作是 BFS，帶三重上限：`limit`（總共檢查幾批，預設 1024）、`queue.size() < limit`（佇列深度）、`numEntries > 1024 && !foundDir`（掃了 1024 個都沒目錄就停）。第三個條件很聰明：**權限只掛在目錄上**（dentry 的 `dirAcl` 只有目錄才有），所以掃到一堆檔案就代表這層沒有更多子目錄要檢查，可以提早結束。

失敗時記錄到 `meta_server.recursive_check_rm_perm_failed`（按 uid 分組）——維運能看到哪個使用者的遞迴刪除被權限擋下。

**未被檢查到的無權限項目怎麼辦**：由 GcManager 在實際回收時再檢查一次（`GcManager.cc:478-502`），沒權限就搬進 `trash/gc-orphans/`（見 §13.1.4）。這是「快速路徑 best-effort + 慢速路徑完整檢查」的兩層設計。

`byDirectoryInodeId`（`PathResolve.cc:138-146`）用於 by-inode 刪除：先讀 inode，再反查它的 dentry，再讀父目錄 ACL。比 by-path 多一次讀取（反查 dentry），但避免了整條路徑解析。

### 7.13 `truncate` — 已廢棄的 RPC

`MetaOperator.cc:314-317`：

```cpp
CoTryTask<TruncateRsp> MetaOperator::truncate(TruncateReq req) {
  XLOGF(CRITICAL, "truncate is deperated, update client {}", req.client.hostname);
  co_return makeError(StatusCode::kNotImplemented, "truncate is deperated, update client");
}
```

**truncate 現在完全由 client 端驅動**（`src/client/meta/MetaClient.cc:880-925`）：

```
client.truncateImpl(userInfo, inode, targetLength)
 ├─ 迴圈：fop.removeChunks(targetLength, remove_chunks_batch_size, dynStripe, {})
 │     └─ 直接對 storage 發 removeChunks，分批直到 more == false
 │        （若某輪 removed == 0 但 more == true → CRITICAL + kFoundBug）
 ├─ 若 dynStripe 不足以覆蓋 targetLength → extendStripe(setAttr)
 ├─ fop.truncateChunk(targetLength)          ← 截斷最後一個 chunk
 └─ SyncReq(user, inode.id, updateLength=true, ..., truncated=true)   ← 通知 meta 重查長度
```

**設計意義**：truncate 的成本與檔案大小成正比，把它放在 meta server 上會長時間佔用一個 FDB 交易（5 秒必然超時）。搬到 client 後，meta 只需在最後收一個 `truncated=true` 的 sync——這個 sync 會讓 `truncateVer` 遞增（`BatchOperation.cc:300-302`），使所有舊的長度回報失效（元資料層報告 §7）。

**RPC ID 13 保留不刪**，因為 serde 的方法 ID 不能複用（見 §3）。舊 client 呼叫會收到 `kNotImplemented` 與一條 CRITICAL 日誌指出是哪台主機。

### 7.14 `pruneSession` — 標記待清理

`store/ops/PruneSession.cc:34-66`。交易數：**1**。

```cpp
for (auto sessionId : req_.sessions) {                   // 最多 32 個併發
  tasks.push_back(prune(txn, sessionId));
  if (tasks.size() == 32 || 是最後一個) co_await waitRequests();
}
// prune():
auto session = FileSession::createPrune(req_.client, sessionId);   // InodeId(-1) 哨兵
CO_RETURN_ON_ERROR(co_await session.store(txn));                   // [W] INOS:{-1}{sessionId}
```

**這個 RPC 不刪 session，只是登記「這些 session 該被清掉」**。因為 client 呼叫 `pruneSession` 時只有 sessionId，**不知道對應的 inodeId**——而 session 的 key 是 `INOS + inodeId + sessionId`，沒有 inodeId 就構造不出 key。

解法是寫進 `InodeId(-1)` 這個哨兵分區（`store/FileSession.h:66-72`），等 `SessionManager` 全量掃描時把它們比對出來（`SessionManager.cc:109-113`）。這是「用一張工作佇列換掉一個二級索引」——元資料層報告 §3.3 提到的被放棄的 `SessionByClient` 索引，就是被這個機制取代的。

注意 `pruneSession` **不鑑權**（`MetaOperator.cc:421-423`）。理由同 close/sync：client 崩潰恢復後需要能清理自己的殘留 session。

### 7.15 `lockDirectory` — 目錄獨佔鎖

`store/ops/LockDirectory.cc:21-60`。交易數：**1**。

```
 1. [R] INOD:{req.inode}          ← 一般讀，read-modify-write 需要衝突檢測
 2. acl.checkPermission(user, WRITE)
 3. !isDirectory() → kNotDirectory
 4. switch (action):
      TryLock:      已被別人持有 → kNoLock；否則 fallthrough
      PreemptLock:  設 lock = {req.client}，若有變化則 [W] INOD
      UnLock:       無鎖 → kNoLock；持有者不符 → kNoLock；否則 fallthrough
      Clear:        清除 lock，若有變化則 [W] INOD
```

**`switch` 刻意用 fallthrough**：`TryLock` 檢查完就掉進 `PreemptLock` 的設定邏輯；`UnLock` 檢查完就掉進 `Clear` 的清除邏輯。所以 `PreemptLock` = 不檢查的 `TryLock`，`Clear` = 不檢查的 `UnLock`。用四個 case 表達「檢查 × 動作」的 2×2 矩陣。

鎖的持有者是 `ClientId`（`Directory::Lock{client}`），檢查函式是 `Directory::checkLock`（`src/fbs/meta/Schema.h:295-300`），在 `create`、`mkdirs`、`symlink`、`hardLink`、`remove`、`rename` 六個寫操作的父目錄檢查裡被呼叫。**這是一個諮詢鎖（advisory lock）**：只擋住需要修改該目錄內容的操作，不擋讀取。

鎖**沒有 TTL**——存在 inode 裡，client 掛掉就永久鎖住。`PreemptLock` 與 `Clear` 就是給管理員（或新的 client）強制搶佔用的逃生門。

### 7.16 `statFs` — 完全不碰 FDB

`store/ops/StatFs.cc:21-24`：

```cpp
CoTryTask<StatFsRsp> run(IReadOnlyTransaction &) override {
  co_return co_await fileHelper().statFs(req_.user, std::chrono::seconds(30));
}
```

參數 `IReadOnlyTransaction &` 完全沒用到。`FileHelper::statFs`（`components/FileHelper.cc:139-149`）只讀一個進程內快取：

```cpp
auto cached = *cachedFsStatus_.rlock();
if (!cached.status_.has_value() || RelativeTime::now() - cached.update_ > statfs_cache_time)
  co_return makeError(StorageClientCode::kResourceBusy, "cached statfs outdate, try again");
co_return *cached.status_;
```

快取由 `FileHelper` 的背景任務每 200 ms 檢查、按 `statfs_update_interval = 5s` 更新（`FileHelper.cc:65-79`）。快取過期（`statfs_cache_time = 60s`）就直接回 `kResourceBusy` 讓 client 重試，**不會同步去查**。這保證 `df` 永遠不會阻塞 meta server——`updateStatFs` 要對每個 storage 節點發 `querySpaceInfo`，在大叢集上是幾百個 RPC。

### 7.17 `getRealPath` — 往上爬

`store/ops/GetRealPath.cc:54-94`。交易數：**1**。兩種模式：

**相對模式**（`absolute = false`）：路徑解析時用 `trace_` 參數記錄實際走過的路徑（`PathResolve.cc:298-304`、`:352-354`），解析完直接 `simplifyPath(trace)`。symlink 展開時會 `trace_->remove_filename()` 再重新累積，所以 trace 反映的是**解析後的真實路徑**。

**絕對模式**：從目標 dentry 出發，反覆 `[S] INOD:{entry.parent}` → `snapshotLoadDirEntry` 往上爬到 tree root，把 name 反序拼起來。這條路徑的成本是 O(深度) 次**成對**讀取（每層要讀 inode 再讀 dentry），是元資料層報告 §5.2 提到的 `Directory::parent` + `name` 反正規化的直接受益者——若沒有這兩個欄位，每層都要反查 dentry 表。

`simplifyPath`（`GetRealPath.cc:16-43`）是純字串處理，處理 `.` 與 `..`：`..` 在有前綴時彈出，在絕對路徑開頭時丟棄（`/..` → `/`），在相對路徑開頭時保留。

### 7.18 `dropUserCache` / `authenticate` / `testRpc`

三個都不開 FDB 交易：

- `dropUserCache`（`MetaOperator.cc:425-432`）：直接操作 `userStore_->cache()`。`dropAll` 清全部，否則清單一 uid。用於使用者權限變更後強制刷新。
- `authenticate`（`:279-282`）：呼叫 `userStore_->authenticate(req.user)` 驗 token，成功就把（可能被補齊 groups 的）`UserInfo` 回傳給 client。失敗計數到 `meta_server.auth_failed`（按 uid 分組，`:266-277`）。
- `testRpc`（`store/MetaStore.cc:84-91`）：`BenchRpcOp` 的 `run` 直接 `co_return TestRpcRsp{}`。純粹用來量測 RPC 往返延遲，排除 FDB 的影響。

### 7.19 `initFs` — 只在初始化時跑

`store/MetaStore.cc:24-82`，`InitFsOp`。不是 RPC，由 `MetaOperator::init(rootLayout)` 呼叫，正式部署走 `admin_cli` 的 init-cluster。

```
 1. chainAlloc.checkLayoutValid(rootLayout)
 2. [R] INOD:{root} 與 [R] INOD:{gcRoot}
 3. root 不存在 → Inode::newDirectory(root, root, "/", Acl::root(), rootLayout, now(1ms))
 4. gcRoot 不存在 → Inode::newDirectory(gcRoot, gcRoot, "/", Acl::gcRoot(), Layout()/*invalid*/, ...)
```

`gcRoot` 的 layout 刻意是**無效的空 Layout**——垃圾桶下面不會直接放檔案（只放各節點的 GC 目錄），所以不需要有效 layout。`Inode::store` 的 `isFile()` 分支才驗 layout（`Inode.cc:125-131`），目錄不驗。

`Inode::store` 對 tree root 有特別保護（`store/Inode.cc:106-118`）：

```cpp
static const std::map<InodeId, Acl> treeRoots{{InodeId::root(), Acl::root()}, {InodeId::gcRoot(), Acl::gcRoot()}};
if (treeRoots.contains(id)) {
  if (!isDirectory() || (!name.empty() && name != "/")) → DFATAL + kFoundBug
  auto expectedAcl = treeRoots.at(id);
  expectedAcl.iflags = acl.iflags;             // iflags 允許改
  if (acl != expectedAcl) → DFATAL + kNoPermission    // 其他一律不准改
}
```

也就是 root 的 uid/gid/perm 在**落盤層**被硬性鎖死，即使上層的 `SetAttr::check` 被繞過也寫不進去。兩層防禦。

---

## 8. `rename`：跨多 key 的原子操作

`store/ops/Rename.cc:236-361`。這是唯一一個在**單一交易裡動到 4~6 把 key、涉及兩個目錄**的操作，也是最能看出 3FS 「把一致性難題全部塞進一個 FDB 交易」策略的地方。

### 8.1 語意選擇

檔頭註解（`Rename.cc:37-49`）明確記錄了語意決策：

```
Note: rename operation in POSIX and HDFS has different semantic when destination exists.
In POSIX, if destination is a file or empty directory, it will be replaced automatically.
In HDFS,
 - if destination is a file, rename operation will raise FileAlreadyExistsException;
 - if destination is a directory and source is file, source will be moved under destination;
 - if both are directories, all children of source will be moved under destination recursively
   (we have decided to not provide this semantic because it's too complicated).
This function implements POSIX semantic.
```

**選 POSIX 而非 HDFS 是關鍵**：HDFS 的「遞迴合併目錄」語意無法在單一交易裡完成（目錄可能有百萬項），會逼出一套分散式協調協定。POSIX 的「目標必須是空目錄或檔案」則天然是 O(1) 的。

### 8.2 完整執行序列

```
RenameOp::run(txn)                                              Rename.cc:236-361
 │
 ├─(1) 併發解析 src 與 dest                                          :241-243
 │      folly::coro::collectAll(resolve(src, NOFOLLOW), resolve(dest, NOFOLLOW))
 │      → 兩條路徑的 [S] 讀取交錯進行，減少一半延遲
 │
 ├─(2) 冪等檢查：dst.uuid != 0 && dst.uuid == req.uuid              :248-255
 │      → CRITICAL 日誌 + 回傳 dst 的 inode（rename 其實已完成）
 │
 ├─(3) src 不存在 → kNotFound                                        :258-260
 ├─(4) req.inodeId 有值且與 src.id 不符 → kNotFound（ABA 防護）      :262-264
 ├─(5) src 與 dst 是同一個 dentry → no-op，回傳 inode                :266-271
 ├─(6) moveToTrash && dst 是檔案 → kExists                           :273-275
 ├─(7) dst 是目錄 → DirEntryList::checkEmpty [R] getRange limit=1     :277-284
 │      非空 → kNotEmpty
 ├─(8) src 是目錄：
 │      ├─ dst 存在但不是目錄 → kNotDirectory（man 2 rename）         :288-292
 │      └─ checkLoop(txn, src, dst, origPath)                          :293
 │
 ├─(9) checkPermission(src, ...)  與 checkPermission(dst, ..., dst=true)  :298-299
 │      各自：[S] parent inode → WRITE 權限 → checkLock
 │            → dst 的 parent.nlink == 0 → kNotFound
 │            → 目標 inode 的 FS_IMMUTABLE_FL → kNoPermission
 │            → sticky bit 檢查
 │
 ├─(10) 四把衝突集：                                                  :302-306
 │        [C] INOD:{srcParent}
 │        [C] DENT:{srcParent}{srcName}
 │        [C] INOD:{dstParent}
 │        [C] DENT:{dstParent}{dstName}
 │
 ├─(11) [R] INOD:{srcEntry.id}    ← 一般讀（要 read-modify-write）    :309
 │      若 src 是目錄：                                                :312-319
 │        inode.asDirectory().parent = dstParentId
 │        inode.asDirectory().name   = dstName
 │        [W] INOD:{srcEntry.id}          ← 更新反向指標
 │
 ├─(12) [D] DENT:{srcParent}{srcName}                                 :322
 │
 ├─(13) removeDst(txn, dst, dstInode)                                 :323
 │        ├─ dst 不存在 → nullopt
 │        ├─ dst 是檔案 → gcManager().removeEntry(...)  ← 進垃圾桶
 │        ├─ dst 是空目錄 → [D] INOD:{dst.id}   ← 直接刪 inode
 │        └─ dst 是 symlink → nlink--; [W] INOD  ← 刻意不刪（見下）
 │
 └─(14) newDstEntry = srcEntry.data();  newDstEntry.uuid = req.uuid   :328-331
        [W] DENT:{dstParent}{dstName}
```

**寫入的 key 總數**：最少 2 把（純改名：刪 src dentry + 寫 dst dentry），最多 6 把（目錄改名 + 覆蓋檔案：src dentry 刪、dst dentry 寫、src inode 寫、舊 dst inode 寫、舊 dst 的 GC dentry 寫、舊 dst 的 dentry 刪）。**恆定為 O(1)**，與目錄大小無關。

### 8.3 大小與範圍限制

FDB 硬性上限是「單交易 10 MB 寫入、5 秒讀版本壽命」。rename 的寫入量由 dentry/inode 的 value 大小決定：inode value 通常 < 200 bytes，dentry < 300 bytes（含 dirAcl 與 origPath）。6 把 key 遠低於 10 MB。

真正的風險在**讀取**，有三處與資料規模相關：

| 讀取 | 規模 | 限制手段 |
|---|---|---|
| 路徑解析（src + dst） | O(路徑深度) | symlink 深度/次數上限；深度本身無硬性上限 |
| `checkEmpty(dst)` | O(1) | `getRange(..., limit=1)` |
| `checkLoop` → `loadAncestors(dstParent)` | O(目錄樹深度) | 環路偵測；每層一次 `[R] INOD` |
| `underTrash` 時的 `recursiveCheckRmPerm` | 受限 | `limit = recursive_remove_perm_check`（1024）、批次 128 |
| `underTrash` 時的 `loadAncestors(srcParent)` | O(深度) | 同上 |

**`loadAncestors` 用的是 `Inode::load`（非 snapshot）**（`store/Inode.cc:208`），註解說明「add dst's ancestors inode into read conflict set」。這是必須的：`checkLoop` 的結論（「src 不是 dst 的祖先」）依賴整條祖先鏈，若鏈上任何一環在提交前被改（例如有人把 dst 的某個祖先 rename 到 src 底下），結論就失效，會造出目錄環。把整條鏈加進衝突集是唯一安全的做法。

**代價**：深層目錄的 rename 會與**所有祖先目錄上的任何寫入**衝突。在一個深度 10 的樹裡 rename 目錄，任何人 `chmod` 其中一層祖先都會讓你重試。這是為正確性付的直接代價，也解釋了為什麼 3FS 建議扁平的目錄結構。

`operation_timeout = 5s` 與 FDB 的 5 秒交易上限對齊，是這一切的兜底：任何讀取加總超過 5 秒，交易會拿到 `kTooOld`，重試若干次後 `OperationDriver` 的 deadline 檢查會終止並回 `kOperationTimeout`。

### 8.4 `checkLoop`：三件事一起做

`Rename.cc:71-139`。只在 src 是目錄時呼叫，做三件事：

**(a) 環路偵測**（`:79-103`）。往上爬 dst 的祖先鏈：

```cpp
for (auto &ancestor : dstAncestors) {
  if (ancestor.id == srcResult.dirEntry->id)                    // src 是 dst 的祖先
    co_return makeError(kInvalidArg, "try to move directory into it's descendent");
  if (ancestor.nlink == 0) co_return makeError(kNotFound);      // 祖先已刪
  if (ancestor.id == ancestor.asDirectory().parent) {           // 到達某個 tree root
    if (ancestor.id == InodeId::root()) break;                  // 正常
    else if (ancestor.id == InodeId::gcRoot())                  // 目標已在垃圾桶裡
      co_return makeError(kNoPermission);
    else → DFATAL + kFoundBug                                   // 第三種自環，不該存在
  }
}
```

第二個分支（`gcRoot`）處理的是競態：dst 的某個祖先在解析後、提交前被刪除搬進了垃圾桶。爬到 `gcRoot` 就知道整條路徑已經在垃圾桶裡，拒絕。

**(b) 「移進 trash」的權限檢查**（`:105-131`）。`underTrash(ancestors)` 的判定（`:58-61`）：

```cpp
return ancestors.size() >= 2 && ancestors.back().id == InodeId::root()
       && ancestors[ancestors.size() - 2].asDirectory().name == "trash";
```

也就是「祖先鏈的倒數第二個是名為 `trash` 的、掛在 root 下的目錄」。**`trash` 是一個約定俗成的普通目錄名，不是特殊 inode**——這是把「回收站」實作成純命名約定，不需要任何 schema 支援。

判定為移進 trash 時：
- `req.moveToTrash == true` 或 `allow_directly_move_to_trash` → 要求 `checkRecursiveRmPerm`（擁有者 + rwx），並跑 best-effort 的遞迴檢查。
- 否則且非 root → **src 必須本來就在 trash 底下**，否則 `kNoPermission`。這防止使用者繞過 `rm -rf` 的權限檢查，直接用 `mv` 把整棵樹丟進垃圾桶。

**(c) 記錄原始路徑**（`:132-136`）。把 src 的完整路徑拼出來存進 `origPath`，寫進事件日誌與 trace log。這樣稽核時能知道「這個被丟進 trash 的目錄原本在哪」。

### 8.5 `removeDst` 的三種型別、三種處理

`Rename.cc:192-234`：

| dst 型別 | 處理 | 理由 |
|---|---|---|
| 檔案 | `gcManager().removeEntry(...)` → nlink--，進垃圾桶 | chunk 要背景回收 |
| 空目錄 | `[D] INOD:{id}` **直接刪，連 dentry 都不用刪**（下一步會被覆蓋） | 空目錄沒有任何附屬資源 |
| symlink | `nlink--`，**只 store 不 remove** | 見下 |

symlink 的處理有一段誠實的註解（`:221-223`）：

```cpp
// NOTE: The fuse client may have cached this symlink. If delete it immediately, kNotFound will be
// reported for subsequent visits. The temporary solution is not to delete the symlink inode.
// This problem needs to be resolved later.
```

下面還留著被註解掉的正確實作：

```cpp
// if (refcnt != 0) { co_await inode->store(txn); } else { co_await inode->remove(txn); }
```

也就是**symlink 的 inode 在 nlink 歸零後仍然留在 FDB 裡，永遠不會被回收**——這是一個已知的、被明確記錄的洩漏。權衡是：symlink inode 很小（只有一個 Path），洩漏成本低；而 FUSE client 快取失效導致的 `kNotFound` 是使用者可見的錯誤。選擇洩漏。

### 8.6 冪等：`DirEntry::uuid` 作為結果指紋

`needIdempotent`（`Rename.cc:63-69`）：

```cpp
if (!req_.checkUuid()) return false;
if (!req_.moveToTrash && !config().idempotent_rename()) return false;
clientId = req_.client.uuid;  requestId = req_.uuid;  return true;
```

也就是 **rename 預設不用 IDEM 表**（`idempotent_rename = false`），只有 `moveToTrash` 才強制開啟。

原因是 rename 有更便宜的冪等機制：新建的 dst dentry 帶著 `req.uuid`（`:330`），重試時直接比對（`:248-255`）。這是「結果上的指紋」，零額外儲存。

**那為什麼 `moveToTrash` 還要 IDEM 表？** 因為 `moveToTrash` 通常是「把 `/data/x` 移到 `/trash/2024xxxx-x`」，而目標名字通常帶時間戳，client 重試時會生成**不同的目標名**。指紋比對就失效了（找不到那個 dst dentry），第二次執行會真的再搬一次——但 src 已經不在了，回 `kNotFound`。IDEM 表能記住第一次的成功結果。

同一套「uuid 指紋」機制也用在 `create`（`BatchOperation.cc:533-538`）、`mkdirs`（`Mkdirs.cc:39-45`）、`symlink`（`Symlink.cc:33-39`）、`hardLink`（`HardLink.cc:41-47`），五處各自實作一次，日誌都是 `XLOGF(CRITICAL, "xxx already finished, ...")`——用 CRITICAL 級別是因為這代表「有回應丟失」，值得維運注意。

---

## 9. `GcManager::removeEntry`：所有刪除的統一入口

`components/GcManager.cc:754-807`。`remove`、`rename`（覆蓋 dst）、`O_TRUNC` 換 inode、GC 遞迴刪除**全部**走這一個函式。

```cpp
CoTryTask<void> GcManager::removeEntry(txn, const DirEntry &entry, Inode &inode, GcInfo gcInfo) {
  if (inode.nlink == 0) → DFATAL + kInconsistent          // dentry 存在但 nlink 已是 0
  if (inode.acl.iflags & FS_IMMUTABLE_FL) → CRITICAL + kNoPermission

  inode.nlink--;
  SetAttr::update(inode.ctime, UtcClock::now(), time_granularity, cmp=true);

  [C] INOD:{inode.id};  [C] DENT:{entry.parent}{entry.name}     // 兩把衝突集

  if (entry.isSymlink()) {
    [D] DENT;
    nlink != 0 ? [W] INOD : [D] INOD;         // symlink 走到 0 就真的刪
  } else {
    [W] INOD;  [D] DENT;                       // 注意順序：先寫 inode 再刪 dentry
    if (inode.isDirectory()) {
      nlink != 0        → DFATAL + kFoundBug   // 目錄的 nlink 必須是 1
      parent 不匹配      → DFATAL + kFoundBug   // 反向指標一致性
    }
    if (inode.nlink == 0) {
      auto gcDirectory = pickGcDirectory();
      co_await gcDirectory->add(txn, inode, config_.gc(), gcInfo);   // [W] DENT:{gcDirId}{合成名}
    }
  }
}
```

三個要點：

**(a) symlink 的處理與 §8.5 不一致。** 這裡 symlink 走到 nlink==0 會被 `[D] INOD` 真的刪掉，而 `RenameOp::removeDst` 刻意不刪。所以 `rm symlink` 會真的刪，`mv x symlink` 覆蓋則會洩漏。這個不一致是 §8.5 那段 TODO 的直接後果。

**(b) 目錄的 nlink 恆為 1。** 3FS **不維護 `..` 對父目錄的連結計數**——傳統 Unix 檔案系統裡目錄的 nlink = 2 + 子目錄數。3FS 讓目錄 nlink 恆為 1，刪除時減到 0。這省掉了「每建一個子目錄就要更新父目錄 inode」的寫放大（那會讓每個 mkdir 都與父目錄產生寫衝突），代價是 `stat` 回報的目錄 nlink 不符合 POSIX 慣例。

**(c) 進垃圾桶的 dentry 名字編碼了排序與優先權**（`GcManager.h:70-72` 與 `:166-194`，詳見元資料層報告 §9）：

```cpp
fmt::format("{}-{:020d}-{}", prefix, timestamp.toMicroseconds(), inode.toHexString())
```

`prefix` 由 `addFile`（`GcManager.cc:201-214`）按檔案的 chunk 數決定：

```cpp
auto chunks = inode.asFile().length / inode.asFile().layout.chunkSize;
prefix = 'f';                                        // FILE_MEDIUM
if (chunks >= large_file_chunks(128))   prefix = 'L';  // FILE_LARGE  → HI_PRI
if (chunks <  small_file_chunks(32))    prefix = 'S';  // FILE_SMALL  → LO_PRI
```

**大檔優先回收**（HI_PRI）是因為空間壓力主要來自大檔；小檔降級（LO_PRI）是因為它們回收的空間少但 RPC 成本一樣。

---

## 10. 冪等機制

### 10.1 兩層冪等

3FS 有兩套獨立的冪等機制，適用於不同情境：

| 機制 | 儲存成本 | 適用 | 實作位置 |
|---|---|---|---|
| **結果指紋**（`DirEntry::uuid`） | 0（複用既有欄位） | 操作結果本身可辨識（create/mkdirs/symlink/hardLink/rename） | 各 op 的開頭檢查 |
| **IDEM 記錄表** | 一把 KV + 背景清理 | 結果無法從狀態反推（remove、moveToTrash） | `store/Idempotent.h` |
| **進程內指紋**（`prevCreatedInodeId_`） | 0 | 僅跨同一操作的交易重試 | `ops/Open.cc:77-83`、`:229` |

### 10.2 誰需要 IDEM 記錄

`IOperation::needIdempotent(Uuid &clientId, Uuid &requestId)` 預設回 `false`（`store/MetaStore.h:56-59`）。只有兩個操作覆寫：

```cpp
// RemoveOp                                                       Remove.cc:52-62
if (!req_.checkUuid()) return false;
if (req_.recursive || config().idempotent_remove())  { 填 uuid; return true; }
return false;

// RenameOp                                                       Rename.cc:63-69
if (!req_.checkUuid()) return false;
if (!req_.moveToTrash && !config().idempotent_rename()) return false;
填 uuid; return true;
```

預設設定（`base/Config.h:118-119`）：`idempotent_remove = true`、`idempotent_rename = false`。所以：

- **所有帶 uuid 的 `remove` 都寫 IDEM 記錄**（`unlink`、`rmdir`、`rm -rf`）。
- **`rename` 只有 `moveToTrash` 寫**。

**為什麼 remove 一定要**：remove 的成功結果是「東西不見了」，而失敗重試看到的也是「東西不見了」，兩者無法區分。`RemoveRsp` 本身是空的（只有 `dummy`），沒有任何可比對的指紋。

### 10.3 記錄的生命週期

Key 佈局與巢狀序列化詳見元資料層報告 §10。這裡補充執行期行為：

**寫入**：在 `runAndCommit` 的兩個分支裡（`store/Operation.h:233`、`:239`），**與業務修改在同一個交易裡提交**。這是原子性的關鍵——不可能出現「刪了但沒記錄」或「記錄了但沒刪」。

**讀取**：`Idempotent::load<Rsp>` 用 `txn.get`（非 snapshot，`Idempotent.h:60`），會進讀衝突集。兩個併發的相同請求，只有一個能提交。

**清理**：`MetaOperator::start` 註冊的 `idempotent_clean` 背景任務（`MetaOperator.cc:200-229`）：

```cpp
bgRunner_->start("idempotent_clean", [&]() -> CoTask<void> {
  if (!isFirstMeta(*mgmtd_, nodeId_)) co_return;          // ← 只有「第一台」meta 做
  auto prev = std::optional<std::string>();
  auto more = true;
  while (more && !stop_) {
    auto result = co_await WithTransaction(strategy).run(txn, [&](auto &txn) {
      return Idempotent::clean(txn, prev, idempotent_record_expire(30min), 2048, t, c);
    });
    if (!result) break;
    prev = result->first;  more = result->second;
  }
}, config_.idempotent_record_clean_getter());             // 每 1 分鐘
```

`isFirstMeta`（`store/Utils.h:67-76`）的定義是「所有 active META 節點中 nodeId 最小的那個」：

```cpp
auto nodes = routing->getNodeBy(selectNodeByType(META) && selectActiveNode());
auto first = std::min_element(nodes.begin(), nodes.end(), [](auto &a, auto &b) {
  return a.app.nodeId < b.app.nodeId;
});
return first != nodes.end() && first->app.nodeId == nodeId;
```

**這是一個無鎖的「單例任務選舉」**：不需要分散式鎖，直接從 mgmtd 的路由資訊推導。代價是路由資訊有傳播延遲，短時間內可能有兩台都認為自己是第一台（重複清理，冪等無害），或都認為自己不是（暫停清理，無害）。用「最終一致的選舉 + 冪等的任務」換掉「強一致的鎖」。

同樣的 `isFirstMeta` 也守著 `SessionManager::scanTask`（`SessionManager.cc:257-259`）。

`Idempotent::clean` 每次掃 2048 筆、回傳游標 `{nextPrev, hasMore}`，讓外層迴圈分批推進到掃完為止。過期時間 30 分鐘——足夠涵蓋任何合理的 client 重試窗口。

---

## 11. Distributor：多 meta server 之間的協調

`components/Distributor.h` / `.cc`。這是本報告與元資料層報告分工最明確的一節：那邊講 HRW 雜湊演算法（§11），這邊講**成員表的維護迴圈與它跟交易衝突集的耦合**。

### 11.1 它到底解決什麼問題

**Distributor 不是分片（sharding）。** meta server 之間沒有資料分區，任何一台都能處理任何請求。`stat`、`list`、`mkdirs`、`rename`、`remove` 等操作**完全不看 Distributor**，就地在本機交易裡完成。

Distributor 只服務四個 RPC：`sync`、`close`、`create`、`setAttr`（by-inode）。這四個的共同點是**都走批次合併**。批次合併只有在「同一個 inode 的請求落在同一台 server」時才有效——否則兩台 server 各自合併出一個交易，仍然在 FDB 層面互相衝突。

所以 Distributor 的職責可以精確表述為：**提供一個所有節點都能算出相同答案的 inode → node 映射，讓批次合併真的能生效。**

### 11.2 兩套 key

```
"META"                    → serde(ServerMap{ active: vector<NodeId> })    成員表
"META-{nodeId:08d}"       → versionstamp（10 bytes）                       各節點心跳
"\xff/metadataVersion"    → versionstamp                                   成員表版本
```

`PerServerKey::pack`（`Distributor.cc:45-47`）用 `fmt::format("{}-{:08d}", kPrefix, nodeId)`，補零到 8 位是為了讓字串排序等於數值排序。而成員表本身用 `"META"`（沒有 `-`），所以 `listByPrefix("META-")` 只會列出心跳，不會撈到成員表——**用一個分隔字元把兩種 key 分開**。

**`\xff/metadataVersion` 是 FDB 的特殊系統鍵**（`src/common/kv/ITransaction.h:19-26` 有完整註解）。它的值隨每個讀版本一起送給 client，**讀它不需要跟 storage server 通訊**。這正是 Distributor 需要的：每個批次交易都要檢查一次成員表版本，若這個檢查要一次網路往返就太貴了。

### 11.3 更新迴圈

`Distributor::start`（`Distributor.cc:60-72`）：先同步跑一次 `update(false)`，再啟動背景任務：

```cpp
bgRunner_->start(fmt::format("distributor_update@{}", nodeId_),
  [this]() -> CoTask<void> { co_await update(false); },
  [&]() { return config_.update_interval() * folly::Random::randDouble(0.8, 1.2); });
```

間隔 1 秒 × 0.8~1.2 的隨機抖動——避免所有 meta server 同時打 FDB。

`Distributor::update(txn, exit)`（`Distributor.cc:218-319`）的完整邏輯：

```
 1. loadServerMap(txn, update=true)                              :220
      ├─ [R] "\xff/metadataVersion"  → versionstamp
      ├─ [R] "META"                  → ServerMap
      └─ 若 versionstamp > 本地快取 → 更新 latest_
 2. 一致性檢查：                                                 :224-242
      current.versionstamp >  latest_.versionstamp → FATAL（不可能）
      current.versionstamp <  latest_.versionstamp → kTooOld，重試
      current.versionstamp == latest_.versionstamp 但 active 不同 → DFATAL
 3. [S] listByPrefix("META-")  → 所有節點的心跳 versionstamp     :244-246
 4. 比對心跳（在 servers_ 的寫鎖內）：                            :249-280
      ├─ 對每個節點：versionstamp 變了 → 更新 {versionstamp, SteadyClock::now()}
      ├─ 自己不在列表裡且 updated_ != 0 → DFATAL
      └─ 對 current.active 裡的每個節點：
           lastUpdate + timeout(30s) < startCheck → 標記為 dead
 5. [W] "META-{自己}" = setVersionstampedValue(...)               :282-284   ← 自己的心跳
 6. 判斷要不要造新成員表：                                        :286-299
      ├─ !exit 且自己不在 active 裡  → 要
      ├─ 有 dead 節點                → 要
      └─ exit                        → 要（把自己加進 dead）
 7. 要的話：active = (current.active - dead) [+ 自己]             :305-317
      [W] "META" = serde(map)
      [W] "\xff/metadataVersion" = setVersionstampedValue(...)
```

**關鍵設計：心跳與成員表分離。** 每秒的心跳只寫 `META-{自己}` 這一把私有 key，**不碰 `\xff/metadataVersion`**。只有成員表真的變化時（新節點加入、舊節點超時、節點退出）才更新版本。這意味著：

- 心跳的寫入完全無衝突（各節點寫各自的 key）。
- `\xff/metadataVersion` 在穩定狀態下**永遠不變**，所以所有批次交易讀它都不會衝突。
- 一旦成員變動，版本一變，**所有正在飛行的批次交易全部衝突重試**——這正是我們要的，因為它們的「我負責這個 inode」假設可能已失效。

**存活判定用 `SteadyTime` 而非心跳裡的時間戳**（`Distributor.cc:267`：`servers[nodeId] = {versionstamp, SteadyClock::now()}`）。也就是「我上次**觀察到**你的 versionstamp 變化是什麼時候」，而不是「你聲稱你什麼時候還活著」。這避免了時鐘偏移問題，也避免了對節點自報時間的信任。

`update(bool exit)` 外層（`:195-216`）最多迴圈 10 次：因為造出新成員表後需要再讀一次才能確認並更新本地快取；若期間又有變化就再來一次。10 次後放棄並回 `kBusy` + CRITICAL 日誌。

### 11.4 `checkOnServer`：把成員關係外包給 FDB 的 MVCC

`Distributor.cc:98-119`，每個 `BatchedOp::run` 的第一步：

```cpp
auto versionstamp = co_await loadVersion(txn);        // [R] "\xff/metadataVersion" → 進衝突集
auto rlock = latest_.rlock();
if (*versionstamp > rlock->versionstamp) {            // 交易看到的比我快取的新
  rlock.unlock();
  CO_RETURN_ON_ERROR(co_await loadServerMap(txn, false));   // 重載
  rlock = latest_.rlock();
}
XLOGF_IF(FATAL, *versionstamp > rlock->versionstamp, ...);  // 重載後仍然更新 → 不可能
if (*versionstamp < rlock->versionstamp) {            // 交易看到的比我快取的舊
  co_return makeError(TransactionCode::kTooOld, "distributor versionstamp changed");
}
auto server = Weight::select(rlock->active, inodeId);
co_return std::pair(server == nodeId, *versionstamp);
```

三種情況：

| 交易版本 vs 本地快取 | 含義 | 處理 |
|---|---|---|
| 交易 > 快取 | 別的節點剛更新了成員表，我還沒看到 | 在**同一個交易裡**重載成員表，用新的算 |
| 交易 == 快取 | 一致 | 正常判定 |
| 交易 < 快取 | 交易的讀版本比我上次讀到的舊（時光倒流） | `kTooOld` → 重試取新讀版本 |

**這整套機制的精髓**：`loadVersion` 用 `txn.get`（非 snapshot），所以 `\xff/metadataVersion` 進入讀衝突集。若在本交易提交前有任何節點更新了成員表，FDB 會判定衝突並拒絕提交。也就是說 —— **「我以為我負責這個 inode」這件事，在交易的提交點被 FDB 保證仍然成立**。

不需要租約（lease）、不需要心跳超時的悲觀等待、不需要 fencing token。整個成員關係的一致性被外包給 FDB 的 MVCC。這是把分散式協調問題化約成單機交易問題的漂亮例子。

### 11.5 成員表為空時的行為

`getServer`（`Distributor.cc:88-91`）在 `active` 為空時，`Weight::selectImpl` 回 `flat::NodeId(0)`（`src/fbs/meta/Utils.h:280-282`），這是無效 nodeId。於是：

- `node == distributor_->nodeId()` 為 false（自己的 nodeId 非 0，`Distributor.h:40` 有 FATAL 檢查）
- 走 forward 路徑 → `Forward::check` 發現 `!node` → 回 `kForwardFailed`（可重試）→ client 重試

也就是**啟動初期成員表還沒建立時，所有 close/sync/create 都會短暫失敗並讓 client 重試**，而不是錯誤地在本機執行。這是 fail-safe 的選擇。

---

## 12. Forward：轉發到正確的節點

`components/Forward.h`。只有四種請求會被轉發（`ForwardMethod` 的四個特化，`Forward.h:65-83`）：`SyncReq`、`CloseReq`、`SetAttrReq`、`CreateReq`——與 Distributor 服務的四個 RPC 一一對應。

### 12.1 防環

`Forward::check`（`Forward.h:85-102`）：

```cpp
if (!node) return makeError(kForwardFailed, "unknown corresponding server");
if (req.forward) {                                     // 已經是被轉發來的
  return makeError(kForwardFailed, "double forward, retry");
}
req.forward = nodeId_;                                 // 蓋上「我轉發的」戳章
XLOGF_IF(FATAL, nodeId_ == node, "forward to self, {} == {}", nodeId_, node);
```

**最多轉發一跳。** 若 A 收到請求算出該給 B，轉給 B；B 因為成員表更新算出該給 C，這時 B 發現 `req.forward != 0`，直接回 `kForwardFailed` 讓 client 重試（client 會重新查路由）。

`ReqBase::forward` 這個欄位就是為此存在的——它是一個攜帶在請求裡的**跳數計數器（上限 1）**。用「一個欄位」取代「TTL 遞減 + 環路偵測」。

`XLOGF_IF(FATAL, nodeId_ == node, ...)` 是防禦性斷言：呼叫端已經判斷過 `node == distributor_->nodeId()` 才走 forward，這裡再確認一次，命中就直接 abort（代表 Distributor 邏輯有 bug）。

### 12.2 位址查找與錯誤轉換

`getAddress`（`Forward.h:104-124`）從 mgmtd 路由資訊裡找目標節點的 `"MetaSerde"` 服務位址，型別由 `addr_type` 決定（預設 RDMA）。三種失敗都回 `kForwardFailed`：路由未就緒、節點不在路由裡、節點沒有對應型別的位址。

`forwardImpl`（`Forward.h:126-164`）的請求選項：

```cpp
opts.timeout = config_.timeout();        // 10s
opts.sendRetryTimes = 3;
opts.compression = std::nullopt;         // 關閉壓縮
```

**關閉壓縮**是因為這四種請求的 payload 都很小（幾十到幾百 bytes），壓縮的 CPU 成本大於頻寬節省。

錯誤轉換（`:151-155`）：

```cpp
if (result.hasError() && StatusCode::typeOf(result.error().code()) == StatusCodeType::RPC) {
  co_return makeError(MetaCode::kForwardTimeout, fmt::format("failed to forward req to {}, error {}", node, ...));
}
```

**所有 RPC 層錯誤被統一轉成 `kForwardTimeout`**。這很重要：`kForwardTimeout` 是 `MetaCode::kRetryable`（3200）與 `kNotRetryable`（3300）之間的碼，所以 `ErrorHandling::retryable` 的 default 分支（`code < MetaCode::kNotRetryable`）判定為**可重試**。而原始的 RPC 錯誤碼在 `ErrorHandling::serverError` 裡會被判定為 server error，語意不同。這層轉換把「轉發失敗」統一表達成一個 client 能理解並重試的元資料錯誤，而不是洩漏內部的網路細節。

業務錯誤（`kNotFound`、`kNoPermission` 等）則原樣穿透。

### 12.3 可測試性：`std::variant<NetClient, MockClient>`

```cpp
using NetClient  = std::reference_wrapper<net::Client>;
using MockClient = std::reference_wrapper<std::map<flat::NodeId, serde::ClientMockContext>>;
std::variant<NetClient, MockClient> client_;
```

`MockMeta`（`service/MockMeta.h:86-97`）在單一進程裡建出多個 `MetaOperator`，各自包一個 `MetaSerdeService` 塞進 `contexts_` map，然後把這個 map 當成 `MockClient` 餵給每個 `Forward`。於是**整個多節點轉發拓撲可以在單元測試裡完整跑起來**，包括 Distributor 的 HRW 選擇與跨節點批次合併，完全不需要網路。

這也是為什麼 `Forward` 的 RPC 呼叫寫成 `ForwardMethod<Req, Rsp, Context>::rpcMethod(ctx, req, &opts, nullptr)`——`Context` 是模板參數，`serde::ClientContext` 與 `serde::ClientMockContext` 共用同一套介面。

---

## 13. 背景組件（`src/meta/components/`）

除了前面章節已展開的 `Distributor`（§11）、`Forward`（§12）、`GcManager`（§9），`components/` 還有四個各司其職的組件。它們的共通形態是：**掛在 `MetaOperator` 上、由 `start(CPUExecutorGroup&)` 啟動、內部跑 `BackgroundRunner` 或 `CoroutinesPool`、設定項全部 `CONFIG_HOT_UPDATED_ITEM`**。

### 13.1 `InodeIdAllocator`：兩層快取的 id 配置器

`components/InodeIdAllocator.h:52-58` 的常數定義揭示了整個設計：

```cpp
static constexpr size_t kAllocatorShard = 32;    // avoid txn conflictation
static constexpr uint64_t kAllocatorShift = 12;  // shift 12 bit
static constexpr uint64_t kAllocatorBit = 64 - kAllocatorShift;  // 52 bits valid
static constexpr uint64_t kAllocatorMask = (1ULL << kAllocatorBit) - 1;
static constexpr uint64_t kAllocateBatch = 1 << kAllocatorShift;  // 4096
```

分兩層：

```
┌─ 第一層：FDB 上的分片計數器 ────────────────────────────────┐
│  kAllocatorKeyPrefix + shard(0..31)                        │
│  每次交易把某個 shard 的計數器 +1，拿到一個「批次號」          │
│  32 個分片是為了「avoid txn conflictation」——               │
│  所有 meta server 都在搶同一個 key 會讓 FDB 衝突率爆炸        │
└──────────────────────┬─────────────────────────────────────┘
                       │ 批次號 << 12
┌──────────────────────▼─────────────────────────────────────┐
│ 第二層：行程內佇列 queue_（容量 2 * kAllocateBatch = 8192）  │
│  一次 DB 交易換來 4096 個連續 InodeId 全部推進佇列            │
│  （InodeIdAllocator.cc:62 的 for 迴圈）                      │
└────────────────────────────────────────────────────────────┘
```

`allocate()`（`:75-95`）的快路徑完全不碰 FDB：

```cpp
auto id = queue_.try_dequeue();
if (LIKELY(id.has_value())) {
  if (queue_.size() < kAllocateBatch / 2) {
    tryStartAllocateTask(co_await folly::coro::co_current_executor);
  }
  co_return id.value();
}
auto result = co_await allocateSlow(timeout);
```

**低水位預取**：佇列剩不到半批（2048）就非同步啟動補充任務，不阻塞當前請求。只有佇列真的見底才走 `allocateSlow` 同步等待，預設逾時 2 秒。

`kAllocatorShift = 12` 的代價是 **id 空間損失 12 bit**——底層 IdAllocator 只有 52 bit 有效。換來的是「一次交易服務 4096 次配置」，把 FDB 交易頻率降到 1/4096。

`:87-92` 還有一道上限檢查：

```cpp
if (result->u64() >= InodeId::kNewChunkEngineMask) {
  XLOGF(DFATAL, "InodeId {} is larger than", *result, InodeId(InodeId::kNewChunkEngineMask));
  co_return makeError(MetaCode::kInodeIdAllocFailed, "InodeId too large, shouldn't happen");
}
```

`InodeId` 的高位被徵用來標記 chunk engine 版本（見《3FS 元資料層深度剖析》的 InodeId 位址空間一節），一旦配置器的計數器爬到那個遮罩就會撞上語意衝突。用 `DFATAL`（debug 版直接 abort、release 版記 fatal log 繼續）表達「這不該發生，但發生了也不能靜默」。

重試策略是 `FDBRetryStrategy({.retryMaybeCommitted = true})`（`:125`）——**允許重試「可能已提交」的交易**。這在一般情況下很危險（可能重複執行），但 id 配置例外：重複提交只會多消耗一個批次號，浪費 4096 個 id 而已，遠比配置失敗好。

### 13.2 `SessionManager`：file session 的逾時清理

`components/SessionManager.h:46-62` 的設定：

| 設定項 | 預設 | 意義 |
|---|---|---|
| `enable` | true | 總開關 |
| `scan_interval` | 5 min | 掃描週期 |
| `scan_batch` | 1024 | 每批掃多少 session |
| `session_timeout` | 5 min | session 多久沒續期算死 |
| `sync_on_prune_session` | false | 清理時是否順帶 sync 檔案長度 |
| `scan_workers` / `close_workers` | `CoroutinesPoolBase::Config` | 兩組獨立的 coroutine pool |

**兩段式流水線**是這個組件的核心結構（`:91-113`）：

```
scanRunner_ (BackgroundRunner, 每 scan_interval 觸發)
      │
      ▼  對每個 shard 產生一個 ScanTask
scanWorkers_ (CoroutinesPool<ScanTask>)
      │  掃 FDB 找出逾時的 session
      ▼  對每個死 session 產生一個 CloseTask
closeWorkers_ (CoroutinesPool<CloseTask>)
      │  呼叫 close_（由 setCloseFunc 注入）
      ▼  走正常的 close 路徑：更新長度、釋放 session
```

拆成兩個 pool 而非一個，是因為兩段的成本特性完全不同：掃描是 FDB range read（IO 密集、可高併發），關閉則要走完整的 close 操作（可能觸發長度查詢與 storage 互動、成本高且不可控）。共用一個 pool 會讓慢的關閉操作把掃描餓死。

`close_` 用 `setCloseFunc` 注入（`:78`）而非直接依賴 `MetaOperator`，是為了打破循環依賴——`MetaOperator` 持有 `SessionManager`，而清理又需要呼叫回 `MetaOperator` 的 close 邏輯。

`pruneManually()`（`:83`）提供手動觸發入口，對應 `admin_cli` 的 `prune-session` 命令。

### 13.3 `AclCache`：帶抖動的分片 LRU

`components/AclCache.h:16-58`。權限檢查在每次路徑解析時都要做（見 §6），每次都讀 FDB 不現實，所以快取 inode → Acl。

```cpp
AclCache(size_t cacheSize) {
  ... folly::EvictingCacheMap<InodeId, CacheEntry>(std::max(cacheSize / kNumShards, 1ul << 10), 128)
}
```

分片 LRU，每片至少 1024 項。真正的巧思在 `get()` 的過期判定（`:46`）：

```cpp
auto deadline = cached->timestamp + ttl * folly::Random::randDouble(0.8, 1.0);
```

**TTL 乘上 0.8–1.0 的隨機因子**。若所有項目都用固定 TTL，同一時刻寫入的大批快取會在同一時刻集體失效，造成瞬間打爆 FDB 的快取雪崩。隨機抖動把失效時間攤在 20% 的區間內。

注意抖動是在**讀取時**計算的，不是寫入時固定的——同一個 entry 每次 `get` 都擲一次骰子，所以它的實際存活時間是「第一次擲出小於已過時間的骰子」為止。這使壽命**與讀取頻率有關**：只被讀一次的 entry 期望壽命是 0.9×TTL（單次擲骰的期望值），而被頻繁讀取的 entry 在 elapsed 超過 0.8×TTL 之後每次 `get` 都有非零機率判定過期，實際壽命會逼近 0.8×TTL 的下界。無論如何都落在 [0.8, 1.0]×TTL 區間內。

`hit` / `miss` 兩個 `CountRecorder`（`:26-27`）是 static 區域變數，符合 Recorder 的慣用法（見 `monitor_collector_main` 報告）。

### 13.4 `ChainAllocator`：建檔時的配鏈

`components/ChainAllocator.h:49-117`。鏈拓樸本身怎麼來的見 [`data_placement` 報告](data_placement-資料佈局求解器深度剖析.md)（離線求解器產生 chain table），鏈上的讀寫協定見 [`storage_main` 報告](storage_main-存儲服務深度剖析.md) §5–§8；此處只講 `ChainAllocator` 在 meta 進程裡的角色。

三個多載構成階梯（`:49`、`:67`、`:82`）：最底層接受任意 `roundRobin` callable，中層綁定外部傳入的 `folly::Synchronized<uint32_t>&`（父目錄私有的 `chainAllocCounter`），頂層用 `ChainAllocator` 自己的計數器表。

頂層那份**不是單一計數器，而是一張 map**（`src/meta/components/ChainAllocator.h:132-133`）：

```cpp
using AllocType = std::pair<flat::ChainTableId, size_t>;
folly::Synchronized<std::map<AllocType, uint32_t>, std::mutex> roundRobin_;
```

**每一組 `(chainTableId, stripeSize)` 各有獨立計數器**，且推進方式不是單純 +1（`ChainAllocator.h:53-64`）：

```cpp
auto initial = folly::Random::rand32(chainCnt) / stripeSize * stripeSize;  // 隨機起點，對齊 stripeSize
auto res = (iter->second % chainCnt) + 1;
iter->second = (iter->second + stripeSize) % chainCnt;                      // 每次前進一整個 stripe
```

隨機起點讓多台 meta server 不必協調就自然錯開；前進 `stripeSize` 而非 1，是因為一次配置就要吃掉 `stripeSize` 條鏈，跳過它們才不會讓下一個檔案跟這個重疊。這讓「per-父目錄的獨立計數器」與「per-(table, stripe) 的行程內計數器」共用同一套配鏈邏輯。

核心兩步（`:113-117`）：

```cpp
auto chainBegin = roundRobin(chainCnt);
// pick a safe shuffle seed
auto seed = find_safe_seed(layout.stripeSize);
if (!seed) co_return makeError(StatusCode::kInvalidArg, "Failed to find safe shuffle seed");
```

`find_safe_seed` 的「safe」定義極其特殊：該 seed 必須在 std / gcc10 / gcc11 三種 `std::shuffle` 實作下產生**相同排列**。因為 seed 會永久存進 inode 的 layout，而 client 開檔時要本地重算 chunk → chain 的映射——**不同編譯器編出的 client 必須算出同一個答案**，否則同一個檔案在不同機器上會讀到不同的 chain。這是把「標準庫實作細節」納入持久化格式相容性考量的罕見案例。

`:105` 與 `:111` 的錯誤訊息說明兩種失敗：chain table 的鏈數不足以支撐 `stripeSize`、以及找不到安全 seed。

### 13.5 `FileHelper`：跨層查詢的門面

`components/FileHelper.h:46-55` 的公開介面只有四個方法，都是「meta 需要向 storage 問事情」的出口（`updateStatFs()` 在 `private:` 區，`:58`，是背景刷新的實作而非對外介面）：

| 方法 | 用途 |
|---|---|
| `queryLength(userInfo, inode, hasHole*)` | 向 storage 查真實檔案長度（close/stat 時校驗 client 回報的長度）。`hasHole` 回報是否有空洞 |
| `remove(userInfo, ..., removeChunksBatchSize)` | 刪除檔案的所有 chunk，分批進行 |
| `statFs(userInfo, cacheDuration)` | 叢集容量統計，帶快取 |
| `updateStatFs()` | 背景刷新容量快取 |

`queryLength` 的存在說明了一件事：**meta 不完全信任 client 回報的檔案長度**。client 在 close 時會帶上它認為的長度，但 meta 可以（依設定）回頭向 storage 求證。這是 VersionedLength 一致性的最後一道防線。

---

## 14. 事件稽核與離線掃描（`src/meta/event/`）

### 14.1 `Event`：結構化操作日誌

`event/Event.h:27` 定義了十種被記錄的事件：

```cpp
enum class Type { Create, Mkdir, HardLink, Remove, Truncate, OpenWrite, CloseWrite, Rename, Symlink, GC };
```

清一色是**改變命名空間或資料的操作**——`Stat`、`List`、`Open`（唯讀）都不在列。這是刻意的取捨：稽核只關心「誰改了什麼」，讀取操作量級太大且無稽核價值。

`Event` 用 `folly::dynamic` 承載欄位（`:37`），`log()` 輸出 JSON（`:41-42`）。建構時自動塞入 `event` 欄位（`:34`）：

```cpp
Event(Type type) { addField("event", magic_enum::enum_name(type)); }
```

`magic_enum::enum_name` 把 enum 轉成字串，所以日誌裡是 `"event": "Rename"` 而非 `"event": 7`——加減 enum 成員不會讓歷史日誌的語意漂移。

檔案後段用 `SERDE_STRUCT_FIELD`（`:51`、`:61`）定義了結構化的事件型別，與 `src/analytics/` 的 `StructuredTraceLog` 對接（見 `monitor_collector_main` 報告）。

### 14.2 `MetaScan`：繞過服務的離線全表掃描

`event/Scan.h:31-60` 的 `MetaScan` 是個特殊組件——它**不屬於 meta server 的執行路徑**，而是一個可獨立使用的全表掃描器：

```cpp
MetaScan(Options options,
         std::shared_ptr<kv::IKVEngine> kvEngine = {} /* create new fdb client if kvEngine is not set */);

std::vector<Inode> getInodes();
std::vector<DirEntry> getDirEntries();
```

`kvEngine` 預設為空時**自己建一個 FDB client**（`:47-48` 的註解明說），只需要 `Options::fdb_cluster_file` 指向叢集設定檔。也就是說它可以在 meta server 完全沒跑的情況下，直接掃 FDB 把所有 inode 與 dentry 撈出來。

`Options`（`:33-45`）的參數揭示它面對的是大規模掃描：

| 參數 | 預設 | 用途 |
|---|---|---|
| `threads` | 4 | 掃描執行緒數 |
| `coroutines` | 8 | 每執行緒的併發 coroutine |
| `items_per_getrange` | -1 | 每次 range read 取幾筆，-1 交給 FDB 決定 |
| `backoff_min_wait` / `max_wait` / `total_wait` | 0.1 / 5 / 60 秒 | 指數退避參數 |

三段式退避（最小 100ms、最大 5s、總計 60s 放棄）是 FDB 大範圍掃描的標準配置——range read 太猛會觸發 `transaction_too_old`，必須退避重試。

內部的 `KeyRange { begin, end, hasMore }`（`:57-60`）是分段掃描的游標：把整個 key 空間切成多段分給不同執行緒，各自推進自己的 `begin` 直到 `hasMore` 為 false。

**它的用途**是離線分析與災難復原：`admin_cli` 的一些掃描類命令、以及把元資料匯出做外部分析，都走這條路而不是打 RPC——避免大掃描擠壓線上請求的資源。

---

## 15. 併發模型、設定與錯誤處理

### 15.1 設定總表

`base/Config.h` 是 meta server 的設定根。幾個關鍵項：

| 設定項 | 預設 | 說明 |
|---|---|---|
| `readonly` | false | 全域唯讀開關，維運時凍結寫入 |
| `authenticate` | false | 是否驗證 user token |
| `grv_cache` | false | 是否快取 FDB 的 read version（降延遲、犧牲新鮮度） |
| `gc.recursive_perm_check` | true | **GcConfig 成員**：GcManager 實際回收目錄時是否再做一次遞迴權限檢查（唯一使用處 `src/meta/components/GcManager.cc:481`）。與路徑解析無關——`PathResolveOp` 的逐層 EXEC 檢查沒有開關可關 |
| `small_file_chunks` / `large_file_chunks` | 32 / 128 | 刪除時判定檔案大小的門檻，影響批次策略 |

GC 的設定（`base/Config.h:23-50`）值得單獨看，它反映了垃圾回收的完整旋鈕：

| 設定項 | 預設 | 說明 |
|---|---|---|
| `scan_interval` | 200 ms | GC 掃描週期——**比 SessionManager 的 5 分鐘密集得多** |
| `scan_batch` | 4096 | 每批掃描量 |
| `gc_file_delay` | 5 min | 檔案進垃圾桶後多久才真正刪 |
| `gc_directory_delay` | 0 s | 目錄不延遲 |
| `gc_file_concurrent` | 32 | 併發刪檔數 |
| `gc_delay_free_space_threshold` | 5 | **空間吃緊時（剩餘 < 5%）取消延遲，立刻回收** |
| `distributed_gc` | true | 隨機挑 GC 目錄，讓多台 meta 自然分工而不重疊 |
| `txn_low_priority` | false | GC 交易是否標記低優先級，讓路給線上請求 |
| `check_session` | true | 刪除前確認沒有活躍 session |

`gc_file_delay` 5 分鐘配上 `gc_delay_free_space_threshold` 是典型的雙模設計：**平時給誤刪留反悔窗口，空間告急時立刻讓路**。

`distributed_gc` 的「隨機選目錄」是無協調的分工——不需要鎖或選主，靠隨機性讓多台 meta server 大機率處理不同目錄，撞上了也只是重複做功而非出錯（`GcManager::removeEntry` 是冪等的，見 §9）。

### 15.2 執行緒與 coroutine

meta server 沒有 per-inode 鎖。併發正確性**全部靠 FDB 交易的可序列化隔離**——衝突的操作會有一方拿到 `not_committed` 並重試（見 §5 的 `OperationDriver`）。這是把分散式併發控制整包外包給 FDB 的直接後果，也是為什麼 `src/meta/` 裡幾乎看不到 `std::mutex`。

少數例外是行程內狀態：`ChainAllocator::roundRobin_` 用 `folly::Synchronized<std::map<…>, std::mutex>`（§13.4）、`AclCache` 用分片鎖——這些都是純快取或提示，即使競態也不影響正確性。

各組件的併發度由 `CoroutinesPoolBase::Config` 與 `PriorityCoroutinePoolConfig`（`base/Config.h:46`）控制。GC 用**帶優先級**的 pool，因為它必須能被線上請求搶佔。

---

## 16. 檔案索引

### `src/meta/`

| 檔案 | 職責 |
|---|---|
| `meta.cpp` | binary 進入點：`TwoPhaseApplication<meta::server::MetaServer>().run(argc, argv)`（`src/meta/meta.cpp:5-8`），全檔僅此三行 |
| `CMakeLists.txt` | `target_add_bin(meta_main "meta.cpp" meta jemalloc)` |
| `base/Config.h` | 設定根：`readonly` / `authenticate` / `grv_cache` / GC 全套旋鈕 / 各 pool 設定 |

### `service/`

| 檔案 | 職責 |
|---|---|
| `MetaServer.cc/h` | 服務容器：組裝 `MetaOperator`、註冊 serde 服務、生命週期管理 |
| `MetaOperator.cc/h` | **核心門面**：所有 RPC 的入口，持有全部背景組件，負責分派與轉發 |
| `MetaSerdeService.h` | serde 服務定義，把 RPC 方法映射到 `MetaOperator` 的成員函式 |
| `MockMeta.h` | 測試用：單進程內建多個 `MetaOperator` 模擬多節點拓撲，含 `MockClient` |

### `store/`

| 檔案 | 職責 |
|---|---|
| `MetaStore.cc/h` | 操作總表：把每個 RPC 對應到一個 `Operation` 子類 |
| `Operation.h` | 操作基類，定義交易生命週期介面 |
| `BatchContext.h` | 批次操作的共享上下文 |
| `Inode.cc/h` | server 端 Inode：繼承 `meta::Inode` 並加上 packKey/load/store/remove |
| `DirEntry.cc/h` | server 端 DirEntry + `DirEntryList` 分頁 |
| `FileSession.cc/h` | file session 的 KV 存取（INOS 前綴） |
| `Idempotent.h` | 冪等記錄（IDEM 前綴），見 §10 |
| `PathResolve.cc/h` | 路徑解析器，見 §6 |
| `Utils.h` | 共用小工具 |

### `store/ops/`

| 檔案 | 對應操作 |
|---|---|
| `BatchOperation.cc/h` | `BatchedOp`：**同一個 inodeId** 上 sync/close/create/setAttr 的批次合併與兩槽流水線（§4.2、§7.6）。與 `batchStat` 無關——後者的 `BatchStatOp` 在 `store/ops/Stat.cc` |
| `Open.cc` | `open` / `create`（含 O_CREAT 語意與 session 建立） |
| `Stat.cc` | `stat` / `getAttr` |
| `SetAttr.cc/h` | `setattr`：chmod / chown / utimes / truncate 的統一入口 |
| `List.cc` | `readdir` 分頁 |
| `Mkdirs.cc` | `mkdir`（含遞迴建立） |
| `Remove.cc` | `unlink` / `rmdir`，走 GC 垃圾桶 |
| `Rename.cc` | `rename`，見 §8 |
| `HardLink.cc` | 硬連結 |
| `Symlink.cc` | 符號連結 |
| `LockDirectory.cc` | 目錄鎖 |
| `PruneSession.cc` | 清理 file session |
| `GetRealPath.cc` | inode → 絕對路徑反查 |
| `StatFs.cc` | 檔案系統容量統計 |

### `components/`

| 檔案 | 職責 |
|---|---|
| `Distributor.cc/h` | 多 meta server 協調：HRW 選擇負責節點（§11） |
| `Forward.h` | 跨節點請求轉發，模板化 Context 以支援 mock（§12） |
| `GcManager.cc/h` | 垃圾桶回收：`GcDirectory` 掃描與分散式分工（§9） |
| `InodeIdAllocator.cc/h` | 兩層 id 配置器：32 分片 FDB 計數器 + 4096 批次行程內佇列（§13.1） |
| `SessionManager.cc/h` | file session 逾時清理，scan/close 兩段式 coroutine pool（§13.2） |
| `AclCache.h` | 分片 LRU + 0.8–1.0 隨機 TTL 抖動的權限快取（§13.3） |
| `ChainAllocator.h` | 建檔配鏈：round-robin baseIndex + 跨編譯器安全的 shuffle seed（§13.4） |
| `FileHelper.cc/h` | 向 storage 查長度、刪 chunk、statFs 的門面（§13.5） |

### `event/`

| 檔案 | 職責 |
|---|---|
| `Event.cc/h` | 十種變更類操作的結構化稽核日誌，JSON 輸出（§14.1） |
| `Scan.cc/h` | `MetaScan`：可獨立於 server 運行的 FDB 全表掃描器，三段式退避（§14.2） |
