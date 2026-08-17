# `hf3fs` / `hf3fs_fuse` Python 客戶端套件深度剖析

> 對應原始碼：`hf3fs/__init__.py`（310 行）、`hf3fs/fuse.py`（44 行）、`hf3fs_fuse/io.py`（139 行）、`hf3fs_fuse/fuse.py`（7 行）、`hf3fs_fuse/fuse_demo.py`（30 行）、`hf3fs_fuse/__init__.py`（0 行）、`hf3fs/__init__.py` 之外無其他 `hf3fs/` 檔案
> 打包定義：`setup.py`
> C 擴充來源：`src/lib/py/usrbio_binding.cc`（**有建置**）、`src/lib/py/binding.cc`（**未建置**）
> 與 USRBIO 報告的關係：本文只講 Python 層。C API、Iov/Ior 的記憶體佈局、喚醒機制等請見 `hf3fs_api_shared-USRBIO旁路IO深度剖析.md`

---

## 0. 一句話總結

這棵樹裡有**兩個** Python 客戶端套件，它們綁的是**兩套不同的 C++ API**：`hf3fs_fuse` 綁的是 USRBIO（`usrbio_binding.cc`，有建置、有打包、有 demo），`hf3fs` 綁的是早已死去的 client-agent `IClient`（`binding.cc`，缺 header、不在建置目標裡、不在打包清單裡）。因此本次任務要查的那個「打包異常」不是遺漏——`setup.py` 不打包 `hf3fs/` 是**正確的**，因為 `hf3fs/__init__.py` 的第一行 import 在實際建出來的 `hf3fs_py_usrbio` 上必然失敗。兩個套件恰好搶同一個模組名 `hf3fs_py_usrbio`，這正是它們是新舊兩代實作的鐵證。

---

## 1. 兩套 Python API 的全景

```
                    ┌──────────────────────────────────────────────┐
                    │  PYBIND11_MODULE(hf3fs_py_usrbio, m)          │
                    │  ★ 兩個 .cc 都宣告了同一個模組名 ★             │
                    └──────────────────────────────────────────────┘
                       ▲                                    ▲
    ┌──────────────────┘                                    └──────────────────┐
    │ src/lib/py/usrbio_binding.cc                          src/lib/py/binding.cc │
    │ ✅ 在 CMakeLists.txt:1                                 ❌ 不在任何建置目標    │
    │ ✅ 匯出 iovec / ioring / register_fd /                 ❌ #include "lib/api/  │
    │    deregister_fd / extract_mount_point /                 Client.h" ← 檔案不存在│
    │    force_fsync / hardlink / punch_hole                 匯出 Client / dirent /  │
    │ ❌ 不匯出 Client                                          stat_result /        │
    │ ❌ 不匯出 HF3FS_SUPER_MAGIC                               HF3FS_SUPER_MAGIC     │
    └───────────┬──────────────────────────────┬─────────────────────┬────────────┘
                │                              │                     │
        ┌───────▼────────┐            ┌────────▼─────────┐   ┌───────▼─────────┐
        │ hf3fs_fuse/    │            │ hf3fs_fuse/      │   │ hf3fs/          │
        │   io.py        │            │   fuse.py        │   │   __init__.py   │
        │   fuse_demo.py │            │   （7 行工具）    │   │   fuse.py       │
        │ ✅ setup.py     │            │ ✅ setup.py       │   │ ❌ 不在 packages │
        │   packages 內   │            │   packages 內     │   │ ❌ import 必失敗 │
        └────────────────┘            └──────────────────┘   └─────────────────┘
             USRBIO（現行）                                      client agent（已死）
```

三層分工（僅就**能用的那一邊**而言）：

| 層 | 檔案 | 抽象層級 |
|---|---|---|
| C 擴充 | `src/lib/py/usrbio_binding.cc` → `hf3fs_py_usrbio` | 直接對應 `hf3fs_*` C 函式，`iovec` 支援 buffer protocol 與切片 |
| 薄封裝 | `hf3fs_fuse/io.py` | 補上 C API 不做的事：建 shm、建 symlink、`__del__` 清理；提供 `read_file` 便利函式 |
| 範例 | `hf3fs_fuse/fuse_demo.py` | 30 行完整流程 |

而 `hf3fs/__init__.py` 那一層（`listdir` / `scandir` / `walk` / `BinaryFile` / thread-local client 快取）是為**另一套 API** 寫的高階封裝，與 USRBIO 沒有任何交集。

---

## 2. 打包異常的查證：`hf3fs/` 到底怎麼被安裝？

### 2.1 現象

`setup.py:83-94`：

```python
setup(
    name="hf3fs_py_usrbio",
    version=version,
    description="Python binding for hf3fs client library",
    long_description="",
    packages=['hf3fs_fuse'],                      # ← 只有 hf3fs_fuse
    ext_modules=[CMakeExtension("hf3fs_py_usrbio")],
    cmdclass={"build_ext": CMakeBuild},
    zip_safe=False,
    extras_require={"test": ["pytest>=6.0"]},
    python_requires=">=3.6",
)
```

而 `hf3fs/__init__.py:11-16` 卻去查一個名叫 `'hf3fs'` 的發行版：

```python
from pkg_resources import get_distribution

try:
    __version__ = get_distribution('hf3fs').version
except:
    __version__ = "debug"
```

`setup_hf3fs_utils.py` 打包的是 `hf3fs_utils`，也不是 `hf3fs`。**整棵樹裡沒有任何一個 `setup.py` 會產生名為 `hf3fs` 的發行版**——所以那個 `try` 永遠走 `except`，`__version__` 恆為 `"debug"`。

### 2.2 決定性證據：第一行 import 就會失敗

`hf3fs/__init__.py:1`：

```python
from hf3fs_py_usrbio import Client, iovec
```

實際建出來的 `hf3fs_py_usrbio` 來自 `usrbio_binding.cc`（`src/lib/py/CMakeLists.txt:1`）：

```cmake
pybind11_add_module(hf3fs_py_usrbio usrbio_binding.cc)
```

`setup.py:79` 也只建這一個 target：

```python
subprocess.check_call(["cmake", "--build", ".", "--target", "hf3fs_py_usrbio"] + build_args, cwd=build_temp)
```

該模組匯出的全部符號是（`usrbio_binding.cc` 的 `m.def` / `py::class_` 逐一清點）：

| 符號 | 種類 | 行號 |
|---|---|---|
| `extract_mount_point` | function | `usrbio_binding.cc:42` |
| `register_fd` / `deregister_fd` | function | `usrbio_binding.cc:62`、`79` |
| `force_fsync` | function | `usrbio_binding.cc:90` |
| `hardlink` | function | `usrbio_binding.cc:100` |
| `iovec` | class | `usrbio_binding.cc:111` |
| `ioring` | class | `usrbio_binding.cc:232` |
| `punch_hole` | function（兩個多載） | `usrbio_binding.cc:437`、`455` |

**沒有 `Client`，也沒有 `HF3FS_SUPER_MAGIC`。** 這兩個符號只存在於 `binding.cc`：

```cpp
m.attr("HF3FS_SUPER_MAGIC") = py::int_(uint32_t(HF3FS_SUPER_MAGIC));   // binding.cc:42
...
py::class_<hl::IClient, std::shared_ptr<hl::IClient>>(m, "Client")     // binding.cc:127
    .def(py::init([](std::string_view mountName, std::string_view token, bool as_super) {
           if (as_super) {
             return unwrap(hl::IClient::newSuperClient(mountName, token));
           }
           ...
         }),
         ...
         py::arg("as_super") = false,
```

而 `binding.cc:5` 的 `#include "lib/api/Client.h"` 指向一個**不存在的檔案**——全樹 `find . -name "Client.h"` 只找得到 `src/common/net/Client.h` 與 `src/common/net/sync/Client.h`，兩者都不是 `lib/api/` 底下的東西，也不定義 `hf3fs::lib::IClient` 的工廠實作。`binding.cc` 因此**無法編譯**，這也是它不在 CMake 目標裡的原因。

### 2.3 交叉驗證：`hf3fs/` 用的每個方法都只在 `binding.cc` 裡

`hf3fs/__init__.py:56-59` 在 import 時就會對 `Client` 反射取出 23 個方法：

```python
for _name in ['stat', 'fstat', 'mkdir', 'rmdir', 'unlink', 'remove', 'realpath', 'readlink', 'opendir', 'readdir',
              'creat', 'symlink', 'link', 'open', 'close', 'chmod', 'chown', 'chdir', 'ftruncate',
              'iovalloc', 'iovfree', 'preadv', 'pwritev']:
    _setupH3Method(_name)
```

（`_setupH3Method` 內部是 `functools.wraps(getattr(Client, name))`，`hf3fs/__init__.py:48`——所以缺任何一個都會在 import 時炸。）

逐一比對：**23 個名字全部出現在 `binding.cc`，一個都沒有出現在 `usrbio_binding.cc`。** 而且方法簽章也精確吻合 `binding.cc` 而非 USRBIO：

| `hf3fs/__init__.py` 的呼叫 | `binding.cc` 的定義 | `src/lib/api/hf3fs.h` 的 `IClient` |
|---|---|---|
| `client.read(fd, buf, readahead=...)`（`:303`） | `binding.cc:436-453`（`readahead` 是 `kw_only`） | `hf3fs.h:151` |
| `client.write(fd, buf, flush=...)`（`:308`） | `binding.cc:455` | `hf3fs.h:152` |
| `client.lseek(fd, pos, how, readahead=...)`（`:282`） | `binding.cc:426-434` | `hf3fs.h:156` |
| `client.open(path, flags, 0o644, dir_fd=...)`（`:261`） | `binding.cc` 的 `PY_METHOD_AT(open…)` | `hf3fs.h:121-122` |
| `client.stat(name, dir_fd=…, follow_symlinks=…)`（`:160`） | `binding.cc:245-252` | `hf3fs.h:85-88` |
| `iovalloc / iovfree / preadv / pwritev` | `binding.cc:347`、`373`… | `hf3fs.h:143-148` |
| `Client(mount_name, token, as_super=…)`（`:73`、`:80`） | `binding.cc:128-138` | `hf3fs.h:51-52` 的 `newClient` / `newSuperClient` |

`hf3fs/fuse.py:4` 同樣如此：

```python
from hf3fs_py_usrbio import HF3FS_SUPER_MAGIC
```

這個屬性只在 `binding.cc:42` 設定。

### 2.4 結論

**`hf3fs/` 是舊「client agent」設計的殘留，不是打包遺漏。**

支撐這個結論的五項證據：

1. `hf3fs/__init__.py:1` 與 `hf3fs/fuse.py:4` 匯入的符號（`Client`、`HF3FS_SUPER_MAGIC`）在實際建置出來的 `hf3fs_py_usrbio` 中不存在——**`import hf3fs` 必然 `ImportError`**。
2. 這些符號的唯一提供者 `src/lib/py/binding.cc` 引用了不存在的 `lib/api/Client.h`，因此**根本無法編譯**，也不在 `src/lib/py/CMakeLists.txt` 的建置清單裡。
3. `Client` 背後的 `IClient`（`src/lib/api/hf3fs.h:49-173`）是一套完整的使用者態 POSIX 介面（`opendir`/`creat`/`chown`/`preadv`/`sharedFileHandles`…），全樹**沒有任何實作**，只有 `binding.cc` 這一個引用者。
4. `src/lib/common/paths.h:4` 留著 `static const char *varTmpPath = "/var/tmp/hf3fs_client_agent";`——這個常數**全樹沒有任何使用點**，是那個「本機 client agent 進程」設計的化石。同一個目錄下的 `PerProcTable.h`（`AllProcMap` 的 fork 感知 fd 表）也只服務於這條死掉的路徑。
5. `hf3fs/`、`hf3fs_fuse/`、`binding.cc` 三者的 git 歷史都停在 `815e55e Initial commit`——開源時就是這個樣子，之後從未被碰過。

因此 `setup.py` 的 `packages=['hf3fs_fuse']` 是**刻意且正確**的：只打包還能用的那一半。`get_distribution('hf3fs')` 那段 try/except 則是舊套件曾經以 `name="hf3fs"` 獨立發佈過的痕跡——它被 `except:` 兜住，所以即使在死程式碼裡也不會多製造一個例外。

`hf3fs/` 的價值現在只剩下**文件性質**：它示範了 3FS 團隊原本想給 Python 使用者的高階 API 長什麼樣。下面幾節就以這個角度來讀它。

---

## 3. `hf3fs_fuse`：現行可用的那一層

### 3.1 `io.py` 的職責邊界

C 擴充層（`usrbio_binding.cc`）刻意**不做**兩件事，全部推給 Python：

1. **不建 shm**：`iovec.__init__` 走的是 `hf3fs_iovwrap`（`usrbio_binding.cc:138`），只包裝一塊呼叫者已經配好的記憶體。
2. **不建 symlink**：因此 iov 不會被 FUSE 進程認得。

`hf3fs_fuse/io.py:48-64` 的 `make_iovec` 補上這兩塊：

```python
def make_iovec(shm, hf3fs_mount_point, block_size=0, numa=-1):
    id = str(uuid4())
    target = os.path.normpath(f'/dev/shm/{shm.name}')
    link = f'{hf3fs_mount_point}/3fs-virt/iovs/{id}{f".b{block_size}" if block_size > 0 else ""}'
    os.symlink(target, link)
    return iovec(h3fio.iovec(shm.buf, id, hf3fs_mount_point, block_size, numa), link)
```

`os.symlink` 那一行就是 USRBIO 的整個註冊協定（詳見 USRBIO 報告 §4.1：symlink 檔名即參數編碼）。注意這裡只組出 `.b<block_size>` 這一個後綴——Python 側的 iov 不可能是 io ring，所以 `.r/.w/.t/.p/.f` 都用不到。

`io.py:9-21` 的 `iovec` wrapper 類別存在的唯一理由，是把 symlink 的生命週期綁上 Python 的物件生命週期：

```python
class iovec:
    def __init__(self, iov, link):
        self.iov = iov
        self.link = link

    def __del__(self):
        os.unlink(self.link)
```

用 `__del__` 而不是 context manager，是為了讓 `iov` 能以模組級變數的形式長期存在（demo 就是這麼用的）。代價是：**參考循環或直譯器結束時的清理順序會讓 `__del__` 不保證執行**。所幸 USRBIO 有第三道保險——FUSE 端會在進程結束時透過 `releasedir` 回收該 pid 的所有 iov（詳見 USRBIO 報告 §4.4）。

`io.py:23-41` 的 `ioring` wrapper 則幾乎只是轉接，唯一的實質邏輯是 `prepare` 的型別轉換（`io.py:31-35`）：

```python
def prepare(self, iov, *args, **kwargs):
    if type(iov) == iovec:
        return self.ior.prepare(iov.iov, *args, **kwargs)
    else:
        return self.ior.prepare(iov, *args, **kwargs)
```

因為 `iov[:512]` 這種切片回傳的是 **C 層的 `h3fio.iovec`**（`usrbio_binding.cc:187-216`）而不是 Python 的 wrapper，所以兩種型別都得吃。

### 3.2 `IorPriority`：一個沒有作用的常數

`io.py:43-46`：

```python
class IorPriority(object):
    HIGH = -1
    NORMAL = 0
    LOW = 1
```

而 `make_ioring` 把 `priority` 一路傳給 `h3fio.ioring(...)`，C 層卻直接丟棄（`usrbio_binding.cc:248`：`(void)priority;`），因為它只呼叫 `hf3fs_iorcreate4` 而該函式沒有 priority 參數（詳見 USRBIO 報告 §3.4 與 §11.1）。

值得補充的是 `IorPriority` 的數值與 C API 的約定一致（`UsrbIo.cc:334-337`：`prio < 0` → 高、`== 0` → 普通、`> 0` → 低），所以這不是設計錯誤，只是 C 層的實作沒跟上。`io.py:97` 的 `read_file` 也照樣把 `priority` 傳下去，同樣無效。

### 3.3 `read_file`：唯一的高階便利函式

`io.py:86-139`。它把「開檔 → 配 shm → 建 iov → 建 ior → 逐塊讀 → 拆解」全包起來，預設塊大小 1 GiB：

```python
def read_file(fn, hf3fs_mount_point=None, block_size=1 << 30, off=0, priority=None, cb=None):
    if hf3fs_mount_point is None:
        hf3fs_mount_point = extract_mount_point(fn)
    ...
    ior = make_ioring(hf3fs_mount_point, 1, priority=priority)     # entries=1，純序列
    i = 0
    roff = off
    while True:
        ior.prepare(iov[:], True, fd, roff)
        done = ior.submit().wait(min_results=1)[0]
        if done.result < 0:
            raise OSError(-done.result)
        if done.result == 0:
            break
        ...
        if done.result < block_size:
            break
        roff += block_size
```

三個設計取捨：

- **`entries=1`**：完全序列，沒有任何並行。這個函式是為「一次讀完一個檔案」而寫，不是為吞吐而寫。
- **短讀即視為結尾**（`io.py:120-121`）：`done.result < block_size` 就 break。這在檔案中間有洞的情況下會**提前截斷**——USRBIO 預設會把洞補零並回報完整長度（`PioV.cc:225-235`），但如果洞剛好在尾端、或是後端回了 `kChunkNotFound`，`result` 就會小於請求長度而無法與 EOF 區分（詳見 USRBIO 報告 §9.2）。
- **`cb` 回呼可以改寫偏移量**（`io.py:112-118`）：

  ```python
  res = cb(shm.buf[:done.result], roff)
  if type(res) == int:
      roff = res
      continue
  elif res:
      return
  ```

  回傳 int 就是「跳到這個偏移繼續讀」，回傳 truthy 就是「停止」。這讓 `read_file` 可以拿來做格式解析（讀 header → 決定下一段位置）。註解建議優先用 `cb` 而非把整個檔案讀進記憶體（`io.py:84-85`）。

`finally` 區塊（`io.py:133-139`）的順序是有意義的：`deregister_fd` → `os.close` → `del ior` → `del iov` → `shm.close()` → `shm.unlink()`。反註冊必須早於關檔（詳見 USRBIO 報告 §3.6）。但這個 `finally` 有一個明顯缺陷：**如果 `os.open` 就失敗了，`fd` 這個名字根本沒被綁定**，`deregister_fd(fd)` 會丟 `NameError` 蓋掉原本的例外。

### 3.4 `hf3fs_fuse/fuse.py`：7 行的位置約定

```python
def get_mount_point(p):
    np = os.path.realpath(p)
    parts = PosixPath(np).parts
    return os.path.join(*parts[:3])
```

它假設**掛載點永遠在路徑的第二層**，即 `/<top>/<mount-name>/...`。這比 USRBIO 的 `extract_mount_point`（掃 `/proc/self/mountinfo`，見 USRBIO 報告 §3.2）粗糙得多，也比 `hf3fs_utils` 的做法（找 `3fs-virt` 子目錄，`hf3fs_utils/cli.py:20-24`）粗糙。三個工具用了三套不同的掛載點判定方式，只有 USRBIO 那套是可靠的。

這個函式在 `hf3fs_fuse` 內部**沒有任何呼叫者**——`io.py` 用的是從 C 層匯入的 `extract_mount_point`（`io.py:2`、`io.py:88`）。它是留給使用者程式的工具函式。

### 3.5 `fuse_demo.py`：官方最小範例

30 行，逐行都有意義（完整內容與逐行解說見 USRBIO 報告 §12.3）。這裡只補充兩處在 Python 層才成立的注意事項：

```python
shm = SharedMemory(size=1024, create=True)
iov = make_iovec(shm, '/hf3fs-cluster', 0, -1)
shm.unlink()      # shm can be unlinked after make_iovec
```

`shm.unlink()` 緊接在 `make_iovec` 之後——因為此時 FUSE 進程已經完成 mmap（symlink 是同步的 FUSE 操作），`/dev/shm` 上的名字不再需要。這與 C 側 io ring 的 `maybeUnlinkShm()` 是同一個手法（`UsrbIo.cc:160-162`）。

```python
ios = [(iov[:512], fd, 512), (iov[512:], fd, 0)]
for io in ios:
    ior.prepare(io[0], True, io[1], io[2], userdata=io)
    # userdata must be a referenced python object, we reference io in the list ios,
    # so it will not be sent to GC
```

`userdata` 的引用計數在 C 層是手動管的：`prepare` 時 `inc_ref`、`wait` 拿回後 `dec_ref`（`usrbio_binding.cc:290`、`417-419`）。demo 特意把 tuple 存在 `ios` 這個 list 裡再多一層保險。**沒被 wait 收割的 IO，其 userdata 會永久洩漏**。

---

## 4. `hf3fs/`：那套沒能上線的高階 API

以下內容描述的是**目前無法執行**的程式碼。之所以仍值得記錄，是因為它揭示了 3FS 原本的 Python 使用者介面設計。

### 4.1 thread-local client 快取

`hf3fs/__init__.py:18-20`：

```python
DEFAULT_CLIENT = th.local()
DEFAULT_CLIENT.client = None
DEFAULT_CLIENT.clients = {}

MOUNT_INFO = {}                     # ← 這個是普通全域 dict，跨執行緒共享
```

設計意圖很清楚：

- **`MOUNT_INFO`（掛載點 → token）跨執行緒共享**——設定資訊沒有執行緒親和性；
- **`Client` 物件每執行緒一份**——推測是因為 `IClient` 不保證執行緒安全（`hf3fs.h` 沒有任何關於併發的說明，而它維護 fd 表這類可變狀態）。

但這個初始化有一個**真實的錯誤**：`th.local()` 的屬性賦值只對**執行賦值的那條執行緒**生效。模組層級的這三行在 import 時執行，因此只有 import 的那條執行緒擁有 `.client` 與 `.clients` 屬性。任何其他執行緒第一次碰到：

```python
elif DEFAULT_CLIENT.client is None:                    # hf3fs/__init__.py:40
```

拿到的不是 `None` 而是 **`AttributeError`**。`setupDefaultClient` 也一樣（`hf3fs/__init__.py:73`）：

```python
client = DEFAULT_CLIENT.clients[mount_name] = Client(...)
```

在新執行緒上 `DEFAULT_CLIENT.clients` 不存在 → `AttributeError`。正確寫法應該是繼承 `threading.local` 並覆寫 `__init__`，或每次存取都用 `getattr(DEFAULT_CLIENT, 'client', None)`。也就是說，**這套 thread-local 快取實際上只在單執行緒下能用**。

### 4.2 三種指定 client 的方式

`_getDefaultClient`（`hf3fs/__init__.py:30-45`）定義了優先序：

```python
def _getDefaultClient(kwargs):
    if 'client' in kwargs and kwargs['client'] is not None:
        client = kwargs['client']; del kwargs['client']            # ① 顯式傳入
    elif 'mount_name' in kwargs and kwargs['mount_name'] is not None:
        mount_name = kwargs['mount_name']
        if mount_name not in DEFAULT_CLIENT.clients:
            setupDefaultClient(mount_name)                          # ② 具名掛載點，延遲建立
        client = DEFAULT_CLIENT.clients[mount_name]
        del kwargs['mount_name']
    elif DEFAULT_CLIENT.client is None:
        raise RuntimeError("default client not setup")
    else:
        client = DEFAULT_CLIENT.client                              # ③ 當前預設
    return client, kwargs
```

注意它**從 kwargs 裡把 `client` / `mount_name` 刪掉**再往下傳，所以底層的 `Client` 方法不會看到這兩個多餘的參數。這是一種相當克制的做法——沒有用 decorator 魔法改簽章，只是移交 kwargs。

### 4.3 動態生成 23 個模組級函式

`hf3fs/__init__.py:47-59`：

```python
def _setupH3Method(name):
    @functools.wraps(getattr(Client, name))
    def wrapper(*args, **kwargs):
        nonlocal name
        client, kwargs = _getDefaultClient(kwargs)
        return getattr(client, name)(*args, **kwargs)
    globals()[name] = wrapper

for _name in [...23 個名字...]:
    _setupH3Method(_name)
```

`globals()[name] = wrapper` 直接往模組命名空間塞函式。目的是讓使用者能寫 `hf3fs.stat(path)`，語感上模仿標準庫的 `os.stat(path)`——**把「不用管 client 物件」做到極致**。`functools.wraps(getattr(Client, name))` 則讓 `help(hf3fs.stat)` 能顯示 C 層寫的中文 docstring。

代價是靜態分析工具（IDE、mypy、linter）完全看不到這 23 個名字。

### 4.4 三種 client 作用域管理

```python
def setMountInfo(mount_name, token, as_super=False):    # :61  只登記，不建立
def setupDefaultClient(mount_name):                      # :65  建立並放進 thread-local 快取
@contextmanager
def defaultClient(mount_name, token, as_super=False):    # :76  作用域內切換，離開還原
```

`defaultClient` 這個 context manager 用了正確的「保存 → 覆寫 → finally 還原」模式（`hf3fs/__init__.py:79-85`），支援巢狀：

```python
lastClient = DEFAULT_CLIENT.client
DEFAULT_CLIENT.client = Client(mount_name, token, as_super=as_super)
try:
    yield DEFAULT_CLIENT.client
finally:
    DEFAULT_CLIENT.client = lastClient
```

注意它**不會關閉**離開作用域的 client，只是還原引用——回收交給 GC。

`as_super` 對應 `IClient::newSuperClient`（`hf3fs.h:52`），`binding.cc:145` 的中文說明是「是否创建有 super 权限的 client（需 token 有 root 权限）」。**token 是必填參數**——這是與 `hf3fs_fuse`（USRBIO）最大的架構差異：USRBIO 完全不需要 token，因為身分由核心的 `fuse_req_ctx()->uid` 決定；而 `IClient` 是直接連 meta/storage 的客戶端，必須自帶憑證。

`hf3fs/fuse.py` 的兩個函式就是為了餵給 `Client(mount_name, ...)`：

```python
def serverPath(p):                                    # /hf3fs-cluster/cpu/abc/def → /abc/def
    np = os.path.normpath(os.path.realpath(p))
    return os.path.join('/', *PosixPath(np).parts[3:])

def mountName(p):                                     # /hf3fs-cluster/cpu/abc/def → cpu
    np = os.path.normpath(os.path.realpath(p))
    return PosixPath(np).parts[2]
```

兩者共用同一個位置約定：`parts[0..1]` 是掛載前綴、`parts[2]` 是 mount name、`parts[3:]` 是伺服端路徑。這與 `hf3fs_fuse/fuse.py` 的 `parts[:3]` 是同一個假設的兩種切法。mount name 對應 FUSE 端的 `fuseMount = appInfo.clusterId`，而且被限制在 32 字元以內（`src/fuse/FuseClients.cc:57-60`）。

### 4.5 `os` 模組的對應實作

`hf3fs/__init__.py` 花了大半篇幅在複刻標準庫：

| 標準庫 | `hf3fs` 對應 | 行號 |
|---|---|---|
| `os.listdir` | `listdir(path='.')` | `:96-111` |
| `os.DirEntry` | `class DirEntry` | `:113-161` |
| `os.scandir` | `scandir(path='.', dir_fd=None)` | `:163-206` |
| `os.walk` | `walk(top, topdown, onerror, followlinks, dir_fd)` | `:232-235` |
| （無對應） | `walk2` — 多回傳一個 dir fd | `:208-230` |
| `open(..., 'rb')` | `class BinaryFile` | `:237-310` |

三個實作細節值得記下：

**(a) `DirEntry` 的 `d_type` 快取路徑。** `DirEntry` 保存了 `readdir` 回來的 `d_type`（`hf3fs/__init__.py:118`），`is_dir()` / `is_file()` 優先用它，只有在「型別不符且允許跟隨 symlink」時才退回真正的 `stat`（`:140-147`）：

```python
def _checkWFollow(self, against, follow_symlinks):
    if self._etype == against[0]:
        return True
    elif follow_symlinks and self.is_symlink():
        st = self.stat(True)
        return (st.st_mode & self._S_IFMT) == against[1]
    else:
        return False
```

`stat` 結果本身也快取（`:158-161` 的 `self._st`）。這與 CPython 的 `os.DirEntry` 策略一致，對 3FS 這種 stat 要走 RPC 的檔案系統效益更大。

**(b) `walk2` 多回傳的那個 dir fd。** 標準 `os.walk` 回傳 `(dirpath, dirnames, filenames)`；`walk2` 回傳四元組，多出目前目錄的 fd（`:223`），而 `walk` 只是把它丟掉的薄封裝（`:232-235`）。有了 dir fd，呼叫者就能用 `openat` 語意處理遍歷到的檔案，省掉每次重新解析完整路徑——這對 3FS 這種「路徑解析 = 逐層 dentry 查詢」的檔案系統是實質優化。

不過 `walk2` 有一個**遞迴時的引數錯誤**。topdown 分支（`:226-227`）遞迴傳的是完整路徑：

```python
yield from walk2(str(topp / dirname), True, onerror, followlinks, sd.dir_fd, dirname, client=client)
```

而 bottom-up 分支（`:219`）傳的是：

```python
yield from walk2(dent.path, False, onerror, followlinks, sd.dir_fd, name, client=client)
```

兩者的 `curr_dir` 都給了單段名字、`dir_fd` 都給了父目錄 fd，這部分一致；但 `scandir` 內部同時用 `self._path`（可能是完整路徑）與 `self._dir_fd`（父目錄）去 open（`:185-187`），當 `curr_dir` 有值時 `_path` 是單段名字、可以正確地相對於 `dir_fd` 解析；當 `curr_dir` 是 `None`（最外層呼叫）時 `_path` 是完整路徑而 `dir_fd` 是 `None`，也正確。所以邏輯是自洽的——只是 `yield` 出去的 `top` 與內部使用的 `_path` 分屬兩套座標，讀起來很容易誤判。

**(c) `BinaryFile` 的模式字串是自訂的。** `:242-253`：

| mode | flags |
|---|---|
| `'r'` | `O_RDONLY` |
| `'r+'` | `O_RDWR` |
| `'r+c'` | `O_RDWR \| O_CREAT` ← **標準庫沒有這個模式** |
| `'w'` | `O_WRONLY \| O_CREAT \| O_TRUNC` |
| `'w+'` | `O_RDWR \| O_CREAT \| O_TRUNC` |

沒有 `'a'`（append）、沒有 `'x'`（exclusive）、沒有文字模式。另外有一個 3FS 專屬的旗標用法（`:255-256`）：

```python
if ignore_cache:
    flags |= os.O_NONBLOCK
```

這對應 `hf3fs.h:119-120` 的註解：

```cpp
// we use O_NONBLOCK flag to indicate we want to ignore the inode cache
// if you want to read file immediately after operating on it, use this flag
```

**把 `O_NONBLOCK` 挪用成「繞過 inode 快取」**——因為分散式檔案系統上非阻塞開檔沒有意義，而 open flags 又沒有多餘的位元可用。

`BinaryFile.read(size=None)` 在不給 size 時會做三次 `lseek` 來算剩餘長度（`:288-292` 的 `_bytesLeft`）：

```python
def _bytesLeft(self):
    off = self._off
    flen = self.seek(0, os.SEEK_END)
    self.seek(off)
    return flen - off
```

`hf3fs.h:154-155` 對此有警告：「may not be very accurate if seek from end, since other clients may be writing and moving the eof when we're seeking」。

`__del__` 呼叫 `close()`（`:263-264`），同時也實作了 context manager（`:266-270`）——雙保險，但 `__del__` 在直譯器關閉時同樣不保證執行。

---

## 5. 兩套 API 的語意落差

假設 `hf3fs/` 有一天被修好，使用者仍會面對兩套差異極大的 API：

| | `hf3fs`（IClient / client agent） | `hf3fs_fuse`（USRBIO） |
|---|---|---|
| 是否需要掛載 | **不需要**（自己是客戶端） | **需要**（要有 `3fs-virt/iovs`） |
| 是否需要 token | **需要** | **不需要** |
| 身分來源 | token 內含 | 核心的 `fuse_req_ctx()->uid` |
| meta 操作 | 完整（mkdir/chown/rename/symlink…） | **無**（只能讀寫已開啟的檔案） |
| 資料路徑 | `preadv`/`pwritev`（零拷貝）或 `read`/`write`（有拷貝） | 只有零拷貝的 Iov |
| 併發模型 | 每執行緒一個 client | 每執行緒一個 ioring |
| 非同步 | 無（全部同步阻塞） | **有**（prep → submit → wait 三段） |
| 檔案物件抽象 | `BinaryFile`（有 offset 狀態） | 無，全部是 `pread`/`pwrite` 語意 |
| 錯誤 | `OSError`（帶 filename） | `OSError`（`usrbio_binding.cc:33-40`） |

兩者對 `OSError` 的處理方式相同——都用 pybind11 的 `register_exception_translator` 把自訂例外轉成帶 errno 的 `OSError`（`usrbio_binding.cc:33-40`、`binding.cc:33-40`），差別是 `binding.cc` 多帶了 filename（`PyErr_SetFromErrnoWithFilename`）。

---

## 6. 限制與陷阱彙總

| 項目 | 影響 | 出處 |
|---|---|---|
| **`import hf3fs` 直接失敗** | 整個 `hf3fs/` 套件不可用 | `hf3fs/__init__.py:1` vs `usrbio_binding.cc` 的匯出清單 |
| `get_distribution('hf3fs')` 永遠失敗 | `__version__` 恆為 `"debug"` | `hf3fs/__init__.py:11-16`；無任何 setup 產生此發行版 |
| thread-local 只在 import 執行緒可用 | 其他執行緒 `AttributeError` | `hf3fs/__init__.py:18-20`、`:40`、`:73` |
| `IorPriority` 無作用 | 傳了也被丟棄 | `hf3fs_fuse/io.py:43-46` → `usrbio_binding.cc:248` |
| `read_file` 的 `finally` 可能 `NameError` | `os.open` 失敗時掩蓋原例外 | `hf3fs_fuse/io.py:93`、`:134` |
| `read_file` 把短讀當 EOF | 尾端有洞時會提前截斷 | `hf3fs_fuse/io.py:120-121` |
| `iovec.__del__` 不保證執行 | symlink 可能殘留（有 FUSE 側保險） | `hf3fs_fuse/io.py:14-15` |
| `userdata` 未收割即洩漏 | `inc_ref` 沒有對應的 `dec_ref` | `usrbio_binding.cc:290`、`417-419` |
| 三套掛載點判定互不相同 | 行為不一致 | `io.py`（走 C 的 mountinfo）/ `hf3fs_fuse/fuse.py:7`（第 2 層）/ `hf3fs/fuse.py:44`（第 2 層） |
| `setup.py` 硬編 `clang-14` | 換編譯器要改原始碼 | `setup.py:48-49` |
| `setup.py` 需要 git | `git rev-parse` 失敗就無法打包 | `setup.py:12` |
| `pkg_resources` 已在新版 setuptools 棄用 | Python 3.12+ 會有 DeprecationWarning | `hf3fs/__init__.py:11` |
| `BinaryFile` 無 append 模式 | 需自行 `seek(0, SEEK_END)` | `hf3fs/__init__.py:242-253` |

---

## 7. 檔案索引表

### 7.1 `hf3fs_fuse/`（現行可用）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `hf3fs_fuse/__init__.py` | 0 | 空檔，僅標記 package；`setup.py:88` 唯一打包的套件 |
| `hf3fs_fuse/io.py` | 139 | USRBIO 薄封裝：`iovec`（綁 symlink 生命週期）、`ioring`（型別轉接）、`IorPriority`（無作用的常數）、`make_iovec`（建 shm symlink 完成註冊）、`make_ioring`、`read_file`（唯一的高階便利函式，支援 `cb` 回呼改寫偏移） |
| `hf3fs_fuse/fuse.py` | 7 | `get_mount_point(p)`：以「掛載點在第 2 層」的位置約定取掛載點；套件內無呼叫者 |
| `hf3fs_fuse/fuse_demo.py` | 30 | 官方最小範例：SharedMemory → make_iovec → make_ioring → register_fd → prepare×2 → submit().wait() → 拆解 |

### 7.2 `hf3fs/`（已失效的舊套件）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `hf3fs/__init__.py` | 310 | client agent 高階 API：thread-local client 快取（`DEFAULT_CLIENT`）、`MountInfo`/`MOUNT_INFO` 的 token 登記、`_setupH3Method` 動態生成 23 個模組級函式、`defaultClient` context manager、`withClient` decorator、`listdir`/`scandir`/`DirEntry`/`walk`/`walk2`、`BinaryFile`。**第 1 行 import 即失敗** |
| `hf3fs/fuse.py` | 44 | `serverPath(p)`（完整路徑 → 伺服端路徑）、`mountName(p)`（完整路徑 → mount name）、四個硬編的 ioctl 命令碼常數。第 4 行 import `HF3FS_SUPER_MAGIC` 亦失敗 |

### 7.3 打包與 C 擴充

| 檔案 | 職責 |
|---|---|
| `setup.py` | pip 套件 `hf3fs_py_usrbio`：`CMakeBuild` 只建 `hf3fs_py_usrbio` target（`:79`）、版本 = `1.2.9+<git short rev>`（`:12-13`）、硬編 clang-14（`:48-49`）、`packages=['hf3fs_fuse']`（`:88`，**刻意不含 `hf3fs`**） |
| `src/lib/py/CMakeLists.txt` | `pybind11_add_module(hf3fs_py_usrbio usrbio_binding.cc)` — 只編 usrbio_binding.cc |
| `src/lib/py/usrbio_binding.cc` | **有建置**：`iovec` / `ioring` / `register_fd` / `deregister_fd` / `extract_mount_point` / `force_fsync` / `hardlink` / `punch_hole`（詳見 USRBIO 報告 §11.1） |
| `src/lib/py/binding.cc` | **未建置**：`Client`（`IClient` 綁定，`:127-138`）、`dirent` / `stat_result` / `iovec`、`HF3FS_SUPER_MAGIC`（`:42`）。`#include "lib/api/Client.h"` 指向不存在的檔案 |
| `src/lib/api/hf3fs.h` | `IClient` 抽象介面（`:49-173`），`hf3fs/` 所有方法的型別來源；全樹無實作 |
| `src/lib/common/paths.h` | `varTmpPath = "/var/tmp/hf3fs_client_agent"`，全樹無使用點——client agent 設計的化石 |
| `src/lib/common/PerProcTable.h` | `PerProcTable` / `AllProcMap`，fork 感知的 fd 表，同屬 client agent 路徑 |
