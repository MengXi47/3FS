# hf3fs_common_shared —— 共用基礎設施深度剖析

> 對應原始碼：`src/common/`（248 個檔案，全專案最大目錄）、`src/memory/`
> CMake 目標：`src/common/CMakeLists.txt:10`
> 使用者：`mgmtd_main`、`meta_main`、`storage_main`、`hf3fs_fuse`、`monitor_collector_main`、`admin_cli`、`migration`、`simple_example` —— **全部 binary 無一例外**

---

## 0. 一句話總結

`hf3fs_common_shared` 是 3FS 的**地基**：它把「RPC 該長什麼樣」「結構體怎麼變成位元組」「設定怎麼寫、怎麼熱更新」這三件事一次性定死，然後所有上層服務都只是在填空。它的三大支柱各自都做了一個激進但一致的選擇——**RPC 層把 RDMA RC QP 硬扮成 stream socket 塞進同一個 epoll 迴圈**、**序列化層用「編譯期反射 + 倒著寫的緩衝區」換取零 IDL 與零欄位標籤**、**設定層用「成員初始化器的副作用」在建構期自建反射註冊表**。三者共通的哲學是：把執行期的靈活性換成編譯期的確定性，代價是巨集很醜、模板很深，而且改一個欄位順序就會炸掉整個叢集的相容性。

---

## 1. 產出物與依賴藍圖

### 1.1 兩個孿生目標

```cmake
# src/common/CMakeLists.txt
target_add_lib(common        memory-common version-info folly ibverbs scn::scn
                             clickhouse-cpp-lib-static toml11 libzstd_static 3fs_liburing)   # :6
add_dependencies(common MonitorCollectorService-fbs)                                          # :7
target_sources(common PRIVATE utils/Linenoise.c)                                              # :8

target_add_shared_lib(hf3fs_common_shared  <同一串依賴>)                                       # :10
add_dependencies(hf3fs_common_shared MonitorCollectorService-fbs)                             # :11
```

`target_add_lib` / `target_add_shared_lib` 定義在 `cmake/Target.cmake:13` 與 `:26`，兩者都用 `file(GLOB_RECURSE ... "*.cc" "*.h")` **無條件吞掉整個目錄**——沒有檔案清單，新增 `.cc` 不需要改 CMake。差別只有三點：

| | `common`（STATIC） | `hf3fs_common_shared`（SHARED） |
|---|---|---|
| 產物 | `libcommon.a` | `libhf3fs_common_shared.so` |
| IPO/LTO | 不啟用 | `target_enable_ipo()`（`Target.cmake:37`，Release 才生效） |
| 額外來源 | 顯式加入 `utils/Linenoise.c` | 不加（GLOB 只抓 `.cc`/`.h`，**`.c` 抓不到**） |

> **陷阱**：`Linenoise.c` 是 C 檔案，GLOB 模式沒有 `*.c`，所以只有靜態版透過 `target_sources` 補上。共享版裡沒有 linenoise —— 這對 `admin_cli` 的互動式輸入是有影響的，它必須連靜態版 `common`。

`src/memory/CMakeLists.txt` 的第一行也值得注意：

```cmake
set(CMAKE_CXX_VISIBILITY_PRESET default)
set(CMAKE_VISIBILITY_INLINES_HIDDEN 0)
```

`memory` 子樹刻意關掉 symbol 隱藏，因為 `GET_MEMORY_ALLOCATOR_FUNC_NAME "getMemoryAllocator"`（`src/memory/common/MemoryAllocatorInterface.h:3`）是要靠 `dlsym` 在執行期找出來的。

### 1.2 依賴逐一交代

| 依賴 | 用在哪 | 具體檔案 |
|---|---|---|
| **`memory-common`** | 全域 allocator 抽象；RDMA 緩衝與 IBSocket 緩衝的 `memalign`/`deallocate` 都走它 | `src/common/net/ib/IBSocket.cc:177`、`RDMABuf.cc:96` |
| **`version-info`** | `VersionInfo::commitHashShort()` 被塞進每一個 RPC 封包的 `Version.hash` 欄位 | `src/common/utils/VersionInfo.cc.in`、`serde/MessagePacket.h:17` |
| **`folly`** | 幾乎無所不在：`coro::Task`、`coro::Baton`、`Expected`（`Result`）、`IOBuf`、`CPUThreadPoolExecutor`、`atomic_shared_ptr`、`Synchronized`、`StampedPtr`、`MPMCQueue`、`crc32c`、`xlog`、`AsyncServerSocket` | 全域 |
| **`ibverbs`** | RDMA 全部。`ibv_create_qp` / `ibv_post_send` / `ibv_poll_cq` / `ibv_reg_mr` / `ibv_query_gid` | `src/common/net/ib/*` |
| **`scn::scn`** | 型別安全的字串掃描。用在 3 個地方：位址解析、序列化 map key 的還原、版本號解析 | `utils/Address.h:77`、`serde/Serde.h:626`、`utils/RenderConfig.cc:10` |
| **`clickhouse-cpp-lib-static`** | monitor 的 ClickHouse reporter（直寫 ClickHouse 而非走 collector） | `monitor/ClickHouseClient.cc` |
| **`toml11`** | **只有 serde 的 TOML 輸出用它**（`toml::value`） | `serde/Serde.cc:12` `#include "toml.hpp"` |
| **`libzstd_static`** | RPC 封包壓縮/解壓 | `net/WriteItem.h:87`、`net/Processor.cc:23` |
| **`3fs_liburing`** | **`src/common/` 內完全沒有任何 `io_uring` 呼叫**——這是傳遞性依賴，給下游 storage 的 `AioReadWorker` 用的 | （common 內零引用） |

**最非顯而易見的一點：專案裡同時存在兩套 TOML 函式庫，而且都佔用 `namespace toml`。**

- `toml11`（`third_party/toml11`，v3.7.1）→ 只被 `serde/Serde.cc` include，用來實作 `serde::Out<TomlObject>` / `In<TomlObject>`（`TomlObject : toml::value`，`Serde.cc:20`）。
- **toml++**（vendored 成單一標頭 `src/common/utils/Toml.hpp`，`TOML_HEADER_ONLY 0`，實作在 `utils/Toml.cc`）→ 被 `ConfigBase.h:26` include，設定系統全部用它的 `toml::table` / `toml::parse_file`。

兩者只靠「沒有任何 TU 同時 include 兩者」而共存。`Serde.h` 不 include `Toml.hpp`，`ConfigBase.h` 不 include `toml.hpp`，這條界線是隱性的、沒有任何編譯期保護。哪天有人在 `Serde.cc` 裡加一行 `#include "common/utils/ConfigBase.h"`，就會得到一堆難以理解的 ODR 錯誤。

---

## 2. 整體分層

```
┌──────────────────────────────────────────────────────────────────────────┐
│  app/          ApplicationBase → OnePhase / TwoPhaseApplication          │
│                訊號處理、設定載入/熱更新/持久化、AppInfo、ConfigStatus     │
├──────────────────────────────────────────────────────────────────────────┤
│  net/          Server ── ServiceGroup[0..3] ── Listener / Processor      │
│                Client ── IOWorker ── TransportPool ── Transport          │
│                                          │                               │
│                        ┌─────────────────┴──────────────────┐            │
│                        │ Socket（純虛介面：fd/poll/recv/send）│            │
│                        ├──────────────────┬─────────────────┤            │
│                        │ tcp/TcpSocket    │ ib/IBSocket     │            │
│                        │ (fd = socket fd) │ (fd = comp_chan)│            │
│                        └──────────────────┴─────────────────┘            │
│                        net/EventLoop（epoll，兩者共用）                    │
├──────────────────────────────────────────────────────────────────────────┤
│  serde/        Serde.h（反射 + 二進位/JSON/TOML 三後端）                   │
│                MessagePacket / Service.h（SERDE_SERVICE 巨集）            │
│                CallContext（server 側）/ ClientContext（client 側）        │
├──────────────────────────────────────────────────────────────────────────┤
│  kv/  IReadOnlyTransaction / IReadWriteTransaction / WithTransaction      │
│  logging/  folly::logging 的 handler / writer / formatter 外掛            │
│  monitor/  Recorder → Collector → Reporter(ClickHouse/Log/Collector)      │
├──────────────────────────────────────────────────────────────────────────┤
│  utils/  ConfigBase（設定）· Reflection+Thief（反射）· Result/Status      │
│          Duration/UtcTime · Coroutine · CoroutinesPool · Shards · ...     │
├──────────────────────────────────────────────────────────────────────────┤
│  src/memory/  GlobalMemoryAllocator（dlsym jemalloc/mimalloc）            │
└──────────────────────────────────────────────────────────────────────────┘
```

依賴方向嚴格由上往下，唯一的例外是 `serde/CallContext.h` 反過來 include 了 `net/Transport.h` 與 `net/ib/IBSocket.h`（`CallContext.h:3-4`）——因為 `CallContext` 要提供 `readTransmission()`/`writeTransmission()` 讓服務實作直接發 RDMA。serde 與 net 在這一點上是互相咬合的，不是乾淨的分層。

---

## 3. 支柱① RPC / 網路框架（`src/common/net/`）

### 3.1 三層抽象

```
    使用者程式碼
        │  MetaSerde<>::stat(ctx, req)             ← Service.h 巨集產生
        ▼
   serde::ClientContext::call<...>()               ← ClientContext.h:40
        │  1. 合併 options   2. Waiter::bind() 拿 uuid
        │  3. serde 序列化成 SerdeBuffer            ← WriteItem.h:52
        │  4. IOWorker::sendAsync(addr, WriteList)
        ▼
   net::IOWorker                                   ← IOWorker.h:23
        │  TransportPool::get(addr) → 有就用，沒有就非同步 connect
        ▼
   net::Transport                                  ← Transport.h:23
        │  MPSCWriteList 排隊 → doWrite → iovec 批次
        ▼
   net::Socket（虛介面）                            ← Socket.h:12
        ├── TcpSocket   ::send(iovec[], n) = ::writev
        └── IBSocket    ::send(iovec[], n) = memcpy 進 ring buffer + ibv_post_send
```

`Socket` 介面只有 7 個純虛函式（`Socket.h:12-40`）：`describe / peerIP / fd / poll / recv / send / flush / check`。整個上層完全不知道底下是 TCP 還是 RDMA——這是把 RDMA 塞進既有 epoll 架構的關鍵設計決策。代價寫在 `flush()` 上：TCP 版是空實作（`TcpSocket.h:41`），只有 RDMA 需要「把半滿的 buffer 也踢出去」。

### 3.2 線上封包格式

一個完整的 RPC 訊息 = **8 byte 裸 header + serde 序列化的 `MessagePacket`**。header 不走 serde（要能在收到完整訊息前就知道長度）。

```
 ┌────────────────────────────────────────────────────────────────────────┐
 │ MessageHeader（8 bytes，原生記憶體佈局，little-endian）                  │  net/MessageHeader.h:20
 ├──────────────────────────────────┬─────────────────────────────────────┤
 │ uint32 checksum                  │ uint32 size                         │
 │  ├ bit  0    : isCompressed       │  serde payload 的位元組數           │
 │  ├ bits 1-7  : 魔數 0x86 的低 7 位 │  上限 kMessageMaxSize = 512 MB      │
 │  └ bits 8-31 : crc32c 的高 24 位  │                                     │
 └──────────────────────────────────┴─────────────────────────────────────┘
 ┌────────────────────────────────────────────────────────────────────────┐
 │ serde(MessagePacket<Req>)  ← 見 §5.3 的二進位規則                        │
 ├────────────────────────────────────────────────────────────────────────┤
 │ varint32  tableLen         整個 packet table 的位元組數                  │
 │ uint64    uuid             Waiter 分配的請求編號（請求/回應配對鍵）       │
 │ uint16    serviceId                                                     │
 │ uint16    methodId                                                      │
 │ uint16    flags            bit0 IsReq / bit1 UseCompress / bit2 CtrlRDMA │
 │ ┌ Version ─────────────────────────────────────────────────────────┐   │
 │ │ varint32 len; uint8 major; uint8 minor; uint8 patch; uint32 hash │   │
 │ └──────────────────────────────────────────────────────────────────┘   │
 │ ┌ payload（PointerWrapper<Req>）────────────────────────────────────┐   │
 │ │ varint32 len; <serde(Req) 的欄位，可能是多張串接的 table>          │   │
 │ └──────────────────────────────────────────────────────────────────┘   │
 │ ┌ optional<Timestamp> ─────────────────────────────────────────────┐   │
 │ │ uint8 tag(0=NullOpt,1=HasValue);                                 │   │
 │ │ [ varint32 len; int64 clientCalled; clientSerialized;            │   │
 │ │   serverReceived; serverWaked; serverProcessed; serverSerialized;│   │
 │ │   clientReceived; clientWaked ]     ← 全部微秒，UtcClock          │   │
 │ └──────────────────────────────────────────────────────────────────┘   │
 └────────────────────────────────────────────────────────────────────────┘
```

#### 3.2.1 checksum 的三合一

```cpp
// net/MessageHeader.h:34
static inline uint32_t calcSerde(const uint8_t *data, size_t size, bool compressed = false) {
  uint32_t crc32c = folly::crc32c(data, size, 0);
  return (crc32c & ~0xffu) | kSerdeMessageMagicNum | compressed;   // kSerdeMessageMagicNum = 0x86
}
bool isSerdeMessage() const { return (checksum & 0xfeu) == kSerdeMessageMagicNum; }   // :24
bool isCompressed()    const { return checksum & 1; }                                  // :26
```

低 8 位的 CRC 被丟掉，換來「魔數（7 bit）+ 壓縮旗標（1 bit）」擠進同一個 `uint32`。這讓 header 維持 8 byte 對齊（`static_assert` 在 `:29` 強制），代價是校驗強度從 32 bit 降到 24 bit。對於一個已經有 RDMA/TCP 底層校驗的通道，這是合理的取捨；但它也意味著**任何非 serde 訊息一律被判為壞包並斷線**（`Processor.h:104`）。

#### 3.2.2 Timestamp 的原地補寫

`SerdeBuffer::create()` 序列化完成後，直接用指標算術回頭改寫已序列化的位元組：

```cpp
// net/WriteItem.h:67
if (packet.timestamp.has_value()) {
  auto timestamp = reinterpret_cast<hf3fs::serde::Timestamp *>(
      ptr->rawMsg() + ptr->header().size - sizeof(hf3fs::serde::Timestamp));
  if (packet.isRequest()) timestamp->clientSerialized = UtcClock::now().time_since_epoch().count();
  else                    timestamp->serverSerialized = UtcClock::now().time_since_epoch().count();
}
```

這行程式碼成立的前提有四個，全部是隱性的：
1. `timestamp` 必須是 `MessagePacket` 的**最後一個欄位**（`MessagePacket.h:69`）；
2. `Timestamp` 的 8 個 `int64_t` 是 `SerdeCopyable`，走 `out.value(o)` 原生 memcpy；
3. serde 二進位輸出是**倒著寫**的（見 §5.3），使得欄位在最終緩衝區裡是正序，因此最後 64 bytes 恰好就是 `Timestamp` 的原生佈局；
4. `Timestamp` 沒有任何額外的長度前綴夾在中間（varint 長度前綴在這 64 bytes 之**前**）。

換來的好處是：延遲埋點不需要重新序列化，也不需要為 timestamp 預留佔位符。這是整份程式碼裡最脆弱也最省的一處優化。

### 3.3 serviceId 全表

serviceId 是 `uint16_t`，由 `SERDE_SERVICE(NAME, ID)` 在編譯期寫死（`serde/Service.h:80`）。掃過 `src/` 產品程式碼的結果：

| serviceId | 服務名 | 宣告位置 | 提供者 binary | 註冊模式 |
|---:|---|---|---|---|
| 3 | `StorageSerde` | `src/fbs/storage/Service.h:8` | `storage_main` | RDMA（`StorageServer.cc:27` `strict=true`） |
| 4 | `MetaSerde` | `src/fbs/meta/Service.h:709` | `meta_main` | RDMA（`MetaServer.cc:64`） |
| **10** | `RDMAControl` | `src/common/net/RDMAControl.h:18` | **每一個 `net::Client`** | RDMA（`Client.h:45`） |
| **10** | `ClientAgentSerde` | `src/fbs/lib/Service.h:195` | **無實作**（OSS 樹中沒有任何 `addSerdeService`） | — |
| 11 | `IBConnect` | `src/common/net/ib/IBConnectService.h:18` | 每個 `network_type=RDMA` 的 ServiceGroup | **強制 TCP**（`Listener.cc:126` 傳 `Address::Type::TCP`） |
| 194 | `MonitorCollector` | `src/fbs/monitor_collector/MonitorCollectorService.h:13` | `monitor_collector_main` | RDMA |
| 217 | `Mgmtd` | `src/fbs/mgmtd/MgmtdServiceBase.h:7` | `mgmtd_main` | TCP（預設） |
| 240 (`0xF0`) | `SimpleExampleSerde` | `src/fbs/simple_example/SerdeService.h:16` | `simple_example` | RDMA |
| 241 (`0xF1`) | `MigrationSerde` | `src/fbs/migration/SerdeService.h:74` | `migration` server | RDMA |
| 10000 | `echo::Service` | `src/common/serde/Echo.h:13` | **每一個 `serde::Services`**（建構子強制加入） | RDMA |
| 10001 | `Core` | `src/fbs/core/service/CoreServiceBase.h:7` | mgmtd / meta / storage / simple_example / migration | TCP |

`tests/` 底下另有三個**測試專用**服務，它們不進產品 binary，但佔用的是同一個 `uint16_t` 編號空間：

| serviceId | 服務名 | 宣告位置 | 備註 |
|---:|---|---|---|
| **1** | `DemoService` | `tests/common/serde/TestService.cc:47` | ⚠️ 佔在最低號段，日後新增正式服務若從小號開始編最容易撞上 |
| 86 | `Echo`（測試用，與 `serde/Echo.h` 的 10000 不同） | `tests/common/net/Echo.h:26` | |
| 87 | `RDMAService` | `tests/common/net/ib/TestRDMA.cc:61` | |

掃描時要注意：`SERDE_SERVICE(X, N) { SERDE_SERVICE_METHOD(...); }` 常寫在同一行，所以用 `grep -v SERDE_SERVICE_METHOD` 過濾會漏掉服務定義本身。

> **serviceId 10 被兩個服務共用，但這不是 bug。** `RDMAControl`（`src/common/net/RDMAControl.h:18`）與 `ClientAgentSerde`（`src/fbs/lib/Service.h:195`）都宣告 ID = 10。關鍵在於**唯一性的作用域是單一 `serde::Services` 實例，而非整個進程**——`Services::addService` 的 `redundant service id` 檢查（`src/common/serde/Services.h:21-23`）作用在自己那份 `std::array<ServiceWrapper, 65536> services_[2]`（`:38`）上，而全專案有兩類彼此獨立的 `Services` 實例：
>
> | 實例 | 位置 | 誰註冊進去 |
> |---|---|---|
> | `net::Client::serdeServices_` | `src/common/net/Client.h:80` | `RDMAControl`（`Client.h:45`，`isRDMA=true`） |
> | `net::ServiceGroup::serdeServices_` | `src/common/net/ServiceGroup.h:74` | 所有 server 端服務；`net::Server` 持有多個 group（`src/common/net/Server.h:95`），`addSerdeService` 分派到其中一個 |
>
> 也就是說一個同時有 Client 與 Server 的進程（3FS 裡幾乎每個 binary 都是），身上至少有兩份互不相干的 65536 格服務表。入站請求走哪一份，取決於連線是掛在哪個 `Processor` 上——而 `Processor` 持有的是建構時傳入的那份 `Services` 參照（`src/common/net/Processor.h:45,214`）。所以即使日後把 `ClientAgentSerde` 實作出來註冊到某個 ServiceGroup，也不會觸發 `net::Client` 那份的重複檢查。
>
> **但「兩份表從不交會」這句話要說得更精確。** 有一條路徑會讓 server 建出的連線扮演 client 角色（`src/common/net/Server.h:36-38`）：
>
> ```cpp
> auto serdeCtxCreator() {
>   return [this](Address addr) { return serde::ClientContext(groups_.front()->ioWorker(), addr, options_); };
> }
> ```
>
> 它用 **ServiceGroup 0 的 IOWorker** 建連線（實際使用者：`src/mgmtd/MgmtdServer.cc:27`，mgmtd 用它建立對其他 mgmtd 節點的 stub）。這種連線上的**入站** request 會查 ServiceGroup 0 的服務表，而不是 `net::Client` 的。若對端在這條連線上回送 `RDMAControl::apply`（serviceId 10，這正是 `src/common/serde/CallContext.cc:18-19` 的反向 RPC 模式），查的就是 ServiceGroup 那格——目前是空的，因為 `RDMAControlImpl` 只註冊在 `net::Client`（`Client.h:45`）。
>
> 所以準確的說法是：**當前設定下兩份表的內容不重疊，但架構上沒有機制保證這一點**。若哪天有人把 `ClientAgentSerde`(10) 註冊進某個 ServiceGroup，而該 group 又透過 `serdeCtxCreator()` 對外建連線並收到反向的 `RDMAControl::apply`，兩者就會在同一格相撞。
>
> 唯一會出事的情境是有人把 `ClientAgentSerde` 註冊進 `net::Client` 的那份 `Services`——但那不是服務註冊的正常途徑。
>
> 另外值得記一筆：`ClientAgentSerde` 目前**是死宣告**。它在 `src/fbs/lib/Service.h:195` 定義了 16 個以上的方法（`statFs`／`stat`／`open`／`close`／`iovalloc`…），但 grep 全 repo（含 `tests/`、`benchmarks/`）只有**定義那一行**命中，零註冊、零呼叫。它是 USRBIO / FUSE 側規劃過但未落地的「client agent」服務，與 `TaosClient`（見 `monitor_collector_main` 報告 §8.4）、`jemalloc_wrapper`／`mimalloc_wrapper`（見記憶體配置器報告 §1）同屬「宣告完整但未接線」這一類。
>
> 注意 `addService` 的 `for (auto i = 0u; i <= uint32_t(isRDMA); ++i)`（`Services.h:19`）：`isRDMA=true` 時會**同時**佔用 TCP（`services_[0]`）與 RDMA（`services_[1]`）兩張表的同一格，`isRDMA=false` 則只佔 TCP 那張。所以 RDMA 服務在兩種傳輸上都可被呼叫，TCP-only 服務則不能走 RDMA 連線。

方法編號（節錄，完整清單見各 `Service.h`）：

| 服務 | methodId → 方法 |
|---|---|
| `IBConnect`(11) | 1=`query`（列舉本機 HCA/port/zone），2=`connect`（交換 QP 資訊並 accept） |
| `RDMAControl`(10) | 1=`apply`（RDMA 傳輸許可申請） |
| `Core`(10001) | 1=`echo` 2=`getConfig` 3=`renderConfig` 4=`hotUpdateConfig` 5=`getLastConfigUpdateRecord` 6=`shutdown` |
| `StorageSerde`(3) | 1=`batchRead` 2=`write` 3=`update` 5=`queryLastChunk` 6=`truncateChunks` 7=`removeChunks` 8=`syncStart` 9=`syncDone` 10=`spaceInfo` 11=`createTarget` 12=`queryChunk` 13=`getAllChunkMetadata` 16=`offlineTarget` 17=`removeTarget`（4/14/15 已回收留空） |
| `MetaSerde`(4) | 1=`statFs` … 21=`batchStatByPath`，50=`testRpc`（13=`truncate` 標註 deprecated 但**編號保留**） |
| `Mgmtd`(217) | 1=`getPrimaryMgmtd`，3..24（**2 號被跳過**，`MgmtdServiceDef.h` 無定義） |

**編號一旦分配就不回收**，這是這套框架唯一的版本相容手段——沒有欄位標籤、沒有服務描述交換，全靠「新增只能往後加」的紀律。

### 3.4 服務註冊與 O(1) 分派

```cpp
// serde/Services.h:38
std::array<CallContext::ServiceWrapper, 65536> services_[2];   // 0 for TCP, 1 for RDMA.
```

這是一張**直接以 serviceId 為索引的完整陣列**，兩份（TCP 一份、RDMA 一份）。`ServiceWrapper` 是 8(函式指標) + 16(pointer-to-member-function) + 8(void*) + 16(shared_ptr) = 48 bytes，所以每一個 `serde::Services` 實例靜態佔用 **2 × 65536 × 48 = 6 MiB**。每個 `net::Server` 的每個 `ServiceGroup` 各持有一份，每個 `net::Client` 也有一份。一台 storage 節點開 4 個 group 就是 24 MiB 純查表結構。

換來的是查表零成本：`getServiceById(id, isRDMA)` 就是一次 `at()`。

註冊邏輯有一個容易看漏的迴圈：

```cpp
// serde/Services.h:19
for (auto i = 0u; i <= uint32_t(isRDMA); ++i) {
  auto &service = services_[i][Service::kServiceID];
  ...
}
```

`isRDMA=false` → 只寫 `services_[0]`（TCP 表）。
`isRDMA=true` → **同時寫 `services_[0]` 和 `services_[1]`**。

也就是說「RDMA 服務」的語意其實是「RDMA **和** TCP 都能呼叫」，而「TCP 服務」才是真正的獨佔。這正是 `IBConnect` 必須顯式以 `Address::Type::TCP` 註冊的原因（`Listener.cc:126`）——它**必須**只在 TCP 表裡，因為它是用來建立 RDMA 連線的，若能經由 RDMA 呼叫就形成雞生蛋問題。`IBConnectService::query/connect` 開頭也各有一道防線：

```cpp
// net/ib/IBConnect.cc:100 / :143
if (!ctx.transport()->isTCP()) {
  XLOGF(ERR, "IBConnectService::query from a non TCP transport!");
  co_return makeError(StatusCode::kInvalidArg);
}
```

方法分派則是編譯期產生的跳表：

```cpp
// serde/Service.h:49
template <class T, class C, auto DEFAULT = nullptr>
class MethodExtractor {
 public:
  static auto get(uint16_t id) {
    constexpr MethodExtractor ins;                      // consteval 建構
    return id <= ins.kMaxThreadId ? ins.table[id] : DEFAULT;
  }
 private:
  static constexpr auto kMaxThreadId = MaxMethodId<FieldInfoList>;   // = max{ MethodInfo::id }
  std::array<Method, kMaxThreadId + 1> table;
};
```

`consteval` 建構子在編譯期把 `table[i]` 填成對應的 `&CallContext::call<FieldInfo>`，未定義的洞填 `DEFAULT`（= `&CallContext::invalidId`）。所以 `StorageSerde` 的 methodId 4/14/15 空洞在表裡是明確的 `invalidId`，不是未定義行為。

server 端的一次分派因此只有兩次間接跳轉：

```cpp
// serde/CallContext.h:35
CoTask<void> handle() {
  auto method = service_.getter(packet_.methodId);   // 陣列索引
  co_await (this->*method)();                        // pointer-to-member 呼叫
}
```

### 3.5 Address 的 5 種型別與分流

```cpp
// utils/Address.h:18
struct Address {
  uint32_t ip{};                                  // Network Byte Order
  uint16_t port{};
  enum Type : uint16_t { TCP, RDMA, IPoIB, LOCAL, UNIX };
  Type type = Type::TCP;
  using is_serde_copyable = void;                 // ← 8 bytes 整包 memcpy 進線上格式

  bool isTCP()  const { return type == TCP || type == IPoIB || type == LOCAL || type == UNIX; }
  bool isRDMA() const { return type == RDMA; }
};
static_assert(sizeof(Address) == sizeof(uint64_t));   // :104
```

**只有 `RDMA` 一種型別會走 `IBSocket`，其餘四種全部是 TCP socket。** 特別是 `IPoIB`——雖然名字有 IB，它就是「跑在 `ib*` 網卡上的普通 TCP」，走 `TcpSocket`。分流點只有一處：

```cpp
// net/Transport.cc:75
std::shared_ptr<Transport> Transport::create(Address addr, IOWorker &io_worker) {
  if (addr.isTCP())       return create(std::make_unique<TcpSocket>(), io_worker, addr);
  else if (addr.isRDMA()) return create(std::make_unique<IBSocket>(io_worker.config_.ibsocket()), io_worker, addr);
  return nullptr;
}
```

五種型別的實際用途：

| Type | 網卡篩選（`Listener.cc:33` `checkNicType`） | 用途 |
|---|---|---|
| `TCP` | `en*` / `eth*` / `bond*` / `xgbe*` / `custom_tcp_nic_prefix` | mgmtd、Core service |
| `RDMA` | **同 TCP**（RDMA 靠 TCP 建連） | meta/storage 資料面 |
| `IPoIB` | `ib*` | 沒有 RDMA 但有 IB 網路時的退路 |
| `LOCAL` | `lo*`，但 `setup()` 時 `addressList_` 記成 `TCP`（`Listener.cc:75`） | 單機測試 |
| `UNIX` | 不掃網卡，用 `/tmp/domain_socket.<port>`（`Address.h:16`、`:51`） | FUSE 本機 IPC；`Transport::getPeerCredentials()` 取 `SO_PEERCRED` 做身分驗證（`Transport.cc:145`） |

注意 `checkNicType` 對 `RDMA` 與 `TCP` 用**完全相同**的前綴規則（`Listener.cc:38-41`），註解說得很白：

> RDMA networks rely on TCP networks to establish connections. Therefore, when the network type is set to RDMA, a TCP network card is still required for listening and handshaking.

所以一個 `network_type = RDMA` 的 ServiceGroup，實際上是在**乙太網卡上開一個 TCP listener**，公告的 `Address` 型別是 `RDMA`，client 收到後會用同一組 `ip:port` 先建 TCP 控制連線：

```cpp
// net/Transport.cc:85
if (addr.isRDMA()) {
  auto tcpAddr = Address(addr.ip, addr.port, Address::TCP);      // ← 同 ip 同 port，只換型別
  auto tcpCtx = serde::ClientContext(ioWorker_, tcpAddr, connectOptions);
  co_return co_await ibSocket->connect(tcpCtx, timeout);
}
```

### 3.6 一次請求的完整時序

```
 CLIENT                                                          SERVER
   │                                                                │
   │ MetaSerde<>::stat(ctx, req)                                    │
   │ └─ ClientContext::call<...>()          ClientContext.h:40      │
   │    ├ options = *options_.load(); options.merge(custom)         │
   │    ├ Waiter::Item item;  uuid = Waiter::bind(item)   [棧上物件] │
   │    ├ packet{uuid, serviceId, methodId, flags=IsReq, version}   │
   │    ├ ts.clientCalled = UtcClock::now()                         │
   │    ├ WriteItem::createMessage(packet, options)                 │
   │    │   └ SerdeBuffer::create → serde 序列化 → 可選 ZSTD 壓縮    │
   │    │     → 補寫 ts.clientSerialized → 算 checksum               │
   │    ├ IOWorker::sendAsync(destAddr, WriteList{item})            │
   │    │   └ TransportPool::get(addr) →（必要時非同步 connect）      │
   │    │     Transport::send → MPSCWriteList::add → doWrite        │
   │    │     → writeAll() → iovec[64] 批次 → Socket::send          │
   │    ├ Waiter::schedule(uuid, options.timeout)   [計時器執行緒]   │
   │    └ co_await item.baton                        ← 協程掛起      │
   │                                                                │
   │  ═══════════ 網路 ═══════════════════════════════════════════▶  │
   │                                                                │
   │                          Transport::doRead（epoll 執行緒或 IO 池）│
   │                          └ MessageWrapper 切出完整訊息          │
   │                            → IOWorker::processMsg               │
   │                              → Processor::processMsg  Processor.h:50
   │                                ├ 若 in-flight ≥ max → 就地拆包  │
   │                                └ 否則丟到 executor_.pickNext()  │
   │                                  → unpackMsg → unpackSerdeMsg   │
   │                                    ├ 驗 checksum（不符→斷線）    │
   │                                    ├ 若壓縮 → ZSTD_decompress   │
   │                                    ├ serde::deserialize(packet) │
   │                                    ├ ts.serverReceived = now    │
   │                                    └ IsReq → tryToProcessSerdeRequest
   │                                       ├ 背壓/凍結/停機檢查       │
   │                                       ├ flags_ += kCountInc      │
   │                                       └ CoroutinesPool 或 executor
   │                                          → processSerdeRequest   │
   │                                            CallContext ctx(...)  │
   │                                            ctx.handle()          │
   │                                              ├ ts.serverWaked    │
   │                                              ├ deserialize(Req)  │
   │                                              ├ (obj->*method)()  │
   │                                              ├ ts.serverProcessed│
   │                                              └ makeResponse(rsp) │
   │                                                 flags=0（非 IsReq）
   │  ◀═══════════ 網路 ═══════════════════════════════════════════   │
   │                                                                │
   │ Transport::doRead → Processor::unpackSerdeMsg                   │
   │   ts.clientReceived = now                                       │
   │   非 IsReq → Waiter::instance().post(packet, buf)  Processor.h:154│
   │     └ find(uuid) 從 shard 取出並 erase → item.baton.post()       │
   │                                                                │
   │ 協程恢復                                                        │
   │   ├ 檢查 item.status（timeout / sendFailed）                    │
   │   ├ serde::deserialize(Result<Rsp>, item.packet.payload)        │
   │   ├ ts.clientWaked = now                                        │
   │   ├ 若 totalLatency ≥ logLongRunningThreshold → WARNING 日誌     │
   │   └ 若 reportMetrics → 上報 5 段延遲                             │
   ▼                                                                ▼
```

`Timestamp` 提供的 5 個派生指標（`serde/MessagePacket.h:37-41`）正好把這條路徑切乾淨：

```cpp
serverLatency()   = serverSerialized - serverReceived     // server 端全程
inflightLatency() = clientReceived   - clientSerialized   // 線上往返
networkLatency()  = inflightLatency  - serverLatency      // 純網路
queueLatency()    = serverWaked      - serverReceived     // server 排隊
totalLatency()    = clientWaked      - clientCalled       // 使用者感知
```

### 3.7 Waiter：uuid 分配、分片、與單執行緒堆式計時器

`Waiter` 是全域單例（`Waiter.cc:10`），負責「把回應叫醒等待中的協程」與「逾時」兩件事。

**uuid 分配**用了執行緒本地批次領取，把原子操作攤到 1/256：

```cpp
// net/Waiter.h:39
size_t bind(Item &item) {
  thread_local size_t start = 0, end = 0;
  if (UNLIKELY(start >= end)) {
    start = uuid_idx_.fetch_add(kShardsSize);      // kShardsSize = 256
    end = start + kShardsSize;
  }
  auto uuid = start++;
  shards_.withLock([&](Map &map) { map.emplace(uuid, item); }, uuid);
  return uuid;
}
```

**`Map = robin_hood::unordered_map<size_t, Item &>` 存的是參考，不是物件。** `Waiter::Item` 是 `ClientContext::call` 棧上（協程幀內）的局部變數（`ClientContext.h:48`）。這代表 map 裡永遠躺著一堆指向他人協程幀的參考，安全性完全靠「`post` / `error` / `timeout` 三條路徑都會 `find()` 並 **erase**」（`Waiter.h:143`）這個一次性契約撐著。任何讓 `call` 協程在 baton 被 post 前就析構的路徑（例如外部取消）都會造成懸空參考。

**計時器**不是時間輪，是「13 個寫入分片 + 單執行緒二元堆」：

```
   工作執行緒 A ──┐
   工作執行緒 B ──┼─▶ taskShards_[hash(thread_id) % 13]   ← 每個 shard 有自己的 mutex
   工作執行緒 C ──┘        {vector<Task>, nearestRunTime_}
                                   │
                                   │ exchangeTasks()（swap，O(1)）
                                   ▼
                        Timer 執行緒（Waiter::run，Waiter.cc:62）
                          ├ 逐 shard swap 出待辦，過濾 contains(uuid)
                          ├ push_heap 進 min-heap（Task::operator< 反向，Waiter.h:157）
                          ├ 彈出所有 runTime ≤ now 的 → Waiter::timeout(uuid)
                          └ cond_.wait_for(下一個 deadline - now)
```

三個細節：
- `schedule()` 只有在**新任務比本 shard 目前最近的 deadline 更早**時才回傳 true 並去搶全域鎖通知（`Waiter.cc:33`、`:52`）。穩態下絕大多數 schedule 完全不碰全域鎖。
- 從 shard 撈出來時就先 `contains(uuid)` 過濾（`Waiter.cc:80`）——已經完成的請求根本不會進堆，避免堆無限膨脹。
- `reserved_` 這個暫存 vector 在 capacity 超過 1 MB 時會 `shrink_to_fit()`（`Waiter.cc:74`），防止一次流量尖峰之後永久佔住記憶體。

### 3.8 逾時、重試、壓縮

**逾時只有一層**：`Waiter::schedule(uuid, options.timeout)`，預設 `kClientRequestDefaultTimeout = 1000_ms`（`RequestOptions.h:11`）。逾時後 `item.status = RPCCode::kTimeout`，然後：

```cpp
// serde/ClientContext.h:89
if (item.status.code() == RPCCode::kTimeout && holds_alternative<net::IOWorker*>(connectionSource_)) {
  if (item.transport) co_await item.transport->closeIB();          // RDMA 連線直接關掉
  std::get<net::IOWorker*>(connectionSource_)->checkConnections(destAddr_, Duration::zero());
}
```

**一次逾時就會拆掉整條連線**（`Duration::zero()` 表示「不管過期時間，全部檢查」）。這是很激進的策略，理由是在 RDMA 上一個請求逾時通常意味著 QP 已經進入 ERROR 或對端掛了，繼續復用只會讓後續請求全部逾時。

**重試是傳輸層的，不是應用層的**。`WriteItem` 帶 `retryTimes` / `maxRetryTimes`（`WriteItem.h:123`，預設 `kDefaultMaxRetryTimes = 1`），只在「連線斷掉、封包還沒送出去」時由 `Transport::tryToCleanUp` → `WriteList::extractForRetry()` → `IOWorker::retryAsync` 重投，中間睡 `wait_to_retry_send`（預設 100 ms，`IOWorker.h:30`）。**已經送出但沒回應的請求不會重試**——那是逾時的範疇，語意上交給呼叫端決定（meta/storage client 各自有 `ExponentialBackoffRetry` / `DefaultRetryStrategy`）。

**壓縮是雙向且不對稱協商的**：

```
client: options.compression.enable(size)  → level>0 且 size ≥ threshold
        → ZSTD_compress → header.checksum bit0 = 1
        → packet.flags |= UseCompress                     ClientContext.h:61
server: packet.useCompress()                              CallContext（Processor.h:167）
        → ctx.responseOptions().compression = { response_compression_level,
                                                response_compression_threshold }
```

也就是說：**client 只要壓了請求，server 就會壓回應**，但壓縮等級/門檻用的是 server 自己的設定（兩者都是 `CONFIG_HOT_UPDATED_ITEM`，`Processor.h:35-36`）。壓縮失敗時 `ZSTD_isError` 會靜默退回不壓（`WriteItem.h:95`）。

### 3.9 Transport 的 9-bit 狀態機

`Transport` 用單一 `std::atomic<uint32_t> flags_`（cacheline 對齊，`Transport.h:110`）表達整條連線的讀寫狀態：

```
 bit 0  kInvalidatedFlag      連線已作廢
 bit 1  kReadAvailableFlag    epoll 說可讀 / 有讀任務在跑
 bit 2  kReadNewWakedFlag     讀任務執行期間又被 epoll 喚醒過
 bit 3  kWriteAvailableFlag   epoll 說可寫 / 有寫任務在跑
 bit 4  kWriteNewWakedFlag    寫任務執行期間又被 epoll 喚醒過
 bit 5  kWriteHasMsgFlag      有待寫訊息 / 有寫任務在跑
 bit 6  kWriteNewMsgFlag      寫任務執行期間又有新訊息入列
 bit 7  kLastReadFinished
 bit 8  kLastWriteFinished
                                                        Transport.cc:33-41
```

核心是 `tryToSuspend<CheckAndRemove, WantToRemove, Name>()`（`Transport.cc:163`）這個「準備睡覺前再看一眼」的 CAS 迴圈：

```cpp
auto flags = flags_.load(std::memory_order_acquire);
while (true) {
  if (UNLIKELY(flags & kInvalidatedFlag)) return Action::Fail;
  else if (flags & CheckAndRemove) {          // 執行期間有新事件 → 不睡，重跑
    flags_ &= ~CheckAndRemove;
    return Action::Retry;
  }
  auto newFlags = flags & ~WantToRemove;      // 讓出「我在跑」的標記
  if (LIKELY(flags_.compare_exchange_strong(flags, newFlags))) return Action::Suspend;
}
```

這解決的是經典的「epoll ET 模式下，讀空了才要睡，但睡之前可能又來資料」競態。`kXxxNewWakedFlag` 是「事件在我工作期間發生過」的紀錄；只要它被設過就不睡，直接重跑。每一種 suspend/retry 都各自掛了一個 `monitor::CountRecorder`（`Transport.cc:171`、`:178`），名稱由模板參數 `MethodName` 在編譯期拼出來——`common_read_available_retry`、`common_write_has_msg_suspend` 等。

`send()` 的入口判斷也值得看：

```cpp
// Transport.cc:108
auto flags = flags_.fetch_or(kWriteHasMsgFlag | kWriteNewMsgFlag);
if ((flags & (kInvalidatedFlag | kWriteHasMsgFlag)) == 0 && (flags & kWriteAvailableFlag) != 0) {
  ioWorker_.startWriteTask(this, false);        // 我是第一個排隊的，且 socket 可寫 → 我來啟動寫任務
} else if (UNLIKELY(flags & kInvalidatedFlag)) {
  return mpscWriteList_.takeOut().extractForRetry();
}
```

`kWriteHasMsgFlag` 同時扮演「有訊息」與「已有寫任務在跑」兩個角色，因此「原本沒有這個 bit」就等價於「沒有人在寫，我負責啟動」。這種一 bit 多義的壓縮讓整個判斷只需要一次 `fetch_or`。

### 3.10 執行緒模型

```
 ThreadPoolGroup（net/ThreadPoolGroup.h:16）
 ├─ procThreadPool_  CPUExecutorGroup  num_proc_threads=2     ← 拆包、跑服務協程
 ├─ ioThreadPool_    CPUExecutorGroup  num_io_threads=2       ← Transport::doRead/doWrite
 ├─ bgThreadPool_    CPUExecutorGroup  num_bg_threads=2       ← 背景任務、統計
 └─ connThreadPool_  folly::IOThreadPoolExecutor  num_connect_threads=2
                                                              ← listener accept、非同步 connect、retry

 EventLoopPool（net/EventLoop.h:71）  num_event_loop=1（IOWorker.h:31）
 └─ 每個 EventLoop 一條 jthread，一個 epoll fd + 一個 eventfd

 Waiter 的 Timer 執行緒（全域單例，Waiter.cc:17 命名為 "Timer"）
 IBManager 的 eventLoop_ + IBSocketManager 的 timerfd（200 ms 週期，IBSocket.cc:1196）
```

`CPUExecutorGroup` 不是單一 `CPUThreadPoolExecutor`，而是 **N 個各持一條執行緒的 executor**（`utils/CPUExecutorGroup.h:36`），提供 6 種策略：

```cpp
enum class ExecutorStrategy {
  SHARED_QUEUE,      // 退化成一個標準 CPUThreadPoolExecutor
  SHARED_NOTHING,    // 每執行緒獨立佇列，零共享
  WORK_STEALING,
  ROUND_ROBIN,       // Config 預設
  GROUP_WAITING_4,   // 4 條一組共用等待
  GROUP_WAITING_8,
};
```

`pickNext()` 是 `next_.fetch_add(1) % size()`，`pickNextFree()` 則會挑一個沒在忙的。`IOWorker::startReadTask` 用 `pickNextFree()`（`IOWorker.cc:123`），`Processor::processMsg` 用 `pickNext()`（`Processor.h:56`）——讀任務怕排隊，拆包任務只要均勻。

**RDMA / TCP 可以各自決定要不要在 epoll 執行緒裡直接做 I/O**：

```cpp
// IOWorker.cc:119
if ((transport->isTCP()  && config_.read_write_tcp_in_event_thread()) ||
    (transport->isRDMA() && config_.read_write_rdma_in_event_thread())) {
  transport->doRead(error, logError);                    // 就地做，省一次排程
} else {
  executor_.pickNextFree().add([tr = transport->shared_from_this(), ...]{ tr->doRead(...); });
}
```

兩個開關都是 `CONFIG_HOT_UPDATED_ITEM`，預設 false。開啟後 epoll 執行緒會被 I/O 佔住，但省掉一次跨執行緒排程——對小訊息高頻場景有意義。

**協程池有兩種**：

- `CoroutinesPool<Job>`（`utils/CoroutinesPool.h:33`）：固定 `coroutines_num` 條協程，`BoundedQueue` 有界佇列，可選 work stealing（每協程一個子佇列，偷取時先 `timedWait` 自己的佇列 5 ms 再去偷別人的，`CoroutinesPool.h:130`）。
- `DynamicCoroutinesPool`（`utils/DynamicCoroutinesPool.h:14`）：`threads_num` 與 `coroutines_num` 都是 `CONFIG_HOT_UPDATED_ITEM`，透過 `ConfigCallbackGuard` 在設定變更時線上增減協程數。`Processor` 用它：

```cpp
// net/Processor.h:190
if (coroutinesPoolGetter_) {
  coroutinesPoolGetter_(packet).enqueue(processSerdeRequest(...));   // 依 packet 選池
} else {
  processSerdeRequest(...).scheduleOn(&executor_.pickNext()).start();
}
```

`coroutinesPoolGetter_` 的簽章是 `DynamicCoroutinesPool &(const serde::MessagePacket<> &)`——**可以依 serviceId/methodId 把不同 RPC 導向不同協程池**，這是 meta server 做讀寫隔離的鉤子。

### 3.11 連線池與重連

```
TransportPool（net/TransportPool.h:67）
 ├ Shards<robin_hood::unordered_map<Address, TransportSet>, 32>   ← 依位址分 32 片
 └ folly::ThreadLocal<map<TransportCacheKey, weak_ptr<Transport>>>  ← TLS 快取，免鎖

TransportSet（:15）
 ├ map<TransportPtr, uint32_t> transports_
 └ vector<iterator> idxToTransport_          ← 同一位址最多 max_connections 條並行連線
```

`TransportCacheKey = {Address, idx}`（`:49`）——同一個位址可以有多條連線（`max_connections`，預設 1），`idx` 決定用哪一條。TLS 快取存 `weak_ptr`，斷線後自然失效，不需要主動清理。

連線建立是**非同步且不阻塞送出**的：

```cpp
// IOWorker.cc:80
TransportPtr IOWorker::getTransport(Address addr) {
  auto [transport, doConnect] = pool_.get(addr, *this);
  if (doConnect) startConnect(transport, addr).scheduleOn(&connExecutor_).start();
  return transport;        // ← 立刻回傳，連線還沒好
}
```

呼叫端拿到的是一個尚未連上的 `Transport`，訊息直接進 `MPSCWriteList` 排隊，等 `kWriteAvailableFlag` 被 epoll 設起來才真正寫出去。

RDMA 建連額外套了併發限流：

```cpp
// IOWorker.cc:98
if (transport->isRDMA()) {
  auto guard = co_await connectConcurrencyLimiter_.lock(addr);      // 每位址預設 4 條並行
  CO_RETURN_AND_LOG_ON_ERROR(co_await transport->connect(addr, config_.rdma_connect_timeout()));
} else {
  CO_RETURN_AND_LOG_ON_ERROR(co_await transport->connect(addr, config_.tcp_connect_timeout()));
}
```

`ConcurrencyLimiter<Address>`（`utils/ConcurrencyLimiter.h:22`）是 16 分片的「key → {當前併發數, 等待中的 baton 佇列}」。這道限流的存在理由很實際：RDMA 建連要跑兩次 RPC + `ibv_create_qp` + 註冊 1 MB 記憶體，一個節點重啟時上千個 client 同時撲上來會直接打爆 HCA 的 QP 建立速率。

定期巡檢由 `ServiceGroup::checkConnectionsRegularly()` 驅動，週期 `check_connections_interval`（60 s），過期時間 `connection_expiration_time`（1 天，`ServiceGroup.h:26-27`）。`Transport::expired()` 用 `lastUsedTime_`（每次 `doRead` 更新，`Transport.cc:186`）判斷。

### 3.12 背壓與拒絕

`Processor` 把「停機旗標 + 兩個凍結旗標 + in-flight 計數」全部塞進一個 `atomic<size_t> flags_`：

```cpp
// net/Processor.h:220
constexpr static size_t kStopFlag   = 1;
constexpr static size_t kFrozenTCP  = 2;
constexpr static size_t kFrozenRDMA = 4;
constexpr static size_t kCountInc   = 8;      // in-flight 計數從 bit 3 開始
size_t processingRequestsNum(size_t flags) const { return flags / kCountInc; }
```

三道防線：

1. **拆包階段的軟背壓**（`Processor.h:51`）：in-flight ≥ `max_processing_requests_num`（4096）時，**不再丟到執行緒池，改在當前（epoll/IO）執行緒就地拆包**。這會拖慢讀取，形成天然的 TCP 反壓，而不是丟棄。
2. **入列階段的硬拒絕**（`Processor.h:174`、`:184`）：`needRefuse` 為真時直接回 `RPCCode::kRequestRefused`。注意這裡做了兩次檢查——`fetch_add` 之前一次、之後再一次，因為 `fetch_add` 本身沒有上限保護。
3. **凍結**（`setFrozen`）：TCP / RDMA 可分別凍結，凍結時請求**直接丟棄、不回應**（`Processor.h:178` 只有 `return`）。這是給優雅下線用的——讓 client 自己逾時並切走，而不是收到一個明確的錯誤而立刻重試。

`stopAndJoin()` 則是先 `fetch_or(kStopFlag)` 拒絕新請求，再自旋等 in-flight 歸零（`Processor.h:66`）。

---

## 4. 支柱①-b RDMA 子系統（`src/common/net/ib/`）

這是整個 common 裡技術密度最高的部分：`IBSocket.cc` 1262 行 + `IBDevice.cc` 843 行 + `IBConnect.cc` 743 行 + `IBSocket.h` 582 行。

### 4.1 控制平面 / 資料平面分工

```
 ┌─────────────────── 控制平面（TCP）────────────────────┐
 │  serviceId 11  IBConnect                              │
 │    method 1  query    → 列舉本機所有 HCA/port/zone     │
 │    method 2  connect  → 交換 QP 資訊，server 端 accept │
 │  serviceId 10  RDMAControl                            │
 │    method 1  apply    → RDMA 傳輸許可（並發限流）       │
 └───────────────────────────────────────────────────────┘
                              │ 建連完成後
                              ▼
 ┌─────────────────── 資料平面（RC QP）──────────────────┐
 │  IBV_WR_SEND            → 模擬 stream socket 的資料     │
 │  IBV_WR_SEND_WITH_IMM   → ACK（credit 回補）/ 連線探針  │
 │  IBV_WR_RDMA_WRITE_WITH_IMM → CLOSE 訊息                │
 │  IBV_WR_RDMA_WRITE（0 長度）→ 活性探測（check）          │
 │  IBV_WR_RDMA_READ / WRITE → 真正的大塊資料搬運           │
 └───────────────────────────────────────────────────────┘
```

`IBConnect` 與 `RDMAControl` 都是「基礎設施自帶的服務」，不需要任何上層 binary 顯式註冊：
- `IBConnect` 由 `Listener::start()` 在 `network_type == RDMA` 時自動加入（`Listener.cc:125`）；
- `RDMAControl` 由 `net::Client::start()` 自動加入（`Client.h:45`）；
- `echo::Service`(10000) 由 `serde::Services` 建構子自動加入（`Services.h:14`）。

### 4.2 IBManager / IBDevice / IBPort / zone

```
IBManager（單例，IBDevice.h:206）
 ├ config_        IBConfig
 ├ devices_       vector<IBDevice::Ptr>          最多 kMaxDeviceCnt = 4
 ├ zone2port_     multimap<zone, {dev, portNum}>
 ├ eventLoop_     專屬 epoll（給 IBSocketManager 與 async event handler）
 ├ socketManager_ IBSocketManager（優雅關閉/drain）
 ├ devBgRunner_   定期 updatePort（重掃 port 狀態）
 └ devEventHandlers_  ibv 非同步事件（port down/up、QP fatal）
```

`IBManager::startImpl` 開頭先處理 fork 安全：

```cpp
// IBDevice.cc:762
if (!forkInited && config.fork_safe()) {
  auto ret = ibv_fork_init();
  ...
}
```

`fork_safe` 預設 true（`IBDevice.h:73`）。這對 FUSE client 很關鍵——已註冊的 MR 在 `fork()` 後如果沒有 `ibv_fork_init`，COW 會讓 HCA 寫到錯誤的實體頁。

**zone 是 3FS 自創的拓樸概念**，不是 IB 的原生概念。`IBConfig::Subnet` 把「網段 → zone 名稱列表」對應起來（`IBDevice.h:60`），`getZonesByAddrs()`（`IBDevice.cc:487`）依 port 上綁的 IP 反查它屬於哪些 zone。`query` RPC 會把每個 port 的 zone 清單送給對端，`selectDevice()` 用 `std::set_intersection` 找交集：

```cpp
// IBConnect.cc:201
std::set_intersection(remotePort.zones.begin(), remotePort.zones.end(),
                      localPort.zones.begin(),  localPort.zones.end(),
                      std::inserter(zones, zones.begin()));
```

選擇策略有三層（`IBConnect.cc:262` `selectDevice`）：

```
for checkZone in {true, false}:            ← 先嚴格比對 zone，再退回任意 zone
    if !checkZone && !allow_unknown_zone: break
    findMatchDevices()  →  ibMatches[] / roceMatches[]（依 link_layer 分類）
    allMatches = ibMatches + roceMatches
    if !allMatches.empty():
        idx = counter % allMatches.size()
        if !ibMatches.empty() && prefer_ibdevice:      ← 預設 true
            idx = counter % ibMatches.size()           ← 只在 IB 裡輪詢
        return allMatches[idx]
```

`counter` 是**每個對端位址一個的遞增計數器**（`IBConnect.cc:376`），第一次用 `folly::Random::rand64()` 初始化：

```cpp
auto guard = config_.roundRobin_.wlock();
if (auto iter = guard->find(addr); iter != guard->end()) cnt = ++iter->second;
else { cnt = folly::Random::rand64(); guard->insert({addr, cnt}); }
```

隨機起點 + 遞增 = 多條連線到同一對端時，會**輪流用不同的 HCA**，而不同節點的起點又不同，避免所有節點都從 mlx5_0 開始造成第一張卡過載。這個 `roundRobin_` 是 `IBSocket::Config` 的 `mutable folly::Synchronized` 成員（`IBSocket.h:118`）——把可變狀態掛在設定物件上，是為了讓它在整個 IOWorker 生命週期內共享。

### 4.3 RoCE GID 的手工探測

`IBConnectConfig::gid_index` 欄位標著 `// unused`（`IBConnect.h:78`），`IBSocket::Config` 的 `gid_index` 設定項與 `IBConfig::default_gid_index` 都被註解掉了（`IBSocket.h:93`、`IBDevice.h:76`），`toIBConnectConfig()` 直接寫死 `.gid_index = 0`（`IBSocket.cc:148`）。真正的 GID 是執行期掃出來的：

```cpp
// IBDevice.cc:94  註解原文：
// Ubuntu 20.04's rdma-core doesn't have ibv_query_gid_ex, and ibv_query_gid_type is a private
// symbol, so we have to implement this.
Result<std::pair<ibv_gid, uint8_t>> queryRoCEv2GID(ibv_context *ctx, uint8_t portNum) {
  for (uint8_t index = 0; index < 32; index++) {
    ibv_gid gid;
    auto ret = ibv_query_gid(ctx, portNum, index, &gid);
    if (ret < 0) { if (ret == ENODATA) continue; return makeError(...); }
    auto ip = folly::IPAddressV6::fromBinary(folly::ByteRange(gid.raw, sizeof(gid.raw)));
    if (ip.isZero())      continue;                      // 未配置
    if (ip.isLinkLocal()) continue;                      // fe80:: → RoCE v1
    auto path = Path(ctx->device->ibdev_path) /
                fmt::format("ports/{}/gid_attrs/types/{}", portNum, index);
    std::string gidType; folly::readFile(path.c_str(), gidType);      // ← 讀 sysfs
    if (!gidType.starts_with(V2_TYPE)) continue;                      // "RoCE v2"
    return std::pair<ibv_gid, uint8_t>(gid, index);
  }
  return makeError(RPCCode::kIBOpenPortFailed, "RoCE v2 GID not found");
}
```

**繞過 verbs API、直接讀 `/sys/class/infiniband/<dev>/ports/<N>/gid_attrs/types/<I>`** 來判斷 GID 版本，只為了在 Ubuntu 20.04 的 rdma-core 上能編過。這是全份程式碼裡最「務實勝過優雅」的一段。探測結果存進 `IBPort::rocev2Gid_`（`IBDevice.h:200`），`getRoCEv2Gid()` 回傳 `{gid, index}`，index 在轉 RTR 時填進 `attr.ah_attr.grh.sgid_index`（`IBConnect.cc:655`）。

`IBPort` 建構子還有一道硬檢查：`XLOGF_IF(FATAL, (dev && isRoCE() && !rocev2Gid_), "{}:{} doesn't find RoCE v2 GID")`（`IBDevice.cc:738`）——找不到 RoCE v2 GID 直接 abort，不允許退回 v1。

### 4.4 非對稱握手

這是最違反直覺的部分：**client 走完 INIT→RTR→RTS 並主動發探針，server 只走到 RTR，靠收到探針才轉 RTS。**

```
 CLIENT (IBSocket::connect, IBConnect.cc:338)        SERVER (IBConnectService::connect, :134)
     │                                                        │
     │ 建立 TCP ClientContext 到同 ip:port                     │
     │                                                        │
     ├──── IBConnect::query (svc 11, m 1) ───────────────────▶│
     │                                                        │ 列舉所有 active port
     │◀─── IBQueryRsp{devices[{dev,name,ports[{port,          │ 附上 zones 與 link_layer
     │       zones,link_layer}]}]} ───────────────────────────┤
     │                                                        │
     │ selectDevice(zone 交集 / prefer_ibdevice / round-robin)│
     │ openPort → checkPort → connectConfig_ = toIBConnectConfig(isRoCE)
     │ checkConfig()  ← 對照本機 device attr 驗證所有上限       │
     │ qpCreate()   comp_channel(O_NONBLOCK) + CQ + QP(RC)    │
     │ initBufs()   一次 memalign + 一次 ibv_reg_mr            │
     │ qpInit()     ──▶ IBV_QPS_INIT                          │
     │ state_ = CONNECTING                                    │
     │                                                        │
     ├──── IBConnect::connect (svc 11, m 2) ─────────────────▶│
     │      IBConnectReq{ 我的 hostname/dev/port/mtu/qp_num/  │ dev = IBDevice::get(req.target_dev)
     │                    lid 或 gid,                          │ port = dev->openPort(req.target_port)
     │                    target_dev/target_devname/          │ new IBSocket(config_, port)
     │                    target_port,                        │ accept(peerIP, req, acceptTimeout):
     │                    config ← 整份 IBConnectConfig }      │   connectConfig_ = req.config  ★
     │                                                        │   checkConfig()
     │                                                        │   qpCreate() / initBufs()
     │                                                        │   qpInit()  ──▶ INIT
     │                                                        │   setPeerInfo(ip, req)
     │                                                        │   qpReadyToRecv() ──▶ RTR
     │                                                        │     └ postRecv() × qpMaxRecvWR()
     │                                                        │   state_ = ACCEPTED          ★★
     │                                                        │   acceptTimeout_ = now + 15s
     │◀─── IBConnectRsp{ server 的 dev/port/mtu/qp_num/gid } ─┤   accept_(std::move(ibsocket))
     │                                                        │     → IOWorker::addIBSocket
     │                                                        │     → EventLoop::add(EPOLLIN|OUT|ET)
     │ setPeerInfo(...)                                       │     → Listener::checkRDMA 起 15s 監看
     │ qpReadyToRecv() ──▶ RTR  + postRecv × N                │
     │ qpReadyToSend() ──▶ RTS                                │
     │   sendBufs_.push(bufCnt); rdmaSem_.signal(max_rdma_wr);│
     │   ackBufAvailable_ = qpAckBufs()                       │
     │                                                        │
     ├──── postConnectProbe() ═══════════════════════════════▶│  (RC QP，非 TCP)
     │      SEND_WITH_IMM, sg_list=nullptr, num_sge=0,        │
     │      imm = ImmData::ack(0), SIGNALED,                  │  onRecved(): state == ACCEPTED
     │      wr_id = WRId::send(0)                             │    → qpReadyToSend() ──▶ RTS
     │                                                        │    → state_ = READY
     │                                                        │    → events |= kEventWritableFlag
     │◀─ 本機 CQ 完成 → onSended(): CONNECTING → READY         │  onImmData(ACK): sendAcked_ += 0
     ▼                                                        ▼
```

★ **server 完全採信 client 送來的 `IBConnectConfig`**（`IBConnect.cc:465` `connectConfig_ = req.config;`），只用 `checkConfig()` 對照本機 HCA 的 `max_sge` / `max_qp_wr` / `max_cqe` / `max_qp_rd_atom` 做上限驗證（`IBConnect.cc:517`）。這是**單向的參數協商**——client 說了算。這麼設計的理由是 ring buffer 的 credit 算術必須兩端完全一致：`buf_ack_batch`、`send_buf_cnt`、`buf_signal_batch` 只要差一個數字，credit 就會漏或超發。與其做雙向協商，不如讓一端獨裁。

★★ **為什麼 server 不轉 RTS？** 三個理由疊在一起：

1. **時序**：server 回應 `IBConnectRsp` 的那一刻，client 的 QP 還停在 INIT。若 server 立刻轉 RTS 並發任何東西（哪怕只是 ACK），對端 QP 沒準備好，會產生 retry 甚至 RNR，浪費 `retry_cnt`（預設 7）配額。
2. **驗證**：收到探針就等於證明「對端 QP 已經到 RTS 且路徑可通」。這比任何超時猜測都可靠。
3. **半開連線清理**：client 若在 RTR 之後崩潰，server 的 socket 會永遠停在 `ACCEPTED`。三道機制兜住它：
   - `Listener::checkRDMA()` 睡 `rdma_accept_timeout`（15 s）後檢查 `checkConnectFinished()`，還在 ACCEPTED 就 `invalidate()`（`Listener.cc:210-225`）；
   - `IBSocket::check()` 在 ACCEPTED 狀態下比對 `acceptTimeout_`（`IBSocket.cc:756`）；
   - 兩者都會 `acceptTimeout.addSample(1)` 上報指標。

探針本身的設計也很講究（`IBSocket.cc:909`）：

```cpp
// 註解原文：Use IBV_WR_SEND_WITH_IMM with ACK(0) as connect probe. This guarantees backward
// compatibility (sendAcked_ += 0) and works well with virtualized RDMA environments.
// Note: Unlike regular postSend, this does NOT consume a send buffer (sg_list = nullptr).
ibv_send_wr wr{
    .wr_id = WRId::send(0),          // signalCount = 0 → onSended 時 sendSignaled_ += 0
    .sg_list = nullptr, .num_sge = 0,
    .opcode = IBV_WR_SEND_WITH_IMM,
    .send_flags = IBV_SEND_SIGNALED,
    .imm_data = ImmData::ack(0),     // → onImmData 時 sendAcked_ += 0
};
```

三個「0」各有用途：`WRId::send(0)` 讓 `sendSignaled_` 不變、`ImmData::ack(0)` 讓對端 `sendAcked_` 不變、`sg_list=nullptr` 讓它不消耗發送緩衝。整個探針對 credit 系統是完全中性的，同時又走了完整的 SEND 路徑（會消耗對端一個 recv WR，然後 `onRecved` 裡 `postRecv(idx)` 補回去，`IBSocket.cc:539`）。舊版 server 收到它只會當成一個 0 長度訊息，因此**向後相容**。

`onRecved` 裡還有一行專門為此留的註解（`IBSocket.cc:542`）：

> Legacy 0-byte connect msgs (no imm) should follow normal recv flow so ACK batching stays aligned with sender's buffer credits.

也就是說更舊的探針格式（沒有 imm data 的 0 長度 SEND）會走正常的 `recvBufs_.push(idx, 0)` 路徑，讓 `recvNotAcked_` 照常累加，credit 才不會錯位。

### 4.5 在 RC QP 上模擬 stream socket

一個 IBSocket 的記憶體佈局是**一整塊、一次註冊**：

```cpp
// IBConnect.cc:724 initBufs()
size_t bufSize = connectConfig_.buf_size;                  // 預設 16 KB
size_t numSend = connectConfig_.send_buf_cnt;              // 預設 32
size_t numRecv = connectConfig_.qpMaxRecvWR();             // = send_buf_cnt + qpAckBufs() + 1
auto bufMem = BufferMem::create(device(), bufSize, numSend + numRecv,
                                IBV_ACCESS_LOCAL_WRITE | IBV_ACCESS_RELAXED_ORDERING);
sendBufs_.init(mem_->ptr,                        mem_->mr, bufSize, numSend);
recvBufs_.init(mem_->ptr + bufSize * numSend,    mem_->mr, bufSize, numRecv);
```

以預設值算：`qpAckBufs() = (32+8-1)/8 + 4 = 8`，`qpMaxRecvWR() = 32+8+1 = 41`，總記憶體 `16 KB × (32+41) = 1168 KB ≈ 1.14 MB / 連線`。QP 參數：`qpMaxSendWR() = 32+8+128+1+1 = 170`、`qpMaxCQEntries() = 170+41 = 211`。

```
        發送側 SendBuffers（ring，IBSocket.h:418）
        ┌────┬────┬────┬────┬────┬────┬────┬────┐
        │ 0  │ 1  │ 2  │ 3  │ ...│ 30 │ 31 │    │  32 × 16 KB
        └────┴────┴────┴────┴────┴────┴────┴────┘
          ▲                        ▲
       frontIdx_（取用）        tailIdx_（歸還）
       兩者都 alignas(hardware_destructive_interference_size)

  send(buf):                                          IBSocket.cc:611
    while (!sendBufs_.empty() && !buf.empty()):
        [idx, sendBuf] = sendBufs_.front()
        memcpy(sendBuf.data(), buf.data(), min(...))   ← 使用者資料複製進註冊記憶體
        if sendBuf 滿:  sendBufs_.pop(); postSend(idx, bufSize)
    if 沒送完:  記錄 sendWaitBufBegin_（延遲統計）

  flush():                                            IBSocket.cc:654
    若 front buffer 有半滿資料 → pop + postSend(idx, 已寫入長度)
```

`flush()` 的存在正是 stream 語意的代價：TCP 有 Nagle 與核心緩衝，寫多少算多少；RDMA 這裡是「填滿 16 KB 才發」，所以 `Transport::doWrite` 在佇列空時必須顯式 `socket_->flush()`（`Transport.cc:270`），否則最後一個半滿的 buffer 會永遠卡住。

**credit 流控是雙變數的**：

```
  發送端                                         接收端
  ──────                                         ──────
  postSend(idx, len)                             onRecved: recvBufs_.push(idx, len)
    sendNotSignaled_++                             events |= kEventReadableFlag
    if sendNotSignaled_ >= buf_signal_batch:     recv(buf):
       flags |= IBV_SEND_SIGNALED                  memcpy 出去
       wr_id = WRId::send(sendNotSignaled_)  ★     buffer 用完 → recvBufs_.pop()
       sendNotSignaled_ = 0                                    → postRecv(idx)  補回 WR
                                                               → recvNotAcked_++
  onSended(wc):                                    if recvNotAcked_ == buf_ack_batch:
    sendSignaled_ += wr.sendSignalCount()  ★         recvNotAcked_ = 0
                                                     if --ackBufAvailable_ >= 0:
  onImmData(ACK):                            ◀────      postAck()  SEND_WITH_IMM
    sendAcked_ += imm.data()                                    imm = ImmData::ack(buf_ack_batch)
                                                     else: 延後，等 onAckSended 時補發
  poll():                                       IBSocket.cc:347
    sendAvailable = min(sendAcked_, sendSignaled_)   ★★
    sendBufs_.push(sendAvailable)
    sendAcked_ -= sendAvailable; sendSignaled_ -= sendAvailable
    events |= kEventWritableFlag
```

★ **signal batching 的巧思**：不是每個 `ibv_post_send` 都設 `IBV_SEND_SIGNALED`，而是每 `buf_signal_batch`（預設 8）個才設一次，**並把「這一批有幾個」編碼進 wr_id**（`IBSocket.cc:846-853`）。完成時一次認領 8 個。CQE 數量降到 1/8，`ibv_poll_cq` 的負擔跟著降。

★★ **`min(sendAcked_, sendSignaled_)` 是整個流控的核心不變式**：一個發送緩衝可以重用，必須同時滿足
- 本地 HCA 已確認送出（`sendSignaled_`，防止覆寫還沒 DMA 出去的資料），且
- 對端已確認消費（`sendAcked_`，防止淹沒對端的 recv 佇列）。

兩個計數器獨立累加、取小值釋放，任一方落後都會自然形成背壓（`sendBufs_.empty()` → `send()` 回傳部分寫入 → `Transport::writeAll` 回 `tryToSuspend`）。

**ACK 本身也要限流**：ACK 是一個 `IBV_WR_SEND_WITH_IMM`，它會佔用發送佇列的一格。`ackBufAvailable_` 初始為 `qpAckBufs()`（`IBConnect.cc:719`），每次 `postAck` 前 `fetch_sub(1)`，若結果 < 1 就不發，改由 `onAckSended` 在有 ACK 完成時補發（`IBSocket.cc:551`）：

```cpp
int IBSocket::onAckSended(const ibv_wc &, Events &) {
  if (UNLIKELY(ackBufAvailable_.fetch_add(1, std::memory_order_relaxed) < 0)) {
    return postAck();      // 之前欠了一個 ACK，現在補
  }
  return 0;
}
```

`ackBufAvailable_` 是 `atomic<int32_t>`，**允許為負**——負值就是「欠了幾個 ACK」。

### 4.6 ImmData、WRId、completion channel

**`ImmData`（`IBSocket.h:339`）** 把 32 bit 的 immediate data 切成 `[8 bit type | 24 bit value]`，並強制大端序上線（RDMA immediate data 在線上是網路序）：

```cpp
static constexpr size_t kTypeOffset = 24;
enum class Type : uint8_t { ACK, CLOSE };
static ImmData create(Type type, uint32_t val) {
  return folly::Endian::big32(((uint8_t)type << kTypeOffset) | val);
}
operator __be32() const { return folly::Endian::big(val); }
```

只用了 2 種型別，24 bit 的 value 只在 ACK 時有意義（credit 數）。

**`WRId`（`IBSocket.h:282`）** 繼承 `folly::StampedPtr<void>`，利用 x86-64 指標只有 48 位有效的事實，把 16 bit 的型別標記塞進指標高位：

```cpp
enum class WRType : uint16_t { SEND, RECV, ACK, RDMA, RDMA_LAST, CLOSE, CHECK };
static uint64_t send(uint32_t signalCount) { return pack(signalCount, WRType::SEND); }
static uint64_t recv(uint32_t bufIndex)    { return pack(bufIndex,    WRType::RECV); }
static uint64_t rdma(RDMAPostCtx *ptr, bool last) { return pack(ptr, last ? RDMA_LAST : RDMA); }
```

`SEND`/`RECV` 的低位存的是整數（batch 計數 / buffer 索引），`RDMA`/`RDMA_LAST` 存的是真指標。一個 `uint64_t wr_id` 就承載了完整的完成事件語意，`wcSuccess()` 一個 switch 分派完（`IBSocket.cc:477`）。

**completion channel 掛進 epoll** 是 RDMA 能與 TCP 共用事件迴圈的關鍵：

```cpp
// IBConnect.cc:549
channel_.reset(ibv_create_comp_channel(device()->context()));
int flags = fcntl(channel_->fd, F_GETFL);
fcntl(channel_->fd, F_SETFL, flags | O_NONBLOCK);      // ← 必須非阻塞
...
cq_.reset(ibv_create_cq(device()->context(), connectConfig_.qpMaxCQEntries(),
                        nullptr, channel_.get(), 0 /* comp vector */));
```

```cpp
// IBSocket.h:134
int fd() const override { return channel_->fd; }        // ← Socket 介面回傳 comp channel fd
```

於是 `EventLoop::add(transport, EPOLLIN|EPOLLOUT|EPOLLET)` 對 TCP 與 RDMA 是同一行程式碼（`IOWorker.cc:37` vs `:51`）。

`poll()` 的四步順序不可調換（`IBSocket.cc:329`）：

```cpp
if (cqGetEvent())     return makeError(...);   // 1. ibv_get_cq_event（消費 fd 上的可讀事件）
cqPoll(events);                                // 2. 撈 CQE
if (cqRequestNotify()) return makeError(...);  // 3. ibv_req_notify_cq（重新武裝通知）
cqPoll(events);                                // 4. 再撈一次 ★
```

★ 第 4 步是為了關掉「步驟 2 撈完之後、步驟 3 重新武裝之前」到達的 CQE 所造成的漏通知窗口。這是 verbs 程式的標準模式，但很多實作會忘記。

`ibv_ack_cq_events` 也做了批次（每 `event_ack_batch = 128` 次才 ack 一次，`IBSocket.cc:405`），因為它內部要拿鎖。析構時補 ack 剩餘的（`IBSocket.cc:323`）。

`cqPoll` 每輪撈 16 個，`ret < kPollCQBatch` 就跳出（`IBSocket.cc:382`）——CQ 空了就不用再呼叫一次。

### 4.7 RDMA 讀寫：批次與 WR linked-list

```
  使用者（storage server / client）
    │  CallContext::readTransmission() / writeTransmission()     CallContext.h:113
    ▼
  IBSocket::RDMAReqBatch                                          IBSocket.h:186
    ├ add(remoteBuf, localBuf)        單一 sge
    └ add(remoteBuf, span<localBufs>) 依 max_sge（16）切段，每段一個 RDMAReq
    │    RDMAReq{ raddr, rkey, localBufFirst, localBufCnt }
    ▼
  IBSocket::rdmaBatch(opcode, reqs, localBufs, &waitLat, &transferLat)   IBSocket.cc:931
    ├ numPosts = ceil(reqs.size() / max_rdma_wr_per_post)   (預設 32)
    ├ 為每個 post 建立 RDMAPostCtx，向 rdmaSem_ 申請 reqs.size() 個 token
    │    rdmaSem_ 是 folly::fibers::BatchSemaphore，RTS 時 signal(max_rdma_wr)=128
    ├ 1 個 post → 直接 co_await；多個 → folly::coro::collectAllRange 併發
    └ 統計 waitLatency = 首次 post 開始 - 進入時刻
              transferLatency = 最後 post 結束 - 首次 post 開始
    ▼
  IBSocket::rdmaPostWR(ctx)                                        IBSocket.cc:1032
    static thread_local folly::small_vector<ibv_send_wr, 256> wrs;   ← 免配置
    static thread_local folly::small_vector<ibv_sge,    2048> sges;
    for req in ctx.reqs:
        wr.next = &wr + 1            ← 串成 linked list
        wr.wr_id = WRId::rdma(&ctx, false)
        wr.send_flags = 0            ← 不設 SIGNALED
        wr.wr.rdma.remote_addr = req.raddr;  wr.wr.rdma.rkey = req.rkey
        for buf in localBufs[first..first+cnt]:
            sge.lkey = buf.getMR(port_.dev()->id())->lkey     ← 本地 device 的 lkey
    wrs.back().next = nullptr
    wrs.back().wr_id = WRId::rdma(&ctx, true)     ← 只有最後一個是 RDMA_LAST
    wrs.back().send_flags |= IBV_SEND_SIGNALED    ← 只有最後一個要 CQE
    ibv_post_send(qp_, &wrs[0], &bad)             ← 整條鏈一次呼叫
```

**只有最後一個 WR 設 SIGNALED** 的設計，在 `ibv_post_send` 部分失敗時會造成無法追蹤，程式碼對此有明確處置（`IBSocket.cc:1096`）：

```cpp
if (bad == &wrs[0]) {
  XLOGF(CRITICAL, "IBSocket {} failed to post RDMA, ...");        // 一個都沒送出去，安全
} else {
  // Only a subset of RDMA work requests were successfully posted. As only the final WR has the
  // IBV_SEND_SIGNALED flag set, there is no way to track when the posted RDMA WRs will complete.
  // We need set the QP to error state.
  XLOGF(DFATAL, "...");
  ibv_qp_attr attr{.qp_state = IBV_QPS_ERR};
  ibv_modify_qp(qp_.get(), &attr, IBV_QP_STATE);                  // ← 主動毀掉 QP
}
```

**寧可炸掉整條連線，也不留下無法回收的 in-flight WR。** 這是正確的選擇：本地緩衝的生命週期綁在 `RDMAPostCtx` 上，若無法得知何時完成，緩衝就永遠不能釋放。

`rdmaSem_` 的 token 數等於 `max_rdma_wr`，正好是 QP send queue 為 RDMA 預留的格數（`qpMaxSendWR() = send_buf_cnt + qpAckBufs() + max_rdma_wr + 2`）。因此 `ibv_post_send` 永遠不會因為 send queue 滿而失敗——訊號量把上限守在 verbs 之外。

### 4.8 多 HCA 全註冊與 rkey 陣列

```cpp
// net/ib/RDMABuf.cc:107
int RDMABuf::Inner::registerMemory() {
  for (auto &dev : IBDevice::all()) {                    // ← 對「每一張」HCA 都註冊一次
    auto mr = dev->regMemory(ptr_, capacity_, kAccessFlags);
    if (!mr) return -1;
    mrs_[dev->id()] = mr;
  }
  return 0;
}
```

`kAccessFlags = LOCAL_WRITE | REMOTE_WRITE | REMOTE_READ | RELAXED_ORDERING`（`RDMABuf.h:243`）。同一塊虛擬記憶體在 N 張 HCA 上各有一個 `ibv_mr`，各自的 lkey/rkey 不同。`mrs_` 是 `std::array<RDMABufMR, IBDevice::kMaxDeviceCnt>`（4 格）。

代價是註冊成本乘以 N（`ibv_reg_mr` 要 pin 頁並建立 HCA 的位址轉換表，是昂貴操作，程式碼專門為它加了 `mrLatency` 指標，`IBDevice.cc:593`）。好處是：**一塊緩衝可以被任何一條連線使用，不管那條連線落在哪張卡上**。對 3FS 這種「同一個 client 對不同 storage 節點的連線可能分散在不同 HCA」的場景，這消除了「緩衝與 HCA 綁定」的複雜度。

遠端側則帶著全部 rkey：

```cpp
// RDMABuf.h:40
class RDMARemoteBuf {
  struct Rkey { uint32_t rkey = 0; int devId = -1; };
  uint64_t addr_;  uint64_t length_;
  std::array<Rkey, IBDevice::kMaxDeviceCnt> rkeys_;
  std::optional<uint32_t> getRkey(int devId) const {         // :64
    for (auto &rkey : rkeys_) if (rkey.devId == devId) return rkey.rkey;
    return std::nullopt;
  }
};
```

使用時：

```cpp
// IBSocket.cc:260
auto rkey = remoteBuf.getRkey(socket_->peerInfo_.dev);      // 對端 QP 所在的 device id
...
auto mr = buf.getMR(port_.dev()->id());                     // 本地 QP 所在的 device id
sge.lkey = mr->lkey;                                        // IBSocket.cc:1063-1071
```

**遠端 buf 帶的是「對端所有卡的 rkey」，選哪一個由「對端 QP 落在哪張卡」決定。** `peerInfo_.dev` 來自握手時對端送來的 `IBConnectInfo::dev`（`IBConnect.h:125`）。

序列化時只寫非空前綴（`RDMABuf.h:355`）：

```cpp
static constexpr auto serialize(const net::RDMARemoteBuf &buf, auto &out) {
  uint8_t len = 0;
  for (auto &rkey : buf.rkeys()) { if (rkey.devId == -1) break; else ++len; }
  for (int8_t i = len - 1; i >= 0; --i) {          // 倒序寫（DownwardBytes）
    serde::serialize(buf.rkeys()[i].devId, out);
    serde::serialize(buf.rkeys()[i].rkey, out);
  }
  serde::serialize(len, out);
  serde::serialize(buf.size(), out);
  serde::serialize(buf.addr(), out);
}
```

單卡機器上一個 `RDMARemoteBuf` 只佔 `8(addr) + 8(size) + 1(len) + 4(rkey) + 4(devId) = 25` bytes，而不是固定的 `8+8+4×8=48`。對於一次 `batchRead` 要帶上百個遠端 buf 的 storage RPC，這個節省是實質的。

`kMaxDeviceCnt = 4` 是硬上限，超過就啟動失敗（`IBDevice.cc:347`）：

```cpp
if (devices.size() > kMaxDeviceCnt) {
  XLOGF(CRITICAL, "too many ibdevices {} > kMaxDeviceCnt {}, please specify device_filter", ...);
  return makeError(StatusCode::kInvalidArg);
}
```

### 4.9 關閉與 drain

RDMA 連線不能像 TCP 那樣 `close(fd)` 就完事——QP 上可能還有 in-flight 的 WR，直接銷毀會產生 flush error 甚至讓對端卡住。3FS 的做法是**兩段式**：

```
  Transport 析構 → IBManager::close(IBSocket::Ptr)              Transport.cc:64
    → IBSocketManager::close(socket)                            IBSocket.cc:1221
       ├ Drainer::create(socket, manager)                       IBSocket.cc:1117
       │    ├ closeGracefully()：post 一個 RDMA_WRITE_WITH_IMM   IBSocket.cc:799
       │    │     imm = ImmData::close(), 0 長度, SIGNALED, INLINE
       │    │     （對端 onImmData(CLOSE) → state = CLOSE → 可讀事件）
       │    └ ibv_query_qp 確認仍在 RTS，否則不 drain
       ├ drainers_.insert(drainer)
       ├ deadlines_.emplace(now + drain_timeout(5s), drainer)
       └ IBManager::eventLoop_->add(drainer, EPOLLIN|EPOLLOUT|ET)
                                    │
       ┌────────────────────────────┘
       ▼
   Drainer::handleEvents → IBSocket::drain()                    IBSocket.cc:1154
     cqGetEvent + cqRequestNotify
     while (ibv_poll_cq(cq_, 1, &wc)):
        錯誤 → 結束
        WRType::CLOSE（本地 close 訊息完成）→ 結束
        RECV + WITH_IMM + ImmData::close()（收到對端 close）→ 結束
        RDMA_LAST → FATAL（drain 期間不該有 RDMA 完成）
     結束時：EventLoop::remove + IBSocketManager::remove
                                    │
   逾時保護：IBSocketManager 有 200 ms 週期的 timerfd            IBSocket.cc:1196
     handleEvents 掃 deadlines_，超過 drain_timeout 的強制移除    IBSocket.cc:1235
```

`~IBSocket()` 最後才把 QP 轉 ERR（`IBSocket.cc:319`）。`IBSocketManager::stopAndJoin()` 最多等 500 ms（`IBSocket.cc:1203`），逾時就 `CRITICAL` 記錄並強制清空。

`IBSocket::check()`（`IBSocket.cc:748`）用一個**零長度、INLINE 的 RDMA_WRITE**做活性探測——不消耗任何緩衝、不觸發對端的任何回呼，只要 QP 還活著就會產生一個成功的 CQE；QP 死了就產生錯誤 CQE，由 `poll()` 轉成 `kSocketError`。`checkMsgSended_` 保證同時只有一個探測在飛。

### 4.10 RDMAControl：跨連線的 RDMA 傳輸許可

這是一個容易被忽略的機制。`enableRDMAControl` 打開後（`net::Client::Config::enable_rdma_control`，`Client.h:28`），client 的請求會帶 `EssentialFlags::ControlRDMA`，然後 server 端要做 RDMA 前必須先反向申請許可：

```
 SERVER 側服務實作                                   CLIENT 側（同一條連線反向）
   │                                                       │
   │ auto batch = ctx.writeTransmission();                 │
   │ batch.add(remoteBuf, localBuf);                       │
   │ co_await batch.applyTransmission(timeout)             │
   │   └ CallContext.cc:15                                 │
   │      serde::ClientContext clientCtx(ctx.transport()); │ ← 用「收到請求的那條 transport」
   │      RDMATransmissionReq{ ctx.packet().uuid }         │   反向發 RPC
   ├──── RDMAControl::apply (svc 10, m 1) ────────────────▶│
   │                                                       │ RDMAControlImpl::apply
   │                                                       │   co_await limiter_->co_wait()   ★
   │                                                       │   Waiter::setTransmissionLimiterPtr(
   │                                                       │       req.uuid, limiter_, startTime)
   │◀─── RDMATransmissionRsp{} ────────────────────────────┤
   │                                                       │
   │ co_await batch.post()   ← 真正做 RDMA                  │
   │ ... 回應送回 client ...                                │
   │                                              Waiter::post(packet, buf)  Waiter.h:79
   │                                                if (item->limiter)
   │                                                   item->limiter->signal(latency)  ★
```

★ **限流的信號量在 client 端持有**（`RDMATransmissionLimiter`，`RDMAControl.h:20`，`max_concurrent_transmission` 預設 64），持有時間是**從許可核發到最終回應返回**——`signal()` 由 `Waiter::post` 觸發（`Waiter.h:83`），而不是由 `apply` 的回應觸發。

這就是它的意義：**client 端限制「有多少個 server 正在對我做 RDMA 寫入」**。因為 RDMA WRITE 是單向的，server 寫進 client 記憶體時 client 的 CPU 完全不知情，沒有任何天然背壓。若十個 storage 節點同時對一個 client 做 RDMA WRITE，client 的 PCIe / HCA 頻寬會被打爆而 client 自己毫無感覺。`RDMAControl` 用一次額外的往返（在 RDMA 之前）把這個控制權交還給 client。

`ClientContext(const net::TransportPtr &tr)` 這個建構子（`ClientContext.h:32`）就是為此存在的——它讓 server 能在**收到請求的那條既有連線上**反向發 RPC，而不需要建立新連線。`connectionSource_` 的三種變體（`IOWorker*` / `ConnectionPool*` / `Transport*`，`ClientContext.h:199`）中，`Transport*` 專門服務這個場景，且它不支援 `callSync`（`ClientContext.h:79` 直接回 `kFoundBug`）。

---

---

## 5. 支柱② 序列化框架（`src/common/serde/`）

16 個檔案、約 2600 行，沒有 IDL、沒有程式碼產生器、沒有 `.proto` 也沒有 `.fbs`。整套框架的核心主張是：**C++ 的類別定義本身就是 schema**，用巨集在編譯期把成員清單「記」下來，然後用 `if constexpr` 的長階梯把任意型別遞迴拆成位元組。代價是完全沒有欄位標籤，相容性紀律全靠人。

### 5.1 檔案清單與職責

| 檔案 | 行數量級 | 職責 |
|---|---:|---|
| `serde/Serde.h` | 1000 | 核心：4 個欄位巨集、`serialize`/`deserialize` 兩座 `if constexpr` 階梯、`Out<>`/`In<>` 三後端、`SerdeMethod<>` 特化點 |
| `serde/Serde.cc` | 409 | `Out<JsonObject>` / `Out<TomlObject>` / `In<JsonObject>` / `In<TomlObject>` 的外顯特化實作（唯一 include `toml.hpp` 的 TU） |
| `serde/SerdeHelper.h` | 59 | `DEFINE_SERDE_HELPER_STRUCT` + `store()`/`load()`/`unpackFrom()`：把 serde 結構直接掛到 KV 交易上 |
| `serde/MessagePacket.h` | 89 | RPC 信封：`Version` / `Timestamp` / `MessagePacket<T>` / `PointerWrapper<T>` |
| `serde/Service.h` | 153 | `SERDE_SERVICE` / `SERDE_SERVICE_METHOD` / `MethodExtractor`（見 §3.3、§3.4） |
| `serde/Services.h` | — | 65536 槽的服務表（見 §3.4） |
| `serde/CallContext.h/.cc` | 139 | server 側分派與回應（見 §3.4） |
| `serde/ClientContext.h/.cc` | 204 | client 側送出與等待（見 §3.6） |
| `serde/ClientMockContext.h` | — | 測試用：起一個真的 `net::Server` + `net::Client`，但介面與 `ClientContext` 同形 |
| `serde/Echo.h` | — | serviceId 10000 的內建服務 |
| `serde/SerdeComparisons.h` | 37 | 用反射自動產生 `equals()` / `compare()`，省掉手寫 `operator==` |
| `serde/Visit.h` | 69 | 另一套訪問器（`VisitOut`），給 CLI 表格化輸出用；不參與線上格式 |
| `serde/TypeName.h` | 36 | `variant` 的型別名稱 ↔ 索引對應 |
| `serde/BigEndian.h` | 25 | `BigEndian<T>`：唯一會做位元組序轉換的包裝型別 |

支撐它的三個 `utils/` 檔案才是真正的機關所在：

- `src/common/utils/Reflection.h`（121 行）——編譯期成員清單的累積器
- `src/common/utils/Thief.h`（34 行）——把「還沒定義完的類別自己」偷渡出來的 34 行黑魔法
- `src/common/utils/DownwardBytes.h`（159 行）——倒著長的緩衝區

### 5.2 反射機制：三層巨集疊出來的編譯期成員清單

#### 5.2.1 第一層：`CollectField` 重載鏈

`src/common/utils/Reflection.h:12-34` 是整套反射的地基：

```cpp
template <size_t N = 64>
struct Rank : Rank<N - 1> {};              // :12  Rank<64> 繼承 Rank<63> … 繼承 Rank<0>
template <>
struct Rank<0> {};                          // :14

[[maybe_unused]] static std::tuple<> CollectField(::hf3fs::refl::Rank<0>);   // :27  種子

#define REFL_NOW decltype(CollectField(::hf3fs::refl::Rank<>{}))             // :29
#define REFL_ADD_SAFE(info, t) \
  static ::hf3fs::refl::Append_t<t, decltype(info)> CollectField(::hf3fs::refl::Rank<std::tuple_size_v<t> + 1>)  // :33
```

機制是**用重載解析當作可變狀態**：

```
 類別內每宣告一個欄位，就多一個 CollectField 重載：

   欄位 0  →  static tuple<F0>          CollectField(Rank<1>);
   欄位 1  →  static tuple<F0,F1>       CollectField(Rank<2>);
   欄位 2  →  static tuple<F0,F1,F2>    CollectField(Rank<3>);
                     ...
   種子    →  static tuple<>            CollectField(Rank<0>);   ← 命名空間層級

 查詢時傳 Rank<64>{}。因為 Rank<64> 是 Rank<63>…Rank<0> 的衍生類別，
 所有重載都是「衍生轉基底」可行的候選；overload resolution 選轉換距離
 最短的那個 → 目前宣告過的最大 N → 也就是「到此為止的完整欄位清單」。
```

`REFL_NOW` 就是「在原始碼的這一行，這個類別已經累積了哪些欄位」。這是 C++ 裡少數能在編譯期產生**順序相關副作用**的技巧。

**硬上限是 64 個 serde 欄位**：`Rank<N = 64>`（`Reflection.h:12`）。第 65 個欄位會宣告 `CollectField(Rank<65>)`，而查詢用的 `Rank<>{}` 是 `Rank<64>`，無法轉成 `Rank<65>`。結果不是編譯錯誤，而是**第 65 個以後的欄位被靜默忽略**——序列化時直接不見。全樹沒有任何 `static_assert` 擋這件事。

#### 5.2.2 第二層：`thief` —— 在類別還沒定義完時取得自己的型別

問題：`REFL_ADD_SAFE` 要產生 `&Inode::id` 這種成員指標，但巨集展開的位置在 `Inode` 的類別本體內，此時 `Inode` 是不完整型別，不能寫 `&Inode::id`，也不能在類別作用域直接寫 `decltype(*this)`。

`src/common/utils/Thief.h` 用 34 行解決：

```cpp
template <typename Tag>
struct Bridge {
  friend consteval auto ADL(Bridge<Tag>);       // :15  只宣告、不定義的 friend
};

template <typename Tag, typename Store>
struct StealType {
  friend consteval auto ADL(Bridge<Tag>) { return std::type_identity<Store>{}; }  // :23  實例化時才定義
};

template <typename Tag, typename Store>
using steal = decltype(detail::StealType<Tag, Store>{});     // :29
template <typename Tag>
using retrieve = typename decltype(ADL(detail::Bridge<Tag>{}))::type;  // :32
```

這是所謂的 *friend injection*：`StealType<Tag, Store>` 一被實例化，就替 `ADL(Bridge<Tag>)` 補上定義；之後任何地方寫 `retrieve<Tag>` 都能透過 ADL 把 `Store` 撈回來。而 `steal` 的實例化被藏在**一個成員函式宣告的尾端回傳型別裡**：

```cpp
// Serde.h:51（SERDE_STRUCT_TYPED_FIELD 內）
constexpr auto T##NAME() -> ::hf3fs::thief::steal<struct T##NAME, std::decay_t<decltype(*this)>>;
```

成員函式的**尾端回傳型別**是少數 `this` 可用、而且不需要類別完整的位置。這個函式從來不會被定義、也不會被呼叫，它存在的唯一目的就是觸發 `StealType` 的實例化，把「我是誰」寫進 `ADL(Bridge<Tid>)`。

#### 5.2.3 第三層：`SERDE_STRUCT_FIELD` 的完整展開

以 `src/fbs/meta/Schema.h:362` 的 `SERDE_STRUCT_FIELD(id, InodeId());` 為例。經過 `SERDE_STRUCT_FIELD`（`Serde.h:59`）→ `SERDE_STRUCT_TYPED_FIELD`（`Serde.h:42`）兩層展開後，實際塞進 `struct Inode` 的是：

```cpp
 private:
  friend struct ::hf3fs::refl::Helper;

  // (1) 快照：把「此行之前已累積的欄位清單」凍結成一個型別
  struct Tid : std::type_identity< decltype(CollectField(::hf3fs::refl::Rank<>{})) > {};

 public:
  // (2) 真正的資料成員（型別由 DEFAULT 推導）
  std::decay_t<decltype(InodeId())> id = InodeId();

 protected:
  // (3) 偷渡：把 Inode 自己的型別存進 ADL(Bridge<Tid>)
  constexpr auto Tid() -> ::hf3fs::thief::steal<struct Tid, std::decay_t<decltype(*this)>>;

  // (4) 追加一個 CollectField 重載，回傳「舊清單 + 本欄位的 FieldInfo」
  static ::hf3fs::refl::Append_t<
             typename Tid::type,
             decltype(::hf3fs::serde::FieldInfo<"id", &::hf3fs::thief::retrieve<struct Tid>::id>{})>
  CollectField(::hf3fs::refl::Rank<std::tuple_size_v<typename Tid::type> + 1>);
```

四件事情按順序發生，缺一不可：

1. `Tid`（型別）記住「到目前為止」的清單長度與內容；
2. 成員本身被宣告，型別由預設值 `std::decay_t<decltype(DEFAULT)>` 推導——**所以 `SERDE_STRUCT_FIELD(x, 0)` 的型別是 `int`，不是 `uint32_t`**，型別由預設值字面量決定，這是很容易踩到的坑，要精確控制得用 `SERDE_STRUCT_TYPED_FIELD`；
3. `Tid`（函式）觸發 friend injection；
4. `CollectField` 的新重載被宣告——注意整段 `FieldInfo<...>` 包在回傳型別裡，**只有 `CollectField` 被具名時才實例化**，那時 `Inode` 已完整，`&Inode::id` 於是合法。

`FieldInfo` 本身只是三個編譯期常數（`Serde.h:64-69`）：

```cpp
template <NameWrapper Name, auto Getter, auto Checker = nullptr>
struct FieldInfo {
  static constexpr std::string_view name = Name;   // 欄位名（只給 JSON/TOML 用）
  static constexpr auto getter = Getter;           // pointer-to-member
  static constexpr auto checker = Checker;         // 選用的檢查器
};
```

**所以「反射」的具體形式是：一個 `std::tuple<FieldInfo<...>, FieldInfo<...>, ...>`，每個元素帶一個成員指標。** 不是 CTAD、不是成員指標陣列，是 tuple of empty types。存取時 `o.*type.getter`（`Serde.h:112`），編譯後就是一個固定偏移量。

四個巨集的差異只在「成員長什麼樣」：

| 巨集 | 成員 | 存取 | 用在哪 |
|---|---|---|---|
| `SERDE_STRUCT_FIELD(n, d)` | `public: T n = d;` | `o.n` | 絕大多數（`Serde.h:59`） |
| `SERDE_CLASS_FIELD(n, d)` | `private: T n_ = d;` | `o.n()` / `o.n()` | 需要封裝時，如 `flat::HeartbeatInfo` 的 `info`（`src/fbs/mgmtd/HeartbeatInfo.h:84`） |
| `SERDE_STRUCT_TYPED_FIELD(T, n, d)` | 同上但型別顯式 | `o.n` | 預設值推不出想要的型別時 |
| `SERDE_CLASS_TYPED_FIELD(T, n, d)` | 同上 | `o.n()` | 同上 |

#### 5.2.4 繼承：欄位清單會跨類別串接

`CollectField` 是 `static` 成員函式，會被**繼承**。所以：

```cpp
// src/fbs/meta/Schema.h:316
struct InodeData {
  SERDE_STRUCT_FIELD(type, (std::variant<File, Directory, Symlink>()));
  SERDE_STRUCT_FIELD(acl, Acl());
  SERDE_STRUCT_FIELD(nlink, uint16_t(1));
  SERDE_STRUCT_FIELD(atime, UtcTime(...));
  SERDE_STRUCT_FIELD(ctime, UtcTime(...));
  SERDE_STRUCT_FIELD(mtime, UtcTime(...));
};

// src/fbs/meta/Schema.h:361
struct Inode : InodeData {
  SERDE_STRUCT_FIELD(id, InodeId());
};
```

`Inode` 的 `FieldInfoList` = `tuple<type, acl, nlink, atime, ctime, mtime, id>`——**基底欄位在前、衍生欄位在後**。而 `FieldInfo::getter` 的型別分別是 `X InodeData::*` 與 `InodeId Inode::*`，`refl::Helper::iterate` 用 `member_pointer_to_class_t`（`src/common/utils/TypeTraits.h:94`）偵測這個轉折：

```cpp
// src/common/utils/Reflection.h:60
if constexpr (!std::is_same_v<member_pointer_to_class_t<FieldInfo<T, idx>::getter>,
                              member_pointer_to_class_t<FieldInfo<T, pre>::getter>>) {
  auto &&first = getFirstParameter(typeChanged...);   // 呼叫「類別換了」回呼
  ...
}
```

這個「類別換了」回呼在二進位序列化時的動作是**收掉當前 table、另開一張新 table**（`Serde.h:285-286`）。後果見 §5.3.4。

一個容易踩的陷阱：`flat::AppInfo : FbsAppInfo`（`src/common/app/AppInfo.h:118`）的 `clusterId`、`tags` 是**普通成員、不是 serde 欄位**。序列化 `AppInfo` 只會輸出 `FbsAppInfo` 的六個欄位，`clusterId` 與 `tags` 靜默消失。程式碼裡的寫法是顯式向下轉型提醒讀者：`serde::toJsonString(static_cast<const flat::FbsAppInfo &>(baseInfo))`（`src/common/app/Utils.cc:248`）。

### 5.3 線上二進位格式

#### 5.3.1 倒著寫的緩衝區

`DownwardBytes`（`src/common/utils/DownwardBytes.h:12`）從緩衝區**尾端往前**寫：

```
        offset_                                     capacity_
           │                                            │
           ▼                                            ▼
  ┌────────┬───────────────────────────────────────────┐
  │ 未使用 │  已寫入的資料（data() = data_ + offset_）   │
  └────────┴───────────────────────────────────────────┘
                                        ▲
  append() 讓 offset_ 減少，新資料插在最前面 ────┘
```

```cpp
void append(auto *data, uint32_t size) {   // :53
  reserve(this->size() + size);
  offset_ -= size;
  std::memcpy(this->data(), data, size);
}
```

配合「欄位倒著走」（`refl::Helper::iterate<T, /*Backwards=*/true>`，`Serde.h:285`），最後一個欄位最先寫（落在最尾端），第一個欄位最後寫（落在最前面）——**最終緩衝區裡的順序仍然是宣告順序**。

倒著寫換來的唯一好處是：**長度前綴不需要佔位符**。`tableBegin()` 只記下當時的 `size()`，等所有欄位寫完後 `tableEnd()` 把 `Varint32(now - start)` 一次 prepend 到最前面（`Serde.h:424-425`）：

```cpp
uint32_t tableBegin(bool) { return out_.size(); }
void tableEnd(uint32_t start) { serde::serialize(Varint32(out_.size() - start), *this); }
```

正著寫的做法必須先預留 4~5 個位元組給長度、寫完再回填（或做一次 memmove）；倒著寫兩者都免了。字串（`Serde.h:433`）與陣列長度（`Serde.h:427`）用的是同一招。

#### 5.3.2 逐型別位元組佈局

以下全部是**最終緩衝區中由左至右的順序**。沒有任何欄位標籤、沒有型別碼、沒有對齊填充。

| C++ 型別 | 線上佈局 | 出處 |
|---|---|---|
| `bool` / 整數 / 浮點 / `enum` / 帶 `is_serde_copyable` 的型別 | `sizeof(T)` 個原生位元組（小端，直接 memcpy） | `Serde.h:365`、`:430` |
| `Varint32` | LEB128，1–5 bytes | `Serde.h:146` |
| `Varint64` | LEB128，1–10 bytes | `Serde.h:195` |
| `std::string` / `string_view` / 可轉字串 | `varint32 len` ‖ 原始位元組 | `Serde.h:433` |
| `hf3fs::Path` | 同字串（`serdeTo` → `std::string`） | `Serde.h:974` |
| serde struct | `varint32 tableLen` ‖ 各欄位串接 | `Serde.h:424` |
| `std::optional<T>` / `unique_ptr` / `shared_ptr` | `uint8 tag`（0=NullOpt，1=HasValue）‖ 有值時接 `T` | `Serde.h:295-308` |
| `Result<T>` | `uint8 hasValue` ‖ 有值接 `T`，否則接 `Status` | `Serde.h:932` |
| `Status` | `uint16 code` ‖ `optional<string> message` | `Serde.h:890`、`src/common/utils/StatusCode.h:8` |
| `std::pair<A,B>` | `A` ‖ `B` | `Serde.h:329` |
| 容器（滿足 `Container` concept） | `varint32 count` ‖ 元素串接 | `Serde.h:332-343` |
| `map`（元素是 pair） | `varint32 count` ‖ (`key` ‖ `value`) × count | `Serde.h:329` + `:332` |
| `std::variant<...>` | `varint32 nameLen` ‖ **型別名稱字串** ‖ 值 | `Serde.h:309-316` |
| `Void` | 0 bytes | `Serde.h:883` |
| `serde::BigEndian<T>` | `sizeof(T)` bytes，**大端** | `src/common/serde/BigEndian.h:21` |

三個值得單獨講的：

**(a) 沒有 zigzag、沒有變長整數。** 除了明確寫 `Varint32`/`Varint64` 的地方（只有長度前綴與陣列計數在用），所有整數都是定長原生 memcpy。`uint64_t uuid` 永遠佔 8 bytes，即使值是 3。這跟 protobuf 的哲學完全相反——換來的是 `parseCopyable` 一行 memcpy（`Serde.h:729`）而不是逐位元組解碼。

**(b) `variant` 的標籤是 C++ 型別名稱字串。** `type_name_v<T>`（`src/common/serde/TypeName.h:11`）預設用 `nameof::nameof_short_type<T>()`，除非型別自己定義 `kTypeNameForSerde`。反序列化時 `variantTypeNameToIndex`（`TypeName.h:25`）在候選名稱陣列裡線性掃描比字串。這意味著**把 variant 成員的 C++ 型別改名，就等於改了線上格式**。`kTypeNameForSerde` 就是為了能把 C++ 名字與線上名字解耦而存在的逃生門。

**(c) 有一段死程式碼。** `Serde.h:344` 的算術向量整塊 memcpy 快速路徑：

```cpp
} else if constexpr (is_vector_v<T> && std::is_arithmetic_v<T> && isBinaryOut) {
  out.value(o);      // Out<>::value(const std::vector<V>&) 在 :437，做整塊 append
```

`std::is_arithmetic_v<T>` 的 `T` 是**向量型別本身**（應該是 `T::value_type`），永遠 false；而且就算條件寫對，`Container<T> && isBinaryOut`（`:332`）排在它前面也會先命中。所以 `std::vector<uint8_t>`（例如 `storage::UInt8Vector`，`src/fbs/storage/Common.h:317`）是**逐元素**序列化的，那個為它準備好的整塊 memcpy 介面（`Serde.h:437-441`）從來沒被呼叫過。

還有一個潛在地雷：`Serde.h:352` 的 `is_map_v` 分支會呼叫 `out.key(...)`，而二進位 `Out` 的 `key()` 是**空實作**（`Serde.h:429` `// ignore key.`）。二進位模式下 map 之所以正確，純粹是因為 `Container<T> && isBinaryOut`（`:332`）排在前面先接住了它。哪天有人塞進一個不滿足 `Container` concept 的 map-like 型別，它會掉進 `:352`，然後**鍵靜默消失**而不報錯。

#### 5.3.3 讀取端：長度前綴撐起整個相容性

`In<std::string_view>`（`Serde.h:683`）就是一個 `string_view` 游標：

```cpp
Result<In> parseTable() {                       // :692
  Varint64 length;
  RETURN_AND_LOG_ON_ERROR(serde::deserialize(length, *this));
  if (UNLIKELY(length > str_.length())) return makeError(kSerdeInsufficientLength, ...);
  auto table = In{str_.substr(0, length)};      // 子視圖，只看得到這張 table
  str_.remove_prefix(length);                   // 外層無條件跳過整段
  return table;
}
```

兩個關鍵行為：

1. **外層永遠依長度整段跳過。** 不管讀者認不認識 table 裡的全部欄位。
2. **子視圖裡讀完欄位後剩下的位元組直接丟棄。** 子視圖是暫存物件，沒有人會回頭檢查它是否讀乾淨。

再加上結構體反序列化的這一段（`Serde.h:502-507`）：

```cpp
return refl::Helper::iterate<T>(
    [&](auto type) -> Result<Void> {
      if (LIKELY(*table)) {                    // operator bool() == !str_.empty()
        return deserialize(o.*type.getter, *table);
      }
      // Missing fields at the end are acceptable.
      return Void{};
    },
    ...);
```

**table 讀空之後，剩下的欄位保持預設值、不報錯。** 這兩條規則湊起來，就是整套框架唯一的相容性機制。

#### 5.3.4 完整範例：`serde::Version` 與繼承造成的雙 table

`serde::Version`（`src/common/serde/MessagePacket.h:17`）：

```
 序列化 Version{major=2, minor=5, patch=1, hash=0xDEADBEEF}

  ┌──────┬──────┬──────┬──────┬───────────────────────────┐
  │ 0x07 │ 0x02 │ 0x05 │ 0x01 │ EF BE AD DE               │
  └──────┴──────┴──────┴──────┴───────────────────────────┘
    ▲       ▲      ▲      ▲      ▲
    │       │      │      │      └ hash: uint32 原生小端 4 bytes
    │       │      │      └────── patch: uint8
    │       │      └───────────── minor: uint8
    │       └──────────────────── major: uint8
    └──────────────────────────── varint32 tableLen = 7（1+1+1+4）

 總共 8 bytes。沒有欄位名、沒有標籤、沒有 padding。
```

`meta::Inode`（`src/fbs/meta/Schema.h:361`，繼承 `InodeData`）——注意這裡是**兩張並排的 table**：

```
  ┌─ table #1：InodeData 的欄位 ──────────────────────────────────────┐
  │ varint32 len1                                                     │
  │   ┌ type   : variant → varint32 nameLen ‖ "File"/"Directory"/…    │
  │   │                    ‖ 該 alternative 的 table                   │
  │   ├ acl    : Acl 的 table（varint32 len ‖ uid ‖ gid ‖ perm ‖ …）   │
  │   ├ nlink  : uint16 原生 2 bytes                                   │
  │   ├ atime  : UtcTime（int64 微秒）                                 │
  │   ├ ctime  : UtcTime                                              │
  │   └ mtime  : UtcTime                                              │
  └───────────────────────────────────────────────────────────────────┘
  ┌─ table #2：Inode 自己的欄位 ──────────────────────────────────────┐
  │ varint32 len2                                                     │
  │   └ id     : InodeId（is_serde_copyable → 原生 8 bytes）           │
  └───────────────────────────────────────────────────────────────────┘
```

寫入端：`refl::Helper::iterate<T, true>` 倒著走，走到 `id`（Inode）→ `mtime`（InodeData）這個轉折時觸發回呼 `out.tableEnd(start), start = out.tableBegin(false)`（`Serde.h:286`）。
讀取端：`refl::Helper::iterate<T>` 正著走，走到同一個轉折時觸發 `table = in.parseTable()`（`Serde.h:508-512`），換到第二張 table。

**這個設計的相容性後果非常正面**：往 `InodeData`（基底）尾端加欄位是安全的，因為它加在 table #1 的尾端，舊讀者讀完 6 個欄位後 table #1 就見底，第 7 個欄位被略過；然後外層依 `len1` 精確跳到 table #2 的開頭，`id` 照樣讀對。**如果沒有這個 table 分割，往基底加欄位就會把所有衍生欄位往後推、直接毀掉整個結構。**

反過來說：**把一個扁平的 struct 重構成「基底 + 衍生」（或反過來合併），會改變 table 張數，是破壞性的線上格式變更**，即使欄位名稱與順序完全沒動。這一點在任何 IDL 系統裡都不存在，是這套「類別即 schema」設計獨有的陷阱。

### 5.4 向後相容規則

沒有欄位標籤、沒有版本協商，全部規則都是 §5.3.3 那兩條的推論。以下逐項對照。

| 變更 | 新讀者讀舊資料 | 舊讀者讀新資料 | 判定 |
|---|---|---|---|
| **在 struct 尾端加欄位** | table 提早見底 → 新欄位取預設值 | 多出來的位元組留在子視圖裡被丟棄 | ✅ 雙向安全 |
| **在 struct 中間插欄位** | 之後所有欄位位移，型別對不上 | 同左 | ❌ 靜默錯亂 |
| **刪除尾端欄位** | 多餘位元組被丟棄 | table 提早見底 → 取預設值 | ✅ 雙向安全 |
| **刪除中間欄位** | 位移 | 位移 | ❌ 靜默錯亂 |
| **調換欄位順序** | 位移 | 位移 | ❌ 靜默錯亂 |
| **改欄位名稱** | 二進位無影響 | 二進位無影響 | ✅ 二進位安全 / ❌ JSON·TOML 會壞（用 `type.name` 當鍵，`Serde.h:288`、`:515`） |
| **改基本型別寬度**（`uint32`→`uint64`） | 多讀 4 bytes，之後全錯 | 少讀 4 bytes | ❌ 靜默錯亂 |
| **改基本型別號誌**（`uint32`→`int32`） | 位元組數相同，值被重新解釋 | 同左 | ⚠️ 佈局安全、語意可能變 |
| **加 / 拿掉 `std::optional<>`** | 多/少一個 tag byte | 同左 | ❌ 靜默錯亂 |
| **往巢狀 struct 尾端加欄位** | 外層依 table 長度跳過 | 同左 | ✅ 雙向安全（最有價值的性質） |
| **往基底類別尾端加欄位** | table #1 提早見底 | 多餘位元組丟棄 | ✅ 雙向安全 |
| **扁平 struct ↔ 基底+衍生 重構** | table 張數變了 | 同左 | ❌ 靜默錯亂 |
| **`enum` 加列舉值** | 原生數值，讀者拿到不認識的值 | 同左 | ⚠️ 二進位不報錯；JSON/TOML 走名稱，會回 `kSerdeUnknownEnumValue`（`Serde.h:675`） |
| **`variant` 尾端加 alternative** | — | 舊讀者找不到型別名 → `kSerdeVariantIndexExceeded`（`Serde.h:571`） | ⚠️ 只有 `AutoFallbackVariant` 能救 |
| **`variant` alternative 改 C++ 名稱** | 名稱字串對不上 | 同左 | ❌ 除非事先定義 `kTypeNameForSerde` |
| **加 / 刪 RPC 方法** | `MethodExtractor` 表裡的洞 → `invalidId` | 舊 server 回 `kInvalidMethodID` | ✅ 有明確錯誤（見 §3.4） |

三個必須單獨強調的結論：

**(1) 「只能往尾端加、編號永不回收」是唯一的升級紀律。** 這跟 §3.3 講的 methodId 紀律是同一件事的兩個層面：方法編號靠人記得跳號（`StorageSerde` 的 4/14/15、`Mgmtd` 的 2 都是空洞），欄位順序靠人記得只往後加。沒有任何編譯期或執行期機制會告訴你違反了。

**(2) 錯誤是靜默的，不是報錯的。** 欄位位移之後，`parseCopyable` 只檢查「剩餘位元組數 ≥ sizeof(T)」（`Serde.h:725`），不檢查內容。一個 `uint64` 被錯位讀成兩個 `uint32` 完全不會報錯。真正會擋下來的只有兩種情況：長度前綴指向緩衝區外（`kSerdeInsufficientLength`），或字串長度荒謬到超出 table。所以升級事故的典型症狀不是連線失敗，而是**資料看起來合法但語意是垃圾**。唯一的第二道防線是 §3.2.1 的 CRC——但它只驗傳輸完整性，不驗結構。

**(3) 沒有版本協商。** `MessagePacket::version`（`src/common/serde/MessagePacket.h:67`）看起來像是為此準備的，實際上是**死欄位**：

- `ClientContext::call`（`src/common/serde/ClientContext.h:56-69`）設了 uuid、serviceId、methodId、flags、timestamp，**唯獨沒有設 version**；
- server 端 `CallContext::makeResponse` 只是把它原樣抄回去（`src/common/serde/CallContext.h:125` `send.version = packet_.version;`）；
- 全樹沒有任何一處比較過 `packet.version`。

所以每個封包都帶著 8 個位元組的全零 `Version`，純粹是佔位。節點之間的版本資訊實際上走的是完全不同的路徑：`flat::ReleaseVersion` 塞在 `AppInfo` 裡經由心跳上報給 mgmtd（`src/common/app/AppInfo.h:114`、`src/fbs/mgmtd/HeartbeatInfo.h:86`），是**帶外的**，不是每包協商的。

`AutoFallbackVariant`（`Serde.h:456`）是全框架唯一一個為「未來的未知」預留的機制：

```cpp
struct UnknownVariantType {
  SERDE_STRUCT_FIELD(type, String{});
  // TODO: add serializedBytes
};
template <typename... Ts>
using AutoFallbackVariant = std::variant<UnknownVariantType, Ts...>;
```

遇到不認識的型別名時，把名字塞進 `UnknownVariantType` 保留（`Serde.h:566-570`），不報錯。

**但這個機制目前是壞的，而且壞得比 `TODO` 暗示的嚴重。** 問題不只是「原始位元組沒保存下來」，而是**那些位元組根本沒有從串流裡被消費掉**。

`parseVariant` 只讀走型別名，然後把**同一個 `In` 串流的參照**當作 value 回傳（`Serde.h:716-720`）：

```cpp
Result<std::pair<std::string_view, In &>> parseVariant() {
  std::string_view typeName;
  RETURN_AND_LOG_ON_ERROR(serde::deserialize(typeName, *this));
  return std::pair<std::string_view, In &>(typeName, *this);   // ← 串流位置停在型別名之後
}
```

正常分支會接著呼叫 `deserialize(type, variant->second)` 把值讀掉；但未知型別分支**直接回傳**，跳過這一步（`Serde.h:562-572`）：

```cpp
if constexpr (is_auto_fallback_variant_v<T>) {
  UnknownVariantType uvt;
  uvt.type = variant->first;
  o = std::move(uvt);
  return Void{};              // ← 值的位元組從未被消費
}
```

於是串流指標停在變體值的**開頭**。外層 table 接下來讀的每一個欄位，都是從這個未消費的值中間開始解讀——**後續欄位全部錯位**。實務後果不是「少拿到一個欄位」，而是整個訊息從該點之後全部是垃圾，且多半不會乾淨地報錯，而是解出看似合理的亂數值。

所以 §5.4 相容性表裡「`variant` 尾端加 alternative」那一列的 ⚠️ 應理解為：`AutoFallbackVariant` **只有在該 variant 是訊息最後一個欄位時才安全**。要真正修好，`UnknownVariantType` 必須連同 `serializedBytes` 一起把值吞掉——這正是那句 `TODO` 沒做完的事。

### 5.5 三個後端與 `Out`/`In` 的分派

同一份 `serialize()`/`deserialize()` 程式碼服務三種輸出，靠 `Out<T>` 的偏特化與一個編譯期旗標分流：

```
                       serde::serialize(o, out)             Serde.h:267
                                  │
                 constexpr bool isBinaryOut =
                   requires { typename Out<O>::is_binary_out; };      :269
                                  │
        ┌─────────────────────────┼──────────────────────────┐
        ▼                         ▼                          ▼
  Out<DownwardBytes<A>>     Out<JsonObject>            Out<TomlObject>
  Out<std::string>          （folly::dynamic）         （toml11 的 toml::value）
  is_binary_out ✓            Serde.h:388 / Serde.cc:29   Serde.cc:91
        │                         │                          │
  key() 是 no-op            key() 寫進物件            key() 寫進 table
  欄位倒序 + table 分割     欄位正序 + 欄位名          欄位正序 + 欄位名
  enum → 原生數值           enum → magic_enum 名稱     enum → 名稱
  variant → 型別名 ‖ 值     {"type":…, "value":…}      同 JSON
```

可讀後端還多了一層轉換鉤子（`Serde.h:270-275`）：`serdeToReadable()` / `serializeReadable()` / `SerdeMethod<T>::serdeToReadable()` 只在**非二進位**時生效。典型用途是 `storage::UInt8Vector`（`src/fbs/storage/Common.h:321`）——二進位照實傳，JSON 只印 `"std::vector<uint8_t>(1048576)"`，避免 log 被 1 MB 的十六進位淹沒。`Status` 也是（`Serde.h:917`）：二進位是 `uint16 + optional<string>`，JSON 是 `{"code":…, "msg":…}`。

`fmt::formatter` 的特化（`Serde.h:980-998`）讓**任何 serde struct 都能直接 `XLOGF(INFO, "{}", req)`**，內部走 `toJsonString`；用 `{:?}` 時開啟 sortKeys + pretty。全樹的 RPC 日誌都靠這個。

### 5.6 效能設計

**零拷貝的 payload。** 這是最重要的一項。`MessagePacket<T>` 的 payload 型別是條件式的（`src/common/serde/MessagePacket.h:34`）：

```cpp
template <class T>
using Payload = std::conditional_t<std::is_same_v<T, Void>, std::string_view, PointerWrapper<T>>;
```

- 送出端用 `MessagePacket<Req>`，payload 是 `PointerWrapper<Req>`——只存一個指標，序列化時 `tableBegin/tableEnd` 包一層長度（`MessagePacket.h:76-80`）。
- 接收端一律先反序列化成 `MessagePacket<>`（= `MessagePacket<Void>`），payload 是 `std::string_view`。而「varint32 長度 + 位元組」的字串佈局跟 `PointerWrapper` 寫出來的「table 長度 + 內容」**位元組層面完全一致**，所以接收端不需要知道 `Req` 的型別就能把 payload 切出來——切出來的是**指向接收緩衝區的 view，零拷貝**。
- 真正的 `Req` 直到 `CallContext::call<F>()`（`src/common/serde/CallContext.h:53`）才從那個 view 反序列化。緩衝區靠 `Processor::processSerdeRequest` 的 `(void)buf;  // keep alive.`（`src/common/net/Processor.h:163`）撐著生命週期。

**一次配置的緩衝區。** `net::Allocator<1_KB>`（`src/common/net/Allocator.h:12`）是帶 TLS 快取的物件池：≤1 KB 的請求直接從 thread-local 的 `ObjectPool` 拿一塊，離開時還回去，**完全不碰 malloc**。超過 1 KB 才 `new uint8_t[]`。`DownwardBytes::reserve` 的擴張策略是 `max(size, capacity * 2)`（`DownwardBytes.h:74`），所以大訊息最多幾次重新配置。

**三個特化的 `DownwardBytes`。** 這是同一套序列化程式碼服務三種需求的手法：

| 特化 | 行為 | 用途 |
|---|---|---|
| `DownwardBytes<net::Allocator<>>` | 正常配置 | `serialize()` / `serializeBytes()`（`Serde.h:805`、`:811`） |
| `DownwardBytes<void>`（`DownwardBytes.h:104`） | `append()` **只累加長度、不寫任何東西** | `serializeLength()`（`Serde.h:823`）——先算長度再決定要不要壓縮 / 要配多大 |
| `DownwardBytes<UserBufferAllocator>`（`:118`） | 寫進呼叫端給的固定緩衝區，溢位就 `XLOGF(FATAL)` | `serializeToUserBuffer()`（`Serde.h:817`）——USRBIO 的共享記憶體路徑 |

`DownwardBytes<void>` 這招值得單獨看：它讓「量長度」跟「真的寫」共用同一份 `serialize()` 邏輯，不需要維護第二份會不同步的長度計算函式。

**`release()` 交出所有權。** `DownwardBytes::release(offset, capacity)`（`DownwardBytes.h:89`）把裸指標讓出去，讓 `net::WriteItem` 直接持有序列化結果、不再複製一次（配合 §3.2.2 的 timestamp 原地補寫）。

**沒有做的事**（誠實列一下）：沒有 arena、沒有序列化結果快取、沒有 SIMD、整數不做變長壓縮。整體策略是「讓每個欄位的成本盡量接近一次 memcpy」，而不是「讓位元組數盡量小」。線上體積的壓縮交給 ZSTD（§3.8）。

### 5.7 與 RPC 框架的結合

呼應 §3，serde 在 RPC 路徑上出現在五個位置：

```
 ① Service.h 的 SERDE_SERVICE_METHOD 用同一套 refl 機制記錄「方法清單」
   （MethodInfo 而非 FieldInfo，Service.h:114-126），
   MethodExtractor 在 consteval 建構子裡把它攤成跳表 → §3.4

 ② ClientContext::call 組出 MessagePacket<Req>，payload 是 PointerWrapper（零拷貝）
   → WriteItem::createMessage → serde::serializeToUserBuffer / serializeBytes

 ③ SerdeBuffer 序列化完成後，用指標算術回頭改寫最後 64 bytes 的 Timestamp
   （§3.2.2）——這只成立於「二進位倒著寫 ⇒ 最後一個欄位落在緩衝區尾端」

 ④ Processor::unpackSerdeMsg 先 deserialize 成 MessagePacket<>（payload = view）
   → 依 serviceId/methodId 分派 → CallContext::call<F> 才 deserialize 真正的 Req

 ⑤ 回應走 Result<Rsp>：SerdeMethod<Result<T>> 把「成功/失敗」壓成一個 bool
   （Serde.h:932），失敗時接 Status（code + optional message）
```

第 ⑤ 點是很省的設計：整個 RPC 沒有獨立的錯誤通道，錯誤就是回應 payload 裡的一個 bool 加一個 `Status`。`DefaultConstructor<Result<T>>::construct()`（`Serde.h:139`）與 `DefaultConstructor<Status>`（`:134`）的存在，是因為 `Result<T>` 與 `Status` 都沒有預設建構子，容器反序列化時需要一個「先造個殼再填」的入口。

另外 `serde::SerdeHelper<T>`（`src/common/serde/SerdeHelper.h:8`）把同一套序列化直接接到 KV 層：

```cpp
CoTryTask<void> store(kv::IReadWriteTransaction &txn, std::string_view key) const;  // :16
static CoTryTask<std::optional<T>> load(kv::IReadOnlyTransaction &txn, std::string_view key);  // :21
```

**所以 RPC 線上格式與 FoundationDB 裡的持久化格式是同一套位元組。** 這放大了 §5.4 的風險等級：改一個欄位順序不只讓叢集內兩個版本的節點互相聽不懂，還會讓**已經寫進 FDB 的舊資料再也讀不回來**。`defaultValueForLoad()` 這個逃生門（`SerdeHelper.h:50`）就是為了讓某些型別在「從舊資料讀進來、尾端欄位缺席」時能取到跟 `T{}` 不同的預設值。

### 5.8 `src/fbs/` 的真相：這裡沒有 flatbuffers

任務要求分析「serde 與 flatbuffers 的分工」。實測結果是：**這棵樹裡沒有 flatbuffers。**

證據：

1. `find src/fbs -type f` 得到 66 個檔案，**全部是 `.h` / `.cc` / `CMakeLists.txt`，沒有任何 `.fbs` schema**。全 repo（排除 `third_party/`）也沒有任何 `.fbs`。
2. `src/fbs/` 底下每一個資料型別都是 serde struct。隨手驗證：
   - `src/fbs/mgmtd/ConfigInfo.h:7` `struct ConfigInfo : public serde::SerdeHelper<ConfigInfo>` + 三個 `SERDE_STRUCT_FIELD`
   - `src/fbs/storage/Common.h:309` `struct ReadIO { SERDE_STRUCT_FIELD(...) }` + `static_assert(serde::Serializable<ReadIO>)`
   - `src/fbs/core/service/Rpc.h:15` 起的 `GetConfigReq` / `RenderConfigReq` / `HotUpdateConfigReq` 全是 serde struct
   - `src/fbs/mgmtd/MgmtdServiceBase.h:7` 用 `SERDE_SERVICE_2(MgmtdServiceBase, Mgmtd, 217)` + `SERDE_SERVICE_METHOD_REFL`
3. CMake 端還留著 `macro(target_add_fbs NAME PATH)`（`cmake/Target.cmake:70`），裡面確實呼叫 `build_flatbuffers(...)` 並帶著 `--hf3fs` 這個自訂 flatc 參數（`cmake/Target.cmake:89`）——但**全樹沒有任何一處呼叫這個 macro**。`src/common/CMakeLists.txt:4` 那行孤零零的 `set(FLATBUFFERS_FLATC_SCHEMA_EXTRA_ARGS --cpp-std=c++17)` 也是死設定。
4. `src/fbs/macros/` 裡的 `IStub.h` / `Stub.h` 保留了 `DEFINE_FBS_SERVICE` / `DEFINE_FBS_SERVICE_METHOD(svc, name, reqtype, rsptype, flatns)` 這些巨集，參數名 `flatns` 就是「flatbuffers namespace」；但現役的 `src/fbs/macros/SerdeDef.h` 已經換成 `DEFINE_SERDE_SERVICE_METHOD_FULL`，`src/fbs/mgmtd/MgmtdServiceDef.h` 這類定義檔全部走 serde 路線。
5. 命名空間 `hf3fs::flat` 保留至今（`ConfigInfo`、`NodeInfo`、`AppInfo`、`ChainInfo` 全在 `namespace hf3fs::flat`）——`flat` 就是 flatbuffers 的殘留。

**結論：`src/fbs/` 是「跨元件共用的資料型別與服務介面定義層」，名字是歷史包袱。** 開源版本的 3FS 曾經用 flatbuffers 做 RPC，後來整套換成 serde，只保留了目錄名、命名空間名、巨集參數名與一個沒人呼叫的 CMake macro。所以「什麼用 serde、什麼用 fbs」這個問題在當前程式碼裡的答案是：**全部用 serde，沒有第二套**。

真正存在「兩套並行」的地方是 TOML 函式庫，不是序列化框架——見 §1.2 講的 toml11（serde 用）與 toml++（設定系統用）共存問題。

---

## 6. 支柱③ 設定系統（`src/common/utils/ConfigBase.h` + `src/common/app/`）

設定系統跟 serde 是**兩套完全獨立的反射**。serde 的反射發生在編譯期（`CollectField` 重載鏈），設定系統的反射發生在**建構期**——靠「成員初始化器的副作用」把每個設定項的 pointer-to-member 塞進一張 `std::map`。兩套機制互不知道對方存在。

### 6.1 巨集家族與展開結果

`src/common/utils/ConfigBase.h` 定義了 6 個巨集。全樹用量約：`CONFIG_HOT_UPDATED_ITEM` 302 處、`CONFIG_ITEM` 206 處、`CONFIG_OBJ` 143 處、`CONFIG_OBJ_ARRAY` 2 處、`CONFIG_SECT` 3 處、`CONFIG_VARIANT_TYPE` 1 處。**熱更新項比冷項還多**——這是刻意的預設姿態。

#### 6.1.1 `CONFIG_ITEM` / `CONFIG_HOT_UPDATED_ITEM`

兩者是同一個巨集的兩個包裝，差別只有一個布林參數（`ConfigBase.h:115-116`）：

```cpp
#define CONFIG_ITEM(name, defaultValue, ...)             CONFIG_ADD_ITEM(name, defaultValue, false, __VA_ARGS__)
#define CONFIG_HOT_UPDATED_ITEM(name, defaultValue, ...) CONFIG_ADD_ITEM(name, defaultValue, true,  __VA_ARGS__)
```

以 `src/common/net/Processor.h:35` 的 `CONFIG_HOT_UPDATED_ITEM(response_compression_level, 1u);` 為例，`CONFIG_ADD_ITEM`（`ConfigBase.h:96`）展開成：

```cpp
 private:
  using Tresponse_compression_level = ::hf3fs::config::ValueType<std::decay_t<decltype(1u)>>;   // = uint32_t
  using Rresponse_compression_level = ::hf3fs::config::ReturnType<Tresponse_compression_level>; // = uint32_t（純值）

 public:
  auto response_compression_level_getter() const { return [this] { return response_compression_level(); }; }
  Rresponse_compression_level response_compression_level() const { return response_compression_level_.value(); }
  bool set_response_compression_level(Rresponse_compression_level v) {
    return response_compression_level_.checkAndSet(v);
  }

 private:
  ::hf3fs::config::Item<Tresponse_compression_level> response_compression_level_ =
      ::hf3fs::config::Item<Tresponse_compression_level>(
          "response_compression_level",
          1u,
          [this] {                                        // ← 這個 lambda 是整個機制的核心
            using Self = std::decay_t<decltype(*this)>;
            ConfigBase<Self>::items_["response_compression_level"] =
                reinterpret_cast<::hf3fs::config::IItem Self::*>(&Self::response_compression_level_);
            return true;                                  // ← supportHotUpdated
          }());
```

關鍵在第三個建構參數：**它是一個立即呼叫的 lambda，回傳 `supportHotUpdated`，但真正的目的是它的副作用**——把 `&Self::response_compression_level_` 這個 pointer-to-member 註冊進基底類別的 `items_` map。C++ 保證成員初始化器按宣告順序執行，而 `items_` 宣告在基底 `ConfigBase`（`ConfigBase.h:854`）裡、比任何衍生成員都早構造完，所以註冊時 map 已經可用。

`ValueType` / `ReturnType`（`ConfigBase.h:305-309`）處理兩件小事：`const char*` 一律換成 `std::string`；`trivially_copyable && sizeof ≤ 8` 的型別按值回傳，其餘按 `const&` 回傳。這個 8 bytes 的分界線同時也決定了執行期的儲存方式（§6.3）。

第四個可選參數是 **checker**，例如 `src/kv/KVStore.h:26` `CONFIG_ITEM(leveldb_sst_file_size, 16_MB, ConfigCheckers::checkGE<size_t, 4_MB>);`。checker 在三個時機被呼叫：`validate()`（`ConfigBase.h:424`）、`update()` 寫入前（`:465`）、`set_xxx()` 手動設定（`:401`）。沒給 checker 時預設是 `[](auto){ return true; }`（`:370`）。

#### 6.1.2 `CONFIG_OBJ` / `CONFIG_SECT` / `CONFIG_OBJ_ARRAY`

`CONFIG_OBJ`（`ConfigBase.h:44`）用同一套「初始化器副作用」註冊子設定，只是進的是 `sections_` 而不是 `items_`：

```cpp
#define CONFIG_OBJ(name, cls, ...)                                                                           \
 public:                                                                                                     \
  cls &name() { return name##_; }                                                                            \
  const cls &name() const { return name##_; }                                                                \
 private:                                                                                                    \
  cls name##_;                                                                                               \
  [[maybe_unused]] bool name##Insert_ = [this]() {                                                           \
    using Self = std::decay_t<decltype(*this)>;                                                              \
    ConfigBase<Self>::sections_[#name] = reinterpret_cast<::hf3fs::config::IConfig Self::*>(&Self::name##_); \
    __VA_OPT__(__VA_ARGS__(name##_);)   /* 選用：對子物件跑一次初始化 lambda */                                \
    return true;                                                                                             \
  }()
```

可選的第三參數讓上層改掉子設定的預設值，例如 `src/common/net/IOWorker.h:35`：

```cpp
CONFIG_OBJ(connect_concurrency_limiter, ConcurrencyLimiterConfig, [](auto &c) { c.set_max_concurrency(4); });
```

**注意這個 `reinterpret_cast` 是 UB 邊緣的關鍵手法**：把 `cls Self::*`（指向具體子設定型別）硬轉成 `IConfig Self::*`。之所以能成立，是因為 `cls` 一定繼承 `ConfigBase<cls>` 繼承 `IConfig`，而 `IConfig` 是第一個（也是唯一的多型）基底，偏移量為 0。`CONFIG_OBJ_ARRAY`（`ConfigBase.h:58`）更進一步，直接對成員指標做整數算術：

```cpp
auto base = reinterpret_cast<::hf3fs::config::IConfig Self::*>(&Self::name##_);
static_assert(sizeof(base) == sizeof(uintptr_t), "sizeof(base) != sizeof(uintptr_t)");   // :81
for (auto i = 0ul; i < k_capacity_##name; ++i) {
  ConfigBase<Self>::sections_[fmt::format(#name "#{}", i)] = base;
  *reinterpret_cast<uintptr_t *>(&base) += sizeof(cls);        // :84  成員指標當整數加
}
```

把 `std::array<cls, N>` 的每個元素各註冊成一個名為 `groups#0`、`groups#1`… 的 section。這是全份設定系統裡最脆弱的一段：它假設 pointer-to-member 就是一個 `uintptr_t` 偏移量（有 `static_assert` 守著）、且陣列元素是 `sizeof(cls)` 等距排列。用它的只有兩處：`src/common/net/Server.h:26` 的 `groups`（上限 4）與 `src/common/monitor/Monitor.h:50` 的 `reporters`（上限 4）。陣列長度單獨存在 `lengths_` map 裡，載入 TOML 時依 array-of-tables 的實際長度回填（`ConfigBase.h:619`）。

`CONFIG_SECT(name, section)`（`ConfigBase.h:91`）只是「就地定義一個匿名子設定類別 + `CONFIG_OBJ`」的語法糖。

#### 6.1.3 `CONFIG_VARIANT_TYPE`

全樹只有一個使用者：`src/common/monitor/Monitor.h:43`。

```cpp
class ReporterConfig : public ConfigBase<ReporterConfig> {
  CONFIG_VARIANT_TYPE("clickhouse");
  CONFIG_OBJ(clickhouse, ClickHouseClient::Config);
  CONFIG_OBJ(log, LogReporter::Config);
  CONFIG_OBJ(monitor_collector, MonitorCollectorClient::Config);
};
```

展開後（`ConfigBase.h:118`）是「一個名為 `type` 的字串設定項 + 一個 checker + 一個標記函式」：

```cpp
CONFIG_ITEM(type, std::string{"clickhouse"}, [this](const std::string &name) {
  using Self = std::decay_t<decltype(*this)>;
  return ConfigBase<Self>::sections_.count(name);        // checker：type 必須是既有 section 的名字
});
 public:
  constexpr static inline bool is_variant_type() { return true; }
```

兩個效果：

1. **checker 保證 `type` 只能填成兄弟 section 的名稱**——寫 `type = "kafka"` 會在載入時就被 `kConfigValidateFailed` 擋下。
2. `toToml()` 遇到 `is_variant_type` 時**只輸出被選中的那個 section**（`ConfigBase.h:648-654`），其餘的不印。

但要注意**它只影響輸出、不影響輸入**：`update()` 沒有對應的分支，所以 TOML 檔裡即使同時寫了 `[clickhouse]` 和 `[log]` 兩段，兩段都會被套用，只是之後 `toToml()` 只印出 `type` 指到的那一段。`render-config` / `verify-config` 的比對（§6.7）因此可能出現「送進去的內容跟印出來的不一樣」的假 diff。

### 6.2 執行期資料結構

```
  config::IConfig（純虛，ConfigBase.h:149）
    │  update / validate / toToml / find / diffWith / atomicallyUpdate / init
    ▼
  ConfigBase<Parent>（CRTP，ConfigBase.h:582）
    ├ mutable std::mutex mutex_                                          :850
    ├ map<string, IConfig Parent::*>  sections_    ← CONFIG_OBJ 註冊     :852
    ├ map<string, IItem   Parent::*>  items_       ← CONFIG_ITEM 註冊    :854
    ├ map<string, size_t  Parent::*>  lengths_     ← CONFIG_OBJ_ARRAY    :856
    └ mutable set<ConfigCallbackGuard *> callbacks_                      :858
                              │
                              ▼
            config::Item<T>（ConfigBase.h:364）
              ├ StoreType<T> value_         ← AtomicValue 或 TLSStore
              ├ string       name_
              ├ bool         supportHotUpdate_
              └ function<bool(ReturnType<T>)> checker_
```

`ConfigBase` 的拷貝語意是特製的（`ConfigBase.h:585-589`）：

```cpp
ConfigBase(const ConfigBase &o) : sections_(o.sections_), items_(o.items_), lengths_(o.lengths_) {}
ConfigBase &operator=(const ConfigBase &) { return *this; }      // 什麼都不做
```

拷貝建構複製三張註冊表（pointer-to-member 是型別相對的偏移量，對同型別的任何實例都有效，所以可以直接複製）；**但 `callbacks_` 刻意不複製**。這一點在 §6.4 的 dry-run 機制裡是關鍵：clone 出來的設定物件沒有任何回呼，所以試套用不會誤觸副作用。賦值運算子什麼都不做，避免把註冊表覆蓋掉。

### 6.3 熱更新的執行緒安全

這是整章最重要的部分。設計上有**兩條完全不同的路徑**，由型別大小決定（`ConfigBase.h:303-307`）：

```cpp
template <class T> inline constexpr bool IsPrimitive = std::is_trivially_copyable_v<T> && sizeof(T) <= 8;
template <class T> using ReturnType = std::conditional_t<IsPrimitive<T>, T, const T &>;
template <class T> using StoreType  = std::conditional_t<IsPrimitive<T>, AtomicValue<T>, TLSStore<T>>;
```

#### 6.3.1 小型別：`AtomicValue<T>` —— 就是一個 `std::atomic`

`src/common/utils/AtomicValue.h:9`。讀是 `value_.load(seq_cst)`、寫是 `value_.store(seq_cst)`，按值回傳。整數、布林、enum、`Duration`、`Size` 全走這條。**讀者永遠拿到某個時刻的完整值，不可能撕裂，也不需要任何鎖。** 這覆蓋了絕大多數熱更新項（`response_compression_level`、`collect_period`、`enable_coroutines_pool`…）。

#### 6.3.2 大型別：`TLSStore<T>` —— atomic_shared_ptr + 每執行緒版本快取

`ConfigBase.h:261`：

```cpp
template <class T>
class TLSStore {
 public:
  const T &value() const {
    auto &cache = *tlsCache_;
    size_t latest = version_.load(std::memory_order_acquire);
    if (UNLIKELY(cache.version != latest)) {       // 版本沒變 → 完全不碰共享狀態
      cache.ptr = ptr_.load(std::memory_order_acquire);
      cache.version = latest;
    }
    return *cache.ptr;                             // 回傳的是 TLS 快取持有的那份
  }

  template <class V>
  void setValue(V &&value) {
    ptr_.store(std::make_shared<T>(std::forward<V>(value)));   // 換上新的一份
    ++version_;                                                // 通知所有執行緒
  }

 private:
  std::atomic<uint64_t> version_ = 1;
  folly::atomic_shared_ptr<T> ptr_{std::make_shared<T>()};
  struct Cache { std::shared_ptr<const T> ptr; uint64_t version = 0; };
  folly::ThreadLocal<Cache> tlsCache_;
};
```

**「讀的人正在用舊值怎麼辦」的答案是：讓他繼續用完，靠 `shared_ptr` 撐住那份記憶體。**

```
 執行緒 A                 共享狀態                     執行緒 B（更新者）
 ────────                 ────────                     ──────────────────
 value()                  version_ = 5
  ├ latest=5              ptr_ ──▶ [V5]  ◀── A 的 TLS cache.ptr 持有一份 refcount
  ├ cache.version==5 相符
  └ return *cache.ptr ──▶ [V5]
                                                        setValue(V6):
                                                          ptr_.store(make_shared(V6))
                          ptr_ ──▶ [V6]                   ++version_  → 6
                          [V5] refcount 仍 ≥1（A 的 TLS 快取）
 （A 手上的 const T& 依然有效、依然是 V5）

 value()   ← A 下一次呼叫
  ├ latest=6
  ├ cache.version(5) != 6 → cache.ptr = ptr_.load() → [V6]
  │                          ↑ 此時 [V5] 的最後一個 refcount 被釋放
  └ return *cache.ptr ──▶ [V6]
```

三個必須說清楚的性質：

1. **穩態下零同步成本。** 沒有更新時，`value()` 只做一次 `version_` 的 acquire load 與一次比較，完全不碰 `atomic_shared_ptr`（那東西在 folly 裡是有成本的）。這對「每個 RPC 都要讀好幾次設定」的熱路徑很重要。
2. **不會有 use-after-free，但會有「陳舊讀」。** 一個執行緒在更新後、下一次呼叫 `value()` 之前，讀到的都是舊值。系統不保證更新的即時可見性，只保證最終可見。
3. **危險：`value()` 回傳的 `const T&` 不能跨越同執行緒的下一次 `value()`。** 因為下一次呼叫可能把 `cache.ptr` 換掉，讓上一次回傳的參考變成懸空。正確用法是 `auto v = cfg.xxx();`（複製）或在單一運算式內用掉；`auto &v = cfg.xxx();` 存下來再過一陣子用是錯的。程式碼裡沒有任何機制阻止這種寫法，這是整套熱更新最容易寫錯的地方。

#### 6.3.3 `mutex_` 保護的是註冊表遍歷，不是值

`ConfigBase::update()`（`ConfigBase.h:593-638`）開頭 `auto lock = std::unique_lock(mutex_);`，`toToml()`（`:641`）、`clone()`（`:727`）、`copy()`（`:733`）、`addCallbackGuard()`（`:784`）也都拿同一把鎖。**但 `Item::value()` 讀值時完全不拿鎖**——鎖只是用來讓「多個更新者不會同時遍歷/修改同一層」以及「clone 時不會撞到正在跑的 update」。真正的讀寫安全交給 `AtomicValue` / `TLSStore`。

這裡有兩個必須知道的漏洞：

- **`IConfig::update(const std::vector<KeyValue>&, bool)`（`src/common/utils/ConfigBase.cc:189`）完全不拿鎖。** 它走 `find(key)` 拿到 `IItem*` 之後直接 `item.update(...)`。命令列 `--config.x=y` 與 `ApplicationBase::loadConfig` 的 flags 套用都走這條。啟動期單執行緒所以沒事，但這代表「鎖」不是一個全域不變量。
- **`clone()` 只鎖最上層的 `mutex_`**（`ConfigBase.h:727`），拷貝建構會遞迴複製所有子設定物件，但**不會鎖住子物件的 `mutex_`**。跟一個正在進行中的 `update()` 併發時，理論上可以 clone 到一份「一半新一半舊」的設定。實務上 `update()` 從最上層進入且持有上層鎖，所以正常路徑不會撞；但 §6.7 的 `set-config`（走 mgmtd 心跳）與 `hot-update-config`（走 Core RPC）是兩條不同的入口，`ApplicationBase` 用一把全域 `appMutex`（`ApplicationBase.cc:115`、`:160`）把它們串起來，這才是真正的序列化點。

#### 6.3.4 `ConfigCallbackGuard`：更新後的副作用

有些設定不是「讀值」就夠了，改了之後得做事——重開日誌、增減協程數、切換 jemalloc profiling。機制是 `ConfigCallbackGuard`（`ConfigBase.h:244`）：

```cpp
auto guard = cfg.addCallbackGuard([&]{ /* 重新套用 */ });    // :782
```

`ConfigBase::update()` 在**所有欄位都套用完之後**、`overallValidate()` 之前，統一觸發本層所有回呼（`ConfigBase.h:634-637`）：

```cpp
for (auto &callback : callbacks_) { callback->callCallback(); }
return overallValidate();
```

三個現役使用者：

| 使用者 | 做什麼 | 出處 |
|---|---|---|
| `makeLogConfigUpdateCallback` | 重新產生 folly logging 設定字串並 `initOrDie` | `src/common/app/Utils.cc:194` |
| `makeMemConfigUpdateCallback` | 開關 jemalloc profiling、dump heap、印 allocator 統計 | `src/common/app/Utils.cc:198` |
| `DynamicCoroutinesPool` | 線上增減協程數 / 執行緒數（見 §3.10） | `src/common/utils/DynamicCoroutinesPool.h:14` |

注意 `callCallback()` 是**在持有 `mutex_` 的情況下被呼叫的**。回呼裡若呼叫同一個設定物件的 `toToml()` / `clone()` 會直接死鎖。現有的回呼都只呼叫 item getter（不拿鎖），所以安全——但這是一條沒有寫下來的約定。

`~ConfigCallbackGuard()` 會 `dismiss()` 自己從 `callbacks_` 移除（`ConfigBase.h:249`、`:253`），所以 guard 的生命週期必須短於設定物件。`TwoPhaseApplication` 把它們存成成員（`src/common/app/TwoPhaseApplication.h:111-112`），跟 `config_` 同生共死。

### 6.4 TOML 載入、型別轉換與驗證

#### 6.4.1 從 TOML 節點到 C++ 值

`tomlNodeToValue<T>`（`ConfigBase.h:311-361`）是一個 `node.visit()` + `if constexpr` 階梯，規則相當嚴格：

| 目標型別 | 接受的 TOML | 拒絕 |
|---|---|---|
| 型別完全相同 | 直接取 | — |
| `bool` | **只接受 TOML boolean** | 明確拒絕任何隱式轉換（`:322-325`，註解寫著 `do not allow any implicit conversion to bool`） |
| 浮點 | `int64` 或 float | 字串 |
| `enum` | **字串**，經 `magic_enum::enum_cast` | 整數（`:341` 明確報錯） |
| 有 `T::from(E)` 的型別 | 走 `T::from` | — |
| `std::constructible_from<T, E>` | 直接建構，包 try/catch | — |
| 其他 | `kConfigInvalidType` | — |

容器另有專門路徑：`updateVectorOrSet`（`:481`）要求 array，`set` 重複元素回 `kConfigUpdateFailed`；`updateMap`（`:532`）要求 table，重複鍵回 `kConfigRedundantKey`。向量的元素若本身是 `IConfig`（子設定陣列），會遞迴呼叫 `v.update(*e.as_table(), /*isHotUpdate=*/false)`（`:501`）——**注意這裡寫死 false**，所以巢狀在 vector 裡的子設定永遠當作冷更新處理。

#### 6.4.2 錯誤碼與錯誤訊息

錯誤全部經由 `Result<Void>` 回傳、帶完整路徑：

| 錯誤碼 | 觸發條件 |
|---|---|
| `kConfigParseError` | TOML 本身語法錯（`ConfigBase.cc:88`、`:118`） |
| `kConfigInvalidType` | 型別對不上、section 不是 table、陣列項不是 table |
| `kConfigInvalidValue` | enum 名稱不存在、檔案不存在 |
| `kConfigRedundantKey` | **TOML 裡有二進位不認識的鍵**（`ConfigBase.h:629`） |
| `kConfigValidateFailed` | checker 回 false |
| `kConfigUpdateFailed` | 想熱更新一個非熱更新項且值真的變了（`ConfigBase.h:472`） |
| `kConfigKeyNotFound` | `toToml(key)` 找不到路徑 |

路徑靠 `config::concat`（`ConfigBase.h:575`）逐層拼出來，所以錯誤訊息會長成 `Not support hot update: server.groups#0.processor.max_coroutines_num` 這種可以直接拿去改的形式。

**`kConfigRedundantKey` 是升級時最容易炸的一個**：`ConfigBase::update` 遍歷的是**傳進來的 TOML 表**，任何在 `items_` / `sections_` / `lengths_` 三張表裡都找不到的鍵，直接讓整份設定套用失敗。所以「從新版二進位刪掉一個設定項，但 mgmtd 上還存著提到它的舊設定」＝ 那個節點永遠套不上設定。反過來「新增設定項」是安全的（舊 TOML 沒提到它，就用編譯期預設值）。

#### 6.4.3 兩段式套用：`atomicallyUpdate` 的 dry-run

```cpp
// ConfigBase.h:739
Result<Void> atomicallyUpdate(std::string_view str, bool isHotUpdate) final {
  Parent newConfig = clone();                              // 1. 複製一份（不含 callbacks_）
  RETURN_ON_ERROR(newConfig.update(str, isHotUpdate));     // 2. 先套在副本上
  auto res = update(str, isHotUpdate);                     // 3. 副本成功 → 套在本體上
  XLOGF_IF(FATAL, !res, "Unexpected update error: {}", res.error());
  return Void{};
}
```

名字叫 atomically，實際語意是「**先在副本上試一遍，全部通過才套用到本體；本體套用若失敗就直接 abort 整個行程**」。

- 這**不是**交易式的原子性。第 3 步是逐項寫入的，如果中途失敗，設定已經處於半新半舊狀態——所以作者選擇 `XLOGF(FATAL)` 直接掛掉，因為那代表「副本能過、本體不能過」，是不可能發生的邏輯矛盾（clone 語意有 bug 才會出現）。
- 真正的價值在第 2 步：**把「格式錯／型別錯／checker 不過／改到非熱更新項」全部擋在副本上**，本體一個位元組都不動。所以 mgmtd 推一份壞設定下來，節點會乾淨地拒絕，不會變成半套狀態。
- clone 不帶 `callbacks_`（§6.2）是這件事成立的前提：dry-run 不會重開日誌、不會動協程池。

`validateUpdate()`（`ConfigBase.h:764`）就是只做第 1、2 步、不做第 3 步的版本——`admin_cli verify-config` 用的就是它。

#### 6.4.4 一個容易誤解的語意：update 是「合併」不是「取代」

`ConfigBase::update` 遍歷的是**傳進來的 TOML 表**，不是自己的 `items_`。所以 **TOML 裡沒提到的設定項會保持當前執行期的值，不會回到預設值**。

這對兩件事有直接影響：

1. 熱更新可以只推一個片段：`admin_cli hot-update-config -s 'a.b.c = 5'` 就只改那一項。
2. 但也意味著 mgmtd 推一份「完整設定」下來時，**它其實也是合併**。如果某個節點先前被 `hot-update-config` 手動改過一個不在 mgmtd 那份設定裡的項，推完整設定並不會把它改回去。這正是 `ConfigStatus::DIRTY` 存在的理由，見 §6.6。

### 6.5 設定值的來源與覆寫優先序

把 `ApplicationBase` / `TwoPhaseApplication` / `app_detail` 三處串起來，一個 server 行程的設定生命週期是：

```
 ① 編譯期預設值
      CONFIG_ITEM(name, defaultValue) 的第二參數，寫死在 .h 裡
      → 每個 Item 的建構子直接吃下（ConfigBase.h:366-370）
                    │
                    ▼
 ② 設定模板（三選一）                       src/common/app/Utils.cc:269 loadConfigTemplate
      --cfg=<path> 非空        → 讀本機檔案                         :262 loadConfigFromFile
      --use_local_cfg          → 用 cfg.toString()（＝純預設值）    :272
      兩者皆無                  → 向 launcher 要（實際上是向 mgmtd 拉）:218 loadConfigFromLauncher
                                  重試 20 次、指數退避 10ms→1s，
                                  失敗直接 XLOGF(FATAL)
                    │
                    ▼
 ③ renderConfig(template, appInfo)          src/common/utils/RenderConfig.cc:71
      OSS 版是 identity（原樣回傳）。介面預留了 appInfo / envs 兩個參數，
      顯然內部版會做 {{nodeId}} / {{hostname}} 這類模板展開。
                    │
                    ▼
 ④ cfg.atomicallyUpdate(rendered, isHotUpdate=false)     Utils.cc:289
      冷更新 → 所有設定項（含非熱更新項）都可以被寫入
                    │
                    ▼
 ⑤ 命令列 --config.<path>=<value>            Utils.cc:292 → ConfigBase.cc:189
      逐項套用，isHotUpdate=false。
      值被包成 `v = <value>` 或 `v = """<value>"""` 再丟給 toml 解析（ConfigBase.cc:197），
      是否加三引號由 Item::isParsedFromString() 決定（ConfigBase.h:454）
                    │
                    ▼
 ⑥ validate()（checker 全跑一遍） + persistConfig（落檔備查）
                    │
                    ▼  ───── 行程開始跑 ─────
                    │
 ⑦ 執行期熱更新（isHotUpdate=true），兩條路徑：
      (a) mgmtd 心跳回應帶新 ConfigInfo → ConfigListener → ApplicationBase::updateConfig
      (b) Core service 的 hotUpdateConfig RPC → ApplicationBase::hotUpdateConfig
      兩者都只能改 CONFIG_HOT_UPDATED_ITEM；碰到值真的改變的 CONFIG_ITEM 就整份拒絕
```

幾個容易看漏的細節：

- **`--config.` 這條路徑優先序最高，而且會造成 DIRTY。** 它在 ④ 之後套用，蓋掉檔案/mgmtd 的值；但它不會被寫回 `lastConfigContent`，所以 §6.6 的 diff 比對會永遠顯示不一致。
- **`configPushable()`（`TwoPhaseApplication.h:86`）= `FLAGS_cfg.empty() && !FLAGS_use_local_cfg`。** 只要用了本機設定檔或 `--use_local_cfg`，這個節點就**拒絕接受 mgmtd 推下來的設定**（`ApplicationBase.cc:126` 回 `kCannotPushConfig`）。這是「本機設定檔 = 我自己管，別碰我」的明確語意。
- **`persistConfig`（`Utils.cc:177`）每次設定變更後都會把完整設定 dump 成一個帶時間戳的新檔案**（`{cfg_persist_prefix}_{YYYYmmdd_HHMMSS}_{微秒}`），`onConfigUpdated()` 在每次成功熱更新後再呼叫一次（`TwoPhaseApplication.h:88`）。所以磁碟上會累積歷次設定快照，方便事後追查「當時到底跑的是哪份設定」。`cfg_persist_prefix` 為空時整個功能關閉。
- **`--dump_default_cfg` / `--dump_cfg`（`ConfigBase.cc:14-16`）** 分別在 ① 之後與 ⑤ 之後印出設定並 `exit(0)`。這是產生設定模板的標準做法。

### 6.6 mgmtd 下發與版本比對

#### 6.6.1 資料模型

`flat::ConfigInfo`（`src/fbs/mgmtd/ConfigInfo.h:7`）就三個欄位：

```cpp
struct ConfigInfo : public serde::SerdeHelper<ConfigInfo> {
  SERDE_STRUCT_FIELD(configVersion, ConfigVersion(0));
  SERDE_STRUCT_FIELD(content, String{});     // 整份 TOML 文字
  SERDE_STRUCT_FIELD(desc, String{});
};
```

mgmtd 端存的是 `configMap : NodeType → (ConfigVersion → ConfigInfo)`（`src/mgmtd/service/MgmtdData.h:27`），**每種節點型別一條獨立的版本鏈，歷史版本全部保留**。

#### 6.6.2 寫入：`set-config`

`SetConfigOperation::handle`（`src/mgmtd/ops/SetConfigOperation.cc:6`）：

```
 1. validateClusterId + validateAdmin（需要管理員身分）
 2. coScopedLock<"SetConfig">           ← 具名的寫入鎖
 3. 讀出該 NodeType 的最大版本 oldVersion（configMap[type].rbegin()->first）
 4. newVersion = nextVersion(oldVersion)     ← 單調遞增，全靠這把鎖保證
 5. 寫進 FDB（state.store_.storeConfig）     ← 先落地
 6. 寫進記憶體 configMap[type][newVersion]
 7. 若 type == MGMTD：mgmtd 自己就是接收者，立刻套用給自己
       updater(content, desc) → ApplicationBase::updateConfig
       成功才把 selfNodeInfo_.configVersion 與 nodeMap 裡自己那筆推進到 newVersion
```

第 7 步的細節值得注意：mgmtd 對自己的設定是**先套用、成功才推版本號**；`updater` 不存在時會 `LOG_OP_WARN(..., "blindly promote to {} since no updater found")` 然後盲推。這跟其他節點的流程（§6.6.3）是對稱的。

`admin_cli set-config -t <TYPE> -f <file>`（`src/client/cli/admin/SetConfig.cc`）只是把檔案內容整包送過去，回傳新的 `ConfigVersion` 印出來。**它不做任何驗證**——驗證是 `verify-config` 的工作，兩者是分開的，所以推一份會讓所有節點都套用失敗的設定是完全做得到的。

#### 6.6.3 下發：搭心跳的便車

**沒有獨立的設定推送通道**，設定是搭在心跳/續租的回應上傳下來的。

伺服器節點（meta / storage / mgmtd）走 `HeartbeatReq` → `HeartbeatRsp`：

```
 節點側（每次心跳）                         mgmtd 側
 ─────────────────────                      ────────
 heartbeatInfo_->configVersion = 我目前的版本
 heartbeatInfo_->configStatus  = ApplicationBase::getConfigStatus()
        src/client/mgmtd/MgmtdClient.cc:704
              │
              ├──── HeartbeatReq ─────────▶  checkConfigVersion(hb.configVersion)
              │                               src/mgmtd/ops/HeartbeatOperation.cc:73
              │                               版本比 server 還新 → kInvalidConfigVersion
              │
              │                              rsp.config = data.getConfig(type, version, latest=true)
              │                               src/mgmtd/ops/HeartbeatOperation.cc:196
              │                               → 只有 version < 最新版時才回傳，
              │                                 相同就回 nullopt（省頻寬）
              │                                 src/mgmtd/service/MgmtdData.cc:30-46
              │◀─── HeartbeatRsp{config?} ──┤
              │
 if (res->config) {
   listener = serverConfigListener_;         ← 就是 ApplicationBase::updateConfig
   if (!listener || (*listener)(content, desc))
     heartbeatInfo_->configVersion = res->config->configVersion;   // ★ 套用成功才推進版本
 }
        src/client/mgmtd/MgmtdClient.cc:734-739
```

FUSE / 純 client 節點走的是 `ExtendClientSessionReq`/`Rsp`，邏輯完全一樣（`src/client/mgmtd/MgmtdClient.cc:362-372`）。

**「套用成功才推進版本號」是整個機制的核心不變量。** 套用失敗的話 `configVersion` 維持舊值，下一次心跳 mgmtd 又會把同一份設定推下來，形成無限重試。好處是不會漏掉設定；壞處是一份壞設定會讓節點每個心跳週期都重試並失敗一次，只能靠日誌與 `ConfigStatus` 看出來。

各服務註冊 listener 的位置：

| 服務 | 位置 |
|---|---|
| meta | `src/meta/service/MetaServer.cc:39` |
| storage | `src/storage/service/Components.cc:85` |
| fuse | `src/fuse/FuseClients.cc:122` |
| simple_example | `src/simple_example/service/Server.cc:36` |
| mgmtd（對自己） | `src/mgmtd/service/helpers.cc:23` `updateSelfConfig` |

全部都是同一個函式指標：`ApplicationBase::updateConfig`。

#### 6.6.4 `ConfigStatus`：把「我到底跑著什麼設定」上報回去

```cpp
// src/common/app/ConfigStatus.h:6
enum class ConfigStatus : uint8_t { NORMAL = 0, DIRTY = 1, FAILED = 2, UNKNOWN = 3, STALE = 4 };
```

`DIRTY` 的判定是整套設定系統裡最巧的一段（`src/common/app/ConfigManager.cc:22-65`）：

```cpp
auto expected = cfg.defaultPtr();                                       // 全新的預設設定
expected->atomicallyUpdate(renderConfig(lastConfigContent), false);     // 疊上「最後一次成功套用的完整內容」
config::IConfig::ItemDiff diffs[10];
auto diffCnt = cfg.diffWith(*expected, std::span(diffs));               // 跟現況逐項比對
configStatus = diffCnt ? ConfigStatus::DIRTY : ConfigStatus::NORMAL;
```

也就是「**把 mgmtd 認為我應該跑的那份設定重新算一遍，跟我實際在跑的比對**」。`diffWith`（`ConfigBase.h:817`）遞迴走 `items_` 與 `sections_`，把最多 10 筆差異記下來並 WARN 到日誌（`ConfigManager.cc:51-58`），格式是 `key: actual X. expected Y`。

會造成 DIRTY 的典型原因正是 §6.5 提到的那兩個：啟動時的 `--config.` 命令列覆寫，以及事後用 `hot-update-config` 打的補丁。兩者都不在 `lastConfigContent` 裡。

`STALE` 則不是節點自己算的，而是 **mgmtd 端算的**（`src/mgmtd/background/MgmtdMetricsUpdater.cc:85-99`）：

```cpp
if (version == 0)                       status = ConfigStatus::UNKNOWN;   // 從沒拉過設定
else if (version != latestVersion)      status = ConfigStatus::STALE;     // 版本落後
++counts[{type, status}];
```

所以 `STALE` 是 mgmtd 在產生監控指標時，拿節點上報的 `configVersion` 跟自己的最新版本比出來的。這條路徑同時涵蓋 `routingInfo.nodeMap`（伺服器節點）與 `clientSessionMap`（client 節點），最後打成 `MgmtdService.ConfigStatus` 指標。`admin_cli list-nodes` / `list-clients` 也會把這個欄位印出來（`src/client/cli/admin/ListNodes.cc:40`）。

`lastConfigUpdateRecord`（`src/common/app/ConfigUpdateRecord.h`）記下最後一次更新的時間、`Status`、描述，可用 `admin_cli get-last-config-update-record` 撈回來——這是排查「為什麼這個節點一直是 STALE」的第一手證據。

### 6.7 `admin_cli` 的四個設定命令

四個命令走兩條完全不同的鏈路：**`set-config` 打 mgmtd（持久、全型別生效）；其餘三個打各節點的 Core service（即時、單點）**。

```
 ┌──────────────────────┐        ┌──────────────────────────────────────┐
 │ set-config           │───────▶│ Mgmtd (svc 217) setConfig            │
 │ -t TYPE -f file      │        │  → 分配 ConfigVersion、寫 FDB        │
 └──────────────────────┘        │  → 之後由心跳自動下發給該型別所有節點 │
                                 └──────────────────────────────────────┘

 ┌──────────────────────┐        ┌──────────────────────────────────────┐
 │ render-config        │        │ Core (svc 10001) 的四個方法：         │
 │ verify-config        │───────▶│  2 getConfig                          │
 │ hot-update-config    │        │  3 renderConfig                       │
 │ (get-config)         │        │  4 hotUpdateConfig                    │
 └──────────────────────┘        │  5 getLastConfigUpdateRecord          │
                                 └──────────────────────────────────────┘
     目標定址三選一：-n <nodeId> / -c <clientId> / -a <addr>
     nodeId → refreshRoutingInfo → node.extractAddresses("Core")
     clientId → getClientSession → extractAddresses(session.serviceGroups, "Core")
```

**`render-config`**（`src/client/cli/admin/RenderConfig.cc`）。送出 `RenderConfigReq{configTemplate, testUpdate, isHotUpdate}`（`src/fbs/core/service/Rpc.h:23`），節點端跑 `ApplicationBase::renderConfig`（`src/common/app/ApplicationBase.cc:135`）：

```cpp
auto renderRes = hf3fs::renderConfig(configContent, globalApp->info());   // 展開模板
if (!testUpdate) return {*renderRes, ""};                                 // 只要展開結果
auto newCfg = cfg->clonePtr();                                            // 否則 clone 一份
auto updateRes = newCfg->atomicallyUpdate(*renderRes, isHotUpdate);       // 試套用
return {*renderRes, updateRes ? newCfg->toString() : Error};              // 回傳套用後的完整設定
```

三個回傳欄位 `configAfterRender` / `updateStatus` / `configAfterUpdate` 對應這三段。`--mock` 模式則完全在本機跑，用假造的 `AppInfo`（可指定 nodeId、hostname、pid、clusterId、tags、releaseVersion、envs）驗證模板展開，**不需要連上任何節點**——這是撰寫設定模板時的主要工具。

**`verify-config`**（`src/client/cli/admin/VerifyConfig.cc`）。對某個 `NodeType` 的**所有活著的節點**並行做三件事：`getConfig` 拿當前設定、`renderConfig(testUpdate=true, hotUpdate=false)` 試冷更新、`renderConfig(testUpdate=true, hotUpdate=true)` 試熱更新。然後把「套用後的設定」與「當前設定」都正規化成 pretty TOML 再比字串，輸出 `Unchanged` / `Changed` / 失敗原因（`VerifyConfig.cc:57-72`）。

**這個「冷/熱各試一次」的設計正是為了回答部署前最重要的兩個問題**：這份設定合法嗎（冷）？能不能不重啟就生效（熱）？如果冷 OK、熱失敗，就代表這次變更碰到了 `CONFIG_ITEM`，必須滾動重啟。

**`hot-update-config`**（`src/client/cli/admin/HotUpdateConfig.cc`）。可以送片段（`-s 'a.b = 1'`）或整份檔案（`-f`），**CLI 端先自己 `toml::parse` 一次擋掉語法錯**（`HotUpdateConfig.cc:57-66`）再送。目標可以是 `-t <TYPE>`（該型別所有活著的節點）、`-n`、`-c`、`-a`。節點端走 `ApplicationBase::hotUpdateConfig` → `ConfigManager::hotUpdateConfig` → `cfg.atomicallyUpdate(str)`（預設 `isHotUpdate=true`）。

成功之後會呼叫 `updateConfigStatus`（`ConfigManager.cc:110`）重新算一次 DIRTY——所以**每一次 hot-update 幾乎必然把節點打成 `ConfigStatus::DIRTY`**，因為打的補丁不在 mgmtd 那份 `lastConfigContent` 裡。這是刻意的：DIRTY 就是「這個節點被手動動過手腳」的標記，提醒運維把變更補回 mgmtd。

**`get-config` / `get-last-config-update-record`** 是查詢類：前者走 `ApplicationBase::getConfigString(configKey)`（`ApplicationBase.cc:253`），支援用點號路徑取子樹（`ConfigBase::toToml(key)`，`ConfigBase.h:671` 遞迴切 `.`）；後者取回最後一次更新的時間與結果。

### 6.8 邊角與已知風險

1. **`renderConfig` 在開源版是 identity**（`src/common/utils/RenderConfig.cc:71-75`）。整套 `render-config` / `--mock-*` / `AppInfo` 傳參的基礎設施都在，但實際的模板展開邏輯不在這棵樹裡。`parseReleaseVersion`（同檔 `:8`）倒是完整的，支援四種版本字串格式。
2. **兩套 TOML 函式庫。** 設定系統用 vendored 的 toml++（`src/common/utils/Toml.hpp`，由 `ConfigBase.h:26` include），serde 用 toml11（由 `Serde.cc:12` include）。兩者都佔用 `namespace toml`，靠「沒有任何 TU 同時 include 兩者」共存，沒有編譯期保護。詳見 §1.2。
3. **64 個熱更新項的分佈失衡。** 302 個 `CONFIG_HOT_UPDATED_ITEM` 對 206 個 `CONFIG_ITEM`，但**執行緒數、佇列容量、緩衝區大小這類真正需要調的東西幾乎都是冷的**（`max_processing_requests_num`、`max_coroutines_num`、`num_io_threads`…）。這不是疏忽——把它們做成熱更新需要對應的執行期重配置邏輯，`DynamicCoroutinesPool` 是唯一做到的例子。
4. **設定項一旦刪除就會讓舊 TOML 整份失效**（`kConfigRedundantKey`）。安全的下線流程是：先把設定項改成無效果的保留項，等所有節點的設定都不再提到它，才真的刪掉。程式碼裡沒有 deprecated 標記機制。
5. **`TLSStore::value()` 回傳的參考不能跨呼叫保存**（§6.3.2）。沒有任何靜態檢查，只能靠 review。
6. **`CONFIG_OBJ_ARRAY` 對成員指標做整數算術**（`ConfigBase.h:84`），只有一個 `static_assert(sizeof(base) == sizeof(uintptr_t))` 守著。只用在 `Server::groups` 與 `Monitor::reporters` 兩處，上限都是 4。

---

## 7. `src/common/utils/`（118 檔）

`utils/` 是全專案最大的單一目錄，也是唯一一個「所有其他目錄都 include 它、它不 include 任何其他目錄」的葉節點——例外只有兩個：`ReentrantLockManager.cc:10` 反過來用了 `monitor::LatencyRecorder`，`ExecutorStatsReporter.h:3` 用了 `monitor::Recorder`。也就是說 utils 對 monitor 有一條回頭邊。

118 個檔案裡有 **9 個是原封不動 vendored 進來的第三方單標頭**（`RobinHood.h` 2484 行、`UnorderedDense.h` 1470 行、`MagicEnum.hpp` 1817 行、`Nameof.hpp` 1287 行、`Toml.hpp` 17402 行、`Utf8.h` 1599 行、`RapidCsv.h` 1541 行、`ArgParse.h` 1606 行、`Linenoise.c`/`.h`），合計約 3 萬行，佔 utils 總行數的 78%。真正 3FS 自己寫的程式碼只有約 8000 行。

### 7.1 分類總覽

逐檔的一句話職責見 §12.8；這裡只給分類、規模與各類的代表檔案。

| # | 類別 | 檔數 | 代表檔案 | 一句話 |
|---:|---|---:|---|---|
| ① | 錯誤處理與狀態碼 | 7 | `Status.h` `StatusCodeDetails.h` `Result.h` | 8 byte 的 `Status` + X-macro 狀態碼表 + 16 個控制流巨集（§7.2） |
| ② | 協程與執行器 | 14 | `CoroutinesPool.h` `BackgroundRunner.cc` `CPUExecutorGroup.cc` | 全專案背景工作的執行模型（§7.4） |
| ③ | 佇列、鎖與併發原語 | 14 | `BoundedQueue.h` `Shards.h` `ReentrantLockManager.cc` | 有界佇列、分片鎖、可調容量信號量 |
| ④ | 時間與大小 | 6 | `Duration.h` `UtcTime.h` `Size.h` | wire 上是整數、設定檔裡是 `"10MB"`／`"1h30min"`（§7.6） |
| ⑤ | 字串、編碼與雜湊 | 14 | `StringUtils.cc` `coding.h` `RobinHood.h` | 保序 key 編碼、varint、雜湊表（§7.5） |
| ⑥ | 型別工具與編譯期魔法 | 10 | `Reflection.h` `Thief.h` `StrongType.h` | serde 與 ConfigBase 的共同底座 |
| ⑦ | 系統、行程與檔案 | 16 | `SysResource.cc` `SysvShm.cc` `Linenoise.c` | hostname/pid/共享記憶體/fd RAII |
| ⑧ | 設定系統 | 6 | `ConfigBase.h` `Toml.hpp` `RenderConfig.cc` | 見同僚撰寫的 §6 |
| ⑨ | 快取、池、追蹤與注錯 | 25 | `ObjectPool.h` `FaultInjection.h` `Tracing.h` | 兩層物件池、請求級注錯、埋點 |
| ⑩ | 其他（`Address.h`、`DownwardBytes.h` 等跨層檔） | 6 | `Address.h` `DownwardBytes.h` | 命名空間屬於 net/serde 但檔案放在 utils |

兩個分類上的怪癖值得先點出來：

- **`utils/Address.h` 的命名空間是 `hf3fs::net` 而不是 `hf3fs`**（`utils/Address.h:14`）。它被放進 utils 是因為 `AppInfo` 要用它，而 `app/` 不該依賴 `net/`。同理 `utils/UtcTimeSerde.h` 的命名空間是 `hf3fs::serde`——把特化放在 utils 側，避免 `utils → serde` 的依賴邊。
- **`utils/SerDeser.h` 與 `serde/` 毫無關係**。前者是手寫的輕量二進位編碼器（`Serializer`/`Deserializer` 兩個結構，`SerDeser.h:25`/`:79`），供 KV key/value 編碼使用；後者是反射式序列化框架。名字相近但用途完全不同。

### 7.2 深入①：`Status` 把狀態碼藏在指標的高 16 位

`Status` 是全專案錯誤的唯一載體，被 `Result<T>` 包起來後，幾乎每個函式的回傳型別裡都有它。所以它的大小直接影響全域效能——3FS 的答案是 **8 bytes，一個指標**。

```cpp
// src/common/utils/Status.h:144
StatusPtr data_;  // |<-- low 48 bits: rep ptr -->|<-- high 16 bits: status code -->|
```

```cpp
// src/common/utils/Status.h:113
static constexpr auto kPtrBits = 48u;
static constexpr auto kPtrMask = ((1ul << kPtrBits) - 1);

// src/common/utils/Status.h:125
static StatusPtr construct(status_code_t code, std::unique_ptr<StatusRep> rep) {
  return StatusPtr(
      reinterpret_cast<StatusRep *>(reinterpret_cast<uintptr_t>(rep.release()) | (uintptr_t(code) << kPtrBits)));
}
static StatusRep *extractPtr(StatusRep *rep) {
  return reinterpret_cast<StatusRep *>(reinterpret_cast<uintptr_t>(rep) & kPtrMask);
}
```

`status_code_t` 是 `uint16_t`（`src/common/utils/StatusCode.h:8`），恰好塞進 x86-64 使用者空間指標未用的高 16 位。`StatusRep`（message + `std::any` payload）只有在真的需要訊息時才配置；`Status(StatusCode::kOK)` 的 `data_` 就是 **nullptr**，零配置。

這個技巧的前提被三道 `static_assert` 鎖死：

```cpp
// src/common/utils/Status.h:15
#if !FOLLY_X64 && !FOLLY_AARCH64
#error "The platform must be 64bit!"
#endif
static_assert(std::endian::native == std::endian::little);      // :18
static_assert(StatusCode::kOK == 0, "StatusCode::kOK must be 0!");        // :110
static_assert(sizeof(status_code_t) == 2, "The width of status_code_t must be 16b");  // :111
```

代價有兩處值得留意：

1. **`StatusRepDeleter` 必須先洗掉高位再 delete**（`Status.h:120`），否則就是把一個帶垃圾高位的指標交給 `operator delete`。
2. **`Status` 的複製是深拷貝**（`Status.h:59`，每次複製都 `make_unique<StatusRep>(*other.rep())`），而移動是 `= default`。所以在錯誤路徑上一律 `std::move`——`Result.h` 的巨集全部照做（`RETURN_ERROR` 是 `makeError(std::move(result.error()))`，`Result.h:9`）。
3. 在 aarch64 上，若核心開了 52-bit VA（`CONFIG_ARM64_VA_BITS_52`），48 位遮罩就會截掉合法位址。`#if !FOLLY_X64 && !FOLLY_AARCH64` 放行了 aarch64 但沒有對 VA 位寬做任何檢查。

#### 7.2.1 狀態碼空間：一份 X-macro 資料檔展開三次

`StatusCodeDetails.h` 是純資料檔，靠呼叫端定義 `RAW_STATUS` / `STATUS` 來決定展開成什麼：

```cpp
// src/common/utils/StatusCode.h:10
#define RAW_STATUS(name, value)                   \
  namespace StatusCode {                          \
  inline constexpr status_code_t k##name = value; \
  }
#define STATUS(ns, name, value)                     \
  namespace ns##Code {                              \
    inline constexpr status_code_t k##name = value; \
  }
#include "StatusCodeDetails.h"
```

同一個檔案在 `StatusCode.cc:11`、`:26` 又被 include 兩次，分別展開成 `toString()` 與 `typeOf()` 的 `switch`。新增一個錯誤碼只要改一行資料檔，三處自動同步。

碼段分配（實測全表）：

| 區段 | 命名空間 | 數量 | 範圍 |
|---:|---|---:|---|
| 0–999 | `StatusCode::`（全域） | 52 | 0 ~ 999 |
| 1000–1999 | `TransactionCode::` | 11 | 1000 ~ 1010 |
| 2000–2999 | `RPCCode::` | 27 | 2000 ~ 2027 |
| 3000–3999 | `MetaCode::` | 27 | 3000 ~ 3999 |
| 4000–4999 | `StorageCode::` | 41 | 4000 ~ 4091 |
| 5000–5999 | `MgmtdCode::` | 19 | 5000 ~ 5018 |
| 6000–6999 | `MgmtdClientCode::` | 5 | 6000 ~ 6004 |
| 7000–7999 | `StorageClientCode::` | 23 | 7000 ~ 7999 |
| 8000–8999 | `ClientAgentCode::` | 6 | 8000 ~ 8006 |
| 10000 | `CliCode::` | 1 | 10000 |
| 11000–11999 | `KvServiceCode::` | 24 | 11000 ~ 11023 |

> **`KvServiceCode` 的 24 個碼在整個 OSS 樹裡沒有任何一處使用**（`grep -rn KvServiceCode src/` 除了 `StatusCode.h`/`.cc` 的 X-macro 展開外零命中）。這是內部 KV 服務被剝離後留下的化石，與 §9.5 的 `KVTB`/`KVNS`/`KVWG` key 前綴互相印證。

`toErrno()`（`StatusCode.cc:39`）是 FUSE 層的出口：整段 `RPCCode::*` 一律映射成 `EREMOTEIO`，`MetaCode` 逐條對應 POSIX errno，其餘一律 `EIO`。注意 `kRequestCanceled` 被刻意映射成 `EINTR` 而非 `ECANCELED`（`StatusCode.cc:62` 有註解說明）。

#### 7.2.2 `Result.h` 的 16 個巨集

`Result<T>` 本身只是別名（`Result.h:118`），真正的份量在 16 個控制流巨集。它們成對存在——同步版 `RETURN_*` 與協程版 `CO_RETURN_*`：

| 巨集 | 語意 |
|---|---|
| `RETURN_ON_ERROR(r)` / `CO_RETURN_ON_ERROR(r)` | 有錯就原樣往上拋 |
| `RETURN_AND_LOG_ON_ERROR` / `CO_RETURN_AND_LOG_ON_ERROR` | 拋之前先 `XLOGF(ERR)` |
| `RETURN_ON_ERROR_WRAP(r, code, fmt, ...)` | 換一個新錯誤碼，把原錯誤附在訊息尾巴 |
| `RETURN_ON_ERROR_MSG_WRAP(r, fmt, ...)` | 保留原錯誤碼，只加訊息 |
| `CHECK_RESULT(name, expr)` | 有錯就 return，沒錯就 `auto &name = *expr` |
| `RUNTIME_ASSERT_RESULT(r, fmt, ...)` | 有錯直接 `XLOGF(FATAL)` |

`CO_RETURN_ERROR` 有一個同步版沒有的行為（`Result.h:79`）：**若原錯誤沒有訊息，它會自動補上 `__FILE__:__LINE__` 與運算式字面量**。

```cpp
// src/common/utils/Result.h:82
if (result.error().message().empty())
  co_return hf3fs::makeError(result.error().code(),
                             fmt::format("{}:{}, '{}' has error", __FILE__, __LINE__, #result));
```

所以同一個錯誤碼，走協程路徑時帶著發生點、走同步路徑時是裸碼。這在追查跨層錯誤時是差異很大的體驗，也解釋了為什麼 storage/meta 的日誌裡常見 `xxx.cc:123, 'co_await foo()' has error`。

---

### 7.3 深入②：`Shuffle.h`——把 libstdc++ 的實作抄進來，只為了「換編譯器不會壞資料」

這是整個 utils 目錄裡最不像工具函式的東西。表面上它是「洗牌」，實際上它是**資料佈局的一部分**。

原因在 `src/fbs/meta/Schema.cc:171`：檔案的 chain list 不是存在 inode 裡的，而是存一個 `baseIndex + seed`，讀取時**當場重跑一次洗牌**還原：

```cpp
// src/fbs/meta/Schema.cc:159
const auto &list = chains.try_emplace_with([&]() -> std::vector<uint32_t> {
  std::vector<uint32_t> chains(stripe);
  for (uint32_t i = 0; i < stripe; i++) chains[i] = baseIndex + i;
  switch (shuffle) {
    case NO_SHUFFLE: break;
    case STD_SHUFFLE_MT19937: { hf3fs_shuffle(chains, seed); break; }   // :171
  }
  return chains;
});
```

也就是說：**任何讀這個 inode 的行程，都必須算出位元級完全相同的洗牌結果，否則就會讀到別人的 chunk。** 而 `std::shuffle` 的輸出**依 libstdc++ 版本而異**——g++11 把 `uniform_int_distribution` 的降尺度演算法從「拒絕取樣 + 除法」換成了 Lemire 的 nearly-divisionless。

3FS 的處理方式是把三個版本的實作全部抄進 repo：

```cpp
// src/common/utils/Shuffle.h:45   Lemire nearly-divisionless（g++11 之後）
inline uint64_t fast_range(std::mt19937_64 &urng, uint64_t range) {
  uint128_t product = uint128_t(urng()) * uint128_t(range);
  uint64_t low = uint64_t(product);
  if (low < range) {
    uint64_t threshold = -range % range;
    while (low < threshold) { product = uint128_t(urng()) * uint128_t(range); low = uint64_t(product); }
  }
  return product >> 64;
}
```

g++10 的舊做法（`Shuffle.h:81`）則是「拒絕取樣 + 除法」：`scaling = urngrange / uerange`，重抽到 `ret < uerange * scaling` 為止再 `ret /= scaling`。兩者對同一個種子產生的序列**不同**。

`gcc_shuffle<T, GCC11>`（`Shuffle.h:106`）連 libstdc++ 的「成對交換」優化都照抄了（`Shuffle.h:123-135`：偶數長度先單獨換第一個，之後用一次分布呼叫產生兩個交換位置），因為那個優化改變了亂數消耗順序，不照抄結果就不同。

選哪一版由 CMake 決定，而且**強制三選一**：

```cmake
# CMakeLists.txt:48
if(SHUFFLE_METHOD STREQUAL "stdshuffle")      add_compile_definitions(USE_STD_SHUFFLE)
elseif(SHUFFLE_METHOD STREQUAL "g++10")       add_compile_definitions(USE_GCC10_SHUFFLE)
elseif(SHUFFLE_METHOD STREQUAL "g++11")       add_compile_definitions(USE_GCC11_SHUFFLE)
```

```cpp
// src/common/utils/Shuffle.h:22
#if (defined(USE_STD_SHUFFLE) + defined(USE_GCC10_SHUFFLE) + defined(USE_GCC11_SHUFFLE)) > 1
#error "multiple shuffle method defined"
#endif
// :32  一個都沒定義也是 #error
```

而且選定的方法會被寫進二進位檔：

```cpp
// src/common/utils/Shuffle.h:27
__attribute__((used)) inline constexpr char HF3FS_SHUFFLE_METHOD[] = "HF3FS_SHUFFLE_METHOD=STD_SHUFFLE";
```

`__attribute__((used))` 阻止連結器丟棄，所以可以直接 `strings` 一個 3FS 二進位檔看它用哪套洗牌——這是給運維排查「不同節點讀到不同 chain」用的。

#### 7.3.1 `find_safe_seed`：只挑三套實作都一致的種子

最後一層保險是配置端。分配 layout 時不是隨便挑一個種子，而是**挑一個讓三套實作輸出完全相同的種子**：

```cpp
// src/common/utils/Shuffle.h:174
inline bool safe_shuffle_seed(uint32_t vec_len, uint64_t mt19937_64_seed) {
  std::vector<uint32_t> vec1(vec_len); std::iota(...);
  std::vector<uint32_t> vec2 = vec1, vec3 = vec1;
  std_shuffle(vec1, seed);                    // 本機 libstdc++
  gcc_shuffle<uint32_t, true>(vec2, seed);    // g++11 演算法
  gcc_shuffle<uint32_t, false>(vec3, seed);   // g++10 演算法
  ...
  return vec1 == vec2 && vec1 == vec3;        // :193  三者全等才算安全
}

// :196
inline std::optional<uint64_t> find_safe_seed(uint32_t vec_len) {
  for (size_t i = 0; i < 1000; i++) {
    auto seed = folly::Random::rand64();
    if (safe_shuffle_seed(vec_len, seed)) return seed;
  }
  XLOGF(DFATAL, "can't find safe shuffle seed for vec size {}", vec_len);
  return std::nullopt;
}
```

呼叫點在 `src/fbs/meta/Schema.cc:122`（`Layout::newChainRange`）與 `src/meta/components/ChainAllocator.h:115`。找不到安全種子時**退化成 `NO_SHUFFLE`**（`Schema.cc:123`：`seed ? STD_SHUFFLE_MT19937 : NO_SHUFFLE`），也就是寧可不打散、也不冒版本不一致的風險。

`safe_shuffle_seed` 對長度較大的向量成功率會下降，測試裡 `tests/common/utils/TestShuffle.cc:56` 甚至有一個 `ASSERT_FALSE(safe_shuffle_seed(200, seed))`——長度 200 時特定種子必然不安全。這也是為什麼 `find_safe_seed` 要迴圈 1000 次。

**設計代價**：每次配置一個檔案的 layout 都要做最多 1000 次「三份洗牌 + 兩次比較」。stripe size 通常是 16 或 32，所以單次很便宜；但這是 metadata 建檔路徑上的同步 CPU 開銷，而且無快取（每個檔案重新找）。

---

### 7.4 深入③：併發骨架三件套

三個檔案構成了 3FS 所有背景工作的執行模型。

```
       CPUExecutorGroup                     BackgroundRunner
       （執行緒 + 佇列策略）                （週期性任務的啟停）
              │                                    │
              │  pickNext() / pickNextFree()       │  scheduleOn(getExecutor())
              ▼                                    ▼
   ┌──────────────────────┐             for (round = 1;;++round) {
   │ CPUThreadPoolExecutor│                 co_await taskGetter();
   │ CPUThreadPoolExecutor│                 sleep(interval - elapsed);
   │        ...           │             }
   └──────────────────────┘
              ▲
              │  start(handler, executorGroup)
       CoroutinesPool<Job>
       （N 條協程消費 BoundedQueue）
```

#### 7.4.1 `CPUExecutorGroup`：六種排隊策略

```cpp
// src/common/utils/CPUExecutorGroup.h:11
enum class ExecutorStrategy {
  SHARED_QUEUE,     // fallback to CPUThreadPoolExecutor
  SHARED_NOTHING,
  WORK_STEALING,
  ROUND_ROBIN,
  GROUP_WAITING_4,
  GROUP_WAITING_8,
};
```

實作分兩路（`CPUExecutorGroup.cc:57`）：

- `SHARED_QUEUE`：**一個** `CPUThreadPoolExecutor` 帶 N 條執行緒，folly 原生行為。
- `SHARED_NOTHING` / `WORK_STEALING` / `ROUND_ROBIN`：**N 個各帶 1 條執行緒**的 executor，差別只在塞給它們的佇列模板（`CPUExecutorGroup.cc:19` 的 `createExecutors<QueueTemplate>`）。三個佇列共用同一個 `SharedState`，所以 work stealing 是在佇列層做的，不是在 executor 層。
- `GROUP_WAITING_4` / `_8`：N/4 或 N/8 個 executor，每個帶 4 或 8 條執行緒，佇列是 `folly::UnboundedBlockingQueue<Task, SimpleSemaphore>`。**執行緒數不是 4/8 的倍數會直接丟 `StatusException`**（`CPUExecutorGroup.cc:42`）。

三個挑選策略：

```cpp
// src/common/utils/CPUExecutorGroup.h:38
folly::CPUThreadPoolExecutor &pickNext() const { return get(next_.fetch_add(1, acq_rel) % size()); }

// src/common/utils/CPUExecutorGroup.cc:91
folly::CPUThreadPoolExecutor &pickNextFree() const {
  auto start = next_.fetch_add(1, std::memory_order_acq_rel);
  auto probeSize = std::min(size(), 4UL);        // 只探測 4 個
  ... 取 getTaskQueueSize() 最小者
}
```

`pickNextFree` 是「power of d choices」的 d=4 版本：從 round-robin 起點連續探 4 個，挑佇列最短的。探測數寫死 4，沒有做成設定項。

#### 7.4.2 `CoroutinesPool`：work stealing 靠 5ms 超時輪詢實作

```cpp
// src/common/utils/CoroutinesPool.h:37
if (config_.enable_work_stealing()) {
  auto queueSizePerCoroutine = (config_.queue_size() + coroutinesNum - 1) / coroutinesNum;
  for (auto i = 0u; i < coroutinesNum; ++i) funcQueues_.push_back(std::make_unique<Queue>(queueSizePerCoroutine));
} else {
  funcQueues_.push_back(std::make_unique<Queue>(config_.queue_size()));   // 只有一條
}
```

關掉 work stealing 時只有**一條**共用佇列；打開時每條協程一條佇列，總容量不變（切成 N 份）。

竊取邏輯不是無鎖窺探，而是「等自己的佇列 5ms，等不到就 try 一個別人的」：

```cpp
// src/common/utils/CoroutinesPool.h:134
CoTask<std::optional<Job>> timedWait(Queue &queue) {
  static constexpr auto timeout = std::chrono::milliseconds(5);
  auto [sleepResult, dequeueResult] =
      co_await folly::coro::collectAnyNoDiscard(folly::coro::sleep(timeout), queue.co_dequeue());
  if (!dequeueResult.hasValue() && !dequeueResult.hasException()) co_return std::nullopt;
  co_return std::move(*dequeueResult);
}

// :121
while (true) {
  if (auto maybeJob = co_await timedWait(*queues[self]); maybeJob) co_return std::move(*maybeJob);
  auto otherIndex = others[nextIndex++ % others.size()];       // :126  輪詢別人
  if (auto maybeJob = queues[otherIndex]->try_dequeue(); maybeJob) co_return std::move(*maybeJob);
}
```

竊取對象順序在建構時被打散一次（`CoroutinesPool.h:112` 的 `std::shuffle`），之後就是固定輪詢。**空閒時每條協程每 5ms 醒一次**——64 條協程就是每秒 12800 次喚醒。這是 work stealing 預設 `false`（`CoroutinesPool.h:19`）的原因。

停止用取消權杖，且 `stopAndJoin` 用 `atomic_flag` 做冪等（`CoroutinesPool.h:72`），因為解構子也會呼叫它。

#### 7.4.3 `BackgroundRunner`：把「週期」而不是「間隔」當作契約

```cpp
// src/common/utils/BackgroundRunner.cc:84
for (int64_t i = 1;; ++i) {
  auto start = SteadyClock::now();
  auto res = co_await co_awaitTry(taskGetter());
  HANDLE_CO_TRY_EXCEPTION();
  if (!intervalGetter) break;                          // startOnce 走這條
  auto interval = intervalGetter().asUs();
  if (interval == 0_us) break;                          // 間隔設為 0 = 停掉這個任務
  auto now = SteadyClock::now();
  if (now < start + interval) {
    co_await co_awaitTry(folly::coro::sleep(std::chrono::duration_cast<std::chrono::microseconds>(start + interval - now)));
  }
}
```

三個細節：

1. **睡眠時間從 `start` 算起，不是從任務結束算起**，所以是固定週期而非固定間隔；任務跑超過一個週期就退化成連續執行（不會補跑）。
2. `intervalGetter` 是 **`std::function<Duration()>` 而不是 `Duration`**——每一輪重新取值，所以熱更新設定裡的週期會在下一輪自動生效，不需要重啟任務。回傳 0 就是關掉這個任務。
3. **非取消的例外一律 `XLOGF(FATAL)`**（`BackgroundRunner.cc:80`），也就是背景任務拋例外 = 行程自殺。這是刻意的：背景任務通常是心跳、GC、同步，靜默死掉比 crash 更難查。

```cpp
// src/common/utils/BackgroundRunner.cc:107
// release all captured resources before stopAll finished
taskGetter = TaskGetter{};
intervalGetter = IntervalGetter{};
...
latch_.countDown();      // :113
```

清空兩個 `std::function` **在 `countDown()` 之前**，確保 `stopAll()` 返回時 lambda 捕獲的物件（通常是 `this`）已經被放掉——否則 `stopAll(); delete this;` 會踩到懸空。

`executor_` 是 `std::variant<folly::Executor::KeepAlive<>, CPUExecutorGroup *>`（`BackgroundRunner.h:30`），用 `std::visit` 分派（`.cc:53`）；解構時把它重設成 `(CPUExecutorGroup *)nullptr`（`.cc:49`）而不是 `KeepAlive{}`，避免 variant 還握著 executor 的引用計數。

---

### 7.5 深入④：保序字串編碼——讓 FDB 的 byte 序等於邏輯序

`meta` 與 `mgmtd` 的 FDB key 常常是「多個變長字串串接」（例如 `DENT` + parentInodeId + name）。直接串接會出問題：`("a", "bc")` 與 `("ab", "c")` 編出來一模一樣。加長度前綴可以解決唯一性，但**會破壞字典序**——而 FDB 的 range scan 完全依賴字典序。

`encodeOrderPreservedString` 的解法是 **1 byte 前導 + `\0` 終止 + `\0` 轉義**：

```cpp
// src/common/utils/StringUtils.cc:26
void encodeOrderPreservedString(String &buf, std::string_view key, bool mayContainNull) {
  if (key.empty()) { buf.push_back(0); return; }     // 空字串 = 單一個 0x00
  buf.push_back(1);                                   // 非空字串前導 0x01
  if (!mayContainNull || key.find('\0') == std::string_view::npos) {
    buf.append(key.data(), key.size());
  } else {
    for (char c : key) {
      if (c == 0) { buf.push_back(0); buf.push_back(0xff); }   // 0x00 → 0x00 0xFF
      else        { buf.push_back(c); }
    }
  }
  buf.push_back(0);                                   // 終止符 0x00
}
```

為什麼這樣就保序：

- 空字串編成 `00`，非空編成 `01 ...`。`0x00 < 0x01`，所以**空字串永遠排在所有非空字串之前**，符合字典序。
- 終止符 `0x00` 比任何非零位元組都小，所以 `"ab"` 編成 `01 'a' 'b' 00`、`"abc"` 編成 `01 'a' 'b' 'c' 00`；比較到第 4 個位元組時 `00 < 'c'`，`"ab" < "abc"` 成立。
- 內含 `\0` 的字串要轉義成 `00 FF`。因為 `0xFF` 比終止符後可能出現的任何東西都大，被轉義的 `\0` 仍然排在真正的終止符之後，順序不變。

解碼端（`StringUtils.cc:47`）靠「`00` 後面若是 `FF` 就是轉義、否則就是終止」來區分，並回傳**剩餘的 `string_view`**，於是多欄位可以直接串下去解：

```cpp
// src/common/utils/StringUtils.cc:91
Result<std::vector<String>> decodeOrderPreservedStrings(std::string_view key, bool mayContainNull) {
  std::string_view remaining = key;
  for (;;) {
    String k;
    auto res = decodeOrderPreservedString(remaining, k, mayContainNull);
    RETURN_ON_ERROR(res);
    remaining = *res;
    result.push_back(std::move(k));
    if (remaining.empty()) break;
  }
```

`mayContainNull` 是效能開關：明知不含 `\0`（例如檔名）就走 `append` 一次搬完，含 `\0`（例如二進位 key）才逐位元組轉義。**但這個旗標必須寫入端與讀取端一致**，沒有任何自描述位元——寫入時傳 `true` 而讀取時傳 `false`，含 `\0` 的字串會被截斷成兩段且不報錯。

配套的 `getPrefixEnd`（`StringUtils.cc:4`）是 range scan 的另一半：把前綴的最後一個非 `\xff` 位元組加一，取得 exclusive 上界；全 `\xff` 就回傳空字串代表「到底」。`kv/ITransaction.cc:42` 的 `prefixListEndKey` 是同一個演算法的另一份實作（差別：一個把 `\xff` 改成 `\0` 保留長度，一個直接 `pop_back` 縮短）。**兩份實作對「全 `\xff` 前綴」的回傳一致（空字串），但對中間有 `\xff` 的前綴會產生不同但等價的上界**。

---

### 7.6 深入⑤：三個「知道怎麼把自己寫成人話」的純量型別

`Size`、`Duration`、`UtcTime` 有一個共同的形狀，這個形狀是設定系統與 serde 的雙重契約：

```cpp
// src/common/utils/Size.h:36
using is_serde_copyable = void;                                  // ① 二進位走原生 memcpy
std::string serdeToReadable() const { return toString(); }       // ② JSON/TOML 輸出用人話
static Result<Size> serdeFromReadable(std::string_view input) { return from(input); }  // ③ 讀回來
```

三者的組合效果是：**同一個欄位在 wire 上是 8 bytes 原生整數，在設定檔與 admin_cli 輸出裡是 `"10MB"` / `"5min"` / `"2026-08-15 03:14:07"`**。

| 型別 | 底層 | 字面量 | 可讀形式 |
|---|---|---|---|
| `Size` | `uint64_t` | `1_B` `1_KB` `1_MB` `1_GB` `1_TB`（1024 進位）<br>`1_K` `1_M` `1_G` `1_T`（1000 進位） | `"10MB"`、`"infinity"` |
| `Duration` | `chrono::nanoseconds` 子類 | `_ns` `_us` `_ms` `_s` `_min` `_h` `_d` | `"1h30min"`（可多段串接） |
| `UtcTime` | `chrono::time_point<UtcClock>`（微秒） | — | `"%F %T"` |

`Size` 同時提供 1024 與 1000 兩套字面量與兩套 label（`Size.h:14-34`），因為儲存容量習慣用 1000 進位（磁碟廠商）、記憶體/緩衝區習慣用 1024 進位。`Size::around()`（`Size.h:46`）是給人看的近似輸出。

`Duration::from` 的解析是**累加式且要求單位遞減**：

```cpp
// src/common/utils/Duration.cc:40
std::chrono::nanoseconds lastUnit = kUnitDay * 10;  // ensure initial lastUnit larger than any valid unit
while (!remaining.empty()) {
  auto [r, i, s] = scn::scan_tuple<int64_t, std::string>(remaining, "{}{}");
  ...
```

所以 `"1h30min"` 合法而 `"30min1h"` 不合法。這條規則沒有寫在任何註解裡，只能從 `lastUnit` 的比較推出來。

`Duration` 繼承 `std::chrono::nanoseconds` 而不是持有它，好處是可以直接參與 chrono 運算；壞處是它同時提供了 `operator std::chrono::microseconds()` 與 `operator std::chrono::milliseconds()` 兩個隱式轉換（`Duration.h:32-33`），在多載決議時偶爾會有歧義——這是為什麼程式碼裡到處看到顯式的 `.asUs()`。

`UtcClock` **只保證微秒粒度**（`UtcTime.h:18` 的 `period = std::ratio<1, 1000000>`），這直接決定了 RPC `Timestamp` 8 個欄位的精度（見 §3.2.2）。`UtcTime::castGranularity(Duration)`（`UtcTime.h:47`）把時間戳向下取整到指定粒度，monitor 用它做時間桶對齊。

值得注意的錯位：`formatter<UtcTime>`（`UtcTime.h:69`）用的是 **`localtime`**，不是 UTC。所以一個叫 `UtcTime` 的型別，`fmt::format("{}", t)` 出來的是本地時間。`serdeToReadable()` 也是同一條路（`UtcTime.cc:20` 呼叫 `fmt::to_string(*this)`）。跨時區的叢集在比對日誌時間戳時要留意。

---

### 7.7 幾個小而巧的東西

**`ObjectPool<T, 64, 64*64>`（`utils/ObjectPool.h`）**：兩層快取，TLS 層 64 個、全域層 64×64 個。回收路徑藏在自訂 deleter 裡：

```cpp
// src/common/utils/ObjectPool.h:6
void operator()(T *item) {
  item->~T();                                                    // 手動解構
  tls().put(std::unique_ptr<Storage>(reinterpret_cast<Storage *>(item)));   // 記憶體回池
}
```

物件解構與記憶體釋放被拆開，所以 `unique_ptr<T, Deleter>` 的語意是「解構並歸還」而不是「delete」。

**`Shards<T, N>`（`utils/Shards.h`）**：34 行，`std::array<std::mutex, N>` + `std::array<T, N>`，分片鍵用 `folly::hash::hash_combine_generic(RobinHoodHasher{}, args...)`。`iterate` 是逐片加鎖，**不是全域快照**——遍歷期間其他片可以被修改。

**`AtomicSharedPtrTable<T>`（`utils/AtomicSharedPtrTable.h`）**：定長槽表，`AvailSlots` 用 `std::set<int> free` + `atomic<int> nextAvail` 混合配置（優先重用釋放的槽，沒有才推進水位）。槽內是 `folly::atomic_shared_ptr`，所以讀取不需要鎖。FUSE 的 fd 表用的就是它。

**`FaultInjection`（`utils/FaultInjection.h`）**：掛在 `folly::RequestData` 上，所以注錯範圍是「一個請求」而不是「一個執行緒」。`MemTransaction` 全部的讀寫方法開頭都有 `FAULT_INJECTION_ON_GET`（`kv/mem/MemTransaction.h:29`），隨機回 `kThrottled`/`kTooOld`；commit 則額外會注 `kMaybeCommitted` 並**有一半機率真的提交**（`MemTransaction.h:48`），用來測試「不確定是否提交」這個 FDB 特有的難題。

**`ConstructLog<"Name">`（`utils/ConstructLog.h`）**：7 行，建構印一行、解構印一行。用法是當成第一個成員（`CPUExecutorGroup.h:56`），於是它的建構最早、解構最晚，日誌自然包住整個物件生命週期。

---

## 8. `src/common/app/`（17 檔）

### 8.1 為什麼 `mgmtd` / `meta` / `storage` 的 main 完全一樣

```cpp
// src/mgmtd/mgmtd.cpp:5
int main(int argc, char *argv[]) { return TwoPhaseApplication<mgmtd::MgmtdServer>().run(argc, argv); }
// src/meta/meta.cpp:5
int main(int argc, char *argv[]) { return TwoPhaseApplication<meta::server::MetaServer>().run(argc, argv); }
// src/storage/storage.cpp:5
int main(int argc, char *argv[]) { return TwoPhaseApplication<storage::StorageServer>().run(argc, argv); }
// src/simple_example/main.cpp:5
int main(int argc, char *argv[]) { return TwoPhaseApplication<simple_example::server::SimpleExampleServer>().run(argc, argv); }
```

四個 binary 的 `main` 只差一個模板參數。`monitor_collector` 是唯一的例外，走另一條路：

```cpp
// src/monitor_collector/monitor_collector.cpp:5
int main(int argc, char *argv[]) {
  return hf3fs::OnePhaseApplication<hf3fs::monitor::MonitorCollectorServer>::instance().run(argc, argv);
}
```

`TwoPhaseApplication<Server>` 對 `Server` 的要求全部是**隱式的關聯型別**（`TwoPhaseApplication.h:16-26`）：

| 需要的成員 | 用途 |
|---|---|
| `Server::Launcher` | 第一階段：拉遠端設定與 AppInfo |
| `Server::Config` | 第二階段：伺服器本體設定 |
| `Server::CommonConfig` | log / monitor / memory 三段共用設定 |
| `Server::kName` | 日誌檔名前綴、monitor 標籤 |
| `Server::setup()` / `start(appInfo)` / `appInfo()` / `describe()` | 生命週期 |

沒有任何 base class、沒有 concept 約束（`OnePhaseApplication` 有一個 `requires`，`OnePhaseApplication.h:23`，但 `TwoPhaseApplication` 連這個都沒有）。缺一個成員就是一頁模板錯誤。

### 8.2 `ApplicationBase::run` 的五步

```cpp
// src/common/app/ApplicationBase.cc:49
int ApplicationBase::run(int argc, char *argv[]) {
  Thread::blockInterruptSignals();          // ① 先擋掉 SIGINT/SIGTERM
  auto parseFlagsRes = parseFlags(&argc, &argv);   // ② 抽走 --config.* 等前綴旗標
  folly::init(&argc, &argv);                // ③ 剩下的交給 gflags/glog
  if (FLAGS_release_version) { ... return 0; }
  auto initRes = initApplication();         // ④ 子類實作
  auto exitCode = mainLoop();               // ⑤ 阻塞等訊號
  memory::shutdown();
  stop();
  return exitCode;
}
```

第①步是關鍵：`Thread::blockInterruptSignals()`（`app/Thread.cc:16`）在**任何執行緒被建立之前**用 `pthread_sigmask(SIG_BLOCK, ...)` 擋掉 SIGINT/SIGTERM。因為 Linux 會把 SIGINT 投遞給**任意一條**已註冊 handler 的執行緒，而阻塞遮罩會被子執行緒繼承——所以之後所有 worker 執行緒天生就是遮蔽的。等到 `mainLoop()` 才把主執行緒解除遮蔽（`ApplicationBase.cc:82`），於是 Ctrl+C 必定落在主執行緒上。原始碼註解把這件事說得很清楚（`Thread.cc:7-15`）。

`mainLoop()` 本身是一個 condvar 等待（`ApplicationBase.cc:84`），handler 只做三件事：設旗標、記 exit code、`notify_one`。`SIGUSR2` 會讓退出碼變成 `128 + SIGUSR2`（`ApplicationBase.cc:43`），這是給 supervisor 區分「正常停止」與「請求重啟」用的。

`parseFlags` 的分離很值得注意——`--config.xxx=yyy` 這類旗標由 **3FS 自己**在 `folly::init` 之前抽走（`ApplicationBase.cc:92` → `config::parseFlags`），因為 gflags 不認識它們，留著會導致 `folly::init` 報未知旗標。抽走的結果變成 `ConfigFlags`（`vector<config::KeyValue>`），稍後餵給設定系統。

### 8.3 兩階段 vs 一階段

```
── TwoPhaseApplication（mgmtd / meta / storage / simple_example / migration）──

  parseFlags:  launcher->parseFlags()                       解析 --app_config.* / --launcher_config.*
               ApplicationBase::parseFlags("--config.")     解析 --config.*
                  │
  initApplication:
    ① launcher_->init()                                    載入 app/launcher 設定、啟動 IBManager
    ② app_detail::loadAppInfo(launcher_->loadAppInfo)       ← 重試 20 次，指數退避 10ms→1s，失敗 FATAL
    ③ app_detail::initConfig(config_, flags, appInfo,
                             launcher_->loadConfigTemplate) ← 同樣重試 20 次
    ④ app_detail::initCommonComponents()                    log → VersionInfo → Waiter → Monitor
    ⑤ makeLogConfigUpdateCallback / makeMemConfigUpdateCallback
    ⑥ app_detail::persistConfig(config_)
    ⑦ server_ = make_unique<Server>(config_.server()); setup()
    ⑧ launcher_->startServer(*server_, appInfo_)
    ⑨ launcher_.reset()                                    ← 用完即丟

── OnePhaseApplication（monitor_collector）──

    ① 從本地 --app_cfg / --cfg 直接讀設定（沒有遠端、沒有 launcher）
    ② IBManager::start → logging::initOrDie → Waiter → Monitor::start
    ③ server_->setup() → 手工組出 AppInfo → server_->start(info_)
```

差別的本質是**設定從哪來**。`TwoPhase` 的第一階段是「拿一個能連上 mgmtd 的最小設定」，第二階段是「從 mgmtd 拉真正的設定」。`Launcher` 本身**不在 `common/app/`**——它在 `src/core/app/ServerLauncher.h`，因為它要依賴 mgmtd client：

```cpp
// src/core/app/ServerLauncher.h:34
Result<Void> init() {
  appConfig_.init(FLAGS_app_cfg, FLAGS_dump_default_app_cfg, appConfigFlags_);
  launcherConfig_.init(FLAGS_launcher_cfg, FLAGS_dump_default_launcher_cfg, launcherConfigFlags_);
  auto ibResult = net::IBManager::start(launcherConfig_.ib_devices());   // :41
  fetcher_ = std::make_unique<RemoteConfigFetcher>(launcherConfig_);     // :45
  return Void{};
}
```

`common/app/` 負責的是**骨架與流程**，`core/app/` 負責的是**與 mgmtd 對話的那一半**。這是為什麼 `TwoPhaseApplication.h` 只寫 `using Launcher = typename Server::Launcher;`（`TwoPhaseApplication.h:16`）——它完全不知道 launcher 長什麼樣。

`configPushable()` 決定這個節點願不願意接受 mgmtd 推設定：

```cpp
// src/common/app/TwoPhaseApplication.h:86
bool configPushable() const final { return FLAGS_cfg.empty() && !FLAGS_use_local_cfg; }
```

也就是：**只要你用 `--cfg` 指定了本地設定檔，這個節點就拒絕所有熱更新**（`ApplicationBase.cc:121` 會直接回 `kCannotPushConfig`）。這是刻意的——本地設定是除錯用的，不該被叢集覆蓋。

### 8.4 `AppInfo`：兩層結構，只有內層上線

```cpp
// src/common/app/AppInfo.h:107
struct FbsAppInfo : public serde::SerdeHelper<FbsAppInfo> {
  SERDE_STRUCT_FIELD(nodeId, NodeId(0));
  SERDE_STRUCT_FIELD(hostname, String{});
  SERDE_STRUCT_FIELD(pid, uint32_t(0));
  SERDE_STRUCT_FIELD(serviceGroups, std::vector<ServiceGroupInfo>{});
  SERDE_STRUCT_FIELD(releaseVersion, ReleaseVersion{});
  SERDE_STRUCT_FIELD(podname, String{});
};

// :118
struct AppInfo : FbsAppInfo {
  String clusterId;                 // ← 不是 SERDE_STRUCT_FIELD
  std::vector<TagPair> tags;        // ← 不是 SERDE_STRUCT_FIELD
};
```

`clusterId` 與 `tags` **不會被序列化**。`app_detail::loadAppInfo` 印日誌時也是分開印的（`app/Utils.cc:248`：先 `serde::toJsonString(static_cast<const flat::FbsAppInfo &>(baseInfo))`，再單獨印 clusterId 與 tags）。原因是這兩個欄位是 launcher 本地知道的，不需要在心跳裡來回傳。

`hostname` 有兩種語意，`SysResource::hostname(bool physicalMachineName)`（`utils/SysResource.h:13`）：`true` 回實體機名、`false` 回 pod 名。`OnePhaseApplication` 兩個都取（`OnePhaseApplication.h:103`、`:106`），分別填進 `hostname` 與 `podname`。

`ReleaseVersion`（`AppInfo.h:36`）有一個很少見的相容性設計：**六個 serde 欄位是 v0 的佈局，但 v1 的語意是靠 `HelperV1` 重新解讀那六個欄位**。

```cpp
// src/common/app/AppInfo.h:84
// just for demonstration
// structVersion_   <=> rv.majorVersion
// isReleaseVersion_<=> rv.minorVersion
// <tagDate_, patchVersion_> <=> rv.buildTimeInSeconds
// pipelineId_      <=> rv.pipelineId
// commitHashShort_ <=> rv.commitHashShort
```

`majorVersion` 預設是 1（`AppInfo.h:99`），代表「這是 v1 語意」；舊節點看到的還是六個整數欄位，不會解析失敗，只是解讀錯誤。`SERDE_STRUCT_FIELD(majorVersion, uint8_t(1))` 上面的註解寫得直白：`// NOTE: other fields are just for storage when majorVersion != 0`（`AppInfo.h:98`）。

兩個特殊 tag key 定義在這裡（`AppInfo.h:129`）：`kDisabledTagKey = "Disable"`（節點被停用）與 `kTrafficZoneTagKey = "TrafficZone"`（RDMA zone 親和性）。

### 8.5 設定熱更新：全域單例 + 記錄最後一次結果

`ApplicationBase` 有一個檔案作用域的 `globalApp` 指標，在建構子裡設定：

```cpp
// src/common/app/ApplicationBase.cc:18
std::mutex appMutex;
ApplicationBase *globalApp = nullptr;
// :38
ApplicationBase::ApplicationBase() { globalApp = this; }
```

所有靜態方法（`updateConfig` / `hotUpdateConfig` / `validateConfig` / `getConfigString` / `getAppInfo` / `getConfigStatus`）都是「拿 `appMutex` → 檢查 `globalApp` → 轉發」。這就是 `CoreService` 的 `getConfig`/`hotUpdateConfig`/`renderConfig`（serviceId 10001，見 §3.3）能在不持有任何物件引用的情況下操作設定的原因。

每次更新都會被記進 `ConfigManager`：

```cpp
// src/common/app/ApplicationBase.cc:31
#define RETURN_AND_RECORD_CONFIG_UPDATE(ret, desc)                                                     \
  Result<Void> _r = (ret);                                                                             \
  getConfigManager().updateConfigRecord(_r.hasError() ? _r.error() : Status(StatusCode::kOK), (desc)); \
  return _r;
```

`ConfigUpdateRecord`（`app/ConfigUpdateRecord.h:7`）是 `{updateTime, result: Status, description}`，可以透過 `CoreService::getLastConfigUpdateRecord` 查回來。`ConfigStatus`（`app/ConfigStatus.h:6`）有五個值：`NORMAL / DIRTY / FAILED / UNKNOWN / STALE`——`DIRTY` 代表「執行中的設定與 mgmtd 上的模板不一致」，`STALE` 代表「模板版本落後」。

`persistConfig`（`app/Utils.cc:177`）在 `--cfg_persist_prefix` 非空時，把完整設定 dump 到 `prefix_YYYYMMDD_HHMMSS_微秒`，每次熱更新都寫一份新檔（`TwoPhaseApplication.h:88` 的 `onConfigUpdated` 就只做這件事）。**沒有輪替、沒有上限**——長期開著會累積檔案。

> **OSS 樹裡 `renderConfig` 是個恆等函式。** `src/common/utils/RenderConfig.cc:71` 的實作是：
> ```cpp
> Result<String> renderConfig(const String &configTemplate,
>                             [[maybe_unused]] const flat::AppInfo *appInfo,
>                             [[maybe_unused]] const std::map<String, String> *envs) {
>   return configTemplate;
> }
> ```
> 兩個參數都標了 `[[maybe_unused]]`，直接回傳輸入。但呼叫路徑非常完整——`app_detail::initConfig`（`Utils.cc:286`）、`ApplicationBase::validateConfig`（`.cc:183`）、`renderConfig`（`.cc:142`）、`hotUpdateConfig` 的 `render` 參數（`.cc:160`）全都認真處理它的錯誤回傳。內部版本顯然會把設定模板裡的 `{{nodeId}}` / `{{hostname}}` / 環境變數替換掉；開源版把替換引擎拿掉了，只留下 `parseReleaseVersion`（`RenderConfig.cc:8`，支援五種版本字串格式）。這解釋了為什麼 3FS 的設定檔範例裡看不到任何模板語法。

---

## 9. `src/common/kv/`（10 檔）

### 9.1 介面在 `common/kv`、實作在 `src/fdb`

```
   common/kv/IKVEngine.h            common/kv/ITransaction.h
      │  createReadonlyTransaction     │  IReadOnlyTransaction
      │  createReadWriteTransaction    │  IReadWriteTransaction
      │                                │  TransactionHelper（靜態工具）
      ├──────────────┬─────────────────┴──────────────┐
      ▼              ▼                                ▼
  fdb/FDBKVEngine  common/kv/mem/MemKVEngine    fdb/HybridKvEngine
  fdb/FDBTransaction  common/kv/mem/MemTransaction  （FDB + Mem 混合，測試用）
      │
      └── fdb/FDBRetryStrategy.h ── 與 common/kv/WithTransaction.h 組合
```

`common/kv/` 裡**沒有任何 FoundationDB 的符號**——連 `#include <foundationdb/...>` 都沒有。FDB 相關的一切（`fdb_error_predicate`、`FDBTransactionOption`）都在 `src/fdb/`。這條界線讓 `common` 不需要連 `libfdb_c`。

代價：`WithTransaction` 用的 `RetryStrategy` 是模板參數而不是虛介面（`WithTransaction.h:16`），所以每個呼叫點都要顯式指定策略型別。`FDBRetryStrategy::onError` 內部再用 `dynamic_cast<FDBTransaction *>(txn)` 把型別偵測回來（`fdb/FDBRetryStrategy.h:77`）——編譯期泛型 + 執行期 RTTI 混用。

另一個容易混淆的地方：**`src/kv/`（沒有 `common/`）是完全不同的東西**。那裡是本地嵌入式 KV（`KVStore.h`、`RocksDBStore`、`LevelDBStore`、`MemDBStore`），給 storage 節點存 chunk metadata 用；`src/common/kv/` 是分散式交易介面，給 meta/mgmtd 存叢集狀態用。兩者沒有繼承關係。

### 9.2 `ITransaction`：四種讀語意

```cpp
// src/common/kv/ITransaction.h:34
class IReadOnlyTransaction {
  virtual void setReadVersion(int64_t version) = 0;
  virtual CoTryTask<std::optional<String>> snapshotGet(std::string_view key) = 0;
  virtual CoTryTask<std::optional<String>> get(std::string_view key) {
    co_return co_await snapshotGet(key);        // :44  唯讀交易的預設退化
  }
  virtual CoTryTask<GetRangeResult> snapshotGetRange(begin, end, limit) = 0;
  virtual CoTryTask<GetRangeResult> getRange(begin, end, limit) = 0;
  virtual CoTryTask<void> cancel() = 0;
  virtual void reset() = 0;
};
```

`snapshotGet` 與 `get` 的差別寫在 `IReadWriteTransaction` 上（`ITransaction.h:92`）：

> The difference of `snapshotGet` and `get` is the former needs no conflict validation and hence won't cause a read-write transaction fail.

也就是 `get` 會把 key 加進讀衝突集、`snapshotGet` 不會。**在唯讀交易裡兩者等價**（基底類別的預設實作直接轉發），但在讀寫交易裡兩個都是純虛，強迫實作者分別處理。這個「基底提供退化預設、衍生類別強制覆寫」的模式（`ITransaction.h:94` 的 `override = 0`）是刻意的：讓 `IReadOnlyTransaction` 的實作者少寫一半程式碼，同時不讓 `IReadWriteTransaction` 的實作者偷懶。

`addReadConflict` / `addReadConflictRange`（`ITransaction.h:98`）讓呼叫端可以「宣告我讀過某個 key」而不真的去讀——用於「我從快取讀到的，但我要它參與衝突偵測」。

`setVersionstampedKey` / `setVersionstampedValue`（`ITransaction.h:107`）暴露了 FDB 的 versionstamp：10 bytes，前 8 是提交版本、後 2 是交易內序號，都是 **big-endian**（`ITransaction.h:28` 的註解）。這是 3FS 裡少數刻意用 big-endian 的地方（其餘見元資料層報告對 endianness 取捨的討論）。

`TransactionHelper::listByPrefix`（`ITransaction.cc:57`）是最常用的工具，它處理了 FDB range scan 的分頁：

```cpp
// src/common/kv/ITransaction.cc:60
auto loadFunc = options.snapshot ? &IReadOnlyTransaction::snapshotGetRange : &IReadOnlyTransaction::getRange;
...
while (limit == 0 || limit > res.size()) {
  auto result = co_await (txn.*loadFunc)(begin, end, limit ? limit - res.size() : 0);
  ...
  XLOGF_IF(FATAL, !kv.key.starts_with(prefix), "key {} not start with {}", ...);   // :71
  if (!result->hasMore) break;
  beginKey = res.back().key;
  begin.key = beginKey;  begin.inclusive = false;                                  // :84
}
```

用成員函式指標在 snapshot / 非 snapshot 之間切換，避免寫兩份迴圈。`XLOGF_IF(FATAL, ...)` 那行是防護：range scan 回傳了不符前綴的 key 代表 `prefixListEndKey` 算錯了，屬於不該發生的資料損壞，寧可當場死掉。

### 9.3 `WithTransaction` + 重試策略

```cpp
// src/common/kv/WithTransaction.h:50
template <typename Handler>
std::invoke_result_t<Handler, IReadWriteTransaction &> run(IReadWriteTransaction &txn, Handler &&handler) {
  auto result = strategy_.init(&txn);
  CO_RETURN_ON_ERROR(result);
  while (true) {
    auto result = co_await runAndCommit(txn, std::forward<Handler>(handler));
    if (!result.hasError()) co_return std::move(result);
    auto retryResult = co_await strategy_.onError(&txn, std::move(result.error()));
    CO_RETURN_ON_ERROR(retryResult);       // 策略說不能重試就往上拋
  }
}
```

`runAndCommit`（`WithTransaction.h:68`）把 handler 與 commit 綁在一起，於是「handler 成功但 commit 衝突」也會走重試路徑——handler 會被**整個重跑**。這意味著 handler 必須是冪等的，或者只操作交易物件本身。

兩套重試策略：

| 策略 | 位置 | 特點 |
|---|---|---|
| `DefaultRetryStrategy<Sleeper>` | `utils/DefaultRetryStrategy.h:50` | 純本地判斷，`isRetryable(error, false)`，指數退避（`RetryState::next()` 每次乘 2） |
| `FDBRetryStrategy` | `fdb/FDBRetryStrategy.h:23` | 優先把退避交給 FDB 自己 |

`FDBRetryStrategy` 的核心是「**能問 FDB 就問 FDB**」：

```cpp
// src/fdb/FDBRetryStrategy.h:85
CoTryTask<void> fdbBackoff(FDBTransaction *txn, Status error) {
  auto errcode = txn->errcode();
  if (UNLIKELY(!errcode)) { ...; co_return co_await defaultBackoff(txn, std::move(error)); }
  FDBErrorPredicate predict =
      config_.retryMaybeCommitted ? FDB_ERROR_PREDICATE_RETRYABLE : FDB_ERROR_PREDICATE_RETRYABLE_NOT_COMMITTED;
  if (!fdb_error_predicate(predict, errcode)) { ...; co_return makeError(std::move(error)); }
  auto ok = co_await txn->onError(errcode);     // :104  FDB 自己決定睡多久、自己 reset
  ...
}
```

`fdb_transaction_on_error` 會自己做退避並重設交易，所以退避節奏與 FDB 叢集的負載回饋一致。只有拿不到 errcode（`txn->errcode()` 為 0）時才退回自己的實作：

```cpp
// src/fdb/FDBRetryStrategy.h:123
auto duration = Duration(backoff_ / 100 * folly::Random::rand32(80, 120));   // ±20% 抖動
co_await folly::coro::sleep(duration.asUs());
```

`retryMaybeCommitted` 這個旗標是關鍵語意開關：`kMaybeCommitted` 代表「commit 送出去了但不知道結果」。設為 `true` 會重試（適合冪等操作），`false` 則直接失敗（適合「配置一個新 ID」這種不能重複執行的操作）。`FDBRetryStrategy::Config` 預設是 `true`（`FDBRetryStrategy.h:30`），但 `mgmtd/MgmtdConfigFetcher.cc:14` 顯式傳 `{1_s, 10, false}` 關掉它。

`TransactionRetry`（`kv/TransactionRetry.h:8`）是設定端：

```cpp
struct TransactionRetry : ConfigBase<TransactionRetry> {
  CONFIG_HOT_UPDATED_ITEM(max_backoff, 1_s);
  CONFIG_HOT_UPDATED_ITEM(max_retry_count, 10u);
};
```

兩個都可熱更新，於是叢集壓力大時可以線上調高退避上限。

### 9.4 `KeyPrefix`：全叢集共享的 4-byte 前綴表

```cpp
// src/common/kv/KeyPrefix.h:8
inline constexpr uint32_t makePrefixValue(const char (&s)[5]) {
  return s[0] + (static_cast<uint32_t>(s[1]) << 8) + ... ;
}
// :13  use enum for avoiding duplicated prefixes.
enum class KeyPrefix : uint32_t {
  Unknown = makePrefixValue("UNKW"),
#define DEFINE_PREFIX(name, s) name = makePrefixValue(s),
#include "KeyPrefix-def.h"
};
```

又是一個 X-macro：`KeyPrefix-def.h` 被 include 兩次，一次展開成 enum 值、一次展開成 `toStr()` 的 switch（`KeyPrefix.h:23`）。**用 enum 而不是 constexpr 常數的理由寫在註解裡**（`KeyPrefix.h:13`）：enum 的重複值會被編譯器發現，散落的常數不會。

`makePrefixValue` 把 4 個字元拼成 little-endian 的 `uint32_t`，所以在記憶體裡直接就是那 4 個 ASCII 字元——FDB 上的 key 是人類可讀的。

全部 17 個前綴（`kv/KeyPrefix-def.h:6`）：

| 前綴 | 名稱 | 擁有者 |
|---|---|---|
| `INOD` | `Inode` | meta |
| `DENT` | `Dentry` | meta |
| `META` | `MetaDistributor` | meta |
| `INOS` | `InodeSession` | meta |
| `IDEM` | `MetaIdempotent` | meta |
| `USER` | `User` | mgmtd |
| `NODE` | `NodeTable` | mgmtd |
| `CHIT` | `ChainTable` | mgmtd |
| `CHIF` | `ChainInfo` | mgmtd |
| `TGIF` | `TargetInfo` | mgmtd |
| `CONF` | `Config` | mgmtd |
| `UTGS` | `UniversalTags` | mgmtd |
| `SING` | `Single` | 「非表格的單一 key」 |
| `CLIS` | `ClientSession` | **已註記 deprecated**（`KeyPrefix-def.h:16`） |
| `KVTB` | `KvTable` | （無使用者） |
| `KVNS` | `KvNamespace` | （無使用者） |
| `KVWG` | `KvWorkerGroup` | （無使用者） |

### 9.5 消失的內建 KV 服務

最後三個前綴（`KVTB` / `KVNS` / `KVWG`）在整個 repo 裡**只出現在定義處**：

```
$ grep -rn "KVTB\|KVNS\|KVWG" . --include="*.h" --include="*.cc" --include="*.cpp" --include="*.rs" --include="*.py" | grep -v third_party
src/common/kv/KeyPrefix-def.h:21:DEFINE_PREFIX(KvTable,          "KVTB")
src/common/kv/KeyPrefix-def.h:22:DEFINE_PREFIX(KvNamespace,      "KVNS")
src/common/kv/KeyPrefix-def.h:23:DEFINE_PREFIX(KvWorkerGroup,    "KVWG")
```

配上 §7.2.1 提到的 24 個 `KvServiceCode`（`StatusCodeDetails.h:257-280`），可以把這個未開源元件的形狀反推出來：

- `KvNamespace` / `KvTable`：一個帶命名空間的表格式 KV 服務，表有 id（`kTableIdMismatch`、`kTableInfoStale`）。
- `KvWorkerGroup` + `kUnknownWorker` / `kNoAvailableWorker` / `kNoAvailableMaster`：master–worker 架構，worker 分組。
- `kStoreLeaseEmpty` / `kStoreLeaseHeld` / `kNotPrimary` / `kStoreLoaded` / `kStoreNotAvailable`：store 以租約分配給 worker，有主從。
- `kHeartbeatVersionStale`：心跳帶版本號。

它把叢集元資料（前綴表）與錯誤碼段（11000–11999）都佔用了，但服務本體、fbs 定義與 client 全部不在開源樹中。跟 §3.3 提到的 `ClientAgentSerde`（serviceId 10，只有宣告沒有註冊）是同一類遺跡。

`kv/mem/`（3 檔）是這一層唯一的完整實作，且**只給測試用**：`MemKV` 是多版本的 `map`，`MemTransaction` 手工模擬了 FDB 的讀衝突集、versionstamp，以及「已提交資料 + 交易內未提交修改」的歸併排序（`MemTransaction.h:292` 的 `getRangeImpl`）。`checkConflict(txn1, txn2)`（`MemTransaction.h:249`）是測試專用的靜態工具。

---

## 10. `src/common/logging/`（22 檔）

### 10.1 為什麼要重寫 folly 的 file handler

folly 自帶 `AsyncFileWriter` 與 `FileHandlerFactory`，但它們寫的是**單一個 `folly::File`**，不支援輪替。3FS 的做法是在 folly 的抽象縫隙裡插一層自己的介面：

```
folly::LoggerDB
   │  registerHandlerFactory
   ▼
hf3fs::logging::FileHandlerFactory ("file")        LogInit.cc:13
hf3fs::logging::EventLogHandlerFactory ("event")   LogInit.cc:14
   │  createHandler → folly::StandardLogHandlerFactory::createHandler(
   │                     type, writerFactory, formatterFactory, options)
   ├──────────────────────────┬─────────────────────────────┐
   ▼                          ▼                             ▼
FileWriterFactory       LogFormatterFactory          EventLogFormatterFactory
   │ createWriter()       （glog 風格 header）          （只輸出訊息本身）
   ├── async_ ? AsyncFileWriter : ImmediateFileWriter
   │                    （folly::AsyncLogWriter 子類 / folly::LogWriter 子類）
   └──── 兩者都持有 shared_ptr<IWritableFile>
                    ├── RotatingFile   （多檔輪替，帶 mutex）
                    └── SingleFile     （單檔 append）
```

關鍵抽象是 `IWritableFile`（`logging/IWritableFile.h:2`），5 個純虛函式：`ttyOutput` / `desc` / `writevFull` / `writeFull` / `flush`。folly 的 writer 只知道這個介面，於是「輪替」與「非同步」變成了兩個正交的維度：

| | `rotate = false` | `rotate = true` |
|---|---|---|
| `async = false` | `ImmediateFileWriter` + `SingleFile` | `ImmediateFileWriter` + `RotatingFile` |
| `async = true` | `AsyncFileWriter` + `SingleFile` | `AsyncFileWriter` + `RotatingFile` |

`AsyncFileWriter`（`logging/AsyncFileWriter.cc`）幾乎是 folly 原版的複製（檔頭還留著 Meta 的 Apache 授權聲明），唯一改動是把 `folly::File` 換成 `shared_ptr<IWritableFile>`；一次最多 `writev` 64 條（`.cc:35`），佇列溢位時寫一行 `"N log messages discarded: logging faster than we can write"`（`.cc:90`）。

### 10.2 `RotatingFile`：`.log` → `.1.log` → `.2.log`

```cpp
// src/common/logging/RotatingFile.cc:10
std::string genFilename(const std::string &base, size_t index) {
  if (index == 0) return base;
  auto [basename, ext] = FileHelper::splitByExtension(base);
  return fmt::format("{}.{}{}", basename, index, ext);      // meta.log → meta.3.log
}

// :34
void RotatingFile::rotate() {
  file_.close();
  for (auto i = options_.maxFiles; i > 0; --i) {            // 從尾往頭搬
    auto src = Path(genFilename(path_, i - 1));
    if (!boost::filesystem::exists(src)) continue;
    boost::filesystem::rename(src, genFilename(path_, i), ec);
    if (ec) { std::this_thread::sleep_for(100ms); rename(...) again;   // :46  重試一次
      if (ec) { file_.reopen(true); currentSize_ = 0; throw ...; } }
  }
  file_.reopen(/*truncate=*/true);
}
```

三個值得注意的行為：

1. **輪替是 O(maxFiles) 次 rename**。`max_files` 預設 100（`LogConfig.h:57`），上限硬編碼為 200000（`RotatingFile.cc:22`）。每次輪替就是最多 100 次 `rename` 系統呼叫，而且**持有 `mu_`**（`RotatingFile.cc:70`），期間所有日誌寫入阻塞。
2. **rename 失敗會重試一次、睡 100ms**（`.cc:46`）。這是為了處理 NFS 或有防毒/監控程式抓著檔案的情況。第二次還失敗就 truncate 當前檔並拋例外——**寧可丟日誌也不阻塞**。
3. `checkRotate`（`.cc:85`）在檔案被外部刪除時會自動 `reopen`（`if (!file_)`），這讓 `logrotate` 這類外部工具搬走檔案後日誌不會靜默消失。

`rotate_on_open` 預設 `false`（`LogConfig.h:59`），打開的話每次啟動就滾一次，於是每次重啟的日誌都在獨立檔案裡。

### 10.3 設定 → JSON → folly

folly 的日誌設定吃的是它自己的字串格式，3FS 不直接寫那個格式，而是先組 `folly::dynamic` 再序列化成 JSON：

```cpp
// src/common/logging/LogConfig.cc:88
String generateLogConfig(const LogConfig &c, String serverName) {
  auto cfg = LogConfigObject::create(c, serverName);
  folly::json::serialization_opts opts;
  opts.pretty_formatting = false;
  json = folly::json::serialize(cfg, opts);
  opts.pretty_formatting = true;
  XLOGF(INFO, "Folly log json configure: {}", folly::json::serialize(cfg, opts));   // :96
  return json;
}
```

序列化兩次——一次緊湊的餵給 folly、一次 pretty 的印進日誌。這讓「為什麼我的 log level 沒生效」變成可以直接從日誌第一頁看出來的問題。

檔名是**在轉譯時**才生成的（`LogConfig.cc:66`）：

```cpp
if (c.file_path().empty()) {
  if (c.name() == "normal") options.insert("path", fmt::format("{}.log", serverName));
  else                      options.insert("path", fmt::format("{}.{}.log", serverName, c.name()));
} else {
  options.insert("path", c.file_path());
}
```

所以 `meta_main` 產生 `meta.log` / `meta.err.log`，`storage_main` 產生 `storage.log` / `storage.err.log`——同一份設定模板可以推給所有節點型別。

### 10.4 三個預設 handler 的分工

```cpp
// src/common/logging/LogConfig.h:130
CONFIG_HOT_UPDATED_ITEM(handlers, std::vector<LogHandlerConfig>(
    {makeNormalHandlerConfig(), makeErrHandlerConfig(), makeFatalHandlerConfig()}));
```

| handler | writer | async | start_level | 目的地 |
|---|---|---|---|---|
| `normal` | FILE | **true** | `MIN_LEVEL`（全收） | `<name>.log` |
| `err` | FILE | **false** | `ERR` | `<name>.err.log` |
| `fatal` | **STREAM** | false | `FATAL` | **stderr** |

`err` 與 `fatal` 都是同步的（`LogConfig.h:106`、`:115`），因為出錯時行程可能馬上就要死，非同步佇列裡的訊息會來不及刷出去。`fatal` 走 stderr 而不是檔案，是為了讓容器/supervisor 的標準錯誤捕捉能拿到臨終遺言。

根 category（`LogConfig.h:77`）掛這三個 handler，level `INFO`。同一則 `ERR` 訊息會**同時**進 `normal` 與 `err`——後者是給運維 `tail` 用的濃縮視圖。

`LogFormatter`（`logging/LogFormatter.cc:27`）是 folly `GlogStyleFormatter` 的改寫，唯一實質改動是四個 `thread_local` 快取：

```cpp
// src/common/logging/LogFormatter.cc:31
thread_local std::string cachedDateTimeStr;      // 同一秒內共用
thread_local std::string cachedThreadIdStr;      // 同一 tid 共用
thread_local folly::Optional<std::string> currentThreadName;
// :41
static const auto timeZoneStr = fmt::format("{:%Ez}", localTime);   // NOTE: assume nobody changes TZ at runtime
```

同一秒內的日誌共用一次 `fmt::localtime` + 日期格式化，時區字串則是整個行程算一次。輸出格式：

```
[2026-08-15T03:14:07.123456789+08:00 IOWorker  1234 Transport.cc:512 INFO] ...
```

多行訊息會**每一行都加上完整 header**（`LogFormatter.cc:84`），這樣 `grep` 一個時間戳不會漏掉堆疊的中間幾行。

### 10.5 `eventlog`：與 `src/analytics/` 的分工

第四個 handler 型別 `EVENT` 不在預設清單裡，要靠 `makeEventCategoryConfig()`（`LogConfig.h:85`）與 `makeEventHandlerConfig()`（`.h:121`）顯式加入：

```cpp
// src/common/logging/LogConfig.h:85
cfg.set_categories(std::vector<String>{"eventlog"});
cfg.set_propagate(folly::LogLevel::ERR);     // 只有 ERR 以上會往父 category 傳
cfg.set_inherit(false);                      // 不繼承父 category 的 handler
cfg.set_handlers({"event"});
```

`inherit = false` + `propagate = ERR` 的組合意思是：**`eventlog.*` 的 INFO 訊息只進 event 檔，不會汙染 normal 日誌**，但 ERR 以上仍然會冒泡上去。

`EventLogFormatter` 完全不加 header：

```cpp
// src/common/logging/LogFormatter.cc:117
std::string EventLogFormatter::formatMessage(const folly::LogMessage &message, const folly::LogCategory *) {
  return fmt::format("{}\n", message.getMessage());
}
```

因為 event log 的每一行本身就是一筆結構化記錄，前面加時間戳反而礙事。使用者是 meta 的檔案系統事件（`src/meta/event/Event.cc:14-23`：`eventlog.Create` / `Mkdir` / `HardLink` / `Remove` / `Truncate` / `OpenWrite` / `CloseWrite` / `Rename` / `Symlink` / `GC`，每個 event 一個 `folly::Logger`）。

分工是這樣的：

| | `src/common/logging/` | `src/analytics/` |
|---|---|---|
| 對象 | 人（與 grep） | 機器（與資料倉儲） |
| 格式 | glog 風格文字行 / event log 純文字行 | serde 物件寫成結構化列（`SerdeObjectWriter.h`、`SerdeSchemaBuilder.h`） |
| 落地 | 本地檔案，輪替 | `StructuredTraceLog.h`，帶 schema |
| 訂閱者 | 運維 tail / 日誌收集 agent | 離線分析 |

`logging/` 不依賴 `analytics/`，`analytics/` 也不依賴 `logging/`——兩條完全獨立的資料路徑。event log 是介於兩者之間的折衷：走 folly logging 的管線（拿到 handler、輪替、非同步這些現成能力），但格式是給機器解析的。

---

## 11. `src/common/monitor/`（17 檔）

這個目錄是 3FS 的指標收集前端：`Recorder`（`monitor/Recorder.h`，counter / latency / distribution 三類記錄器）在各元件裡以靜態物件宣告，`Monitor`（`monitor/Monitor.h`）週期性把它們的值收成 `Sample`（`monitor/Sample.h`），再交給三種 `Reporter`（`monitor/Reporter.h`）之一送出：`MonitorCollectorClient`（走 serviceId 194 的 RPC，最常見）、`ClickHouseClient`（直寫 ClickHouse）、`LogReporter`（寫本地日誌，除錯用），另有 `TaosClient`（TDengine）。`DigestBuilder` 負責分位數摘要，`ScopedMetricsWriter` 是 RAII 的延遲埋點。

**這一層已有專門的深度剖析：[`monitor_collector_main-監控收集器深度剖析.md`](./monitor_collector_main-監控收集器深度剖析.md)**，涵蓋 `Recorder` 的分片與標籤機制、`Monitor` 的收集週期、三種 reporter 的取捨、以及 collector 端的聚合與落庫。此處不重複。

唯一要在本報告補記的一點：`monitor` 是 `utils` 的**反向依賴**——`utils/ReentrantLockManager.cc:10` 與 `utils/ExecutorStatsReporter.h:3` 都 include 了 `monitor/Recorder.h`，打破了「utils 是葉節點」的分層。實務上沒有問題（因為 `monitor` 只依賴 `utils` 裡不含監控的那些檔案），但這是一個沒有編譯期保護的環。

---

## 12. 檔案索引（248 檔）

### 12.1 根目錄（1 檔）

| 檔案 | 職責 |
|---|---|
| `src/common/CMakeLists.txt` | 定義 `common`（STATIC）與 `hf3fs_common_shared`（SHARED）兩個孿生目標（見 §1.1） |

### 12.2 `src/common/app/`（17 檔）

| 檔案 | 職責 |
|---|---|
| `app/AppInfo.h` | `AppInfo` / `FbsAppInfo` / `ServiceGroupInfo` / `TagPair` / `ReleaseVersion` 定義 |
| `app/AppInfo.cc` | 上述型別的比較、建構、`extractAddresses`、`findTag`/`removeTag` 實作 |
| `app/ApplicationBase.h` | 所有 server 的抽象基底：`run` 五步 + 設定熱更新靜態介面 |
| `app/ApplicationBase.cc` | `run`/`mainLoop`/訊號處理/`globalApp` 單例/設定載入與更新轉發 |
| `app/ClientId.h` | `{Uuid, hostname}` 的客戶端識別，serde 可序列化 |
| `app/ConfigManager.h` | 設定更新的狀態機宣告（`lastConfigUpdateRecord` / `configStatus`） |
| `app/ConfigManager.cc` | `updateConfig` / `hotUpdateConfig` / diff 檢查 / 狀態轉移實作 |
| `app/ConfigStatus.h` | `enum class ConfigStatus { NORMAL, DIRTY, FAILED, UNKNOWN, STALE }` |
| `app/ConfigUpdateRecord.h` | `{updateTime, result: Status, description}` 的 serde 結構 |
| `app/gflags.cc` | `--release_version` / `--app_cfg` / `--launcher_cfg` / `--use_local_cfg` / `--cfg_persist_prefix` 定義 |
| `app/NodeId.h` | `STRONG_TYPEDEF(uint32_t, NodeId)`，全部內容只有 7 行 |
| `app/OnePhaseApplication.h` | 純本地設定的啟動骨架，只有 `monitor_collector` 用 |
| `app/Thread.h` | `Thread::blockInterruptSignals` / `unblockInterruptSignals` 宣告 |
| `app/Thread.cc` | 用 `pthread_sigmask` 讓 SIGINT/SIGTERM 只落在主執行緒 |
| `app/TwoPhaseApplication.h` | launcher → 遠端設定 → server 的兩階段啟動骨架（4 個 binary 用） |
| `app/Utils.h` | `app_detail::` 命名空間的輔助函式宣告 |
| `app/Utils.cc` | 設定/AppInfo 的重試載入、`persistConfig`、jemalloc profiling 熱更新回呼 |

### 12.3 `src/common/kv/`（10 檔）

| 檔案 | 職責 |
|---|---|
| `kv/IKVEngine.h` | 15 行純虛介面：建立唯讀 / 讀寫交易 |
| `kv/ITransaction.h` | `IReadOnlyTransaction` / `IReadWriteTransaction` / `TransactionHelper` / `Versionstamp` |
| `kv/ITransaction.cc` | `isRetryable` 白名單、`keyAfter`、`prefixListEndKey`、`listByPrefix` 分頁迴圈 |
| `kv/KeyPrefix.h` | `makePrefixValue` + X-macro 展開的 `KeyPrefix` enum 與 `toStr` |
| `kv/KeyPrefix-def.h` | 17 個 4-byte 前綴的唯一真相來源（X-macro 資料檔） |
| `kv/TransactionRetry.h` | 兩個熱更新設定項：`max_backoff`、`max_retry_count` |
| `kv/WithTransaction.h` | `run(txn, handler)` 重試迴圈，讀寫版把 commit 一起納入重試 |
| `kv/mem/MemKV.h` | 記憶體多版本 KV 儲存 + `VersionstampedKV` 表示 |
| `kv/mem/MemKVEngine.h` | `IKVEngine` 的記憶體實作，唯讀交易直接複用讀寫交易 |
| `kv/mem/MemTransaction.h` | 手工模擬 FDB 語意（讀衝突集、寫集歸併、versionstamp、故障注入） |

### 12.4 `src/common/logging/`（22 檔）

| 檔案 | 職責 |
|---|---|
| `logging/AsyncFileWriter.h` | folly `AsyncLogWriter` 子類宣告，持有 `IWritableFile` |
| `logging/AsyncFileWriter.cc` | 批次 `writev`（每批 64 條）+ 溢位丟棄計數訊息 |
| `logging/FileHandlerFactory.h` | `"file"` 與 `"event"` 兩個 handler factory 宣告 |
| `logging/FileHandlerFactory.cc` | 組裝 writer factory + formatter factory 交給 folly |
| `logging/FileHelper.h` | 低階檔案句柄封裝宣告（open/reopen/write/writev/size/splitByExtension） |
| `logging/FileHelper.cc` | 上述實作 |
| `logging/FileWriterFactory.h` | 解析 `path`/`async`/`rotate`/`max_files` 等選項的 writer 工廠 |
| `logging/FileWriterFactory.cc` | 選項驗證 + 四象限組合（async × rotate）的物件建構 |
| `logging/ImmediateFileWriter.h` | 同步寫入的 `folly::LogWriter` 子類宣告 |
| `logging/ImmediateFileWriter.cc` | 直接呼叫 `IWritableFile::writeFull` |
| `logging/IWritableFile.h` | 5 個純虛函式的可寫檔抽象——輪替與非同步正交的關鍵 |
| `logging/LogConfig.h` | `LogLevel` / `LogCategoryConfig` / `LogHandlerConfig` / `LogConfig` 與五個預設工廠 |
| `logging/LogConfig.cc` | 把 `LogConfig` 轉譯成 folly 吃的 JSON，並 pretty print 一份到日誌 |
| `logging/LogFormatter.h` | `LogFormatter` / `EventLogFormatter` 與兩個 factory 宣告 |
| `logging/LogFormatter.cc` | glog 風格 header + 四個 thread_local 快取；event 版只輸出訊息本身 |
| `logging/LogHelper.h` | `ERRLOGF_IF` / `WARNLOGF_IF` 等「條件性提升日誌等級」巨集 |
| `logging/LogInit.h` | `initLogHandlers` / `init` / `initOrDie` 宣告 |
| `logging/LogInit.cc` | 用函式內 static 保證兩個 handler factory 只註冊一次 |
| `logging/RotatingFile.h` | 輪替檔的 `Options{maxFiles, maxFileSize, rotateOnOpen}` 與介面 |
| `logging/RotatingFile.cc` | 從尾往頭 rename、失敗睡 100ms 重試一次、外部刪檔自動 reopen |
| `logging/SingleFile.h` | 單檔 append 的 `IWritableFile` 實作宣告 |
| `logging/SingleFile.cc` | 包一個 `folly::File`，`writevFull` 直接轉發 |

### 12.5 `src/common/monitor/`（17 檔，詳見專門報告）

| 檔案 | 職責 |
|---|---|
| `monitor/ClickHouseClient.h` | 直寫 ClickHouse 的 reporter 宣告 |
| `monitor/ClickHouseClient.cc` | 用 clickhouse-cpp 建表與批次插入 |
| `monitor/DigestBuilder.h` | 分位數摘要建構器宣告 |
| `monitor/DigestBuilder.cc` | t-digest 風格的分位數合併實作 |
| `monitor/LogReporter.h` | 把 sample 寫進本地日誌的 reporter 宣告（除錯用） |
| `monitor/LogReporter.cc` | 逐筆 sample 格式化落日誌 |
| `monitor/Monitor.h` | `Monitor::Config` 與 `start`/`stop` 靜態介面 |
| `monitor/Monitor.cc` | 收集迴圈、reporter 選擇、標籤注入 |
| `monitor/MonitorCollectorClient.h` | 走 serviceId 194 RPC 的 reporter 宣告 |
| `monitor/MonitorCollectorClient.cc` | 連線管理與批次送出 |
| `monitor/Recorder.h` | `CountRecorder` / `LatencyRecorder` / `DistributionRecorder` 與 `TagRef` |
| `monitor/Recorder.cc` | 記錄器的全域註冊表與分片累加實作 |
| `monitor/Reporter.h` | reporter 的純虛介面 |
| `monitor/Sample.h` | 單筆指標樣本的表示 |
| `monitor/ScopedMetricsWriter.h` | RAII 延遲埋點守衛 |
| `monitor/TaosClient.h` | TDengine reporter 宣告 |
| `monitor/TaosClient.cc` | TDengine 連線與寫入實作 |

### 12.6 `src/common/net/`（47 檔，詳見 §3–§4）

| 檔案 | 職責 |
|---|---|
| `net/Allocator.h` | RPC 緩衝的配置器介面 |
| `net/Client.h` | `net::Client`——客戶端側入口，內建 `RDMAControl` 與 echo 服務 |
| `net/EventLoop.h` | epoll 迴圈宣告，TCP 與 RDMA completion channel 共用 |
| `net/EventLoop.cc` | epoll 註冊/等待/分派實作 |
| `net/IfAddrs.h` | 列舉本機網卡位址 |
| `net/IOWorker.h` | 連線取得與非同步送出的介面 |
| `net/IOWorker.cc` | `sendAsync` 與 `TransportPool` 協作實作 |
| `net/Listener.h` | 監聽器宣告與設定 |
| `net/Listener.cc` | accept 迴圈、按 `Address::Type` 註冊服務（`IBConnect` 強制 TCP） |
| `net/MessageHeader.h` | 8 byte 裸 header：crc32c 高 24 位 + 魔數 + 壓縮旗標 + size |
| `net/MessageHeader.cc` | header 的校驗與判定實作 |
| `net/Network.h` | 網路層常數與型別彙整 |
| `net/PeerInfo.h` | 對端資訊 |
| `net/Processor.h` | 收包狀態機宣告 |
| `net/Processor.cc` | 驗 header、zstd 解壓、分派到 `serde::Services` |
| `net/RDMAControl.h` | serviceId 10：RDMA 傳輸許可服務宣告 |
| `net/RDMAControl.cc` | `apply` 方法與許可計數實作 |
| `net/RequestOptions.h` | 逾時、重試、壓縮等 per-request 選項 |
| `net/Server.h` | `net::Server`——持有 1~4 個 `ServiceGroup` |
| `net/Server.cc` | `setup`/`start`/`stopAndJoin` 與 `describe` 實作 |
| `net/ServiceGroup.h` | 一組服務 + 一組監聽位址 + 一份 `serde::Services` |
| `net/ServiceGroup.cc` | group 的啟停與位址列舉 |
| `net/Socket.h` | 7 個純虛函式的 socket 抽象（TCP/RDMA 共用） |
| `net/ThreadPoolGroup.h` | 網路層執行緒池組合宣告 |
| `net/ThreadPoolGroup.cc` | 各池的建立與 join |
| `net/Transport.h` | 9-bit 狀態機與 `MPSCWriteList` 宣告 |
| `net/Transport.cc` | `doWrite` iovec 批次、狀態轉移、錯誤處理 |
| `net/TransportPool.h` | 依位址快取連線的池宣告 |
| `net/TransportPool.cc` | 取用、非同步重連與淘汰 |
| `net/Waiter.h` | uuid 配置與請求/回應配對表宣告 |
| `net/Waiter.cc` | 分片表 + 單執行緒堆式計時器實作 |
| `net/WriteItem.h` | `SerdeBuffer` 與 `WriteList` 宣告，含 timestamp 原地補寫 |
| `net/WriteItem.cc` | 緩衝配置與 zstd 壓縮實作 |
| `net/ib/IBConnect.h` | serviceId 11：QP 資訊交換的請求/回應結構 |
| `net/ib/IBConnect.cc` | 非對稱握手、`query`/`connect` 實作（拒絕非 TCP transport） |
| `net/ib/IBConnectService.h` | `IBConnect` 的 `SERDE_SERVICE` 宣告 |
| `net/ib/IBDevice.h` | HCA/port/zone 與 `IBManager` 宣告 |
| `net/ib/IBDevice.cc` | 裝置列舉、RoCE GID 手工探測、zone 對應 |
| `net/ib/IBSocket.h` | 在 RC QP 上模擬 stream socket 的介面 |
| `net/ib/IBSocket.cc` | ring buffer、ImmData、credit 流控、drain 實作 |
| `net/ib/RDMABuf.h` | 多 HCA 全註冊的 RDMA 緩衝與 rkey 陣列宣告 |
| `net/ib/RDMABuf.cc` | `ibv_reg_mr` 全裝置註冊與緩衝池實作 |
| `net/sync/Client.h` | 同步（阻塞）RPC 客戶端 |
| `net/sync/ConnectionPool.h` | 同步客戶端的連線池宣告 |
| `net/sync/ConnectionPool.cc` | 連線取用與歸還實作 |
| `net/tcp/TcpSocket.h` | `Socket` 的 TCP 實作宣告，`flush` 為空 |
| `net/tcp/TcpSocket.cc` | `send` = `writev`、`recv` = `readv` 與錯誤映射 |

### 12.7 `src/common/serde/`（16 檔，詳見 §5）

| 檔案 | 職責 |
|---|---|
| `serde/BigEndian.h` | big-endian 標記型別（FDB key 用） |
| `serde/CallContext.h` | server 側呼叫上下文宣告，反向 include `net/Transport.h`（見 §2） |
| `serde/CallContext.cc` | 回應序列化、`readTransmission`/`writeTransmission` 實作 |
| `serde/ClientContext.h` | client 側 `call<>()` 入口與選項合併 |
| `serde/ClientContext.cc` | 送出、等待 `Waiter`、逾時與錯誤轉換實作 |
| `serde/ClientMockContext.h` | 測試用的假 client context |
| `serde/Echo.h` | serviceId 10000 的 echo 服務，每個 `Services` 建構時強制加入 |
| `serde/MessagePacket.h` | 線上封包結構：uuid / serviceId / methodId / flags / Version / payload / Timestamp |
| `serde/Serde.h` | 反射 + 二進位 / JSON / TOML 三後端的核心 |
| `serde/Serde.cc` | TOML 後端實作（`TomlObject : toml::value`，用 toml11） |
| `serde/SerdeComparisons.h` | 由 serde 欄位自動生成比較運算子 |
| `serde/SerdeHelper.h` | `SerdeHelper<T>` CRTP 基底 |
| `serde/Service.h` | `SERDE_SERVICE` / `SERDE_SERVICE_METHOD` 巨集與 `MethodExtractor` 跳表 |
| `serde/Services.h` | `std::array<ServiceWrapper, 65536> services_[2]`，O(1) 分派 |
| `serde/TypeName.h` | 編譯期型別名稱 |
| `serde/Visit.h` | 欄位訪問器 |

### 12.8 `src/common/utils/`（118 檔）

**錯誤與狀態（7）**

| 檔案 | 職責 |
|---|---|
| `utils/Result.h` | `Result<T>` 別名 + 16 個控制流巨集（`RETURN_ON_ERROR` 等） |
| `utils/Status.h` | 8 byte 的 `Status`：高 16 位存碼、低 48 位存 rep 指標 |
| `utils/StatusCode.h` | `status_code_t` + X-macro 展開 11 個命名空間常數 |
| `utils/StatusCode.cc` | `toString` / `typeOf` / `toErrno`（RPC 全轉 `EREMOTEIO`） |
| `utils/StatusCodeDetails.h` | 236 條狀態碼的唯一資料檔 |
| `utils/StatusCodeConversion.h` | 錯誤碼跨層轉換宣告 |
| `utils/StatusCodeConversion.cc` | `convertToStorageClientCode` 實作 |

**協程與執行器（14）**

| 檔案 | 職責 |
|---|---|
| `utils/Coroutine.h` | `CoTask` / `CoTryTask` / `CancellationToken` 別名 |
| `utils/CoroutinesPool.h` | N 條協程消費 `BoundedQueue`，可選 5ms 超時式 work stealing |
| `utils/DynamicCoroutinesPool.h` | 可在執行期調整協程數的池宣告 |
| `utils/DynamicCoroutinesPool.cc` | `setCoroutinesNum` 增減實作 |
| `utils/PriorityCoroutinePool.h` | 帶優先級佇列的協程池 |
| `utils/BackgroundRunner.h` | 週期性任務的啟停介面，executor 用 `std::variant` 二選一 |
| `utils/BackgroundRunner.cc` | 固定週期迴圈、間隔每輪重取、非取消例外一律 FATAL |
| `utils/CPUExecutorGroup.h` | 6 種 `ExecutorStrategy` 與 `pickNext`/`pickNextFree` |
| `utils/CPUExecutorGroup.cc` | 各策略的 executor + 佇列組裝，`pickNextFree` 探測 4 個 |
| `utils/WorkStealingBlockingQueue.h` | `SharedNothing` / `WorkStealing` / `RoundRobin` 三種佇列策略 |
| `utils/CoroSynchronized.h` | `folly::coro::SharedMutex` 的 `Synchronized` 風格包裝 |
| `utils/CoLockManager.h` | 雜湊分片的協程鎖管理器（預設 256 片） |
| `utils/CountDownLatch.h` | 協程可 await 的倒數閂 |
| `utils/DestructionGuard.h` | 等待所有 `shared_ptr` 引用者放手 |

**佇列、鎖與併發原語（14）**

| 檔案 | 職責 |
|---|---|
| `utils/BoundedQueue.h` | 雙信號量的有界佇列，`co_enqueue`/`co_dequeue` |
| `utils/MPSCQueue.h` | 多產一消無鎖佇列，`static_assert` 三條 cacheline |
| `utils/PriorityUnboundedQueue.h` | 協程版優先級無界佇列 |
| `utils/SimpleRingBuffer.h` | 自帶 allocator 的環形緩衝 |
| `utils/Semaphore.h` | 有效容量可調、最大容量固定的信號量 |
| `utils/SemaphoreGuard.h` | `Semaphore` 的 RAII 守衛 |
| `utils/SimpleSemaphore.h` | mutex + condvar 手寫信號量宣告 |
| `utils/SimpleSemaphore.cc` | `Waiter` 鏈結串列與 `fastWait` 快路徑 |
| `utils/LockManager.h` | 雜湊分片的同步鎖管理器 |
| `utils/ReentrantLockManager.h` | 可重入讀寫鎖管理器宣告 |
| `utils/ReentrantLockManager.cc` | 實作 + 四個 `monitor::LatencyRecorder` 埋點 |
| `utils/FairSharedMutex.h` | 用 GNU 私有初始化器強制寫者優先的 `shared_mutex` |
| `utils/Shards.h` | 34 行的分片容器：`array<mutex,N>` + `array<T,N>` |
| `utils/ConcurrencyLimiter.h` | 依 key 限制併發，超額者排 `Baton` |

**時間與大小（6）**

| 檔案 | 職責 |
|---|---|
| `utils/Duration.h` | `chrono::nanoseconds` 子類 + 7 種字面量 |
| `utils/Duration.cc` | `"1h30min"` 的累加式解析（單位必須遞減）與 `toString` |
| `utils/UtcTime.h` | 微秒粒度 `UtcClock`/`UtcTime`；formatter 走 **localtime** |
| `utils/UtcTime.cc` | `now()` / `to_time_t` / `YmdHMS` 實作 |
| `utils/UtcTimeSerde.h` | `SerdeMethod<UtcTime>` 特化（放 utils 以避免 utils→serde 依賴） |
| `utils/Size.h` | 1024 與 1000 兩套字面量、`serdeToReadable`/`serdeFromReadable` |
| `utils/Size.cc` | `"10MB"` / `"infinity"` 雙向轉換與 `around()` 近似輸出 |

**字串、編碼與雜湊（14）**

| 檔案 | 職責 |
|---|---|
| `utils/String.h` | `using String = std::string`（全檔唯一內容） |
| `utils/StringUtils.h` | 保序編碼、hex、enum→字串的宣告與模板 |
| `utils/StringUtils.cc` | `encodeOrderPreservedString` 系列 + `getPrefixEnd` |
| `utils/coding.h` | LevelDB 風格 `PutFixed*` / `PutVarint*` / `GetLengthPrefixedSlice` 宣告 |
| `utils/coding.cc` | 上述編解碼實作 |
| `utils/SerDeser.h` | 與 `serde/` 無關的輕量手寫序列化器，供 KV 編碼用 |
| `utils/Varint32.h` | serde 的 varint32 標記型別 |
| `utils/Varint64.h` | serde 的 varint64 標記型別 |
| `utils/MurmurHash3.h` | MurmurHash3 三個變體宣告 |
| `utils/MurmurHash3.cc` | Austin Appleby 原版實作（public domain） |
| `utils/RobinHood.h` | vendored `robin_hood` v3.11.5（2484 行） |
| `utils/RobinHoodUtils.h` | `RHStringHashFlatMap`/`NodeMap` 別名與異質查找 hasher |
| `utils/UnorderedDense.h` | vendored `ankerl::unordered_dense` v2（1470 行） |
| `utils/Utf8.h` | vendored sheredom `utf8.h`（1599 行） |
| `utils/Uuid.h` | `boost::uuids::uuid` 子類 + hash/format/serde 特化 |
| `utils/Uuid.cc` | `std::hash<Uuid>` 與 `random()`/`max()`/`zero()` 實作 |

**型別工具與編譯期魔法（10）**

| 檔案 | 職責 |
|---|---|
| `utils/TypeTraits.h` | `is_specialization_of`、容器與 optional 偵測 |
| `utils/Reflection.h` | `Rank<N>` 過載排序與 tuple 操作，serde/ConfigBase 共同底座 |
| `utils/Thief.h` | `friend consteval` ADL 橋接，取得私有成員型別 |
| `utils/StrongType.h` | `STRONG_TYPEDEF`（取自 ClickHouse）+ `StrongTyped` concept |
| `utils/NameWrapper.h` | 字串當非型別模板參數（8 行） |
| `utils/ConstructLog.h` | 建構/解構各印一行 INFO（7 行） |
| `utils/MagicEnum.hpp` | vendored magic_enum v0.8.2（1817 行） |
| `utils/Nameof.hpp` | vendored nameof v0.10.2（1287 行） |
| `utils/Conversion.h` | `isSafeConvertTo<To>(from)` 整數窄化檢查 |
| `utils/Selector.h` | 可用 `&&`/`\|\|` 組合的謂詞包裝 |
| `utils/Transform.h` | `transformTo<Container>(span, f)`，vector 走 `reserve` 特化 |
| `utils/OptionalUtils.h` | `makeOptional(ptr)` / `optionalMap` |

**系統、行程與檔案（16）**

| 檔案 | 職責 |
|---|---|
| `utils/SysResource.h` | hostname（實體/pod 雙語意）、pid、fd limit、`exec`、`DiskInfo` |
| `utils/SysResource.cc` | 上述實作，磁碟資訊靠解析 `/sys` 與外部命令 |
| `utils/SysvShm.h` | System V 共享記憶體的 RAII 包裝宣告 |
| `utils/SysvShm.cc` | `shmget`/`shmat`/`shmctl` 實作（USRBIO 的 iov 區） |
| `utils/FdWrapper.h` | fd 的 RAII 包裝，隱式轉 `int`，可 `release` |
| `utils/Path.h` | `using Path = boost::filesystem::path` + hash/format 特化 |
| `utils/Path.cc` | `std::hash<Path>` 實作（5 行） |
| `utils/FileUtils.h` | `loadFile` / `storeToFile` 宣告 |
| `utils/FileUtils.cc` | 兩者實作，錯誤轉 `Result` |
| `utils/BoostFileSystemWrappers.h` | 6 個 boost fs 函式的 `Result` 版宣告 |
| `utils/BoostFileSystemWrappers.cc` | 用 `callBoostFsFunc` 模板統一把 `error_code` 轉 `Result` |
| `utils/suicide.h` | `suicide(bool graceful)` 宣告 |
| `utils/suicide.cc` | 對自己送 SIGTERM 或 SIGKILL |
| `utils/Linenoise.h` | vendored linenoise 標頭 |
| `utils/Linenoise.c` | vendored linenoise 實作——**唯一的 `.c` 檔，GLOB 抓不到**（見 §1.1） |
| `utils/VersionInfo.h` | `VersionInfo::full()` / `commitHashShort()` / `commitHashFull()` 宣告 |
| `utils/VersionInfo.cc.in` | CMake 模板，建置期填入版本、commit hash、時間戳、pipeline id |
| `utils/ZSTD.h` | 定義 `ZSTD_STATIC_LINKING_ONLY` 後 include zstd |

**設定系統（6，詳見 §6）**

| 檔案 | 職責 |
|---|---|
| `utils/ConfigBase.h` | `CONFIG_ITEM` / `CONFIG_OBJ` / `CONFIG_HOT_UPDATED_ITEM` 等巨集與反射註冊（925 行） |
| `utils/ConfigBase.cc` | `config::parseFlags`、TOML 更新、原子更新與驗證 |
| `utils/RenderConfig.h` | `renderConfig` / `parseReleaseVersion` 宣告 |
| `utils/RenderConfig.cc` | **`renderConfig` 在 OSS 版是恆等函式**；`parseReleaseVersion` 支援 5 種格式 |
| `utils/Toml.hpp` | vendored toml++（17402 行，`TOML_HEADER_ONLY 0`） |
| `utils/Toml.cc` | `#define TOML_IMPLEMENTATION` 一行，把 toml++ 編出來 |

**快取、池、追蹤與其他（15）**

| 檔案 | 職責 |
|---|---|
| `utils/ObjectPool.h` | TLS 64 + 全域 64×64 兩層物件池，deleter 負責解構並歸還 |
| `utils/LruCache.h` | `list + unordered_map` 的 LRU |
| `utils/AtomicSharedPtrTable.h` | 定長槽表 + `AvailSlots` 空槽配置，槽內 `atomic_shared_ptr` |
| `utils/AtomicValue.h` | 讓 `std::atomic<T>` 可複製 |
| `utils/IdAllocator.h` | 靠 KV 交易保證唯一的 64-bit ID 配置器，多 shard 降衝突 |
| `utils/FaultInjection.h` | 掛在 `RequestData` 上的按機率注錯，`FAULT_INJECTION()` 巨集 |
| `utils/Tracing.h` | `TRACING_ADD_EVENT` / `SCOPE_SET_TRACING_POINTS` 與 `Point`/`Points` |
| `utils/TracingEvent.h` | 事件編號的位元佈局（prefix / pair / value 三段） |
| `utils/TracingEvent.cc` | `toString(event)` 的 X-macro 展開 |
| `utils/TracingEventDetails.h` | 事件清單資料檔（`PAIR_EVENT(Fdb, NewTransaction, 0)` 等） |
| `utils/RequestInfo.h` | 掛在 `RequestContext` 上的請求級資訊 |
| `utils/LogCommands.h` | `LOG_COMMAND(level, desc, expr)`——統一「宣告、執行、失敗回報」 |
| `utils/HandleException.h` | **空函式佔位**（全檔 8 行，函式體是一個註解） |
| `utils/ExecutorStatsReporter.h` | 把 executor pool stats 送進 monitor 的模板類 |
| `utils/ExecutorStatsReporter.cc` | 針對 `CPUThreadPoolExecutor` / `CPUExecutorGroup` 的顯式實例化 |
| `utils/DefaultRetryStrategy.h` | 純本地判斷的指數退避重試策略 + `CoroSleeper` |
| `utils/ExponentialBackoffRetry.h` | 帶總時限的指數退避計時器（非交易用） |
| `utils/RandomUtils.h` | `randomSelect(vector)` |
| `utils/Int128.h` | `int128_t` / `uint128_t` 別名 + hash 特化 |
| `utils/Shuffle.h` | 三套 libstdc++ 洗牌實作 + `find_safe_seed`（見 §7.3） |
| `utils/DownwardBytes.h` | serde 的倒寫緩衝區（見 §5） |
| `utils/Address.h` | `net::Address`——5 種型別打包成一個 `uint64_t`（見 §3.5） |
| `utils/RapidCsv.h` | vendored rapidcsv（1541 行，`admin_cli` 用） |
| `utils/ArgParse.h` | vendored argparse（1606 行，`admin_cli` 用） |
