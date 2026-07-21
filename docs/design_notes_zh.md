# 設計筆記（Design Notes）

> 本文為 [design_notes.md](design_notes.md) 的繁體中文翻譯。Chain、Version、Target、chunk 等專有名詞保留原文。

## 設計與實作

3FS 系統由四個組件構成：cluster manager、metadata service、storage service 與 client。所有組件都連接在 RDMA 網路（InfiniBand 或 RoCE）中。

Metadata service 與 storage service 會向 cluster manager 發送心跳。Cluster manager 負責處理成員變更，並將叢集組態發布給其他服務與 client。系統會部署多個 cluster manager，並選出其中一個作為 primary；當 primary 失效時，另一個 manager 會被提升為 primary。叢集組態通常存放在可靠的分散式協調服務中，例如 ZooKeeper 或 etcd。在我們的生產環境中，為了減少依賴，我們使用與檔案 metadata 相同的 key-value store 來存放叢集組態。

檔案 metadata 操作（例如開啟或建立檔案／目錄）會被送往 metadata service，由它實作檔案系統語意。由於檔案 metadata 存放在支援交易的 key-value store（例如 FoundationDB）中，metadata service 本身是無狀態（stateless）的，client 可以連線到任意一個 metadata service。

每個 storage service 管理數顆本機 SSD，並提供 chunk store 介面。Storage service 實作了 Chain Replication with Apportioned Queries（CRAQ）以確保強一致性。CRAQ「write-all-read-any」（全寫任讀）的做法有助於釋放 SSD 與 RDMA 網路的吞吐量。一個 3FS 檔案會被切分成大小相等的 chunk，並複製到多顆 SSD 上。

我們為應用程式開發了兩種 client：FUSE client 與 native client。大多數應用程式使用門檻較低的 FUSE client；對效能敏感的應用程式則整合 native client。

## 檔案系統介面

Object store 正逐漸成為資料分析與機器學習領域的熱門選項。然而，檔案系統語意以及「檔案以目錄組織」的統一命名空間，能為應用程式提供更大的彈性。

-   *原子性的目錄操作*　Object store 可以在 object key 中使用斜線（/）來近似階層式目錄結構，但它並不原生支援「原子性地移動檔案／目錄」或「遞迴刪除整個目錄」這類操作。實際上，我們內部應用程式的一個常見模式是：先建立暫存目錄、把檔案寫進去，再將整個目錄移動到最終位置。在處理大量小檔案時，目錄的遞迴刪除也至關重要——少了它，應用程式就得自行走訪每個目錄、逐一刪除檔案。

-   *Symbolic link 與 hard link*　我們的應用程式利用 symbolic link 與 hard link 為動態更新的資料集建立輕量快照，新資料會以獨立檔案的形式附加進資料集。

-   *熟悉的介面*　檔案介面眾所周知、無處不在，不需要學習新的儲存 API。許多資料集以 CSV／Parquet 檔案存放；讓以檔案為基礎的 data loader 改用 3FS 的 FUSE client 或 native client 十分直接。

### FUSE 的限制

FUSE（Filesystem in Userspace）透過 FUSE kernel module 將 I/O 操作轉送到 user-space 行程，簡化了檔案系統 client 的開發。它讓應用程式彷彿在存取本機檔案系統一樣存取遠端檔案系統。然而，它存在效能上的限制：

-   *記憶體拷貝開銷*　User-space 的檔案系統 daemon 無法直接存取應用程式的記憶體。資料在 kernel space 與 user space 之間搬運會消耗記憶體頻寬，並增加端到端延遲。

-   *原始的多執行緒支援*　當應用程式發起 I/O 請求時，FUSE 會把這些請求放進一個由 spin lock 保護的多執行緒共享佇列，再由 user-space 的檔案系統 daemon 從佇列中取出並處理。由於鎖競爭，FUSE 的 I/O 處理能力無法隨執行緒數擴展。我們的基準測試顯示，FUSE 每秒只能處理約 400K 次 4KiB 讀取；繼續提高並行度也無法提升效能，因為鎖競爭會隨之加劇。`perf` 剖析顯示 kernel-space 的 spin lock 佔用了大量 CPU 時間。

大多數應用程式（例如資料分析）在 3FS 上進行大區塊寫入，或者先在記憶體中緩衝資料、等寫入緩衝區滿了再刷寫到 3FS。然而，Linux 5.x 上的 FUSE 不支援對同一檔案的並行寫入[^1]。應用程式的因應方式是同時寫入多個檔案，以最大化總吞吐量。

讀取操作的模式則更為複雜。某些訓練任務需要隨機存取資料集樣本，每個樣本的讀取大小從數 KB 到數 MB 不等，而且樣本在檔案中通常不是 4K 對齊的。Data loader 專門設計來批次抓取樣本，但在 FUSE 掛載的 3FS 上處理小型隨機讀取時表現不佳，SSD 與 RDMA 網路的頻寬無法被充分利用。

### 非同步零拷貝 API

把檔案系統 client 實作成 VFS kernel module 可以避免上述效能問題，但 kernel module 的開發難度遠高於 user-space 系統程式設計：bug 難以診斷，且可能在生產環境造成災難性故障——例如機器直接當機、連一行可供除錯的日誌都不留。升級 kernel module 時，所有使用該檔案系統的行程都必須乾淨地停止，否則就得重新開機。

基於這些理由，我們選擇在 FUSE daemon 內實作一個 native client，提供支援非同步零拷貝 I/O 的介面。檔案的 meta 操作（例如 open／close／stat）仍由 FUSE daemon 處理：應用程式呼叫 `open()` 取得 file descriptor（fd），透過 native API 註冊之後，就能用 native client 對該檔案執行 I/O。這個做法確保 metadata 操作與 POSIX API 保持一致，讓既有程式碼更容易遷移。

這套非同步零拷貝 API 的靈感來自 Linux 的 `io_uring`。以下是 API 中的關鍵資料結構：

-   *Iov*　一塊用於零拷貝讀寫的大型記憶體區域，由使用者行程與 native client 共享，其 InfiniBand memory registration 由 client 管理。在 native API 中，所有讀取的資料都會被讀進 Iov；所有要寫入的資料則必須先寫入 Iov 再呼叫 API。

-   *Ior*　一個用於使用者行程與 native client 之間通訊的小型共享 ring buffer。Ior 的用法類似 Linux 的 `io_uring`：使用者行程將讀寫請求入列，native client 取出這些請求並完成之。請求以批次執行，批次大小由 `io_depth` 參數控制；多個批次會平行處理，無論它們來自不同的 ring 還是同一個 ring。不過對多執行緒應用程式而言，仍建議使用多個 ring——共享同一個 ring 需要同步，可能影響效能。

Native client 內部會啟動多個執行緒從各個 Ior 抓取 I/O 請求。這些請求會被批次化後分發到 storage service，減少小型讀取請求造成的 RPC 開銷。

## 檔案 metadata 儲存

### 檔案 chunk 的位置

3FS 將檔案資料切分為大小相等的 chunk，並將它們條帶化（stripe）分佈到多條 replication chain 上（replication chain 與 chain table 的定義見下文「資料放置（Data placement）」一節）。使用者可以按目錄為單位，為檔案指定 chain table、chunk size 與 stripe size。每個 chunk 獨立存放在多個 storage service 上，其 chunk ID 由檔案的 inode id 與 chunk index 串接而成。

建立新檔案時，metadata service 依 stripe size 以 round-robin 策略從指定的 chain table 中選取連續的 replication chain，接著產生一個隨機 seed 將選中的 chain 洗牌。這個配置策略確保資料能均衡分佈在各條 chain 與各顆 SSD 上。

應用程式開啟檔案時，client 會向 meta service 取得該檔案的資料 layout 資訊。之後 client 便能自行計算資料操作所需的 chunk ID 與 chain，將 meta service 在關鍵路徑上的參與降到最低。

### 存放於交易式 key-value store 的檔案 metadata

3FS 使用 FoundationDB 作為 metadata 的分散式儲存系統。FoundationDB 提供 key-value store 介面，並支援具備 Serializable Snapshot Isolation（SSI）的交易。3FS 將所有 metadata 以 key-value 對的形式存放在 FoundationDB 中。Meta service 採無狀態架構，讓管理者可以無中斷地平滑升級或重啟服務，大幅提升可維護性；client 遇到請求失敗或逾時，可自動 failover 到其他可用的服務。

檔案系統 metadata 主要由兩個核心結構組成：inode 與 directory entry。Inode 存放檔案、目錄與 symbolic link 的屬性資訊，每個 inode 由一個全域唯一、單調遞增的 64-bit 識別碼標識。Inode 的 key 由 "INOD" 前綴串接 inode id 構成；inode id 以 little-endian 位元組序編碼，使 inode 分散到多個 FoundationDB 節點上。Inode 的 value 依其類型而異：

-   所有 inode 類型都包含基本屬性：擁有者、權限、存取／修改／變更時間。

-   檔案 inode 的額外屬性：檔案長度、chunk size、在 chain table 中選定的範圍、shuffle seed。

-   目錄 inode 的額外屬性：父目錄的 inode id、子目錄／檔案的預設 layout 組態（chain table、chunk size、stripe size）。父目錄的 inode id 用於在移動目錄時偵測迴圈：把 `dir_a/dir_b` 移到 `dir_c/` 時，必須確保 `dir_c` 不是 `dir_b` 的子孫，這可以透過向上檢查 `dir_c` 的所有祖先來達成。

-   Symbolic link inode 的額外屬性：目標路徑字串。

Directory entry 的 key 由 "DENT" 前綴、父 inode id 與項目名稱組成；value 存放目標 inode id 與 inode 類型。同一目錄下的所有項目自然形成連續的 key 區間，因此可以透過 range 查詢高效列出目錄內容。

Meta 操作善用 FoundationDB 的交易機制：

-   唯讀交易用於 metadata 查詢：fstat、lookup、listdir 等。

-   讀寫交易用於 metadata 更新：create、link、unlink、rename 等。

對於寫入交易，FoundationDB 會追蹤讀寫的 key 集合以構成衝突偵測集合。當偵測到並行交易衝突時，meta service 會自動重試該交易。這個設計讓多個 meta service 能平行處理請求，同時維持檔案系統 metadata 的一致性。

### 動態檔案屬性

在大多數本機檔案系統上，刪除一個已開啟的檔案會被推遲到所有相關 file descriptor 關閉之後，因此必須追蹤該檔案的所有 file descriptor。訓練任務在啟動期間會開啟大量檔案，若要儲存所有 file descriptor，將對 meta service 與 FoundationDB 造成沉重負載。由於訓練任務並不依賴這項特性，3FS 不追蹤以唯讀模式開啟的 file descriptor。

對每個以寫入模式開啟的 file descriptor（fd），3FS 會維護一個 file session——因為刪除已被寫入模式開啟的檔案，可能因並行寫入產生無法回收的垃圾 chunk。當一個仍有作用中寫入 session 的檔案被刪除時，meta service 會延遲刪除，直到其所有 fd 都關閉為止。為了避免離線 client 留下殘存的 session，3FS 的 meta service 會定期檢查 client 存活狀態，並清理離線 client 的 session。

檔案長度存放在 inode 中。對於正在被更新的檔案，inode 中的長度可能與實際長度不一致。Client 會定期（預設 5 秒）向 meta service 回報每個以寫入模式開啟的檔案的最大寫入位置；若該位置超過 inode 中的長度，且沒有並行的 truncate 操作，該位置就會被採納為新的檔案長度。

由於可能有多個 client 並行寫入，上述方法只能確保檔案長度的最終一致性。處理 close／fsync 操作時，meta service 會向 storage service 查詢最後一個 chunk 的 ID 與長度，以取得精確的檔案長度。由於檔案資料條帶化分佈在多條 chain 上，這個操作的開銷不可忽視。

多個 meta service 並行更新同一檔案的長度可能造成交易衝突，導致檔案長度被重複計算。為了緩解這個問題，meta service 以 inode id 搭配 rendezvous hash 演算法，把檔案長度更新任務分派到多個 meta service 上。

我們的生產環境使用很大的 stripe size：200。對小檔案而言，實際含有其 chunk 的 chain 數遠低於這個數字。檔案 inode 中會存放「可能已使用的 chain 數」，作為更新長度時的 hint：初始值為 16，每當檔案 chunk 被寫入更多 chain 時就加倍。這讓我們在更新小檔案長度時，不必查詢全部 200 條 chain。這項優化也可以延伸應用到小檔案的刪除。

## Chunk 儲存系統

Chunk 儲存系統的設計目標是：即使發生儲存媒體故障，仍能達到盡可能高的頻寬。3FS 的讀寫吞吐量應隨 SSD 數量以及 client 與 storage service 之間的 bisection 網路頻寬線性擴展。應用程式以不關心資料局部性（locality-oblivious）的方式存取 storage service。

### 資料放置（Data placement）

每個檔案 chunk 都透過 chain replication with apportioned queries（CRAQ）複製到一條由多個 storage target 組成的 chain 上。在 CRAQ 中，寫入請求送往 head target 並沿著 chain 傳播；讀取請求則可以送往 chain 上的任一 storage target。通常讀取流量會均勻分散到 chain 上的所有 target，以取得較好的負載平衡。每顆 SSD 上會建立多個 storage target，分別加入不同的 chain。

假設有 6 個節點：A、B、C、D、E、F，每個節點有 1 顆 SSD。在每顆 SSD 上建立 5 個 storage target：1、2、…、5，總共 30 個 target：A1、A2、A3、…、F5。若每個 chunk 有 3 個副本，可以建構如下的 chain table。

| Chain | Version | Target 1 (head) | Target 2 | Target 3 (tail) |
| :---: | :-----: | :-------------: | :------: | :-------------: |
|   1   |    1    |      `A1`       |   `B1`   |      `C1`       |
|   2   |    1    |      `D1`       |   `E1`   |      `F1`       |
|   3   |    1    |      `A2`       |   `B2`   |      `C2`       |
|   4   |    1    |      `D2`       |   `E2`   |      `F2`       |
|   5   |    1    |      `A3`       |   `B3`   |      `C3`       |
|   6   |    1    |      `D3`       |   `E3`   |      `F3`       |
|   7   |    1    |      `A4`       |   `B4`   |      `C4`       |
|   8   |    1    |      `D4`       |   `E4`   |      `F4`       |
|   9   |    1    |      `A5`       |   `B5`   |      `C5`       |
|  10   |    1    |      `D5`       |   `E5`   |      `F5`       |

每條 chain 都有一個版本號。當 chain 發生變更（例如某個 storage target 離線）時，版本號會遞增。只有 primary cluster manager 能修改 chain table。

可以建構多張 chain table 來支援不同的資料放置需求。例如建立兩張 chain table，一張給批次／離線任務、另一張給線上服務，兩張表由互斥的節點與 SSD 上的 storage target 組成。

邏輯上，每條 chain 的狀態獨立變化，且一條 chain 可以被納入多張 chain table。Chain table 這個概念的用意，是讓 metadata service 能為每個檔案挑選一張表，並把檔案的 chunk 條帶化分佈到表內的各條 chain 上。

### 復原期間的流量均衡

假設讀取流量均勻分散在上面那張 chain table 的所有 storage target 上。當 A 故障時，它的讀取請求會被導向 B 與 C。在高負載下，B、C 的讀取頻寬會立刻飽和，B、C 成為整個系統的瓶頸。更換故障 SSD 並將資料同步到新 SSD 可能需要數小時，這段期間讀取吞吐量都會受損。

為了降低效能影響，可以讓更多 SSD 分攤被導走的流量。在下面這張 chain table 中，A 與其他每一顆 SSD 都有配對。當 A 故障時，其他每顆 SSD 只需承接 A 讀取流量的 1/5。

| Chain | Version | Target 1 (head) | Target 2 | Target 3 (tail) |
| :---: | :-----: | :-------------: | :------: | :-------------: |
|   1   |    1    |      `B1`       |   `E1`   |      `F1`       |
|   2   |    1    |      `A1`       |   `B2`   |      `D1`       |
|   3   |    1    |      `A2`       |   `D2`   |      `F2`       |
|   4   |    1    |      `C1`       |   `D3`   |      `E2`       |
|   5   |    1    |      `A3`       |   `C2`   |      `F3`       |
|   6   |    1    |      `A4`       |   `B3`   |      `E3`       |
|   7   |    1    |      `B4`       |   `C3`   |      `F4`       |
|   8   |    1    |      `B5`       |   `C4`   |      `E4`       |
|   9   |    1    |      `A5`       |   `C5`   |      `D4`       |
|  10   |    1    |      `D5`       |   `E5`   |      `F5`       |

為了在復原期間達到最大讀取吞吐量，這個負載平衡問題可以表述為 balanced incomplete block design（均衡不完全區組設計），並用整數規劃求解器取得最佳解。

### 資料複製

CRAQ 是一種為讀取密集型工作負載最佳化的 write-all-read-any（全寫任讀）複製協定。在全快閃儲存系統中，善用所有副本的讀取頻寬是達到最高讀取吞吐量的關鍵。

當 storage service 收到寫入請求時，會經過以下步驟：

1.  Service 檢查寫入請求中的 chain version 是否與已知的最新版本相符；不符則拒絕該請求。寫入請求可能來自 client，也可能來自 chain 上的前驅（predecessor）。

2.  Service 發起 RDMA Read 操作拉取寫入資料。若 client／前驅故障，RDMA Read 可能逾時，寫入隨之中止。

3.  寫入資料抓進本機記憶體緩衝區後，service 向 lock manager 取得待更新 chunk 的鎖。對同一 chunk 的並行寫入會被阻擋；所有寫入都在 head target 上被序列化。

4.  Service 將 chunk 的 committed version 讀進記憶體、套用更新，並把更新後的 chunk 存為 pending version。一個 storage target 可能同時保有一個 chunk 的兩個版本：committed version 與 pending version。每個版本都有單調遞增的版本號：committed version 與 pending version 的版本號分別為 `v` 與 `u`，且滿足 `u = v + 1`。

5.  若該 service 是 tail，committed version 會被原子性地替換為 pending version，並向前驅送出確認（acknowledgment）訊息；否則，寫入請求會被轉發給後繼（successor）。committed version 更新時，當前的 chain version 會作為一個欄位記錄在 chunk metadata 中。

6.  當確認訊息到達某個 storage service 時，該 service 將 committed version 替換為 pending version，並繼續向它的前驅傳播確認訊息，隨後釋放本機的 chunk 鎖。

假設 chain 上有 3 個 target：`A、B、C`。某個寫入請求剛在 `A` 進入步驟 5，`A` 把請求轉發給後繼 `B`；此時 `B` 瞬間故障，被轉發的寫入請求遺失。當 cluster manager 偵測到 `B` 故障，會把 `B` 標記為 offline、移到 chain 尾端，並廣播更新後的 chain table。`A` 收到最新 chain table 後，把寫入請求轉發給新的後繼 `C`。`C` 可能還沒收到最新的 chain table 而拒絕該請求，但 `A` 會持續向 `C` 轉發；最終 `C` 取得最新的 chain table 並接受該請求。

當讀取請求到達 storage service 時：

1.  若 service 只有該 chunk 的 committed version，就把這個版本回傳給 client。

2.  與 CRAQ 原始設計不同，我們的實作不會向 tail target 發出版本查詢。當 committed 與 pending 版本同時存在時，service 會回覆一個特殊狀態碼通知 client。Client 可以稍等片刻後重試，也可以發出 relaxed read 請求直接取得 pending version。

### 故障偵測

Cluster manager 依靠心跳偵測 fail-stop 型故障。若在可設定的時間區間（例如 T 秒）內沒有收到某個服務的心跳，cluster manager 就宣告該服務故障；而服務若有 T/2 秒無法與 cluster manager 通訊，就會停止處理請求並退出。心跳可以視為一種向 manager「續約 lease」的請求。

Metadata service 是無狀態的。Cluster manager 提供的線上 meta service 清單是一種簡單的服務發現機制，協助 client 與 metadata service 建立連線。若某個 meta service 掛掉，client 可以切換到任何其他 metadata service。

在 storage service 的成員變更上，cluster manager 扮演更關鍵的角色：它維護 chain table 與 storage target 狀態的全域視圖。每個 storage target 都有一個 public state 與一個 local state。

Public state 表示該 target 是否已準備好服務讀取請求，以及寫入請求是否會傳播給它。Public state 與 chain table 一起儲存，並發布給各服務與 client。

| Public State | Read | Write | 說明                                    |
| :----------- | :--: | :---: | :-------------------------------------- |
| serving      |  Y   |   Y   | 服務存活，正在服務 client 請求          |
| syncing      |  N   |   Y   | 服務存活，資料復原進行中                |
| waiting      |  N   |   N   | 服務存活，資料復原尚未開始              |
| lastsrv      |  N   |   N   | 服務已下線，且它是最後一個 serving 的 target |
| offline      |  N   |   N   | 服務已下線，或儲存媒體故障              |

Local state 只有 storage service 與 cluster manager 知道，並存放在 cluster manager 的記憶體中。若某個 storage target 發生媒體故障，所屬 service 會在心跳中把該 target 的 local state 設為 offline；若整個 storage service 掛掉，它管理的所有 storage target 都會被標記為 offline。

| Local State | 說明                                          |
| :---------- | :-------------------------------------------- |
| up-to-date  | 服務存活，正在服務 client 請求                |
| online      | 服務存活，target 處於 syncing 或 waiting 狀態 |
| offline     | 服務已下線，或儲存媒體故障                    |

Storage target 的 public state 會依最新的 local state 從一個狀態轉換到另一個狀態，local state 扮演觸發事件的角色。Cluster manager 定期掃描每條 chain，依照狀態轉換表更新 chain 上各 target 的 public state。

-   chain 有更新時，chain version 會遞增。

-   若某個 storage target 被標記為 offline，它會被移到 chain 的尾端。

-   若 storage service 發現自己任一本機 storage target 的 public state 是 lastsrv 或 offline，它會立即退出——該服務可能因網路分割（network partition）而與 cluster manager 隔離。

-   當處於 syncing 狀態的 storage target 完成資料復原後，storage service 會在後續發給 cluster manager 的心跳訊息中，把該 target 的 local state 設為 up-to-date。

| Local State | 當前 Public State | 前驅的 Public State | 下一個 Public State |
| :---------- | :---------------- | :------------------ | :------------------ |
| up-to-date  | serving           | （任意）            | serving             |
|             | syncing           | （任意）            | serving             |
|             | waiting           | （任意）            | waiting             |
|             | lastsrv           | （任意）            | serving             |
|             | offline           | （任意）            | waiting             |
| online      | serving           | （任意）            | serving             |
|             | syncing           | serving             | syncing             |
|             |                   | 非 serving          | waiting             |
|             | waiting           | serving             | syncing             |
|             |                   | 非 serving          | waiting             |
|             | lastsrv           | （任意）            | serving             |
|             | offline           | （任意）            | waiting             |
| offline     | serving           | 沒有前驅            | lastsrv             |
|             |                   | 有前驅              | offline             |
|             | syncing           | （任意）            | offline             |
|             | waiting           | （任意）            | offline             |
|             | lastsrv           | （任意）            | lastsrv             |
|             | offline           | （任意）            | offline             |

### 資料復原

當 storage service 退出（例如行程崩潰、或升級時重啟），或發生儲存媒體故障時，所有相關的 storage target 都會被 cluster manager 標記為 offline 並移到各 chain 的尾端。服務重啟後，該服務上的每個 target 各自獨立進入復原流程。整個復原流程與正常活動重疊進行，把中斷降到最低。

當一個先前 offline 的 storage service 啟動時：

1.  該 service 會定期從 cluster manager 拉取最新的 chain table，但在它所有的 storage target 都已在最新的 chain table 中被標記為 offline 之前，不會發送心跳。這確保它的所有 target 都會走過資料復原流程。

2.  復原期間到達的寫入請求一律是 full-chunk-replace 寫入：本機的 committed version 被更新，任何既有的 pending version 被拋棄。由於當前 service 是 tail，它會向前驅送出確認訊息。前驅的完整狀態就透過連續不斷的 full-chunk-replace 寫入流複製到歸隊的 service 上。

3.  在某個 storage target 的資料復原開始之前，前驅會向歸隊的 service 送出 dump-chunkmeta 請求。該 service 於是走訪本機的 chunk metadata store，收集該 target 上所有 chunk 的 id、chain version 以及 committed／pending 版本號，並將收集到的 metadata 回覆給前驅。

4.  當 sync-done 訊息到達時，service 便知道該 storage target 已是最新狀態，於是在發給 cluster manager 的心跳訊息中把該 target 的 local state 設為 up-to-date。

當 storage service 發現一個先前 offline 的後繼已經上線時：

1. 該 service 開始向後繼轉發正常的寫入請求。Client 可能只更新 chunk 的一部分，但轉發的寫入請求必須包含整個 chunk，即 full-chunk-replace 寫入。

2. 該 service 向後繼送出 dump-chunkmeta 請求。收到後繼 target 上所有 chunk 的 metadata 後，它收集本機 target 上的 chunk metadata，然後比對兩份 chunk metadata，決定哪些 chunk 需要傳輸。

3. 被選中的 chunk 以 full-chunk-replace 寫入請求傳輸給後繼。

   -   先為每個 chunk 取得 chunk 鎖。

   -   讀出 chain version、committed 版本號與 chunk 內容，以 full-chunk-replace 請求傳送給後繼。

   -   釋放 chunk 鎖。

4. 當所有需要的 chunk 都已傳輸完成，向後繼送出 sync-done 訊息。

決定哪些 chunk 需要傳輸的規則如下：

-   若 chunk 只存在於本機 target，應當傳輸。

-   若 chunk 只存在於遠端 target，應當刪除。

-   若本機 chunk 副本的 chain version 大於遠端 chunk 副本的 chain version，應當傳輸。

-   若本機／遠端 chunk 副本的 chain version 相同，但本機 committed 版本號不等於遠端 pending 版本號，應當傳輸。

-   否則，兩個 chunk 副本要麼完全相同，要麼正被進行中的寫入請求更新。

### Chunk 與其 metadata

檔案 chunk 存放在 chunk engine 中。在每顆 SSD 上，chunk engine 的持久化儲存由固定數量的資料檔（存放 chunk 資料）與一個 RocksDB 實例（維護 chunk metadata 及其他系統資訊）組成。此外，chunk engine 在記憶體中維護 chunk metadata 的快取以提升查詢效能，並實作了 chunk allocator 以快速配置新 chunk。Chunk engine 介面透過以下操作提供執行緒安全的存取：

1.  *open/close*　初始化 engine：從 RocksDB 載入 metadata 並重建 chunk allocator 的狀態。

2.  *get*　透過 hashmap 快取取得 chunk metadata 與帶引用計數的 handle，支援平均複雜度 O(1) 的並行存取。

3.  *update*　實作 copy-on-write（COW）語意：先配置新 chunk，再修改資料。舊 chunk 在所有 handle 釋放之前仍可讀取。

4.  *commit*　將更新後的 chunk metadata 以 write batch 提交到 RocksDB 以確保原子更新，並同步刷新 chunk metadata 快取。

Chunk 資料最終存放在實體區塊（physical block）上。實體區塊的大小從 64KiB 到 64MiB、以 2 的冪遞增，共 11 種。Allocator 會分配大小與實際 chunk 大小最接近的實體區塊。每種實體區塊大小各自建構一個資源池，每個池包含 256 個實體檔案。實體區塊的使用狀態以 bitmap 維護在記憶體中：當實體區塊被回收時，其 bitmap 旗標被設為 0，區塊的實際儲存空間仍被保留，並會在後續配置時被優先使用。當沒有可用的實體區塊時，會用 `fallocate()` 在實體檔案中配置一段連續的大空間，一次建立 256 個新實體區塊——這個做法有助於減少磁碟碎片化。

對 chunk 執行寫入操作時，allocator 會先分配一個新的實體區塊，接著系統把既有 chunk 資料讀進緩衝區、套用更新，再把更新後的緩衝區寫入新分配的區塊。針對 append 有一條最佳化路徑：資料直接就地附加在既有區塊的尾端。之後，系統會由新區塊的位置與既有 chunk metadata 建構出一份新的 metadata，並將新的 chunk metadata 與新舊實體區塊的狀態一併在 RocksDB 中原子性地更新。

[^1]: https://elixir.bootlin.com/linux/v5.4.284/source/fs/fuse/file.c#L1573
