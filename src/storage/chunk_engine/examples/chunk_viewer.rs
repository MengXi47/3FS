//! 離線一致性檢查工具：以唯讀方式打開引擎的 RocksDB，比對「group bitset 記錄的已用 chunk 數」
//! 與「實際存在的 chunk meta 數」是否一致 (每個 size class 各比一次)。
//!
//! ```text
//!   RocksDB (read only)
//!     |
//!     +-- 0x02 group bits --> ChunkAllocator::load per size class (64KiB .. 64MiB)
//!     |                         used = allocated_chunks - reserved_chunks   (used_map)
//!     |
//!     +-- 0x01 chunk meta --> for each ChunkMeta:
//!                               allocator.reference(pos, true)  (panics if bit not set
//!                                                                or pos referenced twice)
//!                               real_map[chunk_size] += 1
//!
//!   assert_eq!(used_map, real_map)   -> mismatch means leaked or missing bits
//! ```
//!
//! 執行 (需 Linux；參數是 RocksDB 目錄，即引擎路徑下的 `meta/`)：
//!
//! ```text
//!   cargo run --release --example chunk_viewer -- <engine_path>/meta
//! ```

use chunk_engine::*;
use clap::Parser;
use derse::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};

// 注意：下一行的 `///` 是 clap 的 about 文字，內容與本工具無關 (疑似從別處複製)，
// 但改它會改變 `--help` 輸出，這裡保留原樣。
/// A distributed copy/move tool.
#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Path to rocksdb.
    pub path: PathBuf,
}

fn main() -> Result<()> {
    // 1. 以唯讀模式開 RocksDB；prefix_len 4 對應 C++ 端 sizeof(ChainId)
    let args = Args::parse();

    let meta_config = MetaStoreConfig {
        rocksdb: RocksDBConfig {
            path: args.path,
            create: false,
            read_only: true,
        },
        prefix_len: 4,
    };
    let meta_store = MetaStore::open(&meta_config)?;

    // 2. 對 11 個 size class 各載入一次 ChunkAllocator (只讀 0x02 group bits，不開資料檔)，
    //    記下 bitset 認定的已用數、空位數，以及 full / active group 數
    let mut chunk_allocators = HashMap::new();
    let mut used_map = BTreeMap::new();
    let mut reversed_map = BTreeMap::new();
    let mut group_count = BTreeMap::new();
    let mut chunk_size = CHUNK_SIZE_SMALL;
    let mut real_map = BTreeMap::new();
    loop {
        let counter = Arc::new(AllocatorCounter::new(chunk_size));
        let it = meta_store.iterator();
        let chunk_allocator = ChunkAllocator::load(it, counter.clone(), chunk_size)?;
        let allocated_chunks = counter.allocated_chunks();
        let reserved_chunks = counter.reserved_chunks();
        used_map.insert(chunk_size, allocated_chunks - reserved_chunks);
        reversed_map.insert(chunk_size, reserved_chunks);
        group_count.insert(
            chunk_size,
            (
                chunk_allocator.full_groups.len(),
                chunk_allocator.active_groups.len(),
            ),
        );
        real_map.insert(chunk_size, 0u64);
        chunk_allocators.insert(chunk_size, chunk_allocator);

        if chunk_size >= CHUNK_SIZE_ULTRA {
            break;
        }
        chunk_size *= 2;
    }

    // 3. 從 0x01 前綴開始走過全部 chunk meta：每個 pos 引用一次 (順便驗證 bit 有設、
    //    沒有兩個 chunk 指向同一位置)，並累計各 size class 的實際 chunk 數
    let mut it = meta_store.iterator();
    let end_key = MetaKey::chunk_meta_key_prefix();
    it.seek(&end_key)?;

    if it.key() == Some(end_key.as_ref()) {
        it.next(); // [begin, end)
    }

    loop {
        if !it.valid() {
            break;
        }

        if it.key().unwrap()[0] != MetaKey::CHUNK_META_KEY_PREFIX {
            break;
        }

        let chunk_meta =
            ChunkMeta::deserialize(it.value().unwrap()).map_err(Error::SerializationError)?;

        let chunk_size = chunk_meta.pos.chunk_size();
        let allocator = chunk_allocators.get_mut(&chunk_size).unwrap();
        allocator.reference(chunk_meta.pos, true);
        real_map.entry(chunk_size).and_modify(|v| *v += 1);

        it.next();
    }
    // 4. 印出統計並比對：bitset 的已用數必須等於實際 chunk 數
    //    (變數名 reversed_map 實為 reserved，即各 size class 的空位數)
    println!("{:#?}", used_map);
    println!("{:#?}", reversed_map);
    println!("{:#?}", group_count);
    assert_eq!(used_map, real_map);

    Ok(())
}
