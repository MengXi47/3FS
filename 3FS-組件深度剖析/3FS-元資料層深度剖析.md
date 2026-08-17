# 3FS 元資料層（Metadata）資料結構深度剖析

> 對應原始碼：`src/fbs/meta/`（跨端共用 schema）、`src/meta/store/`（FDB 落盤層）、`src/meta/components/`（背景組件）
> 底層 KV：FoundationDB（`src/fdb/`、`src/common/kv/`）

---

## 0. 一句話總結

3FS 的元資料**沒有自己的儲存引擎**——它把 POSIX 命名空間拆成三張「邏輯表」（Inode / DirEntry / FileSession），全部塞進 FoundationDB 的單一有序 KV 空間，靠 **key 前綴 + 位元組序（endianness）的刻意選擇** 來同時取得「range scan 局部性」與「避免熱點」這兩個互相矛盾的目標，再靠 FDB 的交易語意取得原子性。所有跨節點的一致性難題（rename、gc、length）都被推回成「一個 FDB transaction」或「一個可重試的冪等操作」。

---

## 1. 整體分層

```
┌──────────────────────────────────────────────────────────┐
│ fbs/meta/Service.h      RPC 請求/回應（StatReq、CloseReq…）│  ← 客戶端/伺服器共用
│ fbs/meta/Schema.h       Inode / DirEntry / Layout / Acl    │  ← 落盤 value 的型別
│ fbs/meta/Common.h       InodeId / PathAt / OpenFlags       │
├──────────────────────────────────────────────────────────┤
│ meta/store/Inode.h      + packKey/load/store/remove        │  ← server 端擴充
│ meta/store/DirEntry.h   + packKey/DirEntryList             │
│ meta/store/FileSession.h                                   │
│ meta/store/Idempotent.h                                    │
├──────────────────────────────────────────────────────────┤
│ common/kv/ITransaction  get / snapshotGet / getRange / set │
│ fdb/                    FoundationDB client                │
└──────────────────────────────────────────────────────────┘
```

關鍵設計：`meta::Inode`（純資料、可序列化、client 也看得到）與 `meta::server::Inode`（繼承前者、加上 KV 存取方法）是**繼承關係而非組合**。`src/meta/store/Inode.h:27`：

```cpp
class Inode : public meta::Inode {
  static std::string packKey(InodeId id);
  static CoTryTask<std::optional<Inode>> snapshotLoad(IReadOnlyTransaction&, InodeId);
  CoTryTask<void> store(IReadWriteTransaction&) const;
};
```

好處是同一份 struct 從 FDB 讀出來後可以直接 serde 序列化回 RPC 給 client，零轉換成本。

---

## 2. Key 空間（KeyPrefix）

`src/common/kv/KeyPrefix-def.h` 用 X-macro 定義了整個叢集共享的 4 byte ASCII 前綴：

| Prefix | 常數 | 用途 |
|---|---|---|
| `INOD` | `Inode` | inode 主表 |
| `DENT` | `Dentry` | 目錄項 |
| `INOS` | `InodeSession` | file session（by inode） |
| `CLIS` | `ClientSession` | **已廢棄**（by client 的二級索引） |
| `IDEM` | `MetaIdempotent` | 冪等記錄 |
| `META` | `MetaDistributor` | meta server 成員表 |
| `USER` / `UTGS` | User / UniversalTags | 使用者 |
| `CHIT` / `CHIF` / `NODE` / `TGIF` | mgmtd | chain table / chain / node / target |
| `CONF` / `SING` | Config / Single | 單鍵設定 |
| `KVTB` / `KVNS` / `KVWG` | 內建 KV 服務 |

前綴用 `enum class KeyPrefix : uint32_t` 而非字串常數，`makePrefixValue()` 在編譯期把 4 字元打包成 uint32：

```cpp
inline constexpr uint32_t makePrefixValue(const char (&s)[5]) {
  return s[0] + (s[1] << 8) + (s[2] << 16) + (s[3] << 24);
}
```

**為什麼用 enum**：註解寫得很白 —「use enum for avoiding duplicated prefixes」。這是把「前綴不可重複」這件事交給編譯器檢查（同一 enum 內重複值雖然合法，但集中在一個檔案裡人眼可查），而不是散落在各模組的 `constexpr auto kPrefix = "INOD"`。在 little-endian 機器上這個 uint32 的位元組佈局剛好就是 `'I','N','O','D'`，所以 FDB 裡的 key 是人類可讀的。

---

## 3. 三張表的 Key 佈局（最精妙的部分）

### 3.1 Inode：刻意用 little-endian

```
key   = "INOD" (4B) + InodeId little-endian (8B)          → 12 bytes
value = serde(InodeData)
```

`src/fbs/meta/Common.h:135`：

```cpp
// Use little endian form as key in FoundationDB, it helps to avoid hot spot
using Key = std::array<uint8_t, 8>;
Key packKey() const { return folly::bit_cast<Key>(folly::Endian::little(val_)); }
```

InodeId 是**遞增分配**的。若用 big-endian，連號 inode 的 key 會在位元組序上相鄰，FDB 的所有新建檔案寫入會全部打到同一個 shard（FDB 依 key range 分片）→ 單點熱點。改成 little-endian 後，低位 byte 變成 key 的最高位，連號 inode 在 key space 上被均勻打散到 256 個位置。

**代價**：`getRange` 掃 inode 表不再有任何語意上的局部性。3FS 接受這個代價，因為 inode 表本來就只有點查（`stat` 走 dentry 拿到 id 再點查 inode）。

### 3.2 DirEntry：刻意用前綴聚集

```
key   = "DENT" (4B) + parent InodeId LE (8B) + name (變長，無長度前綴)
value = serde(DirEntryData)
```

`src/meta/store/DirEntry.cc:43`。name 直接 `putRaw` 貼在尾巴，不加長度前綴——因為它是 key 的最後一段，範圍掃描時 `getRawUntilEnd()` 就能還原。

這裡 parent 也用 LE，但目的不同：**同一個目錄的所有 entry 共享前 12 bytes**，於是 `readdir` 就是一次 prefix range scan，天然連續。父目錄之間則被 LE 打散。這是同一個編碼技巧服務兩個相反需求的漂亮例子。

`DirEntryList::snapshotLoad()` 支援 `(prev, end)` 與 `(begin, end)` 兩種游標形式，並可選擇性地用 `loadInodesConcurrent` 併發預載 inode（`readdirplus` 語意）。

### 3.3 FileSession：inode 為主鍵、內建 256 shard 掃描

```
key   = "INOS" (4B) + InodeId LE (8B) + Uuid sessionId (16B)
value = serde(FileSession{inodeId, clientId, sessionId, timestamp, payload})
```

註解掉的 `SessionByClient`（`CLIS`）顯示他們曾經維護「by client」的二級索引以便 client 掉線時快速清理，後來放棄了——維護雙索引要在同一交易裡寫兩把 key，成本高且容易不一致。取而代之的是 `FileSession::scan(shard, prev)`：

```cpp
static constexpr size_t kShard = 256;  // == 1 << 8
auto beginKey = SessionByInode::packKey(InodeId(shard),     Uuid::max());
auto endKey   = SessionByInode::packKey(InodeId(shard + 1), Uuid::zero());
```

因為 inodeId 是 LE 打包，**key 的第一個 byte 就是 inodeId 的最低位 byte**，所以 `shard = inodeId & 0xff` 這件事不需要額外欄位，直接由 key 佈局免費得到。SessionManager 用 8 個 coroutine 輪掃這 256 個 shard，找出 timestamp 超過 `session_timeout`（預設 5 分鐘）的殭屍 session。

另一個小技巧：待清理的 session 記在 `InodeId(-1)` 這個哨兵下（`createPrune` / `listPrune`），等於在同一張表裡開了一個「工作佇列」分區。

---

## 4. InodeId：一個被切成多段的 64-bit 位址空間

`src/fbs/meta/Common.h:132-233` 是整份 codebase 裡最值得細看的常數表。整個 u64 被切成：

```
0x0000000000000000                 root
0x0000000000000001                 gcRoot
0x0000000000001000 ~ 0x00ffffffffffffff   一般 inode（舊 chunk engine）
0x0100000000000000 ~ 0x01ffffffffffffff   一般 inode（新 chunk engine，bit 56 = 1）
        ↑ kNewChunkEngineMask
        ...（巨大空洞）...
0xfdffffe700000001 ~ 0xffffffe700000000   rm-rf / mv symlink 的臨時 inode
0xfffffffe80000000                 3fs-virt/set-conf
0xffffffff00000000                 3fs-virt/get-conf
0xffffffff7ffe0002 ~ 0xffffffff7fff0001   iov（USRBIO 共享記憶體區，65536 個）
0xffffffff80000000                 iov 目錄
0xfffffffffffffffd                 3fs-virt/rm-rf
0xfffffffffffffffe                 3fs-virt（虛擬目錄根）
```

三個設計決策：

**(a) bit 56 當 chunk engine 旗標。** `InodeId::useNewChunkEngine()` 只看一個 bit。這代表 storage 端要用哪個 chunk engine 是**編碼在 inode id 裡**的，不需要額外查詢；檔案一旦建立就永久綁定。`InodeIdAllocator` 因此嚴格檢查分配出的 id 不得 ≥ `kNewChunkEngineMask`（`InodeIdAllocator.h:90`），新引擎的 id 是在分配後由 `withNewChunkEngine()` 或上去的。

**(b) 高位段全部給 client 端的虛擬檔案。** FUSE 掛載點下的 `3fs-virt/`、USRBIO 的 iov、`rm-rf` 介面都是**純 client 端合成的 inode，從不進 FDB**。用 0xff… 高位段保證與伺服器分配的 id 永不衝突。

**(c) 用 `static_assert` 把不變式編譯期化。** 檔案末尾有 `checkSpecialInode()` — 一個 constexpr 函式，驗證所有特殊區段在數值上嚴格遞增（即互不重疊）：

```cpp
static constexpr auto spacialInodeRanges = std::to_array({...11 個邊界值...});
constexpr inline bool checkSpecialInode() {
  for (uint64_t i = 1; i < spacialInodeRanges.size(); i++)
    if (spacialInodeRanges[i] <= spacialInodeRanges[i - 1]) return false;
  return true;
}
static_assert(checkSpecialInode());
```

這是把「位址空間規劃文件」直接寫成編譯期斷言。任何人日後想插入新的特殊區段，寫錯就編不過。

### InodeId 分配器：52 + 12 的兩級分配

`src/meta/components/InodeIdAllocator.h:52`：

```
[ 高 52 bits: 全域 IdAllocator（走 FDB，32 shard） ][ 低 12 bits: 本地遞增 ]
```

一次 FDB 交易換回 4096 個 id，攤提後幾乎不碰 DB。本地維護一個 `folly::coro::BoundedQueue`（容量 8192），當存量低於 2048 時**非同步**觸發補充（`tryStartAllocateTask`），所以正常路徑上 `allocate()` 是純 lock-free 出隊。全域分配器本身再切 32 個 shard 避免 FDB 交易衝突。

---

## 5. Inode 的值：variant 三態

`src/fbs/meta/Schema.h:316`

```cpp
struct InodeData {
  std::variant<File, Directory, Symlink> type;
  Acl      acl;      // {uid, gid, perm, iflags}
  uint16_t nlink = 1;
  UtcTime  atime, ctime, mtime;   // 預設值 = 2023/6/1
};
struct Inode : InodeData { InodeId id; };
```

`id` 不在 `InodeData` 裡——因為 id 是 key 的一部分，落盤 value 只序列化 `data()`，讀回來時從 key 反解。省 8 bytes × 每個 inode。

### 5.1 File

```cpp
struct File {
  uint64_t length;        // ← 只是「快取」，不是權威值（見 §7）
  uint64_t truncateVer;
  Layout   layout;
  Flags    flags;         // kHasHole = 1
  uint32_t dynStripe;     // 0 = 停用動態 stripe
};
```

### 5.2 Directory：反正規化的反向指標

```cpp
struct Directory {
  InodeId  parent;              // ← 反向指標
  Layout   layout;              // 子檔案繼承的預設 layout
  std::string name;             // ← 自己的名字，也是反向指標
  uint32_t chainAllocCounter;   // 目錄私有的 round-robin 游標
  std::optional<Lock> lock;     // 目錄鎖（by ClientId）
};
```

`parent` + `name` 是刻意的反正規化：`getRealPath`（把 inode id 還原成完整路徑）與 `Inode::loadAncestors()` 只靠 inode 表就能往上爬，不必反查 dentry 表。代價是 rename 目錄時要同時改 dentry 與 inode 兩處（Rename.cc 有處理）。

`loadAncestors()` 還帶了迴圈偵測——用 `std::set<InodeId>` 記錄走過的祖先，發現重複就回 `kInconsistent, "directory tree contains loop"`。這是對「rename 造成目錄環」這個經典 bug 的防禦。

`chainAllocCounter` 搭配 `FS_CHAIN_ALLOCATION_FL`（複用了 ext4 的 `FS_INDEX_FL`，即 `chattr +I`）：一般情況下 chain 分配用 meta server 進程內的全域 round-robin，但打上這個旗標的目錄會用自己的計數器，讓該目錄下的檔案在 chain 上連續分佈。這對「一個目錄 = 一個訓練資料集」的場景有意義。

同樣複用 ext4 旗標的還有 `FS_NEW_CHUNK_ENGINE`（= `FS_SECRM_FL`，`chattr +s`）。`FS_FL_INHERITABLE` 定義了這兩個旗標會被子項繼承。

### 5.3 Symlink

只有一個 `Path target`。權限固定 0777 且從不檢查（`Inode::newSymlink` 的註解）。

---

## 6. Layout：檔案 → chain 的映射函式

`src/fbs/meta/Schema.h:71`。這是元資料層與儲存層的介面。

```cpp
struct Layout {
  ChainTableId      tableId;
  ChainTableVersion tableVersion;
  ChunkSize         chunkSize;    // 必須是 2 的冪
  uint32_t          stripeSize;
  std::variant<Empty, ChainRange, ChainList> chains;
};
```

三種 chains 表示法是空間/彈性的權衡：

| 型別 | 內容 | 用途 |
|---|---|---|
| `Empty` | 無 | 目錄的「模板 layout」，建檔時才實體化 |
| `ChainRange` | `{baseIndex, shuffle, seed}` = **12 bytes** | 一般檔案的預設 |
| `ChainList` | `vector<uint32_t>` = 4 × stripeSize | 手動指定 chain |

**`ChainRange` 是重點**。它不存 chain 列表，只存「起點 + 洗牌演算法 + 種子」，實際列表在讀取時用 `folly::DelayedInit` 惰性生成並快取在記憶體：

```cpp
mutable folly::DelayedInit<std::vector<uint32_t>> chains;  // 不參與序列化

std::span<const uint32_t> ChainRange::getChainIndexList(size_t stripe) const {
  return chains.try_emplace_with([&] {
    std::vector<uint32_t> c(stripe);
    for (uint32_t i = 0; i < stripe; i++) c[i] = baseIndex + i;
    if (shuffle == STD_SHUFFLE_MT19937) hf3fs_shuffle(c, seed);
    return c;
  });
}
```

一個 stripe = 1000 的檔案，layout 在 FDB 裡只佔 ~12 bytes 而不是 4 KB。而 `ChainRange` 的 copy constructor 被手動改寫成**不複製 `chains` 快取**（只複製三個種子欄位），因為快取是純衍生資料。

**為什麼需要 shuffle**：如果 N 個檔案都從 `baseIndex` 開始線性取 stripe 條 chain，那麼所有檔案的 chunk 0 都落在同一條 chain 上 → chunk 0 熱點（很多工作負載會先讀檔頭）。用 `find_safe_seed(stripeSize)` 找一個「安全種子」後洗牌，打散這個相關性。

映射函式本身極簡（`Schema.cc:192`）：

```cpp
ChainRef Layout::getChainOfChunk(const Inode&, size_t chunkIndex) const {
  return ChainRef{tableId, tableVersion, getChainIndexList()[chunkIndex % stripeSize]};
}
```

多 track（未來的多副本並行讀）則用一個質數偏移錯開：`chunkIndex = offset / chunkSize + track * TRACK_OFFSET_FOR_CHAIN`，`TRACK_OFFSET_FOR_CHAIN = 7`，註解說明「it's best to be a prime number, so each track can start from different chains」。

### ChunkId：16 bytes，big-endian，為了排序

```cpp
class ChunkId {
  uint8_t tenent_[1];    // 0x00，多租戶預留
  uint8_t reserved_[1];
  uint8_t inode_[8];     // big-endian
  uint8_t track_[2];     // big-endian，多 track 預留
  uint8_t chunk_[4];     // big-endian
};
static_assert(sizeof(ChunkId) == 16);
```

**和 InodeId 相反，這裡用 big-endian**，註解直說「Use big endian form to keep order」。因為 ChunkId 是給 **storage 端**用的 key，storage 需要「一個 inode 的所有 chunk 連續」以便 `ChunkId::range(inodeId)` 一次範圍刪除（刪檔、truncate）。這裡不怕熱點，因為 chunk 本來就分散在不同 chain / 不同 target 上。

同一份系統裡，兩個 id 依據**誰在掃、誰在寫**選了相反的位元組序——這是很清楚的「編碼服務存取模式」思維。

`chunk_` 只有 32 bits，所以 `File::getChunkId()` 明確檢查溢位並回 `kFileTooLarge`。以 512 KB chunk 計，單檔上限約 2 PB。

---

## 7. 檔案長度：`VersionedLength` 與最終一致

這是整個 metadata 設計裡最「非 POSIX」的部分，也最能看出取捨。

```cpp
struct VersionedLength { uint64_t length; uint64_t truncateVer; };
```

**Inode 裡的 `length` 不是權威值。** 寫入路徑上，client 直接寫 storage，**不經過 meta server**。meta server 完全不知道檔案長到哪裡。真正的長度必須靠 `FileOperation::queryChunks()` 去 storage 端問「最後一個 chunk 在哪、多長」（`src/fbs/meta/FileOperation.cc`）。

那 inode 裡的 `length` 是什麼？是 `close`/`sync` 時同步回來的快取。流程（`BatchOperation.cc:255-302`）：

```cpp
CoTryTask<VersionedLength> BatchedOp::queryLength(inode, hintLength, truncate) {
  auto curr = inode.asFile().getVersionedLength();
  if (hintLength && !config().ignore_length_hint()) {
    // 1. client 的 hint 不比現值大 → 不用更新，省一次跨網路查詢
    if (curr.truncateVer >= hint->truncateVer && curr.length >= hint->length) return curr;
    // 2. 同一個 truncate 世代、hint 更大 → 直接信 client
    if (hint->truncateVer == curr.truncateVer && hint->length > ...) return *hint;
  }
  // 3. 只好去問 storage
  auto length = co_await fileHelper().queryLength(user, inode);
  auto ver = (truncate || *length < inode.asFile().length) ? curr.truncateVer + 1 : curr.truncateVer;
  return VersionedLength{*length, ver};
}
```

**`truncateVer` 解決的是什麼問題**：長度單調遞增時，「取 max」就能安全合併多個併發 client 的回報。但 `truncate` 會讓長度變小，此時「取 max」會把舊的大長度復活。加上 `truncateVer` 後，比較變成字典序 `(truncateVer, length)`，truncate 遞增世代號，舊回報自然失效。`VersionedLength::mergeHint()` 就是這個合併函式。

**代價很明確**：`stat` 一個正在被寫入的檔案，拿到的 size 可能是舊的。3FS 用 `dynStripe` 進一步壓低查詢成本——新檔案初始只用 `dynamic_stripe_initial` 條 chain，寫大了才按 `stripeGrowth`（≥2）倍數擴張（`SetAttr.h:130-139`）。查長度時只需掃 `dynStripe` 條 chain 而非全部 `stripeSize` 條，小檔案的 `queryChunks` 成本從 O(stripeSize) 降到 O(1)。`FileOperation.cc:162` 還加了一道保險：若在超出 `dynStripe` 的位置發現 chunk，就報錯重試（代表 dynStripe 資訊過期）。

`Flags::kHasHole` 標記檔案有洞——有洞的檔案不能用「最後一個 chunk」推算長度，必須全掃。

---

## 8. DirEntry 的三個「多餘」欄位

```cpp
struct DirEntryData {
  InodeId  id;
  InodeType type;
  std::optional<Acl>    dirAcl;   // 只有目錄才有
  Uuid                  uuid;
  std::optional<GcInfo> gcInfo;   // {uid, origPath}
};
struct DirEntry : DirEntryData { InodeId parent; std::string name; };
```

三個欄位都是為了消除一次 KV 讀取或解決一個分散式問題：

**`dirAcl` — 路徑解析的關鍵優化。** 解析 `/a/b/c/d` 需要對每一層目錄做 x 權限檢查。若 ACL 只存在 inode 裡，解析 N 層要 2N 次讀取（每層 dentry + inode）。把目錄的 ACL 複製一份到 dentry 後，變成 N 次。`valid()` 用不變式強制兩者對齊：

```cpp
if ((type == InodeType::Directory) != dirAcl.has_value()) return INVALID(...);
```

這是**受控的反正規化**：`chmod` 一個目錄要同時改 inode 與 dentry 兩處，但讀多寫少，划算。搭配 `AclCache`（32 shard 的 `EvictingCacheMap`，TTL 帶 0.8~1.0 隨機抖動避免同時失效）進一步減少讀取。`PathResolveOp::ResolveResult::parent` 是個 `variant<pair<InodeId,Acl>, Inode, DirEntry>`——三種「我知道父目錄多少資訊」的狀態，讓呼叫端按需惰性升級。

**`uuid` — rename 的冪等標記。** rename 是「刪一個 dentry + 建一個 dentry」，若交易提交後回應丟失、client 重試，第二次會找不到來源而失敗（或更糟，覆蓋掉別人的檔案）。`Rename.cc:248` 因此檢查：

```cpp
if (dstEntry->uuid != Uuid::zero() && dstEntry->uuid == req_.uuid) {
  // 這次 rename 其實已經成功了，直接回成功
}
```

新 dentry 寫入時帶上 `req_.uuid`（`Rename.cc:330`）。用「結果上的指紋」而非「操作日誌」達成冪等，零額外儲存。

**`gcInfo` — 垃圾桶的來源資訊。** 見下節。

---

## 9. 刪除：搬進垃圾桶 + 背景 GC

`rm` 一個大目錄不可能在一個 FDB 交易裡完成。3FS 的做法是**兩階段**：

1. `Remove` 操作把 dentry 從原目錄移到 GC 目錄（`InodeId::gcRoot()` 底下的 `GC-Node-{nodeId}`），順便寫入 `GcInfo{uid, origPath}`。這是 O(1) 的原子操作。
2. `GcManager` 背景逐步展開刪除。

GC 目錄下的 entry name 是排序友善的合成字串（`GcManager.h:70`）：

```cpp
fmt::format("{}-{:020d}-{}", prefix, timestamp.toMicroseconds(), inode.toHexString());
```

`prefix` 是任務類型：`d` 目錄、`f` 中檔、`L` 大檔、`S` 小檔。因為 name 是 dentry key 的尾段，**同類型任務自動按時間排序聚集**，掃描時就是一次 prefix range scan，且天然 FIFO。四類任務對應不同的執行緒池優先權（`L` = HI_PRI，`S` = LO_PRI），大檔優先回收空間。這又是一次「把排序需求編碼進 key」。

`GcInfo.origPath` 在遞迴刪除時逐層拼接（`GcManager.cc:507`：`gcInfo->origPath / entry.name`），所以稽核日誌能還原「這個被刪的檔案原本在哪」，即使父目錄的 dentry 早已不存在。`GcInfo.user` 則讓配額回收算到正確的使用者頭上。

`Remove` 之前還有一道 `DirEntryList::recursiveCheckRmPerm()`（`DirEntry.h:128`）——**best-effort** 的遞迴權限檢查，明確帶 `limit` 上限，註解坦承「because the directory may be very large, we may not able to check permissions for entire directory tree」。這是誠實的取捨：完整檢查不可行，就檢查前 N 個並記錄失敗指標。

---

## 10. 冪等記錄（Idempotent）

`src/meta/store/Idempotent.h`。對於無法用「結果指紋」判斷的操作（主要是 remove），把結果本身存進 FDB：

```cpp
key   = "IDEM" + requestId (Uuid) + clientId (Uuid)
value = serde(Record{clientId, requestId, timestamp, Payload<Result<T>>})
```

註解：`// requestId + clientId to avoid hotspot`。requestId 是隨機 Uuid，放在前面讓 key 均勻分佈；若把 clientId 放前面，同一個 client 的所有請求會擠在一起。

`Record::result` 型別是 `serde::Payload<T>`——**巢狀序列化**（把 `Result<T>` 先序列化成 bytes 再當成一個欄位）。這讓 `load()` 可以先只反序列化 `Record<Void>` 檢查 uuid，確認匹配後才反序列化真正的結果，也讓 `clean()` 能只解析 timestamp 而不必知道 T 是什麼。

`clean()` 是背景清理，按 `expire` 掃描過期記錄，回傳 `{nextPrev, hasMore}` 游標供分批推進。

---

## 11. Distributor：把 inode 綁定到 meta server

`src/meta/components/Distributor.h`。多台 meta server 無狀態，但某些操作（batch stat/close/sync 的合併）需要「同一個 inode 的請求盡量落在同一台」。

**選擇演算法是 rendezvous hashing（HRW）**（`src/fbs/meta/Utils.h:251`）：

```cpp
struct Weight : std::array<uint8_t, 16> {
  static Weight calculate(NodeId node, InodeId inode) {
    auto key = Serializer::serRawArgs((uint64_t)node, inode.u64());
    return hash(key.data(), key.size());   // MurmurHash3_x64_128
  }
  static NodeId selectImpl(const vector<NodeId>& nodes, auto& key) {
    // 取 weight 最大者
  }
};
```

兩處 `// NOTE: don't change this` 註解說明這個 hash 是**跨版本相容契約**——所有節點必須算出同一個答案，換 hash 函式會造成滾動升級期間的分裂。

相較 consistent hashing，HRW 的好處是不需要虛擬節點環，成員變動時只有 1/N 的 key 需要遷移，而且無狀態（不需要同步環結構）。代價是 O(N) 選擇，但 meta server 只有個位數台。

**成員表的一致性靠 FDB versionstamp。** 每台 server 定期（1s）用 `setVersionstampedValue` 更新自己的心跳與全域版本鍵。`checkOnServer()` 在交易內讀版本：

```cpp
if (*versionstamp < rlock->versionstamp)
  co_return makeError(TransactionCode::kTooOld, "distributor versionstamp changed");
```

若交易讀到的版本比本地快取舊，直接讓交易失敗重試。這保證「我以為我負責這個 inode」這件事在交易提交點仍然成立——把成員關係的一致性外包給 FDB 的 MVCC，不需要自己做租約。

---

## 12. BatchContext：單一請求內的讀取去重

`src/meta/store/BatchContext.h`。一個 `batchStat` 請求可能要解析多條路徑，它們共享祖先目錄。BatchContext 掛在 `folly::RequestContext` 上（協程間自動傳遞），提供 per-request 的 inode / dentry 讀取快取：

```cpp
LoadGuard<T> loadImpl(map, key) {
  auto guard = map.lock();
  if (auto iter = guard->find(key); iter != guard->end())
    return LoadGuard<T>(false, iter->second);   // 別人已在讀，等他
  auto future = std::make_shared<SharedFuture<T>>();
  guard->emplace(key, future);
  return LoadGuard<T>(true, future);            // 我負責讀
}
```

不只是快取，還是**併發去重**：第二個協程不會發起重複讀取，而是 `co_await future->baton` 等第一個的結果。`LoadGuard` 的解構子確保即使負責讀取的協程異常退出，等待者也會被喚醒（設成 error 而非永久卡死）。

只有 `snapshotLoad` 走這條路徑（`Inode.cc:87`），因為 snapshot 讀不進衝突集，共享結果不影響交易語意。

---

## 13. Snapshot 讀 vs 一般讀：用 NDEBUG 斷言守護

FDB 的 `get` 會把 key 加入讀衝突集（read conflict set），`snapshotGet` 不會。路徑解析全部用 snapshot 讀（`PathResolve.h:25` 的類註解明確說明），因為把整條路徑的每一層都加進衝突集會讓併發寫入互相打架——但這也意味著解析結果可能過期。

3FS 的處理方式很有意思：不是靠文件約定，而是靠 **debug build 的執行期斷言**：

```cpp
#ifndef NDEBUG
  mutable bool snapshotLoaded_ = false;
#endif

CoTryTask<void> Inode::store(IReadWriteTransaction &txn) const {
  assert(!snapshotLoaded_);   // ← snapshot 讀來的物件不准直接寫回
  ...
}
```

`addIntoReadConflict()` 會清掉這個旗標。所以正確用法是「snapshot 讀 → 決定要寫 → 顯式加入衝突集 → 寫」，漏掉中間那步會在 debug build 直接 abort。這把一個微妙的分散式正確性問題轉成本地可測的 bug。

---

## 14. 序列化格式：positional binary，append-only 演進

`src/common/serde/Serde.h`。3FS 自己寫了一套 serde（不用 protobuf/flatbuffers），關鍵性質：

- **無欄位標籤**。二進位輸出時 `void key(std::string_view) {}` 直接忽略欄位名，只按 `SERDE_STRUCT_FIELD` 的宣告順序寫值。欄位名只在 JSON/TOML 輸出時使用。
- **反向長度前綴**。`tableEnd()` 在結構末尾寫 `Varint32(size)`，字串同樣把長度寫在資料後面（`DownwardBytes` 從後往前填）。
- **尾端缺欄位可接受**：

```cpp
if (LIKELY(*table)) return deserialize(o.*type.getter, *table);
// Missing fields at the end are acceptable.
return Void{};
```

這三點合起來決定了 schema 演進規則：**只能在 struct 尾端加欄位，永遠不能刪除、重排或改型別**。舊版讀新資料會忽略尾巴多出來的 bytes，新版讀舊資料時尾端欄位取預設值。

這也解釋了為什麼 `DirEntryData` 裡 `uuid`（rename 冪等）排在 `dirAcl` 之後、`gcInfo` 最後——它們是後來加的。以及 `CloseRsp` 開頭那個 `SERDE_STRUCT_FIELD(_unused, (uint32_t)0)`：一個已廢棄但不能移除的欄位，只能留著佔位。

`std::variant` 的序列化帶 `VariantIndex`，另有 `AutoFallbackVariant`（首個 alternative 是 `UnknownVariantType`）供需要向前相容的場景使用——但 `Layout::chains` 與 `InodeData::type` 都**沒有**用 fallback，代表這兩處被視為封閉集合，新增 alternative 會破壞相容性。

---

## 15. 設計取捨總結

| 決策 | 得到什麼 | 付出什麼 |
|---|---|---|
| InodeId key 用 LE | FDB 寫入無熱點 | inode 表無掃描局部性 |
| DirEntry key 用父 id 前綴 | readdir = 一次 range scan | 目錄過大時單一 FDB shard 壓力集中 |
| ChunkId 用 BE | storage 端可整檔範圍刪除 | — |
| `ChainRange` 惰性展開 | layout 12 bytes vs 4 KB | 每次存取要 shuffle（有快取） |
| dentry 反正規化 `dirAcl` | 路徑解析讀取減半 | chmod 要寫兩處 |
| Directory 反正規化 `parent`/`name` | getRealPath 不查 dentry | rename 目錄要寫兩處 |
| length 最終一致 | 寫入路徑完全繞過 meta server | stat 可能看到舊 size |
| `truncateVer` | 併發長度回報可安全合併 | 每個檔案多 8 bytes |
| `dynStripe` | 小檔案查長度 O(1) | 需要 extend 邏輯與過期偵測 |
| 刪除 = 搬垃圾桶 | rm 大目錄 O(1) 回應 | 空間非即時回收，需要 GC 組件 |
| 冪等靠 uuid / IDEM 表 | 網路重試安全 | 額外儲存 + 清理任務 |
| positional serde | 極小的編碼開銷 | schema 只能 append |
| snapshot 讀 + assert 守護 | 高併發路徑解析 | 需要人工管理衝突集 |

整體風格：**能用 key 編碼解決的，就不加欄位；能用編譯期斷言表達的，就不寫文件；能推遲到背景做的，就不擋在關鍵路徑上。**

---

## 附錄：關鍵檔案索引

| 主題 | 位置 |
|---|---|
| Inode / DirEntry / Layout / Acl 定義 | `src/fbs/meta/Schema.h` |
| InodeId 位址空間 | `src/fbs/meta/Common.h:132-233` |
| Layout → chain 映射 | `src/fbs/meta/Schema.cc:160-197` |
| RPC 請求/回應 | `src/fbs/meta/Service.h` |
| rendezvous hashing | `src/fbs/meta/Utils.h:251` |
| 向 storage 查長度 | `src/fbs/meta/FileOperation.cc` |
| KV key 前綴表 | `src/common/kv/KeyPrefix-def.h` |
| serde 編碼 | `src/common/serde/Serde.h` |
| Inode key 打包 / 載入 | `src/meta/store/Inode.cc:41-103` |
| DirEntry key 打包 / range scan | `src/meta/store/DirEntry.cc:43-120` |
| FileSession 分片掃描 | `src/meta/store/FileSession.cc:230-260` |
| 冪等記錄 | `src/meta/store/Idempotent.h` |
| 長度合併邏輯 | `src/meta/store/ops/BatchOperation.cc:255-302` |
| 路徑解析 | `src/meta/store/PathResolve.h` / `.cc` |
| InodeId 分配器 | `src/meta/components/InodeIdAllocator.h` |
| Distributor | `src/meta/components/Distributor.h` / `.cc` |
| GC | `src/meta/components/GcManager.h` / `.cc` |
| Session 管理 | `src/meta/components/SessionManager.h` |
| ACL 快取 | `src/meta/components/AclCache.h` |
