//! RocksDB 的薄包裝：統一錯誤型別、固定開啟選項、預建 sync / non-sync 兩組 WriteOptions，
//! 再加一個「依前綴迭代」的 iterator。key 格式與 merge 規則等業務語意都不在這裡，
//! 分別在 `meta/meta_key.rs` 與 `meta/meta_merge.rs`。
//!
//! ```text
//!   RocksDB::open::<T: MergeOp>(config)
//!     |
//!     +-- Options
//!     |     create_if_missing(config.create)
//!     |     merge operator "merge":
//!     |         full    -> T::full_merge(key, existing value, operands)
//!     |         partial -> T::partial_merge(key, operands)
//!     |     block-based table: bloom filter, 10 bits per key
//!     |
//!     +-- config.read_only ? DB::open_for_read_only : DB::open
//!     |
//!     +-- write_options[0] = default    (return without fsync)
//!         write_options[1] = sync=true  (fsync WAL before return)
//!
//!   put / delete / write (WriteBatch) all take `sync: bool`
//!   and index write_options[sync as usize]
//! ```

use crate::{Error, Result, Size};
use std::path::PathBuf;

/// `read_only` 給離線檢視工具用 (`examples/chunk_viewer.rs`) 與測試；engine 正式路徑
/// (`Engine::open`) 一律以可寫模式開啟。
#[derive(Debug, Default, Clone)]
pub struct RocksDBConfig {
    pub path: PathBuf,
    pub create: bool,
    pub read_only: bool,
}

/// 整個 engine 只有一個實例 (由 `MetaStore` 持有)，所有 size class 的元資料共用它。
pub struct RocksDB {
    db: rocksdb::DB,
    write_options: [rocksdb::WriteOptions; 2], // 0 for non-sync, 1 for sync.
}

/// RocksDB merge operator 回呼到業務邏輯的介面，實作是 `meta/meta_merge.rs` 的 `MetaMergeOp`。
///
/// 兩個方法都是沒有 `self` 的關聯函式，所以 `RocksDB::open` 用泛型 `T` 在編譯期綁定，
/// 閉包裡不需要捕捉任何物件。
///
/// ```text
///   one key:  put(v0)   merge(a)   merge(b)   merge(c)     get()
///                                                            |
///   full_merge(key, Some(v0), [a, b, c]) -> v1  <------------+  (read / compaction
///                                                                  reaches base value)
///   partial_merge(key, [a, b]) -> ab            (compaction may pre-combine operands
///                                                without the base value)
/// ```
///
/// 回傳 `None` 的意義：full_merge 回 None 表示合併失敗，讀這個 key 會得到錯誤
/// (見 test_rocksdb_invalid_merge)；partial_merge 回 None 表示無法預先合併，
/// RocksDB 會保留原運算元，留到之後的 full_merge 一起處理。
pub trait MergeOp {
    fn full_merge<'a>(
        key: &[u8],
        value: Option<&[u8]>,
        operands: impl Iterator<Item = &'a [u8]>,
    ) -> Option<Vec<u8>>;

    fn partial_merge<'a>(key: &[u8], operands: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>>;
}

impl RocksDB {
    pub fn open<T: MergeOp + 'static>(config: &RocksDBConfig) -> Result<Self> {
        // 1. 基本選項與 merge operator；partial 回呼的 _value 一律忽略 (部分合併沒有基底值)
        let mut db_options = rocksdb::Options::default();
        db_options.create_if_missing(config.create);
        db_options.set_merge_operator(
            "merge",
            |key, value, operands| T::full_merge(key, value, operands.iter()),
            |key, _value, operands| T::partial_merge(key, operands.iter()),
        );

        // 2. bloom filter 加速點查 (get chunk meta 查不到的情況可以免讀 SST block)
        let mut table_options = rocksdb::BlockBasedOptions::default();
        table_options.set_bloom_filter(10.0, true);
        db_options.set_block_based_table_factory(&table_options);

        // 3. 依設定以唯讀或可寫模式開啟
        let db = if config.read_only {
            rocksdb::DB::open_for_read_only(&db_options, &config.path, false)
        } else {
            rocksdb::DB::open(&db_options, &config.path)
        }
        .map_err(|err| Error::RocksDBError(format!("open rocksdb fail: {:?}", err)))?;

        // 4. 預建兩組 WriteOptions，寫入時用 `sync as usize` 直接索引，不必每次重建
        let mut sync_write_options = rocksdb::WriteOptions::new();
        sync_write_options.set_sync(true);

        Ok(Self {
            db,
            write_options: [rocksdb::WriteOptions::new(), sync_write_options],
        })
    }

    /// 用 `get_pinned` 避免把 value 複製出來；key 若有未合併的 merge 運算元，
    /// 會在這裡觸發 full_merge，合併失敗時回傳錯誤。
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<rocksdb::DBPinnableSlice>> {
        match self.db.get_pinned(key) {
            Ok(v) => Ok(v),
            Err(e) => Err(Error::RocksDBError(format!("RocksDB fail: {e:?}"))),
        }
    }

    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>, sync: bool) -> Result<()> {
        match self
            .db
            .put_opt(key, value, &self.write_options[sync as usize])
        {
            Ok(v) => Ok(v),
            Err(e) => Err(Error::RocksDBError(format!("RocksDB fail: {e:?}"))),
        }
    }

    pub fn delete(&self, key: impl AsRef<[u8]>, sync: bool) -> Result<()> {
        match self.db.delete_opt(key, &self.write_options[sync as usize]) {
            Ok(v) => Ok(v),
            Err(e) => Err(Error::RocksDBError(format!("RocksDB fail: {e:?}"))),
        }
    }

    pub fn new_write_batch() -> rocksdb::WriteBatch {
        rocksdb::WriteBatch::default()
    }

    /// 原子套用整個 WriteBatch：裡面的 put / delete / merge 要嘛全部生效、要嘛全部不生效。
    /// `MetaStore` 的每個 chunk 事件 (add / move / remove) 都靠這一點保持多個 key 一致。
    pub fn write(&self, batch: rocksdb::WriteBatch, sync: bool) -> Result<()> {
        match self.db.write_opt(batch, &self.write_options[sync as usize]) {
            Ok(v) => Ok(v),
            Err(e) => Err(Error::RocksDBError(format!("RocksDB fail: {e:?}"))),
        }
    }

    /// 4MiB readahead 對應這個 iterator 的主要用途：大範圍順序掃描，例如啟動時掃 group bits、
    /// compaction 掃整個 group 的 pos->chunk、prefix_len 變更時掃全部 chunk meta。
    pub fn new_iterator(&self) -> RocksDBIterator {
        let mut read_options = rocksdb::ReadOptions::default();
        read_options.set_readahead_size(Size::mebibyte(4).into());
        RocksDBIterator(self.db.raw_iterator_opt(read_options))
    }
}

impl Drop for RocksDB {
    fn drop(&mut self) {
        tracing::info!("RocksDB {:?} is closing...", self.db);
    }
}

/// 包住 `DBRawIterator`，生命週期綁在 `RocksDB` 上。
///
/// RocksDB iterator 建立時就固定了讀取視圖 (implicit snapshot)，之後的寫入看不到；
/// 要看到新資料得重新 `new_iterator()` (test_rocksdb_invalid_merge 就是 drop 後重建)。
pub struct RocksDBIterator<'a>(rocksdb::DBRawIterator<'a>);

impl RocksDBIterator<'_> {
    /// 依前綴迭代：seek 到第一個 >= prefix 的 key，對每個以 prefix 開頭的 key 呼叫 `func`，
    /// 遇到第一個不符的 key 就停；回傳處理筆數。
    ///
    /// ```text
    ///   sorted keys:  .. 01 ff | 02 00 .. | 02 00 .. | 02 ff .. | 03 00 .. ..
    ///                          ^ seek(02)                       ^ !starts_with(02): stop
    ///                          |<--- func(key, value) each ---->|
    /// ```
    ///
    /// - 空 prefix 會掃完整個 DB。
    /// - `func` 回 Err 會立刻中止並回傳該錯誤。
    /// - 迴圈結束後檢查 `status()`，底層錯誤 (例如 full_merge 失敗) 在這裡浮出。
    /// - 同一個 iterator 可以重複呼叫 `iterate`，每次都從新的 prefix 重新 seek。
    pub fn iterate<P, Fn>(&mut self, prefix: P, mut func: Fn) -> Result<usize>
    where
        P: AsRef<[u8]>,
        Fn: FnMut(&[u8], &[u8]) -> Result<()>,
    {
        let it = &mut self.0;
        it.seek(prefix.as_ref());
        let mut count = 0;
        while it.valid() && it.key().unwrap().starts_with(prefix.as_ref()) {
            func(it.key().unwrap(), it.value().unwrap_or(&[]))?;
            it.next();
            count += 1;
        }
        self.status()?;
        Ok(count)
    }

    /// 只定位不迭代，給需要自訂停止條件的呼叫端 (例如 `MetaStore::query_chunks_from_iterator`)。
    pub fn seek<P>(&mut self, prefix: P) -> Result<()>
    where
        P: AsRef<[u8]>,
    {
        self.0.seek(prefix.as_ref());
        self.status()
    }

    pub fn valid(&self) -> bool {
        self.0.valid()
    }

    pub fn status(&self) -> Result<()> {
        self.0
            .status()
            .map_err(|e| Error::RocksDBError(e.to_string()))
    }

    pub fn next(&mut self) {
        self.0.next();
    }

    pub fn key(&self) -> Option<&[u8]> {
        self.0.key()
    }

    pub fn value(&self) -> Option<&[u8]> {
        self.0.value()
    }
}

#[cfg(test)]
mod tests {
    // put/get、WriteBatch、TEST_KEY_PREFIX ('m') 的字串串接 merge、iterate 前綴語意、開啟失敗
    #[test]
    fn test_rocksdb_create_get_set() {
        use super::super::*;
        let dir = tempfile::tempdir().unwrap();

        let config = RocksDBConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        let rocksdb = RocksDB::open::<MetaMergeOp>(&config).unwrap();

        let value = rocksdb.get("merry".as_bytes()).unwrap();
        assert!(value.is_none());

        rocksdb
            .put("merry".as_bytes(), "world".as_bytes(), false)
            .unwrap();

        let value = rocksdb.get("merry".as_bytes()).unwrap();
        assert_eq!(value.as_deref(), Some("world".as_bytes()));

        let mut batch = RocksDB::new_write_batch();
        batch.put("merry", "RocksDB");
        batch.put("peace", "love");
        rocksdb.write(batch, false).unwrap();

        let value = rocksdb.get("merry".as_bytes()).unwrap();
        assert_eq!(value.as_deref(), Some("RocksDB".as_bytes()));
        let value = rocksdb.get("peace".as_bytes()).unwrap();
        assert_eq!(value.as_deref(), Some("love".as_bytes()));

        let mut batch = RocksDB::new_write_batch();
        batch.merge("merry", "1");
        batch.merge("merry", "2");
        for i in 0..16 {
            batch.merge("merge", format!("{i}"));
        }
        rocksdb.write(batch, false).unwrap();

        let value = rocksdb.get("merry".as_bytes()).unwrap();
        assert_eq!(value.as_deref(), Some("RocksDB12".as_bytes()));
        let value = rocksdb.get("merge".as_bytes()).unwrap();
        assert_eq!(value.as_deref(), Some("0123456789101112131415".as_bytes()));

        let mut it = rocksdb.new_iterator();
        let mut count = 0;
        let mut runner = |_: &[u8], _: &[u8]| {
            count += 1;
            crate::Result::Ok(())
        };

        assert_eq!(it.iterate([], &mut runner).unwrap(), 3);
        assert_eq!(it.iterate("m", &mut runner).unwrap(), 2);
        assert_eq!(it.iterate("a", &mut runner).unwrap(), 0);
        assert_eq!(it.iterate("z", &mut runner).unwrap(), 0);

        let config = RocksDBConfig {
            path: std::path::Path::new("/proc/test").into(),
            create: true,
            ..Default::default()
        };
        assert!(RocksDB::open::<MetaMergeOp>(&config).is_err());
    }

    // 16 條執行緒同時寫含 put 與 merge 的 WriteBatch，不應出錯
    #[test]
    fn test_rocksdb_parallel_write() {
        use super::super::*;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();

        let config = RocksDBConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        let rocksdb = Arc::new(RocksDB::open::<MetaMergeOp>(&config).unwrap());

        const T: usize = 16;
        const N: usize = 1000;
        let mut threads = vec![];
        for i in 0..T {
            let rocksdb = rocksdb.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("test-{i}"))
                    .spawn(move || {
                        for j in 0..N {
                            let value = [j as u8; 32];
                            let mut batch = RocksDB::new_write_batch();
                            batch.put(format!("a{}atesta", i * N + j), value);
                            batch.put(format!("b{}btestb", i * N + j), value);
                            batch.merge(format!("m{}mtestm", i * N + j), value);
                            rocksdb.write(batch, false).unwrap();
                        }
                    })
                    .unwrap(),
            )
        }

        for thread in threads {
            thread.join().unwrap();
        }
    }

    // 未知前綴 ('i') 的 merge 讓 full_merge 回 None：get / iterate / seek 都報錯；
    // put 蓋掉後、重建 iterator 才恢復正常；最後以 read_only 重開
    #[test]
    fn test_rocksdb_invalid_merge() {
        use super::super::*;
        let dir = tempfile::tempdir().unwrap();

        let config = RocksDBConfig {
            path: dir.path().into(),
            create: true,
            ..Default::default()
        };

        let rocksdb = RocksDB::open::<MetaMergeOp>(&config).unwrap();

        let mut batch = RocksDB::new_write_batch();
        batch.merge("invalid_merge", "");
        rocksdb.write(batch, false).unwrap();

        assert!(rocksdb.get("invalid_merge").is_err());

        let mut runner = |_: &[u8], _: &[u8]| crate::Result::Ok(());
        let mut it = rocksdb.new_iterator();
        assert!(it.iterate("invalid_merge", &mut runner).is_err());

        assert!(it.seek("invalid_merge").is_err());

        assert!(rocksdb.put("invalid_merge", "ok", false).is_ok());
        assert!(rocksdb.get("invalid_merge").is_ok());
        drop(it);

        let mut it = rocksdb.new_iterator();
        assert_eq!(it.iterate("invalid_merge", &mut runner), Ok(1));

        it.seek("invalid_merge").unwrap();
        assert!(it.valid());
        assert_eq!(it.key().unwrap(), "invalid_merge".as_bytes());
        assert_eq!(it.value().unwrap(), "ok".as_bytes());

        it.next();
        assert!(!it.valid());
        assert!(it.status().is_ok());

        drop(it);
        drop(rocksdb);
        let config = RocksDBConfig {
            path: dir.path().into(),
            create: false,
            read_only: true,
        };
        RocksDB::open::<MetaMergeOp>(&config).unwrap();
    }
}
