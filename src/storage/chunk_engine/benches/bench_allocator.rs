//! criterion 微基準：量測 512KiB size class 的「配置一個 chunk 再立刻釋放」成本。
//!
//! 只測記憶體中的 bitset / 集合操作，不寫資料、不碰 RocksDB；group 事先用
//! `do_allocate_task(1, 1)` fallocate 好一個放進預備池，所以迴圈內不會走同步 fallocate。
//!
//! ```text
//!   allocate(true)  -> Chunk { pos }            rc = 1, bit set
//!   drop(Chunk)     -> dereference(pos)         rc = 0 -> deallocate, bit cleared
//!                      group becomes empty      -> back to allocated pool
//!   (repeat 65536 times per iteration)
//! ```
//!
//! 執行 (需 Linux)：
//!
//! ```text
//!   cargo bench --bench bench_allocator
//! ```

use chunk_engine::*;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::sync::Arc;

// 連續 n 次：配置一個 chunk 後立即 drop，觸發引用計數歸零與位置回收
fn allocate(allocator: &Arc<Allocator>, n: usize) {
    for _ in 0..n {
        drop(allocator.allocate(true).unwrap());
    }
}

fn criterion_benchmark(c: &mut Criterion) {
    // 1. 在暫存目錄建立 512KiB 的 256 個 cluster 檔與 RocksDB
    // 2. 載入 Allocator，預先 fallocate 一個 group 放進預備池
    // 3. 每輪 bench 配置 / 釋放 1 << 16 次
    let dir = tempfile::tempdir().unwrap();

    let cluster_config = ClustersConfig {
        path: dir.path().into(),
        chunk_size: CHUNK_SIZE_NORMAL,
        create: true,
    };
    let clusters = Clusters::open(&cluster_config).unwrap();

    let meta_store_config = MetaStoreConfig {
        rocksdb: RocksDBConfig {
            path: dir.path().join("meta"),
            create: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let meta_store = std::sync::Arc::new(MetaStore::open(&meta_store_config).unwrap());

    let allocator = chunk_engine::Allocator::load(clusters, meta_store.iterator()).unwrap();
    allocator.do_allocate_task(1, 1, &meta_store).unwrap();

    let count: usize = 1 << 16;

    c.bench_with_input(BenchmarkId::new("allocate", count), &count, |b, &c| {
        b.iter(|| allocate(&allocator, c))
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
