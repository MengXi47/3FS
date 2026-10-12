//! `Clusters`：一個 size class 底下 256 個 cluster 檔案的集合。
//!
//! 所有操作都只是用 `cluster()` 欄位當陣列索引，轉給對應的 `Cluster`：
//!
//! ```text
//!  <engine_path>/512KiB/          <- ClustersConfig.path (one size class)
//!    00  01  02  ...  3A  ...  FF    <- files[cluster], named "{:02X}"
//!                     ^
//!   GroupId(512KiB, cluster=0x3A, group=g)   -> allocate / deallocate (whole group)
//!   Position(512KiB, 0x3A, g, idx)           -> pread / pwrite / fd_and_offset
//! ```
//!
//! 每個 `Allocator`（`alloc/allocator.rs`）持有一個 `Clusters`，
//! 由 `Allocators::create` 以 `path.join(chunk_size.to_string())` 開啟。

use super::super::*;
use std::{fmt::Debug, os::fd::AsRawFd, path::PathBuf};

#[derive(Debug, Default)]
pub struct ClustersConfig {
    /// 這個 size class 的目錄，例如 `<engine_path>/512KiB`。
    pub path: PathBuf,
    pub chunk_size: Size,
    /// true 時建立目錄與檔案；false 時檔案必須已存在，否則 open 失敗。
    pub create: bool,
}

/// 一個 size class 的 256 個 cluster 檔案。
pub struct Clusters {
    pub path: PathBuf,
    pub chunk_size: Size,
    /// 長度固定 256，索引即 cluster 編號。
    files: Vec<Cluster>,
}

impl Clusters {
    /// 檔案數，與 `GroupId::COUNT` 相同（cluster 欄位佔 8 位）。
    const COUNT: u32 = 256;

    /// 開啟（或建立）256 個檔案，每個開兩個 fd，共 512 個 fd。
    ///
    /// 整個目錄只做一次檔案系統偵測，結果套用到所有檔案（ZFS 不用 O_DIRECT）。
    pub fn open(config: &ClustersConfig) -> Result<Self> {
        let mut files: Vec<Cluster> = vec![];

        if config.create {
            std::fs::create_dir_all(&config.path)
                .map_err(|e| Error::IoError(format!("create dir {:?} fail: {e:?}", config.path)))?;
        }

        let support_direct_io = FsType::check(&config.path).support_direct_io();
        // 檔名是兩位大寫十六進位：00, 01, ..., 0F, 10, ..., FF
        for cluster_id in 0..Self::COUNT {
            let file_path = config.path.join(format!("{:02X}", cluster_id));
            files.push(Cluster::open(&file_path, config.create, support_direct_io)?);
        }

        Ok(Clusters {
            path: config.path.clone(),
            chunk_size: config.chunk_size,
            files,
        })
    }

    /// fallocate 整個 group，讓之後寫進這個 group 的 chunk 不必再配置磁碟區塊。
    pub fn allocate(&self, group_id: GroupId) -> Result<()> {
        self.files[group_id.cluster() as usize].fallocate(group_id, false)
    }

    /// 對整個 group 打洞，把磁碟空間還給檔案系統。
    pub fn deallocate(&self, group_id: GroupId) -> Result<()> {
        self.files[group_id.cluster() as usize].fallocate(group_id, true)
    }

    pub fn pread(&self, pos: Position, buf: &mut [u8], offset: u32) -> Result<()> {
        self.files[pos.cluster() as usize].pread(pos, buf, offset)
    }

    pub fn pwrite(&self, pos: Position, buf: &[u8], offset: u32) -> Result<()> {
        self.files[pos.cluster() as usize].pwrite(pos, buf, offset)
    }

    /// 讀取路徑的出口：把 chunk 位址翻成 (direct_fd, chunk 起始 offset) 交給 C++。
    ///
    /// ```text
    ///  C++ BatchReadJob
    ///     -> engine.get_raw_chunk(chunk_id)      Arc<Chunk> keeps pos alive
    ///     -> chunk->fd_and_offset()              this fn
    ///     -> AIO read on direct_fd (O_DIRECT)    C++ side, no data copy in Rust
    ///     -> engine.release_raw_chunk(chunk)
    /// ```
    ///
    /// 回傳的 fd 由 `Cluster` 擁有，C++ 不可關閉；它在擁有這個 `Clusters` 的 `Allocator`
    /// 存活期間有效，而 `Chunk` 持有 `Arc<Allocator>`，所以握著 chunk 時 fd 一定有效。
    pub fn fd_and_offset(&self, pos: Position) -> FdAndOffset {
        FdAndOffset {
            fd: self.files[pos.cluster() as usize].direct_fd.as_raw_fd(),
            offset: pos.offset().into(),
        }
    }
}

/// 只印路徑，不印 256 個檔案。
impl Debug for Clusters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clusters")
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 同一檔案上配置 group 0 與 group 3，檔案長度依最大 group 成長為 4 個 group；
    // 打洞後長度不變；在 /proc 下建立目錄必失敗
    #[test]
    fn test_clusters() {
        let dir = tempfile::tempdir().unwrap();

        let config = ClustersConfig {
            path: dir.path().into(),
            chunk_size: CHUNK_SIZE_NORMAL,
            create: true,
        };

        let clusters = Clusters::open(&config).unwrap();

        let group_id = GroupId::new(CHUNK_SIZE_NORMAL, 0, 0);
        let cluster = &clusters.files[0];
        let meta = cluster.normal_fd.metadata().unwrap();
        assert_eq!(meta.len(), 0);

        clusters.allocate(group_id).unwrap();
        let meta = cluster.normal_fd.metadata().unwrap();
        assert_eq!(meta.len(), group_id.size());

        let group_id_3 = GroupId::new(CHUNK_SIZE_NORMAL, 0, 3);
        clusters.allocate(group_id_3).unwrap();
        let meta = cluster.normal_fd.metadata().unwrap();
        assert_eq!(meta.len(), group_id.size() * 4);

        clusters.deallocate(group_id).unwrap();
        let meta = cluster.normal_fd.metadata().unwrap();
        assert_eq!(meta.len(), group_id.size() * 4);

        clusters.deallocate(group_id_3).unwrap();
        let meta = cluster.normal_fd.metadata().unwrap();
        assert_eq!(meta.len(), group_id.size() * 4);

        let config = ClustersConfig {
            path: std::path::Path::new("/proc/test").into(),
            chunk_size: CHUNK_SIZE_NORMAL,
            create: true,
        };
        assert!(Clusters::open(&config).is_err());
    }
}
