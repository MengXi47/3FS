#!/usr/bin/env python3
"""從 FoundationDB 的 fdb.options 產生 fdb_c_options.g.h。

FDB 的 C binding 會在編譯期用 vexillographer 產生這個 header，release tarball
之外拿不到。這裡照同樣的命名規則重建，讓 clangd 能解析 fdb_c.h。
"""
import sys
import xml.etree.ElementTree as ET

SCOPES = {
    "NetworkOption": ("FDBNetworkOption", "FDB_NET_OPTION"),
    "DatabaseOption": ("FDBDatabaseOption", "FDB_DB_OPTION"),
    "TransactionOption": ("FDBTransactionOption", "FDB_TR_OPTION"),
    "StreamingMode": ("FDBStreamingMode", "FDB_STREAMING_MODE"),
    "MutationType": ("FDBMutationType", "FDB_MUTATION_TYPE"),
    "ConflictRangeType": ("FDBConflictRangeType", "FDB_CONFLICT_RANGE_TYPE"),
    "ErrorPredicate": ("FDBErrorPredicate", "FDB_ERROR_PREDICATE"),
}


def main(src, dst):
    root = ET.parse(src).getroot()
    out = [
        "/* 由 fdb.options (FoundationDB release-7.1) 自動產生，僅供 clangd 解析用。*/",
        "#ifndef FDB_C_OPTIONS_G_H",
        "#define FDB_C_OPTIONS_G_H",
        "",
    ]
    for scope in root.iter("Scope"):
        entry = SCOPES.get(scope.get("name"))
        if entry is None:
            continue
        type_name, prefix = entry
        out.append("typedef enum {")
        for opt in scope.iter("Option"):
            if opt.get("hidden") == "true":
                continue
            name = opt.get("name").upper().replace("-", "_").replace(" ", "_")
            desc = (opt.get("description") or "").strip().replace("*/", "* /")
            if desc:
                out.append("    /* %s */" % desc.splitlines()[0])
            out.append("    %s_%s=%s," % (prefix, name, opt.get("code")))
        out.append("} %s;" % type_name)
        out.append("")
    out.append("#endif")
    with open(dst, "w") as f:
        f.write("\n".join(out) + "\n")
    print("已產生 %s" % dst)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
