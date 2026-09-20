#!/usr/bin/env python3
# -*- coding: utf-8 -*-
""" dendro 验证覆盖率报告（类似测试覆盖率的验证侧度量）。

三个维度的可计算定义（docs/VERIFICATION.md §5 度量章节的执行器）：

  [1] 不变式覆盖率   —— 业务路径承诺的分母是 I 系列不变式目录（非 LOC）：
      按检测机制最强者分类（Kani/Verus=形式化-代码级，TLC=形式化-设计级，
      opfuzz=fuzz，回归/✅=测试，⬜/无=零覆盖）。
  [2] vLOC           —— 代码级验证的行覆盖。分子=被证明直接覆盖的生产函数
      LOC（按绑定级分层：B2 真源编译 / B1 镜像）；分母=硬核（不变式族
      对应的实现文件集，见 MANIFEST）。manifest 里的函数名在源文件中
      找不到时警告退出——这是"证明对象存在性"的机械防线。
  [3] cover 路径覆盖 —— Kani cover! 语句数（声明的业务路径）与最近一次
      运行的 SATISFIED/UNSATISFIED（可选 --kani-log 解析运行输出）。

用法：
  python3 scripts/verif-coverage.py [--kani-log /tmp/verify_kani_full.txt]
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------- 覆盖范围声明
# 硬核分母：不变式族 → 实现文件（目录递归 *.rs）。修改须同 PR 更新
# docs/VERIFICATION.md §5。
HARD_CORE = {
    "I-A 提交管线": ["crates/dendro-core/src/pipeline.rs"],
    "I-B OCC/快照": ["crates/dendro-core/src/memtx.rs"],
    "I-C WAL/恢复": [
        "crates/dendro-core/src/wal.rs",
        "crates/dendro-core/src/wal/codec.rs",
        "crates/dendro-core/src/recovery.rs",
    ],
    "I-D 分支/合并": [
        "crates/dendro-core/src/versioned",
        "crates/dendro-core/src/prolly",
        "crates/dendro-core/src/objstore/manifest.rs",
    ],
    "I-F fencing": [
        "crates/dendro-core/src/objstore/fence.rs",
        "crates/dendro-core/src/objstore/cas.rs",
    ],
    "I-G GC": ["crates/dendro-core/src/versioned"],
    "I-H 列存一致": ["crates/dendro-columnar/src"],
}

# 代码级验证对象（绑定级 → 文件 → 被证明覆盖的函数）。B2 = 真源编译
# （dendro-kani #[path]）；B1 = 镜像副本（Verus 旧式）。
VERIFIED_CODE = {
    "B2 真源编译": [
        ("crates/dendro-core/src/wal/codec.rs",
         ["encode_frame", "encode_trailer", "next_frame", "new",
          "from_u16", "encode_txn", "decode_txn"]),
    ],
    "B1 镜像(Verus)": [
        ("verification/verus/order_domain.rs", []),
        ("verification/verus/selectivity_core.rs", []),
        ("verification/verus/norm_user.rs", []),
    ],
}

KANI_SRC = "verification/kani/src/lib.rs"


# ---------------------------------------------------------------- 工具函数
def repo_loc(p: Path) -> int:
    """非空非注释行数（粗口径，与 wc -l 的差异 <5%，只作分母）。"""
    n = 0
    in_block = False
    for line in p.read_text(encoding="utf-8", errors="replace").splitlines():
        s = line.strip()
        if in_block:
            if "*/" in s:
                in_block = False
            continue
        if not s or s.startswith("//"):
            if s.startswith("/*"):
                in_block = "*/" not in s
            continue
        n += 1
    return n


def fn_loc(src: str, name: str):
    """函数体行数：定位 `fn name`，从签名行数到配对大括号闭合。"""
    m = re.search(rf"\bfn\s+{re.escape(name)}\b", src)
    if not m:
        return None
    line0 = src[: m.start()].count("\n")
    depth, started = 0, False
    for i, ch in enumerate(src[m.start():], start=m.start()):
        if ch == "{":
            depth += 1
            started = True
        elif ch == "}":
            depth -= 1
            if started and depth == 0:
                return src[: i].count("\n") - line0 + 1
    return None


# ---------------------------------------------------------------- [1] 不变式
CLASSIFY = [  # (关键词, 类别) —— 顺序即优先级（最强者胜）
    ("Kani", "形式化-代码级"),
    ("Verus", "形式化-代码级"),
    ("TLC", "形式化-设计级"),
    ("opfuzz", "fuzz"),
    ("✅", "测试"),
    ("回归", "测试"),
    ("测试", "测试"),
]


def invariant_coverage():
    text = (ROOT / "docs/VERIFICATION.md").read_text(
        encoding="utf-8", errors="replace")
    rows, unparsed = [], 0
    for line in text.splitlines():
        if not line.startswith("| I-"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 4 or not re.match(r"I-[A-H]\d+", cells[0]):
            unparsed += 1
            continue
        detect = cells[-1]  # I-A/B/C/E 表 5 列、I-D/F/G/H 表 4 列，检测均在末列
        # ⬜ 后跟的括号内容是待办而非已有机制（如"零覆盖（opfuzz 扩档）"）
        if "⬜" in detect and not any(
                k in detect for k in ("✅", "TLC", "Kani", "Verus")):
            rows.append((cells[0], "无覆盖", detect))
            continue
        for kw, cls in CLASSIFY:
            if kw in detect:
                rows.append((cells[0], cls, detect))
                break
        else:
            rows.append((cells[0], "无覆盖", detect))
    from collections import Counter
    dist = Counter(cls for _, cls, _ in rows)
    return rows, dist, unparsed


# ---------------------------------------------------------------- [3] kani 日志
def kani_report(log_path):
    src = (ROOT / KANI_SRC).read_text(encoding="utf-8")
    declared = len(re.findall(r"kani::cover!", src))
    if not log_path:
        return {"declared": declared, "evaluated": None,
                "satisfied": None, "unsatisfied": None,
                "harnesses_ok": None, "harnesses_total": None}
    text = Path(log_path).read_text(encoding="utf-8", errors="replace")
    sat = len(re.findall(r"Status: SATISFIED", text))
    unsat = len(re.findall(r"Status: UNSATISFIED", text))
    # 逐 harness 运行的日志含多行 "N successfully verified ... M total"，累计
    ok_sum = sum(int(m.group(1)) for m in re.finditer(
        r"(\d+) successfully verified harnesses", text))
    tot_sum = sum(int(m.group(1)) for m in re.finditer(
        r"failures, (\d+) total", text))
    return {
        "declared": declared,
        "evaluated": sat + unsat,  # 门禁日志只含被跑的 harness
        "satisfied": sat,
        "unsatisfied": unsat,
        "harnesses_ok": ok_sum or None,
        "harnesses_total": tot_sum or None,
    }


# ---------------------------------------------------------------- main
def main():
    log = None
    if "--kani-log" in sys.argv:
        log = sys.argv[sys.argv.index("--kani-log") + 1]

    print("=== dendro 验证覆盖率报告（三维度，定义见 docs/VERIFICATION.md §5）===")

    # [1] 不变式覆盖率
    rows, dist, unparsed = invariant_coverage()
    total = len(rows)
    formal = dist.get("形式化-代码级", 0) + dist.get("形式化-设计级", 0)
    print(f"\n[1] 不变式覆盖率（业务路径承诺，分母=I 目录 {total} 条）")
    print(f"    形式化 {formal}/{total} ({100 * formal // max(total, 1)}%)"
          f"  = 代码级(Kani/Verus) {dist.get('形式化-代码级', 0)}"
          f" + 设计级(TLC) {dist.get('形式化-设计级', 0)}")
    rest = [f"{k} {v}" for k, v in dist.items() if "形式化" not in k]
    print(f"    其余：{'，'.join(rest) if rest else '—'}")
    if unparsed:
        print(f"    ⚠ {unparsed} 行未解析（表格格式漂移？）")

    # [2] vLOC
    print(f"\n[2] vLOC 代码级验证行覆盖")
    denom = 0
    seen = set()
    for fam, paths in HARD_CORE.items():
        for p in paths:
            full = ROOT / p
            files = sorted(full.rglob("*.rs")) if full.is_dir() else [full]
            for f in files:
                if f in seen:
                    continue
                seen.add(f)
                denom += repo_loc(f)
    missing = []
    for binding, entries in VERIFIED_CODE.items():
        for path, fns in entries:
            src_path = ROOT / path
            if fns:
                src = src_path.read_text(encoding="utf-8")
                got = [fn_loc(src, f) for f in fns]
                bad = [f for f, g in zip(fns, got) if g is None]
                missing += [(path, f) for f in bad]
                loc = sum(g for g in got if g)
                n = len([g for g in got if g])
                print(f"    {binding}: {path} {n} 函数 {loc} 行")
            else:
                print(f"    {binding}: {path} 整文件 {repo_loc(src_path)} 行"
                      f"（镜像口径）")
    print(f"    硬核分母 {denom} 行（{len(seen)} 文件，I-A..I-H 实现面）")
    if missing:
        for path, fn in missing:
            print(f"    ✗ {path}: 函数 {fn} 不存在——证明对象漂移，须修 manifest 或源码")
        sys.exit(1)

    # [3] cover
    kr = kani_report(log)
    print(f"\n[3] Kani cover 路径覆盖")
    if kr["satisfied"] is None:
        print(f"    声明 cover! {kr['declared']} 条（无运行日志，可达性未计）")
    else:
        print(f"    声明 {kr['declared']}（源码全量）/ 本次评估 "
              f"{kr['evaluated']}（门禁 harness 集）")
        print(f"    可达(SATISFIED) {kr['satisfied']} / "
              f"不可达(UNSATISFIED) {kr['unsatisfied']}"
              + ("  ⚠ 存在不可达路径，须解释" if kr["unsatisfied"] else ""))
        if kr["harnesses_total"]:
            print(f"    harness {kr['harnesses_ok']}/{kr['harnesses_total']} 通过")

    print()


if __name__ == "__main__":
    main()
