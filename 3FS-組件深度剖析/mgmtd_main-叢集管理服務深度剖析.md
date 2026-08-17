# 3FS `mgmtd_main`（叢集管理服務）深度剖析

> 對應原始碼：`src/mgmtd/`（98 個檔案）、`src/fbs/mgmtd/`（跨端 schema）、`src/core/app/`（啟動骨架）、`src/client/mgmtd/`（消費端）
> 進入點：`src/mgmtd/mgmtd.cpp` — CMake 定義於 `src/mgmtd/CMakeLists.txt:2` 的 `target_add_bin(mgmtd_main "mgmtd.cpp" mgmtd)`
> 底層 KV：FoundationDB（`src/fdb/`、`src/common/kv/`）

---

## 0. 一句話總結

mgmtd 是 3FS 唯一的「全域真相來源」，但它**沒有自己的複製協定**——選主、腦裂防護、狀態持久化全部外包給 FoundationDB 的一個單鍵（`SINGMgmtdLease`）與交易語意；它自己則是一台**純記憶體的狀態機快取**，開機時把 FDB 全表載進記憶體，之後所有讀請求（`getRoutingInfo`）只碰記憶體，所有寫請求走「先在 FDB 把版本號 +1、再改內容、成功後才動記憶體」的固定順序，用這個順序保證 `RoutingInfoVersion` 在**跨主從切換時仍然單調遞增**。target 的健康狀態則被拆成兩個誰也不能單獨推進的欄位（storage 只寫 `localState`、mgmtd 只寫 `publicState`），透過心跳形成一個分散式握手，把 CRAQ 鏈的重組退化成一個**無副作用的純函式** `generateNewChain()`。

---

## 1. 這個 binary 是什麼 / 啟動流程

### 1.1 從 `main` 到 serve loop

`src/mgmtd/mgmtd.cpp` 只有 8 行：

```cpp
int main(int argc, char *argv[]) {
  using namespace hf3fs;
  return TwoPhaseApplication<mgmtd::MgmtdServer>().run(argc, argv);
}
```

「TwoPhase」指的是**設定的兩階段載入**：第一階段用本機檔案（`--launcher_cfg` / `--app_cfg`）把 FDB 連線資訊讀起來，第二階段從 FDB 讀出真正的服務設定。這解決了雞生蛋問題——設定存在 FDB，但要連 FDB 得先有設定，於是把「連 FDB 所需的最小設定」單獨抽成 launcher config。

完整呼叫鏈（`src/common/app/TwoPhaseApplication.h:36-70`）：

```
main
└─ TwoPhaseApplication<MgmtdServer>::run
   └─ parseFlags                                  TwoPhaseApplication.h:29
   │  ├─ ServerLauncherBase::parseFlags           core/app/ServerLauncher.cc:4
   │  │    解析 --app_config.* / --launcher_config.*
   │  └─ ApplicationBase::parseFlags("--config.")
   └─ initApplication                             TwoPhaseApplication.h:36
      ├─ launcher_->init()                        core/app/ServerLauncher.h:34
      │  ├─ appConfig_.init(FLAGS_app_cfg)        → ServerAppConfig{node_id}
      │  ├─ launcherConfig_.init(...)             → MgmtdLauncherConfig{cluster_id, kv_engine, ib_devices}
      │  ├─ net::IBManager::start(ib_devices)     ← RDMA 裝置在此初始化
      │  └─ fetcher_ = MgmtdConfigFetcher(launcherConfig_)
      │     └─ HybridKvEngine::from(...)          ← 此刻才真正連上 FDB
      ├─ loadAppInfo
      │  ├─ launcher::buildBasicAppInfo(nodeId, clusterId)
      │  └─ MgmtdConfigFetcher::completeAppInfo   mgmtd/MgmtdConfigFetcher.cc:38
      │        從 FDB 的 NODE 表讀回自己的 tags
      ├─ initConfig
      │  └─ MgmtdConfigFetcher::loadConfigTemplate(MGMTD)   MgmtdConfigFetcher.cc:23
      │        store.loadLatestConfig(txn, NodeType::MGMTD) → CONF 表
      ├─ initCommonComponents(log / monitor / memory)
      ├─ initServer
      │  └─ MgmtdServer(config) + net::Server::setup()
      └─ startServer
         └─ MgmtdConfigFetcher::startServer       MgmtdConfigFetcher.cc:52
            └─ MgmtdServer::start(appInfo, kvEngine)        MgmtdServer.cc:46
               └─ net::Server::start(appInfo)
                  └─ MgmtdServer::beforeStart()   MgmtdServer.cc:22
```

### 1.2 `beforeStart()`：真正的組裝點

`src/mgmtd/MgmtdServer.cc:22-38` 是整個進程的接線盤：

```cpp
Result<Void> MgmtdServer::beforeStart() {
  auto env = std::make_shared<core::ServerEnv>();
  env->setAppInfo(appInfo());
  env->setKvEngine(kvEngine_);
  env->setMgmtdStubFactory(std::make_shared<stubs::RealStubFactory<mgmtd::MgmtdServiceStub>>(serdeCtxCreator()));
  env->setBackgroundExecutor(&tpg().bgThreadPool());
  env->setConfigUpdater(ApplicationBase::updateConfig);
  env->setConfigValidater(ApplicationBase::validateConfig);

  mgmtdOperator_ = std::make_unique<MgmtdOperator>(std::move(env), config_.service());
  RETURN_ON_ERROR(addSerdeService(std::make_unique<MgmtdService>(*mgmtdOperator_)));
  RETURN_ON_ERROR(addSerdeService(std::make_unique<core::CoreService>()));

  mgmtdOperator_->start();   // ← 十個背景任務在此啟動
  return Void{};
}
```

三個值得注意的點：

**(a) mgmtd 自己也持有一個 MgmtdStub 工廠。** 它要能對「另一台 mgmtd」發 RPC——因為非 primary 的 mgmtd 必須向 primary 發心跳，才會出現在 RoutingInfo 裡讓 client 知道備援位址（見 §6.5）。

**(b) `configUpdater` 是一個 `std::function`，不是介面。** `ServerEnv::ConfigUpdater = std::function<Result<Void>(const String&, const String&)>`（`src/core/app/ServerEnv.h:34`）。這讓 mgmtd 的服務層完全不依賴 `ApplicationBase`，測試時（`MockMgmtd`，`src/mgmtd/service/MockMgmtd.h:42-46`）可以不設，程式碼會走「blindly promote」分支（`helpers.cc:33`）。

**(c) 背景任務綁在 `bgThreadPool()`**，與處理 RPC 的 proc 執行緒池分開。

### 1.3 兩個監聽埠

`src/mgmtd/MgmtdServer.h:26-35` 用 config 的預設值宣告了兩組 server group：

| group | 埠 | 網路型別 | 服務 | 獨立執行緒池 |
|---|---|---|---|---|
| 0 | 8000 | 預設（configs 裡是 RDMA） | `Mgmtd` | 否 |
| 1 | 9000 | TCP（硬編碼） | `Core` | 是 |

```cpp
CONFIG_OBJ(base, net::Server::Config, [](net::Server::Config &c) {
  c.set_groups_length(2);
  c.groups(0).listener().set_listen_port(8000);
  c.groups(0).set_services({"Mgmtd"});
  c.groups(1).set_network_type(net::Address::TCP);
  c.groups(1).listener().set_listen_port(9000);
  c.groups(1).set_use_independent_thread_pool(true);
  c.groups(1).set_services({"Core"});
});
```

**為什麼 Core 服務要用 TCP + 獨立執行緒池**：Core 是運維通道（`src/core/service/CoreService.h`，提供 renderConfig / hotUpdateConfig / getLastConfigUpdateRecord 等）。若它和業務流量共用 RDMA 與執行緒池，那麼「mgmtd 卡住了」的時候你就連不進去查看設定。獨立執行緒池 + 最普通的 TCP = **保證運維通道在最壞情況下仍可達**。

---

## 2. 整體分層架構

```
                            ┌──────────────────────────────────────┐
                            │   client / meta / storage / admin    │
                            └────────────────┬─────────────────────┘
                                             │ serde RPC (serviceId 217)
┌────────────────────────────────────────────▼─────────────────────────────────┐
│ MgmtdService          純轉接層，一個 macro 展開 23 個方法                        │
│                       src/mgmtd/service/MgmtdService.cc:5-8                    │
├──────────────────────────────────────────────────────────────────────────────┤
│ MgmtdOperator         同樣是 macro 展開，把 Req 包成 XxxOperation 物件            │
│                       再交給 CO_INVOKE_OP_INFO（記 metric + log + 錯誤碼）        │
│                       src/mgmtd/service/MgmtdOperator.cc:8-12                  │
├──────────────────────────────────────────────────────────────────────────────┤
│ ops/*Operation        24 個獨立的 operation 物件，每個一支 handle(MgmtdState&)   │
│                       ┌──────────────────────────────────────────────────┐   │
│                       │ 幾乎所有 handler 的骨架：                            │   │
│                       │   validateClusterId → validateAdmin →              │   │
│                       │   doAsPrimary([&]{ coScopedLock<"Name">() → ... }) │   │
│                       └──────────────────────────────────────────────────┘   │
├──────────────────────────────────────────────────────────────────────────────┤
│ MgmtdState            持有一切：env / config / store / 兩把 CoroSynchronized    │
│  ├─ CoroSynchronized<MgmtdData> data_        RoutingInfo + ConfigMap + Lease  │
│  ├─ CoroSynchronized<ClientSessionMap>       clientId → ClientSession         │
│  └─ folly::coro::Mutex writerMu_             寫操作的全域序列化鎖                │
│                       src/mgmtd/service/MgmtdState.h:21-67                    │
├──────────────────────────────────────────────────────────────────────────────┤
│ background/           10 個週期性任務（BackgroundRunner 驅動）                   │
│  extendLease · checkClientSessions · checkNewBornChains · checkHeartbeat      │
│  sendHeartbeat · updateChains · bumpRoutingInfoVersion · updateMetrics        │
│  persistTargetInfo · loadTargetInfo                                           │
│                       src/mgmtd/background/MgmtdBackgroundRunner.cc:37-80     │
├──────────────────────────────────────────────────────────────────────────────┤
│ store/MgmtdStore      key 編碼 + serde ⇄ FDB，無任何業務邏輯                     │
│                       src/mgmtd/store/MgmtdStore.cc                           │
├──────────────────────────────────────────────────────────────────────────────┤
│ common/kv/ITransaction   get / snapshotGet / getRange / set / clear           │
│ fdb/                     FoundationDB client + FDBRetryStrategy               │
└──────────────────────────────────────────────────────────────────────────────┘
```

`MgmtdService` 與 `MgmtdOperator` 兩層都是**同一個 X-macro 展開兩次**（`src/fbs/mgmtd/MgmtdServiceDef.h`）。這代表新增 RPC 只需改一處 def 檔加一行、寫一個 Operation 類別，另外三個地方（service 宣告、service 實作、operator 宣告、operator 實作、client stub）全部自動生成。代價是 IDE 跳轉困難、編譯錯誤訊息難讀。

---

## 3. 核心資料模型

### 3.1 三層 target 描述：`ChainTargetInfo` / `TargetInfo` / `LocalTargetInfo`

這是全域最容易混淆的地方。同一個 target 在三個地方以三種形狀出現：

| 型別 | 定義 | 欄位 | 誰產生 | 存在哪 |
|---|---|---|---|---|
| `flat::ChainTargetInfo` | `fbs/mgmtd/ChainTargetInfo.h:8` | `targetId`, `publicState` | mgmtd | **落盤**（`CHIF` chain 的一部分） |
| `flat::LocalTargetInfo` | `fbs/mgmtd/LocalTargetInfo.h:8` | `targetId`, `localState`, `diskIndex`, `usedSize`, `chainVersion`, `lowSpace` | storage | 心跳的 payload，**不落盤** |
| `flat::TargetInfo` | `fbs/mgmtd/TargetInfo.h:8` | 上述兩者的聯集 + `chainId`, `nodeId` | mgmtd 記憶體合併 | 部分落盤（`TGIF`，只為了位置資訊） |

再加上兩個 server 端的包裝：

- `mgmtd::TargetInfo`（`src/mgmtd/service/TargetInfo.h:7`）= `WithTimestamp<flat::TargetInfo>` + `locationInitLoaded` / `persistedNodeId` / `persistedDiskIndex` / `importantInfoChangedTime`
- `mgmtd::ChainTargetInfoEx`（`src/mgmtd/service/updateChain.h:8`）= `flat::ChainTargetInfo` + `localState`，**專門為了讓狀態機函式是純函式**而存在

`WithTimestamp<T>`（`src/mgmtd/service/WithTimestamp.h:8`）是個極小的模板：只是把 `T` 和一個 `SteadyTime ts_` 綁在一起。用 `SteadyClock` 而非 `UtcClock` 是刻意的——所有「超時判定」都基於單調時鐘，不受 NTP 校時影響。而 `lastHeartbeatTs`（要給人看的、要回傳給 client 的）才用 `UtcTime`。同一個概念存兩份時間戳，各服務各的目的。

### 3.2 `RoutingInfo`：server 端（可變）vs. `flat::RoutingInfo`（線上格式）

`src/mgmtd/service/RoutingInfo.h:31` 的註解直接標出了持久性等級：

```cpp
struct RoutingInfo {
  bool routingInfoChanged = false;                  // periodically promote routingInfoVersion
  flat::RoutingInfoVersion routingInfoVersion{1};   // ensure version 0 is less than any valid version
  NodeMap nodeMap;                                  // persistent
  ChainTableMap chainTables;                        // persistent
  ChainMap chains;                                  // persistent
  NewBornChains newBornChains;                      // temporal
  OrphanTargetsByTargetId orphanTargetsByTargetId;  // temporal
  OrphanTargetsByNodeId orphanTargetsByNodeId;      // temporal
 private:
  TargetMap targets;  // derived
};
```

**`targets` 是 private 的 derived 資料。** 它不落盤（狀態存在 `chains` 裡）、也不能直接改——外界只能透過 `getTargets()`（const）和 `updateTarget(tid, func)` 存取。`updateTarget` 帶了一道防護（`RoutingInfo.h:60-64`）：

```cpp
auto updateTarget(flat::TargetId tid, auto &&func) {
  XLOGF_IF(DFATAL, !targets.contains(tid), "tid = {}", tid);
  auto &ti = targets[tid];
  return func(ti);
}
```

這把「target 必須先屬於某條 chain 才能被更新」變成一個 debug build 會直接 abort 的不變式。所有繞過此不變式的路徑（storage 上報了不認識的 target）都被強制導向 orphan 集合（§7.4）。

`routingInfoVersion` 初值是 **1 而非 0**，註解說明「ensure version 0 is less than any valid version」。client 用 `routingInfoVersion = 0` 表示「我什麼都不知道，給我全量」，所以 0 必須是保留值。

### 3.3 `ChainTable`：append-only 的多版本

```cpp
using ChainTableVersionMap = std::map<flat::ChainTableVersion, flat::ChainTable>;  // 有序！
using ChainTableMap = robin_hood::unordered_map<flat::ChainTableId, ChainTableVersionMap>;
```

`ChainTableVersionMap` 是 `std::map` 而非 hash map，因為需要 `rbegin()` 取最新版（`SetChainTableOperation.cc:55`）。**舊版本永遠不刪除**——因為 meta 層的 `Layout` 裡記著 `tableVersion`（`src/fbs/meta/Schema.h`），老檔案要能用當時的 chain 表解析自己的 chunk 位置。ChainTable 因此是純 append-only 的歷史。

`flat::RoutingInfo::getChainTable(tableId, tableVersion=0)` 中 `tableVersion == 0` 代表「最新版」（`src/fbs/mgmtd/RoutingInfo.cc:11`）：

```cpp
auto vit = tableVersion != 0 ? tit->second.find(tableVersion) : (--tit->second.end());
```

### 3.4 版本號一覽

| 版本欄位 | 型別 | 誰遞增 | 何時遞增 | 落盤位置 |
|---|---|---|---|---|
| `RoutingInfoVersion` | u64 | mgmtd primary | 任何路由變更；或 `routingInfoChanged` 被置位後由 `bumpRoutingInfoVersion` 週期性推進 | `SINGRoutingInfoVersion`（單鍵） |
| `ChainVersion` | **u32** | mgmtd | chain 的 target 組成或狀態改變 | `CHIF` chain value 內 |
| `ChainTableVersion` | u32 | mgmtd | `setChainTable` 且內容真的變了 | `CHIT` key 的一部分 |
| `ConfigVersion` | u64 | mgmtd | `setConfig` | `CONF` key 的一部分 |
| `HeartbeatVersion` | u64 | **各節點自己** | 每次心跳成功後 +1 | 不落盤（只在 mgmtd 記憶體） |
| `ClientSessionVersion` | u64 | **client 自己** | 每次 extend | 不落盤 |

`ChainVersion` 特意是 uint32 而非 uint64，`MgmtdTypes.h:50` 有註解：

```cpp
// ChainVersion is space sensitive and uint32 is enough for changes of one chain
STRONG_TYPEDEF(uint32_t, ChainVersion);
```

「space sensitive」的原因是 `VersionedChainId{chainId, chainVer}` 會出現在 storage 的每一個寫入請求裡（`src/storage/service/TargetMap.h`），是熱路徑的線上開銷。

---

## 4. 主從選舉與 lease

### 4.1 沒有共識演算法，只有一把 FDB 的鑰匙

整個選主機制濃縮在 `MgmtdStore::extendLease`（`src/mgmtd/store/MgmtdStore.cc:154-189`）這一支函式裡，它在**一個 FDB 讀寫交易中**完成 read-modify-write：

```cpp
CoTryTask<flat::MgmtdLeaseInfo> MgmtdStore::extendLease(txn, nodeInfo, leaseLength, now, rv, checkReleaseVersion) {
  auto storedLeaseInfo = co_await loadMgmtdLeaseInfo(txn);     // 讀 SINGMgmtdLease（非 snapshot！）
  flat::MgmtdLeaseInfo newLeaseInfo(nodeInfo, now, now + leaseLength, rv);

  if (storedLeaseInfo.has_value()) {
    if (checkReleaseVersion && newLeaseInfo.releaseVersion < storedLeaseInfo->releaseVersion)
      co_return *storedLeaseInfo;                              // ① 版本回退防護
    if (storedLeaseInfo->leaseEnd >= now + leaseLength)
      co_return *storedLeaseInfo;                              // ② 已夠長，不寫
    if (storedLeaseInfo->leaseEnd < now)
      co_return co_await storeMgmtdLeaseInfo(txn, newLeaseInfo);  // ③ 已過期，搶！
    if (storedLeaseInfo->primary.nodeId == nodeInfo.nodeId) {
      newLeaseInfo.leaseStart = storedLeaseInfo->leaseStart;   // ④ 我是現任，續約（保留 leaseStart）
      co_return co_await storeMgmtdLeaseInfo(txn, newLeaseInfo);
    }
    co_return *storedLeaseInfo;                                // ⑤ 別人在位且未過期，認輸
  }
  co_return co_await storeMgmtdLeaseInfo(txn, newLeaseInfo);   // ⑥ 從無到有
}
```

因為 `loadMgmtdLeaseInfo` 用的是普通 `get`（不是 `snapshotGet`），這個 key 進入交易的**讀衝突集**。兩台 mgmtd 同時搶 lease → FDB 的可序列化隔離保證只有一個交易能提交，另一個拿到 `not_committed` 後由 `FDBRetryStrategy` 重試，重試時讀到新值就走分支 ⑤ 認輸。**這就是全部的選主邏輯。**

分支 ④ 的 `newLeaseInfo.leaseStart = storedLeaseInfo->leaseStart` 是關鍵細節：續約時 `leaseStart` **不變**。這讓 `leaseStart` 成為「本次任期的唯一識別碼」，下游用它判斷是否換屆（`MgmtdLeaseExtender.cc:233`、`MgmtdHeartbeater.cc:78`）。

### 4.2 `releaseVersion` 防降級

分支 ① 的 `checkReleaseVersion`（由 `extend_lease_check_release_version`，預設 true 控制）拒絕**軟體版本較舊的 mgmtd 搶到 lease**。註解只有一行「releaseVersion should not rollback」，但它擋掉的是滾動升級中一個真實的災難：新版 mgmtd 寫了新版才認得的資料（例如新增的 serde 尾端欄位），舊版接手後解析不出來或誤解。這是把「不可降級」這個運維紀律變成程式碼裡的硬性約束。

### 4.3 腦裂防護：時鐘只是快取，交易才是真相

lease 有一個典型弱點：primary 認為自己在位期間，時鐘漂移可能讓它比實際 leaseEnd 晚退位。3FS 的處理是**雙保險**。

**保險一（快路徑，記憶體）**：`MgmtdState::currentLease`（`MgmtdState.cc:66-75`）

```cpp
CoTask<std::optional<flat::MgmtdLeaseInfo>> MgmtdState::currentLease(UtcTime now) {
  auto dataPtr = co_await data_.coSharedLock();
  const auto &lease = dataPtr->lease;
  bool canTrustLease =
      lease.lease.has_value() && now + config_.suspicious_lease_interval().asUs() < lease.lease->leaseEnd;
  if (canTrustLease && !lease.bootstrapping) return lease.lease;
  return std::nullopt;
}
```

注意這裡不是 `now < leaseEnd`，而是 `now + suspicious_lease_interval < leaseEnd`。以預設值計算：

```
lease_length             = 60s
extend_lease_interval    = 10s
suspicious_lease_interval= 20s

時間軸（以舊 primary 最後一次成功續約為 t=0）：
t=0    ├─ leaseEnd = t+60
t=10   │  續約失敗（網路分區）
t=20   │  續約失敗
t=30   │  續約失敗
t=40   ▼ now + 20 = 60，不再 < leaseEnd(60) → 舊 primary 自認退位  ◄── 提早 20 秒
       ...（無人服務的空窗期）...
t=60   ▼ leaseEnd 到期，新 primary 才可能透過分支 ③ 搶到 lease
```

**舊 primary 停止服務的時刻嚴格早於新 primary 可能上任的時刻，中間留了 20 秒安全邊界。** 代價是主動製造了一段服務空窗（3FS 選擇了可用性換一致性——但這對元資料控制面是正確的，client 端有路由快取，短暫拿不到新路由不影響 I/O）。

**保險二（慢路徑，交易內）**：`withReadWriteTxn`（`src/mgmtd/service/helpers.h:43-62`）

```cpp
auto res = co_await kv::WithTransaction(strategy).run(
    state.env_->kvEngine()->createReadWriteTransaction(),
    [&](kv::IReadWriteTransaction &txn) -> Result {
      if (expectSelfPrimary && state.config_.validate_lease_on_write()) {
        CO_RETURN_ON_ERROR(co_await state.store_.ensureLeaseValid(txn, state.selfId(), state.utcNow()));
      }
      co_return co_await handler(txn);
    });
```

**每一筆寫入交易的第一件事，就是在同一個交易裡重讀 lease key。** 這把「我還是 primary」納入交易的讀衝突集：若在此期間有人搶走了 lease，寫入 lease key 的那個交易與本交易衝突，本交易必定失敗。時鐘判斷再怎麼漂移，都無法讓兩個 primary 同時寫成功。這是**外包給 FDB MVCC 的腦裂防護**——和 meta 層 `Distributor` 用 versionstamp 守護成員關係是同一套思路。

`ensureLeaseValid` 只有 14 行（`MgmtdStore.cc:191-204`），回傳的 `kNotPrimary` 錯誤訊息裡塞了真正 primary 的 nodeId：

```cpp
if (storedLeaseInfo->primary.nodeId != nodeId)
  co_return makeError(MgmtdCode::kNotPrimary, fmt::format("{}", storedLeaseInfo->primary.nodeId));
```

這個「用錯誤訊息當回饋通道」的手法在 mgmtd 出現三次（另外兩處：`kHeartbeatVersionStale` 帶 server 端版本號、`kClientSessionVersionStale` 帶 server 端版本號），client 端用 `scn::scan(msg, "{}", id)` 反解（`src/client/mgmtd/MgmtdClient.cc:146`、`:718`）。不算優雅，但省下了為每個錯誤定義一個結構化 payload 的成本。

### 4.4 接管：`onNewLease` 的全量載入

搶到 lease 之後（更精確說：偵測到 `leaseStart` 改變或 `bootstrapping` 仍為真），`MgmtdLeaseExtender.cc:252-255` 觸發 `onNewLease`。它在**單一 FDB 交易**中併發載入六張表（`MgmtdLeaseExtender.cc:139-152`）：

```cpp
auto [rivRes, nodesRes, configsRes, chainTablesRes, chainsRes, utagsRes] =
    co_await folly::coro::collectAll(loadRoutingInfoVersion(...),   // SING
                                     loadAllNodes(...),             // NODE
                                     loadAllConfigs(...),           // CONF
                                     loadAllChainTables(...),       // CHIT
                                     loadAllChains(...),            // CHIF
                                     loadAllUniversalTags(...));    // UTGS
```

用同一個交易而非六個獨立交易 → 得到一個**跨六張表的一致性快照**。`folly::coro::collectAll` 讓六個 range scan 併發發出，FDB client 端會 pipeline 它們。

載入完成後，`persistNewVersionAndSelfInfo`（`MgmtdLeaseExtender.cc:31-58`）在同一個交易裡把 `RoutingInfoVersion` **寫回 `nextVersion(loaded)`**。這是版本單調性的關鍵一步，見 §9.3。

`onNewLease` 成功後還會 `clientSessionMap->clear()`（`MgmtdLeaseExtender.cc:181-182`）——client session 完全不落盤，新 primary 從零開始收集，client 會在 `auto_extend_client_session_interval`（預設 10s）內重新報到。**session 是「租約制的軟狀態」，重建成本低於持久化成本。**

### 4.5 `bootstrapping`：兩個不同的旗標，一個名字

容易混淆的是有兩個 `bootstrapping`：

| 位置 | 型別 | 語意 | 期間 |
|---|---|---|---|
| `LeaseInfo::bootstrapping` | bool | 「我剛拿到 lease、尚未完成 `onNewLease`」 | 從偵測到換屆到載入完成，通常 < 1 秒 |
| `MgmtdData::bootstrapping(config)` | 函式 | 「距離 `leaseStartTs` 未滿 `bootstrapping_length`(2min)」 | 2 分鐘 |

前者讓 `currentLease` 回 nullopt，等於**在載入完成前，這台 mgmtd 對外一律回 `kNotPrimary`**（`helpers.cc:84-94`），避免服務出半份路由。

後者只是一個寫進 `flat::RoutingInfo.bootstrapping` 的提示旗標（`MgmtdData.cc:97`），告訴 client「我剛接手 2 分鐘內，可能還有節點沒來報到，路由可能不完整」。client 端可用 `accept_incomplete_routing_info_during_mgmtd_bootstrapping`（預設 true）決定是否丟棄（`src/client/mgmtd/MgmtdClient.cc:639-645`）。

---

## 5. RPC 服務面

### 5.1 完整方法表

`SERDE_SERVICE_2(MgmtdServiceBase, Mgmtd, 217)`（`src/fbs/mgmtd/MgmtdServiceBase.h:7`）→ **serviceId = 217**。方法編號來自 `src/fbs/mgmtd/MgmtdServiceDef.h`：

| # | 方法 | Req / Rsp | 需 admin | 需 primary | 寫鎖 | 動 FDB |
|---|---|---|---|---|---|---|
| 1 | `getPrimaryMgmtd` | `GetPrimaryMgmtdReq/Rsp` | – | **否** | – | 快取失效時讀 |
| 2 | *(已廢棄)* | | | | | |
| 3 | `heartbeat` | `HeartbeatReq/Rsp` | – | 是 | ✓`"Heartbeat"` | 節點資訊變更時寫 |
| 4 | `registerNode` | `RegisterNodeReq/Rsp` | ✓ | 是 | ✓ | 寫 |
| 5 | `getRoutingInfo` | `GetRoutingInfoReq/Rsp` | – | 是 | – | 否（純記憶體） |
| 6 | `setConfig` | `SetConfigReq/Rsp` | ✓ | 是 | ✓ | 寫 |
| 7 | `getConfig` | `GetConfigReq/Rsp` | – | 是 | – | 否 |
| 8 | `setChainTable` | `SetChainTableReq/Rsp` | ✓ | 是 | ✓ | 寫 |
| 9 | `enableNode` | `EnableNodeReq/Rsp` | ✓ | 是 | ✓ | 寫 |
| 10 | `disableNode` | `DisableNodeReq/Rsp` | ✓ | 是 | ✓ | 寫 |
| 11 | `extendClientSession` | `ExtendClientSessionReq/Rsp` | – | 是 | – | 否（純記憶體） |
| 12 | `listClientSessions` | `ListClientSessionsReq/Rsp` | – | 是 | – | 否 |
| 13 | `setNodeTags` | `SetNodeTagsReq/Rsp` | ✓ | 是 | ✓ | 有變更時寫 |
| 14 | `unregisterNode` | `UnregisterNodeReq/Rsp` | ✓ | 是 | ✓ | 寫 |
| 15 | `setChains` | `SetChainsReq/Rsp` | ✓ | 是 | ✓ | 寫 |
| 16 | `setUniversalTags` | `SetUniversalTagsReq/Rsp` | ✓ | 是 | ✓ | 有變更時寫 |
| 17 | `getUniversalTags` | `GetUniversalTagsReq/Rsp` | – | 是 | – | 否 |
| 18 | `getConfigVersions` | `GetConfigVersionsReq/Rsp` | – | 是 | – | 否 |
| 19 | `getClientSession` | `GetClientSessionReq/Rsp` | – | 是 | – | 否 |
| 20 | `rotateLastSrv` | `RotateLastSrvReq/Rsp` | ✓ | 是 | ✓ | 有變更時寫 |
| 21 | `listOrphanTargets` | `ListOrphanTargetsReq/Rsp` | – | 是 | – | 否 |
| 22 | `setPreferredTargetOrder` | `SetPreferredTargetOrderReq/Rsp` | ✓ | 是 | ✓ | 寫（**不 bump 版本**） |
| 23 | `rotateAsPreferredOrder` | `RotateAsPreferredOrderReq/Rsp` | ✓ | 是 | ✓ | 有變更時寫 |
| 24 | `updateChain` | `UpdateChainReq/Rsp` | ✓ | 是 | ✓ | 寫 |

**`getPrimaryMgmtd` 是唯一不需要 primary 身份的方法**（`GetPrimaryMgmtdOperation.cc:6-23`）。這是必然的——client 必須能問任何一台 mgmtd「誰是老大」。它先查記憶體 lease，miss 才開一個唯讀交易去 FDB 讀。所以一台從未當過 primary 的 mgmtd 也能正確回答這個問題。

### 5.2 每個 Operation 的統一骨架

以 `SetChainsOperation` 為例（`src/mgmtd/ops/SetChainsOperation.cc:101-142`），幾乎所有寫操作都是這五步：

```cpp
CoTryTask<SetChainsRsp> SetChainsOperation::handle(MgmtdState &state) {
  CO_RETURN_ON_ERROR(state.validateClusterId(*this, req.clusterId));       // ① 叢集隔離
  CO_RETURN_ON_ERROR(co_await state.validateAdmin(*this, req.user));       // ② 權限

  auto handler = [&]() -> CoTryTask<SetChainsRsp> {
    auto writerLock = co_await state.coScopedLock<"SetChains">();          // ③ 全域寫鎖

    { auto dataPtr = co_await state.data_.coSharedLock();                  // ④ 讀 + 驗證（共享鎖）
      CO_RETURN_ON_ERROR(checkChains(...)); }

    auto commitRes = co_await updateStoredRoutingInfo(state, *this,        // ⑤a 落盤
        [&](kv::IReadWriteTransaction &txn) -> CoTryTask<void> { ... });
    CO_RETURN_ON_ERROR(commitRes);

    co_await updateMemoryRoutingInfo(state, *this, [&](RoutingInfo &ri) {  // ⑤b 改記憶體（獨佔鎖）
      for (auto &newChain : newChains) ri.insertNewChain(newChain); });
    co_return SetChainsRsp::create();
  };
  co_return co_await doAsPrimary(state, std::move(handler));               // 包在 primary 檢查裡
}
```

**「④ 讀用共享鎖 → ⑤ 寫用獨佔鎖」中間有一段時間沒拿 `data_` 的鎖**（就是等 FDB 交易的那段），這是安全的**唯一原因**是外層的 `writerMu_` 序列化了所有寫者。這把鎖不保護資料，它保護的是「read-modify-write 這個時間區間」——一個典型的邏輯鎖 / 意向鎖。

`coScopedLock` 用了 `NameWrapper` 模板參數（`MgmtdState.h:54-58`），編譯期字串當模板參數，讓 `WriterMutexGuard` 的解構子能無成本地記錄 per-method 的鎖持有時間：

```cpp
MgmtdState::WriterMutexGuard::~WriterMutexGuard() {
  recordWriterLatency(method_, Duration(SteadyClock::now() - start_));   // MgmtdState.cc:82
}
```

→ metric `MgmtdService.WriterLatency{instance=SetChains}`。**鎖爭用直接變成可觀測指標**，不需要額外埋點。

### 5.3 幾個特殊 operation

**`setPreferredTargetOrder` 刻意不遞增 RoutingInfoVersion**（`SetPreferredTargetOrderOperation.cc:67-70`）：

```cpp
// do not increase RoutingInfoVersion
auto commitRes = co_await withReadWriteTxn(state, [&](kv::IReadWriteTransaction &txn) -> CoTryTask<void> {
  co_return co_await state.store_.storeChainInfo(txn, chainInfo);
});
```

因為 `preferredTargetOrder` 是純運維提示（給 `rotateAsPreferredOrder` 和 chains updater 用），不影響 client 的路由決策——client 只看 `targets` 的順序與 `publicState`。**不影響下游的變更就不製造版本推送**，避免全叢集無謂地拉一次完整 RoutingInfo。

**`updateChain` 新增 target 一律進 `OFFLINE`**（`UpdateChainOperation.cc:50-52`）：

```cpp
flat::ChainTargetInfo cti;
cti.targetId = req.targetId;
cti.publicState = flat::PublicTargetState::OFFLINE;
```

新 target 不能直接是 SERVING（沒有資料），也不能是 WAITING（storage 還沒回報 localState）。放進 OFFLINE，讓 §7 的狀態機在收到心跳後自然推進 `OFFLINE → WAITING → SYNCING → SERVING`。移除則嚴格要求 target 當前為 OFFLINE（`UpdateChainOperation.cc:71-78`），並在同一交易裡順手清掉 `TGIF` 記錄。

**`extendClientSession` 的欄位不變性檢查**（`ExtendClientSessionOperation.cc:60-96`）：如果同一個 clientId 送來的 `universalId` / `description` / `serviceGroups` / `releaseVersion` / `type` / `clientStart` 任一與已記錄的不符，回 `kExtendClientSessionMismatch`。這偵測的是「兩個不同進程用了同一個 clientId」——通常是設定錯誤或 UUID 生成有問題。加上 `only_accept_client_uuid` 開關（預設 false，只警告）強制 clientId 必須是合法 hex UUID。

---

## 6. 背景任務群

`MgmtdBackgroundRunner::start()`（`src/mgmtd/background/MgmtdBackgroundRunner.cc:37-80`）啟動十個任務。全部由 `BackgroundRunner` 驅動，共用同一個 `CPUExecutorGroup`（`bgThreadPool`），每個任務一條獨立的協程迴圈。

### 6.1 總表

| # | 任務名 | 週期設定（預設） | 需 primary | 拿 `writerMu_` | 職責 |
|---|---|---|---|---|---|
| 1 | `extendLease` | `extend_lease_interval` (10s) | **否** | ✓`"ExtendLease"` | 續約 / 搶主 / 接管載入 |
| 2 | `checkClientSessions` | `check_status_interval` (10s) | 是 | ✗ | 清除逾時 client session |
| 3 | `checkNewBornChains` | `check_status_interval` (10s) | 是 | ✗ | 解除新 chain 的靜默期 |
| 4 | `checkHeartbeat` | `check_status_interval` (10s) | 是 | ✓`"CheckHeartbeat"` | 判定節點/target 心跳逾時 |
| 5 | `sendHeartbeat` | `send_heartbeat_interval` (10s) | **反向：非 primary 才做** | ✗ | 本機 mgmtd 向 primary 報到 |
| 6 | `updateChains` | `update_chains_interval` (1s) | 是 | ✓`"UpdateChains"` | 執行 chain 狀態機、落盤 |
| 7 | `bumpRoutingInfoVersion` | `bump_routing_info_version_interval` (5s) | 是 | ✓`"BumpRoutingInfoVersion"` | 推進被延遲的版本號 |
| 8 | `updateMetrics` | `update_metrics_interval` (1s) | 是 | ✗ | 匯出 11 組 metric |
| 9 | `persistTargetInfo` | `target_info_persist_interval` (1s) | 是 | ✗ | 把 target 位置寫進 `TGIF` |
| 10 | `loadTargetInfo` | `target_info_load_interval` (1s) | 是 | ✗ | 從 `TGIF` 補回 target 位置 |

`BackgroundRunner::run`（`src/common/utils/BackgroundRunner.cc:73-116`）的兩個細節：

- 週期是 **fixed-rate 而非 fixed-delay**：`if (now < start + interval) sleep(start + interval - now)`。任務跑超過週期就立刻進下一輪，不累積漂移。
- `intervalGetter` 是**每輪重新求值的 `std::function`**（`state_.config_.update_chains_interval_getter()`），所以熱更新設定會在下一輪立即生效，`interval == 0` 直接讓任務停掉——這給了「不重啟就關閉某個背景任務」的能力。
- 任何**未捕捉的例外直接 `XLOGF(FATAL)`**（BackgroundRunner.cc:80）。設計哲學是：背景任務拋例外代表不變式已被破壞，快速崩潰讓另一台 mgmtd 接管，好過帶病運行。

### 6.2 `updateChains`：整個叢集自癒的心臟（1 秒一次）

`src/mgmtd/background/MgmtdChainsUpdater.cc:13-55`。它是唯一會主動改變 chain 拓撲的常駐任務。

```cpp
CoTryTask<SteadyTime> handle(MgmtdState &state, SteadyTime lastUpdateTs, bool lastAdjustTargetOrderFlag) {
  auto writerLock = co_await state.coScopedLock<"UpdateChains">();
  auto steadyNow = SteadyClock::now();
  {
    auto dataPtr = co_await state.data_.coSharedLock();
    for (const auto &[tid, ti] : ri.getTargets()) {
      if (ti.ts() > lastUpdateTs) {                       // ① 只看「上輪之後有變動」的 target
        candidateChains.insert(ti.base().chainId);
      } else if (!lastAdjustTargetOrderFlag && state.config_.try_adjust_target_order_as_preferred()) {
        candidateChains.insert(ti.base().chainId);        // ② 開關剛被打開 → 全掃一次
      }
      needPromoteRoutingInfoVersion |= ti.importantInfoChangedTime > lastUpdateTs;
    }
    for (auto chainId : candidateChains)
      dataPtr->appendChangedChains(chainId, changedChains, ...);
  }
  if (changedChains.empty() && !needPromoteRoutingInfoVersion) co_return steadyNow;
  ...
}
```

**`lastUpdateTs` 是一個跨輪傳遞的可變參數**，由 `MgmtdBackgroundRunner.cc:61` 的 lambda 捕獲持有：

```cpp
backgroundRunner_->start("updateChains",
    [this, lastUpdateTs = SteadyClock::now()]() mutable { return chainsUpdater_->update(lastUpdateTs); },
    ...);
```

而且只有**成功時才推進**（`MgmtdChainsUpdater.cc:76`）。失敗（例如 FDB 交易衝突）時 `lastUpdateTs` 不動，下一輪會重新掃到同一批 target。這是一個沒有佇列的**增量掃描 + 失敗自動重掃**設計：不需要維護 dirty set，時間戳就是 dirty 標記。

`② lastAdjustTargetOrderFlag` 這個分支處理的是設定熱更新的邊界情況：`try_adjust_target_order_as_preferred` 從 false 改成 true 時，那些「時間戳沒變但需要重排」的 chain 不會被 ① 掃到，所以用一個記憶上輪開關值的 bool 觸發一次全掃。

**`needPromoteRoutingInfoVersion` 與 `changedChains` 是兩件不同的事**：
- `changedChains` 不為空 → chain 的 `publicState` 變了 → 必須落盤 + 推版本
- `importantInfoChangedTime > lastUpdateTs` → target 的 `localState` 或 `nodeId` 變了（`RoutingInfo.cc:65-68`），但 chain 的 publicState 可能沒變 → **不需要落盤，但要推版本**，因為 client 拿到的 `flat::TargetInfo` 裡有 `localState` 和 `nodeId`

### 6.3 `bumpRoutingInfoVersion`：延遲聚合的版本推進（5 秒一次）

`src/mgmtd/background/MgmtdRoutingInfoVersionUpdater.cc:13-26`。它只做一件事：檢查 `routingInfo.routingInfoChanged`，若為 true 就推進版本並落盤。

誰置位 `routingInfoChanged`？只有兩處：
- `HeartbeatOperation.cc:175`：`ri.routingInfoChanged |= statusChanged`（節點狀態變了）
- `MgmtdHeartbeatChecker.cc:74`：`ri.routingInfoChanged = true`（判定節點心跳失敗）

**為什麼要延遲聚合？** 節點狀態變化（CONNECTING→CONNECTED）本身不影響資料路由，client 不急著知道。但每次都推版本會導致：一台 storage 重啟 → 版本 +1 → 全叢集數千個 client 同時拉一次完整 RoutingInfo（可能幾 MB）。用 5 秒的批次窗口把 N 次變更合併成 1 次推送。而真正影響 I/O 的 chain 變更則走 `updateChains` 的 1 秒路徑，不受此延遲影響。

### 6.4 `checkHeartbeat`：兩種逾時判定（10 秒一次）

`src/mgmtd/background/MgmtdHeartbeatChecker.cc:19-78`。同一輪裡做兩件獨立的事：

```cpp
for (const auto &[nodeId, nodeInfo] : ri.nodeMap) {
  switch (nodeInfo.base().status) {
    case DISABLED: case HEARTBEAT_FAILED: case PRIMARY_MGMTD: break;   // 這三種不檢查
    default:
      if (nodeInfo.ts() + heartbeatFailInterval < steadyNow) { ...標記 HEARTBEAT_FAILED... }
  }
}
for (const auto &[tid, ti] : ri.getTargets()) {
  if (ti.base().localState != OFFLINE && ti.ts() + heartbeatFailInterval < steadyNow)
    candidateTargetIds.push_back(tid);     // → localState = OFFLINE
}
```

**node 與 target 分開判定，而非「node 掛了就把它的 target 全標 OFFLINE」。** 原因在 `RoutingInfo::localUpdateTargets`：target 的 `nodeId` 是從心跳學來的，可能與 `nodeMap` 裡的節點不同步（target 遷移到別的節點）。分開判定讓兩者各自基於自己的時間戳，不需要維護 node→targets 的反向索引，也避免了「node 記錄過期但 target 記錄新鮮」的矛盾。

`PRIMARY_MGMTD` 被排除是因為 primary 自己不對自己發心跳（§6.5），它的 `ts()` 永遠不會更新。

注意這個任務**只改記憶體、不落盤、不推版本**——它把 `routingInfoChanged = true` 交給 `bumpRoutingInfoVersion`（5s）處理，把 target 的 OFFLINE 交給 `updateChains`（1s）觸發 chain 重組。**職責嚴格單一：只做判定，不做反應。**

### 6.5 `sendHeartbeat`：mgmtd 也是叢集的一員（10 秒一次）

`src/mgmtd/background/MgmtdHeartbeater.cc:58-92`。這是唯一一個「**是 primary 就跳過**」的任務：

```cpp
auto lease = co_await state_.currentLease(start);
if (!lease.has_value())                          { sendHeartbeatCtx_.reset(); co_return; }  // 無主
else if (lease->primary.nodeId == state_.selfId()){ sendHeartbeatCtx_.reset(); co_return; } // 我就是主
```

備援 mgmtd 必須向 primary 報到，理由有二：

1. 讓自己出現在 `RoutingInfo.nodes` 裡 → client 從路由表學到備援 mgmtd 的位址（`MgmtdClient.cc:215` 的 `tryAddMgmtd`），這樣即使設定檔裡只寫了一個 mgmtd 位址，client 也能發現全部。
2. 從 `HeartbeatRsp.config` 拿到最新設定（`MgmtdHeartbeater.cc:37-45`）——這是備援 mgmtd 唯一的設定更新管道。

primary 自己的 `NodeInfo` 則由 `persistNewVersionAndSelfInfo` 在接管時寫入（`MgmtdLeaseExtender.cc:53-56`）。

`SendHeartbeatContext` 的重建條件是 `sendHeartbeatCtx_->lease.leaseStart != lease->leaseStart`——**換屆時才重建 stub**，正常情況下重用同一條連線。程式碼裡留了 TODO：「consider reuse some facilities of MgmtdClient for auto switching addresses」，目前只用 `addrs[0]`，沒有位址輪替。

### 6.6 `checkNewBornChains`：靜默期（10 秒一次）

`src/mgmtd/background/MgmtdNewBornChainsChecker.cc:13-51`。`newBornChains` 是個 `chainId → SteadyTime` 的 map，只要 chain 在裡面，`appendChangedChains` 第一行就直接 return（`MgmtdData.cc:119`）：

```cpp
void MgmtdData::appendChangedChains(chainId, changedChains, tryAdjustTargetOrder) const {
  if (routingInfo.newBornChains.contains(chainId)) return;
  ...
}
```

也就是 **chains updater 完全不碰處於靜默期的 chain**。誰會進 `newBornChains`？

1. `setChains` 建立的新 chain（`RoutingInfo.cc:100`）
2. **`reset()` 時的全部 chain**（`RoutingInfo.cc:168-171`），註解說得很白：

```cpp
for (const auto &[cid, _] : chains) {
  // avoid update chain too early after restart or the chain status may be not stable
  newBornChains[cid] = steadyNow;
}
```

這是整個 mgmtd 裡最重要的一道安全閥。想像沒有它會發生什麼：新 primary 接管 → 記憶體裡所有 target 的 `localState` 都是 `OFFLINE`（`makeTargetInfo` 的初值，`helpers.cc:18`）→ 1 秒後 `updateChains` 跑起來 → `generateNewChain` 看到全部 OFFLINE → **把整個叢集所有 chain 都降級**，chainVersion 全部 +1，storage 收到後大規模觸發 resync。有了 `new_chain_bootstrap_interval`（預設 2min）的靜默期，storage 節點有充分時間（心跳 10s 一次）把真實的 `localState` 報上來。

解除靜默時（`MgmtdNewBornChainsChecker.cc:42-48`）會把該 chain 所有 target 的 `ts()` 刷成 now，**確保 `updateChains` 下一輪一定會掃到它們**（因為 `ti.ts() > lastUpdateTs`）。

還有一道 lease 換屆防護（`MgmtdNewBornChainsChecker.cc:35-38`）：先用共享鎖記下 `leaseStartTs`，取獨佔鎖後重新比對，不同就整輪放棄。這是一個 **optimistic 的 check-then-act 保護**，用在所有「共享鎖讀 → 獨佔鎖寫」的背景任務裡（`MgmtdTargetInfoLoader.cc:27-31`、`:53-55`、`:84-86` 也是同一手法）。

### 6.7 `persistTargetInfo` / `loadTargetInfo`：只持久化位置，不持久化狀態

這對任務是 `TGIF` 表存在的全部理由。

`MgmtdTargetInfoPersister.cc:22-31` 的過濾條件精確到三個條件缺一不可：

```cpp
if (ti.locationInitLoaded      // ① 我已經知道 KV 裡存的是什麼（避免用未初始化的值覆蓋）
    && ti.base().nodeId        // ② 我知道實際位置（心跳來過）
    && (ti.persistedNodeId != ti.base().nodeId ||
        ti.persistedDiskIndex != ti.base().diskIndex))   // ③ 兩者不同
  candidates.push_back(ti.base());
```

`persistedNodeId` / `persistedDiskIndex` 是**記憶體裡的「KV 影子副本」**，用來避免每秒都對沒變的 target 發起無謂寫入。批次上限 `target_info_persist_batch`（預設 1000）防止單一交易過大（FDB 單交易有 10MB 上限）。

`MgmtdTargetInfoLoader.cc:20-100` 則是「每個 lease 只跑一次」的載入：

```cpp
auto dataPtr = co_await state.data_.coSharedLock();
if (loadedLeaseStart_ >= dataPtr->leaseStartTs) co_return Void{};   // 已載過
loadedLeaseStart_ = dataPtr->leaseStartTs;
```

它分批（`loadTargetsFrom(txn, startTid)`）掃完整張 `TGIF` 表，把 `nodeId` / `diskIndex` 填回記憶體。關鍵在 `:68-72`：

```cpp
if (!ti.base().nodeId) {          // 只在心跳還沒告訴我位置時才填
  ti.base().nodeId = loaded.nodeId;
  ti.base().diskIndex = loaded.diskIndex;
}
```

**KV 是後備、心跳是權威。** 心跳先到就用心跳的，KV 只填補空缺。最後一段（`:82-97`）把所有沒在 `TGIF` 裡找到的 target 也標成 `locationInitLoaded = true`——因為「查過了、沒有」也是一種「已知」，這樣 persister 的條件 ① 才會放行，讓這些 target 首次被寫入 `TGIF`。

**為什麼需要這張表？** target 的狀態（publicState）存在 `CHIF` 的 chain 裡，重啟後可以還原；但「target 100 在 node 5 的第 3 顆盤上」這個資訊只存在於 storage 的心跳裡。mgmtd 重啟後如果某台 storage 正好也掛了，admin 就完全查不到「這個 target 的資料在哪台機器」。`TGIF` 讓這個運維關鍵資訊在 mgmtd 重啟後仍然可查。

### 6.8 `checkClientSessions` 與 `updateMetrics`

`checkClientSessions`（`MgmtdClientSessionsChecker.cc:12-35`）是最單純的一個：掃 `clientSessionMap_`，`ts() + client_session_timeout(20min) < now` 的刪掉。用了「共享鎖收集候選 → 獨佔鎖時**重新驗證**」的模式（`:29`），避免刪掉在兩次加鎖之間剛續約的 session。

`updateMetrics`（`MgmtdMetricsUpdater.cc:44-63`）一次匯出 11 組指標，見 §10.2。它的 `recordCount<MetricName>` 模板（`:11-37`）用一個 function-local `static std::map<Key, ValueRecorder>` 快取 recorder，key 可以是 tuple（用 `std::apply` 展開成標籤）。**這個 static map 只增不減**——一旦某個 (type, status) 組合出現過就永久佔一個 recorder 槽位。對於 nodeId/chainId/targetId 為 key 的指標（`MgmtdService.NodeStatus`、`MgmtdService.AbnormalChainStatus`）這是潛在的記憶體洩漏路徑（見 §12）。

---

## 7. 心跳與 target 狀態機

### 7.1 心跳協定

```
storage/meta/mgmtd(備)                         mgmtd(primary)
        │                                            │
        │  HeartbeatReq{                             │
        │    clusterId,                              │
        │    info: HeartbeatInfo{                    │
        │      app: FbsAppInfo{nodeId,hostname,...}, │
        │      hbVersion,                            │
        │      configVersion,                        │
        │      configStatus,                         │
        │      info: variant<Meta|Storage|Mgmtd>     │
        │           └─ Storage: vector<LocalTargetInfo>
        │    },                                      │
        │    timestamp: UtcTime  ← 用於時鐘偏移檢測      │
        │  }                                         │
        ├───────────────────────────────────────────►│
        │                                            │ ① validateClusterId  ← 唯一在
        │                                            │    doAsPrimary 之外的檢查
        │                                            │ ② doAsPrimary
        │                                            │ ③ nodeId != self && nodeId != 0
        │                                            │ ④ |now - timestamp| < 30s
        │                                            │ ⑤ writerMu_
        │                                            │ ⑥ checkConfigVersion
        │                                            │ ⑦ type 一致 / 非 DISABLED
        │                                            │ ⑧ hbVersion > lastHbVersion
        │                                            │ ⑨ storage: target id 不重複、
        │                                            │           localState 合法
        │                                            │ ⑩ PersistentNodeInfo 變了才落盤
        │                                            │ ⑪ localUpdateTargets
        │  HeartbeatRsp{ config: optional<ConfigInfo> }
        │◄───────────────────────────────────────────┤
        │                                            │
   hbVersion += 1                                    │
   若 config 非空 → 套用並更新 configVersion            │
```

### 7.2 `HeartbeatVersion`：拒絕重放與亂序

`NodeInfoWrapper::version_`（`src/mgmtd/service/NodeInfoWrapper.h:27`）的註解是「used for rejecting stale heartbeat requests」。判定在 `HeartbeatOperation.cc:57-60`：

```cpp
if (oldNode->lastHbVersion() >= hb.hbVersion) {
  return makeError(MgmtdCode::kHeartbeatVersionStale,
                   fmt::format("{}", oldNode->lastHbVersion().toUnderType()));
}
```

**注意是 `>=` 而非 `>`**——同一個版本號的重送也會被拒。這讓心跳處理天然冪等：網路重傳不會導致 `nodeMap` 被同一份資料重複覆寫、也不會刷新 `ts()` 造成假活。

`HeartbeatInfo::hbVersion` 預設值是 **1**（`fbs/mgmtd/HeartbeatInfo.h:87-88`，註解「should be larger than server's so use 1 as default value」），而 server 端 `NodeInfoWrapper::version_` 預設 **0**。所以節點首次心跳（v=1 > 0）必定通過。

節點重啟後 hbVersion 從 1 重新開始，會被 server 拒絕。恢復機制是 client 解析錯誤訊息裡的版本號直接跳過去（`MgmtdHeartbeater.cc:26-34`、`MgmtdClient.cc:716-724`）：

```cpp
if (res.error().code() == MgmtdCode::kHeartbeatVersionStale) {
  uint64_t v = 0;
  auto result = scn::scan(String(res.error().message()), "{}", v);
  if (result) info.hbVersion = flat::HeartbeatVersion(v + 1);
  else        info.hbVersion = flat::HeartbeatVersion(info.hbVersion + 1);
}
```

一次往返即可恢復，不需要持久化 hbVersion。

### 7.3 時間戳偏移檢查

`HeartbeatOperation.cc:134-141`：

```cpp
const auto validWindow = state.config_.heartbeat_timestamp_valid_window().asUs();
if (validWindow.count() != 0 && (timestamp + validWindow < now || now + validWindow < timestamp))
  CO_RETURN_AND_LOG_OP_ERR(*this, MgmtdCode::kHeartbeatFail, "Too much timestamp deviation. ...");
```

雙向 30 秒窗口。這保護的是**整個 lease 機制的前提**：lease 的過期判定用的是各機器自己的 UTC 時鐘（`state.utcNow()`），若某台 mgmtd 的時鐘慢了幾分鐘，它會以為 lease 還沒過期而繼續當 primary。心跳的時間戳檢查是這個假設的執行期驗證——雖然它只驗證 mgmtd↔其他節點，不直接驗證 mgmtd↔mgmtd，但實務上所有節點時鐘來自同一個 NTP 源，任一節點偏移都會被偵測到。設 `validWindow = 0` 可關閉此檢查。

### 7.4 `localUpdateTargets`：全量覆蓋 + orphan 收容

`src/mgmtd/service/RoutingInfo.cc:38-87` 是心跳處理裡最微妙的一段。

```cpp
void RoutingInfo::localUpdateTargets(nodeId, targets, config) {
  auto &orphans = orphanTargetsByNodeId[nodeId];
  for (auto tid : orphans) orphanTargetsByTargetId.erase(tid);
  orphans.clear();                                   // ① 先清空這個節點的 orphan

  for (const auto &lti : targets) {
    auto it = this->targets.find(lti.targetId);
    if (it != this->targets.end()) {
      updateTarget(lti.targetId, [&](auto &ti) {
        bool shouldIgnore = [&] {                    // ② 過期心跳過濾
          if (lti.chainVersion == 0 || !config.heartbeat_ignore_stale_targets()) return false;
          return getChain(ti.base().chainId).chainVersion > lti.chainVersion;
        }();
        if (shouldIgnore) return;

        ti.updateTs(steadyNow);
        bool importantInfoChanged = base.localState != lti.localState || base.nodeId != nodeId;
        if (importantInfoChanged) ti.importantInfoChangedTime = steadyNow;

        base.localState = lti.localState;  base.nodeId = nodeId;
        base.diskIndex = lti.diskIndex;    base.usedSize = lti.usedSize;
      });
    } else {
      orphans.insert(lti.targetId);                  // ③ 不認識 → orphan
      auto &ti = orphanTargetsByTargetId[lti.targetId];
      ti = flat::TargetInfo();  // ensure clean state
      ti.targetId = lti.targetId; ti.localState = lti.localState; ti.nodeId = nodeId; ...
    }
  }
}
```

**① 的「先清空再重建」使心跳成為全量快照語意**：storage 不再上報某個 target，該 target 就自動離開 orphan 集合。不需要 storage 發「刪除」訊息。

**② 是版本柵欄，防止過期心跳把 chain 打回舊狀態。** 場景：mgmtd 把 chain 從 v5 升到 v6（某個 target 轉 SYNCING），但一個 v5 時期發出的心跳晚到了，裡面的 `localState` 反映的是舊拓撲。`lti.chainVersion` 是 storage 在打包心跳時記下的自己所知的 chainVersion（`src/storage/service/Components.cc:255`）；若它比 mgmtd 當前的 chainVersion 舊，整筆更新丟棄。

`lti.chainVersion == 0` 時**不過濾**——因為 storage 只在 `localState != OFFLINE` 時才填 chainVersion（`Components.cc:253-256`）：

```cpp
if (targetInfo.localState != flat::LocalTargetState::OFFLINE) {
  targetInfo.usedSize = target.storageTarget->usedSize();
  targetInfo.chainVersion = target.vChainId.chainVer;
}
```

所以 `chainVersion == 0` 等同「這個 target 已 OFFLINE」，而 **OFFLINE 的消息永遠不該被丟棄**——它是最需要立刻反應的事件。

**③ orphan 集合是雙向索引**（`orphanTargetsByTargetId` + `orphanTargetsByNodeId`），`eraseOrphanTarget`（`RoutingInfo.cc:130-143`）用了三個 `XLOGF_IF(FATAL, ...)`（`:133`、`:134`、`:139`）驗證雙向索引的一致性。這種「用斷言把資料結構不變式寫進程式碼」的密度在整個 mgmtd 隨處可見。

orphan 的實務意義：storage 節點的磁碟上有 target 目錄，但 chain table 裡沒有它。可能是誤刪 chain、可能是換盤後殘留、可能是設定錯誤。`listOrphanTargets`（`ListOrphanTargetsOperation.cc:11-19`）讓 admin 能列出來，配合 `MgmtdService.OrphanTargetCount` 指標告警。

### 7.5 `generateNewChain`：CRAQ 鏈重組的純函式

`src/mgmtd/service/updateChain.cc:25-104` 是整個 mgmtd 邏輯密度最高的 80 行。它的簽章刻意設計成無副作用：

```cpp
std::vector<ChainTargetInfoEx> generateNewChain(const std::vector<ChainTargetInfoEx> &oldTargets);
```

`updateChain.h:37` 有註解「separate this function just for testing friendly」。輸入是 `(publicState, localState)` 對的有序列表，輸出是新的有序列表。**沒有 MgmtdState、沒有時間、沒有 IO** ——一個可以用表格窮舉測試的狀態轉換函式。

演算法按 `publicState` 分桶，然後**按固定優先順序**處理五個桶。順序本身就是語意：

```
處理順序：SERVING → LASTSRV → SYNCING → WAITING → OFFLINE
輸出順序：SERVING → LASTSRV → SYNCING → WAITING → OFFLINE      （updateChain.cc:98）
```

因為 CRAQ 的寫入沿著 vector 順序從 head 流到 tail，**「狀態機」和「鏈重排」是同一個動作**。SERVING 的節點自動排在最前面成為寫入路徑，OFFLINE 的沉到最後。

完整轉換規則：

```
                     localState = UPTODATE   localState = ONLINE      localState = OFFLINE
                    ┌───────────────────────┬────────────────────────┬─────────────────────────┐
 SERVING            │ SERVING               │ SERVING                │ 第一個→LASTSRV           │
                    │                       │                        │ 其餘  →OFFLINE           │
                    ├───────────────────────┼────────────────────────┼─────────────────────────┤
 LASTSRV            │ 無 SERVING → SERVING   │ 無 SERVING → SERVING    │ 無 SERVING → LASTSRV     │
                    │ 有 SERVING → OFFLINE   │ 有 SERVING → OFFLINE    │ 有 SERVING → OFFLINE     │
                    ├───────────────────────┼────────────────────────┼─────────────────────────┤
 SYNCING            │ SERVING               │ 有 SERVING → SYNCING    │ OFFLINE                 │
                    │                       │ 無 SERVING → WAITING    │                         │
                    ├───────────────────────┼────────────────────────┼─────────────────────────┤
 WAITING            │ WAITING               │ 有 SERVING 且無 SYNCING  │ OFFLINE                 │
                    │                       │   → SYNCING             │                         │
                    │                       │ 否則 → WAITING           │                         │
                    ├───────────────────────┼────────────────────────┼─────────────────────────┤
 OFFLINE            │ WAITING               │ 有 SERVING 且無 SYNCING  │ OFFLINE                 │
                    │                       │   → SYNCING             │                         │
                    │                       │ 否則 → WAITING           │                         │
                    └───────────────────────┴────────────────────────┴─────────────────────────┘

最後一道收尾（updateChain.cc:90-95）：
  若最終有任何 SERVING，所有 LASTSRV 一律降為 OFFLINE
```

三個非顯而易見的設計決策：

**(a) 「同時只允許一個 SYNCING」。** WAITING/OFFLINE → SYNCING 的條件都帶 `newTargetsByPs[PS::SYNCING].empty()`。resync 會消耗 head 的頻寬（資料從 SERVING 的副本流向 SYNCING 的副本），一次只讓一個副本追趕，避免恢復流量壓垮線上服務。

**(b) 「全部 SERVING 掉線時，只有第一個變 LASTSRV」**，程式碼裡有一段坦白的註解（`updateChain.cc:36-38`）：

```cpp
// If all SERVING offlined, only the first becomes LASTSRV.
// NOTE: in such cases the whole chain has to wait the HEAD for recovering even
//       when other replicas are complete.
```

這是刻意選擇的保守策略。CRAQ 中，head 是寫入的入口，它擁有的資料版本一定不落後於下游。掛掉的瞬間你無法知道下游是否已經收到最後幾筆寫入，所以只信任 head。**代價是可用性**：即使 tail 的資料是完整的，整條 chain 也得等 head 回來。這就是 `rotateLastSrv` 這個手動 RPC 存在的理由。

**(c) `rotateLastSrv` 是一個帶明確風險警告的逃生口**（`updateChain.h:43-45`）：

```cpp
// If the head of oldTargets is LASTSRV, move it to the tail and let the next target be the new LASTSRV.
// It's used when a LASTSRV target could not recover in a short time. Admin could let the next target be the new
// LASTSRV to resume the service. NOTE: it's on risk of losing some data forever.
```

實作（`updateChain.cc:143-163`）把 head 輪到尾巴，讓次位成為新的 LASTSRV，其餘全部 OFFLINE。註解說明了為何不直接轉 SERVING：

```cpp
// possible conversions:
// * WAITING -> LASTSRV: it's safer to convert a WAITING to LASTSRV than to SERVING
```

轉成 LASTSRV 而非 SERVING，意味著它仍然需要一輪 `generateNewChain`（在 localState 為 ONLINE/UPTODATE 時）才會真正變 SERVING。**多繞一步，讓狀態機重新驗證一次。**

### 7.6 雙向握手：兩端各持一半的狀態機

mgmtd **只寫** `publicState`，storage **只寫** `localState`，兩者互為對方的輸入。storage 端的另一半在 `src/storage/service/TargetMap.cc:331-354`：

```cpp
hf3fs::flat::LocalTargetState TargetMap::updateLocalState(targetId, localState, publicState) {
  if (localState == UPTODATE && (publicState == OFFLINE || publicState == LASTSRV || publicState == WAITING)) {
    return OFFLINE;                    // ← 我以為我是好的，但 mgmtd 說我不在服務中 → 自我下線
  } else if (localState == ONLINE && publicState == SERVING) {
    return UPTODATE;                   // ← mgmtd 說我在服務中 → 我確認自己資料是最新的
  }
  return localState;
}
```

加上 `TargetMap::addStorageTarget`（`:86`，開盤即 ONLINE）與 `TargetMap::syncReceiveDone`（`:120`，resync 完成即 UPTODATE），完整循環是：

```
  storage 端                                mgmtd 端
  ─────────                                ─────────
  開盤 → ONLINE      ──心跳(ONLINE)──►      OFFLINE→WAITING（有 SERVING 且無 SYNCING 則 →SYNCING）
                     ◄──路由(SYNCING)──
  收到 SYNCING，開始從上游拉資料
  syncReceiveDone → UPTODATE
                    ──心跳(UPTODATE)──►     SYNCING + UPTODATE → SERVING
                     ◄──路由(SERVING)──
  updateLocalState(ONLINE, SERVING)
    → UPTODATE（確認）                       穩態
  ─────────────────────────────────────────────────────────────
  磁碟錯誤 → OFFLINE ──心跳(OFFLINE)──►      SERVING + OFFLINE → LASTSRV 或 OFFLINE
  節點崩潰（無心跳） ───────X───────►        checkHeartbeat 逾時 → 強制 localState=OFFLINE
                     ◄──路由(LASTSRV)──
  updateLocalState(UPTODATE, LASTSRV)
    → OFFLINE（自我下線）
```

**沒有任何一端可以單獨推進狀態。** 這消滅了整類「mgmtd 認為 target 在服務、storage 認為自己已下線」的分歧。而 mgmtd 因為心跳逾時而強制設定的 `localState = OFFLINE`（`MgmtdHeartbeatChecker.cc:67-70`）是唯一的例外——這是必要的，否則節點崩潰後狀態機永遠卡住。

---

## 8. 設定管理與熱更新

### 8.1 三層設定

| 層 | 檔案 | 何時讀 | 可熱更新 |
|---|---|---|---|
| Launcher config | `configs/mgmtd_main_launcher.toml` | 進程啟動，本機檔案 | 否（要重啟） |
| App config | `configs/mgmtd_main_app.toml` | 進程啟動，本機檔案 | 否 |
| Server config | **FoundationDB `CONF` 表** | 啟動時拉一次，之後靠 SetConfig 推 | 是 |

launcher config 只有連 FDB 所需的最小資訊：`cluster_id`、`kv_engine.fdb.clusterFile`、`ib_devices`、`allow_dev_version`。`MgmtdLauncherConfig::init`（`MgmtdLauncherConfig.cc:7-14`）額外做一件事：

```cpp
auto rv = flat::ReleaseVersion::fromVersionInfo();
if (!allow_dev_version() && !rv.getIsReleaseVersion()) XLOGF(FATAL, "Dev version is not allowed: {}", rv);
```

生產環境把 `allow_dev_version` 設 false 就能防止有人不小心把開發版二進位檔部署上去。

`use_memkv` 與 `fdb` 兩個欄位在 `MgmtdLauncherConfig.h:9-10` 已標記 deprecated（被 `kv_engine` 取代），但仍保留在 struct 裡——因為 serde 是 positional 編碼、`ConfigBase` 也依賴欄位順序，刪除會破壞既有設定檔的相容性。

### 8.2 `CONF` key 的巧妙編碼

`MgmtdStore.cc:85-95`：

```cpp
String getConfigKey(flat::NodeType nodeType, flat::ConfigVersion version) {
  Serializer s(buf);
  s.put(kv::KeyPrefix::Config);
  s.putShortString(toStringView(nodeType));
  // let the latest version be the first
  auto reversedBigVer = folly::Endian::big64(std::numeric_limits<uint64_t>::max() - version.toUnderType());
  s.put(reversedBigVer);
  return buf;
}
```

兩次變換：先 `max - version`（反轉排序方向），再 `big64`（讓數值大小等於位元組序）。結果是**版本號越大，key 越小**。於是 `loadLatestConfig` 只需一次 `listByPrefix(prefix, limit=1)`（`MgmtdStore.cc:306-309`）就能拿到最新版，不需要掃全部版本再排序。

用 `NodeType` 的**字串名**而非數值當 key 的一部分（`putShortString(toStringView(nodeType))`），代價是 key 變長，好處是 FDB 裡的 key 人眼可讀、且 enum 值重排不會弄丟資料。`decodeConfigKey`（`:97-117`）用 `magic_enum::enum_cast` 反解，遇到不認得的字串回 `kDataCorruption` 而非靜默忽略。

`unpackConfigInfo`（`:129-151`）在解出 value 後**再驗證一次 key 裡的 version 與 value 裡的 configVersion 相同**——這種「key 與 value 冗餘欄位交叉驗證」在 `loadAllNodes`（`:271-276`）也用了一次。防的是編碼 bug 與 KV 層資料損壞。

### 8.3 熱更新的推送路徑：拉而非推

`setConfig` **只寫 FDB 和記憶體 configMap，不主動通知任何節點**（`SetConfigOperation.cc:32-38`）。新設定靠三條被動管道擴散：

```
                      ┌──────────────────────────────────────────┐
                      │  admin_cli set-config --node-type STORAGE│
                      └──────────────────┬───────────────────────┘
                                         │ SetConfig RPC
                      ┌──────────────────▼───────────────────────┐
                      │ mgmtd primary                            │
                      │  ① store_.storeConfig(txn, type, info)   │ ← CONF 表
                      │  ② configMap[type][newVersion] = info    │ ← 記憶體
                      │  ③ 若 type == MGMTD: 立即自我套用          │
                      └──────┬────────────────────┬──────────────┘
                             │                    │
       Heartbeat（10s）       │                    │  ExtendClientSession（10s）
       HeartbeatRsp.config   │                    │  ExtendClientSessionRsp.config
                             ▼                    ▼
                   meta / storage / mgmtd(備)   client / fuse
                             │                    │
                   updateSelfConfig(...)     套用並更新 configVersion
```

server 端的套用邏輯 `updateSelfConfig`（`helpers.cc:23-37`）：

```cpp
Result<Void> updateSelfConfig(MgmtdState &state, const flat::ConfigInfo &cfg) {
  XLOGF_IF(FATAL, state.selfNodeInfo_.configVersion > cfg.configVersion, "...");  // 版本不可回退
  if (state.selfNodeInfo_.configVersion >= cfg.configVersion) return Void{};      // 已是最新
  const auto &updater = state.env_->configUpdater();
  if (!updater) { XLOGF(WARN, "Mgmtd: blindly promote to {} since no listener found", ...); return Void{}; }
  return updater(cfg.content, cfg.genUpdateDesc());
}
```

第一行的 `FATAL` 很嚴厲：**收到比自己更舊的設定版本 = 直接崩潰**。這只可能發生在 mgmtd 換屆且新 primary 的 CONF 表落後的情況，屬於嚴重的一致性破壞，快速失敗是對的。

`configVersion` 的推進**只在套用成功後才發生**（`MgmtdHeartbeater.cc:37-45`、`SetConfigOperation.cc:41-54`）。若 `updater` 回錯（設定內容非法），版本不推進，下次心跳會再試一次。這讓「設定推送失敗」自然變成可重試的狀態，而 `configStatus` 欄位（`ConfigStatus::NORMAL / STALE / UNKNOWN / FAILED`）讓運維能透過 `MgmtdService.ConfigStatusCount` 指標看到有多少節點卡在舊版本。

`checkConfigVersion`（`MgmtdData.cc:9-28`）拒絕比 server 更新的版本：

```cpp
if (version > vit->first)
  RETURN_AND_LOG_OP_ERR(ctx, MgmtdCode::kInvalidConfigVersion,
                        "ConfigVersion {} of {} is newer than server's {}", ...);
```

這偵測的是**節點連到了一台落後的 mgmtd**（例如換屆後新 primary 的 CONF 資料還沒載完）。與其讓節點降級設定，不如報錯讓它重試。

`getConfig` 的 `exactVersion` 旗標（`GetConfigOperation.cc:13`）決定語意：`false`（預設）= 「給我比 X 更新的版本，沒有就回 nullopt」，`true` = 「給我剛好 X 版」。前者是心跳/session 的增量拉取語意，後者是運維查歷史用的。

### 8.4 `MgmtdConfig` 全表

`src/mgmtd/service/MgmtdConfig.h`，**全部 27 個純量項都是 `CONFIG_HOT_UPDATED_ITEM`** —— mgmtd 的每一個行為參數都能在不重啟的情況下調整。

| 參數 | 預設（程式碼） | configs/ 內 | 影響 |
|---|---|---|---|
| `lease_length` | 60s | 1min | lease 有效期 |
| `extend_lease_interval` | 10s | 10s | 續約頻率 |
| `suspicious_lease_interval` | 20s | 20s | 提早退位的安全邊界 |
| `heartbeat_timestamp_valid_window` | 30s | 30s | 時鐘偏移容忍度，0=關閉 |
| `allow_heartbeat_from_unregistered` | **false** | **true** | 自動註冊未知節點 |
| `check_status_interval` | 10s | 10s | 三個 checker 的週期 |
| `heartbeat_fail_interval` | 60s | 1min | 心跳逾時判定 |
| `new_chain_bootstrap_interval` | 2min | 2min | 新 chain 靜默期 |
| `send_heartbeat` / `_interval` | true / 10s | true / 10s | 備援 mgmtd 報到 |
| `client_session_timeout` | 20min | 20min | client session 逾時 |
| `bootstrapping_length` | 2min | 2min | 接管後的「路由可能不完整」提示期 |
| `update_chains_interval` | 1s | 1s | 狀態機執行頻率 |
| `validate_lease_on_write` | true | true | **腦裂防護開關（不應關閉）** |
| `bump_routing_info_version_interval` | 5s | 5s | 版本推進批次窗口 |
| `heartbeat_ignore_unknown_targets` | false（deprecated） | false | — |
| `heartbeat_ignore_stale_targets` | true | true | chainVersion 柵欄 |
| `retry_times_on_txn_errors` | -1（無限） | -1 | 外層交易重試 |
| `update_metrics_interval` | 1s | 1s | metric 匯出頻率 |
| `target_info_persist_interval` / `_batch` | 1s / 1000 | 1s / 1000 | TGIF 寫入 |
| `target_info_load_interval` | 1s | 1s | TGIF 載入輪詢 |
| `try_adjust_target_order_as_preferred` | false | false | 自動照 preferred 順序重排 |
| `extend_lease_check_release_version` | true | true | 防降級 |
| `authenticate` | false | false | admin 權限檢查 |
| `enable_routinginfo_cache` | true | true | RoutingInfo 序列化快取 |
| `only_accept_client_uuid` | false | false | 強制 clientId 為 UUID |

**`allow_heartbeat_from_unregistered` 在程式碼與 configs 裡不一致**（false vs true）。程式碼的預設是安全的（未註冊節點無法混進叢集），configs/ 的範例值是方便部署的（不需要先跑 register-node）。生產環境應該用程式碼的預設值並顯式註冊每個節點。

---

## 9. 持久化：FDB key 佈局與交易邊界

### 9.1 完整 key 空間

mgmtd 使用 `src/common/kv/KeyPrefix-def.h` 定義的 6 個前綴：

```
┌─────────────────────────────────────────────────────────────────────────────────┐
│ SING  單鍵區（Single）                                                            │
│   "SINGMgmtdLease"          → serde(MgmtdLeaseInfo{primary, leaseStart,          │
│                                     leaseEnd, releaseVersion})                   │
│        ↑ 整個叢集的選主錨點，唯一一個有寫入衝突的熱點 key                             │
│   "SINGRoutingInfoVersion"  → Serializer(uint64)                                 │
│        ↑ 每次路由變更都寫，第二熱的 key                                             │
├─────────────────────────────────────────────────────────────────────────────────┤
│ NODE  節點表                                                                      │
│   key   = "NODE" + Serializer(NodeId u32)          （原生位元組序）                 │
│   value = serde(PersistentNodeInfo{nodeId, type, serviceGroups, tags, hostname}) │
│        ↑ 注意：status / lastHeartbeatTs / configVersion 不落盤（是執行期狀態）        │
├─────────────────────────────────────────────────────────────────────────────────┤
│ CHIT  chain table（多版本，append-only）                                           │
│   key   = "CHIT" + ChainTableId(u32) + ChainTableVersion(u32)                    │
│   value = serde(ChainTable{id, version, chains[], desc})                         │
├─────────────────────────────────────────────────────────────────────────────────┤
│ CHIF  chain（單版本，就地覆蓋）                                                     │
│   key   = "CHIF" + ChainId(u32)                                                  │
│   value = serde(ChainInfo{chainId, chainVersion, targets[], preferredTargetOrder})│
│        ↑ target 的 publicState 存在這裡（targets[i].publicState）                  │
├─────────────────────────────────────────────────────────────────────────────────┤
│ TGIF  target 位置（僅為運維查詢）                                                   │
│   key   = "TGIF" + Endian::big64(TargetId)         ← 大端！                       │
│   value = serde(TargetInfo{targetId, publicState, localState, chainId,           │
│                            nodeId, diskIndex, usedSize})                         │
├─────────────────────────────────────────────────────────────────────────────────┤
│ CONF  設定（多版本，倒序）                                                          │
│   key   = "CONF" + ShortString(NodeType名) + big64(MAX - ConfigVersion)          │
│   value = serde(ConfigInfo{configVersion, content, desc})                        │
├─────────────────────────────────────────────────────────────────────────────────┤
│ UTGS  universal tags                                                             │
│   key   = "UTGS" + universalId（原始字串，無長度前綴）                               │
│   value = serde(vector<TagPair>)                                                 │
└─────────────────────────────────────────────────────────────────────────────────┘
```

### 9.2 `TGIF` 用 big-endian：與 meta 層相反的取捨

```cpp
String getTargetInfoKey(flat::TargetId id) {
  auto reversedId = folly::Endian::big64(id.toUnderType());   // MgmtdStore.cc:52
  ...
}
```

變數名叫 `reversedId`（在 little-endian 機器上，big64 就是位元組反轉）。目的在 `loadTargetsFrom`（`MgmtdStore.cc:477-492`）：

```cpp
auto beginKey = getTargetInfoKey(tid);
auto endKey = getTargetInfoKey(flat::TargetId(-1));
auto res = co_await txn.snapshotGetRange(beginSel, endSel, 0);
```

big-endian 讓「key 的位元組序」等於「targetId 的數值序」，於是 `loadTargetInfo` 可以從任意 targetId 開始做**有序分批掃描**，用 `startTid = res->back().targetId + 1` 推進游標（`MgmtdTargetInfoLoader.cc:46`）。

這與 meta 層的 `InodeId` 刻意用 little-endian 打散熱點正好相反（見《3FS 元資料層深度剖析》§3.1）。同一個 codebase 裡兩個相反的選擇，理由完全清楚：**inode 是每秒數萬次的建檔寫入，怕熱點；target 是最多數萬個、每秒最多寫 1000 個的低頻表，只怕沒法有序掃描。**

`loadTargetsFrom` 用的是 `snapshotGetRange`（不進讀衝突集）——因為它只是唯讀的補資料，不需要在提交時保證一致性。

### 9.3 版本單調性：三個機制的疊加

`RoutingInfoVersion` 必須嚴格單調遞增，否則 client 的 `updateRoutingInfo` 會直接 `XLOGF(FATAL, "RoutingInfoVersion rollback from {} to {}")`（`src/client/mgmtd/MgmtdClient.cc:624-628`）。三個機制共同保證：

**機制一：先寫 FDB，後改記憶體。** `updateStoredRoutingInfo`（`helpers.h:72-81`）：

```cpp
template <typename Handler>
inline CoTryTask<void> updateStoredRoutingInfo(MgmtdState &state, core::ServiceOperation &ctx, Handler &&handler) {
  auto dataPtr = co_await state.data_.coSharedLock();
  auto nextv = nextVersion(dataPtr->routingInfo.routingInfoVersion);   // ← 交易外算好
  co_return co_await withReadWriteTxn(state, [&](kv::IReadWriteTransaction &txn) -> CoTryTask<void> {
    CO_RETURN_ON_ERROR(co_await state.store_.storeRoutingInfoVersion(txn, nextv));   // ← 交易第一件事
    LOG_OP_INFO(ctx, "RoutingInfo: bump storage version to {}", nextv);
    co_return co_await handler(txn);
  });
}
```

而記憶體版本在交易成功**之後**才推進（`helpers.cc:77-82`）：

```cpp
void updateMemoryRoutingInfo(RoutingInfo &alreadyLockedRoutingInfo, core::ServiceOperation &ctx) {
  ++ri.routingInfoVersion.toUnderType();
  ri.routingInfoChanged = false;
}
```

不變式：**FDB 版本 ≥ 記憶體版本，永遠成立。** 交易失敗時 FDB 沒動、記憶體也沒動；交易成功但進程崩潰時 FDB 領先記憶體一格。

**機制二：接管時 `nextVersion(loaded)`。** `MgmtdLeaseExtender.cc:20-29`：

```cpp
CoTryTask<void> loadRoutingInfoVersion(ctx, store, txn, flat::RoutingInfoVersion &newVersion) {
  auto res = co_await store.loadRoutingInfoVersion(txn);
  newVersion = nextVersion(*res);        // ← 讀出來 +1
}
```

新 primary 從 FDB 讀到 V，記憶體從 V+1 開始。由於機制一保證 FDB 版本 ≥ 任何曾經送給 client 的記憶體版本，所以 V+1 > 任何已發出的版本。**跨主從切換的單調性成立。**

**機制三：`nextv` 在交易外計算。** 這讓 FDB 交易重試（`retryMaybeCommitted = true`，`MgmtdState.cc:89`）是冪等的——重試寫入同一個 `nextv`，而不是每次重讀再 +1。若在交易內讀 `loadRoutingInfoVersion` 再 +1，一次「可能已提交」的重試就會把版本推兩格（無害但浪費），更糟的是 `handler` 內的其他寫入也會被重複執行。

`shutdownAllChains`（`MgmtdStore.cc:439-459`）是唯一在交易內讀版本再 +1 的地方——因為它是 `admin_cli shutdown-all-chains` 的離線工具（`src/client/cli/admin/ShutdownAllChains.cc:48-50`），**在 mgmtd 服務之外直接改 FDB**，此時沒有記憶體狀態要對齊。

### 9.4 交易邊界一覽

| 操作 | 交易內容 | 是否驗證 lease |
|---|---|---|
| `extendLease` | 讀 NODE(self) + 讀 SING lease + 寫 SING lease | **否**（`expectSelfPrimary=false`） |
| `onNewLease` | 併發讀 SING+NODE+CONF+CHIT+CHIF+UTGS，寫 SING version + NODE(self) | 是 |
| 一般路由變更 | 寫 SING version + 對應表 | 是 |
| `setConfig` | 只寫 CONF（**不 bump RoutingInfoVersion**） | 是 |
| `setPreferredTargetOrder` | 只寫 CHIF（**不 bump**） | 是 |
| `persistTargetInfo` | 批次寫 TGIF（≤1000，**不 bump**） | 是 |
| `loadTargetInfo` | 唯讀 snapshotGetRange TGIF，分批多交易 | 否（唯讀） |
| `getPrimaryMgmtd` | 唯讀 SING lease（僅快取 miss 時） | 否 |

`extendLease` 是唯一一個 `expectSelfPrimary=false` 的寫入（`MgmtdLeaseExtender.cc:212`）——這是必然的，你不能要求「先是 primary 才能成為 primary」。

### 9.5 雙層重試

```
┌─ withReadWriteTxn 的外層 for 迴圈（helpers.h:48-61）───────────────┐
│   retry_times_on_txn_errors = -1（無限），固定 1000ms 間隔          │
│   只重試 StatusCodeType::Transaction 類的錯誤                       │
│   每次重試前 XLOGF(CRITICAL) 記錄                                   │
│   ┌─ kv::WithTransaction + FDBRetryStrategy ──────────────────┐   │
│   │   max_retry_count = 10（configs/），指數退避從 10ms 到 1s     │   │
│   │   retryMaybeCommitted = true                              │   │
│   │   ┌─ FDB 交易 ────────────────────────────────────────┐   │   │
│   │   │  ensureLeaseValid(txn) → handler(txn) → commit    │   │   │
│   │   └──────────────────────────────────────────────────┘   │   │
│   └──────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

`retryMaybeCommitted = true` 意味著 FDB 回報 `commit_unknown_result`（提交結果未知）時仍然重試。這**要求所有寫入必須冪等**——mgmtd 的所有寫入都是「用固定 key 寫固定 value」的 blind put，天然冪等；再加上 §9.3 機制三保證 `nextv` 不會在重試中改變，這個假設成立。

外層無限重試 + 1 秒固定間隔的組合意味著：FDB 長時間不可用時，寫請求會無限期掛住而非快速失敗。搭配 lease 機制，這台 mgmtd 最終會因為續約失敗而自認退位（`currentLease` 回 nullopt），新請求被 `doAsPrimary` 擋在門外，只有已進入重試迴圈的舊請求還在掛著。這些舊請求即使 FDB 恢復後想提交，也會被 `ensureLeaseValid` 擋下。**沒有正確性問題，但有資源洩漏風險**（見 §12）。

---

## 10. 併發、錯誤處理與可觀測性

### 10.1 併發模型

```
RPC 執行緒（proc thread pool，per group）
   │  enable_coroutines_pool=true, max_coroutines_num=256
   │  max_processing_requests_num=4096
   ▼
MgmtdService::xxx → MgmtdOperator::xxx → XxxOperation::handle(state)
   │
   ├─ 【讀操作】 data_.coSharedLock()  ────────────────┐
   │                                                  │  folly::coro::SharedMutex
   ├─ 【寫操作】 writerMu_.co_scoped_lock()            │  （讀者可併發）
   │     │  ← folly::coro::Mutex，全域序列化所有寫者     │
   │     ├─ data_.coSharedLock()   讀+驗證  ───────────┤
   │     ├─ (FDB 交易，不持 data_ 鎖)                   │
   │     └─ data_.coLock()         寫  ────────────────┘
   │
背景執行緒（bgThreadPool，CPUExecutorGroup）
   └─ 10 條獨立協程迴圈，各自 pickNext() 選一個執行緒
```

**三把鎖的分工：**

| 鎖 | 型別 | 保護什麼 | 誰拿 |
|---|---|---|---|
| `writerMu_` | `folly::coro::Mutex` | **時間區間**：一次 read-modify-write 的完整過程（跨 FDB 交易） | 所有寫 operation + 4 個背景任務 |
| `data_` 內的 SharedMutex | `folly::coro::SharedMutex` | **記憶體**：`MgmtdData` 的單次存取 | 所有人 |
| `clientSessionMap_` 內的 SharedMutex | 同上 | client session map | session 相關操作 |

讀操作**不拿 `writerMu_`**，這是關鍵：`getRoutingInfo` 是 QPS 最高的方法（每個 client 每 10 秒一次 × 數千 client），它只需要 `data_` 的共享鎖，不會被進行中的寫操作（可能在等 FDB）阻塞。

`clientSessionMap_` 與 `data_` 是**兩把獨立的鎖**。`extendClientSession` 是全叢集第二高頻的寫入（每個 client 每 10s 一次），如果它要拿 `writerMu_` 或 `data_` 的獨佔鎖，會嚴重干擾路由更新。拆開後，session 更新只碰 `clientSessionMap_`（`ExtendClientSessionOperation.cc:43`），對 `data_` 只取共享鎖讀 config 與 tags（`:34`）。**這是整個 mgmtd 最重要的併發優化。**

需要同時拿兩把鎖的地方（`GetClientSessionOperation.cc:16-17`、`ListClientSessionsOperation.cc:12-13`、`MgmtdMetricsUpdater.cc:45-46`）**加鎖順序一律是 `data_` → `clientSessionMap_`**，一致的順序避免死鎖。

### 10.2 Metrics

`ServiceOperationWithMetric<ServiceName, OpName, MetricNamePrefix>`（`src/core/utils/ServiceOperation.h:72-99`）讓每個 operation 自帶指標。`MetricNamePrefix` 區分前台與背景：

- RPC operation → `"op"` → `MgmtdService.op{instance=GetRoutingInfo}`
- 背景任務 → `"bg"` → `MgmtdService.bg{instance=UpdateChains}`

`SimpleOperationRecorder` 帶 `recordErrorCode=true`，`CO_INVOKE_OP` 巨集（`runOp.h:5-17`）自動記錄成功/失敗與錯誤碼，並在失敗時 log 完整錯誤 + 延遲。

`MgmtdMetricsUpdater` 每秒匯出的 11 組指標：

| 指標 | 標籤 | 用途 |
|---|---|---|
| `MgmtdService.NodeStatusCount` | instance=NodeType, tag=NodeStatus | 各類節點的狀態分佈 |
| `MgmtdService.NodeStatus` | instance=`{type}_{nodeId}` | **單一節點**的狀態（+1 因為 PRIMARY_MGMTD=0） |
| `MgmtdService.ConfigStatusCount` | instance=NodeType, tag=ConfigStatus | 設定推送進度（含 client session） |
| `MgmtdService.ChainCountByReplica` | instance=副本數 | chain 的副本數分佈 |
| `MgmtdService.ChainCount` / `TargetCount` | – | 總量 |
| `MgmtdService.TargetStatusCount` | instance=PublicState, tag=LocalState | **狀態機的二維分佈** |
| `MgmtdService.AbnormalChainStatus` | instance=chainId, tag=targetId | 非全 SERVING 的 chain 明細 |
| `MgmtdService.RoutingInfoVersion` | – | 版本號（用來看變更頻率） |
| `MgmtdService.ConfigVersions` | instance=NodeType | 各類型最新設定版本 |
| `MgmtdService.LeaseLasting` | – | **本任期已持續多久**（換屆偵測） |
| `MgmtdService.ReleaseVersionCount` | instance=NodeType/"SERVER", tag=版本 | 滾動升級進度 |
| `MgmtdService.ChainStatus` | instance=副本數, tag=SERVING-FULL/PARTIAL/SYNCING/UNAVAILABLE | **叢集健康度總覽** |
| `MgmtdService.OrphanTargetCount` | instance=nodeId | 孤兒 target |

加上 `MgmtdService.WriterLatency` / `WriterUsage`（`MgmtdState.cc:19-32`），可以看到每個寫操作持有全域鎖的時間——這是診斷 mgmtd 卡頓的第一手線索。

`reportTargetCount` 的 `abnormal` 判定帶了 `!bootstrapping` 條件（`MgmtdMetricsUpdater.cc:146`）：接管後 2 分鐘內不報 abnormal chain，**避免每次換屆都觸發一大批誤報告警**。

`reportReleaseVersionStatus` 用了一個 `invalidType = 255` 的哨兵（`:189`）額外累計一份「所有 server 端節點」的總和（標籤顯示為 `"SERVER"`），讓運維一眼看出「還有幾台沒升級」而不需要在監控端做跨標籤加總。

### 10.3 錯誤碼

`src/common/utils/StatusCodeDetails.h:193-211`，`MgmtdCode` 佔用 5000-5018：

| 碼 | 名稱 | 語意 | client 的反應 |
|---|---|---|---|
| 5000 | `kNotPrimary` | 我不是 primary（訊息帶真 primary 的 nodeId） | 切換到訊息裡的節點 |
| 5001 | `kNodeNotFound` | | – |
| 5002 | `kHeartbeatFail` | 未註冊 / 型別不符 / DISABLED / 時鐘偏移 / target 重複 | 告警 |
| 5003 | `kClusterIdMismatch` | 連錯叢集 | 致命設定錯誤 |
| 5004 | `kRegisterFail` | nodeId=0 或重複 | – |
| 5005 | `kInvalidRoutingInfoVersion` | client 的版本比 server 新 | 重試/換 primary |
| 5006 | `kInvalidConfigVersion` | 同上 | 同上 |
| 5007 | `kInvalidChainTable` | chain 組成不合法 | – |
| 5008 | `kInvalidChainTableVersion` | | – |
| 5009 | `kHeartbeatVersionStale` | 訊息帶 server 端版本 | 跳到 v+1 重送 |
| 5010 | `kClientSessionVersionStale` | 訊息帶 server 端版本 | 同上 |
| 5011 | `kInvalidTag` | 空 key，或 REMOVE 模式帶了非空 value | – |
| 5012 | `kChainNotFound` | | – |
| 5013 | `kNodeTypeMismatch` | | – |
| 5014 | `kExtendClientSessionMismatch` | clientId 撞號 | 告警 |
| 5015 | `kClientSessionNotFound` | | – |
| 5016 | `kNotAdmin` | `authenticate=true` 且非 admin | – |
| 5017 | `kTargetNotFound` | | – |
| 5018 | `kTargetExisted` | `updateChain ADD` 時 target 已存在 | – |

搭配 client 端的 `MgmtdClientCode` 6000-6004（`kPrimaryMgmtdNotFound` / `kWorkQueueFull` / `kMetaServiceNotAvailable` / `kExit` / `kRoutingInfoNotReady`）。

### 10.4 三種嚴厲程度的斷言

mgmtd 大量使用 `XLOGF_IF(FATAL/DFATAL, ...)` 把不變式編碼進程式碼，三個層級各有明確用途：

| 巨集 | 行為 | 用在哪 |
|---|---|---|
| `XLOGF_IF(FATAL, ...)` | 永遠崩潰 | 資料結構不變式被破壞（`getChain` 找不到 chain、orphan 雙向索引不一致、insert 重複 target、版本回退） |
| `XLOGF_IF(DFATAL, ...)` | debug 崩潰，release 只 log ERROR | 「不該發生但不致命」（`updateTarget` 的 tid 不存在、`rotateAsPreferredOrder` 的參數長度不符） |
| `assert(...)` | debug only | 純內部檢查 |

FATAL 的使用密度很高（`RoutingInfo.cc` 一個檔案就有 11 處：`:29 :34 :91 :96 :105 :111 :113 :119 :133 :134 :139`）。設計哲學明確：**mgmtd 是單點 primary，帶著損壞的狀態繼續服務會污染整個叢集；直接崩潰讓備援接管、從 FDB 重新載入乾淨狀態，是更安全的選擇。** 這與 `BackgroundRunner` 對未捕捉例外的 FATAL 處理是同一個思路。

---

## 11. 與其他組件的互動邊界

```
                      ┌───────────────────────────────────────────────────────┐
                      │                     mgmtd primary                     │
                      └───┬──────────┬──────────┬──────────┬──────────┬──────┘
      getRoutingInfo(5)   │          │          │          │          │  registerNode(4)
      heartbeat(3)        │          │          │          │          │  setChains(15)
      ────────────────────┤          │          │          │          │  setChainTable(8)
                          │          │          │          │          │  setConfig(6)
   ┌──────────────────────▼──┐  ┌────▼─────┐ ┌──▼───────┐ ┌▼─────────┐ ├─ enable/disableNode(9,10)
   │ storage_main            │  │meta_main │ │hf3fs_fuse│ │mgmtd(備) │ ├─ unregisterNode(14)
   │                         │  │          │ │  client  │ │          │ ├─ rotateLastSrv(20)
   │ 上報：LocalTargetInfo[]  │  │上報：Meta │ │extendCli-│ │上報：    │ ├─ updateChain(24)
   │   {targetId,localState, │  │HeartbeatI│ │entSession│ │MgmtdHear-│ ├─ setNodeTags(13)
   │    diskIndex,usedSize,  │  │nfo{dummy}│ │  (11)    │ │tbeatInfo │ ├─ setUniversalTags(16)
   │    chainVersion,lowSpace│  │          │ │          │ │  {dummy} │ ├─ listOrphanTargets(21)
   │   }                     │  │          │ │          │ │          │ ├─ listClientSessions(12)
   │ 消費：publicState        │  │消費：chain│ │消費：完整 │ │消費：    │ │
   │   → 決定 head/tail/     │  │  表 → 建檔│ │  路由表   │ │  config  │ │
   │      SYNCING 觸發 resync│  │  時分配   │ │          │ │          │ │
   └─────────────────────────┘  └──────────┘ └──────────┘ └──────────┘ └─ admin_cli
```

### 11.1 storage 要什麼

- **`publicState`**：決定自己在 chain 中的角色。`TargetMap::updateRouting`（`src/storage/service/TargetMap.cc:170-215`）計算 `isHead = (targetIsServing && it == chain.targets.begin())`，其中 `targetIsServing` 涵蓋 SERVING 和 SYNCING（`:190-191`）——**SYNCING 的副本仍然接收轉發的寫入**，這是 CRAQ 邊 sync 邊服務的關鍵。
- **`chainVersion`**：作為 `VersionedChainId` 的一部分，每個寫入請求都帶著。版本不符直接回 `kRoutingVersionMismatch`（`TargetMap.cc:64`）。
- 收到「我曾是 LASTSRV，現在變成 SERVING/SYNCING/WAITING」時呼叫 `resetUncommitted(chainVer)`（`TargetMap.cc:274-279`）——丟棄 LASTSRV 期間累積的未提交寫入。

### 11.2 meta 要什麼

meta 的心跳 payload 是空的（`MetaHeartbeatInfo{dummy}`，`fbs/mgmtd/HeartbeatInfo.h:11-16`）——meta server 完全無狀態，mgmtd 只需要知道它活著。

meta 消費的是 **chain table**：建檔時把 `ChainTableId` + `ChainTableVersion` 寫進 `Layout`，之後檔案的 chunk→chain 映射就永久綁定該版本。這是 ChainTable 必須 append-only 的直接原因。

### 11.3 client 的 failover 邏輯

`src/client/mgmtd/MgmtdClient.cc` 的 `trySwitchProbeTarget`（`:139-168`）：

```cpp
if (isNetworkError(code) || code == MgmtdCode::kNotPrimary) {
  if (code == MgmtdCode::kNotPrimary) {
    probeContext.probedAddrs.insert(conn.addrs.begin(), conn.addrs.end());   // 整個節點標記已探測
    uint32_t id = 0;
    if (scn::scan(String(error.message()), "{}", id) && mgmtds_.contains(flat::NodeId(id)))
      co_return flat::NodeId(id);          // ← 直接跳到訊息裡指名的 primary
    co_return makeError(kSkipThisNode);
  } else {
    probeContext.probedAddrs.insert(conn.addr());
    conn.switchAddr();                      // 網路錯誤 → 換同一節點的下一個位址
    ...
  }
}
```

兩種錯誤兩種策略：**網路錯誤換位址（同一節點多網卡），`kNotPrimary` 換節點**。`probePrimary` 是 DFS，深度上限硬編碼為 3（`:501-504`，有 TODO 註記），避免 mgmtd 之間互相指來指去造成無限追蹤。

`tryAddMgmtd`（`:215`）讓 client 從 RoutingInfo 的 `nodes` 裡持續學習新的 mgmtd 位址——設定檔裡只要寫一個位址就能發現全部，這也是為什麼備援 mgmtd 必須發心跳。

---

## 12. 設計取捨與潛在坑

### 12.1 取捨總表

| 決策 | 得到什麼 | 付出什麼 |
|---|---|---|
| 選主用 FDB 單鍵 + 交易，不用 Raft | 零額外協定、零額外進程、程式碼 ~35 行 | 完全依賴 FDB 可用性；lease 換屆有數十秒空窗 |
| `suspicious_lease_interval` 提早退位 | 舊 primary 退位嚴格早於新 primary 上任 | 主動製造 20 秒服務空窗 |
| 每筆寫交易重讀 lease | 腦裂在交易層被消滅，時鐘漂移不影響正確性 | 每次寫入多一次 FDB 讀 |
| 全狀態載入記憶體 | `getRoutingInfo` 零 IO，可支撐數千 client 輪詢 | 接管時要全表掃描；記憶體隨叢集規模線性成長 |
| 先寫 FDB 版本、後改記憶體 | 版本跨換屆單調 | 崩潰時 FDB 版本會有空洞（無害） |
| `writerMu_` 全域寫鎖 | 邏輯極簡，所有寫入天然序列化 | **寫入吞吐上限 ≈ 1/FDB 交易延遲**——量級由交易往返決定，程式碼裡沒有 benchmark 可佐證具體數字 |
| session map 獨立鎖 | 高頻 session 續約不干擾路由 | 兩把鎖需維持一致的加鎖順序 |
| session 不落盤 | 零寫入成本 | 換屆後全部 session 資訊丟失 20 秒 |
| `newBornChains` 靜默期 2 分鐘 | 避免換屆後全叢集誤降級 | 新建 chain 要 2 分鐘才會被狀態機推進 |
| chains updater 用時間戳增量掃描 | 不需要 dirty queue；失敗自動重掃 | 每輪仍要遍歷全部 target（O(N) per second） |
| `bumpRoutingInfoVersion` 延遲 5 秒 | 節點狀態抖動被批次合併 | 節點上下線的路由通知延遲 5 秒 |
| `generateNewChain` 純函式 | 可用表格窮舉測試 | 每次都要重建整個 vector |
| 「全 SERVING 掉線只第一個轉 LASTSRV」 | 資料安全（只信 head） | 可用性降低，需要人工 `rotateLastSrv` 介入 |
| 「同時只允許一個 SYNCING」 | resync 不壓垮線上流量 | 多副本同時故障時恢復串行化 |
| publicState/localState 雙向握手 | 消除兩端狀態分歧 | 每次狀態推進至少一個心跳週期（10s） |
| `TGIF` 只存位置不存狀態 | 表小、寫入少 | 需要 loader/persister 兩個背景任務維護 |
| `TGIF` 用 big-endian | 可有序分批掃描 | 無（低頻表，不怕熱點） |
| `CONF` key 用 `MAX - version` | 取最新版 O(1) | key 編碼不直觀 |
| ChainTable append-only | 老檔案的 layout 永遠可解析 | 版本數無上限成長 |
| 設定用「拉」不用「推」 | mgmtd 不需維護與每個節點的長連線 | 設定生效延遲 = 心跳週期（10s） |
| 錯誤訊息當回饋通道 | 省下三個結構化 payload | 依賴字串解析，格式改變會靜默失效 |
| 大量 `XLOGF(FATAL)` | 不變式違反時立即暴露 | 一個邊界情況沒想到就是一次 primary 崩潰 |
| X-macro 展開三層 | 新增 RPC 只改 2 處 | 編譯錯誤難讀、IDE 跳轉失效 |

### 12.2 具體的坑

**(1) `SetChainTable` 的版本覆蓋 bug（已於 commit 22fca04 修復）**

`git show 22fca04` 顯示的舊碼：

```cpp
if (newChainTable.chains != current.chains) {
  newChainTable.chainTableVersion = nextVersion(current.chainTableVersion);
}
if (newChainTable.desc.empty()) { newChainTable.desc = current.desc; }
if (newChainTable == current) { co_return SetChainTableRsp::create(current.chainTableVersion); }
```

若只改 `desc` 不改 `chains`：版本**不**遞增（第一個 if 不成立），但 `newChainTable == current` 也不成立（desc 不同），於是繼續往下落盤。

關鍵在於被寫壞的是**哪一版**。`newChainTable` 在建構時就被硬編碼成版本 1（`src/mgmtd/ops/SetChainTableOperation.cc:39-41`）：

```cpp
auto tableVersion = flat::ChainTableVersion(1);
auto newChainTable = flat::ChainTable::create(tableId, tableVersion, std::move(req.chains), std::move(req.desc));
```

舊碼在「chains 未變」時不會執行 `newChainTable.chainTableVersion = nextVersion(...)`，於是這個 `1` 一路帶到 `storeChainTable`，記憶體端的 `ri.chainTables[tableId][newChainTable.chainTableVersion] = newChainTable`（`:82`）也一併中招。**結果是把 chain table 的第 1 版就地改寫成新內容**，而不是覆蓋當前最新版。對於 meta 層那些記著 `tableVersion = 1` 的老檔案，這是靜默的語意漂移。

修復後（`SetChainTableOperation.cc:56-64`）：

```cpp
if (newChainTable.desc.empty()) { newChainTable.desc = current.desc; }
if (newChainTable.chains != current.chains || newChainTable.desc != current.desc) {
  newChainTable.chainTableVersion = nextVersion(current.chainTableVersion);
} else {
  co_return SetChainTableRsp::create(current.chainTableVersion);   // 完全相同才跳過
}
```

順序也改了：先補 desc 預設值，再比較。這個 bug 說明**「就地覆蓋的多版本表」是危險的模式**——只要有任何一條路徑忘記推版本，就會靜默損壞歷史。

**(2) `MgmtdMetricsUpdater` 的 recorder map 只增不減**

`recordCount`（`MgmtdMetricsUpdater.cc:14`）用 function-local `static std::map<Key, ValueRecorder> recorders`。對於以 `nodeId` / `(chainId, targetId)` 為 key 的指標（`MgmtdService.NodeStatus`、`MgmtdService.AbnormalChainStatus`），節點下線或 chain 刪除後 recorder 不會被移除：
- 舊 recorder 繼續匯出**最後一次的值**，監控上看起來像是「這個節點還在」
- 長期運行下 map 只增不減

`AbnormalChainStatus` 尤其危險：一條 chain 曾經異常過就永久佔一個 recorder，恢復後值不再更新但條目還在。

**(3) 外層無限重試可能累積掛起的協程**

`retry_times_on_txn_errors = -1` + 固定 1 秒間隔（`helpers.h:45-61`），在 FDB 長時間不可用時，每個寫請求會無限期佔用一個協程和一份 `writerMu_`（實際上第一個請求持鎖，後續全部排隊在鎖上）。雖然 lease 過期後 `doAsPrimary` 會擋下新請求，但已進入迴圈的請求不會自行退出，也不響應 `BackgroundRunner` 的取消（那個取消只作用於背景任務的迴圈邊界，不作用於進行中的 FDB 重試）。正確性上安全（提交時會被 `ensureLeaseValid` 擋下），但**資源會一直佔著直到 FDB 恢復**。

**(4) `MgmtdHeartbeater` 不做位址輪替**

`MgmtdHeartbeater.cc:79-87` 只用 `addrs[0]`，程式碼裡有 TODO：「consider reuse some facilities of MgmtdClient for auto switching addresses」。若 primary 的第一個位址的網卡壞了但進程還活著，備援 mgmtd 就永遠報到失敗 → 不出現在 RoutingInfo 裡 → client 學不到它的位址 → primary 真正掛掉時 client 可能找不到備援。

**(5) `allow_heartbeat_from_unregistered` 的預設值不一致**

程式碼 `MgmtdConfig.h:14` 是 `false`，`configs/mgmtd_main.toml:118` 是 `true`。用 configs/ 部署等於**任何能連上 mgmtd 的進程都能自動註冊進叢集**。生產環境應顯式設為 false 並用 `admin_cli register-node` 預先註冊。

**(6) 寫入吞吐的硬上限**

`writerMu_` 序列化所有寫操作（`src/mgmtd/service/MgmtdState.h:66-68` 的註解：「logical lock for protecting the whole processing of a writer operation」），每個寫操作至少一次 FDB 交易往返。寫入吞吐因此被壓在「1 / 單次交易延遲」這個量級——**具體數字取決於部署環境的 FDB 延遲，本報告不做估算**。在 target 大量抖動的場景（例如一批磁碟同時故障），`updateChains` 每輪要落盤數千條 chain，可能單輪就跑滿 1 秒的週期。`MgmtdService.WriterLatency` 是觀測此情況的指標。

**(7) `orphanTargetsByNodeId[nodeId]` 的隱式插入**

`RoutingInfo.cc:43` 的 `auto &orphans = orphanTargetsByNodeId[nodeId];` 對每次 storage 心跳都會插入一個空 entry（如果不存在）。雖然 `eraseOrphanTarget` 會在集合空時 erase（`:141`），但 `localUpdateTargets` 自己不清理——所以每台 storage 節點在 map 裡永久佔一個空 set。數量級與節點數相同，可忽略，但屬於「行為與意圖不符」的小瑕疵。

---

## 13. 檔案索引

### `src/mgmtd/` 根目錄

| 檔案 | 職責 |
|---|---|
| `CMakeLists.txt` | 定義 `mgmtd` 函式庫（依賴 core-app / core-user / core-service / fdb / mgmtd-fbs / memory-common）與 `mgmtd_main` 執行檔 |
| `mgmtd.cpp` | `main()`，8 行，把一切交給 `TwoPhaseApplication<MgmtdServer>` |
| `mgmtd.toml` | 內建的最小 log 設定範本（實際部署用 `configs/mgmtd_main.toml`） |
| `MgmtdServer.h` | `net::Server` 子類宣告；定義兩個 server group（8000/Mgmtd、9000/TCP/Core）與 `Config{base, service}` |
| `MgmtdServer.cc` | `beforeStart()` 組裝 `ServerEnv` + `MgmtdOperator` + 註冊兩個 serde service + 啟動背景任務 |
| `MgmtdConfigFetcher.h` | 兩階段啟動的第二階段：從 FDB 讀設定範本與自身 tags 的介面 |
| `MgmtdConfigFetcher.cc` | 建立 `HybridKvEngine`；`loadConfigTemplate` 讀 CONF 表、`completeAppInfo` 讀 NODE 表、`startServer` 把 kvEngine 交棒給 server |
| `MgmtdLauncherConfig.h` | 第一階段設定：`cluster_id` / `kv_engine` / `ib_devices` / `allow_dev_version`（`use_memkv` 與 `fdb` 已 deprecated） |
| `MgmtdLauncherConfig.cc` | 從檔案載入設定；`allow_dev_version=false` 時拒絕開發版二進位檔（FATAL） |

### `src/mgmtd/service/`

| 檔案 | 職責 |
|---|---|
| `MgmtdService.h` | serde service 宣告，X-macro 展開 **23** 個方法簽章（ID 1、3–24；ID 2 已廢棄，`MgmtdServiceDef.h:4` 只留註解） |
| `MgmtdService.cc` | 23 個方法的實作，每個都只是 `co_return co_await operator_.name(req, peer)` |
| `MgmtdOperator.h` | 持有 `MgmtdState` 與 `MgmtdBackgroundRunner`；宣告 24 個 operator 方法與 `start()` / `stop()` |
| `MgmtdOperator.cc` | X-macro 展開：把 Req 包成 `XxxOperation`，交給 `CO_INVOKE_OP_INFO`（自動記 metric + log + 錯誤碼） |
| `MgmtdState.h` | 進程全域狀態容器：`env_` / `selfNodeInfo_` / `config_` / `store_` / `userStore_` / `CoroSynchronized<MgmtdData> data_` / `CoroSynchronized<ClientSessionMap> clientSessionMap_` / `writerMu_`；`WriterMutexGuard` 自動記鎖延遲 |
| `MgmtdState.cc` | `utcNow` / `validateClusterId` / `validateAdmin` / `currentLease`（含 `suspicious_lease_interval` 提早退位邏輯）/ `createRetryStrategy`；`recordWriterLatency` 的靜態 recorder 表 |
| `MgmtdData.h` | 記憶體狀態的實際內容：`leaseStartTs` / `routingInfo` / `configMap` / `lease` / `universalTagsMap` / `routingInfoCache` |
| `MgmtdData.cc` | `checkConfigVersion` / `getConfig` / `getLatestConfigVersion` / `checkRoutingInfoVersion` / `getRoutingInfo`（含序列化快取）/ `appendChangedChains`（呼叫狀態機）/ `reset`（接管時全量替換）/ `bootstrapping` |
| `MgmtdConfig.h` | 27 個熱更新設定項 + `retry_transaction` + `user_cache` |
| `RoutingInfo.h` | server 端可變路由表：`nodeMap` / `chainTables` / `chains` / `newBornChains` / `orphanTargets*` + private `targets`；`updateTarget` 帶 DFATAL 防護 |
| `RoutingInfo.cc` | `applyChainTargetChanges` / `getChain` / `localUpdateTargets`（心跳合併 + chainVersion 柵欄 + orphan 收容）/ `insertNewChain` / `insertNewTarget` / `removeTarget` / `eraseOrphanTarget`（雙向索引一致性斷言）/ `reset`（把全部 chain 塞進 newBornChains） |
| `updateChain.h` | `ChainTargetInfoEx`（`ChainTargetInfo` + `localState`）；宣告四支純函式並註明 `rotateLastSrv` 的資料遺失風險 |
| `updateChain.cc` | **CRAQ 鏈狀態機**：`generateNewChain`（五桶按序處理）/ `rotateAsPreferredOrder` / `rotateLastSrv` / `shutdownChain` |
| `helpers.h` | `nextVersion` / `doAsPrimary` / `updateMemoryRoutingInfo` / `withReadWriteTxn`（雙層重試 + lease 驗證）/ `withReadOnlyTxn` / `updateStoredRoutingInfo`（先 bump FDB 版本）/ `RECORD_LATENCY` |
| `helpers.cc` | 上述的非模板實作 + `makeTargetInfo`（新 target 初始 localState=OFFLINE）/ `updateSelfConfig`（版本回退 FATAL）/ `updateTags`（REPLACE/UPSERT/REMOVE 三語意）/ `ensureSelfIsPrimary` |
| `LeaseInfo.h` | `{optional<MgmtdLeaseInfo> lease, bool bootstrapping}` |
| `ClientSession.h` | `WithTimestamp<flat::ClientSession>` + `clientSessionVersion` |
| `TargetInfo.h` | `WithTimestamp<flat::TargetInfo>` + `locationInitLoaded` / `persistedNodeId` / `persistedDiskIndex` / `importantInfoChangedTime` |
| `NodeInfoWrapper.h` | `WithTimestamp<flat::NodeInfo>` + `lastHbVersion`（拒絕過期心跳）+ 100 筆環形緩衝的狀態變更歷史 |
| `WithTimestamp.h` | 泛型模板：把任意 `T` 與一個 `SteadyTime` 綁在一起（用單調時鐘做超時判定） |
| `LocalTargetInfoWithNodeId.h` | `{targetId, nodeId, localState}` 的小結構（心跳處理的中間型別） |
| `MockMgmtd.h` | 測試用的 in-process mgmtd：自帶 Config、用假的 AppInfo 與傳入的 kvEngine 直接建 `MgmtdOperator` |

### `src/mgmtd/ops/`

| 檔案 | 職責 |
|---|---|
| `Include.h` | 一次 include 全部 22 個 operation 標頭（給 `MgmtdOperator.cc` 用） |
| `GetPrimaryMgmtdOperation.{h,cc}` | 回報誰是 primary。**唯一不需要 primary 身份**的方法；先查記憶體 lease，miss 才讀 FDB |
| `HeartbeatOperation.{h,cc}` | 心跳處理：叢集/節點/時間戳/型別/hbVersion 五重驗證 → 節點資訊變更才落盤 → `localUpdateTargets` → 回傳新設定 |
| `RegisterNodeOperation.{h,cc}` | 註冊節點（拒絕 nodeId=0 與重複），寫 NODE 表 + 推路由版本 |
| `UnregisterNodeOperation.{h,cc}` | 註銷節點；要求狀態為 DISABLED 或 HEARTBEAT_FAILED，且 storage 節點不得仍被任何 target 引用 |
| `EnableDisableNodeOperation.{h,cc}` | 啟用/停用節點（共用 `changeNodeStatus<method>` 模板）；用 `kDisabledTagKey` tag 標記，禁止對 primary 自己操作 |
| `GetRoutingInfoOperation.{h,cc}` | 增量取路由：版本相同回 nullopt，否則回完整快照（走 `MgmtdData::getRoutingInfo` 的快取） |
| `SetConfigOperation.{h,cc}` | 寫入新設定版本；若 `nodeType == MGMTD` 則當場自我套用並更新自身 `configVersion` |
| `GetConfigOperation.{h,cc}` | 取設定，`exactVersion` 決定「指定版本」或「比 X 更新的最新版」 |
| `GetConfigVersionsOperation.{h,cc}` | 回報各 NodeType 的最新設定版本（運維用） |
| `SetChainTableOperation.{h,cc}` | 建立/更新 chain table；驗證所有 chain 存在且副本數一致；**chains 或 desc 任一改變才推版本**（commit 22fca04 的修復點） |
| `SetChainsOperation.{h,cc}` | 批次建立 chain；已存在的 chain 只做「組成未變」驗證，不修改；新 chain 初始全 SERVING、chainVersion=1 |
| `UpdateChainOperation.{h,cc}` | 單一 chain 增減 target。ADD 一律進 OFFLINE 讓狀態機推進；REMOVE 要求當前為 OFFLINE 並清除 TGIF 記錄 |
| `RotateLastSrvOperation.{h,cc}` | 手動把卡住的 LASTSRV head 輪到尾巴、讓次位接手（**有資料遺失風險的逃生口**） |
| `SetPreferredTargetOrderOperation.{h,cc}` | 設定 chain 的偏好順序；驗證無重複且全部屬於該 chain；**刻意不推路由版本** |
| `RotateAsPreferredOrderOperation.{h,cc}` | 手動觸發一次「照 preferredTargetOrder 重排」 |
| `ExtendClientSessionOperation.{h,cc}` | client 續約；版本單調檢查 + 六個欄位的不變性檢查（偵測 clientId 撞號）；只碰 `clientSessionMap_`，不動 `data_` 獨佔鎖 |
| `GetClientSessionOperation.{h,cc}` | 查單一 client session + 其 universal tags |
| `ListClientSessionsOperation.{h,cc}` | 列出全部 session + 去重後的 tags；順便警告非 UUID 的 clientId |
| `SetNodeTagsOperation.{h,cc}` | 修改節點 tags；`kDisabledTagKey` 的增減會連帶改變節點 status；改自己時同步更新 `selfNodeInfo_` |
| `SetUniversalTagsOperation.{h,cc}` | 修改 universal tags（按 `universalId` 分組，client 用來繼承標籤） |
| `GetUniversalTagsOperation.{h,cc}` | 讀 universal tags |
| `ListOrphanTargetsOperation.{h,cc}` | 列出孤兒 target（storage 有回報但不屬於任何 chain） |

### `src/mgmtd/background/`

| 檔案 | 職責 |
|---|---|
| `MgmtdBackgroundRunner.{h,cc}` | 建立並啟動全部 10 個背景任務，每個綁定各自的 interval getter（支援熱更新與「設 0 即停用」） |
| `MgmtdLeaseExtender.{h,cc}` | **最核心的背景任務**：續約/搶主（`extendLease`），偵測到換屆時執行 `onNewLease` — 單交易併發載入六張表、把 RoutingInfoVersion +1 寫回、清空 client session |
| `MgmtdChainsUpdater.{h,cc}` | 每秒執行 chain 狀態機：用 `lastUpdateTs` 增量篩選變動過的 target → `appendChangedChains` → 落盤 → 更新記憶體。成功才推進 `lastUpdateTs` |
| `MgmtdHeartbeatChecker.{h,cc}` | 判定節點與 target 的心跳逾時（各自獨立判定）；只改記憶體並置 `routingInfoChanged`，不落盤不推版本 |
| `MgmtdNewBornChainsChecker.{h,cc}` | 解除超過 `new_chain_bootstrap_interval` 的 chain 靜默期，並刷新其 target 時間戳確保被下一輪 updater 掃到；帶 lease 換屆防護 |
| `MgmtdRoutingInfoVersionUpdater.{h,cc}` | 每 5 秒檢查 `routingInfoChanged`，把批次累積的節點狀態變更合併成一次版本推進 |
| `MgmtdHeartbeater.{h,cc}` | 非 primary 的 mgmtd 向 primary 發心跳（primary 自己跳過）；`leaseStart` 改變時重建 stub；從回應拿設定；解析 `kHeartbeatVersionStale` 訊息自動修正 hbVersion |
| `MgmtdClientSessionsChecker.{h,cc}` | 清除超過 `client_session_timeout` 的 session（共享鎖收集 → 獨佔鎖時重新驗證） |
| `MgmtdTargetInfoPersister.{h,cc}` | 把 target 的 `(nodeId, diskIndex)` 批次寫入 TGIF 表；三重條件過濾避免無謂寫入 |
| `MgmtdTargetInfoLoader.{h,cc}` | 每個 lease 只跑一次：分批掃 TGIF 把位置資訊補回記憶體（心跳優先，KV 補缺）；最後標記所有 target 為 `locationInitLoaded` |
| `MgmtdMetricsUpdater.{h,cc}` | 每秒匯出 11 組指標；`recordCount<Name>` 模板用 static map 快取 recorder，key 可為 tuple |

### `src/mgmtd/store/`

| 檔案 | 職責 |
|---|---|
| `MgmtdStore.h` | FDB 存取層的完整介面：lease / node / routingInfoVersion / config / chainTable / chain / targetInfo / universalTags 的 CRUD + `shutdownAllChains` |
| `MgmtdStore.cc` | key 編碼（NODE 原生序、CHIT 複合 key、TGIF **big-endian**、CONF `MAX-version` 倒序、UTGS 字串後綴）；`extendLease` 的六分支選主邏輯；`ensureLeaseValid` 的腦裂防護；key/value 冗餘欄位交叉驗證 |

---

## 附錄：關鍵位置速查

| 主題 | 位置 |
|---|---|
| main 進入點 | `src/mgmtd/mgmtd.cpp:5-8` |
| 兩階段啟動 | `src/common/app/TwoPhaseApplication.h:36-70` |
| 組件組裝 | `src/mgmtd/MgmtdServer.cc:22-38` |
| **選主/續約邏輯** | `src/mgmtd/store/MgmtdStore.cc:154-189` |
| **腦裂防護（交易內驗證）** | `src/mgmtd/store/MgmtdStore.cc:191-204` + `src/mgmtd/service/helpers.h:52-54` |
| 提早退位 | `src/mgmtd/service/MgmtdState.cc:66-75` |
| 接管全量載入 | `src/mgmtd/background/MgmtdLeaseExtender.cc:128-188` |
| **版本單調性（先 FDB 後記憶體）** | `src/mgmtd/service/helpers.h:72-81` + `helpers.cc:77-82` |
| **CRAQ 狀態機（純函式）** | `src/mgmtd/service/updateChain.cc:25-104` |
| 狀態機的 storage 端另一半 | `src/storage/service/TargetMap.cc:331-354` |
| 心跳合併 + orphan 收容 | `src/mgmtd/service/RoutingInfo.cc:38-87` |
| 靜默期（防換屆誤降級） | `src/mgmtd/service/RoutingInfo.cc:168-171` + `MgmtdData.cc:119` |
| chain 增量掃描 | `src/mgmtd/background/MgmtdChainsUpdater.cc:25-36` |
| 心跳逾時判定 | `src/mgmtd/background/MgmtdHeartbeatChecker.cc:34-52` |
| RoutingInfo 序列化快取 | `src/mgmtd/service/MgmtdData.cc:80-114` |
| FDB key 編碼 | `src/mgmtd/store/MgmtdStore.cc:16-127` |
| key 前綴表 | `src/common/kv/KeyPrefix-def.h` |
| RPC 方法編號 | `src/fbs/mgmtd/MgmtdServiceDef.h`（serviceId 217 見 `MgmtdServiceBase.h:7`） |
| 錯誤碼 | `src/common/utils/StatusCodeDetails.h:193-211` |
| 設定項全表 | `src/mgmtd/service/MgmtdConfig.h` |
| 背景任務註冊 | `src/mgmtd/background/MgmtdBackgroundRunner.cc:37-80` |
| 背景任務驅動 | `src/common/utils/BackgroundRunner.cc:73-116` |
| Metric 匯出 | `src/mgmtd/background/MgmtdMetricsUpdater.cc:44-259` |
| client 端 failover | `src/client/mgmtd/MgmtdClient.cc:139-168` |
| ChainTable 版本修復 | commit `22fca04` → `src/mgmtd/ops/SetChainTableOperation.cc:56-64` |
