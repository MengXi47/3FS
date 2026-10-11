# 分散式理論骨架：DDIA 第 5／7／8／9 章 × 3FS 原始碼對照

> 範圍：Kleppmann《Designing Data-Intensive Applications》**第一版**的第 5 章（Replication）、第 7 章（Transactions）、第 8 章（The Trouble with Distributed Systems）、第 9 章（Consistency and Consensus）。第二版重排了章節，對照時以第一版編號為準。
>
> 本文是導讀＋原始碼對照，不是書摘。概念說明都是重新用自己的話寫的，要看完整論證請回原書對應小節。

---

## 0. 這份文件怎麼用

建議的讀法分三步：

1. **先讀 DDIA 原書對應小節。** 每個概念後面都標了「對應 DDIA 第 N 章〈小節名〉」。原書的例子和圖比這裡完整很多。
2. **再看本文的「3FS 怎麼做」。** 這部分回答「這個理論問題，3FS 在哪一行程式碼處理、怎麼處理、或刻意不處理」。
3. **最後打開程式碼。** 每條論述都附了 `路徑:行號`（路徑相對於 repo 根目錄 `/Users/boen/3FS`）。讀的時候建議以本文為索引，而不是照本文的描述去「相信」程式碼：程式碼才是準的。

標記慣例：

| 標記 | 意思 |
|---|---|
| **（推論）** | 由我讀到的程式碼邏輯推得，沒實際跑過、也沒有程式碼註解直接證實 |
| **（推測）** | 設計理由的猜測，原始碼和文件都沒寫 |
| **（條件）** | 只有在某個 config 或部署方式下才成立，條件寫在旁邊 |
| **（FDB 語意）** | 這是 FoundationDB 本身的行為，不在 3FS repo 裡，依據是 FDB 的公開文件 |

所有「預設值」都是指程式碼裡 `CONFIG_ITEM` / `CONFIG_HOT_UPDATED_ITEM` 寫的預設，實際部署的 toml 可能覆寫。

### 0.1 先記住 3FS 的「誰說了算」

讀分散式理論時，最重要的問題通常是：**某個狀態的真相（source of truth）在哪裡？誰有權改它？** 3FS 的答案：

| 狀態 | 真相在哪 | 誰能改 | 其他人怎麼知道 |
|---|---|---|---|
| 檔案系統 metadata（inode、目錄項、session） | FoundationDB | 任何一台 meta server，透過 FDB 交易 | 每次操作都進 FDB 讀 |
| 叢集成員與 chain 拓樸（RoutingInfo、chainVersion） | mgmtd primary 記憶體＋持久化到 FDB | 只有持有 lease 的 mgmtd primary | client/storage 定期拉取（pull） |
| 誰是 mgmtd primary | FDB 裡的一個 lease key | 透過 FDB 交易 CAS 搶 lease | `getPrimaryMgmtd` RPC |
| chunk 資料與版本號 | 每個 storage target 本地（chunk engine＋RocksDB） | 只能經由 chain 的 head 寫入 | 讀任一個 serving 副本 |

一句話總結：**3FS 自己不實作任何共識演算法**；凡是需要「大家同意」的事，它都丟給 FoundationDB，然後讓 mgmtd 當唯一的決策者，再用版本號把決策「圍起來」（fencing）。整份文件都可以用這句話當線索。

### 0.2 名詞速查

| 3FS 名詞 | 意思 | 主要定義位置 |
|---|---|---|
| target | 一顆 SSD 上的一個儲存單位，一個 target 屬於一條 chain | `src/storage/service/TargetMap.cc:79-100` |
| chain | 同一份 chunk 的多個副本 target 排成的鏈 | `src/mgmtd/service/updateChain.cc:25-104` |
| `chainVersion` / `chainVer` | chain 成員或順序每變一次就 +1 | `src/mgmtd/service/MgmtdData.cc:133-143` |
| `routingInfoVersion` | 整份 RoutingInfo 的版本號 | `src/mgmtd/background/MgmtdRoutingInfoVersionUpdater.cc:13-26` |
| `updateVer` / `commitVer` | 單一 chunk 的 pending 版本與已提交版本 | `src/storage/store/ChunkReplica.cc:211-245` |
| `PublicTargetState` | mgmtd 決定的 target 狀態：SERVING / LASTSRV / SYNCING / WAITING / OFFLINE | `src/mgmtd/service/updateChain.cc:25-104` |
| `LocalTargetState` | storage 自己回報的狀態：UPTODATE / ONLINE / OFFLINE | `src/storage/service/TargetMap.cc:329-352` |

---

## 1. 第 5 章 Replication（複製）

### 1.1 單主複製（leader / follower）

**概念**（對應 DDIA 第 5 章〈Leaders and Followers〉）
複製的基本問題是：同一份資料存在多台機器上，寫入要怎麼送到每一台、又不讓各台互相矛盾。最常見的做法是指定一台當 leader，所有寫入都先到它，由它決定順序再傳給其他副本（follower）；讀取則可以視需求分給 leader 或 follower。這樣「寫入順序」只有一個人決定，就不會有兩個副本各自接受衝突寫入的問題。代價是 leader 成為寫入瓶頸，且 leader 掛掉時要有人接手。

**3FS 怎麼做**
3FS 的資料層用的是 **鏈式複製（chain replication）的變體 CRAQ**，它是單主複製的一種特殊排法：副本排成一條鏈，鏈頭（head）是唯一接受 client 寫入的 leader，寫入沿鏈往下傳，鏈尾（tail）是最後一個副本。

- client 寫入時預設只挑 head：`src/client/storage/StorageClientImpl.cc:1805-1808`（`TargetSelectionMode::HeadTarget`）。
- 非 head 收到 client 直接寫入會拒絕：`src/storage/service/StorageOperator.cc:338-341`（`kRoutingError`）。
- head 是誰不是選出來的，而是 RoutingInfo 裡這條 chain **第一個** SERVING/SYNCING 的 target：`src/storage/service/TargetMap.cc:190-193`。
- 同一個 chunk 的寫入在 head 上用 chunk 鎖序列化：`src/storage/service/StorageOperator.cc:368-380`。也就是說「寫入順序」由 head 決定，粒度是單一 chunk。
- 讀取則可以分散到任何 SERVING 副本，預設是 `LoadBalance`：`src/client/storage/StorageClientImpl.cc:1647-1649`。這就是 CRAQ 的「write-all, read-any」，`docs/design_notes.md:151` 也說了這是為了吃滿所有副本的讀頻寬。

metadata 層則不是 3FS 自己做複製：meta server 是無狀態的（`docs/design_notes.md:63`），資料全在 FoundationDB，複製由 FDB 負責。mgmtd 是「一主多備」，主是靠 FDB lease 決定的（見 §3.7）。

**取捨**
CRAQ 把所有副本都拿來服務讀取，換到讀吞吐；代價是寫入延遲是整條鏈的總和（要逐跳傳到 tail 再回來）。3FS 的目標負載（AI 訓練讀大量資料）讀遠多於寫，這個取捨跟負載形狀一致（推測：程式碼沒寫理由，`docs/design_notes.md:151` 只說讀頻寬很重要）。

---

### 1.2 同步 vs 非同步複製

**概念**（對應 DDIA 第 5 章〈Synchronous Versus Asynchronous Replication〉）
leader 收到寫入後，可以等所有（或部分）follower 確認再回 client（同步），也可以自己寫完就回、follower 之後再跟上（非同步）。同步複製保證回 ack 時資料已在多處，但任何一個 follower 卡住，寫入就跟著卡；非同步複製延遲低、不怕慢 follower，但 leader 一掛，已經回 ack 的寫入可能還沒傳出去而遺失。很多系統折衷成「半同步」：一個同步、其他非同步。

**3FS 怎麼做：完全同步，而且是「全員」同步**
每個節點的處理順序是：先寫本地 pending 版本 → 轉發給下一個 → **等下一個回來** → 才把自己的版本 commit → 再回給上一個。

- 本地寫 pending：`src/storage/service/StorageOperator.cc:391-404`
- 轉發並阻塞等待後繼回覆：`src/storage/service/StorageOperator.cc:443-450`（呼叫 `forwardWithRetry`）
- 後繼回來之後才 commit 本地：`src/storage/service/StorageOperator.cc:492-508`
- tail 沒有後繼，`forward` 直接回 `kNoSuccessorTarget`，tail 就直接 commit：`src/storage/service/ReliableForwarding.cc:113-117`、`src/storage/service/StorageOperator.cc:488-490`

所以 commit 的順序是 tail 先、head 最後，head 回 ack 給 client 時，整條鏈上所有 SERVING 副本都已 commit。連正在補資料（SYNCING）的副本也在同步路徑上：head 發現後繼是 SYNCING 時照樣轉發給它（`src/storage/service/ReliableForwarding.cc:152-156`、`src/storage/service/TargetMap.cc:229-233`）。

**取捨**
好處：client 拿到 ack 後，任何 SERVING 副本都有這筆資料，不會因為讀到落後的副本而看不到自己剛寫的東西（見 §1.6）。壞處：鏈上任何一個副本慢或死，整條鏈的寫入都會被拖住，直到 mgmtd 把它踢出鏈。`forwardWithRetry` 在等待時每 100ms 檢查一次 routing 裡後繼還在不在（`src/storage/service/ReliableForwarding.cc:82-97`），整體重試上限預設 60s（`src/storage/service/ReliableForwarding.h:18-20`）。這就是鏈式複製的典型代價：**可用性依賴「故障偵測＋重新配置」的速度**（見 §3.3）。

---

### 1.3 加入新的 follower（補資料）

**概念**（對應 DDIA 第 5 章〈Setting Up New Followers〉）
新副本（或停機很久回來的副本）手上的資料是舊的，不能直接拿來服務讀取。一般做法是先複製一份一致的快照，再追上快照之後的變更，追平之後才正式加入。難點在於：補資料期間線上寫入不會停，要確保「背景補的舊資料」不會蓋掉「同時進來的新寫入」。

**3FS 怎麼做：resync，由前驅主動推給後繼**
1. **重啟的 storage 先等 mgmtd 承認它下線了**，才開始服務：`src/storage/service/Components.cc:87-122`。只要自己的 target 在 routing 裡還是 SERVING/SYNCING/WAITING 就一直等。這避免了「舊副本一啟動就被當成最新的來讀」。
2. 回來的 target 回報 `ONLINE`，mgmtd 的狀態機把它從 OFFLINE/WAITING 轉成 SYNCING。**同一條 chain 一次只允許一個 SYNCING**：`src/mgmtd/service/updateChain.cc:70-88`（條件是 `newTargetsByPs[PS::SYNCING].empty()`）。
3. SYNCING target 的**前驅**（不是 SYNCING 自己）啟動 `ResyncWorker::handleSync`：
   - `syncStart` 取得後繼的所有 chunk metadata：`src/storage/sync/ResyncWorker.cc:152-181`
   - 逐 chunk 比對 chainVer / updateVer / commitVer / 狀態 / checksum，決定要推哪些、刪哪些：`src/storage/sync/ResyncWorker.cc:203-275`
   - 推送時以「整個 chunk 覆蓋」的方式送：`src/storage/sync/ResyncWorker.cc:433-446`（`offset = 0`、`isSyncing = true`）
   - 全部推完送 `syncDone`：`src/storage/sync/ResyncWorker.cc:356-377`
4. 後繼收到 `syncDone` 把本地狀態改成 `UPTODATE`：`src/storage/service/TargetMap.cc:111-121`；mgmtd 看到 SYNCING + UPTODATE 就升成 SERVING：`src/mgmtd/service/updateChain.cc:56-58`。

**怎麼避免背景補資料蓋掉線上寫入**
- 背景推送和線上寫入搶同一把 chunk 鎖：`src/storage/sync/ResyncWorker.cc:397-402`；線上寫入在 `handleUpdate` 也拿同一把鎖（`src/storage/service/StorageOperator.cc:368-380`）。
- 拿到鎖後再查一次本地狀態，若 chunk 已被線上寫入改過就跳過刪除、或已被刪就跳過寫入：`src/storage/sync/ResyncWorker.cc:404-424`。
- 線上寫入轉發給 SYNCING 後繼時，若不是整塊寫，前驅會先讀出**整個 chunk** 再送過去，讓後繼直接整塊替換：`src/storage/service/ReliableForwarding.cc:158-215`。這樣後繼不需要有正確的舊內容也能套用這筆寫入。

**和書上的差異**
書上的典型做法是「快照＋複製日誌追趕」。3FS 沒有複製日誌（見 §1.5），用的是**狀態比對（state transfer）**：直接比版本號與 checksum，差異就整塊覆蓋。

---

### 1.4 節點故障：follower 失效與 leader failover

**概念**（對應 DDIA 第 5 章〈Handling Node Outages〉）
follower 掛掉相對好處理：回來後追上即可。leader 掛掉則需要 failover：判定它真的掛了、選出新 leader、讓所有人改找新 leader。書中特別點出 failover 的幾個陷阱：逾時設太短會誤判、新舊 leader 同時存在（split brain）、新 leader 可能缺少舊 leader 已確認的寫入。

**3FS 怎麼做：沒有選舉，由 mgmtd 直接重排鏈**
- **偵測**：mgmtd primary 每 10s（`check_status_interval`）檢查一次，超過 `heartbeat_fail_interval`（預設 60s）沒收到心跳的 node 與 target 被標成失效／OFFLINE：`src/mgmtd/background/MgmtdHeartbeatChecker.cc:41-52`、`src/mgmtd/background/MgmtdHeartbeatChecker.cc:66-74`；預設值在 `src/mgmtd/service/MgmtdConfig.h:15-16`。
- **重排**：`generateNewChain` 是一個純函式，輸入舊 chain 的各 target 狀態，輸出新的順序與狀態：`src/mgmtd/service/updateChain.cc:25-104`。SERVING 的 target 保持原相對順序在前面，失效的往後丟（`src/mgmtd/service/updateChain.cc:98-101`）。chain 有變動就把 `chainVersion` +1：`src/mgmtd/service/MgmtdData.cc:133-143`。
- **新 head 自動產生**：head 不是選出來的，是「新排序裡第一個 SERVING」（`src/storage/service/TargetMap.cc:190-193`）。head 掛掉時，它原本的後繼自然變成新的第一個。
- **新 head 不會缺資料**：因為複製是全員同步的（§1.2），任何已回 ack 的寫入在所有 SERVING 副本上都已 commit，所以任一 SERVING 當 head 都不缺已確認的寫入。這是鏈式複製相對於非同步 leader/follower 的主要優勢。
- **全部 SERVING 都掛了**：只有排第一個的變成 LASTSRV，其餘 OFFLINE；整條鏈會等這一個 LASTSRV 回來，即使其他副本資料其實完整：`src/mgmtd/service/updateChain.cc:31-42`（原始碼註解明講 "the whole chain has to wait the HEAD for recovering even when other replicas are complete"）。運維可以手動 `rotateLastSrv` 換一個當 LASTSRV：`src/mgmtd/service/updateChain.cc:143-163`。

**split brain 怎麼防**：見 §3.7（fencing），靠的是 `chainVersion`。

---

### 1.5 複製日誌的實作方式

**概念**（對應 DDIA 第 5 章〈Implementation of Replication Logs〉）
leader 把「發生了什麼變更」傳給 follower，可以傳 SQL 語句、傳儲存引擎的 WAL、或傳邏輯層的 row 變更。不同做法在可攜性、確定性、版本相容性上各有問題。

**3FS 怎麼做：沒有日誌，只有「每個 chunk 的版本號」**
3FS 的 chain 上傳的是**一個寫入請求本身**（offset、length、資料由下游用 RDMA Read 拉：`src/storage/service/StorageOperator.cc:565-596`），不累積成日誌。順序靠每個 chunk 的 `updateVer`：

- head 收到 client 寫入時 `updateVer` 為 0，由 head 依本地版本 +1 決定：`src/storage/store/ChunkReplica.cc:232-238`，並把結果寫回請求往下傳：`src/storage/service/StorageOperator.cc:409-410`。
- 下游收到帶版本的請求，會檢查它是不是「剛好下一個」：小於等於已 commit 的版本 → `kChunkCommittedUpdate`；小於等於 pending → `kChunkStaleUpdate`；跳號 → `kChunkMissingUpdate`：`src/storage/store/ChunkReplica.cc:216-231`。chunk engine 路徑有對應的檢查：`src/storage/chunk_engine/src/core/engine.rs:359-376`。
- head 對這些結果的處置：committed / stale 視為成功（重複的請求），missing / advance 直接報錯：`src/storage/service/StorageOperator.cc:406-439`。

**取捨**：沒有日誌就沒有「重放」能力，掉隊的副本只能靠 resync 的狀態比對補（§1.3）。好處是不需要管理日誌的截斷、壓縮與保存（推測）。版本號只在單一 chunk 內有序，chunk 之間沒有全域順序（見 §4.3）。

---

### 1.6 複製延遲帶來的讀取異常

**概念**（對應 DDIA 第 5 章〈Problems with Replication Lag〉）
非同步複製下，follower 會落後 leader 一段時間，讀 follower 可能看到舊資料。書中列了三個常見的保證：
- **read-your-writes**：自己剛寫的，自己馬上讀得到。
- **monotonic reads**：同一個人連續讀，不會先看到新的、後看到舊的（時間倒流）。
- **consistent prefix reads**：有因果順序的多筆寫入，讀者看到的順序要一致。

**3FS 怎麼做**

*資料層（chunk）*
- **read-your-writes 原則上成立**：寫入 ack 時所有 SERVING 副本都已 commit（§1.2），之後讀任何一個 SERVING 副本都會看到它。client 只從 SERVING 的 target 挑讀取對象：`src/client/storage/StorageClientImpl.cc:426-444`。
- **讀到「寫到一半」的 chunk 時，兩種儲存後端行為不同**（條件：取決於 target 建立時是否用 chunk engine）：
  - 舊的 `ChunkReplica` 後端：若本地 `commitVer != updateVer`（有 pending 版本），直接回 `kChunkNotCommit`：`src/storage/store/ChunkReplica.cc:61-66`；client 把它歸類為「快速重試」錯誤，縮短等待後重試：`src/client/storage/StorageClientImpl.cc:166-174`、`src/client/storage/StorageClientImpl.cc:1242`。這對應 `docs/design_notes.md:173` 的描述：不像原版 CRAQ 去問 tail，而是叫 client 稍後再試。
  - Rust chunk engine 後端：讀取拿的是 `meta_cache` 裡**已 commit** 的 chunk（`src/storage/chunk_engine/src/core/engine.rs:184-209`），pending 版本放在另一個 `writing_list`（`src/storage/chunk_engine/src/core/engine.rs:441-465`），只有 `commit_chunk` 才會更新 `meta_cache`（`src/storage/chunk_engine/src/core/engine.rs:501-516`）。C++ 端的讀取準備直接把 `commitVer` 與 `updateVer` 都設成 `chunk_ver`（`src/storage/store/ChunkEngine.h:59-63`），所以不會觸發 `kChunkNotCommit`。也就是說，**chunk engine 模式下，讀到寫入中的 chunk 會回傳該副本最後一個已 commit 的版本**，而不是報錯。
  - 哪個是實際部署的？`only_chunk_engine` 程式碼預設是 `false`（`src/storage/store/StorageTargets.h:39`、`src/storage/store/PhysicalConfig.h:27`），但官方的建 chain 腳本產生的 `create-target` 指令帶了 `--use-new-chunk-engine`（`deploy/data_placement/src/setup/gen_chain_table.py:114`、`src/client/cli/admin/CreateTarget.cc:22`）。
- **monotonic reads（推論）**：在 chunk engine 模式下，一筆寫入正在從 tail 往回 commit 時，tail 已經是新版、head 還是舊版。client 讀取是隨機挑副本（LoadBalance），所以同一個讀者連讀兩次，可能先讀到 tail 的新版、再讀到 head 的舊版。我沒有在程式碼裡找到防止這件事的機制。這只發生在「讀與寫同時進行」的時間窗內，寫入 ack 之後就不會。
- **consistent prefix**：單一 chunk 內順序由 head 決定（§1.5），但跨 chunk 沒有任何順序保證；一次跨多個 chunk 的寫入，各 chunk 是獨立的 chain 寫入（見 §2.2）。

*路由資訊的延遲*
在 3FS 裡，比資料複製延遲更值得注意的是**路由資訊（RoutingInfo）的延遲**：client 和 storage 是**拉取**路由的，預設每 10s 一次（`src/client/mgmtd/MgmtdClient.h:19-20`、`src/client/mgmtd/MgmtdClient.cc:280-291`）。不同節點手上的 chain 版本可能不同。3FS 用版本號處理這件事：
- 路由版本只能前進，退回就 FATAL：`src/client/mgmtd/MgmtdClient.cc:626-630`；storage 收到比自己舊的路由直接拒絕：`src/storage/service/TargetMap.cc:132-136`。
- client 送出寫入前再確認一次路由沒變：`src/client/storage/StorageClientImpl.cc:633-640`、`src/client/storage/StorageClientImpl.cc:1831-1834`。
- 請求帶著 `chainVer`，storage 端版本不符就拒絕（`kRoutingVersionMismatch`）：`src/storage/service/TargetMap.cc:58-65`。client 把這個錯誤當快速重試（`src/client/storage/StorageClientImpl.cc:166-174`）。我沒有在 `StorageClientImpl.cc` 找到「遇到版本不符就強制刷新路由」的呼叫，看起來是等背景的定期刷新（推論，見文末待查核清單）。

*metadata 層*
meta 的讀寫都直接進 FDB，沒有「讀 follower」這回事，所以沒有複製延遲問題。唯一的例外是可選的 GRV cache（條件：`grv_cache` 預設 `false`，`src/meta/base/Config.h:60`）：開啟時唯讀操作會設 `FDB_TR_OPTION_USE_GRV_CACHE`（`src/meta/store/Operation.h:170-174`），可能讀到稍舊的版本（FDB 語意）。

---

### 1.7 多主複製（multi-leader）

**概念**（對應 DDIA 第 5 章〈Multi-Leader Replication〉）
允許多個節點同時接受寫入（常見於跨機房），然後彼此同步。代價是必須處理寫入衝突（同一筆資料在兩邊被改成不同值），需要衝突偵測與合併策略（last-write-wins、CRDT 等）。

**3FS 未涉及。** 每個 chunk 只有一個 head 可以寫（`src/storage/service/StorageOperator.cc:338-341`）；mgmtd 只有一個 primary（§3.7）；meta 雖然有多台同時寫，但它們寫的是同一個 FDB，衝突由 FDB 交易處理（§2），不是多主複製。repo 裡沒有跨機房複製或衝突合併邏輯。

---

### 1.8 無主複製與 quorum（leaderless）

**概念**（對應 DDIA 第 5 章〈Leaderless Replication〉）
不設 leader，client 直接寫 n 個副本中的 w 個、讀 r 個，只要 w + r > n，讀寫集合就會重疊，至少讀到一份最新的。好處是不需要 failover、單一副本慢不會拖住整體；代價是一致性語意比較弱（需要 read repair、anti-entropy、版本向量），而且 quorum 本身並不保證 linearizability。

**3FS 未涉及，而且是刻意相反的選擇。** CRAQ 是「寫全部、讀任一」，等於 w = n、r = 1。對照：
- quorum 系統的寫入可以容忍少數副本慢；3FS 的寫入要等整條鏈，慢副本會拖住寫入直到被踢出（§1.2）。
- quorum 系統的讀取要讀多份才放心；3FS 讀一份就夠，讀吞吐是副本數倍（`docs/design_notes.md:151`）。
- 3FS 把「哪些副本算數」交給 mgmtd 集中決定（RoutingInfo），而不是讓每次讀寫自己數票。

---

## 2. 第 7 章 Transactions（交易）

> 3FS 的交易幾乎全發生在 **metadata 層（meta server ＋ FoundationDB）**；資料層（storage）沒有跨 chunk 交易。

### 2.1 ACID

**概念**（對應 DDIA 第 7 章〈The Meaning of ACID〉）
ACID 四個字母分別是：Atomicity（一組寫入要嘛全部生效、要嘛全部不生效，失敗可以放心重試）、Consistency（應用層的不變式，其實是應用的責任）、Isolation（並行的交易互不干擾，理想上像一個一個執行）、Durability（commit 之後不會因為當機而消失）。書中強調這四個字在不同資料庫裡意思差很多，要看實際保證。

**3FS 怎麼做**
- **每個 meta 操作就是一個 FDB 讀寫交易**：`OperationDriver::run` 開交易、執行、commit、遇錯重試：`src/meta/store/Operation.h:156-208`；底層通用的「執行＋commit＋重試」迴圈在 `src/common/kv/WithTransaction.h:49-75`。
- **Atomicity**：例如 rename 在一個交易裡刪 src 目錄項、刪/換 dst 目錄項、寫新目錄項、更新被搬目錄的 parent：`src/meta/store/ops/Rename.cc:308-331`。交易沒 commit，什麼都不會發生。
- **Isolation**：交給 FDB（見 §2.4–2.7）。
- **Durability**：交給 FDB（FDB 語意）。
- **Consistency（不變式）**：由 3FS 程式碼自己檢查，例如不能把目錄搬進自己的子孫：`src/meta/store/ops/Rename.cc:79-84`。
- **資料層沒有 ACID**：storage 的單位是「一個 chunk 的一次更新」，見 §2.2。

---

### 2.2 單物件 vs 多物件操作

**概念**（對應 DDIA 第 7 章〈Single-Object and Multi-Object Operations〉）
幾乎所有儲存系統都保證「單一物件」的寫入是原子的（不會寫一半）。「多物件交易」（同時改好幾筆、要一起成功或失敗）則昂貴得多，很多分散式系統乾脆不提供。

**3FS 怎麼做**
- **metadata：多物件交易**。一個操作常常同時改 inode、目錄項、session、idempotent 記錄，全在同一個 FDB 交易（例：`src/meta/store/Operation.h:228-241` 把操作結果和 idempotent 記錄一起 commit）。
- **單一 chunk：原子**。一次 chain 寫入對單一 chunk 是全有或全無的：pending 版本在 commit 前讀不到（舊後端回 `kChunkNotCommit`，chunk engine 讀舊的 commit 版本，見 §1.6）。
- **跨 chunk：不原子**。client 一次大寫入會拆成多個 chunk 的 IO，分組送出、各自成功或失敗，失敗的單獨收集回報：`src/client/storage/StorageClientImpl.cc:211-248`（`collectFailedOps`）、`src/client/storage/StorageClientImpl.cc:1222-1244`。也就是說，一次 `write()` 跨兩個 chunk，可能一個成功一個失敗。
- **metadata 和 data 之間：沒有交易**。建檔在 FDB，資料在 storage，兩者不在同一個交易裡。3FS 用「順序＋最終一致」補：
  - 檔案長度在寫入期間只保證最終一致，close/fsync 時才去 storage 查實際長度：`docs/design_notes.md:91-93`、`src/meta/components/FileHelper.cc:88-111`。
  - 刪檔時先刪 storage 上的 chunk，**成功後**才在 FDB 刪 inode 與 GC 項目：`src/meta/components/GcManager.cc:413-446`。中途當機重來，刪 chunk 這一步可以重做（刪不存在的 chunk 視為成功：`src/storage/store/ChunkReplica.cc:154-158`）。

---

### 2.3 Read Committed：dirty read 與 dirty write

**概念**（對應 DDIA 第 7 章〈Read Committed〉）
最基本的隔離等級，保證兩件事：不會讀到別人還沒 commit 的資料（no dirty read）、不會覆蓋別人還沒 commit 的資料（no dirty write）。

**3FS 怎麼做**
- **metadata**：FDB 的隔離等級遠高於此（§2.7），天然沒有 dirty read/write（FDB 語意）。
- **chunk 資料**：
  - no dirty read：舊後端遇到 pending 版本回 `kChunkNotCommit`（`src/storage/store/ChunkReplica.cc:61-66`）；chunk engine 只讀已 commit 版本（`src/storage/chunk_engine/src/core/engine.rs:184-209`）。例外是 client 主動開啟 `allowReadUncommitted`（條件：預設 `false`，`src/client/storage/StorageClient.h:193`），會帶上 `ALLOW_READ_UNCOMMITTED` 旗標（`src/client/storage/StorageClientImpl.cc:710-711`、`src/storage/service/StorageOperator.cc:129`），就是 `docs/design_notes.md:173` 說的 relaxed read。
  - no dirty write：同一個 chunk 的寫入在每個節點都要先拿 chunk 鎖（`src/storage/service/StorageOperator.cc:368-380`），舊後端還會拒絕在 DIRTY 狀態上再寫：`src/storage/store/ChunkReplica.cc:181-185`；chunk engine 不允許同一個 chunk 同時有兩個未中止的 pending 寫入：`src/storage/chunk_engine/src/core/engine.rs:452-458`。

---

### 2.4 Snapshot Isolation 與 MVCC

**概念**（對應 DDIA 第 7 章〈Snapshot Isolation and Repeatable Read〉）
讓每個交易看到一個固定時間點的一致快照，即使執行期間別人 commit 了新資料，本交易讀到的仍是舊快照。常用 MVCC（同一筆資料保留多個版本）實作，讀者不擋寫者。它解決「讀到一半資料被改」的不一致，但不能防 write skew（§2.6）。

**3FS 怎麼做**
- **metadata：FDB 的每個交易都有一個 read version，所有讀都是那個版本的快照**（FDB 語意）。3FS 介面上區分兩種讀：
  - `snapshotGet` / `snapshotGetRange`：只讀快照、**不加入 read conflict 集合**；
  - `get` / `getRange`：讀快照、**而且**加入 conflict 集合，commit 時若這些 key 被別人改過，本交易失敗重試。
  - 介面註解：`src/common/kv/ITransaction.h:92-99`；實作分別以 `snapshot = true/false` 呼叫 FDB：`src/fdb/FDBTransaction.cc:265`、`src/fdb/FDBTransaction.cc:280`、`src/fdb/FDBTransaction.cc:297`。
- **chunk 資料：一種很窄的 MVCC**。每個 chunk 最多同時存在「已 commit」和「pending」兩個版本（`docs/design_notes.md:161`）；chunk engine 用 copy-on-write 配新位置寫 pending，舊位置在 commit 前照常給讀者讀：`src/storage/chunk_engine/src/core/engine.rs:386-434`。這不是交易層的 snapshot isolation（沒有跨 chunk 的快照），只是讓讀不被寫擋住。

---

### 2.5 Lost update（更新遺失）

**概念**（對應 DDIA 第 7 章〈Preventing Lost Updates〉）
兩個交易都做「讀 → 改 → 寫回」，後寫的覆蓋前寫的，前一個的修改就不見了。解法有原子操作、顯式加鎖、自動偵測（資料庫發現衝突就讓其中一個重來）、compare-and-set 等。

**3FS 怎麼做**
- **metadata 靠 FDB 的衝突偵測**：只要 read-modify-write 時用的是會加入 conflict 集合的讀（或手動 `addReadConflict`），另一個交易先 commit 改了同一個 key，本交易 commit 會失敗並重試。
  - 例：檔案 inode 的長度/屬性更新，寫回前把 inode 加入 read conflict：`src/meta/store/ops/BatchOperation.cc:97-102`。
  - 例：rename 時把被搬目錄的 inode 用一般 `load`（非 snapshot）讀出再改 parent：`src/meta/store/ops/Rename.cc:309-318`。
- **降低衝突頻率：把同一個 inode 的更新集中到同一台 meta**。`sync`/`close`/`create`/`setAttr` 依 inode（或 parent）以 rendezvous hash 選出「owner」meta server，不是自己就轉送：`src/meta/service/MetaOperator.cc:321-337`、`src/meta/service/MetaOperator.cc:362-367`。`docs/design_notes.md:95` 說明目的是減少多台 meta 同時更新同一檔案長度造成的交易衝突。owner 在交易內再確認一次自己還是 owner：`src/meta/store/ops/BatchOperation.cc:57-64`（細節見 §3.7）。
- **chunk 資料**：同一個 chunk 的所有寫入在 head 被 chunk 鎖序列化（`src/storage/service/StorageOperator.cc:368-380`），而且 chunk 寫入是「寫某個 byte 範圍」而不是 read-modify-write，所以不存在 lost update 的形狀。兩個 client 寫同一範圍，結果就是後到 head 的那個。

---

### 2.6 Write skew 與 phantom

**概念**（對應 DDIA 第 7 章〈Write Skew and Phantoms〉）
兩個交易各自讀了某些資料、根據讀到的結果決定寫**不同**的東西，各自看起來都合法，合起來卻違反不變式（書中例子是值班醫生都請假）。snapshot isolation 擋不住它，因為兩個交易寫的 key 根本不重疊。phantom 是它的特殊形式：交易檢查「某條件下沒有資料」，另一個交易同時插入了符合條件的新資料。

**3FS 怎麼做：在 FDB 上手動把「被依賴的讀」加進 conflict 集合**
3FS 的路徑解析一律用 snapshot 讀，**不**加入 conflict（`src/meta/store/PathResolve.h:22-27`）。這是刻意縮小 conflict 集合，避免每個操作都和所有碰過同一條路徑的操作衝突。代價是程式必須自己挑出「正確性真正依賴的讀」，手動加回 conflict。程式碼裡有三個很好的教材：

1. **rename 形成迴圈（經典 write skew）**
   兩個並行的 rename：`mv /a /b/x` 和 `mv /b /a/y`。各自檢查「目的地不是我的子孫」都通過，但兩個都 commit 後就形成迴圈。3FS 在 `checkLoop` 裡沿著目的地的祖先一路往上讀，用的是**非 snapshot** 的 `Inode::load`，原始碼註解明講是為了把祖先加入 read conflict：`src/meta/store/Inode.cc:207-208`（整個函式 `src/meta/store/Inode.cc:196-224`；呼叫處 `src/meta/store/ops/Rename.cc:71-103`）。第二個 commit 的交易會因為祖先 inode 被第一個改了 parent 而衝突重試，重試時就會看到迴圈並拒絕。
   另外 rename 也把 src/dst 的 parent inode 與目錄項加進 conflict：`src/meta/store/ops/Rename.cc:301-306`。

2. **mkdir 遇上 rmdir（phantom 的一種）**
   mkdir 依賴「parent 還存在」和「這個名字還沒被用」。它把 parent inode 和**還不存在的**目錄項 key 加進 conflict：`src/meta/store/ops/Mkdirs.cc:65-70`（註解寫著 "prevent parent is removed concurrently" 與 "prevent concurrent create"）。對「不存在的 key」加 conflict，就是在擋 phantom：別人在這個 key 插入資料，本交易就衝突（FDB 語意：conflict range 涵蓋不存在的 key）。

3. **刪空目錄（range 版的 phantom 防護）**
   rmdir 要確認目錄是空的。`checkEmpty` 用**非 snapshot** 的 `getRange` 掃整個目錄前綴（limit 1）：`src/meta/store/DirEntry.cc:316-325`；呼叫處 `src/meta/store/ops/Remove.cc:148-156`。因為掃描範圍是整個目錄的 key 區間，任何並行交易在這個目錄下建新項目都會落在這個 read conflict range 內，讓 rmdir 失敗重試（FDB 語意：read conflict range 是實際讀的範圍，空結果時就是整個請求範圍）。這等同書上講的「predicate lock / index-range lock」效果，只是由 FDB 的樂觀衝突偵測實現。

---

### 2.7 Serializable 的三種實作：真正串行、2PL、SSI

**概念**（對應 DDIA 第 7 章〈Serializability〉）
最強的隔離等級：並行執行的結果，等同於某個串行順序。書中介紹三種做法：
- **真正串行執行**：單執行緒一個一個跑，交易要短、資料要能放進記憶體或分區。
- **兩階段鎖（2PL）**：讀要拿共享鎖、寫要拿互斥鎖，鎖一直持有到交易結束；悲觀，會互相阻塞與死鎖。
- **Serializable Snapshot Isolation（SSI）**：樂觀；在 snapshot isolation 上追蹤讀寫依賴，commit 時發現危險就中止重試。

**3FS 怎麼做**
- **metadata：樂觀衝突偵測（FDB）**。3FS 的設計文件說 FDB 提供 SSI（`docs/design_notes.md:63`），衝突時 meta 自動重試（`docs/design_notes.md:83`）。嚴格說 FDB 的機制是「commit 時檢查 read conflict 集合裡的 key 在 read version 之後是否被改過」，能達到 strict serializability（FDB 語意，不在 repo 內）。3FS 端看得到的是：
  - 重試策略：FDB 判定可重試才重試，最多 `maxRetryCount` 次（預設 10）、指數退避：`src/fdb/FDBRetryStrategy.h:27-31`、`src/fdb/FDBRetryStrategy.h:59-109`。
  - 可重試錯誤的分類：`kConflict`、`kTooOld` 等可以；`kMaybeCommitted` 是否重試由參數決定：`src/common/kv/ITransaction.cc:12-31`。
  - meta 操作預設**允許**對 maybe-committed 重試（`src/meta/store/MetaStore.h:54`、`src/meta/store/Operation.h:160`），這就逼出了冪等性問題，見 §3.2。
- **2PL：metadata 沒用**。repo 裡 meta 的交易路徑沒有分散式鎖。唯一像鎖的是目錄的 `LockDirectory`（`src/meta/store/ops/LockDirectory.cc:34-47`），那是應用層功能，不是交易隔離手段。
- **真正串行執行：資料層用了「每個 chunk 一條隊」的版本**。同一個 chunk 的寫入在 head 上被 chunk 鎖完全串行化（`src/storage/service/StorageOperator.cc:368-380`、`docs/design_notes.md:159`）。不同 chunk 之間完全並行。這和書上「分區後每個分區單執行緒」的思路是同一類。

**注意一個與設計文件不一致的小地方**：`docs/design_notes.md:155-159` 描述的順序是「先 RDMA Read 拉資料、再拿 chunk 鎖」；程式碼裡是 `handleUpdate` 先拿鎖（`src/storage/service/StorageOperator.cc:368-380`），再在 `doUpdate` 裡 RDMA Read（`src/storage/service/StorageOperator.cc:565-596`）。以程式碼為準。

---

## 3. 第 8 章 The Trouble with Distributed Systems（分散式系統的麻煩）

### 3.1 部分失效與系統模型

**概念**（對應 DDIA 第 8 章〈Faults and Partial Failures〉與〈System Model and Reality〉）
單機程式要嘛正常、要嘛整台掛掉；分散式系統則常是「一部分壞了、一部分正常、而且你不一定知道哪部分壞了」。設計時要先選一個系統模型：時間上是同步、部分同步還是非同步；節點故障是 crash-stop（掛了就不回來）、crash-recovery（會回來但記憶體狀態丟了）還是 Byzantine（會說謊）。

**3FS 選的模型**
- **故障模型：fail-stop／crash-recovery**。`docs/design_notes.md:177` 明講 mgmtd 用心跳偵測 "fail-stop failures"。storage 重啟後會先讓 mgmtd 把自己判成下線再重新加入（`src/storage/service/Components.cc:87-122`），這是 crash-recovery 的處理方式：回來的節點不被信任，要重新同步。
- **把「半壞」轉成「全壞」**：storage 遇到寫入磁碟失敗或 metadata 寫入失敗，主動把該磁碟上的 target 下線：`src/storage/service/StorageOperator.cc:605-611`。轉發時發現本地 RDMA buffer 的 checksum 跟 client 給的不一樣，判斷本機記憶體壞了，直接送 `SIGUSR2` 自殺：`src/storage/service/ReliableForwarding.cc:264-276`。這是在把難處理的 gray failure 主動轉成 fail-stop。
- **時間模型：部分同步**。大量依賴逾時（心跳 60s、RPC timeout、重試總時間），隱含假設「大多數時候網路和處理延遲有上限」。
- **不處理 Byzantine**，見 §3.8。

---

### 3.2 不可靠的網路、逾時與重試的冪等性

**概念**（對應 DDIA 第 8 章〈Unreliable Networks〉）
送出一個請求沒收到回覆，你無法分辨：請求沒送到？對方掛了？對方處理完了但回覆丟了？還是只是慢？唯一的工具是逾時，但逾時之後重送，對方可能**執行兩次**。所以重試必須搭配冪等性設計（同一個請求執行多次，效果等於一次）。

**3FS 怎麼做：每一層都有自己的去重機制**

*storage 寫入：channel + seqnum*
- client 每個進行中的寫入佔用一個 `UpdateChannel`（id + seqnum）：`src/client/storage/UpdateChannelAllocator.cc:39-63`。**寫入重試時保留同一個 channel 與 seqnum**（`reallocate=false`）：`src/client/storage/StorageClientImpl.cc:1836`。
- server 端以 `(clientId, chainId, channelId)` 為 key 快取上一次結果：`src/storage/service/ReliableUpdate.cc:38-51`。
  - seqnum 比快取舊 → `kDuplicateUpdate`：`src/storage/service/ReliableUpdate.cc:64-69`
  - seqnum 相同且 target 世代沒變 → 直接回快取結果，不重做：`src/storage/service/ReliableUpdate.cc:71-99`
  - 舊後端在「上次做到一半」時，撿回上次分配的 `updateVer` 重送：`src/storage/service/ReliableUpdate.cc:105-109`，下游就會以 `kChunkStaleUpdate` / `kChunkCommittedUpdate` 視為重複而成功（`src/storage/service/StorageOperator.cc:416-431`）。這是一種「往前推完（roll-forward）」而不是回滾的處理。
- 限制：這個快取在記憶體裡（`shards_`），沒有持久化；快取 key 也綁了 target 的 `generationId`（`src/storage/service/ReliableUpdate.cc:72-73`）。head 換人或重啟後的重試會被當成新寫入重做。對「把某段 bytes 寫成某個值」這種操作，重做一次的結果相同（推論：這依賴寫入本身是覆寫語意；truncate/extend 是否也完全安全我沒有逐一驗證）。
- 快取的過期清理預設關閉（條件：`clean_up_expired_clients` 預設 `false`，`src/storage/service/ReliableUpdate.h:22-23`）。

*metadata：兩套機制並用*
- **idempotent 記錄表**：把 `(clientId, requestId)` → 操作結果寫進 FDB（key 前綴 `IDEM`，`src/common/kv/KeyPrefix-def.h:17`），和操作本身在同一個交易 commit：`src/meta/store/Operation.h:221-241`；重試時先查表，有記錄就直接回舊結果（`src/meta/store/Idempotent.h:47-83`）。記錄 30 分鐘後清掉：`src/meta/base/Config.h:116-117`、`src/meta/store/Idempotent.h:100-133`。
  - 條件：只有 remove（`recursive` 或 `idempotent_remove`，後者預設 `true`）和 rename（`moveToTrash` 或 `idempotent_rename`，後者預設 `false`）走這張表：`src/meta/store/ops/Remove.cc:52-62`、`src/meta/store/ops/Rename.cc:63-69`、`src/meta/base/Config.h:118-119`。
- **在目錄項上留下「誰建的」**：create / mkdirs / symlink / hardlink / rename 會把請求的 `uuid` 寫進新的目錄項；重試時若發現目錄項已存在而且 `uuid` 等於自己，就當成功回傳。原始碼註解直接寫出原因："this may happens when FDB returns commit_unknown_result, or we failed to send response to client"：`src/meta/store/ops/BatchOperation.cc:533-538`、`src/meta/store/ops/Mkdirs.cc:37-46`、`src/meta/store/ops/Rename.cc:247-255`。
- 請求 `uuid` 在建構請求時就隨機產生（`src/fbs/meta/Service.h:196`、`src/fbs/meta/Service.h:471` 等），client 重試時重送**同一個** `req` 物件（`src/client/meta/MetaClient.cc:339-398`），所以 uuid 不變。

**這裡值得特別記住的是 FDB 的 `commit_unknown_result`**：交易送出 commit 後網路斷了，client 不知道它有沒有成功。這正是本節「無法分辨」的具體案例。3FS 選擇預設對這種錯誤重試（`src/meta/store/MetaStore.h:54`），所以必須有上面這些去重機制，否則重試的 mkdir 會回 `EEXIST`、重試的 rename 會回 `ENOENT`。

*mgmtd 心跳*
心跳帶遞增的 `hbVersion`，mgmtd 拒絕不比上次新的心跳：`src/mgmtd/ops/HeartbeatOperation.cc:57-60`；client 收到「版本過舊」會把自己的版本跳到伺服器記錄 +1 再送一次：`src/client/mgmtd/MgmtdClient.cc:716-727`。這處理了網路重排或重送造成的舊心跳。

---

### 3.3 逾時與故障偵測

**概念**（對應 DDIA 第 8 章〈Detecting Faults〉與〈Timeouts and Unbounded Delays〉）
逾時設太短，會把只是慢的節點誤判成死掉，引發不必要的 failover（甚至連鎖崩潰）；設太長，真的掛了要很久才發現，期間服務受影響。網路延遲沒有上限，所以沒有「正確」的逾時值，只能依實測延遲分佈取捨。

**3FS 的各層逾時（預設值）**

| 機制 | 預設 | 位置 |
|---|---|---|
| 節點送心跳間隔 | 10s | `src/client/mgmtd/MgmtdClient.h:21-22` |
| mgmtd 判定心跳失敗 | 60s | `src/mgmtd/service/MgmtdConfig.h:16` |
| mgmtd 檢查間隔 | 10s | `src/mgmtd/service/MgmtdConfig.h:15` |
| mgmtd 重算 chain 狀態間隔 | 1s | `src/mgmtd/service/MgmtdConfig.h:22` |
| client/storage 拉路由間隔 | 10s | `src/client/mgmtd/MgmtdClient.h:19-20` |
| storage 轉發重試總時間 | 60s | `src/storage/service/ReliableForwarding.h:18-20` |
| storage client 重試：起始/上限/總時間 | 10s / 30s / 60s | `src/client/storage/StorageClient.h:360-366` |
| meta client 單次 RPC / 總重試 | 5s / 1min | `src/client/meta/MetaClient.h:61-67` |
| meta Distributor 判定其他 meta 死亡 | 30s | `src/meta/components/Distributor.h:31-34` |

**3FS 的取捨**
- 心跳失敗門檻 60s 相對保守，偏向「寧可晚發現也不誤判」（推測）。代價就是 §1.2 說的：一個 storage 靜默死掉，它所在 chain 的寫入可能卡到這個量級的時間（推論：實際時間是心跳門檻＋檢查間隔＋路由傳播，加上 head 端的轉發重試上限）。
- **client 端的「快速繞道」**：讀取時，同一個請求內某個 target 失敗次數到 `max_failures_before_failover`（預設 1）就跳過它改讀別的副本：`src/client/storage/StorageClientImpl.cc:551-565`。這不需要等 mgmtd 的判定，因為讀任一副本都合法。meta client 也有對應的換台邏輯：`src/client/meta/MetaClient.cc:434-454`。
- **新 primary 上任時重置計時**：mgmtd 換 primary 後，載入所有 target 時把它們的時間戳設成「現在」（`src/mgmtd/background/MgmtdLeaseExtender.cc:106-109`），新 primary 要再等一整個心跳門檻才會判任何人死亡，避免剛上任就因為沒有歷史心跳而大量誤判。

---

### 3.4 不可靠的時鐘

**概念**（對應 DDIA 第 8 章〈Unreliable Clocks〉）
每台機器的時鐘都會漂移，NTP 同步也有誤差，甚至會往回跳。「牆上時鐘」（wall clock，表示現在幾點）適合記錄時間點，但拿來跨機器比較先後或判斷 lease 是否過期很危險；「單調時鐘」（monotonic clock）只保證不倒退，適合量本機經過了多久，但不同機器的單調時鐘之間毫無可比性。

**3FS 的時鐘使用盤點**
`UtcClock` 是牆上時鐘（`src/common/utils/UtcTime.h:13-15`），`SteadyClock` 是單調時鐘。

| 用途 | 用哪個鐘 | 是否跨機器比較 | 位置 |
|---|---|---|---|
| mgmtd 判定心跳逾時 | SteadyClock（mgmtd 本機） | 否 | `src/mgmtd/background/MgmtdHeartbeatChecker.cc:25-49`、`src/mgmtd/ops/HeartbeatOperation.cc:173-180` |
| mgmtd lease 起訖時間 | UtcTime（寫 lease 的 mgmtd 的牆上時鐘，存進 FDB） | **是**：別台 mgmtd 用自己的牆上時鐘判斷它過期了沒 | `src/mgmtd/store/MgmtdStore.cc:154-189`、`src/mgmtd/background/MgmtdLeaseExtender.cc:202-207`、`src/core/app/ServerEnv.h:48` |
| mgmtd 自己是否還信任自己的 lease | UtcTime，留 20s 安全邊際 | 是（同上） | `src/mgmtd/service/MgmtdState.cc:66-75`、`src/mgmtd/service/MgmtdConfig.h:12` |
| 心跳時間戳合理性檢查 | UtcTime，節點與 mgmtd 時差超過 30s 就拒收心跳 | **是** | `src/mgmtd/ops/HeartbeatOperation.cc:133-141`、`src/mgmtd/service/MgmtdConfig.h:13` |
| meta 判定其他 meta 死亡 | FDB versionstamp ＋ 本機 SteadyClock | 否 | `src/meta/components/Distributor.cc:260-279`、`src/meta/components/Distributor.cc:282-284` |
| meta 判定 client session 過期 | UtcTime（mgmtd 寫的 `lastExtend` vs meta 本機時間），留 10s 寬限 | **是** | `src/meta/components/SessionManager.cc:66`、`src/mgmtd/ops/ExtendClientSessionOperation.cc:100` |
| meta 判斷檔案 session 是否「剛建立」 | UtcTime（session 的 timestamp vs 本機），1 分鐘寬限 | **是** | `src/meta/components/SessionManager.cc:115-123` |
| idempotent 記錄過期 | UtcTime | 是（寫入與清理可能在不同 meta） | `src/meta/store/Idempotent.h:93`、`src/meta/store/Idempotent.h:106-125` |
| 各種重試退避 | SteadyClock | 否 | `src/common/utils/ExponentialBackoffRetry.h:15`、`src/common/utils/ExponentialBackoffRetry.h:37` |
| chunk metadata 的 timestamp | UtcClock | 只是記錄 | `src/storage/store/ChunkReplica.cc:245` |

**觀察**
- **故障偵測刻意不跨機器比時間**：mgmtd 用本機單調時鐘記「上次收到某節點心跳是什麼時候」，meta 的 Distributor 更巧妙：每台 meta 把一個 FDB versionstamp 寫進自己的 key，其他 meta 只看「這個值有沒有變」，再用自己的單調時鐘量「多久沒變」（`src/meta/components/Distributor.cc:260-284`）。完全不需要時鐘同步。
- **lease 依賴牆上時鐘**：mgmtd lease 的 `leaseEnd` 是持有者用自己的牆上時鐘算出來的，接手者用自己的牆上時鐘判斷它過期沒。安全性因此依賴各 mgmtd 時鐘誤差小於安全邊際（推論：`suspicious_lease_interval` 20s 是 primary 端的提前放手量）。不過 3FS 另外用 FDB 交易擋住了「時鐘錯亂的舊 primary 寫入」，見 §3.7。
- **心跳的 30s 時差檢查**：時鐘偏太多的節點，心跳會被拒（`src/mgmtd/ops/HeartbeatOperation.cc:133-141`），最後被判定失效。這等於把「時鐘壞掉」轉成 fail-stop（推論）。

---

### 3.5 Process pause（行程暫停）

**概念**（對應 DDIA 第 8 章〈Process Pauses〉）
一個行程可能因為 GC、VM 被暫停、swap、作業系統排程等原因，在任意兩行程式碼之間停住幾秒甚至更久，而它自己完全不知道。典型災難：檢查「我的 lease 還有效」→ 停住 → lease 其實已過期且被別人拿走 → 醒來繼續當自己是 leader 去寫資料。書中結論是：不能只靠「先檢查再行動」，資源端要能拒絕過期持有者（fencing）。

**3FS 怎麼做**
- **mgmtd：把「檢查 lease」和「寫入」放進同一個 FDB 交易**。`withReadWriteTxn` 在每個寫交易開頭讀 lease key 並確認「primary 是我且還沒過期」：`src/mgmtd/service/helpers.h:44-56`、`src/mgmtd/store/MgmtdStore.cc:191-204`。條件：`validate_lease_on_write` 預設 `true`（`src/mgmtd/service/MgmtdConfig.h:23`）。lease 讀取用的是會加入 conflict 的 `get`（`src/common/serde/SerdeHelper.h:21-22`），所以就算舊 primary 在檢查後停住，只要新 primary 在這期間改了 lease key，舊 primary 的交易 commit 時會衝突失敗（FDB 語意）。暫停無法造成「過期 primary 寫入 FDB」。
- **mgmtd 的記憶體內操作沒有這層保護**：例如回應 `getRoutingInfo` 只做記憶體內的 `doAsPrimary` 檢查（`src/mgmtd/ops/GetRoutingInfoOperation.cc:6-21`、`src/mgmtd/service/helpers.h:25-29`），舊 primary 醒來後可能回出舊的路由。這個舊路由本身會在 storage 端被 `chainVersion` 擋下（見 §3.7）。
- **storage：拿鎖後重新檢查**。head 等 chunk 鎖可能等很久，拿到鎖之後立刻再用嚴格模式檢查一次 chain 版本：`src/storage/service/StorageOperator.cc:382-387`。
- **meta：操作有 deadline**。`OperationDriver` 每次重試前檢查是否超過 deadline，超過就放棄：`src/meta/store/Operation.h:178-183`。這限制了一個請求在 meta 端被拖延的時間，但不是 fencing。

---

### 3.6 真相由多數決定：節點不能自己說了算

**概念**（對應 DDIA 第 8 章〈Knowledge, Truth, and Lies〉中的〈The Truth Is Defined by the Majority〉）
一個節點對外界的認知可能是錯的：它以為自己是 leader、以為別人死了、以為自己還活著。所以「誰是 leader」「誰死了」這類判斷不能由單一節點自己決定，要由某種法定多數（quorum）或外部權威決定，並且個別節點必須服從，即使它覺得自己沒問題。

**3FS 怎麼做：mgmtd 是唯一權威，FDB 是 mgmtd 的權威**
- **誰死了**：只有 mgmtd primary 判定（`src/mgmtd/background/MgmtdHeartbeatChecker.cc:85-96`，非 primary 直接跳過）。
- **storage 服從 mgmtd 的判定**：
  - 本地認為自己 UPTODATE，但 mgmtd 說它 OFFLINE / LASTSRV / WAITING，就把自己的 target 關掉（本地狀態改成 OFFLINE、釋放 `storageTarget`）：`src/storage/service/TargetMap.cc:329-341`、`src/storage/service/TargetMap.cc:214-219`。
  - 重啟時，就算資料完好，也要等 mgmtd 先把它判下線（`src/storage/service/Components.cc:87-122`）。
- **mgmtd 也不能自己說了算**：「誰是 primary」存在 FDB 裡，由 FDB 交易決定（§3.7）；FDB 內部才有真正的多數決（FDB 語意，不在 repo）。
- **舊消息不算數**：心跳裡回報的 target 若帶著比目前 chain 舊的 `chainVersion`，mgmtd 直接忽略這筆回報（連時間戳都不更新）：`src/mgmtd/service/RoutingInfo.cc:55-61`（條件：`heartbeat_ignore_stale_targets` 預設 `true`，`src/mgmtd/service/MgmtdConfig.h:27`）。

---

### 3.7 Lease 與 fencing token

**概念**（對應 DDIA 第 8 章〈The leader and the lock〉與〈Fencing tokens〉）
lease 是有期限的鎖：持有者要定期續約，過期沒續就自動失效，避免持有者死掉後鎖永遠拿不回來。但 lease 單獨使用不安全（§3.4 的時鐘、§3.5 的暫停）。fencing token 是補救：每次授予 lease 時配一個單調遞增的號碼，持有者存取資源時要帶上這個號碼，**資源端**記住看過的最大號碼，拒絕號碼更小的請求。這樣舊持有者即使還以為自己有權，也寫不進去。

**3FS 的 lease**

*mgmtd primary 的 lease*
- 存在 FDB 的單一 key（`SING` 前綴＋`MgmtdLease`）：`src/mgmtd/store/MgmtdStore.cc:16-17`；內容是 primary 是誰、`leaseStart`、`leaseEnd`、release 版本：`src/fbs/mgmtd/MgmtdLeaseInfo.h:20-23`。
- 每 10s 嘗試續約，租期 60s：`src/mgmtd/service/MgmtdConfig.h:10-11`、`src/mgmtd/background/MgmtdBackgroundRunner.cc:39-42`。
- 續約邏輯是一個在 FDB 交易裡的 compare-and-set：沒有 lease 或 lease 已過期就搶；是自己的就延長；是別人的且未過期就不動：`src/mgmtd/store/MgmtdStore.cc:154-189`。release 版本不能倒退（預設檢查，`src/mgmtd/store/MgmtdStore.cc:167-170`、`src/mgmtd/service/MgmtdConfig.h:34`），避免舊版程式搶回 primary（推測：程式碼只寫 "releaseVersion should not rollback"）。
- primary 自己判斷 lease 時預留 20s：`now + 20s < leaseEnd` 才算數，而且剛上任還在載入（bootstrapping）時也不算：`src/mgmtd/service/MgmtdState.cc:66-75`。
- 新 primary 上任時從 FDB 重新載入全部狀態，並把 `routingInfoVersion` 從 FDB 記錄 +1，保證版本號跨 failover 仍單調：`src/mgmtd/background/MgmtdLeaseExtender.cc:20-29`、`src/mgmtd/background/MgmtdLeaseExtender.cc:128-188`。

*storage 節點對 mgmtd 的「lease」*
`docs/design_notes.md:177` 把心跳描述成「向 manager 續 lease」，並說節點和 manager 失聯 T/2 秒就會停止服務並退出。**這個「T/2 自我退出」我在 `src/` 裡沒有找到對應實作**（`MgmtdClient` 的背景心跳失敗只記 log：`src/client/mgmtd/MgmtdClient.cc:293-304`、`src/client/mgmtd/MgmtdClient.cc:714-727`）。以程式碼來看，storage 不會因為連不上 mgmtd 而自行停止；它只在**收到**新路由、發現自己被降級時才關掉 target（§3.6）。見文末待查核清單。

**3FS 的 fencing token：`chainVersion`（以及 FDB 交易本身）**

3FS 有三層 fencing，都是「資源端檢查號碼」：

1. **storage 檢查請求的 chainVer（routing 層）**
   - client 寫入、上游轉發、resync 的請求都帶 `vChainId = {chainId, chainVer}`；storage 端預設要求**完全相等**，不等就回 `kRoutingVersionMismatch`：`src/storage/service/TargetMap.cc:55-65`、預設嚴格在 `src/storage/service/TargetMap.h:86-87`。
   - 轉發時，上游用**自己目前的** chainVer 覆寫請求（`src/storage/service/ReliableForwarding.cc:149`），下游的版本若比上游新，上游判定自己過時、重新取路由再試：`src/storage/service/ReliableForwarding.cc:125-133`、`src/storage/service/ReliableForwarding.cc:239-246`。
   - 所以：一個被踢出鏈、還以為自己是 head 的舊節點，它轉發出去的寫入會被已拿到新路由的下游拒絕。`docs/design_notes.md:167` 描述的就是這個「一直重送直到下游拿到新路由」的情境。

2. **chunk 上持久化的 chainVer（資料層）**
   這是最接近書中 fencing token 的形式：版本號**跟資料一起存**，資源端拒絕號碼較小的寫入。
   - 舊後端：chunk 已 commit 且記錄的 chainVer 比請求的大，拒絕並回報 fatal：`src/storage/store/ChunkReplica.cc:186-191`；commit 時同樣檢查：`src/storage/store/ChunkReplica.cc:419-424`；commit 成功時把目前 chainVer 寫進 chunk：`src/storage/store/ChunkReplica.cc:451-454`。
   - chunk engine：`req.chain_ver < meta.chain_ver` 就拒絕：`src/storage/chunk_engine/src/core/engine.rs:351-357`。
   - resync 比對時也用 chainVer 判斷誰比較新：`src/storage/sync/ResyncWorker.cc:223-238`。

3. **FDB 交易內檢查 lease（控制層）**
   mgmtd 每個寫交易都在同一個交易內確認 lease 屬於自己（§3.5）。這裡的「fencing token」就是 FDB 裡那個 lease key 本身：舊 primary 讀到的版本若已被改，交易 commit 不會成功（FDB 語意）。
   meta 的 Distributor 用同樣手法確保「inode owner」：在交易內讀 `\xff/metadataVersion`，若伺服器對照表在本交易開始後改了，就拒絕（`src/meta/components/Distributor.cc:98-119`，對照表更新時同步寫這個 key：`src/meta/components/Distributor.cc:137-140`、`src/meta/components/Distributor.cc:184-193`）。

**fencing 的漏洞（推論）**
fencing 擋的是「舊的寫」。**讀**則沒有完整的保護：一個被 mgmtd 移出鏈、但自己還沒拿到新路由的舊 target（例如它和 mgmtd 斷線、但和部分 client 還連得上），對手上也是舊路由的 client 來說，雙方的 chainVer 一致，讀取檢查會通過（`src/storage/service/StorageOperator.cc:101-117`），可能回傳舊資料。這個時間窗受限於 client 的路由刷新（預設 10s），以及這個舊 target 何時拿到新路由。若 `docs/design_notes.md:177` 的「失聯 T/2 自我退出」有實作，這個窗口就會被封住；但我沒在程式碼找到它。另外，讀取端有個開關可以完全不檢查 chainVer（條件：`batch_read_ignore_chain_version` 預設 `false`，`src/storage/service/StorageOperator.h:38`）。

---

### 3.8 Byzantine 故障

**概念**（對應 DDIA 第 8 章〈Byzantine Faults〉）
節點不只會掛，還會說謊或亂傳資料（被入侵、硬體錯誤、bug）。能容忍這種故障的協定（BFT）需要超過三分之二的節點誠實，成本很高，一般資料中心內部系統不做。

**3FS 未涉及 Byzantine 容錯**，所有節點都被假設是誠實的（fail-stop 模型，§3.1）。它做的是「偵測意外損壞」，這和 BFT 是兩回事：
- 資料 checksum（CRC32C）：storage 收到寫入時用 client 提供的 checksum 驗資料：`src/storage/store/ChunkReplica.cc:193-207`、`src/storage/chunk_engine/src/core/engine.rs:309-319`。
- 上下游 checksum 比對：head 會比對自己算的和後繼回報的 chunk checksum，不一致回 `kChecksumMismatch`：`src/storage/service/StorageOperator.cc:469-487`。
- 叢集 ID 檢查防止誤連到別的叢集：`src/mgmtd/service/MgmtdState.cc:47-52`。
- 身份驗證預設關閉（條件：`authenticate` 預設 `false`，`src/mgmtd/service/MgmtdConfig.h:35`）。

---

## 4. 第 9 章 Consistency and Consensus（一致性與共識）

### 4.1 Linearizability（線性一致性）

**概念**（對應 DDIA 第 9 章〈Linearizability〉）
讓整個分散式系統表現得像「只有一份資料、所有操作都是瞬間原子完成」。關鍵的可觀察後果是：一旦有任何讀者讀到了新值，之後（以真實時間計）開始的任何讀都不能再讀到舊值。它是「最近性」保證，不同於可串行化（serializability 講的是交易之間的隔離，不管真實時間順序）。實作線性一致需要單主或共識，代價是延遲與在網路分割時的可用性。

**3FS 怎麼做**
- **metadata：透過 FDB 得到**。FDB 提供 strict serializability（可串行＋線性一致，FDB 語意）。meta server 無狀態、每次都進 FDB，所以 metadata 操作是線性一致的。例外是開啟 GRV cache 時的唯讀操作（條件：預設關，§1.6）。
- **chunk 資料：設計目標是強一致**（`docs/design_notes.md:11` 說 CRAQ "to ensure strong consistency"），具體要看後端：
  - 舊 `ChunkReplica` 後端：有 pending 版本的副本拒絕回答（`kChunkNotCommit`），只有「確定沒有進行中寫入」的副本回答，加上寫入在 ack 前全員 commit，一般情況下符合 CRAQ 的強一致設計（推論）。
  - chunk engine 後端：讀進行中的 chunk 會拿到該副本最後的 commit 版本（§1.6）。tail 先 commit、head 後 commit，於是存在「讀者 A 從 tail 讀到新值後，讀者 B 從 head 讀到舊值」的可能（推論）。這不符合線性一致的定義，但只在寫入尚未 ack 的時間窗內發生。
  - 路由過期的舊 target 回答舊資料的可能（§3.7 末段），也是線性一致的缺口（推論）。
- **mgmtd 的 RoutingInfo：不是線性一致的**。它是拉取式散播，不同節點同一時刻可能持有不同版本；3FS 的做法是接受這點，再用版本號在資料路徑上把「拿著舊版本的人」擋掉。

---

### 4.2 CAP

**概念**（對應 DDIA 第 9 章〈The Cost of Linearizability〉）
網路分割發生時，系統只能二選一：要嘛保持線性一致（某些節點拒絕服務，犧牲可用性），要嘛繼續服務（可能回舊資料，犧牲一致性）。書中也提醒 CAP 的定義很窄，實務上更常見的取捨是「一致性 vs 延遲」。

**3FS 的選擇偏向一致性（CP）**
- 一條 chain 全部 SERVING 都失聯時，不會讓其他「可能有完整資料」的副本頂上，而是等原本排第一的 LASTSRV 回來：`src/mgmtd/service/updateChain.cc:31-42`（註解承認這會讓整條鏈等待）。這是明確地拿可用性換「不讀到可能缺資料的副本」。
- 寫入要整條鏈確認（§1.2）：分割導致某個副本聯絡不上，寫入就卡住直到它被移出鏈。
- mgmtd 選主、meta 的一切都依賴 FDB；FDB 失去多數時，mgmtd 無法續 lease、meta 無法交易，整個控制面停擺（FDB 語意＋推論）。
- 讀取路徑是個例外：讀可以繞過失敗的副本（`src/client/storage/StorageClientImpl.cc:551-565`），這是因為 CRAQ 本來就允許讀任一副本，不需要犧牲一致性。

---

### 4.3 順序、因果與序號

**概念**（對應 DDIA 第 9 章〈Ordering Guarantees〉與〈Sequence Number Ordering〉）
很多一致性問題本質上是順序問題：誰先誰後、哪個結果依賴哪個。因果順序是部分順序（有些事件之間沒有先後關係）；全序則是每兩個事件都排得出先後。單主系統可以讓 leader 發號碼給出全序；多節點可以用 Lamport timestamp 這類邏輯時鐘產生和因果一致的全序。但光有全序號碼還不夠解決「唯一性」類的問題，因為你要等到確定沒有更小的號碼才能下結論。

**3FS 裡的各種序號（全部是「單一發號者」的版本號，沒有 Lamport/vector clock）**

| 序號 | 誰發 | 範圍 | 用途 | 位置 |
|---|---|---|---|---|
| `updateVer` / `commitVer` | chain 的 head | 單一 chunk | 決定寫入順序、偵測重複/漏號 | `src/storage/store/ChunkReplica.cc:211-239` |
| `chainVersion` | mgmtd primary | 單一 chain | chain 成員變更、fencing | `src/mgmtd/service/MgmtdData.cc:133-136` |
| `routingInfoVersion` | mgmtd primary（跨 failover 持久化在 FDB） | 全叢集 | 路由新舊判斷 | `src/mgmtd/service/helpers.h:72-81`、`src/mgmtd/background/MgmtdLeaseExtender.cc:20-29` |
| `hbVersion` | 每個發心跳的節點 | 單一節點 | 丟棄重排/重送的舊心跳 | `src/mgmtd/ops/HeartbeatOperation.cc:57-60` |
| channel `seqnum` | 每個 storage client | 單一 client | 寫入去重 | `src/client/storage/UpdateChannelAllocator.cc:41`、`src/client/storage/UpdateChannelAllocator.cc:58` |
| FDB versionstamp | FDB | 全 FDB 叢集 | meta 存活偵測、Distributor 對照表版本 | `src/common/kv/ITransaction.h:28-32`、`src/meta/components/Distributor.cc:282-284` |

**觀察**
- 每個序號都只有一個發號者，所以號碼天然有全序，不需要邏輯時鐘。這是「把決策集中」的直接紅利。
- 唯一真正全域的順序來源是 FDB 的 commit version（versionstamp 前 8 bytes 就是它，`src/common/kv/ITransaction.h:28-30` 的註解）。3FS 把需要全域順序的東西都放在 FDB 裡。
- chunk 之間沒有順序關係：兩個 client 分別寫 chunk A、chunk B，系統不記錄它們的相對順序。

---

### 4.4 Total order broadcast（全序廣播）

**概念**（對應 DDIA 第 9 章〈Total Order Broadcast〉）
一種訊息傳遞協定，保證兩件事：訊息不會丟（送達所有節點），而且所有節點收到的順序完全相同。它和共識等價，可以用來實作複製狀態機（每個副本照同一順序套用同一串操作就會得到相同狀態），也可以用來實作線性一致的儲存。

**3FS 怎麼做**
- **3FS 沒有實作通用的全序廣播。** 需要它的地方（mgmtd 狀態、metadata）都放在 FDB 裡；FDB 的 commit 順序就是一個全序（FDB 語意）。
- **chain 寫入可以看成「單一 chunk 範圍內的全序廣播」**：head 給每個寫入一個 `updateVer`，沿鏈傳給每個副本，下游拒絕跳號（`kChunkMissingUpdate`，`src/storage/store/ChunkReplica.cc:225-230`）。所以同一個 chunk 的所有副本以同樣順序套用同樣的寫入。它不需要共識，是因為「誰是 head、鏈怎麼排」由外部（mgmtd）決定，而不是副本之間協商。
- **RoutingInfo 的散播不是全序廣播**：它是拉取式、只取最新一份，中間版本可能被跳過（`src/client/mgmtd/MgmtdClient.cc:601-620`）。節點只保證版本不倒退。

---

### 4.5 分散式交易與 Two-Phase Commit（2PC）

**概念**（對應 DDIA 第 9 章〈Atomic Commit and Two-Phase Commit (2PC)〉）
一個交易跨多個節點時，要讓所有節點都 commit 或都 abort。2PC 由協調者先問所有參與者「能不能 commit」（prepare），全部同意後再通知 commit。它的弱點是協調者在 prepare 之後掛掉，參與者只能卡著等（in-doubt），手上的鎖也放不掉。

**3FS 怎麼做：不用 2PC**
- **metadata 不需要跨節點交易協調**：每個 meta 操作是一個 FDB 交易，跨 storage server 的原子提交是 FDB 內部的事（FDB 語意）。3FS 程式碼只看到「一個交易 commit 成功或失敗」。
- **metadata 與 data 之間不用 2PC**，用「操作順序＋可重做」取代（§2.2）：
  - 刪檔：先刪 chunk（可重做），再刪 inode：`src/meta/components/GcManager.cc:413-446`。
  - 檔長：寫入時不同步更新 inode，close/fsync 時查 storage 補正：`docs/design_notes.md:91-93`。
- **CRAQ 的 pending → commit 看起來像兩階段，但不是 2PC**：
  - 沒有「投票」：下游不能說「我不同意」讓整筆 abort；下游失敗時上游是重試（`forwardWithRetry`），或等 mgmtd 把它踢出鏈。
  - 失敗時傾向「往前推完」而不是回滾：重試會撿回原本的 `updateVer`（`src/storage/service/ReliableUpdate.cc:105-109`），已在下游 commit 的版本被當作重複成功（`src/storage/service/StorageOperator.cc:420-426`）。
  - chunk engine 上，失敗的 pending 寫入在 `WritingChunk` 被 drop 時標為 abort，下次寫入可以覆蓋它：`src/storage/chunk_engine/src/alloc/writing_chunk.rs:35-44`、`src/storage/chunk_engine/src/core/engine.rs:443-451`。
  - 協調者（head）掛掉不會讓參與者卡住：mgmtd 會重排鏈，新 head 接手，client 重試。

---

### 4.6 共識（consensus）

**概念**（對應 DDIA 第 9 章〈Fault-Tolerant Consensus〉）
讓多個節點在部分節點故障時仍能對某個值達成一致（例如誰是 leader、下一筆操作是什麼）。Paxos、Raft、Zab、Viewstamped Replication 都是這類演算法，核心是靠多數決與單調遞增的 epoch/term 號碼，在任何時候最多只有一個被多數承認的 leader。

**3FS 怎麼做：完全委託給 FoundationDB**
- repo 裡沒有 Paxos/Raft 實作（`src/` 下搜尋 raft、paxos 無結果；`third_party/` 也沒有 FDB 原始碼，只透過 `fdb_c` 連到外部 FDB 叢集：`src/fdb/FDBConfig.h:10`、`src/fdb/FDBContext.cc:93`）。
- 需要共識的事情，3FS 都改寫成「一個 FDB 交易」：
  - **選 mgmtd primary** = 在 FDB 交易裡 CAS 一個 lease key（`src/mgmtd/store/MgmtdStore.cc:154-189`）。
  - **chain 成員變更** = mgmtd primary 一個人決定（`generateNewChain`），先寫進 FDB 再更新記憶體：`src/mgmtd/background/MgmtdChainsUpdater.cc:41-52`。storage 節點之間不協商。
  - **meta 的 inode owner 對照表** = 寫在 FDB 的一個 key，任何 meta 都可以在交易裡更新：`src/meta/components/Distributor.cc:218-319`。
- **epoch 號碼的對應**：Raft 的 term、Paxos 的 ballot number 在 3FS 裡對應的是 `chainVersion`（資料路徑上用來排除舊 leader）和 FDB 裡的 lease 記錄（控制面）。

**取捨（推測）**：把共識外包讓 3FS 本身的程式碼簡單很多，所有困難的正確性證明都落在 FDB 身上。代價是 FDB 成為整個系統的可用性下限（§4.2）。

---

### 4.7 成員管理與協調服務

**概念**（對應 DDIA 第 9 章〈Membership and Coordination Services〉）
ZooKeeper、etcd 這類服務把共識包裝成好用的原語：線性一致的小型 KV、原子操作、帶 lease 的臨時節點、變更通知。一般應用拿它來做 leader 選舉、服務發現、成員管理、分區指派，而不必自己實作共識。

**3FS 怎麼做：FDB 當 ZooKeeper，mgmtd 當「成員管理服務」**

| ZooKeeper 類原語 | 3FS 的對應 | 位置 |
|---|---|---|
| leader 選舉 | mgmtd 在 FDB 上的 lease | `src/mgmtd/store/MgmtdStore.cc:154-204` |
| 臨時節點／存活偵測 | 心跳＋mgmtd 本機計時 | `src/mgmtd/background/MgmtdHeartbeatChecker.cc:19-78` |
| 服務發現 | RoutingInfo 裡的 node 列表，client 拉取 | `src/client/mgmtd/MgmtdClient.cc:601-658` |
| 變更通知（watch） | 沒有 push；改成定期拉取＋版本號，storage 端在路由更新時觸發 listener | `src/client/mgmtd/MgmtdClient.cc:652-657`、`src/storage/service/Components.cc:129-130` |
| 分區指派 | mgmtd 的 chain table／chain；meta 的 Distributor rendezvous hash | `src/mgmtd/service/updateChain.cc:25-104`、`src/meta/components/Distributor.cc:117-118` |
| client session | mgmtd 記憶體內的 client session（每 10s 續、20 分鐘逾時），**不持久化**，新 primary 上任時清空 | `src/mgmtd/background/MgmtdClientSessionsChecker.cc:12-35`、`src/mgmtd/service/MgmtdConfig.h:20`、`src/mgmtd/background/MgmtdLeaseExtender.cc:181-182` |

**一個值得注意的設計：bootstrapping 期間「不知道」不等於「不存在」**
client session 只存在 mgmtd 記憶體，primary 換人後新 primary 一開始的 session 表是空的。如果 meta 這時去問「哪些 client 還活著」，會以為大家都死了，進而清掉所有人的檔案 session。3FS 的處理：
- mgmtd 回應時附上 `bootstrapping` 旗標（上任後 2 分鐘內為真）：`src/mgmtd/ops/ListClientSessionsOperation.cc:15`、`src/mgmtd/service/MgmtdConfig.h:21`。
- meta 看到 `bootstrapping` 就不做判定：`src/meta/components/SessionManager.cc:51-54`；storage 清理過期 client 時也一樣：`src/storage/service/Components.cc:223-227`。

這是「新 leader 的知識不完整，要等一段時間收集證據」的具體實作，對應 §3.3 新 primary 重置心跳計時的同一個思路。

---

## 5. 總表：DDIA 概念 → 3FS 實作位置 → 3FS 的取捨

| DDIA 概念 | 3FS 實作位置 | 3FS 的取捨 |
|---|---|---|
| 單主複製 | CRAQ head：`src/storage/service/StorageOperator.cc:338-341`、`src/storage/service/TargetMap.cc:190-193` | 寫入單一入口、讀取分散到所有副本 |
| 同步複製 | `src/storage/service/StorageOperator.cc:443-508` | 全員同步，ack 即全員 commit；慢副本拖住寫入 |
| 新 follower | `src/storage/sync/ResyncWorker.cc:101-387`、`src/mgmtd/service/updateChain.cc:70-88` | 狀態比對、整塊覆蓋；每鏈一次只補一個 |
| leader failover | `src/mgmtd/service/updateChain.cc:25-104`、`src/mgmtd/background/MgmtdHeartbeatChecker.cc:19-78` | 不選舉，mgmtd 重排；全滅時等 LASTSRV（犧牲可用性） |
| 複製日誌 | 無；`updateVer` 檢查 `src/storage/store/ChunkReplica.cc:216-239` | 不保存日誌，掉隊靠 resync |
| read-your-writes | 同步複製的副作用 | 寫入 ack 後成立 |
| monotonic reads | 無專門機制 | chunk engine 模式下讀寫並行時可能不成立（推論） |
| multi-leader / leaderless | 未涉及 | 刻意選 write-all/read-any |
| ACID / 多物件交易 | FDB 交易：`src/meta/store/Operation.h:156-250` | metadata 有；data 只有單 chunk 原子 |
| read committed | `src/storage/store/ChunkReplica.cc:61-66`、`src/storage/chunk_engine/src/core/engine.rs:184-209` | 預設不讀 pending；可選 relaxed read |
| snapshot isolation | FDB read version；`src/common/kv/ITransaction.h:92-99` | 路徑解析一律 snapshot 讀以減少衝突 |
| lost update | `src/meta/store/ops/BatchOperation.cc:97-102`、`src/meta/service/MetaOperator.cc:321-337` | FDB 衝突偵測＋inode owner 集中處理 |
| write skew / phantom | `src/meta/store/Inode.cc:196-224`、`src/meta/store/ops/Mkdirs.cc:65-70`、`src/meta/store/DirEntry.cc:316-325` | 手動挑關鍵讀加入 conflict |
| serializable | FDB（樂觀）；storage chunk 鎖（串行） | 不用 2PL |
| 部分失效 / 系統模型 | `src/storage/service/StorageOperator.cc:605-611`、`src/storage/service/ReliableForwarding.cc:264-276` | fail-stop；主動把半壞轉成全壞 |
| 逾時與重試冪等 | `src/storage/service/ReliableUpdate.cc:16-127`、`src/meta/store/Idempotent.h`、`src/meta/store/ops/BatchOperation.cc:533-538` | 每層各自去重；meta 預設重試 maybe-committed |
| 故障偵測 | `src/mgmtd/background/MgmtdHeartbeatChecker.cc:41-52`、`src/meta/components/Distributor.cc:260-284` | 60s 保守門檻；只用本機單調時鐘判斷 |
| 不可靠時鐘 | 見 §3.4 表 | lease 與 session 過期依賴牆上時鐘 |
| process pause | `src/mgmtd/service/helpers.h:44-56`、`src/storage/service/StorageOperator.cc:382-387` | 寫入由 FDB 交易擋；記憶體內回應不擋 |
| 真相由權威決定 | `src/storage/service/TargetMap.cc:329-341`、`src/storage/service/Components.cc:87-122` | storage 服從 mgmtd，重啟先被判下線 |
| lease | `src/mgmtd/store/MgmtdStore.cc:154-204`、`src/mgmtd/service/MgmtdState.cc:66-75` | 60s 租期、20s 安全邊際 |
| fencing token | `src/storage/service/TargetMap.cc:55-65`、`src/storage/store/ChunkReplica.cc:186-191`、`src/storage/chunk_engine/src/core/engine.rs:351-357` | chainVersion 擋寫；讀取擋不完整（推論） |
| Byzantine | 只有 checksum：`src/storage/store/ChunkReplica.cc:193-207` | 不處理 |
| linearizability | metadata：FDB；data：CRAQ | metadata 有；data 依後端與時間窗而定 |
| CAP | `src/mgmtd/service/updateChain.cc:31-42` | 偏 CP |
| 序號 / 順序 | §4.3 表 | 全是單一發號者，沒有邏輯時鐘 |
| total order broadcast | FDB commit 順序；chain 內 `updateVer` | 不自己實作 |
| 2PC | 不使用；`src/meta/components/GcManager.cc:413-446` | 順序＋可重做取代 |
| consensus | FDB（外部） | 完全委託 |
| 協調服務 | FDB＋mgmtd | 拉取式路由；client session 不持久化，靠 bootstrapping 旗標補 |

---

## 6. 3FS 沒教、要另外學的東西

讀完 3FS 會懂「怎麼**使用**共識服務來建一個分散式系統」，但以下主題在 3FS 裡完全看不到，或只看到外殼：

1. **共識演算法本身（Raft / Paxos / Zab / VR）**：3FS 全部丟給 FDB。要懂 leader election、log replication、membership change（joint consensus）的細節，建議讀 Raft 論文並看 etcd/raft 或 TiKV 的 raft-rs 實作。
2. **FoundationDB 的內部**：樂觀並行控制（resolver、conflict range 的檢查方式）、Paxos 協調者、確定性模擬測試（simulation testing）。3FS 依賴的 serializable、`commit_unknown_result`、versionstamp、`\xff/metadataVersion` 的語意，都要去 FDB 文件和論文〈FoundationDB: A Distributed Unbundled Transactional Key Value Store〉查證。
3. **原版 CRAQ 與 chain replication 論文**：van Renesse & Schneider 的 chain replication、Terrace & Freedman 的 CRAQ。3FS 修改了 CRAQ 的讀取（不問 tail，`docs/design_notes.md:173`），要對照原版才知道放棄了什麼。
4. **Quorum／無主複製**：Dynamo 系（Cassandra、Riak）的 sloppy quorum、hinted handoff、read repair、Merkle tree anti-entropy。
5. **衝突解決與 CRDT**：multi-leader、離線優先系統需要的東西，3FS 沒有。
6. **跨機房／地理分散**：3FS 假設單一機房內的 RDMA 網路；沒有跨區複製、沒有處理高延遲 WAN 的設計。
7. **Byzantine 容錯**：PBFT、HotStuff 等，以及區塊鏈類系統。
8. **邏輯時鐘**：Lamport timestamp、vector clock、hybrid logical clock（HLC），以及 Spanner 的 TrueTime（用有界誤差的實體時鐘做 linearizable 交易）。3FS 只用單一發號者的版本號，用不到這些。
9. **分散式交易協定**：2PC 的完整實作與故障恢復、Percolator（TiDB）、Calvin（確定性排程）、Spanner 的 2PC over Paxos。
10. **形式化驗證**：TLA+、Jepsen 式的線性一致性測試。3FS 有 fault injection（例：`src/storage/service/StorageOperator.cc:103-106`），但 repo 裡沒有形式化規格。

---

## 7. 本文的推論與待查核清單

以下論述是我讀程式碼得出、但**沒有程式碼註解或測試直接證實**的，建議讀者自行查核：

1. **chunk engine 模式下的讀取語意**（§1.6、§4.1）：我依據 `engine.rs` 的 `get` 只讀 `meta_cache`、`commit_chunk` 才更新它，以及 `ChunkEngine.h` 把 `commitVer` 和 `updateVer` 設成同一個值，推論「讀寫並行時回最後 commit 版本、不回 `kChunkNotCommit`」，並進一步推論 monotonic reads／linearizability 在這個時間窗可能不成立。沒有實測。這與 `docs/design_notes.md:173` 的描述不同。
2. **`docs/design_notes.md:177` 說的「失聯 T/2 秒自行退出」**：我在 `src/` 下沒找到。可能在我沒搜尋到的地方（例如 FUSE 端、部署腳本、systemd 設定），也可能是未開源或未實作。
3. **舊 target 回舊資料的窗口**（§3.7 末段）：由「讀取只檢查 chainVer 相等」和「storage 只在收到新路由時才自我下線」推得，沒有實測。
4. **Distributor 讀 `\xff/metadataVersion` 能否形成 read conflict**（§3.7）：依賴 FDB 對這個特殊 key 的語意；我沒在 3FS 程式碼裡找到直接證明，只看到它在交易內用 `get` 讀取（`src/meta/components/Distributor.cc:121-135`）。
5. **lease 安全性依賴時鐘誤差小於 20s**（§3.4）：是由 `currentLease` 的比較式推出的；實際上還有 FDB 交易這層保護，兩者合起來的完整安全條件我沒有形式化推導。
6. **storage 寫入重試在 head 換人後被重做是安全的**（§3.2）：我只確認了去重快取是記憶體內的；「重做等價」對 truncate/extend/remove 是否完全成立沒有逐一驗證。
7. **client 遇到 `kRoutingVersionMismatch` 不會強制刷新路由**（§1.6）：只在 `StorageClientImpl.cc` 與 `src/client/` 搜尋 `refreshRoutingInfo` 確認，沒有排除其他間接觸發路徑。
8. **FDB 提供 strict serializability**：FDB 公開文件的說法，3FS 自己的文件寫的是 SSI（`docs/design_notes.md:63`），兩者在本文的論證裡沒有衝突，但 repo 內無法驗證。
9. **寫入卡住的時間量級**（§1.2、§3.3）：由各個逾時預設值推得，實際取決於部署 config 與這些計時器的相位。
