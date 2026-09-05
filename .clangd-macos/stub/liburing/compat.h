/* liburing 由 ./configure 生成的 compat.h，這裡照它在 Linux 上的分支重建一份，
 * 讓 clangd 解析 <liburing.h> 時不會斷在這裡。
 *
 * 對應 third_party/liburing/configure 中 __kernel_timespec / open_how 都存在
 * （我們補了真正的 Linux uapi header）的那條路徑。
 */
#ifndef LIBURING_COMPAT_H
#define LIBURING_COMPAT_H

#include <linux/fs.h> /* __kernel_rwf_t */
#include <linux/time_types.h>
/* <linux/time_types.h> is included above and not needed again */
#define UAPI_LINUX_IO_URING_H_SKIP_LINUX_TIME_TYPES_H 1

#include <linux/openat2.h>

#endif
