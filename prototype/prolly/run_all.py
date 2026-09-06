"""跑全部原型测试 + 打印关键统计摘要。

用法 (二选一):
  cd /home/nzinfo/src.db/dendro && python3 -m prototype.prolly.run_all
  python3 /home/nzinfo/src.db/dendro/prototype/prolly/run_all.py
"""
from __future__ import annotations

import sys
import time
import traceback

if __package__ in (None, ""):
    import branch
    import hash
    import prolly
    import splitter
else:
    from . import branch, hash, prolly, splitter


def main() -> None:
    failed = 0
    print("=" * 72)
    print("dendro prolly 原型 — 单元测试")
    print("=" * 72)
    for mod in (hash, splitter, prolly, branch):
        for t in mod.TESTS:
            t0 = time.perf_counter()
            try:
                t()
            except Exception:
                failed += 1
                print(f"FAIL {mod.__name__}.{t.__name__}")
                traceback.print_exc()
            else:
                print(f"PASS {mod.__name__}.{t.__name__}  ({time.perf_counter() - t0:.2f}s)")
    if failed:
        print(f"\n{failed} 个测试失败, 跳过统计摘要")
        sys.exit(1)

    print()
    print(splitter.report())
    print()
    print(prolly.report())
    print()
    print(branch.report())
    print()
    print("=" * 72)
    print("ALL PASSED")


if __name__ == "__main__":
    main()
