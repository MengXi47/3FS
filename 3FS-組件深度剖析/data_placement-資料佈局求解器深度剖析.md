# data_placement（資料佈局求解器）深度剖析

> 對應原始碼：`deploy/data_placement/`（求解器與產生器）
> 下游消費者：`src/client/cli/admin/UploadChains.cc`、`UploadChainTable.cc`、`CreateTarget.cc`（`admin_cli` 命令）
> 部署脈絡：`deploy/README.md`、`deploy/systemd/*.service`、`deploy/sql/3fs-monitor.sql`

---

## 0. 一句話總結

3FS 的鏈拓樸**不是執行期算出來的，而是離線用整數規劃解出來的**——`deploy/data_placement/` 把「哪些節點該共用同一條 chain」formulate 成一個**組合設計問題**（平衡不完全區組設計 BIBD 的鬆弛版），丟給 HiGHS solver，解出一張 `incidence_matrix`（節點 × 群組的 0/1 關聯矩陣），再由 `gen_chain_table.py` 把這張抽象矩陣**原樣複製到每一顆磁碟上**，展開成具體的 target id、chain id 與 `admin_cli` 命令。整套設計真正要最佳化的東西只有一個：**當任何一顆磁碟壞掉時，重建它所需的讀取流量必須均勻攤在其餘所有節點上**——而這件事沒有寫成目標函數，是寫成**約束**，目標函數是個常數 `1`。

---

## 1. 它在整個 3FS 裡的位置

```
┌─────────────────────────────────────────────────────────────────────────┐
│ 離線（部署前，人工執行一次）                                                │
│                                                                          │
│  參數: v=節點數 k=副本數 r=每盤target數                                     │
│    │                                                                     │
│    ▼                                                                     │
│  ┌────────────────────────────────────────┐                              │
│  │ src/model/data_placement.py            │  Pyomo 建模                   │
│  │   DataPlacementModel.build_model()     │  ──► HiGHS solver            │
│  │   → 0/1 變數 disk_used_by_group[i,g]   │                              │
│  └────────────────┬───────────────────────┘                              │
│                   │ incidence_matrix.pickle                              │
│                   │ {(node, group): True}                                │
│                   ▼                                                      │
│  ┌────────────────────────────────────────┐                              │
│  │ src/setup/gen_chain_table.py           │  抽象矩陣 → 具體 id           │
│  │   generate_chains()                    │  每個 disk_index 複製一份      │
│  └────────────────┬───────────────────────┘                              │
│                   │                                                      │
│    ┌──────────────┼──────────────┬─────────────────────┐                 │
│    ▼              ▼              ▼                     ▼                 │
│ create_target  generated_    generated_          remove_target           │
│ _cmd.txt       chains.csv    chain_table.csv     _cmd.txt                │
└────┬──────────────┬──────────────┬─────────────────────┬─────────────────┘
     │              │              │                     │
     ▼              ▼              ▼                     ▼
 create-target  upload-chains  upload-chain-table   offline/remove-target
     └──────────────┴──────────────┴─────────────────────┘
                          admin_cli（見 admin_cli 報告）
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────┐
│ 線上                                                                     │
│  mgmtd 持有 ChainTable ──► client 開檔時據此算出 chunk → chain 的映射       │
│                       ──► storage 據此知道自己在哪些鏈上、鄰居是誰          │
└─────────────────────────────────────────────────────────────────────────┘
```

關鍵認知：**mgmtd 只是消費者，不是決策者**。`mgmtd` 負責的是「鏈上某個副本掛了怎麼辦」（target 狀態機、resync），而「鏈一開始該怎麼組」完全由這個離線工具決定。叢集拓樸一旦上傳就固定，要改就得重跑求解器再上傳新版本。

---

## 2. 檔案結構

```
deploy/data_placement/
├── README.md                      使用教學（5 節點 / 3 副本 / 16 盤 / 每盤 6 target 的完整範例）
├── requirements.txt               pyomo 6.8.0 + highspy 1.8.0 + pandas/plotly/loguru/psutil
├── .gitignore
├── src/
│   ├── __init__.py                （空）
│   ├── model/
│   │   ├── data_placement.py      549 行 ── 核心：兩個模型 + CLI
│   │   └── data_placement_job.py  108 行 ── smallpond 分散式參數掃描（★ 依賴缺失，見 §9）
│   └── setup/
│       ├── __init__.py            （空）
│       └── gen_chain_table.py     124 行 ── incidence matrix → CSV 與命令
└── test/
    ├── test_model.py               94 行 ── 求解器可行性測試
    ├── test_setup.py               55 行 ── 端到端（解模型 → 產生鏈）
    └── test_plan.py                10 行 ── smallpond 計畫測試（★ 依賴缺失）
```

---

## 3. 問題的數學形式

### 3.1 五個符號

`data_placement.py:54-72` 用 property 把工程參數映射成組合設計的標準符號，這個對應關係是讀懂整份程式碼的鑰匙：

| 符號 | property | 工程意義 | 組合設計意義 |
|---|---|---|---|
| `v` | `num_nodes` | 儲存節點數 | 點數（points） |
| `b` | `num_groups` | chain 數（CR）／EC group 數 | 區組數（blocks） |
| `r` | `num_targets_per_disk` | 每顆盤上的 target 數 | 每個點的重複數（replication number） |
| `k` | `group_size` | 副本數（CR）／EC 群大小 | 區組大小 |
| `λ` | `max_recovery_traffic_on_peer` | 單一節點失效時，任一鄰居要負擔的重建流量上限 | 任兩點共同出現的區組數 |

```python
# data_placement.py:70-72
@property
def λ(self):
    return self.max_recovery_traffic_on_peer
```

用 `λ` 這個希臘字母當 property 名稱（Python 3 識別字允許 Unicode）是刻意的——作者在告訴讀者「這就是 BIBD 裡的那個 λ」。

### 3.2 決策變數

只有一組真正的變數（`data_placement.py:219`）：

```python
model.disk_used_by_group = po.Var(model.disks, model.groups, domain=po.Binary)
```

`disk_used_by_group[i, g] = 1` 表示「節點 i 在群組 g 裡有一個 target」。

**⚠ 命名陷阱**：變數叫 `disks`，但它的索引集是節點數（`data_placement.py:205`）：

```python
model.disks = po.RangeSet(1, self.num_nodes)
```

整個模型的**故障域是節點，不是磁碟**。`num_disks_per_node` 這個參數根本不出現在模型裡——它只在下游的 `gen_chain_table.py` 才登場。理由見 §5.1：同一份解會被原封不動複製到每一顆盤上，所以模型只需要解一次「節點層級」的問題。

### 3.3 約束一：每個節點的容量

`data_placement.py:242-247`：

```python
def each_disk_has_limited_capcity(model, disk):
    if self.all_targets_used:
        return po.quicksum(...) == self.num_targets_per_disk
    else:
        return po.quicksum(...) <= self.num_targets_per_disk
```

每個節點參與的群組數，等於（或不超過）每盤 target 數 `r`。`all_targets_used`（`:82-84`）判斷 `b*k == v*r` 是否成立——能整除就用等式（所有 target 都排得滿），不能整除就退化成不等式並在 `:198-199` 記一筆 warning：「some disks have unused targets」。

### 3.4 約束二：每個群組的大小

`data_placement.py:249-251`：

```python
def enough_disks_assigned_to_each_group(model, group):
    return po.quicksum(model.disk_used_by_group[disk,group] for disk in model.disks) == self.group_size
```

每個群組**恰好**由 `k` 個**不同節點**組成。這一條就是 3FS 的故障域保證：因為變數的第一維是節點，一個群組裡不可能出現同一個節點兩次，所以 **k 個副本必然落在 k 台不同機器上**。這個保證不是靠事後檢查，而是結構性地內建在模型裡。

### 3.5 約束三：重建流量的上下界（核心）

這是整份程式碼的重點。先理解「重建流量」的定義（`data_placement.py:90-100`）：

```python
@property
def recovery_traffic_factor(self):
    return (self.group_size - 1) if self.chain_table_type == "EC" else 1

@property
def sum_recovery_traffic_per_failure(self):
    return self.num_targets_per_disk * self.recovery_traffic_factor

@property
def max_recovery_traffic_on_peer(self):
    return math.ceil(self.sum_recovery_traffic_per_failure / (self.num_nodes-1))
```

拆解：

- 一個節點失效 → 它身上的 `r` 個 target 都要重建。
- **CR（鏈式複製）**：每個 target 只需從**任一個**存活副本拉資料，`factor = 1`。
- **EC（糾刪碼）**：每個 target 需要群組內**所有** `k-1` 個存活分片才能解碼，`factor = k-1`。
- 總重建讀取量 = `r × factor`。
- 理想情況下這些讀取平均攤在其餘 `v-1` 個節點上 → 每個鄰居負擔 `ceil(r × factor / (v-1))`，即 `λ`。

再看約束怎麼寫（`data_placement.py:259-273`）：

```python
def peer_recovery_traffic_upper_bound(model, disk, peer):
    if self.balanced_incomplete_block_design:
        return calc_peer_recovery_traffic(model, disk, peer) == self.max_recovery_traffic_on_peer
    else:
        return calc_peer_recovery_traffic(model, disk, peer) <= self.max_recovery_traffic_on_peer + self.relax_ub

def peer_recovery_traffic_lower_bound(model, disk, peer):
    return calc_peer_recovery_traffic(model, disk, peer) >= max(0, self.max_recovery_traffic_on_peer - self.relax_lb)
```

其中 `calc_peer_recovery_traffic(i, j)` 就是「節點 i 與節點 j 共同出現在幾個群組裡」。

**三種嚴格程度**：

| 模式 | 條件 | 約束形式 | 含義 |
|---|---|---|---|
| 嚴格 BIBD | `bibd_only` 且 `balanced_peer_traffic` 且 `relax_ub==0`（`:102-104`）——其中 `balanced_peer_traffic`（`:86-88`）本身又是 `all_targets_used` **且** 流量整除，所以實際是四個條件的合取 | `== λ` | 任兩節點共現次數**完全相同**，這就是 BIBD 的定義 |
| 雙側鬆弛 | `all_targets_used` | `λ-relax_lb ≤ · ≤ λ+relax_ub` | 允許不均，但差距有界 |
| 僅上界 | 其餘 | `· ≤ λ+relax_ub` | `:274-275` 明確記 log 說明沒加下界 |

**為什麼下界重要？** 上界防止某個鄰居被打爆（熱點），下界防止某個鄰居完全不參與重建（閒置）。只有兩者都管住，重建才真正是「全叢集並行」。`:335-336` 的驗證直接檢查極差：

```python
assert peer_traffic_diff <= self.relax_ub + self.relax_lb + 1e-5
```

### 3.6 目標函數：一個常數

`data_placement.py:277-281`，這是全份程式碼最值得注意的地方：

```python
def total_recovery_traffic(model):
    return po.summation(model.disk_in_same_group) * 2

# model.obj = po.Objective(rule=total_recovery_traffic, sense=po.minimize)
model.obj = po.Objective(expr=1)  # dummy objective
```

**被註解掉的那一行揭露了設計演變**：作者原本想「最小化總重建流量」，後來改成純可行性問題（feasibility problem）——目標函數恆為 1，solver 只要找到**任何一組滿足所有約束的解**就停。

這個取捨很合理：總重建流量其實由 `r × factor × v` 固定（`:330` 的 `max_total_traffic` 就是這個值），能動的只有**分佈**，而分佈的均勻性已經被 §3.5 的上下界約束死了。留著目標函數只會讓 MIP 白白多跑分支定界。**把最佳化目標編碼成約束、再解可行性問題**，是這裡的核心手法。

唯一保留真實目標函數的是子類 `RebalanceTrafficModel`（`:436-442`）：

```python
def total_rebalance_traffic(model):
    return self.total_existing_targets - num_existing_targets_not_moved(model)

model.obj = po.Objective(expr=total_rebalance_traffic, sense=po.minimize)
```

擴容時要最小化「必須搬動的既有 target 數」，這才是貨真價實的最佳化。

---

## 4. 二次項線性化

`calc_disk_in_same_group` 的自然寫法是兩個 0/1 變數相乘（`data_placement.py:225-226`）：

```python
return model.disk_used_by_group[disk,group] * model.disk_used_by_group[peer,group]
```

這讓模型變成二次規劃（MIQP）。但 HiGHS 是線性 solver，所以 `:153-154` 強制開啟線性化：

```python
if "highs" in pyomo_solver:
    self.qlinearize = True
```

線性化引入輔助變數 `disk_in_same_group[i,j,g]`（`:221`）與三條標準的 AND 約束（`:228-235`）：

```
x_i + x_j ≤ y + 1        ← 兩個都是 1 時，強迫 y = 1
y ≤ x_i                  ← 任一個是 0 時，強迫 y = 0
y ≤ x_j
```

```
   x_i  x_j │ 下界要求 y ≥   上界要求 y ≤   結論
   ─────────┼──────────────────────────────────
    0    0  │      -1            0          y=0
    0    1  │       0            0          y=0
    1    0  │       0            0          y=0
    1    1  │       1            1          y=1   ✓
```

代價：變數數量從 `v·b` 膨脹到 `v·b + C(v,2)·b`。以 README 的 `v=5, b=10` 為例是 50 → 150；但節點數一大就很可觀（`v=25, b=?` 的測試案例 `test_model.py:61` 需要 `max_timelimit=30` 搭配 `auto_relax` 才收斂）。這是「用線性 solver」必須付的稅。

---

## 5. 從抽象解到具體拓樸

### 5.1 同一份解複製到每顆盤

`gen_chain_table.py:37-54` 的三層迴圈是整個下游的核心：

```python
for disk_index in range(num_disks_per_node):
    group_slot_idx = defaultdict(int)
    for node_id in range(node_id_begin, node_id_end+1):
        for target_index in range(num_targets_per_disk):
            target_id = calc_target_id(target_id_prefix, node_id, disk_index, target_index)
            target_pos = (node_id - node_id_begin) * num_targets_per_disk + target_index
            ...
            chain_id = (chain_id_prefix * 1_000 + (disk_index+1)) * 1_00_000 + chain_index
```

`disk_index` 在**最外層**，而 `target_pos` 的計算**完全不含 disk_index**——也就是說每一顆盤都套用同一張 incidence matrix，只是 chain_id 前綴不同。

這帶來三個性質：

1. **求解成本與磁碟數無關**。16 顆盤和 1 顆盤解同一個模型，只是展開時複製 16 份。這就是為什麼模型只需要節點層級。
2. **不同盤的鏈完全不相交**。盤 1 的鏈只由各節點的盤 1 組成。一顆盤壞掉，只影響它自己那組鏈。
3. **節點失效時所有盤同時且對稱地重建**。因為每顆盤的重建流量分佈都一樣均勻，疊加起來仍然均勻。

### 5.2 id 的十進位編碼

`gen_chain_table.py:12-13`：

```python
def calc_target_id(target_id_prefix, node_id, disk_index, target_index):
    return ((target_id_prefix * 1_000_000 + node_id) * 1_000 + (disk_index+1)) * 100 + (target_index+1)
```

```
target_id = PP NNNNNN DDD TT
            │  │      │   └─ target_index+1   (2 位, <100)
            │  │      └───── disk_index+1     (3 位, <1000)
            │  └──────────── node_id          (6 位, <1_000_000)
            └─────────────── target_id_prefix (2 位, <100)

chain_id  = CC DDD IIIII
            │  │   └─────── chain_index       (5 位, <100000)
            │  └─────────── disk_index+1      (3 位, <1000)
            └────────────── chain_id_prefix   (2 位, <100)
```

用**十進位**而非位元打包（對比 `chunk_engine` 的 `Position` 用 u64 逐 bit 打包，見該報告 §4）。理由很實際：這些 id 會出現在 `admin_cli` 的輸出、日誌、CSV 與人工維運指令裡，`10100010101` 一眼就能拆成「prefix 10 / node 100001 / disk 01 / target 01」，而位元打包必須靠工具解碼。**面向人的識別碼用十進位，面向機器的用位元打包**——兩邊各取所需。

值域上限在 `:89-96` 用一串 assert 守住：

```python
assert len(incidence_matrix) < 1_00_000
assert args.node_id_end - args.node_id_begin < 1000
assert args.num_disks_per_node < 1000
assert args.num_targets_per_disk < 100
```

### 5.3 CR 與 EC 的分歧

`gen_chain_table.py:44-48`：

```python
if chain_table_type == "EC":
    group_slot_idx[groups[target_pos]] += 1
    chain_index = (groups[target_pos]-1) * group_sizes[0] + group_slot_idx[groups[target_pos]]
else:
    chain_index = groups[target_pos]
```

| | CR | EC |
|---|---|---|
| chain_index | 直接就是群組編號 | 群組編號 × 群大小 + 槽位序號 |
| 每條鏈的 target 數 | `k`（`:66`） | **恰好 1**（`:63`） |
| 鏈總數 | `v·d·r / k`（`:67`） | `v·d·r`（`:64`） |

**EC 模式下「鏈」退化成單一 target**。糾刪碼的群組關係不由 chain 表達，而是被編碼進 `chain_index` 的算術結構裡——連續 `k` 個 chain_index 屬於同一個 EC 群。這是把兩種完全不同的冗餘策略塞進同一張 ChainTable 資料結構的取巧做法。

### 5.4 四份輸出與下游命令的對接

`gen_chain_table.py:100-120` 產生四個檔案。我逐一核對了它們與 `admin_cli` 的介面：

| 輸出檔 | 格式 | 下游 | 驗證 |
|---|---|---|---|
| `generated_chains.csv` | `ChainId,TargetId,TargetId,...` | `upload-chains` | `UploadChains.cc:32` 的範本 header 正是 `ChainId,TargetId,TargetId`；`:48` 檢查第 0 欄必須是 `ChainId`、`:58` 檢查其餘欄必須是 `TargetId`。產生端 `gen_chain_table.py:101` 寫的是 `','.join(['TargetId']*len(chain_list[0].target_list))`——**欄數隨副本數浮動**，與消費端「其餘欄一律 TargetId」的寬鬆檢查恰好相容 |
| `generated_chain_table.csv` | 單欄 `ChainId` | `upload-chain-table` | `UploadChainTable.cc:36` 範本 header 為 `ChainId`、`:50` 檢查一致 |
| `create_target_cmd.txt` | 每行一條 `create-target` | `admin_cli` 逐行執行 | `CreateTarget.cc:16-22` 定義了 `--node-id`／`--disk-index`／`--target-id`／`--chain-id`／`--chunk-size`／`--use-new-chunk-engine`，與 `gen_chain_table.py:114` 產生的旗標**逐一對應** |
| `remove_target_cmd.txt` | 每個 target 兩行：先 `offline-target` 再 `remove-target` | 拆叢集 | 順序正確——必須先下線才能移除 |

`--use-new-chunk-engine` 被**寫死**在 `gen_chain_table.py:114`，沒有開關。也就是說這個工具只產生使用 Rust chunk engine 的 target（見 `chunk_engine` 報告）。

---

## 6. 參數自動搜尋與鬆弛迴圈

### 6.1 `find_params`：湊出可行的參數組合

使用者只給 `v`（節點數）和 `k`（副本數）時，`r` 和 `b` 由 `data_placement.py:106-114` 搜出來：

```python
@staticmethod
def find_params(v, k, min_r=1, max_r=100, bibd_only=False):
    if bibd_only: min_r = max(min_r, k)
    for r in range(min_r, max_r):
        if v * r % k == 0 and r * (k - 1) >= v - 1:
            b = v * r // k
            if not bibd_only or r * (k - 1) % (v - 1) == 0:
                return v, b, r, k
    raise ValueError(f"cannot find valid params: {v=}, {k=}")
```

三個條件各有來歷：

| 條件 | 意義 |
|---|---|
| `v*r % k == 0` | 總 target 數 `v·r` 要能整除群大小 `k`，否則排不滿 |
| `r*(k-1) >= v-1` | 每個節點透過 `r` 個群組能接觸到 `r(k-1)` 個鄰居實例；至少要 ≥ `v-1` 才可能「每個鄰居都分到活」。低於此值 `:193-194` 會警告「some disks do not share recovery traffic」 |
| `r*(k-1) % (v-1) == 0`（僅 BIBD） | λ 必須是整數，否則不可能完全均衡 |

`build_model` 開頭還有三條斷言，但**生效條件不同**（`:184-190`）：`v ≥ k`（`:184`）無條件檢查；Fisher 不等式的 `b ≥ v`（`:188`）與 `r ≥ k`（`:190`）被 `:186` 的 `if self.balanced_incomplete_block_design:` 包住，**只在嚴格 BIBD 模式下才檢查**——因為它們是 BIBD 存在的必要條件，鬆弛模式下本來就允許不成立。

### 6.2 `auto_relax`：解不出來就放寬

`data_placement.py:123-142` 的迴圈：

```python
num_loops = self.max_recovery_traffic_on_peer*2

for loop in range(num_loops):
    try:
        timelimit = min(timelimit + init_timelimit, max_timelimit)
        instance = self.solve(...)
    except (InfeasibleModel, SolverTimeout) as ex:
        if auto_relax:
            self.relax_lb = init_relax_lb + (loop+1) // 2
            self.relax_ub = init_relax_ub + (loop+2) // 2
            continue
```

兩個機制同時推進：

- **時限遞增**：每輪 `+init_timelimit`，上限 `max_timelimit`。給難題更多時間。
- **交錯放寬**：`(loop+1)//2` 與 `(loop+2)//2` 讓 lb 與 ub **輪流**加一，而不是同時加。

```
loop:  0    1    2    3    4    5
lb  : +0   +1   +1   +2   +2   +3
ub  : +0   +1   +2   +2   +3   +3
     初始 └─ub先追平─┘  └同步┘
```

先鬆上界（容忍某些鄰居負擔重一點）再鬆下界（容忍某些鄰居閒一點），因為熱點比閒置更容易被接受——閒置代表重建變慢，熱點只是某台機器忙一點。

`:138-142` 有個**邏輯瑕疵**值得記一筆：

```python
elif loop + 1 < num_loops:
    logger.critical(f"failed to find solution after {num_loops} attempts")
    raise ex
else:
    raise ex
```

兩個分支行為完全相同（都 `raise ex`），差別只在前者多印一行 critical log。而條件寫反了——`loop + 1 < num_loops` 是「還沒跑完」，此時印「failed after N attempts」語意不對；真正跑完最後一輪時反而走 else 不印。不影響正確性（不論哪個分支都正確地拋出例外），純屬訊息瑕疵。

---

## 7. 解的驗證

`check_solution`（`data_placement.py:314-341`）在回傳前做四層檢查，這比模型本身更能反映作者擔心什麼：

```python
for (disk, peer), peer_traffic in peer_traffic_map.items():
    assert peer_traffic <= self.max_recovery_traffic_on_peer + self.relax_ub + 1e-5
    if has_peer_traffic_lower_bound:
        assert peer_traffic >= max(0, self.max_recovery_traffic_on_peer - self.relax_lb) - 1e-5
...
assert peer_traffic_diff <= self.relax_ub + self.relax_lb + 1e-5
if self.balanced_incomplete_block_design:
    assert math.isclose(peer_traffic_diff, 0.0, abs_tol=1e-9)
assert total_traffic <= max_total_traffic + 1e-5
```

| 檢查 | 為什麼需要 |
|---|---|
| 逐對上下界 | solver 回報 optimal 不代表解真的滿足約束（數值誤差、載入錯誤） |
| 極差 ≤ lb+ub | 交叉驗證：即使個別都在界內，整體離散度也要受控 |
| BIBD 時極差 = 0 | 宣稱是 BIBD 就必須真的完全平衡，容差收緊到 `1e-9` |
| 總流量 ≤ 理論上限 | 抓「重複計數」這類建模錯誤 |

注意容差的差異：一般檢查用 `1e-5`，BIBD 用 `1e-9`。因為前者面對的是 MIP solver 的浮點鬆弛，後者是純整數性質，應當精確成立。

`has_peer_traffic_lower_bound` 的判定方式（`:315-318`）是**去 model 裡遍歷約束物件找名字**：

```python
for c in instance.component_objects(po.Constraint):
    if "peer_recovery_traffic_lower_bound_eqn" in str(c):
        has_peer_traffic_lower_bound = True
```

而不是重算一次 `:269-275` 的條件判斷。這是刻意的——驗證邏輯不重複建模邏輯，直接問「模型裡到底有沒有這條約束」，避免兩處條件不同步。

`get_peer_traffic`（`:291-300`）算的是**正規化後**的流量：

```python
... * self.recovery_traffic_factor / (self.group_size - 1)
```

CR 時 `factor=1`，除以 `k-1` 得到分數值（README 範例輸出的 `1.5` 就是這樣來的）；EC 時 `factor=k-1`，剛好約掉得到整數。所以兩種模式的數值可以放在同一個尺度上比較。

---

## 8. 測試涵蓋了什麼

`test_model.py:11-46` 的 `placement_params` 五組參數，註解直接寫明各自要測的邊界：

| 參數 | 註解標示的意圖 | 觸發的程式路徑 |
|---|---|---|
| `v=5,r=6,k=2` / `k=3` | simple cases | 基本可行性 |
| `v=7,r=5,k=4` | `not all targets used` | `all_targets_used == False` → 容量約束退化成 `<=`、不加下界（`:245-246`、`:274-275`） |
| `v=8,r=6,k=5` | `always evenly distributed` | `r(k-1) % (v-1) == 0` → λ 整除 |
| `v=10,r=9,k=5` | `all targets used & evenly distributed` | 最嚴格，逼近 BIBD |

`test_setup.py` 則是端到端：解模型 → `get_incidence_matrix` → `generate_chains`，CR（`:10`）與 EC（`:34`）各兩組。它不驗證輸出內容，而是依賴 `generate_chains` 內部的 assert 自我驗證——全函式共 **12 條**（`gen_chain_table.py:29,30,31,32,50,58,59,60,63,64,66,67`），其中 11 條落在下面點名的兩段區間內：

```python
assert len(global_target_list) == len(set(global_target_list)) == num_nodes * num_disks_per_node * num_targets_per_disk
assert all(x == num_targets_on_node[0] for x in num_targets_on_node[1:])
assert all(x == num_targets_on_disk[0] for x in num_targets_on_disk[1:])
```

分別是：target id 無重複、每個節點 target 數相同、每顆盤 target 數相同。**測試把驗證責任下放給被測函式自己的 assert**——這在生產程式碼裡通常算壞味道（assert 在 `-O` 下會被移除），但這是離線工具，跑一次就丟，這樣寫最省事。

`test_model.py:50` 有個細節：

```python
@pytest.mark.parametrize('qlinearize', qlinearize[1:])
```

`qlinearize = [False, True]`，取 `[1:]` 就是**只測 `True`**。`False` 那條路徑（真正的二次規劃）在 CI 裡從來沒被跑過——因為預設 solver 是 HiGHS，而 `:153-154` 會強制把它改成 `True`，測了也是白測。這一行等於誠實地承認：非線性化路徑實質上是死路徑。

---

## 9. `data_placement_job.py`：無法在本 repo 執行

`src/model/data_placement_job.py` 把參數掃描包裝成分散式計算圖，用來一次跑遍多組 `(v, k)` 找出可行組合（`:71-90`）。但它匯入的東西不在這個 repo 裡：

```python
# data_placement_job.py:13-17
from smallpond.common import pytest_running
from smallpond.logical.dataset import ArrowTableDataSet
from smallpond.logical.node import Context, ConsolidateNode, ...
from smallpond.execution.driver import Driver
from smallpond.execution.task import RuntimeContext, ArrowComputeTask
```

我實際查證：

- `find . -name 'smallpond*' -not -path './third_party/*'` → **零結果**
- `requirements.txt` 裡**沒有** `smallpond`，也沒有 `pyarrow`（`:10` 匯入了 `pyarrow as arrow`）

`test/test_plan.py:1` 同樣依賴 `from smallpond.test_fabric import TestFabric`。

**結論：`data_placement_job.py` 與 `test_plan.py` 無法從本 repo 單獨執行**。smallpond 是 DeepSeek 另外開源的資料處理框架，需自行安裝。這兩個檔案是「內部生產環境的用法」被一併開源出來的產物，檔頭註解也印證了這點（`data_placement_job.py:1-3`）：

```python
# local test
# pytest test/test_plan.py -v -x
# production setup
```

實際部署只需要 `data_placement.py` 與 `gen_chain_table.py` 兩個檔案，README 的教學也只用到這兩個。

---

## 10. `deploy/` 的其餘內容

### 10.1 systemd 服務單元

`deploy/systemd/` 有五個單元，對應五個服務端 binary。以 `storage_main.service` 為例：

```ini
# deploy/systemd/storage_main.service:6-11
[Service]
LimitNOFILE=1000000
LimitMEMLOCK=infinity
TimeoutStopSec=5m
ExecStart=/opt/3fs/bin/storage_main --launcher_cfg ... --app-cfg ...
Type=simple
```

三個設定值直接對應前面幾份報告裡的設計：

| 設定 | 為什麼 |
|---|---|
| `LimitNOFILE=1000000` | chunk engine 每個大小分級開 256 個 cluster 檔 × 11 級 × 多顆盤，加上 io_uring 與網路連線，百萬級 fd 是必要的 |
| `LimitMEMLOCK=infinity` | RDMA 的 `ibv_reg_mr` 需要 pin 住記憶體，受 `RLIMIT_MEMLOCK` 管制。不解除就註冊失敗 |
| `TimeoutStopSec=5m` | 給足時間 flush chunk engine 的 pending 寫入與 RocksDB |

**沒有 `Restart=` 設定**——五個單元都沒有自動重啟。這與 3FS 的故障模型一致：storage 掛掉由 mgmtd 的心跳偵測並把 target 標成 OFFLINE，走 CRAQ 的降級路徑；盲目重啟可能讓一個狀態不一致的節點反覆加入又退出，反而干擾 resync。恢復是**人工介入 + resync 協定**，不是進程層級的自動拉起。

`Requires=network-online.target`／`After=network-online.target` 是五個單元共通的唯一依賴——**服務之間沒有 systemd 層級的啟動順序依賴**。mgmtd 沒起來時 meta/storage 會自己重試連線，不需要 systemd 排序。

### 10.2 ClickHouse schema

`deploy/sql/3fs-monitor.sql` 建立 `3fs` 資料庫與 `counters`／`distributions` 等表，對接 `monitor_collector`（見該報告）。幾個值得注意的欄位設計：

```sql
-- deploy/sql/3fs-monitor.sql:4-22
`TIMESTAMP` DateTime CODEC(DoubleDelta),
`metricName` LowCardinality(String) CODEC(ZSTD(1)),
`host` LowCardinality(String) CODEC(ZSTD(1)),
...
ENGINE = MergeTree
PRIMARY KEY (metricName, host, pod, instance, TIMESTAMP)
PARTITION BY toDate(TIMESTAMP)
TTL TIMESTAMP + toIntervalMonth(1)
```

- `DoubleDelta` 編碼時間戳：等距取樣的時間序列壓縮到近乎零成本
- `LowCardinality` 用在所有標籤欄：字典編碼，`metricName`／`host`／`statusCode` 這類重複度極高的欄位省下大量空間
- **TTL 一個月**：監控資料自動過期，不需要人工清理
- 主鍵順序 `(metricName, host, pod, instance, TIMESTAMP)`：查詢模式是「先選指標、再選機器、最後看時間範圍」，與 Grafana 的典型查詢一致

---

## 11. 設計取捨與潛在坑

| 取捨 | 好處 | 代價／風險 |
|---|---|---|
| 離線求解而非執行期演算法（對比一致性雜湊） | 可以達到接近理論最優的均衡；約束可驗證 | 拓樸僵化。加節點要重跑 `RebalanceTrafficModel` 並上傳新 ChainTable，不是自動的 |
| 目標函數設為常數，把最佳化編碼進約束 | MIP 求解快很多；均衡性有硬保證而非「盡量」 | 「多好」不可比較——兩個都可行的解無從分辨優劣 |
| 模型只到節點層級，磁碟層級靠複製 | 求解成本與磁碟數脫鉤 | 假設所有節點的盤數與容量**完全相同**。異質叢集（有的機器 16 盤、有的 24 盤）無法表達 |
| 故障域只有「節點」一層 | 模型簡單 | **無法表達機架／電源域／交換機層級的故障域**。同一機架的多台機器可能被排進同一條鏈 |
| 十進位 id 編碼 | 人可讀，維運友善 | 欄位寬度寫死在 assert 裡（節點 <1e6、盤 <1000、target <100），超過就得改編碼 |
| EC 用單 target 鏈 + 算術群組 | 複用同一張 ChainTable 結構 | 群組關係是隱式的，靠 `chain_index` 的整除關係推導，容易誤解 |
| `--use-new-chunk-engine` 寫死 | 少一個出錯的旋鈕 | 想產生舊引擎的 target 必須改程式碼 |
| 驗證用 `assert` | 寫起來快 | `python -O` 會全部消失。離線工具可接受，但別把這些函式搬進線上服務 |

**最需要留意的是故障域那一條**。模型保證 k 個副本在 k 台不同機器上，但「不同機器」不等於「不同機架」。如果整個機架斷電，同一條鏈的多個副本可能同時失效。要處理這件事，得在模型裡加一層區組約束（例如限制同一群組內來自同機架的節點數 ≤ 1），目前的程式碼沒有這個維度。README 的六節點範例也全在同一網段（`deploy/README.md` 的硬體表：`192.168.1.1`–`192.168.1.6`），沒有跨機架的示範。

---

## 12. 檔案索引

### `deploy/data_placement/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `README.md` | — | 使用教學：5 節點／3 副本／16 盤／每盤 6 target 的完整命令序列與預期輸出 |
| `requirements.txt` | 12 | 依賴宣告：pyomo 6.8.0（建模）、highspy 1.8.0（solver）、pandas／plotly（視覺化）、loguru（日誌）、psutil（CPU 數）、pytest 系列 |
| `.gitignore` | — | 忽略 `output/` 求解結果 |
| `src/__init__.py` | 0 | 套件標記（空） |
| `src/model/data_placement.py` | 549 | **核心**。`DataPlacementModel`（建模／求解／驗證／視覺化）、`RebalanceTrafficModel`（擴容子類，唯一有真實目標函數者）、四個例外類別、`main()` CLI |
| `src/model/data_placement_job.py` | 108 | smallpond 分散式參數掃描，一次跑遍多組 `(v,k)`。**依賴 smallpond／pyarrow，兩者皆不在本 repo 亦不在 requirements** |
| `src/setup/__init__.py` | 0 | 套件標記（空） |
| `src/setup/gen_chain_table.py` | 124 | 把 incidence matrix 展開成具體拓樸：`calc_target_id`（十進位 id 編碼）、`generate_chains`（三層迴圈 + 8 條自我驗證 assert）、`main()` 輸出四個檔案 |
| `test/test_model.py` | 94 | 求解器可行性測試。五組刻意挑選的邊界參數 + v=25 大規模案例 + rebalance 模型（節點數翻倍） |
| `test/test_setup.py` | 55 | 端到端：解模型 → 產生鏈。CR 與 EC 各兩組參數 |
| `test/test_plan.py` | 10 | smallpond 計畫測試。**依賴 smallpond，無法在本 repo 執行** |

### `deploy/` 其餘

| 檔案 | 職責 |
|---|---|
| `README.md` | 六節點叢集手動部署指南（硬體規格、RDMA 設定、FoundationDB、ClickHouse、各服務啟動順序） |
| `sql/3fs-monitor.sql` | ClickHouse schema：`3fs.counters`／`3fs.distributions` 等表，MergeTree + DoubleDelta/ZSTD 編碼 + 一個月 TTL |
| `systemd/mgmtd_main.service` | mgmtd 服務單元 |
| `systemd/meta_main.service` | meta 服務單元 |
| `systemd/storage_main.service` | storage 服務單元（`LimitNOFILE=1000000`、`LimitMEMLOCK=infinity`、`TimeoutStopSec=5m`） |
| `systemd/monitor_collector_main.service` | 監控收集器服務單元 |
| `systemd/hf3fs_fuse_main.service` | FUSE 客戶端服務單元 |

五個 systemd 單元皆**未設定 `Restart=`**，故障恢復依賴 mgmtd 心跳與 resync 協定，而非進程自動拉起。

---

## 13. 延伸閱讀

- 鏈拓樸如何被消費：`mgmtd_main` 報告（ChainTable 版本管理、target 狀態機）
- 鏈上的讀寫協定：`storage_main` 報告（CRAQ、resync）
- 上傳命令的實作：`admin_cli` 報告（`upload-chains`／`upload-chain-table`／`create-target`）
- target 落盤後的實體佈局：`chunk_engine` 報告
- 監控資料的產生端：`monitor_collector_main` 報告
