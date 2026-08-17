# `hf3fs_utils` / `hf3fs_cli`（Python 命令列工具）深度剖析

> 對應原始碼：`hf3fs_utils/`（`cli.py` 192 行、`fs.py` 233 行、`trash.py` 175 行、`hf3fs_cli` 5 行、`README.md` 36 行）、打包定義 `setup_hf3fs_utils.py`
> 對側：`src/lib/api/fuse.h`（ioctl 協定）、`src/fuse/FuseOps.cc`（ioctl 處理）、`src/meta/store/ops/Rename.cc` 與 `Remove.cc`（伺服端語意）、`src/meta/components/GcManager.*`（真正的回收）、`src/client/trash_cleaner/`（Rust 清理器）、`src/client/cli/admin/`（C++ `admin_cli`）

---

## 0. 一句話總結

`hf3fs_cli` 只做兩件 POSIX 做不到的事：**跨 mount point 的 rename** 與 **帶過期時間的目錄「移到垃圾桶」**。它不連 meta server、不需要 token、不用 USRBIO，全部靠對已掛載的 FUSE 目錄發 `ioctl(2)`——`struct` 由 Python 的 `struct.pack` 手工打包，ioctl 命令碼是把 `src/lib/api/fuse.h` 的 `_IOW` 巨集**用十六進位常數硬抄一遍**。而它負責的「刪除」其實只是把一棵目錄樹 rename 進 `trash/<user>/<過期時間片>/`，真正的資料回收由另外兩個完全不同語言、不同進程、不同權限模型的組件接手。

---

## 1. 交付物定位

`3FS` 這棵樹裡有**兩個互不相干的 pip 套件**：

```
setup.py                  → 套件名 hf3fs_py_usrbio  （CMake 驅動，建 pybind11 C 擴充）
setup_hf3fs_utils.py      → 套件名 hf3fs_utils      （純 Python，唯一外部相依 click）
```

`setup_hf3fs_utils.py` 全文只有 13 行：

```python
setup(
    name="hf3fs_utils",
    version="1.0.7",
    description="3FS 命令行工具",
    packages=['hf3fs_utils'],
    install_requires=["click"],
    python_requires=">=3.6",
    scripts=["hf3fs_utils/hf3fs_cli"],
)
```

三個值得注意的點：

1. **版本號硬編 `1.0.7`**（`setup_hf3fs_utils.py:5`），而根 `setup.py` 的版本是 `"1.2.9+" + git rev-parse --short HEAD`（`setup.py:12-13`）。兩個套件的版本策略完全不同，也不會互相牽動。
2. **`install_requires` 只有 `click`**——它不依賴 `hf3fs_py_usrbio`，也就是說 `hf3fs_cli` 可以安裝在一台**沒有編譯過 3FS** 的機器上，只要那台機器掛載了 3FS 就能用。這是刻意的：它是給終端使用者的工具，不是給運維的。
3. **`scripts=` 而非 `entry_points=`**：直接把 `hf3fs_utils/hf3fs_cli` 這個檔案裝進 `bin/`。該檔全文 5 行（`hf3fs_utils/hf3fs_cli:1-5`）：

```python
#!/usr/bin/env python
from hf3fs_utils import cli

if __name__ == "__main__":
    cli.cli(max_content_width=120)
```

`README.md:3-5` 給的建置指令是 `python3 setup_hf3fs_utils.py bdist_wheel`。

---

## 2. 三層結構

```
┌───────────────────────────────────────────────────────────────────────┐
│ hf3fs_utils/hf3fs_cli          可執行腳本（5 行），只呼叫 cli.cli()     │
├───────────────────────────────────────────────────────────────────────┤
│ hf3fs_utils/cli.py             click 命令定義層                        │
│   ├ get_filesystem()           掛載點探測（HF3FS_CLI_MOUNTPOINT 或掃路徑）│
│   ├ abs_path()                 路徑正規化（symlink 語意的特殊處理）      │
│   ├ mv        指令              → FileSystem.rename()                  │
│   └ rmtree    指令              → Trash.move_to_trash() / FileSystem.remove()│
├───────────────────────────────────────────────────────────────────────┤
│ hf3fs_utils/trash.py           垃圾桶策略層                            │
│   ├ TrashConfig                過期時間 + 時間片 → 目錄命名             │
│   ├ TRASH_CONFIGS              六種預設（1h/3h/8h/1d/3d/7d）           │
│   └ Trash.move_to_trash()      建時間片目錄 + rename(moveToTrash=True)  │
├───────────────────────────────────────────────────────────────────────┤
│ hf3fs_utils/fs.py              ioctl 封裝層（唯一碰系統呼叫的地方）      │
│   ├ FileSystem.__init__        magic number + ioctl 版本檢查            │
│   ├ opendir/split_path         O_DIRECTORY fd + st_dev/虛擬 inode 檢查  │
│   ├ _rename_ioctl              HF3FS_IOC_MOVE   (0x4218680E)           │
│   ├ _remove_ioctl              HF3FS_IOC_REMOVE (0x4110680F)           │
│   └ _ioctl                     fcntl.ioctl + root 禁令                 │
└───────────────────────────────────────────────────────────────────────┘
                                  │ ioctl(2) on FUSE fd
                                  ▼
                    src/fuse/FuseOps.cc  hf3fs_ioctl()
                                  │ MetaClient RPC
                                  ▼
                    meta server  RenameOp / RemoveOp
```

`fs.py` 匯入的模組只有 `os / fcntl / errno / struct / stat / sys / pathlib`——**沒有任何 3FS 自己的擴充模組**。整個工具與 C++ 側的唯一介面就是 ioctl 的二進位協定。

---

## 3. ioctl 協定：手抄的命令碼與手打的結構

### 3.1 四個命令碼

`hf3fs_utils/fs.py:20-29` 把命令碼寫成十六進位常數：

```python
class FileSystem:
    HF3FS_IOCTL_MAGIC_CMD = 0x80046802
    HF3FS_IOCTL_MAGIC_NUM = 0x8F3F5FFF

    HF3FS_IOCTL_VERSION_CMD = 0x80046803

    HF3FS_IOCTL_RENAME_CMD = 0x4218680E
    HF3FS_IOCTL_RENAME_BUFFER_SIZE = 536

    HF3FS_IOCTL_REMOVE_CMD = 0x4110680F
    HF3FS_IOCTL_REMOVE_BUFFER_SIZE = 272
```

把 Linux 的 `_IOC(dir, type, nr, size) = (dir<<30) | (size<<16) | (type<<8) | nr` 拆開驗證，每一個都能對回 `src/lib/api/fuse.h:44-56`：

| Python 常數 | dir | size | type | nr | 對應 C 列舉 | C 端定義 |
|---|---|---|---|---|---|---|
| `0x80046802` | `_IOR` | 4 | `0x68='h'` | 2 | `HF3FS_IOC_GET_MAGIC_NUM` | `fuse.h:47` |
| `0x80046803` | `_IOR` | 4 | `'h'` | 3 | `HF3FS_IOC_GET_IOCTL_VERSION` | `fuse.h:48` |
| `0x4218680E` | `_IOW` | **536** | `'h'` | 14 | `HF3FS_IOC_MOVE` | `fuse.h:54` |
| `0x4110680F` | `_IOW` | **272** | `'h'` | 15 | `HF3FS_IOC_REMOVE` | `fuse.h:55` |

`0x0218 = 536` 與 `0x0110 = 272` 這兩個大小，正是兩個 C struct 的 `sizeof`：

```cpp
struct Hf3fsIoctlMove {          // fuse.h:30-36
  uint64_t srcParent;            //   8
  char srcName[NAME_MAX + 1];    // 256
  uint64_t dstParent;            //   8
  char dstName[NAME_MAX + 1];    // 256
  bool moveToTrash;              //   1  → 529，對齊到 8 的倍數 = 536
};
struct Hf3fsIoctlRemove {        // fuse.h:38-42
  uint64_t parent;               //   8
  char name[NAME_MAX + 1];       // 256
  bool recursive;                //   1  → 265，對齊到 272
};
```

**這是一組沒有任何自動化機制維護的手抄常數。** C 端只要改動 `Hf3fsIoctlMove` 的任何欄位，`size` 就變，命令碼就變，Python 側會拿到 `EINVAL`（因為 FUSE 端的 `switch` 落到 `default`，`FuseOps.cc:2168-2169`）。反過來若只有欄位順序變而大小不變，就是**靜默的資料錯位**。

作者顯然意識到了這個風險，所以加了一個版本探測（`fs.py:68-76`）：

```python
ioctl_version = -1
try:
    buffer = bytearray(4)
    self._ioctl(virt_fd, FileSystem.HF3FS_IOCTL_VERSION_CMD, buffer)
    ioctl_version = int.from_bytes(buffer, sys.byteorder, signed=False)
except OSError:
    pass
assert ioctl_version >= 1
```

FUSE 端回的是硬編的 `1`（`FuseOps.cc:2016`），註解也說明了用途：「add a get version ioctl, application can check ioctl cmd is supported or not」。目前只有「有/沒有」兩種狀態，還不足以做真正的相容性協商。另外這裡用的是 `assert` 而非 `raise`，**`python -O` 執行時這個檢查會被完全移除**。

### 3.2 struct 打包

`fs.py:190-197` 用 `struct.pack` 手工組出 `Hf3fsIoctlMove`：

```python
buffer = struct.pack(
    "N256sN256s?",
    old_dir_ino,
    self._encode_filename(old_filename),
    new_dir_ino,
    self._encode_filename(new_filename),
    move_to_trash,
).ljust(FileSystem.HF3FS_IOCTL_RENAME_BUFFER_SIZE)
```

`"N"` 是 `size_t`，**只在原生位元組序（預設的 `@` 模式）下可用**，這正好對上 C 的 `uint64_t`（在 LP64 上）。格式字串產生 8+256+8+256+1 = 529 B，再 `ljust` 到 536。

一個小細節：`bytes.ljust(width)` 的預設填充字元是**空格 `0x20`**，不是 `\0`。所以那 7 個 padding byte 是空格。因為 C 端從不讀取 tail padding（`FuseOps.cc:2113` 直接 `reinterpret_cast` 後只取具名欄位），這不會出問題；但如果哪天有人加了欄位放進那 7 個 byte，就會讀到 `0x20202020...`。檔名本身是安全的——`256s` 會自動用 `\0` 補齊，而 C 端用 `getCString(move->srcName, NAME_MAX)` 讀（`FuseOps.cc:2115`）。

`_encode_filename`（`fs.py:85-90`）負責 UTF-8 編碼與 255 byte 上限：

```python
assert name and os.sep not in name, name
name_bytes = name.encode("utf8")
if len(name_bytes) > 255:
    raise OSError(errno.ENAMETOOLONG, ...)
```

「不含 `/`」這個前提在 C 端也再檢查一次（`FuseOps.cc:2123-2126` 的 `srcName.has_parent_path()`），是正確的雙重防護。

### 3.3 ioctl 必須發在「來源父目錄」的 fd 上

C 端有一條看似多餘的檢查（`FuseOps.cc:2119-2122`、`2149-2152`）：

```cpp
auto srcParent = real_ino(move->srcParent);
...
if (srcParent != ino) {          // ino 是 ioctl 所在 fd 的 inode
  fuse_reply_err(req, EINVAL);
  return;
}
```

也就是**結構裡的 `srcParent` 必須等於發起 ioctl 的那個 fd 的 inode**。這把「你能不能操作這個父目錄」從純參數檢查降級成「你手上得先有這個目錄的 fd」，等於借用了核心的路徑解析與 `O_DIRECTORY` 開啟權限檢查。Python 側因此必須 `opendir(dirname)` 拿 fd 再發 ioctl（`fs.py:158-166`、`trash.py:154-161`）。

---

## 4. 掛載點探測與路徑安全

### 4.1 探測

`cli.py:10` 與 `cli.py:13-27`：

```python
MOUNTPOINT = os.environ.get("HF3FS_CLI_MOUNTPOINT", None)

def get_filesystem(path: str) -> FileSystem:
    mountpoint = None
    if MOUNTPOINT is not None:
        mountpoint = os.path.abspath(MOUNTPOINT)
    else:
        path = os.path.realpath(path)
        parts = path.split(os.sep)
        for i in range(1, 4):
            p = os.sep.join(parts[:i])
            if os.path.exists(os.path.join(p, "3fs-virt")):
                mountpoint = p
                break
    if not mountpoint:
        abort(f"{path} is not on 3FS")
    return FileSystem(mountpoint)
```

判準與 USRBIO 的 `hf3fs_extract_mount_point` 一致——**看有沒有 `3fs-virt` 子目錄**，而不是看 `/proc/self/mountinfo` 的檔案系統型別（詳見 USRBIO 報告 §3.2）。但這裡的實作弱得多：`range(1, 4)` 表示只往上找**最多 3 層路徑元素**（`/`、`/a`、`/a/b`），所以掛載在 `/mnt/data/3fs/xxx` 這種第四層以上的位置就找不到，必須靠 `HF3FS_CLI_MOUNTPOINT` 環境變數兜底。

`FileSystem.__init__`（`fs.py:31-79`）拿到掛載點後做四層驗證：存在 → 是目錄 → `3fs-virt` 能以 `O_DIRECTORY` 開啟 → magic number 是 `0x8F3F5FFF` → ioctl 版本 ≥ 1，並記下 `self.st_dev`（`fs.py:54`）供後續的同檔案系統檢查。

### 4.2 `abs_path()`：symlink 的刻意例外

`cli.py:30-38`：

```python
def abs_path(path: str) -> str:
    if ".." in path.split(os.path.sep):
        abort(f"Path {path} contains '..', which is not supported yet")
    normpath = os.path.normpath(path)
    # If the user calls rmtree path/symlink, it should delete the symlink instead of the path it points to
    # For dir paths, take the realpath, but keep the filename as is
    dir = os.path.dirname(normpath)
    filename = os.path.basename(normpath)
    return os.path.join(os.path.realpath(dir), filename)
```

**只對父目錄取 `realpath`，最後一段原樣保留**。註解說得很清楚：`hf3fs_cli rmtree path/symlink` 應該刪掉那條 symlink 本身，而不是它指向的目標。這與 `rm` 的語意一致。

`..` 被直接拒絕，理由寫著 "not supported yet"。這是保守做法：因為 `abs_path` 只對 dirname 做 realpath，若中間含 `..` 又混雜 symlink，`normpath` 的純字串處理會得到與核心解析不同的結果。

### 4.3 三道路徑防護

`FileSystem.opendir`（`fs.py:92-113`）在每次開目錄時檢查：

```python
if dir_st.st_dev != self.st_dev:
    raise RuntimeError(f"{dir_path} is not under the 3FS mount point {self.mountpoint}")
if dir_st.st_ino & 0xF000000000000000:
    raise RuntimeError(f"{dir_path} is a virtual path")
```

- **`st_dev` 檢查**：確保操作對象與 `3fs-virt` 在同一個 superblock。
- **虛擬 inode 檢查**：`ino & 0xF000000000000000` 這個判準與 FUSE 端的 `checkIsVirt`（`FuseOps.cc:262`）完全一致：

  ```cpp
  bool checkIsVirt(InodeId ino) { return (ino.u64() & (0xf000000000000000)) != 0; }
  ```

  擋掉 `3fs-virt` 底下所有保留 inode（iov、set-conf、rm-rf、trash 臨時 id 等，見 USRBIO 報告 §4.4 與 `src/fbs/meta/Common.h:200-233`）。

- **root 禁令**：每一次 ioctl 都會先過 `_check_user`（`fs.py:81-83`、`fs.py:232-234`）：

  ```python
  def _check_user(self):
      if os.geteuid() == 0 or os.getegid() == 0:
          raise RuntimeError(f"root user not allowed")
  ```

  注意是 `euid == 0 **or** egid == 0`，連 gid 是 root 都拒絕。為什麼要防 root？因為 meta 伺服端對 root **有豁免**：`Rename.cc:125` 的 `else if (req_.user.uid != flat::Uid(0))` 表示 uid 0 可以跳過「來源必須已經在垃圾桶裡」的檢查，`Rename.cc:176` 的 `!req_.user.isRoot()` 也讓 root 繞過 sticky bit 檢查。也就是說 root 用這個工具做遞迴刪除時，伺服端的保護網會全部消失。客戶端直接拒絕 root 是最省事的縱深防禦。

---

## 5. `mv`：為什麼需要一個 ioctl 來做 rename

### 5.1 命令

```
hf3fs_cli mv OLD_PATH NEW_PATH
```

`cli.py:56-80` 的處理流程：`abs_path` 兩邊 → 若 `NEW_PATH` 已存在且是目錄，改成 `NEW_PATH/basename(OLD_PATH)`（模仿 `mv` 的語意，`cli.py:64-72`）→ `FileSystem.rename()`。

`README.md:31` 對它的說明是：「Move files, supports moving files between different mount points within the same 3FS」。

### 5.2 核心理由：核心的 `renameat` 拒絕跨 mount

Linux 的 `renameat2` 在 VFS 層會檢查兩個路徑的 **mount** 是否相同，不同就回 `EXDEV`——即使兩個 mount 指向**同一個 superblock**（例如同一個 3FS 實例的兩個子目錄各自掛載，或 bind mount）也一樣。3FS 明確支援「把子目錄分開掛載」這種部署（`src/lib/api/UsrbIo.cc:63-67` 的註解就是在講這件事），於是使用者會遇到「明明是同一個檔案系統，`mv` 卻說 Invalid cross-device link」。

`HF3FS_IOC_MOVE` 繞開的正是這個 VFS 檢查：它把 `(srcParent inode, srcName, dstParent inode, dstName)` 四元組直接送到 meta server，由 meta 在 FoundationDB 的一個交易裡完成（`FuseOps.cc:2127-2128` → `MetaClient::rename` → `RenameOp`）。inode id 在整個 3FS 叢集內是全域唯一的，所以「跨 mount」在 meta 眼中根本不存在。

不過 `fs.py:105` 的 `st_dev` 檢查同時限定了**兩邊必須是同一個 superblock**——所以能跨的是「同一個 FUSE 實例的不同掛載點」，不是「兩個不同的 3FS 叢集」。

### 5.3 `rename()` 的前置檢查

`fs.py:128-175` 在發 ioctl 之前做了四件 C 端不做的事：

```python
def rename(self, old_path: str, new_path: str) -> None:
    self._check_user()
    if is_relative_to(os.path.realpath(new_path), os.path.join(self.mountpoint, "trash")):
        raise RuntimeError(f"{new_path} is in the trash")     # (1) 不准直接搬進垃圾桶
    ...
    old_st = os.stat(old_filename, dir_fd=old_dir_fd, follow_symlinks=False)
    if stat.S_ISLNK(old_st.st_mode):
        raise RuntimeError(f"{old_path} is symlink")          # (2) 不搬 symlink
    try:
        os.stat(new_filename, dir_fd=new_dir_fd, follow_symlinks=False)
        raise FileExistsError(errno.EEXIST, ...)              # (3) 目標不得已存在
    except FileNotFoundError:
        pass
    ...
    self._rename_ioctl(..., False)                            # (4) moveToTrash=False
```

(1) 這一條與伺服端呼應：`Rename.cc:105-130` 的 `underTrash(dstAncestors)` 分支規定，把東西移進 trash 目錄**必須**帶 `moveToTrash=True` 旗標（否則除非 `allow_directly_move_to_trash` 設定為真，或呼叫者是 root，或來源本來就在垃圾桶裡）。客戶端提前擋下，錯誤訊息比 `kNoPermission` 好懂。

(3) 值得注意：這是一個 **TOCTOU 的檢查**——`os.stat` 到 ioctl 之間目標可能被建立。真正的原子性由 meta 側的交易保證（`Rename.cc:192-204` 的 `removeDst`），Python 這一層只是提前給出好的錯誤訊息。POSIX 的 `rename(2)` 語意是「靜默覆蓋目標」，這裡改成拒絕，是刻意收緊。

`_rename_ioctl` 是 `mv` 與 `rmtree` **唯一共用**的底層原語——差別只在最後那個 `move_to_trash` 布林值。

---

## 6. `rmtree`：把「刪除」變成「改名」

### 6.1 命令

```
hf3fs_cli rmtree [DIR_PATHS]... --expire [1h|3h|8h|1d|3d|7d] [-y]
```

`cli.py:104-188`。它有**兩個互斥模式**，由「第一個路徑是否已經在垃圾桶裡」決定（`cli.py:134`）：

```python
clean_trash = is_relative_to(first_path, fs_trash.trash_path)
if not clean_trash:
    if not expire:
        abort("Use --expire [1h|3h|8h|1d|3d|7d] to specify the expiration time")
elif expire:
    abort(f"{first_path} is already in the trash")
```

| 模式 | 條件 | 動作 | 可否復原 |
|---|---|---|---|
| 丟進垃圾桶 | 路徑不在 `trash/` 底下，必須給 `--expire` | `Trash.move_to_trash()` | 可，用 `hf3fs_cli mv` 搬回來 |
| 立即清空 | 路徑在 `trash/` 底下，不可給 `--expire` | `FileSystem.remove(recursive=True)` | **不可** |

而且**所有路徑必須屬於同一個模式**（`cli.py:143-148`），混用直接 abort。刪除前有互動確認（`cli.py:168-169` 的 `click.confirm(msg, abort=True)`），`-y` 可跳過。

`ExpireType`（`cli.py:83-101`）是一個 click 自訂型別，順手做了單位正規化：`1hour`/`1hours` → `1h`，`3day`/`3days` → `3d`。

### 6.2 時間片目錄：垃圾桶命名的核心設計

`trash.py:30-57` 的 `TrashConfig` 是整個垃圾桶機制的關鍵：

```python
@dataclasses.dataclass
class TrashConfig:
    name: str
    expire: timedelta
    time_slice: timedelta

    def __post_init__(self):
        assert self.name and "-" not in self.name, f"invalid name {self.name}"
        assert self.expire >= timedelta(minutes=1), self.expire
        assert self.time_slice >= timedelta(minutes=1), self.time_slice
        assert self.time_slice < self.expire, (self.time_slice, self.expire)

    def current_dir(self) -> str:
        base_timestamp = BASE_TIMESTAMP                  # 1980-01-01 UTC+8
        current_timestamp = int(datetime.now(tz=UTC8_TZ).timestamp())
        time_slice_seconds = int(self.time_slice.total_seconds())
        expire_seconds = int(self.expire.total_seconds())
        start_timestamp = ((current_timestamp - base_timestamp) // time_slice_seconds) \
                          * time_slice_seconds + base_timestamp
        end_timestamp = start_timestamp + expire_seconds + time_slice_seconds
        ...
        return f"{self.name}-{format_date(start_datetime)}-{format_date(end_datetime)}"
```

產生的目錄名長這樣：

```
1d-20260814_1500-20260815_1600
│  │              └─ 過期時刻（到期後 trash_cleaner 會整個刪掉）
│  └───────────────── 時間片起點（對齊到 time_slice 的倍數）
└──────────────────── 保留期名稱（TRASH_CONFIGS 的 key）
```

六種預設（`trash.py:60-67`）：

| key | expire | time_slice | 最多同時存在的目錄數 |
|---|---|---|---|
| `1h` | 1 小時 | 10 分鐘 | 7 |
| `3h` | 3 小時 | 30 分鐘 | 7 |
| `8h` | 8 小時 | 30 分鐘 | 17 |
| `1d` | 1 天 | 1 小時 | 25 |
| `3d` | 3 天 | 1 天 | 4 |
| `7d` | 7 天 | 1 天 | 8 |

**「時間片」這個設計要解決的問題是：垃圾桶不能為每個被刪的項目各記一份過期時間。** 3FS 的目錄沒有可掛載自訂屬性的地方（沒有 xattr 這條路），而且清理器如果要逐項判斷過期，就得對每一項做 stat 並解析——項目數是無界的。改成「時間片桶」之後：

- 同一個時間片內刪除的所有東西，共用一個桶目錄；
- 過期時間寫在**桶的名字**裡，清理器只要 `readdir` 垃圾桶根目錄、對每個桶名 `split('-')` 就能判斷，完全不需要進到桶內部；
- 刪除是「刪整個桶」，一次 `HF3FS_IOC_REMOVE(recursive=true)` 解決。

代價是**過期時間被量化**。`end = start + expire + time_slice` 這個公式保證了一個不變式：

```
設項目在時刻 t 被刪除，時間片起點 s 滿足  s ≤ t < s + slice
存活時間 = end - t = (s + expire + slice) - t
          > (s + expire + slice) - (s + slice) = expire
存活時間 ≤ (s + expire + slice) - s = expire + slice
```

也就是**每個項目至少存活 `expire`，最多 `expire + slice`**。`assert time_slice < expire` 這個前置條件確保誤差不超過保留期本身。

`BASE_TIMESTAMP` 取 1980-01-01 UTC+8（`trash.py:13`）純粹是一個對齊原點，讓所有客戶端算出同一組桶邊界。

### 6.3 `move_to_trash`：一次 rename 就完成的「刪除」

`trash.py:114-176`：

```python
trash_dir = os.path.join(self.user_trash_path, config.current_dir())
try:
    os.mkdir(trash_dir, 0o755)
except FileExistsError:
    pass

trash_dir_fd, trash_dir_st = self.filesystem.opendir(trash_dir)

trash_name = trash_name or filename
current_trash_name = trash_name
retry = 0
while True:
    retry += 1
    try:
        self.filesystem._rename_ioctl(
            dir_fd, dir_st.st_ino, filename,
            trash_dir_st.st_ino, current_trash_name,
            True,                                     # ★ moveToTrash = True
        )
        return os.path.join(trash_dir, current_trash_name)
    except OSError as ex:
        if (ex.errno in (errno.ENOTDIR, errno.EEXIST, errno.ENOTEMPTY)
                and append_timestamp_if_exists and retry < 10):
            current_trash_name = f"{trash_name[0:200]}.{get_timestamp_us()}"
        else:
            raise
```

**「刪除一棵 PB 級的目錄樹」在這裡是 O(1) 的**——只是把一個 dentry 從舊父目錄搬到桶目錄，一個 FDB 交易就結束。資料一個 byte 都沒動。

名稱衝突處理很直白：撞名就在名字後面接微秒時間戳（`trash.py:169`），最多重試 10 次。`trash_name[0:200]` 是為了給時間戳留出空間，不超過 255 byte 的檔名上限。

三個前置檢查（`trash.py:71-107`）：

1. **拒絕 root**（`trash.py:82-83`）。
2. **`<mountpoint>/trash` 必須存在**，**`<mountpoint>/trash/<username>` 也必須存在**（`trash.py:86-93`）——`README.md:35` 說明這是管理員的責任：「the administrator needs to create a trash directory for each user at `/{3fs_mountpoint}/trash/{user_name}`」。工具本身不會建。
3. **使用者垃圾桶的 owner 必須是自己**（`trash.py:98-101`）。

`move_to_trash` 對目錄還額外要求「必須是 owner 且有 rwx」（`trash.py:134-138`）：

```python
if stat.S_ISDIR(st.st_mode):
    # The user must be the owner of the directory and have rwx permissions.
    imode = stat.S_IMODE(st.st_mode)
    if st.st_uid != os.geteuid() or (imode & 0o700) != 0o700:
        raise PermissionError(errno.EPERM, ...)
```

這條規則與伺服端的 `Acl::checkRecursiveRmPerm`（`Rename.cc:117`、`Remove.cc:168`）是同一條，只是提前在客戶端執行一次。同樣的檢查也出現在 `FileSystem.remove`（`fs.py:207-211`）。

### 6.4 `moveToTrash=True` 在伺服端多做了什麼

不只是一個標記。`src/meta/store/ops/Rename.cc` 裡它觸發了三件事：

1. **強制冪等**（`Rename.cc:63-69`）：

   ```cpp
   bool needIdempotent(Uuid &clientId, Uuid &requestId) const override {
     if (!req_.checkUuid()) return false;
     if (!req_.moveToTrash && !config().idempotent_rename()) return false;
     ...
   }
   ```

   一般的 rename 只有在 `idempotent_rename` 設定開啟時才走冪等記錄；**`moveToTrash` 的 rename 一律強制冪等**。`src/fbs/meta/Service.h:479` 也對應地要求 `if (moveToTrash) RETURN_ON_ERROR(checkUuid());`，且 `MetaClient.cc:808` 在 `moveToTrash` 時才填 uuid。理由很好懂：刪除是不可逆的，RPC 重試絕不能把「第二次 rename」變成「把垃圾桶裡的東西又搬走一次」。

2. **遞迴權限檢查**（`Rename.cc:110-124`）：`checkRecursiveRmPerm` 加上可設定深度的 `DirEntryList::recursiveCheckRmPerm(..., 128)`，確保使用者對整棵子樹有刪除權限——這是普通 rename 不做的。

3. **記錄原始路徑**（`Rename.cc:132-135`）：把來源的完整路徑重建出來，供事件日誌與日後追查。

---

## 7. 三份「trash 實作」為什麼並存

這是本次調研的重點問題。答案是：**它們不是三份重複實作，而是同一條刪除流水線上三個不同階段，各自跑在不同的進程、語言與權限模型裡。**

```
階段 ①  使用者主動「丟垃圾」          hf3fs_utils/trash.py   （Python，使用者身分，互動式）
        ↓  HF3FS_IOC_MOVE (moveToTrash=true)
        ↓  一次 rename，O(1)，資料不動
   /mnt/3fs/trash/<user>/1d-20260814_1500-20260815_1600/<被刪的東西>

階段 ②  到期後批次清理              src/client/trash_cleaner/  （Rust，root→seteuid，常駐守護）
        ↓  readdir 垃圾桶根 → 依桶名判斷是否過期
        ↓  HF3FS_IOC_REMOVE (recursive=true)
        ↓  meta 的 RemoveOp 把 entry 搬到 GC 目錄
   gcRoot/GC-Node-<nodeId>/<type>-<timestamp>-<inodeId>

階段 ③  真正回收 inode 與 chunk      src/meta/components/GcManager.*  （C++，meta server 內部）
        ↓  背景協程池掃 GC 目錄
        ↓  刪 inode、呼叫 StorageClient 移除 chunk、清 session
   空間釋出
```

### 7.1 為什麼階段 ① 和 ② 要分開

因為**過期是時間驅動的，而使用者不會為此常駐**。`hf3fs_cli` 是一個跑完就結束的互動式命令，不可能守著一小時後再回來刪。所以必須有一個常駐清理器。

### 7.2 為什麼階段 ② 用 Rust 而不是加進 `hf3fs_cli`

關鍵在**權限模型**。`trash_cleaner` 的核心是 `UserContext`（`src/client/trash_cleaner/src/main.rs:358-382`）：

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
impl Drop for UserContext { fn drop(&mut self) { seteuid(...); setegid(...); } }
```

清理器**以 root 啟動**，掃 `trash/` 根目錄（要求該目錄 owner 是 root，`main.rs:398-404`），對每個使用者子目錄先 `seteuid` 成該使用者，再以**那個使用者的身分**去發 remove ioctl（`main.rs:453-457`）。原因是 3FS 的權限判定發生在 meta server，而 meta 看的是 FUSE 傳過去的 `fuse_req_ctx(req)->uid`——所以要讓刪除通過權限檢查，就必須真的變成那個使用者。

這種「root 起動、逐使用者降權、RAII 還原」的模式在 Python 裡實作起來又危險又難保證（`seteuid` 失敗、例外路徑漏還原、GIL 與執行緒交互）。Rust 的 `Drop` 讓還原成為型別系統保證的一部分。而且它需要常駐、寫結構化 JSON 事件日誌（`main.rs:656-670` 用 `tracing` 的 `target: "event"` 分流到獨立的 `event.log`）、日誌輪替——這些都是守護進程的需求，不是 CLI 的需求。

`trash_cleaner` 的偏執程度也遠超 CLI，多處直接 `abort()`：

| 條件 | 行為 | 出處 |
|---|---|---|
| 垃圾桶不在 3FS 上（magic 不符） | `abort()` | `main.rs:118` |
| 命令列參數的路徑不含 `"trash"` 字串 | `abort()` | `main.rs:680-683` |
| 垃圾桶根目錄 owner 不是 root | `abort()` | `main.rs:398-404` |
| 準備刪一個「未過期」的項目 | `abort()` | `main.rs:257-263` |
| 準備刪一個「無法解析名字」的項目 | `abort()` | `main.rs:265-273` |
| 以 root 身分執行到 `Trash::open` | `assert_ne!(geteuid(), ROOT)` | `main.rs:72-74` |

`remove_item` 甚至在真正刪之前把 `check_item` 再跑一次並 `assert_eq!(..., Ok(true))`（`main.rs:275`）——同一個判斷在三行內執行了兩次。對一個會遞迴刪除 PB 級資料的守護進程來說，這種「寧可崩潰也不要誤刪」的態度是合理的。

刪除前它還會先 `readdir` 桶目錄，把即將消失的內容列成 JSON 記進事件日誌（`main.rs:284-325`）——這是唯一的「刪了什麼」的稽核記錄，因為 rename 進垃圾桶時的原始路徑只存在 meta 的事件日誌裡。

### 7.3 為什麼階段 ③ 必須在 meta server 裡

`HF3FS_IOC_REMOVE(recursive=true)` 在 meta 端**也不是真的刪**。`Remove.cc:185`：

```cpp
auto result = co_await gcManager().removeEntry(txn, entry, inode, gcInfo);
```

`GcManager::removeEntry` 做的是把這個 entry 從原本的父目錄搬到一個 GC 目錄底下，改名成一個可排序的鍵（`GcManager.h:70-72`）：

```cpp
static std::string formatGcEntry(char prefix, UtcTime timestamp, InodeId inode) {
  return fmt::format("{}-{:020d}-{}", prefix, (uint64_t)timestamp.toMicroseconds(), inode.toHexString());
}
```

GC 目錄掛在保留的 `InodeId::gcRoot()` 底下，每個 meta 節點一個（`GcManager.h:196-199`）：

```cpp
static std::string nameOf(flat::NodeId nodeId, size_t idx) {
  return idx == 0 ? fmt::format("GC-Node-{}", (uint32_t)nodeId)
                  : fmt::format("GC-Node-{}.{}", (uint32_t)nodeId, idx);
}
```

前綴字元把待回收項目分成四類，各有不同的排程優先級（`GcManager.h:166-194`）：

| 前綴 | 型別 | 優先級 |
|---|---|---|
| `d` | `DIRECTORY` | `MID_PRI` |
| `f` | `FILE_MEDIUM` | `MID_PRI` |
| `L` | `FILE_LARGE` | `HI_PRI` |
| `S` | `FILE_SMALL` | `LO_PRI` |

**大檔優先回收**——因為回收的目的是釋放空間，先處理大檔的空間回報率最高；小檔排最後，避免大量小檔的 meta 操作淹掉佇列。

這一階段只能在 meta server 裡做，理由有三：

1. **它需要跨越 meta 與 storage 兩層**：刪 inode 是 FDB 交易，刪 chunk 要透過 `StorageClient` 對每條 chain 發請求（`GcManager` 建構子就吃了 `mgmtd` 與 `storageClient` 相關的相依，`GcManager.h:74-93`）。客戶端沒有這個能力，也不該有。
2. **它必須是持久且可重入的**：GC 佇列本身就是 FDB 裡的目錄項，meta server 重啟後 `scanAllGcDirectories()`（`GcManager.h:259`）重新掃出來繼續做。客戶端進程隨時會死。
3. **它必須是分散式的**：`distributed_gc` 設定開啟時，任務會被隨機分派到所有 meta 節點的 GC 目錄（`GcManager.h:240-253` 的 `pickGcDirectory`）。

### 7.4 三者的職責邊界一覽

| | 階段 ① `trash.py` | 階段 ② `trash_cleaner` | 階段 ③ `GcManager` |
|---|---|---|---|
| 語言 | Python | Rust | C++ |
| 位置 | 使用者的機器（需掛載） | 專用節點（需掛載 + root） | meta server 進程內 |
| 觸發 | 使用者手動 | 定時掃描（`--interval` 秒） | meta 背景協程池 |
| 權限身分 | 呼叫者本人（禁 root） | root → `seteuid` 成各使用者 | 伺服端內部（無 uid 概念） |
| 對外介面 | `HF3FS_IOC_MOVE` | `HF3FS_IOC_REMOVE` | 直接操作 FDB + StorageClient |
| 成本 | O(1) 一次 rename | O(桶數) 掃描 + 每桶一次 ioctl | O(inode 數 + chunk 數) |
| 資料是否真的被刪 | 否 | 否（只是移進 GC 佇列） | **是** |
| 可否復原 | 可（`hf3fs_cli mv` 搬回） | 不可 | 不可 |
| 崩潰後果 | 無（無狀態） | 下次掃描補上 | 重啟後續掃 GC 目錄繼續 |

**一句話：`trash.py` 決定「什麼時候該死」，`trash_cleaner` 決定「時候到了」，`GcManager` 負責「執行死刑」。** 三者都無狀態或狀態持久化在 3FS 自己身上，任何一個掛掉都不會丟資料，只會延遲空間回收。

### 7.5 兩處共享的隱式協定

階段 ① 與 ② 之間沒有任何程式碼共用，只有兩個**靠約定維繫的格式**：

1. **桶目錄名 `<name>-<start>-<end>`**，日期格式 `%Y%m%d_%H%M`。
   - Python 側：`trash.py:12` 的 `DATE_FORMAT = "%Y%m%d_%H%M"`，`trash.py:57` 的 f-string。
   - Rust 側：`main.rs:33` 的 `parse_from_str(s, "%Y%m%d_%H%M")`，`main.rs:227-230` 的 `name.split("-")` 後要求恰好 3 段。
   - 這也解釋了 `TrashConfig.__post_init__` 為什麼 assert `"-" not in self.name`（`trash.py:37`）——名字裡有連字號就會把 `split("-")` 的段數弄成 4，Rust 側直接判為 `"invalid name"` 而**永遠不會清理**。

2. **時區硬編 UTC+8**。
   - Python 側：`trash.py:11` 的 `UTC8_TZ = timezone(timedelta(hours=8))`。
   - Rust 側：`main.rs:34` 的 `FixedOffset::east_opt(8 * 60 * 60)`。
   - 兩邊都把北京時間寫死進了磁碟格式。清理器比較時用的是 `chrono::Utc::now() > expire_time`（`main.rs:239-246`），跨時區的 `DateTime` 比較在 chrono 裡是比絕對瞬時，所以**執行機器的時區不影響正確性**。

   但 `trash_cleaner` 的測試不是這樣：`test_parse_item`（`main.rs:521-590`）用 `chrono::Local::now()` 格式化再用 `parse_datetime` 解析，然後 `assert_eq!` 兩者相等（`main.rs:539-542`）——只有在**執行機器的本地時區正好是 UTC+8** 時才會通過。這個測試是時區相依的。

---

## 8. 與 `admin_cli` 的關係：互補而非重疊

`src/client/cli/admin/` 底下確實有同名的 `rm`（`Remove.cc`）與 `mv`（`Rename.cc`），但兩者的定位完全不同。

`admin_cli` 的 `rm`（`src/client/cli/admin/Remove.cc:9-28`）：

```cpp
auto parser = argparse::ArgumentParser("rm");
parser.add_argument("path");
parser.add_argument("-r", "--recursive").default_value(false).implicit_value(true);
...
auto res = co_await env.metaClientGetter()->remove(env.userInfo, env.currentDirId, Path(path), recursive);
```

`admin_cli` 的 `mv`（`src/client/cli/admin/Rename.cc:22-26`）：

```cpp
auto res = co_await env.metaClientGetter()->rename(env.userInfo, env.currentDirId, src, env.currentDirId, dst);
```

注意最後那個參數：`MetaClient::rename` 的 `moveToTrash` 有預設值 `false`（`src/client/meta/MetaClient.h:190`），而 `admin_cli` **沒有傳**——所以 `admin_cli` 的 `mv` 永遠不會帶垃圾桶語意，也就無法把東西移進 `trash/`（會被 `Rename.cc:128` 擋成 `kNoPermission`）。

| | `hf3fs_cli`（Python） | `admin_cli`（C++） |
|---|---|---|
| 通道 | ioctl → 已掛載的 FUSE 進程 → meta RPC | **直接** meta RPC（自己是 MetaClient） |
| 前提 | 機器上已掛載 3FS | 有 cluster 設定與 **token** |
| 身分 | 由核心的 `fuse_req_ctx()->uid` 決定，無法偽造 | 由 `env.userInfo` 決定，token 內含身分 |
| 相依 | 只有 `click` | 整個 3FS C++ 客戶端 |
| 使用者 | 一般終端使用者 | 叢集管理員 |
| 垃圾桶 | **支援**（唯一入口） | 不支援 |
| 跨 mount rename | **支援**（本來就不經 VFS） | 不適用（本來就不經 VFS） |
| 其他能力 | 無 | chain table 上傳、target 管理、GC 列表（`ListGc.cc`）、config 熱更新、benchmark…… 共約 60 個命令 |

所以兩者是**互補**的：`admin_cli` 是管理面的全能工具但需要 token，`hf3fs_cli` 是資料面的窄工具但零門檻。垃圾桶功能只在 `hf3fs_cli` 裡，因為它本質上是一個「使用者自助」的功能——需要以真實使用者身分執行、需要互動確認、不該要求發 token。

值得一提的是 `admin_cli` 有 `ListGc`（`src/client/cli/admin/ListGc.cc`），可以查看階段 ③ 的 GC 佇列——這是三個階段裡唯一有觀測工具的一環。階段 ①、② 只能靠 `ls /mnt/3fs/trash` 與 `trash_cleaner` 的 `event.log`。

---

## 9. 錯誤處理與可用性細節

### 9.1 錯誤呈現

`cli.py:41-43` 的 `abort()` 把訊息染紅送到 stderr 再 `sys.exit(1)`：

```python
def abort(msg):
    click.echo(click.style(msg, fg="red"), err=True)
    sys.exit(1)
```

兩個命令的 try/except 都刻意讓 `AssertionError` 穿透（`cli.py:77-80`、`cli.py:179-182`）：

```python
except AssertionError:
    raise
except Exception as ex:
    abort(f"Move failed: {ex}")
```

這**不是**為了處理使用者取消確認。`assert click.confirm(msg, abort=True)`（`hf3fs_utils/cli.py:169`）位在 `try` 之外（try 從 `:172` 才開始），而且 `click.Abort` 繼承的是 `RuntimeError` 不是 `AssertionError`——兩個 except 子句一個都碰不到它，使用者按 N 時例外直接往外拋，由 click 框架印 `Aborted!`。反例更直接：`mv`（`cli.py:60-80`）根本沒有任何 confirm，卻有一模一樣的 `except AssertionError: raise`（`:77-78`）。

真正的理由是讓**底層那些內部斷言**以完整 traceback 呈現，而不是被 `abort()` 壓成一行紅字。這些斷言散在檔案系統層與垃圾桶設定層：`fs.py:186-187`（`_rename_ioctl` 要求檔名不含 `/`）、`fs.py:86`（`_encode_filename`）、`fs.py:76`（ioctl 版本檢查）、`trash.py:36-40`（`TrashConfig.__post_init__`）、`trash.py:97,122`。它們代表「程式自己的前提被違反」，堆疊資訊有診斷價值；而 `except Exception` 那條路徑處理的是 IO 錯誤、權限不足這類**環境問題**，使用者只需要一句人話。這是「把程式錯誤與環境錯誤分流」的簡單做法。

### 9.2 fd 洩漏防護

`fs.py` 裡每一個開 fd 的地方都用 `try/finally` 或 `try/except` 包好：

- `FileSystem.__init__` 的 `virt_fd`（`fs.py:45-79`）
- `opendir` 失敗時的 `os.close(dir_fd)`（`fs.py:111-113`）
- `rename` 的兩個 dir fd（`fs.py:171-175`）
- `remove` 的 dir fd（`fs.py:218-220`）
- `move_to_trash` 的兩個 fd（`trash.py:172-176`）

`Trash.__init__` 甚至只是為了 stat 就開了再立刻關（`trash.py:95-96`）：

```python
user_trash_fd, user_trash_st = filesystem.opendir(user_trash)
os.close(user_trash_fd)
```

之所以不直接用 `os.stat`，是為了複用 `opendir` 裡的 `st_dev` 與虛擬 inode 檢查。

### 9.3 錯誤訊息的路徑補寫

`fs.py` 多處在 catch 之後補上檔名再 re-raise：

```python
except OSError as ex:
    ex.filename = old_path
    ex.filename2 = new_path
    raise
```

（`fs.py:145-147`、`fs.py:154-156`、`fs.py:167-170`、`fs.py:215-217`、`trash.py:130-132`）。因為底層用的是 `dir_fd=` 相對路徑操作，`OSError` 自帶的 `filename` 會是那個沒有上下文的短檔名。這是很細心的處理。

---

## 10. 限制與陷阱

| 項目 | 說明 | 出處 |
|---|---|---|
| 掛載點只往上找 3 層 | `/a/b/c/mnt` 這種深度找不到，須設 `HF3FS_CLI_MOUNTPOINT` | `cli.py:20` |
| 路徑不得含 `..` | 直接 abort，訊息說 "not supported yet" | `cli.py:31-32` |
| root 完全不能用 | `euid==0` 或 `egid==0` 皆拒絕 | `fs.py:81-83`、`trash.py:82-83` |
| `mv` 不搬 symlink | 明確拒絕 | `fs.py:143-144` |
| `mv` 目標不得已存在 | 與 POSIX `rename(2)` 的覆蓋語意不同 | `fs.py:149-153` |
| `mv` 不能搬進垃圾桶 | 客戶端先擋，伺服端也擋 | `fs.py:130-133`、`Rename.cc:128` |
| 垃圾桶目錄需管理員預建 | `trash/` 與 `trash/<user>/` 都不會自動建立 | `trash.py:86-93`、`README.md:35` |
| 過期時間被量化 | 實際存活 `expire` ~ `expire + time_slice` | `trash.py:50-53` |
| 只有六種保留期 | 無法自訂 | `trash.py:60-67` |
| 時區硬編 UTC+8 | 寫進磁碟上的目錄名格式 | `trash.py:11`、`main.rs:34` |
| ioctl 版本檢查是 `assert` | `python -O` 會移除 | `fs.py:76` |
| ioctl 命令碼手抄 | C 端結構一改就靜默失配 | `fs.py:20-29` vs `fuse.h:44-56` |
| 桶名不得含 `-` | 否則 `trash_cleaner` 永遠判為 invalid、不清理 | `trash.py:37`、`main.rs:227-230` |
| `rmtree` 混用兩種模式會 abort | 所有路徑必須同在或同不在垃圾桶 | `cli.py:143-148` |
| `trash_cleaner` 不清「不認識」的項目 | `clean_unknown` 恆為 false，程式碼裡是 `todo!()` | `main.rs:137-138`、`main.rs:212` |

最後一項值得展開：`Trash::clean` 的第一行是 `assert!(!clean_unknown);`（`main.rs:138`），呼叫端傳的也永遠是 `false`（`main.rs:472`）。也就是說**任何名字不符合 `<name>-<start>-<end>` 格式的東西，一旦出現在使用者的垃圾桶根目錄裡，就會永遠留在那裡**，只會在每輪掃描時記一條 warn（`main.rs:205-214`）。使用者若手動在 `trash/<user>/` 底下 `mkdir` 一個目錄再往裡放東西，那些資料永遠不會被自動回收。

---

## 11. 檔案索引表

### 11.1 `hf3fs_utils/`（本次任務的主體）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `hf3fs_utils/__init__.py` | 0 | 空檔，僅標記為 package |
| `hf3fs_utils/hf3fs_cli` | 5 | 可執行入口腳本，`from hf3fs_utils import cli` 後呼叫 `cli.cli(max_content_width=120)` |
| `hf3fs_utils/cli.py` | 192 | click 命令定義：掛載點探測 `get_filesystem`、路徑正規化 `abs_path`、`mv` 與 `rmtree` 兩個子命令、`ExpireType` 參數型別、紅字 `abort` |
| `hf3fs_utils/fs.py` | 233 | ioctl 封裝層：四個命令碼常數、`FileSystem` 的掛載點驗證（magic + version）、`opendir`/`split_path` 的三道安全檢查、`rename`/`remove` 與其 `_*_ioctl` 打包、root 禁令 |
| `hf3fs_utils/trash.py` | 175 | 垃圾桶策略：`TrashConfig`（過期 + 時間片 → 桶目錄名）、六種 `TRASH_CONFIGS`、`Trash` 的權限檢查與 `move_to_trash`（含撞名重試） |
| `hf3fs_utils/README.md` | 36 | 建置方式（`bdist_wheel`）、`rmtree`/`mv` 的 `--help` 輸出、管理員需預建 `trash/<user>` 的說明 |
| `setup_hf3fs_utils.py` | 13 | pip 打包定義：套件名 `hf3fs_utils`、版本硬編 `1.0.7`、僅依賴 `click`、以 `scripts=` 安裝 `hf3fs_cli` |

### 11.2 對側與相關組件（本報告引用到的部分）

| 檔案 | 相關職責 |
|---|---|
| `src/lib/api/fuse.h` | `HF3FS_IOC_*` 命令碼與 `Hf3fsIoctlMove` / `Hf3fsIoctlRemove` 結構定義——`fs.py` 手抄常數的來源 |
| `src/fuse/FuseOps.cc` | `hf3fs_ioctl` 中 `HF3FS_IOC_GET_MAGIC_NUM` / `GET_IOCTL_VERSION` / `MOVE` / `REMOVE` 四個分支；`srcParent != ino` 的 fd 綁定檢查；`checkIsVirt` 的虛擬 inode 判準 |
| `src/client/meta/MetaClient.cc/.h` | `rename(..., moveToTrash)` 與 `remove(..., recursive)` 的 RPC 封裝；`moveToTrash` 時才填 uuid |
| `src/fbs/meta/Service.h` | `RenameReq` 的 `moveToTrash` 欄位與 `checkUuid()` 前置條件 |
| `src/meta/store/ops/Rename.cc` | `underTrash` 判定、`moveToTrash` 強制冪等、遞迴刪除權限檢查、原始路徑記錄、root 豁免 |
| `src/meta/store/ops/Remove.cc` | 遞迴刪除的權限檢查，以及把 entry 交給 `gcManager().removeEntry()` |
| `src/meta/components/GcManager.h/.cc` | 階段 ③：GC 目錄命名（`GC-Node-<id>`）、四類前綴與優先級、分散式 GC 分派、背景協程池 |
| `src/client/trash_cleaner/src/main.rs` | 階段 ②：`UserContext`（root → `seteuid`）、桶名解析與過期判定、`HF3FS_IOC_REMOVE` 的 Rust ioctl 綁定、JSON 事件日誌、多處 `abort()` 防誤刪 |
| `src/client/trash_cleaner/Cargo.toml` / `README.md` | 相依（nix/tracing/structopt/chrono）與命令列參數說明 |
| `src/client/cli/admin/Remove.cc` / `Rename.cc` | `admin_cli` 的 `rm` / `mv`，走 MetaClient RPC，不帶垃圾桶語意 |
| `src/client/cli/admin/ListGc.cc` | 階段 ③ 的唯一觀測入口 |
| `setup.py` | 另一個獨立 pip 套件 `hf3fs_py_usrbio`，與 `hf3fs_utils` 無任何相依關係 |
