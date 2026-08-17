# jemalloc_wrapper / mimalloc_wrapper（記憶體配置器包裝）深度剖析

> 對應原始碼：`src/memory/`（13 個檔案，共約 500 行）
> 建置目標：`src/memory/jemalloc/CMakeLists.txt:1`、`src/memory/mimalloc/CMakeLists.txt:1`
> 建置開關：`CMakeLists.txt:56-72`（`OVERRIDE_CXX_NEW_DELETE`、`SAVE_ALLOCATE_SIZE`）

---

## 0. 一句話總結

`src/memory/` 實作了一套**可在執行期抽換的全域記憶體配置器**：把 `operator new/delete` 全部改導向一個 `MemoryAllocatorInterface` 虛介面，實際的配置器（jemalloc 或 mimalloc）編成獨立的 `.so`，由環境變數 `MEMORY_ALLOCATOR_LIB_PATH` 指定路徑、用 `dlopen` + `dlsym("getMemoryAllocator")` 在第一次配置時載入，順帶在每個執行緒上掛一個分桶的記憶體用量計數器餵給監控系統。**但這整套機制預設是關閉的，而且那兩個 `.so` 目標目前根本不會被建置**——`src/memory/CMakeLists.txt:5-6` 的兩行 `add_subdirectory` 被註解掉了。所以本報告描述的是一套**程式碼齊全但已經腐化**的子系統——它不只是休眠，而是**取消註解後大概率建不起來**（兩處硬證據見 §1.1）。

---

## 1. 先講結論：三個層次的「關閉」

這是讀這份程式碼最容易搞混的地方，必須先釐清。它有三道獨立的開關，任何一道關上，後面的程式碼就完全不執行：

```
┌─ 第一道：wrapper .so 根本不建置 ────────────────────────────────┐
│  src/memory/CMakeLists.txt:4-6                                  │
│    add_subdirectory(common)                                     │
│    # add_subdirectory(jemalloc)      ← 註解掉                    │
│    # add_subdirectory(mimalloc)      ← 註解掉                    │
│  ⇒ libjemalloc_wrapper.so / libmimalloc_wrapper.so 不存在        │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─ 第二道：new/delete 覆寫預設關閉 ───────────────────────────────┐
│  CMakeLists.txt:56                                              │
│    option(OVERRIDE_CXX_NEW_DELETE "..." OFF)   ← 預設 OFF        │
│  setup.py:52 也明確傳 -DOVERRIDE_CXX_NEW_DELETE=OFF              │
│  ⇒ 未定義該巨集時，GlobalMemoryAllocator.h:27-43 的 inline 版本   │
│     直接轉呼 std::malloc / std::free，整個 .cc 檔編譯成空的       │
└─────────────────────────────────────────────────────────────────┘
                              │
┌─ 第三道：即使開了覆寫，也要環境變數才會載入 ─────────────────────┐
│  GlobalMemoryAllocator.cc:31-37                                 │
│    const char *mallocLibPath = std::getenv("MEMORY_ALLOCATOR_LIB_PATH");
│    if (mallocLibPath == nullptr || ...) goto exit;               │
│  ⇒ 沒設就 gAllocator == nullptr，退回 std::malloc               │
└─────────────────────────────────────────────────────────────────┘
```

還有一道條件性強制關閉（`CMakeLists.txt:61-63`）：

```cmake
if (DEFINED SANITIZER AND SANITIZER)
    set(OVERRIDE_CXX_NEW_DELETE OFF)
endif()
```

開 sanitizer 時強制關掉。這是必須的——ASan/TSan 自己要接管 `new`/`delete` 才能追蹤記憶體錯誤，兩套覆寫互搶會直接壞掉。

### 1.1 它不只是休眠，而是已經腐化

「不參與建置」的直接後果是沒有任何編譯器或連結器在守著它。實測有兩處已經壞掉：

**(a) `jemalloc_wrapper` 連結的目標不存在。**

```cmake
# src/memory/jemalloc/CMakeLists.txt:1
target_add_shared_lib(jemalloc_wrapper hf3fs_jemalloc)
```

全 repo grep `hf3fs_jemalloc`，`cmake/Jemalloc.cmake` 只定義了兩個目標：`jemalloc`（INTERFACE，`:1`）與 `hf3fs_jemalloc_shared`（IMPORTED，`:2`）。**`hf3fs_jemalloc` 這個名字不存在**。CMake 對未知的連結名不會報錯，會當成裸連結旗標傳給連結器，於是變成 `-lhf3fs_jemalloc` → 連結期找不到函式庫。

**(b) `OverrideCppNewDelete.h` 的「只能被一個 TU include」約定已經被違反。**

這個標頭在 `#ifdef OVERRIDE_CXX_NEW_DELETE` 內**定義**（而非宣告）全域 `operator new/delete`（見 §9），所以整個連結單元裡只能有一份。實際 include 它的有 9 個 TU：

```
src/mgmtd/mgmtd.cpp                 src/meta/meta.cpp
src/storage/storage.cpp             src/simple_example/main.cpp
src/monitor_collector/monitor_collector.cpp
src/migration/main.cpp              tests/main/TestMain.cc
benchmarks/storage_bench/StorageBench.cc
src/common/monitor/Monitor.cc       ← 關鍵
```

前八個各自屬於不同的 binary，彼此不衝突。**問題出在第九個**：`Monitor.cc` 被編進 `common` 靜態庫（`src/common/CMakeLists.txt:6` 的 `target_add_lib(common ...)`），而該庫連進每一個 binary。所以只要 `OVERRIDE_CXX_NEW_DELETE=ON`，`mgmtd_main` 就會同時從 `mgmtd.cpp` 與 `libcommon.a(Monitor.cc)` 拿到兩份 `operator new` 定義 → 重複符號。

目前之所以沒人發現，正是因為該巨集預設 OFF，兩份定義都被 `#ifdef` 消掉，九個 TU 全部編譯成空的。

**那 `meta_main` 與 `storage_main` 的 `jemalloc` 是怎麼回事？** `src/meta/CMakeLists.txt:2` 與 `src/storage/CMakeLists.txt:5` 確實連結了名為 `jemalloc` 的目標，但那**不是**這裡的 wrapper：

```cmake
# cmake/Jemalloc.cmake:1-2, 20
add_library(jemalloc INTERFACE)
add_library(hf3fs_jemalloc_shared SHARED IMPORTED)
...
target_link_libraries(jemalloc INTERFACE hf3fs_jemalloc_shared)
```

那是一個 INTERFACE 目標，指向 `${JEMALLOC_DIR}/lib/libjemalloc.so.2`（`cmake/Jemalloc.cmake:18`）——**真正的 jemalloc 函式庫，透過傳統的符號插入（symbol interposition）取代 libc 的 malloc**。

所以 3FS 裡有**兩套彼此獨立的配置器機制**：

| | 傳統連結法（目前生效） | wrapper 機制（休眠中） |
|---|---|---|
| 怎麼生效 | 連結 `libjemalloc.so.2`，動態連結器讓它的 `malloc` 蓋掉 libc 的 | 覆寫 `operator new`，內部 `dlopen` 一個實作了 `MemoryAllocatorInterface` 的 `.so` |
| 誰在用 | `meta_main`、`storage_main` | 無人 |
| 能換配置器嗎 | 要重新編譯／連結 | 改環境變數即可 |
| 有記憶體指標嗎 | 沒有（只能靠 jemalloc 自己的 stats） | 有，分 20 桶 + per-thread |
| 覆蓋範圍 | 全進程所有 malloc（含第三方庫） | 只有走 `operator new` 的（C 的 `malloc` 不受影響） |

最後一列是關鍵差異：wrapper 機制**只攔截 C++ 的 new/delete**，第三方 C 函式庫直接呼叫 `malloc` 時完全繞過它。這也解釋了為什麼傳統連結法才是實際採用的方案——覆蓋更完整。

---

## 2. 整體架構

```
                        ┌───────────────────────────────────┐
                        │  應用程式的 new / delete           │
                        └────────────────┬──────────────────┘
                                         │ （僅當 OVERRIDE_CXX_NEW_DELETE 定義時）
                        ┌────────────────▼──────────────────┐
                        │ OverrideCppNewDelete.h:11-17      │
                        │   operator new  → memory::allocate│
                        │   operator delete → ::deallocate  │
                        └────────────────┬──────────────────┘
                                         │
          ┌──────────────────────────────▼──────────────────────────────┐
          │ GlobalMemoryAllocator.cc                                    │
          │                                                             │
          │  allocate(size)                                             │
          │   ├─ call_once → loadMemoryAllocatorLib()  ← 第一次配置時    │
          │   │    dlopen($MEMORY_ALLOCATOR_LIB_PATH)                   │
          │   │    dlsym("getMemoryAllocator")                          │
          │   │    gAllocator = getMemoryAllocatorFunc()                │
          │   ├─ [SAVE_ALLOCATE_SIZE] 多配 kHeaderSize，寫入 size+原指標 │
          │   ├─ gAllocator ? gAllocator->allocate() : std::malloc()    │
          │   └─ gMemCounter.add(allocateSize)   ← thread_local         │
          └──────────────┬────────────────────────────┬─────────────────┘
                         │                            │
        ┌────────────────▼──────────┐   ┌─────────────▼──────────────────┐
        │ MemoryAllocatorInterface  │   │ AllocatedMemoryCounter          │
        │ （純虛介面，5 個方法）      │   │  thread_local 實例              │
        │  allocate / deallocate    │   │  20 個大小桶，每 100MB 上報一次  │
        │  memalign / logstatus     │   │  ──► monitor::CountRecorder     │
        │  profiling                │   └────────────────────────────────┘
        └────────────┬──────────────┘
                     │ 由獨立 .so 實作，extern "C" getMemoryAllocator()
        ┌────────────┴────────────┐
        ▼                         ▼
┌───────────────────┐   ┌────────────────────┐
│ JemallocLib.cc    │   │ MimallocLib.cc     │
│ libjemalloc_      │   │ libmimalloc_       │
│   wrapper.so      │   │   wrapper.so       │
│ （目前不建置）      │   │ （目前不建置）       │
└───────────────────┘   └────────────────────┘
```

---

## 3. 介面設計

### 3.1 五個方法的虛介面

`MemoryAllocatorInterface.h:8-16` 全文只有 9 行：

```cpp
class MemoryAllocatorInterface {
 public:
  virtual ~MemoryAllocatorInterface() = default;
  virtual void *allocate(size_t size) = 0;
  virtual void deallocate(void *mem) = 0;
  virtual void *memalign(size_t alignment, size_t size) = 0;
  virtual void logstatus(char *buf, size_t size) = 0;
  virtual bool profiling(bool active, const char *prefix) = 0;
};
```

值得注意的是**沒有 `realloc`**。這是因為介面只服務 `operator new/delete`，而 C++ 的 new 沒有 realloc 語意——需要擴充的容器（`std::vector`）自己做「配新的 → 搬 → 釋放舊的」。介面只提供最小必要集合。

`logstatus` 用 `char* buf, size_t size` 而非回傳 `std::string`，這是**刻意避免在配置器內部配置記憶體**——在 `operator new` 的實作路徑上做 `std::string` 配置會直接遞迴。

### 3.2 ABI 邊界：一個 C 函式

`MemoryAllocatorInterface.h:4,18`：

```cpp
#define GET_MEMORY_ALLOCATOR_FUNC_NAME "getMemoryAllocator"
using GetMemoryAllocatorFunc = MemoryAllocatorInterface *(*)();
```

`.so` 那一側（`MimallocLib.cc:48-53`）：

```cpp
extern "C" {
hf3fs::memory::MemoryAllocatorInterface *getMemoryAllocator() {
  static hf3fs::memory::MimallocMemoryAllocator mimalloc;
  return &mimalloc;
}
}
```

`extern "C"` 避免 name mangling，讓 `dlsym` 用固定字串就能找到。回傳的是**函式內 static 的位址**——生命週期到進程結束，不需要釋放，也不需要考慮誰擁有它。這是 `dlopen` 場景下最省事的正確做法：wrapper `.so` 一旦載入就不會卸載（`GlobalMemoryAllocator.cc:69-72` 只在**取得失敗時**才 `dlclose`）。

`src/memory/CMakeLists.txt:1-2` 為此特別放寬了符號可見性：

```cmake
set(CMAKE_CXX_VISIBILITY_PRESET default)
set(CMAKE_VISIBILITY_INLINES_HIDDEN 0)
```

專案其他地方用 hidden visibility 減少動態符號表，但這裡必須讓 `getMemoryAllocator` 可被 `dlsym` 看到。

---

## 4. 載入流程

`GlobalMemoryAllocator.cc:25-73` 的 `loadMemoryAllocatorLib()` 是全份程式碼唯一有 `goto` 的地方——用經典的 C 風格集中錯誤出口：

```cpp
static void loadMemoryAllocatorLib() {
  const char *mallocLibPath = std::getenv("MEMORY_ALLOCATOR_LIB_PATH");
  if (mallocLibPath == nullptr || mallocLibPath[0] == '\0') goto exit;

  mallocLib = ::dlopen(mallocLibPath, RTLD_NOW | RTLD_GLOBAL);
  if (mallocLib == nullptr) { fprintf(stderr, ...); goto exit; }

  getMemoryAllocatorFunc = (GetMemoryAllocatorFunc)::dlsym(mallocLib, GET_MEMORY_ALLOCATOR_FUNC_NAME);
  if (getMemoryAllocatorFunc == nullptr) { ...; goto exit; }

  gAllocator = getMemoryAllocatorFunc();
  if (gAllocator == nullptr) { ...; goto exit; }

  gAllocator->logstatus(logBuf, sizeof(logBuf));
  fprintf(stderr, "Memory allocator loaded: %s\n", logBuf);

exit:
  if (gAllocator == nullptr && mallocLib != nullptr) ::dlclose(mallocLib);
}
```

幾個細節：

| 細節 | 原因 |
|---|---|
| 全程用 `fprintf(stderr)` 而非 `XLOG` | 這段程式碼在 `operator new` 裡執行，日誌框架自己會配置記憶體 → 遞迴。`stderr` 是唯一安全的輸出管道 |
| `RTLD_NOW` | 立即解析所有符號。延遲解析（`RTLD_LAZY`）會讓符號錯誤在後續某次配置時才爆炸，難以除錯 |
| `RTLD_GLOBAL` | 讓 `.so` 的符號進入全域命名空間，供後續載入的其他 `.so` 使用 |
| 任何一步失敗都只是 `gAllocator == nullptr` | **失敗不致命**。後續所有路徑都有 `gAllocator ? ... : std::malloc(...)` 的分支（`:90-93`、`:125-129`、`:147-150`），退回系統配置器繼續跑 |
| 失敗時 `dlclose` | 避免洩漏一個載入了但用不上的 `.so` |

觸發時機在 `allocate()` 開頭（`:76-79`）：

```cpp
if (!gAllocatorInited) {
  std::call_once(gInitOnce, loadMemoryAllocatorLib);
  gAllocatorInited = true;
}
```

**懶載入 + `call_once`**。不能在 static 初始化階段載入，因為那時可能已經有其他 static 物件在配置記憶體了（static 初始化順序不確定）。放在第一次配置時是唯一可靠的時機。

注意 `gAllocatorInited` 是普通 `bool` 而非 `atomic`——這是刻意的效能取捨。`call_once` 本身保證只執行一次且有記憶體屏障，外層這個非原子旗標只是為了讓**後續數十億次配置**跳過 `call_once` 的函式呼叫開銷。多執行緒下最壞情況是幾條執行緒都進去呼叫 `call_once`，但 `call_once` 自己會處理，語意仍正確。

---

## 5. `SAVE_ALLOCATE_SIZE`：為了統計而多配一個標頭

### 5.1 標頭佈局

`operator delete` 只拿到指標，拿不到大小，所以無法統計釋放量。`SAVE_ALLOCATE_SIZE` 的解法是在每塊記憶體前面多配一個標頭（`GlobalMemoryAllocator.cc:21-23`）：

```cpp
static const size_t kHeaderSize = alignof(max_align_t);
static_assert(kHeaderSize >= 2 * sizeof(size_t), "kHeaderSize < 2 * sizeof(size_t)");
static_assert(sizeof(void *) <= sizeof(size_t), "sizeof(void *) > sizeof(size_t)");
```

```
一般配置（allocate，:100-106）：

  mem ─┐
       ▼
       ┌──────────┬──────────┬─────────────────────────────┐
       │allocSize │ 原始指標  │ 使用者可見的 size 個 byte      │
       │ (size_t) │ (size_t) │                             │
       └──────────┴──────────┴─────────────────────────────┘
       │◄── kHeaderSize ────►│
                             ▲
                             └─ 回傳給呼叫者的位址

對齊配置（memalign，:138-163）：

  mem ─┐
       ▼
       ┌───填充───┬──────────┬──────────┬────────────────────┐
       │          │allocSize │ 原始指標  │ 使用者可見資料       │
       └──────────┴──────────┴──────────┴────────────────────┘
       │◄─ alignedHeaderSize = max(alignment, kHeaderSize) ─►│
                                                             ▲
                                                    對齊到 alignment 邊界
```

`kHeaderSize` 取 `alignof(max_align_t)`（x86-64 上是 16）而非 `2*sizeof(size_t)`（也是 16），是為了**保證回傳位址仍滿足最大基本對齊要求**。兩個 `static_assert` 分別確保標頭塞得下兩個欄位、以及指標塞得進 `size_t`。

為什麼要存「原始指標」而不只是靠 `ptr - kHeaderSize` 反推？因為 `memalign` 路徑的填充量是 `max(alignment, kHeaderSize)`，不是固定值，所以釋放時（`:116-119`）必須讀出原始指標才能正確歸還：

```cpp
uint8_t *header = static_cast<uint8_t *>(mem) - kHeaderSize;
auto memPtr = reinterpret_cast<size_t *>(header) + 1;
mem = reinterpret_cast<uint8_t *>(*memPtr);
assert(mem <= header);
```

標頭永遠緊貼在使用者指標前方 `kHeaderSize` 處（`memalign` 在 `:158` 刻意把標頭寫在 `mem + (alignedHeaderSize - kHeaderSize)`，即緊貼回傳位址之前），所以讀標頭的算式對兩條路徑一致。

### 5.2 一個實質的計數不對稱

比對 `allocate` 與 `deallocate` 裡計數器的呼叫位置：

```cpp
// allocate，GlobalMemoryAllocator.cc:98 —— 在 #ifdef 之外
  gMemCounter.add(allocateSize);

#ifdef SAVE_ALLOCATE_SIZE
  ... 寫入標頭 ...
#endif
```

```cpp
// deallocate，GlobalMemoryAllocator.cc:115-123 —— 在 #ifdef 之內
#ifdef SAVE_ALLOCATE_SIZE
  ...
  size_t allocateSize = *sizePtr;
  gMemCounter.sub(allocateSize);
#endif
```

`add()` **無條件執行**，`sub()` **只在 `SAVE_ALLOCATE_SIZE` 定義時執行**。

所以在 `OVERRIDE_CXX_NEW_DELETE=ON` 而 `SAVE_ALLOCATE_SIZE=OFF` 這個組合下（`CMakeLists.txt:66-71` 允許這個組合：外層 if 只檢查 `OVERRIDE_CXX_NEW_DELETE`，內層才檢查 `SAVE_ALLOCATE_SIZE`）：

- 每次配置都累加 `allocatedMemory_`
- 釋放時完全不記帳
- `AllocatedMemoryCounter.cc:132` 的 `changed = allocated - deallocated` 恆等於 `allocated`
- 上報的 `memory_allocator.used_bytes` 是個**只增不減的累計配置量**，而不是它名字宣稱的「當前使用量」

這不會造成崩潰或記憶體錯誤——標頭本來就沒配置，`deallocate` 也正確地沒去讀它——純粹是**指標語意失真**。要讓 `used_bytes` 有意義，就必須同時開啟兩個開關。不過由於整套機制目前是休眠的（見 §1），這個不對稱在現況下不會被觸發。

---

## 6. 記憶體用量計數器

### 6.1 分桶

`AllocatedMemoryCounter.h:33-39`：

```cpp
static size_t calcBucket(size_t size) {
  size_t sizeWidth = SIZE_WIDTH - __builtin_clzll(size);
  size_t bucket = sizeWidth > kMinBucketSizeWidth ? sizeWidth - kMinBucketSizeWidth + 1 : 1;
  return std::min(bucket, kMaxNumBuckets - 1);
}

static size_t calcBucketSize(size_t bucketIndex) { return 1ULL << (kMinBucketSizeWidth + bucketIndex - 2); }
```

`__builtin_clzll` 數前導零，`SIZE_WIDTH - clz(size)` 就是「size 的位元寬度」，也就是 `floor(log2(size)) + 1`。所以**分桶是按 2 的冪級數**：

```
kMinBucketSizeWidth = 9,  kMaxNumBuckets = 20

bucket  0 : "Total"          ← 特殊，累計全部（initBucketTagSets:60）
bucket  1 : < 512 B          （所有 sizeWidth ≤ 9 的都歸這裡；標籤寫 256B）
bucket  2 : 512 B
bucket  3 : 1 KiB
   ⋮
bucket 19 : ≥ 64 MiB         （min() 截斷，所有更大的都歸這裡；標籤 67108864B）
```

`bucket 0` 被徵用為「總計」而非最小尺寸桶，所以 `add()`（`:20-24`）每次都寫兩個位置：

```cpp
void add(size_t size) {
  allocatedMemory_[0] += size;              // 總計
  allocatedMemory_[calcBucket(size)] += size;  // 對應大小桶
  tryReport();
}
```

分桶的價值在於**看出配置模式**：大量小配置代表物件開銷，少量大配置代表緩衝區。兩者的最佳化手段完全不同。

### 6.2 thread_local 與上報節流

計數器實例是 `thread_local`（`GlobalMemoryAllocator.cc:19`）：

```cpp
static thread_local hf3fs::memory::AllocatedMemoryCounter gMemCounter;
```

每條執行緒各有一份，`add`/`sub` 完全無鎖無競爭。累積到門檻才上報（`AllocatedMemoryCounter.cc:124-128`）：

```cpp
ssize_t totalAllocated = allocatedMemory_[0];
ssize_t totalDeallocated = deallocatedMemory_[0];
bool reportMemUsage = totalAllocated + totalDeallocated >= gMemoryMetricReportInterval;
```

門檻預設 `100_MB`（`:25`），可用環境變數 `MEMORY_METRIC_REPORT_INTERVAL` 覆寫（`:30-40`）。注意判斷式用的是 **配置量 + 釋放量的總和**，不是淨值——因為反覆配置釋放同樣大小的物件，淨值恆為 0，但那正是最需要被觀測的高頻配置行為。

上報後立即歸零（`:133-135`）：

```cpp
// first clear the (de)allocated memory
allocatedMemory_[bucketIndex] = 0;
deallocatedMemory_[bucketIndex] = 0;
// then report metrics
```

註解特別標明「先清零、再上報」的順序。原因在 §6.3。

執行緒結束時解構子強制上報一次（`:116`），避免短命執行緒的統計丟失：

```cpp
AllocatedMemoryCounter::~AllocatedMemoryCounter() { tryReport(true /*force*/); }
```

### 6.3 遞迴防護

這是整份程式碼最精妙的一處。`tryReport` 會呼叫 `addSample`，而 `addSample` **可能自己配置記憶體**——那就會再次進入 `allocate` → `gMemCounter.add` → `tryReport`，無限遞迴。

防護有三層（`AllocatedMemoryCounter.cc:118-122`）：

```cpp
void AllocatedMemoryCounter::tryReport(bool force) {
  // disable reporting during shutdown and initialization to avoid using the destroyed or uninitialized tag sets
  if (reporting_ || !initialized_ || gShutdown) return;
  reporting_ = true;
  ...
```

| 旗標 | 防的是什麼 |
|---|---|
| `reporting_` | **遞迴**。進入上報就置位，內層再進來直接返回 |
| `initialized_` | **建構期**。建構子 `:108-113` 先做 `initialize()` 與 `initThreadTagSet()` 才把 `initialized_` 設為 true；這兩步本身會配置記憶體，此時 tag set 還沒建好 |
| `gShutdown` | **解構期**。`shutdown()`（`:106`）置位後，全域的 Recorder 可能已被解構，再碰就是 use-after-free |

而 §6.2 提到的「先清零再上報」也是同一個問題的一部分：如果先上報再清零，遞迴進來的那一層會看到尚未清零的數值而重複計算。先清零則遞迴層看到的是 0，不會重複。

`initialize()` 的雙旗標（`:94-104`）處理多執行緒同時首次配置：

```cpp
void AllocatedMemoryCounter::initialize() {
  if (gShutdown || gInitialized) return;
  bool initializing = gInitializing.exchange(true);
  if (!initializing) {
    initFromEnvVars();
    initBucketTagSets();
    gInitialized = true;
  }
}
```

用 `exchange` 讓第一條執行緒負責初始化，其餘直接返回——**不等待**。這代表其他執行緒可能在 tag set 還沒建好時就繼續執行，但它們的 `initialized_` 尚未置位，`tryReport` 會被第二層旗標擋下，所以安全。用「不等待 + 下游擋住」取代鎖，避開了在配置器路徑上使用互斥鎖的風險。

### 6.4 七組指標

`initBucketTagSets`（`:57-80`）為每個桶建立七個 Recorder：

| 指標名 | reset | 語意 |
|---|---|---|
| `memory_allocator.used_bytes` | false | 淨變化累計 = 當前使用量（需 §5.2 兩個開關都開才準） |
| `memory_allocator.allocated_bytes` | true | 區間配置量 |
| `memory_allocator.deallocated_bytes` | true | 區間釋放量 |
| `memory_allocator.thread_allocated_bytes` | true | 同上，帶執行緒標籤 |
| `memory_allocator.thread_deallocated_bytes` | true | 同上 |
| `memory_allocator.thread_accum_allocated_bytes` | false | 執行緒累計配置量 |
| `memory_allocator.thread_accum_deallocated_bytes` | false | 執行緒累計釋放量 |

`reset` 參數區分「區間值」與「累計值」（見 `monitor_collector_main` 報告的 Recorder 家族章節）。

帶 thread 標籤的四個受 `MEMORY_METRIC_THREAD_COUNTER` 環境變數控制（`:46-50`、`:142`、`:149`），預設關閉——因為每條執行緒一組標籤會讓監控系統的 cardinality 暴增。執行緒名取自 `folly::getCurrentThreadName()`（`:89-91`），取不到就用 `"(null)"`。

---

## 7. 兩個實作

### 7.1 JemallocLib.cc

`JemallocLib.cc:9` 起的 `JemallocMemoryAllocator`。它的 `logstatus`（`:11` 起）透過 jemalloc 的 `mallctl` 介面讀六個統計量：`stats.allocated`、`stats.active`、`stats.metadata`、`stats.resident`、`stats.mapped`、`stats.retained`，並在 `:15-40` 用大段註解**原文引用 jemalloc 官方文件**逐一解釋每個欄位的定義與相互關係（例如 `active ≥ allocated` 且是頁大小的倍數、`resident` 是最大值而非精確值）。

把上游文件抄進註解在一般程式碼裡是壞味道，但這裡合理：這六個數字的差異極其微妙（`mapped` 與 `resident` 之間**沒有嚴格大小關係**），查一次文件的成本遠高於多讀 25 行註解。

`profiling` 方法對應 jemalloc 的 heap profiling。**但要注意它其實沒有被接上 §8 的設定項**——`MemoryAllocatorConfig` 的熱更新回呼完全繞過 wrapper（`src/common/app/Utils.cc:198-216`）：

```cpp
return cfg.addCallbackGuard([&cfg, hostname = std::move(hostname)] {
  if (malloc_stats_print == nullptr) { return; }        // weak symbol 偵測有無動態連到 jemalloc
  std::string prefix = fmt::format("{}{}", cfg.prof_prefix(), hostname);
  je_profiling(cfg.prof_active(), prefix.c_str());       // ← 檔案內的 static 函式
  ...
```

`je_profiling`（`Utils.cc:24` 起）直接對 jemalloc 下 `mallctl("prof.active" / "prof.dump" / "prof.prefix")`，整條路徑不碰 `MemoryAllocatorInterface`、不碰 `memory::profiling`、不碰 `gAllocator`。

所以 `JemallocLib.cc` 的 `profiling()` 與 `Utils.cc:24` 的 `je_profiling()` 是**兩份近乎逐字重複的實作，前者無人呼叫**。這反而印證了 §1 的判斷：兩套機制確實彼此獨立——設定項掛的是傳統連結法那一套。

### 7.2 MimallocLib.cc

`MimallocLib.cc` 全檔 53 行，是 jemalloc 版的精簡對照組：

```cpp
void *allocate(size_t size) override { return mi_malloc(size); }
void deallocate(void *mem) override { return mi_free(mem); }
void *memalign(size_t alignment, size_t size) override { return mi_memalign(alignment, size); }

bool profiling(bool, const char *) override {
  fprintf(stderr, "Memory profile not supported by mimalloc\n");
  return true;
}
```

`logstatus`（`:24-31`）回報 mimalloc 的 `current_rss` / `peak_rss` / `current_commit` / `peak_commit` / `page_faults`——與 jemalloc 是**完全不同的一組指標**。這暴露了介面設計的一個折衷：`logstatus` 回傳的是自由格式字串而非結構化欄位，因為不同配置器的統計維度根本無法統一。代價是這個字串只能給人看，無法餵給監控系統做時序分析。

`profiling` 不支援卻回傳 `true`（表示成功），是個小瑕疵——呼叫端無法區分「開啟成功」與「不支援但假裝成功」。

---

## 8. 設定項

`MemoryAllocatorConfig.h:6-9` 全文：

```cpp
struct MemoryAllocatorConfig : public ConfigBase<MemoryAllocatorConfig> {
  CONFIG_HOT_UPDATED_ITEM(prof_prefix, "");
  CONFIG_HOT_UPDATED_ITEM(prof_active, bool{});
};
```

兩項都是 `HOT_UPDATED`，代表**可在服務執行中透過 `admin_cli` 的 `hot-update-config` 改動**（見 `admin_cli` 報告）。用途是線上開關 heap profiling：懷疑某個服務有記憶體洩漏時，把 `prof_active` 打開一段時間、`prof_prefix` 指定 dump 檔前綴，事後用 `jeprof` 分析，不必重啟進程。

`shutdown()`（`GlobalMemoryAllocator.cc:183-192`）在關閉時會主動關掉 profiling 並印最後一次狀態：

```cpp
void shutdown() {
  if (gAllocator) {
    gAllocator->profiling(false, nullptr);
    gAllocator->logstatus(logBuf, sizeof(logBuf));
    fprintf(stderr, "Memory allocator shutdown: %s\n", logBuf);
  }
  memory::AllocatedMemoryCounter::shutdown();
}
```

---

## 9. 未定義 `OVERRIDE_CXX_NEW_DELETE` 時的樣貌

`GlobalMemoryAllocator.h:25-45` 提供一組 inline 的空殼實作，讓呼叫端不需要條件編譯：

```cpp
inline void *allocate(size_t size) { return std::malloc(size); }
inline void deallocate(void *mem) { return std::free(mem); }

// A copy of folly::aligned_malloc() from third_party/folly/folly/Memory.h
inline void *memalign(size_t alignment, size_t size) {
  void *ptr = nullptr;
  int rc = posix_memalign(&ptr, alignment, size);
  return rc == 0 ? (errno = 0, ptr) : (errno = rc, nullptr);
}

inline void logstatus(char *buf, size_t size) { std::snprintf(buf, size, "C++ new/delete not overridden"); }
inline bool profiling(bool, const char *) { return false; }
inline void shutdown() {}
```

`memalign` 的註解明說是從 folly 抄來的，用途是「用 `posix_memalign` 模仿 `memalign` 的行為」——差別在 `posix_memalign` 用回傳值報錯而不設 `errno`，這份包裝把它翻譯回 `errno` 慣例。

同時 `GlobalMemoryAllocator.cc` 整份被 `#ifdef OVERRIDE_CXX_NEW_DELETE`（`:14`）到 `#endif`（`:194`）包住，未定義時編譯出一個**空的 translation unit**——與 `monitor_collector` 報告裡那個被整份註解掉的 `TaosClient.cc` 是同一個效果，但這裡是刻意設計的條件編譯，不是遺留死碼。

`OverrideCppNewDelete.h` 同樣被 `#ifdef` 包住（`:8-19`）。這個檔案很特殊——它在標頭檔裡**定義**（而非宣告）全域 `operator new/delete`，所以只能被**恰好一個** translation unit include，否則就是重複定義。檔案本身沒有任何機制阻止誤用，只能靠約定。

---

## 10. 設計取捨

| 取捨 | 好處 | 代價 |
|---|---|---|
| `dlopen` + 虛介面，而非編譯期選配置器 | 換配置器只需改環境變數；同一份 binary 可在不同環境用不同配置器 | 每次配置多一次虛函式呼叫；只能攔 C++ new，攔不到 C 的 malloc |
| 配置器載入失敗就退回 `std::malloc` | 極高的韌性——設錯路徑不會讓服務起不來 | 失敗是靜默的（只有 stderr 一行），監控上看不出來 |
| 計數器用 `thread_local` | 熱路徑完全無鎖 | 執行緒多時記憶體開銷 = 執行緒數 × 20 桶 × 2 陣列 × 8 byte |
| 上報用「配置量+釋放量」節流 | 抓得到高頻小配置 | 門檻固定 100MB，配置量差異大的服務節奏不一 |
| `logstatus` 回傳自由格式字串 | 各配置器可回報自己獨有的統計 | 無法結構化進監控系統，只能給人看 |
| 三個 `bool` 旗標防遞迴 | 不需要鎖，在配置器路徑上是唯一可行解 | 邏輯分散，`initialized_` / `reporting_` / `gShutdown` 三者的互動要讀完整份才看得懂 |
| `add`/`sub` 的條件編譯不對稱（§5.2） | — | `used_bytes` 在單開一個旗標時語意失真 |
| wrapper 目錄註解掉 | 少建置兩個沒人用的 `.so` | 程式碼持續腐化風險——它現在不會被編譯，任何破壞相容的改動（例如介面加一個純虛方法）都不會被發現 |

最後一項值得展開。`MemoryAllocatorInterface` 若新增一個純虛方法，`JemallocLib.cc` 與 `MimallocLib.cc` 會變成抽象類別而無法實體化——但因為它們不參與建置，**編譯不會報錯**。等到某天有人把 `add_subdirectory` 取消註解，才會發現這兩個檔案早已過時。這與 `TaosClient` 的下場是同一條路徑，只是目前還沒走到。

---

## 11. 檔案索引

| 檔案 | 行數 | 職責 |
|---|---|---|
| `src/memory/CMakeLists.txt` | 6 | 放寬符號可見性（`:1-2`）；只加入 `common` 子目錄，**jemalloc 與 mimalloc 兩行被註解掉**（`:5-6`） |
| `src/memory/common/CMakeLists.txt` | 1 | `target_add_lib(memory-common common)`——編成靜態庫供各 binary 連結 |
| `src/memory/common/MemoryAllocatorInterface.h` | 20 | 五方法純虛介面 + `getMemoryAllocator` 函式名巨集 + 函式指標型別 |
| `src/memory/common/GlobalMemoryAllocator.h` | 47 | 全域配置函式的宣告（覆寫時）與 inline 空殼實作（不覆寫時，轉呼 `std::malloc`／`posix_memalign`） |
| `src/memory/common/GlobalMemoryAllocator.cc` | 196 | **核心**。`dlopen` 載入邏輯、`allocate`／`deallocate`／`memalign` 的標頭處理與計數、`logstatus`／`profiling`／`shutdown` 轉發。整份被 `#ifdef OVERRIDE_CXX_NEW_DELETE` 包住 |
| `src/memory/common/OverrideCppNewDelete.h` | 19 | 在標頭裡**定義**全域 `operator new/delete/new[]/delete[]`，只能被單一 TU include |
| `src/memory/common/AllocatedMemoryCounter.h` | 63 | 計數器類別。20 個 2 冪分桶的 `calcBucket`／`calcBucketSize`、`add`／`sub` 熱路徑 |
| `src/memory/common/AllocatedMemoryCounter.cc` | — | 七組 Recorder 的建立、環境變數解析（`MEMORY_METRIC_REPORT_INTERVAL`／`MEMORY_METRIC_THREAD_COUNTER`）、三層遞迴防護的 `tryReport` |
| `src/memory/common/MemoryAllocatorConfig.h` | 11 | 兩個熱更新設定項：`prof_prefix`、`prof_active`（線上開關 heap profiling） |
| `src/memory/jemalloc/CMakeLists.txt` | 1 | `target_add_shared_lib(jemalloc_wrapper hf3fs_jemalloc)`——**目前不會被執行** |
| `src/memory/jemalloc/JemallocLib.cc` | — | jemalloc 實作。`logstatus` 讀六個 `mallctl` 統計量，附大段官方文件原文註解 |
| `src/memory/mimalloc/CMakeLists.txt` | 1 | `target_add_shared_lib(mimalloc_wrapper mimalloc-static)`——**目前不會被執行** |
| `src/memory/mimalloc/MimallocLib.cc` | 53 | mimalloc 實作。轉呼 `mi_malloc`／`mi_free`／`mi_memalign`；`logstatus` 回報 rss／commit／page_faults；`profiling` 不支援但回傳 true |

---

## 12. 延伸閱讀

- 指標怎麼被收集與落地：`monitor_collector_main` 報告（Recorder 家族、`reset` 參數語意、TagSet cardinality）
- 熱更新設定怎麼下發：`hf3fs_common_shared` 報告（ConfigBase 巨集展開、`CONFIG_HOT_UPDATED_ITEM`）與 `admin_cli` 報告（`hot-update-config` 命令）
- 另一個「宣告完整但不參與建置」的案例：`monitor_collector_main` 報告 §8.4 的 `TaosClient`
- 實際生效的 jemalloc 連結方式：`cmake/Jemalloc.cmake`，由 `meta_main`／`storage_main` 使用
