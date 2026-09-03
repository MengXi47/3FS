# chunk-engine

### 設計

1. 整個 Chunk Engine 可以拆成兩個元件：
   1. **Allocator**：負責配置／回收 chunk，以及修改記憶體中的狀態。
   2. **MetaStore**：負責把配置／回收事件持久化。
2. 寫入一個新 chunk 的流程：
   1. **Allocator** 指派一個新的 chunk position，指向磁碟上的某塊空間（純記憶體操作）。
   2. 把資料寫到這個 chunk position。若此階段發生斷電或寫入失敗，不會影響任何既有資料。
   3. 產生對應的 chunk metadata，連同配置事件一起持久化到 **MetaStore**。利用 RocksDB 的 WriteBatch 保證更新是**原子的**——整個寫入操作要嘛全成功、要嘛全失敗，不存在中間狀態。
3. 維護 Allocator 的記憶體狀態：
   1. 啟動時，Allocator 會**快速地**從 RocksDB 載入所有配置資訊。
   2. 配置先在記憶體中完成，之後才持久化。若在持久化之前失敗，該配置事件就遺失。
   3. 回收則是先把事件持久化到磁碟，再修改記憶體狀態。即使 chunk 刪除事件已經持久化，只要記憶體中還持有它的參照，這個 chunk 仍然可讀。
   4. 這保證了讀寫操作互不衝突：讀取操作會取得一份 chunk 參照，確保該 chunk 在讀取完成之前都是有效的。
4. 用 `Arc` 管理 chunk position 的所有權：
   1. 配置時回傳 `Arc<ChunkPos>`。若持久化失敗，`Arc` 被 drop 時該 position 會自動釋放。
   2. 讀取操作同樣回傳 `Arc<ChunkPos>`，即使同時有其他人在寫入或刪除，也能安全地存取資料。

### Allocator

儲存層級：

1. **Chunk**：基本資料單位，目前建議為 64KB、512KB、4MB。
2. **Group**：每個 group 含 256 個 chunk（依 chunk 大小分別為 16MB、128MB、1GB）。
3. **File**：以 512KB chunk 為例，單一檔案（約 120GB）含約 960 個 group。
4. **Disk**：單顆磁碟容量 30TB，每種 chunk 大小切成 256 個檔案。
5. **Node**：單一節點含 10–20 顆磁碟。

這個配置在單機上可支撐約 12 億個 chunk、約 500 萬個 group。

實作細節：
1. 每個 group 用一個 256-bit bitset（4 個 `uint64_t`）追蹤配置狀態。
2. 記憶體中維護三個結構：
   - `allocated_groups`：已配置空間但尚未指派 chunk 的 group。
   - `unallocated_groups`：尚未配置空間的 group。
   - `active_groups`：`<group_id, group_state>` 的 map，追蹤配置狀態。
3. Chunk 配置流程：
   1. 優先在 `active_groups` 中尋找空位，用 **`__builtin_ctz`** 做快速位元運算。
   2. 若 `active_groups` 為空，從 `allocated_groups` 取一個新的 group。
   3. 若 `allocated_groups` 也為空，就從 `unallocated_groups` 取一個 group，並同步配置磁碟空間。
4. 背景執行緒：
   - **`allocate_thread`**：把 `active_groups` 維持在目標大小區間內，確保記憶體配置的效率。
   - **`compact_thread`**：定期掃描 `active_groups`，把選中 group 裡的所有 chunk 搬遷出去、釋放空間，並把 group 交還給 `allocated_groups`。

### MetaStore

持久化三組映射：
1. **`chunk_id -> chunk_meta`**：metadata 含 chunk 位置、長度、hash、版本等，以 **`derse`** 序列化。
2. **`group_id -> group_state`**：追蹤 group 內各 chunk 的配置狀態，借用 RocksDB 的 **MergeOp** 做原子更新。
3. **`chunk_pos -> chunk_id`**：把實體位置映射回 chunk ID，供 `compact_thread` 搬遷 chunk 時使用。

### Chunk Engine

1. **MetaCache**：在記憶體中維護 `chunk_id -> chunk_info` 映射，其中 `chunk_info` 含 `chunk_meta` 與 `Arc<ChunkPos>`。
2. **讀取操作**：回傳 `chunk_info`。`Arc<ChunkPos>` 保證讀取完成之前資料都能安全存取。
3. **寫入操作流程**：
   1. 查詢 **MetaCache** 取得當前的 `chunk_info`。
   2. 呼叫 `Allocator::allocate()` 取得一個新的 chunk position。
   3. 讀出既有的 chunk 資料、寫到新的 chunk position、附加上這次的寫入請求，並產生 `new_chunk_info`。
   4. 把 `new_chunk_info` 連同原 chunk position 的釋放記錄一起持久化到 **MetaStore**。
