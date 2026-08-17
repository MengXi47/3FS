# 3FS `hf3fs_fuse_main`（FUSE 客戶端）深度剖析

> 對應原始碼：`src/fuse/`（全部 25 個檔案）、進入點 `src/fuse/hf3fs_fuse.cpp`
> 邊界依賴：`src/client/meta/MetaClient.h`、`src/client/storage/StorageClient.h`、`src/lib/api/`（USRBIO）、`src/lib/common/Shm.h`
> 設定檔：`configs/hf3fs_fuse_main.toml`、`configs/hf3fs_fuse_main_launcher.toml`、`configs/hf3fs_fuse_main_app.toml`
> 頂層 Python 綁定：`hf3fs_fuse/`（`io.py`、`fuse.py`、`fuse_demo.py`）

---

## 0. 一句話總結

`hf3fs_fuse_main` 是一個**故意只做一半的 FUSE 檔案系統**：它用 libfuse low-level API 把 POSIX 命名空間完整接上 MetaClient，但**明確承認 FUSE 的資料通道打不到 SSD/RDMA 的極限**，於是在同一個 daemon 內部另外開了一條完全不經過核心的旁路——用 `/dev/shm` 共享記憶體實作的 **io_uring 仿製品（IoRing/USRBIO）**，把「檔案語意」留給 FUSE、把「資料搬運」抽走給共享記憶體環。整份程式碼的所有複雜度，幾乎都源自「同一個 inode 同時被 FUSE 路徑與旁路 IO 路徑寫入，長度要一致」這一個問題。

---

## 1. binary 啟動流程

### 1.1 兩套 main：編譯期二選一

`src/fuse/hf3fs_fuse.cpp` 整個檔案被一個 `#ifdef ENABLE_FUSE_APPLICATION` 切成兩份完全不同的 `main()`：

```cpp
// src/fuse/hf3fs_fuse.cpp:1
#ifdef ENABLE_FUSE_APPLICATION
#include "FuseApplication.h"
int main(int argc, char *argv[]) {
  gflags::AllowCommandLineReparsing();
  return fuse::FuseApplication().run(argc, argv);
}
#else
// ... 手工版 main
#endif
```

這個巨集由 `src/fuse/CMakeLists.txt:10` 控制：

```cmake
target_add_lib(hf3fs_fuse common core-app meta-client storage-client fuse3 client-lib-common)
target_add_bin(hf3fs_fuse_main hf3fs_fuse.cpp hf3fs_fuse)
if (ENABLE_FUSE_APPLICATION)
target_compile_definitions(hf3fs_fuse PUBLIC -DENABLE_FUSE_APPLICATION)
endif()
```

兩者的差別不只是包裝：

| | 舊路徑（未定義巨集） | 新路徑（`ENABLE_FUSE_APPLICATION`） |
|---|---|---|
| 設定來源 | 純本地 toml（`FuseConfig::init(&argc,&argv)`） | 走 `core::ServerLauncher`，可從 **mgmtd 拉遠端設定** |
| `FuseConfig` 頂層欄位 | 自帶 `cluster_id/token_file/mountpoint/log/monitor/ib_devices` | 換成 `CONFIG_OBJ(common, ApplicationBase::Config)`，另有 `FuseLauncherConfig` |
| IBManager / logging / Monitor | main 手動逐項啟動（`hf3fs_fuse.cpp:44-54`） | `app_detail::initCommonComponents()` 統包 |
| 熱更新設定 | 無 | `mgmtdClient->setConfigListener(ApplicationBase::updateConfig)` |

`FuseConfig.h:12-22` 就是這個分岔的證據——同一個 struct 用 `#ifdef` 換掉了頂層欄位集合：

```cpp
// src/fuse/FuseConfig.h:12
#ifdef ENABLE_FUSE_APPLICATION
  CONFIG_OBJ(common, ApplicationBase::Config);
#else
  CONFIG_ITEM(cluster_id, "");
  CONFIG_ITEM(token_file, "");
  CONFIG_ITEM(mountpoint, "");
  ...
#endif
```

**為什麼保留兩份**：舊 main 是自足的，不需要 mgmtd 就能起（設定全在本地檔），對開發/測試/單機 debug 有價值；新路徑則把 FUSE 客戶端變成叢集裡的一等公民，設定可被 admin 從 mgmtd 推下來。`configs/hf3fs_fuse_main_launcher.toml` 與 `configs/hf3fs_fuse_main_app.toml` 的存在（前者含 `mgmtd_client.mgmtd_server_addresses`）說明生產部署走的是新路徑。

### 1.2 新路徑逐層呼叫鏈

```
main()                                          hf3fs_fuse.cpp:8
└─ FuseApplication().run(argc, argv)            [ApplicationBase::run]
   ├─ parseFlags()                              FuseApplication.cc:43
   │  ├─ launcher_->parseFlags()                 ← 解析 --launcher_cfg 等
   │  └─ ApplicationBase::parseFlags("--config.", …)  ← 收集動態覆寫
   ├─ initApplication()                         FuseApplication.cc:55
   │  ├─ [--dump_default_cfg] 印設定後 exit(0)
   │  ├─ launcher_->init()                       ← 建 mgmtd stub、連線
   │  ├─ app_detail::loadAppInfo(launcher_->loadAppInfo(), appInfo)
   │  │     └─ FuseConfigFetcher::completeAppInfo()   FuseConfigFetcher.cc:8
   │  │        └─ mgmtdClient_->getUniversalTags(hostname)  ← 拉機器標籤
   │  ├─ app_detail::initConfig(hf3fsConfig, flags, appInfo, loadConfigTemplate())
   │  ├─ app_detail::initCommonComponents(common(), "Fuse", nodeId)
   │  │     └─ IB / logging / monitor / memory
   │  ├─ makeLogConfigUpdateCallback / makeMemConfigUpdateCallback  ← 熱更新掛勾
   │  ├─ app_detail::persistConfig()
   │  ├─ initFuseClients()                      FuseApplication.cc:86
   │  │     └─ FuseClients::init(appInfo, mountpoint, tokenFile, cfg)
   │  └─ launcher_.reset()                       ← 用完即丟，釋放 launcher 的 net::Client
   └─ mainLoop()                                FuseApplication.cc:115
      ├─ Thread::unblockInterruptSignals()
      └─ fuseMainLoop(programName, allowOther, mountpoint, maxBufSize, clusterId)
```

`launcher_.reset()`（`FuseApplication.cc:81`）值得注意：launcher 自帶一份獨立的 `net::Client` 與 mgmtd stub，只用來拉設定；設定拉完立刻整個銷毀，避免長期佔著一組 RDMA/TCP 連線。

### 1.3 `fuseMainLoop`：掛載參數與事件迴圈

`src/fuse/FuseMainLoop.cc:9` 是唯一碰 libfuse 的地方。它**不解析使用者傳進來的 argv**，而是自己組一份 fuse args：

```cpp
// src/fuse/FuseMainLoop.cc:25-41
fuseArgs.push_back(programName);
if (allowOther) {
  fuseArgs.push_back("-o"); fuseArgs.push_back("allow_other");
  fuseArgs.push_back("-o"); fuseArgs.push_back("default_permissions");
}
fuseArgs.push_back("-o"); fuseArgs.push_back("auto_unmount");
fuseArgs.push_back("-o"); fuseArgs.push_back(fmt::format("max_read={}", maxbufsize));
fuseArgs.push_back(mountpoint);
fuseArgs.push_back("-o"); fuseArgs.push_back("subtype=hf3fs");
fuseArgs.push_back("-o"); fuseArgs.push_back("fsname=hf3fs." + clusterId);
```

四個掛載選項各有明確用意：

| 選項 | 意義 | 為什麼 |
|---|---|---|
| `allow_other` | 允許非掛載者存取 | 多使用者共享同一掛載點；預設 `true`（`FuseConfig.h:18`） |
| `default_permissions` | **由核心做權限檢查** | 與 `allow_other` 綁在一起：既然開放所有人，就必須讓核心用 inode 的 mode/uid/gid 先擋一層；伺服器端仍會再檢一次 |
| `auto_unmount` | daemon 死掉時自動 umount | 避免留下 `Transport endpoint is not connected` 的殭屍掛載 |
| `max_read=1MB` | 單次 READ 上限 | 與 `io_bufs.max_buf_size` 一致，見 §9.1 |
| `fsname=hf3fs.<clusterId>` | 讓 `/proc/self/mountinfo` 帶叢集名 | USRBIO 的 `hf3fs_extract_mount_point()` 靠它判斷掛載點（`UsrbIo.cc:74` 檢查 `fuse.hf3fs`） |

`subtype=hf3fs` 使 mountinfo 中 fstype 顯示為 `fuse.hf3fs`——這正是 `src/lib/api/UsrbIo.cc:74` 掃描 `/proc/self/mountinfo` 時比對的字串。`FuseClients::init` 還特地檢查掛載名長度：

```cpp
// src/fuse/FuseClients.cc:57
XLOGF_IF(FATAL, fuseMount.size() >= 32,
         "FUSE only support mount name shorter than 32 characters, but {} got.", fuseMount);
```

因為 `Hf3fsIoctlGetMountNameArg` 的緩衝就是 `char str[32]`（`src/lib/api/fuse.h:15`），ioctl 回傳時用 `strcpy` 硬拷（`FuseOps.cc:1985`）——溢位就是記憶體毀損，所以在啟動時就 FATAL。

事件迴圈本身很標準：

```cpp
// src/fuse/FuseMainLoop.cc:96-103
if (opts.singlethread) {
  ret = fuse_session_loop(d.se);
} else {
  fuse_loop_cfg_set_clone_fd(config, opts.clone_fd);
  fuse_loop_cfg_set_idle_threads(config, d.maxIdleThreads);   // 預設 -1 = 不回收
  fuse_loop_cfg_set_max_threads(config, d.maxThreads);
  ret = fuse_session_loop_mt(d.se, config);
}
```

`FUSE_USE_VERSION 312`（`FuseClients.h:30`）→ libfuse 3.12 API，`fuse_loop_cfg_*` 系列是 3.12 才有的新設定介面（取代舊的 `struct fuse_loop_config` 直接賦值）。

清理用一個 `std::stack<std::function<void()>>` 反向執行（`FuseMainLoop.cc:17-23`），保證 `unmount → remove_signal_handlers → session_destroy → free args` 的正確順序。

---

## 2. 整體分層架構

```
┌───────────────────────────────────────────────────────────────────────────┐
│ 應用程式                                                                    │
│   一般路徑: open/read/write/stat/readdir …                                  │
│   旁路路徑: hf3fs_iorcreate4 / hf3fs_prep_io / hf3fs_submit_ios / wait_for_ios│
└───────┬───────────────────────────────────────────────┬───────────────────┘
        │ glibc syscall                                 │ 純使用者態
        ▼                                               │ （只有 sem_post/wait 進核心）
┌───────────────────────┐                               │
│ Kernel VFS            │                               │
│  dcache / icache      │                               │
│  page cache           │                               │
└───────┬───────────────┘                               │
        ▼                                               │
┌───────────────────────┐                               │
│ fuse.ko               │                               │
│  /dev/fuse 請求佇列    │                               │
│  （單一 spinlock，     │                               │
│    ~400K IOPS 天花板） │                               │
└───────┬───────────────┘                               │
        │ FUSE_LOOKUP / READ / WRITE …                  │
        ▼                                               ▼
┌───────────────────────────────────────────────────────────────────────────┐
│ hf3fs_fuse_main (使用者態 daemon)                                           │
│                                                                            │
│  ┌──────────────────────────┐        ┌────────────────────────────────┐   │
│  │ fuse_lowlevel_ops         │        │ IoRing 旁路                     │   │
│  │  hf3fs_oper (FuseOps.cc)  │        │  watch 執行緒 ×3 (prio 0/1/2)   │   │
│  │  libfuse worker 執行緒池   │        │  → BoundedQueue<IoRingJob> ×3   │   │
│  │  (max_threads)            │        │  → ioRingWorker 協程 ×128       │   │
│  └────────┬──────────┬───────┘        └───────────────┬────────────────┘   │
│           │          │                                │                    │
│           │          └──────────┬─────────────────────┘                    │
│           ▼                     ▼                                          │
│  ┌────────────────┐   ┌───────────────────────────┐                        │
│  │ MetaClient      │   │ PioV (parallel IO vector) │                        │
│  │ (RPC to meta)   │   │  檔案 offset → chunk 切分  │                        │
│  └────────┬────────┘   └────────────┬──────────────┘                        │
│           │                          ▼                                      │
│           │            ┌───────────────────────────┐                        │
│           │            │ StorageClient             │                        │
│           │            │ batchRead / batchWrite    │                        │
│           │            └────────────┬──────────────┘                        │
│           │                          │                                      │
│  ┌────────▼──────────────────────────▼───────────────────────────────┐     │
│  │ FuseClients（全域單例 `d`）                                          │     │
│  │  inodes map / IovTable / IoRingTable / UserConfig / dirtyInodes    │     │
│  │  MgmtdClientForClient（路由表 + client session）                     │     │
│  └────────────────────────────┬──────────────────────────────────────┘     │
└───────────────────────────────┼────────────────────────────────────────────┘
                                │ RDMA / TCP
                    ┌───────────┴───────────┐
                    ▼                       ▼
             ┌─────────────┐        ┌──────────────┐
             │ meta_main    │        │ storage_main │
             │ (+ FDB)      │        │ (CRAQ 鏈)     │
             └─────────────┘        └──────────────┘
```

關鍵觀察：**兩條路徑共用同一個 `FuseClients` 單例、同一份 `inodes` map、同一個 `PioV`、同一個 `StorageClient`**。旁路不是另一個 daemon，而是同一個進程裡的另一個入口。這是為什麼旁路 IO 仍能維持長度一致性（§7）——它們共享 `RcInode::DynamicAttr`。

---

## 3. FuseClients：共享狀態與初始化

### 3.1 全域單例

```cpp
// src/fuse/FuseOps.cc:55
FuseClients d;
FuseClients &getFuseClientsInstance() { return d; }
```

一個檔案作用域的全域物件，變數名叫 `d`。整份 `FuseOps.cc` 2716 行到處都是 `d.metaClient`、`d.config`、`d.inodes`。這是純粹的實用主義：`fuse_lowlevel_ops` 的 callback 是 C 函式指標，拿不到 `this`；libfuse 雖然有 `userdata`，但每個 callback 都要 `fuse_req_userdata(req)` 再轉型，寫起來比全域還醜。

### 3.2 `FuseClients` 欄位總覽

`src/fuse/FuseClients.h:179-242`，按職責分組：

| 群組 | 欄位 | 說明 |
|---|---|---|
| RPC 客戶端 | `client` / `mgmtdClient` / `storageClient` / `metaClient` | 四層 client 疊起來 |
| 掛載身分 | `fuseToken` / `fuseMount` / `fuseMountpoint` / `fuseRemountPref` | token 從 env 或檔案讀 |
| inode 表 | `inodes` + `inodesMutex` | 全域 `unordered_map<InodeId, shared_ptr<RcInode>>` |
| readdir 快取 | `readdirplusResults` + mutex、`dirHandle` 計數器 | 以 dirId 為 key |
| 旁路 | `iovs`(IovTable) / `iors`(IoRingTable) / `iojqs`(3 條佇列) / `ioWatches`(3 條執行緒) / `cancelIos` | §8 |
| 動態設定 | `userConfig` | §10 |
| 背景同步 | `dirtyInodes` / `lastSynced` / `periodicSyncRunner` / `periodicSyncWorker` | §11.3 |
| 其他 | `bufPool`(RDMA buf pool) / `notifyInvalExec` / `se`(fuse_session) / `jitter` | |

`inodes` 的初始值直接把根 inode 塞進去，refcount 給 2：

```cpp
// src/fuse/FuseClients.h:211
std::unordered_map<InodeId, std::shared_ptr<RcInode>> inodes = {
    {InodeId::root(), std::make_shared<RcInode>(Inode{}, 2)}};
```

refcount 2 而非 1，是為了讓根永遠不可能被 `forget` 掉到 0 而被 erase（核心對根也會做 lookup/forget）。

### 3.3 `FuseClients::init` 的順序

`src/fuse/FuseClients.cc:50-176`，順序不能亂：

```
1. config 指標保存 + 掛載名長度檢查 + remount_prefix 正規化
2. token：優先讀 env HF3FS_FUSE_TOKEN，否則讀 token_file       ← FuseClients.cc:68
3. maxThreads = min(config.max_threads, (logicalCores+1)/2)   ← FuseClients.cc:82
4. bufPool = RDMABufPool::create(max_buf_size, rdma_buf_pool_size)
5. iovs.init(remountPref ?: mountpoint, iov_limit)
   iors.init(iov_limit)                                        ← 順手建 3 個 POSIX semaphore
   userConfig.init(config)
6. net::Client 起來
7. MgmtdClientForClient 建立 + setClientSessionPayload(NodeType::FUSE)
8. mgmtdClient->start() → refreshRoutingInfo(false) → establishClientSession()
9. StorageClient::create(clientId, cfg, mgmtd)
10. MetaClient(clientId, cfg, stubFactory, mgmtd, storage, dynStripe=true)
11. 3 條 IoRingJob 佇列（hi=32 / normal=1024 / lo=4096）
12. batch_io_coros(128) 個 ioRingWorker 協程 scheduleOn(bgThreadPool)
13. 3 條 watch jthread（每個優先級一條）
14. periodicSyncWorker(CoroutinesPool) + periodicSyncRunner(BackgroundRunner)
15. onFuseConfigUpdated 熱更新掛勾（memset_before_read / submit_wait_jitter）
16. notifyInvalExec = IOThreadPoolExecutor(notify_inval_threads=32)
```

**`maxThreads` 的上限計算特別值得看**（`FuseClients.cc:80-85`）：

```cpp
int logicalCores = std::thread::hardware_concurrency();
if (logicalCores != 0) {
  maxThreads = std::min(fuseConfig.max_threads(), (logicalCores + 1) / 2);
}
```

設定檔預設 `max_threads = 256`，但實際被砍到「邏輯核心數的一半」。理由在 `docs/design_notes.md:31` 說得很白：FUSE 的請求佇列受單一 spinlock 保護，執行緒開太多只會加劇鎖競爭、CPU 全耗在 spin 上。與其讓使用者踩坑，不如在程式碼裡硬性限制，把另一半核心留給旁路 IO 的協程與 RDMA 完成處理。

`establishClientSession` 用了 40 次指數退避重試（`FuseClients.cc:36-42`），初始 10ms、封頂 1s——client session 是 mgmtd 判斷 client 存活的依據，拿不到就不該繼續掛載。

### 3.4 `RcInode`：引用計數的 inode 快取

```cpp
// src/fuse/FuseClients.h:67
struct RcInode {
  struct DynamicAttr {
    uint64_t written = 0;    // 每次 finishWrite 遞增
    uint64_t synced = 0;     // 最後一次 period sync 對應的 written 值
    uint64_t fsynced = 0;    // 最後一次 fsync/close/truncate 對應的 written 值
    flat::Uid writer{0};
    uint32_t dynStripe = 1;
    uint64_t truncateVer = 0;
    std::optional<meta::VersionedLength> hintLength;
    std::optional<UtcTime> atime, mtime;
  };
  Inode inode;
  int refcount;                              // ← 由 inodesMutex 保護，不是 atomic
  std::atomic<int> opened;                   // ← 目前實際上沒人用（呼叫處被註解掉）
  std::mutex wbMtx;
  std::shared_ptr<InodeWriteBuf> writeBuf;
  folly::Synchronized<DynamicAttr> dynamicAttr;
  folly::coro::Mutex extendStripeLock;
};
```

三個版本號 `written / synced / fsynced` 是整個長度一致性機制的核心，見 §7.3。

注意 `opened` 是 `std::atomic<int>` 但唯一的使用點（`FuseOps.cc:1441-1447`、`1754`）全被註解掉了——曾經打算在「第二個人 open 同一檔案時強制 sync」，後來放棄。

---

## 4. `fuse_lowlevel_ops` 完整方法表

`src/fuse/FuseOps.cc:2580-2613` 是完整的 ops 表。用 designated initializer 逐項填，沒填的欄位自動為 `nullptr`（= 未實作）。

### 4.1 已實作

| callback | 實作函式 | 行號 | 主要後端呼叫 |
|---|---|---|---|
| `init` | `hf3fs_init` | 321 | 純本地：協商 conn capability |
| `destroy` | `hf3fs_destroy` | 350 | **空函式** |
| `lookup` | `hf3fs_lookup` | 644 | `metaClient->stat(parent, name, false)` |
| `forget` | `hf3fs_forget` | 401 | 純本地：`remove_entry(ino, nlookup)` |
| `getattr` | `hf3fs_getattr` | 732 | `metaClient->stat(ino, nullopt, false)` |
| `setattr` | `hf3fs_setattr` | 808 | `setPermission` / `truncate` / `utimes`（可一次做三段） |
| `readlink` | `hf3fs_readlink` | 909 | **純本地**：讀 `inodes` map 裡的 symlink target |
| `mknod` | `hf3fs_mknod` | 971 | `metaClient->create(..., O_RDONLY)` |
| `mkdir` | `hf3fs_mkdir` | 1008 | `metaClient->mkdirs(..., recursive=false)` |
| `unlink` | `hf3fs_unlink` | 1039 | `metaClient->remove(..., recursive=false)` |
| `rmdir` | `hf3fs_rmdir` | 1085 | `metaClient->remove(..., recursive=false)` ← **與 unlink 同一個 RPC** |
| `symlink` | `hf3fs_symlink` | 1160 | `metaClient->symlink`，或虛擬目錄的四種特異行為 |
| `rename` | `hf3fs_rename` | 1321 | `metaClient->rename` |
| `link` | `hf3fs_link` | 1379 | `metaClient->hardLink` |
| `open` | `hf3fs_open` | 1418 | 寫模式才 `metaClient->open(session, flags)` |
| `read` | `hf3fs_read` | 1473 | `PioV::addRead` + `executeRead` |
| `write` | `hf3fs_write` | 1552 | 寫緩衝 or `flushBuf` |
| `flush` | `hf3fs_flush` | 1704 | **直接轉呼叫 `hf3fs_fsync(datasync=false)`** |
| `release` | `hf3fs_release` | 1737 | 寫模式才 `metaClient->close(session)` |
| `fsync` | `hf3fs_fsync` | 1691 | `flushAndSync(SyncType::Fsync)` |
| `opendir` | `hf3fs_opendir` | 1776 | 純本地：發一個遞增 dirId |
| `releasedir` | `hf3fs_releasedir` | 1792 | 純本地：清 readdirplus 快取 + **清理死進程的 iov** |
| `statfs` | `hf3fs_statfs` | 1837 | `metaClient->statFs()` |
| `setxattr` | `hf3fs_setxattr` | 2376 | 只支援 `hf3fs.lock` → `lockDirectory` |
| `getxattr` | `hf3fs_getxattr` | 2440 | 只支援 `hf3fs.lock` |
| `listxattr` | `hf3fs_listxattr` | 2492 | 只可能回一個 `hf3fs.lock` |
| `removexattr` | `hf3fs_removexattr` | 2540 | `lockDirectory(Clear)` |
| `create` | `hf3fs_create` | 1860 | `metaClient->create(session, perm, flags)` |
| `ioctl` | `hf3fs_ioctl` | 1908 | 12 種 cmd，見 §4.3 |
| `readdirplus` | `hf3fs_readdirplus` | 2173 | `metaClient->list` + `batchStat` |

### 4.2 明確未實作（含被註解掉的）

| callback | 狀態 | 後果 |
|---|---|---|
| `readdir` | **`//    .readdir = hf3fs_readdir,`**（`FuseOps.cc:2602`） | 只提供 readdirplus。libfuse 的 `do_init` 在「有 readdirplus 但沒有 readdir」時會設定 `FUSE_CAP_READDIRPLUS` 且清掉 `FUSE_CAP_READDIRPLUS_AUTO`，於是核心一律走 READDIRPLUS 路徑（本 repo 未 vendor libfuse，此點依 libfuse 語意推得）。代價：即使 `ls` 只要名字，也一定會付出 `batchStat` 的代價 |
| `fsyncdir` | **`//.fsyncdir = hf3fs_fsyncdir,`**（`FuseOps.cc:2604`） | 目錄無需 fsync（元資料在 FDB 交易裡已持久） |
| `access` | 未列 | 靠 `-o default_permissions` 讓核心用 attr 判斷 |
| `getlk` / `setlk` / `flock` | 未列 | **POSIX 檔案鎖完全不支援**；`flock()` 會由核心自行處理成本地鎖（跨節點不生效） |
| `bmap` | 未列 | 非 block device fs，本來就不適用 |
| `poll` | 未列 | 無 |
| `write_buf` / `retrieve_reply` | 未列 | 不用 splice 收寫入資料，儘管 §4.4 有開 `FUSE_CAP_SPLICE_WRITE` |
| `forget_multi` | 未列 | 只有單筆 `forget`；大量 forget 時會多幾次 round-trip |
| `fallocate` | 未列 | `fallocate()` 回 `ENOSYS`；打洞改走自訂 ioctl（`HF3FS_IOC_PUNCH_HOLE`） |
| `copy_file_range` | 未列 | 退化成 read+write |
| `lseek` | 未列 | `SEEK_HOLE/SEEK_DATA` 不支援 |

### 4.3 `ioctl`：私有 API 的集散地

`src/lib/api/fuse.h:44-56` 定義了 12 個 ioctl（含 4 個標準的 `FS_IOC_*`）：

| cmd | 用途 | 實作位置 |
|---|---|---|
| `FS_IOC_GETFLAGS` | 取 inode flags（immutable 等） | `FuseOps.cc:1932` |
| `FS_IOC_SETFLAGS` | 設 inode flags → `metaClient->setIFlags` | `FuseOps.cc:1947` |
| `FS_IOC_FSGETXATTR` | 只映射 `FS_IMMUTABLE_FL → FS_XFLAG_IMMUTABLE` | `FuseOps.cc:1961` |
| `HF3FS_IOC_GET_MOUNT_NAME` | 回 clusterId（≤32 bytes） | `FuseOps.cc:1979` |
| `HF3FS_IOC_GET_PATH_OFFSET` | 回掛載點字串長度 | `FuseOps.cc:1990` |
| `HF3FS_IOC_GET_MAGIC_NUM` | 回 `HF3FS_SUPER_MAGIC` | `FuseOps.cc:2000` |
| `HF3FS_IOC_GET_IOCTL_VERSION` | 回 `1`；讓 client 探測支援度 | `FuseOps.cc:2010` |
| `HF3FS_IOC_RECURSIVE_RM` | `getRealPath` → `remove(recursive=true)` | `FuseOps.cc:2021` |
| `HF3FS_IOC_FSYNC` | **強制** sync（`SyncType::ForceFsync`）+ inval inode | `FuseOps.cc:2041` |
| `HF3FS_IOC_HARDLINK` | 用 inode 號 + 名字建硬連結（繞過路徑解析） | `FuseOps.cc:2051` |
| `HF3FS_IOC_PUNCH_HOLE` | **直接對 StorageClient 下 `removeChunks`** | `FuseOps.cc:2063` |
| `HF3FS_IOC_MOVE` | rename + 可選 moveToTrash | `FuseOps.cc:2108` |
| `HF3FS_IOC_REMOVE` | remove + 可選 recursive | `FuseOps.cc:2140` |

`HF3FS_IOC_PUNCH_HOLE` 是唯一一個**繞過 MetaClient 直接操作 storage** 的路徑（`FuseOps.cc:2088`）：

```cpp
auto removeOp = d.storageClient->createRemoveOp(chainId,
                                                storage::ChunkId(*chunkId),
                                                storage::ChunkId(storage::ChunkId(*chunkId), 1));
```

它要求 start/end 必須 chunk 對齊（`FuseOps.cc:2072`，否則 `EINVAL`），因為它是整 chunk 刪除而非部分清零。元資料端的 `length` 不會變——打完洞後檔案長度不變，中間讀到的是 hole（`concatIoRes` 會零填，見 §6.4）。

`fuse_reply_ioctl_retry` 的用法（例如 `FuseOps.cc:1935`）是 libfuse 的兩階段 ioctl 協定：第一次呼叫時核心不知道要搬多少資料，daemon 回一個 iovec 描述，核心再發第二次帶好緩衝。

### 4.4 `init`：能力協商

```cpp
// src/fuse/FuseOps.cc:321-348
if (d.enableWritebackCache && (conn->capable & FUSE_CAP_WRITEBACK_CACHE)) conn->want |= FUSE_CAP_WRITEBACK_CACHE;
if (conn->capable & FUSE_CAP_SPLICE_WRITE) conn->want |= FUSE_CAP_SPLICE_WRITE;
if (conn->capable & FUSE_CAP_SPLICE_READ)  conn->want |= FUSE_CAP_SPLICE_READ;
if (conn->capable & FUSE_CAP_SPLICE_MOVE)  conn->want |= FUSE_CAP_SPLICE_MOVE;

d.maxBufsize      = std::min(d.config->max_readahead(), d.config->io_bufs().max_buf_size());
conn->max_readahead = d.config->io_bufs().max_buf_size();
conn->max_read      = d.config->io_bufs().max_buf_size();
conn->max_write     = d.config->io_bufs().max_buf_size();
conn->max_background = d.config->max_background();     // 預設 32
conn->time_gran      = d.config->time_granularity().asUs().count() * 1000;  // 1s → 1e9 ns
```

幾個要點：

- **writeback cache 預設關閉**（`FuseConfig.h:38` `enable_writeback_cache = false`）。開了之後核心會緩衝 `write()` 並自行維護 `i_size`，這與 3FS「長度由 meta server 依 chunk 推算」的模型會打架；所以預設不開。
- `time_gran = 1s`：告訴核心此 fs 的時間戳精度只有秒。`RcInode::DynamicAttr` 的 `atime/mtime` 是本地暫存值，最終由 meta server 落盤，秒級足夠且省去無謂的 attr invalidate。
- `max_readahead` 設定項預設 `16MB`，但 `conn->max_readahead` 實際被設成 `io_bufs.max_buf_size`（1MB）——設定項 `max_readahead` 只被用來算 `d.maxBufsize`，而 `d.maxBufsize` 在本版程式碼中沒有其他讀者。這是一處遺留欄位。

---

## 5. inode 與 file handle 管理

### 5.1 `fuse_ino_t` ↔ `InodeId` 的雙向映射

```cpp
// src/fuse/FuseOps.cc:175-193
InodeId real_ino(fuse_ino_t ino) {
  if (ino == FUSE_ROOT_ID)     return InodeId::root();     // 1 → 0
  if (ino == FUSE_ROOT_ID + 1) return InodeId::gcRoot();   // 2 → 1
  return InodeId(ino);
}
fuse_ino_t linux_ino(InodeId ino) {
  if (ino == InodeId::root())   return FUSE_ROOT_ID;
  if (ino == InodeId::gcRoot()) return FUSE_ROOT_ID + 1;
  return ino.u64();
}
```

3FS 的 `InodeId::root()` 是 0（`src/fbs/meta/Common.h`），但 FUSE 規定根一定是 `FUSE_ROOT_ID == 1`。而 3FS 的 `gcRoot()` 恰好是 1（`Common.h:140`），會與 FUSE 根撞號，所以做了一個「swap 0↔1、其餘直通」的小映射。這樣做的好處是**其餘所有 inode 都是恆等映射**，`stat` 回傳的 `st_ino` 就是真正的 3FS InodeId——USRBIO 的 `hf3fs_reg_fd()` 正是靠這一點，用 `statx(STATX_INO)` 拿到的 `stx_ino` 直接當成 `InodeId` 用（`src/lib/api/UsrbIo.cc:585`）。

### 5.2 虛擬 inode 位址空間

`src/fbs/meta/Common.h:150-224` 在 64-bit 位址空間的**最高端**切了一整塊給虛擬物件，並用 `static_assert` 把每一個常數釘死：

| 物件 | InodeId | 常數 |
|---|---|---|
| `/3fs-virt` | `0xfffffffffffffffe` | `InodeId::virt()` |
| `/3fs-virt/rm-rf` | `0xfffffffffffffffd` | `InodeId::rmRf()` |
| `/3fs-virt/iovs` | `0xffffffff80000000` | `InodeId::iovDir()` |
| `/3fs-virt/iovs/<uuid…>` | `iovDir - 65535 - iovd` | `InodeId::iov(iovd)` |
| `/3fs-virt/iovs/submit-ios*` | `iovDir - 1/2/3` | `IoRingTable::lookupSem(prio)` |
| `/3fs-virt/get-conf` | `0xffffffff00000000` | `InodeId::getConf()` |
| `/3fs-virt/set-conf` | `0xfffffffe80000000` | `InodeId::setConf()` |
| rm-rf/mv 的一次性回應 | `-(100<<30) - toRemove` | `InodeId::virtTemporary()` |

判斷是否虛擬只看最高 4 bit：

```cpp
// src/fuse/FuseOps.cc:262
bool checkIsVirt(InodeId ino) { return (ino.u64() & (0xf000000000000000)) != 0; }
```

`remove_entry` 用這個函式來抑制「虛擬 inode 找不到」的錯誤日誌（`FuseOps.cc:268`）——因為虛擬 inode 從未進過 `d.inodes`，核心 forget 它們時當然找不到。

**這套虛擬目錄是整個 daemon 的控制介面**。`/3fs-virt` 只在 `parent == root` 時被 `hf3fs_lookup` 特判出來（`FuseOps.cc:654`），並在 `readdirplus` 根目錄時附加進 entry 列表（`FuseOps.cc:2264`）。四個子目錄各自劫持不同的 POSIX 操作：

| 目錄 | 被劫持的操作 | 語意 |
|---|---|---|
| `rm-rf` | `symlink(target, rm-rf/x)` | 遞迴刪除 `target` |
| `rm-rf` | `rename(x, rm-rf/y)` | 遞迴刪除 `x` |
| `iovs` | `symlink(/dev/shm/xxx, iovs/<uuid>.b4096.r0.pn.t100)` | 註冊共享記憶體 / 建立 IoRing |
| `iovs` | `unlink(iovs/<key>)` | 註銷 |
| `get-conf` | `readdir` / `readlink` | 讀設定值（symlink target 即為值） |
| `set-conf` | `symlink(value, set-conf/usr.readonly)` | 設定值 |

**為什麼用 symlink 當 API**：`symlink(2)` 是唯一一個「帶兩個任意長度字串、且不需要先建立檔案」的 POSIX 呼叫。`ln -s <value> <mount>/3fs-virt/set-conf/usr.readonly` 就是一次帶參數的 RPC，shell 就能操作，不需要任何客戶端程式庫。回應則透過 `fuse_reply_entry` 的 attr 帶回。

代價是危險：`ln -s` 被誤用可能誤刪整棵樹。所以有 `check_rmrf`（`FuseOps.cc:104-173`）——它去讀 `/proc/<pid>/cmdline`，若呼叫者確實是 `ln`，就逐個參數檢查目標路徑必須是**絕對路徑**且**位於本掛載點之下**，否則拒絕：

```cpp
// src/fuse/FuseOps.cc:141
if (!path.is_absolute()) { XLOGF(CRITICAL, "…path '{}' is not absolute"); return false; }
```

註解直說了動機：「if user is trying to recursive remove with ln -s, ensure that there are no extra spaces in the path entered by the user」——防的是 `ln -s /data /mnt/3fs-virt/rm-rf/x` 中路徑被 shell 拆成兩段導致刪錯東西。

### 5.3 lookup count 與 forget

`d.inodes` 的 `refcount` **就是 FUSE 的 lookup count**。三個進入點：

```cpp
// src/fuse/FuseOps.cc:231  正常路徑：lookup / create / mkdir / readdirplus 成功後
void add_entry(const Inode &inode, struct fuse_entry_param *e) {
  if (e) { fillLinuxStat(e->attr, inode); e->ino = e->attr.st_ino; }
  std::lock_guard lock{d.inodesMutex};
  auto it = d.inodes.find(inode.id);
  if (it != d.inodes.end()) { it->second->refcount++; it->second->update(inode); }
  else { d.inodes.insert({inode.id, std::make_shared<RcInode>(inode, 1)}); }
}

// src/fuse/FuseOps.cc:249  readdirplus 中 batchStat 沒拿到 inode 內容時
void add_empty_entry(const InodeId &inodeid);

// src/fuse/FuseOps.cc:264  forget
void remove_entry(InodeId ino, int n);
```

三處細節值得挖：

1. **`add_entry` 不更新 `rcinode->inode`**。第 241 行 `// rcinode->inode = inode;` 被註解掉，只呼叫 `rcinode->update(inode)`——而 `update()` 只更新 `DynamicAttr`（truncateVer / dynStripe / synced），`RcInode::inode` 這份靜態快照從建立起就**再也不變**。這造成 `hf3fs_readlink` 回傳的是第一次 lookup 時的 symlink target（`FuseOps.cc:965`）；也造成 `PioV::addRead` 用的 layout 是舊的（layout 不可變，所以安全）。這是刻意的：`RcInode::inode` 只被當成「不變欄位的容器」（id / type / layout / acl），可變欄位一律走 `DynamicAttr` 或重新 stat。

2. **`refcount` 由 `inodesMutex` 保護而非 atomic**，且 `remove_entry` 在 `refcount < n` 時只記錯誤日誌不修正（`FuseOps.cc:279`）。這在 forget 亂序時會漏刪 map entry（記憶體漏），但不會 crash。

3. **`readdirplus` 的部分回滾**：加完 entry 後若 `fuse_add_direntry_plus` 回報空間不足，會立刻 `remove_entry(id, 1)` 把剛加的 lookup count 退回去（`FuseOps.cc:2354`、`2365`），因為那筆 entry 沒有真的送給核心。這是 readdirplus 實作最容易寫錯的地方，此處處理正確。

### 5.4 file handle

```cpp
// src/fuse/FuseClients.h:146
struct FileHandle {
  std::shared_ptr<RcInode> rcinode;   // 強引用，open 期間 inode 不會被 forget 掉
  bool oDirect;
  Uuid sessionId;                     // 只有寫模式才是有效值
};
struct DirHandle { size_t dirId; pid_t pid; bool iovDir; };
```

`fi->fh` 存的是裸 `new` 出來的指標，`hf3fs_release` 用 `SCOPE_EXIT { delete (FileHandle *)fi->fh; }`（`FuseOps.cc:1744`）釋放。`FileHandle` 持有 `shared_ptr<RcInode>`，所以即使核心先 forget 再 release，`RcInode` 仍活著。

`inodeOf` 的兩個多載體現了這個設計：

```cpp
// src/fuse/FuseOps.cc:417
std::shared_ptr<RcInode> inodeOf(struct fuse_file_info &fi, InodeId ino) {
  if (fi.fh && ((FileHandle *)fi.fh)->rcinode) return ((FileHandle *)fi.fh)->rcinode;
  return inodeOf(ino);   // 退回查 map
}
```

有 fh 就走 fh（免鎖），沒有才去 `d.inodes` 查（要拿 `inodesMutex`）。

---

## 6. 讀路徑

### 6.1 `hf3fs_read` 全流程

```
hf3fs_read(req, ino, size, off, fi)                    FuseOps.cc:1473
├─ pi = inodeOf(*fi, ino)
├─ pi->dynamicAttr.wlock()->atime = now()              ← 本地記 atime，不發 RPC
├─ 若有寫緩衝且 len>0 → flushBuf(flushAll=true)         ← 讀前必刷寫緩衝（同檔 read-after-write）
├─ memh = IOBuffer(bufPool->allocate())                ← 從 RDMA 註冊過的 buf pool 取一塊
├─ [dryrun_bench_mode] 直接回未初始化 buffer 的 size 位元組
├─ [memset_before_read] memset(memh.data(), 0, size)
├─ PioV ioExec(storageClient, chunk_size_limit, res)
├─ ioExec.addRead(0, inode, track=0, off, size, memh.data(), memh)
│     └─ chunkIo(...) 依 layout 切成 N 個 chunk 級 ReadIO
├─ withRequestInfo(req, ioExec.executeRead(userInfo, storage_io.read()))
│     └─ storageClient->batchRead(rios_, userInfo, options)
├─ ioExec.finishIo(allowHoles = true)                  ← 洞零填、算總長度
└─ fuse_reply_buf(req, memh.data(), res[0])
```

`memset_before_read`（預設 false）是為了讓「讀到未寫過區域」時回傳零而非上一次 IO 殘留的資料。它是熱更新項（`FuseClients.cc:167`），可在懷疑資料汙染時線上打開。注意它 memset 的是 `size` 而非實際讀到的長度——是保守做法。

`dryrun_bench_mode`（`FuseOps.cc:1515`）直接回傳未初始化的 buffer，用來量測「除了實際 IO 以外的所有開銷」——FUSE 往返、記憶體配置、reply 路徑。它是 per-user 可設定的（`UserConfig.h:33` `userKeys` 裡有），所以壓測者可以只對自己開。

### 6.2 `PioV::chunkIo`：檔案 offset → chunk 請求

`src/fuse/PioV.cc:98-130` 是整個資料路徑的核心迴圈：

```cpp
const auto &f = inode.asFile();
auto chunkSize = f.layout.chunkSize;
auto chunkOff  = off % chunkSize;
auto rcs = chunkSizeLim_ ? std::min((size_t)chunkSizeLim_, chunkSize.u64()) : chunkSize.u64();

for (size_t lastL = 0, l = std::min((size_t)(chunkSize - chunkOff), len);
     l < len + chunkSize;
     lastL = l, l += chunkSize) {
  l = std::min(l, len);
  auto opOff = off + lastL;
  auto chain  = f.getChainId(inode, opOff, *routingInfo_, track);
  auto fchunk = f.getChunkId(inode.id, opOff);
  auto chunkLen = l - lastL;
  for (size_t co = 0; co < chunkLen; co += rcs) {
    consumeChunk(*chain, chunk, chunkSize, chunkOff + co, std::min(rcs, chunkLen - co));
  }
  chunkOff = 0;
}
```

迴圈的形狀值得拆解：

- 第一段 `l` 初始化為 `min(chunkSize - chunkOff, len)`——把「第一個 chunk 的殘餘」單獨處理，後續每輪固定 `+= chunkSize`。
- 終止條件寫成 `l < len + chunkSize` 而非 `l < len`，配合迴圈內 `l = min(l, len)`，效果是**最後一輪也會執行**（`l` 被夾到 `len` 後仍小於 `len + chunkSize`）。這是一個為了避免額外 if 的緊湊寫法。
- 內層 `rcs`（read chunk size）迴圈把單一 chunk 再切成 ≤`chunk_size_limit` 的片段。`chunk_size_limit` 預設 0（不切）；設成例如 512KB 可以把 16MB 的大 chunk 切成 32 個小請求併發下發，降低尾延遲。這是 `CONFIG_HOT_UPDATED_ITEM(chunk_size_limit, 0_KB)`（`FuseConfig.h:54`），可線上調。

`getChainId(inode, offset, routingInfo, track)` 與 `getChunkId(inodeId, offset)` 由 `src/fbs/meta/Schema.h:265-268` 提供，是純函式——**客戶端自己算出資料在哪條 CRAQ 鏈的哪個 chunk 上，不需要問 meta server**。這是 3FS 資料路徑不經 meta 的關鍵。

`track` 參數（本檔案裡永遠傳 0）是給多副本讀取用的擴充點。

### 6.3 `PioV` 的讀寫互斥設計

```cpp
// src/fuse/PioV.cc:21
if (!wios_.empty()) return makeError(StatusCode::kInvalidArg, "adding read to write operations");
// src/fuse/PioV.cc:62
if (!rios_.empty()) return makeError(StatusCode::kInvalidArg, "adding write to read operations");
```

一個 `PioV` 實例只能全讀或全寫。原因在下游：`StorageClient::batchRead` 與 `batchWrite` 是兩個不同的 API（讀走 CRAQ 尾節點、寫走頭節點），無法混批。

`res_` 是**外部傳入的 `vector<ssize_t>&`**（`PioV.h:53`），每個 `idx` 對應一筆使用者層級的 IO。`addRead(idx, ...)` 會把 `idx` 塞進 `userCtx`（`PioV.cc:48` `reinterpret_cast<void *>(idx)`），讓完成時能把 chunk 級結果聚合回使用者級。FUSE 路徑只有一筆（`res(1)`），USRBIO 路徑則有 `toProc` 筆。

### 6.4 `concatIoRes`：把 chunk 結果拼回使用者 IO，並處理「洞」

`src/fuse/PioV.cc:186-266` 是全檔最微妙的函式。核心問題：一次使用者讀被切成 N 個 chunk 讀，其中某個 chunk 可能：

- **完全不存在**（`kChunkNotFound`）→ 稀疏檔案的洞
- **只讀到前半**（`iolen < io.length`）→ chunk 存在但比預期短（EOF 或洞）

程式碼裡的關鍵註解（`PioV.cc:200-203`）：

> the front part of the data read from a chunk can never be part of a hole when anything is read from the chunk / storage server promises that, or how can it tell us that it only reads into the buffer from the middle?

也就是說 storage server 的協定保證「一個 chunk 若讀到 k 個 byte，那必定是從 chunkOff 開始連續的 k 個」。有了這個保證，客戶端就能推論：**只要後面某個 chunk 讀到了資料，前面那段短少就一定是洞而非 EOF**。於是狀態機是：

```
遇到 iolen < io.length  → inHole = true，記下 holeIo / holeOff / holeSize
遇到 iolen > 0 且 inHole 且仍是同一個使用者 IO
    → 確認前面那段是洞
    → allowHoles ? 零填 + res[idx] += holeSize
                 : res[idx] = -kHoleInIoOutcome (→ ENODATA)
遇到換了使用者 IO (lastIovIdx != iovIdx) → 清除 hole 狀態（把短少當 EOF）
```

零填的實作（`PioV.cc:226-231`）補齊 hole 起始 IO 的尾巴，再把中間整段 IO 全部清零：

```cpp
auto &hio = ios[*holeIo];
memset(hio.data + holeOff, 0, hio.length - holeOff);
for (size_t j = *holeIo + 1; j < i; ++j) memset(ios[j].data, 0, ios[j].length);
res[iovIdx] += holeSize;
```

`allowHoles` 的來源：
- FUSE 讀路徑：永遠 `true`（`FuseOps.cc:1538` `ioExec.finishIo(true)`）——POSIX 語意就是稀疏檔讀到零。
- USRBIO 讀路徑：`!(flags_ & HF3FS_IOR_FORBID_READ_HOLES)`（`IoRing.cc:206`）——應用可以明確要求「碰到洞就報錯」，因為對訓練資料而言讀到一堆零往往是資料損毀而非合法稀疏檔。
- 寫路徑：永遠 `false`（`PioV.cc:272`）——寫沒有洞這回事。

還有一個副作用：發現洞時**無論如何都會 `XLOGF(ERR, ...)`**（`PioV.cc:208`），印出 inode、chunk index、chain id、洞的大小。設計者顯然認為洞在生產環境是異常訊號。

---

## 7. 寫路徑與長度一致性

### 7.1 三條寫入通道

`hf3fs_write`（`FuseOps.cc:1552`）依三個條件分岔：

```
if (readonly)                       → EROFS
if (dryrun_bench_mode)              → 立刻回 fuse_reply_write(size)，什麼都不做
if (oDirect || write_buf_size == 0) → 直寫：從 bufPool 取 buf → memcpy → flushBuf(flushAll=false)
else                                → 進 per-inode 寫緩衝
```

**直寫路徑用 `flushAll=false`**（`FuseOps.cc:1622`）：`flushBuf` 只跑一輪，storage 寫多少就回多少，讓核心自己決定要不要再發一次。緩衝路徑刷出時則一律 `flushAll=true`，迴圈直到全部寫完（`FuseOps.cc:484` 的 `while (flushAll && done < len)`）。

### 7.2 per-inode 寫緩衝

```cpp
// src/fuse/FuseClients.h:60
struct InodeWriteBuf {
  std::vector<uint8_t> buf;
  std::unique_ptr<storage::client::IOBuffer> memh;   // RDMA 註冊 handle
  off_t off{0};
  size_t len{0};
};
```

緩衝是 **per-inode 而非 per-fd**（掛在 `RcInode::writeBuf`，由 `RcInode::wbMtx` 保護）。大小 `io_bufs.write_buf_size = 1MB`。第一次寫入時 lazily 建立並 `storageClient->registerIOBuffer()` 註冊給 RDMA（`FuseOps.cc:1633-1641`）。

刷出時機（**六個**）：

| 觸發點 | 位置 | 條件 |
|---|---|---|
| 寫入不連續 | `FuseOps.cc:1649` | `wb->off + wb->len != off` |
| 緩衝滿 | `FuseOps.cc:1669` | `wb->len == wb->buf.size()` |
| 讀同一檔 | `FuseOps.cc:1487` | `hf3fs_read` 開頭 |
| truncate | `FuseOps.cc:854` | `setattr` 的 `FUSE_SET_ATTR_SIZE` 分支 |
| O_DIRECT 寫 | `FuseOps.cc:1596` | 混用 O_DIRECT 與 buffered fd 時 |
| fsync/flush/getattr/lookup | `FuseOps.cc:613` | `flushAndSync()` 中，`issync \|\| flush_on_stat` |
| 背景 periodic sync | `FuseOps.cc:2692` | `periodic_sync.flush_write_buf`，用 `try_lock` 非阻塞 |

「寫入不連續就整個刷掉」意味著**隨機寫會退化成每次一個 RPC**。這是刻意的取捨：3FS 的目標場景是大塊順序寫（`docs/design_notes.md:33`），為隨機寫維護一個 extent 樹不划算。

寫完之後若緩衝還有殘留，就把 inode 丟進 `dirtyInodes`（`FuseOps.cc:1683`），讓背景 periodic sync 接手。

### 7.3 長度一致性：`written / synced / fsynced` 三版本號

3FS 的檔案長度**不是寫入時更新的**——storage 寫完 chunk 後 meta 端的 `Inode.length` 還是舊值。長度靠兩種方式收斂：

1. **hint**：客戶端知道自己寫到哪，把 `VersionedLength{length, truncateVer}` 當提示送給 meta。
2. **recompute**：meta 掃 chunk 算出真實長度（昂貴）。

`RcInode::DynamicAttr` 的三個計數器編碼了「哪些寫入還沒被 meta 知道」：

```cpp
// src/fuse/FuseClients.h:68-96
uint64_t written = 0;   // 每次 finishWrite 遞增
uint64_t synced  = 0;   // 已被 sync（含 period sync）覆蓋到的 written 值
uint64_t fsynced = 0;   // 已被 fsync/close/truncate 覆蓋到的 written 值
```

`sync()`（`FuseOps.cc:517`）的判斷：

```cpp
auto syncver = fsync ? guard->fsynced : guard->synced;
auto writever = guard->written;
if (!force && syncver >= writever) co_return std::nullopt;   // 沒有新寫入，直接跳過
```

於是「兩種 sync 各有各的水位線」：一次 period sync 會推高 `synced` 但不推高 `fsynced`，所以隨後的 `fsync()` 仍然會真的發 RPC。反過來 `fsync()` 同時推高兩者（`update(inode, writever, fsync=true)` → `synced = max(synced, syncver)` 且 `fsynced = max(...)`）。這正確實現了「fsync 必須有 durability 語意，periodic sync 只是機會性更新」。

`ForceFsync`（只有 `HF3FS_IOC_FSYNC` 用）連 `syncver >= writever` 的短路都跳過，而且**強制丟棄 hint**：

```cpp
// src/fuse/FuseOps.cc:529
auto hint = (fsync && (force || !d.config->fsync_length_hint()))
              ? std::optional<meta::VersionedLength>()   // 不給 hint → 逼 meta 重算
              : guard->hintLength;
```

`fsync_length_hint` 預設 false 且註解標了 `// for test`（`FuseConfig.h:32`），代表**一般的 fsync 也不送 hint**——fsync 就是要一個權威長度，寧可讓 meta 掃 chunk。只有 periodic sync 才走 hint 這條便宜路。

### 7.4 `hintLength` 的毒化語意

```cpp
// src/fbs/meta/Schema.h:233
static std::optional<VersionedLength> mergeHint(std::optional<VersionedLength> h1,
                                                std::optional<VersionedLength> h2) {
  if (!h1 || !h2) return std::nullopt;          // ← 任一為空 → 結果為空
  return h1->length >= h2->length ? h1 : h2;
}
```

`finishWrite`（`FuseOps.cc:2655`）：

```cpp
std::optional<meta::VersionedLength> newHint = std::nullopt;
if (ret >= 0) newHint = meta::VersionedLength{ret ? offset + ret : 0, truncateVer};
auto guard = dynamicAttr.wlock();
guard->written++;
guard->hintLength = meta::VersionedLength::mergeHint(guard->hintLength, newHint);
```

**寫入失敗（`ret < 0`）→ `newHint = nullopt` → 整個 `hintLength` 永久變成 `nullopt`**，直到下一次 sync 成功後被重設。同理 `clearHintLength()`（`FuseClients.h:133`）在每次 sync/close/truncate 失敗時被呼叫。

這是一個「單向毒化」設計：只要有任何一次寫入的結果不確定，客戶端就放棄宣稱自己知道檔案長度，逼 meta server 走昂貴但正確的重算路徑。**hint 只能在完全可信時使用**。

`DynamicAttr::update` 的另一半（`FuseClients.h:86-90`）：

```cpp
synced = std::max(synced, syncver);
if (written == synced) {
  hintLength = meta::VersionedLength{0, 0};   // 清成「零長度 hint」而非 nullopt
}
```

`{0, 0}` 與 `nullopt` 語意不同：前者表示「我沒有新資訊，但我沒有壞掉」，後續 `mergeHint` 會正常取較大者；後者表示「我壞了」。這個區分是整個機制能運作的關鍵。

### 7.5 `beginWrite`：動態 stripe 擴展

寫到檔案更遠處時，可能需要用到更多 chain（stripe）。`RcInode::beginWrite`（`FuseOps.cc:2617`）：

```cpp
auto stripe = std::min((uint32_t)folly::divCeil(offset + length, chunkSize), layout.stripeSize);
{ auto guard = dynamicAttr.rlock();
  if (!guard->dynStripe || guard->dynStripe >= stripe) co_return guard->truncateVer; }  // 快路徑

co_await extendStripeLock.co_lock();       // 只允許一個 task 擴展
SCOPE_EXIT { extendStripeLock.unlock(); };
{ ... 再檢查一次 ... }                      // double-checked locking
auto res = co_await meta.extendStripe(userInfo, inode.id, stripe);
```

標準的 double-checked locking，但鎖是 `folly::coro::Mutex`（協程鎖，不阻塞執行緒）。`dynStripe == 0` 表示這個檔案沒開動態 stripe，直接跳過。

`beginWrite` 的回傳值是 `truncateVer`——寫入必須攜帶當時的 truncate 版本，這樣 `finishWrite` 生出的 hint 才能被 meta 端正確地與併發 truncate 排序。

### 7.6 truncate / close 的一致性

```cpp
// src/fuse/FuseOps.cc:578  truncate
auto writever = inode.dynamicAttr.rlock()->written;   // 先取快照
auto res = co_await d.metaClient->truncate(userInfo, inode.inode.id, length);
if (res.hasError()) inode.clearHintLength();
else inode.update(*res, writever, /*fsync=*/true);    // 推高兩個水位線
```

先取 `written` 快照再發 RPC，是為了避免「RPC 期間又發生寫入」時錯誤地把新寫入也標記成已同步。

```cpp
// src/fuse/FuseOps.cc:551  close
auto updateLength = writever > syncver;   // syncver = fsynced
auto res = co_await d.metaClient->close(userInfo, inode.inode.id, session, updateLength, atime, mtime);
if (updateLength) notify_inval_inode(inode.inode.id);
```

`close` 只在**確實有未同步寫入時**才要求 meta 更新長度——只讀開啟或已 fsync 過的檔案 close 時不會付這個代價。

`hf3fs_release`（`FuseOps.cc:1765`）也只在寫模式時呼叫 `close`：

```cpp
if ((fi->flags & O_ACCMODE) == O_WRONLY || (fi->flags & O_ACCMODE) == O_RDWR) {
  auto res = withRequestInfo(req, close(userInfo, *ptr, sessionId));
}
```

與 `hf3fs_open`（`FuseOps.cc:1450`）對稱：**只讀開啟根本不建立 FileSession**。FileSession 在 meta 端是有成本的（一筆 FDB key + 需要 GC），只讀檔案不需要它——只讀沒有長度要更新，也不需要崩潰後的清理。這讓「大量讀取小檔」的場景省下一半的 meta 寫入。

`hf3fs_flush` 直接轉呼叫 `hf3fs_fsync(datasync=false)`（`FuseOps.cc:1705`）。POSIX 的 `close()` 會觸發 FUSE_FLUSH，於是 `close()` 實際上會同步刷 + sync 一次，然後 FUSE_RELEASE 再 close session。這讓 `close()` 有了「幾乎等同 fsync」的強語意（代價是 `close()` 變慢）。

`fdatasync_update_length`（預設 false，`FuseConfig.h:33`）控制 `fdatasync()` 是否也更新長度：

```cpp
// src/fuse/FuseOps.cc:1699
flushAndSync(req, fino, datasync && !d.config->fdatasync_update_length(), SyncType::Fsync, fi);
//                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^  flushOnly
```

預設 `fdatasync()` 只刷資料不 sync 長度（`flushOnly=true` → `FuseOps.cc:626` 提早 return），符合 `fdatasync` 「不保證 metadata」的 POSIX 語意，且省一次 meta RPC。

---

## 8. 旁路 IO：IoRing / USRBIO

### 8.1 為什麼要有它

`docs/design_notes.md:27-35` 列了 FUSE 的三個硬傷：單次 IO 大小上限、請求佇列的單一 spinlock（實測約 400K 4KiB reads/s 就到頂）、Linux 5.x 不支援同檔併發寫。3FS 的解法不是改核心，而是**在 FUSE daemon 內部塞一個 native client**，用共享記憶體繞過 `/dev/fuse`。

### 8.2 共享記憶體佈局

`IoRing` 的建構子（`src/fuse/IoRing.h:77-120`）把一整塊 shm 切成六段：

```
buf (使用者用 shm_open + mmap 得到，FUSE 端也 mmap 同一塊)
├─ [0]                     int32  sqeHead     ← 由使用者端寫入
├─ [n]                     int32  sqeTail     ← 由 FUSE 端寫入
├─ [2n]                    int32  cqeHead     ← 由 FUSE 端寫入
├─ [3n]                    int32  cqeTail     ← 由使用者端寫入
├─ [4n]                    IoArgs ringSection[entries]   ← 請求參數池（非環）
├─ [4n + sizeof(IoArgs)*E] IoCqe  cqeSection[entries]    ← 完成環
├─ [...]                   IoSqe  sqeSection[entries]    ← 提交環
└─ [...]                   sem_t  cqeSem                 ← 程序間 semaphore（pshared=1）
   + 4096 bytes 保留

n = ringMarkerSize() = round_up(4, alignof(atomic_ref<int32_t>))
entries = ioRingEntries(size) + 1   ← 多一格用來區分「空」與「滿」
```

容量計算（`IoRing.h:60-71`）：

```cpp
static int ioRingEntries(size_t bufSize) {
  auto n = ringMarkerSize();
  return (int)std::min((size_t)INT_MAX,
                       (bufSize - 4096 - n * 4 - sizeof(sem_t))
                         / (sizeof(IoArgs) + sizeof(IoCqe) + sizeof(IoSqe))) - 1;
}
```

四個 head/tail 用 `std::atomic_ref<int32_t>` 疊在裸記憶體上（`IoRing.h:176-179`），這是 C++20 的 `atomic_ref`——**對跨進程共享記憶體做原子操作的標準做法**，不需要把 struct 宣告成 atomic。

三個 section 的分工很關鍵：

- **`ringSection` 不是環**，而是一個由 `AvailSlots slots` 管理的**槽位池**（`IoRing.h:186`）。`hf3fs_prep_io` 先 `slots.alloc()` 拿一個 idx，把 `IoArgs`（bufId / bufOff / fileIid / fileOff / ioLen / userdata）填進 `ringSection[idx]`，再把 idx 推進 sqe 環。
- **`sqeSection` / `cqeSection` 是環**，只裝 `{index, userdata}` 這種輕量描述。

**為什麼分兩層**：`IoArgs` 有 56 bytes，若直接放進環裡，環的推進就要搬 56 bytes；分開之後 sqe 只有 16 bytes，環操作極輕。更重要的是**槽位可以亂序釋放**——完成順序與提交順序不同時，`IoArgs` 槽位可以立刻回收，而環的 tail 必須依序推進。

### 8.3 使用者端提交流程

`src/lib/api/UsrbIo.cc:616-670`：

```c
hf3fs_prep_io(ior, iov, read, ptr, fd, off, len, userdata)
├─ 驗證：ior/iov 非空、read 與 ior->for_read 一致、ptr 在 iov 範圍內、fd 已註冊
├─ regfd = regfds[abs(fd)]；檢查 O_ACCMODE 與 read 方向相容
├─ idx = ring.slots.alloc()                      ← 滿了回 -EAGAIN
├─ args = ring.ringSection[idx]
│    args.bufId  = iov->id (16 bytes UUID)
│    args.bufOff = ptr - iov->base
│    args.fileIid = regfd->iid.u64()             ← 來自 statx 的 st_ino
│    args.fileOff = off; args.ioLen = len; args.userdata = userdata
└─ ring.addSqe(idx, userdata)                    ← 推進 sqeHead
```

```cpp
// src/lib/api/UsrbIo.cc:672
int hf3fs_submit_ios(const struct hf3fs_ior *ior) {
  sem_post(iorh.submitSem);   // 就這一行
  return 0;
}
```

提交只是一個 `sem_post`——**唯一進核心的動作**。sqe 資料早已寫進共享記憶體。

`submitSem` 從哪來？`cqeSem()`（`UsrbIo.cc:332-370`）去讀 `<mount>/3fs-virt/iovs/submit-ios{,.ph,.pl}` 這個 symlink，target 是 `/dev/shm/sem.hf3fs-submit-ios.<uuid>`，然後 `sem_open("/hf3fs-submit-ios.<uuid>")`。

FUSE 端建立這些 semaphore：

```cpp
// src/fuse/IoRing.h:219-226
for (int prio = 0; prio <= 2; ++prio) {
  auto sp = "/" + semOpenPath(prio);
  sems.emplace_back(sem_open(sp.c_str(), O_CREAT, 0666, 0),
                    [sp](sem_t *p) { sem_close(p); sem_unlink(sp.c_str()); });
  chmod(semPath(prio).c_str(), 0666);
}
static std::string semOpenPath(int prio) {
  static std::vector<Uuid> semIds{Uuid::random(), Uuid::random(), Uuid::random()};
  return fmt::format("hf3fs-submit-ios.{}", semIds[prio].toHexString());
}
```

**名字裡帶隨機 UUID**，每次 daemon 啟動都不同。這解決兩個問題：（a）同一台機器掛多個 3FS 叢集時 semaphore 不撞名；（b）daemon 重啟後舊 client 拿著舊 semaphore 也不會誤傷新 daemon——它們發 post 到一個已 unlink 的 semaphore 上，沒有人在等。透過 symlink 發佈名字，讓 client 每次都重新 readlink 取得當前有效的名字。

### 8.4 FUSE 端消費：watch 執行緒 → 佇列 → worker 協程

```
     使用者 sem_post(submitSem[prio])
                 │
                 ▼
  ┌──────────────────────────────────┐
  │ FuseClients::watch(prio)  ×3      │  FuseClients.cc:369
  │  sem_timedwait(sems[prio],        │  超時 = 現在 + jitter(1ms)
  │                now + jitter)      │
  │  do {                             │
  │    掃 iors.ioRings 全表            │
  │    找 priority == prio 的 ring     │
  │    ior->jobsToProc(max_jobs=32)   │  ← 切批
  │    iojqs[prio]->enqueue(job)       │
  │  } while (gotJobs)                │  ← 一直撈到沒得撈才回去 block
  └──────────────┬───────────────────┘
                 ▼
  ┌──────────────────────────────────┐
  │ BoundedQueue<IoRingJob> ×3        │
  │  prio0 (hi):    32   ← 小佇列      │
  │  prio1 (normal): 1024              │
  │  prio2 (lo):    4096               │
  └──────────────┬───────────────────┘
                 ▼
  ┌──────────────────────────────────┐
  │ FuseClients::ioRingWorker(i) ×128 │  FuseClients.cc:218
  │  自身優先級 prio = f(i)             │
  │  job.ior->process(...)             │  → PioV → StorageClient
  └──────────────────────────────────┘
```

**`sem_timedwait` 而非 `sem_wait`**（`FuseClients.cc:379`）：

```cpp
auto nsec = ts.tv_nsec + jitter.load().count();
ts.tv_nsec = nsec % 1000000000;
ts.tv_sec += nsec / 1000000000;
if (sem_timedwait(iors.sems[prio].get(), &ts) < 0 && errno == ETIMEDOUT) continue;
```

`submit_wait_jitter` 預設 1ms 且可熱更新（`FuseClients.cc:168`）。

加超時**不是**為了兜住漏掉的喚醒——逾時路徑什麼事都不做（`src/fuse/FuseClients.cc:379-381`）：

```cpp
if (sem_timedwait(iors.sems[prio].get(), &ts) < 0 && errno == ETIMEDOUT) {
  continue;                 // ← 直接跳回 while 開頭，底下的掃描區段（:383-399）完全不執行
}
```

它的唯一作用是讓 watch 執行緒定期回到迴圈頭檢查 `stop.stop_requested()`（`FuseClients.cc:370`），使 `FuseClients::stop()` 的 `request_stop()`（`FuseClients.cc:190`）能生效。若改用純 `sem_wait`，這個執行緒將永遠停不下來。

真正防止漏喚醒的是兩處**主動補 post**：使用者端每次收割完 cqe 後的 `hf3fs_submit_ios`（`UsrbIo.cc:736`，見 §8.8），以及 io worker 處理完 job 後的 `sem_post(iors.sems[job.ior->priority].get())`（`FuseClients.cc:343-344`）。

**`do { } while (gotJobs)`**（`FuseClients.cc:384-399`）：喚醒一次後持續掃描直到掃不出 job 為止。因為一次 `sem_post` 可能對應多個 ring 的多批 IO，而 semaphore 的計數不等於 job 數。

### 8.5 `jobsToProc`：批次切分與 timeout（含 commit 593cec5 的 bug）

`src/fuse/IoRing.cc:15-65`。`ioDepth` 有三種語意（`src/lib/api/UsrbIo.md` 有對應說明）：

| ioDepth | 語意 |
|---|---|
| `> 0` | **嚴格批次**：必須湊滿 `ioDepth` 筆才下發 |
| `== 0` | **不控制**：有多少發多少 |
| `< 0` | **軟批次**：最多湊 `-ioDepth` 筆；湊不滿就等 `timeout`，超時後有多少發多少 |

```cpp
// src/fuse/IoRing.cc:22
auto cqeAvail = entries - 1 - processing_ - cqeCount();
while (sqes && (int)jobs.size() < maxJobs) {
  int toProc;
  if (ioDepth > 0) {
    toProc = ioDepth;
    if (toProc > sqes || toProc > cqeAvail) break;   // 湊不滿，或算完沒地方放結果
  } else {
    toProc = std::min(sqes, cqeAvail);
    if (ioDepth < 0) {
      auto iod = -ioDepth;
      if (toProc > iod) { toProc = iod; }
      else if (toProc < iod && timeout.count()) {
        auto now = SteadyClock::now();
        if (!lastCheck_) { lastCheck_ = now; break; }          // 第一次發現不足 → 開始計時
        else if (*lastCheck_ + timeout > now) { break; }        // 還沒到時間 → 繼續等
      }
      lastCheck_ = std::nullopt;                                // 超時或湊滿 → 重置
    }
  }
  jobs.emplace_back(IoRingJob{shared_from_this(), spt, toProc});
  spt = (spt + toProc) % entries;
  sqeProcTails_.push_back(spt);
  processing_ += toProc; sqes -= toProc; cqeAvail -= toProc;
}
```

`cqeAvail` 的計算是個容易忽略的正確性條件：**在下發 IO 之前就要確認完成環有位子**。註解寫得很白：「even if we finish the io, we got no place to store the results」。少了這一步，`addCqe` 失敗時只能 `XLOGF(FATAL)`（`IoRing.cc:264`）——結果無處可放就等於資料遺失。

**commit 593cec5 修的 bug**：

```diff
-          auto now = lastCheck_ = SteadyClock::now();
+          auto now = SteadyClock::now();
           if (!lastCheck_) {
             lastCheck_ = now;
             break;
```

舊版把 `lastCheck_` 每次都重設成 `now`，於是後面那行 `*lastCheck_ + timeout > now` 恆為真（`now + timeout > now`），`break` 永遠執行。結果：**`ioDepth < 0` 的軟批次模式下 timeout 完全失效**，一批 IO 若始終湊不滿 `-ioDepth`，就永遠不會被下發，只能等到後續 IO 把批次填滿為止。對「最後一批」（例如檔案讀到尾、只剩 3 筆但 `-ioDepth = 8`）而言，這是永久卡死。修掉之後 `lastCheck_` 只在首次發現不足時設定，計時才真的開始走。

一行 diff，但它是 USRBIO 軟批次模式從「不可用」變成「可用」的差別。

### 8.6 `IoRing::process`：實際執行

`src/fuse/IoRing.cc:67-284`：

```
process(spt, toProc, storageClient, storageIo, userConfig, lookupFiles, lookupBufs)
├─ if (!forRead_ && config.readonly()) → 全部填 -kReadOnlyMode，跳過執行
├─ lookupFiles(inodes, ringSection, sqeSection + spt, ...)      ← 由 FuseClients 提供的 lambda
├─ lookupBufs (bufs,   ringSection, sqeSection + spt, ...)
├─ for i in [0, toProc):
│    ├─ 統計 (ioSize / distinctFiles / distinctBufs)
│    ├─ inodes[i] 為空 → res[i] = -kNotFile
│    ├─ bufs[i] 有錯   → res[i] = -error
│    ├─ memh = co_await bufs[i]->memh(ioLen)                    ← RDMA 註冊 handle（lazy）
│    ├─ [寫] beginWrite(...) → truncateVers[i]
│    └─ ioExec.addRead/addWrite(i, inode, 0, fileOff, ioLen, ptr, memh)
├─ readOpt.allowReadUncommitted = flags_ & HF3FS_IOR_ALLOW_READ_UNCOMMITTED
├─ execRes = co_await (forRead_ ? executeRead : executeWrite)
├─ ioExec.finishIo(!(flags_ & HF3FS_IOR_FORBID_READ_HOLES))
├─ [寫] for i: inodes[i]->finishWrite(uid, truncateVers[i], off, res[i])
├─ 【cqeMtx_ 臨界區】
│    ├─ sqeTail 的亂序完成重排（見下）
│    ├─ for i: addCqe(sqe.index, res>=0 ? res : -toErrno(-res), sqe.userdata)
│    └─ processing_ -= toProc
└─ sem_post(cqeSem)                                             ← 喚醒使用者端 wait_for_ios
```

**兩個 lambda 是跨模組的關鍵接縫**。它們由 `FuseClients::ioRingWorker` 提供（`FuseClients.cc:278-333`），因為只有 `FuseClients` 才有 `inodesMutex` 與 `iovs.shmLock`：

```cpp
auto lookupFiles = [this](std::vector<std::shared_ptr<RcInode>> &ins, const IoArgs *args, const IoSqe *sqes, int sqec) {
  auto lastIid = 0ull;
  std::lock_guard lock(inodesMutex);
  for (int i = 0; i < sqec; ++i) {
    auto idn = args[sqes[i].index].fileIid;
    if (i && idn == lastIid) { ins.emplace_back(ins.back()); continue; }   // ← 相鄰去重
    ...
  }
};
```

**相鄰去重**：批次內連續指向同一檔案的請求只查一次 map，直接複製 `shared_ptr`。`lookupBufs` 也一樣（`FuseClients.cc:302-329`）。因為典型的 USRBIO 使用模式是「同一個檔案、同一個 iov、連續多筆」，這個優化很有效。

`lookupBufs` 還做了範圍檢查（`FuseClients.cc:322`）：`shm->size < arg.bufOff + arg.ioLen` → `kInvalidArg`。**這是必要的安全邊界**——`bufOff`/`ioLen` 來自共享記憶體，惡意或錯誤的使用者程序可以任意寫。

### 8.7 亂序完成的 sqeTail 重排

多個 worker 可以並行處理**同一個 ring 的不同區段**（`IoRing.h:49-52` 的類別註解說得很清楚）。但 `sqeTail` 必須單調且依序推進——它是給使用者端看「哪些 sqe 已被消費」的水位線。

```cpp
// src/fuse/IoRing.cc:230-257
std::lock_guard lock(cqeMtx_);
if (sqeProcTails_.front() != newSpt) {
  sqeDoneTails_.insert(newSpt);          // 我不是最前面那批 → 記下來等
} else {
  sqeTail = newSpt;
  sqeProcTails_.pop_front();
  while (!sqeDoneTails_.empty()) {        // 我完成了 → 順帶把後面已完成的也推上去
    auto first = sqeProcTails_.front();
    auto it = sqeDoneTails_.find(first);
    if (it == sqeDoneTails_.end()) break;
    sqeTail = first; sqeProcTails_.pop_front(); sqeDoneTails_.erase(it);
  }
}
```

`sqeProcTails_`（`deque`）記錄「已認領、處理中」的批次尾巴，順序即認領順序；`sqeDoneTails_`（`set`）記錄「已完成但前面還有人沒完成」的尾巴。這就是經典的 **out-of-order completion, in-order retirement**，與 CPU 的 ROB 同構。

注意 `cqe` 的推進**不需要**這個重排——`addCqe` 只要 `cqeHead` 原子推進即可，因為 cqe 本來就允許亂序（每筆自帶 `index` 與 `userdata`）。只有 sqe 的消費水位線必須有序。

### 8.8 使用者端等待

`src/lib/api/UsrbIo.cc:683-770`：

```
hf3fs_wait_for_ios(ior, cqes, cqec, min_results, abs_timeout)
loop:
  done = ring.cqeCount()
  若有 → 搬出來（每搬一筆檢查 cqeTail != cqeHead，防多消費者搶）
       → 同時 ring.slots.dealloc(cqe.index) 歸還 IoArgs 槽位
  若 filled >= min_results → return
  若還沒到 abs_timeout → sem_timedwait(cqeSem, now + jitter)    ← jitter 預設 1ms
```

同樣是 `sem_timedwait` + 輪詢兜底，且 jitter 可用環境變數 `HF3FS_USRBIO_WAIT_JITTER_MS` 調（`UsrbIo.cc:697`）。

`UsrbIo.cc:736` 還有一處 `hf3fs_submit_ios(ior); continue;`。它位於 `if (done)` 區塊**之內**（`UsrbIo.cc:707-737`），也就是**剛成功搬出至少一筆 cqe 之後**才執行——不是「cqe 為空時的自癒」。原始碼註解就寫在上一行：

```cpp
// post sem to signal the available slots in cqe section
hf3fs_submit_ios(ior);
```

語意是「cqe 區段騰出空位了，請 FUSE 繼續派工」。這件事有必要，因為 `jobsToProc` 的 `cqeAvail = entries - 1 - processing_ - cqeCount()`（`IoRing.cc:22`）會在 cqe 快滿時停止產生 job（`IoRing.cc:27` 註解：「even if we finish the io, we got no place to store the results」）。cqe 真正為空時走的是另一條路：`if (filled >= min_results) return filled;`（`UsrbIo.cc:740-742`）或 `sem_timedwait`（`UsrbIo.cc:766`），兩者都不 post。

### 8.9 `hf3fs_reg_fd`：fd → InodeId 且用 `dup` 釘住生命週期

```cpp
// src/lib/api/UsrbIo.cc:558-596
auto is3fs = hf3fs_is_hf3fs(fd);                    // ioctl(GET_MAGIC_NUM)
statx(fd, "", AT_EMPTY_PATH | AT_STATX_DONT_SYNC, STATX_INO, &stx);
auto dupfd = dup(fd);
auto regfd = std::make_shared<Hf3fsRegisteredFd>(fd, dupfd, InodeId{stx.stx_ino}, fcntl(fd, F_GETFL));
regfds[fd].compare_exchange_strong(empty, regfd);
regfds[dupfd].compare_exchange_strong(empty, regfd);
return -dupfd;
```

三個設計點：

1. **`AT_STATX_DONT_SYNC`**：明確告訴 FUSE「我只要 inode 號，別去 meta server 同步屬性」。避免每次註冊都打一次 RPC。
2. **`dup(fd)`**：註冊時複製一份 fd 並持有到 `hf3fs_dereg_fd` 為止。這樣即使應用程式 `close()` 了原始 fd，核心也不會發 FUSE_RELEASE，於是 `FileHandle` 存活、`RcInode` 的 refcount 不掉、`d.inodes` 裡的項目不被 erase——**`lookupFiles` 才查得到**。這是整個旁路能運作的前提，而且是完全隱式的。
3. **回傳 `-dupfd`**：函式回傳負值代表成功（回傳 dup 後的 fd），這是為了讓 `hf3fs_prep_io` 的 `abs(fd)` 同時接受原 fd 與 dupfd。

### 8.10 三級優先級

優先級從 iov 的 key 字串解析（`IovTable.cc:84-102`）：`.ph` → 0（high）、`.pn`/`.p` → 1（normal）、`.pl` → 2（low）。key 的完整文法（`IovTable.cc:28-111`）：

```
<uuid>[.b<blockSize>][.r<ioDepth>|.w<ioDepth>][.t<timeoutMs>][.f<flagsBinary>][.p{h,n,l}]
```

例如 `deadbeef-….b4096.r0.t100.pl` 表示「block size 4096、讀環、io_depth=0、timeout 100ms、低優先級」。**整個 IoRing 的配置參數全部編碼在 symlink 的檔名裡**——因為建立 IoRing 的唯一 API 是 `symlink()`，只有兩個字串可用（檔名與 target），target 已經被 shm 路徑佔了。

worker 協程的優先級分配（`FuseClients.cc:227`）：

```cpp
auto prio = i < hiThs ? 0 : i < (ths - loThs) ? 1 : 2;
```

`io_worker_coros.hi = 8`、`lo = 8`、總數 `batch_io_coros = 128` → 8 個高優先、112 個普通、8 個低優先。

worker 的搶工作邏輯（`FuseClients.cc:228-275`）在 `enable_priority` 開啟時才生效（預設 false），核心是 `checkHigher` 這個 per-worker 的布林狀態機：

```cpp
// 若 checkHigher，先看更高優先級的佇列是否「滿」，滿了就搶一筆
if (checkHigher) {
  for (int nprio = 0; nprio < prio; ++nprio) {
    if (iojqs[nprio]->full()) {
      auto dres = iojqs[nprio]->try_dequeue();
      if (dres) { checkHigher = false; gotJob = true; job = std::move(*dres); break; }
    }
  }
}
// 然後從 checkHigher ? 高→低 : 低→高 的方向逐一 co_dequeue（帶 1ms 超時）
for (int nprio = checkHigher ? 0 : prio; checkHigher ? nprio <= prio : nprio >= 0;
     nprio += checkHigher ? 1 : -1) { ... }
```

`checkHigher` 在搶到高優先級工作後變 false，下一輪就從自己的優先級往低找；只有當它真的拿到自己優先級的工作時才變回 true（`FuseClients.cc:262`）。這是一個**明確的反飢餓機制**：註解說「checkHigher is used to make sure the job queue with the thread's own priority doesn't starve」。

同時「高優先級佇列很小（32）」與「只有佇列滿了才越級搶」是配套的：小佇列讓「滿」這個訊號來得早且準確。

處理完一批之後還有一個工作竊取式的續接（`FuseClients.cc:343-353`）：

```cpp
if (iojqs[0]->full() || job.ior->priority != prio) {
  sem_post(iors.sems[job.ior->priority].get());     // 喚醒對應的 watcher，讓它重新分派
} else {
  auto jobs = job.ior->jobsToProc(1);               // 自己直接續接同一個 ring 的下一批
  if (!jobs.empty()) { job = jobs.front(); if (!iojqs[0]->try_enqueue(job)) continue; }
}
```

若處理的 ring 就是自己的優先級且高優先佇列不忙，就**不經過佇列直接續做下一批**，省掉一次入隊/出隊與執行緒切換。

---

## 9. 快取策略與失效

### 9.1 各層快取一覽

| 層 | 快取內容 | 控制項 | 預設 |
|---|---|---|---|
| 核心 dcache | dentry（name → inode） | `entry_timeout` | 30s |
| 核心 icache | attr（stat 結果） | `attr_timeout` | 30s |
| 核心 dcache（負） | ENOENT 結果 | `negative_timeout` | 5s |
| 核心（symlink） | symlink 的 entry+attr | `symlink_timeout` | 5s |
| 核心 page cache | 檔案資料 | `enable_read_cache` → `fi->direct_io` | 開啟（即用 page cache） |
| 核心 writeback | 髒頁 | `enable_writeback_cache` | **關閉** |
| daemon | `d.inodes`（RcInode） | lookup count | — |
| daemon | `readdirplusResults` | dirId 生命週期 | — |
| daemon | 寫緩衝 `InodeWriteBuf` | `io_bufs.write_buf_size` | 1MB |

四個 timeout 全部是 `CONFIG_HOT_UPDATED_ITEM` 且都在 `UserConfig::userKeys` 裡（`UserConfig.h:31-39`），意即**每個 uid 可以有自己的一組快取超時**。

`symlink_timeout` 特別短（5s vs 30s）的理由在 `hf3fs_lookup`（`FuseOps.cc:718-721`）：

```cpp
if (res->isSymlink()) {
  e.attr_timeout  = d.userConfig.getConfig(userInfo).symlink_timeout();
  e.entry_timeout = d.userConfig.getConfig(userInfo).symlink_timeout();
}
```

因為 `/3fs-virt` 底下的 API 全是 symlink，而那些 symlink 的「值」（例如設定值）隨時會變；同時真實的 symlink 在 3FS 裡也比一般檔案更常被當成可變的指標使用。

### 9.2 `direct_io` 的三態判斷

```cpp
// src/fuse/FuseOps.cc:1463  hf3fs_open
fi->direct_io = (fi->flags & O_DIRECT)
              || (!d.userConfig.getConfig(userInfo).enable_read_cache() && !(fi->flags & O_NONBLOCK))
                ? 1 : 0;
```

註解（`FuseOps.cc:1461-1462`）：

> O_DIRECT open means direc io
> read cached disabled && not O_NONBLOCK open (for mmap) means direct io too

第二條的 `O_NONBLOCK` 判斷是一個**hack**：`fi->direct_io = 1` 會讓核心不建立 page cache，而 **mmap 需要 page cache**——FUSE 的 `direct_io` 檔案無法 mmap。於是他們約定：想要 mmap 但又處在 `enable_read_cache=false` 模式下的應用，用 `O_NONBLOCK` 當旗標告訴 daemon「別給我 direct_io」。`O_NONBLOCK` 對一般檔案本來就沒有語意，被徵用來當私有旗標。

`hf3fs_create` 的判斷略有不同（`FuseOps.cc:1895`）：

```cpp
fi->direct_io = (!enable_read_cache() || fi->flags & O_DIRECT) ? 1 : 0;
```

沒有 `O_NONBLOCK` 例外——新建檔案不會馬上被 mmap。

### 9.3 主動失效：`notify_inval_*`

```cpp
// src/fuse/FuseOps.cc:315
void notify_inval_inode(InodeId inodeId) { fuse_lowlevel_notify_inval_inode(d.se, linux_ino(inodeId), -1, 0); }
void notify_inval_entry(InodeId parent, std::string name) { fuse_lowlevel_notify_inval_entry(d.se, linux_ino(parent), name.c_str(), name.size()); }
```

呼叫點：

| 場景 | 位置 | 為什麼 |
|---|---|---|
| sync 成功（非 lookup/getattr） | `FuseOps.cc:537` | 長度變了，核心的 attr 快取與 page cache 過期 |
| close 且 `updateLength` | `FuseOps.cc:562` | 同上 |
| `HF3FS_IOC_FSYNC` | `FuseOps.cc:2045` | 強制刷新 |
| rm-rf symlink 成功 | `FuseOps.cc:1210`、`1212` | 被刪目錄的 entry + 剛建的假 symlink |
| `mv:` symlink 成功 | `FuseOps.cc:1295-1305` | 來源與目標 entry |
| `HF3FS_IOC_MOVE` / `HF3FS_IOC_REMOVE` | `FuseOps.cc:2133`、`2162` | 同上 |

**`sync()` 裡的那個 if 是一個很重要的細節**（`FuseOps.cc:534-538`）：

```cpp
if (type != SyncType::GetAttr && type != SyncType::Lookup) {
  // NOTE: shouldn't call inval inode during get_attr or lookup,
  // otherwise fuse may think the result of get_attr or lookup is invalid.
  notify_inval_inode(inode.inode.id);
}
```

`lookup`/`getattr` 內部會為了拿到準確長度而先 sync 一次（`sync_on_stat`，預設 true）。若此時 invalidate inode，核心會認為「這個 inode 剛剛被作廢」而丟棄正要回傳的那筆 attr——導致 stat 的結果被自己的失效通知打掉。

**`notifyInvalExec` 這個 32 執行緒的 IO 池存在的理由**（`FuseClients.h:240`、`FuseClients.cc:171`）：`fuse_lowlevel_notify_inval_entry` 會等核心回應，而核心在處理 invalidate 時可能需要取 inode 鎖——若當前執行緒正在處理一個持有同一把鎖的請求，就會**死鎖**。所以所有「回應之後才做的失效」都被丟到獨立執行緒池：

```cpp
// src/fuse/FuseOps.cc:1212
fuse_reply_entry(req, &e);
d.notifyInvalExec->add([parent, name = std::string(name)]() { notify_inval_entry(parent, name); });
```

更精細的是 `mv:` 路徑（`FuseOps.cc:1295-1305`），它連 `fuse_reply_entry` 本身都丟進池裡，並且依 `res->inode.id != parent` 決定 reply 與 invalidate 的先後順序：

```cpp
d.notifyInvalExec->add([res, viid, parent, name, req, e]() {
  if (res->inode.id != parent) { notify_inval_entry(res->inode.id, res->name); fuse_reply_entry(req, &e); }
  else                          { fuse_reply_entry(req, &e); notify_inval_entry(res->inode.id, res->name); }
  notify_inval_entry(parent, name);
  notify_inval_inode(viid);
});
```

同一個父目錄時必須先回應再失效（否則核心在等我們的回應，而我們在等核心處理失效）；不同父目錄則可以先失效。

### 9.4 `readdirplus` 快取與分頁

`hf3fs_opendir` 只發一個遞增的 `dirId`（`FuseOps.cc:1782`），不做任何 RPC。真正的列舉在第一次 `readdirplus` 時發生，而且是**一次撈完整個目錄**：

```cpp
// src/fuse/FuseOps.cc:2241-2262
bool hasNext = true;
std::string_view prev;
while (hasNext) {
  auto ret = withRequestInfo(req, d.metaClient->list(userInfo, ino, std::nullopt, prev, 256, false));
  for (auto &ent : ret->entries) { entries.push_back(std::move(ent)); inodes.push_back(std::nullopt); }
  hasNext = ret->more;
  if (hasNext) prev = entries.back().name;
}
```

每次 256 筆，迴圈到 `more == false`。結果整份存進 `d.readdirplusResults[dirId]`，之後每次 `readdirplus(off)` 都從這份快照切片。

**一致性語意**：整個 `opendir`→`releasedir` 週期內看到的是**同一個時間點的快照**。目錄中途新增/刪除的檔案不會出現/消失。這是「repeatable read」而非 POSIX 要求的較弱語意，實際上更強。代價是：超大目錄會在第一次 readdir 時把整份 entry list 拉進記憶體，且分頁 RPC 期間阻塞該 FUSE 執行緒。

`needStatus = false`（`FuseOps.cc:2248` 最後一個參數）——list 不順帶拿 inode 內容，`inodes` 全填 `nullopt`。真正的 stat 延後到切片時才用 `batchStat` 補（`FuseOps.cc:2306-2323`）：

```cpp
std::vector<InodeId> queryIds;
for (auto i = off; i < last; i++) if (!inodes[i].has_value()) queryIds.push_back(entries[i].id);
auto queryRet = withRequestInfo(req, d.metaClient->batchStat(userInfo, queryIds));
```

**只 stat 這一頁要用到的**。這是正確的取捨：`ls` 很可能只讀前幾頁就被中斷（例如 `ls | head`），一次 stat 全部是浪費。

切片邊界的計算複製了核心的 dirent 對齊規則（`FuseOps.cc:2292-2304`）：

```cpp
#define FUSE_NAME_OFFSET_DIRENTPLUS 152
#define FUSE_REC_ALIGN(x) (((x) + sizeof(uint64_t) - 1) & ~(sizeof(uint64_t) - 1))
auto entsize = FUSE_DIRENT_ALIGN(FUSE_NAME_OFFSET_DIRENTPLUS + entries[last].name.size());
if (cursize + entsize < size) { cursize += entsize; last++; } else break;
```

152 這個魔數是 `struct fuse_direntplus` 的 name 欄位偏移（`fuse_entry_out` 128 bytes + `fuse_dirent` header 24 bytes）。硬編碼在這裡是為了**先算出這一頁裝得下幾筆，才知道要 batchStat 哪些 id**——若等到 `fuse_add_direntry_plus` 才發現裝不下，就已經多 stat 了。

`off` 的處理有一個 `kOffsetBegin = 2` 的偏移（`FuseOps.cc:2174-2177`、`2352`）：

```cpp
static constexpr off_t kOffsetBegin = 2;
if (off > kOffsetBegin) off -= kOffsetBegin;
...
auto entsize = fuse_add_direntry_plus(req, p, rem, name.data(), &e, i + 1 + kOffsetBegin);
```

第 `i` 筆 entry 的 cookie 是 `i + 3`，於是 offset 1 與 2 被保留（慣例上留給 `.` 與 `..`）。但**這個實作從不回傳 `.` 與 `..`**——entry list 完全來自 `metaClient->list` 加上根目錄的 `3fs-virt`。保留的兩個 offset 從未被使用。

`releasedir` 才清掉快取（`FuseOps.cc:1808-1814`）。若應用程式開著 dirfd 不放，這份快照就一直佔記憶體。

---

## 10. 動態設定 `UserConfig`

### 10.1 兩層設定：sys 與 usr

```cpp
// src/fuse/UserConfig.h:23-39
const std::vector<std::string> systemKeys{
    "storage.net_client.rdma_control.max_concurrent_transmission",
    "periodic_sync.enable", "periodic_sync.interval", "periodic_sync.flush_write_buf",
    "io_worker_coros.hi", "io_worker_coros.lo", "max_jobs_per_ioring", "io_job_deq_timeout"};
const std::vector<std::string> userKeys{
    "enable_read_cache", "readonly", "dryrun_bench_mode", "flush_on_stat", "sync_on_stat",
    "attr_timeout", "entry_timeout", "negative_timeout", "symlink_timeout"};
```

`sys.*` 改的是**全域** `FuseConfig`（影響所有人）；`usr.*` 改的是**呼叫者 uid 專屬的一份副本**。

### 10.2 per-uid 副本的實作

```cpp
// src/fuse/UserConfig.h:50-59
struct LocalConfig {
  LocalConfig(const FuseConfig &globalConfig) : config(globalConfig) {}
  std::mutex mtx;
  FuseConfig config;                            // ← 整份 FuseConfig 的深拷貝
  std::vector<config::KeyValue> updatedItems;   // ← 這個 uid 改過哪些項
};
std::unique_ptr<AtomicSharedPtrTable<LocalConfig>> configs_;   // 以 uid 為 index
std::set<meta::Uid> users_;
```

`configs_` 的容量是 `max_uid + 1`（預設 `1_M + 1`）——一個以 **uid 為索引的百萬項稀疏陣列**（`UserConfig.cc:8`）。`AtomicSharedPtrTable` 是 `vector<folly::atomic_shared_ptr<T>>`，未使用的項是空指標，實際記憶體只有 100 萬個指標（8MB）。用陣列而非 map 的理由：`getConfig()` 在**每一個 FUSE 操作**中都被呼叫多次（例如 `hf3fs_lookup` 就叫了三次），必須是 O(1) 且免鎖查表。

不過 `getConfig` 目前仍要拿 `userMtx_`（`UserConfig.cc:120`）來查 `users_` set：

```cpp
const FuseConfig &UserConfig::getConfig(const meta::UserInfo &ui) {
  std::lock_guard lock(userMtx_);
  auto it = users_.find(ui.uid);
  if (it == users_.end()) return *config_;
  return configs_->table[ui.uid.toUnderType()]->config;
}
```

這把全域 mutex 在每個 FUSE 操作上被搶多次，是可見的擴展性瓶頸（`atomic_shared_ptr` 的 load 本可免鎖，`users_` 這層 set 查詢是多餘的）。

**回傳型別是 `const FuseConfig &`** —— 回傳的是 `LocalConfig::config` 的參考，而 `LocalConfig` 由 `shared_ptr` 持有，該 `shared_ptr` 在函式返回後就沒有持有者了。若同時有人 `setConfig` 把該格換掉，這個參考會懸空。實務上 `setConfig` 只做 in-place `atomicallyUpdate` 不換指標（`UserConfig.cc:98`），只有全域熱更新回調會 `lconf->config = std::move(conf2)`（`UserConfig.cc:26`）——那才是真正的競態窗口。

### 10.3 全域設定變更時的重放

```cpp
// src/fuse/UserConfig.cc:12-28
config.addCallbackGuard([&config, this] {
  storageMaxConcXmit_ = config.storage()...max_concurrent_transmission();
  std::lock_guard lock(userMtx_);
  for (auto u : users_) {
    auto lconf = configs_->table[u.toUnderType()].load();
    FuseConfig conf2 = config;                             // 從新的全域設定重新拷一份
    std::lock_guard lock2(lconf->mtx);
    conf2.atomicallyUpdate(lconf->updatedItems, true);     // 重放該 uid 的所有覆寫
    lconf->config = std::move(conf2);
  }
});
```

這是**覆寫的重放（replay）而非合併**：全域設定變了之後，每個 uid 的設定 = 新全域設定 + 該 uid 歷來所有覆寫依序重放。這保證了「使用者沒改過的項目會跟著全域走」，而不是被凍結在建立副本的那一刻。`updatedItems` 是 append-only 的清單（`UserConfig.cc:100`），從不去重——同一個 key 改十次就重放十次，最後一次生效。

### 10.4 兩道護欄

```cpp
// src/fuse/UserConfig.cc:58-69  護欄一：RDMA 併發上限不得超過系統值的兩倍
if (!strcmp(key, "storage.net_client.rdma_control.max_concurrent_transmission")) {
  auto n = atoi(val);
  if (n <= 0 || n > 2 * storageMaxConcXmit_) return makeError(kInvalidArg, "...larger than twice of system setting");
}
// src/fuse/UserConfig.cc:75-78  護欄二：admin 開了 readonly，使用者不能自己關掉
if (!strcmp(key, "readonly") && strcmp(val, "true") && config_->readonly()) {
  return makeError(kInvalidArg, "cannot turn off readonly mode when it is turned on by the sys admin");
}
```

第二條是必要的：`readonly` 在 `userKeys` 裡（使用者可自訂），但若沒有這道檢查，任何使用者都能用 `ln -s false .../set-conf/usr.readonly` 繞過整個叢集的唯讀保護。

### 10.5 讀寫介面

| 操作 | 實作 |
|---|---|
| 讀單項 | `readlink(<mount>/3fs-virt/get-conf/usr.readonly)` → symlink target 就是值（`UserConfig.cc:143` `config.find(key).value()->toString()`） |
| 列全部 | `ls <mount>/3fs-virt/get-conf/` → `listConfig` 產生 17 個 entry |
| 寫 | `ln -s <value> <mount>/3fs-virt/set-conf/usr.readonly` |

權限用 mode 表達（`UserConfig.cc:144`）：`sys.*` 是 `0444`（人人可讀），`usr.*` 是 `0400`（只有 owner 可讀）。

inode id 由 key index 反算（`UserConfig.h:43`）：

```cpp
meta::InodeId configIid(bool isGet, bool isSys, int kidx) {
  return meta::InodeId{(isGet ? InodeId::getConf() : InodeId::setConf()).u64() - 1
                       - (isSys ? 0 : systemKeys.size()) - kidx};
}
```

**這意味著 key 清單的順序是 ABI**：在 `systemKeys` 中間插入一個新 key 會改變後面所有 key 的 inode id。核心的 dcache 若還快取著舊 id，就會指到錯的設定項。所以新 key 只能 append。

---

## 11. 併發模型與背景任務

### 11.1 執行緒總覽

| 執行緒/協程 | 數量 | 來源 | 職責 |
|---|---|---|---|
| libfuse worker | ≤ `min(max_threads, cores/2)` | `fuse_session_loop_mt` | 所有 `fuse_lowlevel_ops` callback |
| `FuseClients::watch` | 3（每優先級一條） | `std::jthread` | `sem_timedwait` + 掃 ring 產生 job |
| `ioRingWorker` | `batch_io_coros = 128` | `bgThreadPool` 上的協程 | 執行 IoRingJob |
| `periodicSyncWorker` | `CoroutinesPool`，4 條協程 | `bgThreadPool` | 逐 inode 做 period sync |
| `periodicSyncRunner` | 1 | `BackgroundRunner` | 每 30s±30% 掃 dirtyInodes |
| `notifyInvalExec` | 32 | `IOThreadPoolExecutor` | 延後的 `notify_inval_*` + 部分 reply |
| net / storage / meta client | 各自的 `tpg()` 執行緒組 | — | RPC 收發、RDMA |

### 11.2 `blockingWait`：FUSE callback 全部是同步的

```cpp
// src/fuse/FuseOps.cc:85-89
template <typename Awaitable>
auto withRequestInfo(fuse_req_t req, Awaitable &&awaitable) {
  auto guard = RequestInfo::set(req);
  return folly::coro::blockingWait(std::forward<Awaitable>(awaitable));
}
```

**每一個需要 RPC 的 FUSE callback 都用 `blockingWait` 把協程跑完才返回**。也就是說：

- FUSE 路徑的併發度 = libfuse worker 執行緒數（≤ cores/2），不是協程數。
- 一次慢的 meta RPC 會佔住一整條 libfuse 執行緒。
- 這是為什麼 `max_threads` 被限制在 cores/2 就足夠——再多也只是在 FUSE 的 spinlock 上排隊。
- 也是為什麼旁路 IoRing 值得存在：它的併發度是 128 個協程，且不佔用 libfuse 執行緒。

`RequestInfo` 這個 RAII guard（`FuseOps.cc:64-83`）把 `fuse_req_t` 塞進 folly 的 RequestContext，讓下游的 RPC 層能查詢「這個請求被中斷了嗎」：

```cpp
bool canceled() const override {
  if (!d.config->enable_interrupt()) return false;
  return fuse_req_interrupted(req_);
}
std::string describe() const override { return fmt::format("{}@fuse", fuse_req_ctx(req_)->pid); }
```

`enable_interrupt` 預設 false（`FuseConfig.h:24`）。開啟後，使用者按 Ctrl-C 產生的 FUSE_INTERRUPT 能讓進行中的 meta/storage RPC 提前放棄。`describe()` 讓伺服器端的日誌能標出是哪個 pid 發的請求。中斷會被映射成 `EINTR` 而非 `ECANCELED`（`StatusCode.cc:59-62`，註解明說 `// NOTE: use EINTR instead of ECANCELED`）。

### 11.3 periodic sync

```
periodicSyncRunner (BackgroundRunner)                FuseClients.cc:161
  間隔 = periodic_sync.interval(30s) × Random(0.7, 1.3)   ← 抖動防同步風暴
  └─ periodicSyncScan()                              FuseClients.cc:403
     ├─ 若 !enable 或 readonly → 直接 return
     ├─ 取 dirtyInodes 的鎖
     ├─ size <= limit(1000) → 整批 exchange 出來
     │  size >  limit       → 從 lastSynced 開始環狀取 limit 個（公平輪轉）
     └─ 逐個 co_await periodicSyncWorker->enqueue(inode)
                                    │
                                    ▼
        periodicSync(inodeId) ×4 協程                FuseOps.cc:2671
        ├─ pi = inodeOf(inodeId)；沒了就跳過
        ├─ writer = pi->dynamicAttr.rlock()->writer
        ├─ userInfo = UserInfo(writer, Gid(writer), fuseToken)   ← 注意這裡
        ├─ 若該 user readonly → return
        ├─ [flush_write_buf] wbMtx.try_lock() 成功才刷            ← 非阻塞
        └─ sync(userInfo, *pi, SyncType::PeriodSync)
```

**超限時的公平輪轉**（`FuseClients.cc:417-430`）：

```cpp
auto iter = guard->find(lastSynced);
while (dirty.size() < limit) {
  if (iter == guard->end()) { iter = guard->begin(); }
  else { auto inode = *iter; lastSynced = inode; iter = guard->erase(iter); dirty.insert(inode); }
}
```

`dirtyInodes` 是 `std::set<InodeId>`（有序），`lastSynced` 記住上次掃到哪，下次從那裡繼續，掃到尾就回頭。這保證髒 inode 數量長期超過 limit 時**每個 inode 最終都會被 sync 到**，不會有 inode 因為 id 太大而永遠餓死。

**`try_lock` 而非 `lock`**（`FuseOps.cc:2692`）：背景 sync 絕不與前台寫入搶 `wbMtx`。搶不到就跳過刷緩衝，只做 sync。這是正確的優先級決策——背景任務不該讓前台的 `write()` 變慢。

**`UserInfo(writer, flat::Gid(writer), fuseToken)`（`FuseOps.cc:2686`）把 gid 設成 uid**。這是一個真實的隱患：periodic sync 是以「最後一個寫入者」的身分向 meta server 發 sync，但只保留了 uid，gid 被偽造成與 uid 相同的值。若 meta 端的權限檢查依賴 gid，這個 sync 可能被拒絕（或更糟，通過了不該通過的檢查）。根因是 `DynamicAttr` 只存了 `flat::Uid writer`（`FuseClients.h:72`），沒存完整的 `UserInfo`。

相關的另一處：`IoRing::process` 的寫入完成路徑（`IoRing.cc:218`）：

```cpp
inode->finishWrite(userInfo_.uid, truncateVers[i], off, r);
```

`finishWrite` 的簽章第一個參數是 `flat::UserInfo`，這裡傳的是 `flat::Uid`。因為 `UserInfo(Uid uid = {}, Gid gid = {}, ...)` 不是 explicit（`src/fbs/core/user/User.h:33`），會**隱式建構出一個 gid=0、token 為空的 UserInfo**。`finishWrite` 只讀 `userInfo.uid`（`FuseOps.cc:2662`），所以目前沒有實際危害，但這種隱式轉換一旦 `finishWrite` 將來用到 token 就會出事。

`monitor::ValueRecorder dirtyInodesCnt("fuse.dirty_inodes")`（`FuseClients.cc:29`）每輪掃描時上報髒 inode 數（`docs/metrics.md:22` 有記載），是觀察寫入積壓的主要指標。

### 11.4 停機順序

`FuseClients::stop()`（`FuseClients.cc:178-216`）的順序不可調換：

```
notifyInvalExec 停 →  onFuseConfigUpdated 解除 →  cancelIos.requestCancellation()
→ ioWatches 全部 request_stop  →  periodicSyncRunner stopAll  →  periodicSyncWorker stopAndJoin
→ metaClient stop  →  storageClient stop  →  mgmtdClient stop  →  net::Client stopAndJoin
```

由上而下：先切斷新工作的來源（通知、設定回調、IO 產生器），再停背景任務，最後才拆 RPC 客戶端。`cancelIos` 是 `folly::CancellationSource`，128 個 `ioRingWorker` 協程透過 `co_withCancellation` 綁定（`FuseClients.cc:150`），取消時會拋 `OperationCancelled`，worker 捕捉後 break（`FuseClients.cc:358-365`）；任何其他例外則 `XLOGF(FATAL)` ——寧可 crash 也不吞掉未知錯誤。

`FuseClients::~FuseClients()` 也呼叫 `stop()`（`FuseClients.cc:48`），配合 `hf3fs_fuse.cpp:74` 的 `SCOPE_EXIT { d.stop(); }` 形成雙保險（`stop()` 對已停止狀態是冪等的，因為每個欄位都先檢查非空再 reset）。

---

## 12. 錯誤處理與 errno 映射

### 12.1 `handle_error`

```cpp
// src/fuse/FuseOps.cc:284
template <typename T>
void handle_error(fuse_req_t req, const hf3fs::Result<T> &ret, bool entry = false) {
  if (ret.hasError()) {
    if (ret.error().code() != MetaCode::kNotFound)
      XLOGF(INFO, "  handle_error({}, {}, pid={}, cmdline={})", code, msg, pid, proc_cmdline(pid));
    else
      XLOGF(OP_LOG_LEVEL, "  handle_error({}, {}, pid={})", code, msg, pid);

    int err = StatusCode::toErrno(ret.error().code());
    if (entry && code == MetaCode::kNotFound && negative_timeout() != 0) {
      fuse_entry_param e{}; e.entry_timeout = negative_timeout();
      fuse_reply_entry(req, &e);        // ← ino = 0，這是 FUSE 的「負快取項」約定
    } else {
      fuse_reply_err(req, err);
    }
  }
}
```

兩個設計點：

1. **`kNotFound` 用 DBG 級別，其餘用 INFO 並額外讀 `/proc/<pid>/cmdline`**。ENOENT 在檔案系統裡是常態（每次 `open(O_CREAT)` 前的 stat 都會產生一次），若用 INFO 記錄會淹沒日誌；其他錯誤則值得知道是哪個程式發的。代價是 `proc_cmdline()` 每次都開檔讀 `/proc`，在錯誤風暴時本身就是負擔。

2. **負快取**（`entry == true` 且 `negative_timeout != 0`）：回一個 `ino = 0` 的 `fuse_entry_param`，這是 FUSE 協定裡「這個名字確定不存在，請快取 N 秒」的表示法。只有 `hf3fs_lookup` 傳 `entry=true`（`FuseOps.cc:716`）。這對「反覆 stat 不存在的檔案」（Python import 路徑搜尋是典型）幫助很大。

### 12.2 errno 對照表

`src/common/utils/StatusCode.cc:39-104`：

| 內部碼 | errno | 備註 |
|---|---|---|
| 任何 `StatusCodeType::RPC` | `EREMOTEIO` | 整類一網打盡 |
| `ClientAgentCode::kTooManyOpenFiles` | `EMFILE` | iov/ioring 表滿 |
| `StatusCode::kInvalidArg` | `EINVAL` | |
| `StatusCode::kNotImplemented` | `ENOSYS` | |
| `StatusCode::kNotEnoughMemory` | `ENOMEM` | |
| `StatusCode::kAuthenticationFail` | `EPERM` | token 錯 |
| `StatusCode::kReadOnlyMode` | `EROFS` | |
| `MetaCode::kRequestCanceled` / `StorageClientCode::kRequestCanceled` | `EINTR` | **刻意不用 ECANCELED** |
| `StorageClientCode::kNoSpace` | `ENOSPC` | |
| `MetaCode::kNotFound` | `ENOENT` | |
| `MetaCode::kNotEmpty` | `ENOTEMPTY` | |
| `MetaCode::kNotDirectory` | `ENOTDIR` | |
| `MetaCode::kTooManySymlinks` | `ELOOP` | |
| `MetaCode::kIsDirectory` | `EISDIR` | |
| `MetaCode::kExists` | `EEXIST` | |
| `MetaCode::kNoPermission` | `EPERM` | |
| `MetaCode::kInconsistent` | `EIO` | |
| `MetaCode::kNotFile` | `EBADF` | |
| `MetaCode::kMoreChunksToRemove` | `EAGAIN` | 大檔刪除需重試 |
| `MetaCode::kNameTooLong` | `ENAMETOOLONG` | |
| `MetaCode::kNoLock` | `ENOLCK` | 目錄鎖 |
| `MetaCode::kFileTooLarge` | `EFBIG` | |
| `ClientAgentCode::kHoleInIoOutcome` | `ENODATA` | `HF3FS_IOR_FORBID_READ_HOLES` 才會出現 |
| `ClientAgentCode::kOperationDisabled` | `EOPNOTSUPP` | |
| `ClientAgentCode::kIovNotRegistered` / `kIovShmFail` | `EACCES` | |
| **其餘全部** | `EIO` | |

`MetaCode::kMoreChunksToRemove → EAGAIN` 是很特別的一項：刪除超大檔案時，meta server 無法在一個交易裡刪光所有 chunk，於是回一個「還有更多要刪」的碼，客戶端（或使用者）需要重試。這把「分批刪除」的責任推給了呼叫者。

USRBIO 的 cqe 也走同一套映射（`IoRing.cc:262`）：

```cpp
addCqe(sqe.index, r >= 0 ? r : -static_cast<ssize_t>(StatusCode::toErrno(-r)), sqe.userdata);
```

`res[i]` 內部一路都是「負的內部 status code」，只在寫進 cqe 的最後一刻才轉成負 errno。這樣中間層可以區分 `kChunkNotFound` 與 `kNoSpace`，而使用者只看到 POSIX errno。

### 12.3 `XLOGF(FATAL)` 的使用

程式碼裡有多處 `FATAL`（直接中止進程），全部是「不變式被破壞」而非「外部錯誤」：

| 位置 | 條件 |
|---|---|
| `FuseClients.cc:58` | 掛載名 ≥ 32 字元（會讓 ioctl `strcpy` 溢位） |
| `FuseOps.cc:210` | inode 既非 file/dir/symlink |
| `FuseOps.cc:526` | `syncver > writever`（版本號倒退） |
| `FuseOps.cc:545` | sync 成功但回傳的不是檔案 |
| `FuseOps.cc:2682` | periodic sync 拿到的 inode id 與請求不符 |
| `FuseClients.cc:422` | dirtyInodes 迭代器兩次都到 end |
| `IoRing.cc:235`、`245` | `sqeProcTails_` 為空（重排狀態機壞了） |
| `IoRing.cc:264` | `addCqe` 失敗（`cqeAvail` 預留算錯了） |
| `IoRing.h:109` | semaphore 位址超出 shm 範圍 |

態度很明確：**共享記憶體的狀態機一旦不一致就立即死掉**，因為繼續跑下去只會寫壞使用者的記憶體。

---

## 13. 設計取捨、POSIX 語意缺口與潛在坑

### 13.1 明確不支援 / 部分支援的 POSIX 語意

| 語意 | 狀態 | 依據 |
|---|---|---|
| **hard link** | **支援**。`hf3fs_link` → `metaClient->hardLink`（`FuseOps.cc:1407`），另有 `HF3FS_IOC_HARDLINK` 走 inode 號 | `FuseOps.cc:1379` |
| **mmap** | 支援，但**只在非 direct_io 時**。`enable_read_cache=false` 時要用 `O_NONBLOCK` 這個私有旗標才能 mmap | `FuseOps.cc:1461-1466` |
| **O_APPEND** | **不支援原子性**。`hf3fs_open` 只用 O_APPEND 判斷是否為寫模式（`FuseOps.cc:1422`），之後完全不理它；append 由核心轉成「用當前 i_size 當 offset 的 write」，而 i_size 可能是快取的舊值 → 併發 append 會互相覆蓋 |
| **flock / fcntl 鎖** | **完全不支援**。ops 表無 `getlk`/`setlk`/`flock`，核心會退化成本地鎖，跨節點無效 | `FuseOps.cc:2580-2613` |
| **目錄鎖** | 有自訂替代品：`setfattr -n hf3fs.lock -v try_lock <dir>` | `FuseOps.cc:2412-2432` |
| **xattr** | 只支援 `hf3fs.` 前綴，且實際上只有 `hf3fs.lock` 一個 key | `FuseOps.cc:2390` |
| **ACL / POSIX ACL** | 不支援。只有傳統的 uid/gid/mode | `fillLinuxStat` `FuseOps.cc:212` |
| **fallocate** | 不支援（回 ENOSYS）。打洞走 `HF3FS_IOC_PUNCH_HOLE`，且必須 chunk 對齊 | `FuseOps.cc:2072` |
| **稀疏檔案** | 讀取支援（零填），但 `st_blocks` 直接用 `(size+511)/512` 硬算，註解寫 `// we don't allow holes` | `FuseOps.cc:219` |
| **`.` / `..`** | readdirplus **不回傳**這兩項 | `FuseOps.cc:2241-2274` |
| **mknod 特殊檔** | 只允許 REG/DIR/LNK，其餘回 ENOSYS（無 FIFO / socket / device） | `FuseOps.cc:985` |
| **rename flags** | `RENAME_NOREPLACE` / `RENAME_EXCHANGE` 一律 ENOSYS | `FuseOps.cc:1367` |
| **`st_dev` / `st_rdev`** | 恆為 0 | `FuseOps.cc:202` |
| **`f_bavail`** | 直接等於 `f_bfree`，`// TODO: quota` | `FuseOps.cc:1854` |
| **`statfs` block size** | 硬寫 2MB（`2 << 20`），註解說是為了誘導核心合併成更大的讀寫 | `FuseOps.cc:1851` |
| **併發寫同一檔** | Linux 5.x 的 FUSE 本身不支援；旁路 IoRing 可以繞過 | `docs/design_notes.md:33` |

`unlink` 與 `rmdir` 呼叫**同一個** `metaClient->remove(userInfo, parent, name, false)`（`FuseOps.cc:1077`、`1102`），沒有用專門的 `unlink()` / `rmdir()`（MetaClient 其實有提供，`MetaClient.h:180-183`）。這表示型別檢查完全交給伺服器端——`rmdir` 一個普通檔案會拿到伺服器回的 `kNotDirectory → ENOTDIR`，語意正確但多了一次無謂的 RPC 往返。

### 13.2 已知的粗糙處

**1. `RcInode::inode` 永不更新。**
`add_entry` 裡 `rcinode->inode = inode;` 被註解（`FuseOps.cc:241`）。後果：`hf3fs_readlink` 回的是第一次 lookup 時的 target；`PioV` 用的 layout 也是舊的。layout 不可變所以安全，但 symlink target 若被改（3FS 沒有改 symlink 的 API，所以目前無害）就會讀到舊值。這是一個**靠「這些欄位剛好不變」撐著的不變式**，沒有任何斷言保護。

**2. `hf3fs_setattr` 的多段操作非原子。**
`FuseOps.cc:833-898` 依序發三個獨立 RPC：`setPermission` → `truncate` → `utimes`。中間任一失敗就 return，前面已生效的不回滾。`chmod` + `truncate` 同時做（例如 `install -m 644` 的某些實作）時可能只做一半。

**3. `hf3fs_setattr` 對 `pi` 不做空檢查。**
`FuseOps.cc:822` `auto pi = inodeOf(*fi, ino);`，接著在 `FUSE_SET_ATTR_SIZE` 分支直接 `pi->wbMtx`（`FuseOps.cc:854`）。若 inode 不在 map 裡（例如 forget 與 setattr 競態），這是空指標解參考。相較之下 `hf3fs_open`（`FuseOps.cc:1435`）與 `hf3fs_release`（`FuseOps.cc:1748`）都有檢查。

**4. `hf3fs_read` 同樣不檢查 `pi`。**
`FuseOps.cc:1479-1480` 拿到 `pi` 立刻 `pi->dynamicAttr.wlock()`。

**5. periodic sync 的 gid 偽造。** 見 §11.3。

**6. `getConfig` 的全域 mutex。** 見 §10.2。每個 FUSE 操作要搶好幾次。

**7. `readdirplus` 的全量快取。**
超大目錄（百萬檔）第一次 readdir 會同步發數千次 `list` RPC，全部塞進記憶體，且整段期間佔住一條 libfuse 執行緒。

**8. `IovTable::addIov` 的 `while (true)` 迴圈。**
`IovTable.cc:155` 開了一個 `while (true)` 但所有路徑都在第一輪 return 或 return error——沒有任何 `continue`。這是重構後遺留的死迴圈外殼。

**9. `hf3fs_symlink` 的 iovs 分支錯誤處理。**
`FuseOps.cc:1236-1245`：`addIoRing` 失敗時有三個接連的缺陷，但**不會**造成永久阻塞：

```cpp
if (!res2) {
  handle_error(req, res);      // :1237 傳的是已成功的 res，不是失敗的 res2
}                              //       且沒有 return
// record the ior index for later removal
res->second->iorIndex = *res2;  // :1240 對 error 狀態的 Result 解參考
...
fuse_reply_entry(req, &e);      // :1245 無論如何都會執行
```

(a) `handle_error` 收到無錯誤的 Result，什麼都不做，錯誤被靜默吞掉；(b) 因為沒有 `return`，緊接著 `:1240` 對 error 狀態的 `Result`（即 `folly::Expected`，`src/common/utils/Result.h:118`）解參考，會丟 `BadExpectedAccess`——這裡位於 C 連結的 FUSE 回呼中，例外穿出 libfuse 的 C 框架多半直接 `std::terminate`，**比阻塞更糟**；(c) 即使不丟例外，`:1245` 也會回報 symlink 成功，但 io ring 其實沒註冊進 `IoRingTable`。

觸發條件是 `IoRingTable::addIoRing` 的 `ioRings->alloc()` 用盡（`IoRing.h:238-241`），即 io ring 數量達上限。

**10. `sqeTailAfter` 未被使用。**
`IoRing.h:147` 定義了一個處理環繞比較的函式，但整個 codebase 沒有呼叫者。

**11. `res` 大小與 `PioV` 的隱含契約。**
`PioV::addRead(idx, ...)` 直接寫 `res_[idx]`（`PioV.cc:24`），不檢查 `idx < res_.size()`。呼叫者必須自己保證。FUSE 路徑傳 `res(1)` 與 `idx=0`；IoRing 路徑傳 `res(toProc)` 與 `idx ∈ [0, toProc)`。

### 13.3 值得肯定的設計

**1. 虛擬目錄作為控制平面。** 不需要額外的 socket、不需要客戶端程式庫、shell 就能操作、天然有 uid 隔離（`statIov` 檢查 `shm->user != ui.uid`，`IovTable.cc:278`）。

**2. `releasedir` 清理孤兒 iov。**
`FuseOps.cc:1816-1830`：當 `/3fs-virt/iovs` 的最後一個持有者關閉 fd（進程死亡也算），掃描整張 iov 表，把 pid 相符的全部釋放。註解說得很明白：「releasedir() is called only the last process with the inherited fd closes it or exits」。這解決了「USRBIO 應用崩潰後共享記憶體洩漏」的問題——**用核心的 fd 生命週期當成分散式資源的租約**，不需要心跳。

**3. semaphore 名字帶隨機 UUID + 透過 symlink 發佈。** 見 §8.3。

**4. `dup(fd)` 隱式釘住 inode 生命週期。** 見 §8.9。

**5. hint 的單向毒化。** 見 §7.4。「不確定就不宣稱」比「盡量猜」正確得多。

**6. `synced` 與 `fsynced` 兩條水位線。** 讓便宜的 periodic sync 與昂貴的 fsync 各走各的，不互相污染。

**7. 反飢餓的 `checkHigher` 狀態機。** 用一個布林值在每個 worker 上實現「越級搶工作但不餓死自己」。

**8. `sem_timedwait` 而非 `sem_wait`。** 承認「喚醒訊號與可處理工作量之間沒有一一對應」，用 1ms 輪詢兜底而非追求完美的喚醒協定。

### 13.4 整體取捨總結

| 取捨 | 選擇 | 代價 |
|---|---|---|
| 完整 POSIX vs 效能 | 效能。放棄 flock、fallocate、原子 append | 部分應用需改寫 |
| FUSE 資料路徑 vs 旁路 | 兩者都有；FUSE 保相容、旁路保效能 | 兩條路徑共用狀態，複雜度高 |
| 準確長度 vs RPC 次數 | hint + 背景 sync + 失敗即降級 | 短暫的 `stat` 長度不準 |
| 一次撈完 readdir vs 串流 | 一次撈完 | 大目錄記憶體與延遲 |
| 每個 uid 一份設定 vs 全域 | 每個 uid 一份，百萬項陣列 | 8MB 常駐 + 全域 mutex |
| 執行緒數 | 硬性砍到 cores/2 | 使用者無法調高（即使真的需要） |
| 控制介面 | 虛擬目錄 + symlink | 語意詭異、`ln -s` 誤用風險 |
| 錯誤處理 | 不變式破壞就 FATAL | 可用性換正確性 |

---

## 14. 檔案索引

### `src/fuse/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `hf3fs_fuse.cpp` | 83 | 進入點；`#ifdef ENABLE_FUSE_APPLICATION` 分成 Application 版與手工版兩個 `main()` |
| `CMakeLists.txt` | 12 | 定義 `hf3fs_fuse` 靜態庫與 `hf3fs_fuse_main` binary，連結 fuse3 |
| `FuseApplication.h` | 55 | `ApplicationBase` 子類宣告，綁定 AppConfig / LauncherConfig / ConfigFetcher 四個型別 |
| `FuseApplication.cc` | 125 | Application 生命週期實作：parseFlags → initApplication → mainLoop → stop；用 pimpl 隱藏成員 |
| `FuseAppConfig.h` | 16 | 極簡的 app 層設定（`getNodeId()` 恆回 0——FUSE client 沒有 node id） |
| `FuseAppConfig.cc` | 10 | 轉呼叫 `ApplicationBase::initConfig` |
| `FuseLauncherConfig.h` | 24 | launcher 階段設定：cluster_id / mountpoint / token_file / allow_other / mgmtd 位址 |
| `FuseLauncherConfig.cc` | 10 | 轉呼叫 `app_detail::initConfigFromFile` |
| `FuseConfigFetcher.h` | 10 | 繼承 `MgmtdClientFetcher`，只覆寫 `completeAppInfo` |
| `FuseConfigFetcher.cc` | 19 | 向 mgmtd 查 `getUniversalTags(hostname)` 補進 AppInfo |
| `FuseConfig.h` | 92 | **全部執行期設定**：快取超時、IO 緩衝、優先級佇列、periodic sync、以及巢狀的 client/mgmtd/storage/meta 設定 |
| `FuseMainLoop.cc` | 107 | 組裝 fuse 掛載參數、`fuse_session_new/mount/loop_mt`、反向清理堆疊 |
| `FuseMainLoop.h` | 11 | `fuseMainLoop()` 宣告 |
| `FuseClients.h` | 243 | `RcInode`（含 `DynamicAttr` 三版本號）、`InodeWriteBuf`、`FileHandle`、`DirHandle`、`FuseClients` 全域狀態容器 |
| `FuseClients.cc` | 440 | `init()` 16 步啟動序列、`stop()`、`ioRingWorker()` 優先級搶工作、`watch()` semaphore 迴圈、`periodicSyncScan()` 公平輪轉 |
| `FuseOps.h` | 8 | 只導出 `getFuseClientsInstance()` 與 `getFuseOps()` |
| `FuseOps.cc` | 2716 | **全部 `fuse_lowlevel_ops` 實作**；虛擬目錄、inode 映射、錯誤處理、讀寫路徑、sync/close/truncate 語意、12 種 ioctl、readdirplus、xattr；末尾附 `RcInode::beginWrite/finishWrite` 與 `periodicSync` |
| `IoRing.h` | 279 | `IoArgs`/`IoSqe`/`IoCqe` 佈局、`IoRing` 共享記憶體環（含容量計算與 `atomic_ref` 標記）、`IoRingTable`（3 個優先級 semaphore + symlink 發佈） |
| `IoRing.cc` | 285 | `jobsToProc()` 批次切分與 timeout（593cec5 修正處）、`process()` 執行批次 IO 並做亂序完成的 sqeTail 重排 |
| `IovTable.h` | 39 | `IovTable` 介面：addIov / rmIov / lookupIov / statIov / listIovs |
| `IovTable.cc` | 337 | `parseKey()` 解析 symlink 檔名中的 iov/ioring 屬性文法、mmap 共享記憶體、RDMA 註冊、以 uid 做存取控制 |
| `PioV.h` | 59 | `PioV` 介面（parallel IO vector） |
| `PioV.cc` | 275 | `chunkIo()` 檔案 offset → chunk 切分、`executeRead/Write`、`concatIoRes()` 洞偵測與零填 |
| `UserConfig.h` | 64 | `UserConfig` 介面、`systemKeys`/`userKeys` 白名單、`configIid()` inode 編碼 |
| `UserConfig.cc` | 174 | per-uid 設定副本、全域變更時的覆寫重放、兩道護欄、`get-conf`/`set-conf` 虛擬目錄的讀寫 |

### 頂層 `hf3fs_fuse/`（Python 綁定，非 C++ binary 的一部分）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `__init__.py` | 0 | 空 |
| `fuse.py` | 7 | `get_mount_point(p)`：用 realpath 取前三段當掛載點 |
| `io.py` | 139 | `iovec` / `ioring` 對 `hf3fs_py_usrbio` 的薄封裝，`__del__` 時 unlink 掉 `/3fs-virt/iovs/` 下的 symlink |
| `fuse_demo.py` | 30 | USRBIO 使用範例 |

### 相鄰的關鍵邊界檔案

| 檔案 | 與 FUSE 的關係 |
|---|---|
| `src/lib/api/fuse.h` | 12 個私有 ioctl 的 cmd 編碼與參數 struct（FUSE 端與 client 端共用） |
| `src/lib/api/hf3fs_usrbio.h` | USRBIO 公開 C API 宣告，含 `HF3FS_IOR_ALLOW_READ_UNCOMMITTED` / `HF3FS_IOR_FORBID_READ_HOLES` |
| `src/lib/api/UsrbIo.cc` | USRBIO 使用者端實作：**直接 `#include "fuse/IoRing.h"` 並建構 `IoRing`**（`owner=false`），與 FUSE 端共用同一份環操作程式碼 |
| `src/lib/api/UsrbIo.md` | USRBIO API 文件，含 `io_depth` 三態語意的說明 |
| `src/lib/common/Shm.h` | `ShmBuf`（mmap + RDMA 註冊 + 引用計數）與 `ShmBufForIO`（帶 offset 的視圖，檢查不跨 block） |
| `src/common/utils/AtomicSharedPtrTable.h` | `AvailSlots` + `atomic_shared_ptr` 陣列，被 IovTable / IoRingTable / UserConfig / IoRing 四處使用 |
| `src/common/utils/StatusCode.cc` | `toErrno()` 唯一的內部碼 → errno 對照表 |
| `src/fbs/meta/Common.h` | `InodeId` 的虛擬位址空間定義與 `static_assert` 檢查 |
| `src/fbs/meta/Schema.h` | `VersionedLength::mergeHint()`、`File::getChunkId/getChainId` |
| `src/client/meta/MetaClient.h` | 所有元資料操作的 RPC 介面（`sync` 有兩個多載，FUSE 用的是帶 atime/mtime 的那個） |
| `src/client/storage/StorageClient.h` | `createReadIO/createWriteIO/batchRead/batchWrite/removeChunks/registerIOBuffer` |
| `docs/design_notes.md:25-41` | FUSE 效能限制與「在 FUSE daemon 內實作 native client」這個決策的官方說明 |
| `docs/metrics.md:22-26` | `fuse.dirty_inodes` / `fuse.op` / `fuse.piov.bw` / `fuse.write.latency` / `fuse.write.size` 五個指標 |
