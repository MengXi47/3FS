#!/bin/bash

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
INC="$HERE/include"
HOST="${1:-10.0.0.10}"

mkdir -p "$INC"

echo "==> 從 $HOST 取 Linux uapi header"
rsync -a "$HOST:/usr/include/linux/"                  "$INC/linux/"
rsync -a "$HOST:/usr/include/asm-generic/"            "$INC/asm-generic/"
rsync -a "$HOST:/usr/include/x86_64-linux-gnu/asm/"   "$INC/asm/"
rsync -a "$HOST:/usr/include/infiniband/"             "$INC/infiniband/"
rsync -a "$HOST:/usr/include/rdma/"                   "$INC/rdma/"
scp -q "$HOST:/usr/include/libaio.h"                  "$INC/"
scp -q "$HOST:/usr/include/numa.h"                    "$INC/"
scp -q "$HOST:/usr/include/numaif.h"                  "$INC/"

echo "==> 從 $HOST 取 boost uuid（1.83）"
rsync -a "$HOST:/usr/include/boost/uuid/" "$INC/boost/uuid/"

echo "==> 取 libfuse 3.16.2 header"

mkdir -p "$INC/fuse3"
FUSE_BASE=https://raw.githubusercontent.com/libfuse/libfuse/fuse-3.16.2/include
for h in fuse.h fuse_common.h fuse_lowlevel.h fuse_opt.h fuse_log.h fuse_kernel.h cuse_lowlevel.h; do
  curl -sSfL --max-time 30 -o "$INC/fuse3/$h" "$FUSE_BASE/$h"
done

echo "==> 取 FoundationDB C binding header（7.1）"
mkdir -p "$INC/foundationdb"
FDB_BASE=https://raw.githubusercontent.com/apple/foundationdb/release-7.1/bindings/c/foundationdb
for h in fdb_c_types.h fdb_c.h fdb_c_internal.h; do
  curl -sSfL --max-time 30 -o "$INC/foundationdb/$h" "$FDB_BASE/$h"
done
curl -sSfL --max-time 30 -o "$HERE/.fdb.options" \
  https://raw.githubusercontent.com/apple/foundationdb/release-7.1/fdbclient/vexillographer/fdb.options
python3 "$HERE/gen_fdb_options.py" "$HERE/.fdb.options" "$INC/foundationdb/fdb_c_options.g.h"
rm -f "$HERE/.fdb.options"

echo "==> 取 Arrow / Parquet header"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
python3 -m pip download --no-deps --only-binary=:all: -q -d "$TMP" pyarrow
unzip -q "$TMP"/pyarrow-*.whl 'pyarrow/include/*' -d "$TMP/x"
rm -rf "$INC/arrow" "$INC/parquet"
cp -R "$TMP/x/pyarrow/include/arrow" "$TMP/x/pyarrow/include/parquet" "$INC/"

echo "==> 覆蓋手寫的 stub"
cp -R "$HERE/stub/." "$INC/"

echo "==> 產生 compile_commands.json"
python3 "$HERE/gen_compile_commands.py"

echo
echo "完成。在 VS Code 重新載入視窗，或執行 clangd: Restart language server。"
