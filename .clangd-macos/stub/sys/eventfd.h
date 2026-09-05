/* Linux eventfd 的最小宣告（macOS 沒有）。供 clangd 解析 EventLoop.cc 用。 */
#ifndef _SYS_EVENTFD_H
#define _SYS_EVENTFD_H 1

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef uint64_t eventfd_t;

#define EFD_SEMAPHORE 00000001
#define EFD_CLOEXEC 02000000
#define EFD_NONBLOCK 00004000

int eventfd(unsigned int __count, int __flags);
int eventfd_read(int __fd, eventfd_t *__value);
int eventfd_write(int __fd, eventfd_t __value);

#ifdef __cplusplus
}
#endif

#endif
