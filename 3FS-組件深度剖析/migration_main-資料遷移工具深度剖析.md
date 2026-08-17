# `migration_main` 資料遷移工具深度剖析

> 對應原始碼：`src/migration/`（5 檔）、`src/fbs/migration/`（2 檔）、`tests/migration/`（2 檔）
> CMake：`src/migration/CMakeLists.txt:2`
> 風格基準：[`../3FS-元資料層深度剖析.md`](../3FS-元資料層深度剖析.md)

---

## 0. 一句話總結

**`migration_main` 是一個尚未實作的空殼。** 三個 RPC handler 全部 `co_return makeError(StatusCode::kNotImplemented)`（`src/migration/service/Service.cc:13-15`），沒有任何遷移邏輯、沒有任何背景任務、沒有任何 chunk 或 target 的搬移程式碼。它的骨架是**用 `src/simple_example/README.md:10-16` 那段 `sed` 腳本從 `simple_example` 複製出來的**——那段腳本裡的範例變數名字面上就是 `svr_name='migration'`。

但這份空殼不是全無資訊量。真正有內容的是 `src/fbs/migration/SerdeService.h`：那份 RPC schema 把作者**打算**做什麼講得很清楚（job 狀態機、依 mtime 增量、斷點續傳、四類進度計數器），而且從欄位設計可以反推出遷移的粒度是「**檔案**」而不是 chunk 或 target。本報告的價值在於：(a) 舉證它確實未實作，(b) 從 schema 反推設計意圖，(c) 說明它作為「框架複製品」與 `simple_example` 的三處刻意分歧。

---

## 1. 檔案清單與規模

```
src/migration/
├── CMakeLists.txt          2 行
├── main.cpp                7 行     ← 進入點
└── service/
    ├── Server.h           66 行     ← 生命週期 + 設定
    ├── Server.cc          82 行     ← beforeStart / beforeStop
    ├── Service.h          23 行     ← handler 宣告
    └── Service.cc         19 行     ← handler 定義（全部 kNotImplemented）

src/fbs/migration/
├── CMakeLists.txt          1 行
└── SerdeService.h         80 行     ← 唯一有設計資訊的檔案

tests/migration/
├── CMakeLists.txt          1 行
└── TestMigrationService.cc 92 行    ← 只測「伺服器起得來、停得掉」
```

實作總量（不含 fbs 與測試）**199 行**，其中真正的業務邏輯：**0 行**。

---

## 2. 舉證：這是未完成品

### 2.1 三個 handler 全是空的

`src/migration/service/Service.cc:13-15`：

```cpp
DEFINE_SERVICE_METHOD(start, StartJobReq, StartJobRsp) { co_return makeError(StatusCode::kNotImplemented); }
DEFINE_SERVICE_METHOD(stop, StopJobReq, StopJobRsp) { co_return makeError(StatusCode::kNotImplemented); }
DEFINE_SERVICE_METHOD(list, ListJobsReq, ListJobsRsp) { co_return makeError(StatusCode::kNotImplemented); }
```

`MigrationService` 的成員區是空的（`src/migration/service/Service.h:20-21`：一個 `private:` 標籤後面什麼都沒有）。沒有 job 表、沒有執行緒池、沒有游標。

### 2.2 建好的 storage client 被直接丟棄

`src/migration/service/Server.cc:61-65`：

```cpp
auto storageClient = storage::client::StorageClient::create(clientId_, config_.storage_client(), *mgmtdClient_);
if (!storageClient) {
  XLOGF(ERR, "Failed to create storage client!");
  return makeError(StatusCode::kInvalidConfig);
}

return Void{};
```

`storageClient` 是**函式內的區域變數**，`beforeStart()` 一返回就解構。`MigrationServer` 的成員只有 `backgroundClient_` 與 `mgmtdClient_`（`Server.h:62-63`），根本沒有欄位存放 storage client。也就是說：這段程式碼的實質效果只是「檢查 storage client 建得起來」，然後把它扔掉。真正要搬資料的話，這個 client 必須被持有——這是實作缺口留下的直接痕跡。

同樣的問題在 `simple_example` 裡也存在（`src/simple_example/service/Server.cc:42-45`），因為那本來就是範例。migration 把這個 bug 一起複製過來了。

### 2.3 沒有背景執行機制

`Server.h:10` include 了 `common/utils/BackgroundRunner.h`，但整份 `Server.cc` 沒有任何 `BackgroundRunner` 的實例。遷移這種長時間任務必然需要背景 runner；include 存在而使用不存在，說明這行 include 也是從模板複製來的。

### 2.4 沒有任何呼叫端

全 repo 搜尋 `MigrationSerde`，只有三處命中，全部是定義端自己：

| 位置 | 內容 |
|---|---|
| `src/fbs/migration/SerdeService.h:74` | 服務定義 |
| `src/migration/service/Service.h:8` | 服務實作繼承 |
| `src/migration/service/Server.h:39` | 監聽埠綁定服務名 |

沒有任何 client、CLI 子命令、部署腳本或 Python 工具引用它。對照 `admin_cli`（有數十個子命令對應到 mgmtd / meta / storage 的 RPC），migration 的 RPC 完全沒有使用者介面。

### 2.5 測試只驗證「能開能關」

`tests/migration/TestMigrationService.cc:45-90` 這唯一的測試名叫 `StartAndStopServer`。它做的事是：建立一個 3 副本的測試叢集、寫一個 chunk、讀回來、然後 `server.setup()` → `server.start(appInfo)` → `server.stopAndJoin()`。**測試從頭到尾沒有呼叫 `start` / `stop` / `list` 任何一個 RPC**。前半段的 chunk 讀寫（`:51-63`）與 migration 服務毫無關係——那是從 `UnitTestFabric` 的其他測試複製來的樣板。

### 2.6 從未被修改過

```
$ git log -3 --format="%h %ad %s" --date=short -- src/migration/
815e55e 2025-02-27 Initial commit
```

`src/fbs/migration/` 同樣只有 Initial commit。3FS 開源至今（本報告撰於 2026-08），這個目錄一行都沒有動過，期間主線已累積數百個 commit。

---

## 3. 它是 `simple_example` 的 `sed` 產物

`src/simple_example/README.md` 是「怎麼新增一個服務」的操作手冊，第 10-16 行給的範例腳本，變數名就是 migration：

```bash
svr_name='migration'
SrvName='Migration'
mkdir -p "src/$svr_name" && pushd src/simple_example && cp -rf --parents . "../$svr_name" && popd
mkdir -p "src/fbs/$svr_name" && pushd src/fbs/simple_example && cp -rf --parents . "../$svr_name" && popd
find "src/$svr_name" "src/fbs/$svr_name" -type f | xargs sed -i "s/simple_example/$svr_name/g"
find "src/$svr_name" "src/fbs/$svr_name" -type f | xargs sed -i "s/SimpleExample/$SrvName/g"
```

把兩邊的識別字都正規化後做 diff，可以看出複製後只發生了三類分歧：

**(a) `CMakeLists.txt` 完全相同**（正規化後 diff 為空）。連依賴清單都一字不差：

```cmake
target_add_lib(migration core-app core-user core-service fdb migration-fbs mgmtd-client storage-client memory-common analytics)
```
（`src/migration/CMakeLists.txt:1`，對照 `src/simple_example/CMakeLists.txt:1`）

其中 `fdb` 與 `analytics` 這兩個依賴在 migration 的程式碼裡完全沒被用到——`fdb` 是 FoundationDB client、`analytics` 是 Arrow/Parquet 結構化 trace log。它們躺在依賴清單裡是因為 `simple_example` 就是這樣寫的。

**(b) RPC 面板換掉了**：`echo` 一個方法 → `start` / `stop` / `list` 三個方法，型別對應換成 job 相關的 struct。這是唯一「有設計」的改動。

**(c) 啟動模式從 Two-Phase 換成 One-Phase**——這是唯一一處值得單獨討論的技術決策，見下節。

---

## 4. 唯一的實質設計決策：One-Phase 啟動

`simple_example` 用 `TwoPhaseApplication`，migration 改用 `OnePhaseApplication`：

```cpp
// src/simple_example/main.cpp:7
return TwoPhaseApplication<simple_example::server::SimpleExampleServer>().run(argc, argv);

// src/migration/main.cpp:6
return hf3fs::OnePhaseApplication<hf3fs::migration::server::MigrationServer>::instance().run(argc, argv);
```

連帶把 mgmtd client 的角色也換了：`MgmtdClientForServer` → `MgmtdClientForClient`（`Server.h:5`、`Server.h:47`、`Server.h:63`）。

### 兩種啟動模式的差別

```
TwoPhaseApplication (src/common/app/TwoPhaseApplication.h)
  │
  ├─ Phase 1: Launcher（自己的一套 flag 與 mgmtd 連線）
  │    ├─ launcher_->init()                          :42
  │    ├─ launcher_->loadAppInfo()   ← 向 mgmtd 要 nodeId、clusterId   :45
  │    └─ launcher_->loadConfigTemplate() ← 向 mgmtd 拉「遠端設定範本」  :46
  │
  ├─ Phase 2: 用拉回來的設定啟動本體
  │    ├─ initServer()  → Server::setup()            :58
  │    └─ startServer() → launcher_->startServer()   :63
  │
  └─ launcher_.reset()  ← 用完即丟                    :67
     configPushable() = FLAGS_cfg.empty() && !FLAGS_use_local_cfg  :86

OnePhaseApplication (src/common/app/OnePhaseApplication.h)
  │
  └─ 單階段：一切來自本機
       ├─ initConfigFromFile(appConfig_, FLAGS_app_cfg, ...)   :69
       ├─ initConfigFromFile(config_,    FLAGS_cfg, ...)       :74
       ├─ IBManager::start / logging::initOrDie / Monitor::start  :83-96
       ├─ server_ = make_unique<T>(config_.server()); setup()  :99-101
       ├─ info_.nodeId = flat::NodeId(appConfig_.node_id())    :109  ← 命令列給
       └─ server_->start(info_)                                :121
```

差別的實質是**設定與身分從哪裡來**：

| | TwoPhase | OnePhase |
|---|---|---|
| nodeId | 向 mgmtd 註冊取得 | `--app_config.node_id` 命令列指定，且 `allow_empty_node_id` 預設 `true`（`OnePhaseApplication.h:44`） |
| 設定內容 | mgmtd 下發的範本 + 本地覆寫 | 純本地 TOML |
| 設定熱更新 | **條件式**：`configPushable()` 被 override 成 `FLAGS_cfg.empty() && !FLAGS_use_local_cfg`（`TwoPhaseApplication.h:86`），以本機 `--cfg` 啟動時**拒絕**推送 | **支援**：`OnePhaseApplication` 沒有 override，繼承基底預設 `true`（`ApplicationBase.h:70`） |

> ⚠️ 「沒有 override 所以不支援」是把方向想反了。`ApplicationBase::configPushable()` 的**基底預設就是 `true`**（`src/common/app/ApplicationBase.h:70`），全樹僅有的兩個 override（`TwoPhaseApplication.h:86`、`FuseApplication.cc:111`）都是**收緊**而非放寬。判定點在 `ApplicationBase.cc:120-127`：回傳 false 才拒絕推送並記 `kCannotPushConfig`。
>
> 所以 `migration_main` 實際上**可以**被熱更新設定——它註冊了 `core::CoreService`（`src/migration/service/Server.cc:30`）並在 9000 埠開放（`src/migration/service/Server.h:42`），`admin_cli hot-update-config -a <migration_host:9000>` 對它有效，即使它的設定來自本機 TOML。反倒是 TwoPhase 的服務以 `--cfg` 啟動時會拒絕推送。
| 適用角色 | `NodeType::META` / `STORAGE` / `MGMTD` 等**叢集成員** | `NodeType::CLIENT` 等**外掛工具** |

`MigrationServer::kNodeType = flat::NodeType::CLIENT`（`Server.h:22`）。而 mgmtd client 也對應換成 `MgmtdClientForClient`，它與 `MgmtdClientForServer` 的差別在心跳語意：Server 版會 `setAppInfoForHeartbeat()` 上報自己是叢集成員（`simple_example/service/Server.cc:35`），Client 版則是 `setClientSessionPayload()` 註冊一個**客戶端 session**（`migration/service/Server.cc:50-57`），描述字串是 `"Migration: {containerHostname}"`。

這是正確的選擇：遷移工具是運維人員臨時起的一次性進程，不該佔用一個叢集 nodeId、不該進入 mgmtd 的成員表與故障偵測範圍，也不該讓 mgmtd 幫它管設定。同一份考量在 `storage_bench` 上也成立（見該報告，它同樣用 `MgmtdClientForClient` + `ClientSessionData`）。

**這是整個 migration 目錄裡唯一一處「有人想過」的痕跡。** 骨架選對了，肉沒長出來。

---

## 5. 從 RPC schema 反推設計意圖

`src/fbs/migration/SerdeService.h` 是唯一有實質內容的檔案。它沒有實作，但它是一份**設計規格**。

### 5.1 狀態機（寫在註解裡）

`src/fbs/migration/SerdeService.h:10-18`：

```
  NotSubmitted --> Pending --> Running --> Succeeded
                    |           ^  ^
                    |           |  |
                    |  +--------+  |
                    |  |           |
                    v  v           v
                   Stopped     Failed
```

對應 `enum JobStatus`（`:20-27`）：

| 值 | 名稱 | 語意 |
|---|---|---|
| 0 | `NotSubmitted` | 用作「查無此 job」的哨兵回應 |
| 1 | `Pending` | 已提交，排隊中 |
| 2 | `Running` | 執行中 |
| 3 | `Failed` | 失敗（**終態**，圖中沒有回到 Running 的箭頭） |
| 4 | `Stopped` | 使用者主動停止（**可回到 Running**） |
| 5 | `Succeeded` | 完成 |

值得注意的兩條邊：

- `Stopped → Running`：停止不是終態，可以恢復。這與 `StartJobReq.id` 的註解互相印證（見下節）。
- `Failed` 沒有出邊：失敗後只能開新 job，不能原地重試。

`NotSubmitted = 0` 同時是 `StartJobRsp` / `StopJobRsp` / `JobInfo` 的預設值（`:37`、`:45`、`:52`）。在 3FS 的 positional serde 裡（欄位無標籤、尾端缺欄位取預設值，見元資料層報告 §14），把「不存在」編碼成 enum 的零值是慣用手法。

### 5.2 遷移什麼：檔案，不是 chunk、不是 target、不是 chain

`StartJobReq`（`:29-34`）：

```cpp
struct StartJobReq {
  SERDE_STRUCT_FIELD(id, Uuid::zero());   // start a new job if not found at server side; otherwise resume an existing job
  SERDE_STRUCT_FIELD(path, String{});
  SERDE_STRUCT_FIELD(mtime, UtcTime{});   // only files with mtime greater than this value
};
```

三個欄位把粒度講死了：

- **`path`**：來源是一個**檔案系統路徑**（POSIX 命名空間），不是 chain id、不是 target id、不是 chunk range。所以這個工具走的是 meta 層，會做 `readdir` 遞迴，而不是 storage 層的 chain 對 chain 搬移。
- **`mtime`**：註解 `only files with mtime greater than this value`——這是**增量遷移**的過濾條件，語意等同 `rsync --files-from` 搭配時間戳。以 mtime 為過濾鍵只在「檔案」層級有意義；chunk 沒有 mtime。
- **沒有目的地欄位**。`StartJobReq` 裡沒有 `dstPath`、沒有 `dstCluster`、沒有 `dstChainTable`。這是很強的訊號：目的地是**伺服器端設定的**（一個 migration server 實例對應一個固定去向），而不是每個 job 各自指定。這與 `MigrationServer::Config` 只有 `background_client` / `mgmtd_client` / `storage_client` 三個物件（`Server.h:46-48`）互相矛盾——設定裡也沒有目的地欄位。也就是說目的地這件事**連設計都還沒定案**。

`JobInfo` 進一步確認粒度（`:48-60`）：五個計數器全部是 `numFiles*` / `bytesOfFiles*`，沒有任何 `numChunks` / `numTargets`。

### 5.3 斷點續傳：用 job id 而非游標

`id` 欄位的註解是整份 schema 資訊密度最高的一行：

> `// start a new job if not found at server side; otherwise resume an existing job`

`start` 這個方法同時是「建立」與「恢復」，由伺服器端查表決定走哪條路。這是把冪等性建在 API 語意裡：客戶端重試 `start` 不會產生第二個 job，而是恢復第一個。與元資料層 rename 用 `DirEntryData.uuid` 做「結果指紋」冪等（見元資料層報告 §8）是同一個思路——用一個客戶端生成的 Uuid 當作操作身分，而不是維護一張請求日誌。

**但續傳游標存在哪裡沒有答案。** `JobInfo` 裡沒有 `lastScannedPath` 之類的欄位，`Config` 裡沒有 KV 或檔案路徑設定。回應 struct 不需要暴露游標可以理解（那是伺服器內部狀態），但配上 `fdb` 這個 CMake 依賴，最合理的猜測是**打算把 job 狀態存進 FoundationDB**（3FS 所有需要持久化的控制平面狀態都在 FDB，`src/common/kv/KeyPrefix-def.h` 有現成的前綴機制）。這個猜測無法從程式碼驗證。

### 5.4 進度追蹤：五個計數器揭露的流程

`JobInfo:55-59`：

```cpp
SERDE_STRUCT_FIELD(numFilesFound,      uint64_t{});
SERDE_STRUCT_FIELD(numFilesCopied,     uint64_t{});
SERDE_STRUCT_FIELD(numSrcFilesRemoved, uint64_t{});
SERDE_STRUCT_FIELD(bytesOfFilesFound,  uint64_t{});
SERDE_STRUCT_FIELD(bytesOfFilesCopied, uint64_t{});
```

三個 `numFiles*` 蘊含一條三階段流水線：

```
     掃描                 複製                  清理
  ┌──────────┐       ┌───────────┐       ┌──────────────────┐
  │ readdir  │──────▶│  copy to  │──────▶│ remove from src  │
  │ + mtime  │       │    dst    │       │                  │
  │  filter  │       │           │       │                  │
  └──────────┘       └───────────┘       └──────────────────┘
  numFilesFound      numFilesCopied      numSrcFilesRemoved
  bytesOfFilesFound  bytesOfFilesCopied        (無 bytes 計數)
```

三個關鍵推論：

1. **這是「搬移」不是「複製」**。有 `numSrcFilesRemoved` 代表來源會被刪除。
2. **掃描與複製是解耦的**。`numFilesFound` 與 `numFilesCopied` 分開計數，代表掃描不必等複製完成——否則兩者永遠只差流水線深度，分開報就沒意義。`bytesOfFilesFound` 的存在讓進度百分比（`Copied/Found`）可算，這只有在掃描先跑完一段才有價值。
3. **刪除沒有 bytes 計數**。因為刪除的空間回收在 3FS 是非同步的（meta 層的「搬進垃圾桶 + 背景 GC」，見元資料層報告 §9），刪除當下報 bytes 沒有意義。

### 5.5 查詢介面

`ListJobsReq`（`:62-68`）：

```cpp
SERDE_STRUCT_FIELD(path,   String{});                 // 依路徑過濾
SERDE_STRUCT_FIELD(ids,    std::vector<Uuid>{});      // only jobs with specific ids
SERDE_STRUCT_FIELD(status, JobStatus::Running);       // only jobs with this status
SERDE_STRUCT_FIELD(after,  UtcTime{});                // only jobs with start time greater than this value
SERDE_STRUCT_FIELD(limit,  uint32_t{10});             // max number of jobs in response
```

`status` 的預設值是 `Running` 而不是零值 `NotSubmitted`——這是刻意的：不帶參數呼叫 `list` 時的預設語意是「列出正在跑的 job」，這是運維最常要的那個問句。`limit` 預設 10 也是同一個考量（互動式查詢的合理頁大小）。

`after` + `limit` 是分頁游標的雛形，但缺少對應的「下一頁 token」回傳欄位——`ListJobsRsp` 只有 `jobs`（`:70-72`）。要翻頁得靠客戶端自己取最後一筆的 `startTime` 當下一次的 `after`，這在 `startTime` 有重複值時會漏資料。這是規格上的粗糙處。

### 5.6 沒有出現的東西

同樣有資訊量的是 schema **沒有**的欄位：

| 缺失 | 意涵 |
|---|---|
| 限速 / QoS 參數 | `StartJobReq` 沒有頻寬上限、併發度、優先權。而 3FS 的 storage client 本身有 `traffic_control` 設定（見 `storage_bench` 報告 §5.2 引用的 `clientConfig_.traffic_control()`），所以限速可能打算靠 client 設定而非 per-job 參數 |
| 失敗原因 | `JobInfo` 有 `status` 但沒有 `errorMessage` / `lastError`。job 失敗後只知道失敗，不知道為什麼 |
| 回滾 | 沒有 `rollback` 方法，`JobStatus` 也沒有 `RollingBack` / `RolledBack`。設計上不打算支援回滾——考慮到「複製完才刪來源」的流水線順序，中斷時的狀態是「部分檔案已在兩邊、部分只在來源」，這本身是安全的（不會丟資料），恢復靠重跑 job 而非回滾 |
| 與線上讀寫的協調 | 沒有鎖、沒有 freeze、沒有 `FileSession` 相關欄位。遷移期間如果有人在寫同一個檔案，行為未定義。`mtime` 過濾其實暗示了一種弱協調：重跑一次就能撿到遷移期間被改過的檔案 |

---

## 6. 服務框架的接線（複製自 simple_example）

雖然沒有業務邏輯，`MigrationServer` 完整示範了 3FS 服務的接線方式。這部分的逐步拆解見 [`simple_example_main` 報告](simple_example_main-服務框架最小範例深度剖析.md)，此處只記 migration 的具體參數。

### 6.1 雙監聽群組

`src/migration/service/Server.h:36-45`：

```cpp
CONFIG_OBJ(base, net::Server::Config, [](net::Server::Config &c) {
  c.set_groups_length(2);
  c.groups(0).listener().set_listen_port(8000);
  c.groups(0).set_services({"MigrationSerde"});

  c.groups(1).set_network_type(net::Address::TCP);
  c.groups(1).listener().set_listen_port(9000);
  c.groups(1).set_use_independent_thread_pool(true);
  c.groups(1).set_services({"Core"});
});
```

```
                 migration_main 進程
   ┌─────────────────────────────────────────────┐
   │  group 0  :8000  (預設網路型別，可為 RDMA)   │
   │    services = {"MigrationSerde"}            │
   │    → 業務流量                                │
   ├─────────────────────────────────────────────┤
   │  group 1  :9000  TCP                        │
   │    services = {"Core"}                      │
   │    use_independent_thread_pool = true       │
   │    → 管理面（讀寫設定、取狀態）              │
   └─────────────────────────────────────────────┘
```

**`use_independent_thread_pool = true` 是這裡最值得記的一筆。** 管理面用獨立執行緒池，意思是即使業務執行緒池被打滿或卡死，運維仍然能連上 9000 埠診斷、改設定、要求優雅停機。這是「管理通道不可與資料通道共命運」的體現。同時管理面固定用 TCP 而非 RDMA——RDMA 需要 IB 裝置正常，而診斷時往往正是網路出問題的時候。

### 6.2 服務註冊

`Server.cc:29-30`：

```cpp
RETURN_ON_ERROR(addSerdeService(std::make_unique<MigrationService>(), true));
RETURN_ON_ERROR(addSerdeService(std::make_unique<core::CoreService>()));
```

第二個參數 `strict`。看 `src/common/net/Server.h:42-51`：

```cpp
Result<Void> addSerdeService(std::unique_ptr<Service> &&obj, bool strict = false) {
    if (group->serviceNameList().contains(std::string{Service::kServiceName})) {
      return group->addSerdeService(std::move(obj));
    }
  ...
  return groups_.front()->addSerdeService(std::move(obj));
}
```

`strict = true` 要求服務名必須在某個 group 的 `services` 清單裡明確出現，否則報錯；`strict = false` 則允許 fallback 到 `groups_.front()`。所以 `MigrationSerde` 必須被綁到宣告了它的 group 0，而 `CoreService` 走寬鬆模式（雖然 group 1 也確實宣告了 `"Core"`）。這保證業務服務不會因為設定寫錯而悄悄跑到管理埠上。

### 6.3 服務 ID `0xF1`

`src/fbs/migration/SerdeService.h:74`：`SERDE_SERVICE(MigrationSerde, 0xF1)`。

全 repo 的服務 ID 分配：

| ID | 服務 | 位置 |
|---|---|---|
| 3 | `StorageSerde` | `src/fbs/storage/Service.h:8` |
| 4 | `MetaSerde` | `src/fbs/meta/Service.h:709` |
| 10 | `RDMAControl` | `src/common/net/RDMAControl.h:18` |
| 10 | `ClientAgentSerde` | `src/fbs/lib/Service.h:195` |
| 11 | `IBConnect` | `src/common/net/ib/IBConnectService.h:18` |
| 194 | `MonitorCollector` | `src/fbs/monitor_collector/MonitorCollectorService.h:13` |
| **0xF0 (240)** | **`SimpleExampleSerde`** | `src/fbs/simple_example/SerdeService.h:16` |
| **0xF1 (241)** | **`MigrationSerde`** | `src/fbs/migration/SerdeService.h:74` |
| 10000 | `Echo`（測試用） | `src/common/serde/Echo.h:13` |

真實服務用小整數，範例與範例衍生品用 `0xF0` 起跳的高位段——這是一個沒有寫成文件但很清楚的分區慣例。migration 直接取 `simple_example` 的下一個號碼，再次印證它的複製品身分（真要上線的服務，號碼會排進小整數區）。

註：ID `10` 被 `RDMAControl` 與 `ClientAgentSerde` 重複使用。這不是 bug——兩者永遠不會在同一個 `net::Server` 群組裡註冊，ID 只需在單一 group 內唯一。

---

## 7. 它會向 mgmtd / storage 要什麼

雖然沒有實作，從 `beforeStart()` 建立的物件可以確定它**規劃中**的依賴：

```
        migration_main
             │
   ┌─────────┴──────────┐
   │                    │
   ▼                    ▼
backgroundClient_   （未持有的 storageClient）
（net::Client）
   │
   ▼
MgmtdClientForClient  ────RPC───▶  mgmtd_main
   │                              · getRoutingInfo（chain table / target / node）
   │                              · extendClientSession（心跳保活）
   ▼
routing info
   │
   └──────▶ StorageClient ──RDMA/TCP──▶ storage_main
                                        · batchRead / batchWrite
                                        · removeChunks / truncateChunks
```

`Server.cc:32-37` 建立 mgmtd stub factory 並包成 `MgmtdClientForClient`；`:58` 用背景執行緒池啟動它。`:61` 用這個 mgmtd client 建 storage client——這是 3FS 的標準模式：**storage client 不自己連 mgmtd，而是接受一個已經在維護 routing info 的 mgmtd client 引用**（`StorageClient::create(clientId, config, mgmtdClient)`）。

**明顯缺席的是 meta client。** `CMakeLists.txt:1` 的依賴清單裡有 `mgmtd-client` 與 `storage-client`，**沒有 `meta-client`**。但 §5.2 已經論證遷移粒度是「檔案 + 路徑 + mtime」，這必然需要 meta server（`readdir`、`stat`、`open`、`remove`）。

這個矛盾是本報告最實在的一條結論：**依賴清單是從 `simple_example` 原封不動抄來的，沒有依照 `SerdeService.h` 定義的實際需求調整過。** 也就是說，schema 寫完之後，實作連「盤點需要哪些依賴」這一步都沒開始。

---

## 8. 設計取捨總結

由於沒有實作，這裡記錄的是「schema 已定案的取捨」與「未定案的缺口」。

| 決策 | 得到什麼 | 付出什麼 | 狀態 |
|---|---|---|---|
| 遷移粒度 = 檔案 | 可用 mtime 增量、進度以檔案數/bytes 表達、對使用者直觀 | 無法搬 chain / target，做不了「換硬體」型的資料重平衡 | schema 已定 |
| `start` 兼作 resume | 客戶端重試天然冪等，不需要獨立的 resume API | job id 由客戶端生成，撞號會誤接管別人的 job | schema 已定 |
| `Failed` 是終態 | 狀態機簡單，不用管重試次數 | 暫時性錯誤（網路抖動）也得整個 job 重開 | schema 已定 |
| 掃描/複製解耦 | 可算進度百分比、掃描不阻塞複製 | 需要中間佇列與背壓機制 | schema 已定，實作未開始 |
| 先複製後刪除 | 中斷時不丟資料，重跑即可恢復 | 峰值需要兩份空間 | schema 已定 |
| `OnePhaseApplication` + `NodeType::CLIENT` | 不佔叢集 nodeId、不進故障偵測、純本地設定 | 無法透過 mgmtd 熱推設定 | **已實作** |
| 管理面獨立執行緒池 + TCP | 業務卡死時仍可診斷 | 多一個埠、多一組執行緒 | **已實作**（複製自模板） |
| 目的地在哪 | — | **未定案**：schema 與 Config 都沒有欄位 | 缺口 |
| 續傳游標存哪 | — | **未定案**：CMake 有 `fdb` 依賴但無程式碼 | 缺口 |
| 限速 | — | **未定案**：schema 無參數，可能打算靠 storage client 的 traffic_control | 缺口 |
| 與線上讀寫共存 | — | **未定案**：無鎖、無 session 協調 | 缺口 |
| 失敗原因回報 | — | **缺失**：`JobInfo` 無 error 欄位 | 缺口 |
| meta client 依賴 | — | **矛盾**：功能需要但 CMake 沒有 | 缺口 |

---

## 9. 結論

`migration_main` **不應被視為 3FS 的功能組件**。它是：

1. `simple_example` 的 `sed` 複製品，複製腳本就寫在 `src/simple_example/README.md:10-16`，範例變數名字面上是 `migration`；
2. 唯一的實質改動是啟動模式（Two-Phase → One-Phase）與 mgmtd client 角色（Server → Client），這個改動是對的，符合「一次性運維工具」的定位；
3. 唯一有設計資訊的是 `src/fbs/migration/SerdeService.h` 這份 80 行的 RPC 規格，它定義了一個**檔案粒度、支援 mtime 增量、三階段流水線（掃描/複製/刪除）、job 可暫停恢復**的遷移器；
4. 規格本身仍有四個未定案的缺口（目的地、游標持久化、限速、線上共存），而 CMake 依賴清單缺 `meta-client` 這一點證明實作連需求盤點都沒開始；
5. 自 2025-02-27 的 Initial commit 起未曾修改，無任何 CI、腳本或 CLI 引用。

對想理解 3FS 的人來說，讀這個目錄的正確方式是：**把 `src/fbs/migration/SerdeService.h` 當設計文件讀，把 `src/migration/service/` 當 `simple_example` 的第二份拷貝讀。** 若要在 3FS 上實作遷移，這份 schema 是個不錯的起點，但需要補上 meta client 依賴、目的地設定、以及 job 狀態的持久化。

---

## 10. 檔案索引

| 檔案 | 行數 | 職責 |
|---|---|---|
| `src/migration/CMakeLists.txt` | 2 | 定義 `migration` 靜態庫與 `migration_main` 執行檔；依賴清單原封不動抄自 `simple_example`，含未使用的 `fdb`/`analytics`，缺必要的 `meta-client` |
| `src/migration/main.cpp` | 7 | 進入點；`OnePhaseApplication<MigrationServer>::instance().run()`，並 include `OverrideCppNewDelete.h` 以套用自訂配置器 |
| `src/migration/service/Server.h` | 66 | `MigrationServer` 宣告：`kName="Migration"`、`kNodeType=CLIENT`、雙監聽群組（8000 業務 / 9000 管理）、三個子設定物件 |
| `src/migration/service/Server.cc` | 82 | `beforeStart()` 建立 background client → mgmtd client（Client 角色，含 client session payload）→ storage client（**建完即丟棄**）；`beforeStop()` 逆序關閉 |
| `src/migration/service/Service.h` | 23 | `MigrationService` 宣告，三個 handler，成員區為空 |
| `src/migration/service/Service.cc` | 19 | 三個 handler 全部 `co_return makeError(StatusCode::kNotImplemented)` |
| `src/fbs/migration/CMakeLists.txt` | 1 | `migration-fbs` 介面庫，依賴 `mgmtd-fbs` 與 `core-user-fbs` |
| `src/fbs/migration/SerdeService.h` | 80 | **唯一有設計資訊的檔案**：job 狀態機註解圖、`JobStatus` enum、`StartJobReq`（id 兼作 resume / path / mtime 增量）、`JobInfo`（五個進度計數器）、`ListJobsReq` 五個過濾條件、`SERDE_SERVICE(MigrationSerde, 0xF1)` |
| `tests/migration/CMakeLists.txt` | 1 | `target_add_test(test_migration test-fabric-lib migration)` |
| `tests/migration/TestMigrationService.cc` | 92 | 唯一測試 `StartAndStopServer`：搭建 3 副本測試叢集、讀寫一個 chunk（與 migration 無關的樣板）、啟停 `MigrationServer`；**從未呼叫任何 migration RPC** |
