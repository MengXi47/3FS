# 3FS RDMA 編程深度剖析：從 TCP 握手到 RDMA 收發

> 範圍：`src/common/net/`（`ib/`、`tcp/`、`Transport`、`EventLoop`、`IOWorker`、`Listener`）+ `src/common/serde/`（`CallContext`）+ `src/common/net/RDMAControl` + USRBIO（`src/lib/`、`src/fuse/`）+ storage 資料路徑（`src/storage/`、`src/client/storage/`）。所有引用皆為 `檔案:行號`，可點擊跳轉。
>
> 命名空間統一在 `hf3fs::net`。

---

## 目錄

0. [核心設計哲學](#0-核心設計哲學)
1. [傳輸抽象與位址模型](#1-傳輸抽象與位址模型)
2. [階段一：TCP 連線基礎（控制平面）](#2-階段一tcp-連線基礎控制平面)
3. [階段二：IB 裝置初始化](#3-階段二ib-裝置初始化)
4. [階段三：TCP → RDMA 升級握手（核心）](#4-階段三tcp--rdma-升級握手核心)
5. [階段四：RDMA 記憶體註冊](#5-階段四rdma-記憶體註冊)
6. [階段五：在 RC QP 上模擬 Stream Socket（收發資料路徑）](#6-階段五在-rc-qp-上模擬-stream-socket收發資料路徑)
7. [階段六：單向 RDMA Read/Write（bulk 資料平面）](#7-階段六單向-rdma-readwritebulk-資料平面)
8. [階段七：上層 serde RPC 整合與 bulk 觸發](#8-階段七上層-serde-rpc-整合與-bulk-觸發)
9. [階段八：連線關閉與資源回收](#9-階段八連線關閉與資源回收)
10. [全景資料流：一次完整的讀與寫](#10-全景資料流一次完整的讀與寫)
11. [附錄：參數表 / 訊息類型 / 檔案索引](#11-附錄)

---

## 0. 核心設計哲學

理解 3FS RDMA 的第一把鑰匙：**RDMA 並沒有取代 TCP，而是疊加在 TCP 之上。** 整個系統有清楚的「控制平面 / 資料平面」分離：

| 平面 | 通道 | 承載內容 |
|------|------|----------|
| **控制平面** | 一條普通 TCP 連線上的 serde RPC | RDMA 握手（QP 參數交換）、流控信用申請、小型 RPC 請求/回應 |
| **資料平面** | RC（Reliable Connection）QP | 大型 bulk 資料（檔案 chunk 的讀寫），用 one-sided RDMA Read/Write |

這個分離造就了三個反直覺但優雅的設計：

1. **RDMA 連線的建立完全靠 TCP**：QP number、PSN、LID/GID、MTU 等參數，都是透過一條普通 TCP 連線上的 `IBConnect` RPC 交換的，沒有手刻 socket 交換協議（`Transport.cc:84-98`、`IBConnect.cc:338-448`）。
2. **server 端的 accept 永遠先產生 TCP socket**；RDMA 端點是「對端透過 TCP 發起 `IBConnect` 握手」後才被動補建的（`Listener.cc:170-208`）。
3. **bulk 資料一律由 storage server 端主動發起 RDMA**（client 永遠是被動的記憶體目標）：client 讀 = server 用 `RDMA_WRITE` 把資料推回 client；client 寫 = server 用 `RDMA_READ` 從 client 拉資料（`StorageOperator.cc`、`RDMABuf` agent 驗證）。

全景分層圖：

```
                        ┌─────────────────────────────────────────────┐
   應用 / serde RPC ───▶│ ServiceGroup (network_type 預設 RDMA)         │
                        │   ├─ Listener     監聽 + accept(只產生 TCP)   │
                        │   ├─ IOWorker     I/O 任務分派 + 連線生命週期 │
                        │   │    ├─ EventLoopPool  (N×epoll jthread)    │
                        │   │    └─ TransportPool  (位址→連線池)         │
                        │   └─ Processor    收完整訊息 → serde 分派     │
                        └─────────────────────────────────────────────┘
                                          │ 持有
                                          ▼
            ┌──────────────────────────────────────────────────────┐
            │ Transport : EventLoop::EventHandler                    │
            │   std::unique_ptr<Socket>  ← 多型，TCP/RDMA 共用狀態機 │
            └──────────────────────────────────────────────────────┘
                       │                              │
              ┌────────▼─────────┐          ┌─────────▼──────────────────┐
              │ TcpSocket        │          │ IBSocket                    │
              │  fd + epoll      │          │  RC QP + CQ + comp_channel  │
              │  read/sendmsg    │          │  ├ stream 模擬 (send/recv)  │
              │  (控制平面)       │          │  └ one-sided (rdmaRead/Write)│
              └──────────────────┘          └─────────────────────────────┘
                                                       │ 共用
                                        ┌──────────────▼──────────────┐
                                        │ IBManager (singleton)        │
                                        │   IBDevice[] (一裝置一 PD)   │
                                        │   eventLoop_ (epoll thread)  │
                                        └──────────────────────────────┘
```

---

## 1. 傳輸抽象與位址模型

### 1.1 `Address`：8-byte POD，分流的唯一依據

`Address`（`src/common/utils/Address.h:18-29`）是整個分流系統的根基，刻意設計成可無損 `bit_cast` 成 `uint64_t`（`static_assert(sizeof(Address)==sizeof(uint64_t))`, `Address.h:104`），因此能當 serde 欄位、`map` key：

```cpp
struct Address {
  uint32_t ip{};   // Network Byte Order
  uint16_t port{};
  enum Type : uint16_t { TCP, RDMA, IPoIB, LOCAL, UNIX };
  Type type = Type::TCP;
};
```

分流判斷（`Address.h:31-33`）：

```cpp
bool isTCP()  const { return type == TCP || type == IPoIB || type == LOCAL || type == UNIX; }
bool isRDMA() const { return type == RDMA; }
```

**關鍵：只有 `type == RDMA` 走 IBSocket，其餘四種全走 TcpSocket。** 連 IPoIB（IP over InfiniBand，跑在 IB 硬體上）對 3FS 而言仍是 TCP 通訊端。字串形式為 `RDMA://10.0.0.1:8000`（`Address.h:40-43`）。

### 1.2 `Socket`：純虛多型介面

`Socket`（`src/common/net/Socket.h:14-39`）定義所有連線後端的共同契約，這是 TCP/RDMA 能共用同一套 `Transport` 狀態機的根本：

```cpp
class Socket {
  virtual int fd() const = 0;                                   // epoll 監聽的 fd
  using Events = uint32_t;
  constexpr static auto kEventReadableFlag = (1u << 0);
  constexpr static auto kEventWritableFlag = (1u << 1);
  virtual Result<Events> poll(uint32_t events) = 0;            // 翻譯 epoll events → 可讀/可寫
  virtual Result<size_t> recv(folly::MutableByteRange buf) = 0;
  virtual Result<size_t> send(struct iovec iov[], uint32_t len) = 0;
  virtual Result<Void> flush() = 0;                            // 刷出緩衝（RDMA 才有意義）
  virtual Result<Void> check() = 0;                            // 探活
};
```

兩個實作的對照：

| 虛擬方法 | TcpSocket | IBSocket |
|---------|-----------|----------|
| `fd()` | 原生 socket fd (`TcpSocket.h:21`) | **completion channel fd** `channel_->fd` (`IBSocket.h:134`) |
| `poll()` | EPOLLIN/OUT → 旗標 (`TcpSocket.cc:46`) | poll CQ 處理 work completion (`IBSocket.cc:329`) |
| `recv()` | `::read` 迴圈 (`TcpSocket.cc:60`) | 從 RDMA recv ring 取 (`IBSocket.cc:666`) |
| `send()` | `::sendmsg` (`TcpSocket.cc:97`) | 寫入 send ring + post WR (`IBSocket.cc:596`) |
| `flush()` | no-op (`TcpSocket.h:44`) | 把未滿 buffer 真正 post 出去 (`IBSocket.cc:654`) |
| `check()` | no-op (`TcpSocket.h:47`) | 送空 RDMA_WRITE 探活 (`IBSocket.cc:748`) |

注意：**`connect` 不在共同介面裡**，因為兩者簽章不同——`TcpSocket::connect(Address, Duration)` vs `IBSocket::connect(serde::ClientContext&, Duration)`（後者需要一條 TCP RPC context）。分流由 `Transport::connect()` 用 `dynamic_cast` 處理。

### 1.3 `Transport`：一條連線的封裝與無鎖狀態機

`Transport`（`Transport.h:22`）同時是 `EventLoop::EventHandler`（能掛 epoll）與 `enable_shared_from_this`，內部持有 `std::unique_ptr<Socket> socket_`（`Transport.h:104`）。它用一個 `alignas(hardware_destructive_interference_size) std::atomic<uint32_t> flags_` 做無鎖狀態機（`Transport.h:116`），flag 定義於 `Transport.cc:33-41`：

| Flag | 意義 |
|------|------|
| `kInvalidatedFlag` | 連線已失效 |
| `kReadAvailableFlag` / `kWriteAvailableFlag` | 有讀/寫任務在執行（佔位，保證同時最多一個讀、一個寫任務） |
| `kReadNewWakedFlag` / `kWriteNewWakedFlag` | epoll 又通知了新事件 |
| `kWriteHasMsgFlag` / `kWriteNewMsgFlag` | 有待寫訊息 |
| `kLastReadFinished` / `kLastWriteFinished` | 最後一個讀/寫任務結束 |

**socket 工廠（分流點，主動端）** `Transport::create(Address, IOWorker&)`（`Transport.cc:75-82`）：

```cpp
std::shared_ptr<Transport> Transport::create(Address addr, IOWorker &io_worker) {
  if (addr.isTCP()) {
    return create(std::make_unique<TcpSocket>(), io_worker, addr);
  } else if (addr.isRDMA()) {
    return create(std::make_unique<IBSocket>(io_worker.config_.ibsocket()), io_worker, addr);
  }
  return nullptr;
}
```

---

## 2. 階段一：TCP 連線基礎（控制平面）

> 這一層是「RDMA 升級之前」必須存在的底層連線。RDMA 的握手與流控都跑在它之上。

### 2.1 監聽：哪些網卡算 TCP / RDMA / IPoIB

`Listener::setup()`（`Listener.cc:62-116`）列舉本機網卡並依名稱前綴過濾。核心判斷在 `checkNicType()`（`Listener.cc:33-49`）：

```cpp
static bool checkNicType(std::string_view nic, Address::Type type, std::string_view tcp_nic_custom_prefix) {
  switch (type) {
    case Address::TCP:
    case Address::RDMA:   // ← RDMA 與 TCP 共用同一 case！
      return nic.starts_with("en") || nic.starts_with("eth") || nic.starts_with("bond") ||
             nic.starts_with("xgbe") || (!tcp_nic_custom_prefix.empty() && nic.starts_with(tcp_nic_custom_prefix));
    case Address::IPoIB:
      return nic.starts_with("ib");
    case Address::LOCAL:
      return nic.starts_with("lo");
    default: return false;
  }
}
```

**鐵證之一**：`Address::TCP` 與 `Address::RDMA` 走同一個 case，都篩選乙太網卡。這證實「即使 network type 設為 RDMA，仍需要 TCP 網路來建立連線與握手」。

`network_type` 的源頭是 `ServiceGroup::Config`（`ServiceGroup.h:25`）：

```cpp
CONFIG_ITEM(network_type, Address::RDMA);   // 預設就是 RDMA
```

### 2.2 accept 迴圈：永遠先當 TCP

`Listener::start()`（`Listener.cc:118-145`）在 `networkType_ == RDMA` 時，會建立一個 `IBConnectService` 並**強制以 `Address::Type::TCP` 註冊**為普通 serde service（`Listener.cc:124-126`）：

```cpp
auto accept = [this](auto socket) { acceptRDMA(std::move(socket)); };
auto service = std::make_unique<IBConnectService>(ibconfig_, accept, config_.rdma_accept_timeout_getter());
group.addSerdeService(std::move(service), Address::Type::TCP);   // ← RDMA 握手服務跑在 TCP 上
```

而 accept 迴圈 `Listener::listen()`（`Listener.cc:170-186`）**只接受 TCP 連線**，一律先 `acceptTCP()`：

```cpp
while (true) {
  auto result = co_await co_awaitTry(socket.accept());   // 只接受 TCP
  ...
  co_await acceptTCP(std::move(result.value()));         // 一律先當 TCP 處理
}
```

那 IBSocket 從哪來？由 `IBConnectService` 的 callback 經 `Listener::acceptRDMA()`（`Listener.cc:198-208`）補建，並啟動 `checkRDMA()`（`Listener.cc:210-226`）守門：睡 `rdma_accept_timeout`（預設 15s）後若該 IBSocket 仍停在 ACCEPTED 狀態就 `invalidate()` 丟棄，避免半開連線堆積。

### 2.3 EventLoop：epoll 邊緣觸發 + 每迴圈一條 jthread

`EventLoop::start()`（`EventLoop.cc:13-41`）：`epoll_create` + `eventfd`(喚醒哨兵，`data.ptr=nullptr` 識別) + `std::jthread` 跑 `loop()`。

`EventLoop::loop()`（`EventLoop.cc:115-160`）：`epoll_wait` 一次最多取 64 個事件；`event.data.ptr` 指向 `HandlerWrapper`（內含 `weak_ptr<EventHandler>`），事件觸發時先 `lock()` 確認物件存活，再把**原始 `epoll_event.events` 透傳給 `handler->handleEvents()`**——EventLoop 本身不解析讀寫，翻譯工作交給 `Transport::handleEvents()` → `Socket::poll()`。

設計重點：
- **weak_ptr 持有 handler**，EventLoop 不延長 Transport 生命週期。
- **延遲刪除**（`EventLoop.cc:92-113`）：`remove()` 立刻 `epoll_ctl(DEL)`，但 `HandlerWrapper` 的真正刪除丟進 `deleteQueue_`，統一回到 loop 執行緒做，避免「正在回呼」與「正在刪除」競爭。
- `EventLoopPool::add()`（`EventLoop.cc:183-186`）用 **`folly::Random::rand32() % size`** 隨機把連線分配到某條 epoll 執行緒。每個 IOWorker 預設 `num_event_loop=1`（`IOWorker.h:31`）。

### 2.4 TcpSocket：非阻塞 I/O

- `connect()`（`TcpSocket.cc:21-44`）：用 folly `coro::Transport::newConnectedSocket` 非同步建連，成功後抽出原生 fd 自持，再 `init()`。
- `init()`（`TcpSocket.cc:128-159`）：設 `O_NONBLOCK` + `close-on-exec`，`getpeername` 填 `peerAddr_`。
- `recv()`（`TcpSocket.cc:60-81`）：迴圈 `::read` 直到填滿或 `EAGAIN`（返回已讀量）。
- `send()`（`TcpSocket.cc:97-126`）：`::sendmsg` + `MSG_NOSIGNAL` 一次送多個 iovec（scatter-gather），部分寫入用 `updateIOVec()` 推進。
- `poll()`（`TcpSocket.cc:46-58`）：把 `EPOLLIN/OUT/ERR/HUP` 翻譯成 `kEventReadableFlag/kEventWritableFlag`。

### 2.5 IOWorker：執行緒模型與讀寫分派

`IOWorker` 串接 EventLoop（事件來源）、TransportPool（連線池）、Processor（上層分派）與三類執行資源（`IOWorker.h:38-56`）：

| 資源 | 來源 | 用途 |
|------|------|------|
| `executor_` (CPUExecutorGroup) | `tpg.ioThreadPool()` | 跑 `doRead`/`doWrite` 的 CPU 池 |
| `connExecutor_` (IOThreadPoolExecutor) | `tpg.connThreadPool()` | 跑 connect/retry coroutine |
| `eventLoopPool_` | `num_event_loop`(預設1) | epoll 事件監聽 |

讀寫分派 `startReadTask`/`startWriteTask`（`IOWorker.cc:118-136`）：**預設讀寫丟到 io CPU 池**（`read_write_tcp_in_event_thread`/`read_write_rdma_in_event_thread` 預設 false），epoll 執行緒只負責偵測事件 + 分派，避免阻塞事件迴圈。

`Transport::handleEvents()`（`Transport.cc:344-373`）是事件驅動讀寫的樞紐：

```cpp
auto results = socket_->poll(epollEvents);                 // 1. 多型翻譯 epoll events
Socket::Events events = error ? (READ|WRITE) : results.value();
auto flags = flags_.fetch_or(mask);                        // 2. 原子置位
if (doRead  && (flags & kReadAvailableFlag) == 0)
  ioWorker_.startReadTask(this, error);                    // 3. 沒讀任務就啟一個
if (doWrite && (flags & kWriteAvailableFlag) == 0 && (flags & kWriteHasMsgFlag))
  ioWorker_.startWriteTask(this, error);
```

實際讀（`Transport.cc:185-252`）迴圈 `socket_->recv()` 把 buffer 拼成完整訊息（依 `MessageWrapper` 的訊息邊界，見 §8.1）交給 `Processor`；寫（`Transport.cc:254-325`）從 send queue 取 iovec 迴圈 `socket_->send()`，一次最多 64 個 iovec。**讀寫實作完全透過 `socket_->recv/send/poll/flush` 多型介面，對 TCP/RDMA 無差別。**

---

## 3. 階段二：IB 裝置初始化

> 在任何 RDMA 連線建立之前，`IBManager` 必須先完成裝置初始化。這是 per-process 一次性的長壽命資源。

### 3.1 三層職責劃分

| 類別 | 角色 | 持有的 verbs 資源 |
|------|------|------------------|
| `IBManager` | **單例總控**：生命週期、事件迴圈、zone 路由表 | `ibv_fork_init`(全域一次)、`EventLoop`、`IBSocketManager` |
| `IBDevice` | **單一 HCA 封裝** | `ibv_context`、`ibv_pd`（**一裝置一 PD**） |
| `IBPort` | **單一 port 的快照值物件** | 不持資源，持 `ibv_port_attr` + RoCEv2 GID 副本 |

**關鍵邊界**：`IBDevice` 層**不建立 CQ/QP/comp channel**——那些是 per-connection 資源，由 `IBSocket::qpCreate()` 建立（見 §4.4）。

### 3.2 初始化序列：`IBManager::startImpl`（IBDevice.cc:756-820）

```
1. 冪等檢查 (inited_)
2. ibv_fork_init()           ← 全域一次，受 config.fork_safe()(預設 true) 控制
3. IBDevice::openAll()       ← 枚舉並開啟所有裝置
4. EventLoop::create().start("IBManager")   ← 一條專屬 epoll 執行緒
5. 建立 IBSocketManager      ← socket 優雅排空 (EPOLLIN|OUT|ET)
6. 建立 BackgroundRunner     ← timerfd 定時刷新 port 屬性
7. 每裝置註冊 AsyncEventHandler + 建 zone→port 路由表 zone2port_
8. inited_ = true
```

### 3.3 裝置發現：`IBDevice::openAll` / `open`

`openAll`（`IBDevice.cc:299-356`）：

```cpp
auto deviceList = ibv_get_device_list(&deviceCnt);   // 枚舉 RDMA 裝置
SCOPE_EXIT { ibv_free_device_list(deviceList); };     // RAII
// 逐一 IBDevice::open()，裝置數上限 kMaxDeviceCnt = 4
```

`open`（`IBDevice.cc:358-459`）集中三個核心 verbs 呼叫：

```cpp
device->context_.reset(ibv_open_device(dev));           // ① 開裝置
device->pd_.reset(ibv_alloc_pd(device->context_.get())); // ② 配置唯一的 PD
ibv_query_device(device->context_.get(), &device->attr_);// ③ 查裝置屬性
```

`ibv_device_attr` 存入 `attr_`，後續用來夾住 QP 配置上限（`max_sge`/`max_qp_wr`/`max_cqe`/`max_qp_rd_atom`，見 `IBConnect.cc:531-535`）。

值得一提的工程細節：`ibdev2netdev` 映射**刻意不依賴命令列工具**，改為直接掃描 `/sys/class/infiniband`（`IBDevice.cc:190-277`），容器環境缺 sysfs 時降級為空 map。

### 3.4 一裝置一 PD

PD 在 `IBDevice.cc:371` 配置，被該裝置上**所有 socket 的 QP 與所有 MR 共用**：
- 記憶體註冊：`ibv_reg_mr(pd_.get(), ...)`（`IBDevice.cc:594`）
- QP 建立（跨層）：`ibv_create_qp(device()->pd(), ...)`（`IBSocket.cc:594`）

### 3.5 Port 與 GID：RoCEv2 GID 的動態探測

`ibv_query_port`（`IBDevice.cc:403-456`）對每個 phys port 查詢，過濾掉非 active port、非 IB/Ethernet link layer。port 屬性以 `folly::Synchronized<ibv_port_attr>` 包裹（執行緒安全，因背景會更新）。

最精巧的是 GID 處理（`IBDevice.cc:94-138`）。由於舊版 rdma-core 沒有 `ibv_query_gid_ex`，作者自行**遍歷 GID index 0..31 + 讀 sysfs 判斷 RoutingType**：

```cpp
ibv_query_gid(ctx, portNum, index, &gid);                // 逐一查 GID
// 排除空槽 / 全零 / link-local (RoCEv1 fe80::)
auto path = ".../ports/{}/gid_attrs/types/{}";           // 讀 sysfs
folly::readFile(path, gidType);
if (!gidType.starts_with("RoCE v2")) continue;           // 只接受 RoCE v2
return std::pair<ibv_gid, uint8_t>(gid, index);          // 回傳 (GID, gid_index)
```

**因此 `gid_index` 是動態探測而非設定值**（對應 `IBSocket.cc:148` 與 `IBDevice.h:76` 兩處被刻意註解掉的 `default_gid_index`）。

### 3.6 雙重 port 刷新 + 無 busy-poll

- **被動**：`AsyncEventHandler`（`IBDevice.cc:678-726`）監聽 `context->async_fd`，處理 `IBV_EVENT_PORT_ERR/ACTIVE/LID_CHANGE/GID_CHANGE/PKEY_CHANGE/SM_CHANGE` 等，觸發 `updatePort`。
- **主動**：`BackgroundRunner`（`IBDevice.cc:631-676`）用 timerfd（Release 15s / Debug 5s）週期 re-query 所有 port。

**整個系統沒有 busy-poll 的 CQ 輪詢執行緒**：CQ completion 走 completion-channel fd + 共用 epoll（`IBManager::eventLoop_`）事件驅動。

---

## 4. 階段三：TCP → RDMA 升級握手（核心）

這是報告的第一個核心。整個握手是一個跑在 TCP 上的 serde RPC 對話，配合 QP 狀態機遷移。

### 4.1 升級觸發點

當上層要對一個 `RDMA://` 位址發送資料時，`TransportPool` 找不到現成連線就建立一個持有 `IBSocket` 的 `Transport`，接著 `Transport::connect()`（`Transport.cc:84-98`）進入 RDMA 分支：

```cpp
if (addr.isRDMA()) {
  auto tcpAddr = Address(addr.ip, addr.port, Address::TCP);    // 同 IP/port 的 TCP 位址
  auto tcpCtx = serde::ClientContext(ioWorker_, tcpAddr, connectOptions);
  auto ibSocket = dynamic_cast<IBSocket *>(socket_.get());
  co_return co_await ibSocket->connect(tcpCtx, timeout);       // 把 TCP RPC context 交給 IBSocket
}
```

**鐵證**：RDMA 連線建立時，內部先用相同 IP/port 開一條 TCP 的 `serde::ClientContext`，握手就在這條 TCP 上進行。

### 4.2 `IBConnect` 服務定義

握手協議是一個正規的 serde 服務（`IBConnectService.h:18-21`）：

```cpp
SERDE_SERVICE(IBConnect, 11) {                  // serviceId = 11
  SERDE_SERVICE_METHOD(query,   1, IBQueryReq,   IBQueryRsp);
  SERDE_SERVICE_METHOD(connect, 2, IBConnectReq, IBConnectRsp);
};
```

server 端兩個方法都**明確要求 `ctx.transport()->isTCP()`**（`IBConnect.cc:100-103`、`143-146`），確保握手只在 TCP 上發生。

### 4.3 握手交換的資料結構

`IBConnectInfo`（`IBConnect.h:123-137`）是雙方交換的核心資訊：

```cpp
struct IBConnectInfo {
  std::string hostname;
  uint8_t dev; std::string dev_name; uint8_t port;
  uint8_t  mtu;
  uint32_t qp_num;                                                    // 本端 QP number
  std::variant<IBConnectIBInfo, IBConnectRoCEInfo> linklayer;         // IB:lid / RoCE:gid
};
```

- `IBConnectIBInfo{ uint16_t lid }`（`IBConnect.h:109-114`）：InfiniBand 用 LID 定址。
- `IBConnectRoCEInfo{ ibv_gid gid }`（`IBConnect.h:116-121`）：RoCE 用 GID 定址（`ibv_gid` 有自訂 serde，`IBConnect.h:172-186`）。

`IBConnectConfig`（`IBConnect.h:75-107`）攜帶所有 QP 協商參數（sl/traffic_class/pkey_index/start_psn/min_rnr_timer/timeout/retry_cnt/rnr_retry/max_sge/max_rdma_wr/max_rd_atomic/buf_size/send_buf_cnt/buf_ack_batch/buf_signal_batch/...），並內建 QP 容量計算：

```cpp
uint32_t qpMaxSendWR() const { return send_buf_cnt + qpAckBufs() + max_rdma_wr + 1/*close*/ + 1/*check*/; }
uint32_t qpMaxRecvWR() const { return send_buf_cnt + qpAckBufs() + 1/*close*/; }
uint32_t qpMaxCQEntries() const { return qpMaxSendWR() + qpMaxRecvWR(); }
```

### 4.4 client 端完整流程：`IBSocket::connect`（IBConnect.cc:338-448）

```
┌─ 1. IBConnect::query (TCP RPC) ──────────────────────────────┐
│    server 回 IBQueryRsp：列出所有 active 裝置/port/zone/link_layer│
└──────────────────────────────────────────────────────────────┘
   2. selectDevice(counter)        ← 本地↔遠端裝置配對（見 §4.6）
   3. openPort + checkPort
   4. toIBConnectConfig + checkConfig
   5. qpCreate()                   ← comp_channel + CQ + RC QP + buffer MR
   6. qpInit()                     ← QP: RESET → INIT
   7. state = CONNECTING
   8. getConnectInfo()             ← 打包本端 qp_num/mtu/lid或gid
┌─ 9. IBConnect::connect (TCP RPC) ────────────────────────────┐
│    送 IBConnectReq(本端info + target裝置 + config)            │
│    server 回 IBConnectRsp(server 的 info)                     │
└──────────────────────────────────────────────────────────────┘
   10. setPeerInfo()
   11. qpReadyToRecv()             ← QP: INIT → RTR（填對端 qp_num/lid/gid/mtu）
   12. qpReadyToSend()             ← QP: RTR → RTS
   13. postConnectProbe()          ← 送 SEND_WITH_IMM, ImmData=ACK(0)
```

### 4.5 QP 狀態機遷移（ibv_modify_qp 序列）

這是 RDMA 編程的精髓。四個遷移函式：

**`qpCreate()`（IBSocket.cc:542-601）— 建立資源**

```cpp
channel_.reset(ibv_create_comp_channel(device()->context()));   // 每 socket 一個 comp channel
fcntl(channel_->fd, F_SETFL, O_NONBLOCK);                       // ← fd 設非阻塞，供 epoll ET
cq_.reset(ibv_create_cq(device()->context(), qpMaxCQEntries(),
                        nullptr, channel_.get(), 0/*comp vector*/));
ibv_req_notify_cq(cq_.get(), 0);                               // 啟用完成通知

ibv_qp_init_attr attr{};
attr.send_cq = attr.recv_cq = cq_.get();   // send/recv 共用同一 CQ
attr.cap.max_send_wr = qpMaxSendWR();
attr.cap.max_recv_wr = qpMaxRecvWR();
attr.cap.max_send_sge = max_sge;  attr.cap.max_recv_sge = 1;
attr.qp_type = IBV_QPT_RC;                 // Reliable Connection
attr.sq_sig_all = 0;                       // 不是每個 WR 都產生 CQE（配合 signal batching）
qp_.reset(ibv_create_qp(device()->pd(), &attr));
initBufs();                                // 分配並註冊 send/recv ring buffer
```

**`qpInit()`（IBSocket.cc:603-620）— RESET → INIT**

```cpp
attr.qp_state = IBV_QPS_INIT;
attr.qp_access_flags = IBV_ACCESS_REMOTE_WRITE | IBV_ACCESS_REMOTE_READ;  // 允許對端 RDMA 讀寫
attr.pkey_index = connectConfig_.pkey_index;
attr.port_num = port_.portNum();
ibv_modify_qp(qp_, &attr, IBV_QP_STATE|IBV_QP_PKEY_INDEX|IBV_QP_PORT|IBV_QP_ACCESS_FLAGS);
```

**`qpReadyToRecv()`（IBSocket.cc:622-692）— INIT → RTR**（用對端資訊）

```cpp
attr.qp_state           = IBV_QPS_RTR;
attr.path_mtu           = std::min(本端 active_mtu, 對端 mtu);   // 取較小值
attr.dest_qp_num        = peerInfo_.qp_num;                      // 對端 QP number
attr.rq_psn             = connectConfig_.start_psn;
attr.max_dest_rd_atomic = connectConfig_.max_rd_atomic;
attr.min_rnr_timer      = connectConfig_.min_rnr_timer;

if (InfiniBand) {                          // LID 定址，不需 GRH
  attr.ah_attr.is_global = 0;
  attr.ah_attr.dlid      = peer.lid;
} else if (RoCE) {                          // 必須用 global routing (GRH)
  attr.ah_attr.is_global         = 1;
  attr.ah_attr.grh.dgid          = peer.gid;
  attr.ah_attr.grh.sgid_index    = port_.getRoCEv2Gid().second;  // ← 本地動態探測的 gid_index
  attr.ah_attr.grh.hop_limit     = 255;
  attr.ah_attr.grh.traffic_class = connectConfig_.traffic_class;
}
attr.ah_attr.sl = sl;  attr.ah_attr.port_num = peerInfo_.port;
ibv_modify_qp(qp_, &attr, IBV_QP_STATE|IBV_QP_AV|IBV_QP_PATH_MTU|IBV_QP_DEST_QPN|
                          IBV_QP_RQ_PSN|IBV_QP_MAX_DEST_RD_ATOMIC|IBV_QP_MIN_RNR_TIMER);
// RTR 後立刻 post 所有 recv buffer
for (idx in 0..recvBufs_.getBufCnt()) postRecv(idx);
```

**`qpReadyToSend()`（IBSocket.cc:694-722）— RTR → RTS**

```cpp
attr.qp_state     = IBV_QPS_RTS;
attr.timeout      = connectConfig_.timeout;     // 預設 14
attr.retry_cnt    = connectConfig_.retry_cnt;   // 預設 7
attr.rnr_retry    = connectConfig_.rnr_retry;
attr.sq_psn       = connectConfig_.start_psn;
attr.max_rd_atomic= connectConfig_.max_rd_atomic;
ibv_modify_qp(qp_, &attr, IBV_QP_STATE|IBV_QP_TIMEOUT|IBV_QP_RETRY_CNT|
                          IBV_QP_RNR_RETRY|IBV_QP_SQ_PSN|IBV_QP_MAX_QP_RD_ATOMIC);
// 一進 RTS 就初始化流控信用：
sendBufs_.push(sendBufs_.getBufCnt());           // 送出端 buffer 全部可用
rdmaSem_.signal(connectConfig_.max_rdma_wr);     // RDMA WR 信用 = 128
ackBufAvailable_.store(connectConfig_.qpAckBufs()); // ACK buffer 信用
```

### 4.6 裝置配對：`selectDevice`（IBConnect.cc:262-297）

當雙方各有多張 HCA 時，要選一對能互通的 (本地裝置:port, 遠端裝置:port)。演算法（`findMatchDevices`, `IBConnect.cc:224-260`）：

1. 對每組 (遠端 port × 本地 port)，檢查 `link_layer` 一致（IB↔IB 或 RoCE↔RoCE）、本地 port active。
2. 計算雙方 **zone 交集**（`std::set_intersection`）；`checkZone` 開啟時 zone 無交集則跳過。
3. 命中的分成 `ibMatches` 與 `roceMatches`。
4. 用 round-robin counter 選一個（`prefer_ibdevice` 時偏好 IB）；找不到時若 `allow_unknown_zone` 則放寬 zone 再試一輪。

counter 來自 per-address 的 round-robin（`IBConnect.cc:374-383`），達成多卡負載均衡。

### 4.7 非對稱握手：為什麼 server 只到 RTR

這是整個握手最巧妙的地方。**client 與 server 的 QP 推進是不對稱的**：

| 步驟 | Client (`IBSocket::connect`) | Server (`IBSocket::accept`, IBConnect.cc:450-499) |
|------|------------------------------|---------------------------------------------------|
| QP 建立 | qpCreate → qpInit (INIT) | qpCreate → qpInit (INIT) |
| → RTR | qpReadyToRecv | qpReadyToRecv |
| → RTS | **qpReadyToSend** ✅ | ❌ **不做**（停在 RTR + ACCEPTED 狀態） |
| 觸發 READY | postConnectProbe 發第一個 msg | **收到 client 的 probe 後**才 qpReadyToSend → READY |

server 端 `accept` 結束時 `state = ACCEPTED`（`IBConnect.cc:487`），只到 RTR。它的轉 RTS 發生在資料面——`onRecved`（`IBSocket.cc:517-549`）收到第一個訊息時：

```cpp
bool isConnectMsg = (state_.load() == State::ACCEPTED);
if (isConnectMsg) {
  // 收到對端第一個 msg 才 RTR → RTS
  if (qpReadyToSend() != 0) return -1;
  state_ = State::READY;
}
```

**為什麼這樣設計？** RTR 後 QP 已能「收」，但要能「發」必須到 RTS。server 在還沒確認 client 的 QP 真的進入可收狀態前，貿然轉 RTS 並發送會踩到 RNR（Receiver Not Ready）。讓 client 先發一個 connect probe，server 收到才轉 RTS，保證了「對端已就緒」這個前提。同時 client 端 `onSended`（`IBSocket.cc:505-515`）在 probe 送出完成時把自己從 CONNECTING 轉 READY。

connect probe（`postConnectProbe`, `IBSocket.cc:909-929`）刻意用 `IBV_WR_SEND_WITH_IMM` 帶 `ImmData::ack(0)`，`sg_list=nullptr`（不佔 send buffer），ACK(0) 讓對端 `sendAcked_ += 0` 保持向後相容，也能在虛擬化 RDMA 環境下正常運作。

### 4.8 完整狀態機

```
          qpCreate+qpInit         IBConnect::connect RPC + RTR + RTS
  INIT ───────────────▶ CONNECTING ──────────────────────────────┐
   │                        │ (client: probe 送出完成 onSended)    │
   │                        ▼                                       │
   │                     READY ◀──────────────────────────────────┘
   │  (server)
   ├── qpCreate+qpInit+RTR ──▶ ACCEPTED ──(收到 client probe onRecved→RTS)──▶ READY
   │
   └─────────────────────────────────────────────▶ CLOSE / ERROR
```

State 定義於 `IBSocket.h:236-243`：`INIT, CONNECTING, ACCEPTED, READY, CLOSE, ERROR`。

---

## 5. 階段四：RDMA 記憶體註冊

> RDMA 的硬性前提：任何被 NIC 存取的記憶體都必須先 `ibv_reg_mr` 釘住（pin）並取得 lkey/rkey。

### 5.1 唯一註冊入口與 access flags

全專案只有一處 `ibv_reg_mr`，封裝在 `IBDevice::regMemory`（`IBDevice.cc:592-613`），並掛上監控指標（MR 數量/大小/註冊延遲）。

資料平面 buffer 的 access flags（`RDMABuf.h:243-244`）：

```cpp
static constexpr int kAccessFlags =
    IBV_ACCESS_LOCAL_WRITE | IBV_ACCESS_REMOTE_WRITE | IBV_ACCESS_REMOTE_READ | IBV_ACCESS_RELAXED_ORDERING;
```

- `LOCAL_WRITE`：RDMA Read 把資料寫回本地 buffer 時必需。
- `REMOTE_WRITE + REMOTE_READ`：同一塊 buffer 在讀/寫兩種 IO 中扮演不同角色，兩者都開。
- `RELAXED_ORDERING`：放寬 PCIe 排序提升吞吐。

控制平面（IBSocket 的 send/recv ring）用另一組 flag（`IBV_ACCESS_LOCAL_WRITE | IBV_ACCESS_RELAXED_ORDERING`），由 `initBufs` 傳入（`IBConnect.cc:730-731`）——因為 ring buffer 只供本地 send/recv，不需開放遠端存取。

### 5.2 多裝置全註冊（多 HCA 支援的關鍵）

`RDMABuf::Inner::registerMemory`（`RDMABuf.cc:107-127`）對 `IBDevice::all()`（上限 4）**每個裝置各註冊一次**，存成以 devId 為索引的陣列：

```cpp
for (auto &dev : IBDevice::all()) {
  auto mr = dev->regMemory(ptr_, capacity_, kAccessFlags);
  mrs_[dev->id()] = mr;     // std::array<ibv_mr*, kMaxDeviceCnt>
}
```

**為何？** 連線最終走哪張卡在註冊當下未知；多卡環境下對端可能透過任一裝置存取，所以對所有裝置預先註冊，事後按連線實際裝置挑對應的 lkey/rkey：

- **lkey**（本地，組 SGE 用）：`buf.getMR(port_.dev()->id())->lkey`（`IBSocket.cc:1062-1072`）。
- **rkey**（遠端描述子）：`mr->rkey` 蒐集成陣列，發 WR 時 `remoteBuf.getRkey(peerInfo_.dev)`（`IBSocket.cc:260`）線性比對 devId 取出。

### 5.3 三層 buffer 抽象

| 類別 | 職責 | 關鍵成員 |
|------|------|----------|
| `RDMABuf` (`RDMABuf.h:138-316`) | 對已註冊記憶體的**可切片視窗**（view），可自由拷貝 | `shared_ptr<Inner> buf_; uint8_t* begin_; size_t length_` |
| `RDMABuf::Inner` (`RDMABuf.h:240-291`) | 真正持有記憶體 + 每裝置 MR 陣列 | `uint8_t* ptr_; size_t capacity_; array<ibv_mr*,4> mrs_; bool userBuffer_` |
| `RDMABufPool` (`RDMABuf.h:319-349`) | semaphore 限總量 + free-list 重用 `Inner` | `Semaphore sem_; deque<Inner*> freeList_` |

記憶體來源**二分**：
- **server / 控制平面**：`posix_memalign`（頁對齊、**非大頁、非 shm**，`RDMABuf.cc:93-105` + `GlobalMemoryAllocator.h:32-37`）。
- **client USRBIO Iov**：`shm_open + mmap(MAP_SHARED)`（跨行程零拷貝，見 §8.5），透過 `createFromUserBuffer`（`userBuffer_=true`，只註冊不分配）走同一個 `ibv_reg_mr`。

### 5.4 `RDMARemoteBuf`：遠端描述子與序列化

`RDMARemoteBuf`（`RDMABuf.h:40-134`）是「給對端做 RDMA 的目標描述子」，只含 addr + length + 每裝置 rkey：

```cpp
class RDMARemoteBuf {
  uint64_t addr_;
  uint64_t length_;
  std::array<Rkey, kMaxDeviceCnt> rkeys_;   // Rkey{ uint32_t rkey; int devId; }
};
```

從本地 RDMABuf 產生（`RDMABuf.h:220-228`）：`toRemoteBuf()` 用**當前視窗起點 `begin_`** 當 addr（切片後轉描述子仍正確對齊）。

序列化（`RDMABuf.h:354-395`）採緊湊格式：只寫有效的 rkey（反序）+ len + size + addr。它直接內嵌在 storage 的讀寫請求（`fbs/storage/Common.h:309-335`）：

```cpp
struct ReadIO {  ... net::RDMARemoteBuf rdmabuf; };          // client 讀目標
struct UpdateIO { ... net::RDMARemoteBuf rdmabuf;
                      UInt8Vector inlinebuf; };               // 寫；小寫入可走 inline fallback
```

### 5.5 避免重複註冊：沒有位址型 MR 快取

3FS **沒有**「按位址查找避免重複註冊」的全域 MR cache，而是靠兩種長壽命策略：
- **RDMABufPool free-list**（`RDMABuf.cc:146-175`）：歸還的 `Inner` 不解構、不反註冊，放回 free-list，下次 `allocate` 直接重用（MR 還在）。
- **storage BufferPool**（`BufferPool.cc:53-94`）：啟動時一次預配大塊 RDMABuf（`big_rdmabuf_size` 64MB / `rdmabuf_size` 4MB），之後只在已註冊記憶體內切片；同時建一份 `iovec` 陣列供 io_uring/libaio 以 fixed buffer 直寫磁碟，達成**磁碟↔RDMA 同一塊記憶體零拷貝**。

---

## 6. 階段五：在 RC QP 上模擬 Stream Socket（收發資料路徑）

> 第二個核心。RC QP 是**訊息導向**且**需要對端預備 recv buffer**，但上層 serde 需要的是 TCP 般的 byte stream。`IBSocket` 用 ring buffer + credit 流控 + signal batching + immediate-data 控制信號補上這個語意鴻溝。

### 6.1 緩衝區結構

`initBufs`（`IBConnect.cc:724-741`）分配一整塊記憶體註冊一個 MR，切成 send + recv 兩段：
- `sendBufs_`（`SendBuffers`）：`send_buf_cnt`（預設 32）個 `buf_size`（預設 16KB）的環形 buffer。
- `recvBufs_`（`RecvBuffers`）：`qpMaxRecvWR()` 個 buffer，用 `folly::MPMCQueue` 管理已收訊息。

### 6.2 發送路徑

`send(buf)`（`IBSocket.cc:611-652`）：

```cpp
while (!sendBufs_.empty() && !buf.empty()) {
  auto [idx, sendBuf] = sendBufs_.front();
  size_t wsize = min(buf.size(), sendBuf.size());
  memcpy(sendBuf.data(), buf.data(), wsize);     // 拷貝進註冊好的 ring buffer
  sendBuf.advance(wsize);  buf.advance(wsize);
  if (sendBuf.empty()) {                          // 一個 buffer 填滿
    sendBufs_.pop();
    postSend(idx, getBufSize());                  // post IBV_WR_SEND
  }
}
// 若 sendBufs_ 空了還沒送完 → 記錄「等 buffer」時間，返回部分量（上層稍後重試）
```

`postSend`（`IBSocket.cc:836-864`）+ **signal batching**：

```cpp
ibv_sge sge{ .addr=(uint64_t)getBuf(idx), .length=len, .lkey=getMr()->lkey };
uint32_t signal = 0;
if (++sendNotSignaled_ >= buf_signal_batch) {    // 每 8 個 WR 才 signal 一次
  signal = sendNotSignaled_;  sendNotSignaled_ = 0;
  flags |= IBV_SEND_SIGNALED;                    // 只有這個 WR 產生 CQE
}
ibv_send_wr wr{ .wr_id=WRId::send(signal), .sg_list=&sge, .num_sge=1, .opcode=IBV_WR_SEND, .send_flags=flags };
ibv_post_send(qp_, &wr, &badWr);
```

因 `sq_sig_all=0`，不是每個 WR 都產生 CQE。每 `buf_signal_batch` 個才設一次 `IBV_SEND_SIGNALED`，`WRId::send(signalCount)` 記下這次 signal 涵蓋幾個 WR，完成時一次補回（大幅降低 CQE 處理開銷）。`flush()`（`IBSocket.cc:654-664`）把未滿的 buffer 也 post 出去。

### 6.3 接收路徑

RTR 後預先 post 所有 recv buffer（`IBConnect.cc:685-689`）。`onRecved`（`IBSocket.cc:517-549`）把收到的訊息 `push` 進 `recvBufs_` 並標記 readable。`recv(buf)`（`IBSocket.cc:666-706`）從 `recvBufs_` memcpy 出來，用完的 buffer 重新 `postRecv`，並驅動 ACK 流控。

### 6.4 Credit-based 流控（核心機制）

RC send 若對端沒有 recv buffer 會觸發 RNR。3FS 用**信用機制**避免：

```
發送端                                          接收端
sendBufs_ (32 credits)
   │ send → postSend(IBV_WR_SEND) ──────────▶ onRecved: push 到 recvBufs_
   │                                              │ recv() 消費 buffer
   │                                              │ 每消費 buf_ack_batch(8) 個：
   │                                              ▼ postAck()
   │ onImmData(ACK):                          IBV_WR_SEND_WITH_IMM
   │   sendAcked_ += imm.data() ◀───────────── imm = ImmData::ack(8)
   ▼ poll(): 補回 sendBufs_
   sendBufs_.push(min(sendAcked_, sendSignaled_))
```

關鍵在 `poll()`（`IBSocket.cc:346-353`）：buffer 要同時滿足「本地 signal 確認送出」（`sendSignaled_`）與「對端 ACK 確認收到」（`sendAcked_`）才能釋放：

```cpp
auto sendAvailable = std::min(sendAcked_, sendSignaled_);
if (sendAvailable) {
  sendBufs_.push(sendAvailable);                 // 補回可用 buffer
  sendAcked_ -= sendAvailable;  sendSignaled_ -= sendAvailable;
  events |= kEventWritableFlag;                  // 通知上層可再寫
}
```

ACK 本身也要佔一個 send WR slot，所以也有信用 `ackBufAvailable_`（`onAckSended`/`recv` 中增減，`IBSocket.cc:551-558`、`688-696`）。

### 6.5 `ImmData`：用 immediate data 傳控制信號

`ImmData`（`IBSocket.h:339-378`）把 32-bit immediate data 切成 **8-bit type + 24-bit value**：

```cpp
enum class Type : uint8_t { ACK, CLOSE };
static ImmData ack(uint32_t count) { return create(Type::ACK, count); }   // ACK 帶 credit 數
static ImmData close()             { return create(Type::CLOSE, 0); }     // 關閉通知
```

`onImmData`（`IBSocket.cc:566-583`）：ACK → `sendAcked_ += data`；CLOSE → `state = CLOSE`。

### 6.6 CQ 事件驅動 polling

`IBSocket::poll()`（`IBSocket.cc:329-362`）由 EventLoop 在 `channel_->fd` 可讀時呼叫，是經典的「不漏事件」模式：

```cpp
cqGetEvent();      // 1. ibv_get_cq_event 從 comp channel 取事件（event_ack_batch 批次 ack）
cqPoll(events);    // 2. ibv_poll_cq 批次處理（一次 16 個 WC）
cqRequestNotify(); // 3. ibv_req_notify_cq 重新武裝通知
cqPoll(events);    // 4. 再 poll 一次 ← 防止 req_notify 前後的空窗期漏掉 WC
// 5. 補回 send buffer（§6.4）；6. 檢查 QP error
```

`cqPoll`（`IBSocket.cc:364-389`）批次 16 個 WC，每個交給 `wcSuccess`/`wcError` 分派。`wcSuccess`（`IBSocket.cc:477-503`）按 `WRId` 的 type 路由到 `onSended`/`onRecved`/`onAckSended`/`onRDMAFinished`/CLOSE/CHECK 處理。

`WRId`（`IBSocket.h:282-337`）用 `folly::StampedPtr` 把「指標/索引 + WRType stamp」打包進 64-bit `wr_id`，完成時無需查表即可分辨 WR 種類（SEND/RECV/ACK/RDMA/RDMA_LAST/CLOSE/CHECK）。

### 6.7 連線就緒的資料面觸發

回顧 §4.7：QP 的 READY 狀態實際是在第一個 WC 到達時觸發的——
- client：`onSended`（probe 送出）→ `CONNECTING → READY`。
- server：`onRecved`（收到 probe）→ `qpReadyToSend()` → `ACCEPTED → READY`。

---

## 7. 階段六：單向 RDMA Read/Write（bulk 資料平面）

> 這才是 RDMA 真正的價值所在：one-sided 操作，對端 CPU 完全不參與。3FS 用它搬檔案 chunk。

### 7.1 API 與批次抽象

對外 API（`IBSocket.h:154-167`）：`rdmaRead`/`rdmaWrite`，單 buffer 或 `std::span<RDMABuf>`，內部都導向 `rdma(opcode, remoteBuf, localBufs)` → `RDMAReqBatch`。

`RDMAReqBatch::add`（`IBSocket.cc:277-305`）把 localBufs 按 **`max_sge`** 切成多個 `RDMAReq`（每個 req = 一個 WR，可帶多個 local buffer 變多個 SGE），同時 `remoteBuf.advance(total)` 推進遠端位址，使每個 WR 對應遠端的連續區段：

```cpp
struct RDMAReq { uint64_t raddr; uint32_t rkey; uint32_t localBufFirst; uint32_t localBufCnt; };
```

### 7.2 三段式流控與切批

`rdmaBatch`（`IBSocket.cc:931-1009）：

```
reqs ──按 max_rdma_wr_per_post(32) 切──▶ 多個 RDMAPostCtx
  每個 post：
    ├─ rdmaSem_.try_wait(reqs.size())     ← BatchSemaphore 信用（QP 在途 WR 上限 = max_rdma_wr 128）
    ├─ rdmaPost(ctx)                       ← 等信用 → checkState → rdmaPostWR → 等 baton
    └─ 多個 post 用 folly::coro::collectAllRange 併發
```

`rdmaPost`（`IBSocket.cc:1011-1030`）：

```cpp
if (ctx.waiter) co_await ctx.waiter->baton;      // 等 semaphore 信用
auto guard = makeGuard([&]{ rdmaSem_.signal(ctx.reqs.size()); });  // 完成後歸還
CO_RETURN_ON_ERROR(checkState());
rdmaPostWR(ctx);                                  // 組 WR 並 post
co_await ctx.baton;                               // 等完成（由 onRDMAFinished post）
if (ctx.status != IBV_WC_SUCCESS) co_return makeError(kRDMAError);
```

### 7.3 WR linked-list 與 ibv_post_send

`rdmaPostWR`（`IBSocket.cc:1032-1115`）把一個 post 的所有 req 組成一條 **`ibv_send_wr` linked-list**（`next` 串接），一次 `ibv_post_send` 送整串：

```cpp
for (auto &req : ctx.reqs) {
  auto &wr = wrs[wrId++];
  wr.wr_id  = WRId::rdma(&ctx, false);
  wr.next   = &wr + 1;                            // 串成 linked-list
  wr.sg_list= &sges[sgeId];
  wr.num_sge= req.localBufCnt;
  wr.opcode = ctx.opcode;                         // IBV_WR_RDMA_READ 或 RDMA_WRITE
  wr.send_flags = 0;                              // 中間 WR 不 signal
  wr.wr.rdma.remote_addr = req.raddr;
  wr.wr.rdma.rkey        = req.rkey;
  for (auto &buf : localBufs.subspan(req.localBufFirst, req.localBufCnt)) {
    auto mr = buf.getMR(port_.dev()->id());       // ← 本連線裝置的 MR
    sges[sgeId++] = { .addr=(uint64_t)buf.ptr(), .length=buf.size(), .lkey=mr->lkey };
  }
}
wrs.rbegin()->next      = nullptr;
wrs.rbegin()->wr_id     = WRId::rdma(&ctx, true);  // 只有最後一個是 RDMA_LAST
wrs.rbegin()->send_flags|= IBV_SEND_SIGNALED;      // 只有最後一個產生 CQE
ibv_post_send(qp_, &wrs[0], &bad);
```

**設計巧思**：整批只有最後一個 WR 設 `IBV_SEND_SIGNALED`。RC QP 保證順序完成，所以「最後一個完成」即代表「整批完成」，只需一個 CQE。

完成時 `onRDMAFinished`（`IBSocket.cc:560-564`）→ `ctx.finish()` → `baton.post()` 喚醒等待的 coroutine。

**錯誤處理**：若 `ibv_post_send` 部分成功（`bad != &wrs[0]`），因為只有最後一個 WR signaled，無法追蹤已 post 的 WR 何時完成，所以直接把 QP 設成 `IBV_QPS_ERR`（`IBSocket.cc:1099-1112`），讓整條連線進入錯誤狀態由上層重建。

### 7.4 方向語意：RDMA 一律由 storage server 發起

**極重要**：3FS 中 client 永遠是被動的記憶體目標，RDMA 一律由 storage server 的 HCA 主動發起：

| IO 類型 | opcode | 發起方 | 資料流向 | server 端取得 batch |
|---------|--------|--------|----------|---------------------|
| Client **讀** | `IBV_WR_RDMA_WRITE` | server | server → client | `ctx.writeTransmission()` |
| Client **寫** | `IBV_WR_RDMA_READ` | server | client → server | `ibSocket->rdmaReadBatch()` |

- **讀**：server 從磁碟讀進本地 BufferPool buffer，再 `RDMA_WRITE` 推回 client 的 Iov（`BatchReadJob::addBufferToBatch`, `BatchReadJob.cc:74-93` 把 `readIO().rdmabuf` + local buf 配對加入 writeBatch）。server read buffer 用 4096-byte AIO 對齊（`alignedLength = length + headLength + tailLength`），RDMA Write 前用 `subrange(headLength, length)` 去掉對齊 padding。
- **寫**：server 配本地 buffer，`RDMA_READ` 從 client buffer 拉資料，再把自己的 `toRemoteBuf()` 透過 CRAQ chain 往後繼節點轉發（`ReliableForwarding`）。

---

## 8. 階段七：上層 serde RPC 整合與 bulk 觸發

### 8.1 Wire 格式

**`MessageHeader`（8 byte，`MessageHeader.h:20-30`）**：

```cpp
struct MessageHeader {
  uint32_t checksum;   // CRC32c 高 24 位 | magic 0x86 | compressed bit(bit0)
  uint32_t size;       // payload 長度（不含 header）
};
```

`checksum` 欄位一石三鳥（`MessageHeader.h:32-38`）：

```cpp
uint32_t crc32c = folly::crc32c(data, size, 0);
return (crc32c & ~0xffu) | kSerdeMessageMagicNum | compressed;   // 低 8 bit 放 magic+compressed
```

- `isSerdeMessage()`：`(checksum & 0xfe) == 0x86`
- `isCompressed()`：`checksum & 1`
- `kMessageMaxSize = 512MB`

`MessageWrapper`（`MessageHeader.h:40-94`）負責在 TCP byte stream 上切訊息邊界：`messageLength() = 8 + header().size`，`messageComplete()` 判斷一則訊息是否收齊，`next()` 移到下一則。

**`MessagePacket`**（serde 層）攜帶 `uuid / serviceId / methodId / flags`，flags 含 `IsReq / UseCompress / ControlRDMA`。`serviceId` 路由到對應服務（如 `IBConnect`=11、`RDMAControl`=10、storage=3、meta=4、mgmtd=217）。

### 8.2 `CallContext`：RPC 內觸發 RDMA

`CallContext::RDMATransmission`（`CallContext.h:94-114`）內部持有一個 `IBSocket::RDMAReqBatch` 並決定方向：

```cpp
RDMATransmission readTransmission()  { return {*this, IBV_WR_RDMA_READ}; }   // 從對端拉
RDMATransmission writeTransmission() { return {*this, IBV_WR_RDMA_WRITE}; }  // 推給對端
```

server 處理一個帶 `RDMARemoteBuf` 的請求時，用這些方法把 client 的 (addr,len,rkey) 與 server 本地 buffer 配對，發起 one-sided RDMA。

### 8.3 `RDMAControl`：流控握手（serviceId 10）

`RDMAControl`（`RDMAControl.h:10-18`）是獨立服務，唯一方法 `apply(RDMATransmissionReq{uuid} → Rsp)`：

```cpp
SERDE_SERVICE(RDMAControl, 10) { SERDE_SERVICE_METHOD(apply, 1, RDMATransmissionReq, RDMATransmissionRsp); };
```

`RDMATransmissionLimiter`（`RDMAControl.h:20-40`）用 semaphore 限「同時進行的 RDMA 傳輸數」（`max_concurrent_transmission` 預設 64），可熱更新。流程（`RDMAControl.cc:30-41`）：

```cpp
co_await limiter_->co_wait();                                          // 取得一個傳輸名額
Waiter::instance().setTransmissionLimiterPtr(req.uuid, limiter_, ...); // 用 uuid 綁到 Waiter
// → 傳輸完成時 signal(latency) 歸還名額並記錄網路延遲
```

client 端在 `CallContext::RDMATransmission::applyTransmission`（`CallContext.cc:15-26`）發起這個 apply，送 `RDMATransmissionReq{ctx.packet().uuid}`，用 packet uuid 把「流控 apply」與「真正資料傳輸」綁成同一筆交易。

### 8.4 三層流控全貌

```
┌─ 層 1：RDMATransmissionLimiter (serviceId 10, 預設 64) ──────────┐
│   全域限制併發 RDMA 傳輸筆數；uuid 綁定 Waiter，回應時歸還信用    │
└──────────────────────────────────────────────────────────────────┘
┌─ 層 2：storage 每-device semaphore (預設 256) ────────────────────┐
│   限制每張 HCA 的併發傳輸                                          │
└──────────────────────────────────────────────────────────────────┘
┌─ 層 3：IBSocket rdmaSem_ (BatchSemaphore, 預設 128) ──────────────┐
│   QP 級在途 WR 上限（max_rdma_wr），配合 max_rdma_wr_per_post 切批 │
└──────────────────────────────────────────────────────────────────┘
```

### 8.5 USRBIO Iov 零拷貝串接

client 端的 `Iov` 是「跨行程共享 + 預先 RDMA 註冊」的大緩衝區，串接鏈（已逐行驗證）：

```
hf3fs_iovcreate (lib/api/hf3fs_usrbio.h:71-93)
  → ShmBuf::mapBuf: shm_open + ftruncate + mmap(MAP_SHARED)  (Shm.cc:149-175)
  → IovTable::addIov → ShmBuf::registerForIO (按 block 逐塊)  (Shm.cc:97-117)
  → StorageClient::registerIOBuffer                           (StorageClient.cc:97-110)
  → RDMABuf::createFromUserBuffer (userBuffer_=true,只註冊)   (RDMABuf.cc:42-48)
  → Inner::registerMemory → 每裝置 ibv_reg_mr                 (RDMABuf.cc:107-127)
```

`MAP_SHARED` 是讓 NIC 與應用同時可見、可被 `ibv_reg_mr` 釘住的關鍵。**Iov 在建立時就一次性註冊（每 HCA 一份 MR），之後所有 IO 只傳 addr+rkey、不再註冊、不再複製。** IO 時用請求帶的 `bufId`(UUID) 在 `IovTable::shmsById` 做 O(1) 查找（`FuseClients.cc:301-332`），並拒絕跨 block 的 IO（`Shm.h:78-99`）。

### 8.6 Transport 池化與 Waiter

- **TransportPool**：按 address 分 32 shard + thread-local cache，每 `(node, type, idx)` 一條連線，`max_connections` 預設 1。`get()` 找不到就觸發 `Transport::create` + `connect`。
- **Waiter**（`Waiter.h`）：`uuid → Item(Baton)` 分 256 shard。RPC 送出後在此等待；回應到達時 `post(uuid)` 喚醒對應 coroutine；後台 Timer 執行緒處理逾時。這就是 serde RPC 請求-回應配對的核心。

---

## 9. 階段八：連線關閉與資源回收

> RDMA 的特殊挑戰：in-flight WR 可能還在存取已註冊記憶體，貿然釋放會造成 NIC use-after-free。3FS 用 graceful drain 解決。

### 9.1 優雅關閉

`close()`（`IBSocket.cc:708-737`）：

```cpp
closed_ = true;
if (closeGracefully()) {                          // 發 CLOSE 訊息
  while (state == READY/CONNECTING && 未逾時 200ms)
    co_await sleep(50ms);                          // 等對端確認
}
ibv_modify_qp(qp_, {.qp_state=IBV_QPS_ERR}, IBV_QP_STATE);  // 最後 QP → ERROR
```

`closeGracefully()`（`IBSocket.cc:799-834`）post 一個空的 `IBV_WR_RDMA_WRITE_WITH_IMM` 帶 `ImmData::close()`，兼具「通知對端」與「flush 前序 WR」的作用。對端 `onImmData(CLOSE)` 收到後轉 `CLOSE` 狀態。

### 9.2 Drainer：確保 in-flight WR 完成

`IBSocketManager` + `Drainer`（`IBSocket.cc:1117-1260`）負責關閉後的排空：
- `close(socket)` 把 socket 交給 manager，建 `Drainer` 掛到 `IBManager::eventLoop_`（`EPOLLIN|OUT|ET`）。
- `Drainer` 持續 `drain()`（`IBSocket.cc:1153-1187`）poll CQ，直到收到自己的 CLOSE WC 或對端的 CLOSE imm，或逾時（`drain_timeout`）。
- `IBSocketManager` 用 timerfd（200ms）週期檢查逾時的 drainer（`IBSocket.cc:1189-1260`）。

這保證所有已 post 的 WR（含 RDMA）都完成或失敗後，才真正解構 IBSocket（`~IBSocket` 把 QP 轉 ERROR、ack 剩餘 CQ events，`IBSocket.cc:315-327`），避免釋放仍被 NIC 存取的記憶體。

### 9.3 探活

`check()`（`IBSocket.cc:748-796`）post 一個 0-byte、inline、signaled 的 `IBV_WR_RDMA_WRITE`（`WRId::check`）。若送不出去或對端死亡，會在後續 poll 收到 error WC，從而偵測連線中斷。

### 9.4 四種控制訊息對照

| 訊息 | opcode | 載荷 | 用途 |
|------|--------|------|------|
| connect probe | `IBV_WR_SEND_WITH_IMM` | `ImmData::ack(0)`，0-byte，不佔 buffer | 觸發 server RTS→READY；client CONNECTING→READY |
| ACK | `IBV_WR_SEND_WITH_IMM` | `ImmData::ack(buf_ack_batch)` | credit 流控歸還 |
| CLOSE | `IBV_WR_RDMA_WRITE_WITH_IMM` | `ImmData::close()`，0-byte | 優雅關閉通知 |
| CHECK | `IBV_WR_RDMA_WRITE` | 0-byte，inline | liveness 探活 |

---

## 10. 全景資料流：一次完整的讀與寫

### 10.1 連線建立（client 首次連 storage server）

```
1. 上層對 RDMA://storage:8000 發 RPC
2. TransportPool 無連線 → Transport::create(RDMA) → new IBSocket
3. Transport::connect: 建 Address::TCP 的 ClientContext，呼叫 IBSocket::connect
4. [TCP] IBConnect::query  → 取得 server 裝置清單
5.        selectDevice     → 配對本地/遠端 HCA（zone+linklayer）
6.        qpCreate+qpInit  → comp_channel + CQ + RC QP(INIT) + ring buffer MR
7. [TCP] IBConnect::connect → 交換 qp_num/mtu/lid或gid
8.        RTR → RTS         → QP 可收可發
9.        postConnectProbe  → 送 SEND_WITH_IMM ACK(0)
10. server onRecved(probe) → RTR→RTS → READY
    client onSended(probe) → CONNECTING → READY
✅ RDMA 連線就緒
```

### 10.2 Client 讀檔案（batchRead）

```
1. [TCP/RDMA serde] client 送 BatchReadReq，內含每個 ReadIO.rdmabuf
   = client Iov buffer 的 (addr, len, rkeys[])
2. (可選) client 先發 RDMAControl::apply 取得傳輸信用
3. server 從磁碟（io_uring/libaio, fixed buffer）讀進本地 BufferPool buffer
4. server: ctx.writeTransmission() → RDMAReqBatch
   addBufferToBatch: 把 (client rdmabuf, server local buf) 配對
5. server ibv_post_send(IBV_WR_RDMA_WRITE)  ← 一次 linked-list，最後一個 signaled
   資料直接寫入 client 的 Iov（client CPU 不參與）
6. server onRDMAFinished → 釋放信用 → 回 BatchReadRsp
7. client 從 Iov 讀到資料（零拷貝）
```

### 10.3 Client 寫檔案（write，CRAQ chain）

```
1. [serde] client 送 UpdateReq，內含 UpdateIO.rdmabuf = client 資料 buffer 描述子
   （小寫入可走 inlinebuf 不用 RDMA）
2. HEAD storage: ibSocket->rdmaReadBatch() → IBV_WR_RDMA_READ 從 client 拉資料進本地 buffer
3. HEAD 寫本地磁碟，並把自己的 toRemoteBuf() 經 ReliableForwarding 往 chain 下一節點轉發
4. 沿 chain 傳播（每一跳都是 RDMA Read pending 資料）
5. TAIL commit → ACK 沿 chain 回傳 → HEAD 回 client
```

---

## 11. 附錄

### 11.1 關鍵設定參數（IBSocket::Config，IBSocket.h:83-120）

| 參數 | 預設 | 說明 |
|------|------|------|
| `buf_size` | 16 KB | 每個 send/recv ring buffer 大小 |
| `send_buf_cnt` | 32 | send ring buffer 數 |
| `buf_ack_batch` | 8 | 每收 N 個 buffer 回一次 ACK |
| `buf_signal_batch` | 8 | 每 N 個 send WR signal 一次 |
| `event_ack_batch` | 128 | 每 N 個 CQ event 批次 ack |
| `max_sge` | 16 | 單 WR 最大 SGE 數 |
| `max_rdma_wr` | 128 | QP 在途 RDMA WR 上限（rdmaSem_ 初值） |
| `max_rdma_wr_per_post` | 32 | 單次 ibv_post_send 最大 WR 數 |
| `max_rd_atomic` | 16 | QP `max_rd_atomic` |
| `timeout` | 14 | QP timeout（RTS） |
| `retry_cnt` | 7 | QP retry count |
| `min_rnr_timer` | 1 | RNR timer |
| `drain_timeout` | 5 s | 關閉時排空逾時 |

### 11.2 Service ID 速查

| serviceId | 服務 | 通道 | 角色 |
|-----------|------|------|------|
| 10 | `RDMAControl` | TCP/RDMA | RDMA 傳輸流控（apply 信用） |
| 11 | `IBConnect` | **TCP** | RDMA 握手（query/connect 交換 QP 參數） |
| 3 | `StorageSerde` | RDMA | storage 讀寫（bulk 走 one-sided RDMA） |
| 4 | `MetaSerde` | RDMA | meta 操作 |
| 217 | `Mgmtd` | RDMA | 叢集管理 |

### 11.3 QP 狀態遷移與 ibverbs 呼叫對照

| 階段 | 函式 | ibverbs 呼叫 | 關鍵屬性 |
|------|------|--------------|----------|
| 資源建立 | `qpCreate` | `ibv_create_comp_channel` / `ibv_create_cq` / `ibv_req_notify_cq` / `ibv_create_qp` | RC type, sq_sig_all=0, send_cq=recv_cq |
| RESET→INIT | `qpInit` | `ibv_modify_qp` | qp_access_flags(REMOTE_READ\|WRITE), pkey_index, port_num |
| INIT→RTR | `qpReadyToRecv` | `ibv_modify_qp` + `ibv_post_recv`×N | path_mtu, dest_qp_num, rq_psn, ah_attr(dlid 或 grh.dgid) |
| RTR→RTS | `qpReadyToSend` | `ibv_modify_qp` | timeout, retry_cnt, sq_psn, max_rd_atomic |
| 收發 | `postSend/postRecv/postAck` | `ibv_post_send` / `ibv_post_recv` | IBV_WR_SEND / SEND_WITH_IMM |
| bulk | `rdmaPostWR` | `ibv_post_send`(linked-list) | IBV_WR_RDMA_READ / RDMA_WRITE |
| poll | `poll/cqPoll` | `ibv_get_cq_event` / `ibv_poll_cq` / `ibv_req_notify_cq` / `ibv_ack_cq_events` | 批次 16 |
| 關閉 | `close` | `ibv_modify_qp`(→ERR) / `ibv_destroy_*` | IBV_QPS_ERR |

### 11.4 檔案索引

| 主題 | 檔案 |
|------|------|
| 位址模型 | `src/common/utils/Address.h` |
| Socket 介面 | `src/common/net/Socket.h` |
| TCP socket | `src/common/net/tcp/TcpSocket.{h,cc}` |
| 連線封裝/狀態機 | `src/common/net/Transport.{h,cc}` |
| epoll 事件迴圈 | `src/common/net/EventLoop.{h,cc}` |
| I/O 工作者 | `src/common/net/IOWorker.{h,cc}` |
| 監聽/分流 | `src/common/net/Listener.{h,cc}` |
| 服務群組設定 | `src/common/net/ServiceGroup.{h,cc}` |
| 連線池 | `src/common/net/TransportPool.{h,cc}` |
| **RDMA 握手協議** | `src/common/net/ib/IBConnect.{h,cc}` |
| **RDMA 握手服務** | `src/common/net/ib/IBConnectService.h` |
| **RDMA socket（收發+bulk）** | `src/common/net/ib/IBSocket.{h,cc}` |
| IB 裝置管理 | `src/common/net/ib/IBDevice.{h,cc}` |
| 記憶體註冊/buffer | `src/common/net/ib/RDMABuf.{h,cc}` |
| RDMA 流控 | `src/common/net/RDMAControl.{h,cc}` |
| wire 格式 | `src/common/net/MessageHeader.h` |
| RPC 內 RDMA 觸發 | `src/common/serde/CallContext.{h,cc}` |
| 請求-回應配對 | `src/common/net/Waiter.{h,cc}` |
| USRBIO API | `src/lib/api/hf3fs_usrbio.h` |
| shm/mmap | `src/lib/common/Shm.cc` |
| storage buffer 池 | `src/storage/service/BufferPool.cc` |
| storage 讀路徑 | `src/storage/aio/BatchReadJob.cc`、`src/storage/service/StorageOperator.cc` |
| wire 內嵌 rdmabuf | `src/fbs/storage/Common.h` |

---

## 一頁總結

3FS 的 RDMA 設計可以濃縮成五句話：

1. **控制平面走 TCP，資料平面走 RDMA**：QP 握手、流控信用、小型 RPC 都在 TCP 上；只有大型 chunk 走 one-sided RDMA。
2. **RDMA 連線靠 TCP 握手建立**：`IBConnect`(serviceId 11) 這個 TCP serde 服務交換 qp_num/mtu/lid或gid，完成 `INIT→RTR→RTS` 遷移；server 採非對稱握手，只到 RTR，靠 client 的 connect probe 觸發 RTS。
3. **在 RC QP 上模擬 stream socket**：ring buffer + credit 流控（ImmData 夾帶 ACK 信用）+ signal batching（降 CQE）+ completion-channel/epoll 事件驅動（不 busy-poll）。
4. **bulk 用 WR linked-list 批次 + 只簽最後一個**：一次 `ibv_post_send` 送整串，靠 RC 順序保證「最後完成=整批完成」；三層流控（傳輸/裝置/QP）節制併發。
5. **記憶體多 HCA 全註冊 + 零拷貝**：同一段記憶體對每張卡各註冊一份 MR，按連線挑 lkey/rkey；client 的 shm Iov 一次註冊終身重用，達成跨行程+磁碟的全鏈路零拷貝。整套 RDMA 一律由 storage server 主動發起（讀=RDMA_WRITE 推、寫=RDMA_READ 拉）。
