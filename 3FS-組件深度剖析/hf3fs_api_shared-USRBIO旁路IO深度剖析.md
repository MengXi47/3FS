# hf3fs_api_shared（USRBIO 使用者態旁路 IO）深度剖析

> 對應原始碼：`src/lib/api/`（公開 API 與實作）、`src/lib/common/`（共享記憶體原語）、`src/lib/py/`（Python 綁定）、`src/lib/rs/hf3fs-usrbio-sys/`（Rust FFI）
> 關鍵對側：`src/fuse/IoRing.*`、`src/fuse/IovTable.*`、`src/fuse/PioV.*`、`src/fuse/FuseClients.cc`、`src/fuse/FuseOps.cc`
> 建置目標：`src/lib/api/CMakeLists.txt:2` → `target_add_shared_lib(hf3fs_api_shared client-lib-common storage-client numa rt)`

---

## 0. 一句話總結

USRBIO 不是「另一個檔案系統客戶端」，它是**把 FUSE 進程裡已經存在的那條 `PioV → StorageClient → RDMA` 資料路徑，透過一塊共享記憶體環直接開放給使用者進程呼叫**。使用者進程完全不需要連 mgmtd／meta／storage，也不需要懂 CRAQ 與 chain table；它只做三件事：把資料放進一塊 FUSE 進程已經 `ibv_reg_mr` 過的共享記憶體（Iov）、把「buf 位移 + inode id + 檔案位移 + 長度」寫進一個 io_uring 形狀的共享環（Ior）、然後等 cqe。**Iov / Ior 的建立、銷毀與喚醒沒有新增任何 RPC，也不使用 ioctl**，而是全部復用「在 `3fs-virt/iovs/` 這個虛擬目錄裡建立／刪除 symlink」這一個動作——symlink 的**檔名本身就是參數編碼**，symlink 的**目標就是 `/dev/shm` 路徑**；`src/lib/api/fuse.h:44-56` 的 12 個 `HF3FS_IOC_*` 沒有任何一個是為 USRBIO 新增的。

但要精確：USRBIO 的流程中仍有兩處必經的**既有** ioctl。`hf3fs_reg_fd` / `hf3fs_dereg_fd` 第一件事就是呼叫 `hf3fs_is_hf3fs`（`UsrbIo.cc:34-43`），後者用 `HF3FS_IOC_GET_MAGIC_NUM` 判斷 fd 是否落在 3FS 掛載點上（見 §3.6）；寫入後要讓 `stat` 看到正確長度，得用 `HF3FS_IOC_FSYNC`（`fuse.h:51`，Python 綁定以 `force_fsync` 暴露，見 §12.2）。

---

## 1. 整體分層與產出物

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ 使用者程式（fio plugin / PyTorch dataloader / Rust 服務）                       │
├──────────────────────────────────────────────────────────────────────────────┤
│ 語言綁定                                                                       │
│   src/lib/py/usrbio_binding.cc     → pybind11 模組 hf3fs_py_usrbio            │
│   hf3fs_fuse/io.py                 → Python 薄封裝（make_iovec/make_ioring）  │
│   src/lib/rs/hf3fs-usrbio-sys/     → bindgen + 安全包裝（Iov/Ior/RegisteredFd）│
├──────────────────────────────────────────────────────────────────────────────┤
│ C ABI 公開介面   src/lib/api/hf3fs_usrbio.h   （唯一需要對外散佈的 header）      │
│ 實作             src/lib/api/UsrbIo.cc        （807 行，整個共享庫的本體）      │
├──────────────────────────────────────────────────────────────────────────────┤
│ 共用原語         src/lib/common/Shm.h/.cc     ShmBuf：shm_open+mmap+IB 註冊    │
│                  src/lib/common/PerProcTable.h AllProcMap（fork 感知的表）     │
├──────────────────────────────────────────────────────────────────────────────┤
│ ★ 與 FUSE 進程共用的環定義（同一份 header，兩個進程各自編譯一份）                 │
│                  src/fuse/IoRing.h            IoArgs/IoSqe/IoCqe/IoRing        │
└──────────────────────────────────────────────────────────────────────────────┘
```

最值得注意的一點在最底層：**`src/lib/api/UsrbIo.cc:13` 直接 `#include "fuse/IoRing.h"`**。也就是說使用者態共享庫與 FUSE 服務端**共用同一個 `IoRing` class 定義**，只是以不同的 `owner` 旗標實例化（`UsrbIo.cc:398` 傳 `false`，`IoRing.h:87` 預設 `true`）。這代表：

- 環的記憶體佈局不需要另外定義一份「ABI 結構」，兩側天然一致；
- 但也代表 **FUSE 進程與使用者程式必須用同一版本的 3FS 編譯**，因為 `sizeof(IoArgs)` 之類一改就是靜默的記憶體錯位。程式碼裡沒有任何版本協商欄位。

CMake 同時產出靜態與動態兩份（`src/lib/api/CMakeLists.txt:1-2`），並在建置後把 `.so` symlink 到 Rust 綁定的 `lib/` 目錄：

```cmake
target_add_lib(hf3fs_api client-lib-common storage-client numa rt)
target_add_shared_lib(hf3fs_api_shared client-lib-common storage-client numa rt)

add_custom_command(
    TARGET hf3fs_api_shared POST_BUILD
    COMMAND ${CMAKE_COMMAND} -E create_symlink
      "${CMAKE_CURRENT_BINARY_DIR}/libhf3fs_api_shared.so"
      "${CMAKE_SOURCE_DIR}/src/lib/rs/hf3fs-usrbio-sys/lib/libhf3fs_api_shared.so"
    COMMENT "linking usrbio library to rust binding lib dir")
```

`target_add_shared_lib`（`cmake/Target.cmake:26-38`）是 `file(GLOB_RECURSE ... "*.cc" "*.h")` 掃整個目錄，所以 `src/lib/api/` 底下多放一個 `.cc` 就會自動進 `.so`。連結項 `numa`（NUMA 綁定）與 `rt`（POSIX 具名 semaphore／shm）是 USRBIO 特有的兩個依賴，後文會逐一指出用在哪。

注意這個 `.so` **並不小**：它連結了 `storage-client`，也就是整個 StorageClient、folly、RDMA 相關的程式碼都會被拉進來。但使用者進程實際上**一次都不會呼叫 StorageClient**——`UsrbIo.cc` 用到的只有 `IoRing` 的 inline 方法與 `lib::ShmBuf`，而 `ShmBuf` 的建構子恰好又相依 `storage::client::IOBuffer`（`src/lib/common/Shm.h:73`）。這是連結相依而非執行期相依。

---

## 2. 為什麼需要 USRBIO：FUSE 那條路到底貴在哪

`src/lib/api/UsrbIo.md:4` 的官方說法是「bypassing certain limitations inherent to FUSE itself... avoids the maximum single I/O size restriction」。這個說法不完整。把 `FuseOps.cc` 的 read 實作攤開來看，一次 4 MB 的 `pread()` 在 FUSE 路徑上要付的帳是：

`src/fuse/FuseOps.cc:1473-1549`：

```cpp
void hf3fs_read(fuse_req_t req, fuse_ino_t fino, size_t size, off_t off, struct fuse_file_info *fi) {
  ...
  auto memh = IOBuffer(folly::coro::blocking_wait(d.bufPool->allocate()));   // 1511
  ...
  std::vector<ssize_t> res(1);
  PioV ioExec(*d.storageClient, config.chunk_size_limit(), res);
  auto retAdd = ioExec.addRead(0, inode, 0, off, size, memh.data(), memh);   // 1526
  auto retExec = withRequestInfo(req, ioExec.executeRead(userInfo, ...));    // 1532
  ioExec.finishIo(true);
  ...
  fuse_reply_buf(req, (char *)memh.data(), res[0]);                          // 1548
}
```

逐項拆帳：

1. **每個請求兩次 context switch + 兩次資料搬移**。使用者 `read()` → 核心 → FUSE 裝置 → `hf3fs_read` 回呼；資料落在 `bufPool` 的 RDMA buffer 裡，`fuse_reply_buf` 再把它 copy 回核心，核心再 copy 到使用者 buffer。USRBIO 的資料**從頭到尾只存在於 Iov 這一塊 shm**，storage server 的 RDMA write 直接落在使用者進程看得到的位址上（`IoRing.cc:172` 把 `bufs[i]->ptr()` 當成 `addRead` 的目的地，那個指標就是 `shm->bufStart + bufOff`，見 `Shm.h:83-86`）。零 copy 是真的零。

2. **單請求大小被 FUSE 與 buffer pool 雙重卡死**。`FuseClients.cc:86` 用 `fuseConfig.io_bufs().max_buf_size()`（預設 `1_MB`，`FuseConfig.h:74`）建 `RDMABufPool`，所以就算核心願意給更大的 read，FUSE 端也只有 1 MB 的 buffer。USRBIO 側完全沒有這個限制：唯一的長度檢查是「這段 IO 必須整個落在 Iov 內」（`UsrbIo.cc:626-628`）。

3. **一個請求佔住一條 FUSE 工作執行緒直到完成**。`blocking_wait`（1511）與 `withRequestInfo(...)`（1532）都是同步阻塞。`maxThreads` 被夾在 `min(max_threads, (邏輯核心數+1)/2)`（`FuseClients.cc:80-85`），所以並發度上限就是那幾百條執行緒，且每條執行緒等的是一次網路往返。USRBIO 走的是**協程池 + 批次**：`batch_io_coros` 預設 128 條協程（`FuseConfig.h:46`），每條協程一次處理一整個 batch（`IoRing::process` 的 `toProc` 個 IO），`PioV::executeRead` 把整批打成一次 `batchRead`（`PioV.cc:139`）。1000 個 4 KB 隨機讀在 FUSE 路徑上是 1000 次獨立往返，在 USRBIO 是 1 次 `batchRead`。

4. **每個請求都要重新查 inode、重新拿 buffer**。USRBIO 把這兩件事都前置了：inode 在 `hf3fs_reg_fd` 時就換成了 `InodeId`（`UsrbIo.cc:585`），buffer 在 `hf3fs_iovcreate` 時就完成了 `ibv_reg_mr`（`IovTable.cc:223` → `Shm.cc:103`）。跑批的時候這兩件事的成本都是零。

USRBIO **沒有**省掉的部分（很重要，決定了它的適用邊界）：

- meta 操作完全沒省。`open`/`stat`/`unlink` 還是走 FUSE。USRBIO 只接管 `pread`/`pwrite` 這兩個動作。
- 寫入的長度更新沒省。`IoRing.cc:162-168` 每個寫 IO 前都要 `beginWrite`（可能觸發 `extendStripe` 的 meta RPC），寫完 `finishWrite`（`IoRing.cc:218`）把 inode 塞進 `dirtyInodes`，仍由 `periodicSync` 背景寫回 meta。
- FUSE 進程還是在資料路徑上。它不是「kernel bypass」，是「**FUSE 協定 bypass**」——資料不經核心，但控制仍在 FUSE 進程的執行緒池裡。

---

## 3. 完整公開 API 表

`src/lib/api/hf3fs_usrbio.h` 是唯一需要散佈的 header（173 行，純 C，`extern "C"` 包住）。下表逐個函式列出簽章、語意、回傳值與**呼叫順序約束**。

### 3.1 型別

```c
struct hf3fs_iov {          // hf3fs_usrbio.h:18-27
  uint8_t *base;            // mmap 後的起始位址，使用者直接讀寫這裡
  hf3fs_iov_handle iovh;    // 實際是 hf3fs::lib::ShmBuf*；iovwrap 出來的是 nullptr
  char id[16];              // Uuid，FUSE 端用它反查 shm
  char mount_point[256];    // 掛載點字串（複製一份進來，故不可超過 255 字元）
  size_t size;
  size_t block_size;        // 0 = 整塊一次註冊；>0 = 分段註冊，且 IO 不可跨段
  int numa;                 // -1 = 不綁
};

struct hf3fs_ior {          // hf3fs_usrbio.h:32-49
  struct hf3fs_iov iov;     // ★ ior 內嵌一個 iov：環本身也是一塊 shm
  hf3fs_ior_handle iorh;    // 實際是 Hf3fsIorHandle*（IoRing + submit semaphore）
  char mount_point[256];
  bool for_read;            // 一個環只能單向
  int io_depth;             // 批次策略，見 §6.3
  int priority;             // <0 高、0 普通、>0 低
  int timeout;              // 毫秒；io_depth<0 時的湊批等待上限
  uint64_t flags;
};

struct hf3fs_cqe {          // hf3fs_usrbio.h:51-56
  int32_t index;            // 對應 hf3fs_prep_io() 的回傳值
  int32_t reserved;
  int64_t result;           // >=0 實際位元組數；<0 為 -errno
  const void *userdata;
};
```

`hf3fs_cqe` 與 FUSE 內部的 `IoCqe`（`src/fuse/IoRing.h:34-39`）**欄位完全一致**，這不是巧合——`hf3fs_wait_for_ios` 是逐欄位手抄過去的（`UsrbIo.cc:718-720`），沒有直接 `memcpy` 只是為了留一層轉換餘地。

### 3.2 探測與掛載點

| 函式 | 簽章 | 回傳 | 說明 |
|---|---|---|---|
| `hf3fs_is_hf3fs` | `bool (int fd)` | true/false | `ioctl(fd, HF3FS_IOC_GET_MAGIC_NUM)` 後比對 `0x8f3f5fff`。`UsrbIo.cc:34-43` |
| `hf3fs_extract_mount_point` | `int (char *out, int size, const char *path)` | 掛載點長度+1；`-1` 表示不在 3FS 上 | 掃 `/proc/self/mountinfo`。`UsrbIo.cc:45-107` |

`hf3fs_extract_mount_point` 有兩個非顯而易見的行為（`UsrbIo.cc:63-91`）：

```cpp
// there are two kinds of mounts:
// 1. directly mount the whole hf3fs
// 2. mount some subdirectories separately
// for case 1, we find the root of the fs directly in the mountinfo
// for case 2, we find the parent of the 3fs-virt dir
```

它認的不是「檔案系統型別是 fuse.hf3fs」而是「這個掛載點底下有沒有 `3fs-virt` 目錄」，因為 bind-mount 子目錄的情況下 mountinfo 的 fstype 欄位不會是 `fuse.hf3fs`。環境變數 `HF3FS_USRBIO_DONT_CHECKFS_FOR_MP=yes` 可以完全跳過 fstype 檢查（`UsrbIo.cc:46-47`）。另外它是**反向迭代** `std::set<path>`（`UsrbIo.cc:95`），也就是字典序由大到小，因此巢狀掛載時會優先命中最深（最長）的掛載點。

回傳值語意要小心：成功時回傳的是 `mp.size() + 1`，且**只有在 `mp.size() < size` 時才真的寫入 buffer**（`UsrbIo.cc:99-102`）。所以正確用法是「回傳值 > size 就代表 buffer 太小」。

### 3.3 Iov 生命週期

| 函式 | 回傳 | 語意 |
|---|---|---|
| `hf3fs_iovcreate(iov, mp, size, block_size, numa)` | `0` / `-errno` | 建立 `/dev/shm/hf3fs-iov-<uuid>`、mmap、在 `<mp>/3fs-virt/iovs/` 建 symlink 註冊給 FUSE |
| `hf3fs_iovopen(iov, id[16], mp, size, block_size, numa)` | `0` / `-errno` | 以既有 uuid 開啟別人建立的 iov（跨進程共用同一塊 buffer） |
| `hf3fs_iovwrap(iov, base, id[16], mp, size, block_size, numa)` | `0` / `-EINVAL` | 只填 struct，不建 shm、**不建 symlink**（symlink 要呼叫者自己建） |
| `hf3fs_iovunlink(iov)` | void | 只 `shm_unlink`，不動 symlink 也不 munmap |
| `hf3fs_iovdestroy(iov)` | void | 刪 symlink + `shm_unlink` + munmap + `delete ShmBuf` |

`hf3fs_iovwrap` 的存在理由寫在 header 註解（`hf3fs_usrbio.h:82-86`）：使用者可能已經有一塊 shm（例如 Python 的 `multiprocessing.shared_memory.SharedMemory`），不想再多配一塊。代價是**它不負責註冊**——Python 綁定必須自己 `os.symlink`（`hf3fs_fuse/io.py:58-64`），Rust 綁定也是（`src/lib/rs/hf3fs-usrbio-sys/src/lib.rs:39-45`）。而且 `iovwrap` 出來的 iov 的 `iovh == nullptr`，`hf3fs_iovdestroy` 會直接印錯誤並拒絕處理（`UsrbIo.cc:196-201`）。

`hf3fs_iovwrap` 是唯一在使用者側直接呼叫 `numa` 函式庫的地方（`UsrbIo.cc:320-322`）：

```cpp
if (numa >= 0) {
  numa_tonode_memory(buf, size, numa);
}
```

`hf3fs_iovcreate` 路徑上的 NUMA 綁定則發生在 `ShmBuf` 建構子裡（`src/lib/common/Shm.cc:28-30`），同樣是 `numa_tonode_memory`。這就是 CMake 連結 `numa` 的全部理由——只有這兩處。注意 `numa_tonode_memory` 是「建議」而非「保證」：它對已經 fault-in 的頁不會搬動，而 `mmap` 剛回來的頁還沒 fault-in，所以放在 mmap 之後是正確時機。

### 3.4 Ior 生命週期

四個 `iorcreate` 變體是純粹的相容性堆疊（`UsrbIo.cc:422-516`），呼叫鏈是 `iorcreate → iorcreate2 → iorcreate3`，而 `iorcreate4` 是獨立分支：

```
hf3fs_iorcreate (entries, for_read, io_depth, numa)
   └─► hf3fs_iorcreate2 (+priority=0)
          └─► hf3fs_iorcreate3 (+timeout=0)
                 └─► hf3fs_iovcreate_general(is_io_ring=true, ...) + hf3fs_iorwrap(flags=0)

hf3fs_iorcreate4 (entries, for_read, io_depth, timeout, numa, flags)
   └─► hf3fs_iovcreate_general(is_io_ring=true, priority=0, ...) + hf3fs_iorwrap(priority=0, flags)
```

**`iorcreate4` 沒有 priority 參數，內部硬編 0**（`UsrbIo.cc:500`、`508`）。也就是說「要 flags 就沒有 priority，要 priority 就沒有 flags」——這兩組參數在公開 API 上無法同時指定。Python 綁定走的是 `iorcreate4`，所以它的 `priority` 參數被明確丟棄（`usrbio_binding.cc:248`：`(void)priority;`）。

| 函式 | 回傳 | 錯誤條件 |
|---|---|---|
| `hf3fs_ior_size(entries)` | 位元組數 | 純計算，`= IoRing::bytesRequired(entries)` |
| `hf3fs_iorcreate*` | `0` / `-errno` | `-EINVAL`：ior 為 NULL、掛載點空、`timeout < 0`、掛載點過長；`-EIO`：shm 建立失敗；`-errno`：symlink 失敗、readlink/sem_open 失敗 |
| `hf3fs_iordestroy(ior)` | void | 對 NULL 安全 |
| `hf3fs_io_entries(ior)` | `int` | `= IoRing::ioRingEntries(ior->iov.size)`，即可用槽位數 |

還有一個**沒有出現在公開 header 裡**的函式：`hf3fs_iorwrap`（`UsrbIo.cc:372-420`）。header 第 17 行的註解明明寫著「if you already has a shared buffer, skip hf3fs_iovwrap() and go for hf3fs_iorwrap() directly」，但那行宣告從未加進 header。它是一個有外部連結的符號（非 `static`），所以硬要用可以自己宣告，但這顯然是文件與實作脫節。`hf3fs_iovcreate_general` / `hf3fs_iovdestroy_general` 同樣是未宣告但可連結的符號。

### 3.5 fd 註冊

| 函式 | 回傳 | 說明 |
|---|---|---|
| `hf3fs_reg_fd(int fd, uint64_t flags)` | **成功回傳 `-dupfd`（≤0）**；**失敗回傳正的 `errno`** | `flags` 目前完全未使用（`UsrbIo.cc:559`：`(void)flags;`） |
| `hf3fs_dereg_fd(int fd)` | void | 同時清掉 fd 與 dupfd 兩個表項 |

這個回傳值約定極容易踩雷：**成功是負數或 0，失敗是正數**，與這個 API 其他所有函式（成功 0、失敗 `-errno`）恰好相反。header 也明說了（`hf3fs_usrbio.h:136`）：`// <= 0 for io-preppable file handle, errno for error`。Python 綁定的判斷是 `if (res > 0) throw`（`usrbio_binding.cc:66-68`），Rust 綁定則**完全忽略回傳值**（`lib.rs:215`）。

### 3.6 IO 提交與收割

```c
int hf3fs_prep_io(const struct hf3fs_ior *ior, const struct hf3fs_iov *iov,
                  bool read, void *ptr, int fd, size_t off, uint64_t len,
                  const void *userdata);
int hf3fs_submit_ios(const struct hf3fs_ior *ior);
int hf3fs_wait_for_ios(const struct hf3fs_ior *ior, struct hf3fs_cqe *cqes,
                       int cqec, int min_results, const struct timespec *abs_timeout);
```

| 函式 | 回傳 | 錯誤 |
|---|---|---|
| `hf3fs_prep_io` | `≥0` = 槽位索引（等於後續 cqe 的 `index`） | `-EINVAL`（參數不合／方向不符／`[ptr,ptr+len)` 越出 iov／`len==0`）、`-EBADF`（fd 未註冊）、`-EACCES`（開檔模式與讀寫方向衝突）、`-EAGAIN`（環滿） |
| `hf3fs_submit_ios` | `0` | `-EINVAL`（ior 或 iorh 為 NULL） |
| `hf3fs_wait_for_ios` | `≥0` = 取得的 cqe 數（**可能少於就緒數，需再呼叫**） | `-EINVAL`（`cqec <= 0` 或 ior 為 NULL） |

`hf3fs_prep_io` 的參數檢查一次寫在 `UsrbIo.cc:626-628`：

```cpp
if (!ior || !ior->iorh || read != ior->for_read || !iov || len <= 0 || !iov->base ||
    p < iov->base || p + len > iov->base + iov->size || afd >= (int)regfds.size()) {
  return -EINVAL;
}
```

注意這裡**沒有任何對齊檢查**——`ptr`、`off`、`len` 都不需要對齊 4 KB 或任何值。這與 `O_DIRECT` 的世界觀不同，因為資料路徑是 RDMA 而非 block layer。

呼叫順序約束（違反的後果都是靜默錯誤而非報錯）：

```
hf3fs_iorcreate*  ──┐
hf3fs_iovcreate   ──┤  兩者順序無所謂，但都必須早於 prep_io
open() + hf3fs_reg_fd ─┘

  ┌──────────────────────────────────────────┐
  │ hf3fs_prep_io  × N     ← 單一執行緒！      │
  │ hf3fs_submit_ios       ← 只是提示，可省略  │
  │ hf3fs_wait_for_ios     ← 可以是另一條執行緒│
  └──────────────────────────────────────────┘

hf3fs_dereg_fd → close()          ← 順序不可反
hf3fs_iovdestroy / hf3fs_iordestroy
```

`hf3fs_dereg_fd` 必須在 `close()` 之前，因為它自己會 `hf3fs_is_hf3fs(fd)` 做 ioctl（`UsrbIo.cc:599`），fd 關了就變成 `EBADF` 而直接 return，登記項就此洩漏。Python 文件也特別警告了這點（`usrbio_binding.cc:84`：「文件如果已被注册，则在 close 前必须取消注册，否则可能导致后续其他文件读取错误」）。

### 3.7 兩個搭便車的 ioctl 包裝

```c
int hf3fs_hardlink(const char *target, const char *link_name);   // UsrbIo.cc:772
int hf3fs_punchhole(int fd, int n, const size_t *start, const size_t *end, size_t flags); // UsrbIo.cc:794
```

這兩個與 USRBIO 無關，只是剛好放在同一個 `.so` 裡。它們走的是 `ioctl(HF3FS_IOC_HARDLINK / HF3FS_IOC_PUNCH_HOLE)`（定義於 `src/lib/api/fuse.h:52-53`），是 3FS 對 POSIX 缺口的補丁：FUSE 的 `link()` 在 3FS 的實作裡走不通，所以改用 ioctl 帶著 `{parent ino, name}` 進去。它們的回傳值又是另一套約定：**成功 0、失敗回傳正的 `errno`**。`punch_hole` 一次最多 1000 個區間（`fuse.h:12`）。

---

## 4. Iov：共享記憶體資料緩衝區

### 4.1 建立：一條 symlink 就是一次 RPC

整個 USRBIO 控制平面最巧妙的設計在 `hf3fs_iovcreate_general`（`UsrbIo.cc:109-183`）：

```cpp
auto p = fmt::format("/hf3fs-iov-{}", hf3fs::Uuid::random());                    // 129
shm = new hf3fs::lib::ShmBuf(p, size, block_size, numa,
                             hf3fs::meta::Uid(getuid()), getpid(), getppid());   // 133

auto target = hf3fs::Path("/dev/shm") / p;
auto link = fmt::format("{}/3fs-virt/iovs/{}{}{}{}{}{}",
                        hf3fs_mount_point,
                        shm->id.toHexString(),                                   // uuid
                        block_size ? fmt::format(".b{}", block_size) : "",       // 區塊大小
                        is_io_ring ? fmt::format(".{}{}", for_read?'r':'w', io_depth) : "",
                        is_io_ring && priority != 0 ? fmt::format(".p{}", priority<0?'h':'l') : "",
                        is_io_ring ? fmt::format(".t{}", timeout) : "",
                        is_io_ring && flags != 0 ? fmt::format(".f{:b}", flags) : "");
auto lres = symlink(target.c_str(), link.c_str());                               // 155
```

也就是說，**一個 `symlink(2)` 系統呼叫就完成了「請 FUSE 進程 mmap 這塊 shm、註冊給 RDMA、建立 IoRing、掛進表裡」這一整套操作**，參數靠檔名編碼。範例檔名：

```
3f2a8c1e5d4b47a9b0e6c9f13a7d2b88.b1073741824.r0.t1.f10
└──────────── uuid (32 hex) ───────────┘ │      │  │  └ flags=0b10=2（FORBID_READ_HOLES）
                                         │      │  └─ timeout=1 ms
                                         │      └──── 讀環，io_depth=0
                                         └─────────── block_size=1 GiB
```

FUSE 端的解析在 `IovTable.cc:28-111` 的 `parseKey()`，是一個 `folly::split('.', key, parts)` 之後的 `switch (dec[0])`。這裡有一條反向檢查：`.t` / `.f` / `.p` 這些屬性只允許出現在 io ring 上，否則 `kInvalidArg`（`IovTable.cc:106-108`）。

FUSE 端的入口是 `hf3fs_symlink` 這個 FUSE 回呼（`FuseOps.cc:1215-1246`）：

```cpp
} else if (*dname == "iovs") {
  auto res = d.iovs.addIov(name, Path(link), pid, userInfo,
                           &d.client->tpg().bgThreadPool().randomPick(), *d.storageClient);
  ...
  auto &[inode, ior] = *res;
  if (ior) {                                        // 是 io ring 才走這裡
    auto res2 = d.iors.addIoRing(..., ior->bufStart, ior->size, ior->ioDepth, *ior->iora);
    res->second->iorIndex = *res2;                  // 記下索引以便日後移除
  }
```

`IovTable::addIov`（`IovTable.cc:124-242`）依序做：`parseKey` → `stat` shm 檔（必須是 regular file，`IovTable.cc:145`）→ 用 `ShmBuf(path, 0, st_size, blockSize, id)` 這個「非 owner」建構子（`Shm.cc:33-52`）mmap → `iovs->alloc()` 拿槽位 → `registerForIO` 做 IB 註冊 → 記進 `iovds_[key]` 與 `shmsById[uuid]` 兩張表。

**io ring 不做 IB 註冊**（`IovTable.cc:222`）：

```cpp
if (!iovaRes->isIoRing) {  // io ring bufs don't need to be registered for ib io
  folly::coro::blockingWait(shm->registerForIO(exec, sc, recordMetrics));
}
```

理由很直白：環裡跑的是控制訊息，資料從來不經過它，沒有 RDMA 的必要。

### 4.2 記憶體從哪來：`shm_open` + `mmap`，不是 hugepage

`ShmBuf::mapBuf()`（`Shm.cc:149-175`）：

```cpp
auto fd = shm_open(path.c_str(), O_RDWR | (owner_ ? O_CREAT | O_EXCL : 0), 0666);
...
if (owner_) { ftruncate(fd, size); }
bufStart = (uint8_t *)mmap(nullptr, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off);
```

三個決定值得注意：

- **`/dev/shm` 上的 tmpfs，不是 hugetlbfs**。程式碼裡沒有任何 `MAP_HUGETLB` 或 `madvise(MADV_HUGEPAGE)`。要用大頁只能靠系統的 THP，或是自己 mmap 大頁記憶體後走 `hf3fs_iovwrap`。
- **`O_EXCL`**：owner 建立時若 uuid 撞名直接失敗，不會靜默接管別人的 shm。
- **`0666` 權限**：任何使用者都能開這個 shm 檔。真正的存取控制發生在 FUSE 端——`IovTable::statIov` 檢查 `shm->user != ui.uid` 才回 `kNoPermission`（`IovTable.cc:278-282`），且 `listIovs` 只列出 `iov->user == ui.uid` 的項目（`IovTable.cc:323`）。也就是說**別人的 iov 你在 `3fs-virt/iovs` 裡看不到也 stat 不到，但如果你猜得到 uuid，`/dev/shm` 上那個檔案是可讀寫的**。

`fd` 在 mmap 之後立刻 `close`（`Shm.cc:161` 的 `SCOPE_EXIT`），mapping 本身持有引用。

### 4.3 `block_size`：為了迴避 IB 註冊的長度問題

`ShmBuf` 建構子把 `blockSize` 為 0 的情況正規化成「整塊一段」（`Shm.cc:13`：`blockSize(bsz ? bsz : sz)`），並據此配置 `memhs_` 陣列（`Shm.cc:21`）。註冊時逐段呼叫：

```cpp
for (size_t i = 0; i < memhs_.size(); ++i) {
  auto res = sc.registerIOBuffer(bufStart + blockSize * i,
                                 std::min(size - blockSize * i, blockSize));   // Shm.cc:103
```

Python 文件把原因說得最清楚（`hf3fs_fuse/io.py:55`）：「系统会按照 block_size 按块分配内存，防止触发 IB 注册驱动问题」。`UsrbIo.md:84` 的說法是「This parameter is for optimization on IB register time」。兩者都對——超大單段 MR 的註冊延遲很糟，某些驅動甚至會失敗。

代價寫在 `ShmBufForIO::memh()`（`Shm.h:87-94`）：

```cpp
CoTryTask<storage::client::IOBuffer *> memh(size_t len) const {
  if (len && off_ / buf_->blockSize != (off_ + len - 1) / buf_->blockSize) {
    co_return makeError(StatusCode::kInvalidArg);
  }
  co_return (co_await buf_->memh(off_)).get();
}
```

**單一 IO 不得跨越 block 邊界**，否則整個 IO 以 `kInvalidArg` 失敗（會反映成 cqe 的負 result）。這個檢查在 FUSE 端才做，使用者端的 `hf3fs_prep_io` 不檢查，所以是一個「提交時看起來成功、收割時才發現失敗」的陷阱。

註冊是非同步的：`registerForIO` 把工作丟到背景執行緒池（`Shm.cc:121-125`），用 `folly::coro::Baton memhBaton_` 同步。`memh(off)` 若發現該段還沒註冊好就 `co_await memhBaton_`（`Shm.cc:140-147`）。所以第一批 IO 可能會等 IB 註冊完成，之後就是純查表。

### 4.4 生命週期與清理：三道保險

Iov 的清理是這個設計裡防禦最深的部分，一共三層：

**第一層 — 正常路徑**。`hf3fs_iovdestroy` → `hf3fs_iovdestroy_general`（`UsrbIo.cc:185-222`）重建同一個 symlink 檔名並 `unlink`，FUSE 端 `hf3fs_unlink` 回呼（`FuseOps.cc:1053-1069`）呼叫 `rmIov`，接著 `delete ShmBuf` 觸發 munmap + `shm_unlink`。

**第二層 — io ring 立刻 unlink shm**。`UsrbIo.cc:160-162`：

```cpp
if (is_io_ring) {
  shm->maybeUnlinkShm();
}
```

symlink 一建好、FUSE 已經 mmap 完成，馬上把 `/dev/shm` 上的檔名刪掉。此後兩個進程都還持有 mapping（POSIX 語意），但**檔案系統上再也看不到它**。任一進程崩潰，核心自動回收，不會在 `/dev/shm` 留下垃圾。一般 iov 不這麼做，因為 `hf3fs_iovopen` 需要靠這個路徑名再開一次（`UsrbIo.cc:241` 的 `realpath`）。

**第三層 — 進程死亡偵測（最巧妙的一招）**。`UsrbIo.cc:175-180`：

```cpp
std::lock_guard lock(alive.mtx);
if (alive.mountFds.find(hf3fs_mount_point) == alive.mountFds.end()) {
  auto fd = open(fmt::format("{}/3fs-virt/iovs", hf3fs_mount_point).c_str(), O_DIRECTORY);
  alive.mountFds[hf3fs_mount_point] = fd;
  XLOGF(INFO, "fd {} for mount {}", fd, hf3fs_mount_point);
}
```

第一次建 iov 時，對 `3fs-virt/iovs` 這個虛擬目錄開一個 `O_DIRECTORY` 的 fd，然後**永遠不關**。這個 fd 的唯一用途是：進程死掉時核心會關閉它，FUSE 收到 `releasedir`，於是：

`FuseOps.cc:1816-1830`：

```cpp
if (dh->iovDir) {
  // releasedir() is called only the last process with the inherited fd closes it or exits
  auto &iovs = *d.iovs.iovs;
  auto n = iovs.slots.nextAvail.load();
  for (int i = 0; i < n; ++i) {
    auto iov = iovs.table[i].load();
    if (iov && iov->pid == dh->pid) {
      XLOGF(INFO, "unlinking iov {} symlink from dead pid {}", iov->key, dh->pid);
      d.iovs.rmIov(iov->key.c_str(), ...);
      if (iov->isIoRing) { d.iors.rmIoRing(iov->iorIndex); }
    }
  }
}
```

`opendir` 時就標記了 `iovDir` 與 `pid`（`FuseOps.cc:1782-1787`）。這是一個**用 FUSE 的 `releasedir` 語意當作進程存活探針**的做法，等價於 io_uring 靠 fd 生命週期做清理，但這裡沒有 fd 可以綁，只好綁一個目錄。

註解裡那句「releasedir() is called only the last process with the inherited fd closes it or exits」也直接回答了 fork 的問題：**fork 之後子進程繼承這個 fd，因此只要還有任何一個子進程活著，父進程的 iov 就不會被回收**。

---

## 5. IoRing：記憶體佈局與索引協定

### 5.1 尺寸計算

`src/fuse/IoRing.h:55-71`：

```cpp
static int ringMarkerSize() {
  auto n = std::atomic_ref<int32_t>::required_alignment;
  return (4 + n - 1) / n * n;                                  // x86-64 上 = 4
}
static int ioRingEntries(size_t bufSize) {
  auto n = ringMarkerSize();
  return (int)std::min((size_t)std::numeric_limits<int>::max(),
                       (bufSize - 4096 - n * 4 - sizeof(sem_t)) /
                       (sizeof(IoArgs) + sizeof(IoCqe) + sizeof(IoSqe))) - 1;
}
static size_t bytesRequired(int entries) {
  auto n = ringMarkerSize();
  return n * 4 + sizeof(sem_t) +
         (sizeof(IoArgs) + sizeof(IoCqe) + sizeof(IoSqe)) * (entries + 1) + 4096;
}
```

在 x86-64 + glibc 上代入實際數字：

| 型別 | 欄位 | 大小 |
|---|---|---|
| `IoArgs` | `bufId[16]` + `bufOff` + `fileIid` + `fileOff` + `ioLen` + `userdata` | **56 B** |
| `IoCqe` | `index`(4) + `reserved`(4) + `result`(8) + `userdata`(8) | **24 B** |
| `IoSqe` | `index`(4) + padding(4) + `userdata`(8) | **16 B** |
| 合計/槽 | | **96 B** |
| `sem_t` | glibc | 32 B |
| marker | `int32_t × 4` | 16 B |

所以 `hf3fs_ior_size(1024)` = `16 + 32 + 96 × 1025 + 4096` = **102 544 B**，約 100 KiB。一個 1024 深度的環只要 100 KB，這也是為什麼文件鼓勵「多執行緒就開多個環」。

`entries + 1` 與 `- 1` 這一組加減是經典的環形佇列技巧，註解直說了（`IoRing.h:59`）：`// allocate 1 more slot for queue emptiness/fullness checking`。實際成員 `entries`（`IoRing.h:90`）是 `ioRingEntries(size) + 1`，也就是**陣列長度**；而 `slots`（`IoRing.h:104`）容量是 `entries - 1`，也就是**使用者可見的深度**。`hf3fs_io_entries()` 回傳的是後者。

最後那 `+ 4096` 是純粹的餘裕。它掩蓋了建構子裡一個算錯的斷言（`IoRing.h:109-114`）：

```cpp
XLOGF_IF(FATAL, (uintptr_t)(sqeSection + entries + sizeof(sem_t)) > (uintptr_t)(buf + size), ...);
```

`sqeSection` 是 `IoSqe*`，`+ sizeof(sem_t)` 是加 32 個 **`IoSqe` 元素**（512 B）而非 32 個位元組。因為多算了 480 B 且有 4096 B 的墊底，這個過度保守的檢查從來不會誤觸發。

### 5.2 逐欄位記憶體佈局

以 `E = entries`（= 使用者要求的深度 + 1）表示：

```
偏移        大小            內容
──────────────────────────────────────────────────────────────────────────────
0x0000      4 B             sqeHead   ← 生產者（使用者進程）獨佔寫
0x0004      4 B             sqeTail   ← 消費者（FUSE io worker）獨佔寫
0x0008      4 B             cqeHead   ← 生產者（FUSE）獨佔寫
0x000c      4 B             cqeTail   ← 消費者（使用者）以 CAS 更新
──────────────────────────────────────────────────────────────────────────────
0x0010      56 × E          ringSection : IoArgs[E]    ← 依「槽位索引」定址（非環）
                              ├ bufId[16]   Iov 的 uuid
                              ├ bufOff      在該 Iov 內的位移
                              ├ fileIid     由 hf3fs_reg_fd 換來的 InodeId
                              ├ fileOff     檔案內位移
                              ├ ioLen       長度
                              └ userdata    使用者自訂指標
──────────────────────────────────────────────────────────────────────────────
0x0010+56E  24 × E          cqeSection  : IoCqe[E]     ← 真正的環，由 cqeHead/cqeTail 索引
0x…+24E     16 × E          sqeSection  : IoSqe[E]     ← 真正的環，由 sqeHead/sqeTail 索引
                              ├ index       指向 ringSection 的槽位
                              └ userdata
──────────────────────────────────────────────────────────────────────────────
0x…+16E     32 B            sem_t cqeSem   ← 行程間 semaphore（FUSE 端 sem_init(sem,1,0)）
0x…+32      ≥4096 B         未使用的餘裕
```

**最重要的結構性決定：`ringSection`（IoArgs）不是環。** sqe/cqe 才是環；`IoArgs` 是一個以「槽位索引」定址的定長池，槽位由 `AvailSlots slots`（`common/utils/AtomicSharedPtrTable.h:10-49`）配發。這個分離帶來三個好處：

1. sqe 只有 16 B，環的推進成本低，cache 友善；
2. 同一個 IO 的參數在整個生命週期中位址不動，FUSE 端可以持續引用 `ringSection[sqe.index]`（`IoRing.cc:129`）；
3. cqe 只需要回傳 `index`，使用者拿著它就能對回自己的 metadata（Python 綁定的 `self->iovs[res]` 就是這麼做的，`usrbio_binding.cc:305`）。

槽位的配發與釋放**完全在使用者側**：`alloc()` 在 `hf3fs_prep_io`（`UsrbIo.cc:644`），`dealloc()` 在 `hf3fs_wait_for_ios` 收割 cqe 時（`UsrbIo.cc:732`）。FUSE 端從不碰 `slots`。推論：**如果使用者只 prep 不 wait，槽位會耗盡，`hf3fs_prep_io` 開始回 `-EAGAIN`**，即使 FUSE 早已把 IO 做完。

### 5.3 索引推進與記憶體序

四個索引都是 `std::atomic_ref<int32_t>`（`IoRing.h:176-179`），底層就是 shm 上的 4 個 int32。`atomic_ref` 的預設記憶體序是 `seq_cst`，程式碼裡沒有任何一處指定較弱的序，也沒有顯式的 fence。

生產端（使用者，`IoRing.h:133-146`）：

```cpp
bool addSqe(int idx, const void *userdata) {
  auto h = sqeHead.load();
  if ((h + 1) % entries == sqeTail.load()) { return false; }   // 滿
  auto &sqe = sqeSection[h];
  sqe.index = idx;
  sqe.userdata = userdata;
  sqeHead.store((h + 1) % entries);                            // ★ 發布點
  return true;
}
```

`hf3fs_prep_io` 先填 `ringSection[*idx]`（`UsrbIo.cc:649-656`）再 `addSqe`，而 `addSqe` 內部也是先寫 sqe payload 再 `store` head。`seq_cst` 的 store 蘊含 release，所以 FUSE 端只要以 `seq_cst` 讀到新的 `sqeHead`，之前所有的 payload 寫入都可見。**這是整個協定唯一的同步點**。

消費端（FUSE，`IoRing.h:189`）：

```cpp
int sqeCount() const { return (sqeHead.load() + entries - sqeProcTail_) % entries; }
```

注意消費端用的是**私有的** `sqeProcTail_`（`IoRing.h:212`）而不是共享的 `sqeTail`。共享的 `sqeTail` 只在一個 batch 真正完成、且所有更早的 batch 也都完成之後才推進（`IoRing.cc:238-257`），因為多個 io worker 可以並行處理同一個環的不同區段。`sqeProcTails_`（已認領）與 `sqeDoneTails_`（已完成）兩個容器就是在做**亂序完成、順序推進**的重排：

```cpp
if (sqeProcTails_.front() != newSpt) {
  sqeDoneTails_.insert(newSpt);            // 我不是最舊的，先掛起來
} else {
  sqeTail = newSpt;                        // 我是最舊的，可以推進
  sqeProcTails_.pop_front();
  while (!sqeDoneTails_.empty()) {         // 把後面已完成的一併推進
    auto first = sqeProcTails_.front();
    auto it = sqeDoneTails_.find(first);
    if (it == sqeDoneTails_.end()) break;
    sqeTail = first; sqeProcTails_.pop_front(); sqeDoneTails_.erase(it);
  }
}
```

這一段在 `std::lock_guard lock(cqeMtx_)` 保護下執行——**行程間靠 atomic，行程內執行緒間靠 mutex**，`IoRing.cc:231-232` 的註解正是這麼寫的：

```cpp
// lock for between threads (io workers)
// atomics for between processes (io worker & io generator)
```

cqe 端的多消費者處理在使用者側（`UsrbIo.cc:711-733`），用的是「先抄再 CAS」：

```cpp
auto t = ring.cqeTail.load();
if (t == ring.cqeHead.load()) break;                 // 空了
const auto &cqe = ring.cqeSection[t];
// first record the info in curr cqe tail, if we inc first, the info may be overwritten
cqes[filled].index = cqe.index; ... 
if (!ring.cqeTail.compare_exchange_strong(t, (t + 1) % ring.entries)) break;  // 輸了就放棄
++filled;
ring.slots.dealloc(cqe.index);
```

註解說明了為何順序不能反：**先推進 tail 的話，FUSE 可能在你讀完之前就覆寫這個槽**。所以是「樂觀讀 → CAS 認領 → 認領失敗就丟棄剛抄的資料」。

### 5.4 多生產者？多消費者？

| 端 | 角色 | 併發安全性 |
|---|---|---|
| sqe 生產（使用者 `prep_io`） | **單生產者，且不保證執行緒安全** | `sqeHead.store` 是普通 store，兩條執行緒同時 prep 必然覆寫彼此的槽 |
| sqe 消費（FUSE io worker） | **多消費者** | `cqeMtx_` + `jobsToProc` 分派互斥區段 |
| cqe 生產（FUSE `addCqe`） | **多生產者，但序列化** | 一律在 `cqeMtx_` 內呼叫（`IoRing.cc:233-266`） |
| cqe 消費（使用者 `wait_for_ios`） | **多消費者，無鎖** | `cqeTail` 的 CAS |

header 的警告寫得非常直白（`hf3fs_usrbio.h:148-151`）：

```c
// this functioon is *NOT* thread safe!!!!!
// do not prepare io in the same ioring from different threads
// or the batches may be mixed and things may get ugly for *YOU*
// with such assumption, we don't waste time for the thread-safety
```

`UsrbIo.md:240` 給出的精確規則是：「只能有一條執行緒 `prep_io` + `submit_ios`，只能有一條執行緒 `wait_for_ios`，但這兩者可以是不同的執行緒」。實作上 cqe 端的 CAS 其實能撐多消費者（`UsrbIo.cc:712` 的註解「drained by another consumer?」證明作者考慮過），但 `slots.dealloc` 與使用者自己的 index→metadata 對映通常撐不住，所以文件保守。

`IoRing.h:49-52` 的 class 註解把整體策略總結得最好：

```cpp
// we allow multiple io workers to process the same ioring, but different ranges
// so 1 ioring can be used to submit ios processed in parallel
// however, we don't allow multiple threads to prepare ios in the same ioring
```

---

## 6. 喚醒機制：兩個 semaphore、一個輪詢、一個湊批計時器

USRBIO 一共有**兩種 semaphore**，方向相反，很容易搞混：

```
                      ┌──────────────── submit semaphore（每個優先級一個，全域共用）
使用者 ─ sem_post ────►│  /dev/shm/sem.hf3fs-submit-ios.<uuid>
                      │  FUSE 的 watch 執行緒 sem_timedwait
                      └────────────────────────────────────────────
                      ┌──────────────── cqe semaphore（每個環一個，就在環的 shm 裡）
FUSE ── sem_post ────►│  IoRing::cqeSem
                      │  使用者的 hf3fs_wait_for_ios sem_timedwait
                      └────────────────────────────────────────────
```

### 6.1 submit semaphore：具名 + 隨機 uuid + symlink 發布

FUSE 啟動時建立三個具名 semaphore，名字帶隨機 uuid（`IoRing.h:217-228`、`256-266`）：

```cpp
void init(int cap) {
  for (int prio = 0; prio <= 2; ++prio) {
    auto sp = "/" + semOpenPath(prio);
    sems.emplace_back(sem_open(sp.c_str(), O_CREAT, 0666, 0),
                      [sp](sem_t *p) { sem_close(p); sem_unlink(sp.c_str()); });
    chmod(semPath(prio).c_str(), 0666);
  }
  ioRings = std::make_unique<AtomicSharedPtrTable<IoRing>>(cap);
}
static std::string semOpenPath(int prio) {
  static std::vector<Uuid> semIds{Uuid::random(), Uuid::random(), Uuid::random()};
  return fmt::format("hf3fs-submit-ios.{}", semIds[prio].toHexString());
}
```

名字隨機，所以使用者無法猜到；FUSE 透過在 `3fs-virt/iovs/` 底下放三個**假的 symlink** 來發布它們（`IovTable.cc:313-319` 的 `listIovs` 與 `IoRing.h:267-277` 的 `lookupSem`），symlink 的目標就是 `/dev/shm/sem.hf3fs-submit-ios.<uuid>`：

```
/mnt/3fs/3fs-virt/iovs/submit-ios      → /dev/shm/sem.hf3fs-submit-ios.<uuid1>   (普通)
/mnt/3fs/3fs-virt/iovs/submit-ios.ph   → /dev/shm/sem.hf3fs-submit-ios.<uuid0>   (高)
/mnt/3fs/3fs-virt/iovs/submit-ios.pl   → /dev/shm/sem.hf3fs-submit-ios.<uuid2>   (低)
```

這三個 symlink 有自己的保留 inode id（`iovDir - 1/2/3`，`IoRing.h:269-274`），權限 `0666`，且 `hf3fs_unlink` 明確拒絕刪除它們（`FuseOps.cc:1054-1058` 回 `EPERM`）。

使用者側的 `cqeSem()`（函式名取錯了，它拿的是 submit sem，`UsrbIo.cc:332-370`）做的是：`readlink` → 驗證目標必須在 `/dev/shm` 底下、必須是單一層、必須以 `sem.` 開頭 → 去掉 `sem.` 前綴 → `sem_open(name, 0)`。這幾層驗證是為了防止 FUSE 端被騙出一個任意路徑。

**FUSE 重啟後 uuid 會變**，舊的 semaphore 名字失效——這是刻意的，避免跨生命週期的殘留 post。

優先級的編碼在兩側**不一致**，必須注意：

| | 高 | 普通 | 低 |
|---|---|---|---|
| 使用者側 `ior->priority`（`UsrbIo.cc:334-337`） | `< 0` | `== 0` | `> 0` |
| symlink 後綴 | `.ph` | 無後綴 | `.pl` |
| FUSE 側 `IorAttrs::priority`（`IovTable.cc:88-101`） | `0` | `1` | `2` |
| sem 檔名（`IoRing.h:263-265`） | `submit-ios.ph` | `submit-ios` | `submit-ios.pl` |

### 6.2 FUSE 的 watch 執行緒：sem + 1 ms 輪詢的混合

`FuseClients::watch`（`FuseClients.cc:369-401`）每個優先級一條 `std::jthread`：

```cpp
void FuseClients::watch(int prio, std::stop_token stop) {
  while (!stop.stop_requested()) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    auto nsec = ts.tv_nsec + jitter.load().count();          // submit_wait_jitter，預設 1 ms
    ts.tv_nsec = nsec % 1000000000;
    ts.tv_sec += nsec / 1000000000;
    if (sem_timedwait(iors.sems[prio].get(), &ts) < 0 && errno == ETIMEDOUT) {
      continue;                                              // ← 逾時也只是重跑迴圈
    }
    auto gotJobs = false;
    do {
      gotJobs = false;
      auto n = iors.ioRings->slots.nextAvail.load();
      for (int i = 0; i < n; ++i) {                          // ★ 掃描全部 io ring
        auto ior = iors.ioRings->table[i].load();
        if (ior && ior->priority == prio) {
          auto jobs = ior->jobsToProc(config->max_jobs_per_ioring());   // 預設 32
          for (auto &&job : jobs) { gotJobs = true; iojqs[prio]->enqueue(std::move(job)); }
        }
      }
    } while (gotJobs);
  }
}
```

關鍵在 `continue` 那一行：**逾時與被喚醒走的是同一條路徑**。`sem_timedwait` 逾時後迴圈重跑，仍會進入掃描區段嗎？不會——`continue` 直接跳過掃描回到 `while` 開頭再等一次。這意味著：

- 有人 `sem_post` → 立刻掃描；
- 完全沒有 post → 每 1 ms 空轉一次 `sem_timedwait`，**不掃描**。

那 `UsrbIo.md:202` 說的「the FUSE process also scan new operations periodically」是怎麼成立的？靠的是 `hf3fs_wait_for_ios` 每次收割完 cqe 都會補一次 `sem_post`（`UsrbIo.cc:735-736`）：

```cpp
// post sem to signal the available slots in cqe section
hf3fs_submit_ios(ior);
```

以及 io worker 處理完一個 job 後可能 `sem_post` 喚醒 watcher（`FuseClients.cc:343-344`）。所以 semaphore 的計數其實同時承載了兩種語意：「有新的 sqe」與「cqe 區段空出來了」。這也解釋了為什麼 `hf3fs_submit_ios` 在文件裡被稱為「a last resort hint」（`hf3fs_usrbio.h:41`）——它只是 `sem_post` 一下，完全不帶資訊：

```cpp
int hf3fs_submit_ios(const struct hf3fs_ior *ior) {          // UsrbIo.cc:672-681
  if (!ior || !ior->iorh) return -EINVAL;
  auto &iorh = *(Hf3fsIorHandle *)ior->iorh;
  sem_post(iorh.submitSem);
  return 0;
}
```

`while (gotJobs)` 這個外迴圈也很重要：只要這一輪掃到了任何 job 就再掃一輪，直到掃空為止才回去阻塞。這讓一次 post 能吸乾所有堆積的 sqe，避免「N 個 sqe 需要 N 次 post」。

### 6.3 `io_depth` 與 `timeout` 的互動，以及 593cec5 修的到底是什麼

`IoRing::jobsToProc`（`IoRing.cc:15-65`）是整個批次策略的所在：

```cpp
auto cqeAvail = entries - 1 - processing_ - cqeCount();
while (sqes && (int)jobs.size() < maxJobs) {
  int toProc;
  if (ioDepth > 0) {                       // 【嚴格湊批】
    toProc = ioDepth;
    if (toProc > sqes || toProc > cqeAvail) break;   // 湊不滿就完全不做
  } else {
    toProc = std::min(sqes, cqeAvail);
    if (ioDepth < 0) {                     // 【上限批 + 逾時】
      auto iod = -ioDepth;
      if (toProc > iod) {
        toProc = iod;
      } else if (toProc < iod && timeout.count()) {
        auto now = SteadyClock::now();
        if (!lastCheck_) { lastCheck_ = now; break; }        // 第一次發現不夠，開始計時
        else if (*lastCheck_ + timeout > now) { break; }     // 還沒到期，繼續等
      }
      lastCheck_ = std::nullopt;
    }
  }                                        // ioDepth == 0：【有多少做多少】
  ...
}
```

三種 `io_depth` 語意（與 `hf3fs_usrbio.h:38-44`、`hf3fs_fuse/io.py:69-72` 的文件一致）：

| `io_depth` | 行為 | 風險 |
|---|---|---|
| `> 0` | 嚴格每批 `io_depth` 個，湊不滿就不送 | **湊不滿會永久卡住**（Python 文件明說「用户需保证最终有足量任务，否则 ioring 在 wait 时会卡住」） |
| `== 0` | 有多少送多少，`timeout` 無作用 | 小 IO 無法成批，吞吐低 |
| `< 0` | 每批最多 `-io_depth` 個；不足時等 `timeout` 毫秒再送 | 最實用的模式；`timeout == 0` 時退化為 `== 0` |

`cqeAvail` 的計算是防止 cqe 溢位的關鍵：`entries - 1 - processing_ - cqeCount()`。註解在 `IoRing.cc:27`：「even if we finish the io, we got no place to store the results」。這保證了 `addCqe` 永遠不會失敗——真的失敗時是 `XLOGF(FATAL, "failed to add cqe")`（`IoRing.cc:264`），即整個 FUSE 進程掛掉。

**commit 593cec5「fix(IoRing): correct timeout check logic for batch processing」修的正是這個湊批計時器**：

```diff
-          auto now = lastCheck_ = SteadyClock::now();
+          auto now = SteadyClock::now();
           if (!lastCheck_) {  // first time to find the (not enough) ios, wait till timeout
             lastCheck_ = now;
             break;
```

修正前的語意：每次進到這個分支都先把 `lastCheck_` 覆寫成當下時間，於是下一行的 `*lastCheck_ + timeout > now` 恆等於 `now + timeout > now`，**永遠為真**，永遠 `break`。結果是：`io_depth < 0` 的環一旦進入「sqe 數量不足一批」的狀態，逾時**永遠不會觸發**，那幾個 IO 就一直卡著，直到後續有足夠多的 sqe 把 `toProc` 推到 `>= iod` 為止。對於「最後一批不滿」的場景（訓練資料的尾批、fio 收尾）就是直接 hang。

修正後 `lastCheck_` 只在**第一次**發現不足時設定，之後每次比較的都是「距離第一次發現已經過了多久」，逾時得以正確到期。這是一個典型的「賦值運算式順手塞進宣告」造成的邏輯錯誤。

還有一個細節：`lastCheck_` 是 `IoRing` 的私有成員（`IoRing.h:173`），而 `jobsToProc` 在 `cqeMtx_` 保護下執行，所以計時器是每個環一份、且是全域的（不分優先級 watcher）。

### 6.4 cqe semaphore：使用者側的等待

`hf3fs_wait_for_ios`（`UsrbIo.cc:683-770`）的等待不是單純的阻塞，而是**以 jitter 為粒度的分段等待**：

```cpp
auto jitter = 1;
auto js = getenv("HF3FS_USRBIO_WAIT_JITTER_MS");
if (js && atoi(js)) { jitter = atoi(js); }
...
auto nsec = ts.tv_nsec + jitter * 1000000;         // 預設 1 ms
...
if (abs_timeout && ts.tv_sec >= abs_timeout->tv_sec) {   // 不超過使用者給的絕對逾時
  ts.tv_sec = abs_timeout->tv_sec;
  ts.tv_nsec = std::min(ts.tv_nsec, abs_timeout->tv_nsec);
}
// wait for cqe sem, don't care if it succeeds, times out, or even fails
// we check the cqe section again in any case
sem_timedwait(ring.cqeSem.get(), &ts);
```

也就是每次最多睡 1 ms 就醒來重查 cqe 區段，完全不信任 semaphore 的計數。理由是 semaphore 的 post 次數與 cqe 個數並非一一對應——FUSE 端是**一個 batch 只 post 一次**（`IoRing.cc:271`，在 `cqeMtx_` 之外），而使用者可能一次收割多個。用「semaphore 當提示、共享記憶體當真相」是這個設計一貫的態度。

`abs_timeout` 的檢查在等待之前（`UsrbIo.cc:750-754`），使用 `CLOCK_REALTIME`（因為 `sem_timedwait` 只認 `CLOCK_REALTIME`）。這代表**系統時間被往前調整會讓等待提早結束、往後調整會讓等待變長**。

`min_results` 的語意：`filled >= min_results` 且當下沒有更多立即可得的結果時就回傳（`UsrbIo.cc:740-742`）。若 `min_results <= 0`，這就是一次非阻塞輪詢。

---

## 7. 從 prep 到 cqe 的完整時序

```
使用者進程                          共享記憶體                       FUSE 進程 (hf3fs_fuse_main)
──────────                          ──────────                       ─────────────────────────

 hf3fs_prep_io()
 ├ slots.alloc() → idx  ──────────► ringSection[idx] = {bufId, bufOff,
 │  UsrbIo.cc:644                     fileIid, fileOff, ioLen, userdata}
 │                                    UsrbIo.cc:649-656
 └ addSqe(idx)          ──────────► sqeSection[sqeHead] = {idx, userdata}
    IoRing.h:133                      sqeHead.store(+1)  ★ 發布

 hf3fs_submit_ios()
 └ sem_post(submitSem)  ──────────► /dev/shm/sem.hf3fs-submit-ios.*
                                                                  ┌─► watch(prio) 醒來
                                                                  │   FuseClients.cc:379
                                                                  │
                                                                  ├─ 掃描所有 ior
                                                                  │  jobsToProc(max_jobs_per_ioring)
                                                                  │  IoRing.cc:15
                                                                  │  ├ ioDepth 決定 toProc
                                                                  │  ├ cqeAvail 決定會不會爆 cqe
                                                                  │  └ sqeProcTail_ 前進、記入 sqeProcTails_
                                                                  │
                                                                  └─ iojqs[prio]->enqueue(job)
                                                                     BoundedQueue（hi/普通/lo 三條）

                                                                  ┌─► ioRingWorker 協程 co_dequeue
                                                                  │   FuseClients.cc:218-357
                                                                  │   （batch_io_coros=128 條）
                                                                  │
                                                                  ├─ lookupFiles: fileIid → RcInode
                                                                  │  FuseClients.cc:279-295
                                                                  │  （在 d.inodes 表裡查，相鄰同 iid 直接複用）
                                                                  │
                                                                  ├─ lookupBufs: bufId(uuid) → ShmBuf
                                                                  │  FuseClients.cc:296-333
                                                                  │  （iovs.shmsById 查表 + 越界檢查）
                                                                  │
                                                                  ├─ IoRing::process()  IoRing.cc:67
                                                                  │  ├ memh = 該段的 IOBuffer（IB MR）
                                                                  │  ├ 寫入才有：beginWrite → extendStripe
                                                                  │  ├ PioV::addRead/addWrite × toProc
                                                                  │  │   PioV.cc:14 / 55
                                                                  │  │   └ chunkIo() 依 layout 切 chunk
                                                                  │  │      PioV.cc:98-130
                                                                  │  │      chunk → (ChainId, ChunkId, off, len)
                                                                  │  └ executeRead/Write → batchRead/batchWrite
                                                                  │      PioV.cc:139 / 182
                                                                  │      └ StorageClient → RDMA ─┐
                                                                  │                              │
   Iov 的那一段記憶體  ◄──────────────────────── RDMA write/read ──┘
   （storage server 直接寫進使用者的 shm）
                                                                  │
                                                                  ├─ finishIo(allowHoles) 彙總每個 IO 的位元組數
                                                                  │  PioV.cc:268
                                                                  ├─ 寫入才有：finishWrite → dirtyInodes
                                                                  │  IoRing.cc:218
                                                                  │
                                                                  ├─ 【cqeMtx_ 臨界區】IoRing.cc:230-269
                                                                  │  ├ 依序推進 sqeTail（亂序完成、順序推進）
                                                                  │  └ addCqe × toProc  ─────────┐
                                                                  │                              │
   cqeSection[cqeHead] = {index, result, userdata}  ◄──────────────┘
   cqeHead.store(+1)
                                                                  │
   sem_post(cqeSem)  ◄────────────────────────────────────────────┘  IoRing.cc:271

 hf3fs_wait_for_ios()
 ├ cqeCount() > 0?
 ├ 抄 cqe → CAS cqeTail → slots.dealloc(index)
 │  UsrbIo.cc:711-733
 ├ sem_post(submitSem)（告知 cqe 空間釋出）UsrbIo.cc:736
 └ 不足則 sem_timedwait(cqeSem, now+1ms) 重試
```

`process()` 裡有一個容易忽略的細節：`lookupFiles` / `lookupBufs` 都被呼叫**兩次**（`IoRing.cc:107-110`、`114-117`），因為 sqe 區段是環形的，一個 batch 可能跨越陣列結尾：

```cpp
lookupFiles(inodes, ringSection, sqeSection + spt, std::min(toProc, entries - spt));
if ((int)inodes.size() < toProc) {
  lookupFiles(inodes, ringSection, sqeSection, toProc - (int)inodes.size());
}
```

另外 `lookupFiles` 有一個針對「同一檔案的連續多個 IO」的快取（`FuseClients.cc:284-289`）：若 `fileIid` 與上一個相同就直接複用 `shared_ptr`，省掉一次雜湊查找。`lookupBufs` 也有同樣的優化。這對「一個 batch 全部讀同一個大檔」的訓練場景很有效。

---

## 8. 檔案註冊：fd → InodeId

FUSE 進程根本看不到使用者進程的 fd 表，它只認 inode。`hf3fs_reg_fd` 做的就是這個換算（`UsrbIo.cc:558-596`）：

```cpp
int hf3fs_reg_fd(int fd, uint64_t flags) {
  (void)flags;
  auto is3fs = hf3fs_is_hf3fs(fd);
  if (!is3fs || fd >= (int)regfds.size()) return EBADF;
  else if (regfds[fd].load()) return EINVAL;

  struct statx stx;
  auto sres = statx(fd, "", AT_EMPTY_PATH | AT_STATX_DONT_SYNC, STATX_INO, &stx);   // ★
  if (sres < 0) return errno;

  auto dupfd = dup(fd);                                                             // ★
  if (dupfd < 0) return errno;
  else if (regfds[dupfd].load()) { close(dupfd); return EINVAL; }

  int status = fcntl(fd, F_GETFL);                                                  // ★

  std::shared_ptr<Hf3fsRegisteredFd> empty;
  auto regfd = std::make_shared<Hf3fsRegisteredFd>(fd, dupfd, hf3fs::meta::InodeId{stx.stx_ino}, status);
  if (!regfds[fd].compare_exchange_strong(empty, regfd)) return EINVAL;
  if (!regfds[dupfd].compare_exchange_strong(empty, regfd)) { ...; return EINVAL; }

  return -dupfd;
}
```

四個設計點：

1. **登記表是一個定長陣列，長度 = `RLIMIT_NOFILE` 的硬上限**（`UsrbIo.cc:545-556`）。這是一個 `std::vector<folly::atomic_shared_ptr<...>>`，在 `.so` 載入時就配置好（靜態初始化），以 fd 值直接索引，查表是 O(1) 且無鎖（`UsrbIo.cc:631`）。代價是啟動時就吃掉 `rlim_max × 8 B` 的記憶體——`folly::atomic_shared_ptr` 的唯一成員是 `Atom<PackedPtr>`，而 `PackedPtr = folly::PackedSyncPtr<BasePtr>`（`third_party/folly/folly/concurrency/AtomicSharedPtr.h:80`）內部只有一個 `PicoSpinLock<uintptr_t, 15>`，即 8 bytes。若系統的 `RLIMIT_NOFILE` 硬上限設成 1048576，這就是 **8 MB**（與 FUSE 報告 §10.2 對 `AtomicSharedPtrTable` 的估算一致）。

2. **`statx` 的 `AT_STATX_DONT_SYNC`**：只要 inode 號碼，明確要求不要觸發 3FS 的長度同步（3FS 的 `stat` 預設會 flush/sync，見 `FuseConfig.h:79-80` 的 `flush_on_stat` / `sync_on_stat`）。這讓註冊變成一個廉價操作。

3. **`dup(fd)` 與「兩個索引指向同一個物件」**。`dupfd` 的作用有二：(a) 它是回傳給呼叫者的「USRBIO 專用 fd」（回傳 `-dupfd`），(b) 它讓核心持有這個檔案的一個額外引用，這樣即使使用者 `close(fd)` 了，FUSE 端的 inode 引用計數也不會歸零、`d.inodes` 表裡的項目不會消失。這正是 header 那句奇怪警告的機制來源（`hf3fs_usrbio.h:138-140`）：

   ```c
   // registered fds should not be closed, and even if it's closed, the old inode will still be used to prep io
   // also, if a registered fd is closed, and a new fd with the same integer value is to be registered
   // the registration will fail with an EINVAL
   ```

   後半句是因為 `regfds[fd]` 還留著舊項目，`compare_exchange_strong(empty, ...)` 會失敗。這是一個**會靜默用錯檔案**的陷阱：`close(fd)` 後 fd 被 reuse 給另一個檔案，但 `hf3fs_prep_io(fd)` 仍會用舊的 `InodeId`。

4. **`prep_io` 同時接受 `fd` 與 `-dupfd`**：`auto afd = abs(fd);`（`UsrbIo.cc:625`）。兩個索引都指向同一個 `Hf3fsRegisteredFd`，所以效果相同。

`Hf3fsRegisteredFd` 的解構子 `close(dupfd)`（`UsrbIo.cc:537`），因此 `hf3fs_dereg_fd` 把兩個 `atomic_shared_ptr` 清空後，最後一個引用消失時 dup 出來的 fd 自動關閉——**沒有顯式的 close，靠的是 shared_ptr**。

存取模式檢查發生在 prep 而非 reg（`UsrbIo.cc:636-639`）：

```cpp
int status = regfd->status;                       // 註冊時抓的 F_GETFL
if ((read && (status & O_ACCMODE) == O_WRONLY) || (!read && (status & O_ACCMODE) == O_RDONLY)) {
  return -EACCES;
}
```

因為是註冊時的快照，之後用 `fcntl(F_SETFL)` 改狀態不會反映。

FUSE 端拿到 `fileIid` 後在 `d.inodes` 這張表裡查（`FuseClients.cc:290-293`）：

```cpp
auto iid = meta::InodeId(idn);
auto it = inodes.find(iid);
ins.push_back(it == inodes.end() ? (std::shared_ptr<RcInode>()) : it->second);
```

查不到就是 `nullptr`，`process()` 回 `-MetaCode::kNotFile`（`IoRing.cc:141-144`）。這張表由 FUSE 的 `lookup`/`open` 回呼填充並計數（`FuseOps.cc:231-247` 的 `add_entry`），由 `forget` 遞減（`FuseOps.cc:264-281` 的 `remove_entry`）。**所以檔案必須在 FUSE 端「還開著」，USRBIO 才能對它做 IO**——這也是為什麼 `dup` 那一手是必要的。

---

## 9. 錯誤、短讀與部分完成

### 9.1 cqe 的 result 怎麼算出來的

`IoRing.cc:262`：

```cpp
auto addRes = addCqe(sqe.index, r >= 0 ? r : -static_cast<ssize_t>(StatusCode::toErrno(-r)), sqe.userdata);
```

`res[i]` 在 FUSE 內部是 3FS 自己的 `StatusCode`（負值），送出前統一轉成 `-errno`。所以使用者永遠只看到 POSIX errno，看不到 3FS 的內部錯誤碼。

### 9.2 短讀、EOF 與「洞」

`PioV::finishIo` → `concatIoRes`（`PioV.cc:186-266`）負責把「一個使用者 IO 被切成的多個 chunk IO」的結果合併回一個位元組數。核心邏輯：

```cpp
if (iolen < io.length) {         // 這個 chunk 讀到的比要求的少
  inHole = true;
  if (!holeIo) { holeIo = i; holeOff = iolen; holeSize = 0; }
  holeSize += io.length - iolen;
}
res[iovIdx] += iolen;
```

若後面的 chunk 又讀到了資料（`iolen > 0 && inHole && lastIovIdx == iovIdx`），代表中間那段是**檔案空洞**而非 EOF。此時分兩路（`PioV.cc:225-238`）：

```cpp
if (read && allowHoles) {           // 預設：把洞補零
  auto &hio = ios[*holeIo];
  memset(hio.data + holeOff, 0, hio.length - holeOff);
  for (size_t j = *holeIo + 1; j < i; ++j) { memset(ios[j].data, 0, ios[j].length); }
  res[iovIdx] += holeSize;
} else {                            // FORBID_READ_HOLES：整個 IO 判定失敗
  res[iovIdx] = -static_cast<ssize_t>(ClientAgentCode::kHoleInIoOutcome);
}
```

`allowHoles` 由環的 flag 決定（`IoRing.cc:206`）：

```cpp
ioExec.finishIo(!(flags_ & HF3FS_IOR_FORBID_READ_HOLES));
```

而 `kChunkNotFound` 的讀取錯誤被直接忽略（`PioV.cc:243-244`），視同「讀到 0 位元組」，因為稀疏檔案就是沒有那個 chunk。

於是使用者能觀察到的語意是：

| 情況 | `cqe.result` | 如何區分 |
|---|---|---|
| 完整讀到 | `== len` | — |
| 讀到檔案結尾 | `0 <= result < len` | **無法**與「尾端是洞」區分 |
| 中間有洞（預設） | `== len`，洞的部分被填 0 | 資料上看不出來 |
| 中間有洞（`FORBID_READ_HOLES`） | `-EIO` 之類的負值 | 明確報錯 |
| 某個 chunk IO 失敗 | 負值 | — |
| 寫入 | 一律不允許洞（`finishIo(false)`，`PioV.cc:272`） | — |

Python 綁定的註解把這個限制講得最白（`usrbio_binding.cc:218-219`）：「如读取字节数小于请求字节数，可能是文件已到末尾，或者读到文件中间空洞，这两种情况需用户自行区分」。**要區分只能靠 `HF3FS_IOR_FORBID_READ_HOLES` 或另外 `stat` 檔案長度**。

### 9.3 批次中的個別失敗

`IoRing::process` 的錯誤處理是**逐 IO 獨立**的：`res` 是一個 `std::vector<ssize_t>(toProc)`，每個位置各自記錄。inode 查不到、buf 查不到、memh 拿不到、`beginWrite` 失敗，都只影響該筆（`IoRing.cc:141-169`）。只有 `executeRead/Write` 整批失敗時才會把所有「還沒有錯」的項目一起標成錯誤（`IoRing.cc:199-204`）：

```cpp
if (!execRes) {
  for (auto &r : res) { if (r >= 0) { r = -static_cast<ssize_t>(execRes.error().code()); } }
}
```

唯讀模式是最粗的一刀（`IoRing.cc:96-97`）：若使用者設定了 `readonly`，整批寫入直接變成 `-kReadOnlyMode`，連 lookup 都不做。

### 9.4 `HF3FS_IOR_ALLOW_READ_UNCOMMITTED`

`IoRing.cc:188-191`：

```cpp
auto readOpt = storageIo.read();
if (flags_ & HF3FS_IOR_ALLOW_READ_UNCOMMITTED) {
  readOpt.set_allowReadUncommitted(true);
}
```

這會讓讀取繞過 CRAQ 的 commit 語意，讀到尚未在鏈上完全提交的資料。用於「寫入者與讀取者是同一個應用、能自行保證順序」的場景。是 per-ring 的設定，不能 per-IO。

---

## 10. 限制與陷阱

### 10.1 硬性限制

| 項目 | 限制 | 出處 |
|---|---|---|
| 讀寫方向 | **一個環只能單向**，`read != ior->for_read` 直接 `-EINVAL` | `UsrbIo.cc:626` |
| 對齊 | **無任何對齊要求**（ptr / off / len 都不用） | `UsrbIo.cc:626-628` 沒有對齊檢查 |
| 單一 IO 長度上限 | 只受 Iov 大小限制；但若設了 `block_size`，**不得跨 block 邊界** | `Shm.h:90-92` |
| `len == 0` | 拒絕（`len <= 0`） | `UsrbIo.cc:626` |
| 在途 IO 數 | `entries`（`hf3fs_io_entries()`），超過回 `-EAGAIN` | `UsrbIo.cc:644-647` |
| 掛載點字串 | `< 256` 字元 | `UsrbIo.cc:124-127` |
| iov 數量 | FUSE 端 `iov_limit`，預設 `1_MB` 個 | `FuseConfig.h:44`、`FuseClients.cc:88-89` |
| `punch_hole` 區間數 | 1000 | `src/lib/api/fuse.h:12` |
| 內部 chunk 切分 | 受 `chunk_size_limit`（可 per-user 覆寫）進一步細分 | `IoRing.cc:119`、`PioV.cc:108` |

### 10.2 執行緒與 fork

- **同一個環不可多執行緒 prep**。這不是「效能建議」而是正確性要求：`sqeHead.store` 沒有任何互斥，兩條執行緒會寫進同一個 sqe 槽。文件建議「多執行緒就一人一個環」（`UsrbIo.md:9`），一個環只要 ~100 KB，這個建議很便宜。
- **prep+submit 一條執行緒、wait 另一條執行緒是允許的**（`UsrbIo.md:240`）。
- **fork 之後子進程繼承 mapping 與 fd**。共享記憶體是 `MAP_SHARED`，所以父子會**同時操作同一個環**——這幾乎必然出錯，除非只有一方使用。同時，因為 `alive.mountFds` 那個 `O_DIRECTORY` fd 也被繼承，**父進程結束時 iov 不會被回收**，要等所有子進程都結束（`FuseOps.cc:1817` 的註解）。
- `ShmBuf` 建立時記錄了 `pid` 與 `ppid`（`UsrbIo.cc:133`），`AllProcMap::procTable`（`PerProcTable.h:175-201`）有一套「若 `ppid` 命中既有表就繼承父表」的 fork 感知邏輯——但這套機制服務的是 `src/lib/api/hf3fs.h` 那個舊的 IClient 路徑，USRBIO 本身並未使用（`ProcShmBuf` 這個 alias 在 `Shm.h:101` 定義後在 USRBIO 路徑上沒有使用點）。

### 10.3 實作層面的坑（讀碼才會發現）

**(a) 帶 flags 的環，symlink 刪不掉。** 建立時用二進位格式化，銷毀時用十進位：

```cpp
// UsrbIo.cc:154（create）
is_io_ring && flags != 0 ? fmt::format(".f{:b}", flags) : std::string()
// UsrbIo.cc:213（destroy）
is_io_ring && flags != 0 ? fmt::format(".f{}", flags) : std::string()
```

`flags = 2`（`HF3FS_IOR_FORBID_READ_HOLES`）建立時檔名是 `....f10`，銷毀時去 unlink `....f2` —— **unlink 必然失敗**，symlink 與 FUSE 端的 IoRing 都留在原地，直到進程結束由 `releasedir` 那條保險清掉。`flags = 1` 剛好兩種格式相同，所以只有 `flags >= 2` 會踩到。

**(b) `sem_destroy` 作用在已 munmap 的位址上。** `hf3fs_iordestroy`（`UsrbIo.cc:518-529`）先 `hf3fs_iovdestroy_general`（其中 `delete ShmBuf` → `unmapBuf()` → `munmap`），**之後**才 `delete (Hf3fsIorHandle *)ior->iorh`，而 `IoRing` 的 `cqeSem` 是帶 `sem_destroy` 刪除器的 `unique_ptr`（`IoRing.h:183`），且該刪除器**不看 `owner`**。指標指向的正是環 shm 內的 `sem_t`（`IoRing.h:115`）。這在 glibc 上不會炸，只因為 glibc 的 `sem_destroy` 實質是 no-op；換一個真的會去碰記憶體的 libc 就是 use-after-munmap。

**(c) `hf3fs_iovopen` 的 `size` 由呼叫者提供。** 它不去 `fstat` 那個 shm（`UsrbIo.cc:256-262`），直接用傳入的 `size` 做 mmap。給大了會 `SIGBUS`，給小了會靜默截斷。相對地，FUSE 端的 `addIov` 是老老實實 `stat` 的（`IovTable.cc:144-147`）。

**(d) iov inode id 的文件與實作不一致。** `src/fbs/meta/Common.h:152` 註解寫「iov InodeId range [0xffffffff7ffe0002, 0xffffffff7fff0001]」，static_assert 也只驗證到 65536 個（`Common.h:204-205`）；但 `iov_limit` 預設是 1 MB 個（`FuseConfig.h:44`），而 `IovTable::iovDesc`（`IovTable.cc:115-122`）實際接受的範圍一路到 `INT_MAX - 65535`。實務上不會撞車（往下一個保留 id 是 `getConf() = 0xffffffff00000000`，還有 ~2×10⁹ 的空間），但註解與 static_assert 描述的窗口比實作小得多。

**(e) `IoRing::sqeTailAfter`（`IoRing.h:147-158`）是死碼。** 全樹沒有任何呼叫點。

**(f) `src/lib/py/binding.cc` 是死檔。** 它 `#include "lib/api/Client.h"`，而 `src/lib/api/` 底下**沒有 `Client.h`**；`src/lib/py/CMakeLists.txt:1` 也只編譯 `usrbio_binding.cc`。同樣地 `src/lib/api/hf3fs.h` 定義的 `IClient` 這套完整 POSIX 介面除了 `binding.cc` 之外無人實作、無人使用——`FuseOps.cc:52` 與 `UsrbIo.cc:15` 引入它只是為了 `HF3FS_SUPER_MAGIC` 這個巨集。這是一段被 USRBIO 取代掉的舊「client agent」設計的殘骸。

**(g) 環的容量與 io_depth 的一致性由 FUSE 檢查。** `IovTable.cc:151-153`：`ioDepth > IoRing::ioRingEntries(st.st_size)` 會讓 symlink 建立失敗（回 `kInvalidArg`），使用者看到的是 `hf3fs_iorcreate` 回 `-EINVAL`。注意這只擋正的 `io_depth`。

### 10.4 環境變數

| 變數 | 作用 | 出處 |
|---|---|---|
| `HF3FS_USRBIO_LIB_LOG` | 共享庫日誌等級，預設 `WARN` | `UsrbIo.cc:21-22` |
| `HF3FS_USRBIO_DONT_CHECKFS_FOR_MP` | `=yes` 時跳過 mountinfo 的 fstype 檢查 | `UsrbIo.cc:46-47` |
| `HF3FS_USRBIO_WAIT_JITTER_MS` | `hf3fs_wait_for_ios` 的重查間隔（毫秒），預設 1 | `UsrbIo.cc:696-699` |

日誌初始化是**靜態物件的建構子**（`UsrbIo.cc:19-25`），也就是 `.so` 一被載入就跑，早於 `main()`：

```cpp
struct Hf3fsInitLib {
  Hf3fsInitLib() {
    auto v = getenv("HF3FS_USRBIO_LIB_LOG");
    hf3fs::logging::initOrDie(v && *v ? v : "WARN");
  }
};
static Hf3fsInitLib initLib;
```

`initOrDie` 這個命名不是誇飾——初始化失敗會直接終止進程。

---

## 11. 語言綁定

### 11.1 Python：`hf3fs_py_usrbio` + `hf3fs_fuse.io`

分兩層：pybind11 的 C++ 層（`src/lib/py/usrbio_binding.cc`，471 行）與純 Python 的薄封裝（`hf3fs_fuse/io.py`，139 行）。

**`iovec` 類別**（`usrbio_binding.cc:111-230`）：

- 建構子只走 `hf3fs_iovwrap`（`usrbio_binding.cc:138`），**不會自己建 shm 也不會建 symlink**。symlink 由 Python 側的 `make_iovec` 手動建立（`hf3fs_fuse/io.py:58-64`），並由 `iovec.__del__` 手動 `os.unlink`（`io.py:14-15`）。這是與 C API 最大的結構差異。
- 支援 buffer protocol（`def_buffer`，`usrbio_binding.cc:117-119`），所以可以 `memoryview(iov)`、`numpy.frombuffer(iov)`。
- **切片語意**：`iov[a:b]` 與 `iov.slice_by(buf)` 產生一個新的 `iovec`，其 `base_iov` 指回原始物件（`usrbio_binding.cc:175`、`205`）。`prepare` 時把 `base_iov` 傳給 `hf3fs_prep_io` 作為範圍檢查的依據，而把 slice 的 `base` 當作實際位址（`usrbio_binding.cc:293-300`）。步長必須為 1（`usrbio_binding.cc:192-193`）。
- **長度來自切片**：`hf3fs_prep_io(..., off, iov->size, ...)`（`usrbio_binding.cc:299`）。也就是說 Python 的 `prepare` **沒有 `len` 參數**，IO 長度就是那個 slice 的長度。這是比 C API 更安全但更受限的設計。
- 結果回填到 iovec 物件本身：`iov->result = cqes[i].result`（`usrbio_binding.cc:404`），所以 `wait()` 回傳的是一串 `iovec` 而非 cqe。

**`ioring` 類別**（`usrbio_binding.cc:232-435`）：

- 只走 `hf3fs_iorcreate4`，因此 **`priority` 參數被明確丟棄**（`usrbio_binding.cc:248`：`(void)priority;`）。`hf3fs_fuse/io.py:43-46` 甚至定義了 `IorPriority.HIGH/NORMAL/LOW` 常數，但傳下去也沒有作用。這是 Python 與 C API 最明顯的語意落差。
- `userdata` 的引用計數是手動管的：`prepare` 時 `inc_ref`（`usrbio_binding.cc:290`），`wait` 拿回來後 `dec_ref`（`usrbio_binding.cc:417-419`）。**若 IO 從未被 wait 收割，該物件永久洩漏**。
- 所有阻塞呼叫都 `py::gil_scoped_release`（`usrbio_binding.cc:291`、`327`、`346`），所以多執行緒 Python 能真正並行。
- `wait()` 內部是一個迴圈，湊不滿 `min_results` 就帶著逾時再呼叫一次；已經湊夠之後改傳 `&start`（一個已經過去的時間點）當逾時，等於非阻塞地把剩下的撈乾淨（`usrbio_binding.cc:380-384`）。
- 有一個 `XLOGF(FATAL, "same cqe {} fetched more than once")` 的防護（`usrbio_binding.cc:401-403`），會直接讓 Python 進程死掉。

`hf3fs_fuse/io.py` 另外提供 `read_file()`（`io.py:86-139`），一個「一次一個 block 循序讀完整個檔案」的便利函式，也是官方唯一的完整 Python 用法示範來源之一。`hf3fs_fuse/fuse_demo.py` 則是最精簡的 30 行範例。

打包由根目錄的 `setup.py` 負責：它是一個 CMake 驅動的 `build_ext`，只建 `hf3fs_py_usrbio` 這一個 target（`setup.py:79`），並硬編 `clang-14`（`setup.py:48-49`）。另一個 `setup_hf3fs_utils.py` 打包的是 `hf3fs_utils`（`hf3fs_cli` 命令列工具，處理 trash/rmtree/mv），與 USRBIO 無關。

### 11.2 Rust：`hf3fs-usrbio-sys`

`build.rs`（20 行）用 bindgen 直接吃公開 header：

```rust
println!("cargo::rustc-link-search=native={}/lib", topdir);
println!("cargo::rustc-link-lib=hf3fs_api_shared");
let bindings = bindgen::Builder::default()
    .header(PathBuf::from(topdir).join("../../api/hf3fs_usrbio.h").display().to_string())
    .clang_arg("-std=c99")
    ...
```

`lib/` 目錄裡只有一個 `.dummy` 佔位檔——真正的 `.so` 是 CMake 建置後 symlink 進去的（見 §1）。`README` 只有一句話：先建 `hf3fs_api_shared` 再 `cargo build`。

`src/lib.rs` 在生成的 binding 之上包了三個型別：

| Rust 型別 | 對應 | 特點 |
|---|---|---|
| `Iov` | `hf3fs_iov` | 只有 `wrap()`，且**自己建 symlink**（`lib.rs:39-45`），與 Python 一致；`unsafe impl Send + Sync` |
| `Ior` | `hf3fs_ior` | 只包 `iorcreate4`；`Drop` 呼叫 `hf3fs_iordestroy`；只 `unsafe impl Send`（不是 `Sync`，正確反映「單執行緒 prep」的約束） |
| `RegisteredFd` | `hf3fs_reg_fd` | 持有 `OwnedFd`，`Drop` 時 `hf3fs_dereg_fd`；**忽略註冊的回傳值**（`lib.rs:215`） |

`prepare<T>(...)` 把使用者的 `extra: T` 裝進 `Box<PreparedIo<T>>` 並 `Box::into_raw` 當作 `userdata`（`lib.rs:125-140`），`poll` 時 `Box::from_raw` 取回（`lib.rs:183`）。**這是正確的所有權轉移，但同樣有洩漏風險**：任何沒被 poll 回來的 IO，那個 Box 就永遠洩漏。而且 `Ior::drop` 不會回收這些 Box。

`poll()` 的逾時是相對毫秒轉絕對時間（`lib.rs:158-166`），並用 `transmute` + `__BindgenOpaqueArray` 繞過 bindgen 把 `timespec` 當不透明型別的問題——因為 header 沒有 `#include <time.h>` 的完整定義路徑，bindgen 只看到一個不透明結構。這段 `transmute` 假設了 `timespec` 就是兩個 `i64`（`lib.rs:238-242`），在 32-bit 平台上會錯。

`lib.rs:244-279` 有一個 `#[test] fn test_io()`，是全樹唯一的 USRBIO 單元測試，但它硬編 `/3fs/test` 路徑，需要真的掛載才能跑。

### 11.3 三種綁定的語意落差彙總

| | C | Python | Rust |
|---|---|---|---|
| Iov 建立 | `iovcreate`（自建 shm + symlink） | `iovwrap` + 手動 symlink | `iovwrap` + 手動 symlink |
| IO 長度 | 明確的 `len` 參數 | **由 slice 長度決定** | 由 `Range` 決定 |
| priority | `iorcreate2/3` 支援 | **接受但丟棄** | 不支援 |
| flags | `iorcreate4` 支援 | 支援 | 支援 |
| userdata 生命週期 | 呼叫者全權負責 | `inc_ref`/`dec_ref`，未收割即洩漏 | `Box` 轉移，未收割即洩漏 |
| 註冊失敗 | 回正的 errno | 拋 `OSError` | **完全忽略** |
| 結果表示 | `hf3fs_cqe` | 回填到 `iovec.result` | `PreparedIo::result` |

---

## 12. 最小可用範例

### 12.1 C：讀取（出處：`UsrbIo.md:244-275` + `benchmarks/fio_usrbio/hf3fs_usrbio.cpp`）

```c
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>
#include "hf3fs_usrbio.h"

#define NUM_IOS    64
#define BLOCK_SIZE (4u << 20)          /* 4 MiB */

int main(void) {
    const char *mp = "/hf3fs/mount/point";
    struct hf3fs_ior ior;
    struct hf3fs_iov iov;
    int fd, i, n;
    struct hf3fs_cqe cqes[NUM_IOS];

    /* 1) 建環：深度 NUM_IOS、讀向、io_depth=0（有多少做多少）、timeout=0、numa=-1、flags=0 */
    if (hf3fs_iorcreate4(&ior, mp, NUM_IOS, true, 0, 0, -1, 0) < 0) return 1;

    /* 2) 建資料緩衝：block_size=0 代表整塊一次 IB 註冊 */
    if (hf3fs_iovcreate(&iov, mp, (size_t)NUM_IOS * BLOCK_SIZE, 0, -1) < 0) return 1;

    /* 3) 開檔並註冊。注意：成功回傳 <= 0，失敗回傳正的 errno */
    fd = open("/hf3fs/mount/point/example.bin", O_RDONLY);
    if (fd < 0) return 1;
    if (hf3fs_reg_fd(fd, 0) > 0) return 1;

    /* 4) 逐一 prep。ptr 必須落在 [iov.base, iov.base + iov.size) 內 */
    for (i = 0; i < NUM_IOS; i++) {
        int idx = hf3fs_prep_io(&ior, &iov, /*read=*/true,
                                iov.base + (size_t)i * BLOCK_SIZE,
                                fd, (size_t)i * BLOCK_SIZE, BLOCK_SIZE,
                                /*userdata=*/NULL);
        if (idx < 0) { fprintf(stderr, "prep failed %d\n", idx); return 1; }
    }

    /* 5) 提示 FUSE 開工（可省略，FUSE 也會被其他事件喚醒） */
    hf3fs_submit_ios(&ior);

    /* 6) 收割。回傳值可能小於就緒數，必須迴圈 */
    for (int got = 0; got < NUM_IOS; ) {
        n = hf3fs_wait_for_ios(&ior, cqes, NUM_IOS, NUM_IOS - got, /*abs_timeout=*/NULL);
        if (n < 0) { fprintf(stderr, "wait failed %d\n", n); return 1; }
        for (i = 0; i < n; i++) {
            if (cqes[i].result < 0)
                fprintf(stderr, "io #%d failed: %s\n", cqes[i].index, strerror(-cqes[i].result));
            else if (cqes[i].result < BLOCK_SIZE)
                fprintf(stderr, "io #%d short read %lld (EOF or hole)\n",
                        cqes[i].index, (long long)cqes[i].result);
        }
        got += n;
    }

    /* 7) 拆解：dereg 必須早於 close */
    hf3fs_dereg_fd(fd);
    close(fd);
    hf3fs_iovdestroy(&iov);
    hf3fs_iordestroy(&ior);
    return 0;
}
```

編譯與連結（出處：`benchmarks/fio_usrbio/Makefile:11-12`）：

```
-I<3FS>/src/lib/api  -L<3FS>/build/src/lib/api  -lhf3fs_api_shared  -Wl,-rpath=<...>
```

### 12.2 C：寫入路徑的差異

寫入必須另開一個環（`for_read = false`），fio 插件就是這麼做的（`benchmarks/fio_usrbio/hf3fs_usrbio.cpp:79-89`）：

```cpp
hf3fs_iorcreate3(&ior_r, mountpoint, iodepth, /*for_read=*/true,  ior_depth, 0, ior_timeout, -1);
hf3fs_iorcreate(&ior_w,  mountpoint, iodepth, /*for_read=*/false, ior_depth, -1);
```

並且**兩個環共用同一個 Iov**（`hf3fs_usrbio.cpp:242-249`，只在 `iomem_alloc` 時建一次）。這是合法且推薦的：Iov 與 Ior 是正交的資源。

寫入前資料要先由使用者自己填進 Iov（`UsrbIo.md:7`：「all write data should be written to Iov by user first」）。寫入完成後**檔案長度不會立刻在 `stat` 上反映**，因為是靠 `dirtyInodes` + `periodicSync`（預設 30 秒，`FuseConfig.h:85`）背景寫回 meta。要立刻看到正確長度得呼叫 `force_fsync`（Python 綁定有暴露，`usrbio_binding.cc:90-99`，走 `HF3FS_IOC_FSYNC` ioctl）。

### 12.3 Python（出處：`hf3fs_fuse/fuse_demo.py`，逐行驗證過）

```python
from hf3fs_fuse.io import make_iovec, make_ioring, register_fd, deregister_fd
from multiprocessing.shared_memory import SharedMemory
import os

MP = '/hf3fs-cluster'

# 1) 自己配 shm，交給 make_iovec 去建 symlink 註冊
shm = SharedMemory(size=1024, create=True)
iov = make_iovec(shm, MP, 0, -1)      # shm, mountpoint, block_size, numa
shm.unlink()                          # symlink 建好後 /dev/shm 上的名字就不需要了

# 2) 建環
ior = make_ioring(MP, 100, True, 0)   # mountpoint, entries, for_read, io_depth

# 3) 開檔 + 註冊
fd = os.open(f'{MP}/testread', os.O_RDONLY)
register_fd(fd)

# 4) prepare：長度由 slice 決定，沒有 len 參數
ios = [(iov[:512], fd, 512), (iov[512:], fd, 0)]   # (iov slice, fd, 檔案位移)
for io in ios:
    ior.prepare(io[0], True, io[1], io[2], userdata=io)
    # userdata 必須是被外部持有的 Python 物件，否則會被 GC

# 5) 提交 + 等待（submit() 回傳 self，可鏈式呼叫）
resv = ior.submit().wait(min_results=2)
for res in resv:
    assert res.result == len(memoryview(res.userdata[0]))

# 6) 拆解
deregister_fd(fd)
os.close(fd)
```

### 12.4 Rust（出處：`src/lib/rs/hf3fs-usrbio-sys/src/lib.rs:249-278` 的測試）

```rust
use hf3fs_usrbio_sys::{Iov, Ior, RegisteredFd, HF3FS_IOR_FORBID_READ_HOLES};
use shared_memory::ShmemConf;

let shm = ShmemConf::new().os_id("/123").size(10 << 10).create().unwrap();
let iov = Iov::wrap("/3fs/test", &shm, 0).unwrap();          // 內部自己建 symlink

let ior = Ior::create("/3fs/test", /*for_read=*/true, /*entries=*/1000,
                      /*iodepth=*/0, /*timeout_ms=*/1000, /*numa=*/-1,
                      HF3FS_IOR_FORBID_READ_HOLES as _).unwrap();

let fd = RegisteredFd::open_and_register("/3fs/test/somefile").unwrap();
ior.prepare(&iov, 0..52, &fd, 0, (1u64, 2usize, 3usize)).unwrap();  // extra 泛型當 userdata
ior.submit();
let rs = ior.poll::<(u64, usize, usize)>(1..=1, 1000);              // (min..=max, timeout_ms)
assert_eq!(rs[0].result, 52);
```

### 12.5 fio 插件：批次的正確用法

`benchmarks/fio_usrbio/README.md:30-36` 給出了「跑小 IO 批次」的參數組合，這四個必須同時設成同一個值：

```
iodepth=1024
iodepth_batch_submit=1024
iodepth_batch_complete_min=1024
iodepth_batch_complete_max=1024
```

原因從插件實作看得很清楚：`hf3fs_usrbio_commit`（`hf3fs_usrbio.cpp:143-190`）是**同步**的——prep 完整批、submit、然後 `hf3fs_wait_for_ios(min_results = queued)` 一路等到全部完成才回傳。所以 fio 的 batch 參數如果小於 iodepth，就湊不出一個完整批次，USRBIO 的批次優勢就消失了。這個插件也示範了「讀寫方向切換時必須先排空」的處理（`hf3fs_usrbio.cpp:126-133`：`ddir` 改變且還有排隊中的 IO 就回 `FIO_Q_BUSY`）。

---

## 13. 可觀測性

`IoRing::process` 內建了一整組監控指標（`IoRing.cc:75-87`），全部帶 `mount_name` 標籤，並在 `addSample` 時附上 `{io: read|write}` 與 `{uid}`：

| 指標 | 型別 | 意義 |
|---|---|---|
| `usrbio.piov.overall` | Latency | 整個 batch 從取出到 cqe 寫完 |
| `usrbio.piov.prepare` | Latency | lookup + PioV 切 chunk 的時間 |
| `usrbio.piov.submit` | Latency | `batchRead`/`batchWrite` 的時間（真正的網路往返） |
| `usrbio.piov.complete` | Latency | 回填 cqe 的時間 |
| `usrbio.piov.io_size` | Distribution | 單筆 IO 大小 |
| `usrbio.piov.io_depth` | Distribution | **實際**批次大小（不是設定值） |
| `usrbio.piov.total_bytes` | Distribution | 每批總位元組 |
| `usrbio.piov.distinct_files` | Distribution | 每批涉及幾個不同檔案 |
| `usrbio.piov.distinct_bufs` | Distribution | 每批涉及幾個不同 Iov |
| `usrbio.piov.bw` | Count | 完成位元組數 |

`distinct_files` / `distinct_bufs` 這兩個指標的存在暴露了設計意圖：**批次的效率取決於同一批 IO 是否集中在少數檔案與少數 buffer 上**（因為 `lookupFiles`/`lookupBufs` 的相鄰去重、以及 chunk 合併都依賴這個局部性）。調參時如果發現 `distinct_files` 接近 `io_depth`，代表 IO 打得太散。

Iov 側的指標在 `IovTable::addIov`（`IovTable.cc:130-136`）：`fuse.iov.times`、`fuse.iov.bytes`、`fuse.iov.total_bytes`、`fuse.iov.latency.map`、`fuse.iov.bytes.ib_reg`、`fuse.iov.latency.ib_reg`。最後兩個是診斷「IB 註冊太慢」的直接依據，也是 `block_size` 這個參數該不該調的判準。

---

## 14. 設計評註

把整個 USRBIO 讀完，有幾個判斷值得記下來：

**用檔案系統當控制平面是聰明而非取巧的。** `symlink` 是一個原子的、帶權限檢查的、有 `unlink` 對稱操作的、能被 `ls` 觀察的動作。用它取代自訂 ioctl 的好處是：使用者程式不需要任何特殊權限、不需要知道 FUSE 的內部協定、清理路徑天然存在（`unlink`）、除錯時 `ls -l /mnt/3fs/3fs-virt/iovs/` 就能看到誰註冊了什麼。代價是參數只能編碼進檔名，於是有了 `.b1073741824.r0.t1.f10` 這種字串——以及 §10.3(a) 那個進位制不一致的 bug。

**「semaphore 只是提示，共享記憶體才是真相」貫穿全部。** 兩側的等待都是 `sem_timedwait(1ms)` 後重查，沒有任何一處依賴 semaphore 的計數正確性。這讓協定對「漏 post」「多 post」「進程崩潰時 post 丟失」全都免疫，代價是 1 ms 的輪詢底噪與最差 1 ms 的額外延遲。對於一個目標吞吐而非微秒延遲的系統，這是對的取捨。

**IoArgs 池與 sqe 環的分離值得學。** io_uring 是「SQE 陣列 + SQ 環（存索引）」，3FS 這裡是「IoArgs 池 + sqe 環（存索引）」，形式相同但動機不同：io_uring 是為了讓使用者能預先填好 SQE 再一次提交，3FS 是為了讓 cqe 只需要回傳一個 int32 索引、且參數位址在 IO 全程不動。

**這個設計把「誰擁有資源」切得很乾淨。** 槽位（`slots`）完全歸使用者、環的 shm 歸使用者建但 FUSE 也 map、IB MR 完全歸 FUSE、inode 引用歸 FUSE（靠使用者的 `dup`）。每一項都只有一個所有者，跨進程的協調只發生在四個 int32 上。

**最脆弱的地方是版本一致性。** 使用者程式連結的 `libhf3fs_api_shared.so` 與運行中的 `hf3fs_fuse_main` 必須來自同一次建置，因為它們共用 `IoRing.h` 的結構佈局，卻沒有任何 magic number 或版本欄位可供檢查。`IoRing` 建構子裡那個 `XLOGF_IF(FATAL, ...)` 只檢查 semaphore 的位址是否越界，擋不住欄位錯位。

---

## 15. 檔案索引表

### 15.1 `src/lib/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `src/lib/CMakeLists.txt` | 3 | 依序納入 `common`、`api`、`py` 三個子目錄 |
| `src/lib/api/CMakeLists.txt` | 9 | 建 `hf3fs_api`（靜態）與 `hf3fs_api_shared`（動態），並把 `.so` symlink 給 Rust 綁定 |
| `src/lib/api/hf3fs_usrbio.h` | 173 | **唯一對外散佈的公開 C header**：`hf3fs_iov`/`hf3fs_ior`/`hf3fs_cqe` 三個結構與全部 `hf3fs_*` 函式宣告 |
| `src/lib/api/UsrbIo.cc` | 808 | **共享庫本體**：掛載點探測、Iov/Ior 建立銷毀、fd 註冊表、prep/submit/wait、hardlink/punchhole 的 ioctl 包裝 |
| `src/lib/api/UsrbIo.md` | 276 | 官方 API 參考文件（英文），README 直接連向它 |
| `src/lib/api/fuse.h` | 59 | FUSE ioctl 的命令碼與參數結構（`HF3FS_IOC_*`、`Hf3fsIoctlHardlinkArg` 等），使用者態與 FUSE 端共用 |
| `src/lib/api/hf3fs.h` | 175 | 舊「client agent」的完整 POSIX 介面 `IClient` 抽象類別；**目前無實作、無使用者**，只有 `HF3FS_SUPER_MAGIC` 巨集還被引用 |
| `src/lib/api/hf3fs_expected.h` | 2031 | 內嵌的 `expected-lite`（Martin Moene，Boost 授權），供 `hf3fs.h` 的 `Result<T>` 使用 |
| `src/lib/common/CMakeLists.txt` | 1 | 建 `client-lib-common`，連結 `common`、`numa`、`rt` |
| `src/lib/common/Shm.h` | 103 | `IorAttrs`（priority/timeout/flags）、`ShmBuf`（shm 生命週期 + IB 註冊）、`ShmBufForIO`（帶位移的視圖，含跨 block 檢查）宣告 |
| `src/lib/common/Shm.cc` | 177 | `ShmBuf` 實作：`shm_open`+`ftruncate`+`mmap`、`numa_tonode_memory`、分段 `registerIOBuffer`、以 `folly::coro::Baton` 同步的非同步註冊 |
| `src/lib/common/PerProcTable.h` | 226 | `PerProcTable`（以 pid/ppid 為鍵的 fd 風格表，支援 fork 時繼承父表）與 `AllProcMap`；服務於舊 client agent 路徑 |
| `src/lib/common/paths.h` | 5 | 單一常數 `varTmpPath = "/var/tmp/hf3fs_client_agent"` |
| `src/lib/py/CMakeLists.txt` | 14 | `pybind11_add_module(hf3fs_py_usrbio usrbio_binding.cc)`，連結 `hf3fs_api_shared` |
| `src/lib/py/usrbio_binding.cc` | 471 | **實際建置的 Python 綁定**：`iovec`（buffer protocol + 切片）、`ioring`（prepare/submit/wait）、`register_fd`/`punch_hole`/`hardlink` 等模組級函式 |
| `src/lib/py/binding.cc` | 496 | 舊 `IClient` 的 pybind11 綁定；`#include "lib/api/Client.h"` 而該檔不存在，**不在建置目標內，已是死碼** |
| `src/lib/rs/hf3fs-usrbio-sys/Cargo.toml` | 11 | crate 定義；依賴 `shared_memory`、`uuid`，建置期依賴 `bindgen` |
| `src/lib/rs/hf3fs-usrbio-sys/build.rs` | 20 | 對 `hf3fs_usrbio.h` 跑 bindgen 產生 `bindings.rs`，並指示連結 `hf3fs_api_shared` |
| `src/lib/rs/hf3fs-usrbio-sys/src/lib.rs` | 279 | 安全包裝：`Iov`（含自建 symlink）、`Ior`（RAII 銷毀）、`RegisteredFd`（RAII 反註冊）、`PreparedIo<T>` 泛型 userdata、唯一的單元測試 |
| `src/lib/rs/hf3fs-usrbio-sys/README` | 1 | 提示必須先建 CMake target 才能 `cargo build` |
| `src/lib/rs/hf3fs-usrbio-sys/lib/.dummy` | 0 | 佔位檔，讓 `lib/` 目錄能進版控；`.so` 由 CMake symlink 進來 |

### 15.2 對側與周邊（本報告引用到的關鍵檔案）

| 檔案 | 職責（限於 USRBIO 相關部分） |
|---|---|
| `src/fuse/IoRing.h` | `IoArgs`/`IoSqe`/`IoCqe`/`IoRing`/`IoRingTable` 定義；環的尺寸計算與記憶體佈局；submit semaphore 的建立與 symlink 發布 |
| `src/fuse/IoRing.cc` | `jobsToProc`（批次與逾時策略，593cec5 修的地方）、`process`（lookup → PioV → 回填 cqe → 順序推進 sqeTail） |
| `src/fuse/IovTable.h/.cc` | symlink 檔名解析（`parseKey`）、iov 的加入/移除/查詢/列舉、以 uuid 反查 shm 的索引 |
| `src/fuse/PioV.h/.cc` | 把「檔案位移+長度」依 layout 切成 chunk IO（`chunkIo`）、批次執行、洞與短讀的結果合併（`concatIoRes`） |
| `src/fuse/FuseClients.h/.cc` | io worker 協程池、三優先級 job queue、`watch` 執行緒（sem + 1 ms 輪詢）、`lookupFiles`/`lookupBufs` |
| `src/fuse/FuseOps.cc` | `hf3fs_symlink`（註冊入口）、`hf3fs_unlink`（銷毀入口）、`hf3fs_releasedir`（進程死亡清理）、`hf3fs_read`（對照組：FUSE 原生路徑） |
| `src/fuse/FuseConfig.h` | `iov_limit`、`batch_io_coros`、`io_jobq_sizes`、`submit_wait_jitter`、`max_jobs_per_ioring`、`chunk_size_limit` 等調參項 |
| `src/fbs/meta/Common.h` | `InodeId::iov(iovd)` / `iovDir()` 的保留位址空間 |
| `src/common/utils/AtomicSharedPtrTable.h` | `AvailSlots`（槽位配發）與 `AtomicSharedPtrTable`（iov/ior 表） |
| `benchmarks/fio_usrbio/hf3fs_usrbio.cpp` | fio 外掛，**最完整的 C 語言實戰範例**（雙環、共用 Iov、方向切換） |
| `benchmarks/fio_usrbio/README.md` / `Makefile` | 建置方式與批次參數建議 |
| `hf3fs_fuse/io.py` | Python 薄封裝：`make_iovec`/`make_ioring`/`read_file` |
| `hf3fs_fuse/fuse_demo.py` | 30 行的 Python 最小範例 |
| `setup.py` | CMake 驅動的 `hf3fs_py_usrbio` wheel 打包（硬編 clang-14） |
| `setup_hf3fs_utils.py` / `hf3fs_utils/` | 與 USRBIO 無關的 `hf3fs_cli` 命令列工具（rmtree/mv/trash），走 ioctl 而非 USRBIO |
