#!/usr/bin/env python3
"""在 macOS 上產生 compile_commands.json，讓 clangd 能索引 3FS。

3FS 只能在 Linux 上真正編譯（liburing、RDMA、FoundationDB、fuse3），所以這裡
不跑 cmake，而是照 CMakeLists.txt 的 include 路徑與 -D 定義，替每個 .cc 合成一
筆編譯命令。目的只有一個：讓 clangd 建出索引，跳轉／補全／找引用能用。
產物不能拿來真的編譯。

用法：
    python3 .clangd-macos/gen_compile_commands.py
"""
import json
import os
import re
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SHIM = os.path.join(ROOT, ".clangd-macos", "include")
BREW = "/opt/homebrew"

# 掃描這些目錄下的原始檔
SOURCE_DIRS = ["src", "tests", "benchmarks"]
SOURCE_EXTS = (".cc", ".cpp", ".cxx")

# include 路徑，順序有意義：專案 > third_party > 補件目錄 > Homebrew。
#
# 注意 third_party/folly 刻意不列入：submodule 那份 folly 依賴 libstdc++ 內部符號
# （_Sp_counted_base）與 libc++ 已移除的 char_traits<unsigned char>，在 macOS 的
# libc++ 下每個檔案會多噴 60+ 個錯誤。改用 Homebrew 的 folly（本來就是為 macOS
# 編的），紅線幾乎清空；少數新版沒有的 header 由 sync_folly_shims() 轉發回
# submodule。代價是跳進 folly 內部時看到的是較新的版本，讀 3FS 自己的程式碼不受
# 影響。
INCLUDE_DIRS = [
    "src",
    ".",
    "target/cxxbridge",
    "third_party/fmt/include",
    "third_party/googletest/googletest/include",
    "third_party/googletest/googlemock/include",
    "third_party/rocksdb/include",
    "third_party/leveldb/include",
    "third_party/scnlib/include",
    "third_party/toml11",
    "third_party/liburing/src/include",
    "third_party/jemalloc/include",
    "third_party/mimalloc/include",
    "third_party/pybind11/include",
    "third_party/clickhouse-cpp",
    "third_party/clickhouse-cpp/contrib",
    "third_party/zstd/lib",
]

DEFINES = [
    "ROCKSDB_NAMESPACE=rocksdb_internal",
    "USE_STD_SHUFFLE",
    # 新版 glog/gflags 要求呼叫端宣告要用哪組 export macro
    "GLOG_USE_GLOG_EXPORT",
    "GLOG_USE_GLOG_EXPORT_H",
    # glibc 專屬型別，macOS 的 sys/resource.h 沒有
    "__rlimit_resource_t=int",
    # 新版 folly 已移除這個 macro，專案還在用
    "FOLLY_MAYBE_UNUSED=[[maybe_unused]]",
    # glibc 專屬的 rwlock 初始值，FairSharedMutex.h 沒有它會 #error
    "PTHREAD_RWLOCK_WRITER_NONRECURSIVE_INITIALIZER_NP={}",
]

EXTRA_FLAGS = [
    "-xc++",
    "-std=c++20",
    "-fsyntax-only",
    # CMakeLists.txt 在 clang 下會帶 -fcoroutines-ts，但 clang 16 之後已移除該旗標
    # （C++20 協程內建於 -std=c++20），這裡不能加，否則 driver 直接報錯。
    # 只是給 IDE 看的，不要因為 macOS/Linux 差異噴一整排紅線
    "-Wno-everything",
    "-ferror-limit=0",
    # Linux 專屬型別的補丁，見 include/macos_compat.h
    "-include",
    os.path.join(SHIM, "macos_compat.h"),
]

def clang_path():
    for candidate in (BREW + "/opt/llvm/bin/clang++", "clang++"):
        if os.path.isabs(candidate):
            if os.path.exists(candidate):
                return candidate
        elif shutil.which(candidate):
            return shutil.which(candidate)
    sys.exit("找不到 clang++")


def sysroot():
    """macOS SDK 路徑，讓 clangd 找得到系統 header。"""
    result = subprocess.run(["xcrun", "--show-sdk-path"], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else ""


def collect_sources():
    sources = []
    for base in SOURCE_DIRS:
        base_path = os.path.join(ROOT, base)
        if not os.path.isdir(base_path):
            continue
        for dirpath, dirnames, filenames in os.walk(base_path):
            dirnames[:] = [d for d in dirnames if d not in (".git", "build")]
            for name in filenames:
                if name.endswith(SOURCE_EXTS):
                    sources.append(os.path.join(dirpath, name))
    return sorted(sources)


def sync_stub():
    """把手寫的 stub 蓋到 include/。

    include/ 是 bootstrap.sh 的產物（不進版控），stub/ 才是手寫的來源；
    改完 stub 忘了同步的話會很難查，所以每次產生 CDB 都重蓋一次。
    """
    src = os.path.join(ROOT, ".clangd-macos", "stub")
    if not os.path.isdir(src) or not os.path.isdir(SHIM):
        return
    for dirpath, _, filenames in os.walk(src):
        rel = os.path.relpath(dirpath, src)
        dst_dir = os.path.join(SHIM, rel) if rel != "." else SHIM
        os.makedirs(dst_dir, exist_ok=True)
        for name in filenames:
            shutil.copy2(os.path.join(dirpath, name), os.path.join(dst_dir, name))


def sync_folly_shims():
    """替 Homebrew folly 沒有、但 submodule folly 有的 header 產生轉發檔。

    新版 folly 把 folly/experimental/* 搬走或刪掉了，3FS 還在 include 舊路徑。
    轉發過去的舊 header 自己又會 include 別的舊 header，所以要一路跟著補，
    直到收斂。
    """
    pattern = re.compile(r"<(folly/[A-Za-z0-9_/]+\.h)>")

    def folly_includes(path):
        with open(path, errors="ignore") as f:
            return pattern.findall(f.read())

    pending = set()
    for base in SOURCE_DIRS:
        for dirpath, _, filenames in os.walk(os.path.join(ROOT, base)):
            for name in filenames:
                if name.endswith(SOURCE_EXTS + (".h", ".hpp")):
                    pending.update(folly_includes(os.path.join(dirpath, name)))

    shim_root = os.path.join(SHIM, "folly")
    if os.path.isdir(shim_root):
        shutil.rmtree(shim_root)

    made = []
    seen = set()
    while pending:
        header = pending.pop()
        if header in seen:
            continue
        seen.add(header)
        if os.path.exists(os.path.join(BREW, "include", header)):
            continue
        vendored = os.path.join(ROOT, "third_party", "folly", header)
        if not os.path.exists(vendored):
            continue
        dst = os.path.join(SHIM, header)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        with open(dst, "w") as f:
            f.write("/* 自動產生：Homebrew 的 folly 沒有這個 header，轉回 submodule 那份。*/\n")
            f.write("#pragma once\n")
            f.write('#include "%s"\n' % vendored)
        made.append(header)
        pending.update(folly_includes(vendored))
    return sorted(made)


def main():
    compiler = clang_path()
    flags = list(EXTRA_FLAGS)

    sdk = sysroot()
    if sdk:
        flags += ["-isysroot", sdk]

    sync_stub()
    shims = sync_folly_shims()

    for d in INCLUDE_DIRS:
        flags.append("-I" + os.path.join(ROOT, d))
    # 補件目錄（bootstrap.sh 準備的）排在專案與 third_party 之後、Homebrew 之前：
    # 既不會蓋掉專案自己的 header，又能覆蓋 Homebrew 版本不相容的部分（boost/uuid）。
    flags.append("-I" + SHIM)
    flags.append("-I" + BREW + "/include")

    for d in DEFINES:
        flags.append("-D" + d)

    sources = collect_sources()
    db = [
        {
            "directory": ROOT,
            "file": src,
            "arguments": [compiler] + flags + [src],
        }
        for src in sources
    ]

    out = os.path.join(ROOT, "compile_commands.json")
    with open(out, "w") as f:
        json.dump(db, f, indent=2)

    print("已寫入 %s（%d 個原始檔）" % (out, len(db)))
    if shims:
        print("folly 轉發 shim：%s" % ", ".join(shims))
    if not os.path.isdir(os.path.join(SHIM, "linux")):
        print("警告：%s 看起來還沒準備好，先執行 .clangd-macos/bootstrap.sh" % SHIM)


if __name__ == "__main__":
    main()
