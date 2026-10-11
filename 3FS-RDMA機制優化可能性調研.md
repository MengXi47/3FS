# 3FS RDMA 機制優化可能性調研（邏輯層面）

> 目的：不談參數調校的皮毛，而是逐一檢視 3FS RDMA 子系統的**設計決策**，問「這個決策的代價是什麼、有沒有更優的邏輯結構、換掉的風險在哪」。
>
> 範圍：`src/common/net/ib/`、`src/common/net/RDMAControl.*`、`src/common/serde/CallContext.*`、`src/storage/service/StorageOperator.cc`、`src/client/storage/StorageClientImpl.cc`、`configs/*.toml`。
>
> 對照文件：`3FS-RDMA編程深度剖析.md`（現況是什麼）。本文是「現況該不該是這樣」。

---

## 0. 結論摘要

3FS 的 RDMA 層在**正確性結構**上做得相當乾淨：控制平面走 TCP、非對稱握手避 RNR、WR linked-list 只簽最後一個、三層流控、多 HCA 全註冊。這幾項我認為**不應該動**，動了只會變糟（見 §12）。

真正有改進空間的是另外三類問題：

1. **每連線私有資源模型**（CQ / completion channel / 1.14 MiB 註冊緩衝區 / QP 全部一對一）——這是 RC 的經典 N×M 爆炸，在 500 client × 180 storage 的規模下開始咬人。
2. **延遲路徑上的冗餘等待**——讀路徑白等一個 RTT（等 RDMA_WRITE 的 CQE 才發回應）；小讀取的 inline 路徑預設關閉；`max_inline_data = 0` 讓最小的訊息也要走 DMA read。
3. **流控的計量單位錯了**——三層 semaphore 全部以「WR 個數」計量，而不是位元組。4 KB 的 WR 和 1 MiB 的 WR 佔用同一個 token，對 incast 與擁塞控制而言這個訊號幾乎沒有意義。

優先序（效益／風險比由高到低）：

| # | 項目 | 類型 | 預期效益 | 風險 | 改動範圍 |
|---|------|------|---------|------|---------|
| 1 | 小讀取走 inline（`max_inline_read_bytes` 預設 0） | 設定 | 小 IO 延遲 −30~50% | 低 | 設定檔 |
| 2 | `max_inline_data` 從 0 改成 64~256 | 單行 + 驗證 | 控制平面小訊息少一次 DMA read | 低 | `IBConnect.cc:590` |
| 3 | credit / signal 餘數防呆（`==` → `>=` + 耗盡時 flush） | 健壯性 | 消除設定錯誤導致的永久停擺 | 低 | `IBSocket.cc:688,847` |
| 4 | RoCE `flow_label` 隨機化、`traffic_class` 給預設 DSCP | 設定 + 數行 | ECMP 路徑分散、PFC/DCQCN 分類正確 | 低 | `IBConnect.cc:654,657` |
| 5 | 混合輪詢（busy-poll N µs 後才 re-arm） | 中 | 高負載下省掉每批一次 syscall + 中斷 | 中 | `IBSocket::poll` / `EventLoop` |
| 6 | write-then-send：讀路徑省一個 RTT | 中 | 讀延遲 −1 RTT（4~10 µs） | 中高 | `StorageOperator.cc:176-225` |
| 7 | ACK 捎帶在資料 SEND 的 `imm_data` | 中 | 省掉專用 ACK WR 與其佔用的 recv WQE | 中 | `IBSocket` 收發路徑 |
| 8 | 流控改以位元組計量 | 中 | incast / 擁塞訊號才有意義 | 中 | 三層 semaphore |
| 9 | `num_event_loop > 1` + CQ 分片 | 中 | 單執行緒 CQ drain 不再是天花板 | 中 | 設定 + IOWorker |
| 10 | SRQ + 共享 CQ | 架構 | 記憶體 O(N)→O(1)、fd 數 O(N)→O(1) | 高 | `IBSocket` 資源模型 |
| 11 | DC transport (DCT) 取代 RC | 架構 | 徹底解 N×M QP 爆炸 | 高 | 需 mlx5dv，失去可攜性 |
| 12 | 接收端 grant 捎帶（取代 `apply` 額外 RTT） | 架構 | 有 incast 保護又不付 RTT | 高 | RDMAControl 協定 |
| 13 | GPUDirect RDMA / DMABUF 註冊 | 架構 | AI 訓練場景省掉 host bounce | 高 | RDMABuf + USRBIO |

---

## 1. 先把成本模型算清楚

要判斷「更優」必須先量化現況。以出廠設定（`configs/storage_main.toml`、`buf_size=16384`、`send_buf_cnt=32`、`buf_ack_batch=8`、`max_rdma_wr=128`）代入 `IBConnect.h:101-106`：

```
qpAckBufs      = (32 + 8 - 1) / 8 + 4          = 8
qpMaxSendWR    = 32 + 8 + 128 + 1 + 1          = 170
qpMaxRecvWR    = 32 + 8 + 1                    = 41
qpMaxCQEntries = 170 + 41                      = 211
```

`initBufs`（`IBConnect.cc:724-737`）配置 `numSend + numRecv = 32 + 41 = 73` 個 16 KiB 緩衝：

**每條 IBSocket 連線的固定成本：**

| 資源 | 數量 | 備註 |
|------|------|------|
| 註冊記憶體（pinned） | **1.14 MiB** | 73 × 16 KiB，`ibv_reg_mr` 鎖頁 |
| `ibv_mr` | 1 | NIC 端 ICM / MTT 條目 |
| `ibv_cq` | 1（211 CQE） | `IBConnect.cc:564` |
| `ibv_comp_channel` | 1（**佔一個 fd**） | `IBConnect.cc:549` |
| `ibv_qp` | 1（170 SQ + 41 RQ WQE） | `attr.srq = nullptr`，`IBConnect.cc:585` |

而 `TransportPool.h:17` 的 `kDefaultMaxConnections = 1`、所有 `configs/*.toml` 也都是 `max_connections = 1`，所以連線數 ≈ **對端行程數**。

以 3FS 論文的叢集規模（180 storage node、數百 client node）估算，單一 storage 行程面對 500 個 client：

- pinned memory：500 × 1.14 MiB ≈ **570 MiB**（純粹是 socket 模擬用的 bounce buffer，還沒算 `rdmabufPool` 的資料緩衝）
- epoll 中的 fd：**500 個 completion channel**，全部掛在 **一個** event loop 執行緒上（`configs/storage_main.toml:110` `num_event_loop = 1`）
- QP：500 個，每個 QP 的 context 在 NIC cache 中競爭 → QP cache miss 造成的效能懸崖是 RC 的已知病灶

這個成本模型是後面所有討論的基準。**規模到 5000 client 時，pinned memory 5.7 GiB、單執行緒 5000 個 fd，設計就撐不住了。**

---

## 2. 軸 A：每連線私有 CQ + completion channel

### 現況

`qpCreate`（`IBConnect.cc:547-600`）為**每條連線**建立獨立的 comp channel 與 CQ，send_cq 與 recv_cq 指向同一個 CQ，`attr.srq = nullptr`。`Transport::handleEvents`（`Transport.cc:344-346`）在 event loop 執行緒裡呼叫 `IBSocket::poll`，而 `poll`（`IBSocket.cc:325-360`）的序列是：

```
ibv_get_cq_event()   ← read() syscall on channel fd
ibv_poll_cq()        ← drain
ibv_req_notify_cq()  ← MMIO write，重新 arm
ibv_poll_cq()        ← 再 drain 一次，關掉 arm 與 drain 之間的競態窗口
```

### 代價

1. **每條連線一次中斷 + 一次 syscall。** 一個 storage node 上 500 條連線同時有流量，就是 500 個獨立的中斷源與 500 次 `read()`。這正是 RC + comp channel 模式最貴的部分。
2. **單執行緒瓶頸。** `num_event_loop = 1`，所有 CQ 的 drain 都在同一個執行緒。這個執行緒不只要處理 socket 的 recv 完成，還要處理**所有 RDMA bulk 傳輸的完成**（`onRDMAFinished` → `ctx.finish()` → 喚醒協程）。400 Gbps × 4 MiB IO = 12 K 完成/秒還好，但 4 KiB 小 IO 的場景下是 12 M 完成/秒，單執行緒必死。
3. **記憶體 O(N)。** §1 已算。

### 更優解

**（a）共享 CQ（優先做）**：CQ 可以被多個 QP 共用。改成「每個 event loop 執行緒一個 CQ + 一個 comp channel」，QP 建立時指定該 CQ，`wr_id` 已經用 `folly::StampedPtr` 打包了型別與指標（`IBSocket.h` 的 `WRId`），只要再把 `IBSocket*` 塞進 `qp_context` 或用 `wc.qp_num → IBSocket` 的映射表，就能從共享 CQ 分派回各連線。

- 效益：fd 數 O(N) → O(執行緒數)；中斷數暴跌；CQ 記憶體從 N×211 CQE 變成 1×(大 CQ)。
- 風險：**CQ overrun 變成真正的風險**。現在的程式碼有一句註解「We pre-allocate enough size for CQ, so CQ overrun shouldn't happen!」（`IBSocket.cc:352`），這個保證建立在「CQ 只服務一個 QP、容量 = 該 QP 全部 WQE」之上。共享 CQ 後必須改成動態計帳：每個 QP 掛上共享 CQ 時把 `qpMaxCQEntries()` 加進總帳，超過就換一個 CQ。這個帳要做對，不然是 silent data loss。
- 附帶好處：`ibv_poll_cq(cq, 16, ...)` 一次能撈到跨連線的完成，batching 效率提升。

**（b）SRQ（接著做）**：`attr.srq = nullptr` 表示每個 QP 有自己的 41 個 recv WQE 與對應的 41 × 16 KiB 緩衝。改用 SRQ 後，一個裝置共用一池 recv 緩衝，記憶體從 `N × 41 × 16KiB` 降到 `pool_size × 16KiB`。

- 阻礙：**現有的 credit 協定與 recv 緩衝是綁死的**。`recvBufs_` 用 `bufIndex` 定位緩衝（`WRId::recv(idx)`，`postRecv(idx)`），ACK 的信用單位也是「緩衝格數」。換 SRQ 要把 credit 從「每連線固定 32 格」改成「從共享池借用」，也就是說 credit 協定要從**靜態配額**改成**動態借還**。這是實質的協定改動，不是換個 API。
- 折衷方案：保留 per-QP RQ，但把 recv 緩衝改成從共享池 lazy 分配（只在真的有流量時才綁）。對「大量閒置連線」的場景（3FS 的典型情況：client 連著所有 storage node，但同時只跟少數幾個有流量）收益很大，複雜度遠低於 SRQ。

**（c）DC transport（DCT）**：mlx5 的 Dynamically Connected transport 讓一個 DCI（initiator）能對任意多個 DCT（target）發起操作，QP 數從 O(N×M) 降到 O(執行緒數)。這是解 RC 爆炸的**正統答案**（NVMe-oF、UCX、NCCL 都走這條路）。

- 代價：需要 `mlx5dv` 私有 API（`grep` 全庫目前**零使用**），失去跨廠商可攜性；DC 的連線建立是隱式的，第一次通訊有額外延遲；DC 不支援某些 verbs 語意。
- 判斷：**3FS 現階段不值得**。3FS 的連線數是「行程數」而非「執行緒數」，500~1000 個 QP 還在 RC 的舒適區。等到規模破萬，或是要支援 client 側多行程（例如每個訓練 rank 一個行程）時才需要。先做 (a) + (c的折衷方案)，把天花板從 1000 拉到 10000，比直接跳 DCT 划算。

---

## 3. 軸 B：純中斷驅動的完成通知

### 現況

`EventLoop.cc:122` 是 `epoll_wait(epfd_, events, kMaxEvents, -1)`——**無限阻塞**。完成通知的唯一來源是 comp channel 的中斷。全庫沒有任何 busy-poll 路徑。

### 代價

RDMA 的延遲優勢有一大半會被中斷吃掉。一次完成的端到端路徑是：

```
NIC 寫 CQE → NIC 發中斷 → 核心 IRQ handler → eventfd 喚醒
→ epoll_wait 返回 → ibv_get_cq_event (read syscall) → ibv_poll_cq
```

中斷 + 排程喚醒的成本在 3~10 µs 量級，而 RDMA 本身的線上延遲是 1~3 µs。**通知機制比傳輸本身還貴。**

### 更優解

**混合輪詢（hybrid polling）**，這是所有高效能 RDMA 系統的標配：

```
poll 迴圈：
  busy-poll CQ 若干微秒（例如 5~20 µs，或直到連續 K 次空轉）
  仍有流量 → 繼續 busy-poll，完全不 arm，不進 epoll
  空轉超過門檻 → ibv_req_notify_cq + 回到 epoll_wait
```

- 效益：高負載下 **完全消除** comp channel 的 syscall 與中斷；低負載下行為與現在相同（不燒 CPU）。
- 這也順便解決 §2 提到的「每連線一次 syscall」——busy-poll 模式下連 `ibv_get_cq_event` 都不用呼叫。
- 風險：CPU 佔用。要有 adaptive 門檻（類似 NAPI / DIM），並且 busy-poll 的執行緒要與 IO 執行緒分離，否則會餓死其他工作。
- 配套：`read_write_rdma_in_event_thread = false`（`configs/storage_main.toml`）目前讓 poll 之後的 recv/copy 工作再丟到 IO 執行緒池，又多一次排程跳躍。若做了 busy-poll，這個 flag 應該對 RDMA 改成 `true`，讓完成處理在同一個執行緒內一路做完。

**這一項的效益／風險比在我看來僅次於前四項的設定調整，是最值得投資的「中等改動」。**

---

## 4. 軸 C：讀路徑白等一個 RTT（write-then-send）

### 現況

`StorageOperator::batchRead`（`StorageOperator.cc:176-225`）：

```cpp
auto writeBatch = ctx.writeTransmission();
batch.addBufferToBatch(writeBatch);
...
SemaphoreGuard guard(rdmaSemaphoreIter->second);
co_await guard.coWait();
auto postResult = co_await writeBatch.post();   // ← 等 RDMA_WRITE 的 CQE
...
co_return rsp;                                   // ← 才發回應
```

`writeBatch.post()` 最終落到 `IBSocket::rdmaPost`（`IBSocket.cc:1004-1024`），`co_await ctx.baton` 要等最後一個 WR 的 signaled CQE。**而 RC 的 send 完成 CQE 意味著對端 NIC 的 ACK 已經回來了——那就是一整個 RTT。**

然後才把回應塞進 socket 送出去，client 再收到。時序：

```
server:  [RDMA_WRITE] ──────► client NIC 落地
              ◄────────────── ACK          (½ RTT)
         CQE → 協程恢復 → makeResponse → [SEND]
                        ──────────────────► client 收到回應  (½ RTT)
```

### 更優解：利用 RC 的順序保證

InfiniBand RC 保證**同一個 QP 上的請求依序被對端處理**。所以：

```
server:  [RDMA_WRITE (unsignaled)] [SEND 回應] ← 同一次 ibv_post_send 或連續兩次
                        ──────────────────► client 收到回應時，資料必然已在記憶體
```

client 收到回應的 receive completion，本身就證明前面的 RDMA_WRITE 已經完成落地。這是 FaRM、eRPC、NVMe-oF 都在用的標準技巧，**直接省掉一整個 RTT（4~10 µs）**。

對 4 KiB 小讀取（總延遲 30~80 µs，含 NVMe）而言是 **10~25% 的延遲改善**；對命中 page cache 的讀取，改善幅度更大。

### 但是有三個必須處理的細節（這是真正的難點）

1. **緩衝區生命週期。** 現在 `co_await post()` 回來才釋放 `rdmabufPool` 的 buffer 與 device semaphore。改成不等 CQE 後，buffer 的擁有權必須移交給 completion handler（`onRDMAFinished`）。這是把「協程持有」改成「完成回呼持有」，是實質的重構。
2. **錯誤回報語意改變。** 現在 RDMA 失敗會把 `lengthInfo` 設成錯誤，client 收到明確的 per-IO 錯誤。改了之後，RDMA 失敗 → QP 進 ERROR → 後續 SEND 被 flush → client 收不到回應 → 超時重試。**降級為「連線層級失敗」**。3FS 的 client 本來就有 chain 層級的重試（`StorageClientImpl`），所以語意上可接受，但要確認重試路徑不會被放大（一條連線壞掉會讓該批次全部重來）。
3. **`IBV_ACCESS_RELAXED_ORDERING` 的疑慮（關鍵）。** `RDMABuf.h:244` 註冊 client 端目標緩衝時帶了 `IBV_ACCESS_RELAXED_ORDERING`。Relaxed ordering 允許 NIC 對 PCIe 寫入重排以提升頻寬，代價正是**放棄「後續操作的完成隱含先前資料已可見」這個保證**。要用 write-then-send，就必須：
   - 對這些 MR **關掉** RELAXED_ORDERING（會損失一些 PCIe 頻寬，尤其在有 IOMMU 的機器上），或
   - 在 WRITE 與 SEND 之間插入 fence（`IBV_SEND_FENCE`），但 fence 會序列化，可能吃掉一部分收益，或
   - 實測驗證目標平台上的順序行為（**不建議**，這是靠未定義行為賺效能）。

   **這一點必須先驗證再動手。** 我的建議是先在測試環境量測「關掉 RELAXED_ORDERING 的頻寬損失」vs「省掉一個 RTT 的延遲收益」，兩者在小 IO 場景下大機率是後者贏、大 IO 場景下是前者贏——那就按 IO 大小分流：小 IO 走 write-then-send + 嚴格 ordering，大 IO 維持現狀。

### 一個零風險的替代方案：先把 inline 開起來

`StorageClientImpl.cc:705` 的條件是 `requestedBytes < max_inline_read_bytes()`，而 `StorageClient.h:422` 的預設值是 **`Size{0}`**——**inline 路徑預設完全關閉**。也就是說今天連 512 B 的讀取，都要走「RDMA_WRITE + 等 CQE + 發回應」的完整流程。

對小於 ~4 KiB 的讀取，把資料直接塞進回應訊息（`batch.copyToRespBuffer`，`StorageOperator.cc:176-177`）幾乎必然比單向 RDMA 快：省掉一次 RDMA 往返、省掉 device semaphore 排隊、省掉 `rdmabufPool` 分配。代價只是一次 memcpy 進 16 KiB 的 socket bounce buffer。

**這是整份調研裡效益／風險比最高的一項，而且只要改設定。** 建議先量測 `max_inline_read_bytes = 4KB / 8KB / 16KB` 的延遲曲線。寫入側的 `max_inline_write_bytes` 同理（`StorageClientImpl.cc:1898`）。

---

## 5. 軸 D：incast 管控與流控的計量單位

### 現況：三層 semaphore，全部以「WR 個數」計量

| 層級 | 位置 | 容量 | 計量單位 |
|------|------|------|---------|
| `RDMATransmissionLimiter` | `RDMAControl.h:44` | 64 | **請求數** |
| per-device semaphore | `StorageOperator.h:39-40` | 256 | **批次數** |
| `IBSocket::rdmaSem_` | `IBConnect.cc:719` | `max_rdma_wr` = 128 | **WR 數** |

而且第一層 `RDMATransmissionLimiter` 的觸發條件是 `ctx.packet().controlRDMA()`（`StorageOperator.cc:201,211`），這個 flag 來自 client 的 `enableRDMAControl`（`ClientContext.h:64`）——**而所有出廠設定裡 `enable_rdma_control = false`**（`configs/` 下 18 處全是 false）。

### 兩個獨立的問題

**問題一：接收端驅動的 incast 保護，寫好了卻沒開。**

`RDMAControlImpl::apply`（`RDMAControl.cc:31-43`）是個漂亮的接收端准入控制：storage server 在推資料**之前**，先反向對 client 發一個 `apply` RPC 申請配額；client 端最多發出 64 個配額，配額在該請求的回應送達時釋放（`Waiter.h:78-86`）。這正是 Homa / NDP 那類 receiver-driven 傳輸的核心想法，用來擋 incast：N 個 storage node 同時對同一個 client 的 NIC 灌資料。

沒開的原因很明顯：**它在關鍵路徑上多加一個完整 RTT**。在沒有 incast 的情況下，這個 RTT 是純虧損。

**更優解：把 grant 捎帶進原本的請求／回應，不要用獨立的 RPC。**

- client 發 `BatchReadReq` 時，順便帶上「我現在還有多少接收配額」或「這個請求的准入 token」。
- server 若手上有有效 token 就直接推；沒有才退回去發 `apply`。
- 等於把「一定要問」改成「樂觀執行 + 必要時才問」，常態路徑零額外 RTT，壅塞時才退化成現有行為。
- 更進一步：client 可以在配額回收時**主動**推一個 credit 更新給 server（piggyback 在下一個請求上），做成連續的信用流，完全不用停等。

這在邏輯上就是把「同步 grant」改成「非同步 credit」，與 §7 要對 socket credit 做的事是同一個模式。

**問題二：計量單位錯了。**

三層全部數「個」。一個 WR 可能是 4 KiB，也可能是 `max_msg_sz`（通常 1 GiB，實務上受 `RDMAReqBatch::add` 與 chunk size 限制，但仍可到數 MiB）。同樣 128 個 token，可能代表 512 KiB 在飛，也可能代表 512 MiB 在飛——**差三個數量級**。

對「防止 NIC 排隊爆掉 / 防止 incast」這個目的而言，唯一有意義的單位是**位元組**（或者更精確地說，bytes-in-flight，也就是 BDP 的近似）。

**更優解**：把三層都換成「位元組配額 + 最小請求數保底」的雙重門檻。例如 `rdmaSem_` 從 128 個 WR 改成 `min(128 WR, 4 MiB)`。`folly::fibers::BatchSemaphore` 本來就支援一次取 N 個 token（`rdmaSem_.try_wait(waiter, post.reqs.size())`，`IBSocket.cc:962`），改成取 `bytes / granularity` 個 token 是很直接的改動。

副作用（正面）：位元組配額天然提供**公平性**——大 IO 佔用多、小 IO 佔用少，不會像現在這樣一個 1 MiB 的請求跟一個 4 KiB 的請求搶同一個 token，導致小 IO 的延遲被大 IO 拖累。

---

## 6. 軸 E：小訊息路徑上的兩個浪費

### E-1：`max_inline_data = 0`

`IBConnect.cc:590` 明確設成 0。這代表**每一個** `ibv_post_send`，不論多小，NIC 都必須：

```
讀 WQE → 依 sge 發 PCIe read → 等資料回來 → 才能上線
```

設成 64~256 之後，小於門檻的 payload 直接寫進 WQE，NIC 少一次 PCIe round trip（約 0.5~1 µs）。對控制平面的小 RPC（mgmtd 心跳、meta 的 stat/lookup、ACK/close/check 訊息）是穩賺。

有趣的是，程式裡已經有兩處在用 `IBV_SEND_INLINE`——`check()`（`IBSocket.cc:770`）與 `closeGracefully()`（`IBSocket.cc:822`）——但因為 `num_sge = 0`、長度為 0，所以 `max_inline_data = 0` 也能過。**這兩處其實是「宣告了意圖但沒有效果」的程式碼**，正好說明 inline 的收益被漏掉了。

改動：`attr.cap.max_inline_data = kInlineThreshold`，並在 `postSend` 中當 `len <= kInlineThreshold` 時加上 `IBV_SEND_INLINE`。要注意 inline 會增加 WQE 大小，進而增加 SQ 的記憶體佔用（170 × WQE size），選 128 是常見的平衡點。

### E-2：stream 模擬的兩次 memcpy

`IBSocket::send`（`IBSocket.cc:611-651`）把 payload memcpy 進 16 KiB 的註冊緩衝；`IBSocket::recv`（`IBSocket.cc:664-706`）再 memcpy 出來給上層。每個 RPC 兩次拷貝。

這是「在 RC QP 上模擬 stream socket」這個抽象的固有成本，換來的是**整套 `Transport` 狀態機在 TCP / RDMA 之間完全共用**。我認為這筆交易是划算的（見 §12），不建議為了省這兩次 memcpy 去做零拷貝的訊息式 RPC 重寫。

但有一個**便宜的局部優化**：一個小於 16 KiB 的 RPC 會佔掉整整一格 credit。credit 窗口是 32 格，所以**一條連線最多只能有 32 個未確認的出站訊息**，不管它們是 100 B 還是 16 KiB。對 metadata-heavy 的工作負載（meta service 每秒數十萬次 lookup），這個「以格計量」的窗口是個實際的管線深度限制。

- 更優解：允許一格緩衝裝多個小訊息（訊息聚合 / message coalescing）。`flush()` 的存在（`IBSocket.cc:653-663`）說明架構上已經支援「攢一攢再送」——目前只在 `WriteList` 耗盡時觸發，可以再加上「攢滿 N 個訊息或 T 微秒」的條件。
- 這同時緩解 §7 的 credit 壓力：同樣 32 格能承載更多訊息。

---

## 7. 軸 F：credit 協定的兩個結構問題

### F-1：專用 ACK 訊息吃掉 recv WQE

`postAck`（`IBSocket.cc:865-886`）用 `IBV_WR_SEND_WITH_IMM`、`num_sge = 0`、`imm_data = ImmData::ack(buf_ack_batch)` 送純信用訊息。**`SEND_WITH_IMM` 在對端要消耗一個 recv WQE**，所以 `qpMaxRecvWR` 才要預留 `qpAckBufs() = 8` 格（`IBConnect.h:101,105`），`ackBufAvailable_` 這整套自我節流機制（`IBSocket.cc:552,690`）也是為了不讓 ACK 撐爆對端的 RQ。

**更優解：把 credit 捎帶在資料訊息的 `imm_data` 上。**

現在資料送出走的是 `IBV_WR_SEND`（`IBSocket.cc:853`），沒有 imm。改成 `IBV_WR_SEND_WITH_IMM` 並把「我方目前累積要還你的信用數」填進 `imm_data`，則：

- 雙向都有流量時（RPC 的常態），**完全不需要專用 ACK 訊息**。
- `qpAckBufs()` 預留的 8 格 recv WQE 可以回收給資料用。
- `ackBufAvailable_` 這套節流機制可以整個刪掉。
- 只在「單向流量持續一段時間」時才退回發專用 ACK。

阻礙：`onRecved`（`IBSocket.cc:512-545`）目前假設 **imm 與資料互斥**——`assert(wc.byte_len == 0)`，收到 imm 就當控制訊息、直接 `postRecv` 還格子。要改成「先處理 imm 裡的信用，再照常處理 `byte_len` 的資料」。`ImmData` 的格式（24 bit value + 8 bit type，`IBSocket.h`）本來就夠用。

這是個乾淨的改動，而且 `specs/RDMASocket/` 已經有 P 語言的形式化模型（`PSrc/RDMASocket.p` 556 行、`PSpec/SystemSpec.p` 163 行）可以拿來驗證改完的協定——**改流控協定時有形式化模型當安全網，這是 3FS 少見的優勢，應該用起來。**

### F-2：餘數停擺（latent bug）

兩處計數：

```cpp
// IBSocket.cc:688  —— ACK 側
if (recvNotAcked_ == connectConfig_.buf_ack_batch) { recvNotAcked_ = 0; postAck(); }

// IBSocket.cc:847  —— signal 側
if (++sendNotSignaled_ >= connectConfig_.buf_signal_batch) { signal = sendNotSignaled_; ... }
```

而緩衝釋放需要**兩者同時滿足**（`IBSocket.cc:347`）：

```cpp
auto sendAvailable = std::min(sendAcked_, sendSignaled_);
```

出廠設定 `send_buf_cnt = 32`、`buf_ack_batch = 8`、`buf_signal_batch = 8`，32 是 8 的整數倍，所以 32 個緩衝用完時剛好湊滿 4 個 ACK 與 4 個 signal，餘數為 0，一切正常。

**但 `checkConfig`（`IBConnect.cc:507-528`）只檢查 `buf_ack_batch <= send_buf_cnt`，不檢查整除。** 若有人把 `send_buf_cnt` 調成 30（或把 `buf_ack_batch` 調成 7），則：

- 發送端用完 30 格 → 停等信用
- 接收端收到 30 個 → 只湊出 3 個 ACK（24 格）→ 剩 6 格的信用永遠不還
- 發送端永遠等不到那 6 格 → **連線永久停擺，不是超時、不是錯誤，是靜默卡死**

signal 側同理：`sendNotSignaled_` 的餘數對應的緩衝永遠拿不到 CQE。

三個修法（建議全做）：

1. `checkConfig` 加上 `send_buf_cnt % buf_ack_batch == 0 && send_buf_cnt % buf_signal_batch == 0` 的檢查——**最便宜的防呆**。
2. `recvNotAcked_ ==` 改成 `>=`（防禦性，雖然目前邏輯上不會跳過）。
3. **真正的修法**：當送出緩衝即將耗盡時，強制 flush 一次部分 ACK / 部分 signal。也就是把「定量批次」改成「定量批次 + 低水位觸發」。這同時消除了「接收端讀取很慢時信用回收有 7 格延遲」的次要問題。

---

## 8. 軸 G：RoCE 的路徑分散與流量分類

兩行程式碼：

```cpp
// IBConnect.cc:654
attr.ah_attr.grh.flow_label = 0;
// IBConnect.cc:657
attr.ah_attr.grh.traffic_class = connectConfig_.traffic_class;  // 預設 0（IBDevice.h:77）
```

### G-1：`flow_label = 0` → ECMP 熵不足

RoCEv2 封裝在 UDP 裡，交換機的 ECMP 是對五元組做雜湊。`flow_label` 是 RoCEv2 產生 UDP source port 的主要熵來源；固定為 0 意味著**同一對節點的流量傾向落在同一條 spine 路徑上**。在 3FS 這種「每對節點只有一條 QP（`max_connections = 1`）」的拓撲下，路徑選擇的粒度就是「節點對」，很容易出現部分 spine 鏈路打滿、部分閒置。

- 更優解：`flow_label` 用連線的 hash（例如 `qp_num ^ peer_qp_num`）或隨機值填入。這是**一行的改動**，在 fat-tree 上通常能顯著改善鏈路利用的長尾。
- 進階：一條邏輯連線開 K 條 QP、每條給不同的 `flow_label`，把大 IO 條帶化到多條路徑上（見 §9）。

### G-2：`traffic_class = 0` → DSCP 沒有標記

生產環境的 RoCE 必須把流量標到正確的 DSCP/PCP，才能對上交換機的無損佇列（PFC priority）與 DCQCN 的 ECN 設定。預設 0 代表「用 best-effort 佇列」，在啟用 PFC 的網路上要嘛沒有無損保護、要嘛與其他流量混在同一佇列。

- 這是部署設定而非程式碼問題（`IBSocket::Config::traffic_class` 可調），但**出廠預設值應該給一個典型值並在文件裡註明**，而不是留 0 讓人踩坑。
- 建議同時把資料平面（bulk RDMA）與控制平面（socket 模擬）分到不同的 SL/TC——控制平面的小訊息不該排在大 IO 後面。這需要兩條 QP 或 RoCE 的 SL 分流，屬於中等改動。

---

## 9. 軸 H：單一 QP / 單一 HCA 的頻寬上限

### 現況

- `TransportPool.h:17`：`kDefaultMaxConnections = 1`
- `IBQueryRsp::selectDevice`（`IBConnect.cc:280-297`）：以 round-robin 計數器**為每條連線**挑一個裝置，挑完就固定

所以：**一對節點之間只有一條 QP，走一張 HCA。** 節點上有 4 張 HCA 時，是靠「對不同 peer 的連線輪流挑不同裝置」在統計上把負載攤開。

### 這在什麼時候會出問題

- **點對點大流量**：一個 client 從單一 storage node 拉大量資料（例如 checkpoint 載入、KVCache 的熱點）時，被單張 NIC 的線速卡死，其餘 3 張閒置。
- **單 QP 的 head-of-line blocking**：RC 保證順序，所以一個丟包造成的重傳會阻塞該 QP 上後續所有操作。大 IO 與小 IO 混在同一條 QP，小 IO 的長尾延遲會被大 IO 的重傳拖住。
- **QP 層級的硬體上限**：單 QP 的 outstanding read 受 `max_rd_atomic`（設定為 16，`IBSocket.h`）限制，這對 `RDMA_READ`（也就是寫入路徑，server 主動拉 client 資料）是實際的管線深度上限。

### 更優解

**（a）多 QP 條帶化**：`max_connections` 改成 > 1 是**現成的設定**（`TransportPool::Config`），但目前的實作是「隨機挑一條連線送整個請求」（`TransportSet::acquire(idx)`），做的是**請求層級**的負載平衡，不是**單一請求的條帶化**。要讓一個 4 MiB 的讀取同時走 4 條 QP，需要在 `RDMAReqBatch` 層把 WR 分派到多個 socket，這是中等規模的改動。

**（b）多軌聚合（multi-rail）**：讓一條邏輯連線綁定多張 HCA，按 chunk 邊界切分。3FS 已經有「記憶體對所有裝置全註冊」的基礎設施（`RDMABuf` 的 `mrs_[devId]`、`RDMARemoteBuf::rkeys_` 是 `std::array<Rkey, kMaxDeviceCnt>`，`RDMABuf.h:127`）——**這個設計本來就是為了多軌準備的**，只是上層沒有用起來。`getRkey(devId)` 的存在說明「同一塊遠端記憶體可以從任一張 HCA 存取」已經成立，缺的只是「把一個批次拆到多個 IBSocket」的排程邏輯。

**我認為這是 3FS RDMA 層最大的一塊未兌現的設計紅利**：底層資料結構已經支援多軌，上層卻只用單軌。

**（c）分離大小 IO 的 QP**：開兩條 QP，小 IO 與控制訊息走一條、大 IO 走另一條，避免 head-of-line blocking。這比完整的條帶化便宜得多，而且對長尾延遲的改善通常更明顯。

---

## 10. 軸 I：記憶體註冊

### 現況

- 唯一入口 `IBDevice::regMemory`（`IBDevice.cc:592`），一裝置一 PD。
- `RDMABuf` 對所有裝置（上限 `kMaxDeviceCnt = 4`，`IBDevice.h:119`）各註冊一份，存 `mrs_[devId]`。
- socket bounce buffer 每條連線各自註冊（`IBConnect.cc:731`）。
- 無 ODP（On-Demand Paging）、無 MR cache、無 DMABUF/GPU 記憶體支援（全庫 `grep -i cuda|gpudirect` 零結果）。

### 三個可改進點

**（a）per-socket MR → 共享 MR。** §2 已提，跟 SRQ 是同一件事。N 個 MR 會吃 NIC 的 MTT/ICM 空間，在連線數大時造成 address translation cache miss，對效能的影響是懸崖式的。

**（b）GPUDirect RDMA / DMABUF。** 3FS 的定位是 AI 訓練的儲存層（KVCache、checkpoint、dataloader），而目前資料路徑是：

```
NVMe → host memory (rdmabufPool) → RDMA → client host memory → memcpy → GPU
```

支援用 `ibv_reg_dmabuf_mr` 註冊 GPU 記憶體（或走 nvidia-peermem）後可以變成：

```
NVMe → host memory → RDMA → GPU memory (直接)
```

省掉 client 端的一次 host→device 拷貝與一次 host memory 往返。對 dataloader 這種頻寬密集的路徑，收益可能是兩位數百分比。

阻礙：`RDMABuf` 目前的配置器是 `hf3fs::memory::memalign`（`IBSocket.cc:170`）與自有的 pool，要引入「外部記憶體（GPU / DMABUF fd）也能當 RDMABuf」需要抽象一層 allocator。USRBIO 的 `Iov` 註冊介面（`src/lib/api/`）是自然的切入點——使用者本來就是拿 shared memory 來註冊，改成能接受 dmabuf fd 是相容的擴充。

**這是「有沒有更優解」這個問題裡，對 3FS 的目標場景潛在收益最大的一項**，但也是工程量最大的一項。

**（c）ODP / MR cache。** 對 USRBIO 這種「使用者預先註冊大塊 iovec」的模型，ODP 的價值不大（註冊是一次性的）。不建議。

---

## 11. 軸 J：錯誤處理的兩處設計取捨

### J-1：`rnr_retry = 0`

`configs/*.toml` 全部是 `rnr_retry = 0`（搭配 `min_rnr_timer = 1`）。意思是：**只要對端 RQ 沒有可用的 recv WQE，QP 立刻死掉，不重試。**

這是「我的 credit 協定保證不會 RNR，所以 RNR 發生 = 有 bug，寧可炸掉也不要掩蓋」的哲學。從除錯角度我認同；但從生產可用性角度，§7 的 F-2 已經證明了 credit 帳目有靜默出錯的可能，而 `rnr_retry = 0` 讓任何一個帳目誤差都直接升級成連線中斷。

- 建議：`rnr_retry = 3`（不是 7 —— 7 是無限重試，會把真正的死結變成永久掛起）。三次重試足以吸收暫態的競態，又不會掩蓋真正的協定錯誤；同時加上一個 RNR 計數器 metric，只要不為 0 就告警。這樣既有安全網，又保留了「RNR = bug」的可觀測性。

### J-2：`ibv_post_send` 部分成功 → 直接把 QP 打死

`rdmaPostWR`（`IBSocket.cc:1073-1095`）：

```cpp
// Only a subset of RDMA work requests were successfully posted. As only the final WR has the
// IBV_SEND_SIGNALED flag set, there is no way to track when the posted RDMA WRs will complete.
// We need set the QP to error state.
```

註解說得很誠實：因為只有最後一個 WR 是 signaled，部分投遞成功時無法追蹤已投遞的那些何時完成，只好整條 QP 進 ERROR。

- 這是「只簽最後一個 WR」這個優化的代價，而且這個優化本身是對的（§12）。
- 更優解：**投遞前先確保不會失敗**。`ibv_post_send` 的失敗主因是 SQ 滿，而 SQ 的容量是 `qpMaxSendWR()` 已知、`rdmaSem_` 也已經在限制 RDMA WR 數量。理論上不該滿。所以真正該做的是**把 semaphore 的配額與 SQ 容量嚴格對上**（包含 socket send、ACK、close、check 這些也佔 SQ 的 WR），讓部分失敗在結構上不可能發生，而不是事後補救。目前 `qpMaxSendWR() = send_buf_cnt + qpAckBufs + max_rdma_wr + 2` 看起來是有算過的，但 `rdmaSem_` 只管 `max_rdma_wr`，socket 側的 send 與 ACK 是各自獨立計數，三者之間沒有共同的上限檢查。值得補一個 assertion 與壓力測試。

---

## 12. 不應該改的部分（這些已經是好設計）

調研的另一半價值是說清楚哪些地方**不要動**：

**（a）控制平面走 TCP 完成 RDMA 握手**（`IBConnect.cc:338-448`）。比 RDMA CM 簡單、可除錯（tcpdump 看得到）、不需要額外的 CM 服務、失敗語意清楚。不要換成 `rdma_cm`。

**（b）非對稱握手：server 停在 RTR，靠 client 的 connect probe 才轉 RTS**（`IBConnect.cc:487-510`、`IBSocket.cc:512-522`）。這是避免「server 先發資料但 client 還沒 post recv → RNR」的正確做法，在 `rnr_retry = 0` 的前提下更是必要。

**（c）bulk RDMA 一律由 storage server 主動發起**。理由很硬：只有 server 知道資料何時從 NVMe 讀完、何時可以推。改成 client 主動 `RDMA_READ` 會需要「server 通知就緒 → client 發起讀」，反而多一個 RTT。而且 client buffer 當被動目標、rkey 只在請求裡出現一次，攻擊面比反過來小。

**（d）WR linked-list 批次投遞 + 只有最後一個 `IBV_SEND_SIGNALED`**（`IBSocket.cc:1052-1056`）。利用 RC 的順序保證把 N 個 CQE 縮成 1 個，是教科書級的正確優化。

**（e）`wr_id` 用 `folly::StampedPtr` 打包型別 + 指標**（`IBSocket.h` 的 `WRId`）。64 bit 的 `wr_id` 塞下 16 bit type + 48 bit pointer，乾淨且零成本。做共享 CQ 時這個設計會直接派上用場。

**（f）在 RC QP 上模擬 stream socket 這個抽象本身。** 代價是每個 RPC 兩次 memcpy 與「以格計量」的 credit（§6）。換來的是 `Transport`、`Processor`、`IOWorker`、`serde` 整套在 TCP 與 RDMA 之間**完全共用**，RDMA 的支援只是換一個 `Socket` 實作。若改成訊息式的零拷貝 RPC（eRPC / UCX 風格），要重寫的是整個上層而不只是傳輸層。**以 3FS 的工作負載（大 IO 佔絕大多數位元組、小 RPC 佔絕大多數次數但延遲要求沒那麼極端）而言，這筆交易是划算的。**

**（g）`specs/RDMASocket/` 的 P 形式化模型。** 721 行的 P 模型涵蓋了 QP、Network、credit 交換。這是很少見的資產，上面任何動到 credit 協定的改動（§7）都應該先在模型裡驗證。

---

## 13. 建議的落地順序

### 第一階段：純設定 + 量測（一週內可完成，零架構風險）

1. `max_inline_read_bytes` / `max_inline_write_bytes` 從 0 掃到 16 KiB，畫出延遲曲線，找出交叉點。
2. `checkConfig` 加整除檢查（§7 F-2）。
3. `flow_label` 隨機化、`traffic_class` 設定正確的 DSCP（§8）。
4. `rnr_retry` 0 → 3，加 RNR metric（§11 J-1）。
5. 用現成的 metric 建立基線：`common.ib.rdma_wait_latency`、`rdma_read_latency`、`rdma_write_latency`、`common.ib.wait_send_buf`、`common.transmission.*`（`IBSocket.cc:130-136`、`RDMAControl.cc:12-15`）。**`wait_send_buf` 的分佈直接告訴你 credit 窗口夠不夠**，這是決定 §6 要不要做訊息聚合的依據。

### 第二階段：局部改動（收益明確、可獨立驗證）

6. `max_inline_data` = 128（§6 E-1）。
7. 混合輪詢（§3）——建議做成可設定的門檻，先在 storage node 上開。
8. 流控改以位元組計量（§5 問題二）。
9. `num_event_loop` 調大 + 驗證 CQ drain 是否還是瓶頸（§2 代價 2）。

### 第三階段：協定改動（需要形式化驗證 + 壓力測試）

10. credit 捎帶在 `imm_data`（§7 F-1）——先改 P 模型，跑 `specs/RDMASocket/PTst` 驗證。
11. write-then-send（§4）——**先解決 `RELAXED_ORDERING` 的疑慮**，再處理 buffer 生命週期重構。
12. 接收端 grant 捎帶（§5 問題一）。

### 第四階段：架構改動（需要立項）

13. 共享 CQ + lazy recv buffer（§2）。
14. 多軌聚合（§9 b）——**底層已經支援，這是最大的未兌現紅利**。
15. GPUDirect / DMABUF（§10 b）——對 AI 場景潛在收益最大。

---

## 14. 驗證方法

每一項都應該用同一套方法驗證，避免「改了感覺變快」：

- **微觀**：`ib_read_lat` / `ib_write_bw` 建立硬體上限的基線，任何改動後的數字都要跟這個上限比。
- **中觀**：`storage_bench`（`src/tools/storage_bench` 系列，見 `3FS-組件深度剖析/storage_bench-存儲壓測工具深度剖析.md`），掃 IO 大小 4 KiB → 4 MiB、佇列深度 1 → 256。
- **NIC 計數器**：`/sys/class/infiniband/*/ports/*/counters/`——特別是 `port_xmit_wait`（發送被流控擋住的次數）與 RoCE 的 `np_ecn_marked_roce_packets` / `rp_cnp_handled`。**若 `port_xmit_wait` 很高，那瓶頸在網路而不在程式碼，先做 §8 的流量分類比做任何程式碼優化都有效。**
- **既有 metric**：`common.ib.poll_cpu_time` 直接量 event loop 執行緒花在 poll 上的時間，是判斷 §3 混合輪詢是否值得、以及 §2 單執行緒是否飽和的關鍵指標。
- **形式化**：動到 credit 協定就跑 `specs/RDMASocket/`。

---

## 附錄：關鍵檔案索引

| 主題 | 位置 |
|------|------|
| QP / CQ / comp channel 建立 | `src/common/net/ib/IBConnect.cc:547-600` |
| QP 狀態遷移（INIT/RTR/RTS） | `src/common/net/ib/IBConnect.cc:602-722` |
| socket 緩衝初始化與註冊 | `src/common/net/ib/IBConnect.cc:724-737` |
| 容量計算公式 | `src/common/net/ib/IBConnect.h:101-106` |
| CQ 輪詢與事件處理 | `src/common/net/ib/IBSocket.cc:325-420` |
| credit / ACK 協定 | `src/common/net/ib/IBSocket.cc:512-560, 664-706, 836-886` |
| RDMA 批次投遞 | `src/common/net/ib/IBSocket.cc:930-1095` |
| 接收端准入控制 | `src/common/net/RDMAControl.cc`、`src/common/net/Waiter.h:64-95` |
| 讀路徑（RDMA_WRITE 推） | `src/storage/service/StorageOperator.cc:176-225` |
| 寫路徑（RDMA_READ 拉） | `src/storage/service/StorageOperator.cc:563-595` |
| inline 門檻 | `src/client/storage/StorageClient.h:422-423`、`StorageClientImpl.cc:705,1898` |
| 遠端緩衝與多裝置 rkey | `src/common/net/ib/RDMABuf.h:39-130, 244` |
| 形式化模型 | `specs/RDMASocket/` |
