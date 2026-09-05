/* Linux 的 <sys/vfs.h> 只是 statfs/statvfs 的入口。3FS 實際用的是 POSIX 的
 * struct statvfs，macOS 由 <sys/statvfs.h> 提供。
 */
#pragma once
#include <sys/mount.h>
#include <sys/statvfs.h>
