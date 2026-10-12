//! 寫入吞吐量壓測：多執行緒持續建立新 chunk，每秒印出吞吐量與空間使用。
//!
//! 每個執行緒寫 `count` 個全新的 512KiB chunk (offset 0、整塊寫滿)，chunk_id 為
//! `(thread_index << 32) + n` 的 8-byte big-endian，所以各執行緒的 id 互不重疊。
//! 這條路徑會走 `Engine::write` -> `update_chunk` (new chunk) -> `commit_chunk`，
//! 同時用 `start_allocate_workers(2)` 在背景補 group。
//!
//! 執行 (需 Linux：用到 O_DIRECT / fallocate)：
//!
//! ```text
//!   cargo run --release --bin bench -- bench.toml
//!
//!   # bench.toml (all fields required)
//!   threads = 8
//!   count   = 10000
//!   level   = "info"          # info | debug | anything else = warn
//!
//!   [engine]
//!   path       = "/data/bench_engine"
//!   create     = true
//!   prefix_len = 4
//! ```

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use anyhow::{Context, Result};
use chunk_engine::*;
use serde::Deserialize;

/// TOML 設定檔內容；`engine` 直接對應 `EngineConfig` (path / create / prefix_len)。
#[derive(Debug, Default, Deserialize)]
struct Config {
    engine: EngineConfig,
    threads: usize,
    count: usize,
    level: String,
}

fn main() -> Result<()> {
    // 1. 讀第一個命令列參數作為設定檔路徑並解析 TOML
    let mut iter = std::env::args();
    iter.next();
    let config_path = iter
        .next()
        .ok_or(anyhow::anyhow!("get config path failed"))?;

    let content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to open config file {:?}", config_path))?;

    let config: Config = toml::from_str(&content)
        .with_context(|| format!("failed to parse config file {:?}", config_path))?;

    // 2. 設定 log 等級並輸出到 stdout
    let level = match config.level.as_str() {
        "info" => tracing::Level::INFO,
        "debug" => tracing::Level::DEBUG,
        _ => tracing::Level::WARN,
    };
    tracing_subscriber::fmt().with_max_level(level).init();
    tracing::info!("config content: {:#?}", config);

    // 3. 開啟引擎、啟動 2 個背景配置執行緒，稍等讓預備池先補一些 group
    let engine = chunk_engine::Engine::open(&config.engine).unwrap();
    engine.start_allocate_workers(2);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let bytes = Arc::new(AtomicUsize::default());
    let running = Arc::new(AtomicUsize::default());

    // 4. 每個執行緒準備一塊填滿 i 的 512KiB 對齊 buffer (checksum 只算一次)，連續寫新 chunk
    let threads = (0..config.threads)
        .map(|i| {
            let engine = engine.clone();
            let bytes = bytes.clone();
            let running = running.clone();

            let mut vec = create_aligned_buf(CHUNK_SIZE_NORMAL);
            vec.fill(i as u8);
            let checksum = crc32c::crc32c(&vec);
            running.fetch_add(1, Ordering::SeqCst);

            Ok(std::thread::spawn(move || {
                let mut chunk_id: usize = i << 32;
                for _ in 0..config.count {
                    engine
                        .write(&chunk_id.to_be_bytes(), &vec, 0, checksum)
                        .unwrap();
                    chunk_id += 1;
                    bytes.fetch_add(vec.len(), Ordering::SeqCst);
                }
                running.fetch_sub(1, Ordering::SeqCst);
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    // 5. 主執行緒每秒取走累計 bytes 印出吞吐量，直到所有寫入執行緒結束
    while running.load(Ordering::Acquire) > 0 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let bytes = bytes.swap(0, Ordering::Acquire);
        let used_size = engine.used_size();
        tracing::info!(
            "throughput: {:?}/s, allocated: {:?}, reserved: {:?}",
            Size::from(bytes),
            used_size.allocated_size,
            used_size.reserved_size,
        );
    }

    // 6. 收尾：join 寫入執行緒、停止背景配置執行緒，再跳過 meta_cache 的解構直接結束
    for thread in threads {
        thread.join().unwrap();
    }

    engine.stop_and_join();
    // SAFETY: This benchmark exits immediately after this call.
    unsafe { engine.speed_up_quit(); }
    Ok(())
}
