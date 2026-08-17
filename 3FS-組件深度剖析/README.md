# 3FS 組件深度剖析

依 CMake `target_add_bin` / `target_add_shared_lib` 逐一盤點的**獨立產出物（binary / shared library）**，每個組件一份深度報告。報告要求：逐檔掃過原始碼、所有論述附 `檔案路徑:行號`、以純文字繪製架構圖、重設計取捨而非 API 目錄。

風格基準：[`../3FS-元資料層深度剖析.md`](../3FS-元資料層深度剖析.md)

> 📖 **想直接讀源碼？** 見 [源碼研讀順序.md](源碼研讀順序.md)——分階段的檔案清單、該跳過什麼、時間受限時的取捨。

---

## 組件盤點（來源：`grep -rn "target_add_bin\|target_add_shared_lib" --include=CMakeLists.txt`）

### 服務端 binary

| 產出物 | 原始碼 | CMake | 報告 |
|---|---|---|---|
| `mgmtd_main` | `src/mgmtd/mgmtd.cpp` | `src/mgmtd/CMakeLists.txt:2` | ✅ [報告](mgmtd_main-叢集管理服務深度剖析.md) |
| `meta_main` | `src/meta/meta.cpp` | `src/meta/CMakeLists.txt:2` | ✅ [報告](meta_main-元資料服務深度剖析.md) |
| `storage_main` | `src/storage/storage.cpp` | `src/storage/CMakeLists.txt:5` | ✅ [報告](storage_main-存儲服務深度剖析.md) |
| `monitor_collector_main` | `src/monitor_collector/monitor_collector.cpp` | `src/monitor_collector/CMakeLists.txt:2` | ✅ [報告](monitor_collector_main-監控收集器深度剖析.md) |

### 客戶端 / 工具 binary

| 產出物 | 原始碼 | CMake | 報告 |
|---|---|---|---|
| `hf3fs_fuse_main` | `src/fuse/hf3fs_fuse.cpp` | `src/fuse/CMakeLists.txt:8` | ✅ [報告](hf3fs_fuse_main-FUSE客戶端深度剖析.md) |
| `admin_cli` | `src/client/bin/admin_cli.cc` | `src/client/bin/CMakeLists.txt:1` | ✅ [報告](admin_cli-管理命令列工具深度剖析.md) |
| `hf3fs-admin` | `src/tools/admin.cc` | `src/tools/CMakeLists.txt:1` | ✅ 併入 [admin_cli 報告](admin_cli-管理命令列工具深度剖析.md) |
| `migration_main` | `src/migration/main.cpp` | `src/migration/CMakeLists.txt:2` | ✅ [報告](migration_main-資料遷移工具深度剖析.md) |
| `simple_example_main` | `src/simple_example/main.cpp` | `src/simple_example/CMakeLists.txt:2` | ✅ [報告](simple_example_main-服務框架最小範例深度剖析.md) |
| `storage_bench` | `benchmarks/storage_bench/StorageBench.cc` | `benchmarks/storage_bench/CMakeLists.txt:1` | ✅ [報告](storage_bench-存儲壓測工具深度剖析.md) |

### Shared library

| 產出物 | 原始碼 | CMake | 報告 |
|---|---|---|---|
| `hf3fs_api_shared`（USRBIO） | `src/lib/api/` | `src/lib/api/CMakeLists.txt:2` | ✅ [報告](hf3fs_api_shared-USRBIO旁路IO深度剖析.md) |
| `hf3fs_common_shared` | `src/common/` | `src/common/CMakeLists.txt:10` | ✅ [報告](hf3fs_common_shared-共用基礎設施深度剖析.md) |
| `jemalloc_wrapper` | `src/memory/jemalloc/` | `src/memory/jemalloc/CMakeLists.txt:1` | ✅ [報告](memory_allocator_wrappers-記憶體配置器包裝深度剖析.md) |
| `mimalloc_wrapper` | `src/memory/mimalloc/` | `src/memory/mimalloc/CMakeLists.txt:1` | ✅ [報告](memory_allocator_wrappers-記憶體配置器包裝深度剖析.md) |

### Python 交付物（來源：`setup.py`、`setup_hf3fs_utils.py`、`deploy/`）

CMake 之外的獨立交付物，初次盤點時遺漏，第三輪補上。

| 產出物 | 原始碼 | 打包定義 | 報告 |
|---|---|---|---|
| `hf3fs_utils`（`hf3fs_cli`） | `hf3fs_utils/` | `setup_hf3fs_utils.py` | ✅ [報告](hf3fs_utils-Python命令列工具深度剖析.md) |
| `hf3fs` / `hf3fs_fuse` | `hf3fs/`、`hf3fs_fuse/` | `setup.py` | ✅ [報告](hf3fs-Python客戶端套件深度剖析.md) |
| `data_placement`（chain table 求解器） | `deploy/data_placement/` | `requirements.txt` | ✅ [報告](data_placement-資料佈局求解器深度剖析.md) |

### 函式庫層與跨組件流程（第四輪補齊）

前三輪按 binary 盤點，遺漏了**客戶端函式庫層**（不是獨立產出物，但被所有客戶端連結）與**跨組件的端到端流程**。

| 主題 | 原始碼 | 報告 |
|---|---|---|
| 客戶端存儲函式庫 | `src/client/storage/` 13 檔 4432 行 | ✅ [報告](client_storage_lib-客戶端存儲函式庫深度剖析.md) |
| 客戶端元資料與管理函式庫 | `src/client/meta/`、`src/client/mgmtd/` 19 檔 3636 行 | ✅ [報告](client_meta_mgmtd_lib-客戶端元資料與管理函式庫深度剖析.md) |
| 端到端資料流（讀寫一個檔案） | 跨全部組件 | ✅ [報告](端到端資料流-客戶端讀寫檔案深度剖析.md) |

### Rust 產出物（來源：根目錄 `Cargo.toml` workspace members）

| 產出物 | 原始碼 | 報告 |
|---|---|---|
| `trash_cleaner` | `src/client/trash_cleaner/` | ✅ [報告](trash_cleaner-Rust垃圾回收工具深度剖析.md) |
| `chunk_engine` | `src/storage/chunk_engine/` | ✅ [報告](chunk_engine-Rust本地儲存引擎深度剖析.md) |
| `hf3fs-usrbio-sys` | `src/lib/rs/hf3fs-usrbio-sys/` | 併入 USRBIO 報告 |

---

## 完成狀態

**20 / 20 個產出物 + 客戶端函式庫層 + 端到端資料流，共 20 份報告**，合計約 29900 行。第二輪補齊 `hf3fs_common_shared` 的缺失章節並做了跨報告交叉驗證（見下）。

## 驗收方式

每份報告交付後都做兩道機械化檢查：
1. 抽出報告內所有 `檔案路徑:行號` 引用，逐一驗證檔案存在且行號未超出檔案長度
2. 用目錄實際檔案清單反查報告，確認「逐檔掃過」不是空話
再人工抽查若干條實質論述，與原始碼對照。

---

## 跨報告交叉驗證（第二輪）

逐組件報告各自成篇時，最容易出錯的是**共享事實**——同一個 serviceId、同一條不變式、同一個機制被多份報告各自描述，說法可能互相矛盾。第二輪針對這類跨切面事實做了一次審計。

### 發現並修正的矛盾

**serviceId 10 的性質**。三份報告給出相反結論：

| 報告 | 原本的說法 |
|---|---|
| `hf3fs_common_shared` | 「衝突是真的…一旦註冊會在啟動時直接回錯」 |
| `migration_main` | 「不是 bug——兩者永遠不在同一個 `net::Server` 群組註冊」 |
| `simple_example_main` | 「不是 bug——重複檢查是每個 `ServiceGroup` 各自一份」 |

讀原始碼定讞：`Services::addService` 的重複檢查（`src/common/serde/Services.h:21-23`）作用在自己那份 `std::array<ServiceWrapper, 65536> services_[2]`（`:38`）上，而全專案有兩類彼此獨立的 `Services` 實例——`net::Client::serdeServices_`（`src/common/net/Client.h:80`，`RDMAControl` 註冊於此）與 `net::ServiceGroup::serdeServices_`（`src/common/net/ServiceGroup.h:74`，server 端服務註冊於此）。**後兩份報告正確，`hf3fs_common_shared` 已修正**並補上完整的實例歸屬表。

**失效的交叉引用**。`meta_main` §13.4 引用了《3FS 存儲層》——該報告不在磁碟上。已改為指向實際存在的 `data_placement` 與 `storage_main` 報告。

### 驗證通過的共享事實

| 事實 | 涉及報告 | 驗證來源 |
|---|---|---|
| chunk 大小 11 個 2 冪分級（64KiB–64MiB） | `chunk_engine`、`storage_main` | `src/storage/chunk_engine/src/types/constants.rs:7-8` |
| IoRing 兩側共用同一份定義，USRBIO 側 `owner=false` | `hf3fs_fuse_main`、`hf3fs_api_shared` | `src/lib/api/UsrbIo.cc:13,398`、`src/fuse/IoRing.h:87` |
| 心跳逾時 60s | `mgmtd_main`、`storage_main` | `src/mgmtd/service/MgmtdConfig.h:16` |
| 垃圾回收三階段流水線（CLI 搬入垃圾桶 → `trash_cleaner` 到期刪除 → `GcManager` 真正回收） | `trash_cleaner`、`hf3fs_utils`、`meta_main` | 三份對分工的敘述一致，且引用同一組原始碼 |
| 全部 markdown 交叉連結有效 | 全部 | 逐一檢查連結目標存在 |

### 「宣告完整但未接線」的四個案例

這是本專案反覆出現的型態，四處性質相同：程式碼寫得完整、看起來正常，但不參與建置或不被註冊，因此不會被編譯器或連結器發現腐化。

| 案例 | 位置 | 狀態 | 詳見 |
|---|---|---|---|
| `TaosClient` | `src/common/monitor/TaosClient.{h,cc}` | `.cc` 全 138 行被 `/* */` 註解，實體化會**連結期**失敗 | `monitor_collector_main` §8.4 |
| `jemalloc_wrapper` / `mimalloc_wrapper` | `src/memory/{jemalloc,mimalloc}/` | `src/memory/CMakeLists.txt:5-6` 的 `add_subdirectory` 被註解掉 | 記憶體配置器報告 §1 |
| `ClientAgentSerde` | `src/fbs/lib/Service.h:195` | 定義 16+ 個方法，全 repo 零註冊零呼叫 | `hf3fs_common_shared` §3.3 |
| `data_placement_job.py` | `deploy/data_placement/src/model/` | 依賴 `smallpond`／`pyarrow`，兩者皆不在 repo 亦不在 requirements | `data_placement` §9 |
| `target_add_fbs` CMake macro | `cmake/Target.cmake:70` | 定義了 flatbuffers 建置流程，全樹零呼叫 | `hf3fs_common_shared` §5.8 |
| 內建 KV 服務（KVTB/KVNS/KVWG 前綴） | `src/common/kv/` | 前綴保留但服務已消失 | `hf3fs_common_shared` §9.5 |

### 一個被推翻的前提：`src/fbs/` 裡沒有 flatbuffers

我派工時要求分析「serde 與 flatbuffers 的分工」，agent 讀完程式碼後**推翻了這個前提**，並舉證：全 repo（排除 `third_party/`）零個 `.fbs` schema 檔；`src/fbs/` 全部 66 個檔案都是 `.h`/`.cc`/`CMakeLists.txt`，型別一律是 serde struct（如 `src/fbs/mgmtd/ConfigInfo.h:7`）；`cmake/Target.cmake:70` 的 `target_add_fbs` macro 無人呼叫；命名空間 `hf3fs::flat`、巨集參數 `flatns`、目錄名 `fbs` 全是歷史殘留。

**結論：3FS 曾用 flatbuffers 做 RPC，後來整套換成自製 serde，只留下名字。** 當前程式碼裡沒有第二套序列化框架。這個修正已逐項複驗成立。

### 報告中指出的潛在缺陷（已逐一複驗成立）

| 缺陷 | 位置 | 性質 |
|---|---|---|
| `sem_destroy` 作用在已 `munmap` 的位址 | `src/lib/api/UsrbIo.cc:520-529` 先 `munmap` 後 `delete IoRing`，而 `src/fuse/IoRing.h:183` 的 `cqeSem` 刪除器不看 `owner` | 潛在 use-after-munmap；glibc 的 `sem_destroy` 是 no-op 才沒爆 |
| FFI 邊界零驗證 | `src/storage/chunk_engine/src/core/engine.rs:304-310` 只檢查 `length != 0` 就 `from_raw_parts` | 記憶體安全完全依賴 C++ 呼叫端紀律 |
| 記憶體計數不對稱 | `GlobalMemoryAllocator.cc:98` 的 `add()` 無條件執行、`:122` 的 `sub()` 只在 `SAVE_ALLOCATE_SIZE` 下執行 | 只開一個旗標時 `used_bytes` 語意失真 |
| `auto_relax` 的日誌分支條件寫反 | `deploy/data_placement/src/model/data_placement.py:138-142` | 純訊息瑕疵，不影響正確性 |

---

## 對抗式事實查核（第三輪）

前兩輪只做了機械檢查（引用行號存在、檔案涵蓋率）與零星抽查，沒有系統性核對**論述實質**是否與程式碼相符。第三輪派出 6 個查核 agent 覆蓋全部 17 份報告，任務定位是「盡力推翻」而非「確認正確」，判定分四級。

**查核 427 條承重論述：CONFIRMED 345 / REFUTED 31 / OVERSTATED 41 / UNVERIFIABLE 10。**

| 查核範圍 | 條數 | C | R | O | U |
|---|---:|---:|---:|---:|---:|
| mgmtd、meta | 62 | 48 | 8 | 4 | 2 |
| storage、chunk_engine | 62 | 40 | 10 | 9 | 3 |
| FUSE、USRBIO | 62 | 50 | 4 | 5 | 3 |
| 共用基礎設施 | 58 | 46 | 3 | 7 | 2 |
| admin_cli、hf3fs_utils、hf3fs、migration | 88 | 81 | 2 | 5 | 0 |
| monitor、bench、trash_cleaner、simple_example、data_placement、記憶體包裝 | 95 | 80 | 4 | 11 | 0 |

全部 82 條負面判定都由主 session 重新打開原始碼複核過，無一誤判，並已逐條修正。

### 錯誤分布的規律

**核心機制的正確率遠高於周邊細節。** 選主與腦裂防護、CRAQ 三版本與狀態機、心跳協定、冪等機制、resync 五維比對、序列化的 16 條相容性規則——這些「錯了會誤導讀者做出錯誤決策」的部分逐條比對後幾乎全部成立。錯誤集中在三類：

1. **數字與型別細節**：「63 是質數」（實為 7×9）、`NodeId u64`（實為 u32）、方法數 24（實為 23，把廢棄的空號算了進去）、FATAL 計數、分桶標籤 32 MiB（實為 64 MiB）、`unsafe` 分類加總 28（實為 26，三處被重複計入兩類）。
2. **因果解釋**：這是最容易腦補的一類。`sem_timedwait` 逾時被說成「不漏事件的保底輪詢」，實際逾時路徑一個 ioring 都不掃；`assert` passthrough 被歸因於處理使用者取消，實際 `click.Abort` 繼承 `RuntimeError` 根本落不到那個 except。
3. **主 session 自撰或補寫的章節**：meta §13–16、storage §10–13、`data_placement`、記憶體包裝——82 條負面判定裡有相當比例落在這幾處。自我驗證確實有盲點。

### `OVERSTATED` 這一級抓到的東西

技術寫作最常見的失真不是說錯，而是**把條件性行為寫成無條件**。幾個代表：

- 「serviceId 10 的兩條分派路徑從不交會」→ 當前設定下成立，但 `Server::serdeCtxCreator()`（`src/common/net/Server.h:36-38`）是一條架構上的交會點。
- 「USRBIO 控制平面沒有 ioctl」→ Iov/Ior 的建立銷毀確實沒有，但 `hf3fs_reg_fd` 必經 `HF3FS_IOC_GET_MAGIC_NUM`。
- 「AclCache 期望壽命略低於 0.9×TTL」→ 只適用於一生被讀一次的 entry，頻繁讀取者逼近 0.8×TTL 下界。
- 「SetChainTable bug 覆蓋掉舊 version」→ `tableVersion` 建構時硬編碼為 1（`SetChainTableOperation.cc:40`），實際永遠改寫第 1 版。
- 「記憶體 wrapper 處於休眠狀態」→ 不只休眠，**取消註解後大概率建不起來**（見下）。

### 查核反而讓兩處結論變得更嚴重

有兩條原本被寫得太輕：

**`AutoFallbackVariant` 不是「保不住原始位元組」，而是不消費位元組。** `parseVariant`（`src/common/serde/Serde.h:716-720`）只讀走型別名就回傳同一個串流參照，未知型別分支（`:562-572`）直接 return 未呼叫 `deserialize`。串流指標停在值的開頭，**外層 table 後續欄位全部錯位**，解出看似合理的垃圾而非乾淨報錯。這個機制只有在該 variant 是訊息最後一個欄位時才安全。

**記憶體 wrapper 已經腐化，不只是休眠。** 兩處硬證據：(a) `src/memory/jemalloc/CMakeLists.txt:1` 連結的 `hf3fs_jemalloc` 目標**不存在**（只有 `jemalloc` 與 `hf3fs_jemalloc_shared`）；(b) `OverrideCppNewDelete.h` 的「只能被一個 TU include」約定已被違反——9 個 TU include 它，其中 `src/common/monitor/Monitor.cc` 編進連給每個 binary 的 `common` 靜態庫，開啟覆寫後必然重複定義 `operator new`。

### 被獨立驗證強化的缺陷指控

`UsrbIo.cc` 的 use-after-munmap 經獨立查核**成立且證據更強**：查核者找出了不對稱的根因——FUSE 端 `IoRing` 持有 `shared_ptr<ShmBuf>` 保住映射（`src/fuse/IoRing.h:206`，建構子註解明說用途），而使用者端 `UsrbIo.cc:388` 傳的是**空的 shared_ptr**，這道保護被刻意繞過。

### 最終驗收

2685 個唯一 `檔案:行號` 引用，零懸空、零行號超界（兩處誤報為日誌格式範例與敘述佔位符，非引用）。
