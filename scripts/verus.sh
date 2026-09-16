#!/usr/bin/env bash
# L3（Verus 函数级证明）运行器：verification/verus/*.rs 全量验证。
# 工具链：本机 verus-0.2026.09 + z3-4.16.0（verus 强版本匹配）；
# builtin/builtin_macros 经 --extern 显式注入（本发布布局不自动链接）。
set -euo pipefail
VDIR="${VERUS_HOME:-/home/nzinfo/tools/verus-0.2026.09.06.8dea4a2/verus-x86-linux}"
Z3DIR="${Z3_HOME:-/home/nzinfo/tools/z3-4.16.0/z3-4.16.0-x64-glibc-2.39}/bin"
cd "$(dirname "$0")/.."
STATUS=0
for f in verification/verus/*.rs; do
  echo "== $f"
  if ! PATH="$Z3DIR:$PATH" "$VDIR/verus" \
      --extern "builtin=$VDIR/libverus_builtin.rlib" \
      --extern "builtin_macros=$VDIR/libverus_builtin_macros.so" \
      "$f" 2>&1 | grep -E "^verification results"; then
    STATUS=1
  fi
done
exit $STATUS
