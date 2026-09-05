/* 由 gen_compile_commands.py 以 -include 強制引入。
 *
 * 這裡放 Linux/glibc 有、macOS 沒有的零星型別。只為了讓 clangd 解析得下去，
 * 不是相容層，不要拿來編譯。
 */
#pragma once

#ifdef __APPLE__

#include <sys/types.h>

/* SO_PEERCRED 傳回的憑證，glibc 定義在 <sys/socket.h>（_GNU_SOURCE）。
 * src/common/net/Transport.h 直接用 struct ucred。
 */
#ifndef __ucred_defined
#define __ucred_defined 1
struct ucred {
  pid_t pid;
  uid_t uid;
  gid_t gid;
};
#endif

/* struct stat 的納秒欄位：Linux 叫 st_atim，macOS 叫 st_atimespec。 */
#ifndef st_atim
#define st_atim st_atimespec
#define st_mtim st_mtimespec
#define st_ctim st_ctimespec
#endif

/* Linux 專屬的 open() 旗標，值取自 asm-generic/fcntl.h。macOS 用 F_NOCACHE
 * 達到類似效果，沒有對應的 open flag。
 */
#ifndef O_DIRECT
#define O_DIRECT 040000
#endif
#ifndef O_NOATIME
#define O_NOATIME 01000000
#endif
#ifndef O_TMPFILE
#define O_TMPFILE 020200000
#endif

/* glibc 在 <sys/socket.h> 提供，macOS 只有內部的 __DARWIN_ALIGN32。 */
#ifndef CMSG_ALIGN
#define CMSG_ALIGN(len) (((len) + sizeof(long) - 1) & ~(sizeof(long) - 1))
#endif

/* CPU affinity：glibc 的 <sched.h> 有，macOS 沒有。liburing.h 也會用到。 */
#ifndef CPU_SETSIZE
#define CPU_SETSIZE 1024
#define __NCPUBITS (8 * sizeof(unsigned long))
typedef struct {
  unsigned long __bits[CPU_SETSIZE / __NCPUBITS];
} cpu_set_t;
#define CPU_ZERO(s) __builtin_memset((s), 0, sizeof(cpu_set_t))
#define CPU_SET(c, s) ((s)->__bits[(c) / __NCPUBITS] |= (1UL << ((c) % __NCPUBITS)))
#define CPU_CLR(c, s) ((s)->__bits[(c) / __NCPUBITS] &= ~(1UL << ((c) % __NCPUBITS)))
#define CPU_ISSET(c, s) (((s)->__bits[(c) / __NCPUBITS] & (1UL << ((c) % __NCPUBITS))) != 0)
#endif

/* 在 Linux 上 folly 會間接把 gflags 帶進來，macOS 版的 folly 不會，
 * 於是 DECLARE_string 這類宏就找不到了。
 */
#include <gflags/gflags_declare.h>

#endif /* __APPLE__ */
