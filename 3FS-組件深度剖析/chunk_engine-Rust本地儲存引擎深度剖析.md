# 3FS `chunk_engine`：Rust 本地儲存引擎深度剖析

> 對應原始碼：`src/storage/chunk_engine/`（獨立 Cargo crate，3FS 唯一的 Rust 核心組件）
> C++ 消費端：`src/storage/store/ChunkEngine.h` / `.cc`、`src/storage/store/StorageTarget*`、`src/storage/worker/AllocateWorker.cc`
> 建置整合：`cmake/AddCrate.cmake`、`src/storage/CMakeLists.txt`、根 `Cargo.toml`
> 版本：`chunk_engine 0.1.11`，MSRV 1.85.0

---

## 0. 一句話總結

`chunk_engine` 是一個**只做一件事**的本地儲存引擎：把「變長的 chunk 寫入」壓縮成「在 11 種 2 的冪固定大小的槽位裡挑一個空位、寫進去、再用一筆 RocksDB WriteBatch 原子地宣告它」。它不做 B-tree、不做 LSM 資料層、不做 WAL——**資料完全不經過任何 log，直接落在預先 `fallocate` 好的檔案偏移上**；所有需要原子性的東西（chunk meta、位置佔用點陣、用量統計、時間戳索引）全部塞進 RocksDB 的同一個 WriteBatch。

它的三個核心賭注是：

1. **位置即身分**：一個 `Position` 就是一個 u64，逐 bit 打包了 chunk 大小、cluster 檔編號、group 編號、槽位索引；由它可以純算術推出「哪個 fd、哪個 offset」，不需要任何查表。
2. **記憶體先行、磁碟後補**：配置只改記憶體（丟了就丟了，槽位變成暫時洩漏的保留空間，重啟時自然回收）；回收先寫磁碟再改記憶體（`Arc` 引用還在就讀得到）。
3. **舊版本靠 `Arc` 而不是 GC 保留**：讀者手上的 `Arc<Chunk>` 就是「這塊物理空間還不能被重用」的憑證，refcount 歸零時槽位才真正回到 bitmap。

---

## 1. 它在 3FS 裡的位置

```
┌──────────────────────────────────────────────────────────────────────┐
│ StorageService / UpdateWorker / AioReadWorker            （C++）      │
├──────────────────────────────────────────────────────────────────────┤
│ StorageTarget                                            （C++）      │
│   useChunkEngine() ? ChunkEngine::xxx(*engine_, …)                    │
│                    : chunkStore_.xxx()      ← 舊引擎（ChunkStore）    │
├──────────────────────────────────────────────────────────────────────┤
│ ChunkEngine（純靜態函式的轉接層）  storage/store/ChunkEngine.h        │
│   ChunkId + ChainId  →  bytes key                                     │
│   RawMeta            →  ChunkMetadata（含 ~checksum 反相）            │
├══════════════════════════════ cxx bridge ════════════════════════════┤
│ chunk_engine::ffi   （target/cxxbridge/chunk_engine/src/cxx.rs.h）    │
├──────────────────────────────────────────────────────────────────────┤
│ Engine        core/engine.rs      對外唯一入口、meta_cache、writing   │
│ Allocators    alloc/allocators.rs 11 個 chunk 大小分級                │
│ Allocator     alloc/allocator.rs  Mutex<ChunkAllocator> + Clusters    │
│ MetaStore     meta/meta_store.rs  RocksDB 的 5 張邏輯表               │
│ Clusters      file/clusters.rs    256 個 cluster 檔（雙 fd）          │
└──────────────────────────────────────────────────────────────────────┘
```

一個 storage node 上，**每顆磁碟一個 `Engine`**（`StorageTargets.cc:52-70`，路徑是 `<target_path>/engine`），同一顆碟上的所有 target（chain）共用它。`chain_id` 被當成 chunk key 的前綴（`prefix_len = sizeof(ChainId) = 8`，`StorageTargets.cc:59`），所以「一條 chain 的所有 chunk」在 RocksDB 裡天然連續。

新舊引擎是**共存**的：`StorageTarget::useChunkEngine()` 讀 `targetConfig_.only_chunk_engine`（`StorageTarget.h:162`），逐 target 決定走 Rust 引擎還是舊的 `ChunkStore`。`punchHole()`、`sync()`、`setEmergencyRecycling()` 在新引擎下全部退化成 no-op（`StorageTarget.h:84-140`）——因為這些事情 Rust 引擎自己在背景做了。

### 1.1 建置鏈

`cmake/AddCrate.cmake` 是整條鏈的全部內容，只有 33 行：

```cmake
macro(add_crate NAME)
    set(LIBRARY "${PROJECT_SOURCE_DIR}/target/${TARGET_DIR}/lib${NAME}.a")
    set(SOURCES
        "${PROJECT_SOURCE_DIR}/target/cxxbridge/${NAME}/src/cxx.rs.h"
        "${PROJECT_SOURCE_DIR}/target/cxxbridge/${NAME}/src/cxx.rs.cc")
    add_library(${NAME} STATIC ${SOURCES} ${LIBRARY})
    target_link_libraries(${NAME} pthread dl ${LIBRARY})
    target_include_directories(${NAME} PUBLIC "${PROJECT_SOURCE_DIR}/target/cxxbridge")
endmacro()
```

要點：

- **cargo 是 build 的第一等公民**：`add_custom_target(cargo_build_all ALL COMMAND cargo build [--release])` 在 workspace 根目錄跑一次，所有 crate 一起編（`AddCrate.cmake:9-13`）。CMake 不管 Rust 的增量，交給 cargo。
- **產物是 `.a` 靜態庫**，因為 `Cargo.toml:9` 宣告 `crate-type = ["lib", "staticlib"]`。
- **cxx 產生的 `.h`/`.cc` 被當成一般 C++ 原始檔加進 target**，路徑固定在 `target/cxxbridge/<crate>/src/cxx.rs.{h,cc}`。`build.rs` 只有四行，就是叫 `cxx_build::bridge("src/cxx.rs")` 生成它們。
- `-Wno-dollar-in-identifier-extension`：cxx 生成的 C++ 符號含 `$`。
- `src/storage/CMakeLists.txt:1,3` 把 `chunk_engine` 當成一般的 lib 連進 `storage`。

根 `Cargo.toml` 的 workspace 設定值得單獨看：

```toml
[workspace]
members = ["src/client/trash_cleaner", "src/storage/chunk_engine", "src/lib/rs/hf3fs-usrbio-sys"]
default-members = ["src/client/trash_cleaner", "src/storage/chunk_engine"]

[profile.release-cmake]
debug = true
inherits = "release"
lto = true
```

`hf3fs-usrbio-sys` 在 `members` 但不在 `default-members`，所以 `cargo build` 不會編它（它需要先有 C++ 產物）。`release-cmake` profile 開 `lto = true` 又保留 `debug = true`——完整優化 + 完整符號，讓 storage 節點上的 perf/core dump 能看到 Rust 的行號。不過 `AddCrate.cmake:5` 實際用的是 `cargo build --release`，`release-cmake` 這個 profile 目前並未被 CMake 引用。

---

## 2. 模組地圖

```
src/
├── lib.rs           7 個 mod + 全部 pub use *（扁平命名空間）
├── cxx.rs           #[cxx::bridge] + 所有 raw_* 轉接函式          604 行
├── core/engine.rs   Engine：對外唯一入口                          1705 行
├── alloc/
│   ├── allocators.rs      11 個分級的 Allocator 陣列
│   ├── allocator.rs       Mutex<ChunkAllocator> + Clusters
│   ├── chunk_allocator.rs 五個集合的槽位狀態機 + position_rc
│   ├── group_allocator.rs allocated/unallocated group 池
│   ├── chunk.rs           Chunk：COW / safe_write / RAII 引用
│   ├── writing_chunk.rs   WritingChunk：未提交寫入的 RAII 憑證
│   ├── allocator_counter.rs 原子計數器
│   └── metrics.rs         18 個 AtomicU64
├── meta/
│   ├── meta_key.rs        9 個 key 前綴的編解碼
│   ├── meta_store.rs      RocksDB 上的 5 張邏輯表                 878 行
│   ├── meta_merge.rs      RocksDB MergeOperator
│   └── rocksdb.rs         rocksdb crate 的薄封裝
├── file/
│   ├── clusters.rs        256 個 cluster 檔
│   ├── cluster.rs         單檔雙 fd（O_SYNC / O_DIRECT）
│   └── fs_type.rs         statfs 偵測 ZFS
├── types/
│   ├── position.rs        u64 位元打包（本文核心）
│   ├── group_id.rs        u64 位元打包
│   ├── group_state.rs     256-bit bitset
│   ├── merge_state.rs     bitset 的差量（給 MergeOperator）
│   ├── chunk_meta.rs      落盤的 value 型別
│   └── constants.rs       11 個 chunk 大小分級
└── utils/                 Size / AlignedBuffer / ShardsMap / Worker / Error
```

依賴方向是嚴格單向的：`types` ← `utils` ← `file` ← `meta` ← `alloc` ← `core` ← `cxx`。`lib.rs` 把所有東西 `pub use *` 成一個扁平命名空間，各模組內部一律 `use super::super::*` 取回全部符號——這在 3FS 的其他 Rust code 也是同樣風格。

---

## 3. 空間管理：四層階層

### 3.1 Chunk → Group → Cluster 檔 → 分級目錄

```
Engine 根目錄 <target_path>/engine/
├── meta/                     ← RocksDB（所有 metadata）
├── 64KiB/                    ← 分級 0
│   ├── 00  01  02 … FF       ← 256 個 cluster 檔
├── 128KiB/                   ← 分級 1
│   ├── 00 … FF
├── 256KiB/ … 4MiB/ … 64MiB/  ← 分級 2 … 10
```

目錄名來自 `Size::Display`（`utils/size.rs:206-222`），檔名是 `format!("{:02X}", cluster_id)`，`cluster_id ∈ [0, 256)`（`file/clusters.rs:18,29-31`）。

單一 cluster 檔內部：

```
cluster 檔 "07"（分級 = 512KiB）
┌────────────────────────────────────────────────────────────────────┐
│ group 0            │ group 1            │ group 2            │ …   │
│ 256 × 512KiB = 128MiB                                              │
├──┬──┬──┬─────┬──┬──┼──┬──┬─────────┬──┼─────────────────────┼─────┤
│ 0│ 1│ 2│ ……  │254│255│ 0│ 1│  ……   │255│                     │     │
└──┴──┴──┴─────┴──┴──┴──┴──┴─────────┴──┴─────────────────────┴─────┘
  ↑                                                                    
  index（8 bit），一個 slot 就是一個完整的 chunk 大小

byte offset = chunk_size × (group × 256 + index)
```

**group 是 `fallocate` 的最小單位**，`GroupState::TOTAL_BITS = 256` 個槽位（`types/group_state.rs:14-15`：`[u64; 4]` → 32 bytes → 256 bits）。`GroupId::size() = chunk_size × 256`（`types/group_id.rs:38-40`），所以：

| 分級 | chunk 大小 | group 大小 | 一次 fallocate |
|---|---|---|---|
| 0 | 64 KiB | 16 MiB | 16 MiB |
| 1 | 128 KiB | 32 MiB | 32 MiB |
| 2 | 256 KiB | 64 MiB | 64 MiB |
| 3 | 512 KiB | 128 MiB | 128 MiB |
| … | … | … | … |
| 6 | 4 MiB | 1 GiB | 1 GiB |
| 10 | 64 MiB | 16 GiB | 16 GiB |

### 3.2 為什麼是 11 級、為什麼是 2 的冪

```rust
// src/storage/chunk_engine/src/types/constants.rs:3-8
pub const CHUNK_SIZE_SMALL: Size = Size::kibibyte(64);
pub const CHUNK_SIZE_NORMAL: Size = Size::kibibyte(512);
pub const CHUNK_SIZE_LARGE: Size = Size::mebibyte(4);
pub const CHUNK_SIZE_ULTRA: Size = Size::mebibyte(64);
pub const CHUNK_SIZE_SHIFT: usize = 16; // 64KiB is 2^16
pub const CHUNK_SIZE_NUMBER: usize = 11; // from 64KiB to 64MiB
```

`Allocators::new` 就是 `for i in 0..11 { chunk_size = 64KiB << i }`（`alloc/allocators.rs:14-16`），11 級 = 2^16 .. 2^26。

2 的冪不是為了省空間，是為了讓**兩個查找函式退化成一個 `trailing_zeros`**：

```rust
// src/storage/chunk_engine/src/alloc/allocators.rs:43-67
pub fn select_by_pos(&self, pos: Position) -> Result<&Arc<Allocator>> {
    let chunk_size = pos.chunk_size();
    if chunk_size.is_power_of_two() && CHUNK_SIZE_SMALL <= chunk_size && chunk_size <= CHUNK_SIZE_ULTRA {
        Ok(&self.vec[chunk_size.trailing_zeros() as usize - CHUNK_SIZE_SHIFT])
    } else { … }
}

pub fn select_by_size(&self, size: Size) -> Result<&Arc<Allocator>> {
    if size <= CHUNK_SIZE_SMALL { Ok(&self.vec[0]) }
    else if size <= CHUNK_SIZE_ULTRA {
        Ok(&self.vec[size.next_power_of_two().trailing_zeros() as usize - CHUNK_SIZE_SHIFT])
    } else { … }
}
```

`select_by_size` 是「向上取到最近的 2 的冪」，也就是**最壞浪費接近 50%**——這是刻意的取捨：換來的是槽位完全同構、bitmap 只需要一個 bit、`Position` 不需要記長度、compact 時搬移不需要重新配置大小。而 3FS 的實際負載是固定 chunk 大小（`chunk_size_list` 由上層配置），所以浪費在生產環境接近 0。

超過 64MiB 的請求直接 `Error::InvalidArg`——不會有這種請求，因為上層的單次寫入被 chunk 大小限制住了。

`select_by_pos` 的 `is_power_of_two()` 檢查是**防禦性的反序列化驗證**：`Position` 從 RocksDB 讀回來，如果 bit 被打壞，chunk_size 欄位可能不是 2 的冪，此時寧可報錯也不能拿它當陣列下標。

### 3.3 fallocate 與 punch hole

`Cluster` 對一個檔案開**兩個 fd**：

```rust
// src/storage/chunk_engine/src/file/cluster.rs:11-41
pub struct Cluster {
    pub normal_fd: File,   // O_SYNC
    pub direct_fd: File,   // O_DIRECT（ZFS 上退化成 O_SYNC）
}
```

`FsType::check` 用 `libc::statfs` 認出檔案系統，只有 ZFS 被判定為 `!support_direct_io`（`file/fs_type.rs:20-31`）。

空間的兩個操作都走同一個 syscall，只差 flags：

```rust
// src/storage/chunk_engine/src/file/cluster.rs:9,43-61
const PUNCH_HOLE_FLAGS: i32 = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;

pub fn fallocate(&self, group_id: GroupId, punch_hole: bool) -> Result<()> {
    let res = unsafe {
        libc::fallocate(
            self.direct_fd.as_raw_fd(),
            if punch_hole { PUNCH_HOLE_FLAGS } else { 0 },
            group_id.offset().into(),
            group_id.size().into(),
        )
    };
    …
}
```

- **配置 group**（`Clusters::allocate`，`file/clusters.rs:41-43`）：`mode = 0` 的 `fallocate`，實體配置 `[group.offset, +group.size)`，檔案 size 隨之增長。
- **釋放 group**（`Clusters::deallocate`，`file/clusters.rs:45-47`）：punch hole + `KEEP_SIZE`，把該區間打洞歸還給檔案系統，但**檔案表觀大小不變**。

`file/clusters.rs:94-109` 的測試把這個語意釘死了：配置 group 0 再配置 group 3，檔案長度變成 `4 × group.size`（中間 group 1、2 是空洞）；punch hole 掉 group 0 和 group 3 之後，**長度仍然是 `4 × group.size`**。

保持檔案 size 不變是關鍵：`Position` 算出來的 offset 是絕對位址，如果檔案會縮短，已配置的高位 group 位址就會失效。`KEEP_SIZE` 讓位址空間永遠是單調增長的，只有實體佔用會回收。

group 什麼時候被 punch hole，見 §6.5 的 compact 流程。

---

## 4. `Position` 與 `GroupId` 的位元打包

這是整個 crate 最密的地方。

### 4.1 `GroupId`：64 bit

```rust
// src/storage/chunk_engine/src/types/group_id.rs:13-19
// 32bit chunk size + 24bit group + 8bit cluster
const SHIFT: u32 = 8;
pub const COUNT: u32 = (1 << Self::SHIFT);   // 256

pub const fn new(chunk_size: Size, cluster: u8, group: u32) -> Self {
    Self(chunk_size.0 << 32 | (group << Self::SHIFT | cluster as u32) as u64)
}
```

```
GroupId（u64）
 63                             32 31                        8 7        0
┌─────────────────────────────────┬───────────────────────────┬──────────┐
│      chunk_size（原值，位元組）  │      group（24 bit）      │ cluster  │
└─────────────────────────────────┴───────────────────────────┴──────────┘
        chunk_size = 2^16 … 2^26              0 … 16,777,215     0 … 255
        → 實際只用到 bit 48 … bit 58
```

`chunk_size` 存的是**原始位元組數**（65536、131072 …），不是指數。因為它是 2 的冪，實際只有一個 bit 被設起來，落在 bit 48（64KiB）到 bit 58（64MiB）之間。bit 59..63 永遠是 0。

`cluster` 放在**最低位**是刻意的，因為 `GroupId::plus_one()` 只是 `self.0 + 1`（`types/group_id.rs:42-44`）——遞增時先跑滿 256 個 cluster 才進位到 group + 1。`types/group_id.rs:91-113` 的測試專門驗證這個進位：連續 `plus_one()` 1000 × 256 次，每 256 次 group 加一、cluster 歸零。

這代表**新 group 的配置順序是「橫著鋪」而不是「豎著鋪」**：group 0 的 256 個 cluster 檔各配一塊，再進到 group 1。負載天然打散到 256 個檔案上。

### 4.2 `GroupId::offset()` 的運算子優先級陷阱

```rust
// src/storage/chunk_engine/src/types/group_id.rs:33-36
pub fn offset(&self) -> Size {
    const MARKS: u64 = !(GroupId::COUNT - 1) as u64;
    self.chunk_size() * (self.0 & MARKS)
}
```

`!(GroupId::COUNT - 1) as u64`——Rust 的一元 `!` 優先級**高於** `as`，所以這是 `(!(255u32)) as u64` = `0x0000_0000_FFFF_FF00`，不是 `!(255u64)` = `0xFFFF_FFFF_FFFF_FF00`。

差別是決定性的：前者同時遮掉了**低 8 bit 的 cluster** 和**高 32 bit 的 chunk_size**，剩下 `group << 8`，於是 `offset = chunk_size × group × 256` = 該 group 在 cluster 檔內的起始位元組。若寫成後者，chunk_size 會被乘進去，結果是天文數字。

對比 `Position::new` 裡同名的常數，括號是**顯式**的：

```rust
// src/storage/chunk_engine/src/types/position.rs:14
const CLEAN: u64 = !((GroupId::COUNT - 1) as u64);   // 0xFFFF_FFFF_FFFF_FF00
```

同一個 crate 裡兩個寫法、兩個值、兩種用途——這是讀這份 code 最容易看走眼的一處。

### 4.3 `Position`：64 bit

```rust
// src/storage/chunk_engine/src/types/position.rs:10-43
const SHIFT: u32 = 8;

// 24bit chunk size + 8bit cluster + 24bit group + 8bit zero
pub const fn new(group_id: GroupId, index: u8) -> Self {
    const CLEAN: u64 = !((GroupId::COUNT - 1) as u64);
    Self(group_id.inner() & CLEAN | index as u64 | (group_id.cluster() as u64) << 32)
}

pub fn chunk_size(&self) -> Size { Size::new(self.0 >> 40 << 8) }
pub fn cluster(&self)    -> u8   { (self.0 >> 32) as u8 }
pub fn group(&self)      -> u32  { (self.0 as u32) >> Self::SHIFT }
pub fn index(&self)      -> u8   { self.0 as u8 }
pub fn offset(&self)     -> Size { self.chunk_size() * self.0 as u32 as u64 }
```

逐 bit 拆解：

```
Position（u64）
 63          58 …… 48    47   40 39        32 31                        8 7        0
┌──────────────────────────┬──────┬───────────┬───────────────────────────┬──────────┐
│   chunk_size >> 8        │ (0)  │  cluster  │      group（24 bit）      │  index   │
│   （24 bit 欄位）        │      │  （8 bit）│                           │ （8 bit）│
└──────────────────────────┴──────┴───────────┴───────────────────────────┴──────────┘
 bit 40 起的 24 bit 欄位存 chunk_size>>8               ↑                       ↑
 chunk_size = 2^16..2^26 → 欄位值 = 2^8..2^18          │                       │
 → 唯一置位落在 bit 48 … bit 58                        │                       │
                                                        │                       │
        ┌───────────────────────────────────────────────┴───────────────────────┘
        │  低 32 bit 合起來就是「這個 chunk 在 cluster 檔內的第幾個 slot」
        │  slot_no = group × 256 + index
        │  byte offset = chunk_size × slot_no
        └── 這就是 offset() 為什麼可以寫成 chunk_size * (self.0 as u32)
```

三個設計決定：

**(a) `chunk_size` 被右移 8 位再存。** `GroupId` 裡 chunk_size 從 bit 32 起，`Position` 裡從 bit 40 起——因為 bit 32..40 這 8 個 bit 被挪去放 cluster 了。`Position::new` 做的就是「把 GroupId 的 cluster 從 bit 0..8 搬到 bit 32..40，把騰出來的 bit 0..8 給 index」。chunk_size 的位元位置**完全沒動**（`group_id.inner() & CLEAN` 保留了高位原封不動），`chunk_size()` 的 `>> 40 << 8` 只是換一個角度讀同樣的 bit。

**(b) 低 32 bit 就是 slot 序號。** 這讓 `offset()` 退化成一次乘法：`self.0 as u32 as u64` 直接截斷出 `group << 8 | index`。整條讀寫路徑上，從 `Position` 到 `(fd, byte_offset)` 只需要：一次陣列索引（`files[pos.cluster()]`）、一次位移、一次乘法。**沒有任何查表、沒有任何鎖。**

**(c) `group_id()` 是無損可逆的。**

```rust
// src/storage/chunk_engine/src/types/position.rs:18-22
pub fn group_id(&self) -> GroupId {
    const MARKS: u64 = (GroupId::COUNT - 1) as u64;
    const CLEAN: u64 = !(MARKS | MARKS << 32);   // 清掉 bit 0..8 與 bit 32..40
    GroupId::from(self.0 & CLEAN | self.cluster() as u64)
}
```

清掉 index 與「bit 32 的 cluster」，再把 cluster 塞回低 8 位——完美還原原 `GroupId`。`types/position.rs:94-116` 的測試把 `GroupId::new(64KiB, 23, 233)` → `Position::new(_, 223)` → `group_id()` 這一圈的每個欄位都比對過。

### 4.4 值域夠不夠用

| 欄位 | 位元 | 值域 | 是否夠用 |
|---|---|---|---|
| `index` | 8 | 0..255 | 恰好等於 `GroupState::TOTAL_BITS`，一 bit 不多一 bit 不少 |
| `group` | 24 | 0..16,777,215 | 單一 (分級, cluster) 下 16.7M 個 group |
| `cluster` | 8 | 0..255 | 等於 `Clusters::COUNT` |
| `chunk_size` | 24（實用 11） | 2^16..2^26 | 11 個分級，剩 13 bit 完全未用 |

單一分級的定址上限 = `chunk_size × 2^32`。最小分級 64KiB 時是 **256 TiB**；4MiB 分級時是 16 PiB。README 的規劃是「單碟 30TB、每個 chunk 大小 256 個檔案」，`group` 的 24 bit 用掉不到 1%。

`chunk_size` 欄位的 24 bit 只被用了 11 個值——理論上可以擴展到 2^8（256B）至 2^31（2GiB）而不動 `Position` 的編碼。

### 4.5 `Position` 落盤時是大端

三張 RocksDB 表裡凡是拿 `Position`/`GroupId` 當 key 的，一律 `to_be_bytes()`（`meta/meta_key.rs:50,56,79,86`）——這與 3FS 元資料層在 FDB 上刻意用 little-endian 打散熱點的做法**恰好相反**。原因見 §7.2：這裡要的就是排序局部性，而 RocksDB 是本機單執行緒 LSM，沒有 shard 熱點問題。

---

## 5. `GroupState`：256-bit 點陣圖與 level 分桶

```rust
// src/storage/chunk_engine/src/types/group_state.rs:4-18
type Item = u64;
type Bits = [Item; 4];

pub struct GroupState { bits: Bits, count: u32 }

const TOTAL_BYTES: usize = 32;
pub const TOTAL_BITS: usize = 256;
pub const ITEM_BITS:  u8   = 64;
pub const LEN:        usize = 4;
pub const LEVELS:     usize = 4;
```

配置就是「找第一個 0 bit」：

```rust
// src/storage/chunk_engine/src/types/group_state.rs:56-68
pub fn allocate(&mut self) -> Option<u8> {
    for (i, v) in self.bits.iter_mut().enumerate() {
        if let Some(mark) = NonZeroU64::new(!*v) {
            let idx = mark.trailing_zeros();
            *v |= 1 << idx;
            self.count += 1;
            return Some(i as u8 * Self::ITEM_BITS + idx as u8);
        }
    }
    None
}
```

`NonZeroU64::new(!*v)` 這個寫法值得注意：`u64::trailing_zeros()` 對 0 回傳 64（未定義槽位），包成 `NonZeroU64` 之後編譯器知道值非 0，`tzcnt`/`bsf` 可以省掉分支。這正是 README 提到的 `__builtin_ctz` 快速路徑。

`count` 是**冗餘欄位**：它可以從 `bits` 算出來（`from()` 和 `update()` 都用 `count_ones().sum()` 重算，`group_state.rs:30,111`），但 `is_empty()` / `is_full()` / `level()` 在配置熱路徑上被高頻呼叫，維護一個 u32 比每次 4 次 popcount 便宜。

`level()` 把佔用率分成 4 桶（`group_state.rs:74-76`）：

```
level = count / 64
  level 0: count ∈ [  0,  64)   最空
  level 1: count ∈ [ 64, 128)
  level 2: count ∈ [128, 192)
  level 3: count ∈ [192, 256)   最滿（256 就不在 active 裡了）
```

`ChunkAllocator::allocate` **從 level 3 往下找**（`alloc/chunk_allocator.rs:99`：`for level in (0..GroupState::LEVELS).rev()`）——優先填滿已經很滿的 group，讓空 group 保持全空，好被 compact 掉整個 punch hole。這是典型的 best-fit / 減少碎片策略，代價是分配時可能要試 4 個集合。

**落盤格式是裸 32 bytes**：

```rust
// src/storage/chunk_engine/src/types/group_state.rs:114-120
pub fn as_bytes(&self) -> &[u8; Self::TOTAL_BYTES] {
    unsafe { std::mem::transmute(&self.bits) }
}
```

`count` 不落盤，`GroupState::from()` 讀回來時重算並校驗長度必須剛好 32（`group_state.rs:22-28`）。`[u64;4]` 到 `[u8;32]` 的 transmute 是安全的（同 size、u8 對齊要求更寬鬆），但**位元組序是本機序**——group bits 的 blob 不能跨 endianness 遷移。這在同構叢集裡不是問題。

### 5.1 `MergeState`：為什麼不直接寫整個 bitset

```rust
// src/storage/chunk_engine/src/types/merge_state.rs:6-42
pub struct MergeState { pub acquire: HashSet<u8>, pub release: HashSet<u8> }

pub fn merge(&mut self, right: &Self) {
    for pos in &right.acquire { self.acquire.insert(*pos); self.release.remove(pos); }
    for pos in &right.release { self.acquire.remove(pos);  self.release.insert(*pos); }
}
```

如果每次配置/釋放都 read-modify-write 整個 32 byte bitset，就必須先讀 RocksDB、再算、再寫——這是一次同步讀 + 一個 race window。`MergeState` 把它變成**只寫差量**：「我要 set 第 88 個 bit」序列化成一個小 blob，`write_batch.merge(...)`。RocksDB 的 MergeOperator 在讀取或 compaction 時才把差量疊到 base 上。

`merge()` 的 `acquire`/`release` 互相踢除，保證同一個 index 在合併後的 `MergeState` 裡只會出現在一邊——這使得多個差量可以任意結合而結果一致（滿足 MergeOperator 對 `partial_merge` 的結合律要求）。`types/merge_state.rs:74-86` 的測試把 256 個 acquire 疊起來得到 full state，再疊 256 個 release 回到 empty。

---

## 6. 分配器：五個集合的狀態機

### 6.1 資料結構

```rust
// src/storage/chunk_engine/src/alloc/chunk_allocator.rs:7-15
pub struct ChunkAllocator {
    pub full_groups:    ShardsSet<GroupId>,                   // count == 256
    pub active_groups:  ShardsMap<GroupId, GroupState>,       // 0 < count < 256
    active_levels:      [ShardsSet<GroupId>; 4],              // active 依 level 分桶
    frozen_groups:      ShardsMap<GroupId, GroupState>,       // 正在被 compact
    group_allocator:    GroupAllocator,
    position_rc:        ShardsMap<Position, u32>,             // 位置引用計數
    counter:            Arc<AllocatorCounter>,
}

// src/storage/chunk_engine/src/alloc/group_allocator.rs:5-10
pub struct GroupAllocator {
    allocated_groups:   ShardsSet<GroupId>,   // 磁碟空間已 fallocate、無任何 chunk
    unallocated_groups: ShardsSet<GroupId>,   // 曾被 punch hole 掉的 group id（可重用）
    next_group_id:      GroupId,              // 從未用過的下一個 id
    counter:            Arc<AllocatorCounter>,
}
```

一個 group 在任意時刻**恰好**屬於五個集合之一（或處於 `next_group_id` 之後的「不存在」狀態）：

```
                       ┌───────────────────────────────────────────┐
                       │            不存在（≥ next_group_id）      │
                       └────────────────────┬──────────────────────┘
                                            │ get_unallocated_group_id()
                                            ↓
     ┌───────────────────────┐   fallocate  ┌──────────────────────┐
     │  unallocated_groups   │─────────────→│  allocated_groups    │
     │  （id 已知，無空間）  │              │  （有空間，0 chunk） │
     └───────────────────────┘              └──────────┬───────────┘
                 ↑                                     │ 第一次 allocate()
    punch hole   │                                     ↓
                 │                          ┌──────────────────────┐
                 │              deallocate  │   active_groups      │
                 └──────────────────────────│  + active_levels[l]  │
                        count 掉到 0        │  （1 ≤ count ≤ 255） │
                                            └───┬──────────────┬───┘
                                    count → 256 │              │ get_compact_task
                                                ↓              ↓
                                     ┌──────────────┐  ┌────────────────┐
                                     │ full_groups  │  │ frozen_groups  │
                                     │  (count=256) │  │  （搬遷中）    │
                                     └──────────────┘  └────────────────┘
```

`ShardsSet` / `ShardsMap` 是 64 路分片的 `HashSet`/`HashMap`（`utils/shards_map.rs:8`、`utils/shards_set.rs:5`），用 `DefaultHasher` 取模分片。**注意它們不含任何鎖**——分片的目的不是併發，而是**避免單一巨大 HashMap 在 rehash 時的長尾延遲**：`position_rc` 的初始容量是 `1 << 20`（`chunk_allocator.rs:91`），一次 rehash 百萬條目會造成明顯卡頓，切成 64 份之後每次只 rehash 1/64。整個 `ChunkAllocator` 被一把 `Mutex` 保護（見 §12）。

### 6.2 配置的快慢路徑

```rust
// src/storage/chunk_engine/src/alloc/chunk_allocator.rs:97-131
pub fn allocate(&mut self, clusters: &Clusters, allow_to_allocate: bool) -> Result<Position> {
    if !self.active_groups.is_empty() {
        for level in (0..GroupState::LEVELS).rev() {          // ← 從最滿的桶開始
            let set = &mut self.active_levels[level];
            if let Some(&group_id) = set.iter().next() {
                let state = self.active_groups.get_mut(&group_id).unwrap();
                let index = state.allocate().unwrap();
                if state.is_full() {                          // 升格為 full
                    self.full_groups.insert(group_id);
                    self.active_groups.remove(&group_id);
                    set.remove(&group_id);
                } else if state.level() != level as u32 {     // 跨桶，往上搬
                    set.remove(&group_id);
                    self.active_levels[level + 1].insert(group_id);
                }
                let pos = Position::new(group_id, index);
                self.reference(pos, true);
                self.counter.allocate_chunk();
                return Ok(pos);
            }
        }
    }
    // 慢路徑：跟 GroupAllocator 要一個新 group
    let group_id = self.group_allocator.allocate(clusters, allow_to_allocate)?;
    …
}
```

快路徑是純記憶體：一次 `HashSet::iter().next()`、一次 `tzcnt`、幾個集合的增刪。`benches/bench_allocator.rs` 專門 bench 這條路徑（65536 次 allocate + drop）。

慢路徑（`alloc/group_allocator.rs:29-46`）分三段：

```rust
pub fn allocate(&mut self, clusters: &Clusters, allow_to_allocate: bool) -> Result<GroupId> {
    if let Some(&group_id) = self.allocated_groups.iter().next() {
        self.allocated_groups.remove(&group_id);
        Ok(group_id)                                     // (1) 背景執行緒已備好
    } else if allow_to_allocate {
        let group_id = self.get_unallocated_group_id();
        tracing::info!("allocate group slow path {:?}", group_id);
        let result = clusters.allocate(group_id);        // (2) 同步 fallocate！
        if let Err(err) = result {
            self.unallocated_groups.insert(group_id);    //     失敗要還回去
            return Err(err);
        }
        self.counter.allocate_group();
        Ok(group_id)
    } else {
        Err(Error::NoSpace)                              // (3) 上層已宣告磁碟滿
    }
}
```

(2) 是真正的**同步 syscall in the write path**——所以它會打 `tracing::info!("allocate group slow path")`，出現在日誌裡就代表背景配置沒跟上。`allow_to_allocate = false` 由 C++ 側的 `CheckWorker` 在磁碟接近滿時關掉（`src/storage/worker/CheckWorker.cc:181`：`set_allow_to_allocate(!rejectCreateChunk)`）。

`get_unallocated_group_id()`（`group_allocator.rs:52-61`）優先重用被 punch hole 掉的 id，沒有才推進 `next_group_id`——**group id 不回收就會單調膨脹**，重用避免了在 cluster 檔尾端無限追加。

### 6.3 `position_rc`：舊版本靠什麼保留

這是整個引擎最關鍵的一個資料結構，也是 README 講的「用 `Arc` 管理 chunk position 所有權」的落地：

```rust
// src/storage/chunk_engine/src/alloc/chunk_allocator.rs:133-171
pub fn reference(&mut self, pos: Position, first_ref: bool) {
    // 先斷言這個位置真的是被佔用的
    if let Some(state) = self.active_groups.get_mut(&group_id) {
        assert!(state.check(pos.index()), "ref pos failed: {:?}", pos);
    } else if let Some(state) = self.frozen_groups.get_mut(&group_id) {
        assert!(state.check(pos.index()), "ref pos failed: {:?}", pos);
    } else {
        assert!(self.full_groups.contains(&group_id));
    }
    let rc = /* position_rc[pos] += 1，不存在則插入 1 */;
    if first_ref { assert!(rc == 1, "should be first ref to pos {:?}, rc {}", pos, rc); }
}

pub fn dereference(&mut self, pos: Position) {
    let count = self.position_rc.get_mut(&pos).unwrap();
    *count -= 1;
    if *count == 0 {
        self.position_rc.remove(&pos);
        self.deallocate(pos);            // ← 只有到這裡 bitmap 的 bit 才真的被清掉
    }
}
```

而 `Chunk` 的 `Clone`/`Drop` 就是這兩個函式的 RAII 包裝：

```rust
// src/storage/chunk_engine/src/alloc/chunk.rs:296-306
impl Clone for Chunk {
    fn clone(&self) -> Self { self.allocator.reference(self.meta.clone(), false) }
}
impl Drop for Chunk {
    fn drop(&mut self) { self.allocator.dereference(self.meta.pos); }
}
```

**所以：一個 chunk 的舊版本不是靠 refcount 欄位、不是靠版本鏈、不是靠 GC 標記——是靠「有沒有人手上還握著一個指向該 `Position` 的 `Chunk` 物件」。** 具體來說：

1. 讀者 `Engine::get()` 拿到 `Arc<Chunk>`（`core/engine.rs:201`），rc = 1（快取持有）。
2. C++ 側 `get_raw_chunk` 用 `Arc::into_raw` 把一個強引用洩漏給 C++（`cxx.rs:101`），rc = 2。
3. 此時另一個執行緒對同一 chunk_id 做 COW 寫入 → 配置**新** `Position`，寫完後 `commit_chunk` 把 meta_cache 換成新 `Chunk`，舊 `Arc` 的快取引用被丟掉，rc = 1。
4. **舊資料還在磁碟上、bitmap 的 bit 還是 1、group 還沒被 compact**，正在讀的 C++ 執行緒完全無感。
5. C++ 讀完呼叫 `release_raw_chunk` → `Arc::from_raw` → drop → rc = 0 → `dereference` → `deallocate(pos)` → bitmap 清 bit → 槽位可被重用。

`core/engine.rs:838-921` 的測試把這條時序線逐步驗證：`drop(chunk0)` 之後下一次 `allocate` 才拿到 index 0；`drop(chunk1)` 時因為它還在 meta_cache 裡，`reserved_size` 不變。

`first_ref` 參數是為了抓 bug：從 RocksDB 載入或首次配置時必須 rc == 1，否則代表兩個獨立的來源同時認為自己「首次」持有這個位置——這是災難性的資料損壞前兆，直接 panic 比繼續跑安全。

**注意配置和釋放的順序是刻意不對稱的**（README 第 3 點）：

- 配置：先改記憶體（bitmap set），**後**寫 RocksDB。中途崩潰 → 記憶體狀態消失，磁碟上該 bit 仍是 0，槽位自然回到空閒。沒有洩漏。
- 釋放：先寫 RocksDB（`merge(release)`），**後**改記憶體。中途崩潰 → 磁碟上 bit 已清、記憶體引用還在。重啟後該位置變空閒；但已經沒有人能讀到它（chunk meta 也在同一個 WriteBatch 裡被刪了）。也沒問題。

### 6.4 `AllocateTask` 與背景配置

```rust
// src/storage/chunk_engine/src/alloc/group_allocator.rs:12-17,63-89
pub enum AllocateTask { None, Allocate(GroupId), Deallocate(GroupId) }

pub fn get_allocate_task(&mut self, min_remain: usize, max_remain: usize) -> AllocateTask {
    if self.allocated_groups.len() < min_remain {
        AllocateTask::Allocate(self.get_unallocated_group_id())
    } else if self.allocated_groups.len() > max_remain {
        let group_id = *self.allocated_groups.iter().next().unwrap();
        self.allocated_groups.remove(&group_id);
        AllocateTask::Deallocate(group_id)
    } else { AllocateTask::None }
}
```

`get_allocate_task` / `finish_allocate_task` 是**分兩階段**的：取任務時就把 group id 從池子裡摘出來（避免併發重複配置），執行 syscall 時**不持有 `ChunkAllocator` 的鎖**，完成後再依成敗放回正確的集合（`group_allocator.rs:75-89`）。

```rust
// src/storage/chunk_engine/src/alloc/allocator.rs:63-88
pub fn do_allocate_task(&self, min_remain, max_remain, meta_store: &MetaStore) -> Result<AllocateTask> {
    let task = self.get_allocate_task(min_remain, max_remain);   // 鎖 → 摘任務 → 解鎖
    let result = match task {
        AllocateTask::None => return Ok(task),
        AllocateTask::Allocate(group_id) => (|| {
            self.clusters.allocate(group_id)?;                   // fallocate（無鎖）
            meta_store.allocate_group(group_id)                  // 寫空 bitset（sync=true）
        })(),
        AllocateTask::Deallocate(group_id) => (|| {
            tracing::warn!("deallocate group: {:?}", group_id);
            meta_store.remove_group(group_id)?;                  // 先刪 key
            self.clusters.deallocate(group_id)                   // 後 punch hole
        })(),
    };
    self.finish_allocate_task(task, result.is_ok());             // 鎖 → 歸位 → 解鎖
    result?;  Ok(task)
}
```

Deallocate 的順序（先刪 RocksDB key、後 punch hole）保證：崩潰在中間 → key 沒了但空間還在 → 重啟時該 group 被視為 `unallocated`，之後重新 `fallocate` 同一區間（冪等）。反過來做就會出現「key 說有空間、實際是洞」的假象。

驅動它的有兩處：

- **Rust 內建 worker**（`core/engine.rs:140-158`）：`start_allocate_workers(n)` 起 n 條名為 `Allocate{i}` 的執行緒，參數硬編為 `allocate_groups(1, 2, 2, false)`，忙時 `Continue`、閒時 `Wait(100ms)`。
- **C++ 的 `AllocateWorker`**（`src/storage/worker/AllocateWorker.cc:36-53`）：每 100ms 對每個 engine 呼叫 `allocate_groups(min_remain=4, max_remain=8, batch=128)`、`allocate_ultra_groups(0, 4, 32)`、`compact_groups(max_reserved=1GB)`。生產路徑走的是這一條。

`allocate_ultra_groups` 是把 > 4MiB 的分級單獨拿出來管（`alloc/allocators.rs:83-86` 的 `is_ultra` 過濾），因為它們的 group 是 8GiB/16GiB 等級——預留 4 個 ultra group 就是 64GiB 的實體空間，不能跟小分級用同一套水位。

### 6.5 Compact 與 `get_allocate_tasks` 的容量修正

Compact 的目標是**騰出整個 group 好 punch hole**：

```rust
// src/storage/chunk_engine/src/alloc/chunk_allocator.rs:207-233
pub fn get_compact_task(&mut self, max_reserved: u64) -> Option<GroupId> {
    let reserved = self.counter.reserved_chunks();
    if reserved <= max_reserved { return None; }          // 保留空間不夠多，不值得搬
    for set in &mut self.active_levels {                  // ← 從 level 0（最空）開始
        if let Some(&group_id) = set.iter().next() {
            set.remove(&group_id);
            let state = self.active_groups.remove(&group_id).unwrap();
            self.frozen_groups.insert(group_id, state);   // 凍結：不再接受新配置
            return Some(group_id);
        }
    }
    None
}
```

凍結之後，`Engine::compact_groups` 掃該 group 的所有 chunk 逐一搬走：

```rust
// src/storage/chunk_engine/src/core/engine.rs:117-138
pub fn compact_groups(&self, max_reserved: u64) -> usize {
    let group_ids = self.allocators.get_allocate_tasks(max_reserved);
    for group_id in group_ids {
        let mut it = self.meta_store.iterator();
        let result = it.iterate(
            MetaKey::group_to_chunks_key_prefix(group_id),          // ← 見 §7.2
            |_, chunk_id| self.move_chunk(chunk_id).map(|_| ()),
        );
        …
        self.allocators.finish_compact_task(group_id);
    }
}
```

搬完之後，`frozen_groups` 裡的 `state` 會隨著每個 chunk 的 `dereference` 遞減，歸零時 group 進 `allocated_groups`（`chunk_allocator.rs:186-191`），下一輪 `get_allocate_task` 發現超過 `max_remain` 就把它 punch hole。若沒搬乾淨（有讀者還持著引用），`finish_compact_task` 把它放回 `active_groups`（`chunk_allocator.rs:225-233`）。

`core/engine.rs:1085-1166` 的測試把整條鏈驗證了一遍，包括「有背景讀執行緒持續讀取時 compact 必須不影響正確性」。

#### commit 1831776 修的是什麼

```diff
- pub fn get_allocate_tasks(&self, max_reserved: u64) -> tinyvec::ArrayVec<[GroupId; 3]> {
+ pub fn get_allocate_tasks(&self, max_reserved: u64) -> tinyvec::TinyVec<[GroupId; 3]> {
      self.vec
          .iter()
          .filter_map(|allocator| allocator.get_compact_task(max_reserved))
          .collect()
```

`self.vec` 有 **11** 個 allocator（`CHUNK_SIZE_NUMBER = 11`），`filter_map` 最多可以吐出 11 個 `GroupId`，但 `ArrayVec<[GroupId; 3]>` 的容量是**固定 3**——tinyvec 的 `ArrayVec` 在 `FromIterator` 溢出時是 **panic**，不是靜默截斷。

所以修正前的行為是：

- 平時（保留空間充足）`get_compact_task` 對大多數分級回傳 `None`，實際元素 ≤ 3，一切正常。
- 一旦磁碟碎片化嚴重到**同時有 4 個以上的 chunk 大小分級**的 `reserved_chunks > max_reserved`，`collect()` 就在 `AllocateWorker` 的執行緒裡 panic。而且是**最需要 compact 的時候才會炸**。
- 更嚴重的是這個 panic **不會只終止一次呼叫，而是讓整個 `storage_main` 行程 abort**。`compact_groups` 是經 cxx bridge 給 C++ 呼叫的，簽章回傳 `usize` 而非 `Result`（`src/storage/chunk_engine/src/cxx.rs:476`），而 cxx 為所有 `extern "Rust"` 函式插入 `prevent_unwind` 守衛——panic 觸發守衛解構子的第二次 panic，Rust 定義為 **abort**。呼叫點是 `src/storage/worker/AllocateWorker.cc:51`，那是每 100ms 跑一次的背景迴圈，所以在磁碟碎片化最嚴重時會進入 **crash loop**。
- 不過 group **不會永久凍結**：`frozen_groups` 是純記憶體狀態、未落盤，重啟後 `ChunkAllocator::load`（`src/storage/chunk_engine/src/alloc/chunk_allocator.rs:49-76`）由 RocksDB 的 group bits 重建，group 會回到 `active_groups`。代價是反覆重啟，不是空間永久損失。

改成 `TinyVec` 之後，超過 3 個就自動溢出到堆上（`TinyVec` 是 `Inline(ArrayVec) | Heap(Vec)` 的列舉），行為變成「最多 11 個都收下」。保留 `[GroupId; 3]` 的內聯容量是因為常態確實 ≤ 3，避免每輪 compact 都配一次堆記憶體。

`GroupId` 能當 tinyvec 的 `Array` 元素是因為它 `impl Default`（`types/group_id.rs:6-10`）——tinyvec 要求元素可 `Default` 以填充未使用槽位。

---

## 7. MetaStore：RocksDB 上的五張邏輯表

### 7.1 Key 前綴表

```rust
// src/storage/chunk_engine/src/meta/meta_key.rs:7-16
pub const CHUNK_META_KEY_PREFIX:   u8 = 1;
pub const GROUP_BITS_KEY_PREFIX:   u8 = 2;
pub const POS_TO_CHUNK_KEY_PREFIX: u8 = 3;
pub const USED_SIZE_KEY_PREFIX:    u8 = 4;
pub const USED_SIZE_PREFIX_LEN_KEY: u8 = 5;
pub const TIMESTAMP_KEY_PREFIX:    u8 = 6;
// pub const WRITING_CHUNK_KEY_PREFIX: u8 = 7;      ← 已廢棄的舊格式
pub const VERSION_KEY:             u8 = 8;
pub const WRITING_CHUNK_KEY_PREFIX: u8 = 9;
pub const TEST_KEY_PREFIX:         u8 = b'm';       // = 109，只給單元測試用
```

| 前綴 | 名稱 | Key 結構 | Value | 寫入方式 |
|---|---|---|---|---|
| `1` | chunk meta | `1` + **按位元反相**的 chunk_id（變長） | `derse(ChunkMeta)` | `put` |
| `2` | group bits | `2` + `GroupId` 大端 8B | `[u8; 32]` bitset | `merge` |
| `3` | pos → chunk | `3` + `Position` 大端 8B | chunk_id（原值） | `put` |
| `4` | used size | `4` + chunk_id 的前 `prefix_len` byte | `i64` 小端 | `merge` |
| `5` | used size prefix len | `5`（單鍵） | `u32` 小端 | `put` |
| `6` | timestamp 索引 | `6` + prefix + `u64` 大端 timestamp + chunk_id 剩餘部分 | chunk_id 或空 | `put` |
| `8` | schema version | `8`（單鍵） | 單 byte | `put(sync=true)` |
| `9` | writing chunk log | `9` + chunk_id（**原值，不反相**） | `derse(ChunkMeta)` | `put(sync=true)` |

`7` 被註解掉並在 `9` 重新定義，是一次不相容的格式變更：舊 key 直接被遺棄（不會被讀到，也沒有清理程式碼），新格式從 `9` 開始。

### 7.2 三張核心表的 key 佈局

**(a) chunk meta：唯一做按位元反相的表**

```rust
// src/storage/chunk_engine/src/meta/meta_key.rs:28-42
pub fn chunk_meta_key(chunk_id: &[u8]) -> Self {
    let mut out = Self::chunk_meta_key_prefix();
    for num in chunk_id { out.0.push(!num) }        // ← 每個 byte 取反
    out
}
pub fn parse_chunk_meta_key(key: &[u8]) -> Bytes {
    let mut out = Bytes::new();
    for num in &key[1..] { out.push(!num); }        // 反相是自身的逆運算
    out
}
```

```
chunk_id = chainId(8B, 主機序) ‖ ChunkId::data()（變長）
           └──────────── prefix_len = 8 ────────────┘

RocksDB key:
┌────┬───────────────────────────────────────────────────────────┐
│ 01 │ ~b0 ~b1 ~b2 … ~bn      （逐 byte 反相，長度不變）          │
└────┴───────────────────────────────────────────────────────────┘
```

反相把 RocksDB 的**升序**迭代器變成 chunk_id 的**降序**掃描（`!` 是保序反轉的雙射：`a < b ⟺ ~a > ~b`）。這使得 `query_chunks(begin, end, max)` 可以：

```rust
// src/storage/chunk_engine/src/meta/meta_store.rs:59-97
let end_key = MetaKey::chunk_meta_key(end.as_ref());
it.seek(&end_key)?;
if it.key() == Some(end_key.as_ref()) { it.next(); }   // 半開區間 [begin, end)
for _ in 0..max_count {
    if !it.valid() { break; }
    if it.key().unwrap()[0] != MetaKey::CHUNK_META_KEY_PREFIX { break; }
    let chunk_id = MetaKey::parse_chunk_meta_key(it.key().unwrap());
    if begin.as_ref() <= chunk_id.as_ref() { out.push(…) } else { break; }
    it.next();
}
```

一次 `seek` + 單向 `next()` 就掃出 `(begin, end]` 內、**由大到小**排列的 chunk。RocksDB 的 `DBRawIterator` 反向迭代（`prev()`）在 LSM 上比正向慢得多（要在多層 SST 之間回溯），這個編碼把它徹底避開了。

C++ 側把這個順序寫進了介面契約：`StorageTarget.h:77` 的註解「the chunk ids in result are in reverse lexicographical order」，而 `ChunkEngine::getAllMetadata` 收尾時也用 `a.chunkId > b.chunkId` 排序保持一致（`ChunkEngine.h:226`）。

還有一個副作用：兩個「終止條件」（`key[0] != 1` 與 `chunk_id < begin`）都是 `break`，所以掃描一旦離開 chunk meta 區就立刻停——不需要額外的上界 key。

**(b) pos → chunk：為 compact 而生的反向索引**

```rust
// src/storage/chunk_engine/src/meta/meta_key.rs:76-88
pub fn group_to_chunks_key_prefix(group_id: GroupId) -> Self {
    let mut out = Self::pos_to_chunk_key_prefix();
    out.0.extend_from_slice(&Position::new(group_id, 0).to_be_bytes());
    out.0.pop();                                  // ← 砍掉最後一個 byte
    out
}
pub fn pos_to_chunk_key(pos: Position) -> Self {
    let mut out = Self::pos_to_chunk_key_prefix();
    out.0.extend_from_slice(&pos.to_be_bytes());
    out
}
```

```
pos_to_chunk_key（9 bytes）
┌────┬──────────────────────────────────────────────────┬────────┐
│ 03 │ Position 大端的前 7 byte                          │ index  │
│    │ = chunk_size, cluster, group（大端最高位在前）    │ (8bit) │
└────┴──────────────────────────────────────────────────┴────────┘
      └────────── group_to_chunks_key_prefix（8 bytes）──────────┘
```

因為 `Position` 的 `index` 在**最低 8 bit**，大端序列化後就落在最後一個 byte。`pop()` 掉它，剩下的 8 byte 前綴恰好涵蓋該 group 的全部 256 個槽位，而且它們在 RocksDB 裡是**物理連續**的。於是 `compact_groups` 只要一次 prefix scan 就拿到「這個 group 裡還有哪些 chunk」（`core/engine.rs:126-129`）。

這也是**為什麼 `Position` 落盤用大端而 chunk meta 用反相**：兩張表對排序的需求完全不同，各自選了最省事的編碼。

**(c) group bits：只用高 4 byte 做前綴掃描**

```rust
// src/storage/chunk_engine/src/meta/meta_key.rs:48-52
pub fn group_bits_chunk_size_prefix(group_id: GroupId) -> Self {
    let mut out = Self::group_bits_key_prefix();
    out.0.extend_from_slice(&group_id.to_be_bytes()[..4]);   // 只取高 4 byte
    out
}
```

`GroupId` 的高 32 bit 就是 chunk_size，大端序列化後是前 4 byte。所以 `2 + chunk_size_be(4B)` 這個 5 byte 前綴恰好圈定「某一個分級的所有 group」——`ChunkAllocator::load` 啟動時就是用它做一次 prefix scan 把整個分級載入記憶體（`alloc/chunk_allocator.rs:48-49`）。11 個分級 = 11 次獨立 scan，`Allocators::new` 依序做（`alloc/allocators.rs:14-18`）。

**(d) timestamp 索引：把 prefix 提到 timestamp 前面**

```rust
// src/storage/chunk_engine/src/meta/meta_key.rs:126-145
pub fn timestamp_key(timestamp: u64, chunk_id: &[u8], prefix_len: usize) -> Self {
    let mut out = Self::timestamp_key_filter(&chunk_id[..prefix_len], timestamp);
    out.0.extend_from_slice(&chunk_id[prefix_len..]);
    out
}
```

```
┌────┬─────────────────┬──────────────────────┬──────────────────────┐
│ 06 │ chunk_id 前 8B  │ timestamp 大端 8B    │ chunk_id 第 8 byte起 │
│    │ = chainId       │  （微秒）            │  = 真正的 ChunkId    │
└────┴─────────────────┴──────────────────────┴──────────────────────┘
```

chunk_id **被切成兩半，中間插進 timestamp**。這讓「某條 chain 在某時間窗內被改過的 chunk」變成一次連續 range scan（`meta_store.rs:99-136`），是 3FS 做增量同步/巡檢的基礎。timestamp 用大端，所以時間順序 = 位元組順序。

`parse_timestamp_key` 再把兩半拼回完整 chunk_id（`meta_key.rs:132-145`）。

### 7.3 MergeOperator：兩種完全不同的合併語意

```rust
// src/storage/chunk_engine/src/meta/meta_merge.rs:9-59
fn full_merge<'a>(key: &[u8], value: Option<&[u8]>, operands: impl Iterator<Item=&'a [u8]>) -> Option<Vec<u8>> {
    match key[0] {
        MetaKey::GROUP_BITS_KEY_PREFIX => {
            let mut merge_bits = MergeState::empty();
            for op in operands { merge_bits.merge(&MergeState::from(op).ok()?); }
            let mut bits = if let Some(gb) = value { GroupState::from(gb).ok()? } else { GroupState::empty() };
            bits.update(&merge_bits);
            Some(Vec::from(bits.as_bytes()))
        }
        MetaKey::USED_SIZE_KEY_PREFIX => { /* i64 小端累加 */ }
        MetaKey::TEST_KEY_PREFIX => { /* 直接串接，只給測試 */ }
        _ => None,
    }
}
```

**用 `key[0]` 做 dispatch** 是整個 MergeOperator 的關鍵設計：RocksDB 的 MergeOperator 是 DB 全域唯一的，這裡靠 key 的第一個 byte 在同一個 operator 裡塞了三種語意。前綴常數的數值選擇（1..9）因此不只是命名空間，而是 dispatch tag。

不認得的前綴回傳 `None`——RocksDB 會把它當成 **corruption**。`meta/rocksdb.rs:264-291` 的測試就驗證了這點：對一個沒有 merge 語意的 key 做 `merge("")` 之後，`get()` 和 `iterate()` 都會回傳 `Err`，直到用 `put` 覆蓋掉才恢復。這是刻意的「fail loud」：與其讓一個 bug 靜默產生錯誤的 bitmap，不如整個 key 讀不出來。

`partial_merge` 只實作了 group bits 和 used size 兩種（`meta_merge.rs:61-89`），前者輸出的是**序列化後的 `MergeState`**（差量之間先合併），後者輸出累加後的 i64。TEST 前綴故意不實作 `partial_merge`，所以 `rocksdb.rs:190-201` 的測試能觀察到 16 個 operand 逐個串接的結果。

### 7.4 `add_chunk_mut`：一次寫入要改幾個 key

```rust
// src/storage/chunk_engine/src/meta/meta_store.rs:149-191
pub fn add_chunk_mut(&self, chunk_id, chunk_meta, write_batch) -> Result<()> {
    // 1. chunk meta
    write_batch.put(MetaKey::chunk_meta_key(chunk_id), derse(chunk_meta));
    // 2. pos -> chunk
    write_batch.put(MetaKey::pos_to_chunk_key(chunk_meta.pos), chunk_id);
    // 3. group bits：merge 一個 acquire 差量
    write_batch.merge(MetaKey::group_bits_key(chunk_meta.pos.group_id()),
                      derse(MergeState::acquire(chunk_meta.pos.index())));
    // 4. used size：merge +chunk_size
    self.update_used_size(chunk_id, chunk_meta.pos.chunk_size().0 as i64, write_batch)?;
    // 5. timestamp -> chunk
    write_batch.put(MetaKey::timestamp_key(chunk_meta.timestamp, chunk_id, prefix_len), chunk_id);
    // 6. 刪掉 writing chunk log
    self.remove_writing_chunk_mut(chunk_id, write_batch);
}
```

**六個 key、一個 WriteBatch、一次原子提交**。這就是 README 說的「使用 RocksDB 的 WriteBatch 確保原子更新——整個寫入操作要麼成功要麼失敗，沒有中間狀態」。

`move_chunk_mut`（`meta_store.rs:205-271`）多了一層條件：

```rust
if old_meta.pos != new_meta.pos {
    // 只有位置真的變了才動 pos->chunk 與 group bits
    write_batch.delete(pos_to_chunk_key(old_pos));
    write_batch.merge(group_bits_key(old_pos.group_id()), release(old_pos.index()));
    write_batch.put(pos_to_chunk_key(new_meta.pos), chunk_id);
    write_batch.merge(group_bits_key(new_meta.pos.group_id()), acquire(new_meta.pos.index()));
    self.update_used_size(chunk_id, new_size - old_size, write_batch)?;
}
```

原地追加（見 §8.2）走的正是 `old_meta.pos == new_meta.pos` 這條路——只改 chunk meta 與 timestamp 索引，**兩個 key**。這是熱路徑上最便宜的提交。

一個小不一致值得記錄：`add_chunk_mut` 的 timestamp key value 是 `chunk_id`（第 185 行），而 `move_chunk_mut` 寫的是空 value（第 262 行）。這不影響正確性，因為 `query_chunks_by_timestamp` 完全從 key 反解 chunk_id（`meta_store.rs:125`），value 從頭到尾沒被讀過。

### 7.5 `used_size` 與 `prefix_len` 遷移

`used_size` 表是**按 chunk_id 前綴聚合的用量計數器**，`prefix_len = 8` 時就是「每條 chain 用了多少位元組」。C++ 的 `ChunkEngine::chainUsedSize` 直接查它（`ChunkEngine.h:256-264`），這是 target 上報容量的來源。

`prefix_len` 是開機參數，一旦改了，整張表就得重建：

```rust
// src/storage/chunk_engine/src/meta/meta_store.rs:526-570
fn update_used_size_if_need(&mut self) -> Result<()> {
    let old_len = /* 從 key 5 讀出上次的 prefix_len，沒有則 0 */;
    if old_len == self.config.prefix_len { return Ok(()); }

    let mut map = HashMap::<Bytes, u64>::new();
    if prefix_len == 0 { map.insert(Bytes::new(), 0); }
    let mut it = self.iterator();
    it.iterate(MetaKey::chunk_meta_key_prefix(), |key, value| {
        let mut chunk_id = MetaKey::parse_chunk_meta_key(key);
        chunk_id.resize(prefix_len, 0);                       // 截斷成新前綴
        let chunk_meta = ChunkMeta::deserialize(value)?;
        map.entry(chunk_id).and_modify(|v| *v += chunk_meta.pos.chunk_size().0)
                           .or_insert(chunk_meta.pos.chunk_size().0);
        Ok(())
    })?;
    /* 一個 WriteBatch 寫入新的 prefix_len + 全部聚合結果 */
    self.write(write_batch, true)
}
```

**全表掃描重算**，在 `MetaStore::open` 裡同步執行（`meta_store.rs:31`）。這是開機時唯一的 O(chunk 數) 操作（`ChunkAllocator::load` 是 O(group 數)，小兩個數量級）。`meta_store.rs:800-877` 的測試涵蓋了 `prefix_len` 0 → 1 → 0 的來回遷移。

注意 `map.entry(...).or_insert(...)` 用的是**覆蓋寫**（`put`）而非 merge——這讓遷移是冪等的，中途崩潰重來一次結果相同。

### 7.6 Schema 版本升級

```rust
// src/storage/chunk_engine/src/meta/meta_store.rs:572-573
pub const V1_FIX_TIMESTAMP: u8 = 1;
pub const LATEST_VERSION:   u8 = Self::V1_FIX_TIMESTAMP;

// src/storage/chunk_engine/src/core/engine.rs:723-746
pub fn upgrade_version(&self) -> Result<()> {
    let version = self.meta_store.get_version()?;
    if version < MetaStore::V1_FIX_TIMESTAMP {
        let mut write_batch = RocksDB::new_write_batch();
        self.meta_store.remove_range_mut(MetaKey::TIMESTAMP_KEY_PREFIX, &mut write_batch)?;
        self.meta_store.write(write_batch, true)?;
        new_version = MetaStore::V1_FIX_TIMESTAMP;
    }
    …
}
```

升級手段是**整段刪掉再讓它自然重建**——timestamp 索引是純衍生資料，丟掉不損失任何真相。`remove_range_mut` 有一個白名單守衛（`meta_store.rs:590-609`），明確拒絕對五個「真相來源」前綴（1、2、3、4、5）做 range delete：

```rust
if prefix == CHUNK_META_KEY_PREFIX || prefix == GROUP_BITS_KEY_PREFIX
   || prefix == POS_TO_CHUNK_KEY_PREFIX || prefix == USED_SIZE_KEY_PREFIX
   || prefix == USED_SIZE_PREFIX_LEN_KEY {
    return Err(Error::InvalidArg(format!("invalid remove range: {}", prefix)));
}
write_batch.delete_range(&[prefix], &[prefix + 1]);
```

`&[prefix]` 到 `&[prefix+1]` 這個範圍能涵蓋整個前綴，因為所有 key 都以單 byte 前綴開頭且是半開區間。

### 7.7 RocksDB 的配置

```rust
// src/storage/chunk_engine/src/meta/rocksdb.rs:11-14,36-38,47-53,92-94
pub struct RocksDB {
    db: rocksdb::DB,
    write_options: [rocksdb::WriteOptions; 2],   // 0 = non-sync, 1 = sync
}
table_options.set_bloom_filter(10.0, true);      // 10 bits/key，block-based
read_options.set_readahead_size(Size::mebibyte(4).into());
```

`write_options` 做成長度 2 的陣列，呼叫時用 `&self.write_options[sync as usize]` 索引（`rocksdb.rs:67,74,85`）——避免每次寫入都構造一個 `WriteOptions`。

Bloom filter 對 `get_chunk_meta` 的點查很關鍵：meta_cache miss 時要下探 LSM，bloom 讓不存在的 chunk_id 在絕大多數情況下不用讀 SST。

4MiB 的 readahead 是給 `new_iterator()` 用的——所有的 prefix scan（開機載入 group bits、compact 掃 group、query_chunks）都吃這個設定。

沒有設定 column family、沒有調 write buffer、沒有壓縮設定——用的都是預設值。

---

## 8. 寫入路徑

### 8.1 兩階段：`update_chunk` → `commit_chunk`

C++ 的 `UpdateJob` 分成 update 和 commit 兩個 RPC 階段（CRAQ 的 propagate/commit），Rust 側對應：

```
C++  ChunkEngine::update()                    C++  ChunkEngine::commit()
       │                                             │
       ↓ engine.update_raw_chunk(key, req, err)      ↓ chunk->set_chain_ver(v)
  ┌────────────────────────────────────┐             ↓ engine.commit_raw_chunk(chunk, sync, err)
  │ Engine::update_chunk               │        ┌──────────────────────────────┐
  │  1. 驗 checksum（讀 req.data）      │        │ Engine::commit_chunk         │
  │  2. get 舊 chunk（meta_cache）      │        │  1. set_committed()          │
  │  3. 驗 chain_ver / update_ver / etag│       │  2. 鎖 meta_cache entry       │
  │  4. COW 或 原地寫（真正落磁碟）     │        │  3. add/move/remove（WriteBatch）│
  │  5. 進 writing_list                 │        │  4. 換 meta_cache            │
  │  6. persist_writing_chunk（sync）   │        │  5. commit_succ()            │
  │  → 回傳 *mut WritingChunk           │        └──────────────────────────────┘
  └────────────────────────────────────┘
```

**資料在階段 1 就已經落到磁碟了**（`Cluster` 的 fd 帶 `O_SYNC`/`O_DIRECT`），階段 2 只寫 RocksDB。這是「先寫資料、再宣告」的順序，中間崩潰只會留下一塊沒人指向的髒空間（見 §10）。

### 8.2 COW vs 原地寫：決策樹

```rust
// src/storage/chunk_engine/src/core/engine.rs:386-434
let mut new_chunk = match old_chunk {
    // (A) 刪除：clone 舊 chunk，位置不變
    Some(old_chunk) if req.is_remove => old_chunk.as_ref().clone(),

    // (B) 必須 COW
    Some(old_chunk)
        if req.is_syncing                                   // 同步修復：長度可能縮短
            || (req.length > 0 && req.offset < old_chunk.meta().len)   // 覆寫既有資料
            || req.offset + req.length > old_chunk.capacity() =>       // 超出槽位容量
    {
        old_chunk.copy_on_write(data, req.offset, req.checksum, req.is_syncing,
                                allow_to_allocate, &self.allocators, &self.metrics)?
    }

    // (C) 原地追加：clone（refcount +1，同一個 Position）後直接寫
    Some(old_chunk) => {
        let mut new_chunk = old_chunk.as_ref().clone();
        new_chunk.safe_write(data, req.offset, req.checksum, req.is_truncate, &self.metrics)?;
        new_chunk
    }

    // (D) 全新 chunk
    None => {
        let mut new_chunk = self.allocators.allocate(Size::from(req.offset + req.length),
                                                     allow_to_allocate)?;
        new_chunk.safe_write(…)?;
        new_chunk
    }
};
```

**這是最違反直覺的一點：`chunk_engine` 並不是無條件 COW。** 純追加（`offset >= old.len` 且不超容量）**直接寫在原位置**，因為那段位元組從來沒被任何已提交版本覆蓋過——讀者只會讀到 `meta.len` 為止，看不到追加中的尾巴。省下的是一次完整的 chunk 讀 + 寫（對 512KiB chunk 就是 1MiB 的 IO）。

`(C)` 分支的 `clone()` 會讓同一個 `Position` 的 rc 變成 2（快取一份、writing 一份），確保寫入期間該槽位絕不會被 compact 搬走或被 dereference 回收。

`3FS` 的實際負載大量是順序追加（`3FS` 的 chunk 是檔案的固定大小切片，追加寫是常態），所以 `(C)` 是主路徑，`Metrics::safe_write_direct_append` 就是它的計數器。

### 8.3 `copy_on_write` 的三種 checksum 策略

```rust
// src/storage/chunk_engine/src/alloc/chunk.rs:89-174
pub fn copy_on_write(&self, data, offset, checksum, is_syncing, allow_to_allocate, allocators, metrics)
    -> Result<Chunk>
{
    let new_len = std::cmp::max(self.meta.len, offset + data.len() as u32);
    let mut new_chunk = allocators.allocate(Size::from(new_len), allow_to_allocate)?;   // 可能換分級！

    let skip_read = is_syncing || (offset == 0 && data.len() >= self.meta.len as usize);
    let checksum = Self::BUFFER.with(|v| {
        let mut vec = v.borrow_mut();
        if !skip_read {
            let len = self.meta.len.next_multiple_of(ALIGN_SIZE.into());
            self.pread(&mut vec[..len as usize], 0)?;          // 讀舊資料進 TLS buffer
        }
        if skip_read && is_aligned_io(data, offset) {
            new_chunk.pwrite(data, offset)?;                   // 完全覆蓋 + 已對齊 → 直接寫
        } else {
            if self.meta.len < offset { vec[self.meta.len as usize..offset as usize].fill(0); }
            vec[offset as usize..][..data.len()].copy_from_slice(data);
            new_chunk.pwrite(&vec[..new_len.next_multiple_of(ALIGN_SIZE.into()) as usize], 0)?;
        }
        Result::Ok(if skip_read { checksum } else { crc32c::crc32c(&vec[..new_len as usize]) })
    })?;
    new_chunk.meta.len = if is_syncing { offset + data.len() as u32 } else { new_len };
    new_chunk.meta.checksum = checksum;
    Ok(new_chunk)
}
```

三條 checksum 路徑，各有一個計數器：

| 情境 | checksum 來源 | Metric |
|---|---|---|
| `skip_read`（整塊覆蓋 / syncing） | **直接沿用** client 送來的 `req.checksum` | `checksum_reuse` |
| 需要讀舊資料 | 對合併後的完整 buffer **重算** | `checksum_recalculate` |
| `safe_write` 追加 | `crc32c_combine` / `crc32c_append` **增量合併** | `checksum_combine` |

`crc32c_combine(old, new, new_len)` 是 CRC 的數學性質：兩段資料的 CRC 可以在不重讀舊資料的前提下合成整體 CRC。這讓 512KiB chunk 的第 100 次追加不需要重讀前 99 次的資料——這是追加路徑能做到「零讀放大」的關鍵。

`is_syncing` 是 CRAQ 的修復路徑：長度可能**縮短**（`new_chunk.meta.len = offset + data.len()`，第 166-170 行，而不是 `max`），所以必須 COW（不能原地寫，因為舊資料要保留給還在讀的人）且 checksum 直接採信來源副本的值。

還有一個容易漏掉的點：`copy_on_write` 用 `allocators.allocate(Size::from(new_len))`，也就是**COW 可能換一個 chunk 大小分級**。一個 64KiB 的 chunk 寫到 70KiB，新位置會落在 128KiB 分級的 cluster 檔裡。`move_chunk_mut` 的 `update_used_size` 因此要算差值（`meta_store.rs:251-255`）。

### 8.4 `safe_write`：三條對齊路徑

`safe_write`（`alloc/chunk.rs:176-281`）是原地寫的實作，也是全 crate 分支最密的函式：

```
                    ┌─ truncate && offset < len ──→ 讀 [0, offset) 重算 checksum，只改 meta
                    │                                （縮短不需要寫磁碟）
                    │
safe_write ─────────┼─ len/offset/data 全對齊 ──→ ① offset > len：先寫 ZERO 補洞
                    │   （direct IO 可用）           ② 再直接 pwrite(data, offset)
                    │                               checksum 用 combine/append
                    │
                    └─ 未對齊 && 會變長 ────────→ 讀回尾端那個 4KiB block、
                                                    在 TLS buffer 裡拼好、
                                                    整段從對齊邊界寫回
```

未對齊路徑的關鍵在 `alloc/chunk.rs:240-248`：

```rust
let start = self.meta.len & !(ALIGN_SIZE.0 as u32 - 1);      // 向下取到 4KiB 邊界
if start != self.meta.len {
    self.pread(&mut vec[start as usize..][..ALIGN_SIZE.into()], start)?;   // 讀回半滿的尾塊
}
```

**只讀回最後那一個 4KiB block**，不是整個 chunk。因為 O_DIRECT 要求寫入的起點/長度/緩衝區都對齊 4KiB，而追加的起點通常在某個 block 中間——把那個 block 讀回來、在記憶體裡補上新資料、整塊寫回去。讀放大固定是 4KiB，與 chunk 大小無關。`safe_write_read_tail_times` / `safe_write_read_tail_bytes` 就是量這個的。

`ZERO` 是一塊 lazy_static 的 64MiB 全零對齊緩衝區（`alloc/chunk.rs:16-22`），專門給 truncate-extend 補洞用——避免每次都 memset。

`Chunk::BUFFER` 是 **thread_local 的 64MiB 對齊緩衝區**（`alloc/chunk.rs:25-27`）。每個碰過寫入路徑的執行緒都會分配一塊 64MiB（等於 `CHUNK_SIZE_ULTRA`）。這是為了讓最大分級的 COW 也能一次性在記憶體裡完成，代價是常駐記憶體 = 執行緒數 × 64MiB。

### 8.5 版本檢查

```rust
// src/storage/chunk_engine/src/core/engine.rs:351-383
if req.chain_ver < req.out_chain_ver {
    return Err(Error::ChainVersionMismatch(…));          // 舊 chain 版本的寫入一律拒絕
}
let new_chunk_ver = if req.is_syncing {
    req.update_ver                                        // 修復：無條件採信
} else if req.update_ver > 0 {
    if req.update_ver <= req.out_commit_ver {
        return Err(Error::ChunkCommittedUpdate(…));       // 重放：已經寫過了
    } else if req.update_ver > req.out_commit_ver + 1 {
        return Err(Error::ChunkMissingUpdate(…));         // 跳號：中間漏了
    }
    req.update_ver
} else {
    req.out_commit_ver + 1                                // 本地自增
};
if !req.expected_tag.is_empty() && req.expected_tag != etag {
    return Err(Error::ChunkETagMismatch(…));              // CAS 語意
}
```

`ChunkCommittedUpdate` 與 `ChunkMissingUpdate` 是兩個對 CRAQ 鏈複製至關重要的錯誤：前者代表重複投遞（上游重試），後者代表鏈上漏了一個版本（必須觸發 resync）。C++ 側把它們映射成 4008 / 4007（`cxx.rs:163-164`）。

ETag 機制（`expected_tag` / `desired_tag`）提供 compare-and-swap 語意；沒指定 `desired_tag` 時會塞一個隨機 u64 的十六進位字串（`alloc/chunk.rs:47-50`），確保每次寫入 ETag 都變。`ChunkMeta::set_default_etag_if_need`（`types/chunk_meta.rs:29-33`）則對舊資料（沒有 ETag）用 checksum 的十六進位當預設值——所以老 chunk 的 ETag 是它內容的函式，新 chunk 的 ETag 是隨機的。

---

## 9. 讀取路徑與 `meta_cache`

```rust
// src/storage/chunk_engine/src/core/engine.rs:184-209
pub fn get(&self, chunk_id: &[u8]) -> Result<Option<ChunkArc>> {
    let mut entry = self.meta_cache.entry_by_ref(chunk_id);      // ← 取得該 key 的鎖
    self.get_with_entry(chunk_id, &mut entry)
}

fn get_with_entry(&self, chunk_id, entry: &mut EntryByRef<Bytes, [u8], ChunkArc>) -> … {
    match entry.get() {
        Some(chunk) => Ok(Some(chunk.clone())),                  // Arc clone，不動 position_rc
        None => {
            let meta = self.meta_store.get_chunk_meta(chunk_id)?;
            if let Some(mut meta) = meta {
                meta.set_default_etag_if_need();
                let allocator = self.allocators.select_by_pos(meta.pos)?;
                let chunk = Arc::new(allocator.reference(meta, true));   // position_rc = 1
                entry.insert(chunk.clone());
                Ok(Some(chunk))
            } else { Ok(None) }
        }
    }
}
```

`meta_cache: Arc<LockMap<Bytes, ChunkArc>>`（`core/engine.rs:22`，容量 `1 << 20`、256 shard，第 45 行）是 `lockmap` crate 提供的**每 key 一把鎖**的 map。`entry_by_ref` 回傳的 `EntryByRef` 是一個持鎖的 guard，離開作用域才釋放。

這解釋了 `commit_chunk` 裡的關鍵註解：

```rust
// src/storage/chunk_engine/src/core/engine.rs:501-514
// update rocksdb under lock protection.
let mut entry = self.meta_cache.entry_by_ref(chunk_id);
match self.get_with_entry(chunk_id, &mut entry)? {
    Some(old_chunk) => self.meta_store.move_chunk(chunk_id, old_chunk.meta(), new_chunk.meta(), sync)?,
    None            => self.meta_store.add_chunk(chunk_id, new_chunk.meta(), sync)?,
}
entry.insert(new_chunk.clone());
drop(entry);
```

**RocksDB 的寫入是在持有該 chunk_id 的 entry 鎖時做的**，所以「讀舊 meta → 寫新 meta → 換快取」對同一個 chunk_id 是原子的。不同 chunk_id 完全並行。

`meta_cache` 沒有淘汰策略——它是「有引用就在、沒引用就走」：`Arc<Chunk>` 被塞進 map，只有 `entry.remove()`（刪除、批量刪除）或被新版本 `insert` 覆蓋才會離開。這意味著**快取大小 = 活躍 chunk 數**，不設上限。對一個單碟 engine 來說這是可接受的（README 估算單機 12 億 chunk，但實際活躍集遠小於此）。

`move_chunk`（compact 用）有一個 ABA 防護（`core/engine.rs:237-247`）：

```rust
let mut entry = self.meta_cache.entry_by_ref(chunk_id);
match entry.get() {
    Some(chunk) if Arc::ptr_eq(chunk, &old_chunk) => { /* 真的做搬遷 */ }
    _ => Ok(None),   // chunk is updated or deleted by other thread.
}
```

用 `Arc::ptr_eq` 而不是比較 meta——搬遷期間（`copy_chunk` 是無鎖的長操作）如果有人寫了新版本，指標就變了，這次搬遷直接放棄。新版本自己會在新的 group 裡，舊版本的引用會隨著讀者放手而消失。

`batch_get`（`core/engine.rs:211-224`）為了避免逐 key 加鎖解鎖的開銷，**先把所有 entry guard 收集在一個 `Vec` 裡**，全部處理完才一次性 drop。輸入是 `BTreeSet<Bytes>`，所以加鎖順序是全域一致的字典序——這是防死鎖的必要條件。

---

## 10. Crash Recovery

### 10.1 問題：資料先落盤、meta 後提交，中間崩了怎麼辦

階段 1（`update_chunk`）結束時，磁碟上已經有一塊新寫的資料，但 RocksDB 完全不知道它的存在。如果此刻斷電：

- **新 chunk 的情況**：那個 `Position` 在 group bitmap 裡是 0，重啟後會被分配給別人 → 資料被覆蓋（沒關係，反正沒人指向它）。
- **COW 的情況**：舊 chunk meta 還指向舊位置，新位置未被記錄 → 同上。
- **原地追加的情況**：舊 chunk meta 的 `len` 還是舊值，追加的尾巴讀不到 → 資料自然被忽略。

看起來都安全。但 3FS 是鏈複製系統，**上游可能已經認為這筆寫入成功了**（在 CRAQ 中，update 階段成功就代表 dirty 版本已寫入），重啟後必須能把它找回來並決定 commit 或 abort。這就是 writing chunk log 的用途。

### 10.2 writing chunk log

```rust
// src/storage/chunk_engine/src/meta/meta_store.rs:348-365
pub fn persist_writing_chunk(&self, chunk_id: &[u8], chunk_meta: &ChunkMeta) -> Result<()> {
    self.rocksdb.put(MetaKey::writing_chunk_key(chunk_id), derse(chunk_meta), /*sync=*/true)
}
```

`update_chunk` 的最後一步（`core/engine.rs:468-469`）用 **sync 寫**把「我正要把 chunk X 寫成這個樣子（含新的 `Position`）」記到 key 前綴 `9` 下。之後的 `add_chunk_mut`/`move_chunk_mut`/`remove_mut` 第 6 步一律在同一個 WriteBatch 裡刪掉它（`meta_store.rs:188,268,316`）——提交成功則 log 消失，提交失敗或崩潰則 log 留著。

### 10.3 開機三步：占位 → 建立引用 → 騰位

`Engine::open` 的順序極其講究（`core/engine.rs:41-70`）：

```rust
let mut meta_store = MetaStore::open(&meta_config)?;
let uncommitted_chunks = meta_store.occupy_uncommitted_positions()?;   // ① 先占位
let meta_store = Arc::new(meta_store);
let allocators = Allocators::new(&config.path, config.create, meta_store.clone())?;  // ② 再載入 bitmap
…
if !uncommitted_chunks.is_empty() {
    for (chunk_id, meta, _) in &uncommitted_chunks {
        let old_chunk = engine.get(chunk_id)?;
        let prefix: Bytes = chunk_id[..engine.prefix_len].into();
        let mut writing_list = engine.writing_list.entry(prefix).or_default();
        let allocator = engine.allocators.select_by_pos(meta.pos)?;
        let chunk = allocator.reference(meta.clone(), old_chunk.is_none());   // ③ 建立記憶體引用
        writing_list.insert(chunk_id.clone(), WritingHolder { chunk, abort: true });
    }
    meta_store.vacate_uncommitted_positions(uncommitted_chunks)?;             // ④ 撤掉磁碟占位
}
engine.upgrade_version()?;
```

**① `occupy_uncommitted_positions`**（`meta_store.rs:367-405`）掃全部 writing chunk log，對每一條：

```rust
match self.get_chunk_meta(&chunk_id)? {
    Some(meta) if meta.pos == writing_meta.pos => {
        uncommitted_chunks.push((chunk_id, writing_meta, false));    // 原地追加，bit 早就是 1
    }
    _ => {
        uncommitted_chunks.push((chunk_id.clone(), writing_meta, true));
        write_batch.put(MetaKey::pos_to_chunk_key(pos), chunk_id);   // 把 bit 打上去
        write_batch.merge(MetaKey::group_bits_key(pos.group_id()), acquire(pos.index()));
    }
}
```

它把未提交寫入所佔的 `Position` **暫時**標記成已使用，用 sync WriteBatch 落盤。

**② `Allocators::new`** 這時才去掃 group bits——於是它看到的 bitmap 已經包含了那些未提交的位置，**絕不會把它們分配給別人**。這就是為什麼 ① 必須在 ② 之前。

**③ `allocator.reference(meta, old_chunk.is_none())`** 在記憶體裡建立 `position_rc` 條目。`first_ref` 參數的值是 `old_chunk.is_none()`：

- 全新 chunk（沒有已提交版本）→ `first_ref = true`，斷言 rc 必須是 1。
- 原地追加（`get()` 已經為同一個 `Position` 建了 rc=1）→ `first_ref = false`，容許 rc 變成 2。

這一行是整個恢復流程裡最容易寫錯的地方，也是為什麼 `reference` 要有這個看似多餘的參數。

**④ `vacate_uncommitted_positions`**（`meta_store.rs:407-434`）把 ① 打上的磁碟標記撤掉（只撤 `occupied == true` 的那些），因為此時記憶體裡的 `position_rc` 已經接管了保護責任。撤掉的理由是：如果現在再次崩潰，這些位置本來就該回到「未使用」——真相由 writing log 重新推導，而不是由一個半永久的 bitmap 位元。

恢復出來的 `WritingHolder { chunk, abort: true }` 的 `abort = true` 標記表示「這是上輩子留下的、還沒人認領的寫入」。

### 10.4 上層決定 commit 還是 abort

恢復後的寫入躺在 `writing_list` 裡，由 C++ 側在 chain 恢復服務時處理：

```rust
// src/storage/chunk_engine/src/core/engine.rs:592-618
pub fn handle_uncommitted_chunks(&self, prefix: &[u8], chain_ver: u32) -> Result<Vec<(Bytes, ChunkMeta)>> {
    let mut writing_list = self.writing_list.entry(prefix.into()).or_default();
    for (chunk_id, holder) in writing_list.iter_mut() {
        holder.chunk.set_chain_ver(chain_ver);      // 蓋上新的 chain 版本
        chunks.push(WritingChunk { chunk_id: chunk_id.clone(), chunk: holder.chunk.clone(),
                                   list: self.writing_list.clone(), prefix_len: …,
                                   is_remove: false, commit_succ: false });
    }
    drop(writing_list);
    self.commit_chunks(chunks, true)?;              // 全部提交
    Ok(uncommitted_chunks)
}
```

**策略是「全部 commit」而不是「全部 abort」**，並蓋上當前的 `chain_ver`。C++ 的 `ChunkEngine::resetUncommittedChunks`（`ChunkEngine.h:158-181`）把每一條都用 `XLOGF(CRITICAL, …)` 記下來——這是可稽核的資料變更。

配套的 `queryUncommittedChunks`（`ChunkEngine.h:138-156`）讓上層在決定之前先看看有哪些。

如果 `abort` 標記還在（沒被新的 `update_chunk` 覆蓋），下一次對同一 chunk_id 的 `update_chunk` 會直接把它替換掉：

```rust
// src/storage/chunk_engine/src/core/engine.rs:443-465
match writing_list.entry(chunk_id.into()) {
    Entry::Occupied(mut e) if e.get().abort => { e.insert(WritingHolder { chunk: new_chunk.clone(), abort: false }); }
    Entry::Occupied(e) => return Err(Error::InvalidArg(format!("chunk {:?} is in writing ({:?})", …))),
    Entry::Vacant(e) => { e.insert(WritingHolder { chunk: new_chunk.clone(), abort: false }); }
}
```

`abort == false` 的 Occupied 表示**同一個 chunk 正在被另一個執行緒寫**——直接報錯，`chunk_engine` 不支援對同一 chunk 的併發寫入（CRAQ 保證了單一寫入者）。

### 10.5 `WritingChunk::Drop`：以 panic 保護不變式

```rust
// src/storage/chunk_engine/src/alloc/writing_chunk.rs:35-50
impl Drop for WritingChunk {
    fn drop(&mut self) {
        let prefix = &self.chunk_id[..self.prefix_len as usize];
        if let Some(mut map) = self.list.get_mut(prefix) {
            if self.commit_succ {
                if map.remove(&self.chunk_id).is_some() { return; }   // 提交成功 → 移出清單
            } else if let Some(holder) = map.get_mut(&self.chunk_id) {
                holder.abort = true;                                   // 失敗 → 標記待處理
                return;
            }
        }
        panic!("chunk id {:?} is not in the writing list!", self.chunk_id);
    }
}
```

`WritingChunk` 的 drop 是**唯一**把寫入從 `writing_list` 移除的地方。找不到自己的條目就 panic——這代表某處的簿記已經壞了，而 3FS 選擇立即崩潰而不是繼續在不一致的狀態上運行。`writing_chunk.rs:107-123` 有三個 `#[should_panic]` 測試專門覆蓋這三種找不到的情形。

C++ 側的 `ChunkEngineUpdateJob` 是這個 RAII 的鏡像（`src/storage/update/UpdateJob.h:14-41`）：解構時若還持有指標就呼叫 `release_writing_chunk`，`commit` 成功後則 `release()` 把指標交出去避免二次釋放。

---

## 11. Checksum：CRC32C 與 `~checksum` 之謎

Rust 側全程用 `crc32c` crate：

```rust
crc32c::crc32c(data)                              // engine.rs:311、chunk.rs:157,195
crc32c::crc32c_append(self.meta.checksum, buf)    // chunk.rs:213,266
crc32c::crc32c_combine(a, b, len_b)               // chunk.rs:229
```

C++ 側則在**每一次跨界時都取補數**：

```cpp
// src/storage/store/ChunkEngine.h:22       Rust → C++
out.checksumValue = ~in.checksum;
// src/storage/store/ChunkEngine.h:65       Rust → C++
state.chunkChecksum = ChecksumInfo{ChecksumType::CRC32C, ~meta.checksum};
// src/storage/store/ChunkEngine.cc:42      C++ → Rust
req.checksum = ~updateIO.checksum.value;
// src/storage/store/ChunkEngine.cc:66      Rust → C++
result.checksum = ChecksumInfo{ChecksumType::CRC32C, ~req.out_checksum};
```

原因在 folly 的 CRC 實作慣例。`folly::crc32c(data, n, startingChecksum = ~0U)` 最終落到：

```cpp
// third_party/folly/folly/hash/Checksum.cpp:114-117
boost::crc_optimal<32, CRC_POLYNOMIAL, ~0U, 0, true, true> sum(startingChecksum);
sum.process_bytes(data, nbytes);
return sum.checksum();
```

模板參數第四位是 **final XOR = `0`**。也就是說 **folly 回傳的是 CRC 暫存器的原始值，沒有做標準要求的最終取反**；而 `startingChecksum` 也直接就是暫存器初值（預設 `~0U` = 標準初值）。這個設計讓 `folly::crc32c(b, n, folly::crc32c(a, m))` 可以直接串接。

Rust 的 `crc32c` crate 回傳的是**標準 CRC-32C**（含最終 XOR `0xFFFFFFFF`）。

因此：`folly_value == ~rust_value`。C++ 端的 `~` 是**兩套 API 慣例之間的橋**，不是什麼位元魔術。3FS 的 `ChecksumInfo::combine` 也自己補了一次反相（`src/fbs/storage/Common.h:191`：`folly::crc32c_combine(~value, o.value, length)`），道理相同。

**這件事的實務意義**：任何直接讀 RocksDB 裡 `ChunkMeta.checksum` 的工具（例如 `examples/chunk_viewer.rs`）看到的是標準 CRC-32C；任何從 3FS RPC/日誌裡看到的 checksum 是它的補數。兩者比對時必須先反相。

`Metrics` 的三個 checksum 計數器（`checksum_reuse` / `checksum_combine` / `checksum_recalculate`）被 C++ 的 `CheckWorker` 每秒抓一次上報（`src/storage/worker/CheckWorker.cc:247`），`recalculate` 佔比高就代表寫入模式偏離了追加，值得調查。

---

## 12. 併發模型

### 12.1 沒有 async

整個 crate **沒有一個 `async fn`、沒有 tokio、沒有 futures**。所有 IO 都是阻塞的 `pread`/`pwrite`/`fallocate`，所有同步都是 `std::sync`。併發完全由呼叫端提供：C++ 側用 folly 的 `CPUThreadPoolExecutor` 把 update/commit 派到執行緒池上（`StorageTarget::updateChunk(job, executor)`），Rust 只需要保證執行緒安全。

這是刻意的：cxx 橋接無法傳遞 Rust 的 `Future`，而引擎的操作都是短的（一次 syscall 級別），沒有值得掛起的等待。

### 12.2 鎖的清單與粒度

| 鎖 | 位置 | 粒度 | 保護什麼 |
|---|---|---|---|
| `Mutex<ChunkAllocator>` | `alloc/allocator.rs:5` | **每個 (磁碟, chunk 大小分級) 一把**，即每 engine 11 把 | 五個 group 集合 + `position_rc` |
| `LockMap<Bytes, ChunkArc>` | `core/engine.rs:22` | **每個 chunk_id 一把**（256 shard） | meta_cache 條目 + 該 chunk 的 RocksDB 提交 |
| `DashMap<Bytes, HashMap<…>>` | `core/engine.rs:27` | **每個 prefix（= chain）一把** | writing_list |
| `Mutex<Vec<Worker>>` | `core/engine.rs:23` | 全域，只在啟停時用 | 背景執行緒 handle |
| RocksDB 內部 | — | — | LSM |

`Mutex<ChunkAllocator>` 是唯一的粗粒度鎖，但臨界區都是純記憶體的集合操作（幾十奈秒）。真正慢的操作都在鎖外：`do_allocate_task` 的 `fallocate` 在 `get_allocate_task` 和 `finish_allocate_task` 之間（`alloc/allocator.rs:69-84`），`copy_chunk` 的讀寫在 `move_chunk` 的兩次取鎖之間。

### 12.3 死鎖避免：全域排序取鎖

兩處需要同時持有多把 `LockMap` 鎖，都用排序解決：

```rust
// src/storage/chunk_engine/src/core/engine.rs:520-535（commit_chunks）
let chunk_ids = chunks.iter().map(|c| c.chunk_id.clone()).collect::<BTreeSet<_>>();
if chunk_ids.len() != chunks.len() {
    return Err(Error::InvalidArg("same chunk id in the batch!".into()));
}
for chunk_id in &chunk_ids { /* BTreeSet 保證字典序 */ entries.insert(chunk_id.clone(), entry); }

// src/storage/chunk_engine/src/core/engine.rs:693-700（batch_remove）
for batch in chunk_ids.chunks_mut(BATCH_SIZE) {
    batch.sort(); // acquire locks in sequence to avoid deadlocks.
    …
}
```

`commit_chunks` 額外拒絕同一批次裡出現重複 chunk_id——否則會對同一把鎖重入而 self-deadlock。

`batch_remove` 以 4096 為一批，每批一個 WriteBatch。分批的理由是限制單個 WriteBatch 的大小與持鎖時間。

### 12.4 原子計數器的 ordering

`AllocatorCounter`（`alloc/allocator_counter.rs`）全部用 `AtomicU64`。ordering 的選擇有點隨性：`allocate_group`/`allocate_chunk` 用 `SeqCst`（第 69-87 行），`position_count`/`position_rc` 用 `AcqRel`（`chunk_allocator.rs:150,155,163,167`），讀取用 `Acquire`。由於這些計數器全部在 `Mutex<ChunkAllocator>` 內被修改，實際上任何 ordering 都足夠——`SeqCst` 是保守選擇，代價是每次寫入一個 `mfence`。

`Metrics` 的 18 個計數器都用 `AcqRel` 的 `fetch_add`，讀取時用 `swap(0, AcqRel)`（`cxx.rs:269-307`）——**讀即清零**，所以 `get_metrics` 回傳的是「距上次呼叫以來的增量」，且只能有一個消費者。C++ 的 `CheckWorker` 每秒調一次。

延遲類指標在 `get_metrics` 裡當場除以次數變成平均值（`cxx.rs:280,283-284,304,306`），`std::cmp::max(1, times)` 防除零。

### 12.5 `Worker`：條件變數驅動的背景執行緒

```rust
// src/storage/chunk_engine/src/utils/worker.rs:63-92
builder.spawn(move || {
    let mutex = Mutex::new(());
    while !stopping_clone.load(Ordering::Acquire) {
        match f() {
            WorkerState::Continue => continue,
            WorkerState::Pause    => drop(condvar_clone.wait(mutex.lock().unwrap()).unwrap()),
            WorkerState::Wait(d)  => drop(condvar_clone.wait_timeout(mutex.lock().unwrap(), d).unwrap()),
            WorkerState::Stop     => break,
        }
    }
})
```

`Mutex::new(())` 是**每個 worker 執行緒自己 local 的**——它不保護任何東西，純粹是 `Condvar::wait` 的 API 要求（Rust 的 `Condvar` 必須配 `MutexGuard`）。因為 mutex 是 thread-local 的，`wait` 之後有可能錯過 `notify_all`；`stop_and_join` 先 `store(true)` 再 `notify_all`（第 94-100 行），配合 `Wait(duration)` 的逾時，最壞情況只是多等一個週期。這是「夠用就好」的取捨。

---

## 13. `unsafe` 逐塊審查（全 26 處）

全 crate（含測試與 bin）共 **26 處** `unsafe`，分布如下：

```
12  src/cxx.rs              ← FFI 邊界（7 個實作 + 5 個 bridge 宣告）
 4  src/core/engine.rs      ← 1 個裸指標轉切片、1 個刻意洩漏、2 個測試
 3  src/file/cluster.rs     ← 1 個 syscall、2 個測試
 2  src/utils/aligned.rs    ← 手動配置／釋放對齊記憶體
 2  src/types/group_state.rs← 型別重解釋
 2  src/file/fs_type.rs     ← syscall
 1  src/bin/bench.rs        ← 呼叫 speed_up_quit（非正式路徑）
```

先給結論，因為它是這一章唯一真正重要的事：

> **C++ 傳進來的指標與長度，Rust 這邊沒有做任何驗證。** `req.data`（u64 裸位址）與 `req.length`（u32）在 `core/engine.rs:304-310` 只被檢查了 `length != 0`，然後直接 `slice::from_raw_parts`。沒有 null 檢查、沒有可讀性檢查、沒有「這塊記憶體真的有 length 這麼長」的檢查。`chunk_id`、`expected_tag`、`desired_tag`、`begin`/`end`、`prefix` 這些 `&[u8]` 參數的情況更隱蔽——它們在**我們的原始碼裡看不到任何 `unsafe`**，因為 `from_raw_parts` 發生在 cxx 產生的 shim 裡（`target/cxxbridge/…/cxx.rs.cc`）。**整個引擎的記憶體安全，在 FFI 邊界上完全建立在 C++ 呼叫端的紀律之上**，而 Rust 這一側連一行斷言都沒有。

以下按風險性質分類，同一類的風險模型才相同。

### 13.1 FFI 邊界（10 處）——最大的信任邊界

| 位置 | 形式 | 繞過了什麼檢查 | 安全性論證 | 前提被破壞會怎樣 |
|---|---|---|---|---|
| `core/engine.rs:309-310` | `slice::from_raw_parts(req.data as *const _, req.length as usize)` | ①指標非 null ②指標可讀 ③`length` 個 byte 全部已初始化 ④借用期間無人改寫 ⑤`size ≤ isize::MAX` | 原始碼裡有六行 SAFETY 註解：「pointer 必須在 `update_chunk` 全程有效，資料在 `safe_write`/`copy_on_write` 內同步消費完畢；若未來改成非同步寫入，必須改成取得所有權（`Vec<u8>`）」。C++ 側 `req.data = reinterpret_cast<uint64_t>(state.data)`（`ChunkEngine.cc:53`）指向 `UpdateJob` 持有的 RDMA 接收緩衝區，其生命週期涵蓋整個 update 呼叫。⑤ 因 `length: u32 ≤ 4GiB` 而結構性成立，是**唯一**被型別系統擋住的一項 | ①→立即 segfault（`crc32c::crc32c(data)` 是呼叫後第一件事，會馬上碰到記憶體，這算是「幸運的失敗模式」）②③→讀到未初始化或別人的記憶體，**checksum 對得上就會被原樣寫進 chunk**，成為靜默的資料污染 ④→checksum 用的位元組與寫入磁碟的位元組不同，事後校驗才會發現 |
| `cxx.rs:131-135` | `unsafe fn release_raw_chunk`：`Arc::from_raw(chunk)` | 指標必須來自對應的 `Arc::into_raw`，且只能被消費一次 | 與 `cxx.rs:101`（`get_raw_chunk`）與 `cxx.rs:119`（`get_raw_chunks`）的 `Arc::into_raw` 一一對應；函式開頭有 `if !chunk.is_null()`。**釋放紀律完全由 C++ 的 `ChunkEngineReadJob` RAII 保證**（`BatchReadJob.h:32-36`：`std::exchange(engine_, nullptr)->release_raw_chunk(chunk_)`，配合刪除拷貝建構、移動時 `std::exchange` 置空） | 少呼叫→`Chunk` 永不 drop→`position_rc` 永不歸零→**該槽位永久洩漏、所在 group 永遠 compact 不掉** 多呼叫→refcount 提早歸零→use-after-free，且槽位會被重新分配給別的 chunk，造成資料互相覆蓋 |
| `cxx.rs:137-141` | `unsafe fn release_writing_chunk`：`Box::from_raw` | 同上 | 與 `cxx.rs:150`（`update_raw_chunk` 的 `Box::into_raw`）對應；由 `ChunkEngineUpdateJob` RAII 保證（`UpdateJob.h:31-35`） | drop 會觸發 `WritingChunk::Drop`，把 `abort` 設成 true；二次釋放則是 double free |
| `cxx.rs:172-183` | `unsafe fn commit_raw_chunk`：`Box::from_raw(new_chunk)` **取回所有權** | 同上 | 呼叫後 C++ 手上的指標即失效。`ChunkEngine::commit` 因此在下一行立刻 `job.chunkEngineJob().release()`（`ChunkEngine.cc:103`）把指標從 RAII 守衛摘掉 | 若忘了 `release()`，守衛解構時會再呼叫 `release_writing_chunk` → double free。**這兩行的順序耦合沒有任何機制保護** |
| `cxx.rs:185-196` | `unsafe fn commit_raw_chunks`：`reqs.iter().map(\|c\| *Box::from_raw(*c))` | 同上，且陣列裡每個指標都必須有效且互不重複 | 批量提交；`*Box::from_raw(*c)` 解引用取出值本身，`Box` 隨即釋放 | 陣列裡出現重複指標 → double free；`commit_chunks` 內部另有「同批次不得有重複 chunk_id」的檢查（`engine.rs:525-527`），但那是檢查 id 不是檢查指標 |
| `cxx.rs:479` | bridge 宣告 `unsafe fn speed_up_quit(&self);` | — | 見 §13.4 | — |
| `cxx.rs:483` | bridge 宣告 `unsafe fn release_raw_chunk(...)` | — | — | — |
| `cxx.rs:484` | bridge 宣告 `unsafe fn release_writing_chunk(...)` | — | — | — |
| `cxx.rs:493` | bridge 宣告 `unsafe fn commit_raw_chunk(...)` | — | — | — |
| `cxx.rs:500` | bridge 宣告 `unsafe fn commit_raw_chunks(...)` | — | — | — |

關於那 5 個 bridge 宣告，有兩件事必須講清楚：

1. **`unsafe` 在跨越邊界時蒸發了。** 在 `#[cxx::bridge] extern "Rust"` 區塊裡把函式標成 `unsafe fn`，cxx 產生的 C++ 簽章是**一個完全普通的成員函式**——C++ 沒有 `unsafe` 這個概念，也沒有任何 lint 或編譯期檢查會提醒呼叫者。所以 `engine.release_raw_chunk(chunk)`（`ChunkEngine.h:98`）在 C++ 看起來跟 `engine.raw_used_size()` 一樣無害。這個標記**只對 Rust 讀者有意義**。
2. **它讓函式本體變成隱式 unsafe block。** Rust 2021 中 `unsafe fn` 的本體整個是一個 unsafe 區塊，這就是為什麼 `release_raw_chunk` 的 `Arc::from_raw` 沒有再包一層 `unsafe {}`——這 5 個宣告與 4 個實作因此都算 `unsafe` 站點，不是重複計數。

還有一類**看不見的** `unsafe`：bridge 上所有 `&[u8]` / `&str` 參數（`chunk_id`、`begin`、`end`、`prefix`、`path`）在 C++ 側是 `rust::Slice<const uint8_t>` / `rust::Str`，轉成 Rust 引用的 `from_raw_parts` 由 cxx 在 `target/cxxbridge/` 的產生碼裡做。這些不會出現在上面的 26 處統計裡，但信任模型與 `req.data` 完全相同：**cxx 保證的是「有一個長度欄位跟著指標一起傳」，不保證那個指標指向的記憶體真的存在或真的有那麼長。**

### 13.2 `Send` / `Sync`：沒有 `unsafe impl`，但契約同樣沒被檢查

`grep -rn "unsafe impl" src` 的結果是**空的**——整個 crate 沒有任何一處手動宣告 `Send`/`Sync`。所有跨執行緒能力都是編譯器自動推導的。

但這不代表沒有風險，因為**跨到 C++ 之後，那些自動推導出來的 bound 沒有任何地方被要求**：

| 型別 | 怎麼跨執行緒 | 憑什麼成立 | 誰在檢查 |
|---|---|---|---|
| `Engine` | `*mut Engine` → `rust::Box<Engine>`，被多個 C++ 執行緒同時呼叫 `&self` 方法 | 需要 `Engine: Sync`。所有欄位都是 `Arc<T>`（`T: Send+Sync`）、`usize`、`Allocators`；`Mutex<ChunkAllocator>` 在 `T: Send` 時是 `Sync`；`rocksdb::DB` 的 `Send`/`Sync` 由 `rocksdb` crate 自己的 `unsafe impl` 向上傳導 | **crate 內部**有證據：`meta/rocksdb.rs:224-262` 把 `Arc<RocksDB>` 丟進 16 條執行緒併發寫入、`core/engine.rs:1671-1704` 把 `Engine` clone 到兩條執行緒——這些程式碼能編譯就代表 bound 成立。**但 cxx 的 `extern "Rust"` opaque type 不要求 `Send`/`Sync`**，邊界本身不檢查 |
| `Chunk` | `Arc::into_raw` 交給 C++，可能在**另一條執行緒**呼叫 `release_raw_chunk` | `Arc<T>` 跨執行緒送出需要 `T: Send + Sync`。`Chunk { ChunkMeta, Arc<Allocator> }` 兩者皆滿足 | 同上，無邊界檢查 |
| `WritingChunk` | `Box::into_raw` 交給 C++，update 與 commit **可能在不同執行緒**（folly executor 排程） | `Box<T>` 跨執行緒移動需要 `T: Send`。所有欄位滿足 | 同上，無邊界檢查 |

實務含意：如果有人往 `Engine` 裡加一個 `Rc<…>` 或 `Cell<…>` 欄位，**編譯不會失敗**（除非碰巧撞到那幾個內部多執行緒測試），但生產環境會出現資料競爭。要釘住這件事，正確做法是加一行 `static_assertions::assert_impl_all!(Engine: Send, Sync);`——crate 已經依賴 `static_assertions`（用來斷言 layout，`cxx.rs:585-604`），但沒有用它斷言任何 auto trait。

### 13.3 對齊記憶體配置（2 處）

```rust
// src/storage/chunk_engine/src/utils/aligned.rs:4-26
pub const ALIGN_SIZE: Size = Size::new(4096);
pub struct AlignedBuffer(&'static mut [u8]);

impl AlignedBuffer {
    pub fn new(size: usize) -> Self {
        Self(unsafe {
            let size = std::cmp::max(size, 1).next_multiple_of(ALIGN_SIZE.into());
            let layout = Layout::from_size_align_unchecked(size, ALIGN_SIZE.into());
            let ptr = std::alloc::alloc(layout);
            std::slice::from_raw_parts_mut(ptr, size)
        })
    }
}
impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe {
            let layout = Layout::from_size_align_unchecked(self.0.len(), ALIGN_SIZE.into());
            std::alloc::dealloc(self.0.as_mut_ptr(), layout);
        }
    }
}
```

| 位置 | 形式 | 繞過了什麼檢查 | 安全性論證 | 前提被破壞會怎樣 |
|---|---|---|---|---|
| `utils/aligned.rs:10-15` | `Layout::from_size_align_unchecked` + `alloc` + `from_raw_parts_mut` 造出 `&'static mut [u8]` | ①align 必須是 2 的冪 ②`size` 向上取整後不得溢位 `isize` ③`alloc` 回傳值非 null ④`'static` lifetime 的正當性 | ①`ALIGN_SIZE = 4096` 是編譯期常數、是 2 的冪 ②`max(size,1)` 保證不是 0（zero-size `alloc` 是 UB），呼叫端最大值是 `CHUNK_SIZE_ULTRA = 64MiB`，離溢位極遠 ④`'static` 是謊言，但欄位是私有的，`Deref`/`DerefMut` 把它重借成 `&'a [u8]`（`'a` 綁在 `&self` 上），洩漏不出去 | ③**沒有檢查**：OOM 時 `alloc` 回傳 null，`from_raw_parts_mut(null, size)` 立即是 UB。這是本 crate 唯一一個「論證不完整」的 `unsafe`。緩解因素：Rust 預設的 OOM 行為本來就是 abort，而 64MiB 級的配置失敗在 storage 節點上等同於系統已經完蛋 |
| `utils/aligned.rs:21-24` | `dealloc` | layout 必須與 alloc 時**完全一致** | size 存在 slice 自身的長度裡（`self.0.len()` 就是當初取整後的值）、align 是同一個常數；`Drop` 由編譯器保證只跑一次 | 若有人改動 `ALIGN_SIZE` 而舊 buffer 尚存活（不可能，它是常數）→ 堆損壞 |

**4096 這個對齊要求從哪來：`O_DIRECT`。** Linux 的 direct IO 要求緩衝區位址、檔案偏移、傳輸長度三者都是邏輯區塊大小的倍數；4096 是覆蓋常見裝置的安全上界。C++ 側用的是同一個數字（`src/fbs/storage/Common.h:80`：`constexpr auto kAIOAlignSize = 4096ul`）。**不是為了 RDMA**——RDMA 記憶體註冊不要求 4096 對齊。

**沒對齊會發生什麼：不是 UB，是降級。** `Cluster::pread`/`pwrite` 在每次迴圈都重新判斷對齊（`file/cluster.rs:64-71,86-93`），任一項不合就改用 `normal_fd`（`O_SYNC`）。所以萬一 `AlignedBuffer` 回傳了未對齊的記憶體，`is_aligned_buf` 會是 false，IO 靜默走到慢路徑——**效能塌掉但資料正確**。這也是為什麼這一處的 `unchecked` 在實務上比看起來安全。

### 13.4 刻意洩漏（2 處）

```rust
// src/storage/chunk_engine/src/core/engine.rs:171-182
/// Intentionally leaks Arc pointers to avoid shutdown overhead.
///
/// # Safety
///
/// This function **must only be called** when the process is guaranteed to exit immediately afterwards.
pub unsafe fn speed_up_quit(&self) {
    let _ = Arc::into_raw(self.meta_cache.clone());
    let _ = Arc::into_raw(self.writing_list.clone());
}
```

| 位置 | 形式 | 繞過了什麼檢查 | 安全性論證 | 前提被破壞會怎樣 |
|---|---|---|---|---|
| `core/engine.rs:178-182` | `pub unsafe fn` + `Arc::into_raw` 丟棄回傳值 | 沒有繞過任何記憶體安全檢查——**這裡的 `unsafe` 純粹是一個「別亂用」的標記**。洩漏在 Rust 裡是 safe 的（`mem::forget` 就是 safe fn） | 目的是跳過 `meta_cache`（可達百萬個 `Arc<Chunk>`，每個 drop 都要進 `Mutex<ChunkAllocator>` 做 `dereference`）與 `writing_list` 的遞迴解構，把關機時間從秒級壓到毫秒級 | 在非退出路徑呼叫 → 這兩個結構的記憶體永久洩漏，且 `position_rc` 永不歸零。**不會 UB，只會漏**。作者用 `unsafe` 標記是為了強制呼叫端寫出 `unsafe {}` 並停下來想一想——這是把 `unsafe` 當「請求覆核」用，而非表達記憶體不安全 |
| `bin/bench.rs:88` | `unsafe { engine.speed_up_quit(); }` | 同上 | **不在正式路徑上**：`src/bin/bench.rs` 是 crate 自帶的獨立壓測二進位，第 88 行是 `main` 的倒數第二行，緊接著 `Ok(())` 後行程退出，契約滿足 | 無（工具程式） |

正式路徑上的呼叫在 C++：`src/storage/service/Components.cc:200-204`，包在 `if (config.speed_up_quit())` 裡（配置項預設 `true`，`Components.h:65`），位置是 `Components::stop` 的最後一步。

### 13.5 型別重解釋 / `transmute`（5 處）

| 位置 | 形式 | 繞過了什麼檢查 | 安全性論證 | 前提被破壞會怎樣 |
|---|---|---|---|---|
| `types/group_state.rs:115` | `transmute(&[u64;4]) -> &[u8;32]` | 型別系統對 layout 的檢查 | 陣列 layout 有保證：`[u64;4]` = 32 byte / align 8，`[u8;32]` = 32 byte / align 1；目標對齊要求更寬鬆；不可變借用不引入別名問題 | 不會 UB。真正的風險是**位元組序**：落盤的 group bitmap 是本機序，blob 不能跨 endianness 遷移。同構叢集下無影響 |
| `types/group_state.rs:119` | 同上，`&mut` 版 | 同上 | `&mut` 獨佔，無別名 | 同上 |
| `cxx.rs:54`（`Chunk::raw_meta`） | `transmute(&ChunkMeta) -> &ffi::RawMeta` | **把一個引用轉成指向更小型別的引用** | 兩者都是 `#[repr(C)]`（`types/chunk_meta.rs:6`；cxx 對 shared struct 一律加 `#[repr(C)]`），欄位型別與順序逐一對應：`u64,u32,u32,u32,u32,u64,u64,u64,u64` → offset `0,8,12,16,20,24,32,40,48`，size 56 / align 8。`ChunkMeta` 尾端多出 `etag: TinyVec` 與 `uncommitted: bool`，所以 `RawMeta` 是它的**精確前綴** | **`static_assertions` 只斷言 align 相等，刻意不斷言 size 相等**（`cxx.rs:585-588`）——因為 size 本來就不同。代價是：任何人在 `ChunkMeta` 中間插入或重排一個欄位，**編譯照過、斷言照過**，C++ 讀到的每一個 meta 欄位全部錯位。這是全 crate 最脆弱的一處靜態耦合 |
| `cxx.rs:356`（`RawChunks::chunk_meta`） | 同上 | 同上 | 同上 | 同上 |
| `cxx.rs:90`（`Engine::raw_used_size`） | `transmute(UsedSize) -> ffi::RawUsedSize`（**按值**） | 同上 | `UsedSize` 是 `#[repr(C)]`（`alloc/allocator_counter.rs:13-20`），其 `Size` 欄位也是 `#[repr(C)] Size(pub u64)`（`utils/size.rs:1-3`），展開後就是 4 個 u64。**size 與 align 兩項都被 `const_assert_eq!` 釘住**（`cxx.rs:589-596`） | 欄位增減會被斷言擋下 → 編譯失敗。**這是唯一一組防護完整的 transmute** |

`Metrics` 另有一組 size/align 斷言（`cxx.rs:597-604`）：Rust 側 18 個 `AtomicU64`、cxx 側 18 個 `u64`。`AtomicU64` 的 layout 與 `u64` 相同是標準庫的保證，但這裡是靠斷言釘住而非型別系統——不過 `Metrics` 並沒有被 transmute（`get_metrics` 是逐欄位 `swap` 出來重新組裝的，`cxx.rs:267-309`），所以這組斷言其實是純防禦性的。

### 13.6 libc / syscall（3 處）

所有 `extern "C"` 呼叫在 Rust 裡一律是 `unsafe`，因為編譯器無從得知 C 函式會對指標做什麼。

| 位置 | 形式 | 繞過了什麼檢查 | 安全性論證 | 前提被破壞會怎樣 |
|---|---|---|---|---|
| `file/fs_type.rs:15` | `mem::zeroed::<libc::statfs>()` | 「值必須是該型別的合法位元模式」 | `statfs` 是純整數欄位的 POD C struct，沒有 `NonNull`/enum/引用，全零是合法值。零初始化只是為了讓後續的 `&mut stat` 有定義良好的初值 | 若未來 libc 把某欄位改成有 niche 的型別 → UB。實務上不會發生 |
| `file/fs_type.rs:16` | `libc::statfs(path_cstr.as_ptr(), &mut stat)` | extern "C" 呼叫 | `path_cstr` 是同一函式的區域變數，在呼叫期間存活；`CString::new(...).unwrap()` 的 panic 條件是路徑含內嵌 NUL——Unix 路徑不可能含 NUL；回傳值被檢查，非 0 時退化成 `FsType::OTHER` | 失敗時保守地回 `OTHER`，`support_direct_io()` 仍為 true → 對真正的 ZFS 會開 `O_DIRECT` 而失敗。這是**降級而非崩潰** |
| `file/cluster.rs:44-51` | `libc::fallocate(fd, mode, offset, len)` | extern "C" 呼叫 | fd 來自 `self.direct_fd`（一個活著的 `File`，所有權在 `Cluster` 手上）；`mode` 是編譯期常數（`0` 或 `PUNCH_HOLE\|KEEP_SIZE`）；`offset`/`len` 由 `GroupId::offset()`/`size()` 純算術產生，必為正且對齊 group 邊界；回傳 `-1` 轉成 `Error::IoError` 並帶上 `last_os_error()` | **前提破壞的後果不是 UB，是靜默資料毀損**：punch hole 打在錯誤的 offset 上，會把該區間的**活躍 chunk 內容歸零**，而且沒有任何錯誤回報——`fallocate` 會成功回 0。這使得 §4.2 的運算子優先級陷阱（`GroupId::offset()`）具有實質破壞力：算錯 offset 不會 crash，只會悄悄清掉別人的資料 |

### 13.7 測試專用（4 處）

這四處只在 `#[cfg(test)]` 內，不進入任何發行產物：

| 位置 | 形式 | 說明 |
|---|---|---|
| `core/engine.rs:805` | `unsafe { engine.speed_up_quit(); }` | 測試**故意違反**契約以覆蓋該函式；原始碼註解明說「This test intentionally violates that contract… will cause memory leaks that persist for the remainder of the test process」 |
| `core/engine.rs:1308-1310` | `transmute::<&[u8], &[u8]>(chunk.meta().etag.as_slice())` | **lifetime laundering**：把一個短命的 `&[u8]` 洗成 `&'static [u8]`，好塞進 `UpdateReq::expected_tag`。這暴露了 §15.4 講的問題——`expected_tag`/`desired_tag` 的 `'static` 標註本來就是謊言，連 Rust 自己的測試都得靠 transmute 才餵得進去 |
| `file/cluster.rs:167-168` | `File::from_raw_fd(23333)` × 2 | 用一個幾乎必然無效的 fd 構造 `Cluster` 以測試 IO 錯誤路徑；緊接著 `std::mem::forget(cluster)`（第 172 行）避免 drop 時 `close(23333)` 誤關到別人的 fd |

### 13.8 小結

| 類別 | 處數 | 風險等級 | 判準 |
|---|---|---|---|
| FFI 邊界（1 個 `from_raw_parts` + 4 個實作 + 5 個 bridge 宣告） | 10 | **高** | 論證完全依賴 C++ 呼叫端；Rust 側零驗證；`unsafe` 標記在 C++ 看不見 |
| 型別重解釋 transmute（`cxx.rs:54,90,356`、`types/group_state.rs:115,119`） | 5 | **中高** | 前三處是 `ChunkMeta`→`RawMeta` 之類的靜態耦合，改欄位不會被發現；後兩處是 bitset，陣列 layout 有標準保證，唯一議題是位元組序可攜性 |
| 對齊配置 | 2 | 低（一項論證不完整） | `alloc` 未檢查 null；其餘皆由編譯期常數保證 |
| syscall | 3 | 低（後果嚴重但前提穩固） | 參數皆為內部純算術產物；`fallocate` 算錯 offset 會靜默毀資料 |
| 刻意洩漏 | 2 | 極低 | 不涉及記憶體安全，`unsafe` 被當作「請求覆核」使用 |
| 測試專用 | 4 | 不適用 | 不進發行產物 |
| **合計** | **26** | | 與 `grep -rn "unsafe" src benches examples \| wc -l` 相符 |

**如果只能改一件事**：在 `update_chunk` 進入點加上 `if req.length != 0 && req.data == 0 { return Err(Error::InvalidArg(...)) }`。這一行擋不住「長度謊報」，但能把最常見的失效模式（C++ 側忘了設 `state.data`）從 segfault 變成一個帶錯誤碼的 `Result`，成本是每次寫入一次比較。目前 `cxx.rs:153-166` 那張錯誤碼映射表裡已經有 `Error::InvalidArg => 3`，接得上。

---

## 14. 錯誤處理：`Result<T>` 怎麼過河

Rust 側是一個扁平的錯誤列舉：

```rust
// src/storage/chunk_engine/src/utils/result.rs:1-17
#[derive(Debug, PartialEq)]
pub enum Error {
    IoError(String), RocksDBError(String), MetaError(String), InvalidArg(String),
    SerializationError(derse::Error), ChecksumMismatch(String), ChainVersionMismatch(String),
    ChunkETagMismatch(String), ChunkAlreadyExists, ChunkCommittedUpdate(String),
    ChunkMissingUpdate(String), NoSpace,
}
pub type Result<T> = std::result::Result<T, Error>;

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)      // Display 直接委派給 Debug
    }
}
```

`Display` 委派給 `Debug` 意味著 C++ 收到的錯誤字串長這樣：`InvalidArg("invalid pos")`（`utils/result.rs:29-33` 的測試釘住了這個格式）。變體名稱本身就是分類資訊。

cxx 橋接**不使用 `Result`**——因為 `Result<*mut T>` 在 cxx 裡沒有對應。取而代之的是兩個慣例：

**慣例 A：`Pin<&mut CxxString> error` 出參 + 哨兵回傳值**

```rust
// src/storage/chunk_engine/src/cxx.rs:93-108
fn get_raw_chunk(&self, chunk_id: &[u8], error: Pin<&mut CxxString>) -> *const Chunk {
    match self.get(chunk_id) {
        Ok(None)    => { error.clear(); std::ptr::null() }        // 不存在：null + 空 error
        Ok(Some(c)) => { error.clear(); Arc::into_raw(c) }        // 成功：指標 + 空 error
        Err(e)      => { error.push_str(&e.to_string()); std::ptr::null() }  // 失敗：null + error
    }
}
```

C++ 端必須**先檢查 `error.empty()` 再檢查指標**，才能區分「不存在」與「出錯」：

```cpp
// src/storage/store/ChunkEngine.h:86-93
auto chunk = engine.get_raw_chunk(toSlice(key), error);
if (UNLIKELY(!error.empty())) return makeError(StorageCode::kChunkMetadataGetError, std::move(error));
if (chunk == nullptr)         return makeError(StorageCode::kChunkMetadataNotFound);
```

注意 `Ok(_)` 的兩個分支都呼叫 `error.clear()`——因為 C++ 側往往重用同一個 `std::string`，不清會殘留上次的錯誤。

**慣例 B：`UpdateReq` 的 `out_error_code` 出參**

寫入路徑需要把錯誤**分類**傳給 C++（不同錯誤在 CRAQ 裡觸發不同動作），所以額外做了一張映射表：

```rust
// src/storage/chunk_engine/src/cxx.rs:153-166
req.out_error_code = match e {
    Error::IoError(_)              => 4011,   // ChunkWriteFailed
    Error::RocksDBError(_)         => 4003,   // ChunkMetadataSetError
    Error::MetaError(_)            => 4002,   // ChunkMetadataGetError
    Error::InvalidArg(_)           => 3,      // InvalidArg
    Error::SerializationError(_)   => 4002,   // ChunkMetadataGetError
    Error::ChecksumMismatch(_)     => 4080,   // ChecksumMismatch
    Error::ChainVersionMismatch(_) => 4081,   // ChainVersionMismatch
    Error::ChunkETagMismatch(_)    => 4083,   // ChunkETagMismatch
    Error::ChunkAlreadyExists      => 4084,   // ChunkAlreadyExists
    Error::ChunkCommittedUpdate(_) => 4008,   // ChunkCommittedUpdate
    Error::ChunkMissingUpdate(_)   => 4007,   // ChunkMissingUpdate
    Error::NoSpace                 => 7021,   // NoSpace
};
```

這些數字是 3FS `StorageCode` 的值，**硬編在 Rust 裡並用註解標名**。C++ 端直接 `makeError(req.out_error_code, std::move(error))`（`ChunkEngine.cc:70`）。這是整個橋接最脆弱的耦合點：改動 `StorageCode` 的數值而忘了改這裡，錯誤會被靜默誤分類。沒有任何編譯期檢查保護它。

**慣例 C：`Box<RawChunks>` 空物件**

批量查詢失敗時回傳 `Default::default()`（空的 `RawChunks`）並填 error（`cxx.rs:198-239`），C++ 檢查 error 而非長度。

---

## 15. cxx 橋接：完整型別對應與所有權轉移

### 15.1 opaque 型別

`#[cxx::bridge(namespace = "hf3fs::chunk_engine")]`（`cxx.rs:368`）宣告了 5 個 `extern "Rust"` 區塊，暴露 5 個 opaque 型別：

| Rust 型別 | C++ 看到的 | 傳遞方式 |
|---|---|---|
| `Engine` | `chunk_engine::Engine` | `*mut Engine`（create）→ `rust::Box<Engine>::from_raw`（`StorageTargets.cc:63`） |
| `Chunk` | `chunk_engine::Chunk` | `*const Chunk`，實為洩漏的 `Arc` 強引用 |
| `WritingChunk` | `chunk_engine::WritingChunk` | `*mut WritingChunk`，實為洩漏的 `Box` |
| `RawChunks` | `chunk_engine::RawChunks` | `rust::Box<RawChunks>`（cxx 原生支援，自動析構） |
| `LogGuard` | `chunk_engine::LogGuard` | `*mut LogGuard`，永不釋放（進程級日誌 guard） |

### 15.2 shared struct（值型別，兩邊都能構造）

| struct | 欄位 | 方向 |
|---|---|---|
| `UpdateReq` | 15 個 in + 5 個 `out_*` | C++ 建構，`Pin<&mut>` 傳入，Rust 回填 out |
| `GetReq<'a>` | `chunk_id: &'a [u8]`、`chunk_ptr: *const Chunk` | C++ 建陣列，Rust 回填 ptr |
| `RawMeta` | `pos, chain_ver, chunk_ver, len, checksum, timestamp, last_request_id, last_client_low, last_client_high` | Rust → C++（transmute 出的引用） |
| `RawUsedSize` | `allocated_size, reserved_size, position_count, position_rc` | Rust → C++（transmute 的值） |
| `FdAndOffset` | `fd: i32, offset: u64` | Rust → C++，**讓 C++ 直接對 fd 發 io_uring** |
| `Metrics` | 18 個 u64 | Rust → C++（讀即清零） |

`FdAndOffset` 是效能上最重要的一個：讀路徑**不經過 Rust**。`ChunkEngine::aioPrepareRead`（`ChunkEngine.h:34-73`）只從 Rust 拿到 `(fd, offset)` 和 `(len, checksum)`，實際的讀取由 C++ 的 aio/io_uring 直接對 `direct_fd` 發起。`Clusters::fd_and_offset` 回傳的就是 `direct_fd.as_raw_fd()`（`file/clusters.rs:57-62`）——**Rust 把裸 fd 借給 C++，生命週期由 `Arc<Chunk>` 的引用保證 `Cluster` 不會被關閉**。

### 15.3 所有權轉移一覽

```
C++                                     Rust
────────────────────────────────────────────────────────────────────────
create(path, …)                    ──→  Box::into_raw(Box::new(Engine))
rust::Box<Engine>::from_raw(p)     ←──  （C++ 接管，析構時呼叫 release）
release(Box<Engine>)               ──→  fn release(_engine: Box<Engine>) {}   ← 空函式，靠 drop

get_raw_chunk() → *const Chunk     ←──  Arc::into_raw(chunk)          【強引用 +1】
release_raw_chunk(p)               ──→  Arc::from_raw(p)              【強引用 -1】

update_raw_chunk() → *mut Writing… ←──  Box::into_raw(Box::new(WritingChunk))
release_writing_chunk(p)           ──→  Box::from_raw(p)              【放棄，abort=true】
commit_raw_chunk(p, sync)          ──→  Box::from_raw(p) + commit     【消費】

query_raw_chunks() → Box<RawChunks>←──  Box::new(RawChunks { vec })   【cxx 自動管理】
```

`fn release(_engine: Box<Engine>) {}`（`cxx.rs:25`）這個空函式體是 cxx 的慣用寫法：`Box<Engine>` 進入函式即取得所有權，函式結束就 drop。`Engine::drop` 會呼叫 `stop_and_join()` 停掉背景 worker（`core/engine.rs:749-753`）。

`get_raw_chunks`（批量版，`cxx.rs:110-129`）對**每一個** `GetReq` 都做 `Arc::into_raw`，所以 C++ 必須對每一個非 null 的 `chunk_ptr` 呼叫 `release_raw_chunk`。

`commit_raw_chunks`（`cxx.rs:185-196`）一次接管一批 `*mut WritingChunk`：`reqs.iter().map(|c| *Box::from_raw(*c)).collect::<Vec<_>>()`——注意 `*Box::from_raw(*c)` 是解引用取出 `WritingChunk` 值本身，`Box` 隨即釋放。

### 15.4 `Pin<&mut CxxString>` 與 `&'static [u8]`

`UpdateReq` 的 `expected_tag` / `desired_tag` 宣告成 `&'static [u8]`（`cxx.rs:388-389`）。這個 `'static` **是謊言**——它們實際指向 C++ 棧上的字串。cxx 不支援在 shared struct 裡放具名 lifetime，所以只能用 `'static` 蒙混，靠「Rust 在 `update_chunk` 返回前用完它們」來保證安全。`core/engine.rs:1308-1310` 的測試裡甚至要顯式 `transmute::<&[u8], &[u8]>` 來偽造這個 lifetime。

這與 #9（`req.data` 的裸指標）是同一類問題的兩種表現：**cxx 的 shared struct 無法表達借用關係，所有生命週期契約都靠註解和呼叫約定維持**。

---

## 16. C++ 側呼叫點總覽

| C++ 位置 | 呼叫的 Rust API | 用途 |
|---|---|---|
| `StorageTargets.cc:59` | `chunk_engine::create` | 每顆碟一個 engine，`prefix_len = sizeof(ChainId) = 8`，並行開啟（folly coroutine） |
| `ChunkEngine.h:47,86` | `get_raw_chunk` | 讀路徑 / 單 chunk 查詢 |
| `ChunkEngine.h:67` | `Chunk::fd_and_offset` | 拿 fd/offset 給 aio |
| `ChunkEngine.cc:60` | `update_raw_chunk` | CRAQ update 階段 |
| `ChunkEngine.cc:96,102` | `WritingChunk::set_chain_ver`、`commit_raw_chunk` | CRAQ commit 階段 |
| `ChunkEngine.h:119` | `query_uncommitted_raw_chunks` | 恢復時列出未提交 |
| `ChunkEngine.h:162` | `handle_uncommitted_raw_chunks` | 恢復時全部提交並蓋 chain_ver |
| `ChunkEngine.h:118` | `query_raw_chunks` | 範圍查詢（降序） |
| `ChunkEngine.h:199,235` | `query_all_raw_chunks` | 列出一條 chain 的全部 chunk（含寫入中） |
| `ChunkEngine.h:189` | `raw_batch_remove` | 刪掉整條 chain |
| `ChunkEngine.h:259` | `query_raw_used_size` | chain 用量 |
| `AllocateWorker.cc:49-51` | `allocate_groups` / `allocate_ultra_groups` / `compact_groups` | 100ms 週期的空間維運 |
| `CheckWorker.cc:181` | `set_allow_to_allocate` | 磁碟將滿時停止新配置 |
| `CheckWorker.cc:227,247` | `raw_used_size` / `get_metrics` | 每秒上報 monitor |
| `Components.cc:202` | `speed_up_quit` | 關機加速（由 `speed_up_quit` 配置項控制，預設 true） |

`ChunkEngine::copyMeta` 裡有一個給舊引擎的相容欄位值得一提：

```cpp
// src/storage/store/ChunkEngine.h:23-24
out.innerFileId = ChunkFileId{std::max(uint32_t(in.pos >> 48 << 16), 512u * 1024), 256};
out.innerOffset = in.pos;
```

`in.pos >> 48 << 16` 正是把 `Position` 的 chunk_size 欄位還原成位元組數（bit 40 存 `chunk_size>>8`，右移 48 = 右移 8 再右移 40 → `chunk_size >> 16`，左移 16 還原）。`std::max(…, 512KiB)` 是為了讓舊格式的 `ChunkFileId` 至少是 512KiB。`innerOffset` 則直接塞整個 `Position` u64——舊格式的欄位在新引擎下只是佔位。

---

## 17. 測試涵蓋了哪些不變式

`#[cfg(test)]` 分佈在 18 個檔案裡，反推作者最擔心的東西：

**(1) 位元打包必須可逆**（`types/position.rs:94-116`、`types/group_id.rs:91-113`）
每個欄位 round-trip、`Debug` 輸出字串逐字比對、`plus_one()` 的 256 進位跑 25.6 萬次。

**(2) bitmap 的 allocate/deallocate 必須嚴格配對**（`types/group_state.rs:127-162`）
配滿 256 個確認第 257 次回 `None`；隨機順序釋放，**每次釋放後立刻再釋放一次確認回 `Err`**（雙重釋放偵測）；序列化 round-trip；長度不對必須 `Err`。

**(3) MergeState 必須滿足結合律**（`types/merge_state.rs:49-88`）
acquire 後 release 等於什麼都沒做；256 個 acquire 疊出 full；再 256 個 release 回到 empty。

**(4) 損壞的 merge operand 必須讓讀取失敗而非產生錯資料**（`meta/rocksdb.rs:264-291`）
明確驗證 `get()` 和 `iterate()` 都回 `Err`，並在 `put` 覆蓋後恢復。

**(5) `prefix_len` 遷移必須正確重算**（`meta/meta_store.rs:800-877`）
0 → 1 → 0 來回，1024 個混合大小的 chunk，逐前綴核對用量。

**(6) `Position` 的引用計數必須精確控制槽位重用時機**（`core/engine.rs:838-921`）
這是最長的一段測試：寫 → move → 再配置 → drop 舊引用 → 確認下一次配置拿到**恰好那個 index**。每一步都斷言 `reserved_size`。

**(7) compact 期間的併發讀必須完全正確**（`core/engine.rs:1122-1165`）
起一條背景執行緒不停讀 512 個 chunk 並核對 checksum，同時主執行緒做 compact + punch hole。

**(8) 寫入中崩潰後重啟必須能恢復**（`core/engine.rs:1532-1669`）
兩個測試：一個是單次未提交寫入；另一個更刁鑽——連續三次 `update_chunk` 都 drop 掉（模擬三次失敗），重啟後 `query_uncommitted_chunks` 必須只回 1 條，且 `handle_uncommitted_chunks` 之後長度正確。

**(9) `WritingChunk` 掉出 writing_list 必須 panic**（`alloc/writing_chunk.rs:107-123`）
三個 `#[should_panic]`，覆蓋 prefix 不存在、chunk_id 不存在、commit_succ 但找不到三種情形。

**(10) 併發 update 與 get 不能 panic 或損壞**（`core/engine.rs:1671-1704`）
兩條執行緒跑 2 秒：一條不停 write + remove，一條不停 get。

**(11) 版本檢查的完整狀態機**（`core/engine.rs:1223-1270`）
重放（同版本再送）必須 `Err`、跳號必須 `Err`、`is_syncing` 必須繞過檢查、chain_ver 倒退必須 `Err`——而且每次失敗後 `out_*` 欄位必須保持在正確的舊值（讓上游知道真實狀態）。

**(12) 對不存在位置的 deallocate 必須 panic**（`alloc/chunk_allocator.rs:298-303`）

值得注意的是**沒有**的測試：沒有 fuzz、沒有 property-based test、沒有真正的斷電注入（`drop(engine)` 是乾淨關閉，不等於 `kill -9`）、沒有多 engine 並發測試。crash recovery 的驗證依賴「drop 掉 `WritingChunk` 而不 commit」這個代理行為。

---

## 18. 幾處值得記錄的觀察

1. **`ChunkMeta` 的 `derse` 編碼是「長度前綴 + 固定寬度小端 + 變長尾巴」。** `types/chunk_meta.rs:71-79` 的測試把 byte 序列寫死了：首 byte `63` 是總長度，接著 `pos`(8B LE)、`chain_ver`/`chunk_ver`/`len`/`checksum`(各 4B)、`timestamp`/`last_request_id`/`last_client_low`/`last_client_high`(各 8B) = 56 bytes，然後 `5,'h','e','l','l','o'`（長度前綴的 etag），最後 `0`（`uncommitted` bool）。**沒有欄位標籤、沒有版本號**——欄位順序一改就不相容，靠 `MetaKey::VERSION_KEY` 在上層做遷移。

2. **`ChunkMeta::default()` 的 `pos` 是 `GroupId::new(Size::GB, 0, 0)`**（`types/chunk_meta.rs:39`）。1GiB 不是任何合法分級（分級最大 64MiB），所以 `select_by_pos` 對它會回 `InvalidArg`。這是刻意的「毒值」：預設構造出來的 meta 若被誤用會立即報錯，而不是靜默指向 offset 0。

3. **`prefix_len = 0` 時整個 used_size 表退化成單一鍵**（`meta_store.rs:546-548` 特判插入空 `Bytes`），所有測試預設就是這個模式。生產環境是 8。

4. **`occupy_uncommitted_positions` 用 `mem::swap` 臨時把 `prefix_len` 改成 0**（`meta_store.rs:368-371），好讓 `query_uncommitted_chunks(&[])` 的 `check_prefix` 通過。這是為了掃描「全部 chain 的未提交寫入」而繞過前綴校驗的權宜寫法。

5. **`Chunk::BUFFER` 與 `ZERO` 各佔 64MiB。** 前者是 thread_local（每條寫入執行緒 64MiB），後者是全域一份。在一個開 32 條寫入執行緒的節點上，光是這兩者就是 2GiB 常駐。這是用記憶體換「最大分級也能一次搞定」的簡潔性。

6. **`Cluster` 的 `normal_fd` 帶 `O_SYNC` 而非 buffered。** 未對齊的寫入走 `normal_fd`，每次都同步落盤。這讓「對齊走 O_DIRECT、不對齊走 O_SYNC」兩條路徑的持久性語意一致，代價是未對齊寫入很慢——這也是 `safe_write` 費那麼大力氣讀回尾塊湊對齊的原因。

7. **`ChunkAllocator::load` 在掃描時斷言 group id 單調遞增**（`chunk_allocator.rs:53-56`：`assert!(current <= group_id)`）。這依賴 RocksDB 大端 key 的字典序等於 `GroupId` 的數值序——由 §7.2 的 key 佈局保證。中間的空洞被填進 `unallocated_groups`，所以「曾經被 punch hole 的 group」在重啟後自動被識別出來，不需要額外記錄。

8. **`ShardsMap::iter()` 的實作有個微妙前提**（`utils/shards_map.rs:68-73`）：`array_it` 從 `shards[1..]` 開始、`inner_it` 初始化成 `shards[0].iter()`。若 `S == 0` 會 panic，但 `S` 是 const generic 且從未被實例化為 0。

9. **`examples/chunk_viewer.rs` 是離線一致性檢查工具**：以唯讀模式開 RocksDB，對 11 個分級各載入一次 `ChunkAllocator`，然後掃全部 chunk meta 逐一 `reference(pos, true)`，最後 `assert_eq!(used_map, real_map)`——驗證「group bitmap 記錄的已用槽位數」等於「chunk meta 表裡實際存在的 chunk 數」。`first_ref = true` 同時檢查了「沒有兩個 chunk 指向同一個 Position」。這是最強的離線不變式檢查。

10. **`Size` 的 `Display` 與 `Debug` 語意不同**（`utils/size.rs:206-228`）：`Display` 只在整除時用大單位（`512KiB`），`Debug` 一律用兩位小數的近似（`0.50MiB`）。目錄名用的是 `Display`（`allocators.rs:33`），所以目錄一定叫 `64KiB` 而不是 `0.06MiB`。

11. **3FS 有兩個層次完全不同的 storage 壓測工具，不要搞混。**

    | | `chunk_engine/src/bin/bench.rs`（Rust） | `benchmarks/storage_bench/`（C++） |
    |---|---|---|
    | 測什麼 | **單機本地引擎**：多執行緒直接呼叫 `Engine::write` | **整條網路路徑**：client → RDMA → storage service → CRAQ 鏈複製 → 引擎 |
    | 繞過什麼 | 繞過 RPC、RDMA、chain 複製、mgmtd 路由 | 什麼都不繞過 |
    | 配置 | 一個 toml（`engine` / `threads` / `count` / `level`），90 行 | 40+ 個 gflags：`serverMode`/`clientMode`/`clusterMode`、`numChains`/`numReplicas`、`verifyReadChecksum`、`injectRandomServerError` … |
    | 輸出 | 每秒印一行 `throughput / allocated / reserved`（`bin/bench.rs:70-80`） | monitor 指標 + ClickHouse 上報 |
    | 何時用 | 判斷「配置器／磁碟／RocksDB 是不是瓶頸」 | 判斷「端到端吞吐與延遲」 |

    另有 `benches/bench_allocator.rs`（criterion）測的是更小的一層——**純記憶體的槽位配置快路徑**，連磁碟都不碰。三者由內而外構成三個同心圓：`bench_allocator` ⊂ `bin/bench` ⊂ `storage_bench`。定位效能問題時應該由內往外逐層排除。

---

## 19. 檔案索引表

### `src/storage/chunk_engine/` 根目錄

| 檔案 | 行數 | 職責 |
|---|---|---|
| `Cargo.toml` | 42 | crate 定義：`crate-type = ["lib","staticlib"]`、17 個依賴（rocksdb / cxx / derse / crc32c / lockmap / dashmap / tinyvec …） |
| `build.rs` | 4 | 呼叫 `cxx_build::bridge("src/cxx.rs")` 生成 C++ 側的 `.h`/`.cc` |
| `README.md` | 62 | 作者自述的設計要點：Allocator + MetaStore 兩分、三步寫入流程、`Arc` 管理位置所有權 |
| `.gitignore` | 2 | 忽略 `target/` 與 `lcov.info` |
| `docs/architecture.drawio.svg` | — | 架構圖（drawio 匯出） |
| `benches/bench_allocator.rs` | 42 | criterion benchmark：65536 次 `allocate` + `drop` 的純記憶體快路徑 |
| `examples/chunk_viewer.rs` | 94 | 離線工具：唯讀開啟 RocksDB，交叉驗證 group bitmap 與 chunk meta 表的一致性 |

### `src/` — 頂層

| 檔案 | 行數 | 職責 |
|---|---|---|
| `lib.rs` | 18 | 7 個 mod 宣告 + 全部 `pub use *`，構成扁平命名空間 |
| `cxx.rs` | 604 | `#[cxx::bridge]` 定義、所有 `raw_*` 轉接函式、`Error` → `StorageCode` 的數值映射、5 組 layout 斷言 |
| `bin/bench.rs` | 90 | 獨立壓測二進位：讀 toml 配置、多執行緒寫入、每秒印吞吐量。**只測本地引擎**，繞過 RPC/RDMA/chain 複製——與 `benchmarks/storage_bench/`（測整條網路路徑的 C++ 工具）是兩個層次，見 §18-11 |

### `src/core/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `mod.rs` | 3 | 宣告 `engine` |
| `engine.rs` | 1705 | **對外唯一入口**：開機與 crash recovery、`get`/`batch_get`/`meta_cache`、`update_chunk`/`commit_chunk(s)`、`move_chunk`、`batch_remove`、compact 驅動、schema 升級、11 個整合測試 |

### `src/alloc/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `mod.rs` | 17 | 8 個子模組宣告與再匯出 |
| `allocators.rs` | 200 | 11 個 chunk 大小分級的 `Allocator` 陣列；`select_by_pos`/`select_by_size` 的 `trailing_zeros` 查找；`get_allocate_tasks`（commit 1831776 的修正點） |
| `allocator.rs` | 258 | `Mutex<ChunkAllocator>` + `Clusters` + `AllocatorCounter` 的組合體；`do_allocate_task` 的取任務／執行 syscall／歸位三段式 |
| `chunk_allocator.rs` | 304 | 槽位狀態機核心：五個 group 集合、`active_levels` 四桶、`position_rc` 引用計數、快慢路徑配置、compact 任務的凍結／解凍 |
| `group_allocator.rs` | 190 | `allocated_groups` / `unallocated_groups` 兩池 + `next_group_id`；`AllocateTask` 的水位判斷與成敗歸位 |
| `chunk.rs` | 312 | `Chunk` 的 RAII 引用語意（`Clone` = reference、`Drop` = dereference）；`copy_on_write`、`safe_write`、`copy_chunk`；三種 checksum 策略；TLS 64MiB buffer 與全域 `ZERO` buffer |
| `writing_chunk.rs` | 124 | `WritingChunk`：未提交寫入的 RAII 憑證；`Drop` 時依 `commit_succ` 決定移出清單或標記 `abort`，找不到就 panic |
| `allocator_counter.rs` | 88 | 4 個 `AtomicU64`（allocated/reserved chunks、position count/rc）與 `UsedSize` 值型別（`#[repr(C)]`，直接 transmute 給 C++） |
| `metrics.rs` | 27 | 18 個 `AtomicU64` 的效能計數器（COW 次數/延遲、checksum 三分類、safe_write 五分類、allocate/pwrite 延遲） |

### `src/meta/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `mod.rs` | 9 | 4 個子模組宣告 |
| `meta_key.rs` | 217 | 9 個 key 前綴常數與各自的編解碼；chunk_id 按位元反相（降序掃描）、`Position`/`GroupId` 大端序、`group_to_chunks_key_prefix` 的 `pop()` 技巧、timestamp key 的前綴切分 |
| `meta_store.rs` | 878 | RocksDB 上的 5 張邏輯表；`add_chunk_mut`/`move_chunk_mut`/`remove_mut` 的六步 WriteBatch；writing chunk log 的寫入與 crash recovery 的 occupy／vacate；`used_size` 的 `prefix_len` 遷移；schema 版本與 `remove_range_mut` 白名單 |
| `meta_merge.rs` | 152 | RocksDB MergeOperator：用 `key[0]` 分派 group bits（`MergeState` 差量疊加）、used size（i64 累加）、test（串接）三種語意 |
| `rocksdb.rs` | 314 | `rocksdb` crate 的薄封裝：bloom filter 10 bits/key、4MiB readahead、`[WriteOptions; 2]` 的 sync/non-sync 切換、`RocksDBIterator::iterate` 的前綴掃描 |

### `src/file/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `mod.rs` | 7 | 3 個子模組宣告 |
| `clusters.rs` | 118 | 256 個 cluster 檔的容器；依 `pos.cluster()` 分派 pread/pwrite/fallocate；`fd_and_offset` 把裸 fd 借給 C++ 做 io_uring |
| `cluster.rs` | 176 | 單一 cluster 檔的雙 fd（`O_SYNC` + `O_DIRECT`）；依對齊狀況選 fd；`fallocate` 與 punch hole（`FALLOC_FL_KEEP_SIZE`）；`EINTR` 重試迴圈 |
| `fs_type.rs` | 33 | `statfs` 偵測 EXT4/NFS/XFS/ZFS；只有 ZFS 被判定不支援 O_DIRECT |

### `src/types/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `mod.rs` | 13 | 6 個子模組宣告 |
| `position.rs` | 117 | `Position(u64)`：24bit chunk_size + 8bit cluster + 24bit group + 8bit index 的位元打包；`offset()` 的一次乘法定址；`group_id()` 的無損還原 |
| `group_id.rs` | 114 | `GroupId(u64)`：32bit chunk_size + 24bit group + 8bit cluster；`plus_one()` 的 cluster-先進位；`offset()` 的運算子優先級陷阱 |
| `group_state.rs` | 163 | 256-bit（`[u64;4]`）槽位點陣圖；`NonZeroU64::trailing_zeros` 的快速找空位；`level()` 四桶分級；裸 32 byte 落盤 |
| `merge_state.rs` | 89 | bitset 的 acquire/release 差量集合；`merge()` 的互踢保證結合律；供 RocksDB MergeOperator 使用 |
| `chunk_meta.rs` | 84 | 落盤的 value 型別（11 個欄位，`#[repr(C)]` + derse）；`now()` 微秒時間戳；`set_default_etag_if_need` 用 checksum 當舊資料的 ETag |
| `constants.rs` | 8 | 4 個具名 chunk 大小 + `CHUNK_SIZE_SHIFT = 16` + `CHUNK_SIZE_NUMBER = 11` |

### `src/utils/`

| 檔案 | 行數 | 職責 |
|---|---|---|
| `mod.rs` | 15 | 7 個子模組宣告 |
| `size.rs` | 283 | `Size(u64)` newtype：`const fn` 建構、對 5 種整數型別的完整運算子多載（macro 生成）、`Display`（整除才用大單位）與 `Debug`（兩位小數）兩套格式 |
| `aligned.rs` | 57 | `AlignedBuffer`：4096 對齊的手動 alloc/dealloc；`is_aligned_buf`/`is_aligned_len`/`is_aligned_io` 三個 O_DIRECT 前置檢查 |
| `bytes.rs` | 1 | `pub type Bytes = tinyvec::TinyVec<[u8; 28]>`——28 byte 以內的 chunk_id 不進堆 |
| `result.rs` | 34 | 12 個變體的 `Error` 列舉；`Display` 委派給 `Debug`（跨界字串形如 `InvalidArg("…")`） |
| `shards_map.rs` | 153 | 64 路分片的 `HashMap`（const generic `S`）；分片目的是分攤 rehash 長尾，不含鎖 |
| `shards_set.rs` | 128 | 同上的 `HashSet` 版本 |
| `worker.rs` | 136 | 背景執行緒封裝：`WorkerState::{Continue, Pause, Wait(d), Stop}` 四態；`Condvar` + thread-local dummy `Mutex`；`stop_and_join` |

---

## 20. 收束

`chunk_engine` 的價值不在於它做了什麼複雜的事，而在於它**拒絕**做的事：

- 沒有 WAL——資料直接寫最終位置，靠「先寫資料、後宣告 meta」的順序和 writing log 取得可恢復性。
- 沒有變長分配——11 個 2 的冪分級把配置壓成一個 `tzcnt`。
- 沒有版本鏈或 GC——舊版本的生命週期就是 `Arc` 的生命週期。
- 沒有 async runtime——併發完全外包給 C++ 的執行緒池。
- 沒有 metadata 的自訂持久化——RocksDB 的 WriteBatch + MergeOperator 承擔全部原子性。

它把所有難的東西（原子性、崩潰一致性、併發控制）分別推給了三個成熟機制：RocksDB 的 WriteBatch、Rust 的 `Arc`、Linux 的 `fallocate`。剩下的自己寫的部分——位元打包、bitmap、五個集合的狀態機——都是可以在一頁紙上畫完的東西。這是它敢用 Rust 從零寫、而且只花七千行的原因。
