# macOS 上的 clangd 設定

讓 VS Code（clangd 擴充）在 macOS 上能對 3FS 做跳轉、找引用、補全。

3FS 只能在 Linux 上編譯 —— 它直接用 io_uring、RDMA verbs、fuse3、FoundationDB、
以及一堆 glibc 專屬的東西。所以這裡不跑 cmake，而是**合成**一份
`compile_commands.json`：照 `CMakeLists.txt` 的 include 路徑與 `-D` 定義，替每個
`.cc` 湊出一條編譯命令，再把 macOS 上缺的 header 補進來。

目的只有一個：讓 clangd 建得出索引。**這裡的東西不能拿來真的編譯。**

## 重建

```bash
./.clangd-macos/bootstrap.sh [linux-host]   # 預設 10.0.0.10
```

會做四件事：

1. 從 Linux 主機 rsync 真正的 uapi header（`linux/`、`asm/`、`infiniband/`、
   `rdma/`、`libaio.h`、`numa.h`）—— 抓真的比手寫 stub 準確。
2. 從上游抓 libfuse 3.16.2 與 FoundationDB 7.1 的 header（版本對齊 dockerfile 與
   CMakeLists），並用 `fdb.options` 生出建置時才會有的 `fdb_c_options.g.h`。
3. 從 pyarrow 的 wheel 取出 Arrow/Parquet 的 C++ header（`src/analytics` 要用），
   省下裝 `apache-arrow` 那 29 個依賴。
4. 把 `stub/` 蓋上去，然後產生 `compile_commands.json`。

只改了原始碼、想重新產生 CDB 的話，跑 `gen_compile_commands.py` 就好。

## 檔案

| 路徑 | 說明 |
| --- | --- |
| `bootstrap.sh` | 一鍵準備 `include/` |
| `gen_compile_commands.py` | 產生 `compile_commands.json`（可單獨重跑） |
| `gen_fdb_options.py` | 由 `fdb.options` 生成 `fdb_c_options.g.h` |
| `stub/` | 手寫的補丁 header，**這部分值得進版控** |
| `include/` | bootstrap 的產物，體積大，已被 `.git/info/exclude` 排除 |

`stub/` 裡是「建置時才生成」或「macOS 根本沒有」的東西：liburing 的 `compat.h`／
`io_uring_version.h`、libfuse 的 `libfuse_config.h`、`sys/epoll.h`、`sys/timerfd.h`、
`sys/eventfd.h`、`sys/vfs.h`、glibc 的 `bits/*`、libc++ 已移除的 `<experimental/array>`，
以及 `macos_compat.h`（用 `-include` 強制引入，補 `struct ucred`、`cpu_set_t`、
`O_DIRECT`、`st_atim` 這類零星差異）。

## 幾個刻意的取捨

**folly 用 Homebrew 版，不用 `third_party/folly`。** submodule 那份依賴 libstdc++
的內部符號（`_Sp_counted_base`）和 libc++ 已經移除的 `char_traits<unsigned char>`，
在 macOS 上每個檔案會多噴 60 幾個錯誤。改用 Homebrew 的 folly 之後幾乎清空；新版
folly 沒有的那幾個 header（`folly/experimental/*`），`gen_compile_commands.py` 會自動
產生轉發檔指回 submodule。代價是跳進 folly 內部時看到的是比較新的版本。

**boost 只覆蓋 `uuid/`。** Homebrew 的 boost 已經是 1.90，而 1.86 改掉了 uuid 的
介面，`src/common/utils/Uuid.h` 在新版下編不過。從 Linux 主機抓 1.83 的 `boost/uuid/`
（約 150KB）蓋掉這一小塊，其餘 boost 仍走 Homebrew 版，實測沒有衝突。

**`.clangd` 改過一行。** 原本寫 `Add: -fcoroutines-ts`，但 clang 16 之後已經移除這個
旗標，新版 clangd 會直接以 driver error 收場，整個檔案都不會解析。改成把它從命令列
移除。

## 還會剩下的紅線

這些是 macOS 與 Linux 的本質差異，修不掉，不影響跳轉：

- `Recorder.h` 的 `folly::ThreadLocal` —— 新版 folly 的建構子要求回傳值而非指標。
  試過轉發 submodule 版，錯誤反而從 4 個變成 20 個。
- `FairSharedMutex.h` 的 `native_handle` —— libstdc++ 的 `shared_mutex` 有，libc++ 沒有。
- `Shuffle.h` 的 `#error "not libstdc++"` —— 專案硬性要求 libstdc++。試過用
  `-D__GLIBCXX__` 假裝，整體錯誤從 48 暴增到 1008，不划算。
- serde 的 `dependent_false` static assert —— Linux x86-64 的 `uint64_t` 是
  `unsigned long`（等同 `size_t`），macOS arm64 是 `unsigned long long`，所以
  `size_t` 沒被特化覆蓋到。同源的還有 `off_t` 的 `auto` 推導不一致。
