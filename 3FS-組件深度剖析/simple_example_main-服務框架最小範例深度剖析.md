# `simple_example_main` 服務框架最小範例深度剖析

> 對應原始碼：`src/simple_example/`（5 檔）、`src/fbs/simple_example/`（2 檔）
> 框架本體：`src/common/serde/`、`src/common/net/`、`src/common/app/`、`src/core/app/`
> 風格基準：[`../3FS-元資料層深度剖析.md`](../3FS-元資料層深度剖析.md)

---

## 0. 一句話總結

`simple_example` 是 3FS 服務框架的**最小可運行骨架**：`echo` 一個字串，總共 121 行程式碼。它的價值不在功能，而在於它把「寫一個 3FS 服務要碰哪些東西」壓縮到最小——**定義 serde 服務 → 實作 handler → 註冊到 Server → 兩階段啟動**。

它最不顯而易見的一點是：`src/simple_example/README.md` 提供的不是說明文件，而是一段**可直接執行的 `sed` 腳本**，範例變數就叫 `migration`。3FS 團隊自己用它產出了 `src/migration/`（見 [migration 報告](migration_main-資料遷移工具深度剖析.md)）。所以這個目錄的定位是「**程式碼模板**」而非「教學範例」——它的正確用法是複製，不是閱讀後模仿。

本報告把它當框架教學來拆：從一個 echo 請求的完整生命週期出發，逐層說明框架要求你提供什麼、又替你做了什麼。

---

## 1. 檔案清單

```
src/simple_example/
├── CMakeLists.txt          2 行
├── README.md              16 行   ← 複製腳本
├── main.cpp                8 行   ← 進入點
└── service/
    ├── Server.h           72 行   ← 生命週期骨架（最重要的檔案）
    ├── Server.cc          68 行   ← 相依組件的建立與拆除
    ├── Service.h          21 行   ← handler 宣告
    └── Service.cc         21 行   ← handler 實作

src/fbs/simple_example/
├── CMakeLists.txt          1 行
└── SerdeService.h         17 行   ← RPC 契約（跨端共用）
```

**實作總計 121 行。** 這 121 行完整涵蓋：TCP + RDMA 雙埠監聽、mgmtd 註冊與心跳、設定熱推送、routing info 訂閱、storage client 建立、優雅停機。框架代勞的部分遠大於使用者撰寫的部分——這正是這個範例要展示的事。

---

## 2. 一個 echo 請求的完整生命週期

先看全貌，再逐層拆。

```
 客戶端                                                         伺服器端
   │
   │  SimpleExampleSerde<>::echo(ctx, req)
   │    ← SERDE_SERVICE_METHOD 巨集自動生成的 sender
   │      （fbs/simple_example/SerdeService.h:16）
   │
   ├─ serde::serialize(req)  → positional binary，無欄位標籤
   │
   ├─ MessagePacket{ serviceId = 0xF0, methodId = 1, payload, uuid, timestamp }
   │
   ▼ TCP 或 RDMA
┌─────────────────────────────────────────────────────────────────────┐
│ Listener   接受連線（net/Listener.h）                                │
│    ↓                                                                │
│ IOWorker   讀出完整封包（net/IOWorker.h）                            │
│    ↓                                                                │
│ Processor  解析 MessagePacket 表頭（net/Processor.h）                │
│    ↓                                                                │
│ serde::Services::getServiceById(packet.serviceId, isRDMA)           │
│    → services_[isRDMA][0xF0]                                        │
│      這是一個 array<ServiceWrapper, 65536>，O(1) 查表                │
│      （serde/Services.h:34, 38）                                    │
│    ↓                                                                │
│ CallContext{packet, transport, serviceWrapper}                      │
│    ↓  handle()                （serde/CallContext.h:35-38）          │
│ auto method = service_.getter(packet_.methodId);                    │
│    → MethodExtractor<...>::get(1)                                   │
│      consteval 期建好的跳表，O(1)（serde/Service.h:48-78）           │
│    ↓                                                                │
│ co_await (this->*method)()  →  CallContext::call<MethodInfo>()      │
│                                （serde/CallContext.h:46-76）        │
│    ├─ serde::deserialize(req, packet_.payload)                      │
│    │     失敗 → onDeserializeFailed()                               │
│    ├─ packet_.timestamp->serverWaked = now()                        │
│    ├─ co_await (obj->*F::method)(*this, req)   ★ 使用者的 handler ★  │
│    │     ↓                                                          │
│    │   SimpleExampleService::echo(CallContext&, const Req&)         │
│    │     { resp.message = req.message; co_return resp; }            │
│    │     （simple_example/service/Service.cc:13-17）                 │
│    ├─ packet_.timestamp->serverProcessed = now()                    │
│    └─ makeResponse(result.value())                                  │
│          回填相同的 uuid / serviceId / methodId / version / timestamp │
│          （serde/CallContext.h:119-128）                             │
└─────────────────────────────────────────────────────────────────────┘
   │
   ▼ 回程
 客戶端 co_return
```

**使用者只寫了流程圖裡標星號的那一行。** 其餘全部由框架用 C++20 的 requires / consteval / 反射巨集組裝出來。

### 2.1 兩層 O(1) 分派

分派完全沒有雜湊表、沒有 `std::map`、沒有虛擬函式的動態查找：

**第一層：service id → ServiceWrapper**（`src/common/serde/Services.h:38`）

```cpp
std::array<CallContext::ServiceWrapper, 65536> services_[2];  // 0 for TCP, 1 for RDMA.
```

兩個 65536 項的陣列直接索引。`ServiceWrapper` 是 32 bytes 左右（`CallContext.h:17-23`：一個函式指標、一個錯誤處理指標、一個 void*、一個 shared_ptr），所以這兩個陣列大約 4 MB 常駐記憶體。**用 4 MB 換取零雜湊、零分支的分派**——對一個每秒處理數十萬請求的存儲系統，這個取捨很划算。

`services_[2]` 這個 TCP/RDMA 二分是為了讓同一個 service id 在兩種傳輸上可以掛不同的物件（實務上通常是同一個，見 `Services.h:19` 的迴圈 `for (auto i = 0u; i <= uint32_t(isRDMA); ++i)`——註冊 RDMA 服務時會同時填入兩格，註冊 TCP 服務時只填第 0 格）。

**第二層：method id → 成員函式指標**（`src/common/serde/Service.h:48-78`）

```cpp
template <class T, class C, auto DEFAULT = nullptr>
class MethodExtractor {
 public:
  static auto get(uint16_t id) {
    constexpr MethodExtractor ins;
    return id <= ins.kMaxThreadId ? ins.table[id] : DEFAULT;
  }
 protected:
  consteval MethodExtractor() {
    for (uint16_t i = 0; i <= kMaxThreadId; ++i) { table[i] = calc(i); }
  }
  template <size_t I = 0>
  consteval auto calc(uint16_t id) {
    if constexpr (I == std::tuple_size_v<FieldInfoList>) { return DEFAULT; }
    else {
      using FieldInfo = std::tuple_element_t<I, FieldInfoList>;
      return FieldInfo::id == id ? &C::template call<FieldInfo> : calc<I + 1>(id);
    }
  }
 private:
  static constexpr auto kMaxThreadId = MaxMethodId<FieldInfoList>;
  std::array<Method, kMaxThreadId + 1> table;
};
```

`consteval` 建構子在**編譯期**把跳表填好，大小是 `max(methodId) + 1`。`SimpleExampleSerde` 只有 method id = 1，所以這張表只有 2 項。查表時 `id <= kMaxThreadId` 這個上界檢查防止越界，超出範圍回傳 `DEFAULT`（= `&CallContext::invalidId`，`Services.h:24`）。

也就是說：**method id 應該從小整數連號分配。** 若某人寫 `SERDE_SERVICE_METHOD(foo, 60000, ...)`，跳表就會膨脹到 60001 項。這個約束沒有寫在任何文件裡，只能從實作推出來。`MigrationSerde` 用 1/2/3、`MetaSerde` 也是從 1 開始連號，慣例是一致的。

### 2.2 `FieldInfoList` 從哪來：巨集式編譯期反射

`SERDE_SERVICE_METHOD` 展開成兩半（`src/common/serde/Service.h:86-88`）：

```cpp
#define SERDE_SERVICE_METHOD(NAME, ID, REQ, RSP)  \
  SERDE_SERVICE_METHOD_SENDER(NAME, ID, REQ, RSP) \
  SERDE_SERVICE_METHOD_REFL(NAME, ID, REQ, RSP)
```

**`_SENDER` 那一半生成客戶端**（`:90-112`）：

```cpp
template <class Context>
static CoTryTask<RSP> NAME(Context &ctx, const REQ &req, const UserRequestOptions *options = nullptr,
                           serde::Timestamp *timestamp = nullptr) {
  co_return co_await ctx.template call<kServiceNameWrapper, #NAME, REQ, RSP, kServiceID, ID>(req, options, timestamp);
}
template <class Context>
static Result<RSP> NAME##Sync(Context &ctx, ...) { ... }
```

所以**客戶端呼叫程式碼是零手寫的**——`SimpleExampleSerde<>::echo(ctx, req)` 直接可用，同步版 `echoSync` 也一併生成。`Context` 是模板參數，所以同一份定義能搭配真實的 `serde::ClientContext`、測試用的假 context、或任何提供 `call<...>()` 的型別。

**`_REFL` 那一半生成編譯期型別清單**（`:114-126`）：

```cpp
struct MethodId##ID : std::type_identity<REFL_NOW> {};
static auto CollectField(::hf3fs::refl::Rank<std::tuple_size_v<typename MethodId##ID::type> + 1>) {
  if constexpr (std::is_void_v<T>) {
    return refl::Append_t<typename MethodId##ID::type, serde::MethodInfo<#NAME, T, REQ, RSP, ID, nullptr>>{};
  } else {
    return refl::Append_t<typename MethodId##ID::type, serde::MethodInfo<#NAME, T, REQ, RSP, ID, &T::NAME>>{};
  }
}
friend struct ::hf3fs::refl::Helper
```

利用 `Rank<N>` 多載順位的技巧，每個 `SERDE_SERVICE_METHOD` 都往型別清單尾端 append 一項，最終 `refl::Helper::FieldInfoList<Service<T>>` 就是一個 `std::tuple<MethodInfo<...>, ...>`。

**`if constexpr (std::is_void_v<T>)` 這個分支是關鍵**。`SERDE_SERVICE_2` 把服務定義成模板（`Service.h:82-84`）：

```cpp
#define SERDE_SERVICE_2(STRUCT_NAME, SERVICE_NAME, ID) \
  template <class T = void>                            \
  struct STRUCT_NAME : public ::hf3fs::serde::ServiceBase<#SERVICE_NAME, ID>
```

- `T = void`（客戶端）：`MethodInfo::method = nullptr`，只需要 sender，不需要知道有誰實作。
- `T = 具體實作類別`（伺服器端）：`MethodInfo::method = &T::NAME`，指向真正的 handler。

同一份 `SerdeService.h` 因此**同時是客戶端 stub 定義與伺服器端分派表定義**，兩端引用同一個檔案，不可能不同步。這是元資料層報告 §14 提到的「positional serde」思路在 RPC 層的延伸：**契約由一份原始碼定義，靠編譯器保證兩端一致，而不是靠 IDL 產生器與版本管理。**

### 2.3 `ServiceWrapper` 綁定：`ServiceWrapper<Self, Serde>` CRTP

使用者的實作類別長這樣（`src/simple_example/service/Service.h:8`）：

```cpp
class SimpleExampleService : public serde::ServiceWrapper<SimpleExampleService, SimpleExampleSerde> {
```

`serde::ServiceWrapper`（`src/common/serde/Service.h:37-46`）：

```cpp
template <class T, template <class> class Service>
struct ServiceWrapper {
  static constexpr std::string_view kServiceName = Service<void>::kServiceName;
  static constexpr uint16_t kServiceID = Service<void>::kServiceID;

 protected:
  friend struct ::hf3fs::refl::Helper;
  template <class U = T>
  static auto CollectField(::hf3fs::refl::Rank<> rank) -> refl::Helper::FieldInfoList<Service<U>>;
};
```

三件事一次完成：

1. 把 `kServiceName` / `kServiceID` 從契約複製到實作類別（`Services::addService` 需要）。
2. 把 `refl::Helper::FieldInfoList<SimpleExampleService>` 轉發成 `FieldInfoList<SimpleExampleSerde<SimpleExampleService>>`——也就是**帶著實作型別重新實例化契約模板**，於是 `&T::echo` 這個成員函式指標被填進 `MethodInfo`。
3. 這是 CRTP：`T` 是衍生類別自己。

**編譯期檢查的價值在這裡兌現**：如果 `SimpleExampleService` 少寫了 `echo`，`&T::echo` 就無法解析，編譯失敗。如果簽章不對（例如漏了 `CallContext&`、或 `REQ` 型別不符），`CallContext::call<F>` 裡的 `(obj->*F::method)(*this, req)` 會編譯失敗。**契約與實作的不一致永遠不會跑到執行期。**

---

## 3. 步驟一：定義 serde 服務

`src/fbs/simple_example/SerdeService.h`（全文 17 行）：

```cpp
#pragma once

#include "common/serde/Serde.h"
#include "common/serde/Service.h"

namespace hf3fs::simple_example {

struct SimpleExampleReq {
  SERDE_STRUCT_FIELD(message, String{});
};

struct SimpleExampleRsp {
  SERDE_STRUCT_FIELD(message, String{});
};

SERDE_SERVICE(SimpleExampleSerde, 0xF0) { SERDE_SERVICE_METHOD(echo, 1, SimpleExampleReq, SimpleExampleRsp); };

}  // namespace hf3fs::simple_example
```

### 3.1 為什麼放在 `src/fbs/` 而不是 `src/simple_example/`

`src/fbs/` 是**跨端共用的契約層**。慣例是：任何客戶端與伺服器端都要看到的型別，放 `src/fbs/<模組>/`；只有伺服器端看的，放 `src/<模組>/`。

對應的 CMake（`src/fbs/simple_example/CMakeLists.txt:1`）：

```cmake
target_add_lib(simple_example-fbs mgmtd-fbs core-user-fbs)
```

`simple_example-fbs` 是獨立的靜態庫，只依賴 `mgmtd-fbs`（routing info / node 型別）與 `core-user-fbs`（`UserInfo`）。客戶端只要連這個庫就能發請求，**不必連整個伺服器端實作**。

### 3.2 `SERDE_STRUCT_FIELD` 的三個約束

`SERDE_STRUCT_FIELD(name, defaultValue)` 同時宣告欄位、預設值、與序列化順位。元資料層報告 §14 詳述了編碼格式，這裡只記使用者必須遵守的規則：

| 規則 | 原因 |
|---|---|
| **只能在 struct 尾端加欄位** | 二進位輸出無欄位標籤，只按宣告順序寫值 |
| **永遠不能刪除、重排、改型別** | 同上；舊版讀新資料靠「尾端多餘 bytes 直接忽略」 |
| **尾端缺欄位可接受** | 新版讀舊資料時，缺的欄位取 `defaultValue` |

第三條是向下相容的來源。`src/fbs/meta/Service.h` 的 `CloseRsp` 開頭那個 `SERDE_STRUCT_FIELD(_unused, (uint32_t)0)` 就是廢棄欄位不能移除、只能留著佔位的實例。

**寫新服務時的實務建議**：即使目前只需要一個欄位，也要想好未來會加什麼，並確保加在尾端不會破壞語意。

### 3.3 服務 ID 與 method ID 的分配

`SERDE_SERVICE(SimpleExampleSerde, 0xF0)` 的第二個參數是 16 位元服務 ID。全 repo 分配情況：

| ID | 服務 | 位置 |
|---|---|---|
| 3 | `StorageSerde` | `src/fbs/storage/Service.h:8` |
| 4 | `MetaSerde` | `src/fbs/meta/Service.h:709` |
| 10 | `RDMAControl` | `src/common/net/RDMAControl.h:18` |
| 10 | `ClientAgentSerde` | `src/fbs/lib/Service.h:195` |
| 11 | `IBConnect` | `src/common/net/ib/IBConnectService.h:18` |
| 194 | `MonitorCollector` | `src/fbs/monitor_collector/MonitorCollectorService.h:13` |
| **0xF0** | **`SimpleExampleSerde`** | `src/fbs/simple_example/SerdeService.h:16` |
| **0xF1** | **`MigrationSerde`** | `src/fbs/migration/SerdeService.h:74` |
| 10000 | `Echo`（框架內建） | `src/common/serde/Echo.h:13` |

慣例（未成文）：**真實服務用小整數，範例與衍生品用 `0xF0` 起跳的高位段。**

ID `10` 被 `RDMAControl` 與 `ClientAgentSerde` 共用不是 bug——`Services::addService` 的重複檢查（`Services.h:21-23`）是**每個 `ServiceGroup` 各自一份**，兩者永遠不在同一個 group 註冊。

method ID 則如 §2.1 所述應從 1 開始連號，否則 `MethodExtractor` 的跳表會膨脹。

---

## 4. 步驟二：實作 handler

`src/simple_example/service/Service.h`：

```cpp
class SimpleExampleService : public serde::ServiceWrapper<SimpleExampleService, SimpleExampleSerde> {
 public:
  SimpleExampleService();

#define DECLARE_SERVICE_METHOD(METHOD, REQ, RESP) CoTryTask<RESP> METHOD(serde::CallContext &, const REQ &req)

  DECLARE_SERVICE_METHOD(echo, SimpleExampleReq, SimpleExampleRsp);

#undef DECLARE_SERVICE_METHOD

 private:
};
```

`src/simple_example/service/Service.cc:13-17`：

```cpp
DEFINE_SERVICE_METHOD(echo, SimpleExampleReq, SimpleExampleRsp) {
  SimpleExampleRsp resp;
  resp.message = req.message;
  co_return resp;
}
```

### 4.1 簽章的四個要素

```cpp
CoTryTask<RSP>   METHOD(serde::CallContext &, const REQ &req)
    ↑                        ↑                    ↑
    │                        │                    └─ 已反序列化好的請求，const 引用
    │                        └─ 傳輸層上下文（多數 handler 用不到，可略去參數名）
    └─ = folly::coro::Task<Result<RSP>>；協程 + 錯誤即值
```

`CoTryTask<T>` 是 `folly::coro::Task<Result<T>>` 的別名。回傳成功用 `co_return resp;`（隱式轉成 `Result`），回傳錯誤用 `co_return makeError(StatusCode::kXxx);`。

**不能拋例外。** `CallContext::call` 用 `co_awaitTry` 包住 handler，抓到例外會直接 `XLOGF(FATAL, ...)` 讓進程死掉（`src/common/serde/CallContext.h:62-70`）：

```cpp
auto result = co_await folly::coro::co_awaitTry((obj->*F::method)(*this, req));
if (UNLIKELY(result.hasException())) {
  XLOGF(FATAL, "Processor has exception: {}, request {}:{} {}",
        result.exception().what(), packet_.serviceId, packet_.methodId, serde::toJsonString(req));
  co_return;
}
```

這是刻意的 fail-fast：**框架把「handler 拋例外」視為程式碼 bug 而非可恢復狀況**，並且在死之前把整個請求 dump 成 JSON（`serde::toJsonString(req)`——同一份 serde 定義也支援 JSON 輸出，見元資料層報告 §14）。

### 4.2 `CallContext` 提供什麼

多數 handler 用不到它（`simple_example` 與 `migration` 都直接省略參數名）。它提供的是傳輸層能力：

- `tracingPoints()`（`CallContext.h:92`）：分散式追蹤埋點
- `RDMATransmission`（`CallContext.h:94-`）：handler 要主動發起 RDMA 傳輸時使用。這是 3FS storage 路徑的核心——server 端主動 RDMA read/write 客戶端的緩衝區（見 RDMA 深度報告）
- `transport()`：拿到底層連線，例如 `StorageOperator.cc:176` 用 `ctx.transport()->ibSocket()` 取得 IB socket

**寫一個普通的控制平面服務不需要碰它；寫資料平面服務（要做零複製傳輸）才需要。**

### 4.3 可選的 `onError` 鉤子

`Services::addService`（`src/common/serde/Services.h:27-29`）：

```cpp
if constexpr (requires { Service{}.onError(Status::OK); }) {
  service.onError = &CallContext::customOnError<Service, &Service::onError>;
}
```

如果你的 service 類別提供 `Status onError(Status)`，框架會自動偵測（C++20 `requires`）並在錯誤回傳前呼叫它。用途是把內部錯誤碼轉譯成對外的錯誤碼、或統一補上上下文。`simple_example` 沒提供，所以走預設的 `serviceOnError`（原樣回傳，`CallContext.h:83`）。

注意這個偵測用 `Service{}` 預設建構一個臨時物件——所以**service 類別必須可預設建構**才能被偵測到（實際上只在 unevaluated context 中，不會真的建構，但型別必須合法）。

### 4.4 那個空的 `private:`

`Service.h:18-19` 有一個什麼都沒有的 `private:` 標籤。這是模板留給你的位置：**service 的狀態放這裡**。`migration` 複製後也留著這個空標籤沒填（見 migration 報告 §2.1），正是它未實作的證據之一。

實務上這裡會放什麼：指向 KV engine 的 shared_ptr、後端組件的引用、統計 recorder。要注意 **service 物件被 `Services` 用 `shared_ptr` 持有**（`Services.h:18, 26`），且同一個物件會被多個 IO 執行緒併發呼叫——**handler 必須是執行緒安全的**。

---

## 5. 步驟三：Server 骨架

`src/simple_example/service/Server.h` 是整個範例最值得逐行讀的檔案。它宣告了框架要求 Server 類別提供的**全部靜態契約**。

```cpp
class SimpleExampleServer : public net::Server {
 public:
  // ── (1) 身分 ──────────────────────────────────────────
  static constexpr auto kName = "SimpleExample";
  static constexpr auto kNodeType = flat::NodeType::CLIENT;

  // ── (2) 通用設定（log / monitor / ib / memory）─────────
  struct CommonConfig : public ApplicationBase::Config {
    CommonConfig() {
      using logging::LogConfig;
      log().set_categories({LogConfig::makeRootCategoryConfig(), LogConfig::makeEventCategoryConfig()});
      log().set_handlers({LogConfig::makeNormalHandlerConfig(),
                          LogConfig::makeErrHandlerConfig(),
                          LogConfig::makeFatalHandlerConfig(),
                          LogConfig::makeEventHandlerConfig()});
    }
  };

  // ── (3) 兩階段啟動所需的四個型別 ───────────────────────
  using AppConfig = core::ServerAppConfig;
  struct LauncherConfig : public core::ServerLauncherConfig {
    LauncherConfig() { mgmtd_client() = hf3fs::client::MgmtdClientForServer::Config{}; }
  };
  using RemoteConfigFetcher = core::launcher::ServerMgmtdClientFetcher;
  using Launcher = core::ServerLauncher<SimpleExampleServer>;

  // ── (4) 業務設定 ──────────────────────────────────────
  struct Config : public ConfigBase<Config> {
    CONFIG_OBJ(base, net::Server::Config, [](net::Server::Config &c) {
      c.set_groups_length(2);
      c.groups(0).listener().set_listen_port(8000);
      c.groups(0).set_services({"SimpleExampleSerde"});

      c.groups(1).set_network_type(net::Address::TCP);
      c.groups(1).listener().set_listen_port(9000);
      c.groups(1).set_use_independent_thread_pool(true);
      c.groups(1).set_services({"Core"});
    });
    CONFIG_OBJ(background_client, net::Client::Config);
    CONFIG_OBJ(mgmtd_client, ::hf3fs::client::MgmtdClientForServer::Config);
    CONFIG_OBJ(storage_client, storage::client::StorageClient::Config);
  };

  // ── (5) 生命週期鉤子 ──────────────────────────────────
  SimpleExampleServer(const Config &config);
  ~SimpleExampleServer() override;
  Result<Void> beforeStart() final;
  Result<Void> beforeStop() final;

 private:
  const Config &config_;
  std::unique_ptr<net::Client> backgroundClient_;
  std::shared_ptr<::hf3fs::client::MgmtdClientForServer> mgmtdClient_;
};
```

### 5.1 `kName` 與 `kNodeType`

`kName` 用於日誌檔名與監控標籤（`TwoPhaseApplication.h:49-51` 把它傳給 `initCommonComponents` 與 log 回呼）。

`kNodeType` 決定這個進程在 mgmtd 眼中的角色。`ServerLauncher` 用它拉設定範本（`src/core/app/ServerLauncher.h:30, 50`）：

```cpp
static constexpr auto kNodeType = Server::kNodeType;
...
Result<std::pair<String, String>> loadConfigTemplate() {
  auto res = fetcher_->loadConfigTemplate(kNodeType);
```

**mgmtd 為每種 NodeType 保存一份設定範本**，所以 `kNodeType` 選什麼決定了你的服務讀到哪份設定。`simple_example` 選 `CLIENT` 而非新增一個 enum 值，是務實的做法——不必改 mgmtd 的 schema。

### 5.2 `CommonConfig`：把日誌管線寫進建構子

`CommonConfig` 繼承 `ApplicationBase::Config`（提供 `log` / `monitor` / `memory` 三個子物件），建構子裡設定日誌管線：

```
categories:  Root（一般日誌）+ Event（結構化事件）
handlers:    Normal（stdout/檔案）+ Err（錯誤流）+ Fatal（崩潰）+ Event（事件流）
```

**「一般日誌」與「事件日誌」分流**是 3FS 的通用模式：Event category 承載結構化的稽核／追蹤事件（對照 `trash_cleaner` 用 tracing 的 `target: "event"` 做同樣的事，見 [trash_cleaner 報告 §7](trash_cleaner-Rust垃圾回收工具深度剖析.md)），與人類閱讀的日誌分開，方便下游用 `analytics` 模組（Arrow/Parquet）攝取。

在**建構子**而非設定檔裡設定，意思是這是**程式碼層級的預設值**，設定檔仍可覆寫。3FS 的設定系統一貫如此：程式碼給合理預設，TOML 只寫差異。

### 5.3 四個型別別名：兩階段啟動的接線

```cpp
using AppConfig = core::ServerAppConfig;                          // 只有 node_id
struct LauncherConfig : public core::ServerLauncherConfig { ... }; // cluster_id / ib / mgmtd 連線
using RemoteConfigFetcher = core::launcher::ServerMgmtdClientFetcher;
using Launcher = core::ServerLauncher<SimpleExampleServer>;
```

`ServerLauncher`（`src/core/app/ServerLauncher.h:24-78`）是個模板，透過這四個別名反向取得所有需要的型別：

```cpp
template <typename Server>
class ServerLauncher : public ServerLauncherBase {
 public:
  using AppConfig = typename Server::AppConfig;
  using LauncherConfig = typename Server::LauncherConfig;
  using RemoteConfigFetcher = typename Server::RemoteConfigFetcher;
  static constexpr auto kNodeType = Server::kNodeType;
```

三個設定的職責切得很乾淨：

| 設定 | 內容 | 從哪來 | 何時用 |
|---|---|---|---|
| `AppConfig` | `node_id`、`allow_empty_node_id`（`ServerAppConfig.h:9-10`） | `--app_cfg` 本地檔 | 最早，決定「我是誰」 |
| `LauncherConfig` | `cluster_id`、`ib_devices`、`client`、`mgmtd_client`、`allow_dev_version`（`ServerLauncherConfig.h:10-14`） | `--launcher_cfg` 本地檔 | 第二，決定「怎麼連上 mgmtd」 |
| `Config`（業務） | 監聽埠、服務清單、各種 client 設定 | **mgmtd 下發的範本**（可被 `--cfg` 覆寫） | 第三，決定「我怎麼運作」 |

**這個三層切分是兩階段啟動的核心。** 前兩層必須是本地的（不然雞生蛋），第三層才能是中央管理的。

`LauncherConfig` 特地在建構子裡把 `mgmtd_client()` 換成 `MgmtdClientForServer::Config{}`——因為基底的 `ServerLauncherConfig` 預設用通用的 `MgmtdClient::Config`，而伺服器角色需要心跳相關的欄位。

### 5.4 `Config`：`CONFIG_OBJ` 與 lambda 初始化器

```cpp
CONFIG_OBJ(base, net::Server::Config, [](net::Server::Config &c) { ... });
```

`CONFIG_OBJ(name, cls, initializer)`（`src/common/utils/ConfigBase.h:44-56`）宣告一個子設定物件並提供可選的初始化 lambda。展開後生成 `name()` 存取器（const 與非 const 兩版）與私有成員 `name_`。

三種設定巨集的差異：

| 巨集 | 語意 |
|---|---|
| `CONFIG_ITEM(name, default)` | 純量欄位，**啟動後不可變更** |
| `CONFIG_HOT_UPDATED_ITEM(name, default)` | 純量欄位，**可熱更新**（mgmtd 推送後立即生效） |
| `CONFIG_OBJ(name, cls, init?)` | 巢狀設定物件 |
| `CONFIG_OBJ_ARRAY(name, cls, cap, init?)` | 固定容量的設定物件陣列（如 `groups`，上限 4） |

`net::Server::Config`（`src/common/net/Server.h:23-27`）：

```cpp
struct Config : public ConfigBase<Config> {
  CONFIG_OBJ(thread_pool, ThreadPoolGroup::Config);
  CONFIG_OBJ(independent_thread_pool, ThreadPoolGroup::Config);
  CONFIG_OBJ_ARRAY(groups, ServiceGroup::Config, 4);
};
```

**最多 4 個 service group，編譯期固定。**

### 5.5 雙埠設計：為什麼要兩個 group

```
             simple_example_main 進程
  ┌───────────────────────────────────────────────────────┐
  │ tpg_ ("Svr")                  獨立 tpg ("SvrI")        │
  │   │                             │                     │
  │   ▼                             ▼                     │
  │ ┌─────────────────────┐   ┌──────────────────────┐    │
  │ │ group 0   :8000     │   │ group 1   :9000  TCP │    │
  │ │ network_type = RDMA │   │ use_independent_     │    │
  │ │   （ServiceGroup::  │   │   thread_pool = true │    │
  │ │    Config 預設值）   │   │                      │    │
  │ │ services:           │   │ services:            │    │
  │ │  "SimpleExampleSerde"│  │  "Core"              │    │
  │ │ → 業務流量           │   │ → 管理面              │    │
  │ └─────────────────────┘   └──────────────────────┘    │
  └───────────────────────────────────────────────────────┘
```

`ServiceGroup::Config`（`src/common/net/ServiceGroup.h:22-31`）的預設 `network_type` 是 `Address::RDMA`（`:25`），所以 group 0 不寫就是 RDMA；group 1 明確設成 TCP。

`Server` 的建構子依 `use_independent_thread_pool` 把 group 掛到不同執行緒池（`src/common/net/Server.cc:16-21`）：

```cpp
for (auto i = 0ul; i < serviceNum; ++i) {
  auto &tpg = config_.groups(i).use_independent_thread_pool() ? independentTpg_ : tpg_;
  groups_.emplace_back(std::make_unique<ServiceGroup>(config_.groups(i), tpg));
}
```

**這個設計是為了讓管理面與資料面不共命運。** 業務執行緒池被打滿或死鎖時，管理面（`CoreService`：讀寫設定、取狀態、觸發 dump）仍然可用。管理面固定 TCP 而非 RDMA 也是同一個考量——診斷時往往正是 IB 出問題的時候。

**這是每個新服務都應該照抄的模式**，`migration`、`meta`、`storage`、`mgmtd` 全都是這個結構。

---

## 6. 步驟四：`beforeStart` 建立相依組件

`src/simple_example/service/Server.cc:21-54`：

```cpp
Result<Void> SimpleExampleServer::beforeStart() {
  // (1) 背景 net client：所有對外連線的傳輸層
  if (!backgroundClient_) {
    backgroundClient_ = std::make_unique<net::Client>(config_.background_client());
    RETURN_ON_ERROR(backgroundClient_->start());
  }

  // (2) mgmtd client：用 backgroundClient_ 建 stub factory
  if (!mgmtdClient_) {
    auto ctxCreator = [this](net::Address addr) { return backgroundClient_->serdeCtx(addr); };
    mgmtdClient_ = std::make_shared<::hf3fs::client::MgmtdClientForServer>(
        appInfo().clusterId,
        std::make_unique<stubs::RealStubFactory<mgmtd::MgmtdServiceStub>>(std::move(ctxCreator)),
        config_.mgmtd_client());
  }

  // (3) 掛上心跳、設定監聽器，然後啟動
  mgmtdClient_->setAppInfoForHeartbeat(appInfo());
  mgmtdClient_->setConfigListener(ApplicationBase::updateConfig);
  mgmtdClient_->updateHeartbeatPayload(flat::MetaHeartbeatInfo{});
  folly::coro::blockingWait(mgmtdClient_->start(&tpg().bgThreadPool().randomPick()));
  auto mgmtdClientRefreshRes = folly::coro::blockingWait(mgmtdClient_->refreshRoutingInfo(/*force=*/false));
  XLOGF_IF(FATAL, !mgmtdClientRefreshRes, "Failed to refresh initial routing info!");

  // (4) storage client：吃 mgmtd client 的 routing info
  auto storageClient = storage::client::StorageClient::create(ClientId::random(appInfo().hostname),
                                                              config_.storage_client(),
                                                              *mgmtdClient_);
  XLOGF_IF(FATAL, !storageClient, "Failed to create storage client!");

  // (5) 檢查 appInfo
  auto appInfo = ApplicationBase::getAppInfo();
  XLOGF_IF(FATAL, !appInfo, "AppInfo not set!");
  XLOGF_IF(FATAL, !appInfo->nodeId, "Invalid nodeId {}", appInfo->nodeId);

  // (6) 註冊服務
  RETURN_ON_ERROR(addSerdeService(std::make_unique<SimpleExampleService>(), true));
  RETURN_ON_ERROR(addSerdeService(std::make_unique<core::CoreService>()));

  return Void{};
}
```

### 6.1 依賴鏈：一條直線

```
  net::Client (backgroundClient_)
      │  提供 serdeCtx(addr) → 一個能發 RPC 的 context
      ▼
  RealStubFactory<MgmtdServiceStub>
      │  把 context 包成 typed stub
      ▼
  MgmtdClientForServer
      │  · 定期心跳（setAppInfoForHeartbeat + updateHeartbeatPayload）
      │  · 訂閱設定更新（setConfigListener）
      │  · 維護 routing info（refreshRoutingInfo）
      ▼
  StorageClient::create(clientId, config, *mgmtdClient_)
      │  不自己連 mgmtd，接受一個已在維護 routing info 的 mgmtd client
      ▼
  （業務邏輯）
```

**`StorageClient::create` 收 mgmtd client 引用而非自己建立**，這是 3FS 的固定模式。它保證整個進程只有一份 routing info、只有一條 mgmtd 連線、只有一組心跳。

### 6.2 三行 mgmtd 接線各做什麼

```cpp
mgmtdClient_->setAppInfoForHeartbeat(appInfo());        // 我是誰（nodeId、serviceGroups、版本）
mgmtdClient_->setConfigListener(ApplicationBase::updateConfig);  // 設定推送的接收端
mgmtdClient_->updateHeartbeatPayload(flat::MetaHeartbeatInfo{}); // 心跳附帶的角色專屬資訊
```

`setConfigListener(ApplicationBase::updateConfig)` 是**設定熱更新的全部接線**——只要註冊這一行，`CONFIG_HOT_UPDATED_ITEM` 標記的欄位就能被 mgmtd 推送修改。使用者不需要寫任何 reload 邏輯。

`updateHeartbeatPayload(flat::MetaHeartbeatInfo{})` 送的是 **meta** 的心跳酬載——這是複製模板時沒改的殘留（simple_example 不是 meta server）。無害，因為 mgmtd 對 `CLIENT` 型節點不解讀這個欄位，但這是模板需要人工調整的地方之一。

`refreshRoutingInfo(force=false)` 失敗直接 FATAL——**沒有 routing info 就無法工作，寧可不啟動也不要半死不活。**

### 6.3 `blockingWait` 出現在協程程式碼裡

`beforeStart()` 回傳 `Result<Void>` 而非 `CoTryTask<Void>`，所以碰到協程 API 只能 `folly::coro::blockingWait`。這在**啟動路徑**上是可接受的（單次、無併發、失敗就退出），但如果在請求路徑上這樣寫就是嚴重的效能 bug。

`&tpg().bgThreadPool().randomPick()` 是把 mgmtd client 的背景任務排到 server 主執行緒池的背景池上，隨機挑一個 executor 分散負載。

### 6.4 一個 gotcha：nodeId 不得為 0

`:49` 檢查 `appInfo->nodeId` 不為 0，為 0 就 FATAL。但 `ServerAppConfig::allow_empty_node_id` 預設是 `true`（`src/core/app/ServerAppConfig.h:10`），`ServerAppConfig::init` 只在該旗標為 false 時才擋（`ServerAppConfig.cc:10`）：

```cpp
XLOGF_IF(FATAL, !allow_empty_node_id() && node_id() == 0, "node_id is not allowed to be 0");
```

**於是不帶 `--app_config.node_id` 啟動 `simple_example_main` 會通過 launcher 的檢查，然後在 `beforeStart()` FATAL。** 這是模板的一個粗糙處：要嘛把 `AppConfig` 改成 `allow_empty_node_id = false`（在 launcher 階段就報錯，訊息更清楚），要嘛放寬 `beforeStart` 的檢查。複製模板做新服務時應該擇一處理。

### 6.5 `addSerdeService` 的 `strict` 參數

```cpp
RETURN_ON_ERROR(addSerdeService(std::make_unique<SimpleExampleService>(), true));   // strict
RETURN_ON_ERROR(addSerdeService(std::make_unique<core::CoreService>()));            // 非 strict
```

`src/common/net/Server.h:41-52`：

```cpp
template <class Service>
Result<Void> addSerdeService(std::unique_ptr<Service> &&obj, bool strict = false) {
  for (auto &group : groups_) {
    if (group->serviceNameList().contains(std::string{Service::kServiceName})) {
      return group->addSerdeService(std::move(obj));
    }
  }
  if (strict) {
    return makeError(RPCCode::kInvalidServiceName);
  }
  return groups_.front()->addSerdeService(std::move(obj));
}
```

`strict = true`：服務名必須出現在某個 group 的 `services` 集合裡，否則回錯。
`strict = false`：找不到就 fallback 到第一個 group。

**業務服務應該一律用 `strict = true`**，否則設定檔把服務名打錯時，服務會悄悄跑到管理埠上——而管理埠是 TCP，效能與隔離都不對，且很難察覺。

`ServiceGroup::addSerdeService`（`src/common/net/ServiceGroup.h:38-41`）再把 group 的 `network_type` 傳下去：

```cpp
return serdeServices_.addService(std::move(obj), type.value_or(config_.network_type()) == Address::RDMA);
```

這個布林最終決定 `Services::addService` 要填 `services_[0]`（TCP）還是同時填 `[0]` 與 `[1]`（RDMA），見 §2.1。

### 6.6 `beforeStop`：逆序拆除

```cpp
Result<Void> SimpleExampleServer::beforeStop() {
  folly::coro::blockingWait([this]() -> CoTask<void> {
    if (mgmtdClient_) { co_await mgmtdClient_->stop(); }
  }());
  if (backgroundClient_) { backgroundClient_->stopAndJoin(); }
  return Void{};
}
```

**先停 mgmtd client（會註銷心跳），再停背景 client**——與建立順序相反。`mgmtdClient_` 用 `backgroundClient_` 的 context 發 RPC，反過來停會用到已釋放的資源。

### 6.7 四個生命週期鉤子

`net::Server` 提供四個虛擬函式（`src/common/net/Server.h:77-88`），`simple_example` 只用了兩個：

| 鉤子 | 呼叫時機 | 用途 |
|---|---|---|
| `beforeStart()` | `Server::start` 開頭，**在 group 開始監聽之前**（`Server.cc:39`） | 建立相依組件、註冊服務 |
| `afterStart()` | 所有 group 啟動後（`Server.cc:44`） | 啟動只有在能接請求後才有意義的背景任務 |
| `beforeStop()` | `stopAndJoin` 開頭，**在 group 停止之前**（`Server.cc:54`） | 通知外部「我要走了」（如註銷心跳） |
| `afterStop()` | 所有 group 停止後（`Server.cc:62`） | 釋放資源 |

**`beforeStart` 在監聽之前執行是關鍵性質**：服務註冊必須在這裡完成，否則第一批請求會找不到 handler。同時它也保證「一旦開始接受連線，所有相依組件都已就緒」——不會有半初始化狀態下收到請求的競態。

`stopAndJoin` 用 `stopped_.test_and_set()` 做冪等保護（`Server.cc:49-52`），所以解構子與顯式停止可以重複呼叫。

---

## 7. 步驟五：`main` 與兩階段啟動

`src/simple_example/main.cpp`（全文 8 行）：

```cpp
#include "common/app/TwoPhaseApplication.h"
#include "memory/common/OverrideCppNewDelete.h"
#include "simple_example/service/Server.h"

int main(int argc, char *argv[]) {
  using namespace hf3fs;
  return TwoPhaseApplication<simple_example::server::SimpleExampleServer>().run(argc, argv);
}
```

第二行的 `OverrideCppNewDelete.h` 必須在 `main.cpp`（而非任何 header）include，因為它在**全域作用域定義 `operator new` / `operator delete`**——每個連結單元只能有一份。詳見 [記憶體配置器包裝報告](memory_allocator_wrappers-記憶體配置器包裝深度剖析.md)。

### 7.1 `ApplicationBase::run` 的固定五步

`src/common/app/ApplicationBase.cc:49-74`：

```cpp
int ApplicationBase::run(int argc, char *argv[]) {
  Thread::blockInterruptSignals();                   // 1. 先擋掉訊號
  auto parseFlagsRes = parseFlags(&argc, &argv);     // 2. 解析 --app_config.* / --config.*
  XLOGF_IF(FATAL, !parseFlagsRes, "Parse flags failed: {}", parseFlagsRes.error());
  folly::init(&argc, &argv);                         // 3. folly 初始化（吃剩下的 gflags）
  if (FLAGS_release_version) { ... return 0; }
  auto initRes = initApplication();                  // 4. 子類別實作（Two/One-Phase）
  XLOGF_IF(FATAL, !initRes, "Init application failed: {}", initRes.error());
  auto exitCode = mainLoop();                        // 5. 等訊號
  memory::shutdown();
  stop();
  return exitCode;
}
```

`mainLoop()`（`:76-90`）掛上 SIGINT / SIGTERM / SIGUSR1 / SIGUSR2 的處理器，然後在 condition variable 上等待。

**先 `blockInterruptSignals()` 再在 `mainLoop` 裡 `unblockInterruptSignals()`** 的順序很重要：初始化期間（可能會建立大量執行緒）不接受訊號，否則新生的執行緒可能繼承到訊號遮罩狀態不一致。且所有子執行緒都在遮罩期間建立，於是訊號只會遞送到主執行緒。

### 7.2 `TwoPhaseApplication::initApplication`

`src/common/app/TwoPhaseApplication.h:36-70`：

```
Phase 1（Launcher，本地設定 + mgmtd 拉取）
  ├─ launcher_->init()                                       :42
  │    · appConfig_.init(FLAGS_app_cfg, ...)                  ServerLauncher.h:35
  │    · launcherConfig_.init(FLAGS_launcher_cfg, ...)        ServerLauncher.h:36
  │    · net::IBManager::start(launcherConfig_.ib_devices())  ServerLauncher.h:41
  │    · fetcher_ = make_unique<RemoteConfigFetcher>(...)     ServerLauncher.h:45
  │
  ├─ loadAppInfo(): buildBasicAppInfo(nodeId, clusterId)
  │                 + fetcher_->completeAppInfo(appInfo)      ServerLauncher.h:55-59
  │                 ← 向 mgmtd 補齊 hostname / podname / 版本
  │
  └─ initConfig(config_, configFlags_, appInfo_,
                [] { return launcher_->loadConfigTemplate(); })   TwoPhaseApplication.h:46
       ← 向 mgmtd 拉該 NodeType 的設定範本，再套上 --config.* 覆寫

  ── 中間 ──
  ├─ initCommonComponents(config_.common(), Server::kName, nodeId)  :49
  │    ← 日誌、監控、記憶體配置器統計一起啟動
  ├─ makeLogConfigUpdateCallback / makeMemConfigUpdateCallback      :51-52
  │    ← 讓 log 與 memory 設定也能熱更新
  └─ persistConfig(config_)                                         :55
       ← 把最終生效的設定落到本地檔，便於事後排查

Phase 2（Server）
  ├─ initServer():  server_ = make_unique<Server>(config_.server());
  │                 server_->setup();                               :92-95
  │                 ← 綁定監聽埠、建立執行緒池，但尚未接受連線
  ├─ startServer(): launcher_->startServer(*server_, appInfo_);      :99
  │                 → server.start(appInfo)
  │                   → beforeStart() → 各 group->start() → afterStart()
  └─ launcher_.reset()                                              :67
       ← Launcher 用完即丟，釋放它的 mgmtd 連線
```

`configPushable()`（`:86`）：

```cpp
bool configPushable() const final { return FLAGS_cfg.empty() && !FLAGS_use_local_cfg; }
```

**只有在沒有用本地設定檔時，mgmtd 才能推送設定。** 這是個安全閥：運維用 `--cfg` 手動指定設定來除錯時，不希望 mgmtd 半路把它覆蓋掉。

`onConfigUpdated()`（`:88`）每次設定更新後重新落檔，所以本地永遠有一份「當前生效設定」的快照。

### 7.3 `OnePhaseApplication`：什麼時候該用另一個

`migration` 用的是 `OnePhaseApplication`（`src/common/app/OnePhaseApplication.h`）。差別見 [migration 報告 §4](migration_main-資料遷移工具深度剖析.md)，簡表：

| | `TwoPhaseApplication` | `OnePhaseApplication` |
|---|---|---|
| 需要的型別別名 | `AppConfig` / `LauncherConfig` / `RemoteConfigFetcher` / `Launcher` / `CommonConfig` | 只需 `Config` 與 `kName` |
| nodeId | 向 mgmtd 註冊取得 | `--app_config.node_id` |
| 業務設定 | mgmtd 下發範本 | 純本地 TOML |
| 設定熱推送 | 支援 | 不支援 |
| 適用 | 叢集成員（META / STORAGE / MGMTD） | 一次性工具、壓測、遷移 |

**選擇規則**：這個進程需要出現在 mgmtd 的成員表與故障偵測範圍裡嗎？需要 → Two-Phase；不需要 → One-Phase。

`simple_example` 選了 Two-Phase 但 `kNodeType = CLIENT`，這個組合有點矛盾（CLIENT 型節點通常不需要中央設定管理）。它作為模板要展示「完整流程」，所以選了功能較多的那個；複製後應該依實際角色調整。

---

## 8. CMake：兩個目標，一條規則

`src/simple_example/CMakeLists.txt`：

```cmake
target_add_lib(simple_example core-app core-user core-service fdb simple_example-fbs mgmtd-client storage-client memory-common analytics)
target_add_bin(simple_example_main "main.cpp" simple_example)
```

`target_add_lib`（`cmake/Target.cmake:13-24`）：

```cmake
macro(target_add_lib NAME)
    file(GLOB_RECURSE FILES CONFIGURE_DEPENDS RELATIVE ${CMAKE_CURRENT_SOURCE_DIR} "*.cc" "*.h")
    add_library(${NAME} STATIC ${FILES} ${FBS_FILES})
    ...
endmacro()
```

**用 `GLOB_RECURSE` 而非顯式列檔**，搭配 `CONFIGURE_DEPENDS` 讓 CMake 在建置時偵測新檔。所以新增一個 `.cc` 不需要動 CMakeLists——這正是「複製整個目錄」這種模板用法能成立的前提。

注意 `main.cpp` **不會**被 glob 進去（模式只有 `*.cc` 與 `*.h`，`.cpp` 不在內），而是由 `target_add_bin` 單獨指定。這個 `.cc` / `.cpp` 副檔名的區分是刻意的：**`.cpp` = 進入點，`.cc` = 一般實作**。全 repo 一致（`meta.cpp`、`storage.cpp`、`mgmtd.cpp`、`monitor_collector.cpp` 全是進入點）。

依賴清單裡有兩個 `simple_example` 用不到的：`fdb`（FoundationDB client）與 `analytics`（Arrow/Parquet 結構化 trace）。它們在這裡是為了讓模板複製後「通常會需要」——但複製者應該刪掉不用的。`migration` 就沒刪（見 migration 報告 §7）。

---

## 9. `README.md`：這是腳本，不是文件

`src/simple_example/README.md` 全文 16 行：

```bash
svr_name='migration'
SrvName='Migration'
mkdir -p "src/$svr_name" && pushd src/simple_example && cp -rf --parents . "../$svr_name" && popd
mkdir -p "src/fbs/$svr_name" && pushd src/fbs/simple_example && cp -rf --parents . "../$svr_name" && popd
find "src/$svr_name" "src/fbs/$svr_name" -type f | xargs sed -i "s/simple_example/$svr_name/g"
find "src/$svr_name" "src/fbs/$svr_name" -type f | xargs sed -i "s/SimpleExample/$SrvName/g"
```

加上前面的五步文字說明（`:3-7`），第 5 步是唯一需要手動做的：把新目錄加進 `src/CMakeLists.txt` 與 `src/fbs/CMakeLists.txt`。

`sed` 完成後**還需要人工調整**的地方（腳本沒處理，也無法處理）：

| 項目 | 位置 | 為什麼 sed 做不到 |
|---|---|---|
| 服務 ID | `fbs/<name>/SerdeService.h` 的 `SERDE_SERVICE(..., 0xF0)` | 必須全域唯一，得看現有分配 |
| RPC 方法 | 整個 `SerdeService.h` + `Service.h` + `Service.cc` | 這是實際要設計的東西 |
| 監聽埠 | `Server.h` 的 8000 / 9000 | 同機多服務會衝突 |
| `kNodeType` | `Server.h:22` | 依角色決定 |
| 心跳酬載 | `Server.cc:37` 的 `flat::MetaHeartbeatInfo{}` | 這是 meta 的型別，殘留 |
| CMake 依賴 | `CMakeLists.txt:1` | 刪掉用不到的 `fdb` / `analytics`，加上需要的（如 `meta-client`） |
| 啟動模式 | `main.cpp` 的 `TwoPhaseApplication` | 一次性工具應改 One-Phase |
| nodeId 檢查 | `Server.cc:47-49` | 見 §6.4 |

`migration` 做了其中的服務 ID、RPC 方法、啟動模式三項，其餘全部沒改（見 migration 報告 §3）。

**這份 README 揭露了一個重要的框架特性**：3FS 的服務框架不是靠繼承基底類別或實作介面來擴充的，而是靠**複製一份完整的骨架再改**。這在小團隊、服務種類少（全 repo 只有 6 個服務）的情況下是務實的——沒有抽象洩漏的風險，每個服務可以自由偏離模板。代價是模板的改進不會自動傳播到已複製的服務（例如 §6.4 的 nodeId 檢查問題會被複製到每個新服務）。

---

## 10. 新增一個服務的完整檢查清單

把前面各節收斂成可執行的步驟：

```
□ 1. 執行 README.md 的 sed 腳本，命名新服務
□ 2. src/CMakeLists.txt 與 src/fbs/CMakeLists.txt 各加一行 add_subdirectory

□ 3. 設計 RPC 契約（src/fbs/<name>/SerdeService.h）
     □ 挑一個未使用的 service ID（grep "SERDE_SERVICE(" 確認）
     □ method ID 從 1 開始連號（避免 MethodExtractor 跳表膨脹）
     □ 請求/回應 struct 的欄位順序 = 序列化順序，日後只能往尾端加
     □ 每個欄位都要有合理的預設值（新版讀舊資料時會用到）

□ 4. 實作 handler（service/Service.h + .cc）
     □ 簽章：CoTryTask<RSP> METHOD(serde::CallContext &, const REQ &)
     □ 不拋例外——錯誤用 co_return makeError(...)
     □ 狀態放 private 區；必須執行緒安全
     □ 需要錯誤轉譯的話，加 Status onError(Status)（框架自動偵測）

□ 5. 調整 Server 骨架（service/Server.h）
     □ kNodeType 依實際角色設定
     □ 兩個 group 的監聽埠改成不衝突的值
     □ 保留「group 0 = 業務 RDMA / group 1 = 管理 TCP + 獨立執行緒池」的結構
     □ Config 裡加上業務需要的子設定物件

□ 6. 調整 beforeStart / beforeStop（service/Server.cc）
     □ 刪掉用不到的 client（simple_example 建了 storage client 卻沒持有——那是 bug，別照抄）
     □ 需要持有的組件記得加成員變數
     □ 業務服務用 addSerdeService(..., true) 開 strict
     □ updateHeartbeatPayload 換成自己角色的型別，或整行刪掉
     □ beforeStop 的拆除順序與建立相反

□ 7. 選擇啟動模式（main.cpp）
     □ 叢集成員 → TwoPhaseApplication（要提供 AppConfig/LauncherConfig/
       RemoteConfigFetcher/Launcher 四個別名）
     □ 一次性工具 → OnePhaseApplication（只需 Config 與 kName）
     □ 保留 #include "memory/common/OverrideCppNewDelete.h"

□ 8. 清理 CMake 依賴（CMakeLists.txt）
     □ 刪掉用不到的（fdb / analytics 常常用不到）
     □ 加上實際需要的（meta-client / kv / ...）

□ 9. 加測試（tests/<name>/）
     □ target_add_test(test_<name> test-fabric-lib <name>)
     □ 至少要有一個真正呼叫 RPC 的測試——別像 tests/migration 那樣只測啟停
```

---

## 11. 設計取捨總結

| 決策 | 得到什麼 | 付出什麼 |
|---|---|---|
| 契約用 C++ 巨集而非 IDL 檔 | 客戶端 stub 與伺服器端分派表由同一份原始碼生成，型別不一致在編譯期就爆掉；不需要 codegen 步驟 | 契約只能被 C++ 消費；Rust / Python 端只能走 FUSE 或手工實作（見 [trash_cleaner 報告 §6](trash_cleaner-Rust垃圾回收工具深度剖析.md) 的手工 ioctl ABI） |
| `array<ServiceWrapper, 65536>` 做服務分派 | 零雜湊、零分支的 O(1) 查表 | 每個 `ServiceGroup` 固定 ~4 MB 記憶體 |
| `consteval` 跳表做方法分派 | 編譯期建表，執行期一次陣列索引 | method ID 必須密集分配，否則表膨脹 |
| `if constexpr (is_void_v<T>)` 分離客戶端/伺服器端 | 一份定義兩端共用 | 模板巢狀較深，編譯錯誤訊息不友善 |
| handler 拋例外 → FATAL | 把「不該發生的事」變成立即可見的崩潰，並 dump 完整請求 | 沒有降級運行的餘地 |
| `beforeStart` 在監聽之前 | 保證「能接請求時所有組件都就緒」，無半初始化競態 | 啟動路徑上被迫用 `blockingWait` |
| 雙 group（業務 RDMA / 管理 TCP + 獨立執行緒池） | 業務卡死時仍可診斷；IB 故障時管理面仍可達 | 多一個埠、多一組執行緒池 |
| 三層設定（App / Launcher / 業務） | 前兩層本地解決啟動雞蛋問題，第三層中央管理 | 三個設定檔、三套 flag 前綴 |
| `configPushable()` 在有 `--cfg` 時關閉 | 除錯時 mgmtd 不會覆蓋手動設定 | 需要記住這個規則 |
| `CONFIG_HOT_UPDATED_ITEM` 標記熱更新欄位 | 熱更新能力是宣告式的，一行 `setConfigListener` 接線完畢 | 哪些欄位可熱更新只能查程式碼 |
| 擴充靠複製模板而非繼承介面 | 每個服務可自由偏離；無抽象洩漏 | 模板的改進不會傳播；模板的瑕疵會被複製（如 §6.4 的 nodeId 檢查、未持有的 storage client） |
| `GLOB_RECURSE` 收集原始碼 | 新增檔案不必改 CMake，讓「複製整個目錄」可行 | 建置系統對檔案系統狀態敏感 |

---

## 12. 檔案索引

| 檔案 | 行數 | 職責 |
|---|---|---|
| `src/simple_example/CMakeLists.txt` | 2 | `target_add_lib(simple_example ...)` 收集 `service/` 下所有 `.cc`/`.h`；`target_add_bin(simple_example_main "main.cpp" simple_example)`。依賴含用不到的 `fdb` 與 `analytics` |
| `src/simple_example/README.md` | 16 | **可執行的複製腳本**，非說明文件。`:10` 的範例變數就是 `migration`，`src/migration/` 即由此產出 |
| `src/simple_example/main.cpp` | 8 | 進入點。`TwoPhaseApplication<SimpleExampleServer>().run(argc, argv)`；必須在此 include `OverrideCppNewDelete.h`（全域 `operator new` 只能有一份定義） |
| `src/simple_example/service/Server.h` | 72 | **框架契約的完整清單**。`:21-22` `kName`/`kNodeType`；`:24-33` `CommonConfig` 日誌雙管線；`:35-40` 兩階段啟動的四個型別別名；`:42-56` 業務 `Config`（雙 group 埠設定 + 三個 client 子設定）；`:58-63` 生命週期鉤子；`:66-69` 成員 |
| `src/simple_example/service/Server.cc` | 68 | `:21-54` `beforeStart`：net client → mgmtd client（心跳/設定監聽/routing info）→ storage client（**建完即丟棄，是模板的 bug**）→ appInfo 檢查 → 註冊兩個服務；`:56-66` `beforeStop` 逆序拆除 |
| `src/simple_example/service/Service.h` | 21 | `SimpleExampleService : serde::ServiceWrapper<Self, SimpleExampleSerde>`；`DECLARE_SERVICE_METHOD` 巨集統一簽章；`:18-19` 空的 `private:` 是留給 service 狀態的位置 |
| `src/simple_example/service/Service.cc` | 21 | `:13-17` 唯一的 handler：`resp.message = req.message; co_return resp;` |
| `src/fbs/simple_example/CMakeLists.txt` | 1 | `target_add_lib(simple_example-fbs mgmtd-fbs core-user-fbs)`——契約層獨立成庫，客戶端只需連這個 |
| `src/fbs/simple_example/SerdeService.h` | 17 | RPC 契約。`:8-14` 請求/回應 struct；`:16` `SERDE_SERVICE(SimpleExampleSerde, 0xF0)` + `SERDE_SERVICE_METHOD(echo, 1, ...)` |

### 框架側的關鍵檔案

| 檔案 | 關係 |
|---|---|
| `src/common/serde/Service.h` | `:37-46` `ServiceWrapper` CRTP；`:48-78` `MethodExtractor` consteval 跳表；`:80-126` 四個核心巨集（`SERDE_SERVICE` / `_2` / `_METHOD` / `_SENDER` / `_REFL`）；`:128-151` `SERDE_SERVICE_CLIENT` |
| `src/common/serde/Services.h` | `:14` 建構時自動註冊內建 Echo 服務；`:16-32` `addService`（TCP/RDMA 雙填、重複 ID 檢查、`onError` 自動偵測）；`:38` `array<ServiceWrapper, 65536>[2]` |
| `src/common/serde/CallContext.h` | `:17-23` `ServiceWrapper`；`:35-38` `handle()` 兩層分派；`:40-44` `invalidId`；`:46-76` `call<F>()` 反序列化→呼叫→時間戳→回應；`:78-88` 錯誤處理與 `customOnError`；`:94-` `RDMATransmission`；`:119-128` `makeResponse` |
| `src/common/net/Server.h` | `:23-27` `Config`（兩個執行緒池 + 最多 4 個 group）；`:41-52` `addSerdeService` 與 `strict`；`:77-88` 四個生命週期鉤子 |
| `src/common/net/Server.cc` | `:16-21` 依 `use_independent_thread_pool` 分派執行緒池；`:23-33` `setup`；`:35-46` `start` 的鉤子順序；`:48-65` `stopAndJoin` 的冪等與逆序 |
| `src/common/net/ServiceGroup.h` | `:22-31` `Config`（`network_type` 預設 RDMA）；`:38-41` `addSerdeService` 傳遞 RDMA 旗標 |
| `src/common/app/ApplicationBase.cc` | `:49-74` `run()` 五步；`:76-90` `mainLoop()` 訊號處理 |
| `src/common/app/TwoPhaseApplication.h` | `:36-70` 兩階段 `initApplication`；`:86` `configPushable()`；`:91-103` `initServer` / `startServer` |
| `src/common/app/OnePhaseApplication.h` | 單階段替代方案，`migration` 與大部分工具使用 |
| `src/core/app/ServerLauncher.h` | `:24-78` `ServerLauncher<Server>` 模板；`:34-47` `init`；`:49-59` `loadConfigTemplate` / `loadAppInfo` |
| `src/core/app/ServerAppConfig.h` / `.cc` | `node_id` + `allow_empty_node_id`（預設 `true`，與 `Server.cc:49` 的檢查有落差） |
| `src/core/app/ServerLauncherConfig.h` | `cluster_id` / `ib_devices` / `client` / `mgmtd_client` / `allow_dev_version` |
| `src/common/utils/ConfigBase.h` | `:44-56` `CONFIG_OBJ`；`:58-` `CONFIG_OBJ_ARRAY`；`:115-116` `CONFIG_ITEM` / `CONFIG_HOT_UPDATED_ITEM` |
| `cmake/Target.cmake` | `:13-24` `target_add_lib`（`GLOB_RECURSE` + `CONFIGURE_DEPENDS`）；`:40-53` `target_add_bin`；`:55-68` `target_add_test` |
