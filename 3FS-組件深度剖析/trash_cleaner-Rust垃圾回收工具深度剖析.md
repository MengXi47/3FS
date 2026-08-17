# `trash_cleaner` Rust 垃圾回收工具深度剖析

> 對應原始碼：`src/client/trash_cleaner/`（3 檔）
> 生產者端：`hf3fs_utils/`（Python CLI）
> 核心介面：`src/lib/api/fuse.h` 的 `HF3FS_IOC_REMOVE` ioctl
> 對照組：`src/meta/components/GcManager.h`（meta server 內建 GC）
> 風格基準：[`../3FS-元資料層深度剖析.md`](../3FS-元資料層深度剖析.md)

---

## 0. 一句話總結

`trash_cleaner` 不是 3FS 的垃圾回收器——**真正的垃圾回收器是 meta server 裡的 `GcManager`**。`trash_cleaner` 是一個**保留期到期掃除器**：它掃 `/{mnt}/trash/{user}/` 底下由 Python CLI 建立的、**把到期時間編碼在目錄名字串裡**的目錄，發現過期就對它下一個 `HF3FS_IOC_REMOVE` ioctl，之後真正的空間回收交給 `GcManager`。

它之所以必須是「外部工具」而非 meta server 內建，有一個決定性的理由：**它必須以檔案擁有者的身分執行刪除**。`scan_trash` 對每個使用者的垃圾桶 `seteuid`/`setegid` 成該使用者（`src/client/trash_cleaner/src/main.rs:454-457`），讓 meta server 走**完全一般的權限檢查路徑**。meta server 內部無法做這件事——它沒有一個乾淨的方式「假裝自己是某個 uid」而仍受自身的權限檢查約束。整份程式碼的設計重心不在效率，而在「**證明自己沒有 root 權限、證明目標確實過期、然後才敢刪**」。

---

## 1. 檔案清單

```
src/client/trash_cleaner/
├── Cargo.toml       25 行   ← 依賴宣告
├── README.md        21 行   ← 只有 --help 輸出
└── src/main.rs     705 行   ← 全部邏輯（含 93 行測試）
```

`main.rs` 是唯一的原始碼檔，沒有模組拆分。它在 workspace 中的位置（根目錄 `Cargo.toml:2-10`）：

```toml
[workspace]
members = ["src/client/trash_cleaner", "src/storage/chunk_engine", "src/lib/rs/hf3fs-usrbio-sys"]
default-members = ["src/client/trash_cleaner", "src/storage/chunk_engine"]
```

它是 `default-members` 之一，所以 CMake 的 `cargo_build_all` 目標（`cmake/AddCrate.cmake:9-13`，在根目錄跑 `cargo build`）會連帶建出它。但**它沒有 `add_crate()` 呼叫**——`add_crate` 全 repo 只用在 `chunk_engine` 一處（`src/storage/CMakeLists.txt:1`），因為那個 crate 要用 cxx bridge 連進 C++。`trash_cleaner` 是獨立執行檔，不需要橋接，所以只搭 cargo 的順風車被建出來，產物落在 `target/release/trash_cleaner`。

**它是活的、有測試的。** `tests/fuse/test_trash.sh:95` 在端到端測試中實際執行它：

```bash
sudo ${BINARY}/trash_cleaner --interval 0 --paths ${MOUNT1}/trash/
```

該腳本 `:76-93` 預先鋪設了七種情境的目錄（合法名、非法名、時間戳倒置、已過期、未過期、無權限、root 擁有、他人擁有），`:98-124` 逐一驗證哪些被刪、哪些必須留下。這是一份寫得相當完整的行為規格，本報告多處以它佐證。

---

## 2. 全局：兩層垃圾桶

3FS 有**兩個彼此獨立、語意完全不同**的「垃圾桶」。混淆這兩者是理解這個工具最容易犯的錯。

```
┌────────────────────────────────────────────────────────────────────────┐
│  第一層：使用者可見的「保留期垃圾桶」                                    │
│  路徑：/{mountpoint}/trash/{user_name}/{policy}-{begin}-{expire}/      │
│  可見：ls 得到、可以 cd 進去、可以撈回來                                │
│  進入方式：hf3fs_cli rmtree --expire 3h <path>   （使用者主動、選擇性） │
│  離開方式：(a) hf3fs_cli mv 撈回   (b) 到期後被 trash_cleaner 掃掉       │
│  策略載體：目錄名字串（沒有任何 schema 欄位）                           │
│  執行者：trash_cleaner（本文主角，外部 Rust 進程）                      │
└──────────────────────────────┬─────────────────────────────────────────┘
                               │ HF3FS_IOC_REMOVE (recursive=true)
                               │   → metaClient->remove()
                               ▼
┌────────────────────────────────────────────────────────────────────────┐
│  第二層：內部不可見的「刪除中繼站」                                      │
│  位置：InodeId::gcRoot() 下的 GC-Node-{nodeId}（FDB 裡的一個 dentry 分區）│
│  可見：不在 POSIX 命名空間裡，ls 不到                                   │
│  進入方式：任何 remove（不論來源），O(1) 原子搬移                        │
│  離開方式：GcManager 背景遞迴展開、刪 chunk、刪 inode                    │
│  策略載體：dentry name = "{類型}-{時間戳20位}-{inode hex}"（見元資料層 §9）│
│  執行者：meta_main 進程內的 GcManager                                   │
└────────────────────────────────────────────────────────────────────────┘
```

**兩者是流水線的前後段，不是競爭關係。** `trash_cleaner` 刪掉的東西，下一秒就出現在 `GcManager` 的工作佇列裡。`trash_cleaner` 不碰任何 chunk、不連 storage、不連 FoundationDB；它只是「按時間表按下刪除鍵」的那隻手。

一句話區分：**第一層管「什麼時候該刪」，第二層管「刪了之後怎麼把空間收回來」。**

---

## 3. 為什麼需要一個外部工具

這是本報告最核心的問題。`GcManager` 已經是一個功能完整、支援優先權排程、分散式分片、可熱更新併發度的背景 GC（`src/meta/components/GcManager.h:57-259`，設定見 `src/meta/base/Config.h:22-53`）。為什麼保留期到期這件事不順手做進去？

### 3.1 決定性理由：必須以「使用者身分」執行刪除

`scan_trash` 的核心五行（`main.rs:453-460`）：

```rust
// create user context
let _user_ctx = UserContext::new(
    Uid::from_raw(entry_stat.st_uid),
    Gid::from_raw(entry_stat.st_gid),
);

// open trash
let trash = match Trash::open(entry_stat.st_uid, entry_name, trash_root.join(entry_name)) {
```

`UserContext` 是個 RAII guard（`main.rs:358-382`）：

```rust
impl UserContext {
    pub fn new(uid: Uid, gid: Gid) -> UserContext {
        assert_ne!(uid, Uid::from_raw(0));
        assert_ne!(gid, Gid::from_raw(0));
        let orig_uid = geteuid();
        let orig_gid = getegid();
        setegid(gid).expect("setegid failed, should be root or has CAP_SETUID");
        seteuid(uid).expect("seteuid failed, should be root or has CAP_SETGID");
        UserContext { orig_uid, orig_gid }
    }
}
impl Drop for UserContext {
    fn drop(&mut self) {
        seteuid(self.orig_uid).expect("restore euid failed");
        setegid(self.orig_gid).expect("restore egid failed");
    }
}
```

進程以 root 啟動（`test_trash.sh:95` 用 `sudo`），但**在接觸任何使用者資料前先降權成該垃圾桶的擁有者**，離開作用域時自動還原。這樣一來，後續所有 syscall（`open`、`fstat`、`readdir`、`ioctl`）到達 FUSE 層時，`fuse_req_ctx(req)->uid` 就是那個使用者，meta server 走的是**一般路徑的權限檢查**——沒有任何特殊模式、沒有繞過、沒有信任外部宣稱的 uid。

`test_trash.sh:82-83` 明確測試這個性質：

```bash
# trash cleaner 在删除时使用的是普通用户的 uid/gid, 不应该有 root 权限
sudo mkdir -p ${MOUNT1}/trash/`id -un`/noperm-20240801_1200-20240801_1300/data-{1..20}
```

這個目錄名是合法且已過期的，但裡面的內容屬於 root。`:104` 驗證清掃後它**必須還在**：

```bash
check_exists ${MOUNT1}/trash/`id -un`/noperm-20240801_1200-20240801_1300/data-{1..20}
```

**meta server 內部做不到這件事。** `GcManager` 的刪除是以「系統」身分執行的，它的權限模型是「東西進了 gcRoot 就代表已經通過權限檢查了」。要讓它按到期時間刪 `/trash/` 下的東西，就得在 server 內部合成一個 `UserInfo` 去做檢查——那等於在 meta server 裡開一個「以任意使用者身分操作」的能力，是一個明顯比外部工具更糟的攻擊面。

### 3.2 策略載體是目錄名字串，meta schema 裡沒有位置

到期時間存在哪裡？**存在目錄的名字裡。** 生產端 `hf3fs_utils/trash.py:42-57`：

```python
def current_dir(self) -> str:
    base_timestamp = BASE_TIMESTAMP
    current_timestamp = int(datetime.now(tz=UTC8_TZ).timestamp())
    time_slice_seconds = int(self.time_slice.total_seconds())
    expire_seconds = int(self.expire.total_seconds())
    start_timestamp = ((current_timestamp - base_timestamp) // time_slice_seconds) * time_slice_seconds + base_timestamp
    end_timestamp = start_timestamp + expire_seconds + time_slice_seconds
    ...
    return f"{self.name}-{format_date(start_datetime)}-{format_date(end_datetime)}"
```

產出形如 `1d-20250814_1200-20250815_1300` 的目錄名。消費端 `main.rs:218-252` 純字串解析：

```rust
let parts: Vec<&str> = name.split("-").collect();
if parts.len() != 3 { return Err("invalid name".to_string()); }
match (parse_datetime(parts[1]), parse_datetime(parts[2])) {
    (Some(begin_time), Some(expire_time)) => {
        if begin_time > expire_time { return Err(...); }
        let now = chrono::Utc::now();
        return Ok(now > expire_time);
    }
    _ => return Err("invalid name".to_string()),
}
```

**`meta::Inode` / `meta::DirEntry` 裡沒有任何「到期時間」欄位**（見 `src/fbs/meta/Schema.h`）。要讓 meta server 原生支援保留期，就得在 schema 尾端加欄位（3FS 的 positional serde 只能 append，見元資料層報告 §14）、加索引（否則掃描過期項目是全表掃）、加背景任務。把策略編碼進**名字**則零 schema 改動、零索引、零 server 端狀態——代價是策略只能表達成一個可排序的字串，而且解析邏輯散落在兩個語言的兩份程式碼裡。

這與元資料層 `GcManager` 用 `"{prefix}-{timestamp:020d}-{inode_hex}"` 當 dentry name 是**同一個手法**（元資料層報告 §9），只是一個在使用者可見的命名空間、一個在內部分區。3FS 的一貫風格：**能用 key/name 編碼解決的，就不加欄位。**

### 3.3 這是選擇性的運維政策，不是檔案系統語意

`hf3fs_utils/README.md:37` 講得很清楚：

> If you want to use `rmtree` command, the administrator needs to create a trash directory for each user at `/{3fs_mountpoint}/trash/{user_name}`. The cleanup of the trash directory is handled by the `trash_cleaner`.

沒有 `/trash/{user}` 目錄，`rmtree` 就不能用（`test_trash.sh:68-71` 專門測這個）。整套機制**預設是關的**，由管理員逐個使用者開通。保留策略（`1h`/`3h`/`8h`/`1d`/`3d`/`7d`，`trash.py:60-67`）、掃描頻率（`--interval`）、要看管哪些掛載點（`--paths`）全部是部署決策。把部署決策做進 meta server 意味著要為它們加設定項、加熱更新、加監控。

### 3.4 meta server 唯一的讓步：把 `/trash` 這個路徑寫死

雖然邏輯在外部，meta server 還是為這套機制開了一個特例。`src/meta/store/ops/Rename.cc:58-61`：

```cpp
static bool underTrash(const std::vector<Inode> &ancestors) {
  return ancestors.size() >= 2 && ancestors[ancestors.size() - 1].id == InodeId::root() &&
         ancestors[ancestors.size() - 2].asDirectory().name == "trash";
}
```

**根目錄下那個叫 `trash` 的目錄，在 meta server 裡是硬編碼的特殊區域。** `Rename` 操作對「目的地在 trash 底下」的情況有專屬處理（`Rename.cc:105-136`）：

```cpp
if (underTrash(dstAncestors)) {
  XLOGF_IF(FATAL, !srcResult.dirEntry->isDirectory(), "{} not directory", *srcResult.dirEntry);
  auto srcAncestors = std::vector<Inode>();
  CO_RETURN_ON_ERROR(co_await Inode::loadAncestors(txn, srcAncestors, srcResult.getParentId()));

  if (req_.moveToTrash || config().allow_directly_move_to_trash()) {
    auto acl = srcResult.dirEntry->dirAcl;
    // try to move a directory into trash directory, should be owner and have rwx permission
    CO_RETURN_ON_ERROR(acl->checkRecursiveRmPerm(req_.user, config().recursive_remove_check_owner()));
    auto recursiveCheck = config().recursive_remove_perm_check();
    if (recursiveCheck) {
      auto res = co_await DirEntryList::recursiveCheckRmPerm(txn, srcResult.dirEntry->id, req_.user, recursiveCheck, 128);
      CO_RETURN_ON_ERROR(res);
    }
  } else if (req_.user.uid != flat::Uid(0)) {
    // src should already in trash
    if (!underTrash(srcAncestors)) {
      co_return makeError(MetaCode::kNoPermission, "try to move into trash directory without moveToTrash");
    }
  }
  origPath = Path(srcResult.dirEntry->name);
  for (auto &ancestor : srcAncestors) { origPath = ancestor.asDirectory().name / *origPath; }
}
```

四個要點：

1. **只有目錄能進垃圾桶**（`XLOGF_IF(FATAL, !isDirectory)`）。單一檔案沒有保留期機制。
2. **一般 `mv` 不能把東西搬進垃圾桶**（`else if` 分支）。必須帶 `moveToTrash=true` 這個 RPC 旗標——而該旗標只有 `HF3FS_IOC_MOVE` ioctl 能設（`src/lib/api/fuse.h:35`、`src/fuse/FuseOps.cc:2118`），普通 `rename(2)` 走不到。`test_trash.sh:157-162` 驗證這條：`mv` 進垃圾桶必失敗，垃圾桶**內部**互相 `mv` 則允許（因為那時 `underTrash(srcAncestors)` 為真）。
3. **帶 `moveToTrash` 時要做遞迴權限檢查**，`recursiveCheckRmPerm(..., 128)` 是 best-effort（只查前 128 項，見元資料層報告 §9）。
4. **記錄 `origPath`**：逐層拼接祖先目錄名，讓稽核日誌知道這東西原本在哪。

`allow_directly_move_to_trash` 預設 `false`（`src/meta/base/Config.h:113`），所以生產環境走的是嚴格路徑。

小結：meta server 承認 `/trash` 的存在並保護它（防止東西被誤搬進去、確保搬進去的人有權限刪），但**不管理它的生命週期**。生命週期管理外包給 `trash_cleaner`。

---

## 4. 完整資料流

```
使用者                     hf3fs_cli (Python)              FUSE            meta server
  │                             │                            │                 │
  │ rmtree --expire 3h /a/b     │                            │                 │
  ├────────────────────────────▶│                            │                 │
  │                             │ 1. 檢查 /mnt/trash/{user} 存在  trash.py:86-93│
  │                             │ 2. 檢查是目錄、euid == owner   trash.py:97-112│
  │                             │ 3. 檢查來源是自己的、rwx      trash.py:134-138│
  │                             │ 4. mkdir 3h-20250814_1200-20250814_1530      │
  │                             │                          trash.py:140-144    │
  │                             │ 5. ioctl HF3FS_IOC_MOVE (moveToTrash=true)   │
  │                             ├───────────────────────────▶│  fs.py:177-193  │
  │                             │                            ├────────────────▶│
  │                             │                            │  rename(..., moveToTrash)
  │                             │                            │  Rename.cc:105-136
  │                             │ 6. EEXIST → 名字加時間戳重試（最多 10 次）    │
  │                             │                          trash.py:151-171    │
  ▼                             ▼                            ▼                 ▼

              ...… 保留期內，使用者可 hf3fs_cli mv 撈回 …...

trash_cleaner (Rust, 每 --interval 秒)         FUSE               meta server     GcManager
  │                                              │                    │              │
  │ scan_trash("/mnt/trash")           main.rs:384│                    │              │
  │  ├ fstat 根目錄，uid/gid 必須是 0，否則 abort  :398-404             │              │
  │  ├ for each 使用者子目錄:                     │                    │              │
  │  │   ├ 必須是目錄                    :442-447 │                    │              │
  │  │   ├ 不得為 root 擁有（否則跳過）   :448-451 │                    │              │
  │  │   ├ UserContext::new(uid, gid) ← seteuid  :453-457              │              │
  │  │   ├ Trash::open()                         │                    │              │
  │  │   │   ├ assert geteuid() != 0 且 == user  :72-74                │              │
  │  │   │   ├ open(O_RDONLY|O_DIRECTORY)        :77-89 │              │              │
  │  │   │   ├ fstat 驗 uid/gid 相符      :92-103 │      │              │              │
  │  │   │   ├ ioctl GET_MAGIC == 0x8f3f5fff     ├──────▶│ FuseOps.cc:2000-2008       │
  │  │   │   │       不符 → abort()      :107-119│      │              │              │
  │  │   │   └ ioctl GET_VERSION（存在性檢查）    ├──────▶│ FuseOps.cc:2010-2019       │
  │  │   │                               :122-127│      │              │              │
  │  │   └ Trash::clean(false)                   │      │              │              │
  │  │       ├ dup(fd) → 第二個 Dir 供迭代 :140-147      │              │              │
  │  │       └ for each 項目:                    │      │              │              │
  │  │           ├ check_item()          :218-252│      │              │              │
  │  │           │   必須：目錄 / utf8 / 3 段 /   │      │              │              │
  │  │           │   兩個時間戳可解析 / begin<=expire     │              │              │
  │  │           │   → Ok(now > expire)          │      │              │              │
  │  │           └ 過期 → remove_item()   :254-355│      │              │              │
  │  │               ├ 再跑一次 check_item，不過期就 abort :255-275     │              │
  │  │               ├ 列出子項目寫進 event 稽核日誌 :285-325           │              │
  │  │               └ ioctl HF3FS_IOC_REMOVE    ├──────▶│              │              │
  │  │                  {parent: 垃圾桶 ino,     │       │ FuseOps.cc:2140-2166       │
  │  │                   name, recursive: true}  │       ├─────────────▶│              │
  │  │                                   :327-341│       │ metaClient->remove(         │
  │  │                                           │       │   user, parent, name, true) │
  │  │                                           │       │              ├─ 搬進 gcRoot │
  │  │                                           │       │              │  (O(1) 原子) │
  │  │                                           │       │              ├─────────────▶│
  │  │                                           │       │              │  背景遞迴展開 │
  │  │                                           │       │              │  刪 chunk    │
  │  └ UserContext drop → seteuid 還原 root :377-382     │              │  刪 inode    │
  ▼                                              ▼       ▼              ▼              ▼
```

---

## 5. 安全性保證：十一道關卡

這個工具會遞迴刪除使用者資料，所以它的程式碼有**超過一半是在做檢查**。逐一盤點：

### 5.1 路徑必須含 "trash"（字面字串比對）

`main.rs:678-684`：

```rust
for path in &opt.paths {
    let path = path.to_str().unwrap();
    if !path.contains("trash") {
        error!("trash path {} doesn't contains trash", path);
        std::process::abort();
    }
}
```

粗暴但有效：防止手滑打成 `--paths /mnt/data`。注意是 `abort()` 而非 `exit(1)`——連清理都不做，直接產 core dump。

### 5.2 垃圾桶根目錄必須是 root 擁有

`main.rs:397-404`：

```rust
let dir_stat = nix::sys::stat::fstat(dir_fd)?;
if dir_stat.st_uid != 0 || dir_stat.st_gid != 0 {
    error!("trash root {:?} is not owned by root user, {}:{}", trash_root, dir_stat.st_uid, dir_stat.st_gid);
    abort();
}
```

保證 `/mnt/trash` 這一層只有管理員能改動——普通使用者不能建立新的使用者垃圾桶、不能改名、不能塞進來一個指向別處的東西。

### 5.3 使用者垃圾桶不得是 root 擁有

`main.rs:448-451`：

```rust
if entry_stat.st_uid == 0 || entry_stat.st_gid == 0 {
    error!("trash directory {:?} owned by root user", entry.file_name());
    continue;
}
```

與 5.2 恰好相反的方向。理由是 `UserContext::new` 會 `assert_ne!(uid, 0)`——工具**拒絕以 root 身分刪任何東西**。root 擁有的垃圾桶只能被跳過。`test_trash.sh:86-87, 117-118` 驗證：`/trash/user-root` 及其內容在清掃後必須完好無損。

### 5.4 降權後再斷言一次

`Trash::open` 開頭（`main.rs:71-74`）：

```rust
pub fn open<P: AsRef<Path>>(user: libc::uid_t, user_name: &str, path: P) -> nix::Result<Trash> {
    const ROOT: Uid = Uid::from_raw(0);
    assert_ne!(geteuid(), ROOT);
    assert_eq!(geteuid(), Uid::from_raw(user));
```

呼叫端已經建了 `UserContext`，這裡再斷言一次。`remove_item` 在真正下 ioctl 前**第三次**斷言（`main.rs:276-278`）。三次重複檢查同一件事，而且用 `assert!`（在 release build 也會保留，Rust 的 `assert!` 不受 `debug_assertions` 影響）——這是刻意的：降權失敗而繼續執行的後果是以 root 身分刪別人的資料。

### 5.5 目錄擁有者必須與宣稱的一致

`main.rs:92-103`：`fstat` 已開啟的 fd（而非 `stat` 路徑，避免 TOCTOU），驗證 `st_uid` 與 `st_gid` 都等於預期使用者，否則回 `EACCES`。

### 5.6 必須真的是 3FS

`main.rs:105-119`：

```rust
let mut magic_number: u32 = 0;
unsafe {
    ioctl_get_magic(trash_dir.as_raw_fd(), &mut magic_number)
        .expect(&format!("trash directory {:?} not on 3fs", path.as_ref()));
}
if magic_number != HF3FS_MAGIC_NUM {
    error!("trash directory {:?} magic number {:x} != {:x}, not 3fs", ...);
    std::process::abort();
}
```

`HF3FS_MAGIC_NUM = 0x8f3f5fff`（`main.rs:15`，對應 `src/lib/api/hf3fs_usrbio.h:11` 的 `HF3FS_SUPER_MAGIC`）。用 ioctl 而非 `statfs` 取 magic，因為 FUSE 掛載的 `f_type` 是通用的 `FUSE_SUPER_MAGIC`，分不出是哪個 FUSE 檔案系統。

這道關卡的意義：如果 3FS 掛載掉了，掛載點會退化成本機根檔案系統上的一個空目錄。此時若沒有這道檢查，工具就會去刪本機磁碟上的東西。ioctl 失敗（本機 ext4 不認得 `_IOR('h', 2, u32)`）→ `expect` panic；ioctl 成功但 magic 不符 → `abort`。兩條路都不會誤刪。

### 5.7 ioctl 版本檢查（存在性檢查）

`main.rs:121-127`：

```rust
let mut version: u32 = 0;
unsafe {
    ioctl_get_version(trash_dir.as_raw_fd(), &mut version)
        .expect("get ioctl version failed, should update fuse version");
}
```

讀回來的 `version` **從未被使用**。這不是遺漏——`src/fuse/FuseOps.cc:2011` 的註解說明了設計意圖：

```cpp
// add a get version ioctl, application can check ioctl cmd is supported or not
```

這個 ioctl 存在的唯一目的就是**讓客戶端能偵測 FUSE 端是否夠新**。`trash_cleaner` 正是這樣用的：只在乎 `expect` 會不會 panic，不在乎值。若 FUSE 端太舊沒有這個 ioctl，工具當場掛掉，而不是繼續執行然後在 `HF3FS_IOC_REMOVE`（號碼更大）上失敗。

### 5.8 目錄名必須完全合法

`check_item`（`main.rs:218-252`）依序檢查：

| 檢查 | 失敗回傳 | 行號 |
|---|---|---|
| 必須是目錄（不是檔案、不是 symlink） | `Err("not directory")` | `:219-221` |
| 名字必須是合法 UTF-8 | `Err("not utf8")` | `:222-225` |
| 以 `-` 切開必須恰好 3 段 | `Err("invalid name")` | `:227-230` |
| 第 2、3 段必須能解析成 `%Y%m%d_%H%M` | `Err("invalid name")` | `:231, :248-250` |
| `begin <= expire` | `Err("invalid timestamp, ...")` | `:232-238` |
| 全部通過 → 回傳 `now > expire` | `Ok(bool)` | `:239-246` |

`test_trash.sh:76-78` 對應測這三種非法情形，`:100-102` 驗證它們清掃後仍在。

注意「恰好 3 段」意味著**策略名與時間戳都不能含 `-`**。生產端 `trash.py:37` 用斷言強制：`assert self.name and "-" not in self.name`。這是跨語言的隱性契約，靠兩邊各自的檢查維持。

### 5.9 刪除前再檢查一次，不過期就 abort

`remove_item` 的前 21 行（`main.rs:254-275`）：

```rust
fn remove_item(&self, entry: &nix::dir::Entry) -> nix::Result<()> {
    match Self::check_item(entry) {
        Ok(true) => (),
        Ok(false) => {
            error!("trash directory {:?} try clean unexpired item {:?}", self.path, entry.file_name());
            std::process::abort();
        }
        Err(msg) => {
            error!("trash directory {:?} try clean unknown item {:?}, {}", ...);
            std::process::abort();
        }
    }
    assert_eq!(Self::check_item(entry), Ok(true));
```

呼叫端 `clean_if_expired`（`:175-216`）已經確認過期才會呼叫 `remove_item`，這裡**重跑一次並在不符時 `abort()`**，接著再用 `assert_eq!` 跑**第三次**。三次呼叫同一個純函式，成本可忽略（純字串解析），換來的是：任何導致「未過期項目被送進刪除路徑」的重構失誤，都會在測試時立刻炸掉而不是靜默刪資料。

這是全檔最能代表其設計哲學的一段：**寧可 crash，絕不誤刪。**

### 5.10 「清理未知項目」功能被刻意封死

`Trash::clean` 第一行（`main.rs:137-138`）：

```rust
pub fn clean(self, clean_unknown: bool) -> nix::Result<usize> {
    assert!(!clean_unknown);
```

參數存在，但傳 `true` 立刻 panic。唯一呼叫點傳 `false`（`main.rs:472`）。對應的實作缺口標在 `main.rs:212`：

```rust
// todo!(support clean unknown)
return false;
```

所以名字不合規的目錄（如 `test_trash.sh:76` 的 `invalid-name`）會**永遠留著**，只留一行 `warn!` 日誌。這是安全的預設：無法判定的東西不動它，留給人處理。同時這也是個運維陷阱——`/trash/{user}` 底下若被塞進亂七八糟的名字，會無限期佔用空間，沒有任何自動機制清理。

### 5.11 刪除用 (parent inode, name) 而非路徑

`main.rs:327-341`：

```rust
let mut remove_arg = Hf3fsIoctlRemove {
    parent: self.dir_stat.st_ino,
    name: [0 as libc::c_char; NAME_MAX + 1],
    recursive: true,
};
let ret = unsafe {
    let file_name = entry.file_name();
    assert!(file_name.len() <= NAME_MAX);
    std::ptr::copy(file_name.as_ptr(), remove_arg.name.as_mut_ptr(), file_name.len());
    hf3fs_ioctl_remove(self.dir.as_raw_fd(), &mut remove_arg)
};
```

傳的是**父目錄的 inode 號 + 單一名字**，不是路徑字串。FUSE 端三重驗證（`src/fuse/FuseOps.cc:2140-2157`）：

```cpp
if (in_bufsz != sizeof(hf3fs::lib::fuse::Hf3fsIoctlRemove)) { fuse_reply_err(req, EINVAL); return; }
auto remove = (const hf3fs::lib::fuse::Hf3fsIoctlRemove *)(in_buf);
auto parent = real_ino(remove->parent);
auto name = Path(getCString(remove->name, NAME_MAX));
if (parent != ino) { fuse_reply_err(req, EINVAL); return; }        // ← 關鍵
if (name.has_parent_path()) { fuse_reply_err(req, EINVAL); return; }
auto res = withRequestInfo(req, d.metaClient->remove(userInfo, parent, name, recursive));
```

`parent != ino` 這一行是核心：`ino` 是**被 ioctl 的那個 fd 所指向的 inode**。也就是說，你只能刪除「你手上這個已開啟的目錄 fd」底下的東西，不能指定任意 inode。搭配 `name.has_parent_path()` 檢查（禁止名字裡有 `/`），路徑穿越攻擊完全不可能。

而 `in_bufsz != sizeof(...)` 這道檢查順帶解決了 ABI 對齊問題——見下節。

---

## 6. Rust 怎麼呼叫 3FS：純 ioctl，沒有 USRBIO、沒有 FDB

這是個值得強調的否定結論。`trash_cleaner` **不使用** 3FS 的任何專用客戶端：

| 途徑 | 是否使用 | 佐證 |
|---|---|---|
| USRBIO（`hf3fs-usrbio-sys`） | **否** | `Cargo.toml:11-21` 依賴清單裡沒有 |
| FoundationDB 直連 | **否** | 沒有 fdb crate |
| serde RPC（連 meta/mgmtd） | **否** | 沒有網路相關依賴 |
| cxx bridge 進 C++ | **否** | 沒有 `add_crate()`，無 `build.rs` |
| **FUSE 掛載點 + POSIX syscall + ioctl** | **是** | 全部依賴都是 `nix` / `libc` |

它看到的 3FS 就是一個**普通的 POSIX 目錄樹**：`open`、`fstat`、`fstatat`、`readdir`、`dup`、`seteuid`。唯一的 3FS 專屬介面是三個 ioctl，用 `nix` 的巨集手工宣告（`main.rs:16-30`）：

```rust
const HF3FS_MAGIC_NUM: u32 = 0x8f3f5fff;
ioctl_read!(ioctl_get_magic, b'h', 2, u32);
ioctl_read!(ioctl_get_version, b'h', 3, u32);

const NAME_MAX: usize = 255;

#[repr(C)]
struct Hf3fsIoctlRemove {
    parent: u64,
    name: [libc::c_char; NAME_MAX + 1],
    recursive: bool,
}

ioctl_write_ptr!(hf3fs_ioctl_remove, b'h', 15, Hf3fsIoctlRemove);
```

對照 C++ 端 `src/lib/api/fuse.h:38-42, 45-55`：

```cpp
struct Hf3fsIoctlRemove {
  uint64_t parent;
  char name[NAME_MAX + 1];
  bool recursive;
};
enum {
  HF3FS_IOC_GET_MAGIC_NUM     = _IOR(HF3FS_IOCTYPE_ID, 2, uint32_t),
  HF3FS_IOC_GET_IOCTL_VERSION = _IOR(HF3FS_IOCTYPE_ID, 3, uint32_t),
  HF3FS_IOC_REMOVE            = _IOW(HF3FS_IOCTYPE_ID, 15, Hf3fsIoctlRemove),
};
```

**這份 ABI 是手工複製的，沒有 bindgen、沒有共用標頭。** 兩邊各自宣告 `NAME_MAX`（Rust 寫死 255，C++ 從 `<linux/limits.h>` 取），各自宣告 struct 佈局。這在原則上很脆弱。

不過 3FS 用了一個簡單的方式讓它不脆弱：**ioctl 命令號本身把型別大小編碼進去了**。`_IOW(type, nr, size)` 的 32 位元命令號包含 `sizeof(Hf3fsIoctlRemove)`（14 bits）。Rust 的 `ioctl_write_ptr!` 用同樣的公式從 Rust 端的 `size_of::<Hf3fsIoctlRemove>()` 算出命令號。**如果兩邊的 struct 大小不一致，命令號就不一致**，FUSE 端的 `switch (cmd)` 會掉進 default 而不是誤解記憶體。再加上 `FuseOps.cc:2141` 的 `in_bufsz != sizeof(...)` 二次確認。

也就是說：ABI 不一致的後果是**功能壞掉**（收到 EINVAL 或 ENOTTY），而不是**記憶體錯亂**。對一個刪資料的工具來說，這個失效模式是可接受的。

**選擇 ioctl 而非 RPC 的三個好處**：

1. 權限檢查免費。FUSE 把 `fuse_req_ctx(req)->uid/gid` 自動帶給 meta client，`trash_cleaner` 只要 `seteuid` 就能控制身分——不需要自己拿 token、不需要自己組 `UserInfo`（對照 `admin_cli` 得處理 token）。
2. 不需要 routing info、不需要連 mgmtd、不需要維護 client session。FUSE 進程已經在做這些。
3. `recursive: true` 一次呼叫就完成整棵子樹的刪除，`trash_cleaner` 不用自己遞迴。遞迴展開由 meta 的 `GcManager` 在背景做（元資料層報告 §9），這正是「兩層垃圾桶」分工的體現。

**代價**：`trash_cleaner` 只能在有 3FS FUSE 掛載的機器上跑，而且只能用同步阻塞式單執行緒（無 io_uring、無協程）。對一個每小時跑一次的清掃工具，這完全不是問題。

---

## 7. 稽核日誌：雙軌 tracing

`main.rs:630-676` 建了三個 tracing layer，用 `target` 欄位做路由：

```
                      tracing::registry()
                             │
        ┌────────────────────┼────────────────────┐
        ▼                    ▼                    ▼
  ┌──────────┐       ┌──────────────┐     ┌──────────────┐
  │  log     │       │   stdout     │     │   event      │
  │ 每日輪替  │       │  彩色, ANSI   │     │ 每日輪替, JSON│
  │ log/*.log│       │              │     │ event/*.log  │
  │ filter:  │       │ filter:      │     │ filter:      │
  │ target   │       │ target       │     │ target       │
  │ != event │       │ != event     │     │ == event     │
  │ level:   │       │ level:       │     │ level:       │
  │ --log-   │       │ --stdout-    │     │ --log-level  │
  │  level   │       │  level(warn) │     │              │
  └──────────┘       └──────────────┘     └──────────────┘
```

`target: "event"` 的兩筆記錄是**結構化稽核軌跡**，而非人類可讀日誌：

**刪除前先記錄將被刪掉的東西**（`main.rs:284-325`）：

```rust
// record trash item entries before remove
let sub_entries = Dir::openat(Some(self.dir.as_raw_fd()), entry.file_name(), ...)
    .and_then(|mut subdir| { ... collect DirEntry { ino, ftype, name } ... });

info!(
    target: "event",
    user = self.user.as_raw(),
    user_name = self.user_name,
    op = "list",
    item = format!("{:?}", entry.file_name()),
    sub_entries = serde_json::to_string(&sub_entries).unwrap_or("null".to_string()),
    errno = 0
);
```

**刪除後記錄結果**（`main.rs:342-352`）：

```rust
info!(target: "event", user = ..., user_name = ..., op = "remove",
      item = ..., errno = match ret { Ok(_) => 0, Err(errno) => errno as i32 });
```

三個設計點：

1. **「先記錄再刪」**。若進程在刪除中途被 kill，日誌裡至少留下了「當時這個目錄底下有什麼」的快照。反過來的順序（刪完才記錄）會在崩潰時丟失所有線索。
2. **只記錄一層子項目**（`Dir::openat` + 單層迭代，不遞迴）。這是成本與資訊量的權衡：完整遞迴列舉一個大目錄可能有百萬項，代價太高。一層就足以回答「這次刪掉的大概是什麼」。
3. **列舉失敗不阻止刪除**（`main.rs:314-324` 的 `Err` 分支只是把 `errno` 記進日誌，`sub_entries` 留空，然後繼續往下走）。稽核是盡力而為，不是刪除的前提。

`DirEntry` 這個 struct（`main.rs:41-60`）只為了 `serde::Serialize` 而存在，且對非 UTF-8 檔名有 fallback（`format!("non-utf8-{:?}", ...)`），確保 JSON 序列化永遠不會失敗。

---

## 8. 命令列與執行模型

`Opt`（`main.rs:593-628`，`structopt`）：

| 參數 | 預設 | 用途 |
|---|---|---|
| `-p, --paths <PathBuf>...` | 無（可多個） | 垃圾桶根目錄，每個都必須含字串 "trash" |
| `-i, --interval <u64>` | **必填** | 掃描間隔（秒）；**0 = 掃一次就結束** |
| `--abort-on-error` | false | 任一路徑掃描失敗就 `abort()` |
| `--log <PathBuf>` | `./` | 日誌根目錄，底下再分 `log/` 與 `event/` |
| `--log-level <Level>` | `info` | 檔案日誌等級 |
| `--stdout-level <Level>` | `warn` | 終端日誌等級 |

主迴圈（`main.rs:686-703`）：

```rust
loop {
    for path in &opt.paths {
        info!("scan trash {:?}", path);
        match scan_trash(&path) {
            Ok(_) => info!("scan trash {:?} success", path),
            Err(errno) => {
                error!("scan trash {:?} failed, {}", path, errno);
                if opt.abort_on_error { std::process::abort(); }
            }
        }
    }
    if opt.interval == 0 { break; }
    std::thread::sleep(Duration::from_secs(opt.interval));
}
```

**單執行緒、同步、無並行**。沒有 tokio、沒有 rayon。每次掃描把所有使用者、所有項目串行走一遍。這是刻意的：

- `seteuid`/`setegid` 是**進程層級**的狀態（Linux 上 glibc 的 `seteuid` 會透過 signal 同步到所有執行緒）。多執行緒同時切換 euid 會互相踩踏——`UserContext` 這個 RAII 模式在多執行緒下根本不成立。**單執行緒是降權模型的必要條件，不是效能上的疏忽。**
- 刪除的實際成本在 meta server 與 `GcManager`（見 `src/meta/base/Config.h:28-32` 的 `gc_file_concurrent=32` / `gc_directory_concurrent=4`）。`trash_cleaner` 這端只是發起請求，並行化沒有意義，反而會對 meta server 造成不必要的突發負載。

`--interval 0` 的單次模式讓它能被 cron 或 systemd timer 驅動，而不必自己當常駐服務。`test_trash.sh:95` 用的就是這個模式。

---

## 9. 時區：寫死 UTC+8

兩端都把時區硬編碼成北京時間。Rust 端（`main.rs:32-39`）：

```rust
fn parse_datetime(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if let Ok(native_datetime) = chrono::NaiveDateTime::parse_from_str(s, "%Y%m%d_%H%M") {
        let utc8 = chrono::FixedOffset::east_opt(8 * 60 * 60).unwrap();
        Some(native_datetime.and_local_timezone(utc8).unwrap())
    } else { None }
}
```

Python 端（`hf3fs_utils/trash.py:11-13`）：

```python
UTC8_TZ = timezone(timedelta(hours=8))
DATE_FORMAT = "%Y%m%d_%H%M"
BASE_TIMESTAMP = int(datetime(year=1980, month=1, day=1, tzinfo=UTC8_TZ).timestamp())
```

比較則用 `chrono::Utc::now()`（`main.rs:239`）——這是正確的，`chrono` 的 `DateTime` 比較會正規化到同一瞬間，跨時區不會出錯。

寫死 UTC+8 的**好處**是目錄名對維運人員直接可讀（`ls` 出來就是本地時間），且不受機器 `TZ` 環境變數影響——如果用本地時區，同一個叢集裡設定不同 TZ 的機器會對同一個目錄名算出不同的到期時刻。**代價**是這套工具事實上綁定在中國時區部署，海外部署會看到偏移 8 小時的目錄名（功能仍正確，只是不直觀）。

`BASE_TIMESTAMP = 1980-01-01` 是時間切片對齊的原點，只在 Python 端使用（`trash.py:50-52`）。

### 時間切片：為什麼保留期是「至少」而非「恰好」

`trash.py:50-53`：

```python
start_timestamp = ((current_timestamp - base_timestamp) // time_slice_seconds) * time_slice_seconds + base_timestamp
end_timestamp = start_timestamp + expire_seconds + time_slice_seconds
```

`TRASH_CONFIGS`（`trash.py:60-67`）：

| 策略 | expire | time_slice |
|---|---|---|
| `1h` | 1 小時 | 10 分鐘 |
| `3h` | 3 小時 | 30 分鐘 |
| `8h` | 8 小時 | 30 分鐘 |
| `1d` | 1 天 | 1 小時 |
| `3d` | 3 天 | 1 天 |
| `7d` | 7 天 | 1 天 |

`current_dir()` 把當下時刻**向下對齊到 time_slice 邊界**當作 `begin`。效果是：同一個時間切片內的所有 `rmtree` 呼叫共用**同一個垃圾桶目錄**。

```
time_slice = 1h，expire = 1d

  12:05  rmtree A ─┐
  12:31  rmtree B ─┼─▶ 全部進 1d-{今天}_1200-{明天}_1300/
  12:59  rmtree C ─┘

  13:02  rmtree D ───▶ 進 1d-{今天}_1300-{明天}_1400/
```

好處是垃圾桶下的目錄數量有上界（`expire / time_slice + 1`，最多幾十個），掃描成本恆定，不會因為使用者狂刪東西而膨脹成幾萬個目錄。

`end = begin + expire + time_slice` 這個「多加一個 slice」是關鍵補償：切片內**最早**的那次刪除（12:05）與切片起點（12:00）差 5 分鐘，若 end 只是 `begin + expire`，它實際只被保留 `1d - 5min`。加上一個完整 slice 後，保證**切片內任何一次刪除都至少被保留 `expire` 這麼久**，最多多保留一個 slice。這是把「至少保留 N」這個承諾用最簡單的算術做對。

### 命名衝突處理

`trash.py:151-171`：搬進垃圾桶時若目標名已存在（`ENOTDIR` / `EEXIST` / `ENOTEMPTY`），就把名字截到 200 字元再接上微秒時間戳重試，最多 10 次：

```python
current_trash_name = f"{trash_name[0:200]}.{get_timestamp_us()}"
```

`test_trash.sh:149-155` 驗證：連續三次 `rmtree` 同名目錄後，垃圾桶裡會同時有 `another-dir` 與 `another-dir.{timestamp}`。

截到 200 字元是為了給 `.{16 位微秒}` 留空間，總長不超過 `NAME_MAX`（255）。

---

## 10. 小瑕疵盤點

| 位置 | 問題 | 影響 |
|---|---|---|
| `Cargo.toml:16` | `scopeguard = "0"` 被宣告但 `main.rs` 從未 import | 無害的多餘依賴；推測早期版本用它做降權還原，後來改成 `Drop for UserContext` |
| `main.rs:111` | 已用 `expect` 確保 ioctl 成功，接著又檢查 magic 值並 `abort()` | 兩種失效模式（panic / abort）處理同一類問題，不一致但不影響正確性 |
| `main.rs:110` | `.expect(&format!(...))` 無論成敗都會先 format 字串 | 效能可忽略（每個垃圾桶一次），clippy 會建議 `unwrap_or_else` |
| `main.rs:212` | `// todo!(support clean unknown)` | 非法命名的目錄永遠不會被清理，可能無限期佔用空間，且無告警機制（只有 `warn!` 日誌） |
| `main.rs:227` | `name.split("-")` 要求恰好 3 段 | 策略名與時間戳都不能含 `-`；此契約靠 `trash.py:37` 的斷言單方面維持，沒有共用定義 |
| `main.rs:20` vs `fuse.h` | `NAME_MAX` 在 Rust 端寫死 255，C++ 端取自 `<linux/limits.h>` | 若平台 `NAME_MAX` 不是 255，struct 大小不符 → ioctl 命令號不符 → 功能失效（但不會記憶體錯亂，見 §6） |
| `main.rs:506` | `test_usercontext` 掛 `#[ignore]` | 因為需要 root 才能 `seteuid`；意味著 CI 上降權邏輯無單元測試覆蓋，只靠 `test_trash.sh` 的端到端測試 |
| `main.rs:472` | `clean(false)` 是唯一呼叫點 | `clean_unknown` 參數形同虛設，`assert!(!clean_unknown)` 讓它連傳 `true` 都不行 |

這些都是小問題。以「一個會遞迴刪除使用者資料的工具」的標準看，這份程式碼的謹慎程度是高於平均的。

---

## 11. 與 `GcManager` 的分工對照

| 面向 | `trash_cleaner` | `GcManager` |
|---|---|---|
| 位置 | 獨立 Rust 進程 | `meta_main` 進程內的組件 |
| 語言 / 規模 | Rust，705 行（含測試） | C++，`GcManager.h` 259 行 + `.cc` 數百行 |
| 觸發 | 定時輪詢（`--interval`），或單次（cron） | 常駐背景任務，`scan_interval` 預設 200ms（`src/meta/base/Config.h:24`） |
| 掃描對象 | POSIX 目錄樹 `/trash/{user}/` | FDB 裡 `gcRoot()` 下的 dentry 分區 `GC-Node-{nodeId}` |
| 掃描方式 | `readdir` 逐項 | FDB prefix range scan，`scan_batch=4096`（`Config.h:25`） |
| 判定依據 | 目錄名裡的到期時間戳 | dentry name 裡的類型前綴（`d`/`f`/`L`/`S`）+ 時間戳 |
| 並行度 | 1（`seteuid` 的必然限制） | `gc_file_concurrent=32` / `gc_directory_concurrent=4`（`Config.h:28,30`），另有 8 個優先權協程（`Config.h:46-49`） |
| 權限模型 | `seteuid` 成擁有者，走一般權限檢查 | 系統身分；進 gcRoot 前已檢查過 |
| 動作 | 一次 ioctl，`recursive=true` | 遞迴展開目錄、`removeChunks`（`remove_chunks_batch_size=32`）、刪 inode |
| 碰 storage 嗎 | 否 | 是（透過 storage client 刪 chunk） |
| 碰 FDB 嗎 | 否 | 是（所有操作都在 FDB 交易裡） |
| 使用者可見 | 是（可 `ls`、可撈回） | 否 |
| 可取消 | 是（`hf3fs_cli mv` 撈回） | 否（進了就回不來） |
| 是否必要 | 否，預設關閉，管理員逐使用者開通 | 是，關掉就會漏空間 |
| 延遲策略 | `expire` 由使用者選（1h~7d） | `gc_file_delay=5min`，且空閒空間低於 5% 時取消延遲（`Config.h:27,34-35`） |

一句話：**`GcManager` 是檔案系統的一部分，`trash_cleaner` 是部署在上面的一項政策。**

---

## 12. 設計取捨總結

| 決策 | 得到什麼 | 付出什麼 |
|---|---|---|
| 外部工具而非 meta 內建 | 能 `seteuid` 成擁有者，讓 meta 走一般權限檢查路徑，不必在 server 開「代任意使用者操作」的後門 | 多一個要部署、要監控、要排程的進程；掛掉了沒人知道 |
| 到期時間編碼在目錄名 | 零 schema 改動、零索引、meta server 零狀態；`ls` 直接看得到策略 | 解析邏輯散在 Rust 與 Python 兩份程式碼；策略名不能含 `-`；改格式要同時改兩邊 |
| 用 ioctl 而非 RPC | 權限、routing info、session 全部搭 FUSE 的便車 | 只能跑在有掛載點的機器；ABI 靠手工同步 |
| ioctl 命令號含 struct 大小 | ABI 不符時功能壞掉而非記憶體錯亂 | — |
| `recursive=true` 一次搞定 | 客戶端不用遞迴；遞迴成本推給 meta 的 `GcManager` | 單次 ioctl 可能對應巨量後續工作，client 端無進度可見 |
| 單執行緒 | `seteuid` 的 RAII 模型才成立；不對 meta 造成突發負載 | 掃描時間隨使用者數線性成長（實務上每小時跑一次，無所謂） |
| 時間切片對齊 | 垃圾桶目錄數有上界，掃描成本恆定 | 實際保留期比 `--expire` 多最多一個 slice |
| `end = begin + expire + slice` | 保證「至少保留 expire」對切片內每一次刪除都成立 | 平均多佔用約半個 slice 的空間 |
| 時區寫死 UTC+8 | 目錄名可讀、不受機器 TZ 影響 | 事實上綁定中國時區部署 |
| 不合規項目一律跳過 | 無法判定的東西絕不亂動 | 亂命名的目錄永久佔空間，無告警 |
| 三重檢查 + `abort()` | 誤刪的可能性壓到極低；重構失誤會立刻炸掉 | 遇到任何異常直接死掉，沒有降級運行模式 |
| `--interval 0` 單次模式 | 可交給 cron / systemd timer，不必自己當常駐服務 | — |

整體風格：**把所有可以外包的東西外包（權限給 FUSE、遞迴給 meta、排程給 cron、空間回收給 GcManager），自己只留下一件事——判斷「這個目錄到期了嗎」，並且用三道重複檢查確保這個判斷不會出錯。**

---

## 13. 檔案索引

| 檔案 | 行數 | 職責 |
|---|---|---|
| `src/client/trash_cleaner/Cargo.toml` | 25 | 依賴：`nix`（fs/ioctl/dir/user）、`libc`、`chrono`、`structopt`、`tracing` 三件套、`serde`/`serde_json`（稽核日誌）；`scopeguard` 已宣告但未使用；dev-dep 只有 `tempfile` |
| `src/client/trash_cleaner/README.md` | 21 | 只有 `--help` 輸出的複製貼上，無設計說明 |
| `src/client/trash_cleaner/src/main.rs` | 705 | 全部邏輯。`:15-30` ioctl ABI 手工宣告；`:32-39` UTC+8 時間解析；`:41-60` 稽核用 `DirEntry`；`:62-135` `Trash::open`（六道開啟前檢查）；`:137-173` `clean` 主迴圈；`:175-216` `clean_if_expired`；`:218-252` `check_item` 名字解析與到期判定；`:254-355` `remove_item`（三重檢查 + 稽核 + ioctl）；`:358-382` `UserContext` 降權 RAII；`:384-497` `scan_trash` 逐使用者掃描；`:499-591` 單元測試（`test_usercontext` 標 `#[ignore]`）；`:593-628` `structopt` 命令列；`:630-704` tracing 三軌設定與主迴圈 |

### 相關但不屬於本組件的檔案

| 檔案 | 關係 |
|---|---|
| `hf3fs_utils/trash.py` | **生產者端**：`TrashConfig.current_dir()`（`:42-57`）產生 `trash_cleaner` 要解析的目錄名；`Trash.move_to_trash()`（`:114-176`）發 `HF3FS_IOC_MOVE` |
| `hf3fs_utils/cli.py` | `hf3fs_cli rmtree` 子命令（`:98-187`），使用者入口 |
| `hf3fs_utils/fs.py` | `_rename_ioctl()`（`:177-193`）把 `Hf3fsIoctlMove` 打包成 `struct.pack("N256sN256s?")` 下 ioctl |
| `src/lib/api/fuse.h` | ioctl ABI 權威定義：`Hf3fsIoctlRemove`（`:38-42`）、命令號 enum（`:44-56`） |
| `src/fuse/FuseOps.cc` | ioctl 分派：`GET_MAGIC_NUM`（`:2000-2008`）、`GET_IOCTL_VERSION`（`:2010-2019`）、`MOVE`（`:2108-2138`）、`REMOVE`（`:2140-2166`） |
| `src/meta/store/ops/Rename.cc` | `underTrash()`（`:58-61`）把 `/trash` 硬編碼為特殊區；`:105-136` 搬入垃圾桶的權限檢查與 `origPath` 記錄 |
| `src/meta/base/Config.h` | `allow_directly_move_to_trash`（`:113`，預設 false）；`GcConfig`（`:22-53`） |
| `src/meta/components/GcManager.h` | 第二層垃圾桶的執行者，`trash_cleaner` 刪除動作的下游 |
| `tests/fuse/test_trash.sh` | 端到端行為規格：七種目錄情境（`:76-93`）+ 逐項驗證（`:98-124`），是本工具最完整的「文件」 |
