# 3FS `monitor_collector_main`（監控收集器）深度剖析

> 對應原始碼：`src/monitor_collector/`（binary 本體，7 檔）、`src/common/monitor/`（埋點與上報框架，15 檔）、`src/analytics/`（結構化 trace log，8 檔）
> 協定定義：`src/fbs/monitor_collector/MonitorCollectorService.h`
> 落地：ClickHouse（`deploy/sql/3fs-monitor.sql`）+ Parquet（trace log）
> 設定：`configs/monitor_collector_main.toml`、各組件的 `[common.monitor]` 區塊

---

## 0. 一句話總結

`monitor_collector_main` 是整個 3FS 叢集裡**唯一持有 ClickHouse 帳密的進程**：它把「每個節點每秒產生一小批 Sample」這種對 MergeTree 最不友善的寫入模式，用一個 32 執行緒、20 萬容量的 MPMC 佇列吸收下來，再合併成最多 4096 批的巨大 `INSERT` block 打進 ClickHouse；而在客戶端這一側，`src/common/monitor/` 用「thread-local 分片累加 + 每秒一次全執行緒 exchange」把埋點的熱路徑壓到一次 relaxed 原子加法，代價是**所有背壓最終都以「靜默丟棄整批 Sample」收場**——監控資料在 3FS 裡被明確地當成可丟失的旁路資料。

---

## 1. 整體分層與端到端資料流

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  業務進程（meta_main / storage_main / mgmtd_main / hf3fs_fuse_main / ...）   │
│                                                                             │
│  【埋點熱路徑】使用者執行緒                                                   │
│    CountRecorder::addSample(1)  ──► TLS.val_.fetch_add()      ← 無鎖          │
│    LatencyRecorder::addSample(ns)──► folly::DigestBuilder     ← CPU-local     │
│    ValueRecorder::set(v)        ──► atomic store                             │
│    OperationRecorder::record()  ──► RAII Guard（total/current/latency/fails） │
│                     │                                                        │
│                     ▼ 註冊時寫入                                              │
│  ┌───────────────────────────────────────────────────────┐                  │
│  │ Collector（進程唯一全域單例，函式內 static）             │                  │
│  │   maps_[bucket] : {(name, TagSet) -> Recorder*}        │                  │
│  │   bucket 數 = num_collectors（預設 1）                  │                  │
│  └───────────────────────────────────────────────────────┘                  │
│                     │  collectAll(bucketIndex, samples, cleanInactive)       │
│                     ▼                                                        │
│  【Collector 執行緒】每 collect_period（預設 1s）跑一次                        │
│    對每個 Recorder 呼叫 collectAndClean() → 產出 std::vector<Sample>          │
│                     │                                                        │
│                     ▼  ProducerConsumerQueue（容量 60 批，滿了「丟棄」）        │
│  【Reporter 執行緒】                                                          │
│    for (reporter : reporters) reporter->commit(samples)                      │
│                     │                                                        │
│                     ▼  MonitorCollectorClient::commit()                      │
│                        folly::coro::blockingWait(MonitorCollector::write())   │
└─────────────────────┼───────────────────────────────────────────────────────┘
                      │  TCP（serde RPC，serviceId=194，methodId=1）
                      │  payload = std::vector<Sample>
                      ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  monitor_collector_main（部署在 meta 節點，可多副本掛在 VIP 後）               │
│                                                                             │
│  net::Server（TCP :10000，group services = ["MonitorCollector"]）             │
│    Processor：max_processing_requests_num=4096、coroutine pool 256           │
│                     │                                                        │
│                     ▼  MonitorCollectorService::write(ctx, samples)          │
│  ┌───────────────────────────────────────────────────────┐                  │
│  │ MonitorCollectorOperator                              │                  │
│  │   folly::MPMCQueue<vector<Sample>>  容量 204800       │                  │
│  │   blockingWrite() ← 滿了「阻塞 RPC 協程」               │                  │
│  └───────────────────────────────────────────────────────┘                  │
│                     │  32 條 connThread 搶（共用一把 mutex，實質序列化）        │
│                     ▼                                                        │
│    聚批：一次最多撈 batch_commit_size(4096) 批 → 合併成一個 vector<Sample>     │
│    過濾：blacklisted_metric_names 黑名單就地剔除                              │
│                     │                                                        │
│                     ▼  Reporter::commit()                                    │
│         ┌────────────────┬────────────────┬──────────────────────┐          │
│         │ ClickHouseClient│  LogReporter   │ MonitorCollectorClient│         │
│         │  (預設)         │  (除錯)         │  (中繼到下一層 collector)│        │
│         └────────┬───────┴────────────────┴──────────────────────┘          │
└──────────────────┼──────────────────────────────────────────────────────────┘
                   ▼
      ┌────────────────────────────────────────┐
      │ ClickHouse（LZ4 壓縮連線）               │
      │   3fs.counters      ← isNumber()  樣本   │
      │   3fs.distributions ← isDistribution()  │
      │   MergeTree / PARTITION BY toDate()     │
      │   TTL 1 month                            │
      └────────────────────────────────────────┘

（另一條完全獨立的旁路）
  meta / storage 進程 ──► analytics::StructuredTraceLog<T> ──► 本地 Parquet 檔
                          （不經過 monitor_collector）
```

兩條鏈路的分工在第 9 章詳述：**metric 走 collector 進 ClickHouse，event trace 走 Parquet 落本地磁碟**，兩者唯一的交集是 trace log 自己也埋了三個 `LatencyRecorder`。

---

## 2. binary 啟動流程

### 2.1 `main` 只有一行

`src/monitor_collector/monitor_collector.cpp:5`：

```cpp
int main(int argc, char *argv[]) {
  return hf3fs::OnePhaseApplication<hf3fs::monitor::MonitorCollectorServer>::instance().run(argc, argv);
}
```

`OnePhaseApplication` 這個名字是相對於 `TwoPhaseApplication`（mgmtd / meta / storage 用的：先向 mgmtd 註冊拿設定，再啟動）。monitor_collector **不需要 mgmtd、不需要 FoundationDB、沒有 node_id**——它是整個叢集裡依賴最少的服務，這也是 `deploy/README.md:56` 把「建 ClickHouse 表」與「裝 monitor 服務」排在 Step 1、Step 2、早於 mgmtd 的原因。

`src/monitor_collector/CMakeLists.txt` 只有兩行：

```cmake
target_add_lib(monitor_collector common memory-common MonitorCollectorService-fbs)
target_add_bin(monitor_collector_main "monitor_collector.cpp" monitor_collector)
```

注意第二個依賴 `memory-common`：`monitor_collector.cpp:2` include 了 `memory/common/OverrideCppNewDelete.h`，在 binary 的 translation unit 裡覆寫全域 `operator new/delete`（`src/memory/common/OverrideCppNewDelete.h:11-17`），讓自訂配置器的記憶體統計生效。這個 header 必須且只能在**恰好一個 .cpp** 裡展開，所以它出現在 `main` 檔而不是任何 library 檔。

### 2.2 `OnePhaseApplication::initApplication()` 的順序

`src/common/app/OnePhaseApplication.h:67-125`，順序有實質意義：

| 步驟 | 行號 | 動作 |
|---|---|---|
| 1 | 68-75 | 解析 `--app_cfg` / `--cfg`，套用 `--config.xxx` 命令列覆蓋 |
| 2 | 83 | `net::IBManager::start()`（即使 monitor 只用 TCP 也會初始化） |
| 3 | 86-88 | 依 `T::kName`（`"MonitorCollector"`）產生 log 設定並 `initOrDie` |
| 4 | 93 | 初始化 `net::Waiter` 單例（RPC 回應等待表） |
| 5 | **95** | **`monitor::Monitor::start(config_.common().monitor())`** |
| 6 | 99-101 | `new MonitorCollectorServer(config)` + `setup()` |
| 7 | 121 | `server_->start(info_)` |

第 5 步值得單獨標記：**monitor_collector 自己也是 Monitor 框架的使用者**。它在啟動 server 之前先啟動自己的 Collector/Reporter 執行緒。而 `configs/monitor_collector_main.toml` 裡**完全沒有 `[common.monitor]` 區塊**——於是 `reporters_length()` 為 0，`Monitor::start()` 會啟一條 Collector 執行緒每秒收集、一條 Reporter 執行緒收下後對空的 reporter 清單迭代（`src/common/monitor/Monitor.cc:163-165`），也就是**自身指標預設收集完就丟掉**。要看到 collector 自己的指標，必須手動在該檔加上 `[[common.monitor.reporters]] type='monitor_collector'` 指向自己或另一台 collector。

### 2.3 `MonitorCollectorServer` 的兩個 hook

`src/monitor_collector/service/MonitorCollectorServer.h:13-19` 用 lambda 改寫了 `net::Server::Config` 的預設值：

```cpp
CONFIG_OBJ(base, net::Server::Config, [](net::Server::Config &c) {
  c.set_groups_length(1);
  c.groups(0).listener().set_listen_port(10000);
  c.groups(0).set_network_type(hf3fs::net::Address::TCP);
  c.groups(0).set_services({"MonitorCollector"});
});
```

**只有一個 group、只走 TCP、固定 10000 埠**。`deploy/README.md:97` 明說「Other services communicate with the monitor service over a TCP connection」——監控流量不佔 RDMA 資源，且 FUSE 客戶端所在的運算節點未必與存儲節點在同一 IB 分區。

`beforeStart()`（`MonitorCollectorServer.cc:13-17`）做兩件事：

```cpp
monitorCollectorOperator_ = std::make_unique<MonitorCollectorOperator>(config_.monitor_collector());
RETURN_ON_ERROR(addSerdeService(std::make_unique<MonitorCollectorService>(*monitorCollectorOperator_), true));
```

`Operator` 先建（建構子裡就把 32 條 conn thread 拉起來，見第 7 章），再把 `Service` 註冊進 group。第二個參數 `true` 是 `strict`，`src/common/net/Server.h:42-51` 會要求該 service 名稱必須在 group 的 `services` 清單裡；因為預設值把 `services` 設成 `{"MonitorCollector"}`，兩者對得上。

`beforeStop()` 只是 `monitorCollectorOperator_.reset()`——`~MonitorCollectorOperator()` 會對 32+1 條 `std::jthread` 逐一 `request_stop()` 再 `join()`（`MonitorCollectorOperator.cc:19-26`）。注意**先 request 全部再 join 全部**，不是 request-join 交替，否則 32 條執行緒要串行等 32 個 5 秒級的喚醒。

---

## 3. 埋點層：Recorder 家族

### 3.1 共同基底

`src/common/monitor/Recorder.h:32-94` 定義了 `Recorder` 抽象基底。三個關鍵成員：

```cpp
std::string name_;      // 指標名，如 "storage.req_read.total"
TagSet tag_;            // 這個 recorder 固定攜帶的標籤
ConcurrentMap map_;     // 動態標籤 -> 子 recorder
MonitorInstance &monitor_;
```

`ConcurrentMap` 的型別（`Recorder.h:55-58`）是：

```cpp
using ConcurrentMap = folly::ConcurrentHashMap<TagSet,
                                               std::unique_ptr<Recorder>,
                                               folly::HeterogeneousAccessHash<TagSet>,
                                               folly::HeterogeneousAccessEqualTo<TagSet>>;
```

也就是說**每一個 Recorder 自己就是一棵兩層樹**：根節點帶靜態標籤，子節點按呼叫端傳進來的動態 `TagSet` 展開。這是 3FS 支援 `addSample(val, tag)` 這種「同一個指標名、不同標籤值」寫法的機制。

### 3.2 動態標籤的建立路徑（含一個 `reinterpret_cast` 陷阱）

`src/common/monitor/Recorder.cc:52-65`：

```cpp
template <typename T>
Recorder::TagRef<T> Recorder::getRecorderWithTag(const TagSet &tag) {
  auto iter = map_.find(tag);
  if (UNLIKELY(iter == map_.end())) {
    TagSet new_tag_set = tag_;
    for (const auto &kv : tag) {
      new_tag_set.addTag(kv.first, kv.second);
    }
    auto newRecorder = std::make_unique<T>(*reinterpret_cast<T *>(this), new_tag_set);
    auto result = map_.insert(tag, std::move(newRecorder));
    iter = std::move(result.first);
  }
  return iter;
}
```

三個要點：

1. **map 的 key 是「呼叫端傳入的 tag」，子 recorder 身上帶的卻是「父 tag ∪ 子 tag」的聯集**。查表用小的、上報用大的，省掉每次查表都要合併字串的成本。
2. `reinterpret_cast<T *>(this)` 沒有型別檢查。`T` 由呼叫端的包裝函式決定（例如 `CountRecorderWithTLSTag::getRecorderWithTag` 固定傳自己的型別，`Recorder.h:119-121`），所以實務上安全，但這是靠呼叫慣例而非型別系統保證的。
3. 這裡有**良性競態**：兩條執行緒同時 miss 會各自 `make_unique` 一個子 recorder，`ConcurrentHashMap::insert` 只有一個會贏，輸的那個 unique_ptr 立刻析構。子 recorder 的建構子走的是「不註冊」版本（各家 `Recorder(parent, tag)` 建構子都傳 `needRegister=false`，例如 `Recorder.h:110-111`、`166-167`），所以輸家析構時不會去動全域 Collector——這是為什麼子 recorder 必須不註冊的真正原因：不是為了省事，是為了讓競態下的即棄物件無副作用。

### 3.3 Recorder 完整型別表

| 類別 | 定義位置 | 聚合語意 | 上報成 | thread-local? | 預設 reset |
|---|---|---|---|---|---|
| `CountRecorderWithTLSTag<Tag>` | `Recorder.h:96-154` | 累加 `int64_t` | `counters`（單一 int64） | 是（`folly::ThreadLocal<TLS, Tag>`） | true |
| `CountRecorder` | `Recorder.h:159`（= `<SharedThreadLocalTag>`） | 同上 | 同上 | 是 | true |
| `DistributionRecorder` | `Recorder.h:161-191` | TDigest → cnt/sum/min/max/p50/p90/p95/p99 | `distributions` | 是（folly DigestBuilder 內部 CPU-local） | 恆 reset |
| `LatencyRecorder` | `Recorder.h:193-209` | 同上，輸入強制為 `std::chrono::nanoseconds` | `distributions` | 同上 | 恆 reset |
| `SimpleDistributionRecorder` | `Recorder.h:211-282` | 只算 cnt/sum/min/max，**無分位數** | `distributions`（p50/p90/p95/p99 全為 0） | 是（4 個 atomic） | 恆 reset |
| `OperationRecorderT<L>` | `Recorder.h:284-346` | 組合器：total + fails + current + succ_latency + fail_latency | 前三者進 counters、後兩者進 distributions | 繼承成員 | 見下 |
| `ValueRecorder` | `Recorder.h:351-386` | 直接 store 一個瞬時值 | `counters` | 否（單一 atomic） | 可選 |
| `LambdaRecorder` | `Recorder.h:388-411` | 收集時才呼叫使用者 lambda 求值 | `counters` | 否（mutex 保護 `std::function`） | 由 lambda 自理 |

### 3.4 `CountRecorder`：thread-local 分片 + 全執行緒 exchange

熱路徑只有一句（`Recorder.h:116`）：

```cpp
void addSample(int64_t val) { tls_->addSample(val); }
```

`TLS::addSample` 是 `val_ += val`，對 `std::atomic<int64_t>` 用預設的 seq_cst `+=`（`Recorder.h:140`）。由於是 thread-local 物件，這條 cache line 不會被別的執行緒碰到，實際成本就是一次無爭用的 `lock add`。

收集端（`Recorder.cc:79-103`）：

```cpp
int64_t sum = resetWhenCollect_ ? sum_.exchange(0) : sum_.load();
for (auto &tls : tls_.accessAllThreads()) {
  sum += resetWhenCollect_ ? tls.exchange() : tls.load();
}
...
if (sum) {
  TagSet sample_tag = tag_;
  sample_tag.addTag("host", gHostname);
  sample_tag.addTag("pod", gPodname);
  samples.emplace_back(Sample{name_, std::move(sample_tag), now, sum});
  cumulativeVal_ += sum;
  active_ = true;
}
```

`folly::ThreadLocal` 的 `accessAllThreads()` 會取一把全域讀寫鎖並走訪所有還活著的 TLS 實例。已結束的執行緒在 `~TLS()` 裡把殘值加回父物件的 `sum_`（`Recorder.h:139`），**所以執行緒退出不會漏帳**。

三個副作用值得記住：

- **`if (sum)` ——值為 0 就完全不產生 Sample**。counter 為 0 的那一秒在 ClickHouse 裡是**沒有列**，不是值為 0 的列。畫圖時的空洞需要在查詢端補零。
- `resetWhenCollect_=false` 的用法（`Recorder.h:251` `OperationRecorderT` 的 `current_`）是把 counter 當 gauge 用：`record()` 時 `+1`、Guard 析構時 `-1`，收集時只 `load` 不清零。`docs/metrics.md` 表格裡「Reset After Report = N」的那些指標全是這一類。
- `logPer30s`：`Recorder.cc:96-102` 每 30 秒把累計值打一行 `DBG3` log。這是離線除錯用的後備通道，正式環境的日誌等級是 `INFO`（`configs/monitor_collector_main.toml:14`），所以不會輸出。

### 3.5 `DistributionRecorder`：TDigest 與分位數

熱路徑（`Recorder.h:171`）：

```cpp
void addSample(double value) { tdigest_.append(value); }
```

`tdigest_` 的型別是 **`folly::DigestBuilder<folly::TDigest>`**（`Recorder.h:189`），參數：

```cpp
static constexpr size_t kDigestMaxSize = 512;      // TDigest centroid 上限
static constexpr size_t kDigestBufferSize = 128_KB; // 緩衝的「元素個數」
```

`kDigestBufferSize` 是**元素個數不是位元組數**（folly `DigestBuilder(bufferSize, digestSize)` 的 bufferSize 語意是 buffer 的 `reserve` 容量）。131072 個 `double` = 1 MB 的 CPU-local 緩衝，**每個 recorder、每個 CPU cache line group 各一份**。以 storage 節點動輒數十個 `LatencyRecorder` 來算，這是實打實的常駐記憶體；不過 folly 的實作只在真的寫滿一次之後才把容量拉到 `bufferSize_`（`third_party/folly/folly/stats/DigestBuilder-inl.h` 的 `nextPowTwo(...)` 邏輯），冷門指標不會真的佔到 1 MB。

收集端（`Recorder.cc:121-160`）把 TDigest 攤平成 8 個 double：

```cpp
auto digest = tdigest_.build();
if (!digest.empty()) {
  Distribution dist;
  dist.cnt = digest.count();
  dist.sum = digest.sum();
  dist.min = digest.min();
  dist.max = digest.max();
  dist.p50 = digest.estimateQuantile(0.50);
  dist.p90 = digest.estimateQuantile(0.90);
  dist.p95 = digest.estimateQuantile(0.95);
  dist.p99 = digest.estimateQuantile(0.99);
  ...
}
```

**分位數是在客戶端算完才上報的**。這意味著：

- ClickHouse 裡存的 p99 是「該節點該秒的 p99」。跨節點或跨時間**不能對 p99 取平均**——這是所有 pre-aggregated 分位數系統共有的限制，3FS 沒有上報 digest 本身，所以查詢端無法重新合併。
- `mean` 甚至不是欄位：`Distribution::mean()` 是 `sum/cnt` 的推導值（`Sample.h:93`），`ClickHouseClient` 在插入時才算（`ClickHouseClient.cc:123`）。
- `build()` 是**破壞性的**：`folly::DigestBuilder::build()` 把緩衝換掉並清空，所以 distribution 類永遠是「兩次收集之間」的視窗統計，沒有 `resetWhenCollect` 選項。

`LatencyRecorder` 只是 `DistributionRecorder` 的薄包裝（`Recorder.h:193-209`），做兩件事：把 `addSample(double)` 和 `addSample(uint64_t, TagSet)` **`= delete`** 掉，強制呼叫端傳 `std::chrono::nanoseconds`；以及覆寫 `logPer30s` 把單位換成 µs 印出。這個 `= delete` 是很典型的防呆：延遲指標最常見的 bug 就是有人不小心傳了毫秒。

### 3.6 `SimpleDistributionRecorder`：不要分位數時的省錢版

`Recorder.h:242-273` 的 TLS 用 4 個 `std::atomic<int64_t>`（sum/count/min/max）取代 TDigest，全部用 `memory_order_relaxed`。它的 min/max 更新（`Recorder.h:258-259`）是 load-min-store 而非 CAS 迴圈：

```cpp
min_.store(std::min(min_.load(std::memory_order_relaxed), val), std::memory_order_relaxed);
```

因為是 thread-local 物件，沒有競爭，這樣寫是對的。但在 `~Tls()`（`Recorder.h:246-253`）把值合併回父物件時用的是**同一套 load-min-store**，而父物件是多執行緒共享的——多條執行緒同時退出時 min/max 可能丟更新。這是刻意的取捨：min/max 是輔助資訊，不值得為它付 CAS 迴圈。

收集端的守衛條件是 `if (count != 0 && sum != 0)`（`Recorder.cc:203`），比 CountRecorder 更嚴：**如果一秒內全部樣本的和恰好是 0（例如記錄的是有正有負的差值），整批統計會被丟掉**。

### 3.7 `OperationRecorder`：RAII 的操作組合器

`Recorder.h:284-346` + `Recorder.cc:245-301`。一個 `OperationRecorder{"storage.req_read"}` 會展開成 5 個底層 recorder（`Recorder.cc:249-253`）：

```cpp
total_(fmt::format("{}.total", name), tag),
fails_(fmt::format("{}.fails", name), tag),
current_(fmt::format("{}.current", name), tag, /*resetWhenCollect=*/false),
succ_latencies_(fmt::format("{}.succ_latency", name), tag),
fail_latencies_(fmt::format("{}.fail_latency", name), tag),
```

這正好解釋了 `docs/metrics.md:59-63` 那組成套出現的指標名。用法（`src/storage/service/StorageOperator.cc:87`）：

```cpp
auto recordGuard = storageReqReadRecorder.record(monitor::instanceTagSet(std::to_string(req.userInfo.uid)));
```

`record()` 立刻對 `total_` 和 `current_` 各 `+1`，回傳 `[[nodiscard]] Guard`。Guard 析構時（`Recorder.h:295`）呼叫 `report(false)`，除非中途呼叫過 `succ()`。三個逃生口：

- `succ()`：標記成功，之後析構時計成功延遲。
- `reportWithCode(code)`：明確帶狀態碼上報，`code == kOK` 視為成功。
- `dismiss()`：只把 `current_` 減回去，**不計入延遲分佈**——用於「這次操作不算數」的路徑。

`recordErrorCode_` 開啟時（`Recorder.cc:269-276`）會把狀態碼字串塞進 `statusCode` 標籤。這裡有一個很有意思的優化：

```cpp
thread_local std::array<String, 65536> errorCodeStrings;
thread_local std::array<std::optional<TagSet>, 65536> errorCodeTagSets;
```

`Recorder.cc:20-21`。**兩個各 65536 槽的 thread-local 陣列**，用狀態碼直接當索引快取「碼 → 字串」與「碼 → TagSet」。`StatusCode::toString()` 與 `TagSet::create()` 都要配置記憶體，而失敗路徑常常是暴風式的（一台存儲掛了，每秒幾萬個同樣的錯誤碼），這個表把它降成一次陣列索引。代價是每條執行緒 64K × (sizeof(String) + sizeof(optional<TagSet>)) 的虛擬位址空間——因為是稀疏觸碰，實體記憶體只有真正出現過的錯誤碼那幾頁。

### 3.8 `ValueRecorder` 與 `LambdaRecorder`

`ValueRecorder::collect`（`Recorder.cc:308-318`）：

```cpp
int64_t val = resetWhenCollect_ ? val_.exchange(0) : val_.load();
if (val > 0) { ... }
```

**`val > 0` 而不是 `val != 0`**。這是全套 Recorder 裡最容易踩的坑：`storage.target_state` 是 `ValueRecorder`（`src/storage/service/Components.cc:15`），語意見 `docs/metrics.md:108`「0:invalid 1:uptodate 2:online 4:offline」——**target 進入 invalid(0) 狀態時，這個指標根本不會上報**，ClickHouse 裡看到的會是它最後一次非零的狀態值一路延續。同理 `storage.disk_info.read_only`（`src/storage/worker/CheckWorker.cc:46`）只有在 read_only=1 時才有資料點，恢復可寫時不會有 0 的資料點。

`LambdaRecorder`（`Recorder.cc:329-339`）是「收集時求值」：

```cpp
void LambdaRecorder::collect(std::vector<Sample> &samples) {
  auto lock = std::unique_lock(mutex_);
  auto value = getter_();
  lock.unlock();
  if (value) { ... }
}
```

用途見 `src/storage/service/StorageOperator.cc:61-65`：

```cpp
storageReadAvgBytes.setLambda([&] {
  auto totalReadBytes = totalReadBytes_.exchange(0);
  auto totalReadIOs = totalReadIOs_.exchange(0);
  return totalReadBytes / std::max(1ul, totalReadIOs);
});
```

也就是「平均值必須在同一時刻同時取分子分母」的場合。注意 lambda 是在 **Collector 執行緒**上執行的，且持有 `mutex_`——lambda 裡不能做阻塞操作，否則會拖垮整個進程的指標收集。`StorageOperator::stopAndJoin()`（`StorageOperator.cc:76`）第一件事就是 `storageReadAvgBytes.reset()`，把 lambda 換成 `[]{return 0;}`，避免 Collector 執行緒在物件析構後還持有捕獲的 `this`。

### 3.9 `ScopedMetricsWriter`：兩個 RAII 小工具

`src/common/monitor/ScopedMetricsWriter.h`：

- `ScopedLatencyWriter`（:48-71）：建構記時間、析構算 elapsed 丟給 `LatencyRecorder`。用 `SteadyClock`。
- `ScopedCounterWriter`（:7-46）：建構時 `counter += value` 並把**新的計數值**丟給一個 `DistributionRecorder`，析構時 `counter -= value` 再丟一次。這是在做「並發度的分佈統計」——不是統計某個計數器的值，而是統計「進出臨界區時的瞬間並發度」的分佈，能算出 p99 併發度。

### 3.10 全域自我監控：記憶體配置器

`src/memory/common/AllocatedMemoryCounter.cc:12` 定義：

```cpp
using AllocatedMemoryCountRecorder = monitor::CountRecorderWithTLSTag<monitor::AllocatedMemoryCounterTag>;
```

這是 `CountRecorderWithTLSTag` 為什麼要模板化 TLS tag 的唯一原因。`folly::ThreadLocal<T, Tag>` 用 Tag 區分不同的 TLS arena；記憶體計數器如果和一般業務指標共用同一個 arena，**arena 自身的擴容會呼叫 `operator new`，而 `operator new` 又會去更新記憶體計數器 → 無限遞迴**。分開 Tag 就把兩個 arena 的生命週期解耦了。

同一個模板還特化了收集行為（`Recorder.cc:110-113`）：

```cpp
template <>
void CountRecorderWithTLSTag<AllocatedMemoryCounterTag>::collectAndClean(std::vector<Sample> &samples, bool) {
  Recorder::collectAndClean(samples, false /*cleanInactive*/);
}
```

**強制忽略 `cleanInactive` 參數**。記憶體 bucket 的子 recorder（`AllocatedMemoryCounter.cc:57-80` 為每個 size bucket 建 7 個 recorder）就算 5 分鐘沒動也絕不能被清掉——`memory_allocator.used_bytes` 是 `resetWhenCollect=false` 的累計量，清掉就等於把已配置的記憶體帳歸零。

---

## 4. TagSet 與 Sample 的資料模型

### 4.1 `TagSet`

`src/common/monitor/Sample.h:15-68`：

```cpp
class TagSet {
 public:
  using Map = folly::sorted_vector_map<std::string, std::string>;
  void addTag(std::string tagName, std::string tagValue) { tag_set_.emplace(tagName, tagValue); }
  ...
 private:
  SERDE_CLASS_FIELD(tag_set, Map{});
};
```

四個設計決策：

1. **`folly::sorted_vector_map` 而非 `std::map`/`unordered_map`**。標籤數量極小（實務上 1~4 個），排序向量在這個規模下的查找、迭代、比較全面優於紅黑樹，而且**記憶體連續**——這對「每個 Sample 都要複製一份 TagSet」的場景很關鍵。
2. **有序 ⇒ 相等比較與 hash 是確定性的**。`std::hash<TagSet>` 特化（`Sample.h:113-118`）直接 `folly::hash::hash_range(cbegin, cend)`，不需要先排序或做交換律 hash。這是 `Recorder::map_` 能用 `ConcurrentHashMap<TagSet, ...>` 的前提。
3. **`addTag` 用 `emplace` 而不是 `insert_or_assign`——已存在的 key 不會被覆寫**。收集時 `sample_tag.addTag("host", gHostname)`（`Recorder.cc:139`）如果該 recorder 自己已經帶了 `host` 標籤，全域 hostname **不會**蓋掉它。這在寫 recorder 時是個沉默的陷阱：自訂 `host`/`pod` 標籤會贏。
4. **值型別是 `std::string`，全部深複製**。`TagSet::newTagSet` 是 copy + add（`Sample.h:33-37`）。

序列化成本：`SERDE_CLASS_FIELD` 讓整個 map 直接進 serde。每一個 Sample 都攜帶完整的 key 與 value 字串——`"host"` 這 4 個字元 + hostname 本身，**每個指標每秒重複一次**。一台存儲節點若有 2000 個活躍的 (name, tag) 組合，光是 host/pod 標籤每秒就重送 2000 次。3FS 沒有做字串 interning 或 tag 字典，換來的是協定極簡與 collector 端零狀態。

兩個工廠函式（`Sample.h:70-78`）把最常用的兩個標籤名固化下來：

```cpp
inline TagSet instanceTagSet(std::string instance);   // {"instance": ...}
inline TagSet threadTagSet(const std::string_view&);  // {"thread": ...}
```

`instance` 是 3FS 裡的萬用維度：uid（`StorageOperator.cc:87`）、磁碟編號（`StorageTarget.cc:68`）、target id（`StorageTarget.cc:156`）、IB 裝置名（`StorageOperator.cc:205`）、記憶體 bucket 大小（`AllocatedMemoryCounter.cc:61`）全都塞在同一個 `instance` 標籤裡。這也是為什麼 ClickHouse schema 裡 `instance` 是唯一沒有 `LowCardinality` 修飾的標籤欄位（見 4.3）。

### 4.2 `Sample` 與 `Distribution`

`Sample.h:96-109`：

```cpp
struct Sample {
  SERDE_STRUCT_FIELD(name, std::string{});
  SERDE_STRUCT_FIELD(tags, TagSet{});
  SERDE_STRUCT_FIELD(timestamp, UtcTime{});
  SERDE_STRUCT_FIELD(value, (std::variant<int64_t, Distribution>{}));
};
```

**整個監控系統的資料模型就這四個欄位**，而且 `value` 是一個只有兩個備選的 variant。這個決策一路傳導到最後：ClickHouse 就是兩張表，`ClickHouseClient::commit` 開頭就是按 `isNumber()` 分成兩堆（`ClickHouseClient.cc:64-72`）。想加第三種指標型別（例如直接存 histogram bucket）要改動整條鏈路。

`Distribution`（`Sample.h:80-94`）是 8 個 `double`（cnt/sum/min/max/p50/p90/p95/p99），連 count 都用 double——為了跟 TDigest 的回傳型別一致，避免轉換。

`timestamp` 是 `UtcTime`，但落地時 `UtcClock::to_time_t(sample.timestamp)`（`ClickHouseClient.cc:79`）截斷到**秒**，ClickHouse 欄位型別是 `DateTime`（秒精度）。收集週期預設就是 1 秒，剛好對齊；但如果把 `collect_period` 調到 500ms，**同一秒的兩批資料會有相同的 timestamp**，在 ClickHouse 裡變成兩列無法區分的資料。

### 4.3 ClickHouse 表結構

`deploy/sql/3fs-monitor.sql`：

```sql
CREATE TABLE IF NOT EXISTS 3fs.counters (
  `TIMESTAMP` DateTime CODEC(DoubleDelta),
  `metricName` LowCardinality(String) CODEC(ZSTD(1)),
  `host` LowCardinality(String) CODEC(ZSTD(1)),
  `tag` LowCardinality(String) CODEC(ZSTD(1)),
  `val` Int64 CODEC(ZSTD(1)),
  `mount_name` LowCardinality(String) CODEC(ZSTD(1)),
  `instance` String CODEC(ZSTD(1)),
  `io` LowCardinality(String) CODEC(ZSTD(1)),
  `uid` LowCardinality(String) CODEC(ZSTD(1)),
  `pod` String CODEC(ZSTD(1)),
  `thread` LowCardinality(String) CODEC(ZSTD(1)),
  `statusCode` LowCardinality(String) CODEC(ZSTD(1))
)
ENGINE = MergeTree
PRIMARY KEY (metricName, host, pod, instance, TIMESTAMP)
PARTITION BY toDate(TIMESTAMP)
ORDER BY (metricName, host, pod, instance, TIMESTAMP)
TTL TIMESTAMP + toIntervalMonth(1)
SETTINGS index_granularity = 8192;
```

`distributions` 表（:24-51）把 `val` 換成 `count/mean/min/max/p50/p90/p95/p99` 八個 `Float64`，並**多一個 `method` 欄位**。

四個觀察：

1. **標籤不是 map，是寫死的欄位清單**。`counters` 認得 8 個標籤名（host, tag, mount_name, instance, io, uid, pod, thread, statusCode），`distributions` 多一個 `method`。程式碼裡新增一個沒在表裡的標籤名，`ClickHouseClient::commit` 會在 `Insert` 時丟例外 → **整批 4096 個 Sample 全部丟失**（見第 8 章）。加標籤等於要 `ALTER TABLE`。
2. `instance` 和 `pod` 是唯二**沒有** `LowCardinality` 的字串欄位。`instance` 被拿來裝 uid、target id 等高基數值，`pod` 在容器環境下每次重啟都變——這兩個位置正是基數爆炸的風險點，schema 已經預先承認了。
3. `PARTITION BY toDate` + `TTL 1 month` ⇒ 保留 30 個 partition，過期整塊 drop。
4. `ORDER BY (metricName, host, pod, instance, TIMESTAMP)` ⇒ 「查單一指標的時間序列」是連續掃描；「查某台機器的所有指標」則要掃全表。這個排序假設了主要查詢模式是 dashboard 式的「先選指標再選機器」。
5. `distributions` 的 `method` 欄位在目前的程式碼裡沒有任何 recorder 使用（全 repo 搜不到以 `method` 為 key 的 `TagSet`），屬於歷史遺留；反過來 `counters` 表**沒有** `method` 欄位，若哪天有人在 counter 上加 `method` 標籤會直接炸掉插入。

---

## 5. 進程內收集與定期上報

### 5.1 `Collector`：分桶的全域註冊表

`src/common/monitor/Monitor.cc:44-47`：

```cpp
struct Collector::Impl {
  std::shared_mutex resizeMutex_;
  std::vector<std::unique_ptr<LockableRecorderMap>> maps_;
};
```

其中 `LockableRecorderMap = folly::Synchronized<robin_hood::unordered_map<RecorderKey, Recorder*, RecorderKeyHash>, std::mutex>`（`Monitor.cc:40-41`），`RecorderKey = std::pair<std::string, TagSet>`。

註冊（`Monitor.cc:56-68`）：

```cpp
void Collector::add(const std::string &name, Recorder &var) {
  auto key = std::make_pair(name, var.tag_);
  auto hash = RecorderKeyHash()(key);
  auto lock = std::shared_lock(impl->resizeMutex_);
  auto map = impl->maps_[hash % impl->maps_.size()]->lock();
  auto [it, succeed] = map->try_emplace(std::move(key), &var);
  XLOGF_IF(FATAL, !succeed, "Monitor two variables with same name and same tag: {}...", name, ...);
}
```

**重名同標籤 ⇒ `FATAL`，進程直接掛掉**。這是刻意的嚴格：兩個 recorder 撞名會讓 ClickHouse 裡出現無法區分的兩條時間序列，寧可在啟動時炸掉。由於絕大多數 recorder 是檔案作用域 static，這個檢查在 `main` 之前就會生效。

`resize(n)`（`Monitor.cc:95-117`）在 `Monitor::start` 時依 `num_collectors` 把桶數改成 n，並把既有 recorder 重新分桶。它用 `unique_lock(resizeMutex_)` 排除所有 `add`/`del`。**注意 `collectAll` 沒有取 `resizeMutex_`**（`Monitor.cc:83-93`）——resize 只在啟動時發生一次、且發生在 collect 執行緒建立之前（`Monitor.cc:186` 早於 `:220`），所以實務上安全，但這是靠時序而非鎖保證的。

### 5.2 `MonitorInstance` 與那個共享的 `getCollector()`

`src/common/monitor/Monitor.h:70-73`：

```cpp
Collector &getCollector() {
  static Collector collector;
  return collector;
}
```

這是**非 static 成員函式裡的 function-local static**——所有 `MonitorInstance` 物件共用同一個 `Collector`。所以 `Monitor::createMonitorInstance()`（`Monitor.cc:175`）雖然能造出第二個 MonitorInstance（有自己的執行緒與 reporter），它**看到的仍然是同一張全域 recorder 表**。目前 repo 裡沒有任何呼叫者使用 `createMonitorInstance()`，這條路徑是預留的；但如果真的用了，兩個 instance 會對同一批 recorder 各自呼叫 `collect()`，而 `collect()` 是破壞性的（`exchange(0)`、`tdigest_.build()`），結果是**兩邊各拿到一半的資料**。

`Monitor::getDefaultInstance()`（`Monitor.cc:177-180`）是真正的單例。所有 recorder 的便利建構子都預設走它（例如 `Recorder.cc:71`、`:119`、`:304`、`:325`）。

### 5.3 未初始化時的行為

Recorder 大多是檔案作用域 static，在 `main` 之前就建構完成並註冊；`Monitor::start()` 則要等到 `initApplication()` 才呼叫。這段空窗期裡：

- 埋點照常運作，數值累積在各自的 TLS/atomic/TDigest 裡。
- 沒有任何執行緒在收集，所以資料**不會丟，但也不會上報**。
- 第一次 `collect()` 會把啟動至今累積的全部量報成「那一秒的值」——**開機第一個資料點必然是尖峰**。畫圖時通常要忽略首個資料點。
- 若 `Monitor::start()` 從未被呼叫（例如某些工具型 binary），TDigest 緩衝會持續成長到 `kDigestBufferSize` 上限後開始 merge，記憶體有界；CountRecorder 的 int64 則可能溢位。

`Monitor.cc:25-33` 有一個很容易被忽略的小物件：

```cpp
class DummyVariable : public Recorder {
  ...
} dummyVariable("dummy");  // keep variables alive until reporter thread stopped.
```

這個 namespace 作用域的 static 在載入期建構，它的建構子會先觸發 `Monitor::getDefaultInstance()` 再觸發 `getCollector()`，於是這兩個 function-local static **的建構時機被強制提早到 `dummyVariable` 之前**。C++ 保證 static 物件以建構的逆序析構，因此 `dummyVariable` 會比 Collector 和 MonitorInstance 更早被銷毀——註解說的「keep variables alive until reporter thread stopped」就是這個效果。實務上 `ApplicationBase::stopAndJoin`（`src/common/app/ApplicationBase.cc:281`）會在 server 停掉後明確呼叫 `monitor::Monitor::stop()`，不依賴這個析構順序技巧。

### 5.4 收集執行緒

`MonitorInstance::periodicallyCollect`（`Monitor.cc:119-153`）：

```cpp
static constexpr auto kCleanPeriod = std::chrono::seconds(300);
...
while (!stop_) {
  auto samples = SampleBatchPool::get();

  if (currentTime - cleanTime > kCleanPeriod) {
    getCollector().collectAll(context.bucketIndex, *samples, true);
    cleanTime = std::chrono::steady_clock::now();
  } else {
    getCollector().collectAll(context.bucketIndex, *samples, false);
  }

  if (LIKELY(!samples->empty())) {
    context.samplesQueue_.write(std::move(samples));
    context.reporterCond_.notify_one();
  }

  auto collect_period = config.collect_period();
  auto nextRunTime = currentTime + collect_period;
  currentTime = std::chrono::steady_clock::now();
  if (currentTime <= nextRunTime) {
    auto lock = std::unique_lock(context.mutex_);
    context.collectorCond_.wait_for(lock, nextRunTime - currentTime);
    currentTime = nextRunTime;
  } else {
    XLOGF(ERR, "A report task takes more than {} second: {}ms", ...);
  }
}
```

要點：

- **每 300 秒做一次 `cleanInactive=true` 的收集**。`Recorder::collectAndClean`（`Recorder.cc:31-45`）走訪 `map_` 裡的所有動態標籤子 recorder，把 `active_` 為 false 的刪掉；而 `active_` 只在 `collect()` 產出過非零 Sample 時被設為 true。這是 3FS 對抗**標籤基數爆炸**的唯一機制：以 uid 為標籤的 recorder，某個 uid 停止活動 5 分鐘後其子 recorder 就被回收。
- `currentTime = nextRunTime` 這一行是**漂移補償**：下一輪的基準時間用理論值而不是實際喚醒時間，所以收集節奏長期不會漂移。
- 超時只印 `ERR` log，不做任何補救。
- **`samplesQueue_.write()` 的回傳值被忽略**。這是全鏈路第一個丟棄點，詳見 5.5。

### 5.5 佇列、物件池與第一個丟棄點

`Monitor.h:76-90`：

```cpp
static constexpr size_t kMaxNumSampleBatches = 60;
using SampleBatch = std::vector<Sample>;
using SampleBatchPool = ObjectPool<SampleBatch, kMaxNumSampleBatches, kMaxNumSampleBatches>;

struct CollectorContext {
  folly::ProducerConsumerQueue<SampleBatchPool::Ptr> samplesQueue_ =
      folly::ProducerConsumerQueue<SampleBatchPool::Ptr>(kMaxNumSampleBatches);
  size_t bucketIndex = 0;
  std::mutex mutex_;
  std::condition_variable collectorCond_;
  std::condition_variable reporterCond_;
  std::jthread collectThread_;
  std::jthread reportThread_;
};
```

- 佇列是 **SPSC**（`folly::ProducerConsumerQueue`），因為一個 context 恰好有一條 collect 執行緒和一條 report 執行緒。無鎖環形緩衝。
- 容量 60 批，配上 1 秒的收集週期 ⇒ **Reporter 側最多容忍 60 秒的停滯**。超過就開始丟。
- `write()` 在滿時回傳 `false` 且不移動參數，`Monitor.cc:135` 沒有檢查回傳值 ⇒ `samples`（一個 `SampleBatchPool::Ptr`）在迴圈結尾析構，**該秒的全部指標靜默消失**。沒有計數器記錄丟了多少（沒有 `dropped` 指標），這是整條鏈路最「安靜」的失敗模式。
- `ObjectPool<SampleBatch, 60, 60>`（`src/common/utils/ObjectPool.h`）池化的是 `std::vector<Sample>` 這個物件的**外殼記憶體**（24 bytes 的 header）。`Deleter` 呼叫 `item->~T()`（`ObjectPool.h:21`），vector 的堆緩衝在此時被 `free`，`SampleBatchPool::get()` 則是 placement-new 一個空 vector。**所以池化並沒有省下 Sample 陣列本身的配置**，只省了外殼。考慮到一批 Sample 動輒數千個帶字串的物件，這個池的收益相當有限。

### 5.6 上報執行緒

`Monitor.cc:155-167`：

```cpp
void MonitorInstance::reportSamples(CollectorContext &context, std::vector<std::unique_ptr<Reporter>> reporters) {
  while (!stop_) {
    SampleBatchPool::Ptr samples;
    if (!context.samplesQueue_.read(samples)) {
      auto lock = std::unique_lock(context.mutex_);
      context.reporterCond_.wait(lock);
      continue;
    }
    for (auto &reporter : reporters) {
      reporter->commit(*samples);
    }
  }
}
```

- **所有 reporter 串行 commit**，且 `commit` 的 `Result<Void>` 回傳值被忽略。上報失敗不重試、不記錄。
- `reporterCond_.wait(lock)` 沒有謂詞（predicate），是裸等待。搭配 `stop()`（`Monitor.cc:230-240`）裡的 `notify_one()` 才能收尾。這裡有一個經典的 lost-wakeup 窗口：如果 collect 執行緒在 reporter 檢查完佇列為空、但還沒進入 `wait` 之前 `notify_one()`，這次通知就丟了。實際影響有限——collect 執行緒每秒都會再 notify 一次，最多延遲一個週期。
- `reporters` 是**按值傳入的 vector**（每個 CollectorContext 一份獨立的 reporter 實例）。`Monitor.cc:204-226` 的迴圈對每個 collector 各建一組 reporter，所以 `num_collectors=4` 就會有 4 條 TCP 連線到 collector，或 4 條 ClickHouse 連線。

### 5.7 Reporter 的建立與一個邊界 bug

`Monitor.cc:204-218`：

```cpp
for (int i = 0; i < config.num_collectors(); ++i) {
  std::vector<std::unique_ptr<Reporter>> reporters;
  for (auto i = 0ul; i < config.reporters_length(); ++i) {
    auto &reporterConfig = config.reporters(i);
    if (reporterConfig.type() == "clickhouse") {
      reporters.push_back(std::make_unique<ClickHouseClient>(reporterConfig.clickhouse()));
    } else if (reporterConfig.type() == "log") {
      reporters.push_back(std::make_unique<LogReporter>(reporterConfig.log()));
    } else if (reporterConfig.type() == "monitor_collector") {
      reporters.push_back(std::make_unique<MonitorCollectorClient>(reporterConfig.monitor_collector()));
    }
    RETURN_ON_ERROR(reporters.back()->init());
  }
  ...
}
```

**如果 `type` 是三者之外的字串，什麼都不會被 push，接著 `reporters.back()` 對空 vector 取用 ⇒ UB。** 對比 collector server 端的同一段邏輯（`MonitorCollectorOperator.cc:44-46`）有明確的 `XLOGF(FATAL, "Invalid reporter type: {}", ...)`——客戶端這側漏了這個分支。不過 `CONFIG_VARIANT_TYPE`（`src/common/utils/ConfigBase.h:118-125`）在設定載入時就會檢查 `type` 的值必須是某個既存子區段的名稱，所以 toml 打錯字通常在更早的階段被擋下。

外層迴圈變數 `i` 被內層同名的 `i` 遮蔽——不影響正確性（外層在迴圈體內沒再用 `i`），但屬於明顯的可讀性缺陷。

`Monitor::Config`（`Monitor.h:49-53`）：

```cpp
CONFIG_OBJ_ARRAY(reporters, ReporterConfig, 4, [](auto &) { return 0; });
CONFIG_ITEM(num_collectors, 1);
CONFIG_HOT_UPDATED_ITEM(collect_period, 1_s);
```

**最多 4 個 reporter，預設 0 個**。`collect_period` 是 hot-updated 的（可線上調），`num_collectors` 不是（要重啟）。

---

## 6. 傳輸層：RPC 協定與批次格式

### 6.1 服務定義

`src/fbs/monitor_collector/MonitorCollectorService.h`：

```cpp
struct MonitorCollectorRsp {
  SERDE_STRUCT_FIELD(dummy, Void{});
};

SERDE_SERVICE(MonitorCollector, 194) { SERDE_SERVICE_METHOD(write, 1, std::vector<Sample>, MonitorCollectorRsp); };
```

**整個服務只有一個方法**。serviceId = 194（0xC2），在 3FS 的服務編號表裡屬於高位段（對比：`StorageSerde`=3、`MetaSerde`=4、`ClientAgentSerde`=10、`SimpleExampleSerde`=0xF0、`MigrationSerde`=0xF1）。

請求體直接是 `std::vector<Sample>`——**沒有信封、沒有 batch id、沒有 sequence number、沒有 client id**。回應體是一個 `Void` 佔位。這帶來兩個直接後果：

- **無法做重複檢測**。RPC 若因 timeout 重送，同一批 Sample 會在 ClickHouse 裡插兩次（`MergeTree` 不去重）。不過 `MonitorCollectorClient` 把 `sendRetryTimes` 留在預設值 1（`src/common/net/RequestOptions.h:13`），所以重送機率低。
- **無法做流量歸因**。collector 不知道 Sample 來自哪個節點——它必須從 Sample 自帶的 `host`/`pod` 標籤去讀，這正是 `Recorder::collect` 每次都要塞這兩個標籤的原因（`Recorder.cc:87-89` 等處）。

### 6.2 客戶端

`src/common/monitor/MonitorCollectorClient.cc`：

```cpp
Result<Void> MonitorCollectorClient::init() {
  auto serverAddr = net::Address::fromString(config_.remote_ip(), net::Address::TCP);
  client_ = std::make_unique<net::Client>(config_.client(), "MCC");
  client_->start("MCC");
  ctx_ = std::make_unique<serde::ClientContext>(client_->serdeCtx(serverAddr));
  return Void{};
}

Result<Void> MonitorCollectorClient::commit(const std::vector<Sample> &samples) {
  MonitorCollector client;
  folly::coro::blockingWait(client.write(*ctx_, samples));
  return Void{};
}
```

值得逐點拆：

- **每次 commit 都在 stack 上建一個 `MonitorCollector client`**——這個型別是 `SERDE_SERVICE` 產生的空 struct，所有方法都是 static-like 的模板，建構成本為零。
- **`blockingWait`**：在 Reporter 執行緒上同步阻塞等 RPC 回來。Reporter 是專用執行緒，阻塞它不影響業務；但這正是背壓從 collector 傳回客戶端的路徑（見 11.2）。
- **`write()` 的回傳值被完全丟棄**。`folly::coro::blockingWait` 的結果沒有賦值給任何變數，函式無條件 `return Void{}`。**RPC 失敗（連不上、逾時、對端 500）在客戶端是完全不可見的**，連一行 log 都沒有。要診斷「指標為什麼沒進 ClickHouse」，只能從 `net::Client` 內部的通用 RPC 指標下手——但那些指標本身也要靠這條壞掉的鏈路才報得出去。
- `config_.remote_ip()` 的格式是 `"127.0.0.1:10000"`（`MonitorCollectorClient.h:15` 註解）。所有節點的設定裡預設是空字串（`configs/meta_main.toml:75`、`configs/storage_main.toml:56`、`configs/mgmtd_main.toml:56`、`configs/hf3fs_fuse_main.toml:154`），部署時填 collector 或 VIP 的地址。
- `net::Client::Config` 的預設值（`src/common/net/Client.h:23-30`）：`default_timeout = 1000ms`（`RequestOptions.h:11`）、`default_send_retry_times = 1`、`default_compression_level = 0`（**壓縮預設關閉**）、`default_compression_threshold = 128KB`。各節點的 toml 裡都沒有 `[common.monitor.reporters.monitor_collector.client]` 區段，全走預設。
- **1 秒的 RPC timeout 對上 1 秒的收集週期**：如果 collector 端處理超過 1 秒，客戶端就開始逾時。這個配對很緊。

### 6.3 線路格式

`serde::MessagePacket`（`src/common/serde/MessagePacket.h:53-71`）：

```cpp
SERDE_STRUCT_FIELD(uuid, uint64_t{});
SERDE_STRUCT_FIELD(serviceId, uint16_t{});    // 194
SERDE_STRUCT_FIELD(methodId, uint16_t{});     // 1
SERDE_STRUCT_FIELD(flags, uint16_t{});        // IsReq | UseCompress | ControlRDMA
SERDE_STRUCT_FIELD(version, Version{});
SERDE_STRUCT_FIELD(payload, Payload<T>{});    // = vector<Sample>
SERDE_STRUCT_FIELD(timestamp, std::optional<Timestamp>{});
```

外層再包 `net::MessageHeader`（`src/common/net/MessageHeader.h:20-27`）：8 bytes，`{uint32 checksum; uint32 size;}`，checksum 的低 8 bit 被改寫成魔數 `0x86` 加壓縮位元。單一訊息上限 `kMessageMaxSize = 512MB`（:17）。

所以一次 `write` 的完整格式是：

```
┌──────────────┬───────────────────────────────────────────────────────────┐
│ MessageHeader│ serde(MessagePacket<vector<Sample>>)                       │
│ 8 bytes      │                                                            │
│ crc32c|0x86  │ uuid | 194 | 1 | flags | version | [Sample...] | timestamp │
│ size         │                                                            │
└──────────────┴───────────────────────────────────────────────────────────┘

每個 Sample =
  name(string) | tags(sorted_vector_map<string,string>) | timestamp(u64) | variant<i64, Distribution>
```

一批的大小完全由「該進程該秒有多少活躍的 (name, tag) 組合」決定，沒有分片機制。一台埋了大量標籤的存儲節點，單次 RPC 可能是幾 MB。壓縮預設關閉、threshold 又是 128KB，所以這些重複度極高的標籤字串是**明碼上網**的。

---

## 7. Collector server 端架構

### 7.1 Service 層：一句話轉手

`src/monitor_collector/service/MonitorCollectorService.cc`：

```cpp
CoTryTask<MonitorCollectorRsp> MonitorCollectorService::write(serde::CallContext &ctx, std::vector<Sample> &samples) {
  co_await monitorCollectorOperator_.write(std::move(samples));
  co_return MonitorCollectorRsp{};
}
```

注意參數是 **`std::vector<Sample> &`（非 const）**，於是可以 `std::move` 進 Operator，反序列化出來的 vector 零複製地交棒。回應在 Operator 收下（而非落地）後就發出——**這是一個 fire-and-forget 語意的 RPC**：`write` 回 OK 只代表「進了 collector 的記憶體佇列」，不代表進了 ClickHouse。

### 7.2 Operator：佇列 + 執行緒池

`src/monitor_collector/service/MonitorCollectorOperator.h:20-29`：

```cpp
void connThreadFunc(std::stop_token stoken);
void monitorThreadFunc(std::stop_token stoken);

const MonitorCollectorService::Config &cfg_;
std::vector<std::jthread> threads_;
std::mutex m_;
std::condition_variable_any cv_;
folly::MPMCQueue<std::vector<Sample>> sampleQueue_;
```

設定（`MonitorCollectorService.h:18-26`）：

| 項目 | 預設 | 意義 |
|---|---|---|
| `reporter` | `type="clickhouse"` | 單一 reporter（不是陣列！客戶端可以有 4 個，collector 只能有 1 個） |
| `conn_threads` | 32 | 建立多少條 reporter 執行緒（= 多少條 ClickHouse 連線） |
| `queue_capacity` | 204800 | MPMC 佇列容量（單位是「批」不是「Sample」） |
| `batch_commit_size` | 4096 | 一次 commit 最多合併多少批 |
| `blacklisted_metric_names` | `{}` | 就地丟棄的指標名黑名單 |

建構子（`MonitorCollectorOperator.cc:9-17`）把 `conn_threads` 條 conn 執行緒 + 1 條監控執行緒一次拉起。

### 7.3 入隊：`blockingWrite` 就是背壓

```cpp
CoTryTask<void> MonitorCollectorOperator::write(std::vector<Sample> &&samples) {
  sampleQueue_.blockingWrite(std::move(samples));
  numQueueingSamples.addSample(1);
  cv_.notify_one();
  co_return Void();
}
```

`MonitorCollectorOperator.cc:28-33`。**`blockingWrite` 在佇列滿時阻塞呼叫執行緒**——而呼叫者是 RPC 的協程執行緒（`server.base.groups.processor` 的 coroutine pool，`configs/monitor_collector_main.toml:124-126` 是 256 個協程、4096 個並行請求上限）。所以背壓鏈是：

```
ClickHouse 慢 → conn threads 卡在 commit → sampleQueue_ 塞滿 204800 批
             → blockingWrite 阻塞 → RPC 協程執行緒被佔滿（proc thread 只有 2 條！）
             → 新請求排隊 → 客戶端 1s timeout → 客戶端 Reporter 執行緒卡在 blockingWait
             → 客戶端 ProducerConsumerQueue（60 批）塞滿 → 客戶端靜默丟棄
```

注意 `configs/monitor_collector_main.toml:61` 的 `num_proc_threads = 2`——**只有 2 條協程處理執行緒**。`blockingWrite` 一旦阻塞就會吃掉一整條，兩個請求同時卡住整個 collector 就完全停擺。這是一個很陡峭的懸崖：佇列從「有空間」到「完全癱瘓」之間沒有緩衝地帶。

### 7.4 出隊與聚批（含兩個實作問題）

`MonitorCollectorOperator.cc:35-86`，這是整個 binary 最核心的 50 行：

```cpp
void MonitorCollectorOperator::connThreadFunc(std::stop_token stoken) {
  std::unique_ptr<Reporter> reporter;
  auto &reporterConfig = cfg_.reporter();
  if (reporterConfig.type() == "clickhouse") {
    reporter = std::make_unique<ClickHouseClient>(reporterConfig.clickhouse());
  } else if (reporterConfig.type() == "log") {
    reporter = std::make_unique<LogReporter>(reporterConfig.log());
  } else if (reporterConfig.type() == "monitor_collector") {
    reporter = std::make_unique<MonitorCollectorClient>(reporterConfig.monitor_collector());
  } else {
    XLOGF(FATAL, "Invalid reporter type: {}", reporterConfig.type());
  }

  auto result = reporter->init();
  XLOGF_IF(FATAL, result.hasError(), "Initializing reporter failed. {}", result.error().describe());

  std::vector<Sample> samples;

  while (!stoken.stop_requested()) {
    std::unique_lock lk(m_);
    bool has_data = cv_.wait(lk, stoken, [this, &samples]() { return sampleQueue_.read(samples); });

    if (has_data) {
      std::vector<Sample> gather;
      for (int i = 1; i < cfg_.batch_commit_size(); i++) {
        bool continue_gather = sampleQueue_.read(gather);
        if (!continue_gather) {
          break;
        } else {
          samples.insert(samples.end(), gather.begin(), gather.end());
        }
        numQueueingSamples.addSample(-1);
      }

      samples.erase(std::remove_if(samples.begin(), samples.end(),
                                   [&cfg = this->cfg_](const Sample &s) {
                                     return cfg.blacklisted_metric_names().count(s.name) > 0;
                                   }),
                    samples.end());

      try {
        reporter->commit(samples);
      } catch (error_t e) { ... }
    }
  }
}
```

**每條 conn 執行緒各持有一個獨立的 Reporter**。對 ClickHouse 而言就是 32 條獨立連線，各自有自己的 `errorHappened_` 狀態。這是 `conn_threads` 命名的由來——它不是「處理執行緒數」，是「資料庫連線數」。

聚批的效果：把 N 個節點各自每秒送來的小 vector 合併成一個最多 4096 批的巨型 vector。以 100 個節點、每節點每秒 2000 個 Sample 計，一次 commit 可以是 20 萬列的單一 `INSERT` block。**這正是 collector 存在的技術理由**——MergeTree 對小批量寫入會產生大量 part，觸發不停的 merge，是 ClickHouse 最經典的效能殺手。

三個實作層面的問題：

**(a) 32 條執行緒實際上被序列化。** `std::unique_lock lk(m_)` 的作用域是整個 while 迴圈體，`cv_.wait` 回來時鎖是持有的，接下來的聚批、過濾、以及**`reporter->commit(samples)` 這個動輒數百毫秒的 ClickHouse 網路往返，全都在鎖內完成**。所以任一時刻只有一條執行緒能真正工作，另外 31 條在 `m_` 上排隊。32 條連線帶來的並不是 32 倍吞吐，而是「32 個預熱好的連線輪流用」。若要真正並行，`lk.unlock()` 應該加在聚批完成之後、commit 之前。

**(b) `numQueueingSamples` 的計數不平衡。** 入隊時每批 `+1`；出隊時第一次 `read`（在 `cv_.wait` 的謂詞裡）**不減**，只有聚批迴圈中每次成功的 `read` 才 `-1`。所以每完成一輪 commit 就淨多出 `+1`。而這個 recorder 是 `static CountRecorder numQueueingSamples{"monitor_collector.num_queueing_samples"}`（`MonitorCollectorOperator.cc:7`），`resetWhenCollect` 走預設 `true`，也就是**每秒清零的增量**而非 gauge。實際上它報出來的是「每秒完成的 commit 輪數」，跟名字說的「排隊中的樣本數」完全不同。真正的佇列深度只出現在 log 裡（見 (c)）。

**(c) 真正的佇列深度只進 log。** `monitorThreadFunc`（`MonitorCollectorOperator.cc:88-94`）：

```cpp
while (!stoken.stop_requested()) {
  XLOGF(INFO, "Sample queue capacity: {} / {}.", sampleQueue_.size(), cfg_.queue_capacity());
  std::this_thread::sleep_for(5s);
}
```

每 5 秒印一行 INFO。這是唯一能觀察 collector 是否接近飽和的訊號，而且**它沒有被做成指標**——要監控監控系統本身，得去 grep `/var/log/3fs/monitor_collector_main.log`。另外 `sleep_for(5s)` 不理會 `stop_token`，所以停機時這條執行緒最多要等 5 秒才退出。

**(d) `catch (error_t e)` 抓不到東西。** `error_t` 是 `<errno.h>` 的整數型別。`ClickHouseClient::commit` 內部已經自己 `catch (const std::exception &)` 了（`ClickHouseClient.cc:100`、`:153`），不會往外拋；就算拋了也是 `std::exception` 而非 int。這個 try-catch 是空轉的。

### 7.5 黑名單：唯一的中央化管控手段

```cpp
CONFIG_ITEM(blacklisted_metric_names, std::set<std::string>{});
```

`MonitorCollectorService.h:25`。這是 collector 存在的第二個技術理由：**當某個節點上的某個指標失控（例如標籤基數爆炸），運維可以在 collector 這一台改設定就止血，不必去 N 台節點逐一改設定重啟。** 過濾發生在 commit 之前、聚批之後，用 `std::remove_if` + `erase` 就地做 erase-remove idiom。

代價是它按**完整指標名精確比對**（`std::set::count`），沒有前綴或正則支援——想封掉整個 `storage.chunk_engine.*` 系列要一個一個列。

---

## 8. Reporter 家族與 ClickHouse 落地

### 8.1 介面

`src/common/monitor/Reporter.h`：

```cpp
class Reporter {
 public:
  virtual ~Reporter() = default;
  virtual Result<Void> init() = 0;
  virtual Result<Void> commit(const std::vector<Sample> &samples) = 0;
};
```

兩個方法，都回 `Result<Void>`——但如第 5.6、7.4 節所述，**兩端的呼叫者都忽略了 commit 的回傳值**。這個介面設計了錯誤傳遞的能力卻沒有人使用。

四個實作：

| Reporter | 標頭 / 實作 | 用途 | 誰會用 |
|---|---|---|---|
| `ClickHouseClient` | `ClickHouseClient.h` / `.cc` | 正式落地 | collector server（預設） |
| `LogReporter` | `LogReporter.h` / `.cc` | 把指標印進本地 log | 開發/單機除錯、collector 的 `type='log'` |
| `MonitorCollectorClient` | `MonitorCollectorClient.h` / `.cc` | 轉送給另一個 collector | 所有業務節點；collector 也可用來做分層中繼 |
| `TaosClient` | `TaosClient.h` / `.cc` | TDengine 落地 | **無人，且無法連結**（詳見 8.4） |

前三者是活的，第四者不是——`TaosClient` 的狀況比「沒被引用」更徹底，單獨拉到 8.4 講。

### 8.2 `ClickHouseClient`

**連線建立**（`ClickHouseClient.cc:13-30`）：

```cpp
client_ = std::make_unique<clickhouse::Client>(clickhouse::ClientOptions()
    .SetHost(config_.host()).SetPort(std::stoi(config_.port()))
    .SetUser(config_.user()).SetPassword(config_.passwd())
    .SetDefaultDatabase(config_.db())
    .SetPingBeforeQuery(false)
    .SetCompressionMethod(clickhouse::CompressionMethod::LZ4));
```

- `port` 在設定裡是**字串**（`ClickHouseClient.h:17`），這裡 `std::stoi`。空字串會丟 `std::invalid_argument`，被外層 catch 成 `kMonitorInitFailed` → `XLOGF_IF(FATAL, ...)`（`MonitorCollectorOperator.cc:49`）→ 進程掛掉。**設定沒填就開不起來**，這是刻意的 fail-fast。
- `SetPingBeforeQuery(false)`：省掉每次 insert 前的往返。代價是連線壞掉時要靠 insert 本身失敗才發現。
- LZ4 壓縮：對重複度極高的指標名與標籤字串效果顯著。

**標籤展平**（`ClickHouseClient.cc:32-62`）：

```cpp
static folly::sorted_vector_map<std::string, std::shared_ptr<clickhouse::ColumnString>> createTagColumns(
    const std::vector<Sample> &samples) {
  folly::sorted_vector_set<std::string> tag_key_set;
  for (auto &sample : samples) {
    for (auto &[key, value] : sample.tags) {
      if (key.empty()) { XLOGF(ERR, "Empty tag: {}", sample.name); }
      else { tag_key_set.insert(key); }
    }
  }
  ...
  for (auto &sample : samples) {
    for (auto &key : tag_key_set) {
      auto it = sample.tags.find(key);
      tag_columns[key]->Append(it != sample.tags.end() ? it->second : "");
    }
  }
  return tag_columns;
}
```

兩趟掃描：第一趟收集整批出現過的所有標籤 key 的聯集，第二趟為每個 Sample 的每個 key 填值（缺的填空字串）。**這是 O(samples × distinct_keys) 的字串 map 查找**，在 20 萬列 × 8 個 key 的規模下是 160 萬次 `sorted_vector_map::find`，屬於 collector 的主要 CPU 開銷。

關鍵風險：**若整批裡出現任何一個 ClickHouse 表沒有的欄位名，`Insert` 會拋例外，整批 20 萬列全部丟失**。而 `createTagColumns` 是對「整批」取聯集的——一個節點上某個新加的標籤，會毒死整批來自其他所有節點的資料。這也是 `blacklisted_metric_names` 的實戰用途。

**插入與錯誤處理**（`ClickHouseClient.cc:94-103`）：

```cpp
try {
  if (UNLIKELY(errorHappened_)) {
    client_->ResetConnection();
    errorHappened_ = false;
  }
  client_->Insert("counters", block);
} catch (const std::exception &e) {
  XLOGF(ERR, "ClickHouse insert counters failed: {}", e.what());
  errorHappened_ = true;
}
```

- **失敗即丟，沒有重試、沒有本地緩衝**。整個 3FS 監控鏈路裡沒有任何持久化的緩衝。
- `errorHappened_` 是**延遲重連**：這次失敗只記旗標，**下一次** commit 開頭才 `ResetConnection()`。好處是不在錯誤路徑上做昂貴的重連（若 ClickHouse 整個掛了，每秒重連一次比不斷重試好）；壞處是每次故障至少會浪費一批資料——第一批因錯誤而丟，重連後才恢復。
- counters 和 distributions 是**兩次獨立的 Insert，各有各的 try-catch**。前者失敗不影響後者。
- 表名寫死為 `"counters"` / `"distributions"`（:99、:152），資料庫名來自 `SetDefaultDatabase(config_.db())`。

### 8.3 `LogReporter`：與 ClickHouse 路徑的分工

#### 8.3.1 介面與設定

`src/common/monitor/LogReporter.h` 全檔只有 29 行，是四個 Reporter 裡最小的一個：

```cpp
class LogReporter : public Reporter {
 public:
  struct Config : ConfigBase<Config> {
    CONFIG_ITEM(ignore, std::string{});
  };

  LogReporter(const Config &config) : config_(config) {}
  Result<Void> init() final;
  Result<Void> commit(const std::vector<Sample> &samples) final;

 private:
  const Config &config_;
  std::optional<std::regex> ignore_;
};
```

**只有一個設定項 `ignore`**（`LogReporter.h:13`），是一條正則字串。對比 `ClickHouseClient::Config` 的 5 項連線參數（`ClickHouseClient.h:16-20`）與 `MonitorCollectorClient::Config` 的 remote_ip + 整包 `net::Client::Config`（`MonitorCollectorClient.h:15-16`），這個 Reporter 是零外部依賴的——不連網、不開檔、不需要憑證，這正是它的定位所在。

`ignore_` 是 `std::optional<std::regex>`（`LogReporter.h:25`），在 `init()` 裡才編譯（`LogReporter.cc:10-16`）：

```cpp
Result<Void> LogReporter::init() {
  auto ignore = config_.ignore();
  if (!ignore.empty()) {
    ignore_ = std::regex(ignore);
  }
  return Void{};
}
```

空字串就維持 `nullopt`，`commit` 裡靠 `ignore_.has_value()` 短路（`LogReporter.cc:21`），避免對每個 Sample 都跑一次無謂的正則。注意 `std::regex` 的建構在語法錯誤時會拋 `std::regex_error`，而這裡沒有 try-catch，也沒有轉成 `Result` 的錯誤——**設定寫錯正則會直接讓進程在啟動期異常終止**。以一個純除錯用的 Reporter 來說這是可接受的取捨（fail-fast 好過安靜地忽略掉整份 ignore 清單）。

#### 8.3.2 與 `ClickHouseClient` 的分工

兩者實作同一個 `Reporter` 介面，但目標完全不重疊：

| | `ClickHouseClient` | `LogReporter` |
|---|---|---|
| 輸出去向 | 遠端 ClickHouse | 本地 `XLOG` |
| 資料保真度 | 完整（每個標籤各自成欄） | **有損**（標籤完全不輸出） |
| 可查詢性 | SQL、可跨節點聚合 | 只能 grep |
| 過濾機制 | 無（過濾在 collector 端做） | `ignore` 正則 |
| 外部依賴 | ClickHouse server + 憑證 | 無 |
| 失敗模式 | 丟批次 + `XLOGF(ERR)` | 不會失敗 |
| 數值呈現 | 原始值（bytes 就是 bytes、latency 就是 ns） | **人類可讀**（KB / µs） |

最關鍵的差異在倒數兩列。`LogReporter` **完全不輸出 `sample.tags`**——`commit` 從頭到尾只用到 `sample.name`、`sample.number()`、`sample.dist()`（`LogReporter.cc:20-61`）。所以帶動態標籤展開的指標（例如按 uid 展開的 `storage.req_read.total`）在 log 裡會出現多行同名、值不同的紀錄，**無從分辨是哪個 uid**。這使它不能拿來做正式觀測，只能看總體趨勢。

反過來，它做了 ClickHouse 路徑刻意不做的事：**在寫出時就把單位換算好**。ClickHouse 那側必須存原始值，因為換算屬於呈現層（Grafana）的職責；log 沒有呈現層，所以換算只能發生在寫出的當下。

#### 8.3.3 使用情境

三個實際會用到它的場合：

1. **單機開發**。在筆電上跑 `storage_main` 而沒有 ClickHouse 時，把 `[[common.monitor.reporters]] type='log'` 加進設定，指標就會每秒印進 `/var/log/3fs/*.log`。這是唯一不需要任何基礎設施就能看到指標的方式。
2. **collector 自身的除錯**。`MonitorCollectorOperator.cc:40-41` 的 type 分派支援 `"log"`，把 collector 的 reporter 設成 log 就能確認「資料到底有沒有送達 collector」——這是排查「指標沒進 ClickHouse」時區分「客戶端沒送出」與「collector 沒寫進去」的關鍵手段。
3. **與其他 Reporter 並用**。客戶端最多可掛 4 個 reporter（`Monitor.h:50`），可以同時掛 `monitor_collector` 與 `log`，正常上報的同時在本地留一份。此時 `ignore` 正則就派上用場——只留下關心的那幾個指標，避免每秒幾千行淹沒日誌。

第 3 點也解釋了為什麼 `ignore` 是**排除式**（regex_search 命中就跳過）而不是**包含式**：預設空字串代表全部輸出，符合「加上去就先看看有什麼」的除錯直覺；要收斂時再逐步加排除規則。

#### 8.3.4 輸出格式

計數類（`LogReporter.cc:25-30`）：

```cpp
if (sample.isNumber()) {
  if (sample.name.find("bytes") != std::string::npos) {
    XLOGF(INFO, "{}: {}", sample.name, Size::around(sample.number()));
  } else {
    XLOGF(INFO, "{}: {}", sample.name, sample.number());
  }
}
```

分佈類（`LogReporter.cc:32-60`）先依名字決定除數與單位，再印九個欄位：

```cpp
auto base = 1;
auto unit = "";
if (sample.name.find("latency") != std::string::npos)    { base = 1000;    unit = " us"; }
else if (sample.name.find("bytes") != std::string::npos) { base = 1 << 10; unit = " KB"; }
```

實際輸出長這樣：

```
                                      ← commit 開頭的空行（LogReporter.cc:19）
storage.req_write.total: 12043
storage.write.bytes: 1.5GB
storage.do_commit.succ_latency: count:12043 mean:842.3 us min:120.0 us max:9821.5 us p50:790.1 us p90:1203.4 us p95:1544.2 us p99:3102.8 us
fuse.write.size: count:8801 mean:512.0 KB min:4.0 KB max:1024.0 KB p50:512.0 KB p90:1024.0 KB p95:1024.0 KB p99:1024.0 KB
```

三個細節：

- `commit` 第一行是 `XLOGF(INFO, "")`（`LogReporter.cc:19`），印一個空行把每個收集週期的區塊隔開。因為沒有時間戳欄位（`sample.timestamp` 也沒被輸出），這個空行是**唯一能看出「哪些行屬於同一秒」的線索**。
- 格式字串用了 `fmt` 的**位置參數**（`{0}`、`{2}` 重複出現，`LogReporter.cc:43-50`），因為 `unit` 要在七個地方重複貼上。
- **單位判定是子字串比對，不是型別判定**。任何名字裡含 `latency` 的 distribution 都會被當成奈秒除以 1000；含 `bytes` 的 counter 都會被 `Size::around()` 格式化。這是一個**基於命名慣例的隱式契約**——`fuse.write.latency`（`docs/metrics.md:25`）之所以叫這個名字不只是為了可讀性，還直接決定了它在 log 裡的呈現。反面案例：一個以奈秒為單位但名字裡沒有 `latency` 的 distribution，會被原樣印出裸數字；而一個名字裡剛好含 `bytes` 但語意不是位元組的 counter（例如 `..._bytes_per_op`），會被錯誤地格式化成 `1.5GB`。

### 8.4 `TaosClient`：一個宣告完整但實作被整份註解掉的 Reporter

`src/common/monitor/TaosClient.h` 是一份**語法完整、看起來完全正常**的類別宣告：

```cpp
class TaosClient : public Reporter {
 public:
  struct Config : ConfigBase<Config> {
    CONFIG_ITEM(host, "");
    CONFIG_ITEM(user, "");
    CONFIG_ITEM(passwd, "");
    CONFIG_ITEM(db, "");
    CONFIG_ITEM(cfg_dir, "/tmp");
    CONFIG_ITEM(log_dir, "/tmp");
  };

  TaosClient(const Config &config) : config_(config) {}
  ~TaosClient() override { stop(); }

  Result<Void> init() final;
  void stop();
  static void cleanUp();

  Result<Void> query(const std::string &sql);
  Result<Void> commit(const std::vector<Sample> &samples) final;

 protected:
  Result<Void> insert(std::string &buffer, std::vector<uintptr_t> &offsets);

 private:
  const Config &config_;
  void *taos_ = nullptr;
};
```

它繼承 `Reporter`、`override` 了兩個純虛方法、有自己的 `Config`——從標頭檔看不出任何異狀。但它是**死碼，而且死得比一般的死碼更徹底**。

#### 8.4.1 四層證據

**第一層：實作檔整份被區塊註解包住。** `src/common/monitor/TaosClient.cc` 第 1 行是 `/*`、第 138 行是 `*/`，中間 136 行的實作（`init` / `stop` / `cleanUp` / `query` / `commit` / `insert`）**全部落在註解內**。這個檔案編譯出來是一個空的 translation unit。

**第二層：因此連結不過。** `TaosClient.h` 宣告了 `init()`、`commit()`、`stop()`、`query()`、`insert()`、`cleanUp()` 六個非 inline 成員，但沒有任何 translation unit 提供定義。真正致命的是 `~TaosClient()`（`TaosClient.h:22`）是 inline 且會呼叫 `stop()`——所以**任何人只要寫下一行 `TaosClient c(cfg);`，就會在連結期收到 undefined reference**。這不是「跑起來會出錯」，是「編譯不過」。

**第三層：沒有任何活的引用點。** 全 repo 搜尋 `TaosClient` 與 `taos` 只命中三個檔案，全部都是死的：`src/common/monitor/TaosClient.h`、`.cc`，以及第三個——`tests/common/monitor/TestTaosClient.cc`，那份單元測試的本體（`:9-59`）同樣被 `/* */` 整段包住。除此之外零命中：沒有 include、沒有 type 分派、`CMakeLists.txt` 與 `cmake/` 裡沒有 TDengine 的依賴宣告、`configs/` 與 `deploy/` 裡沒有任何 taos 設定。連測試都被一起註解掉，說明這是一次有意識的整體停用，而非遺忘。

**第四層：設定系統根本掛不上去。** `Monitor::ReporterConfig`（`Monitor.h:42-47`）是：

```cpp
class ReporterConfig : public ConfigBase<ReporterConfig> {
  CONFIG_VARIANT_TYPE("clickhouse");
  CONFIG_OBJ(clickhouse, ClickHouseClient::Config);
  CONFIG_OBJ(log, LogReporter::Config);
  CONFIG_OBJ(monitor_collector, MonitorCollectorClient::Config);
};
```

**沒有 `CONFIG_OBJ(taos, TaosClient::Config)`**。而 `CONFIG_VARIANT_TYPE` 的檢查器（`src/common/utils/ConfigBase.h:118-122`）要求 `type` 的值必須對應到某個已宣告的子區段名稱，所以在 toml 裡寫 `type = 'taos'` 會在設定載入階段就被擋下——連走到 `Monitor.cc:209-215` 那串 if-else 的機會都沒有。

#### 8.4.2 它仍在專案裡留下的痕跡

即使實作全被註解，這段歷史仍在兩個地方留有化石：

1. **狀態碼表**。`src/common/utils/StatusCodeDetails.h:30-32` 定義了三個 monitor 狀態碼：

   ```cpp
   COMMON_STATUS(MonitorInitFailed, 12)
   COMMON_STATUS(MonitorQueryFailed, 13)
   COMMON_STATUS(MonitorWriteFailed, 14)
   ```

   其中 `MonitorInitFailed` 還有活著的使用者（`ClickHouseClient.cc:26`），但 **`MonitorQueryFailed(13)` 與 `MonitorWriteFailed(14)` 的唯一使用者全部位於被註解掉的 `TaosClient.cc` 內**（分別在 :64 與 :106、:131）。也就是說叢集的公用錯誤碼空間裡，有兩個編號是為一個不存在的 Reporter 保留的。這兩個碼不能隨意回收——它們可能已經出現在舊版本的日誌或跨版本的線路協定裡。

2. **`Reporter` 介面的形狀**。`Reporter::init()` 這個方法之所以存在（而不是把初始化放進建構子），部分原因就是 TDengine 客戶端需要一次性的全域初始化：`TaosClient.cc:14-31`（註解內）用 `std::call_once` 設定 `TSDB_OPTION_CONFIGDIR` 與 log 目錄，且 `cleanUp()` 是 **static** 的——這是一個典型的「C 函式庫需要進程級 init/cleanup」的形狀。`ClickHouseClient` 和 `LogReporter` 其實都不需要分離的 `init()`（前者可以在建構子連線，後者只是編譯正則），這個介面設計是被 TaosClient 的需求塑造出來的。

#### 8.4.3 為什麼這件事值得寫進報告

從註解掉的實作可以看出，TDengine 路徑用的是**完全不同的寫入模型**（`TaosClient.cc:70-135`）：把 Sample 序列化成 InfluxDB line protocol 的文字（`name,tag=val val=123u <微秒時間戳>`），以 `\0` 分隔塞進一個大 buffer，每滿 256KB 就呼叫 `taos_schemaless_insert()`。對比 ClickHouse 路徑的「按欄位建 `clickhouse::Column*` 再組 Block」（`ClickHouseClient.cc:74-104`），兩者對 `Sample` 這個資料模型的要求截然不同：

- line protocol 是 **schemaless** 的——標籤不需要預先在資料庫裡建欄位，加一個新標籤名不會讓寫入失敗。
- ClickHouse 路徑是 **schema-bound** 的——如 8.2 所述，出現表裡沒有的標籤名會讓整批 20 萬列全滅。

所以 4.3 與 8.2 提到的「新增標籤要先 `ALTER TABLE`、否則整批全滅」這個坑，**在 TDengine 路徑下是不存在的**。3FS 選擇 ClickHouse 換來了查詢效能與壓縮率，代價就是把 schema 演進的負擔搬到了運維流程上。這個對照只有從這份死碼裡才看得到。

至於為什麼是註解而不是刪除：`git log` 顯示 `TaosClient.cc` 只有一次提交（`815e55e Initial commit`）——**它在 3FS 開源的那一刻就已經是註解狀態**。合理的推測是內部版本仍有 TDengine 部署、但外部版本不想引入 `taos.h` 這個非開源的依賴，於是用最省事的方式（包一層 `/* */`）切斷編譯依賴、同時保留內部合併時的可還原性。這是開源與內部分支並行維護時很常見的手法，代價是外部讀者會看到一個看起來能用、實際上連結不過的類別。

### 8.5 為什麼要有一個獨立的 collector 進程

從程式碼裡能找到的證據，按強度排序：

1. **憑證集中**。ClickHouse 的 user/passwd 只出現在 `configs/monitor_collector_main.toml:136-141`。`meta_main.toml`、`storage_main.toml`、`mgmtd_main.toml`、`hf3fs_fuse_main.toml` 的 monitor 區段裡**只有 `remote_ip`**。若每個節點直連 ClickHouse，資料庫密碼就得散佈到所有存儲節點與所有掛載 FUSE 的運算節點上——後者是使用者可登入的機器。
2. **寫入放大控制**。`batch_commit_size = 4096` 把 N 個節點的小批合成一個巨型 block。直連的話 ClickHouse 每秒要吃 N 個小 INSERT，MergeTree 的 part 數量會失控。
3. **中央化止血閥**。`blacklisted_metric_names` 讓運維改一台的設定就能封掉全叢集的某個失控指標，不必滾動重啟所有節點（`MonitorCollectorOperator.cc:69-74`）。
4. **可水平擴展且對客戶端透明**。`deploy/README.md:96`：「Multiple instances of monitor services can be deployed behind a virtual IP address to share the traffic.」客戶端只認一個 `remote_ip`，是無狀態的 fire-and-forget，所以 VIP 後掛幾台都行——這只有在 collector 完全無狀態時才成立，而 `MonitorCollectorRsp` 是空的、協定裡沒有 session，正是為此設計。
5. **可分層中繼**。`MonitorCollectorOperator.cc:42-43` 明確允許 collector 的 reporter 型別是 `monitor_collector`，也就是 collector → collector 的樹狀彙聚。大規模叢集可以做「機架級 collector → 中央 collector → ClickHouse」。
6. **依賴隔離**。FUSE 客戶端跑在使用者的運算節點上，讓它持有到 ClickHouse 的長連線在網路分區、防火牆策略上都是負擔；TCP 到單一 collector 地址則簡單得多（`deploy/README.md:97`）。

---

## 9. 結構化 trace log（`src/analytics`）

### 9.1 定位：與 metric 正交的另一條鏈路

metric 回答「每秒有多少次寫、p99 延遲多少」，trace log 回答「**是哪一個 inode、哪一個 uid、在什麼時間、做了什麼**」。前者是聚合後的數值，後者是逐事件的結構化記錄。兩者的落地路徑完全不同：

| | metric | structured trace log |
|---|---|---|
| 資料模型 | `Sample`（name + tags + 一個數值） | 任意 `SerdeType` struct |
| 聚合 | 每秒在進程內聚合 | 不聚合，逐事件 |
| 傳輸 | RPC 到 collector | **無網路傳輸** |
| 落地 | ClickHouse | 本地 Parquet 檔（ZSTD） |
| 保留 | TTL 1 個月 | 由外部工具/清理腳本負責 |
| 開關 | 恆開 | `enabled`（release 預設 true、debug 預設 false） |

使用者只有兩個：`meta` 的 `MetaEventTrace`（`src/meta/service/MetaOperator.h:193`、`src/meta/base/Config.h:66`）與 `storage` 的 `StorageEventTrace`（`src/storage/service/StorageOperator.h:153`）。

`MetaEventTrace` 的欄位（`src/meta/event/Event.h:50-72`）說明了它的用途——eventType、inodeId、parentId、entryName、dstParentId、dstEntryName、ownerId、userId、client、tableId、inodeType、nlink、length、truncateVer、dynStripe、oflags、recursiveRemove、removedChunks、pruneSession、symLinkTarget、origPath。這是一份可以離線重建命名空間變更史的審計日誌，用 metric 完全無法表達。

### 9.2 Parquet schema 由 C++ 型別編譯期生成

`src/analytics/` 的核心是三個 CRTP visitor：

```
             ┌──────────────────┐        ┌───────────────────┐
             │ StructVisitor    │        │ ObjectVisitor      │
             │（只走型別，無值） │        │（走型別 + 值）      │
             └────────┬─────────┘        └─────────┬─────────┘
                      │                            │
             BaseStructVisitor<D>          BaseObjectVisitor<D>
                      │                    ┌───────┴────────┐
            SerdeSchemaBuilder<T>    SerdeObjectWriter<T>  SerdeObjectReader<T>
             產出 parquet schema      寫一列                 讀一列
```

`SerdeSchemaBuilder<T>::getSchema()`（`SerdeSchemaBuilder.h:18-29`）在**編譯期**用 `refl::Helper::iterate<T>` 走訪 serde struct 的所有欄位，映射成 parquet 的 `PrimitiveNode`。型別對應規則（:36-116）：

| C++ 型別 | Parquet |
|---|---|
| `bool` | `BOOLEAN` |
| `int16/uint16` | `INT32` + `Int(16, signed?)` |
| `int32/uint32` | `INT32` + `Int(32, signed?)` |
| `int64/uint64` | `INT64` + `Int(64, signed?)` |
| `enum` | `INT32` + `Int(32, true)` |
| 可轉 string | `BYTE_ARRAY` + `String()` |
| `StrongTyped<U>` | 遞迴成 `U` |
| 巢狀 serde struct | 攤平，欄位名用 `_` 串接 |
| `vector` / `set` | **JSON 字串**（`BYTE_ARRAY`） |
| `optional` | **JSON 字串**，空值寫 `""` |
| `variant` | 額外開一欄 `<k>ValIdx`（`uint32`）記 index，**再把每個備選型別各開一欄** |
| `folly::Expected`（= `Result`） | 額外開一欄 `<k>Error`，值欄永遠有值（失敗時填 default） |

三個值得注意的設計：

1. **所有欄位都是 `Repetition::REQUIRED`**——Parquet 層面完全沒有 nullable。`optional` 和 `Result` 的「無值」語意被搬到應用層（空 JSON 字串 / Error 欄）。這讓 schema 極度扁平，代價是 `optional<T>` 失去了型別化查詢的能力（變成要 parse JSON）。
2. **variant 展開成「所有備選各一欄」**。`visitVariant`（`SerdeObjectVisitor.h:52-64`）不論 index 是多少都會對每個備選呼叫一次 func，未命中的備選填 default 值。所以 `variant<A,B,C>` 佔 4 欄。這在 `Sample` 這種只有 2 個備選的型別上還好，欄位多的 variant 會嚴重膨脹。
3. **欄位名做字元過濾**。`filterOutInvalidChars`（`SerdeSchemaBuilder.h:207-212`）只保留 `[A-Za-z0-9]`，其餘一律**刪除**（不是換成底線）。所以 `snake_case` 的欄位名 `trace_meta` 會變成 `tracemeta`——巢狀時再用 `_` 串接成 `tracemeta_timestamp`。這是 Parquet 欄位名的既定慣例造成的，但也意味著 `foo_bar` 與 `foobar` 兩個欄位會撞名。

`SerdeObjectWriter<T>`（`SerdeObjectWriter.h`）用同一套 visitor 邏輯的「帶值」版本，把物件寫進 `parquet::StreamWriter`。寫入設定（:47-54）：

```cpp
writerBuilder.set_sorting_columns(sortedColumns);
writerBuilder.max_row_group_length(maxRowGroupLength);
writerBuilder.data_page_version(parquet::ParquetDataPageVersion::V2);
writerBuilder.compression(parquet::Compression::ZSTD);
```

`SerdeObjectReader<T>` 是完全對稱的讀取端，讓離線分析工具能用同一份 C++ struct 定義把 Parquet 讀回來。

### 9.3 `StructuredTraceLog`：writer 池與隨機化的 flush

`src/analytics/StructuredTraceLog.h`。設定（:33-45）：

```cpp
CONFIG_ITEM(trace_file_dir, Path{"."});
#ifndef NDEBUG
  CONFIG_HOT_UPDATED_ITEM(enabled, false);
  CONFIG_HOT_UPDATED_ITEM(dump_interval, 60_min);
#else
  CONFIG_HOT_UPDATED_ITEM(enabled, true);
  CONFIG_HOT_UPDATED_ITEM(dump_interval, 30_s);
#endif
CONFIG_HOT_UPDATED_ITEM(max_num_writers, size_t{1}, ConfigCheckers::checkPositive);
CONFIG_HOT_UPDATED_ITEM(max_row_group_length, size_t{100'000});
```

**debug build 預設關閉、release 預設開啟**，而且 dump 間隔差了 120 倍（60 分鐘 vs 30 秒）。這個反直覺的配對是因為 debug build 是給開發者跑單測用的，不該在工作目錄裡撒 Parquet 檔。

包裝結構（:19-27）：

```cpp
struct TraceMeta {
  SERDE_STRUCT_FIELD(timestamp, std::time_t{});
  SERDE_STRUCT_FIELD(hostname, String{});
};
struct StructuredTrace {
  SERDE_STRUCT_FIELD(trace_meta, TraceMeta{});
  SERDE_STRUCT_FIELD(_, SerdeType{});
};
```

業務結構被塞進一個叫 `_` 的欄位。經過 `filterOutInvalidChars` 之後 `_` 變成空字串，所以最終的 Parquet 欄位名就是業務欄位名本身（沒有前綴），而 meta 欄位是 `tracemeta_timestamp` / `tracemeta_hostname`。這是一個相當巧妙的命名 hack。

**首次 flush 時間隨機化**（:70-72）：

```cpp
uint64_t secsUntilFirstDump =
    folly::Random::rand64(config_.dump_interval().asSec().count() / 2, config_.dump_interval().asSec().count());
nextDumpTime_ = microsecondsSinceEpoch(UtcClock::now() + std::chrono::seconds{secsUntilFirstDump});
```

在 `[interval/2, interval)` 之間隨機挑首次 dump 時間。目的是**避免整個叢集的所有節點在同一秒同時關檔開檔**——同時 flush 會造成本地磁碟 IO 的週期性尖峰，而 trace log 落的正是存儲節點自己的磁碟。

**RAII 的 entry 介面**（:84-90）：

```cpp
std::shared_ptr<SerdeType> newEntry(const SerdeType &init = SerdeType{}) {
  auto ptr = new SerdeType(init);
  return std::shared_ptr<SerdeType>(ptr, [this](SerdeType *ptr) {
    this->append(*ptr);
    delete ptr;
  });
}
```

用**自訂 deleter** 讓「物件生命週期結束」自動觸發寫入。呼叫端拿到一個可以隨時填欄位的 `shared_ptr`，離開作用域時自動落檔，不需要記得呼叫 `append`。

**writer 池**（:208-219）：`folly::UnboundedQueue<WriterPtr>` 當池子，`getOrCreateWriter()` 先 `try_dequeue`，池空且未達 `max_num_writers` 就 CAS 遞增計數並新建，否則阻塞 `dequeue()` 等別人還回來。**這是一個借還式的並發控制**——每條寫入執行緒必須獨佔一個 writer（`parquet::StreamWriter` 非執行緒安全），`max_num_writers` 就等於寫入並行度，也等於同時開著的 Parquet 檔數。

**檔案路徑**（:224-227）：

```cpp
Path logfilePath = config_.trace_file_dir() / Path{fmt::format("{:%Y-%m-%d}", timestamp)} / Path{hostname_} /
    Path{fmt::format("{}.{}.{:%Y-%m-%d-%H-%M-%S}.{}.parquet", typename_, hostname_, timestamp, nextLogFileIndex_++)};
```

`<dir>/<日期>/<主機名>/<型別>.<主機名>.<時間戳>.<序號>.parquet`——天然按日期分區、按主機分目錄，方便 Spark/DuckDB 之類的工具直接掃描。

**flush 的防重入與逾時放棄**（:124-195）：用 `std::atomic_flag dumpingTrace_` 的 `test_and_set` 做防重入；`std::async` 非同步關檔。迴圈裡有一句保護：

```cpp
for (size_t i = 0; numWritersToClose > 0; i++) {
  if (i >= 10 * maxNumWriters_) break;   // 試太多輪就放棄
  auto writer = writerPool_.dequeue();
  if (!writer) continue;
  if (writer->createTime() > now) { writerPool_.enqueue(writer); continue; }  // 是本輪新建的，跳過
  ...
}
```

`createTime() > now` 這一行防止「剛剛在本輪 flush 裡新建的 writer 又被自己關掉」的無限循環。`i >= 10 * maxNumWriters_` 則是在有其他執行緒持續佔用 writer 時放棄，避免 flush 執行緒無限等待——**寧可少 flush 一些也不卡住**。

**自我監控**（:53-56）：

```cpp
latencyTagSet_({{"tag", typename_}, {"instance", fmt::to_string(fmt::ptr(this))}}),
createLatency_("trace_log.create_latency", latencyTagSet_),
appendLatency_("trace_log.append_latency", latencyTagSet_),
flushLatency_("trace_log.flush_latency", latencyTagSet_),
```

trace log 自己埋了三個 `LatencyRecorder`，走 metric 鏈路上報。這是兩條鏈路唯一的交集點。注意 `instance` 標籤用的是**物件指標的十六進位字串**——這保證同進程內多個 trace log 實例不撞名（因為 `Collector::add` 撞名會 FATAL），但也意味著這個標籤在 ClickHouse 裡是純噪音（每次重啟都變），而且 `instance` 正是那個沒有 `LowCardinality` 的欄位。

**寫入失敗即自我關閉**（:104-113）：

```cpp
if (UNLIKELY(writer == nullptr)) {
  XLOGF(CRITICAL, "Cannot get a writer of {} trace log in directory {}", ...);
  enableTraceLog(false);
  return;
}
*writer << trace;
auto writerOk = writer->ok();
writerPool_.enqueue(std::move(writer));
if (UNLIKELY(!writerOk)) enableTraceLog(false);
```

磁碟寫不進去就把 `enabled_` 設為 false，**永久停用**（除非有人改 config 觸發 callback 重新啟用）。這是一個保守但正確的選擇：trace log 是旁路，不該讓它拖垮 meta/storage 的主路徑。

---

## 10. 3FS 實際埋了哪些指標

`docs/metrics.md` 列了一份局部清單。挑幾組代表性的、把語意講到能實際使用的程度。

### 10.1 `OperationRecorder` 生成的五件套

以 `storage.do_commit` 為例（`docs/metrics.md:59-63`）：

| 指標 | 型別 | reset | 語意 |
|---|---|---|---|
| `storage.do_commit.total` | count | Y | 每秒發起的 commit 次數 = **IOPS** |
| `storage.do_commit.current` | count | **N** | 當下**正在進行**的 commit 數 = 並發度 |
| `storage.do_commit.fails` | count | Y | 每秒失敗數 |
| `storage.do_commit.succ_latency` | latency | Y | 成功路徑的延遲分佈（ns） |
| `storage.do_commit.fail_latency` | latency | Y | 失敗路徑的延遲分佈（ns） |

**成功與失敗的延遲被分開統計**是很值得學的設計：失敗通常是超時，延遲比成功大一到兩個數量級，混在一起會把 p99 完全污染。

`current` 為什麼是 count 而非 value：因為它是 `resetWhenCollect=false` 的 CountRecorder（`Recorder.cc:251`），用 `+1/-1` 維護。用 counter 做 gauge 的好處是**不需要任何鎖也能正確處理並發的進出**；用 `ValueRecorder::set` 反而會有 lost update。

在 ClickHouse 裡要算「這一秒的平均並發度」不能直接用 `current`（它是抽樣瞬時值），要用 Little's Law：`total × mean(succ_latency) / 1e9`。

同一套模式在 `docs/metrics.md` 裡出現在 `storage.do_query`、`storage.do_remove`、`storage.do_update`、`storage.remove_range`、`storage.req_update`、`storage.reliable_forward`、`storage.engine_commit`、`storage.engine_update` 等處。

### 10.2 帶 `instance` 標籤的高基數指標

`src/storage/service/StorageOperator.cc:87`：

```cpp
auto recordGuard = storageReqReadRecorder.record(monitor::instanceTagSet(std::to_string(req.userInfo.uid)));
```

`storage.req_read.*` 這一組被 **uid** 展開。一個叢集有多少活躍的 uid，這組指標就有多少條時間序列。`Recorder` 的 300 秒 inactive 清理（`Monitor.cc:120`、`Recorder.cc:38`）是唯一的收斂機制。同一個檔案裡還有以 **IB 裝置名**（:205、:582）和**磁碟編號**（`StorageTarget.cc:68`）為 instance 的用法，基數都是有界的。

FUSE 側則是以 **mount_name** 為固定標籤（`src/fuse/IovTable.cc:130-136`、`src/fuse/IoRing.cc:75-87`），再以 `{"io": ioType, "uid": uids}` 為動態標籤（`IoRing.cc:139`、`:180`）——`io` 是 read/write 這種低基數，`uid` 又是高基數。`3fs-monitor.sql` 裡 `io` 和 `uid` 都有 `LowCardinality`，`uid` 這個選擇是賭「叢集裡的使用者數不會太多」。

### 10.3 `ValueRecorder` 類：容量與狀態

`docs/metrics.md:55-58`、`:105-108`：`storage.disk_info.capacity/available/free/read_only`、`storage.target.used_size/reserved_size/unrecycled_size`、`storage.target_state`。全部是 `reset=N` 的瞬時值。

如 3.8 節所述，這一類全部受 `if (val > 0)` 影響：**值為 0 的狀態不會產生資料點**。對容量類指標影響不大（磁碟不會真的 0 位元組可用），但對 `storage.target_state`（0 = invalid）與 `storage.disk_info.read_only`（0 = 可寫）是實質的語意漏洞。

### 10.4 RPC 與記憶體的框架級指標

不在 `docs/metrics.md` 裡但同樣存在：

- `common.rpc.compressed_count` / `compressed_bytes` / `decompressed_bytes`（`src/common/net/Processor.cc:8-10`）
- `common.rpc.deserilize.fails`（`src/common/serde/CallContext.cc:9`，注意拼字是 `deserilize`，少一個 `a`——查詢時要照抄）
- `common.apply_rdma_transmission.*`（OperationRecorder，`CallContext.cc:10-11`）
- `memory_allocator.used_bytes` / `allocated_bytes` / `deallocated_bytes` / `thread_*`（`src/memory/common/AllocatedMemoryCounter.cc:62-79`），以 size bucket 為 `instance` 標籤，bucket 0 是 `"Total"`，其餘是 `"<size>B"`
- `trace_log.create_latency` / `append_latency` / `flush_latency`（`StructuredTraceLog.h:54-56`）
- `monitor_collector.num_queueing_samples`（`MonitorCollectorOperator.cc:7`，語意見 7.4(b)）

### 10.5 記憶體計數器的環境變數開關

`AllocatedMemoryCounter.cc:28-55` 讀兩個環境變數：

- `MEMORY_METRIC_REPORT_INTERVAL`（預設 `100_MB`）：每配置/釋放這麼多位元組才更新一次指標，是採樣率控制。
- `MEMORY_METRIC_THREAD_COUNTER`：設了才啟用 per-thread 的計數（`thread_allocated_bytes` 等）。

這是 3FS 裡唯一一組**不走 toml 而走環境變數**的監控設定——因為它必須在 static 初始化階段（config 系統還沒起來）就生效。

---

## 11. 設計取捨與潛在坑

### 11.1 資料丟失有四個獨立的位置

| # | 位置 | 條件 | 是否可觀測 |
|---|---|---|---|
| 1 | `Monitor.cc:135` | 客戶端 SPSC 佇列（60 批）滿 | **完全靜默**（無 log、無指標） |
| 2 | `MonitorCollectorClient.cc:23` | RPC 失敗/逾時 | **完全靜默**（回傳值被丟棄） |
| 3 | `MonitorCollectorOperator.cc:69-74` | 命中黑名單 | 設計預期，無記錄 |
| 4 | `ClickHouseClient.cc:100`、`:153` | Insert 例外 | 有 `XLOGF(ERR)`，但**沒有指標** |

四個丟棄點裡三個是靜默的。要判斷「ClickHouse 裡的資料是否完整」，在系統內部找不到答案——必須從外部比對（例如檢查每個 host 每秒是否都有預期數量的列）。這是把監控當旁路的必然結果，但**至少 #1 和 #2 應該各有一個 CountRecorder**（諷刺的是它們自己也會受同樣的丟失影響，不過至少能發現）。

### 11.2 背壓鏈條的傳導路徑

```
ClickHouse 變慢
  └─► conn thread 卡在 Insert（且持有共用 mutex m_，另外 31 條全部堵住）
      └─► sampleQueue_（204800 批）填滿
          └─► blockingWrite 阻塞 RPC 協程（proc thread 只有 2 條）
              └─► 新請求排隊到 max_processing_requests_num=4096
                  └─► 客戶端 1s RPC timeout
                      └─► 客戶端 Reporter 執行緒卡在 blockingWait
                          └─► 客戶端 SPSC 佇列（60 批 ≈ 60 秒）填滿
                              └─► 靜默丟棄
```

好消息：**背壓不會傳到業務路徑**。埋點本身（`addSample`）永遠是無鎖的、永遠不阻塞；Collector 執行緒即使 write 失敗也只是丟掉繼續跑。監控故障不會拖垮 3FS 本身。

壞消息：7.4(a) 的共用 mutex 讓 collector 的實際吞吐等於單執行緒，`conn_threads=32` 這個設定給了錯誤的容量預期。

### 11.3 基數爆炸的三道防線與各自的缺口

1. **300 秒 inactive 清理**（`Monitor.cc:120`、`Recorder.cc:36-43`）：只清 recorder 樹的第二層（動態標籤子節點），而且只在該 recorder 該週期沒產出非零 Sample 時才清。對 `resetWhenCollect=false` 的 counter 幾乎無效——它們永遠有值、永遠 active。
2. **collector 黑名單**：精確名稱比對，不支援萬用字元。
3. **ClickHouse 的 `LowCardinality`**：只是儲存層優化，擋不住基數本身。而 `instance` 和 `pod` 恰恰沒有這個修飾。

真正的缺口是 `instance` 標籤被當成萬用維度使用（uid / target id / 指標名 / 物件指標全塞進去）。在一個有數萬個 uid 的環境裡，`storage.req_read.*` 五個指標 × uid 數 × host 數就是幾百萬條時間序列。

### 11.4 時間戳

- 客戶端在 `collect()` 當下取 `UtcClock::now()`（例如 `Recorder.cc:85`），**每個 recorder 各取一次**——同一批 Sample 的 timestamp 不完全相同（相差微秒級）。
- ClickHouse 欄位是 `DateTime`（秒精度），`to_time_t` 截斷。所以同一批在庫裡通常落在同一秒，但**跨秒邊界時會被切成兩秒**。
- 用的是**牆鐘時間**，不是單調時鐘。NTP 回撥會讓時序出現重複或倒退的資料點。
- 收集節奏本身用 `steady_clock`（`Monitor.cc:122`），所以節奏不受 NTP 影響，只有標記時間受影響。

### 11.5 `hf3fs::DigestBuilder` 是一份不會被編譯進任何呼叫點的死碼

`src/common/monitor/DigestBuilder.{h,cc}` 是 folly 的 `DigestBuilder` 的 vendored 副本（保留了 Meta 的 Apache 2.0 授權頭），把 CPU-local 緩衝改成了 `folly::ThreadLocal`。但 `Recorder.h:189` 用的是**folly 原版**：

```cpp
folly::DigestBuilder<folly::TDigest> tdigest_{kDigestBufferSize, kDigestMaxSize};
```

全 repo 沒有任何地方實例化 `hf3fs::DigestBuilder`——`Recorder.h:19` 只是 `#include "DigestBuilder.h"`。

這件事之所以值得寫進報告，是因為那份副本的 `append()` **沒有迴圈出口**（`DigestBuilder.cc:70-95`）：

```cpp
void DigestBuilder::append(double value) {
  for (;;) {
    ...
    state->buffer.push_back(value);
    if (state->buffer.size() == bufferSize_) { ... }
  }   // ← 沒有 break / return
}
```

三個 `continue` 分支之後，成功路徑走完就回到 `for(;;)` 的開頭再 push 一次，永遠不返回。folly 原版在對應位置有 `return`。因為它是死碼，這個缺陷不會造成任何運行時影響——但任何人想「改用專案自己的 DigestBuilder」都會立刻踩中。

另一個副作用：**hf3fs 版用 thread-local、folly 版用 CPU-local**（`third_party/folly/folly/stats/DigestBuilder-inl.h` 的 `cpuLocalBuffers_.resize(cl.numCachesByLevel[0])`）。實際跑的是 CPU-local 版本，所以 `kDigestBufferSize = 128_KB` 的記憶體開銷是「每 recorder × L1 cache group 數」，而不是「每 recorder × 執行緒數」——在幾百條執行緒的存儲進程裡，這個差別是兩個數量級。

### 11.6 兩份「看起來能用、實際上不能用」的程式碼

`src/common/monitor/` 這個 15 檔的目錄裡有**兩個檔案是陷阱**，而且兩者的失效方式不同：

| | `DigestBuilder.{h,cc}` | `TaosClient.{h,cc}` |
|---|---|---|
| 失效方式 | 有定義、可連結，但 `append()` 是無窮迴圈 | 實作整份被 `/* */` 包住，連結不過 |
| 誤用的後果 | **執行期掛死**（編譯連結全過） | **連結期報錯**（立刻發現） |
| 危險程度 | 高 | 低 |
| 為何存在 | folly 原版的 vendored 副本（改 CPU-local 為 ThreadLocal） | 內部版仍用 TDengine，開源版切斷依賴 |

諷刺的是**比較「壞」的那一個反而比較安全**：`TaosClient` 誤用會在連結期立刻爆掉，`DigestBuilder` 誤用則要到跑起來才發現整條執行緒卡死。如果要對這個目錄做清理，`DigestBuilder` 的優先序應該高於 `TaosClient`——前者是會咬人的地雷，後者只是佔空間的化石。

### 11.7 其他值得記住的細節

- **`Collector::add` 撞名會 `FATAL`**（`Monitor.cc:62-66`）。新增 recorder 時若不小心與既有的 (name, tag) 完全相同，是啟動即崩，不是安靜的資料錯亂。
- **`MonitorInstance::getCollector()` 是跨實例共享的**（`Monitor.h:70-73`），`createMonitorInstance()` 目前無人使用，若使用會造成雙重收集。
- **collector 的 reporter 只有一個**（`MonitorCollectorService.h:19`），客戶端可以有四個（`Monitor.h:50`）。想讓 collector 同時寫 ClickHouse 與 log 做不到。
- **`ValueRecorder` 的 `val > 0` 守衛**（`Recorder.cc:310`）會吃掉 0 與負值。
- **`SimpleDistributionRecorder` 的 `count != 0 && sum != 0` 守衛**（`Recorder.cc:203`）會吃掉正負相消的批次。
- **`monitorThreadFunc` 的 `sleep_for(5s)` 不理會 stop_token**（`MonitorCollectorOperator.cc:92`），停機延遲最多 5 秒。
- **`errorCodeStrings` / `errorCodeTagSets` 各 65536 槽 × 每執行緒**（`Recorder.cc:20-21`）——虛擬位址空間佔用可觀，實體記憶體按需分頁。
- **序列化的 `TagSet` 每個 Sample 都重複攜帶完整字串**，且預設不壓縮（`default_compression_level = 0`）。在標籤多的節點上，網路流量的大宗是重複的標籤字串。
- **`counters` 表沒有 `method` 欄位**（`distributions` 有）。任何在 counter 上加 `method` 標籤的改動會讓整批插入失敗。

---

## 12. 檔案索引

### 12.1 `src/monitor_collector/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `monitor_collector.cpp` | 7 | binary 進入點；`OnePhaseApplication<MonitorCollectorServer>::instance().run()` 一行帶過，並在此覆寫全域 new/delete |
| `CMakeLists.txt` | 2 | 宣告 `monitor_collector` lib 與 `monitor_collector_main` bin，依賴 `common` / `memory-common` / `MonitorCollectorService-fbs` |
| `service/MonitorCollectorServer.h` | 36 | `net::Server` 子類；把預設監聽埠釘在 TCP 10000、group services 釘成 `{"MonitorCollector"}` |
| `service/MonitorCollectorServer.cc` | 24 | `beforeStart` 建 Operator 並註冊 Service（strict 模式）；`beforeStop` 釋放 Operator 觸發執行緒收尾 |
| `service/MonitorCollectorService.h` | 37 | serde service 包裝 + 全部運行參數（reporter / conn_threads=32 / queue_capacity=204800 / batch_commit_size=4096 / 黑名單） |
| `service/MonitorCollectorService.cc` | 14 | `write` RPC handler；`std::move` 進 Operator 後立刻回空回應（fire-and-forget） |
| `service/MonitorCollectorOperator.h` | 31 | Operator 宣告：MPMC 佇列、jthread 陣列、共用 mutex 與 `condition_variable_any` |
| `service/MonitorCollectorOperator.cc` | 96 | 本組件真正的引擎：32 條 conn 執行緒的聚批-過濾-commit 迴圈、5 秒一次的佇列深度 log 執行緒 |

### 12.2 `src/common/monitor/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `Sample.h` | 118 | 資料模型全在這裡：`TagSet`（sorted_vector_map）、`Distribution`（8 個 double）、`Sample`（name/tags/timestamp/variant）、`std::hash<TagSet>` 特化 |
| `Recorder.h` | 413 | Recorder 家族的完整宣告：Count / Distribution / Latency / SimpleDistribution / Operation / Value / Lambda，以及 TLS 內部結構 |
| `Recorder.cc` | 341 | 各 Recorder 的 `collect()` 實作、動態標籤子 recorder 建立、狀態碼字串的 thread-local 快取、30 秒 log |
| `Monitor.h` | 100 | `Collector` / `Monitor`（Config 與靜態門面）/ `MonitorInstance`（CollectorContext、SPSC 佇列、ObjectPool）的宣告 |
| `Monitor.cc` | 257 | 全域註冊表的分桶實作、收集執行緒、上報執行緒、reporter 工廠、`dummyVariable` 的生命週期把戲 |
| `Reporter.h` | 17 | 兩個純虛方法 `init()` / `commit()` 的介面；`init()` 之所以與建構子分離，是被 `TaosClient` 的進程級初始化需求塑造的 |
| `ClickHouseClient.h` | 36 | `ClickHouseClient` 宣告 + 5 項連線設定（host/port/user/passwd/db，port 是字串）；用前置宣告避免把 `clickhouse/client.h` 洩漏給所有引用者 |
| `ClickHouseClient.cc` | 162 | 唯一的正式落地實作：連線建立、標籤展平成欄位、counters/distributions 兩次 Insert、`errorHappened_` 延遲重連 |
| `LogReporter.h` | 28 | `LogReporter` 宣告；全類只有一個設定項 `ignore`（正則字串）與一個 `std::optional<std::regex>` 成員，是四個 Reporter 裡唯一零外部依賴的 |
| `LogReporter.cc` | 66 | 把 Sample 印進 log：`init()` 編譯正則、`commit()` 依指標名猜單位（bytes → `Size::around`、latency → µs、distribution bytes → KB）；**不輸出任何標籤** |
| `MonitorCollectorClient.h` | 34 | `MonitorCollectorClient` 宣告 + `remote_ip`（格式 `127.0.0.1:10000`）與整包 `net::Client::Config` |
| `MonitorCollectorClient.cc` | 27 | 業務節點用的 reporter：建 `net::Client` + `serde::ClientContext`，`blockingWait` 送出 `MonitorCollector::write`；**回傳值被完全丟棄** |
| `TaosClient.h` | 39 | TDengine reporter 的**完整類別宣告**（6 項設定、繼承 `Reporter`、`override` 兩個純虛方法），從標頭看不出異狀 |
| `TaosClient.cc` | 138 | 上述宣告的實作——**但第 1 行是 `/*`、第 138 行是 `*/`，整份被註解掉**。編譯出空 TU，任何實例化都會在連結期報 undefined reference。詳見 8.4 |
| `DigestBuilder.h` | 80 | folly `DigestBuilder` 的 vendored 副本宣告（把 CPU-local 改成 `folly::ThreadLocal`），保留 Meta 的 Apache 2.0 授權頭 |
| `DigestBuilder.cc` | 97 | 上述的實作。**未被任何程式碼實例化**（`Recorder.h:189` 用的是 folly 原版），且 `append()` 的 `for(;;)` 缺少出口，誤用會執行期卡死 |
| `ScopedMetricsWriter.h` | 73 | 兩個 RAII 工具：`ScopedLatencyWriter`（建構記時、析構上報）與 `ScopedCounterWriter`（進出臨界區各記一次並發度） |

### 12.3 `src/analytics/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `Common.h` | 65 | 一組 concept：`ConvertibleToString`、`WithSerdeMethod`、`WithReadableSerdeMemberMethod` 等，用來在 visitor 裡做重載分派；以及兩個欄位名後綴常數 |
| `SerdeStructVisitor.h` | 123 | 「只看型別、不看值」的 CRTP visitor 基底；variant 的靜態展開 `visitVariant<T>` |
| `SerdeObjectVisitor.h` | 129 | 「型別 + 值」的 CRTP visitor 基底；variant 的動態展開（**所有備選都訪問一次**） |
| `SerdeSchemaBuilder.h` | 219 | 用 `BaseStructVisitor` 從 C++ serde struct 編譯期生成 Parquet schema；欄位名過濾與巢狀攤平 |
| `SerdeObjectWriter.h` | 241 | 用 `BaseObjectVisitor` 把物件寫進 `parquet::StreamWriter`；ZSTD 壓縮、V2 data page、row group 長度控制 |
| `SerdeObjectReader.h` | 238 | `SerdeObjectWriter` 的對稱讀取端，離線分析工具用同一份 struct 讀回 Parquet |
| `StructuredTraceLog.h` | 285 | 對外的門面：writer 池、隨機化首次 flush、`newEntry()` 的 shared_ptr deleter 技巧、日期/主機分層路徑、寫失敗即自我停用、三個自我監控 LatencyRecorder |
| `CMakeLists.txt` | 1 | `target_add_lib(analytics common apache_arrow_static)` |

### 12.4 相關的協定與部署檔

| 檔案 | 職責 |
|---|---|
| `src/fbs/monitor_collector/MonitorCollectorService.h` | serviceId=194、method `write(vector<Sample>) -> MonitorCollectorRsp{Void}` |
| `src/fbs/monitor_collector/CMakeLists.txt` | `target_add_lib(MonitorCollectorService-fbs common)` |
| `configs/monitor_collector_main.toml` | collector 的完整設定；注意**沒有 `[common.monitor]` 區段**（自身指標預設不上報） |
| `deploy/sql/3fs-monitor.sql` | `3fs.counters` 與 `3fs.distributions` 兩張 MergeTree 表，PARTITION BY 日期、TTL 1 個月 |
| `deploy/README.md:56-98` | 部署順序（monitor 是 Step 2，早於 mgmtd）、VIP 多副本、TCP 通訊的官方說明 |
| `docs/metrics.md` | 四種指標型別與各自落地表的對照；約 80 個指標的語意與是否 reset |
