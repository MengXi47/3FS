# 3FS 客戶端元資料與管理函式庫（`src/client/meta/` + `src/client/mgmtd/`）深度剖析

> 對應原始碼：`src/client/meta/`（4 檔，1682 行）、`src/client/mgmtd/`（15 檔，1636 行）
> 邊界（本文只讀介面、不展開）：`src/fbs/meta/Service.h`（RPC schema）、`src/fbs/meta/Utils.h`（錯誤分類 + 權重雜湊）、`src/fbs/mgmtd/`、`src/stubs/`
> 對接文件：伺服端視角見 `mgmtd_main-叢集管理服務深度剖析.md`（§4 lease、§6.5 心跳）與 `meta_main-元資料服務深度剖析.md`

---

## 0. 一句話總結

這兩個目錄是 3FS 全叢集**唯一的出站控制平面**：`MgmtdClient` 把「叢集拓樸從哪來」收斂成一個單執行緒 actor——所有對 mgmtd 的呼叫（不論來自背景心跳、前台 admin 命令，還是 client session 續期）都被塞進同一條 `BoundedQueue`，由一條協程依序取出執行，因此 primary 探測狀態（`primaryMgmtdId_`、`addrMap_`、`mgmtds_`）可以是**裸的、無鎖的成員變數**；而 `MetaClient` 反過來是**完全無狀態的**——它不快取任何 inode、不快取任何路徑，只維護一份「壞掉的 meta server 黑名單」`errNodes_`，把負載平衡整包外包給 `ServerSelectionStrategy`。

三個最違反直覺的事實：

1. **`MetaClient::truncate` 根本不發 truncate RPC**——它自己拿 routing info 去 storage 逐批刪 chunk，最後才發一個 `sync` 讓 meta server 更新長度（`src/client/meta/MetaClient.cc:860-925`）。`truncate` 這個 RPC method id 13 在 `src/fbs/meta/Service.h:731` 被明確標註 `// deperated:`。
2. **file session 的「續期」不在 `MetaClient` 裡**——`MetaClient` 只負責建立（open/create 帶 `SessionInfo`）與釋放（close/pruneSession）。維持存活的是 `MgmtdClient` 的 `extendClientSession`，meta server 端則靠 `listClientSessions()` 反查誰還活著（`src/meta/components/SessionManager.cc:43-73`）。**兩個獨立函式庫透過 mgmtd 間接握手。**
3. **四分介面（`ICommonMgmtdClient` / `ForServer` / `ForClient` / `ForAdmin`）不是為了多型**——四個介面背後永遠是同一個 `MgmtdClient` 實作，拆分的真正產物是**三組互不相同的背景任務預設值**（見 §3.4）。

---

## 1. 這兩個函式庫是什麼、誰在用

### 1.1 消費者地圖

`src/client/mgmtd/` 被叢集裡**每一個** binary 使用；`src/client/meta/` 只被三個使用者引用。

| 消費者 | mgmtd 客戶端型別 | meta 客戶端 | 引用位置 |
|---|---|---|---|
| `hf3fs_fuse_main` | `MgmtdClientForClient` | ✔ `MetaClient` | `src/fuse/FuseClients.cc:98,131` |
| `meta_main` | `MgmtdClientForServer` | ✘ | `src/meta/service/MetaServer.cc:32` |
| `storage_main` | `MgmtdClientForServer` | ✘ | `src/storage/service/Components.cc:80` |
| `mgmtd_main` | （自己也是 mgmtd 的 client，向 primary 發心跳） | ✘ | 見 mgmtd 報告 §6.5 |
| `admin_cli` | `MgmtdClientForAdmin` | ✔ `MetaClient` | `src/client/bin/admin_cli.cc:130,185` |
| `hf3fs-admin`（`src/tools/admin.cc`） | `MgmtdClientForClient` | ✔ `MetaClient` | `src/tools/admin.cc:64,75` |
| `migration_main` | `MgmtdClientForClient`（存成 `IMgmtdClientForClient`） | ✘ | `src/migration/service/Server.cc:34` |
| `simple_example` | `MgmtdClientForServer` | ✘ | `src/simple_example/service/Server.cc:29` |
| 所有 server 的 **launcher 階段** | 裸 `MgmtdClient`（無介面包裝） | ✘ | `src/core/app/MgmtdClientFetcher.cc:55` |

最後一列是關鍵：`MgmtdClientFetcher` 在 server 還沒有設定檔的時候，直接 `new MgmtdClient`，用 `startBackground=false` 起一個**不帶任何背景任務**的實例，只為了呼叫 `getConfig()` 把設定從 mgmtd 拉下來（`src/core/app/MgmtdClientFetcher.cc:17-31`）。設定拉到、server 起來之後，同一個 `shared_ptr<MgmtdClient>` 被交棒給 `MgmtdClientForServer` 包起來繼續用：

```cpp
// src/storage/service/StorageServer.cc:52-58
hf3fs::Result<Void> StorageServer::start(const flat::AppInfo &info,
                                         std::unique_ptr<::hf3fs::net::Client> client,
                                         std::shared_ptr<::hf3fs::client::MgmtdClient> mgmtdClient) {
  components_.netClient = std::move(client);
  components_.mgmtdClient = std::make_unique<hf3fs::client::MgmtdClientForServer>(std::move(mgmtdClient));
  return net::Server::start(info);
}
```

（`src/meta/service/MetaServer.cc:104-110` 是同一個形狀。）這個交棒讓 launcher 階段探測出來的 primary 位址、已知 mgmtd 清單、已抓到的 routing info 全部**不必重來一次**——因為狀態都在 `Impl` 裡，換的只是外面那層介面殼。

### 1.2 兩個函式庫的依賴方向

```
MetaClient ──需要─→ ICommonMgmtdClient        （拿 routing info、註冊變更 listener）
MetaClient ──需要─→ storage::client::StorageClient  （truncate 時直接刪 chunk）
ServerSelectionStrategy ──需要─→ ICommonMgmtdClient
MgmtdClient ──不依賴任何 meta / storage 東西
```

`MetaClient` 的建構子簽名把這件事寫得很清楚（`src/client/meta/MetaClient.h:96-101`）：

```cpp
MetaClient(ClientId clientId,
           const Config &config,
           std::unique_ptr<StubFactory> factory,
           std::shared_ptr<ICommonMgmtdClient> mgmtd,
           std::shared_ptr<storage::client::StorageClient> storage,
           bool dynStripe);
```

它要的是**最窄的那個介面** `ICommonMgmtdClient`，不是 `IMgmtdClientForClient`。所以 `admin_cli` 可以把一個 `MgmtdClientForAdmin` 直接餵給 `MetaClient`（`src/client/bin/admin_cli.cc:185-192`），FUSE 餵的則是 `MgmtdClientForClient`（`src/fuse/FuseClients.cc:134`）——兩者都能通過編譯，正是因為兩個介面共同的基底是 `ICommonMgmtdClient`。

---

## 2. 整體分層架構

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│  使用端：FuseOps / admin_cli 命令 / hf3fs-admin / storage 的 TargetMap / meta 的 GC  │
└───────────────┬──────────────────────────────────────────┬───────────────────────┘
                │                                          │
   ┌────────────▼────────────────┐          ┌──────────────▼────────────────────────┐
   │  meta::client::MetaClient   │          │  四分介面（client/mgmtd/I*.h）            │
   │  src/client/meta/           │          │   ICommonMgmtdClient   ← MetaClient 只要這個│
   │                             │          │      ├── IMgmtdClientForServer          │
   │  ┌───────────────────────┐  │  拿routing│      ├── IMgmtdClientForClient          │
   │  │ retry(func, req, cfg) │──┼──────────▶      └── IMgmtdClientForAdmin           │
   │  │  ├ getServerNode()    │  │          └──────────────┬────────────────────────┘
   │  │  ├ Semaphore 併發閘    │  │                         │ 全部由同一個模板實作
   │  │  ├ 錯誤三分法          │  │          ┌──────────────▼────────────────────────┐
   │  │  ├ failover→errNodes_ │  │          │  CommonMgmtdClient<Base>（模板轉接層）    │
   │  │  └ waitRoutingInfo()  │  │          │  MgmtdClientForServer / ForClient /     │
   │  └───────────────────────┘  │          │  ForAdmin ＝ Base + 各自的 Config 預設   │
   │                             │          └──────────────┬────────────────────────┘
   │  ┌───────────────────────┐  │                         │ 持有 shared_ptr
   │  │ ServerSelectionStrategy│  │          ┌──────────────▼────────────────────────┐
   │  │  RoundRobin           │  │          │  MgmtdClient（門面，130 行純轉接）        │
   │  │  UniformRandom        │  │          │   └─ Impl（pimpl，998 行的全部內容）      │
   │  │  RandomFollow ★       │  │          │      ┌──────────────────────────────┐  │
   │  │  ← routing listener   │◀─┼──────────┼──────│ actor()：單協程 work loop      │  │
   │  └───────────────────────┘  │  推播     │      │ BoundedQueue<WorkItem>        │  │
   │                             │          │      │  Exit/Refresh/Heartbeat/      │  │
   │  ┌───────────────────────┐  │          │      │  Callable(泛型 op)             │  │
   │  │ 背景 closer            │  │          │      ├──────────────────────────────┤  │
   │  │  bgCloseTasks_ (時間堆) │  │          │      │ withRetry：primary 探測+換機   │  │
   │  │  batchPrune_ (批次)    │  │          │      │ Conn / addrMap_ / mgmtds_     │  │
   │  │  CoroutinesPool 8×128 │  │          │      ├──────────────────────────────┤  │
   │  └───────────────────────┘  │          │      │ BackgroundRunner ×3           │  │
   └─────────────┬───────────────┘          │      │  AutoRefresh / AutoHeartbeat  │  │
                 │                          │      │  AutoExtendClientSession      │  │
                 │ MetaServiceStub           │      └──────────────────────────────┘  │
                 ▼                          └──────────────┬────────────────────────┘
   ┌─────────────────────────────┐                         │ MgmtdServiceStub
   │ serde RPC → meta_main        │                         ▼
   │ （MetaSerde，22 個 method）   │          ┌─────────────────────────────────────┐
   └─────────────────────────────┘          │ serde RPC → mgmtd_main（24 個 method） │
                                            └─────────────────────────────────────┘

               共用資料結構：client::RoutingInfo（RoutingInfo.h/.cc + ServiceInfo.h）
               ＝ shared_ptr<flat::RoutingInfo> + 抓取時間戳 + 幾個 selector 查詢
```

兩邊的對稱性值得注意：`MetaClient` 與 `MgmtdClient` **都**實作了「探測 + 換機 + 退避重試」，但機制完全不同：

| | `MetaClient` | `MgmtdClient` |
|---|---|---|
| 目標節點怎麼來 | routing info 裡所有 `META` 型別的 active node | 設定檔寫死的 `mgmtd_server_addresses` + 探測學到的 |
| 選誰 | `ServerSelectionStrategy`（三種策略可熱切換） | 只認 primary，其餘一律不打 |
| 換機時機 | server error 累計達 `max_failures_before_failover`（預設 1） | 網路錯誤或 `kNotPrimary` |
| 重試上限 | `retry_total_time`（預設 60s）內的指數退避 | 一次（`withRetry` 的 `retried` 旗標） |
| 壞節點記憶 | `errNodes_` 集合 + 5 秒探活 | `ProbeContext::probedAddrs`，**只活在單次呼叫內** |
| 併發 | `Semaphore` 限流 128 | actor 串行，天然併發 1 |

---

## 3. 四分介面：為什麼要拆這麼細

### 3.1 四個介面各自暴露什麼

| 介面 | 檔案 | 純虛方法 | 誰實作 | 誰使用 |
|---|---|---|---|---|
| `ICommonMgmtdClient` | `ICommonMgmtdClient.h:10-39` | `getRoutingInfo` / `refreshRoutingInfo` / `add·removeRoutingInfoListener` / `listClientSessions` / `getConfig` / `getConfigVersions` / `getUniversalTags` / `getClientSession`；`start` / `startBackgroundTasks` / `stop` 有**空的預設實作** | 三個 `MgmtdClientForXxx` 共同繼承 | `MetaClient`、`ServerSelectionStrategy`、meta 的 `ChainAllocator` / `GcManager` / `SessionManager` / `FileHelper` / `Forward` |
| `IMgmtdClientForServer` | `IMgmtdClientForServer.h:7-23` | `heartbeat` / `triggerHeartbeat` / `setAppInfoForHeartbeat` / `setConfigListener` / `updateHeartbeatPayload` | `MgmtdClientForServer` | `storage::Components::mgmtdClient`（`src/storage/service/Components.h:102`）、`StorageOperator`、`MetaOperator` |
| `IMgmtdClientForClient` | `IMgmtdClientForClient.h:7-22` | `extendClientSession` / `setConfigListener` / `setClientSessionPayload` | `MgmtdClientForClient` | FUSE 的 `establishClientSession`（`src/fuse/FuseClients.cc:31`）、`migration` |
| `IMgmtdClientForAdmin` | `IMgmtdClientForAdmin.h:8-56` | 13 個叢集寫入操作：`setChains` / `setChainTable` / `setConfig` / `enableNode` / `disableNode` / `registerNode` / `unregisterNode` / `setNodeTags` / `setUniversalTags` / `rotateLastSrv` / `rotateAsPreferredOrder` / `setPreferredTargetOrder` / `listOrphanTargets` / `updateChain` | `MgmtdClientForAdmin` | `AdminEnv::mgmtdClientGetter`（`src/client/cli/admin/AdminEnv.h:20`） |

**沒有任何一個介面同時暴露 heartbeat 與 extendClientSession。** 這是拆分最直接的可觀察後果：一個 storage server 拿到的 `IMgmtdClientForServer` 型別上就**不存在** `extendClientSession()` 這個方法，反之 FUSE 拿到的 `IMgmtdClientForClient` 上不存在 `heartbeat()`。

### 3.2 為什麼要這樣拆——三個依據

問題是：底層 `MgmtdClient`（`MgmtdClient.h:12-129`）把 **全部** 方法都攤在一個類別上，四個介面沒有一個是為了讓不同實作可替換（整個 repo 只有 `MgmtdClient` 一個實作，加上測試用的包裝）。那拆分買到了什麼？程式碼支持三個答案：

**(a) 「角色 → 背景任務」的綁定被寫進了 Config 建構子。** 這是最硬的證據——三個 `MgmtdClientForXxx::Config` 各自在**建構子裡**改預設值：

```cpp
// src/client/mgmtd/MgmtdClientForServer.h:9-15
struct Config : MgmtdClient::Config {
  Config() {
    set_enable_auto_refresh(true);
    set_enable_auto_heartbeat(true);
    set_enable_auto_extend_client_session(false);
  }
};
```

```cpp
// src/client/mgmtd/MgmtdClientForClient.h:9-15
struct Config : MgmtdClient::Config {
  Config() {
    set_enable_auto_refresh(true);
    set_enable_auto_heartbeat(false);
    set_enable_auto_extend_client_session(true);
  }
};
```

```cpp
// src/client/mgmtd/MgmtdClientForAdmin.h:9-16
struct Config : MgmtdClient::Config {
  Config() {
    set_enable_auto_refresh(false);
    set_enable_auto_heartbeat(false);
    set_enable_auto_extend_client_session(false);
    set_accept_incomplete_routing_info_during_mgmtd_bootstrapping(true);
  }
};
```

拆介面 = 拆型別 = 拆 `Config` 型別 = 拆背景任務組合。使用者只要選對型別，背景行為就對了，不必記「storage 該開哪幾個開關」。

**(b) 阻止「用錯 API」在編譯期就失敗。** 心跳的語意是「我是一個註冊在案的節點，我還活著」；client session 的語意是「我是一個匿名 client，我掛著這些 file session」。兩者在 mgmtd 端寫的是完全不同的資料結構（`HeartbeatInfo` vs `ClientSession`）。如果只有一個大介面，一個 FUSE 掛載點誤呼 `heartbeat()` 會在執行期才炸——實際上會停在 `heartbeatImpl` 的第一個檢查（`MgmtdClient.cc:689-694`，`AppInfo not set`）。拆開之後這個誤用**根本編譯不過**。

**(c) `setConfigListener` 被「同名但不同義」地重載了。** 這是最精巧的一處。兩個介面各自宣告了一個簽名完全相同的 `setConfigListener`：

```cpp
// IMgmtdClientForServer.h:18-19
using ConfigListener = std::function<hf3fs::Result<hf3fs::Void>(const String &, const String &)>;
virtual void setConfigListener(ConfigListener listener) = 0;
```
```cpp
// IMgmtdClientForClient.h:11-12
using ConfigListener = std::function<hf3fs::Result<hf3fs::Void>(const String &, const String &)>;
virtual void setConfigListener(ConfigListener listener) = 0;
```

但它們接到底層的**是兩個不同的欄位**：

```cpp
// MgmtdClientForServer.h:31
void setConfigListener(ConfigListener listener) final { client_->setServerConfigListener(std::move(listener)); }
// MgmtdClientForClient.h:29
void setConfigListener(ConfigListener listener) final { client_->setClientConfigListener(std::move(listener)); }
```

底層兩個欄位分別在**不同的 RPC 回應路徑**上被觸發：`serverConfigListener_` 在心跳回應裡被呼叫（`MgmtdClient.cc:731-740`），`clientConfigListener_` 在 extendClientSession 回應裡被呼叫（`MgmtdClient.cc:364-372`）。所以「設定熱更新怎麼下發」對 server 走心跳、對 client 走 session 續期——**兩條完全獨立的通道，被介面層統一成同一個名字**。使用者側因此可以無腦寫 `mgmtdClient->setConfigListener(ApplicationBase::updateConfig)`（FUSE：`src/fuse/FuseClients.cc:122`；meta server：`src/meta/service/MetaServer.cc:39`），不必知道自己走的是哪條。

### 3.3 `CommonMgmtdClient<Base>`：把共同部分寫一次

`CommonMgmtdClient.h:7-49` 是一個以「要實作的介面」為模板參數的轉接層：

```cpp
template <typename Base>
class CommonMgmtdClient : public Base {
 public:
  explicit CommonMgmtdClient(std::shared_ptr<MgmtdClient> client) : client_(std::move(client)) {}
  CoTask<void> start(folly::Executor *backgroundExecutor = nullptr, bool startBackground = true) final {
    return client_->start(backgroundExecutor, startBackground);
  }
  ...
 protected:
  std::shared_ptr<MgmtdClient> client_;
};
```

九個 `ICommonMgmtdClient` 方法在這裡一次性轉接完，三個子類只需補上自己那份差異方法。注意所有轉接都標了 `final`——衍生類別**不能**再覆寫 routing info 的取得方式。

還有一個容易忽略的細節：`client_` 是 `shared_ptr`，而且兩個建構子並存——

```cpp
// MgmtdClientForServer.h:17-23
explicit MgmtdClientForServer(std::shared_ptr<MgmtdClient> client) : CommonMgmtdClient(std::move(client)) {}
MgmtdClientForServer(String clusterId, std::unique_ptr<MgmtdClient::MgmtdStubFactory> stubFactory, const Config &config)
    : CommonMgmtdClient(std::make_shared<MgmtdClient>(std::move(clusterId), std::move(stubFactory), config)) {}
```

第一個建構子（接管既有實例）正是 §1.1 說的 launcher 交棒路徑；第二個是從零建。同一個 `MgmtdClient` 理論上可以被兩個不同的介面殼同時包住（因為是 `shared_ptr`），repo 內未見這種用法。

### 3.4 三個角色的行為差異總表

| | ForServer | ForClient | ForAdmin |
|---|---|---|---|
| `enable_auto_refresh` | ✔ | ✔ | ✘ |
| `enable_auto_heartbeat` | ✔ | ✘ | ✘ |
| `enable_auto_extend_client_session` | ✘ | ✔ | ✘ |
| `accept_incomplete_routing_info_during_mgmtd_bootstrapping` | 繼承基底（`true`） | 繼承基底（`true`） | 明寫 `true` |
| 設定下發通道 | Heartbeat 回應 | ExtendClientSession 回應 | 無（自己呼 `getConfig`） |
| routing info 從哪來 | 10 秒自動 + 手動 | 10 秒自動 + 手動 | **只有手動** |

最後一列對 `admin_cli` 是重要的操作語意：因為 auto refresh 關閉，`admin_cli` 在初始化時強制拉一次（`src/client/bin/admin_cli.cc:136`），之後**每個需要新鮮拓樸的命令都自己 `refreshRoutingInfo(force=true)`**，例如 `src/client/cli/admin/ListNodes.cc:69`、`DumpChainTable.cc:35`、`OfflineTarget.cc:38`、`QueryChunk.cc:43`。`env.mgmtdClientGetter` 還加了一道保險：拿不到 routing info 就直接丟例外（`admin_cli.cc:157-162`）：

```cpp
env.mgmtdClientGetter = [&] {
  ensureMgmtdClient();
  if (auto ri = mgmtdClient->getRoutingInfo(); !ri || !ri->raw())
    throw StatusException(Status(MgmtdClientCode::kRoutingInfoNotReady));
  return mgmtdClient;
};
```
而 `env.unsafeMgmtdClientGetter`（`admin_cli.cc:164-167`）刻意跳過這個檢查——給那些「整個叢集都掛了，我就是要連上去看看」的命令用，例如 `ShutdownAllChains.cc:39` 就是靠比對 `kPrimaryMgmtdNotFound` 來確認叢集已下線。

### 3.5 `ServiceInfo.h`：唯一零報告提及的檔案

`ServiceInfo.h` 只有 25 行，定義一個 POD：

```cpp
// src/client/mgmtd/ServiceInfo.h:8-24
struct ServiceInfo {
  String name;
  uint16_t id{0};
  flat::NodeId nodeId{0};
  flat::NodeStatus nodeStatus{flat::NodeStatus::HEARTBEAT_CONNECTING};
  std::vector<net::Address> endpoints;

  std::vector<net::Address> filterAddress(net::Address::Type type) const {
    std::vector<net::Address> addresses;
    std::copy_if(endpoints.begin(), endpoints.end(), std::back_inserter(addresses), [&](const auto &addr) {
      return addr.type == type;
    });
    return addresses;
  }
  ServiceInfo() = default;
};
```

它的存在理由只有一個：`flat::NodeInfo` 是以「節點」為中心組織的（一個 node 有多個 service group，每個 group 有多個 service name 與多個 endpoint），而查詢時常常想以「服務」為中心（我要所有提供 `MetaSerde` 的位址）。`ServiceInfo` 就是**把節點視角轉置成服務視角**後的那一列，由 `RoutingInfo::getServiceBy()` 現場拼出來（`RoutingInfo.h:35-58`）：

```cpp
auto smap = node.getAllServices();
for (auto &[si, addrVec] : smap) {
  ServiceInfo info;
  info.name = si;
  info.nodeId = node.app.nodeId;
  info.nodeStatus = node.status;
  info.endpoints = std::move(addrVec);
  if (serviceSelector(info)) res.push_back(std::move(info));
}
```

注意 `id` 欄位（`uint16_t id{0}`）在 `getServiceBy` 裡**從未被賦值**，永遠是 0。`filterAddress()` 在本兩個目錄內也沒有呼叫點——這是給上層查詢用的工具方法（`admin_cli` 的服務清單類命令）。

---

## 4. `MgmtdClient` 內部解剖

### 4.1 pimpl + 單協程 actor

`MgmtdClient.h` 只有 130 行、全部是宣告，實作全在 `struct MgmtdClient::Impl`（`MgmtdClient.cc:116-771`）。門面的每一個方法都是一行轉接，例如：

```cpp
// MgmtdClient.cc:811-817
std::shared_ptr<RoutingInfo> MgmtdClient::getRoutingInfo() { return impl_->getRoutingInfo(); }
CoTryTask<void> MgmtdClient::refreshRoutingInfo(bool force) { co_return co_await impl_->refreshRoutingInfo(force); }
CoTryTask<mgmtd::HeartbeatRsp> MgmtdClient::heartbeat() { co_return co_await impl_->heartbeat(); }
Result<Void> MgmtdClient::triggerHeartbeat() { return impl_->triggerHeartbeat(); }
```

核心是 `actor()`（`MgmtdClient.cc:378-420`）：一條協程，永遠在 `workItems_.dequeue()` 上等，取到 `WorkItem` 就 `std::visit` 分派：

```cpp
for (;;) {
  auto item = co_await workItems_.dequeue();
  auto exitTag = co_await std::visit(handler, item->variant);
  if (exitTag) break;
}
```

`WorkItem` 是四選一的 variant（`MgmtdClient.cc:68-69`）：

| WorkItem | 承載 | 回傳型別 | 產生者 |
|---|---|---|---|
| `ExitWorkItem` | 無 | `Void` | `stop()` |
| `RefreshWorkItem` | `bool force` | `Void` | `refreshRoutingInfo()`、`AutoRefresh` |
| `HeartbeatWorkItem` | 無 | `HeartbeatRsp` | `heartbeat()`、`triggerHeartbeat()`、`AutoHeartbeat` |
| `CallableWorkItem` | `methodName` + `std::function<CoTask<void>(Impl&)>` | `Void` | 其餘所有 RPC（透過 `invoke(op)`） |

`WorkItemBase<T, Value>::makeItemWithWaiter`（`MgmtdClient.cc:38-43`）用 `folly::coro::makePromiseContract` 把「丟進佇列」和「等結果」接起來，所以呼叫端寫起來像同步呼叫，實際上是跨協程的：

```cpp
CoTryTask<void> refreshRoutingInfo(bool force) {
  auto [item, waitItem] = WorkItem::makeItemWithWaiter<RefreshWorkItem>(force);
  CO_RETURN_ON_ERROR(tryEnqueue(std::move(item)));
  co_return co_await std::move(waitItem);
}
```

`triggerHeartbeat()` 是唯一的例外——它用 `makeItem`（無 promise）**射後不理**，回傳只表示「有沒有排進佇列」：

```cpp
// MgmtdClient.cc:666-669
Result<Void> triggerHeartbeat() {
  auto item = WorkItem::makeItem<HeartbeatWorkItem>();
  return tryEnqueue(std::move(item));
}
```

**這個 actor 模型換到的是什麼**：`primaryMgmtdId_`、`addrMap_`、`mgmtds_`、`heartbeatInfo_`、`clientSessionReq_` 全部是裸成員（`MgmtdClient.cc:757-766`），沒有任何鎖。只有需要被 actor 之外讀取的欄位才用 `folly::atomic_shared_ptr` / `folly::Synchronized`：`routingInfo_`、`routingInfoListeners_`、四個 listener/payload 欄位。

`tryEnqueue`（`MgmtdClient.cc:442-451`）是唯一的入口守衛：

```cpp
Result<Void> tryEnqueue(std::unique_ptr<WorkItem> item) {
  auto lock = std::unique_lock(backgroundRunningMu);
  if (!backgroundRunning) return makeError(MgmtdClientCode::kExit);
  if (!workItems_.try_enqueue(std::move(item))) return makeError(MgmtdClientCode::kWorkQueueFull);
  return Void{};
}
```

`work_queue_size` 預設 100（`MgmtdClient.h:16`）。佇列滿了直接回 `kWorkQueueFull`（6001）而不阻塞——背景任務收到這個錯誤只會記一行 log 然後等下一輪。`kExit`（6003）在 `StatusCodeDetails.h:218` 註明 `// internal used`，三個 auto 任務都會特判它並直接 `co_return`（`MgmtdClient.cc:288-290, 301-303, 315-317`）。

### 4.2 `Conn`：一個節點的多位址輪替

```cpp
// MgmtdClient.cc:88-112
struct Conn {
  flat::NodeId nodeId{0};
  std::vector<net::Address> addrs;

  net::Address addr() const { return addrs[addrIdx_]; }
  MgmtdStub &stub(MgmtdStubFactory &factory) {
    if (!stub_) stub_ = factory.create(addr());
    return *stub_;
  }
  void switchAddr() {
    stub_.reset();
    addrIdx_ = (addrIdx_ + 1) % addrs.size();
  }
 private:
  size_t addrIdx_{0};
  std::unique_ptr<MgmtdStub> stub_;
};
```

一個 mgmtd 節點可能同時掛 RDMA 與 TCP 位址（也可能多張網卡），`Conn` 讓「換位址」和「換節點」變成兩個不同層級的動作：`switchAddr()` 只在同一節點內輪替並丟掉 stub 強制重連。

兩張表維護節點與位址的雙向映射（`MgmtdClient.cc:759-761`）：

```cpp
robin_hood::unordered_map<net::Address, flat::NodeId> addrMap_;   // addr → nodeId，0 表示 UNKNOWN
robin_hood::unordered_map<flat::NodeId, Conn> mgmtds_;            // nodeId → Conn
flat::NodeId primaryMgmtdId_{0};
```

`initMgmtds()`（`MgmtdClient.cc:459-476`）把設定檔的位址全部以 `NodeId(0)` 塞進 `addrMap_`，並在此驗證位址型別：

```cpp
if (config_.network_type() && addr.type != *config_.network_type())
  return makeError(StatusCode::kInvalidConfig,
                   fmt::format("Invalid MGMTD address type: {}. expected {}", ...));
```

之後每學到一個真實節點就呼叫 `addMgmtd()`（`MgmtdClient.cc:584-591`）把那些位址的 value 從 0 改成真的 nodeId。`getMgmtdConn(id)` 帶了一道不變式檢查（`MgmtdClient.cc:551-555`）：

```cpp
Conn &getMgmtdConn(flat::NodeId id) {
  auto &conn = mgmtds_[id];
  XLOGF_IF(DFATAL, conn.nodeId != id, "Invalid conn: expected NodeId = {}. actual = {}", id, conn.nodeId);
  return conn;
}
```
注意這裡用的是 `operator[]`——查不到會**插入一個 nodeId 為 0 的空 Conn**，然後 DFATAL。所以在 debug build 它會 abort，release build 會拿到一個沒有任何位址的 `Conn`，隨後 `addr()` 的 `assert(addrIdx_ < addrs.size())` 也會踩空。呼叫點都先確認過 `mgmtds_.contains()`（如 `MgmtdClient.cc:148`）。

### 4.3 primary 探測：三個同名多載構成的 DFS

這是 `MgmtdClient` 最複雜的部分。三個 `probePrimary` 多載：

```cpp
CoTryTask<std::optional<flat::NodeId>> probePrimary(ProbeContext &, Conn &, int probeChainLength = 0);  // :480 核心
CoTryTask<std::optional<flat::NodeId>> probePrimary(ProbeContext &);                                     // :558 入口
CoTryTask<std::optional<flat::NodeId>> probePrimary(net::Address addr);                                  // :577 單位址
```

**核心多載的邏輯**（`MgmtdClient.cc:480-537`）：

1. `probeChainLength > 3` 直接放棄，回 `kSkipThisNode`。註解自陳 `// TODO: avoid hard code`（`MgmtdClient.cc:483`）。
2. 內層 `for(;;)`：若當前位址已探過就 `switchAddr()`；若換完還是探過的，把這個節點**所有**位址標記為已探並回 `kSkipThisNode`。
3. 對當前位址發 `getPrimaryMgmtd`。失敗就 `trySwitchProbeTarget()` 決定下一步。
4. 成功但回應說「沒有 primary」→ 回 `std::nullopt`（不是錯誤）。
5. 成功且拿到 primary 的 `NodeInfo` → `tryAddMgmtd()` 把它的位址學起來。
6. **關鍵的收斂條件**：

```cpp
if (addrMap_[conn.addr()] == primary.nodeId) {
  XLOGF(INFO, "MgmtdClient: probePrimary succeeded at {}", primary.nodeId);
  co_return primary.nodeId;
}
co_return co_await probePrimary(probeContext, getMgmtdConn(primary.nodeId), probeChainLength + 1);
```

也就是說：**只有當「我問的這台」就是「它說的 primary」時才算探測成功**，否則遞迴去問它指的那台。這保證了拿回來的 primary 是**自認為是 primary 的那台**，而不是道聽塗說。

**`trySwitchProbeTarget()`**（`MgmtdClient.cc:138-168`）把錯誤分成三類：

```cpp
if (isNetworkError(code) || code == MgmtdCode::kNotPrimary) {
  if (code == MgmtdCode::kNotPrimary) {
    probeContext.probedAddrs.insert(conn.addrs.begin(), conn.addrs.end());   // 整台標記已探
    uint32_t id = 0;
    auto result = scn::scan(String(error.message()), "{}", id);              // ★ 從錯誤訊息裡解析 nodeId
    if (result) {
      if (mgmtds_.contains(flat::NodeId(id))) co_return flat::NodeId(id);
      else XLOGF(WARN, "MgmtdClient: found unknown potential primary: {}", flat::NodeId(id));
    }
    co_return makeError(kSkipThisNode);
  } else {
    probeContext.probedAddrs.insert(conn.addr());   // 只標記這個位址
    conn.switchAddr();
    if (!probeContext.probed(conn.addr())) co_return conn.nodeId;   // 同節點換位址再試
    ...
    co_return makeError(kSkipThisNode);
  }
}
co_return makeError(std::move(error));   // 其他錯誤原樣往上拋
```

`kNotPrimary` 的處理路徑帶了一個**跨進程的隱形協定**：mgmtd 在拒絕非 primary 請求時，把當前 primary 的 nodeId 寫進錯誤訊息的文字裡，client 用 `scn::scan(msg, "{}", id)` 從字串裡摳出來當提示。這條協定沒有型別保護——訊息格式一改，client 只會退化成「解析失敗 → `kSkipThisNode` → 換一台盲試」，不會壞掉但會變慢。同樣的技巧在 heartbeat 與 client session 的版本衝突處理裡又用了兩次（§4.6、§4.7）。

`kSkipThisNode = 65535`（`MgmtdClient.cc:25`）是一個**假錯誤碼**，建構子裡明確斷言它不屬於任何已註冊的分類：

```cpp
// MgmtdClient.cc:196
assert(StatusCode::typeOf(kSkipThisNode) == StatusCodeType::Invalid);
```
註解也提醒了 `// NOTE: callers must handle kSkipThisNode.`（`MgmtdClient.cc:479`）——它絕不能外洩給呼叫端。

**入口多載**（`MgmtdClient.cc:558-574`）定義了搜尋順序：

```
1. 若已知 primaryMgmtdId_ ≠ 0 → 先從它開始探
2. 否則（或上一步 kSkipThisNode）→ 依序遍歷 mgmtds_ 裡所有已知節點
3. 都不行 → probeUnknownAddrs()：只探 addrMap_ 裡 nodeId 仍為 0 的位址
```

第 3 步（`MgmtdClient.cc:539-549`）刻意只探「未知」位址，因為已知位址在第 2 步已經透過 `mgmtds_` 覆蓋過了。

### 4.4 `withRetry`：所有 RPC 的統一外殼

```cpp
// MgmtdClient.cc:170-189
template <typename F>
auto withRetry(std::string_view methodName, F &&f) -> std::invoke_result_t<F, MgmtdStub &> {
  ProbeContext probeContext;
  if (primaryMgmtdId_ == 0) CO_RETURN_ON_ERROR(co_await connect(probeContext));

  for (bool retried = false;;) {
    auto &conn = getMgmtdConn(primaryMgmtdId_);
    auto &stub = conn.stub(*mgmtdStubFactory_);
    ...
    auto res = co_await f(stub);
    LOG_RESULT(INFO, res, "MgmtdClient: {}", methodName);
    if (!res.hasError() || retried) co_return res;

    CO_RETURN_ON_ERROR(co_await retryOnError(probeContext, conn, res.error()));
    retried = true;
  }
}
```

**只重試一次。** `retried` 一旦為 true，下一次失敗就原樣回傳。這與 `MetaClient` 的「60 秒內指數退避」形成強烈對比——因為 mgmtd 側的失敗多半意味著「primary 換人了」，重新探測一次就夠；如果探測後仍失敗，多半是叢集層面的問題，再重試沒有意義（而且會拖住 actor，堵住整條佇列）。

`retryOnError`（`MgmtdClient.cc:123-136`）：

```cpp
auto switchRes = co_await trySwitchProbeTarget(probeContext, conn, error);
if (switchRes.hasError() && switchRes.error().code() != kSkipThisNode) CO_RETURN_ERROR(switchRes);
if (!switchRes.hasError()) {
  if (*switchRes == conn.nodeId) co_return Void{};   // 同節點換了位址，直接重試
  primaryMgmtdId_ = *switchRes;                       // 有提示的新 primary，先信它
}
auto primaryRes = co_await probePrimary(probeContext);   // 再完整探一次確認
if (!primaryRes.hasError() && primaryRes->has_value()) {
  primaryMgmtdId_ = primaryRes->value();
  co_return Void{};
}
co_return makeError(MgmtdClientCode::kPrimaryMgmtdNotFound);
```

注意即使拿到了「錯誤訊息裡的提示 nodeId」，它也**不會直接拿去發請求**，而是先寫進 `primaryMgmtdId_` 再走一次 `probePrimary`——讓 §4.3 的收斂條件（自認 primary）再驗證一次。

`ProbeContext` 是**函式區域變數**，每次 `withRetry` 呼叫都新建一個。這代表「這台探不通」的記憶只活在一次 RPC 的重試週期內，下一次 RPC 會重新給每台機會。

24 個 mgmtd RPC 全部由一個 X-macro 展開成 `XxxOp` 結構（`MgmtdClient.cc:774-794`），每個都在 `handle()` 裡呼叫 `withRetry`：

```cpp
#define DEFINE_SERDE_SERVICE_METHOD_FULL(svc, name, Name, id, reqtype, rsptype)             \
  struct Name##Op : core::ServiceOperationWithMetric<"MgmtdClient", #Name, "op"> {          \
    ...                                                                                     \
    CoTryTask<ResType> handle(Impl &impl) {                                                 \
      co_return co_await impl.withRetry(#Name, [&](MgmtdStub &stub) -> CoTryTask<ResType> { \
        co_return co_await stub.name(req);                                                  \
      });                                                                                   \
    }                                                                                       \
    ReqType req;                                                                            \
  };
#include "fbs/mgmtd/MgmtdServiceDef.h"
```

同一個 `MgmtdServiceDef.h`（24 行）在伺服端展開成 service + operator（見 mgmtd 報告 §2），在客戶端展開成 op 結構。新增一個 mgmtd RPC 只要在 def 檔加一行 + 在 `MgmtdClient` 加一個門面方法。

### 4.5 routing info：拉取、版本比對、推播

`refreshRoutingInfoImpl`（`MgmtdClient.cc:601-620`）：

```cpp
auto currentInfo = getRoutingInfo();
auto curRoutingInfo = currentInfo ? currentInfo->raw() : nullptr;
auto currentVersion = curRoutingInfo ? curRoutingInfo->routingInfoVersion : flat::RoutingInfoVersion(0);

auto res = co_await withRetry("RefreshRoutingInfo", [&](MgmtdStub &stub) -> CoTryTask<mgmtd::GetRoutingInfoRsp> {
  co_return co_await stub.getRoutingInfo(
      mgmtd::GetRoutingInfoReq::create(clusterId_, force ? flat::RoutingInfoVersion{0} : currentVersion));
});
```

**版本比對在伺服端做**：client 把自己手上的版本號送上去，mgmtd 若發現 client 已是最新就回一個**空的 `info`**（`res->info` 為 `nullopt`），省掉整份拓樸的序列化與傳輸。`force=true` 時送 0，強制拿全量——這正是 mgmtd 報告 §3.2 說「版本 0 是保留值」的客戶端對應。

這是**輪詢，不是推播**：`AutoRefresh` 每 `auto_refresh_interval`（預設 10 秒）跑一次。mgmtd 沒有任何主動通知 client 的通道。

`updateRoutingInfo`（`MgmtdClient.cc:622-658`）做四件事：

```cpp
XLOGF_IF(FATAL, currentVersion > newRoutingInfo->routingInfoVersion,
         "RoutingInfoVersion rollback from {} to {}", ...);
```
**(a) 版本回退直接 FATAL。** 這是把 mgmtd 端的單調性保證（見 mgmtd 報告 §0）在 client 側做成硬斷言——寧可讓進程死掉，也不接受一份倒退的拓樸。

```cpp
for (const auto &[id, node] : newRoutingInfo->nodes) {
  if (node.status == flat::NodeStatus::HEARTBEAT_CONNECTING) connectingNodes.push_back(id);
  tryAddMgmtd(node.app.nodeId, node.app.serviceGroups);
}
```
**(b) 順手從新拓樸裡學習 mgmtd 節點。** `tryAddMgmtd` 對非 mgmtd 節點會因為 `extractAddresses(..., "Mgmtd", ...)` 為空而回 false（`MgmtdClient.cc:593-599`），所以這個迴圈實際只會撿出 mgmtd。這讓 client 的備援 mgmtd 清單**隨拓樸自動擴充**，不必改設定檔。

```cpp
if (!config_.accept_incomplete_routing_info_during_mgmtd_bootstrapping() && newRoutingInfo->bootstrapping &&
    !connectingNodes.empty()) {
  XLOGF(INFO, "MgmtdClient: discard incomplete routing info version {}, [{}] still connecting", ...);
  return;
}
```
**(c) bootstrapping 期間的不完整拓樸可以被丟棄**——但預設值是 `true`（`MgmtdClient.h:25`），即**預設接受**。要拒絕必須明確關掉這個開關。

```cpp
auto ri = std::make_shared<RoutingInfo>(newRoutingInfo ? newRoutingInfo : curRoutingInfo, SteadyClock::now());
routingInfo_.store(ri, std::memory_order_release);

if (newRoutingInfo) {
  auto listenersPtr = routingInfoListeners_.rlock();
  for (const auto &[_, listener] : *listenersPtr) listener(ri);
}
```
**(d) 即使沒有新資料也會重建 `RoutingInfo` 物件**——因為 `lastRefreshTime_` 要更新（拓樸沒變但「我剛確認過」這件事本身是資訊）。但 **listener 只在真的有新資料時才觸發**。這個不對稱是 `ServerSelectionStrategy` 能用指標比對做快速去重的前提（§7.1）。

listener 是同步呼叫，且**在持有 `routingInfoListeners_` 讀鎖的情況下**呼叫。任何 listener 裡若嘗試 `addRoutingInfoListener`（需要寫鎖）就會死鎖。實際的 listener（`ServerSelectionStrategy::update`、storage 的 `Components::refreshRoutingInfo`）都只做本地資料替換。

### 4.6 心跳：版本號的自我修正

`heartbeatImpl(retryable)`（`MgmtdClient.cc:688-743`）：

```cpp
if (!heartbeatInfo_) {
  auto appInfo = appInfo_.load(std::memory_order_acquire);
  if (!appInfo) co_return makeError(StatusCode::kInvalidArg, "AppInfo not set");
  heartbeatInfo_ = std::make_unique<flat::HeartbeatInfo>(*appInfo);
}
auto payload = heartbeatPayload_.load(std::memory_order_acquire);
if (!payload) co_return makeError(StatusCode::kInvalidArg, "Payload not set");
heartbeatInfo_->set(*payload);
heartbeatInfo_->configStatus = ApplicationBase::getConfigStatus();
```

`AppInfo`（誰我是）只設一次，`Payload`（我現在的狀態）每次心跳前重新讀取——`updateHeartbeatPayload` 是 storage 每次 target 狀態變化時呼叫的。`configStatus` 每次都從 `ApplicationBase` 現撈，這是 client 向 mgmtd 回報「我的設定套用成功了沒」的通道。

版本衝突處理：

```cpp
if (res.error().code() == MgmtdCode::kHeartbeatVersionStale) {
  uint64_t v = 0;
  auto result = scn::scan(String(res.error().message()), "{}", v);
  if (result) heartbeatInfo_->hbVersion = flat::HeartbeatVersion(v + 1);
  else        heartbeatInfo_->hbVersion = flat::HeartbeatVersion(heartbeatInfo_->hbVersion + 1);
  if (retryable) co_return co_await heartbeatImpl(/*retryable=*/false);
}
```

又一次「從錯誤訊息文字裡 scan 出數字」。`retryable` 參數保證這個自我修正**只做一次**（`HeartbeatOp::handle` 傳 `true`，`MgmtdClient.cc:674`；遞迴時傳 `false`）。成功路徑也遞增版本（`MgmtdClient.cc:730`），所以 `hbVersion` 在正常情況下每次心跳 +1。

設定下發（`MgmtdClient.cc:731-740`）：

```cpp
if (res->config) {
  auto listener = serverConfigListener_.load(std::memory_order_acquire);
  // do not update config version when listener return false
  if (!listener || (*listener)(res->config->content, fmt::format("{}", res->config->configVersion))) {
    if (!listener) XLOGF(WARN, "MgmtdClient: discard new config since no listener found");
    heartbeatInfo_->configVersion = res->config->configVersion;
  }
}
```

**listener 回傳失敗時不更新本地記錄的 `configVersion`**，因此下一次心跳仍會帶著舊版本上去，mgmtd 會再下發一次——形成一個「套用成功前不斷重試」的迴圈。註解 `// do not update config version when listener return false` 直接說明了這個意圖。但要注意條件是 `!listener || (*listener)(...)`：**沒有設 listener 時也會推進版本**（並記一行 WARN），等於默默丟棄設定。

### 4.7 client session：註冊即續期

`ExtendClientSessionOp::handle`（`MgmtdClient.cc:326-375`）：

```cpp
auto payload = impl.clientSessionPayload_.load(std::memory_order_acquire);
if (!payload) co_return makeError(StatusCode::kInvalidArg, "ClientSessionPayload not set");
if (!impl.clientSessionReq_ || impl.clientSessionReq_->clientId != payload->clientId ||
    impl.clientSessionReq_->data != payload->data) {
  auto req = std::make_unique<mgmtd::ExtendClientSessionReq>();
  req->clusterId = impl.clusterId_;
  req->clientId = payload->clientId;
  req->data = payload->data;
  req->type = payload->nodeType;
  req->user = payload->userInfo;
  req->clientStart = impl.startTime_;
  impl.clientSessionReq_ = std::move(req);
}
impl.clientSessionReq_->configStatus = ApplicationBase::getConfigStatus();
```

請求物件被**快取並重複使用**，只有在 `clientId` 或 `data` 改變時才重建。`clientStart` 用的是 `Impl` 建構時記下的 `startTime_`（`MgmtdClient.cc:745`），這讓 mgmtd 能區分「同一個 clientId 重啟過」與「持續存活」。

版本衝突的處理與心跳同構（`kClientSessionVersionStale` → scan → +1），但**沒有自動重試**——只把版本修好，等下一個 10 秒週期。

「註冊」與「續期」是同一個 RPC。FUSE 的首次註冊只是把它包在一個重試迴圈裡（`src/fuse/FuseClients.cc:31-45`）：

```cpp
Result<Void> establishClientSession(client::IMgmtdClientForClient &mgmtdClient) {
  return folly::coro::blockingWait([&]() -> CoTryTask<void> {
    auto retryInterval = std::chrono::milliseconds(10);
    constexpr auto maxRetryInterval = std::chrono::milliseconds(1000);
    Result<Void> res = Void{};
    for (int i = 0; i < 40; ++i) {
      res = co_await mgmtdClient.extendClientSession();
      if (res) break;
      co_await folly::coro::sleep(retryInterval);
      retryInterval = std::min(2 * retryInterval, maxRetryInterval);
    }
    co_return res;
  }());
}
```
最多 40 次、退避上限 1 秒——最壞情況約 39 秒後放棄掛載。

FUSE 端的 payload 內容（`src/fuse/FuseClients.cc:112-120`）：

```cpp
mgmtdClient->setClientSessionPayload({clientId.uuid.toHexString(),
                                      flat::NodeType::FUSE,
                                      flat::ClientSessionData::create(
                                          /*universalId=*/*physicalHostnameRes,
                                          /*description=*/fmt::format("fuse: {}", *containerHostnameRes),
                                          appInfo.serviceGroups,
                                          appInfo.releaseVersion),
                                      flat::UserInfo{}});
```

`clientId.uuid.toHexString()` 是**同一個 `ClientId`**，稍後也被傳給 `MetaClient` 建構子（`FuseClients.cc:131`）與 `StorageClient::create`（`FuseClients.cc:128`）。這個共用的 UUID 就是 §0 說的「兩個函式庫間接握手」的介質：meta server 用 `listClientSessions()` 拿到的 clientId 集合，與 file session 上記錄的 `SessionInfo::client.uuid` 比對（`src/meta/components/SessionManager.cc:55-72`）。

### 4.8 生命週期

```cpp
// MgmtdClient.cc:216-238
CoTask<void> start(folly::Executor *backgroundExecutor, bool startBackground) {
  auto lock = std::unique_lock(backgroundRunningMu);
  if (backgroundRunning == false) {
    if (!backgroundExecutor) backgroundExecutor = co_await folly::coro::co_current_executor;
    backgroundExecutor_ = backgroundExecutor;
    actor().scheduleOn(backgroundExecutor).start();
  }
  if (startBackground && !backgroundRunner_) startBackgroundTasksWithLock();
  backgroundRunning = true;
}
```

`startBackgroundTasks()` 是給「先 `start(…, false)`、後來才要開背景」的場景用的——正是 §1.1 的 launcher 交棒路徑。但 repo 內沒有任何呼叫點（`grep startBackgroundTasks` 只命中宣告與轉接），也就是說 launcher 交棒後的 server **背景任務是在哪裡啟動的**？答案在 `beforeStart()`：server 會再呼叫一次 `mgmtdClient_->start(...)`（`src/meta/service/MetaServer.cc:41`、`src/storage/service/Components.cc:90`），此時 `backgroundRunning` 已是 true 但 `backgroundRunner_` 仍是 null，於是走 `if (startBackground && !backgroundRunner_)` 分支補啟動。

`stop()`（`MgmtdClient.cc:263-278`）：先把 `backgroundRunning` 設 false（此後所有 `tryEnqueue` 回 `kExit`），再丟一個帶 promise 的 `ExitWorkItem` 進去等 actor 確實退出，最後停背景任務。`~Impl()` 直接 `folly::coro::blockingWait(stop())`（`MgmtdClient.cc:199`），所以忘記呼叫 `stop()` 也不會洩漏協程。

---

## 5. `client::RoutingInfo`：一層薄包裝與一個時間戳

`RoutingInfo.h/.cc` 共 125 行，包住 `flat::RoutingInfo`（線上格式）並加兩樣東西：

```cpp
// src/client/mgmtd/RoutingInfo.h:10-63
class RoutingInfo {
 public:
  RoutingInfo(std::shared_ptr<flat::RoutingInfo> info, SteadyTime refreshTime);
  const auto &raw() const { return info_; }
  SteadyTime lastRefreshTime() const { return lastRefreshTime_; }
  std::optional<flat::ChainInfo> getChain(flat::ChainId id) const;
  std::optional<flat::ChainInfo> getChain(flat::ChainRef ref) const;
  std::optional<flat::TargetInfo> getTarget(flat::TargetId id) const;
  std::optional<flat::NodeInfo> getNode(flat::NodeId id) const;
  template <UnaryPredicate<flat::NodeInfo> NodeSelector> std::vector<flat::NodeInfo> getNodeBy(...) const;
  template <...> std::vector<ServiceInfo> getServiceBy(...) const;
 private:
  std::shared_ptr<flat::RoutingInfo> info_;
  SteadyTime lastRefreshTime_;
};
```

三個設計選擇值得指出：

**(a) 四個 `getXxx` 全部回 `std::optional<T>`（值拷貝），而 `raw()` 回的是 `shared_ptr` 的 const 參考。** `flat::RoutingInfo::getChain()` 回的是裸指標，這裡把它轉成值：

```cpp
// RoutingInfo.cc:12-17
std::optional<flat::ChainInfo> RoutingInfo::getChain(flat::ChainId id) const {
  if (auto *c = info_ ? info_->getChain(id) : nullptr) return *c;
  return std::nullopt;
}
```
拷貝的代價換到的是**呼叫端不必持有 `RoutingInfo` 的生命週期**。而需要遍歷整份拓樸的熱路徑（storage client 的定址、`MetaClient::truncateImpl` 的 `FileOperation`）則用 `raw()` 直接拿內部指標，避開拷貝。`info_` 本身可能是 null（預設建構的 `RoutingInfo()` 就是），所有存取都做了 `info_ ?` 檢查。

**(b) `lastRefreshTime_` 用 `SteadyTime`。** 與 mgmtd 報告 §3.1 的觀察一致：所有超時判定都基於單調時鐘。這個欄位在本兩個目錄內**沒有任何讀取點**——它是給上層（storage client 判斷拓樸新鮮度）用的。

**(c) `logUnavailableChains()`（`RoutingInfo.cc:40-58`）是一個自由函式而非成員。** 它遍歷所有 chain，找出「沒有任何一個 target 處於 `SERVING`」的 chain 並記 WARN：

```cpp
if (raw->bootstrapping) {
  XLOGF(WARN, "Skip log unavailable chains since mgmtd is bootstrapping");
}
for (const auto &[cid, ci] : raw->chains) {
  auto it = std::find_if(ci.targets.begin(), ci.targets.end(),
                         [](const auto &cti) { return cti.publicState == flat::PublicTargetState::SERVING; });
  if (it == ci.targets.end()) XLOGF(WARN, "Found unavailable chain in new RoutingInfo {}: {}", ...);
}
```

注意 `bootstrapping` 分支只印了一行 WARN 就**繼續往下跑**（沒有 `return`），所以 bootstrapping 期間仍然會逐條印出不可用的 chain。從日誌措辭（"Skip log unavailable chains"）看，這與實際行為不一致。

---

## 6. `MetaClient`：無狀態的重試機

### 6.1 完整方法表與 RPC 對照

`MetaClient` 對外暴露 28 個公開方法（`MetaClient.h:106-226`，不含 `start`/`stop`），映射到 `MetaSerde` 的 22 個 method id 中的 16 個：

| `MetaClient` 方法 | RPC（method id） | 請求型別 | 帶冪等 uuid | 備註 |
|---|---|---|---|---|
| `authenticate` | `authenticate` (18) | `AuthReq` | ✘ | 回傳補完的 `UserInfo` |
| `stat` | `stat` (2) | `StatReq` | ✘ | |
| `batchStat` | `batchStat` (20) | `BatchStatReq` | ✘ | 舊 server 回 `kInvalidMethodID` 時退化成逐個 `stat` |
| `batchStatByPath` | `batchStatByPath` (21) | `BatchStatByPathReq` | ✘ | 同上 |
| `statFs` | `statFs` (1) | `StatFsReq` | ✘ | |
| `getRealPath` | `getRealPath` (14) | `GetRealPathReq` | ✘ | |
| `open` | `open` (8) | `OpenReq` | ✘ | 經 `openCreate()` |
| `create` | `create` (3) | `CreateReq` | ✔ | 經 `openCreate()` |
| `close`（兩個多載） | `close` (10) | `CloseReq` | ✘ | 失敗會轉背景重試 |
| `setPermission` | `setAttr` (15) | `SetAttrReq::setPermission` | ✘ | |
| `setIFlags` | `setAttr` (15) | `SetAttrReq::setIFlags` | ✘ | |
| `utimes` | `setAttr` (15) | `SetAttrReq::utimes` | ✘ | |
| `setLayout` | `setAttr` (15) | `SetAttrReq::setLayout` | ✘ | |
| `extendStripe` | `setAttr` (15) | `SetAttrReq::extendStripe` | ✘ | |
| `mkdirs` | `mkdirs` (4) | `MkdirsReq` | ✔ | |
| `symlink` | `symlink` (5) | `SymlinkReq` | ✔ | |
| `remove` / `rmdir` / `unlink` | `remove` (7) | `RemoveReq` | ✔ | 三者只差 `atFlags` / `recursive` / `checkType` |
| `rename` | `rename` (11) | `RenameReq` | ✔ | 回應無 `stat` 時補一次 `stat` |
| `list` | `list` (12) | `ListReq` | ✘ | |
| `sync`（兩個多載） | `sync` (9) | `SyncReq` | ✘ | |
| `hardLink` | `hardLink` (6) | `HardLinkReq` | ✔ | |
| `lockDirectory` | `lockDirectory` (19) | `LockDirectoryReq` | ✘ | |
| `truncate` | **無**（storage 直操作 + `sync`） | — | — | 見 §6.12 |
| `testRpc` | `testRpc` (50) | `TestRpcReq` | ✘ | |
| （背景）`pruneSession` | `pruneSession` (16) | `PruneSessionReq` | ✘ | 不走 `retry()` |

**`MetaClient` 從不呼叫的 stub 方法**：`truncate` (13，schema 註明 `// deperated:`)、`dropUserCache` (17，只有 `src/client/cli/admin/DropUserCache.cc:55` 自建 stub 呼叫)。

三個「一個 RPC 多個介面」的整併值得注意：`setAttr` 被五個方法共用（靠 `SetAttrReq` 的五個靜態工廠區分，`src/fbs/meta/Service.h:592-616`），`remove` 被三個方法共用，`RemoveReq` 三個參數的組合就是三種語意：

```cpp
// MetaClient.cc:780-799
remove: RemoveReq(userInfo, PathAt(parent, path), AtFlags(0),           recursive, false)
unlink: RemoveReq(userInfo, PathAt(parent, path), AtFlags(0),           false,     true)
rmdir:  RemoveReq(userInfo, PathAt(parent, path), AtFlags(AT_REMOVEDIR), recursive, true)
```
`checkType`（第 5 個參數）為 false 就是「不檢查型別，是什麼都刪」——這正是 `remove()` 註解說的 `// remove without check inode type`（`MetaClient.h:176`）。

### 6.2 `retry()`：所有前台請求的統一骨架

`MetaClient::retry`（`MetaClient.cc:339-469`）是本函式庫最重要的 130 行。它的簽名用成員函式指標把「打哪個 RPC」變成參數：

```cpp
template <typename Func, typename Req>
auto MetaClient::retry(Func &&func, Req &&req, RetryConfig retryConfig, std::function<void(const Status &)> onError)
    -> std::invoke_result_t<Func, Stub::IStub, Req &&, const net::UserRequestOptions &, serde::Timestamp *>;
```

呼叫端一律寫成 `co_await retry(&IMetaServiceStub::stat, req)`。

**前置**（`MetaClient.cc:342-354`）：

```cpp
req.client = clientId_;          // ★ 客戶端身分在此統一注入
CHECK_REQUEST(req);              // req.valid()，失敗直接回錯，不發網路
auto opName = MetaSerde<>::getRpcName(req);
ExponentialBackoffRetry backoff(retryConfig.retry_init_wait().asMs(),
                                retryConfig.retry_max_wait().asMs(),
                                retryConfig.retry_total_time().asMs());
auto options = net::UserRequestOptions();
options.timeout = retryConfig.rpc_timeout();
OperationRecorder::Guard record(OperationRecorder::client(), opName, req.user.uid);
```

`req.client = clientId_` 是整個檔案裡唯一設定 `ClientId` 的地方——所有請求的身分注入集中於此。`CHECK_REQUEST` 在送出前跑 `req.valid()`（`MetaClient.cc:60-67`），例如 `CreateReq::valid()` 會檢查「非唯讀開啟必須帶 session」（`src/fbs/meta/Service.h:208`）。

**主迴圈**分四段：

**① 取得伺服器**（`MetaClient.cc:356-384`）。`getServer()` 是個 lambda，只在 `server` 為空時才選一台；選不到就退避後重試（而不是直接失敗）：

```cpp
auto waitTime = backoff.getWaitTime();
if (waitTime.count() == 0) CO_RETURN_ERROR(result);
co_await folly::coro::sleep(waitTime);
co_return Void{};
```
所以「叢集裡暫時沒有可用 meta server」不會立刻報錯，會在 `retry_total_time`（60 秒）內持續等待。

**② 發請求**（`MetaClient.cc:387-415`），包在 `SemaphoreGuard` 裡：

```cpp
SemaphoreGuard concurrentReq(concurrentReqSemaphore_);
co_await concurrentReq.coWait();
serde::Timestamp timestamp;
auto result = co_await (server->stub.get()->*func)(std::forward<Req>(req), options, &timestamp);
RECORD_TIMESATAMP(opName, result, timestamp);
```

`serde::Timestamp` 由 RPC 層填入，`RECORD_TIMESATAMP`（`MetaClient.cc:74-82`）把它拆成三個指標——`inflightLatency` / `serverLatency` / `networkLatency`——並且**只在非 RPC 類錯誤時記錄**（因為 RPC 失敗時時間戳沒有意義）：

```cpp
if (r || hf3fs::StatusCode::typeOf(r.error().code()) != hf3fs::StatusCodeType::RPC) { ... }
```

**③ 成功路徑**（`MetaClient.cc:400-412`）：

```cpp
if (ErrorHandling::success(result)) {
  record.finish(result);
  errNodes_.wlock()->erase(server->node.nodeId);       // ★ 成功即從黑名單移除
  ...
  if (result.hasValue()) CO_RETURN_ON_ERROR(co_await waitRoutingInfo(*result, retryConfig));
  co_return result;
}
```

注意 `ErrorHandling::success()` 的語意是「操作正常完成，**或回了一個預期內的錯誤碼**」（`src/fbs/meta/Utils.h:43-44` 的註解）。`kNotFound`、`kExists`、`kNoPermission` 這些都算 success——它們會沿著這條路徑回給呼叫端，而不會觸發重試。

**④ 失敗路徑**（`MetaClient.cc:416-468`）：

```cpp
switch (error.code()) {
  case RPCCode::kRequestRefused:
    reqRejected.addSample(1, {{"tag", fmt::format("{}", server->node.nodeId.toUnderType())}});
    [[fallthrough]]
  case RPCCode::kSendFailed:
  case MetaCode::kBusy:
    waitTime = std::min(retryConfig.retry_init_wait(), retryConfig.retry_fast());
    break;
  default: break;
}
```
三種「快速重試」錯誤把等待時間壓成 `min(retry_init_wait, retry_fast)` = `min(500ms, 1s)` = 500ms（預設值下）。注意 `kRequestRefused` 那個 case **沒有 break**，是刻意的 fallthrough（但原始碼未加 `[[fallthrough]]` 標註）。

```cpp
auto retryable = ErrorHandling::retryable(error);
bool failover = false;
if (ErrorHandling::serverError(error)) {
  serverError.addSample(1, {{"tag", ...}});
  failover = (++server->failure) >= retryConfig.max_failures_before_failover();
}
if (failover) {
  errNodes_.wlock()->insert(server->node.nodeId);
  server = std::nullopt;                          // 下一圈 getServer() 會重選
}
if (!retryable) { record.finish(error); co_return makeError(std::move(error)); }
if (waitTime.count() == 0 || backoff.getElapsedTime() > retryConfig.retry_total_time().asMs()) {
  XLOGF(CRITICAL, "Op {}{} failed {}, retry timeout", opName, req, error);
  record.finish(error); co_return makeError(std::move(error));
}
co_await folly::coro::sleep(waitTime);
record.retry()++;
```

`server->failure` 是**這次 `retry()` 呼叫的區域計數**——`server` 是函式內的 `std::optional<ServerNode>`，換機後計數歸零。`max_failures_before_failover` 預設 1（`MetaClient.h:67`），所以**預設行為是第一次 server error 就換機**。

### 6.3 錯誤三分法

`ErrorHandling`（`src/fbs/meta/Utils.h:40-156`）是 client 與 server 共用的分類器，三個正交的判斷：

| 判斷 | 語意 | client 用途 |
|---|---|---|
| `success(status)` | 操作完成（含預期內的業務錯誤） | 決定要不要回給呼叫端、要不要記 failed 指標 |
| `retryable(status)` | 重試有意義 | 決定要不要進下一圈 |
| `serverError(status)` | 是這台伺服器的問題 | 決定要不要 failover |

具體分類（節選）：

```cpp
// success: Meta 類
case MetaCode::kNotFound: case kNotEmpty: case kNotDirectory: case kTooManySymlinks:
case kIsDirectory: case kExists: case kNoPermission: case kNotFile: case kInvalidFileLayout:
case kMoreChunksToRemove: case kNameTooLong:  → true
default: return (code >= MetaCode::kExpected && code < MetaCode::kRetryable);   // 3100~3199
```

```cpp
// retryable: Meta 類明確不可重試的
kNotFound, kNotEmpty, kNotDirectory, kTooManySymlinks, kIsDirectory, kExists, kNoPermission,
kInconsistent, kNotFile, kBadFileSystem, kInvalidFileLayout, kFileHasHole, kNameTooLong,
kRequestCanceled, kFoundBug  → false
// 明確可重試的
kOTruncFailed, kMoreChunksToRemove, kBusy, kInodeIdAllocFailed → true
default: return code < MetaCode::kNotRetryable;   // < 3300
```

```cpp
// serverError
case StatusCodeType::Transaction: return code == TransactionCode::kNetworkError;
case StatusCodeType::RPC:         return true;
default:                          return false;
```

錯誤碼區間本身編碼了語意（`src/common/utils/StatusCodeDetails.h:118-147`）：`3100 kExpected` / `3200 kRetryable` / `3300 kNotRetryable` 是三個分界哨兵，新增錯誤碼時只要放進對的區間就自動獲得正確的分類行為。

一個容易誤解的地方：`retryable` 的 `default` 分支（非 Common / Meta / StorageClient 類型）是 `return code != RPCCode::kInvalidMethodID`——**所有 RPC 錯誤都可重試，除了「這台伺服器不認得這個 method」**。這正是 §6.14 相容性退化的觸發點。

### 6.4 failover 與 `errNodes_` 探活

`errNodes_` 是 `folly::Synchronized<std::set<flat::NodeId>>`（`MetaClient.h:297`），語意是「暫時別選這幾台」。它被三處寫入：

- `retry()` 成功 → `erase`（`MetaClient.cc:403`）
- `retry()` failover → `insert`（`MetaClient.cc:452`）
- `checkServers()` 探活成功 → `erase`（`MetaClient.cc:277`）

`checkServers()`（`MetaClient.cc:261-299`）由 `BackgroundRunner` 每 `check_server_interval`（預設 5 秒）跑一次，對黑名單裡的每一台**並行**發一個 `testRpc`：

```cpp
auto guard = errNodes_.wlock();
auto iter = guard->begin();
while (iter != guard->end()) {
  auto nodeId = *iter;
  auto node = serverSelection_.load()->get(nodeId);
  if (!node.has_value()) {
    XLOGF(INFO, "Node {} not found", nodeId);
    iter = guard->erase(iter);          // 節點已從拓樸消失，不必再追蹤
  } else {
    tasks.push_back(folly::coro::co_invoke(check, *node).scheduleOn(co_await folly::coro::co_current_executor).start());
    iter++;
  }
}
guard.unlock();
co_await folly::coro::collectAllRange(std::move(tasks));
```

注意寫鎖在**啟動所有探測協程之後才釋放**，而探測協程內部又要拿寫鎖 `erase`（`MetaClient.cc:277`）——`.start()` 只是排程，實際執行在 `collectAllRange` 之後，而那時鎖已釋放，所以不會自我死鎖。

`ASSERT_NO_REQUEST_CONTEXT()`（`MetaClient.cc:84-88`）在三個背景任務入口（`scanCloseTask`、`runCloseTask`、`checkServers`）都被呼叫：

```cpp
auto ctx = folly::RequestContext::try_get();
XLOGF_IF(FATAL, ctx != nullptr, "RequestContext {} != NULL", (void *)ctx);
```
背景協程若攜帶了前台請求的 `RequestContext`，取消訊號會誤傳。這是一個**用 FATAL 保護的不變式**。

### 6.5 `waitRoutingInfo`：成功路徑上的隱藏同步點

```cpp
// MetaClient.cc:319-337
template <typename Rsp>
CoTryTask<Void> MetaClient::waitRoutingInfo(const Rsp &rsp, const RetryConfig &retryConfig) {
  auto begin = SteadyClock::now();
  auto waitTime = retryConfig.retry_total_time() / 2;
  while (true) {
    auto routingInfo = mgmtd_->getRoutingInfo()->raw();
    if (RoutingInfoChecker::checkRoutingInfo(rsp, *routingInfo)) co_return Void{};
    if (begin + waitTime < SteadyClock::now()) {
      XLOGF(ERR, "routing info not ready, rsp {}", rsp);
      co_return makeError(MgmtdClientCode::kRoutingInfoNotReady);
    }
    XLOGF(WARN, "wait new routing Info, rsp {}", rsp);
    co_await folly::coro::sleep(std::chrono::seconds(1));
  }
}
```

**它解決的問題**：meta server 可能拿到比 client 更新的 routing info，於是回了一個 layout 指向 client 還不知道的 chain table 版本。若直接回給呼叫端，接下來的 storage 讀寫會定址失敗。

**`RoutingInfoChecker`（`src/fbs/meta/Utils.h:300-434`）是編譯期反射的一個漂亮應用**：`hasInode<T>()` 遞迴走過 serde 結構、variant、vector、set、map、optional，判斷型別 `T` 裡到底有沒有藏著 `Inode`。沒有的話 `checkRoutingInfo` 直接 `if constexpr (!hasInode<T>()) return true;` 被編譯掉，零執行期成本。有的話才真的去比對：

```cpp
// Utils.h:347-370
static bool checkRoutingInfo(const Inode &inode, const flat::RoutingInfo &routing) {
  if (inode.isFile()) {
    auto table = inode.asFile().layout.tableId;
    auto tableVer = inode.asFile().layout.tableVersion;
    switch (inode.asFile().layout.type()) {
      case Layout::Type::ChainRange:
        if (!routing.getChainTable(table, tableVer)) { XLOGF(WARN, ...); return false; }
        break;
      case Layout::Type::ChainList:
        if (table && tableVer && !routing.getChainTable(table, tableVer)) { ... return false; }
        break;
      case Layout::Type::Empty: break;
    }
  }
  return true;
}
```

檔案末尾用 `static_assert` 把幾個回應型別釘死（`Utils.h:436-446`）：`OpenRsp` / `StatRsp` / `SyncRsp` / `CloseRsp` / `BatchStatRsp` 都必須 `hasInode`——如果有人重構掉了這些欄位，編譯就會失敗。

**兩個值得注意的性質**：

1. 這個迴圈**不主動觸發 refresh**——它只 sleep 並重讀 `mgmtd_->getRoutingInfo()`，依賴 `MgmtdClient` 的 10 秒 `AutoRefresh` 把新拓樸帶進來。預設值下最長等 30 秒（`retry_total_time / 2`），期間最多能等到 3 次自動刷新。
2. 逾時後回 `kRoutingInfoNotReady`，**整個操作被判為失敗**——即使 RPC 本身成功了。副作用（例如檔案已建立）已經發生，client 只是拿不到結果。

### 6.6 併發閘：`Semaphore`

```cpp
// MetaClient.cc:122
concurrentReqSemaphore_(config_.max_concurrent_requests())     // 預設 128
```

`hf3fs::Semaphore`（`src/common/utils/Semaphore.h:10-59`）是「最大容量固定、有效容量可調」的信號量——建構時就配置 `maxTokens`（預設 4096），然後把多出來的 token 全部 `wait()` 掉：

```cpp
for (size_t i = usableTokens_; i < maxTokens_; i++) semaphore_.wait();
```
`changeUsableTokens()` 則靠成對的 `wait()` / `signal()` 增減。這讓 `max_concurrent_requests` 可以熱更新（`MetaClient.cc:147-149`）：

```cpp
if (config_.max_concurrent_requests() != concurrentReqSemaphore_.getUsableTokens()) {
  concurrentReqSemaphore_.changeUsableTokens(config_.max_concurrent_requests());
}
```
縮容時 `changeUsableTokens` 內的 `semaphore_.wait()` 是**阻塞式**的（不是協程 await），所以在請求滿載時調小這個值會阻塞設定回呼執行緒直到有請求完成。

`SemaphoreGuard::coWait()`（`src/common/utils/SemaphoreGuard.h:28-35`）先試 `try_wait()`，失敗才 `co_await`——省掉無競爭時的協程掛起。

**這個閘只護前台 `retry()`**。背景的 `tryClose` / `tryPrune`（§6.11）不經過它。

### 6.7 冪等 token

冪等 token 由**請求建構子**決定，而非 `MetaClient`。`ReqBase` 的第二個參數就是 uuid（`src/fbs/meta/Service.h:51-57`）：

```cpp
ReqBase(UserInfo user = {}, Uuid uuid = Uuid::zero()) : user(std::move(user)), uuid(uuid) { ... }
```

六個請求型別在建構子裡傳 `Uuid::random()`：`CreateReq`（:196）、`MkdirsReq`（:235）、`SymlinkReq`（:264）、`HardLinkReq`（:287）、`RemoveReq`（:322）、`RenameReq`（:471）。其餘全部留 `Uuid::zero()`。

`checkUuid()`（`Service.h:59-63`）要求 **client uuid 與 request uuid 都非零**：

```cpp
Result<Void> checkUuid() const {
  if (client.uuid == Uuid::zero()) return INVALID("Invalid client uuid");
  if (uuid == Uuid::zero()) return INVALID("Invalid request uuid");
  return VALID;
}
```
而 `client.uuid` 正是 `retry()` 第一行注入的（`MetaClient.cc:342`）。兩者缺一，伺服端就關閉冪等保護。

**伺服端如何使用**（邊界，只列證據）：`RemoveOp::needIdempotent`（`src/meta/store/ops/Remove.cc:52-62`）與 `RenameOp::needIdempotent`（`src/meta/store/ops/Rename.cc:63-70`）：

```cpp
bool needIdempotent(Uuid &clientId, Uuid &requestId) const override {
  if (!req_.checkUuid()) return false;
  if (req_.recursive || config().idempotent_remove()) { clientId = req_.client.uuid; requestId = req_.uuid; return true; }
  return false;
}
```
```cpp
bool needIdempotent(Uuid &clientId, Uuid &requestId) const override {
  if (!req_.checkUuid()) return false;
  if (!req_.moveToTrash && !config().idempotent_rename()) return false;
  clientId = req_.client.uuid; requestId = req_.uuid; return true;
}
```

**所以冪等是三重條件的合取**：請求型別帶 uuid × client 注入了身分 × （伺服端設定開啟 或 這是危險操作）。`recursive remove` 與 `moveToTrash rename` 是**無條件**啟用冪等的兩個操作——它們的 `valid()` 也強制要求 uuid（`Service.h:330`、`Service.h:479`）：

```cpp
if (recursive) RETURN_ON_ERROR(checkUuid());     // RemoveReq::valid
if (moveToTrash) RETURN_ON_ERROR(checkUuid());   // RenameReq::valid
```

create / symlink / hardLink 走的是另一條路——把 uuid 寫進 `DirEntry`，重試時比對就知道自己上次已經成功了（`src/meta/store/ops/BatchOperation.cc:489,533`、`Symlink.cc:33`、`HardLink.cc:41`）。

### 6.8 客戶端快取：沒有

**`MetaClient` 不快取任何元資料。** 它的全部成員（`MetaClient.h:279-325`）是：

```cpp
folly::Synchronized<PrunSessionBatch, std::mutex> batchPrune_;               // 待批次的 session
std::unique_ptr<BackgroundRunner> bgRunner_;
std::unique_ptr<CoroutinesPool<CloseTask>> bgCloser_;
folly::Synchronized<std::multimap<SteadyTime, CloseTask>, std::mutex> bgCloseTasks_;   // 待重試的 close
folly::Synchronized<std::set<flat::NodeId>> errNodes_;                       // 壞節點黑名單
ClientId clientId_; const Config &config_; const bool dynStripe_;
std::unique_ptr<StubFactory> factory_;
std::shared_ptr<ICommonMgmtdClient> mgmtd_;
std::shared_ptr<storage::client::StorageClient> storage_;
folly::atomic_shared_ptr<ServerSelectionStrategy> serverSelection_;
std::unique_ptr<ConfigCallbackGuard> onConfigUpdated_;
Semaphore concurrentReqSemaphore_;
```

沒有 inode 快取、沒有 dentry 快取、沒有屬性快取。每一次 `stat()` 都是一次真實 RPC。

**唯一的「快取」是 routing info**，而且不歸 `MetaClient` 管——它每次都現問 `mgmtd_->getRoutingInfo()`（一次原子讀）。一致性維持機制是 `MgmtdClient` 的 10 秒輪詢 + 版本單調性 + `waitRoutingInfo` 的成功路徑校驗（§6.5）。失效時機：`updateRoutingInfo` 每次成功刷新時整份替換（copy-on-write，舊的 `shared_ptr` 持有者繼續看到舊值直到放手）。

真正的元資料快取在**上一層**：FUSE 靠核心的 dentry/attr timeout（`init_entry(&e, attr_timeout(), entry_timeout())`，`src/fuse/FuseOps.cc:1891`）。所以「3FS 的元資料快取一致性」這個問題的答案是：**由 FUSE 的 timeout 語意決定，MetaClient 不參與**。

### 6.9 路徑解析：客戶端做多少

分工邊界非常清楚：**客戶端不解析路徑**。

所有請求攜帶的是 `PathAt{InodeId parent, std::optional<Path> path}`（`src/fbs/meta/Common.h:235-260`），伺服端負責從 `parent` 出發逐段解析 `path`、處理 symlink、檢查每一層權限。客戶端在這條路上只做兩件事：

**(a) 格式檢查。** `PathAt::validForCreate()` 拒絕空路徑、`.`、`..`、`/` 作為檔名（`Common.h:251-258`）。這在 `CHECK_REQUEST` 時執行，屬於**送出前的本地驗證**，不是解析。

**(b) 兩個操作會先做一次 `stat` 拿 inode id。** 這是客戶端唯一觸碰「路徑 → inode」映射的地方：

```cpp
// MetaClient.cc:771-778
CoTryTask<void> MetaClient::removeImpl(RemoveReq &req) {
  if (req.recursive) {
    auto res = co_await stat(req.user, req.path.parent, req.path.path, false);
    CO_RETURN_ON_ERROR(res);
    req.inodeId = res->id;
  }
  co_return (co_await retry(&IMetaServiceStub::remove, req)).then(RETURN_VOID);
}
```
```cpp
// MetaClient.cc:807-812
auto req = RenameReq(userInfo, PathAt(srcParent, src), PathAt(dstParent, dst), moveToTrash);
if (moveToTrash) {
  auto res = co_await stat(req.user, srcParent, src, false);
  CO_RETURN_ON_ERROR(res);
  req.inodeId = res->id;
}
```

兩者都只在「危險模式」（recursive / moveToTrash）下額外預取 inodeId 塞進請求。這與 §6.7 的冪等條件是同一組操作——把 inodeId 一起帶上去，伺服端重試時就能確認「我要刪的還是同一個 inode」，避免路徑在重試間隙被重用導致誤刪。

**`ReqBase::forward` 欄位客戶端從不設定。** 它固定是 `flat::NodeId(0)`（`Service.h:41`）。設定它的是 meta server 之間的轉發元件（`src/meta/components/Forward.h:91-95`）：

```cpp
if (req.forward) { XLOGF_IF(INFO, config_.debug(), "request is forward from {}, can't forward again, req {}.", ...); }
req.forward = nodeId_;
```
換句話說 client **看到的永遠是「某一台 meta server」，但實際處理者可能是被轉發到的另一台**——這對 `ServerSelectionStrategy` 的負載平衡有含義（§7.3）。

### 6.10 file session 的客戶端生命週期

| 階段 | 誰做 | 位置 |
|---|---|---|
| 產生 SessionId | **FUSE**，`SessionId::random()` | `src/fuse/FuseOps.cc:1451`（open）、`:1882`（create） |
| 註冊到 meta | `MetaClient::open/create` 帶 `SessionInfo{clientId_, sessionId}` | `MetaClient.cc:625,641` |
| 保持存活 | **`MgmtdClient::extendClientSession`**，10 秒一次 | `MgmtdClient.cc:306-318` |
| 正常釋放 | `MetaClient::close` 帶同一個 sessionId | `MetaClient.cc:655-669` |
| 釋放失敗 → 背景重試 | `enqueueCloseTask` → `bgCloser_` | `MetaClient.cc:664-667` |
| open/create 失敗 → 清理 | `pruneSession()` 批次 | `MetaClient.cc:589-591` |
| client 整個掛掉 | meta server 掃描 + `listClientSessions()` 比對 | `src/meta/components/SessionManager.cc:60` |

`OPTIONAL_SESSION` 巨集（`MetaClient.cc:54-55`）把 `optional<SessionId>` 轉成 `optional<SessionInfo>` 並注入 `clientId_`：

```cpp
#define OPTIONAL_SESSION(sessionId) \
  ((sessionId).has_value() ? std::optional(meta::SessionInfo(clientId_, *sessionId)) : std::nullopt)
```

**唯讀開啟不建 session**（`MetaClient.cc:638`）：

```cpp
if ((flags & O_ACCMODE) == O_RDONLY) sessionId = std::nullopt;
```
即使呼叫端傳了 sessionId 也會被丟掉。FUSE 側也做了對稱的判斷（只在 `O_WRONLY`/`O_RDWR` 時才產生 session，`FuseOps.cc:1449-1451`），兩層各做一次。session 的用途是保護「有人正在寫」，唯讀者不需要。

**client 崩潰時的回收機制**（邊界證據）：meta server 的 `SessionManager::ScanTask` 定期呼叫 `getActiveClients()`（`src/meta/components/SessionManager.cc:43-73`），把 mgmtd 回報的 client session 清單轉成 `set<ClientId>`，凡是 file session 的 owner 不在這個集合裡就清掉：

```cpp
if (timeout.has_value() && session.lastExtend + *timeout + 10_s < UtcClock::now()) {
  XLOGF(WARN, "Client {} timeout, last extended {}, ", session.description, session.lastExtend);
  continue;    // 不放進 active 集合
}
```
額外的 `+ 10_s` 是給時鐘偏差與網路抖動的餘裕。

### 6.11 背景 closer：兩種任務、兩種批次策略

```cpp
// MetaClient.h:233-241
struct CloseTask {
  std::variant<CloseReq, PruneSessionReq> req;
  Duration backoff;
};
```

啟動（`MetaClient.cc:153-165`）：

```cpp
bgCloser_ = std::make_unique<CoroutinesPool<CloseTask>>(config_.background_closer().coroutine_pool());
bgCloser_->start(folly::partial(&MetaClient::runCloseTask, this), exec);
bgRunner_ = std::make_unique<BackgroundRunner>(&exec.pickNext());
bgRunner_->start("ScanCloseTask", folly::partial(&MetaClient::scanCloseTask, this),
                 config_.background_closer().task_scan_getter());       // 50ms
bgRunner_->start("CheckServer", folly::partial(&MetaClient::checkServers, this),
                 config_.check_server_interval_getter());               // 5s
```

協程池預設 8 條協程、佇列 128（`MetaClient.h:76-79`）。

**待重試佇列是一個以到期時間為鍵的 multimap**（`MetaClient.h:282`），`scanCloseTask` 每 50ms 掃一次把到期的搬進協程池（`MetaClient.cc:200-217`）：

```cpp
while (true) {
  auto guard = bgCloseTasks_.lock();
  if (guard->empty()) break;
  auto iter = guard->begin();
  if (iter->first > now) break;           // multimap 有序，第一個沒到期就全都沒到期
  auto task = std::move(iter->second);
  guard->erase(iter);
  guard.unlock();
  co_await bgCloser_->enqueue(std::move(task));
}
```
注意 `guard.unlock()` 在 `enqueue` **之前**——`enqueue` 可能因佇列滿而掛起，若持鎖掛起會擋住所有 `enqueueCloseTask`。

**退避在 `runCloseTask` 裡加倍**（`MetaClient.cc:233-240`）：

```cpp
if (needRetry) {
  auto max = config_.background_closer().retry_max_wait();     // 10s
  auto next = Duration(task.backoff * 2);
  task.backoff = std::min(next, max);
  enqueueCloseTask(std::move(task));
}
```
從 `retry_first_wait`（100ms）翻倍到 10 秒封頂，**沒有總時限**——只要 `needRetry` 為真就無限重試。而 `needRetry` 的條件（`MetaClient.cc:690`）：

```cpp
needRetry = req.session.has_value() && ErrorHandling::retryable(result.error());
```
所以無 session 的 close 失敗就直接放棄（沒有資源需要回收）。

**`PrunSessionBatch` 是雙觸發批次**（`MetaClient.h:244-268`）：

```cpp
std::vector<SessionId> take(size_t batchSize, Duration batchDur) {
  if (sessions.size() < batchSize && (sessions.empty() || SteadyClock::now() - lastTime < batchDur)) return {};
  return std::exchange(sessions, {});
}
std::vector<SessionId> push(SessionId session, size_t batchSize) {
  if (sessions.empty()) { lastTime = SteadyClock::now(); sessions.reserve(batchSize); }
  sessions.push_back(session);
  if (sessions.size() >= batchSize) return std::exchange(sessions, {});
  return {};
}
```

- **數量觸發**：`push()` 攢到 `prune_session_batch_count`（128）立刻回傳整批（`MetaClient.cc:674`）。
- **時間觸發**：`scanCloseTask` 每 50ms 呼叫 `take()`，若已攢了東西且距首次 push 超過 `prune_session_batch_interval`（10 秒）就沖出去（`MetaClient.cc:193-198`）。

`lastTime` 記的是**這批的第一個元素進來的時刻**，不是最後一個——所以 10 秒是「這批最舊的那個等了多久」，延遲有上界。

`tryClose` / `tryPrune`（`MetaClient.cc:681-714`）**刻意不走 `retry()`**：

```cpp
auto server = co_await getServerNode();
CO_RETURN_ON_ERROR(server);
auto options = net::UserRequestOptions();
options.timeout = config_.retry_default().rpc_timeout();
auto result = (co_await ((server->stub)->close(req, options, &timestamp))).then(EXTRACT(stat));
```
單發一次、不重試、不佔用併發閘。重試由外層的 backoff 佇列負責。這避免了「前台重試機制」與「背景重試機制」疊乘出 60s × 無限次的爆炸。

### 6.12 `truncate`：客戶端直接指揮 storage

這是整個 `MetaClient` 最不像「元資料客戶端」的一段（`MetaClient.cc:860-931`）：

```cpp
CoTryTask<Inode> MetaClient::truncate(const UserInfo &userInfo, InodeId inodeId, uint64_t length) {
  auto inode = co_await stat(userInfo, inodeId, std::nullopt, true);
  CO_RETURN_ON_ERROR(inode);
  co_return co_await truncateImpl(userInfo, *inode, length);
}
```

`truncateImpl` 的五個步驟：

```cpp
CHECK_FILE(inode);                    // 必須是檔案，且 layout 必須 valid
GET_RAW_ROUTING_INFO();               // 拿 raw routing，拿不到回 kRoutingInfoNotReady
FileOperation fop(*storage_, *rawRoutingInfo, userInfo, inode, recorder);

// ① 迴圈刪 chunk
while (more) {
  if (reqInfo && reqInfo->canceled()) co_return makeError(MetaCode::kRequestCanceled, ...);
  auto remove = co_await fop.removeChunks(targetLength, config_.remove_chunks_batch_size(),
                                          dynStripe_ && config_.dynamic_stripe(), {});
  CO_RETURN_ON_ERROR(remove);
  removed += remove->first;
  more = remove->second;
  if (remove->first == 0 && more) {    // ② 活鎖偵測
    XLOG(CRITICAL, msg);
    co_return makeError(MetaCode::kFoundBug, msg);
  }
}
// ③ 需要的話先擴 stripe
auto stripe = std::min((uint32_t)folly::divCeil(targetLength, (uint64_t)inode.asFile().layout.chunkSize),
                       inode.asFile().layout.stripeSize);
if (inode.asFile().dynStripe && inode.asFile().dynStripe < stripe)
  CO_RETURN_ON_ERROR(co_await extendStripe(userInfo, inode.id, stripe));
// ④ 截斷最後一個 chunk
CO_RETURN_ON_ERROR(co_await fop.truncateChunk(targetLength));
// ⑤ 通知 meta 更新長度
auto req = SyncReq(userInfo, inode.id, true, std::nullopt, std::nullopt, true /* truncated */);
co_return (co_await retry(&IMetaServiceStub::sync, req)).then(EXTRACT(stat));
```

幾個要點：

- **meta server 完全不參與刪 chunk**。`FileOperation`（`src/fbs/meta/FileOperation.h:16-79`）持有 `StorageClient&` 與 `flat::RoutingInfo&`，直接對 storage 發 remove。同一個類別在 meta server 端也被使用（`OperationRecorder` 有 `meta_server` 與 `meta_client` 兩個實例，`Utils.h:170-178`），所以這段邏輯是 client / server 共用的。
- **中途可取消**：每一輪檢查 `RequestInfo::get()->canceled()`——FUSE 請求被中斷時能及時停下。這是 `MetaClient` 內唯一使用 `RequestInfo` 的地方（`retry()` 裡對應的程式碼是被註解掉的，`MetaClient.cc:345,391-395`）。
- **`remove_chunks_max_iters` 已停用**：設定項還在（`MetaClient.h:88` 標了 `// deprecated`），但檢查邏輯整段被註解掉（`MetaClient.cc:883-892`），改由「刪了 0 個卻說還有更多」這個矛盾條件當作 bug 偵測。
- 失敗中途返回時**檔案處於部分截斷狀態**——沒有回滾。呼叫端（`openCreate` 的 `O_TRUNC` 路徑）會在失敗時補一個 `close`（`MetaClient.cc:600-607`）：

```cpp
if (truncateResult.hasError()) {
  co_await close(req.user, result->stat.id,
                 req.session.has_value() ? std::optional(req.session->session) : std::nullopt,
                 false, true);
  CO_RETURN_ERROR(truncateResult);
}
```

### 6.13 `openCreate`、layout 與 dynamic stripe

`open` 與 `create` 共用 `openCreate()`（`MetaClient.cc:578-611`）：

```cpp
req.dynStripe &= config_.dynamic_stripe();
bool needPrune = false;
auto onError = [&](const Status &error) { if (ErrorHandling::needPruneSession(error)) needPrune = true; };
auto retryConfig = config_.retry_default().clone();
auto result = co_await retry(func, req, retryConfig, onError);
if (result.hasError() && needPrune && req.session.has_value()) co_await pruneSession(req.session->session);
```

`onError` 是 `retry()` 的第四個參數，**每一次失敗都會被呼叫**（`MetaClient.cc:416-418`）。`needPruneSession()`（`Utils.h:135-155`）的判斷是「這個錯誤下，session 有可能已經在伺服端建立了嗎」：

```cpp
case StatusCodeType::Transaction: return code == TransactionCode::kMaybeCommitted;
case StatusCodeType::Meta:        return code == MetaCode::kOTruncFailed;
case StatusCodeType::RPC:
  switch (code) {
    case kSendFailed: case kConnectFailed: case kIBInitFailed: case kInvalidMethodID: return false;
    default: return true;    // ★ 超時等「送出去了但不知道結果」的情況
  }
```
四個「確定沒送到」的 RPC 錯誤回 false，其餘（主要是逾時）回 true。`pruneSession` 的註解說明了目的（`MetaClient.cc:672`）：`// file session may create on FoundationDB, prune session to avoid file session leak.`

`req.dynStripe &= config_.dynamic_stripe()` 與建構子的 `dynStripe_ && config_.dynamic_stripe()`（`MetaClient.cc:629,643`）形成雙重閘門：**建構參數**（只有 FUSE 傳 true，`FuseClients.cc:136`；`admin_cli` 傳 false，`admin_cli.cc:191`）**且** 設定開關。`MetaClient.h:317` 的註解直說：`// only fuse client support dynamic stripe.`

`updateLayout()`（`MetaClient.cc:471-497`）在 `create` / `mkdirs` 帶 layout 時把 chain table 的**當前版本**填進去：

```cpp
if (layout.empty() || !std::holds_alternative<Layout::ChainList>(layout.chains) || layout.tableId == 0 ||
    layout.tableVersion != 0) {
  XLOGF(INFO, "Don't need update layout {}", layout);
  co_return Void{};
}
auto table = routing->raw()->getChainTable(layout.tableId);
if (!table) co_return makeError(MetaCode::kInvalidFileLayout);
layout.tableVersion = table->chainTableVersion;
```
四個 early-return 條件的合取才會補版本：**必須是非空的 ChainList、有 tableId、且 tableVersion 尚未指定**。這讓呼叫端可以只寫「我要 table 3」，由 client 補上「當時的 table 3 是第幾版」。

### 6.14 兩處向後相容退化

**(a) `batchStat` / `batchStatByPath` 遇到舊 server**（`MetaClient.cc:522-536`）：

```cpp
if (rsp.error().code() != RPCCode::kInvalidMethodID) co_return rsp;
XLOGF(ERR, "batchStat get kInvalidMethodID, need update Meta Server.");
std::vector<std::optional<Inode>> inodes;
for (auto &inodeId : inodeIds) {
  auto rsp = co_await stat(userInfo, inodeId, std::nullopt, false);
  if (rsp.hasError() && rsp.error().code() == MetaCode::kNotFound) { inodes.push_back(std::nullopt); continue; }
  CO_RETURN_ON_ERROR(rsp);
  inodes.push_back(std::move(*rsp));
}
```
`kInvalidMethodID` 是 `ErrorHandling::retryable` 唯一排除的 RPC 錯誤（`Utils.h:120`）——正是為了讓它能一路傳到這裡觸發退化。退化後是 N 次串行 `stat`，效能崩塌但功能正確。`batchStat` 的語意（找不到 → `nullopt`）與 `batchStatByPath`（找不到 → `Result` 裡的錯誤）在退化路徑上被分別重建。

**(b) `rename` 回應沒有 `stat`**（`MetaClient.cc:813-821`）：

```cpp
auto result = co_await retry(&IMetaServiceStub::rename, req);
CO_RETURN_ON_ERROR(result);
if (result->stat.has_value()) co_return std::move(*result->stat);
// NOTE: for compatibility, this maybe not atomic
XLOGF(WARN, "rename doesn't return inode, server maybe not updated, stat dst after rename!");
co_return co_await stat(userInfo, dstParent, dst, false /* don't follow symlink */);
```
`RenameRsp::stat` 是 `optional<Inode>`（`Service.h:485`），舊 server 不填。註解明確承認退化路徑**不是原子的**——rename 與後續 stat 之間目標可能又被改動。

還有一處防禦性檢查（`MetaClient.cc:519,547`）：

```cpp
XLOGF_IF(DFATAL, rsp->size() != inodeIds.size(), "{} != {}", rsp->size(), inodeIds.size());
```
batch 回應的元素數必須與請求一致，debug build 下不一致直接 abort。

---

## 7. `ServerSelectionStrategy`：負載平衡與容錯的核心

這兩個檔案（`ServerSelectionStrategy.h` 146 行 + `.cc` 228 行）決定了「一個 client 的請求打到哪一台 meta server」。

### 7.1 `ServerList`：從 routing info 蒸餾出候選清單

```cpp
// ServerSelectionStrategy.h:52-59
struct ServerList {
  std::shared_ptr<RoutingInfo> routing;
  std::vector<flat::NodeId> nodeList;
  std::map<flat::NodeId, net::Address> nodeAddress;
  bool update(std::shared_ptr<RoutingInfo> newRouting, net::Address::Type addrType);
  std::optional<ServerSelectionStrategy::NodeInfo> get(flat::NodeId nodeId) const;
};
```

`update()`（`ServerSelectionStrategy.cc:37-79`）有四道關卡：

```cpp
if (newRouting == routing) return false;          // ① 指標相等 → 同一份，直接跳過
routing = newRouting;
auto nodes = routing->getNodeBy(flat::selectNodeByType(flat::NodeType::META) && flat::selectActiveNode());
if (nodes.empty() && !nodeList.empty()) {         // ② 新拓樸沒有活的 meta，但我手上有 → 保留舊的
  XLOGF(WARN, "meta::ServerList ignore routing info version {}, because no active meta server", ...);
  return false;
}
```

第 ① 道用的是 `shared_ptr` 指標比較，能生效正是因為 `MgmtdClient::updateRoutingInfo` 每次都建新物件、且**只在真有新資料時才通知 listener**（§4.5）。

第 ② 道是一條刻意的**降級保護**：如果新的 routing info 裡一台活著的 meta server 都沒有，寧可繼續用（可能已經過期的）舊清單也不要清空。但注意它的條件是 `!nodeList.empty()`——**第一次啟動時若沒有任何活的 meta server，`nodeList` 保持空**，後續 `select()` 會回 `kMetaServiceNotAvailable`。

```cpp
for (const auto &node : nodes) {
  auto addrs = node.extractAddresses("MetaSerde", addrType);     // ③ 服務名 + 網路型別過濾
  if (addrs.empty()) {
    XLOGF(WARN, "meta::ServerList get node {} {}@{} without address of type {}", ...);
    continue;
  }
  nodeList.push_back(node.app.nodeId);
  nodeAddress[node.app.nodeId] = addrs[0];        // ④ 一台節點只取第一個位址
}
```

第 ③ 道用的服務名是硬編碼字串 `"MetaSerde"`——來自 `SERDE_SERVICE(MetaSerde, 4)`（`src/fbs/meta/Service.h:709`），server 端以 `MetaSerdeService` 註冊（`src/meta/service/MetaServer.cc:64`）。第 ④ 道 `addrs[0]`：**一台 meta server 即使有多個同型別位址，client 也只用第一個**，沒有位址級別的容錯（對比 `MgmtdClient::Conn::switchAddr()`）。

最後的變更偵測（`ServerSelectionStrategy.cc:67-77`）：

```cpp
if (oldNodes != nodeList || oldAddress != nodeAddress) {
  XLOGF(INFO, "meta::ServerList update routing info {}, find {} meta servers, {}", ...);
  return true;
}
```
**回傳 true 才會觸發 `onUpdate()`**——routing info 版本變了但 meta server 集合沒變（最常見的情況：某個 storage target 狀態改變）不會擾動選擇策略。這對 `RandomFollow` 特別重要（§7.3）。

### 7.2 三種策略

`ServerSelectionMode` 是三選一的 enum（`ServerSelectionStrategy.h:27-31`），由 `create()` 工廠分派（`.cc:91-104`），非法值 `throw std::invalid_argument`。

| 模式 | `selectFrom` 實作 | 狀態 | 特性 |
|---|---|---|---|
| `RoundRobin` | `next_.fetch_add(1) % nodes.size()`（`.cc:181-186`） | `std::atomic<size_t> next_` | 無鎖；但 `nodes` 大小會隨 skip 集合變動，取模基準跟著變 |
| `UniformRandom` | `dist_(gen_, index_range(0, nodes.size()-1))`（`.cc:188-194`） | `std::mt19937_64` + `std::mutex` | 需要互斥鎖（mt19937 非執行緒安全） |
| `RandomFollow` | 見 §7.3 | `Uuid token_` + `std::vector<flat::NodeId> prefer_` | **預設值**（`MetaClient.h:83`） |

`RoundRobin` 有一個微妙處：它對「已扣掉 skip 節點的清單」取模。假設 3 台機、第 2 台進了黑名單，`nodes` 變成 2 個元素，`next_` 繼續遞增但取模基數從 3 變 2——輪替仍是均勻的，只是與黑名單前的序列不連續。這不影響正確性。

### 7.3 `RandomFollow`：每個 client 固定黏一台

這是預設策略，也是三者中唯一有「親和性」的。

```cpp
// ServerSelectionStrategy.h:117-133
struct RandomFollowServerSelection : public BaseSelectionStrategy {
  RandomFollowServerSelection(...) : BaseSelectionStrategy(mgmtd, addrType), token_(Uuid::random()) { registerListener(); }
  ...
 private:
  Uuid token_;
  std::vector<flat::NodeId> prefer_;
};
```

`token_` 是**這個 client 進程啟動時隨機生成一次**的 UUID，此後不變。

```cpp
// ServerSelectionStrategy.cc:209-226
void RandomFollowServerSelection::onUpdate(ServerList &servers) {
  prefer_ = servers.nodeList;
  std::sort(prefer_.begin(), prefer_.end(), [&](const auto &node1, const auto &node2) {
    return Weight::calculate(node1, token_) > Weight::calculate(node2, token_);
  });
  XLOGF(INFO, "RandomFollowServerSelection meta server order {}", fmt::join(prefer_.begin(), prefer_.end(), ","));
  assert(prefer_.size() == servers.nodeList.size());
  if (!prefer_.empty()) {
    auto prefer = *prefer_.begin();
    preferMetaServer.set(prefer.toUnderType());
    XLOGF(INFO, "RandomFollowServerSelection choose [{}, {}, {}] as preferred", ...);
  }
}
```

**這是 rendezvous hashing（HRW，Highest Random Weight）**。`Weight::calculate`（`src/fbs/meta/Utils.h:262-266`）：

```cpp
static Weight calculate(flat::NodeId node, Uuid clientId) {
  // todo: maybe should change to client host name?
  auto key = Serializer::serRawArgs((uint64_t)node.toUnderType(), clientId.data);
  return hash(key.data(), key.size());
}
static Weight hash(void *key, size_t len) {
  // NOTE: don't change this
  Weight w;
  MurmurHash3_x64_128(key, len, 0, &w);
  return w;
}
```
`Weight` 是 `std::array<uint8_t,16>`（`Utils.h:251`），比較用的是 array 的字典序 `operator>`。`// NOTE: don't change this` 這行註解說明雜湊函數被視為協定的一部分。

性質：
- **每個 client 得到一個獨立的全序**（因為 token 不同），所以 N 個 client 會均勻散布在 M 台 meta server 上——不會像「大家都選第一台」那樣傾斜。
- **拓樸變動時擾動最小**：加入/移除一台 server 只會影響排序中它自己的位置，其他 server 的相對順序不變。這正是 HRW 相對於「取模」的優勢。
- **同一個 client 的請求全部打同一台**（只要它健康），因此 meta server 端的 per-client 快取（如 user cache）命中率高。

`selectFrom` 只是照序找第一台不在 skip 集合裡的（`.cc:196-207`）：

```cpp
assert(!nodes.empty() && !prefer_.empty());
for (auto node : prefer_) {
  if (!skipHint.contains(node)) return node;
}
auto index = folly::Random::rand32(nodes.size());
return nodes[index];
```

兩個邊界值得指出：

1. **`prefer_` 沒有扣掉 skip 節點以外的東西**——它是全量排序。所以最後的 fallback（全部 prefer 都被 skip）幾乎不可能走到，因為 `select()` 在呼叫 `selectFrom` 前已保證 `nodes` 非空（§7.4）。
2. **`prefer_` 可能是空的**：`onUpdate` 只在 `ServerList::update` 回 true 時被呼叫（`.cc:141-144`）。若 client 啟動時拿到的 routing info 一台活的 meta server 都沒有，`update` 回 false（§7.1 的第 ④ 個 return），`prefer_` 保持空。此時 `assert(!prefer_.empty())` 在 debug build 會觸發；release build 下 for 迴圈空轉、落到隨機選擇——但那條路徑同樣要求 `nodes` 非空，而 `nodes` 來自 `nodeList`，也是空的，所以 `select()` 早已在更前面回了 `kMetaServiceNotAvailable`。**兩者是同一個條件，所以 release 下不會真的踩空。**

### 7.4 `select()`：兩段式與 skip 集合

```cpp
// ServerSelectionStrategy.cc:150-177
Result<ServerSelectionStrategy::NodeInfo> BaseSelectionStrategy::select(const std::set<flat::NodeId> &skipNodes) {
  auto rlock = servers_.rlock();
  if (rlock->nodeList.empty()) {
    XLOGF(ERR, "meta::ServerSelection doesn't find available node.");
    return makeError(MgmtdClientCode::kMetaServiceNotAvailable);
  }
  if (!skipNodes.empty()) {
    auto nodes = skip(rlock->nodeList, skipNodes);
    if (!nodes.empty()) {
      auto nodeId = selectFrom(nodes, skipNodes);
      ...
      return NodeInfo{nodeId, addr, hostname};
    }
    noMetaAfterSkip.addSample(1);
    XLOGF(WARN, "meta::SeverSelection have no available nodes after skip nodes {}.", ...);
  }
  auto nodeId = selectFrom(rlock->nodeList, skipNodes);   // ★ 全部都壞了 → 從全量裡挑
  ...
}
```

**關鍵設計：黑名單是「建議」不是「禁令」。** 當所有節點都在 `errNodes_` 裡時，`select()` 不會失敗，而是記一個 `meta_client.no_meta_after_skip` 指標並**從全量清單裡照樣挑一台**。這避免了「一次網路抖動把所有節點都拉黑 → client 徹底失聯 → 5 秒後 checkServers 才能救回來」的死局。

注意第二次 `selectFrom` 仍把 `skipNodes` 當作 `skipHint` 傳進去——但三個實作中只有 `RandomFollow` 會讀這個參數，另外兩個的簽名裡參數名是空的（`.cc:182,189`），直接忽略。

`skip()` 是個 O(n·log m) 的自由函式（`.cc:106-116`），每次 `select` 都重建一個 vector。以 meta server 通常個位數的規模，這個成本可以忽略。

### 7.5 listener 註冊、生命週期與熱切換

```cpp
// ServerSelectionStrategy.cc:118-133
BaseSelectionStrategy::BaseSelectionStrategy(std::shared_ptr<ICommonMgmtdClient> mgmtd, net::Address::Type addrType)
    : name_(fmt::format("meta-server-selection-{}", Uuid::random().toHexString())), addrType_(addrType), mgmtd_(mgmtd) {}

BaseSelectionStrategy::~BaseSelectionStrategy() { mgmtd_->removeRoutingInfoListener(name_); }

void BaseSelectionStrategy::registerListener() {
  mgmtd_->addRoutingInfoListener(name_, [this](auto routing) { update(routing); });
  auto routing = mgmtd_->getRoutingInfo();
  if (!routing) XLOGF(ERR, "meta::ServerSelection failed to get routing info from mgmtd!");
  else update(routing);
}
```

`name_` 帶隨機 UUID，因為 `MgmtdClient::addRoutingInfoListener` 用 `try_emplace`（`MgmtdClient.cc:825`），同名會註冊失敗。**多個策略實例可以並存**——這正是熱切換時會發生的事。

`registerListener()` 帶了註解 `// NOTE: must call this in subclass`（`.h:74-75`），因為它會呼叫虛函式 `onUpdate()`，在基底建構子裡呼叫會分派到基底版本。三個子類的建構子都在最後一行呼叫它（`.h:87,103,121`）。

**熱切換**（`MetaClient.cc:138-146`）：

```cpp
onConfigUpdated_ = config_.addCallbackGuard([this]() {
  auto strategy = serverSelection_.load();
  if (strategy->mode() != config_.selection_mode() || strategy->addrType() != config_.network_type()) {
    XLOGF(INFO, "MetaClient turn to server selection mode {}, network {}", ...);
    serverSelection_ = ServerSelectionStrategy::create(config_.selection_mode(), mgmtd_, config_.network_type());
  }
  ...
});
```

`serverSelection_` 是 `folly::atomic_shared_ptr`——舊策略被新策略原子替換，正在使用舊策略的協程（`serverSelection_.load()` 已經拿到 `shared_ptr`）繼續安全使用，最後一個放手時舊策略解構、自動反註冊 listener。**切換期間兩個策略同時掛在 listener 上**，各自更新自己的 `ServerList`，不互相干擾。

`select` 之外的第二個公開方法 `get(nodeId)`（`.h:67`）只做位址查詢，給 `checkServers()` 用來對黑名單節點發探活 RPC。`RandomFollowServerSelection` 覆寫了 `get()`（`.h:127`）但實作與基底完全相同——是一處冗餘。

### 7.6 監控指標

```cpp
// ServerSelectionStrategy.cc:32-35
monitor::ValueRecorder preferMetaServer("meta_client.prefer_server", std::nullopt, false);
monitor::CountRecorder noMetaAfterSkip("meta_client.no_meta_after_skip");
```

`preferMetaServer` 記的是**當前偏好節點的 nodeId 數值**——這讓運維可以直接從監控面板看出「這個 client 現在黏在哪一台」。`BaseSelectionStrategy::onUpdate` 的預設實作把它設為 0（`.cc:179`），只有 `RandomFollow` 會設真值。所以 RoundRobin / UniformRandom 模式下這個指標恆為 0。

---

## 8. 端到端時序：FUSE 寫一個檔案

```
FUSE 掛載                                                MgmtdClient actor         mgmtd primary
  │                                                            │                        │
  ├─ MgmtdClientForClient(clusterId, stubFactory, cfg)          │                        │
  ├─ setClientSessionPayload({uuid, FUSE, data, {}})            │                        │
  ├─ setConfigListener(ApplicationBase::updateConfig)           │                        │
  ├─ start(bgThreadPool)  ──────────────────────────────────▶ actor() 起                 │
  │                                                     AutoRefresh(10s) 起              │
  │                                                     AutoExtendClientSession(10s) 起  │
  ├─ refreshRoutingInfo(force=false) ───────────────────▶ RefreshWorkItem                │
  │                                                     └ withRetry → connect()          │
  │                                                       └ probePrimary DFS ──────────▶ getPrimaryMgmtd
  │                                                       └ getRoutingInfo(ver=0) ─────▶ 全量拓樸
  │                                                     updateRoutingInfo → 廣播 listener │
  ├─ establishClientSession()（≤40 次退避重試） ──────────▶ ExtendClientSessionOp ───────▶ extendClientSession
  ├─ StorageClient::create(clientId, cfg, *mgmtdClient)         │                        │
  ├─ MetaClient(clientId, cfg, stubFactory, mgmtdClient, storageClient, dynStripe=true)
  │    └ ServerSelectionStrategy::create(RandomFollow, mgmtd, RDMA)
  │        └ registerListener() → addRoutingInfoListener("meta-server-selection-<uuid>")
  │        └ update(routing) → ServerList{META ∧ active} → onUpdate() → HRW 排序 prefer_
  ├─ metaClient->start(bgThreadPool)
  │    └ CoroutinesPool<CloseTask> 8×128 + ScanCloseTask(50ms) + CheckServer(5s)
  │
  ▼ 使用者 open("/x", O_WRONLY|O_CREAT)                        meta server（HRW 選出的那台）
  │                                                                     │
  ├─ session = SessionId::random()                                      │
  ├─ metaClient->create(user, parent, "x", session, perm, flags)        │
  │    └ CreateReq{uuid=random(), session={clientId,session}}           │
  │    └ retry(&IMetaServiceStub::create, req)                          │
  │        ├ req.client = clientId_                                     │
  │        ├ CHECK_REQUEST → CreateReq::valid()（非唯讀必須帶 session）   │
  │        ├ getServerNode() → select(errNodes_) → prefer_[0]           │
  │        ├ Semaphore(128).coWait()                                    │
  │        ├ stub->create(req, {timeout=5s, ...}) ────────────────────▶ 建 inode + file session
  │        ├ ErrorHandling::success? → errNodes_.erase(node)            │
  │        └ waitRoutingInfo(rsp)：rsp 裡的 Inode.layout 版本本地有嗎？    │
  │             有 → 回傳；沒有 → 每秒重查，最多 30s                        │
  ▼ 寫入（走 StorageClient，不經 MetaClient）                             │
  ▼ 使用者 close()                                                       │
  ├─ metaClient->close(user, ino, session, read=false, written=true)    │
  │    └ CloseReq{updateLength=true, atime=nullopt, mtime=now}          │
  │    └ retry(&IMetaServiceStub::close) ─────────────────────────────▶ 更新長度 + 移除 file session
  │       失敗且 retryable 且有 session → enqueueCloseTask(backoff=100ms)
  │                                       └ 50ms 掃描 → CoroutinesPool → tryClose（單發不重試）
  │                                          再失敗 → backoff ×2（上限 10s）→ 無限重試
  │
  ▼ 進程被 kill（沒有 close）
     AutoExtendClientSession 停止 → mgmtd 的 ClientSession 過期
     → meta server 的 SessionManager::ScanTask 呼叫 listClientSessions()
     → 發現 file session 的 owner 不在 active 集合 → 清理
```

---

## 9. 設定項全表

### `MgmtdClient::Config`（`MgmtdClient.h:14-26`）

| 設定項 | 預設值 | 熱更新 | 說明 |
|---|---|---|---|
| `mgmtd_server_addresses` | `{}` | ✘ | 空的話 `initMgmtds()` 回 `kInvalidConfig` |
| `work_queue_size` | 100 | ✘ | actor 佇列容量，滿了回 `kWorkQueueFull` |
| `network_type` | `nullopt` | ✘ | 設了就強制檢查所有 mgmtd 位址型別 |
| `enable_auto_refresh` | true | ✔ | 三個角色 Config 各自覆寫 |
| `auto_refresh_interval` | `10_s` | ✔ | |
| `enable_auto_heartbeat` | true | ✔ | 基底預設 true，但只有 ForServer 保留 |
| `auto_heartbeat_interval` | `10_s` | ✔ | |
| `enable_auto_extend_client_session` | true | ✔ | 只有 ForClient 保留 |
| `auto_extend_client_session_interval` | `10_s` | ✔ | |
| `accept_incomplete_routing_info_during_mgmtd_bootstrapping` | true | ✔ | false 才會丟棄不完整拓樸 |

三個 auto 任務的 enable 開關**同時被建構子預設值與熱更新讀取**：`startBackgroundTasksWithLock()` 用它決定要不要註冊任務（`MgmtdClient.cc:243-260`），任務本身每輪又檢查一次（`:281-284, 294-297, 307-310`）。所以熱更新關掉開關會讓任務**空轉**（每 10 秒印一行 `is disabled, skip`）而不是真的停掉；熱更新打開則**不會**讓已跳過註冊的任務長出來。

### `MetaClient::Config`（`MetaClient.h:82-94`）

| 設定項 | 預設值 | 說明 |
|---|---|---|
| `selection_mode` | `RandomFollow` | 熱更新會重建策略物件 |
| `network_type` | `RDMA` | 同上 |
| `check_server_interval` | `5_s` | 黑名單探活週期 |
| `max_concurrent_requests` | 128 | 熱更新走 `changeUsableTokens` |
| `remove_chunks_batch_size` | 32 | truncate 每輪刪幾個 chunk |
| `remove_chunks_max_iters` | 1024 | **已停用**（`// deprecated`，檢查邏輯被註解） |
| `dynamic_stripe` | false | 與建構參數 `dynStripe` 取 AND |

### `MetaClient::RetryConfig`（`MetaClient.h:60-68`，全部熱更新）

| 設定項 | 預設值 | 說明 |
|---|---|---|
| `rpc_timeout` | `5_s` | 單次 RPC 逾時 |
| `retry_send` | 1 | 傳給 `UserRequestOptions::sendRetryTimes`，**只在 `checkServers` 用到**（`MetaClient.cc:268`）；`retry()` 沒有設這個欄位 |
| `retry_fast` | `1_s` | 與 `retry_init_wait` 取 min 作為快速重試等待 |
| `retry_init_wait` | `500_ms` | 指數退避起點 |
| `retry_max_wait` | `5_s` | 退避上限 |
| `retry_total_time` | `1_min` | 總時限；`waitRoutingInfo` 用它的一半 |
| `max_failures_before_failover` | 1 | 預設「一次 server error 就換機」 |

### `MetaClient::CloserConfig`（`MetaClient.h:70-80`，全部熱更新）

| 設定項 | 預設值 |
|---|---|
| `task_scan` | `50_ms` |
| `prune_session_batch_interval` | `10_s` |
| `prune_session_batch_count` | 128 |
| `retry_first_wait` | `100_ms` |
| `retry_max_wait` | `10_s` |
| `coroutine_pool.coroutines_num` | 8 |
| `coroutine_pool.queue_size` | 128 |

---

## 10. 監控指標

| 指標名 | 型別 | 產生位置 | 語意 |
|---|---|---|---|
| `meta_client.server_error` | Count（tag=nodeId） | `MetaClient.cc:438` | 觸發 failover 計數的伺服器錯誤 |
| `meta_client.req_rejected` | Count（tag=nodeId） | `MetaClient.cc:425` | `kRequestRefused`（伺服端過載保護） |
| `meta_client.get_server` | Latency | `MetaClient.cc:250` | 選機耗時 |
| `meta_client.truncate_too_many_chunks` | Count | `MetaClient.cc:98` | 宣告了但**無任何 `addSample` 呼叫點**（隨 `remove_chunks_max_iters` 一起停用） |
| `meta_client.truncate_iters` | Distribution | `MetaClient.cc:873` | truncate 迴圈輪數 |
| `meta_client.inflight_time` / `server_latency` / `network_latency` | Latency（tag=op） | `MetaClient.cc:78-80` | 由 `serde::Timestamp` 拆解；RPC 類錯誤時不記 |
| `meta_client.op_{total,failed,running,code,latency,retry,idempotent,duplicate}` | 見 `Utils.h:241-248` | `OperationRecorder::client()` | 每個操作的完整統計，tag 帶 op 與 uid |
| `meta_client.prefer_server` | Value | `ServerSelectionStrategy.cc:33` | 當前偏好節點 id（僅 RandomFollow） |
| `meta_client.no_meta_after_skip` | Count | `ServerSelectionStrategy.cc:34` | 黑名單吃掉全部節點的次數 |
| `meta_client.remove_chunks*` / `truncate*` / `query*` | 見 `FileOperation.h:18-39` | `FileOperation::Recorder("meta_client")` | truncate 對 storage 的操作統計 |
| `MgmtdClient.<Method>.op` | ServiceOperationWithMetric | `MgmtdClient.cc:775` 等 | 24 個 RPC + Heartbeat/Refresh/ExtendClientSession |

`OperationRecorder::Guard` 的 `retry_` 初值是 **1 而非 0**（`Utils.h:233`），所以 `meta_client.op_retry` 的分布圖上「一次成功」對應的值是 1。

---

## 11. 邊角與風險清單

以下每一條都直接對應原始碼，不含推測性的架構評論。

1. **`kNotPrimary` / `kHeartbeatVersionStale` / `kClientSessionVersionStale` 三處都靠 `scn::scan` 從錯誤訊息字串裡解析數字**（`MgmtdClient.cc:145, 718, 353`）。這是跨進程的隱性協定，沒有型別或版本保護。解析失敗都有 fallback（盲試 / 版本 +1），所以不會壞，但會退化。

2. **`getMgmtdConn` 用 `operator[]`**（`MgmtdClient.cc:552`），查不到會插入一個 nodeId=0 的空 `Conn` 並 DFATAL。呼叫點都先檢查過 `contains()`，但這道防線靠人工維持。

3. **`probeChainLength > 3` 是硬編碼**，原始碼自陳 `// TODO: avoid hard code`（`MgmtdClient.cc:483`）。mgmtd 節點數超過 4 且探測鏈很長時會提前放棄。

4. **`ServerList` 一台節點只取 `addrs[0]`**（`ServerSelectionStrategy.cc:65`）。meta server 的多網卡容錯不存在——位址不通只能靠 failover 換整台。

5. **`waitRoutingInfo` 不主動觸發 refresh**（`MetaClient.cc:323-336`），只被動等 `AutoRefresh`。若 client 的 `enable_auto_refresh` 被關掉（例如 `admin_cli`），這個迴圈會空轉 30 秒後回 `kRoutingInfoNotReady`。

6. **`RandomFollowServerSelection::selectFrom` 的 `assert(!prefer_.empty())`** 在「啟動時沒有任何 active meta server」的情境下會在 debug build 觸發——不過該路徑在 `select()` 的前置檢查下實際不可達（§7.3）。

7. **`MetaClient::getServerNode` 的 `!mgmtd_` 分支**（`MetaClient.cc:251-253`）回傳一個 mock 節點，但建構子有 `assert(mgmtd_)`（`:135`）。這個分支在 debug build 不可達，release build 下若真傳了 null 會拿到一個位址為空的 stub。

8. **`logUnavailableChains` 的 bootstrapping 分支只印 WARN 不 return**（`RoutingInfo.cc:43-45`），與日誌措辭 "Skip log unavailable chains" 描述的行為不一致。

9. **設定 listener 未設定時仍推進 `configVersion`**（`MgmtdClient.cc:734-739`、`:366-371`），只記一行 WARN。意即「忘記設 listener」＝「靜默丟棄所有熱更新設定」。

10. **背景 close 重試無總時限**（`MetaClient.cc:233-240`）。只要錯誤持續 retryable，任務會以 10 秒間隔永遠重試下去；`bgCloseTasks_` 也沒有容量上限。

11. **`ServiceInfo::id` 永遠是 0**——`RoutingInfo::getServiceBy` 建構它時不賦值（`RoutingInfo.h:44-48`）。

12. **`meta_client.truncate_too_many_chunks` 是死指標**（宣告於 `MetaClient.cc:98`，無呼叫點）。

13. **`retry_send` 設定項在前台 `retry()` 路徑上未使用**——只有 `checkServers` 的探活 RPC 設了 `options.sendRetryTimes`（`MetaClient.cc:268`）。

14. **`MetaClient::retry` 的請求取消檢查整段被註解掉**（`MetaClient.cc:391-395`）。目前只有 `truncateImpl` 的迴圈會檢查 `RequestInfo::canceled()`。

15. **`MgmtdClient::startBackgroundTasks()` 在 repo 內無呼叫點**——實際的補啟動路徑是再呼叫一次 `start()`（§4.8）。

---

## 12. 檔案索引

### `src/client/meta/`（4 檔，1682 行）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `MetaClient.h` | 327 | `MetaClient` 類別宣告：三層 Config（`Config` / `RetryConfig` / `CloserConfig`）、28 個公開方法、`retry()` 模板宣告、`ServerNode` / `CloseTask` / `PrunSessionBatch` 三個內部型別、全部成員欄位 |
| `MetaClient.cc` | 981 | 全部實作。核心是 `retry()`（:339-469，統一重試骨架）、`truncateImpl()`（:860-925，直接指揮 storage 刪 chunk）、背景 closer 四函式（:180-247）、`checkServers()`（:261-299）、`waitRoutingInfo()`（:319-337）；其餘 20 餘個方法都是「組請求 → `retry` → 抽欄位」的三行式 |
| `ServerSelectionStrategy.h` | 146 | `ServerSelectionMode` enum、`ServerSelectionStrategy` 抽象基底與 `NodeInfo`、`ServerList`（候選清單）、`BaseSelectionStrategy` 與三個具體策略的類別宣告、`NodeInfo` 的 fmt formatter |
| `ServerSelectionStrategy.cc` | 228 | `ServerList::update()`（從 routing info 蒸餾 META 節點）、工廠 `create()`、`BaseSelectionStrategy` 的 listener 註冊與 `select()` 兩段式邏輯、三個 `selectFrom()` 實作、`RandomFollow` 的 HRW 排序 |

### `src/client/mgmtd/`（15 檔，1636 行）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `MgmtdClient.h` | 130 | `MgmtdClient` 門面類別宣告：`Config`（10 個設定項）、pimpl 的 `struct Impl;` 前置宣告、約 30 個方法宣告（涵蓋四個介面的聯集） |
| `MgmtdClient.cc` | 998 | 全部實作。`struct Impl`（:116-771）包含：actor 模型（`WorkItem` variant + `actor()` + `tryEnqueue`）、`Conn` 與位址映射、primary 探測 DFS（三個 `probePrimary` 多載 + `trySwitchProbeTarget`）、`withRetry`、routing info 刷新與廣播、`heartbeatImpl`、`ExtendClientSessionOp`、三個 auto 背景任務；末尾是 X-macro 展開的 24 個 `XxxOp` 與門面轉接 |
| `ICommonMgmtdClient.h` | 40 | 最小共同介面：生命週期三方法（帶空預設實作）+ routing info 四方法 + 四個唯讀查詢。`MetaClient` 與 meta server 的五個元件都只依賴這個 |
| `IMgmtdClientForServer.h` | 24 | server 專屬：`heartbeat` / `triggerHeartbeat` / `setAppInfoForHeartbeat` / `setConfigListener`（→ server 通道）/ `updateHeartbeatPayload`；定義 `HeartbeatPayload = flat::HeartbeatInfo::Payload` |
| `IMgmtdClientForClient.h` | 23 | client 專屬：`extendClientSession` / `setConfigListener`（→ client 通道）/ `setClientSessionPayload`；定義 `ClientSessionPayload{clientId, nodeType, data, userInfo}` |
| `IMgmtdClientForAdmin.h` | 57 | 13 個叢集寫入操作（chain / chain table / config / node 註冊與啟停 / tag / target 順序 / orphan target） |
| `CommonMgmtdClient.h` | 50 | `template <typename Base> class CommonMgmtdClient : public Base`——把 `ICommonMgmtdClient` 的九個方法一次性 `final` 轉接到 `shared_ptr<MgmtdClient>`；三個具體類別的共同基底 |
| `MgmtdClientForServer.h` | 35 | `CommonMgmtdClient<IMgmtdClientForServer>` + `Config{refresh=on, heartbeat=on, session=off}` + 五個方法轉接（`setConfigListener` → `setServerConfigListener`） |
| `MgmtdClientForClient.h` | 31 | `CommonMgmtdClient<IMgmtdClientForClient>` + `Config{refresh=on, heartbeat=off, session=on}` + 三個方法轉接（`setConfigListener` → `setClientConfigListener`） |
| `MgmtdClientForAdmin.h` | 98 | `CommonMgmtdClient<IMgmtdClientForAdmin>` + `Config{三個全 off, accept_incomplete=true}` + 13 個方法轉接 |
| `RoutingInfo.h` | 66 | `client::RoutingInfo` 類別：`raw()` / `lastRefreshTime()` / 四個 `getXxx` 回 optional / 兩個模板查詢 `getNodeBy` 與 `getServiceBy`；宣告 `logUnavailableChains` |
| `RoutingInfo.cc` | 59 | 四個 `getXxx` 的實作（裸指標 → optional 值拷貝）+ `logUnavailableChains()` |
| `ServiceInfo.h` | 25 | `struct ServiceInfo{name, id, nodeId, nodeStatus, endpoints}` + `filterAddress(type)`；把節點視角的 `flat::NodeInfo` 轉置成服務視角的中間結構 |

---

## 附：與其他報告的接續點

| 主題 | 本文位置 | 對接文件 |
|---|---|---|
| primary lease 的伺服端機制 | §4.3 客戶端探測 | `mgmtd_main` 報告 §4 |
| `RoutingInfoVersion` 單調性 | §4.5 的 FATAL 斷言 | `mgmtd_main` 報告 §0、§3.2 |
| 心跳的伺服端處理與 target 狀態機 | §4.6 客戶端送出 | `mgmtd_main` 報告 §7 |
| 冪等實作（`DirEntry.uuid` / idempotent record） | §6.7 客戶端 token 生成 | `meta_main` 報告 |
| 路徑解析、權限檢查 | §6.9 分工邊界 | `meta_main` 報告 |
| file session 的 FDB 儲存與 GC | §6.10 客戶端生命週期 | `meta_main` 報告 |
| `FileOperation` 對 storage 的 chunk 操作 | §6.12 | `storage_main` / `client_storage` 報告 |
| FUSE 的 dentry/attr 快取與 session 產生 | §6.8、§6.10 | `hf3fs_fuse_main` 報告 |
