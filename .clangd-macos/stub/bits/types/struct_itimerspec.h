/* glibc 內部 header 的替身。macOS 沒有 POSIX timer，所以 struct itimerspec
 * 也不存在，這裡照 POSIX 的定義補上。
 */
#pragma once
#include <time.h>

#ifndef __itimerspec_defined
#define __itimerspec_defined 1
struct itimerspec {
  struct timespec it_interval;
  struct timespec it_value;
};
#endif
