/* libfuse 建置時才生成的 header，這裡補一份最小版本供 clangd 解析。
 * fuse_lowlevel.h 只靠它判斷有沒有 versioned symbols。
 */
#ifndef LIBFUSE_CONFIG_H_
#define LIBFUSE_CONFIG_H_

#define LIBFUSE_BUILT_WITH_VERSIONED_SYMBOLS 1

#endif
