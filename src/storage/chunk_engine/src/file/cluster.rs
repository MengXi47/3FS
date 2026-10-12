//! `Cluster`：一個 cluster 檔案（例如 `<engine_path>/512KiB/3A`）的 I/O。
//!
//! 同一個檔案開兩個 fd，依每次 I/O 是否 4KiB 對齊選用：
//!
//! ```text
//!                     pread / pwrite(pos, buf, offset)
//!                                  |
//!            buf ptr, buf len, offset all 4KiB aligned ?
//!                     |                          |
//!                    yes                         no
//!                     v                          v
//!              direct_fd (O_DIRECT)       normal_fd (O_SYNC)
//!              bypass page cache          via page cache, each write synchronous
//!              (O_SYNC instead on ZFS)
//!
//!  file offset = pos.offset() + offset     (chunk start + offset inside chunk)
//! ```
//!
//! 磁碟空間以 group 為單位管理：
//!
//! ```text
//!  fallocate(mode 0)                         allocate [group.offset, +group.size)
//!  fallocate(PUNCH_HOLE | KEEP_SIZE)         free the blocks, file size unchanged
//!
//!  file: | group 0 | group 1 (hole) | group 2 | group 3 | ...
//!                    ^ reads return zeros, no disk blocks
//! ```
//!
//! 這個檔案用到 `O_DIRECT`、`fallocate`、`FALLOC_FL_*`，只能在 Linux 上編譯。

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};

use super::super::*;

/// 回收 group 用的 fallocate mode：打洞釋放磁碟區塊，但保留檔案大小，
/// 後面 group 的 offset 因此不受影響。
const PUNCH_HOLE_FLAGS: i32 = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;

/// 一個 cluster 檔案；兩個 fd 指向同一個檔案，只差 open flags。
pub struct Cluster {
    /// O_SYNC：給未對齊的 I/O（例如追加寫的尾端、小於 4KiB 的讀）。
    pub normal_fd: File,
    /// O_DIRECT（ZFS 上改為 O_SYNC）：給 4KiB 對齊的 I/O、fallocate，
    /// 也經由 `Clusters::fd_and_offset` 交給 C++ 做 AIO 讀取。
    pub direct_fd: File,
}

impl Cluster {
    /// 開啟（或建立）檔案兩次。只有 normal_fd 帶 `create`，direct_fd 在它之後開啟，
    /// 此時檔案已存在。
    pub fn open(path: &Path, create: bool, support_direct_io: bool) -> Result<Self> {
        let normal_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .custom_flags(libc::O_SYNC)
            .open(path)
            .map_err(|err| Error::IoError(format!("open {:?} failed: {:?}", path, err)))?;

        let direct_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(if support_direct_io {
                libc::O_DIRECT
            } else {
                libc::O_SYNC
            })
            .open(path)
            .map_err(|err| Error::IoError(format!("open {:?} failed: {:?}", path, err)))?;

        Ok(Self {
            normal_fd,
            direct_fd,
        })
    }

    /// 配置（`punch_hole = false`）或回收（`true`）整個 group 的磁碟空間。
    ///
    /// 範圍是 `[group_id.offset(), group_id.offset() + group_id.size())`。
    /// mode 0 會在需要時把檔案延長到範圍結尾，所以檔案大小隨 group 編號成長。
    pub fn fallocate(&self, group_id: GroupId, punch_hole: bool) -> Result<()> {
        let res = unsafe {
            libc::fallocate(
                self.direct_fd.as_raw_fd(),
                if punch_hole { PUNCH_HOLE_FLAGS } else { 0 },
                group_id.offset().into(),
                group_id.size().into(),
            )
        };
        if res == -1 {
            Err(Error::IoError(format!(
                "fallocate {} error: {:?}",
                self.direct_fd.as_raw_fd(),
                std::io::Error::last_os_error()
            )))
        } else {
            Ok(())
        }
    }

    /// 從 chunk `pos` 內的 `offset` 讀滿 `buf`。讀到檔尾（回 0）視為錯誤，
    /// 例如讀取尚未 fallocate 的 group。
    pub fn pread(&self, pos: Position, mut buf: &mut [u8], offset: u32) -> Result<()> {
        // aligned 只在進迴圈前算一次（buf 指標、buf 長度、offset 都對齊 4KiB）。
        // 迴圈內只再檢查剩餘長度：若部分讀取的 n 不是 4KiB 倍數，剩下的長度也不對齊，
        // 自動改走 normal_fd。
        let aligned = is_aligned_io(buf, offset);
        let mut offset = pos.offset() + offset;
        while !buf.is_empty() {
            let fd = if aligned && is_aligned_len(buf.len() as u32) {
                &self.direct_fd
            } else {
                &self.normal_fd
            };

            match fd.read_at(buf, offset.into()) {
                Ok(0) => return Err(Error::IoError(format!("read {:?} return 0", fd))),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n;
                }
                Err(e) => Self::handle_error(e)?,
            }
        }
        Ok(())
    }

    /// 把 `buf` 整個寫到 chunk `pos` 內的 `offset`；fd 選擇規則同 `pread`。
    ///
    /// 不檢查是否超出 chunk 容量，由上層（`alloc/chunk.rs`）保證 `offset + len <= chunk_size`。
    pub fn pwrite(&self, pos: Position, mut buf: &[u8], offset: u32) -> Result<()> {
        let aligned = is_aligned_io(buf, offset);
        let mut offset = pos.offset() + offset;
        while !buf.is_empty() {
            let fd = if aligned && is_aligned_len(buf.len() as u32) {
                &self.direct_fd
            } else {
                &self.normal_fd
            };

            match fd.write_at(buf, offset.into()) {
                Ok(0) => return Err(Error::IoError(format!("write {:?} return 0", fd))),
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) => Self::handle_error(e)?,
            }
        }
        Ok(())
    }

    /// EINTR 視為可重試（回 Ok，讓迴圈再發一次），其餘錯誤轉成 `IoError`。
    fn handle_error(e: std::io::Error) -> Result<()> {
        if e.kind() == std::io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(Error::IoError(format!("io error: {:?}", e)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    // 三種 chunk 大小下：未 fallocate 時讀取失敗、fallocate 後檔案長度等於 group 大小、
    // 讀寫一致、打洞後長度不變；以及 /dev/null、無效 fd、EINTR 的錯誤處理
    #[test]
    fn test_cluster_open() {
        let dir = tempfile::tempdir().unwrap();
        let support_direct_io = FsType::check(&dir).support_direct_io();

        for chunk_size in [CHUNK_SIZE_NORMAL, CHUNK_SIZE_SMALL, CHUNK_SIZE_LARGE] {
            let file_path = dir.path().join(format!("test.cluster.{}", chunk_size));
            assert!(Cluster::open(&file_path, false, support_direct_io).is_err());

            let cluster = Cluster::open(&file_path, true, support_direct_io).unwrap();
            let meta = cluster.normal_fd.metadata().unwrap();
            assert_eq!(meta.len(), 0);

            let cluster = Cluster::open(&file_path, false, support_direct_io).unwrap();
            let group_id = GroupId::new(chunk_size, 0, 0);

            let mut buf = [0u8; 5];
            let pos = Position::new(group_id, 0);
            assert!(cluster.pread(pos, &mut buf, 0).is_err());

            cluster.fallocate(group_id, false).unwrap();
            let meta = cluster.normal_fd.metadata().unwrap();
            assert_eq!(meta.len(), group_id.size());

            let bytes = "hello world!".as_bytes();
            assert!(cluster.pwrite(pos, bytes, 0).is_ok());

            assert!(cluster.pread(pos, &mut buf, 0).is_ok());
            assert_eq!(&buf, &bytes[0..buf.len()]);

            cluster.fallocate(group_id, true).unwrap();
            let meta = cluster.normal_fd.metadata().unwrap();
            assert_eq!(meta.len(), group_id.size());
        }

        assert!(Cluster::open(Path::new("/dev/null"), false, support_direct_io).is_err());

        let cluster = Cluster {
            normal_fd: File::open("/dev/null").unwrap(),
            direct_fd: File::open("/dev/null").unwrap(),
        };
        assert!(cluster.fallocate(GroupId::default(), false).is_err());
        assert!(cluster.fallocate(GroupId::default(), true).is_err());
        assert!(cluster.pwrite(Position::from(0), &[1], 0).is_err());

        let cluster = Cluster {
            normal_fd: unsafe { File::from_raw_fd(23333) },
            direct_fd: unsafe { File::from_raw_fd(23333) },
        };
        let mut buf = [0u8; 32];
        assert!(cluster.pread(Position::from(0), &mut buf, 0).is_err());
        std::mem::forget(cluster);

        assert!(Cluster::handle_error(std::io::Error::from_raw_os_error(libc::EINTR)).is_ok());
    }
}
