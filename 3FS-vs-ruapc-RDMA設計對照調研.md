# 3FS 與 ruapc 的 RDMA 邏輯對照調研

> 兩套獨立演化的 RDMA 傳輸層，解同一組問題卻在幾乎每個關鍵決策上分岔。本文逐軸對照，並指出哪些分岔是「定位不同」、哪些是「其中一方確實比較好」。
>
> **3FS**：`/Users/boen/3FS`，C++／folly，DeepSeek 的分散式檔案系統傳輸層，已在 180 storage node 規模的生產叢集驗證。
> **ruapc**：`~/ruapc`（`SF-Zhou/ruapc`，本文對照 commit `d24981c`），Rust，通用 RPC 框架，`#![forbid(unsafe_code)]` 的核心 + 隔離的 unsafe verbs 綁定。
>
> 姊妹文件：`3FS-RDMA編程深度剖析.md`（3FS 現況）、`3FS-RDMA機制優化可能性調研.md`（3FS 的改進空間）。本文的許多對照，正好是那份優化調研裡「建議做」的項目在 ruapc 裡的**既成實作**。

---

## 0. 先說清楚可比與不可比

| | 3FS | ruapc |
|---|---|---|
| 定位 | 分散式檔案系統的內部傳輸層 | 通用 RPC 框架（TCP / WS / HTTP / RDMA 同一介面） |
| 語言 | C++20 + folly coroutine | Rust + tokio |
| RDMA 程式碼量 | 約 4,700 行（`src/common/net/ib/` + `RDMAControl`） | 約 14,300 行（`ruapc/src/rdma/` + `ruapc-rdma/` + `ruapc-bufpool/`） |
| 驗證規模 | 生產叢集，180 storage node × 數百 client | 單機 loopback，20,000 連線／單 CQ；534 個測試 |
| 形式化驗證 | 有（`specs/RDMASocket/`，721 行 P 模型） | 無（但有極詳盡的設計文件與成對量測報告） |

**不可比的部分**：ruapc 的 benchmark 數字（serial 16B 33.1 µs、1024 tasks 417 kops/s，`docs/benchmark.md`）是**同一個行程內 client + server 走本機 NIC loopback**，含 tokio 排程，不是裸 RDMA 延遲，也不是跨機生產數字。不能拿來跟 3FS 的叢集數字比。本文只比**設計邏輯**，不比效能數字。

**可比的部分**：兩者都在 RC QP 上疊了一層「可靠訊息傳輸 + 單向 bulk 傳輸」，都要解同一組問題——連線資源、完成通知、信用流控、bulk 方向、記憶體註冊、准入限速、握手生命週期、完成識別。這八個軸上兩者的選擇幾乎完全不同。

---

## 1. 總對照表

| 軸 | 3FS | ruapc | 誰更優 |
|---|---|---|---|
| **A 抽象層次** | 在 RC 上模擬 **byte stream**，重用整套 TCP `Transport` 狀態機 | **自帶長度的 frame**（`[4B len][4B meta_len][meta][payload]`），訊息式 | 各有理，見 §2 |
| **B CQ / 連線資源** | 每連線私有 CQ + completion channel + QP | **每裝置 CQ 分片**（預設 1 個，65536 entry），多 QP 共用；軟體完成信用准入 | ruapc |
| **C 完成通知** | 純中斷：`epoll_wait(-1)` → `ibv_get_cq_event` | **混合輪詢**：完成後 busy-poll `poll_spin_us`（預設 50 µs），空轉才 arm + `poll(2)` | ruapc |
| **D 信用流控** | 專用 ACK 訊息（`SEND_WITH_IMM`，吃掉對端 recv WQE） | **信用捎帶在資料 SEND 的 imm_data**（兩個 16-bit 欄位：data delta \| ack delta） | ruapc |
| **E bulk 方向** | **server 主動**：讀 = `RDMA_WRITE` 推、寫 = `RDMA_READ` 拉 | **一律 `RDMA_READ`，由資料目的端發起**；反向要送資料時改發 reverse RPC 讓對方來拉 | 見 §6，是真正的分歧點 |
| **F 記憶體與拷貝** | 每連線 1.14 MiB 私有 bounce buffer；送收各一次 memcpy | **共享 buddy pool 一次註冊**；序列化直接寫進已註冊 buffer，**送出零拷貝**；收端 >1 KiB 零拷貝、≤1 KiB 才複製 | ruapc |
| **G 准入 / 限速** | 三層 semaphore，全部**數「個」** | 每 NIC READ WR 信號量 + **GCRA 位元組級限速**（依 port 標稱頻寬，收發獨立） | ruapc |
| **H 握手** | TCP 上的 `IBConnect` RPC，**非對稱**（server 停 RTR，靠 probe 轉 RTS） | TCP 上版本化 bootstrap RPC（v2）+ **prepare/commit/cancel + lease 狀態機**，雙向 limits 協商 | ruapc（但複雜度高得多） |
| **I 完成識別** | `wr_id` = `StampedPtr`（16-bit type + 48-bit 指標） | **`(CQ, QPN, WRType, sequence)` 四元組** + QPN lease + retired floor | ruapc |
| **J 多路徑** | 連線建立時靜態 round-robin 挑裝置；wire 格式帶 **4 個 rkey** | 負載感知 placement + 背景 rebalance/遷移；wire 只帶 **1 個 rkey** | 各贏一半，見 §11 |
| **K 大訊息** | 走 RDMA bulk（chunk 級） | `max_msg_size` 預設 256 KiB 內走 SEND；更大請用 remote read/write | 等價 |
| **L `max_inline_data`** | 0 | **也是 0** | 兩者都漏了 |

---

## 2. 軸 A：抽象層次 —— byte stream vs self-delimiting frame

### 3FS：在 RC QP 上模擬 stream socket

`IBSocket` 實作 `Socket` 純虛介面（`src/common/net/Socket.h:14-39`），提供 `send(iovec)` / `recv(MutableByteRange)` / `flush()` / `poll()`，語意跟 `TcpSocket` 完全一樣。上層的 `Transport`、`Processor`、`IOWorker`、`serde` 一行都不用改。訊息邊界由上層的 `Processor` 從 byte stream 重組。

**代價**：`IBSocket::send`（`IBSocket.cc:611-651`）把 payload memcpy 進 16 KiB 的註冊環形緩衝；`recv`（`:664-706`）再 memcpy 出來。每個 RPC 兩次拷貝。而且信用是**以「緩衝格」計量**，一個 100 B 的 RPC 也佔整整一格。

### ruapc：自帶長度的 frame

`ruapc/src/rdma/frame.rs` 定義每次 SEND 的內容是一串 `[4B frame_len][4B meta_len][meta][payload]`，通常只有一個 frame。註解把理由講得很直白：

> Uniform framing makes messages self-delimiting, so **aggregation is plain concatenation** and the receive path has a single parse loop.

這個決定連帶產生三個好處：

1. **送出零拷貝**：`serialize_msg`（`rdma_socket.rs:194-215`）直接從共享 pool 取一塊**已註冊**的 buffer，serde 把訊息序列化**進去**，然後 `post_send` 直接用那塊 buffer 的 lkey。沒有 bounce buffer、沒有額外 memcpy。
2. **視窗滿時的聚合是免費的**：送出視窗用完時，訊息交給 poll thread 排隊；poll thread 用**單純的位元組串接**把多個待送訊息打包成一次 SEND（上限 `MAX_AGG_BYTES = 64 KiB`，`poller/conn.rs:20`）。3FS 要做同樣的事就得在 stream 語意上額外發明訊息邊界。
3. **收端 poll thread 完全不解析**：`poller/mod.rs` 的註解稱之為 *zero-parse poll thread*——流控是**以 work completion 計量**（一個 recv WC = 一個信用，不管裡面塞了幾個訊息），收到的 buffer 直接批次丟給固定的 dispatch worker（預設 32 個 SPSC queue），由 tokio worker 去走 frame 迴圈解析。poll thread 不做 `tokio::spawn`、不阻塞。

### 誰更優

**ruapc 的抽象在效能上嚴格更好**（少一到兩次拷貝、聚合免費、信用以 WC 計量而非以格計量）。

但 3FS 的選擇不是失誤，是**用效能換取程式碼複用**：RDMA 支援只是換一個 `Socket` 實作，TCP 與 RDMA 共用同一套連線狀態機、重試、逾時、壓縮、追蹤。ruapc 為此付出的代價，是 `sockets/` 底下 TCP / WS / HTTP / RDMA 各自實作 `SocketTrait`，RDMA 那條路徑（含 poller、flow、dispatch、bufpool）是完全獨立的 14,000 行。

**判斷**：以 3FS 的工作負載（位元組量由 bulk RDMA 主導，小 RPC 只吃次數）而言，這筆交易划算，不建議為了省兩次 memcpy 去重寫。但 §5 提到的「信用以格計量」是這個抽象的**可修補副作用**，值得單獨處理。

---

## 3. 軸 B：CQ 與連線資源模型

### 3FS：每連線一整套

`qpCreate`（`src/common/net/ib/IBConnect.cc:547-600`）為每條連線建立**獨立的 comp channel + CQ + QP**，`attr.srq = nullptr`。以出廠設定算（`IBConnect.h:101-106`）：

```
每連線 = 1 comp channel (1 個 fd) + 1 CQ (211 CQE) + 1 QP (170 SQ / 41 RQ)
        + 73 × 16 KiB 註冊緩衝 = 1.14 MiB pinned + 1 個 MR
```

500 個 client → 500 個 fd 全掛在**一個** event loop 執行緒上（`configs/storage_main.toml:110` `num_event_loop = 1`）+ 570 MiB pinned。

### ruapc：每裝置 CQ 分片 + 軟體完成信用

`poller/mod.rs` 的模型是：**每個 RDMA 裝置有若干 CQ 分片（`poll_threads_per_device`，預設 1），每個分片一條專屬 OS poll thread**；連線依可用的**完成信用**選一個分片。`device_cq_len` 預設 65,536。

要讓多個 QP 共用一個 CQ，必須解掉兩個問題，ruapc 兩個都正面解了：

**問題一：CQ overrun。** 共用 CQ 後，「CQ 容量 ≥ 該 QP 全部 WQE」這個保證不再自動成立。ruapc 的答案是**軟體完成信用准入**（`docs/rdma-capacity.md`）：建立 QP **之前**先向分片申請信用，公式是

```
sum(R + W + A) + min(H, sum(K))
```

涵蓋 recv、send window、ACK 上限、以及共享的 READ 預算，**包含未簽名 WR 被 flush 時會產生的 CQE**。信用歸 QP 所有直到「provider 銷毀成功 + 之後觀察到 CQ 為空」；光是把 poller 狀態移掉不會釋放信用。若一個分片塞不下就換下一個分片。

> 這正好回答了 `3FS-RDMA機制優化可能性調研.md` §2 對共享 CQ 提出的風險：「必須改成動態計帳，這個帳要做對，不然是 silent data loss」。ruapc 的計帳方式可以直接當參考實作。

**問題二：完成要路由回哪條連線。** 見 §10。

實測結果：`docs/rdma-capacity.md` 記錄**單一 CQ 上同時維持 20,000 條健康連線**、驗證 40,000 次序列 echo。作為對照，改造前的基線在 16,384 個 QP 就因 route 耗盡而停止，512/1024 條連線的 RPC 就被舊的 256 連線 CQ 預算擋下。

### 但 ruapc 沒解決的部分（要誠實講）

**接收環的記憶體仍然是 O(N)。** `recv_queue_len`（預設 8）× `max_msg_size`（預設 256 KiB）= **每連線 2 MiB**，比 3FS 的 1.14 MiB 還大。`RingReservation`（`poller/mod.rs:84-104`）只是**記帳**，不是共享。capacity 報告自己也寫明：20,000 連線時 RSS 約 7.96 GiB，並註明「CQ capacity does not remove receive-buffer costs」。

ruapc 唯一的緩解是**優雅降級**：repost 時才從共享 pool 配置緩衝，配置失敗就讓接收環縮小而不是殺掉連線（`poller/conn.rs:237-250`）。加上 ≤1 KiB 的訊息**複製出來**讓緩衝立刻回收（`SMALL_MSG_COPY_MAX`，`conn.rs:40`），註解明確說這是為了避免「pool 耗盡 → 接收環縮到 0 → 連 ACK 都收不到 → 死鎖螺旋」。

**結論：兩邊都沒有 SRQ。** 共享接收緩衝池這件事，3FS 與 ruapc 都還沒做。ruapc 贏在 CQ / fd / 完成路由那一半。

---

## 4. 軸 C：完成通知

### 3FS：純中斷

`EventLoop.cc:122` 是 `epoll_wait(epfd_, events, kMaxEvents, -1)`，無限阻塞。完成通知的唯一來源是 comp channel 中斷。每次通知的路徑是「NIC 中斷 → IRQ → epoll 喚醒 → `ibv_get_cq_event`（read syscall）→ `ibv_poll_cq` → `ibv_req_notify_cq`」。全庫沒有任何 busy-poll 路徑。

### ruapc：明確的兩相位混合輪詢

`poller/mod.rs:1-15` 的模組註解直接把它寫成規格：

- **Busy phase**：任何完成之後，執行緒持續輪詢 CQ 達 `poll_spin_us`（預設 **50 µs**），**消除中斷 + epoll + task 喚醒的延遲**。
- **Idle phase**：spin window 到期才 `req_notify`，**再輪詢一次關掉競態窗口**，然後在 comp channel fd 與一支 wake pipe 上 `poll(2)` 睡覺。
- **Wake pipe** 由三種事件寫入：送出視窗滿而排隊的 sender、`RdmaSocket::set_error`、連線註冊／關閉。

這正是 `3FS-RDMA機制優化可能性調研.md` §3 建議的結構，連「arm 之後要再 drain 一次」這個細節都一樣（3FS 在 `IBSocket::poll` 裡已經做對了這一步，只是缺前面的 spin 階段）。

另外 ruapc 用的是**每 CQ 分片一條專屬 OS 執行緒**，不是共用的 epoll 執行緒。這讓 busy-poll 的 CPU 成本是**有界且可預期的**（`poll_threads_per_device` 條執行緒），而不是把整個 event loop 執行緒燒掉。3FS 若要加 busy-poll，必須先處理「同一條 event loop 執行緒還要服務其他 fd」的問題——這是 3FS 做這項改動時最大的結構障礙。

**ruapc 更優，而且是可直接移植的設計。**

---

## 5. 軸 D：信用流控協定

這一軸是兩者差距最大、也最能說明設計成熟度的地方。

### 3FS：專用 ACK 訊息

- 接收端每釋放 `buf_ack_batch`（8）格緩衝，送一個 `IBV_WR_SEND_WITH_IMM`、`num_sge=0`、`imm_data = ack(8)` 的**純信用訊息**（`IBSocket.cc:865-886`）。
- 這個 ACK **在對端消耗一個 recv WQE**，所以 `qpMaxRecvWR` 必須預留 `qpAckBufs() = 8` 格，還要一整套 `ackBufAvailable_` 自我節流（`IBSocket.cc:552, 690`）避免撐爆對端 RQ。
- 送出端的緩衝釋放要**同時滿足**本地 signaled CQE 與對端 ACK：`sendAvailable = min(sendAcked_, sendSignaled_)`（`IBSocket.cc:347`）。
- 兩側都是**定量批次**（`recvNotAcked_ == buf_ack_batch`、`sendNotSignaled_ >= buf_signal_batch`），沒有低水位觸發。

### ruapc：信用捎帶 + 多重觸發條件

`poller/flow.rs` 的模型：

- **信用編碼在 `imm_data` 的兩個 16-bit 欄位**：高 16 bit 是 ACK delta、低 16 bit 是 data delta（`peer_ack`、`due_ack`、`mark_acked`）。資料 SEND 與 keepalive 都能捎帶，**雙向有流量時不需要任何專用 ACK 訊息**。
- **信用以 work completion 計量**，不是以緩衝格：「一個 recv WR 消耗一個信用，不管它承載幾個 frame」。配合 §2 的聚合，一次 SEND 可以裝 64 KiB 的多個小訊息卻只吃一格信用。
- **釋放條件同樣是雙重的**：`finished_data() = min(data_completed, data_confirmed)`，註解寫明「publishing only one side would reuse registered buffers or peer receive slots too early」——這點兩者一致，都做對了。
- **ACK 的觸發是三選一**：累積達 `send_window/2`、或累積的 ACK-of-ACK 達門檻、或距上次 ACK 超過 `ACK_KEEPALIVE = 5s`。所以**不會有「餘數卡住」的情況**。
- **防止純 ACK 乒乓**：`due_ack` 裡有一段專門的註解——門檻為 1 時「每個 standalone ACK 都會再生一個 ACK，無限下去」，所以 ACK-of-ACK 至少要攢兩個。capacity 報告記載這正是最小接收環（R2）場景下實測到的病灶。
- **送出視窗尾端強制簽名**：`rdma_socket.rs:275-287` 的不變式——「一個被用滿的送出視窗必須至少含一個 signaled WR，否則它的 slot 會被擱置（未簽名的完成只會被後來的 signaled WR 掃掉），連線會卡到 5 秒 keepalive 才恢復」。

### 這直接命中了 3FS 的一個潛在 bug

`3FS-RDMA機制優化可能性調研.md` §7 F-2 指出：3FS 的 `checkConfig`（`IBConnect.cc:507-528`）只檢查 `buf_ack_batch <= send_buf_cnt`，**不檢查整除**。出廠值 32/8/8 剛好整除所以沒事，但若 `send_buf_cnt` 被調成 30，就會有 6 格信用永遠不還，連線**靜默永久停擺**。

ruapc 用兩個機制讓這個 bug 在結構上不可能發生：**（1）keepalive 逾時觸發**保證任何殘餘信用最多 5 秒後一定送出；**（2）視窗尾端強制簽名**保證本地 CQE 那一側也不會有殘餘。

**這是 3FS 可以直接抄的修法，而且 ruapc 的 `flow.rs` 附了 7 個針對性單元測試**（包含 `ack_cadence_scales_with_the_negotiated_window` 掃 window = 1/2/4/8/32/128、`single_activation_does_not_start_standalone_ack_ping_pong`、`large_ack_deltas_preserve_credits_across_multiple_wire_messages`），可以當測試案例的藍本。

---

## 6. 軸 E：bulk 資料的方向 —— 這是真正的分歧點

### 3FS：storage server 一律主動

- client 讀 → server `RDMA_WRITE` 把資料**推**進 client 的緩衝（`StorageOperator.cc:176-225`）
- client 寫 → server `RDMA_READ` 從 client 的緩衝**拉**資料（`StorageOperator.cc:563-595`）

client 的記憶體永遠是被動目標。

### ruapc：完全不用 `RDMA_WRITE`

全 workspace `grep RDMA_WRITE` **零結果**；`WRType` 只有 `Recv / SendData / SendImm / Read`（`ruapc-rdma/src/types/wrid.rs:23-32`）。

- 「server 想讀 client 的資料」→ server 直接 `RDMA_READ`（跟 3FS 的寫路徑一樣）
- 「server 想寫資料給 client」→ **不推**，而是送一個 reverse RPC `MemoryService::read_into_target`，把自己的來源緩衝當成 `read_regions` 廣告出去，**由 client 自己 `RDMA_READ` 進它 pin 住的目的緩衝**（`ruapc/src/services/memory_service.rs`）

`remote_memory/context.rs` 把理由寫得很明確：

> No one-sided RDMA WRITE is used: on RDMA the server sends a reverse `MemoryService::read_into_target` RPC ... and the *client* executes the RDMA READs into its pinned buffers — **their lifetime is anchored client-side, which makes the transfer safe against client timeouts.**

### 這個取捨的真正內容

**ruapc 買到的是記憶體安全。** 3FS 的讀路徑有一個結構性風險：server 對 client 的緩衝發 `RDMA_WRITE` 時，client 可能已經逾時、`Waiter` 已經把該請求判失敗、上層已經把那塊緩衝回收去做別的事。此時遲到的 RDMA 寫入會**汙染已經被重新使用的記憶體**，而且沒有任何本地程式碼能攔截——這是遠端 DMA 寫進「邏輯上已釋放」的記憶體。3FS 靠「USRBIO 的 iovec 是長期註冊、不會 dereg」避免了 MR 層級的崩潰，但**邏輯層級的資料汙染依然存在**。ruapc 讓目的端自己拉，緩衝的生命週期與發起 DMA 的一方是同一邊，這個風險從根上消失。

**順帶買到的是天然的 incast 控制。** 接收端自己決定何時拉、拉多少，N 個 server 不可能同時對一個 client 灌資料。3FS 為了補這一點，寫了 `RDMAControl` 的 apply RPC（接收端准入，`RDMAControl.cc:31-43`）——但因為要付一個完整 RTT，**所有出廠設定裡 `enable_rdma_control = false`**（`configs/` 下 18 處）。也就是 3FS 的 incast 保護寫好了卻沒開。

**ruapc 付出的代價是一個 reverse RPC 的往返。** server→client 的每一次 bulk 推送都變成「RPC 過去 → client 拉 → client 回應」。對 3FS 的讀路徑（絕大多數流量）而言，這等於把「1 次 WRITE + 回應」換成「1 次 RPC + 1 次 READ + 1 次回應」，延遲明顯更差。

### 誰更優？

**沒有單一答案，這取決於流量形狀：**

- **3FS 的讀路徑**（server 早就知道資料何時從 NVMe 讀完、一次推 4 MiB、client 在等）→ **server push 是對的**。改成 pull 會平白多一個 RTT。
- **ruapc 的通用 RPC**（handler 可能在任意時刻決定要送多少資料給 client，client 端的緩衝生命週期由使用者程式碼控制）→ **pull 是對的**，因為它換到的是安全性，而通用框架沒有辦法假設使用者會正確處理逾時。

**3FS 該學的不是「改成 pull」，而是 §7 F-1 + §4 提到的那兩件事**：把 apply RPC 的同步 grant 改成捎帶式的非同步 credit（拿到 incast 保護但不付 RTT），以及正視「client 逾時後緩衝被重用」這個風險——至少要讓緩衝在 RDMA 完成前不可被回收（例如把 `RDMABuf` 的擁有權延後到 CQE 才釋放，而不是隨請求協程結束）。

---

## 7. 軸 F：記憶體註冊與拷貝

| | 3FS | ruapc |
|---|---|---|
| socket 緩衝 | **每連線私有** 73 × 16 KiB，各自 `ibv_reg_mr`（`IBConnect.cc:731`） | 共享 **buddy pool**（1/4/16/64 MiB 區塊，2 MiB 對齊），**每區塊對每個裝置註冊一次** |
| MR 數量 | O(連線數) | O(pool 區塊數 × 裝置數) |
| 送出路徑 | serde → IOBuf → **memcpy** 進 16 KiB 環 → post | serde **直接序列化進已註冊的 pool buffer** → post（零拷貝） |
| 接收路徑 | recv WQE → **memcpy** 出來給上層 | >1 KiB 零拷貝（`Bytes` 切片）；≤1 KiB 才複製，換緩衝立即回收 |
| bulk 緩衝 | `RDMABuf` / `rdmabufPool`，對**所有裝置（上限 4）各註冊一份** | 同一個 buddy pool；`RemoteBufferInfo` 只帶**一個** rkey |
| GPU / DMABUF | 無 | 無 |
| `max_inline_data` | **0**（`IBConnect.cc:590`） | **也是 0**（`queue_pair.rs` 從未設定 `cap.max_inline_data`） |

ruapc 的 pool 設計有一個值得注意的細節：`MemoryRegistrar` 是 **unsafe trait**，合約寫在 `ruapc-bufpool/src/device.rs`——實作者可以保留記憶體以維持註冊有效，但**不得把記憶體暴露給呼叫端、不得獨立於 pool 的個別配置去存取它的位元組**。安全的應用層 `Device` wrapper 只提供 registrar 參照，拿不到 backing memory。這讓「哪段程式碼可以把記憶體交給 DMA」變成型別系統強制的邊界，而不是靠註解與 code review。3FS 在 C++ 裡沒有等價機制（唯一入口 `IBDevice::regMemory` 是靠約定，不是靠編譯器）。

**`max_inline_data = 0` 兩者都有**，兩邊都白白讓 NIC 對每一個小 WR 多做一次 PCIe read。這是這份對照裡唯一「兩邊都該修」的項目。

---

## 8. 軸 G：准入與限速

### 3FS：三層計數型 semaphore

| 層 | 位置 | 容量 | 單位 |
|---|---|---|---|
| `RDMATransmissionLimiter` | `RDMAControl.h:44` | 64 | 請求數（**預設關閉**） |
| per-device semaphore | `StorageOperator.h:39-40` | 256 | 批次數 |
| `IBSocket::rdmaSem_` | `IBConnect.cc:719` | 128 | WR 數 |

**三層全部數「個」。** 一個 WR 可能是 4 KiB 也可能是數 MiB，同樣 128 個 token 可能代表 512 KiB 在飛、也可能是 512 MiB，差三個數量級。

### ruapc：計數 + 位元組雙軌

- **計數軌**：`max_inflight_read_wrs`（預設 32）是**每個本地 NIC 共享**的 in-flight READ WR 上限，再加上每 QP 自己的 SQ READ 容量（`rdma_socket/read.rs:94-97` 同時取 `sq` 與 `device` 兩個 permit）。READ 記帳表每 QP 分四個 shard，沒有 outstanding READ 的 QP 直接跳過逾時掃描。
- **位元組軌**：`rate_limiter.rs` 是一個 **GCRA（Generic Cell Rate Algorithm）限速器**——單一原子的 theoretical-arrival-timestamp，無鎖；依 **port 標稱頻寬 × `bandwidth_limit_ratio`（預設 0.95）** 設速率，**收送各一份獨立預算**，`reserve_within` 會算出需要等多久，超過 `max_wait`（預設 1s）就不預約直接拒絕。

`3FS-RDMA機制優化可能性調研.md` §5「流控的計量單位錯了」建議 3FS 改成「位元組配額 + 最小請求數保底」的雙重門檻——**ruapc 就是這個形狀**，而且它的 GCRA 實作（單一 `AtomicU64`、無鎖、支援單一大請求的 burst 容忍）可以直接當參考。

**ruapc 更優，且是 3FS 可以最低成本借鑑的一項。**

---

## 9. 軸 H：握手與連線生命週期

### 3FS：極簡的非對稱握手

TCP 上兩個 RPC（`IBConnect` serviceId 11，query=1 / connect=2），交換 qp_num / mtu / lid或gid。**client 走完整 INIT→RTR→RTS 並發 connect probe；server 只到 RTR（state=ACCEPTED），收到 probe 才轉 RTS**（`IBConnect.cc:487-510`、`IBSocket.cc:512-522`）。這個非對稱性是為了避免「server 先發資料但 client 還沒 post recv → RNR」，在 `rnr_retry = 0` 的前提下是必要的。

優點：**簡單、可 tcpdump 除錯、失敗語意清楚**。缺點：沒有協商，config 由 client 單方面決定（`IBConnectReq.config`）並由 server 照單全收；沒有明確的失敗回滾協定，靠 `acceptTimeout_`（15 秒）兜底。

### ruapc：版本化 bootstrap + lease 狀態機

`docs/rdma-connection.md` 描述的是一套完整的三階段協定：

1. **discover**：回傳 protocol version（必須完全相同，沒有舊版相容分支）、裝置／port／GID、負載、limits。advertisement 快取 30 秒。
2. **prepare_connection**：發起端先**雙向協商 limits**（`send = min(L.max_send_wr, R.max_recv_wr)`、`recv = min(L.max_recv_wr, R.max_send_wr)`、`ring`、`message` 各取交集），接收端按自身當下能力**再協商一次**並回傳實際值；發起端**驗證對稱性**（對方的 `max_send_wr` 必須等於自己的 `max_recv_wr`，`recv_queue_len` 與 `max_msg_size` 必須完全一致），不一致就握手失敗回滾。
3. **commit_connection**：接收端進入 Confirmed，開始計激活期限。

外加一個四態 lease 狀態機（`Pending / ReceiveObserved / Confirmed / Active`），各有獨立期限與到期動作，`cancel_connection` 依完整身分比對且冪等，獨立 sweeper 按 `clamp(lease/4, 100ms, 1s)` 檢查。失敗時 `BootstrapRollback` guard 負責「本地 QP 置 ERR + 用獨立的清理 context 發 TCP cancel」，cancel 也失敗時才退回讓接收端的 lease 過期回收。

還有一個 3FS 完全沒有的概念：**發布的原子性**。首次建立 `connections_per_peer` 條 stripe 時，全部 `EstablishedSocket` 都保留回滾 guard，**直到每一條都成功且發布前健康檢查通過，才一次替換 `active` 集合**；任一條失敗就回滾所有尚未發布的連線。

### 判斷

ruapc 的協定明顯更嚴謹，尤其是**雙向 limits 協商 + 對稱性驗證**——3FS 的單向 config 下發意味著兩端若對 `send_buf_cnt` 的理解不一致（例如滾動升級期間），信用會直接算錯而沒有任何檢查攔得住。

但要公允地說：ruapc 的複雜度也高得多（`rdma_socket_pool/` 有 9 個檔案、3,800 行，其中 `placement.rs` 就 1,129 行）。3FS 的握手是幾百行、一眼看得完。**這是「複雜度預算花在哪裡」的選擇**，不是單純的優劣。3FS 真正該借的是那條最便宜的：**在握手時把 `send_buf_cnt` / `buf_ack_batch` / `buf_signal_batch` 做一次對稱性與整除性驗證**，這是十幾行的事，能擋掉 §5 提的靜默停擺。

---

## 10. 軸 I：完成識別與 QPN 重用安全

這一軸 ruapc 的處理深度明顯超出 3FS，而且是共享 CQ 的**前提條件**。

### 3FS：`wr_id` 直接帶指標

`IBSocket::WRId` 用 `folly::StampedPtr` 把 16-bit type + 48-bit 指標塞進 64-bit `wr_id`（`IBSocket.h`）。因為**一個 CQ 只服務一個 QP、且 CQ 與 QP 的生命週期綁在同一個 `IBSocket` 物件裡**，所以 `wr_id` 裡的裸指標永遠有效——這個前提一旦被共享 CQ 破壞，模型就崩了。

### ruapc：四元組身分 + QPN lease + retired floor

`docs/wrid.md`（492 行）定義完成身分是 `(CQ, hardware QPN, WRType, sequence)`：

- WRID 只帶 **2-bit type + 62-bit sequence**（`(sequence << 2) | type`），**不含任何路由 slot 或 generation**；QPN 由 CQE 自己提供。
- 每個 QP 向 CQ 註冊它的硬體 QPN，取得一個**不可複製**的 `IdentityLease`（含 `first_sequence` 底線），持有到 **provider 銷毀成功**為止。
- CQ 維護一個 **`retired_floor`**：QP 銷毀時 `retired_floor = max(retired_floor, send_next, recv_next, first_sequence + 1)`。所以後來重用同一個 QPN 的新 QP，起始 sequence 必然高於舊佔用者曾發出的所有 sequence——**舊完成永遠落在新身分的 floor 之下而被拒絕**，不需要存任何歷史 QPN。
- SQ 與 RQ 各有自己的 sequence 串流，取 max 而不是相加（兩者共用 CQ 時這一點是必要的）。
- `CompletionQueue::poll_batch` 出借的是**不可複製的 completion token**，`QueuePair::complete` 消費 token 時會驗證來源 CQ、QPN、sequence floor，全部通過才動用擁有權表。**複製出來的 WRID、`CompletionIdentity` 或裸 CQE 都不是有效憑證。**
- 銷毀失敗時**直接 abort 行程**，而不是釋放仍可能被 DMA 存取的資源。

它甚至處理了一個極窄的競態：provider 已經回收了舊 QPN、但舊的 Rust lease 還沒退休時，新 QP 註冊會失敗並乾淨地回滾，**不會繼承舊的 floor**。

### 判斷

3FS 目前的做法在「一 CQ 一 QP」的前提下是正確且零成本的。但 `3FS-RDMA機制優化可能性調研.md` §2 建議的共享 CQ 一旦要做，**就必須先解決這一軸**——而 ruapc 的 `(CQ, QPN, WRType, sequence) + retired floor` 是一個已經寫成文件、有 65537 條獨立路由測試撐著的完整答案。**這是 3FS 做共享 CQ 時最該先讀的一份材料。**

---

## 11. 軸 J：多路徑與多軌

### 3FS：wire 格式領先，排程落後

- **wire 格式領先**：`RDMARemoteBuf::rkeys_` 是 `std::array<Rkey, kMaxDeviceCnt>`（`RDMABuf.h:39-130`），一塊遠端緩衝**同時帶最多 4 個裝置的 rkey**，對端可以用任一張 HCA 去存取它。`RDMABuf` 對所有裝置全註冊。這是**為多軌準備好的資料結構**。
- **排程落後**：`selectDevice`（`IBConnect.cc:280-297`）在**連線建立時**用一個 round-robin 計數器挑一個裝置就固定；`TransportSet::kDefaultMaxConnections = 1`，所有 `configs/*.toml` 也都是 1。所以**一對節點之間只有一條 QP，走一張 HCA**，多軌只靠「對不同 peer 挑不同裝置」在統計上攤開。
- `attr.ah_attr.grh.flow_label = 0`（`IBConnect.cc:654`）→ RoCE 的 ECMP 熵不足。

### ruapc：排程領先，wire 格式落後

- **排程領先**：`placement.rs`（1,129 行）依「連線域匹配、link 類別、本地連線數、遠端 advertisement 裡的負載」列舉候選路徑並選擇；`min_connections_per_remote_nic` 保證每張遠端 NIC 的最低連線數；`maintenance.rs` 每 5 秒做背景**負載再平衡與連線遷移**（`rebalance_threshold`、`drain_timeout_ms`），遷移時先握手替代連線、更新集合、再排空舊連線。握手失敗的 NIC 對會被記為 30 秒的失敗路徑。
- **wire 格式落後**：`RemoteBufferInfo`（`ruapc-bufpool/src/key.rs`）只帶**一個** `MemoryKey`。`remote_buffer_info(device_index)` 在廣告時就得決定用哪個裝置的 rkey——**廣告出去的遠端緩衝綁死在一張 NIC 上**。連線若遷移到另一張 NIC，那個 rkey 就不能用了。

### 判斷

**兩邊各贏一半，而且剛好互補。** 3FS 有多軌需要的 wire 格式卻沒有用；ruapc 有多軌需要的排程卻被 wire 格式限制住。

對 3FS 而言結論不變：`3FS-RDMA機制優化可能性調研.md` §9 說的「最大的未兌現設計紅利」依然成立——**底層資料結構已經支援多軌，缺的只是把一個批次拆到多個 IBSocket 的排程邏輯**，而那個排程邏輯的形狀可以參考 ruapc 的 `placement.rs` + `maintenance.rs`。

兩者也都只做到**請求層級**的負載平衡（3FS 的 `TransportSet::acquire(idx)`、ruapc 的 `next_stripe.fetch_add` round-robin），**都沒有把單一大傳輸條帶化到多條 QP**。

---

## 12. 軸 K：錯誤處理與可測性

| | 3FS | ruapc |
|---|---|---|
| `rnr_retry` | **0**（任何 RNR 立刻殺 QP） | 由 `QpConnectionConfig` 驗證後設定，非固定 0 |
| 部分投遞失敗 | 只有最後一個 WR signaled，無法追蹤 → **直接把 QP 打死**（`IBSocket.cc:1073-1095`，註解誠實說明） | `WrSlots` 事前預留 slot，投遞失敗回滾 buffer/READ 註冊但**不回滾 sequence**（刻意允許 gap） |
| 銷毀失敗 | `XLOGF(CRITICAL)` 後繼續 | **abort 行程**，理由是資源仍可能被 DMA 存取 |
| 連線層錯誤 | WC 錯誤 → `state_ = ERROR` → 整條連線拆掉，pending 請求全部 `kTimeout` | 同樣是連線層級，但有 `ErrorKind` 分類保留階段資訊（prepare / 驗證 / QP 連接 / 註冊 / commit） |
| 形式化驗證 | **有**：`specs/RDMASocket/` 的 P 模型（QP、Network、credit 交換，721 行） | 無，但有 534 個測試 + 逐次改動的成對量測報告（`docs/*.md` 共 2,105 行） |
| 可觀測性 | `monitor::` recorder（`common.ib.*`、`common.transmission.*`） | `tracing` span（`RUST_LOG=ruapc::rdma=debug`），文件附「哪個訊息看哪些欄位」對照表 |

兩邊的驗證哲學不同但都紮實：**3FS 用形式化模型驗協定、ruapc 用大量測試 + 嚴謹的成對量測方法論驗實作**。ruapc 的量測報告值得一提的是它的**誠實度**——`docs/rdma-capacity.md` 明確保留了負面樣本（1 MiB READ 初始 5 對量到 −11.85%，獨立 10 對量到 +12.81%，並直說「the observed variability prevents a precise small-effect estimate」），也標明 loopback 結果不能外推。這種寫法在效能報告裡相當罕見。

---

## 13. 兩邊互相可以借鑑什麼

### 3FS 可以從 ruapc 借（依效益／風險比排序）

1. **信用捎帶在 `imm_data`**（§5）——取消專用 ACK 訊息與 `ackBufAvailable_` 整套節流，回收 8 格 recv WQE。`flow.rs` 的雙 16-bit 編碼與 7 個單元測試可以直接當藍本。改完用 `specs/RDMASocket/` 的 P 模型驗證。
2. **信用回收的 keepalive + 低水位觸發**（§5）——直接消滅 `3FS-RDMA機制優化可能性調研.md` §7 F-2 指出的靜默停擺，且不需要依賴「設定值必須整除」這種脆弱前提。
3. **GCRA 位元組級限速**（§8）——把三層「數個」的 semaphore 換成「位元組 + 計數」雙軌。單一 `AtomicU64`、無鎖，移植成本低。
4. **混合輪詢的兩相位結構**（§4）——3FS 已經把 arm-then-drain 那一步做對了，缺的只是前面的 spin 階段。但要先處理「event loop 執行緒還要服務其他 fd」的結構問題。
5. **握手時做對稱性驗證**（§9）——十幾行，擋掉滾動升級期間兩端信用參數不一致的整類問題。
6. **共享 CQ 的兩個前提**（§3 + §10）——軟體完成信用准入、以及 `(CQ, QPN, WRType, sequence) + retired floor` 的完成身分。這兩份是 3FS 要做共享 CQ 之前必讀的材料。
7. **正視 client 逾時後緩衝被重用的風險**（§6）——不必改成 pull 模型，但至少讓 `RDMABuf` 的擁有權延後到 CQE 才釋放。

### ruapc 可以從 3FS 借

1. **多 rkey 的 wire 格式**（§11）——`RemoteBufferInfo` 改成帶一組 `(devId, rkey)`，遠端緩衝就不再綁死在一張 NIC 上，連線遷移與多軌才有意義。ruapc 的 pool 本來就對每個裝置都註冊了，只差把它們廣告出去。
2. **形式化模型**（§12）——ruapc 的 `flow.rs` 已經複雜到值得一個 P/TLA+ 模型；測試涵蓋不了的是「任意交錯下不會死鎖」這類性質。
3. **`max_inline_data`**（§7）——兩邊都是 0，兩邊都該修。

---

## 14. 一句話總結

**ruapc 是「把 RDMA 傳輸層當成一個獨立的、要被證明正確的系統」來寫的**：共享 CQ + 軟體信用准入、混合輪詢、捎帶式信用、位元組級限速、四元組完成身分、lease 握手狀態機、`forbid(unsafe_code)` 的核心——每一項都有文件、有量測、有負面樣本。代價是 14,000 行與很高的概念密度。

**3FS 是「把 RDMA 當成 TCP 的一個更快的替身」來寫的**：換一個 `Socket` 實作，上層一行不改。這個決定讓它用 4,700 行拿到了生產可用的 RDMA，並且正確地做對了非對稱握手、WR linked-list 批次、多裝置 rkey 這些真正關鍵的事。代價是每 RPC 兩次 memcpy、每連線一套資源、以及一組**在出廠設定下剛好正確、但沒有結構性保護**的信用參數。

兩者的差距**不在於誰更懂 RDMA，而在於複雜度預算花在哪裡**。3FS 要往上走，不需要變成 ruapc；只需要把上面第 13 節前三項（捎帶信用、keepalive 觸發、位元組限速）補進去——那是幾百行的事，卻能同時消掉一個潛在的靜默停擺、一批無謂的 ACK WR，和一個意義不明的擁塞訊號。

---

## 附錄：對照檔案索引

| 主題 | 3FS | ruapc |
|---|---|---|
| QP / CQ 建立 | `src/common/net/ib/IBConnect.cc:547-600` | `ruapc-rdma/src/verbs/queue_pair.rs`、`completion_queue.rs` |
| 容量／准入計算 | `src/common/net/ib/IBConnect.h:101-106` | `docs/rdma-capacity.md`、`ruapc/src/rdma/poller/budget.rs` |
| 完成輪詢 | `src/common/net/ib/IBSocket.cc:325-420` | `ruapc/src/rdma/poller/mod.rs` |
| 信用流控 | `IBSocket.cc:512-560, 664-706, 836-886` | `ruapc/src/rdma/poller/flow.rs` |
| 送出路徑 | `IBSocket.cc:611-663` | `ruapc/src/rdma/rdma_socket.rs:188-305` |
| 連線狀態 | `IBSocket.cc`（State enum） | `ruapc/src/rdma/poller/conn.rs` |
| wire framing | 無（byte stream） | `ruapc/src/rdma/frame.rs` |
| bulk 傳輸 | `StorageOperator.cc:176-225, 563-595` | `ruapc/src/rdma/rdma_socket/read.rs`、`ruapc/src/services/memory_service.rs` |
| 遠端緩衝 wire 型別 | `src/common/net/ib/RDMABuf.h:39-130` | `ruapc-bufpool/src/key.rs` |
| 准入／限速 | `src/common/net/RDMAControl.*`、`StorageOperator.h:39-40` | `ruapc/src/rdma/rate_limiter.rs`、`rdma_socket/read.rs:94-97` |
| 握手 | `src/common/net/ib/IBConnect.cc:338-510` | `ruapc/src/rdma/rdma_socket_pool/{handshake,setup,accept,connect}.rs`、`docs/rdma-connection.md` |
| 完成識別 | `src/common/net/ib/IBSocket.h`（`WRId`） | `ruapc-rdma/src/types/wrid.rs`、`docs/wrid.md` |
| 多路徑 | `IBConnect.cc:280-297` | `ruapc/src/rdma/rdma_socket_pool/placement.rs`、`maintenance.rs` |
| 記憶體池 | `RDMABuf.h`、`IBDevice.cc:592` | `ruapc-bufpool/`（buddy + 每裝置註冊） |
| 設定預設值 | `configs/*.toml`、`IBSocket.h`（Config） | `ruapc/src/rdma/config.rs` |
| 驗證材料 | `specs/RDMASocket/`（P 模型） | `docs/{wrid,rdma-capacity,rdma-connection,safe-boundaries,qp-registry}.md` |
