/* epoll 的最小宣告，供 macOS 上的 clangd 解析用（macOS 沒有 epoll）。
 * 直接照抄 glibc 的 sys/epoll.h 會牽出一整串 bits/ 內部 header，所以只留
 * 3FS 實際會用到的型別與函式簽名。不可用於編譯。
 */
#ifndef _SYS_EPOLL_H
#define _SYS_EPOLL_H 1

#include <signal.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum EPOLL_EVENTS {
  EPOLLIN = 0x001,
  EPOLLPRI = 0x002,
  EPOLLOUT = 0x004,
  EPOLLRDNORM = 0x040,
  EPOLLRDBAND = 0x080,
  EPOLLWRNORM = 0x100,
  EPOLLWRBAND = 0x200,
  EPOLLMSG = 0x400,
  EPOLLERR = 0x008,
  EPOLLHUP = 0x010,
  EPOLLRDHUP = 0x2000,
  EPOLLEXCLUSIVE = 1u << 28,
  EPOLLWAKEUP = 1u << 29,
  EPOLLONESHOT = 1u << 30,
  EPOLLET = 1u << 31
};

#define EPOLL_CLOEXEC 02000000

#define EPOLL_CTL_ADD 1
#define EPOLL_CTL_DEL 2
#define EPOLL_CTL_MOD 3

typedef union epoll_data {
  void *ptr;
  int fd;
  uint32_t u32;
  uint64_t u64;
} epoll_data_t;

struct epoll_event {
  uint32_t events;
  epoll_data_t data;
};

int epoll_create(int __size);
int epoll_create1(int __flags);
int epoll_ctl(int __epfd, int __op, int __fd, struct epoll_event *__event);
int epoll_wait(int __epfd, struct epoll_event *__events, int __maxevents, int __timeout);
int epoll_pwait(int __epfd,
                struct epoll_event *__events,
                int __maxevents,
                int __timeout,
                const sigset_t *__ss);

#ifdef __cplusplus
}
#endif

#endif /* sys/epoll.h */
