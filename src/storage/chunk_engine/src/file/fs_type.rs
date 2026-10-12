//! `FsType`：用 `statfs` 偵測目錄所在的檔案系統，目前只用來決定能不能開 O_DIRECT。
//!
//! ```text
//!  Clusters::open(dir)
//!     -> FsType::check(dir)  --statfs--> f_type magic
//!            EXT4 / XFS / NFS / OTHER  -> support_direct_io = true  -> direct_fd: O_DIRECT
//!            ZFS                       -> support_direct_io = false -> direct_fd: O_SYNC
//! ```
//!
//! `EXT4_SUPER_MAGIC` 等常數只在 Linux 的 libc 中定義。

use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

/// 偵測到的檔案系統種類；認不得或 `statfs` 失敗一律為 `OTHER`。
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum FsType {
    EXT4,
    NFS,
    XFS,
    ZFS,
    OTHER,
}

impl FsType {
    /// 對 `path` 呼叫 `statfs`，依 `f_type` 的 magic number 判斷。
    ///
    /// path 含 NUL 位元組時 `CString::new` 會 panic（unwrap）。
    pub fn check(path: impl AsRef<Path>) -> Self {
        let path_cstr = CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::statfs(path_cstr.as_ptr(), &mut stat) };
        if result != 0 {
            Self::OTHER
        } else {
            match stat.f_type {
                libc::EXT4_SUPER_MAGIC => Self::EXT4,
                libc::NFS_SUPER_MAGIC => Self::NFS,
                libc::XFS_SUPER_MAGIC => Self::XFS,
                0x2FC12FC1 => Self::ZFS, // https://github.com/openzfs/zfs/blob/33174af15112ed5c53299da2d28e763b0163f428/include/sys/fs/zfs.h#L1339
                _ => Self::OTHER,
            }
        }
    }

    /// 只排除 ZFS；包括 `statfs` 失敗的 `OTHER` 在內，其餘都當作支援 O_DIRECT。
    pub fn support_direct_io(&self) -> bool {
        !matches!(self, FsType::ZFS)
    }
}
