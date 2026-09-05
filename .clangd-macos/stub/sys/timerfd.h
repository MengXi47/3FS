/* Linux timerfd 的最小宣告（macOS 沒有）。供 clangd 解析 IBSocket.cc /
 * IBDevice.cc 用，不可編譯。
 */
#ifndef _SYS_TIMERFD_H
#define _SYS_TIMERFD_H 1

#include <bits/types/struct_itimerspec.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

#define TFD_CLOEXEC 02000000
#define TFD_NONBLOCK 04000

#define TFD_TIMER_ABSTIME (1 << 0)
#define TFD_TIMER_CANCEL_ON_SET (1 << 1)

int timerfd_create(int __clock_id, int __flags);
int timerfd_settime(int __ufd, int __flags, const struct itimerspec *__utmr, struct itimerspec *__otmr);
int timerfd_gettime(int __ufd, struct itimerspec *__otmr);

#ifdef __cplusplus
}
#endif

#endif
