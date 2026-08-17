# 3FS admin_cli（叢集管理命令列工具）深度剖析

> 對應原始碼：`src/client/bin/admin_cli.cc`（進入點）、`src/client/cli/common/`（分派框架）、`src/client/cli/admin/`（72 個命令）
> 姊妹 binary：`src/tools/admin.cc` + `src/tools/commands/`（`hf3fs-admin`）
> 主要下游：`src/client/mgmtd/`（MgmtdClientForAdmin）、`src/client/meta/`、`src/client/storage/`、`src/core/user/`、`src/fdb/`

---

## 0. 一句話總結

`admin_cli` 不是「一個 RPC 客戶端」，而是**把 3FS 全部五種後端存取路徑（mgmtd RPC、meta RPC、storage RPC、core RPC、直連 FoundationDB）縫在同一個 `AdminEnv` 裡的萬用工具**——它靠一個 `std::map<name, HandlerInfo>` 的極簡分派器、七個 lazy 初始化的 client getter，以及 argparse + linenoise 這兩個 header-only 依賴，把 72 個命令壓縮成「每個命令一個 `.cc` 檔、每檔一個 `getParser()` + 一個 `handle()`」的模板；代價是**幾乎沒有任何危險操作保護**，破壞性與唯讀命令走完全相同的程式路徑。

---

## 1. 兩個 binary 的定位與分工

3FS 編出兩個管理用 binary，名字很像但完全不是一回事。

| | `admin_cli` | `hf3fs-admin` |
|---|---|---|
| 進入點 | `src/client/bin/admin_cli.cc` | `src/tools/admin.cc` |
| CMake | `target_add_bin(admin_cli "admin_cli.cc" admin-cli)`（`src/client/bin/CMakeLists.txt:1`） | `target_add_bin(hf3fs-admin "admin.cc" admin-commands)`（`src/tools/CMakeLists.txt:1`） |
| 連結的 lib | `admin-cli`：mgmtd + meta + common-cli + 四個 client + kv + arrow + chunk_engine（`src/client/cli/admin/CMakeLists.txt:1`） | `admin-commands`：只有 `meta-client mgmtd-client`（`src/tools/commands/CMakeLists.txt:1`） |
| 命令數 | 72 | 2 |
| 命令選擇方式 | 位置參數（`admin_cli list-nodes`）或互動式 shell | gflags 布林開關（`--set_dir_layout` / `--create_with_layout`） |
| 存取 FDB | 會（`env.kvEngineGetter()`） | 不會 |
| 存取 storage | 會 | 不會（`MetaClient` 的 storageClient 傳 `nullptr`，`src/tools/admin.cc:79`） |
| 使用者身分來源 | `--user_info.token` 設定項 + `geteuid()` | `~/.hf3fs/admin_token.toml`（`src/tools/admin.cc:85`） |
| 設定檔預設 | 無（純 flags） | `~/.hf3fs/admin.toml`（`src/tools/admin.cc:47-48`） |

### 1.1 從程式碼看得出的分工

`hf3fs-admin` 的 `main()` 只有 99 行，末尾是一個雙分支：

```cpp
// src/tools/admin.cc:94-98
if (FLAGS_set_dir_layout) {
  setDirLayout(*metaClient, ui);
} else if (FLAGS_create_with_layout) {
  createWithLayout(*metaClient, ui);
}
```

兩個功能（`src/tools/commands/SetDirLayout.cc:9`、`src/tools/commands/CreateWithLayout.cc:11`）在 `admin_cli` 裡都有等價命令（`set-layout` 與 `create --chain-table-id ...`），而且 `admin_cli` 的版本功能更強（`set-layout` 會先 `stat` 確認是目錄、能以既有 layout 為 base 做增量修改，見 `src/client/cli/admin/SetLayout.cc:27` 與 `src/client/cli/admin/Layout.h:36-40`；`hf3fs-admin` 的版本直接 `XLOGF_IF(FATAL, ...)` 掛掉）。

三個關鍵證據指向「`hf3fs-admin` 是給終端使用者、`admin_cli` 是給營運人員」：

1. **路徑基準點不同**。`hf3fs-admin` 一律從 root 出發：`metaClient.setLayout(ui, meta::InodeId::root(), Path(FLAGS_dir_path), layout)`（`src/tools/commands/SetDirLayout.cc:11`）；`admin_cli` 有 `env.currentDirId` 的 shell 狀態，支援 `cd`。
2. **身分預設不同**。`hf3fs-admin` 預設用 `getuid()`，要當 root 得顯式加 `--as_super`（`src/tools/admin.cc:87-90`）；`admin_cli` 的 `AdminEnv` 預設 `flat::UserInfo{Uid{0}, Gid{0}, ""}`（`src/client/cli/admin/AdminEnv.h:13`），只是 `main()` 會用 `geteuid()` 覆蓋（`src/client/bin/admin_cli.cc:65`）。
3. **日誌預設不同**。`hf3fs-admin` 把日誌設成 `CRITICAL:out:err`，直接吐到 stdout/stderr（`src/tools/admin.cc:41-43`）；`admin_cli` 預設 `DBG:normal; normal=file:path=cli.log`（`src/client/bin/admin_cli.cc:47`），寫檔案，因為它的正常輸出是表格。

`hf3fs-admin` 在 codebase 裡沒有被其他地方引用，兩個命令與 `admin_cli` 功能重疊，實質上是**歷史遺留的簡化版**。本報告其餘部分聚焦在 `admin_cli`。

---

## 2. 啟動流程與整體架構圖

### 2.1 分層

```
┌──────────────────────────────────────────────────────────────────┐
│ src/client/bin/admin_cli.cc                                      │
│   Config（ConfigBase）: client / ib_devices / fdb / mgmtd_client  │
│                        / meta_client / storage_client / monitor  │
│   組裝 AdminEnv（7 個 lazy getter）                               │
│   registerAdminCommands(dispatcher)  →  dispatcher.run(...)      │
├──────────────────────────────────────────────────────────────────┤
│ src/client/cli/common/                                           │
│   Dispatcher   name → HandlerInfo{usage, help, parserGetter, fn} │
│   ArgsParser   一行字串 → argv（處理引號與反斜線）                 │
│   Printer      OutputTable（vector<vector<String>>）→ 對齊文字     │
│   IEnv         空基底類別，靠 dynamic_cast 取回 AdminEnv          │
├──────────────────────────────────────────────────────────────────┤
│ src/client/cli/admin/  ×64 個 .cc（72 個命令）                    │
│   每檔：static getParser() + static handle() + registerXxxHandler │
│   共用：AdminEnv.h / Utils.{h,cc} / Layout.h / FileWrapper.{h,cc} │
├──────────────────────────────────────────────────────────────────┤
│ 五條下游路徑（全在 AdminEnv 裡）                                   │
│   mgmtdClientGetter  → MgmtdClientForAdmin → mgmtd RPC           │
│   metaClientGetter   → MetaClient          → meta RPC            │
│   storageClientGetter→ StorageClient       → storage RPC (RDMA)  │
│   coreClientGetter   → CoreClient          → 任意節點的 Core 服務  │
│   kvEngineGetter     → FDBKVEngine         → 直連 FoundationDB    │
│   clientGetter       → net::Client         → 手搓 serde stub      │
└──────────────────────────────────────────────────────────────────┘
```

### 2.2 啟動時序

```
main()
 │
 ├─ config.init(&argc, &argv)         ← ConfigBase 吃掉所有 --xxx flags
 ├─ FLAGS_release_version ? 印版本退出
 ├─ logging::initOrDie(config.log())
 ├─ SysResource::increaseProcessFDLimit(524288)
 ├─ Monitor::start(config.monitor())
 │
 ├─ 建 AdminEnv，掛上 7 個 lambda（此時什麼連線都還沒建）
 │
 ├─ cmd = argc > 1 ? join(argv[1..], " ") : ""
 │
 └─ blockingWait([]{
      registerAdminCommands(dispatcher)   ← 65 次 registerHandler
      dispatcher.run(env, promptGetter, cmd, verbose, profile, breakOnFailure)
    })
      │
      ├─ cmd 為空  → 互動式：linenoise 迴圈
      └─ cmd 非空  → 一次性：以 ';' 切成多行依序執行
 │
 ├─ mgmtdClient->stop()（若曾建立）
 ├─ client->stopAndJoin()（若曾建立）
 └─ return result.hasError()            ← exit code 只有 0/1
```

**關鍵設計：全部 lazy**。`admin_cli` 支援的命令橫跨「只需要 FDB」（`init-cluster`、`user-add`）、「只需要 mgmtd」（`list-nodes`）、「需要 mgmtd + meta + storage」（`read-file`）、「什麼都不需要」（`decode-user-token`）。若在 `main()` 一開始就把五種 client 都建起來，`decode-user-token` 這種純本機計算的命令也得等 RDMA 初始化和 mgmtd 握手。

解法是把每個 getter 寫成「內含 `static bool inited` 的 lambda」：

```cpp
// src/client/bin/admin_cli.cc:126-139
auto ensureMgmtdClient = [&] {
  [[maybe_unused]] static bool inited = [&] {
    ensureClient();
    mgmtdClient = std::make_shared<MgmtdClientForAdmin>(...);
    folly::coro::blockingWait(mgmtdClient->start(&client->tpg().bgThreadPool().randomPick()));
    folly::coro::blockingWait(mgmtdClient->refreshRoutingInfo(/*force=*/false));
    return true;
  }();
};
```

`static` 局部變數的初始化在 C++11 之後是執行緒安全且只跑一次的，這裡直接拿它當「一次性初始化」的語言級鎖。缺點是這個 `static` 綁在 lambda 的閉包型別上、`&` 捕獲了 `main()` 的局部變數——**在單次 process 內是對的，但這段程式碼永遠不能被搬進函式庫重複呼叫**。

同樣的技巧也用在 IB 裝置初始化上（`src/client/bin/admin_cli.cc:104-110`），且 `ensureIbInited()` 被 `ensureClient()` 呼叫，形成一條 `metaClient → mgmtdClient → netClient → IBManager` 的隱式依賴鏈。

---

## 3. 命令註冊與分派機制

### 3.1 註冊：手寫的線性清單，不是巨集也不是自動註冊

很多 CLI 專案會用「靜態物件建構期自動註冊」的技巧（宣告一個全域 `Registrar` 物件，建構子把自己塞進全域表）。3FS **刻意不用**——`src/client/cli/admin/registerAdminCommands.cc:71-138` 是一份 65 行的手寫清單：

```cpp
CoTryTask<void> registerAdminCommands(Dispatcher &dispatcher) {
  CO_RETURN_ON_ERROR(co_await registerAdminUserCtrlHandler(dispatcher));
  CO_RETURN_ON_ERROR(co_await registerInitClusterHandler(dispatcher));
  CO_RETURN_ON_ERROR(co_await registerUploadChainTableHandler(dispatcher));
  ...
  CO_RETURN_ON_ERROR(co_await registerRecursiveChownHandler(dispatcher));
  co_return Void{};
}
```

搭配上方 63 行的 `#include`（`registerAdminCommands.cc:3-67`）。這個選擇的三個後果：

1. **註冊失敗會終止整個程式**。`registerHandler` 回傳 `CoTryTask<void>`，重複命令名會回 `kInvalidArg`（`Dispatcher.cc:115`），`CO_RETURN_ON_ERROR` 讓它一路冒到 `main()` 印出 `Register commands failed`（`admin_cli.cc:217`）。也就是說**命名衝突在啟動時就炸，不會靜默覆蓋**。
2. **靜態連結不會被 linker 剪掉**。自動註冊最惡名昭彰的坑就是 `--gc-sections` 或靜態函式庫把「沒人引用的 translation unit」整個丟掉。明確的 `#include` + 明確的呼叫消除了這個問題。
3. **順序無關緊要但可讀性差**。因為底層是 `std::map`（有序），`help` 的輸出是按命令名字典序，不是註冊順序。這 65 行的順序純粹是歷史增長的記錄。

### 3.2 兩種註冊介面

`Dispatcher` 提供三個 `registerHandler` 多載：

```cpp
// src/client/cli/common/Dispatcher.h:22-32
CoTryTask<void> registerHandler(String usage, String help,
                                ParserGetter parserGetter, Handler handler, bool replace = false);
CoTryTask<void> registerHandler(ParserGetter parserGetter, Handler handler, bool replace = false);

template <typename Handler>
CoTryTask<void> registerHandler() {
  co_return co_await registerHandler(&Handler::getParser, &Handler::handle);
}
```

三者的分工：

- **兩參數版（最常用，60+ 個命令）**：`usage` 與 `help` 從 parser 自動生成（`Dispatcher.cc:78-83`），呼叫 `parser.usage()` 與 `parser.help().str()`。
- **四參數版**：只有 `ls`（`List.cc:67-68`）與 `gc-list`（`ListGc.cc:58-59`）用。原因是這兩個命令的路徑是**可選的位置參數且經由 `unknownArgs` 傳遞**（見 §4.3），argparse 生成的 usage 不會顯示 `[path]`，所以要手寫：
  ```cpp
  constexpr auto usage = "Usage: ls [path] [-l limit] [-s]";
  co_return co_await dispatcher.registerHandler(usage, usage, getParser, handleList);
  ```
- **模板版**：只有 `AdminUserCtrl.cc` 用（`registerAdminUserCtrlHandler`，402 行、8 個命令），把每個命令包成一個 `struct` 帶兩個 `static` 成員：
  ```cpp
  // src/client/cli/admin/AdminUserCtrl.cc:65-75
  struct UserAddHandler {
    static auto getParser() { argparse::ArgumentParser cmd("user-add"); ... return cmd; }
    static CoTryTask<Dispatcher::OutputTable> handle(IEnv &, const argparse::ArgumentParser &, const Dispatcher::Args &);
  };
  ```
  這是唯一一個「一個檔案裝多個命令」的情況，因為八個 user 命令共用 `ensureAdmin()`（`AdminUserCtrl.cc:27`）、`printUserAttr()`（`:50`）、`printUserInfo()`（`:333`）三個 helper。

### 3.3 分派表本體

```cpp
// src/client/cli/common/Dispatcher.h:49
std::map<String, std::unique_ptr<HandlerInfo>> handlers_;
```

```cpp
// src/client/cli/common/Dispatcher.cc:57-68
struct Dispatcher::HandlerInfo {
  String usage;
  String help;
  ParserGetter parserGetter;   // std::function<argparse::ArgumentParser()>
  Handler handler;             // std::function<CoTryTask<OutputTable>(IEnv&, const ArgumentParser&, const Args&)>
};
```

注意 **`parserGetter` 存的是工廠函式而不是 parser 物件**。原因在 `run()`：

```cpp
// src/client/cli/common/Dispatcher.cc:144-150
auto parser = info.parserGetter();          // ← 每次執行都重新建一個
Args unknownArgs;
try {
  unknownArgs = parser.parse_known_args(args);
} catch (const std::exception &e) {
  co_return wrongUsage(info.usage, fmt::format("parse failed: {}", e.what()));
}
```

`argparse::ArgumentParser` 是**有狀態的**——`parse_known_args` 會把解析結果寫進 `Argument::m_values` / `m_is_used`。在互動式 shell 裡同一個命令會被執行很多次，如果重用同一個 parser 物件，第二次的 `parser.present<T>("-x")` 會讀到第一次的值。存工廠函式讓「每次執行拿到全新 parser」這件事變成結構性保證，代價是每次執行都要重建整個 parser（包含所有 `add_argument` 的 `std::string` 配置）——對互動式工具來說完全不是瓶頸。

### 3.4 分派流程

```
run(env, args)
 │
 ├─ args.empty() → kInvalidArg "empty args"
 │
 ├─ args[0] == "help" ──→ handleHelp(args.size() > 1 ? args[1] : "")
 │                          ├─ 空 → 遍歷 handlers_，每列 {name, usage}
 │                          └─ 有名字 → findMethod → 印 help（完整說明）
 │
 ├─ findMethod(args[0])
 │    └─ handlers_.find(name)；找不到 → kInvalidArg "Unknown method name: {}"
 │
 ├─ parser = info.parserGetter()
 ├─ unknownArgs = parser.parse_known_args(args)   ← 丟例外 → kWrongUsage + usage
 │
 └─ info.handler(env, parser, unknownArgs)
      catch (StatusException &e):
        e.code == kWrongUsage → wrongUsage(info.usage, e.message())
        否則                   → makeError(e.get())
```

`help` 是**唯一一個沒有註冊在 `handlers_` 裡的命令**（硬編碼在 `Dispatcher::run` 的 `if (methodName == "help")`，`Dispatcher.cc:136`）。這意味著 `help` 不能被覆寫，也不會出現在 `help` 自己列出的清單裡。

---

## 4. 參數解析與互動式 shell

### 4.1 三層解析

`admin_cli` 的參數解析分三層，各由不同東西負責：

```
第 0 層  gflags / ConfigBase        ── main() 之前，吃掉 --cluster_id / --user_info.token / --log ...
   ↓     （config.init(&argc, &argv) 會就地改寫 argc/argv，只留下非 flag 參數）
第 1 層  ArgsParser                 ── 把「一整行字串」切成 vector<String>（處理引號/跳脫）
   ↓     （只在互動式 shell 與 ';' 切分後使用）
第 2 層  argparse::ArgumentParser   ── vector<String> → 具名參數
```

第 0 層的存在造成一個實務上的坑：`admin_cli --cluster_id foo list-nodes` 可行，但 `admin_cli list-nodes --cluster_id foo` 也可行（gflags 會掃描整個 argv），而**互動式 shell 裡輸入的 `--cluster_id` 不會生效**，因為那層在 `main()` 開頭就結束了。

### 4.2 第 1 層：`ArgsParser`（手寫 66 行）

`src/client/cli/common/Parser.cc` 是一個字元級狀態機，狀態只有三個布林/字元：`prevQuoted`、`escaped`、以及隱含的「當前是否在 token 內」。

```cpp
// src/client/cli/common/Parser.cc:23-46（節錄）
for (;;) {
  auto c = *start;
  if (prevQuoted) {
    if (c == prevQuoted) prevQuoted = '\0';
    else res.push_back(c);
  } else if (escaped) {
    escaped = false; res.push_back(c);
  } else if (c == '\\') {
    escaped = true;
  } else if (c == '\'' || c == '"') {
    prevQuoted = c;
  } else if (std::isspace(c)) {
    break;
  } else {
    res.push_back(c);
  }
  if (++start == s_.end()) break;
}
if (prevQuoted || escaped) {
  return makeError(StatusCode::kInvalidArg, fmt::format("leave with {}", prevQuoted ? "quoted" : "escaped"));
}
```

與 POSIX shell 的差異值得注意：

- **單引號內的反斜線也會被當跳脫**——不對，實際上單引號內走的是 `prevQuoted` 分支，反斜線會被原樣 `push_back`。這點跟 shell 一致。
- **雙引號內的反斜線同樣不跳脫**（也走 `prevQuoted` 分支），這點跟 shell **不一致**：shell 裡 `"a\"b"` 是 `a"b`，這裡是 `a\`（遇到第二個 `"` 就結束引號了）。
- **沒有變數展開、沒有 glob、沒有管線、沒有重導向**。`;` 的切分不在這一層做，而在 `Dispatcher::run` 用最樸素的 `cmd.find_first_of(';')`（`Dispatcher.cc:260`）——這代表 **`;` 不能被引號保護**，`create "a;b"` 會被切成兩條命令。

### 4.3 第 2 層：argparse（vendored）

`src/common/utils/ArgParse.h` 是 p-ranav/argparse 的完整內嵌版本（MIT 授權，檔頭在 `:1-30`）。用到的 API：

| API | 用途 | 例子 |
|---|---|---|
| `add_argument("name")` | 位置參數 | `parser.add_argument("path")` |
| `add_argument("-x", "--xxx")` | 選項（多別名） | `parser.add_argument("-t", "--table-id")` |
| `.scan<'u', uint32_t>()` | 型別+進位解析（`u`=unsigned、`i`=有號、`o`=八進位、`x`=十六進位） | `SetPermission.cc:16` 用 `'o'` 直接吃八進位權限 |
| `.default_value(v).implicit_value(true)` | 布林旗標慣用組合 | 幾乎每個 `--force` |
| `.required()` | 必填 | `CreateTarget.cc:16-19` 四個都必填 |
| `.remaining()` | 吞掉剩餘全部 | `SetNodeTags.cc:36` 的 `tags` |
| `.nargs(nargs_pattern::any / at_least_one)` | 變長列表 | `DumpChunkMeta.cc:20` 的 `--chain-ids` |
| `.action(fn)` | 解析時驗證 | `SetNodeTags.cc:32-35` 檢查 mode 合法 |
| `.help(...)` / `.add_description(...)` | 說明文字 | 多處用 `magic_enum::enum_names` 動態生成選項清單 |
| `parse_known_args(args)` | 解析並**回傳未識別的參數** | `Dispatcher.cc:147` |
| `usage()` / `help()` / `program_name()` | 生成說明、取命令名 | `Dispatcher.cc:78-83, 105` |

**`parse_known_args` 而非 `parse_args` 是關鍵設計。** 它讓未識別的參數不會報錯，而是回傳給 handler。絕大多數命令的第一行是：

```cpp
ENSURE_USAGE(args.empty());
```

也就是「我不接受任何多餘參數」。但 `ls` 與 `gc-list` 例外：

```cpp
// src/client/cli/admin/List.cc:26-28
ENSURE_USAGE(args.size() <= 1);
auto path = args.empty() ? "." : args[0];
```

為什麼不用 `add_argument("path").default_value(".")`？因為 argparse 的位置參數若有預設值仍會參與位置匹配，`ls -l 10` 會把 `10` 當成 `path`。用 `unknownArgs` 收尾等於把「可選的尾隨位置參數」這件事完全繞過 argparse 的位置匹配邏輯。**代價是 `ls` 的 usage 必須手寫**（§3.2）。

`ENSURE_USAGE` 本身是三行巨集（`src/client/cli/common/Utils.h:5-9`）：

```cpp
#define ENSURE_USAGE(condition, ...)                                                                       \
  do {                                                                                                     \
    if (!UNLIKELY(static_cast<bool>(condition)))                                                           \
      throw hf3fs::StatusException(hf3fs::Status(hf3fs::CliCode::kWrongUsage __VA_OPT__(, ) __VA_ARGS__)); \
  } while (false)
```

它**丟例外**，而不是回傳 `Result`。這是刻意的：`CoTryTask` 的 `CO_RETURN_ON_ERROR` 在深層 helper（例如 `Layout.h` 的 `parseLayout`，它不是協程）裡沒法用。`Dispatcher::run` 在最外層 catch 這個特定例外並把 usage 貼上去（`Dispatcher.cc:154-157`）。

一個廣泛使用的慣用法是「互斥選項計數」：

```cpp
// src/client/cli/admin/GetConfig.cc:87-88
ENSURE_USAGE(nodeId.has_value() + clientId.has_value() + type.has_value() + addr.has_value() + listVersions == 1,
             "must and can only specify one of -n, -c, -t, -a, and -l");
```

`std::optional::has_value()` 回傳 `bool`，直接相加成 `int`。這個模式在 `GetConfig`、`HotUpdateConfig`、`RenderConfig`、`GetLastConfigUpdateRecord`、`RemoteCall`、`SetNodeTags`、`UserSetToken` 至少 7 個地方重複出現，是整份 CLI 最常見的參數驗證形態。

### 4.4 互動式 shell：linenoise

`Dispatcher::run` 的長版（`Dispatcher.cc:218-295`）在 `cmd` 為空時進入 REPL：

```cpp
// src/client/cli/common/Dispatcher.cc:238-257
if (cmd.empty()) {
  for (;;) {
    auto input = linenoise(fmt::format("{} > ", promptGetter()).c_str());
    if (!input) break;                    // Ctrl-D / EOF
    linenoiseHistoryAdd(input);
    line = input;
    linenoiseFree(input);
    ...
    auto res = co_await runLine(env, *this, line);
    print(printer, res);
  }
}
```

Prompt 是 `env.currentDir`（`admin_cli.cc:222` 傳的 `[&env] { return env.currentDir; }`），所以 `cd` 之後 prompt 會變成 `/foo/bar > `。

linenoise 是 antirez 的 ~1000 行 readline 替代品（`src/common/utils/Linenoise.h` 是它的標頭）。3FS 用了它的三個回呼：

```cpp
// src/client/cli/common/Dispatcher.cc:193-216
static std::map<String, String, std::less<>> methodToUsages;   // ← 全域，因為 linenoise 回呼是 C 函式指標

static void completion(const char *buf, linenoiseCompletions *lc) {
  for (const auto &[k, _] : methodToUsages)
    if (k.starts_with(buf)) linenoiseAddCompletion(lc, k.c_str());
}

static char *usageHint(const char *buf, int *color, int *bold) {
  auto sv = std::string_view{buf};
  if (auto it = methodToUsages.find(sv); it != methodToUsages.end()) {
    *bold = 1;
    auto usage = std::string_view{it->second}.substr(7 /*"Usage: "*/ + sv.size());
    auto *hint = new char[usage.size() + 1];
    ...
    return hint;
  }
  return nullptr;
}
```

三個細節：

1. **`methodToUsages` 是檔案級全域變數**，在 `run()` 開頭用 `getUsages()` 填、在 `SCOPE_EXIT` 只清回呼不清資料（`Dispatcher.cc:227-236`）。這是被 linenoise 的 C API 強迫的——回呼是裸函式指標，沒有 user data 欄位。
2. **`substr(7)` 是硬編碼的魔數**，配合 argparse `usage()` 生成的 `"Usage: " + program_name`（`ArgParse.h:1323`）。任何一方改格式，hint 就會錯位或越界。
3. **`new char[]` 配 `linenoiseSetFreeHintsCallback(freeUsageHint)`**（`Dispatcher.cc:216, 230`）——正確配對了 `new[]`/`delete[]`，但 hint 的生命週期完全交給 C 函式庫管理。

**沒有做的事**：沒有 `linenoiseHistoryLoad` / `linenoiseHistorySave`，所以**歷史記錄只存活於單次 process**，離開 shell 就沒了。也沒有 `linenoiseSetMultiLine`。

### 4.5 一次性模式與 `;` 分隔

```cpp
// src/client/cli/common/Dispatcher.cc:258-293（節錄）
while (!cmd.empty()) {
  auto it = cmd.find_first_of(';');
  if (it == std::string_view::npos) {
    auto res = co_await runLine(env, *this, cmd);
    print(printer, res);
    cmd = {};
    if (res.hasError()) co_return makeError(std::move(res.error()));   // ← 最後一條失敗 → 整體失敗
  } else {
    auto line = cmd.substr(0, it);
    auto res = co_await runLine(env, *this, line);
    print(printer, res);
    cmd = cmd.substr(it + 1);
    if (breakMultiLineCommandOnFailure && res.hasError()) break;       // ← 中間失敗只在 flag 開啟時中斷
  }
}
```

這裡有一個**行為不對稱**，直接影響腳本化使用：

| 情境 | `break_multi_line_command_on_failure=false`（預設） | `=true` |
|---|---|---|
| 只有一條命令，失敗 | exit code 1 | exit code 1 |
| `A; B`，A 失敗 B 成功 | **繼續跑 B，exit code 0** | 中斷，`co_return Void{}` → **exit code 0** |
| `A; B`，A 成功 B 失敗 | exit code 1 | exit code 1 |

也就是說：**只有最後一條命令的成敗會反映到 exit code**。`break` 出去之後走的是 `co_return Void{}`（`Dispatcher.cc:294`），錯誤被吞掉了。用 `admin_cli "upload-chains x.csv; upload-chain-table 1 y.csv"` 這種寫法做自動化時，前半段失敗不會被 CI 抓到。

一次性模式**也會**設置 linenoise 回呼（`Dispatcher.cc:228-230` 在 `if (cmd.empty())` 之前），這是無害但無意義的浪費。

---

## 5. 輸出格式化層

### 5.1 `OutputTable` = `vector<vector<String>>`

```cpp
// src/client/cli/common/Dispatcher.h:12-14
using Args = std::vector<String>;
using OutputRow = std::vector<String>;
using OutputTable = std::vector<OutputRow>;
```

所有 handler 的回傳型別都是 `CoTryTask<OutputTable>`。這是整個 CLI 唯一的輸出抽象——**沒有欄位型別、沒有對齊指示、沒有 JSON 模式開關**。

### 5.2 `Printer::print`：兩趟掃描的簡易對齊

```cpp
// src/client/cli/common/Printer.cc:4-25
void Printer::print(const std::vector<std::vector<String>> &table) const {
  static constexpr std::string_view separator = "  ";
  std::vector<size_t> widths;
  for (const auto &r : table) {                      // 第一趟：算每欄最大寬
    if (widths.size() < r.size()) widths.resize(r.size(), 0);
    for (size_t col = 0; col < r.size(); ++col)
      widths[col] = std::max(widths[col], r[col].size() + separator.size());
  }
  for (const auto &r : table) {                      // 第二趟：補空白
    String s;
    for (size_t col = 0; col < r.size(); ++col) {
      s.append(r[col]);
      if (col + 1 < r.size()) s.append(widths[col] - r[col].size(), ' ');
    }
    stdout_(s);
  }
}
```

特性與後果：

- **全部左對齊**，數字也左對齊（`list-targets` 的 `UsedSize` 欄因此不易比對）。
- **欄寬跨越整張表統一**，包括表頭列。`ListChains` 靠這點讓表頭 `"Target"` 重複 N 次（`ListChains.cc:108-110`）與資料列的 target 對齊。
- **row 長度可以不一致**。這被大量利用：`stat` 的輸出是 2 欄的 key-value（`Stat.cc:151-160`），但 `--display-chunks` 時插入 4 欄的 chunk 資訊（`Stat.cc:79-93`）。混在同一張表裡，欄寬由最寬的那列決定。
- **`widths` 加了 `separator.size()`（2）但 separator 本身從未被 append**——分隔效果純粹來自 padding 多出的 2 個空白。最後一欄不補 padding（`col + 1 < r.size()` 判斷），所以不會有尾隨空白。
- **空 row（`table.push_back({})`）產生空行**。`VerifyConfig.cc:141` 用 `SCOPE_EXIT { table.push_back({}); }` 在每個節點的細節後插空行，最後再 `if (table.back().empty()) table.pop_back();`（`:153-155`）去掉多餘的尾巴。

### 5.3 沒有 JSON 模式——但有五種繞過方式

CLI 層級**沒有全域 `--json`**。實務上有五種繞道：

1. **單一命令的 `--json`**：只有 `user-list` 有（`AdminUserCtrl.cc:280, 299-315`）。它定義了一個本地的 `UserAttrLite` struct（刻意不含 token），`serde::toJsonString(users, false, true)` 後塞進 `output.push_back({...})` 當單格。
2. **直接 `std::cout`，繞過 Printer**。`query-chunk`（`QueryChunk.cc:54, 69`）、`parse-target-meta`（`ParseTargetMeta.cc:55`）、`read-file`（`ReadFile.cc:125`）、`write-file`（`WriteFile.cc:120`）、`create-targets`（`CreateTargets.cc:48, 64`）都這麼幹。因為 `Printer` 的輸出在 handler 回傳**之後**才印，這些 `cout` 會**先於**表格出現。
3. **`-o` 寫檔**：`get-config`、`remote-call`、`render-config`、`dump-chain-table`、`dump-chains`、`checksum`、`read-file`、`parse-target-meta`。
4. **Parquet**：`dump-inodes -q`、`dump-dentries`（強制 parquet）、`dump-chunkmeta -q`、`dump-session`。走 `analytics::SerdeObjectWriter`。
5. **XLOG**：`dump-inodes`、`find-orphaned-chunks`、`remove-chunks`、`recursive-chown` 的主要進度輸出全是 `XLOGF(CRITICAL, ...)`，而預設 log 設定是**寫到 `cli.log` 檔案**（`admin_cli.cc:47`）。`recursive-chown` 意識到這個問題，自己定義了雙寫巨集：
   ```cpp
   // src/client/cli/admin/RecursiveChown.cc:9-14
   #define LOG_WITH_STDOUT(level, ...)      \
     do { auto msg = fmt::format(__VA_ARGS__); XLOG(level, msg); fmt::print("{}\n", msg); } while (0)
   ```

**這是 CLI 最大的一致性缺陷**：同樣是「輸出結果」，五種機制混用，且沒有任何命令標註自己走哪一條。

### 5.4 錯誤的輸出格式

```cpp
// src/client/cli/common/Dispatcher.cc:41-51
void print(Printer &printer, const Result<Dispatcher::OutputTable> &res) {
  if (res.hasError()) {
    auto buf = fmt::format("Encounter error: {}({})", res.error().code(), StatusCode::toString(res.error().code()));
    if (!res.error().message().empty()) buf += fmt::format("\n{}", res.error().message());
    printer.print(buf);          // ← 注意：print，不是 printError
  } else if (!res.value().empty()) {
    printer.print(res.value());
  }
}
```

`Printer` 建構時明明分開傳了 stdout 與 stderr handler（`Dispatcher.cc:224-225`），但**錯誤走的是 `print()` 也就是 stdout**。`printError()` 在整個 codebase 裡沒有任何呼叫點。這意味著 `admin_cli xxx 2>/dev/null` 不會過濾掉錯誤訊息。

---

## 6. 連線、路由與認證

### 6.1 找到 mgmtd

`admin_cli` **不從設定檔讀 mgmtd 位址**——它讀的是 `MgmtdClientForAdmin::Config`（繼承 `MgmtdClient::Config`），裡面有 `mgmtd_server_addresses` 這類欄位，由 `--mgmtd_client.mgmtd_server_addresses` 之類的 flag 提供。`main()` 只覆寫四個行為開關：

```cpp
// src/client/bin/admin_cli.cc:53-58
CONFIG_OBJ(mgmtd_client, MgmtdClientForAdmin::Config, [&](auto &c) {
  c.set_enable_auto_refresh(true);
  c.set_auto_refresh_interval(1_s);
  c.set_enable_auto_heartbeat(false);
  c.set_enable_auto_extend_client_session(false);
});
```

三個關掉的東西意義明確：**admin_cli 不是叢集成員**。它不發心跳（不會出現在 `list-nodes` 裡）、不維持 client session（不會出現在 `list-clients` 裡）。但它**開啟**了 1 秒間隔的自動路由刷新——對一個通常只跑幾秒的 process 來說幾乎沒作用，真正的刷新來自命令自己的顯式呼叫。

`MgmtdClientForAdmin::Config` 的基底建構子還設了第四項：

```cpp
// src/client/mgmtd/MgmtdClientForAdmin.h:10-15
Config() {
  set_enable_auto_refresh(false);
  set_enable_auto_heartbeat(false);
  set_enable_auto_extend_client_session(false);
  set_accept_incomplete_routing_info_during_mgmtd_bootstrapping(true);
}
```

最後一項是 admin 專屬的：**mgmtd 正在 bootstrapping（尚未收齊所有節點心跳）時，一般 client 會拒絕使用不完整的 routing info，admin_cli 則接受**。這是必要的——叢集出問題時，管理員需要在 mgmtd 還沒穩定的狀態下就能查看現況。`list-clients` 因此會在輸出尾端加一列警告：

```cpp
// src/client/cli/admin/ListClients.cc:98-101
if (res->bootstrapping) {
  table.push_back({"Mgmtd Bootstrapping", "The client list may be incomplete"});
}
```

### 6.2 兩個 mgmtd getter 的差異

`AdminEnv` 有兩個 mgmtd getter，差別在 routing info 就緒檢查：

```cpp
// src/client/bin/admin_cli.cc:157-167
env.mgmtdClientGetter = [&] {
  ensureMgmtdClient();
  if (auto ri = mgmtdClient->getRoutingInfo(); !ri || !ri->raw())
    throw StatusException(Status(MgmtdClientCode::kRoutingInfoNotReady));
  return mgmtdClient;
};

env.unsafeMgmtdClientGetter = [&] {
  ensureMgmtdClient();
  return mgmtdClient;
};
```

只有兩個命令用 `unsafe` 版：

- **`refresh-routing-info`**（`RefreshRoutingInfo.cc:27`）——顯然：目的就是「現在還沒有 routing info，去拿一份」。
- **`shutdown-all-chains --check-offline`**（`ShutdownAllChains.cc:37`）——更微妙：它要**證明**叢集已經下線，所以刻意期待 `refreshRoutingInfo` 失敗（見 §9.2）。

同樣的檢查也重複在 `storageClientGetter`（`admin_cli.cc:176-178`）與 `metaClientGetter`（`:194-196`）裡，因為這兩者都依賴 routing info 決定要打哪個節點。

### 6.3 五條下游路徑

| Getter | 建立的東西 | 用於 | 典型命令 |
|---|---|---|---|
| `clientGetter` | `net::Client` | 手搓任意 serde stub | `drop-user-cache`（`DropUserCache.cc:53-55`）、`remote-call`（`RemoteCall.cc:207`） |
| `mgmtdClientGetter` | `MgmtdClientForAdmin` | 拓樸、鏈、節點、設定 | `list-nodes`、`upload-chains` |
| `metaClientGetter` | `MetaClient` | POSIX 語意 | `ls`、`stat`、`mkdir` |
| `storageClientGetter` | `StorageClient` | chunk 層 I/O 與 target 管理 | `read-file`、`create-target` |
| `coreClientGetter` | `CoreClient` | 每節點的設定熱更新/渲染 | `hot-update-config`、`get-config -n` |
| `kvEngineGetter` | `FDBKVEngine` | 繞過所有 server 直改 FDB | `init-cluster`、`user-add`、`shutdown-all-chains` |

注意 `metaClientGetter` 的初始化裡強制先建 storage client：

```cpp
// src/client/bin/admin_cli.cc:185-191
metaClient = std::make_shared<MetaClient>(clientId, config.meta_client(),
                                          std::make_unique<MetaClient::StubFactory>(...),
                                          mgmtdClient,
                                          env.storageClientGetter(),   // ← 觸發 storage client 初始化
                                          false);
```

所以任何用到 `metaClientGetter` 的命令（包括最單純的 `ls`）都會**順帶建起完整的 storage client 與 RDMA 連線**。這是 `admin_cli ls /` 啟動比預期慢的原因。對比 `hf3fs-admin` 傳的是 `nullptr`（`src/tools/admin.cc:79`）。

### 6.4 認證：三種完全不同的機制

`admin_cli` 的權限檢查**沒有統一入口**，三條路徑各查各的：

**(a) mgmtd RPC —— server 端查 token，且可被關閉**

```cpp
// src/mgmtd/service/MgmtdState.cc:54-64
CoTryTask<void> MgmtdState::validateAdmin(const core::ServiceOperation &ctx, const flat::UserInfo &userInfo) {
  if (config_.authenticate()) {                       // ← 設定開關
    auto ret = co_await userStore_.getUser(userInfo.token);
    CO_RETURN_ON_ERROR(ret);
    if (!ret->admin) CO_RETURN_AND_LOG_OP_ERR(ctx, MgmtdCode::kNotAdmin, "");
    LOG_OP_INFO(ctx, "Act as admin user {}({})", ret->name, ret->uid.toUnderType());
  }
  co_return Void{};
}
```

`SetChains`、`SetChainTable`、`SetConfig`、`RegisterNode`、`UnregisterNode`、`SetNodeTags`、`RotateLastSrv`、`UpdateChain` 等寫入操作都先呼叫它（例：`src/mgmtd/ops/SetChainTableOperation.cc:37`）。**若 mgmtd 的 `authenticate` 設為 false，任何人都能改叢集拓樸。**

**(b) FDB 直寫 —— CLI 端自己查**

繞過 mgmtd 的命令（user 管理）必須自己驗，`AdminUserCtrl.cc:27-48` 的 `ensureAdmin` 在**同一個 FDB 交易裡**完成：

```cpp
CoTryTask<void> ensureAdmin(kv::IReadOnlyTransaction &txn, core::UserStore &store,
                            std::string_view token, std::optional<flat::Uid> self = std::nullopt) {
  auto uid = core::decodeUidFromUserToken(token);
  if (UNLIKELY(uid.hasError())) co_return makeError(StatusCode::kAuthenticationFail, "Token decode failed");
  auto user = co_await store.getUser(txn, *uid);
  CO_RETURN_ON_ERROR(user);
  if (!user->has_value()) co_return makeError(StatusCode::kAuthenticationFail, fmt::format("Uid {} not found", ...));
  if (auto res = user->value().validateToken(token, UtcClock::now()); res.hasError()) {
    co_return makeError(StatusCode::kAuthenticationFail, "Token validate failed");
  }
  if (*uid != 0 && !user->value().admin && (!self || *self != *uid)) {
    co_return makeError(StatusCode::kAuthenticationFail, "Not Admin");
  }
  co_return Void{};
}
```

三個細節：`uid == 0` 無條件放行（root 後門）；`self` 參數讓 `user-stat` 可以查自己（`AdminUserCtrl.cc:261`）；token 有過期時間（`validateToken(token, UtcClock::now())`）。

**引導問題的解法**：第一個使用者無法通過 `ensureAdmin`（表是空的），所以 `user-add` 特別處理：

```cpp
// src/client/cli/admin/AdminUserCtrl.cc:95-103
auto userList = co_await store.listUsers(txn);
CO_RETURN_ON_ERROR(userList);
if (userList->empty()) {
  if (!isAdmin) co_return makeError(StatusCode::kInvalidArg, "The first user must be admin");
} else {
  CO_RETURN_ON_ERROR(co_await ensureAdmin(txn, store, env.userInfo.token));
}
```

「第一個使用者必須是 admin」——一旦叢集有了第一個 admin，這個入口就自動關閉。整個檢查在一個 FDB 交易內完成，所以兩人同時執行 `user-add` 不會產生兩個 bootstrap admin。

**(c) storage / core RPC —— 幾乎不查**

`create-target`、`offline-target`、`remove-target`、`hot-update-config` 走的是 storage/core 服務，`AdminEnv::userInfo` 通常沒有被傳過去（例：`OfflineTarget.cc:52` 的 `client->offlineTarget(nodeId, req)` 完全沒有 userInfo 參數）。**這幾個最危險的命令是最沒有認證的。**

### 6.5 使用者身分怎麼來

```cpp
// src/client/bin/admin_cli.cc:64-79
flat::UserInfo generateUserInfo(const UserConfig &cfg) {
  auto uid = cfg.uid() != -1 ? cfg.uid() : static_cast<int64_t>(geteuid());
  auto gid = cfg.gid() != -1 ? cfg.gid() : static_cast<int64_t>(getegid());
  std::vector<flat::Gid> groups;
  if (cfg.gids().empty()) {
    auto n = getgroups(0, nullptr);
    std::vector<gid_t> gs(n);
    if (getgroups(n, gs.data()) < 0) { XLOGF(ERR, "failed to get supplementary groups ..."); }
    for (auto g : gs) groups.emplace_back(g);
  } else {
    for (auto g : cfg.gids()) groups.emplace_back(g);
  }
  return flat::UserInfo(flat::Uid(uid), flat::Gid(gid), std::move(groups), cfg.token());
}
```

預設繼承 process 的 euid/egid 與**完整的補充群組列表**（`getgroups`），可用 `--user_info.uid/gid/gids/token` 覆蓋。這份 `UserInfo` 只影響 meta 層的權限檢查與 mgmtd 的 admin 驗證，**token 則完全由設定提供，沒有互動式輸入**。

`user-switch` 命令（`AdminUserCtrl.cc:343-370`）可以在 shell 內即時改身分，包括 token：

```cpp
if (u) env.userInfo.uid = flat::Uid(*u == -1 ? geteuid() : *u);
if (g) env.userInfo.gid = flat::Gid(*g == -1 ? getegid() : *g);
if (t) env.userInfo.token = *t;
```

**它不做任何驗證**——只是改本地變數，實際的檢查發生在下一次 RPC。搭配 `current-user`（`:372-388`）查看目前身分。注意 `printUserInfo` 會**明文印出 token**（`:340`），`user-list` 的非 JSON 模式也會（`:322`）。

---

## 7. 完整命令表

共 **72 個註冊命令** + 1 個內建 `help`。「後端」欄標示實際打的 RPC 或操作。

### 7.1 叢集初始化與拓樸

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `init-cluster` | 初始化根目錄 layout 並灌入各角色初始設定 | `chaintableid chunksize stripesize`，`--mgmtd/--meta/--storage/--fuse <path>`，`--allow-config-existed`，`--skip-config-check` | **直寫 FDB**：`MetaStore::initFileSystem` + `MgmtdStore::storeConfig` | `InitCluster.cc:32-181` |
| `upload-chain-table` | 上傳 chain table（CSV） | `tableId csv-file-path`，`-d/--dump-template`，`--desc` | `SetChainTableReq` | `UploadChainTable.cc:13-89` |
| `upload-chains` | 上傳 chain 定義（CSV） | `csv-file-path`，`-d`，`--set-preferred-target-order` | `SetChainsReq` | `UploadChains.cc:13-100` |
| `dump-chain-table` | 匯出 chain table 為 CSV | `tableId csv-file-path`，`-v/--version` | 本地讀 routing info | `DumpChainTable.cc:13-53` |
| `dump-chains` | 匯出全部 chain，依副本數分檔 | `csv-file-path-prefix` | 本地讀 routing info | `DumpChains.cc:13-58` |
| `list-chain-tables` | 列出所有 chain table 版本 | 無 | `refreshRoutingInfo(force)` | `ListChainTables.cc:10-52` |
| `list-chains` | 列出 chain 與健康狀態 | `-t/--table-id`，`-v/--version` | `refreshRoutingInfo(force)` | `ListChains.cc:12-163` |
| `refresh-routing-info` | 強制刷新本地 routing info | `-f/--force` | `GetRoutingInfoReq`（unsafe getter） | `RefreshRoutingInfo.cc:12-34` |
| `shutdown-all-chains` | **把所有 chain 的 target 打成 OFFLINE** | `-c/--check-offline`，`--display-chains` | **直寫 FDB**：`MgmtdStore::shutdownAllChains` | `ShutdownAllChains.cc:21-96` |

### 7.2 鏈與 target 維運

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `list-targets` | 列出 target 及角色/狀態 | `-c/--chain-id`，`--orphan` | routing info 或 `ListOrphanTargetsReq` | `ListTargets.cc:13-102` |
| `create-target` | 在指定節點磁碟建立單一 target | `--node-id --disk-index --target-id --chain-id`（皆必填），`--add-chunk-size`，`--chunk-size`，`--use-new-chunk-engine` | `CreateTargetReq`（storage） | `CreateTarget.cc:14-81` |
| `create-targets` | 依 routing info 批次重建某節點多顆磁碟的 target | `--node-id`（必填），`--disk-index`（1+），`--allow-existing-target`，`--add-chunk-size`，`--use-new-chunk-engine` | `CreateTargetReq` ×N | `CreateTargets.cc:13-77` |
| `offline-target` | 讓 target 下線 | `--target-id`（必填），`--node-id`，`--force` | `OfflineTargetReq`（storage） | `OfflineTarget.cc:14-62` |
| `remove-target` | **刪除 target（含資料）** | `--node-id --target-id`（皆必填），`--force` | `RemoveTargetReq`（storage） | `RemoveTarget.cc:14-62` |
| `update-chain` | 對單一 chain 增/刪 target | `chainId targetId`，`-m/--mode add\|remove`（必填） | `UpdateChainReq` | `UpdateChain.cc:13-49` |
| `rotate-lastsrv` | 把 LASTSRV target 輪轉到鏈尾 | `chainId` | `RotateLastSrvReq` | `RotateLastSrv.cc:12-36` |
| `rotate-as-preferred-order` | 依 preferred order 重排鏈 | `chainId` | `RotateAsPreferredOrderReq` | `RotateAsPreferredOrder.cc:12-36` |
| `set-preferred-target-order` | 設定 chain 的偏好 target 順序 | `chainId targetIds...` | `SetPreferredTargetOrderReq` | `SetPreferredTargetOrder.cc:13-41` |

### 7.3 節點與客戶端

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `list-nodes` | 列出節點及設定版本狀態 | 無 | `refreshRoutingInfo(force)` + `GetConfigVersionsReq` | `ListNodes.cc:14-91` |
| `register-node` | 註冊節點 | `nodeId type` | `RegisterNodeReq` | `RegisterNode.cc:15-39` |
| `unregister-node` | 註銷節點 | `nodeId type` | 先 `refreshRoutingInfo` 驗型別，再 `UnregisterNodeReq` | `UnregisterNode.cc:15-51` |
| `set-node-tags` | 改節點/客戶端標籤（含 Disable） | `mode`（replace/update/remove），`tags...`，`-n/--node-id` 或 `-u/--client-hostname` | `SetNodeTagsReq` 或 `SetUniversalTagsReq` | `SetNodeTags.cc:25-83` |
| `list-clients` | 列出客戶端 session | 無 | `ListClientSessionsReq` + `GetConfigVersionsReq` | `ListClients.cc:12-107` |
| `drop-user-cache` | 讓所有 meta 節點丟棄使用者快取 | `-u/--uid`，`-a/--all` | 對每個 META 節點手搓 `MetaServiceStub::dropUserCache` | `DropUserCache.cc:11-75` |

### 7.4 設定管理

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `set-config` | 上傳某角色的設定到 mgmtd | `-t/--type -f/--file`（皆必填），`--desc` | `SetConfigReq` | `SetConfig.cc:33-70` |
| `get-config` | 取設定（依 nodeId/clientId/addr/type，或列版本） | 五選一：`-n`/`-c`/`-a`/`-t`/`-l`；`-k/--config-key`，`-o/--output-file` | `GetConfigReq`（mgmtd）或 `CoreService::getConfig` | `GetConfig.cc:33-178` |
| `hot-update-config` | 熱更新設定（不重啟） | 四選一：`-n`/`-t`/`-c`/`-a`；二選一：`-s/--string` 或 `-f/--file`；`--render` | `CoreService::hotUpdateConfig` | `HotUpdateConfig.cc:18-119` |
| `render-config` | 渲染設定模板（可 mock、可測試套用） | `-f/--template-file -o/--output-file`（皆必填），`-n`/`-c`/`-a`/`--mock` 四選一，`-u/--test-update`，`--hot`，一整組 `--mock-*` | `CoreService::renderConfig` 或本地 `renderConfig()` | `RenderConfig.cc:15-182` |
| `verify-config` | 對全部同型節點併發驗證設定模板 | `-t/--node-type -f/--template-file`（皆必填），`--verbose` | 每節點 `getConfig` + 兩次 `renderConfig`（cold/hot） | `VerifyConfig.cc:17-162` |
| `get-last-config-update-record` | 查最近一次設定更新結果 | `-n`/`-c`/`-a` 三選一 | `CoreService::getLastConfigUpdateRecord` | `GetLastConfigUpdateRecord.cc:12-81` |

### 7.5 檔案系統（走 MetaClient）

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `ls` | 列目錄 | `[path]`（unknownArgs），`-l/--limit`，`-s/--status` | `MetaClient::list`（分頁迴圈） | `List.cc:13-68` |
| `stat` | 看 inode 詳情/layout/chunk 分佈 | `path`，`-L`，`--display-layout`，`--display-chain-list`，`--display-chunks`，`--display-all-chunks`，`--inode` | `MetaClient::stat` + `FileOperation::queryChunksByChain` | `Stat.cc:26-192` |
| `cd` | 切換目錄（改 shell 狀態） | `path`，`-L`，`--inode` | `MetaClient::stat` + `getRealPath` | `Chdir.cc:12-73` |
| `mkdir` | 建目錄 | `path`，`-p/-r/--recursive`，`--perm`，layout 五參數 | `MetaClient::mkdirs` | `Mkdir.cc:12-49` |
| `create` | 建檔 | `path`，`-p/--perm`，layout 五參數 | `MetaClient::create` | `Create.cc:14-49` |
| `rm` | 刪除 | `path`，`-r/--recursive` | `MetaClient::remove` | `Remove.cc:9-33` |
| `mv` | 改名/搬移 | `src dst` | `MetaClient::rename` | `Rename.cc:9-32` |
| `ln` | 建符號連結 | `target linkname` | `MetaClient::symlink` | `Symlink.cc:8-32` |
| `getrealpath` | 解析真實路徑 | `path` | `MetaClient::getRealPath` | `GetRealPath.cc:12-33` |
| `set-layout` | 改目錄的預設 layout | `path`，layout 五參數 | `stat` 驗證是目錄 → `MetaClient::setLayout` | `SetLayout.cc:9-39` |
| `set-perm` | 改 uid/gid/mode/iflags | `path`，`-u/-g/-p`（八進位）/`--iflags` | `MetaClient::setPermission` | `SetPermission.cc:12-56` |
| `recursive-chown` | 遞迴改擁有者（併發） | `path uid gid`，`-t/--threads`(16)，`-c/--concurrency`(2048)，`--debug` | `list` + `setPermission` 大量併發 | `RecursiveChown.cc:18-231` |
| `scan-tree` | 併發遍歷子樹找壞路徑 | `--inode` 或 `--path`（至少一），`--output` | `stat` + `list`（8 執行緒 / 128 協程） | `ScanTree.cc:39-186` |
| `gc-list` | 列出 GC 垃圾桶內容 | `[path]`，`-l/--limit`(256)，`-p/--prev` | `MetaClient::list(gcRoot)` + `GcManager::parseGcEntry` | `ListGc.cc:14-59` |
| `dump-session` | 匯出所有 file session 為 Parquet | `--output` | **直讀 FDB**：`FileSession::scan` × 256 shard | `DumpSession.cc:22-86` |
| `session-prune` | 手動清理過期 session | 無 | 本地建 `SessionManager` → `pruneManually()` | `PruneSession.cc:14-43` |

### 7.6 資料面 I/O 與校驗（走 StorageClient）

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `read-file` | 讀檔（可選 target、可補洞、可 hex） | `path`，`--offset/--length`，`--mode`，`--stat`，`--disableChecksum`，`--fillZero`，`--verbose`，`-o`，`--hex` | `batchRead` + MD5 | `ReadFile.cc:23-132` |
| `write-file` | 寫檔（stdin/檔案/重複填值） | `path`，`--offset/--length`，`--timeout`，`--notTruncate`，`-i/--input`，`--value` | `truncate` + `batchWrite` + `sync` | `WriteFile.cc:21-127` |
| `checksum` | 對每個副本各讀一遍比對 checksum | `path...`(1+)，`--list`，`--batch`(8)，`--md5`，`--fillZero`，`-o` | 每副本 `batchRead`（ManualMode） | `Checksum.cc:25-166` |
| `fill-zero` | 把檔案的洞補成 0 | `path`，`--dry-run`，`--verbose` | `batchRead` 找洞 → `batchWrite` 填零 | `FillZero.cc:20-120` |
| `query-chunk` | 查單一 chunk 在各副本的中繼資料 | `--chunk`，`-c/--chain-id`，`--read`，`--index`，`--touch` | `QueryChunkReq`（+ 可選 `batchRead`/`batchWrite`） | `QueryChunk.cc:16-125` |
| `dump-chunkmeta` | 匯出各 target 的全部 chunk metadata | `-c/--chain-ids`，`-m/--chunkmeta-dir`，`-q/--parquet-format`，`-h/--only-head`，`-p/--parallel`(32) | `getAllChunkMetadata` ×N | `DumpChunkMeta.cc:18-260` |
| `parse-target-meta` | 把 target metadata 檔解成 JSON | `path`，`-o/--output`，`--format` | 純本地 `serde::deserialize` | `ParseTargetMeta.cc:20-63` |
| `bench` | 寫入 + 雙端讀取校驗壓測 | `path`，`--rank`，`--timeout`，`--coroutines`，`--seconds`(60)，`--remove` | `writeFile` + 兩次 `readFile`（Head/Tail） | `Bench.cc:30-134` |
| `read-bench` | 隨機讀/寫吞吐壓測 | `path`，`--threads`(16)，`--coroutines`，`--seconds`(60)，`--write`，`--bs`，`--iodepth`，`--mode` | `batchRead`/`batchWrite` 迴圈 | `ReadBench.cc:33-210` |
| `create-range` | 批次建檔（壓測用） | `prefix inclusive_start exclusive_end`，`-c/--concurrency` | `MetaClient::create` ×N | `CreateRange.cc:12-79` |
| `remove-range` | 批次刪檔 | 同上 | `MetaClient::remove` ×N | `RemoveRange.cc:11-75` |
| `open-range` | 批次開檔 | 同上 + `-r/--round` | `MetaClient::open` ×N | `OpenRange.cc:12-81` |

### 7.7 使用者與權杖（直寫 FDB）

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `user-add` | 新增使用者 | `uid name`，`--root`，`--admin`，`--groups`，`--token` | `UserStore::addUser`（FDB txn） | `AdminUserCtrl.cc:65-112` |
| `user-remove` | 刪除使用者 | `uid` | `UserStore::removeUser` | `AdminUserCtrl.cc:114-141` |
| `user-set` | 改屬性 | `uid`，`--name`，`--root 0\|1`，`--admin 0\|1`，`--groups` | `UserStore::setUserAttr` | `AdminUserCtrl.cc:143-198` |
| `user-set-token` | 設定/輪替 token | `uid`，`--new` 或 `--token`（二選一），`--invalidate-existed-tokens`，`--max-active-tokens`(5)，`--last-token-lifetime-days`(7) | `UserStore::setUserToken` | `AdminUserCtrl.cc:200-241` |
| `user-stat` | 查單一使用者 | `uid` | `UserStore::getUser`（唯讀 txn，允許查自己） | `AdminUserCtrl.cc:243-275` |
| `user-list` | 列出全部使用者 | `--json` | `UserStore::listUsers` | `AdminUserCtrl.cc:277-331` |
| `user-switch` | 切換本地身分（不驗證） | `-u/--uid`，`-g/--gid`，`-t/--token` | 純本地改 `env.userInfo` | `AdminUserCtrl.cc:343-370` |
| `current-user` | 顯示目前身分 | 無 | 純本地 | `AdminUserCtrl.cc:372-388` |
| `decode-user-token` | 解出 token 內的 uid/timestamp | `token` | 純本地 `core::decodeUserToken` | `DecodeUserToken.cc:10-34` |

### 7.8 離線分析與除錯

| 命令 | 用途 | 主要參數 | 後端 | 原始碼 |
|---|---|---|---|---|
| `dump-inodes` | 掃描全部 inode 到檔案 | `-n/--num-inodes-perfile`(1e7)，`-f/--fdb-cluster-file`，`-i/--inode-dir`，`-q/--parquet-format`，`-a/--all-inodes`，`-t/--threads`(4) | **直讀 FDB**：`MetaScan::getInodes` | `DumpInodes.cc:30-339` |
| `dump-dentries` | 掃描全部 dentry 到 Parquet | `-n`，`-f`，`-d/--dentry-dir`，`-t/--threads` | **直讀 FDB**：`MetaScan::getDirEntries` | `DumpDirEntries.cc:25-161` |
| `find-orphaned-chunks` | 比對 inode dump 與 chunkmeta dump 找孤兒 chunk | `-i/--inode-path`，`-m/--chunkmeta-path`，`-n/--inode-ids`，`-o/--orphaned-dir`，`-x/--ignore-chunkid-prefix`(F)，`-v/--only-chunkid-prefix`，`-S/--skip-safety-check`，`-q`，`-p/--parallel`(32) | 純本地檔案比對 | `FindOrphanedChunks.cc:17-180` |
| `remove-chunks` | **刪除孤兒 chunk** | `-n`，`-f`，`-i/--inode-dir`(inodes2)，`-o/--orphaned-path`，`-r/--do-remove`，`-q` | 重掃 FDB → `batchRead` 確認 → `removeChunks` | `RemoveChunks.cc:16-203` |
| `remote-call` | 對任意服務發任意 RPC（JSON 進出） | `-n`/`-c`/`-a`/`--primary`/`--list-methods` 五選一，`-s/--service-name`，`-m/--method-name`，`-i/--method-id`，`-j/--input-json`，`-f/--input-json-file`，`-o/--output-file` | 反射遍歷四個 ServiceBase 找方法 | `RemoteCall.cc:25-230` |
| `help` | 列出全部命令或單一命令說明 | `[command]` | 純本地（**未註冊在 handlers_**） | `Dispatcher.cc:136-137, 171-183` |

---

## 8. 重點命令深入剖析

### 8.1 `remote-call`：用 serde 反射把整個 RPC 表變成 CLI

這是整份 CLI 技術含量最高的命令。它讓管理員能對 mgmtd/core/storage/meta 四個服務的**任意方法**發送 JSON 請求。

支援的服務是一個編譯期陣列（`RemoteCall.cc:17-23`）：

```cpp
using MgmtdBase  = mgmtd::MgmtdServiceBase<>;
using CoreBase   = core::CoreServiceBase<>;
using StorageBase = storage::StorageSerde<>;
using MetaBase   = meta::MetaSerde<>;
const auto services = std::array{MgmtdBase::kServiceName, CoreBase::kServiceName,
                                 StorageBase::kServiceName, MetaBase::kServiceName};
```

核心是 `getRemoteCaller<ServiceBase>`（`RemoteCall.cc:46-88`），用 `refl::Helper::iterate<ServiceBase>` 走訪服務定義的所有方法型別，每個方法型別 `Type` 帶有 `name`、`id`、`ReqType`、`RspType`：

```cpp
auto handler = [&](auto type) {
  using Type = decltype(type);
  if (Type::name == methodName || static_cast<int32_t>(Type::id) == methodId) {
    methodName = Type::name;                      // 補完：給了 id 就填名字，反之亦然
    methodId = static_cast<int32_t>(Type::id);
    caller = [methodName](serde::ClientContext &ctx, std::string_view input, bool prettyFormat) -> CoTryTask<String> {
      auto req = typename Type::ReqType{};
      auto fromRes = serde::fromJsonString(req, input);
      if (fromRes.hasError()) co_return makeError(...);
      auto callRes = co_await ctx.template call<ServiceBase::kServiceNameWrapper, Type::nameWrapper,
                                                typename Type::ReqType, typename Type::RspType,
                                                ServiceBase::kServiceID, Type::id>(req, nullptr, nullptr);
      if (callRes.hasError()) co_return makeError(...);
      co_return serde::toJsonString(*callRes, /*sortKeys=*/false, prettyFormat);
    };
    return false;   // 停止 iterate
  }
  return true;
};
auto matchNothing = refl::Helper::iterate<ServiceBase>(std::move(handler));
```

`iterate` 的回傳值語意是「handler 全程都回 true（沒有任何方法被匹配）」，所以 `matchNothing` 為真時報 `kInvalidMethodID`。

四個服務的分派用巨集展開（`RemoteCall.cc:178-189`）：

```cpp
#define DISPATCH(service, function, ...)                                                       \
  if (service == MgmtdBase::kServiceName)        { CO_RETURN_ON_ERROR(function<MgmtdBase>(__VA_ARGS__)); }   \
  else if (service == CoreBase::kServiceName)    { CO_RETURN_ON_ERROR(function<CoreBase>(__VA_ARGS__)); }    \
  else if (service == StorageBase::kServiceName) { CO_RETURN_ON_ERROR(function<StorageBase>(__VA_ARGS__)); } \
  else if (service == MetaBase::kServiceName)    { CO_RETURN_ON_ERROR(function<MetaBase>(__VA_ARGS__)); }    \
  else { co_return makeError(StatusCode::kInvalidArg, fmt::format("Unknown service: {}", service)); }
```

同一個巨集服務兩個泛型函式（`listMethods` 與 `getRemoteCaller`），這是「用巨集做 runtime string → compile-time type 的分派」的教科書案例——C++ 沒有辦法把執行期字串變成模板參數，只能窮舉。

`--primary` 選項（`RemoteCall.cc:140-149`）會從 routing info 找出唯一的 `PRIMARY_MGMTD` 節點，找不到回 `kPrimaryMgmtdNotFound`，找到多個回 `"Found multiple primary mgmtds"`。這對「mgmtd 選舉異常」的排查很有用。

發送邏輯（`:206-223`）是**依序嘗試每個位址，第一個成功就 `break`**——因為一個節點可能同時有 TCP 與 RDMA 位址。

### 8.2 `stat --display-chunks`：從 inode 一路查到每條鏈的最後 chunk

普通的 `stat` 就是一次 `MetaClient::stat` 加上一堆 `table.push_back`。真正有意思的是 `--display-chunks`：

```cpp
// src/client/cli/admin/Stat.cc:165-170
if (displayChunks || displayAllChunks) {
  meta::FileOperation fop(*env.storageClientGetter(), *routingInfo->raw(), env.userInfo, o);
  auto result = co_await fop.queryChunksByChain(displayAllChunks, false);
  CO_RETURN_ON_ERROR(result);
  CO_RETURN_ON_ERROR(printLayoutAndChunks(table, *routingInfo->raw(), o.asFile().layout, *result, displayAllChunks));
}
```

`meta::FileOperation` 是 meta server 內部用來重建檔案長度的類別（`src/fbs/meta/FileOperation.h`），CLI 直接借用它。輸出把 layout 的每個 chain index 映射成實際 chainId：

```cpp
// src/client/cli/admin/Stat.cc:84-96
for (auto index : chainIndexes) {
  auto cid = ri.getChainId({layout.tableId, layout.tableVersion, index});
  if (!cid) return makeError(MgmtdClientCode::kRoutingInfoNotReady);
  if (chunks.contains(*cid)) {
    auto chunk = chunks.find(*cid)->second;
    table.push_back({format(*cid), format(chunk.lastChunk), format(chunk.lastChunkLen), format(chunk.length)});
    ...
    totalChunkLength += chunk.totalChunkLen;
    totalLength = std::max(chunk.length, totalLength);
  }
}
```

注意兩個彙總量的計算方式不同：`totalLength` 取**最大值**（檔案長度由最靠後的 chunk 決定），`totalChunkLength` 取**總和**（實際佔用）。最後 `--display-all-chunks` 會印出兩者的差（`Stat.cc:106-110`），這個差值就是「稀疏空洞的大小」——正是 `fill-zero` 命令要處理的東西。

還有一個小巧的參數依賴：

```cpp
// src/client/cli/admin/Stat.cc:132-134
if (displayChainList) { displayLayout = true; }
```

`--display-chain-list` 隱含 `--display-layout`，因為 chain list 是 layout 的一部分。

### 8.3 `checksum`：用 ManualMode 逐副本讀取來偵測靜默資料損毀

`checksum` 是 CRAQ 一致性驗證的主力工具。核心邏輯（`Checksum.cc:56-78`）：

```cpp
auto replicasNum = file.replicasNum();
CO_RETURN_AND_LOG_ON_ERROR(replicasNum);
for (auto i = 0u; i < *replicasNum; ++i) {
  std::ofstream out("/dev/null");
  auto readResult = co_await file.readFile(out, file.length(), 0, true, false,
                                           storage::client::TargetSelectionMode::ManualMode,
                                           i == 0 && md5Enabled ? &md5 : nullptr, fillZero, false, 0);
  CO_RETURN_AND_LOG_ON_ERROR(readResult);
  if (i == 0) {
    result.checksum = *readResult;
  } else if (result.checksum != *readResult) {
    co_return makeError(StorageCode::kChecksumMismatch,
                        fmt::format("file checksum mismatch, path {}, head {}, now:{} {}", path, result.checksum, i, *readResult));
  }
}
```

三個設計點：

1. **`replicasNum()`（`FileWrapper.cc:30-51`）只數 SERVING 的 target**——正在 SYNCING 的副本資料可能還沒同步完，不該參與比對。
2. **輸出丟到 `/dev/null`**——只要 checksum，不要資料。但資料仍然完整經過網路傳輸與 RDMA 拷貝。
3. **`ManualMode` 但 `targetIndex` 永遠傳 0**——這是一個 bug 或至少是可疑之處：`readFile` 的最後一個參數硬編碼 `0`（`Checksum.cc:69`），迴圈變數 `i` 只用來決定 md5 與比對邏輯。看 `FileWrapper::readFile` 的 `readOptions.targetSelection().set_targetIndex(targetIndex)`（`FileWrapper.cc:117`），這意味著**每一輪都讀同一個副本，實際上並沒有比對不同副本**。

MD5 只在第一輪計算（`i == 0 && md5Enabled ? &md5 : nullptr`），因為後續輪次讀到的應該是相同資料。

批次執行用固定大小的視窗（`Checksum.cc:140-157`），每 `--batch`（預設 8）個檔案 `collectAllRange` 一次，避免同時開太多檔案。

### 8.4 `find-orphaned-chunks` + `remove-chunks`：三階段的孤兒 chunk 回收

這是唯一一組需要**多次執行、有嚴格時序要求**的命令，設計上非常小心。

**階段一：`dump-inodes`**（時間 T1）掃 FDB 拿到所有存活的 inode id。
**階段二：`dump-chunkmeta`**（時間 T2）從每個 target 拿到所有 chunk 的 metadata。
**階段三：`find-orphaned-chunks`** 比對兩者，chunk 的 inode 不在 inode 集合裡 → 孤兒。

安全檢查的核心是時序：

```cpp
// src/client/cli/admin/FindOrphanedChunks.cc:100-107
if (!skipSafetyCheck && metadata.timestamp >= inodeDumpTime) {
  XLOGF(FATAL, "Chunk metadata dump time '{:%c}' >= inode snapshot time '{:%c}', skipping file: {}",
        fmt::localtime(metadata.timestamp), fmt::localtime(inodeDumpTime), chunkmetaFilePath);
  return false;
}
```

**必須 T2 < T1**，也就是 chunkmeta 要**比 inode 快照更早**。反直覺但正確：若 chunkmeta 較新，可能包含「inode 快照之後才建立的檔案」的 chunk，這些 chunk 的 inode 當然不在舊快照裡，會被誤判為孤兒。`inodeDumpTime` 取的是所有 inode 檔案的**最小** timestamp（`DumpInodes.cc:212`），保守到底。

`-x/--ignore-chunkid-prefix` 預設值是 `"F"`（`FindOrphanedChunks.cc:23`）——這是 chunk id 十六進位字串的第一個字元。3FS 的 InodeId 空間裡 `0xfxxxxxxxxxxxxxxx` 一段是保留給虛擬 inode（`3fs-virt`、iov、rm-rf 臨時 inode）的，這些 chunk 不該被當成孤兒處理。

`remove-chunks` 又加了**三道獨立的保險**：

```cpp
// src/client/cli/admin/RemoveChunks.cc:47  第一道：重新 dump 一次 inode（不信任舊的）
auto dumpRes = co_await dumpInodesFromFdb(fdbClusterFile, numInodesPerFile, inodeDir, parquetFormat);

// src/client/cli/admin/RemoveChunks.cc:83-90  第二道：孤兒檔案時間仍須早於新 inode 快照
if (orphanedChunkmeta.timestamp >= inodeDumpTime) { ... co_return makeError(StatusCode::kInvalidArg); }

// src/client/cli/admin/RemoveChunks.cc:106-113  第三道：逐 chunk 再確認 inode 確實不存在
if (uniqInodeIds->count(metaInodeId)) {
  XLOGF(CRITICAL, "Stop removing since inode {} of chunk {} still exists, file: {}", metaInodeId, metaChunkId, orphanedChunkPath);
  co_return makeError(StatusCode::kInvalidArg);
}
```

第三道檢查是 `co_return` 而非 `continue`——**一旦發現任何一個 chunk 的 inode 還活著，整個命令立刻中止**，不繼續刪剩下的。

然後還會先發 1 byte 的 `batchRead` 探測 chunk 是否真的存在（`:132`），`kChunkNotFound` 的直接跳過（`:138-139`），其他錯誤則「照刪不誤」（`:147-153` 的 `"removing it anyway"`）。

**`-r/--do-remove` 預設 false**（`RemoveChunks.cc:22`），不給就是 dry-run（`:166-169`）。這是整份 CLI 裡**唯一一個把「破壞性操作預設關閉」做對的命令**。

### 8.5 `verify-config`：全叢集併發設定演練

在推設定之前先確認「這份模板套到每一台機器上會不會出事」。做法是對每個節點併發跑三件事（`VerifyConfig.cc:39-54`）：

```cpp
result.currentConfig = (co_await coreClient->getConfig(addresses, core::GetConfigReq::create())).then(...);
result.coldConfigRes = co_await coreClient->renderConfig(addresses,
    core::RenderConfigReq::create(config, /*testUpdate=*/true, /*hotUpdate=*/false));
result.hotConfigRes = co_await coreClient->renderConfig(addresses,
    core::RenderConfigReq::create(config, /*testUpdate=*/true, /*hotUpdate=*/true));
```

「冷啟動套用」與「熱更新套用」分開測，因為有些設定項只能冷啟動改。比對方法是把兩份 toml 都用同一個 formatter 正規化再比字串（`VerifyConfig.cc:25-30`）：

```cpp
String toPrettyToml(const String &s) {
  auto t = toml::parse(s);
  std::stringstream ss;
  ss << toml::toml_formatter(t, toml::toml_formatter::default_flags & ~toml::format_flags::indentation);
  return ss.str();
}
```

關掉 `indentation` flag 讓縮排差異不影響比對。**這是「語意等價」的窮人版實作**——正確的做法是遞迴比較 toml AST，這裡用「同一個 serializer 產生的字串相同」來近似。

彙總用一個 `std::map<String, int>` 直方圖（`:104-130`），最後輸出成一行 `total=100 verify_cold_unchanged=98 verify_hot_changed=2` 的形式。這種「先給一行摘要、`--verbose` 才給細節」的設計在整份 CLI 裡是獨一份。

併發用 `scheduleOn(&client->tpg().procThreadPool().randomPick()).start()`（`:99`）搶佔 net::Client 自己的執行緒池，然後 `collectAllRange`。

### 8.6 `drop-user-cache`：不經 mgmtd，逐台敲 meta server

這是唯一一個「用 `clientGetter` 手搓 stub」的正規維運命令（`remote-call` 是除錯工具）：

```cpp
// src/client/cli/admin/DropUserCache.cc:38-60
auto client = env.clientGetter();
for (const auto &[nid, ni] : routingInfo->nodes) {
  if (ni.type != flat::NodeType::META) continue;
  if (ni.status != flat::NodeStatus::HEARTBEAT_CONNECTED) {
    table.push_back({std::to_string(nid), "Skip", fmt::format("Status is {}", toStringView(ni.status))});
    continue;
  }
  auto addrs = ni.extractAddresses("MetaSerde");
  if (addrs.empty()) { table.push_back({std::to_string(nid), "Skip", "Address of MetaSerde not found"}); continue; }
  auto result = co_await [&]() -> CoTask<std::vector<Status>> {
    std::vector<Status> rets;
    for (auto addr : addrs) {
      auto ctx = client->serdeCtx(addr);
      auto stub = meta::MetaServiceStub<serde::ClientContext>(ctx);
      auto ret = co_await stub.dropUserCache(req, {});
      if (!ret.hasError()) co_return std::vector<Status>{};   // 任一位址成功即算成功
      rets.push_back(std::move(ret.error()));
    }
    co_return rets;
  }();
  table.push_back(result.empty() ? Dispatcher::OutputRow{std::to_string(nid), "Succeed", ""}
                                 : Dispatcher::OutputRow{std::to_string(nid), "Failed", fmt::format("[{}]", fmt::join(result, ", "))});
}
```

三個值得注意的地方：

1. **不用 MetaClient**——`MetaClient` 會依 inode 做一致性雜湊選 server，而這裡要「敲遍所有 server」。
2. **「空的錯誤 vector」表示成功**——用 `result.empty()` 當成功旗標，是一種省一個 bool 的緊湊寫法。
3. **逐節點記錄結果而非早退**——某台 meta server 掛了不影響其他台，最終表格逐列呈現。這是整份 CLI 裡少見的「部分失敗友善」設計。

### 8.7 `dump-inodes`：有界併發的自製背壓

`dump-inodes` 要掃出上億筆 inode，記憶體與 I/O 都是問題。它的處理迴圈（`DumpInodes.cc:116-145`）：

```cpp
while (true) {
  auto inodes = scan->getInodes();
  for (const auto &inode : inodes) {
    if (inode.isFile() || dumpAllInodes) { inodeBatch.inodes.push_back({timestamp, inode}); numInodesSaved++; }
  }
  bool fullBatch = inodeBatch.inodes.size() >= numInodesPerFile;
  bool lastBatch = inodes.empty() && !inodeBatch.inodes.empty();
  if (fullBatch || lastBatch) {
    running++;
    auto task = folly::coro::co_invoke(dumpInodeTable, numInodesSaved,
                                       std::exchange(inodeBatch, InodeTable{.timestamp = timestamp}))
                    .scheduleOn(exec.get()).start();
    tasks.push_back(std::move(task));
    inodeBatch.inodes.reserve(numInodesPerFile);
    while (running >= threads) { std::this_thread::sleep_for(std::chrono::milliseconds(50)); }
  }
  if (inodes.empty()) break;
}
```

背壓機制是 `while (running >= threads) std::this_thread::sleep_for(50ms)` —— **在協程裡用阻塞式 sleep 做流控**。這在 folly coroutine 的語境下是反模式（會卡住整個 executor 執行緒），但在這個場景可以接受，因為 `dumpInodesFromFdb` 是被 `blockingWait` 起來的、獨佔主執行緒。用 `folly::fibers::Semaphore`（`RecursiveChown.cc:198-199` 就是這麼做的）才是正解。

`std::exchange(inodeBatch, InodeTable{...})` 是零拷貝交棒：把當前 batch 的所有權移進 lambda，同時原地重置。

預設**只 dump 檔案**（`inode.isFile()`），要目錄與符號連結得加 `-a/--all-inodes`——因為 `find-orphaned-chunks` 只關心檔案。

Parquet 輸出前有一個防禦性處理（`DumpInodes.cc:301-307`）：

```cpp
for (const auto &inode : inodes) {
  if (inode.inode.isDirectory()) {
    utf8makevalid((utf8_int8_t *)inode.inode.asDirectory().name.c_str(), '?');
  }
  serdeWriter << inode;
```

3FS 的檔名是任意 byte 序列（POSIX 只禁 `/` 與 `\0`），但 Parquet 的 UTF8 欄位要求合法 UTF-8。這裡就地把非法 byte 換成 `?`。注意它是 **`const_cast` 式的就地改寫**（`(utf8_int8_t *)name.c_str()`），改的是記憶體裡的副本，不影響 FDB。

### 8.8 `recursive-chown`：雙 semaphore 的自適應併發

`recursive-chown` 要遍歷整棵樹並對每個檔案發 `setPermission`。難點是「列目錄」與「改權限」兩種任務的併發需要分別控制。

```cpp
// src/client/cli/admin/RecursiveChown.cc:196-199
folly::CPUThreadPoolExecutor exec_;
folly::fibers::Semaphore listSemaphore_;
folly::fibers::Semaphore chownSemaphore_;
```

關鍵在 `listDir` 的三個多載形成的「拿不到就自己做」模式（`:110-167`）：

```cpp
for (const auto &entry : res->entries) {
  auto nextPath = arg.path / entry.name;
  if (entry.isDirectory()) {
    ++listTasks_;
    if (listSemaphore_.try_wait()) {
      listDir(TaskArg{.ino = entry.id, .path = nextPath}).scheduleOn(&exec_).start();   // 有配額 → 併發
    } else {
      args.push_back(TaskArg{.ino = entry.id, .path = nextPath});                       // 沒配額 → 排隊給自己做
    }
  } else {
    ++chownTasks_;
    co_await chownSemaphore_.co_wait();                                                 // chown 一定等
    chown(TaskArg{.ino = entry.id, .path = nextPath}).scheduleOn(&exec_).start();
  }
}
```

`try_wait()` 對子目錄、`co_wait()` 對檔案——因為子目錄可以延後處理（放進 `args` 讓當前協程自己遞迴），而 chown 必須真的執行。這避免了「所有協程都卡在等 semaphore、沒人在推進」的死結。

完成判定是輪詢六個原子計數器（`:71-94`）：

```cpp
if (listTasks == listTasksSuccess + listTasksFailed && chownTasks == chownTasksSuccess + chownTasksFailed) break;
```

用 `memory_order_acquire` 讀取，每秒印一次進度。這比用 `CountDownLatch` 麻煩，但支援「總數在執行中持續增加」的情境。

### 8.9 `scan-tree`：CancellationToken 收尾的無界佇列

`scan-tree` 的目標是找出「stat 或 list 會失敗的路徑」，用於資料損毀後的損害評估。架構是經典的工作佇列（`ScanTree.cc:60-80`）：

```cpp
CoTryTask<std::vector<BrokenPath>> run(size_t threads, size_t coroutines) {
  auto result = co_await meta_.stat(user_, path_.parent, path_.path, false);
  CO_RETURN_AND_LOG_ON_ERROR(result);
  enqueue({"", {}, *result});

  auto exec = std::make_unique<folly::CPUThreadPoolExecutor>(threads);
  std::vector<folly::SemiFuture<folly::Unit>> workers;
  while (workers.size() < coroutines) workers.push_back(worker().scheduleOn(exec.get()).start());

  while (pending_) co_await folly::coro::sleep(std::chrono::milliseconds(100));
  cancel_.requestCancellation();
  for (auto &worker : workers) worker.wait();

  co_return broken_.withWLock([](auto &broken) { return std::exchange(broken, {}); });
}
```

`folly::coro::UnboundedQueue` 沒有「關閉」語意，所以用 `folly::CancellationSource` 讓卡在 `dequeue()` 的 worker 拋例外退出：

```cpp
// src/client/cli/admin/ScanTree.cc:97-101
auto dequeue = co_await folly::coro::co_awaitTry(
    folly::coro::co_withCancellation(cancel_.getToken(), queue_.dequeue()));
if (UNLIKELY(dequeue.hasException())) break;
```

固定用 8 執行緒 / 128 協程（`ScanTree.cc:173` 的 `scan.run(8, 128)`），**沒有做成參數**。`list` 用 `readdirplus` 語意（最後一個參數 `true`，`:129`），一次拿 512 筆並附帶 inode，讓子節點不必再 stat 一次（`:116-119` 的 `if (!task.inode.has_value())`）。

### 8.10 `query-chunk`：從 chunk id 反推 chain 的完整鏈路

`query-chunk` 展示了 3FS 各層 ID 之間的可逆映射。若使用者沒給 `-c/--chain-id`，它要從 chunk id 反推：

```cpp
// src/client/cli/admin/QueryChunk.cc:49-63
auto metaChunk = meta::ChunkId::unpack(chunk.data());                    // ① chunk id → (inode, chunk index)
auto statResult = co_await env.metaClientGetter()->stat(env.userInfo,
                     meta::InodeId(metaChunk.inode()), std::nullopt, false);   // ② inode id → Inode
...
const auto &file = inode.asFile();
auto offset = metaChunk.chunk() * file.layout.chunkSize;                  // ③ chunk index → byte offset
auto chainResult = file.getChainId(inode, offset, *routingInfo, 0);       // ④ offset + layout → chainId
req.chainId = *chainResult;
```

四步反推：chunk id 內嵌了 inode id 與 chunk 序號，inode 帶 layout，layout 加 offset 決定 chain index，chain index 加 routing info 得到 chainId。這條路徑同時被 `FileWrapper::prepareBlocks`（`FileWrapper.cc:15-28`）與 `read-file`/`fill-zero`/`read-bench` 使用。

`--read` 與 `--touch` 兩個選項讓它從「查詢」變成「驗證」與「修復」工具：

- `--read --index N`：用 `ManualMode` 從第 N 個副本讀一遍，可以定位「哪個副本壞了」（`QueryChunk.cc:88-97`）。
- `--touch`：對 chunk 發一個 0 長度、offset 0 的 `batchWrite`（`:111-113`），觸發 CRAQ 鏈的重新複製。

chunkSize 是從 query 結果動態推出來的（`:71-76`），而不是從 layout 讀——因為這個命令的使用情境是「meta 可能已經壞了」。

---

## 9. 危險操作與保護機制

### 9.1 總體評估：幾乎沒有保護

**沒有任何命令有互動式二次確認。** 全 codebase 搜不到 `"Are you sure"`、`readline` 確認提示或 `--yes` 旗標。破壞性命令與唯讀命令走完全相同的程式路徑，唯一的差別是它們做的事不一樣。

| 命令 | 破壞性 | 二次確認 | dry-run | 前置檢查 |
|---|---|---|---|---|
| `shutdown-all-chains` | 極高（全叢集停擺） | 無 | 無 | 可選 `-c/--check-offline` |
| `remove-target` | 高（刪 target 資料） | 無 | 無 | 無（`--force` 只是往下傳） |
| `offline-target` | 高 | 無 | 無 | 無 |
| `upload-chains` | 高（改拓樸） | 無 | 無 | server 端 `checkChains` |
| `upload-chain-table` | 高 | 無 | 無 | server 端 `checkChains` |
| `unregister-node` | 中 | 無 | 無 | 驗證 nodeId 與 type 相符 |
| `rm -r` | 高（刪檔案） | 無 | 無 | 無 |
| `remove-range` | 高 | 無 | 無 | 無 |
| `user-remove` | 中 | 無 | 無 | `ensureAdmin` |
| `set-config` | 中（推設定） | 無 | 無 | 無（但有 `verify-config` 可先跑） |
| `hot-update-config` | 中 | 無 | 無 | 本地 toml 語法檢查 |
| `remove-chunks` | 高 | 無 | **有（預設）** | 三道 inode 存在性檢查 |
| `fill-zero` | 中（改資料） | 無 | **有** | 無 |
| `create-target` | 中 | 無 | 無 | **LASTSRV 檢查** |
| `create-targets` | 中 | 無 | 無 | **LASTSRV 檢查（跳過而非中止）** |
| `init-cluster` | 高（覆蓋設定） | 無 | 無 | `--allow-config-existed` 預設 false |

只有 **`remove-chunks`（`-r/--do-remove`）與 `fill-zero`（`--dry-run`）** 有 dry-run，且只有 `remove-chunks` 把安全值設為預設。

### 9.2 `shutdown-all-chains --check-offline`：用「必須失敗」當前置條件

這是全 CLI 最巧妙的保護機制。`shutdown-all-chains` 直接繞過 mgmtd 改 FDB，把所有 chain 的 target 打成離線狀態——**只有在 mgmtd 已經全部停掉時執行才安全**，否則活著的 mgmtd 會在下一輪心跳把狀態覆蓋回去，造成不一致。

```cpp
// src/client/cli/admin/ShutdownAllChains.cc:35-46
bool checkClusterOfflined = parser.get<bool>("-c");
if (checkClusterOfflined) {
  auto mgmtdClient = env.unsafeMgmtdClientGetter();
  auto res = co_await mgmtdClient->refreshRoutingInfo(/*force=*/true);
  if (!res.hasError() || res.error().code() != MgmtdClientCode::kPrimaryMgmtdNotFound) {
    co_return makeError(StatusCode::kInvalidArg,
        fmt::format("Cluster not offlined. expected status: MgmtdClientCode::kPrimaryMgmtdNotFound. actual status: {}",
                    res.hasError() ? res.error().describe() : "OK"));
  }
}
```

**成功就是失敗，失敗才是成功**——它要求 `refreshRoutingInfo` 必須以 `kPrimaryMgmtdNotFound` 這個**特定**錯誤碼失敗。任何其他結果（包括成功、包括其他錯誤如網路不通）都會中止操作。用 `unsafeMgmtdClientGetter` 是因為安全版會在 routing info 為空時直接丟例外（`admin_cli.cc:159-161`），根本走不到這個檢查。

**但 `-c` 預設是 false**（`ShutdownAllChains.cc:23`）。這個精心設計的保護預設是關閉的。

實際的 FDB 操作（`src/mgmtd/store/MgmtdStore.cc:439-459`）會遞增 routing info version、遍歷所有 chain、對變更過的 chain 遞增 chainVersion 後寫回——這是原子的（單一 FDB 交易，重試策略 `{1_s, 10, false}`，`ShutdownAllChains.cc:19`）。

### 9.3 LASTSRV 保護：唯一的資料安全前置檢查

`create-target` 是全 CLI **唯一一個在 CLI 端做資料安全檢查**的命令：

```cpp
// src/client/cli/admin/CreateTarget.cc:56-65
auto chainInfo = routingInfo->getChain(req.chainId);
if (chainInfo && chainInfo->targets.size() > 1) {
  // chain info exists and the number of replicas is greater than one.
  for (auto target : chainInfo->targets) {
    if (target.targetId == req.targetId && target.publicState == flat::PublicTargetState::LASTSRV) {
      co_return makeError(StorageClientCode::kRoutingError,
                          fmt::format("creation of this target is prohibited {}", target));
    }
  }
}
```

`LASTSRV` 表示「這是這條鏈上最後一個還有完整資料的副本」。重新建立（等於清空）它會導致**該 chain 的資料永久遺失**。`targets.size() > 1` 的條件是為了排除單副本鏈（那種鏈的唯一 target 必然是 LASTSRV，但重建單副本鏈是合法的維運動作）。

`create-targets`（複數）有同樣檢查但行為不同——**跳過而非中止**：

```cpp
// src/client/cli/admin/CreateTargets.cc:45-51
if (targetInfo.publicState == flat::PublicTargetState::LASTSRV) {
  auto chainInfo = routingInfo->getChain(targetInfo.chainId);
  if (chainInfo->targets.size() > 1) {
    std::cout << fmt::format("creation of this target is prohibited {}, skip\n", targetInfo);
    continue;
  }
}
```

這是合理的：批次重建整顆磁碟時，跳過危險的個別 target 比整批中止有用。但注意這裡 `chainInfo` **沒有 null 檢查**就直接 `->targets`——若 routing info 裡有 target 指向不存在的 chain，會 crash。

### 9.4 `--force` 的語意：它不是保護，是繞過保護

`offline-target --force` 與 `remove-target --force` 的 `force` **不需要打破任何 CLI 端的限制**——它只是被塞進 request 傳給 storage server：

```cpp
// src/client/cli/admin/RemoveTarget.cc:30-31
req.targetId = flat::TargetId(parser.get<flat::TargetId::UnderlyingType>("--target-id"));
req.force = parser.get<bool>("--force");
```

也就是說**不加 `--force` 執行 `remove-target` 一樣會刪除 target**，`--force` 只是讓 server 跳過它自己的檢查。這與 `rm -f` 的直覺完全一致，但與「`--force` 是危險操作的守門員」的期待相反。

### 9.5 server 端的保護：`upload-chains` 的不變式

CLI 端只做 CSV 格式檢查，真正的邏輯保護在 mgmtd。`SetChainsOperation`（`src/mgmtd/ops/SetChainsOperation.cc:60-98`）對每條 chain：

- **已存在的 chain** → `ensureChainNotChanged`（`:23-58`）：chainId 相同、target **數量**相同、target **集合**（排序後）相同。也就是**已存在的 chain 只能被「重新提交同一份定義」，不能透過 `upload-chains` 增刪 target**。要改必須用 `update-chain`。
- **新 chain** → 檢查每個 target 都是全新的（不在 routing info 也不在本批其他 chain 裡，`:85-93`），防止一個 target 被兩條 chain 引用。

`SetChainTableOperation`（`src/mgmtd/ops/SetChainTableOperation.cc:7-32`）的 `checkChains`：

- chains 不可為空；
- 每個 chainId 必須已存在；
- **所有 chain 的副本數必須一致**（`:19-29`）。這是 chain table 的核心不變式：同一張表內的所有 chain 必須是同構的，否則 layout 的 stripe 計算會亂掉。

版本管理（`SetChainTableOperation.cc:51-65`）：

```cpp
auto ctit = dataPtr->routingInfo.chainTables.find(tableId);
if (ctit != dataPtr->routingInfo.chainTables.end()) {
  const auto &current = ctit->second.rbegin()->second;      // 最新版本
  if (newChainTable.desc.empty()) newChainTable.desc = current.desc;   // desc 空則沿用
  if (newChainTable.chains != current.chains || newChainTable.desc != current.desc) {
    newChainTable.chainTableVersion = nextVersion(current.chainTableVersion);
  } else {
    co_return SetChainTableRsp::create(current.chainTableVersion);     // 完全相同 → 不建新版本
  }
}
```

**冪等**：重複上傳同一份 CSV 不會產生新版本。這對「腳本重跑」很重要。（`git log` 顯示最近的 commit `22fca04 [Fix] Ensure ChainTable's version is monotonically increasing` 就是在修這一段的版本推進邏輯。）

### 9.6 `init-cluster` 的設定覆蓋保護

```cpp
// src/client/cli/admin/InitCluster.cc:93-102
flat::ConfigVersion ver(1);
if (configInfo->has_value()) {
  if (!allowConfigExisted) {
    co_return makeError(StatusCode::kUnknown,
                        fmt::format("Config for {} existed, version {}", toString(nodeType),
                                    configInfo->value().configVersion.toUnderType()));
  }
  ver = flat::ConfigVersion(configInfo->value().configVersion + 1);
}
```

預設拒絕覆蓋已存在的設定，要 `--allow-config-existed` 才會建新版本。這個保護做對了（**危險行為需要顯式開啟**）。

但同一個檔案裡有一段被繳械的檢查：

```cpp
// src/client/cli/admin/InitCluster.cc:129-131
auto allowConfigExisted = parser.get<bool>("--allow-config-existed");
// TODO: consider how to check
// auto skipConfigCheck = parser.get<bool>("skip-config-check");
auto skipConfigCheck = true;
```

`--skip-config-check` 這個參數**已註冊但被硬編碼成永遠 true**（`:42` 註冊、`:131` 覆蓋）。也就是 `handleInitConfig` 裡的 `cfg.update(configStr, /*isHotUpdate=*/false)` 語意驗證（`:82-88`）**永遠不會執行**。灌進去的設定只要是合法 toml 就會被接受，是否符合各角色的 schema 完全沒驗。

---

## 10. ChainTable 檔案格式與上傳流程

### 10.1 兩種 CSV

3FS 的拓樸資訊用兩份 CSV 描述，兩個命令、兩種格式：

**`upload-chains` 的 CSV：定義每條 chain 由哪些 target 組成**

```csv
ChainId,TargetId,TargetId
123,100001,100101
234,100002,100102
```

- **第 0 欄必須恰好是 `ChainId`**（`UploadChains.cc:48-51`）。
- **第 1..N 欄必須全部是 `TargetId`**（`:57-62`）——重複的欄名，靠位置區分。這是 rapidcsv 允許重複欄名才可行的寫法。
- 至少要有一個 TargetId 欄（`:53-55`）。
- 每列的欄數必須與表頭一致（`:71-75`）。
- 每個 TargetId 必須 > 0（`:81-84`）。**ChainId 在這裡沒有檢查正負**，與 `upload-chain-table` 不同。
- 欄的**順序就是 CRAQ 鏈的順序**：第一個 target 是 HEAD、最後一個是 TAIL（`ListTargets.cc:31` 的 `i == 0 ? "HEAD" : (i == n - 1 ? "TAIL" : "MIDDLE")` 印證了這一點）。

**`upload-chain-table` 的 CSV：定義一張表引用哪些 chain**

```csv
ChainId
123
234
```

- **必須恰好一欄，且欄名為 `ChainId`**（`UploadChainTable.cc:46-53`）。
- 不可為空（`:55-57`）。
- ChainId 必須 > 0（`:68-70`）且不可溢位 `flat::ChainId::UnderlyingType`（`:71-76`）——先用 `int64_t` 讀進來再檢查上界，因為 rapidcsv 直接讀成目標型別會靜默截斷。
- **順序有語意**：chain 在表中的索引就是 layout 的 chain index，`Layout::getChainIndexList()` 產生的索引會經 `RoutingInfo::getChainId({tableId, tableVersion, index})` 查回 chainId（`Stat.cc:55`）。**重排 CSV 的列順序等於重排所有檔案的資料分佈**。

### 10.2 `-d/--dump-template`：自產範本

兩個命令都支援 `-d`，行為是「不上傳，改成寫一份範本到指定路徑」：

```cpp
// src/client/cli/admin/UploadChains.cc:29-37
if (parser.get<bool>("-d")) {
  std::ofstream of(csvFilePath);
  of.exceptions(std::ofstream::failbit | std::ofstream::badbit);
  of << "ChainId,TargetId,TargetId\n";
  of << "123,100001,100101\n";
  of << "234,100002,100102\n";
  table.push_back({fmt::format("Dump template to {} succeeded", csvFilePath)});
  co_return table;
}
```

**`-d` 與正常上傳共用同一個 `csv-file-path` 位置參數**。所以打錯旗標就會**覆蓋掉你的 CSV**：`upload-chains -d mychains.csv` 不是「用 -d 模式上傳」，而是「把 mychains.csv 覆蓋成範本」。`std::ofstream of(path)` 是無條件截斷的，`of.exceptions(...)` 只會在 I/O 錯誤時拋，不會阻止覆蓋。

### 10.3 反向：`dump-chains` 與 `dump-chain-table`

`dump-chain-table` 產生的檔案可以直接餵回 `upload-chain-table`（格式完全一致，`DumpChainTable.cc:42-45`）。

`dump-chains` 比較特別——它**按副本數分檔**：

```cpp
// src/client/cli/admin/DumpChains.cc:32-52
robin_hood::unordered_map<size_t, std::vector<flat::ChainId>> chainsByReplicaCount;
for (const auto &[cid, ci] : routingInfo->raw()->chains) chainsByReplicaCount[ci.targets.size()].push_back(cid);

for (const auto &[rc, chains] : chainsByReplicaCount) {
  auto path = fmt::format("{}.{}", csvFilePathPrefix, rc);      // ← 例如 backup.csv.3
  std::ofstream of(path);
  of << "ChainId";
  for (size_t i = 0; i < rc; ++i) of << ",TargetId";
  of << "\n";
  for (auto cid : chains) { ... }
}
```

因為 CSV 是矩形的，不同副本數的 chain 無法放在同一份檔案裡。所以參數叫 `csv-file-path-prefix` 而非 `csv-file-path`，實際輸出是 `<prefix>.2`、`<prefix>.3` 等。**還原時必須逐個檔案分別 `upload-chains`**。

### 10.4 完整的叢集拓樸建置流程

從程式碼可以還原出標準流程：

```
① admin_cli user-add 0 root --admin --token <token>       ← bootstrap admin（第一個必須是 admin）
② admin_cli init-cluster <tableId> <chunkSize> <stripeSize> \
             --mgmtd m.toml --meta t.toml --storage s.toml --fuse f.toml
                                                           ← 寫根目錄 layout + 四份初始設定（直寫 FDB）
③ admin_cli register-node <id> STORAGE   ×N               ← 註冊每個節點
④ admin_cli create-target --node-id N --disk-index D \
             --target-id T --chain-id C                    ← 在每顆磁碟上建 target
   （或 create-targets --node-id N --disk-index 0 1 2 ...）
⑤ admin_cli upload-chains chains.csv                       ← 定義 chain 組成（SetChainsReq）
⑥ admin_cli upload-chain-table 1 table.csv                 ← 定義 chain table（SetChainTableReq）
⑦ admin_cli list-chains / list-targets                     ← 驗證
```

注意 ④ 與 ⑤ 的順序：`create-target` 需要 `--chain-id`，但那時 chain 還不存在。看 `CreateTarget.cc:56` 的 `routingInfo->getChain(req.chainId)` 允許回 null（`if (chainInfo && ...)`），所以**先建 target 後定義 chain 是被允許的**。反過來 `upload-chains` 要求新 chain 的 target 都不在 routing info 裡（`SetChainsOperation.cc:86`）——但 target 是透過 storage server 心跳才進入 routing info 的，所以實務上兩種順序都能走通，取決於心跳時機。這是一個**未文件化的時序相依**。

---

## 11. 錯誤處理與 exit code

### 11.1 三種錯誤傳遞機制並存

| 機制 | 用在哪 | 誰接 |
|---|---|---|
| `Result<T>` / `CoTryTask<T>` | 絕大多數 handler | `CO_RETURN_ON_ERROR` 逐層冒泡 |
| `throw StatusException` | `ENSURE_USAGE`、`AdminEnv` getter 的 routing info 檢查 | `Dispatcher::run` 的 catch（`Dispatcher.cc:154-157`） |
| `XLOGF(FATAL, ...)` | `dump-inodes`（`:195, 241`）、`find-orphaned-chunks`（`:68, 96`）、`read-bench`（`:146`）、`remote-call`（`:151`） | **沒人接——直接 abort** |

第三種是真正的問題。`XLOGF(FATAL, ...)` 在 folly 裡會 `abort()`，**不執行任何清理、不回傳 exit code 1 而是 SIGABRT**。`FindOrphanedChunks.cc:96` 的「載入 chunkmeta 檔案失敗」是完全可恢復的情況（跳過那個檔案即可），卻用 FATAL 處理。

### 11.2 兩層 catch

`runLine` 有一層通用的 catch-all（`Dispatcher.cc:32-38`）：

```cpp
try {
  co_return co_await dispatcher.run(env, args);
} catch (const std::exception &e) {
  co_return makeError(StatusCode::kUnknown, e.what());
} catch (...) {
  co_return makeError(StatusCode::kUnknown);
}
```

這保證任何 handler 拋出的例外（例如 `rapidcsv::Document` 建構失敗、`toml::parse` 失敗、`std::ofstream` 的 `failbit` 例外）都不會讓整個 process 掛掉，會被轉成 `kUnknown` 錯誤。**代價是錯誤碼資訊全部遺失**——`rapidcsv` 開不了檔跟 `toml` 語法錯誤在使用者看來都是 `kUnknown`。

`Dispatcher::run` 內層則專門處理 `StatusException`（`:154-157`），對 `kWrongUsage` 特殊處理成「錯誤訊息 + usage」。

### 11.3 exit code：只有 0 和 1

```cpp
// src/client/bin/admin_cli.cc:237
return result.hasError();
```

`bool` 隱式轉 `int`——成功 0，失敗 1。**所有錯誤碼都被壓成 1**，即使 `Status` 內部有 `kWrongUsage(10000)`、`kNotAdmin`、`kChainNotFound` 這些精細的碼。腳本無法區分「參數打錯」與「叢集掛了」。

且如 §4.5 所述，多命令模式下只有最後一條的成敗會反映出來。

### 11.4 錯誤訊息格式

```
Encounter error: 10000(Cli::WrongUsage)
Usage: upload-chains [-d] [--set-preferred-target-order] csv-file-path
```

第一行是 `Encounter error: <數字碼>(<字串碼>)`（`Dispatcher.cc:43`），第二行起是 message。`kWrongUsage` 的 message 就是 usage 字串（`Dispatcher.cc:122-128` 的 `wrongUsage`）。

**這些全部印到 stdout**（§5.4），沒有走 stderr。

---

## 12. 設計取捨與潛在坑

### 12.1 做對的地方

1. **每個命令一個檔案 + 統一簽章**。`getParser()` / `handle(IEnv&, const ArgumentParser&, const Args&)` / `registerXxxHandler(Dispatcher&)` 三件套讓新增命令的成本極低，也讓 72 個命令的程式碼高度可預測。
2. **Lazy client 初始化**。讓 `decode-user-token` 這種純本地命令不需要任何網路連線。
3. **`parserGetter` 存工廠而非物件**。從結構上杜絕了 argparse 狀態污染。
4. **註冊失敗即啟動失敗**。命名衝突在啟動時就爆，不會靜默覆蓋。
5. **`remove-chunks` 的三道保險 + 預設 dry-run**。這是全 CLI 最嚴謹的破壞性操作。
6. **`shutdown-all-chains --check-offline` 的「必須失敗」檢查**。用特定錯誤碼當前置條件，思路很好。
7. **`upload-chain-table` 的冪等**。重跑不產生新版本。

### 12.2 潛在坑

**(a) `-d/--dump-template` 會覆蓋輸入檔**（§10.2）。`upload-chains -d chains.csv` 銷毀 `chains.csv`。應該用獨立的 `--template-output` 參數。

**(b) `;` 不受引號保護**。`Dispatcher.cc:260` 的 `cmd.find_first_of(';')` 在 `ArgsParser` **之前**執行，所以 `create "a;b"` 會被切成 `create "a` 與 `b"`，前者因 `leave with quoted` 報錯。

**(c) 多命令模式吞掉非最後一條的錯誤**（§4.5）。CI 腳本裡的 `admin_cli "A; B"` 若 A 失敗會靜默通過。

**(d) 錯誤走 stdout**（§5.4）。`printError` 從未被呼叫。

**(e) `--skip-config-check` 是死參數**（§9.6）。`InitCluster.cc:131` 硬編碼 `skipConfigCheck = true`，讓設定 schema 驗證永遠不執行。

**(f) `checksum` 可能沒有真的比對副本**（§8.3）。`Checksum.cc:69` 的 `targetIndex` 硬編碼 0，迴圈 `i` 沒有傳進去。若這是 bug，`checksum` 給出的「一致」結論是無效的。

**(g) 沒有 enable/disable node 命令**。`IMgmtdClientForAdmin` 明明宣告了 `enableNode` / `disableNode`（`IMgmtdClientForAdmin.h:23-25`），但 CLI 裡完全沒有呼叫點。實際的做法是透過 tag：

```cpp
// src/common/app/AppInfo.h:129-130
inline constexpr auto kDisabledTagKey = "Disable";
inline constexpr auto kTrafficZoneTagKey = "TrafficZone";
```

```cpp
// src/mgmtd/ops/SetNodeTagsOperation.cc:44-49
if (findTag(node.tags, flat::kDisabledTagKey) == -1 && node.status == flat::NodeStatus::DISABLED) {
  node.status = flat::NodeStatus::HEARTBEAT_CONNECTING;
}
if (findTag(node.tags, flat::kDisabledTagKey) != -1 && node.status != flat::NodeStatus::DISABLED) {
  node.status = flat::NodeStatus::DISABLED;
}
```

也就是 `admin_cli set-node-tags -n 5 update Disable` 才是停用節點的方式，`set-node-tags -n 5 remove Disable` 是重新啟用。`SetNodeTags` 的 parser 有把這兩個 key 寫進 description（`SetNodeTags.cc:17, 27`），但這是唯一的線索——**功能被藏在一個通用的 tag 命令背後**。`getUniversalTags`（`ICommonMgmtdClient.h:34`）同樣沒有對應命令。

**(h) 任何用到 MetaClient 的命令都會建 StorageClient**（§6.3）。`ls /` 也會初始化 RDMA 與完整的 storage client。

**(i) `dump-inodes` 用阻塞 sleep 做背壓**（§8.7）。`std::this_thread::sleep_for(50ms)` 在協程裡是反模式，同檔案的 `RecursiveChown` 用 `folly::fibers::Semaphore` 才是正解。

**(j) `create-targets` 缺少 null 檢查**。`CreateTargets.cc:47` 的 `chainInfo->targets.size()` 沒有先驗 `chainInfo != nullptr`。

**(k) `cd` 的已知限制被寫在註解裡但沒修**：

```cpp
// src/client/cli/admin/Chdir.cc:25-27
// TODO: now cd can't correctly handle many cases e.g.:
// - path contains symlink.
// - and so on.
```

**(l) `linenoise` 歷史不持久化**（§4.4）。沒有 `linenoiseHistoryLoad/Save`。

**(m) `usageHint` 的 `substr(7)` 魔數**（§4.4）。硬綁 argparse 的 `"Usage: "` 前綴，格式一變就越界。

**(n) token 明文輸出**。`user-list`（`AdminUserCtrl.cc:322`）與 `printUserInfo`（`:340`）都會把 token 印到 stdout。`user-list --json` 用的 `UserAttrLite` 刻意不含 token（`:300-305`），但這個保護只在 JSON 模式生效。

**(o) `--display-chunks` 對大檔案會發出海量 RPC**。`stat --display-all-chunks` 會對 layout 的每條 chain 發 query，沒有上限也沒有進度顯示。

**(p) 輸出機制五分天下**（§5.3）。表格、`std::cout`、檔案、Parquet、XLOG（預設寫檔案）混用，且沒有任何一致性標記。

### 12.3 對比：如果重新設計

從這份程式碼可以看出幾個明確的改進方向：

1. **把 `IEnv` + `dynamic_cast` 換成模板**。`IEnv` 是空基底類別（`IEnv.h:5-7`），每個 handler 第一行都是 `dynamic_cast<AdminEnv&>(ienv)`，`Dispatcher` 也只服務這一個 env 型別。這是為了未來可能的第二種 CLI 而付出的抽象成本，但那個未來從未到來。
2. **`OutputTable` 應該帶欄位型別**。`vector<vector<String>>` 讓數字無法右對齊、無法排序、無法輸出 JSON。一個 `variant<String, int64_t, ...>` 或加一層 `ColumnSpec` 就能同時支援表格與 JSON 輸出。
3. **危險命令應有共用的確認機制**。可以是 `Dispatcher` 層的一個 `bool destructive` 旗標，配合全域的 `--yes` / `HF3FS_ADMIN_ASSUME_YES`。
4. **exit code 應反映錯誤類別**。至少區分 usage 錯誤（2）、認證失敗（77）、叢集錯誤（1）。

---

## 13. 檔案索引

### 13.1 `src/client/bin/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `admin_cli.cc` | 238 | 進入點：`Config` 定義、`AdminEnv` 組裝（7 個 lazy getter）、註冊命令、跑 dispatcher、回傳 exit code |
| `CMakeLists.txt` | 1 | `target_add_bin(admin_cli "admin_cli.cc" admin-cli)` |

### 13.2 `src/client/cli/common/`（分派框架）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `Dispatcher.h` | 51 | `Dispatcher` 介面：`Args`/`OutputTable`/`ParserGetter`/`Handler` 型別別名、三個 `registerHandler` 多載、兩個 `run` |
| `Dispatcher.cc` | 296 | 分派表 `std::map<String, unique_ptr<HandlerInfo>>`、`help` 硬編碼處理、linenoise 補全與 hint 回呼、互動式與一次性兩種 `run` |
| `Parser.h` | 23 | `ArgsParser` 介面（`parseOne` / `parseAll`） |
| `Parser.cc` | 66 | 字元級狀態機：處理引號、反斜線跳脫、空白分隔 |
| `Printer.h` | 29 | `Printer`：stdout/stderr 兩個 handler、三個 `print` 多載 |
| `Printer.cc` | 27 | 兩趟掃描的欄位對齊（`separator = "  "`，全左對齊） |
| `IEnv.h` | 9 | 空基底類別，靠 `dynamic_cast` 取回具體 env |
| `Utils.h` | 10 | `ENSURE_USAGE` 巨集（丟 `StatusException(CliCode::kWrongUsage)`） |
| `CMakeLists.txt` | 2 | `target_add_lib(common-cli common)` |

### 13.3 `src/client/cli/admin/`（共用基礎設施）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `AdminEnv.h` | 25 | `AdminEnv : IEnv`：`userInfo`、`currentDirId`/`currentDir`（shell 狀態）、6 個 client getter + kvEngineGetter |
| `registerAdminCommands.h` | 8 | 宣告 `registerAdminCommands(Dispatcher&)` |
| `registerAdminCommands.cc` | 139 | 63 行 `#include` + 65 次 `registerXxxHandler` 呼叫，唯一的命令總表 |
| `Utils.h` | 11 | 宣告 `statNode` / `statChainInfo` |
| `Utils.cc` | 38 | `statNode`（節點屬性表格化）、`statChainInfo`（chain 屬性表格化，被 4 個命令共用） |
| `Layout.h` | 67 | `addLayoutArguments`（5 個 layout 參數）+ `parseLayout`（支援以既有 layout 為 base 做增量修改） |
| `FileWrapper.h` | 83 | `Block` struct + `FileWrapper` 類別：`prepareBlocks` / `replicasNum` / `showChunks` / `readFile` / `writeFile` / `openOrCreateFile` |
| `FileWrapper.cc` | 287 | 檔案 I/O 的共用實作：offset → (chainId, chunkId, offset, length) 切塊、批次讀寫、checksum 合併、洞的補零、256MB RDMA buffer pool |
| `CMakeLists.txt` | 1 | `target_add_lib(admin-cli mgmtd meta common-cli mgmtd-client meta-client storage-client core-client kv apache_arrow_static chunk_engine)` |

### 13.4 `src/client/cli/admin/`（命令實作，每檔一命令除非另註）

> 所有 `.h` 除下表另註者外，一律是 8 行的 `#pragma once` + `class Dispatcher;` 前向宣告 + `registerXxxHandler` 宣告。

| 檔案（`.cc`） | 行數 | 命令 | 職責 |
|---|---|---|---|
| `AdminUserCtrl.cc` | 402 | **8 個** user 命令 | `ensureAdmin` 共用檢查 + `UserAdd/Remove/Set/SetToken/Stat/List/Switch` + `CurrentUser` 八個 handler struct |
| `Bench.cc` | 137 | `bench` | 寫入後用 HeadTarget 與 TailTarget 各讀一次比對 checksum 的壓測 |
| `Chdir.cc` | 75 | `cd` | 改 `env.currentDirId`/`currentDir`；支援 `--inode` 直接跳 inode |
| `Checksum.cc` | 169 | `checksum` | 每副本讀一遍比對 checksum，支援 MD5、檔案清單、批次併發 |
| `Create.cc` | 51 | `create` | 建檔（可指定 layout 與八進位權限） |
| `CreateRange.cc` | 81 | `create-range` | 批次建 `<prefix><N>` 檔案，分片併發 |
| `CreateTarget.cc` | 83 | `create-target` | 單一 target 建立，**含 LASTSRV 保護** |
| `CreateTargets.cc` | 80 | `create-targets` | 依 routing info 批次重建某節點磁碟上的 target，LASTSRV 者跳過 |
| `DecodeUserToken.cc` | 36 | `decode-user-token` | 純本地解碼 token（唯一完全不需要任何 client 的命令） |
| `DropUserCache.cc` | 77 | `drop-user-cache` | 手搓 `MetaServiceStub` 逐台敲所有 META 節點 |
| `DumpChains.cc` | 60 | `dump-chains` | 匯出全部 chain，**按副本數分檔** |
| `DumpChainTable.cc` | 54 | `dump-chain-table` | 匯出單張 chain table 為 CSV（格式可直接回灌） |
| `DumpChunkMeta.h/.cc` | 35/262 | `dump-chunkmeta` | `ChunkMetaRow`/`ChunkMetaTable` serde 型別 + 四種 dump/load（binary / JSON / Parquet）+ 併發拉取 |
| `DumpDirEntries.h/.cc` | 29/163 | `dump-dentries` | `DirEntryRow`/`DirEntryTable` + 直讀 FDB 掃 dentry 寫 Parquet（含 UTF-8 修正） |
| `DumpInodes.h/.cc` | 43/342 | `dump-inodes` | `InodeRow`/`InodeTable` + `dumpInodesFromFdb` / `loadInodeFromFiles` / `listFilesFromPath`（被 `find-orphaned-chunks` 與 `remove-chunks` 共用） |
| `DumpSession.cc` | 89 | `dump-session` | 直讀 FDB，逐 shard（256 個）掃 `FileSession` 寫 Parquet |
| `FillZero.cc` | 123 | `fill-zero` | 讀檔找洞、以零填補；支援 `--dry-run` |
| `FindOrphanedChunks.cc` | 182 | `find-orphaned-chunks` | 比對 inode dump 與 chunkmeta dump，**含時序安全檢查** |
| `GetConfig.cc` | 180 | `get-config` | 五種取設定方式（node/client/addr/type/list-versions） |
| `GetLastConfigUpdateRecord.cc` | 83 | `get-last-config-update-record` | 查節點最近一次設定更新的結果 |
| `GetRealPath.cc` | 35 | `getrealpath` | 解析符號連結後的真實路徑 |
| `HotUpdateConfig.cc` | 121 | `hot-update-config` | 熱更新設定，**本地先驗 toml 語法**；`-t` 模式對全部同型節點逐台推 |
| `InitCluster.cc` | 182 | `init-cluster` | 直寫 FDB 初始化根目錄 layout 與四種角色的初始設定 |
| `List.cc` | 70 | `ls` | 分頁列目錄；path 走 `unknownArgs`；**usage 手寫** |
| `ListChains.cc` | 165 | `list-chains` | 列 chain 及彙總健康狀態（SERVING/SYNCING/UNAVAILABLE/SERVING(n/m)） |
| `ListChainTables.cc` | 54 | `list-chain-tables` | 列所有 chain table 版本與副本數集合 |
| `ListClients.cc` | 109 | `list-clients` | 列客戶端 session，按 universalId → description → clientId 排序 |
| `ListGc.cc` | 61 | `gc-list` | 列 GC 垃圾桶（`InodeId::gcRoot()`），解析 gc entry 名稱；**usage 手寫** |
| `ListNodes.cc` | 93 | `list-nodes` | 列節點，含設定版本與 UPTODATE/DIRTY/FAILED 狀態 |
| `ListTargets.cc` | 104 | `list-targets` | 列 target 及 HEAD/MIDDLE/TAIL 角色；`--orphan` 走專用 RPC |
| `Mkdir.cc` | 51 | `mkdir` | 建目錄（可遞迴、可指定 layout 與權限） |
| `OfflineTarget.cc` | 64 | `offline-target` | 讓 target 下線；未給 node-id 時從 routing info 反查 |
| `OpenRange.cc` | 83 | `open-range` | 批次開檔壓測，支援多輪 |
| `ParseTargetMeta.cc` | 66 | `parse-target-meta` | 純本地把 target metadata 二進位檔解成 JSON |
| `PruneSession.cc` | 45 | `session-prune` | 在 CLI 進程內建一個 `SessionManager` 呼叫 `pruneManually()` |
| `QueryChunk.cc` | 128 | `query-chunk` | 查 chunk 在各副本的中繼資料；可從 chunkId 四步反推 chainId；`--read`/`--touch` |
| `ReadBench.cc` | 213 | `read-bench` | 隨機讀/寫吞吐壓測，每秒印一次頻寬 |
| `ReadFile.cc` | 135 | `read-file` | 讀檔，可選 target selection mode、hex 輸出、補洞、MD5 |
| `RecursiveChown.cc` | 233 | `recursive-chown` | 雙 semaphore 的「拿不到配額就自己做」併發遍歷 + chown |
| `RefreshRoutingInfo.cc` | 36 | `refresh-routing-info` | 強制刷新（用 unsafe getter） |
| `RegisterNode.cc` | 41 | `register-node` | 註冊節點 |
| `RemoteCall.cc` | 232 | `remote-call` | serde 反射把四個服務的全部 RPC 方法暴露成 JSON 介面 |
| `Remove.cc` | 35 | `rm` | 刪除（可遞迴） |
| `RemoveChunks.cc` | 205 | `remove-chunks` | 刪除孤兒 chunk；**三道保險 + 預設 dry-run** |
| `RemoveRange.cc` | 77 | `remove-range` | 批次刪檔 |
| `RemoveTarget.cc` | 64 | `remove-target` | 刪除 target |
| `Rename.cc` | 34 | `mv` | 改名/搬移（src 與 dst 共用 `currentDirId` 為基準） |
| `RenderConfig.cc` | 184 | `render-config` | 渲染設定模板；支援完整的 `--mock-*` 離線模式 |
| `RotateAsPreferredOrder.cc` | 38 | `rotate-as-preferred-order` | 依偏好順序重排鏈 |
| `RotateLastSrv.cc` | 38 | `rotate-lastsrv` | 把 LASTSRV target 輪到鏈尾 |
| `ScanTree.cc` | 187 | `scan-tree` | `UnboundedQueue` + `CancellationSource` 的併發子樹遍歷，找壞路徑 |
| `SetConfig.cc` | 72 | `set-config` | 上傳角色設定到 mgmtd |
| `SetLayout.cc` | 41 | `set-layout` | 改目錄預設 layout（先驗證是目錄） |
| `SetNodeTags.cc` | 85 | `set-node-tags` | 改節點或客戶端 tag；**實質上是 enable/disable node 的入口** |
| `SetPermission.cc` | 58 | `set-perm` | 改 uid/gid/perm/iflags（perm 用 `scan<'o'>` 直吃八進位） |
| `SetPreferredTargetOrder.cc` | 43 | `set-preferred-target-order` | 設定 chain 的偏好 target 順序 |
| `ShutdownAllChains.cc` | 98 | `shutdown-all-chains` | 直寫 FDB 把全部 chain 打成離線；`--check-offline` 要求 mgmtd 必須已下線 |
| `Stat.cc` | 194 | `stat` | inode 詳情；`--display-chunks` 借用 `meta::FileOperation` 查每條 chain 的最後 chunk |
| `Symlink.cc` | 34 | `ln` | 建符號連結 |
| `UnregisterNode.cc` | 53 | `unregister-node` | 註銷節點（先驗證 nodeId 與 type 相符） |
| `UpdateChain.cc` | 51 | `update-chain` | 對單一 chain 增/刪 target |
| `UploadChains.cc` | 101 | `upload-chains` | CSV → `SetChainsReq`；含 `-d` 範本輸出 |
| `UploadChainTable.cc` | 90 | `upload-chain-table` | CSV → `SetChainTableReq`；含 `-d` 範本輸出與 ChainId 溢位檢查 |
| `VerifyConfig.cc` | 164 | `verify-config` | 對全部同型節點併發做 cold/hot 兩種套用演練，輸出直方圖 |
| `WriteFile.cc` | 130 | `write-file` | 寫檔（stdin / 檔案 / `--value` 重複填值三種來源），可 truncate |

### 13.5 `src/tools/`（`hf3fs-admin`）

| 檔案 | 行數 | 職責 |
|---|---|---|
| `admin.cc` | 99 | `hf3fs-admin` 進入點：讀 `~/.hf3fs/admin.toml` 與 `~/.hf3fs/admin_token.toml`，用兩個 gflags 布林選命令 |
| `CMakeLists.txt` | 3 | `target_add_bin(hf3fs-admin "admin.cc" admin-commands)` + `add_subdirectory(commands)` |
| `commands/Commands.h` | 9 | 宣告 `setDirLayout` / `createWithLayout` |
| `commands/Layout.h` | 10 | 宣告 `layoutFromFlags()` |
| `commands/Layout.cc` | 38 | 從五個 gflags（`--chain_table_id` 等）建 `meta::Layout` |
| `commands/SetDirLayout.cc` | 15 | `--set_dir_layout`：對 `--dir_path` 呼叫 `MetaClient::setLayout`（一律以 root 為基準） |
| `commands/CreateWithLayout.cc` | 29 | `--create_with_layout`：依 `--create_dir` 決定呼叫 `mkdirs` 或 `create` |
| `commands/CMakeLists.txt` | 1 | `target_add_lib(admin-commands meta-client mgmtd-client)` |

---

## 附錄：命令名 → 原始碼檔案 速查

```
bench                          → Bench.cc                    ls                             → List.cc
cd                             → Chdir.cc                    mkdir                          → Mkdir.cc
checksum                       → Checksum.cc                 mv                             → Rename.cc
create                         → Create.cc                   offline-target                 → OfflineTarget.cc
create-range                   → CreateRange.cc              open-range                     → OpenRange.cc
create-target                  → CreateTarget.cc             parse-target-meta              → ParseTargetMeta.cc
create-targets                 → CreateTargets.cc            query-chunk                    → QueryChunk.cc
current-user                   → AdminUserCtrl.cc            read-bench                     → ReadBench.cc
decode-user-token              → DecodeUserToken.cc          read-file                      → ReadFile.cc
drop-user-cache                → DropUserCache.cc            recursive-chown                → RecursiveChown.cc
dump-chain-table               → DumpChainTable.cc           refresh-routing-info           → RefreshRoutingInfo.cc
dump-chains                    → DumpChains.cc               register-node                  → RegisterNode.cc
dump-chunkmeta                 → DumpChunkMeta.cc            remote-call                    → RemoteCall.cc
dump-dentries                  → DumpDirEntries.cc           remove-chunks                  → RemoveChunks.cc
dump-inodes                    → DumpInodes.cc               remove-range                   → RemoveRange.cc
dump-session                   → DumpSession.cc              remove-target                  → RemoveTarget.cc
fill-zero                      → FillZero.cc                 render-config                  → RenderConfig.cc
find-orphaned-chunks           → FindOrphanedChunks.cc       rm                             → Remove.cc
gc-list                        → ListGc.cc                   rotate-as-preferred-order      → RotateAsPreferredOrder.cc
get-config                     → GetConfig.cc                rotate-lastsrv                 → RotateLastSrv.cc
get-last-config-update-record  → GetLastConfigUpdateRecord.cc scan-tree                     → ScanTree.cc
getrealpath                    → GetRealPath.cc              session-prune                  → PruneSession.cc
help                           → Dispatcher.cc（內建）        set-config                     → SetConfig.cc
hot-update-config              → HotUpdateConfig.cc          set-layout                     → SetLayout.cc
init-cluster                   → InitCluster.cc              set-node-tags                  → SetNodeTags.cc
list-chain-tables              → ListChainTables.cc          set-perm                       → SetPermission.cc
list-chains                    → ListChains.cc               set-preferred-target-order     → SetPreferredTargetOrder.cc
list-clients                   → ListClients.cc              shutdown-all-chains            → ShutdownAllChains.cc
list-nodes                     → ListNodes.cc                stat                           → Stat.cc
list-targets                   → ListTargets.cc              unregister-node                → UnregisterNode.cc
ln                             → Symlink.cc                  update-chain                   → UpdateChain.cc
upload-chain-table             → UploadChainTable.cc         upload-chains                  → UploadChains.cc
user-add / user-remove / user-set / user-set-token / user-stat / user-list / user-switch    → AdminUserCtrl.cc
verify-config                  → VerifyConfig.cc             write-file                     → WriteFile.cc
```
