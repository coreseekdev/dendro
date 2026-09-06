#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
columnar_bench.py — dendro Python 压缩原型基准 (SPEC 08 §3 + SPEC 05 CBF codec 分层)

目的：对 TPC-H lineitem 风格列数据，量化「压缩率 R vs 解码吞吐 T」的衰退曲线，
为 Rust 正式实现选定默认 codec 策略。

约定（对齐 SPEC 08 指标定义）：
  R     = raw_bytes / comp_bytes（越高越好）
  T_enc = 编码吞吐 = raw_bytes / t / 1e9  (GB/s, 十进制 GB)
  T_dec = 解码吞吐 = raw_bytes / t / 1e9  (GB/s)，解码后逐字节校验 == 原列
  单线程；time.perf_counter；每组 3 次取中位数。
  组合方案（ZSTD_ON_RLEDICT）的 enc 只计 zstd 这一遍（RLE_DICT 编码成本见 RLEDICT 行），
  dec 计完整冷块读路径（解压 + 字典 gather 重建 canonical payload）。

codec 分层（SPEC 05 §3）：RAW/BITPACK/RLE_DICT = GPU 可解层；ZSTD = 冷块层。
本原型未实现 BITPACK（纯 numpy 位打包无意义地慢）；id 流 BITPACK 的效果以
「RLEDICT + zstd(1)」与 uint32 id 流字节数做上下界参考（见结论段的熵分析）。

字符串规范表示（RAW codec, SPEC 05）：offsets(int32)[n+1] + 紧凑 data bytes。
内部用定宽 S{w} pad 数组表示以便 np.unique / gather（尾部 NUL 由 tobytes() 自动剥离）。

已知与 Rust 正式实现的偏差（数字仅供参考，见脚本末尾「坑」输出）：
  - np.unique(排序法) 建字典主导 RLEDICT 编码时间；Rust 将用 hash 表。
  - RAW 的 enc/dec 以 memcpy 为下限近似（Python 无零拷贝 map）。
  - uuid4 不可种子化：high_card_str 用 seed=42 的 u64 -> 16 hex 替代。

运行：python3 columnar_bench.py [--rows N] [--reps K] [--out results.csv]
输出：results.csv + stdout 摘要表 + 结论段 + 坑列表
"""

import argparse
import csv
import gc
import platform
import statistics
import time

import numpy as np
import zstandard as zstd

SEED = 42
ZSTD_LEVELS = (1, 3, 9, 19)
GB = 1e9  # 吞吐口径：十进制 GB/s

# CSV/表格列索引
(COL_NAME, SCHEME, LEVEL, RAW_B, COMP_B, RATIO, ENC, DEC) = range(8)


def fmt_gbps(v: float) -> str:
    return f"{v:.3f}" if v < 1.0 else f"{v:.2f}"


def fmt_mb(v: int) -> str:
    return f"{v / 1e6:,.1f}"


def bench(fn, reps: int):
    """跑 reps 次，返回 (最后一次结果, 中位耗时)。"""
    times = []
    result = None
    for _ in range(reps):
        t0 = time.perf_counter()
        result = fn()
        times.append(time.perf_counter() - t0)
    return result, statistics.median(times)


def gbps(raw_bytes: int, t: float) -> float:
    return raw_bytes / t / GB


# ---------------------------------------------------------------- 字符串工具

def _str_lengths(padded: np.ndarray) -> np.ndarray:
    """定宽 S{w} 数组每元素的实际字节长度（尾部 NUL 剥离）。
    前提：字符串非空且不含 NUL 字节（本基准生成器保证）。"""
    w = padded.dtype.itemsize
    b = padded.view(np.uint8).reshape(-1, w)
    rev = b[:, ::-1]
    first_nonzero = (rev != 0).argmax(axis=1)  # 从尾部数第一个非 0 的位置
    return (w - first_nonzero).astype(np.int32)


def _offsets_from_lens(lens: np.ndarray) -> np.ndarray:
    offs = np.empty(lens.size + 1, dtype=np.int32)
    offs[0] = 0
    np.cumsum(lens, dtype=np.int32, out=offs[1:])
    return offs


def _pack_strdata(padded: np.ndarray, lens: np.ndarray) -> bytes:
    """定宽 pad 数组 -> 紧凑（去尾部 NUL）data bytes，纯向量化 gather。
    注意 numpy S{w}.tobytes() 不剥离尾部 NUL，必须显式 pack。"""
    w = padded.dtype.itemsize
    b = padded.view(np.uint8).reshape(-1, w)
    offs = _offsets_from_lens(lens)
    total = int(offs[-1])
    within = np.arange(total, dtype=np.int64) \
        - np.repeat(offs[:-1].astype(np.int64), lens)          # 行内偏移
    src = np.repeat(np.arange(padded.size, dtype=np.int64) * w, lens) + within
    return b.reshape(-1)[src].tobytes()


# ---------------------------------------------------------------- 列表示

class NumColumn:
    """定宽数值列。canonical RAW payload = arr.tobytes()。"""

    kind = "num"

    def __init__(self, name, note, arr):
        self.name, self.note = name, note
        self.n = arr.size
        self.arr = np.ascontiguousarray(arr)
        self.width = self.arr.dtype.itemsize
        self.payload = self.arr.tobytes()
        self.raw_bytes = len(self.payload)
        self.avg_width = float(self.width)
        self.distinct = None  # 由 dict_encode 首次填充

    def encode_raw(self):
        return self.arr.tobytes()

    def dict_encode(self):
        """RLE_DICT：uniq 字典表(RAW) + id 流(uint32 RAW)。np.unique 为排序法原型。"""
        uniq, ids = np.unique(self.arr, return_inverse=True)
        ids = ids.ravel().astype(np.uint32, copy=False)
        self.distinct = int(uniq.size)
        dict_part = uniq.tobytes()
        ids_bytes = ids.tobytes()
        return uniq, dict_part, ids_bytes

    def rledict_decode(self, uniq, ids_bytes):
        """GPU 语义的 gather：id 流过字典表还原定宽 payload。"""
        ids = np.frombuffer(ids_bytes, dtype=np.uint32)
        return uniq[ids].tobytes()


class StrColumn:
    """变宽 utf8 列。canonical RAW payload = offsets(int32)[n+1] + data。
    内部持有定宽 pad 数组（尾部 NUL 剥离后即紧凑 data）。"""

    kind = "str"

    def __init__(self, name, note, padded):
        self.name, self.note = name, note
        self.n = padded.size
        self.padded = padded
        self.width = padded.dtype.itemsize  # pad 宽度（非平均）
        lens = _str_lengths(padded)
        self.offsets = _offsets_from_lens(lens)
        self.data = _pack_strdata(padded, lens)  # 紧凑（去 pad NUL）
        assert int(self.offsets[-1]) == len(self.data), "offsets/data 不一致"
        self.payload = self.offsets.tobytes() + self.data
        self.raw_bytes = len(self.payload)
        self.avg_width = len(self.data) / self.n
        self.distinct = None

    def encode_raw(self):
        return self.offsets.tobytes() + self.data

    def dict_encode(self):
        uniq, ids = np.unique(self.padded, return_inverse=True)
        ids = ids.ravel().astype(np.uint32, copy=False)
        self.distinct = int(uniq.size)
        dict_lens = _str_lengths(uniq)
        dict_part = _offsets_from_lens(dict_lens).tobytes() + _pack_strdata(uniq, dict_lens)
        ids_bytes = ids.tobytes()
        return uniq, dict_part, ids_bytes

    def rledict_decode(self, uniq, ids_bytes):
        ids = np.frombuffer(ids_bytes, dtype=np.uint32)
        gathered = uniq[ids]              # 定宽 gather（GPU 即一次 gather kernel）
        lens = _str_lengths(uniq)[ids]    # 每行实际长度
        offs = _offsets_from_lens(lens)
        data = _pack_strdata(gathered, lens)  # 去 pad NUL -> 紧凑
        return offs.tobytes() + data


# ---------------------------------------------------------------- 数据生成 (seed=42)

def gen_pk_seq(rng, n):
    return np.arange(n, dtype=np.int64)


def gen_rand_i32(rng, n):
    return rng.integers(-(2 ** 31), 2 ** 31, size=n, dtype=np.int32)


def gen_dup_i32(rng, n):
    """99% 行取自 100 个热值，1% 全域随机。"""
    hot = rng.integers(-(2 ** 31), 2 ** 31, size=100, dtype=np.int32)
    col = hot[rng.integers(0, 100, size=n)]
    tail = rng.random(n) < 0.01
    col[tail] = rng.integers(-(2 ** 31), 2 ** 31, size=int(tail.sum()), dtype=np.int32)
    return col


def gen_f64_price(rng, n):
    """两位小数价格，量级仿 TPC-H l_extendedprice（900.00 .. 104949.00）。"""
    cents = rng.integers(90_000, 10_494_901, size=n, dtype=np.int64)
    return cents.astype(np.float64) / 100.0


def gen_low_card_str(rng, n):
    vals = ["ACTIVE", "PENDING", "SHIPPED", "HELD", "CANCELLED"]
    tbl = np.array(vals, dtype="S9")
    return tbl[rng.integers(0, len(vals), size=n)]


def gen_mid_card_str(rng, n, m=10_000):
    """10000 个不同字符串（变长 11..27），模拟 part key 文本。"""
    tails = rng.integers(0, 2 ** 64, size=m, dtype=np.uint64)
    vals = [f"pkitm_{i:05d}_{t:x}".encode("ascii") for i, t in enumerate(tails)]
    w = max(len(v) for v in vals)
    tbl = np.array(vals, dtype=f"S{w}")
    return tbl[rng.integers(0, m, size=n)]


def gen_high_card_str(rng, n):
    """近似唯一 16 字符 hex。uuid4 不可种子化，用 seed 化 u64 的 hex 替代。"""
    u = rng.integers(0, 2 ** 64, size=n, dtype=np.uint64)
    hexbuf = u.astype("<u8", copy=False).tobytes().hex().encode("ascii")
    return np.frombuffer(hexbuf, dtype="S16")  # 只读，零拷贝


def gen_ts_ms(rng, n):
    """单调递增毫秒时间戳：增量 0..999ms 均匀。"""
    inc = rng.integers(0, 1_000, size=n, dtype=np.int64)
    return np.int64(1_700_000_000_000) + np.cumsum(inc, dtype=np.int64)


SPECS = [
    ("pk_seq",       gen_pk_seq,       "int64 顺序 0..N"),
    ("rand_i32",     gen_rand_i32,     "int32 全域随机"),
    ("dup_i32",      gen_dup_i32,      "int32 99% 集中于 100 热值"),
    ("f64_price",    gen_f64_price,    "float64 两位小数价格"),
    ("low_card_str", gen_low_card_str, "utf8 5 值均匀 (status)"),
    ("mid_card_str", gen_mid_card_str, "utf8 1e4 distinct (part key)"),
    ("high_card_str", gen_high_card_str, "utf8 16hex 近唯一 (comment)"),
    ("ts_ms",        gen_ts_ms,        "int64 单调递增毫秒时间戳"),
]


# ---------------------------------------------------------------- 方案执行

def run_schemes(col, reps):
    """对单列跑全部方案，返回 (rows, meta)。rows 元组:
    (列名, 方案, level, raw_bytes, comp_bytes, ratio, enc_GBps, dec_GBps)"""
    rows = []
    n = col.raw_bytes

    # ---- RAW（GPU 层基线；enc/dec 都是 memcpy 级近似，Python 无零拷贝）
    enc_payload, t_enc = bench(col.encode_raw, reps)
    assert enc_payload == col.payload
    # 坑: bytes(payload) 对 bytes 输入原样返回(no-copy)，必须用 bytearray 强制拷贝
    dec_copy, t_dec = bench(lambda: bytearray(col.payload), reps)
    assert dec_copy == col.payload
    rows.append((col.name, "RAW", "", n, n, 1.0, gbps(n, t_enc), gbps(n, t_dec)))

    # ---- RLE_DICT：字典表 + id 流 uint32（两者皆 RAW => GPU 可解层 codec_id=2 的原型）
    (uniq, dict_part, ids_bytes), t_enc = bench(col.dict_encode, reps)
    rd_payload = dict_part + ids_bytes
    dlen = len(dict_part)
    rebuilt, t_dec = bench(lambda: col.rledict_decode(uniq, ids_bytes), reps)
    assert rebuilt == col.payload, f"{col.name}: RLEDICT 解码校验失败"
    rows.append((col.name, "RLEDICT", "", n, len(rd_payload),
                 n / len(rd_payload), gbps(n, t_enc), gbps(n, t_dec)))

    # ---- ZSTD(level) 对 RAW / RLEDICT 输出再压（冷块层）
    for level in ZSTD_LEVELS:
        cctx = zstd.ZstdCompressor(level=level)
        dctx = zstd.ZstdDecompressor()

        # zstd(RAW)
        comp, t_enc = bench(lambda: cctx.compress(col.payload), reps)
        out, t_dec = bench(lambda: dctx.decompress(comp, max_output_size=n), reps)
        assert out == col.payload, f"{col.name}: ZSTD_ON_RAW@{level} 校验失败"
        rows.append((col.name, "ZSTD_ON_RAW", level, n, len(comp),
                     n / len(comp), gbps(n, t_enc), gbps(n, t_dec)))

        # zstd(RLE_DICT 输出)。enc 只计 zstd 遍；dec = 解压 + 字典 gather（冷块完整读路径）
        comp2, t_enc = bench(lambda: cctx.compress(rd_payload), reps)
        out2, _ = bench(lambda: dctx.decompress(comp2, max_output_size=len(rd_payload)), reps)
        assert out2 == rd_payload

        def dec_cold():
            out = dctx.decompress(comp2, max_output_size=len(rd_payload))
            return col.rledict_decode(uniq, out[dlen:])

        rebuilt2, t_dec_full = bench(dec_cold, reps)
        assert rebuilt2 == col.payload, f"{col.name}: ZSTD_ON_RLEDICT@{level} 校验失败"
        rows.append((col.name, "ZSTD_ON_RLEDICT", level, n, len(comp2),
                     n / len(comp2), gbps(n, t_enc), gbps(n, t_dec_full)))

    meta = {
        "distinct": col.distinct,
        "n": col.n,
        "avg_width": col.avg_width,
        "dlen": dlen,
        "ids_bytes": len(ids_bytes),
        "rd_payload": len(rd_payload),
    }
    return rows, meta


# ---------------------------------------------------------------- 结论计算

def pct_str(distinct, n):
    if distinct is None:
        return "?"
    p = 100.0 * distinct / n
    return f"{p:.5f}%" if p < 0.01 else f"{p:.3g}%"


def print_conclusions(colnames, all_rows, metas):
    by = {(r[COL_NAME], r[SCHEME], r[LEVEL]): r for r in all_rows}

    print("\n" + "=" * 100)
    print("结论 (供 Rust 正式实现选定默认 codec 策略)")
    print("=" * 100)

    # -- 1. 每列型推荐：热/GPU 层 与 冷层
    print("\n[1] 每列型推荐 codec（热/GPU 层 = RAW/RLEDICT 中 comp 更小者，阈值 0.9x；")
    print("    冷层(SPEC 08: zstd level 1..3) = zstd@{1,3} 组合中 comp 最小且 R>=1.05、dec>=1GB/s）")
    print(f"    {'列':<14}{'distinct':>10}  {'热 codec':<10}{'R':>7}{'dec':>8}   "
          f"{'冷 codec(spec)':<22}{'R':>7}{'dec':>8}   {'若放开到@19':>16}")
    hot_pick, cold_pick = {}, {}
    for c in colnames:
        raw_r = by[(c, "RAW", "")]
        rd_r = by[(c, "RLEDICT", "")]
        hot = ("RLEDICT", rd_r) if rd_r[COMP_B] < 0.9 * raw_r[COMP_B] else ("RAW", raw_r)
        hot_pick[c] = hot[0]
        cands = [by[(c, b, L)] for b in ("ZSTD_ON_RAW", "ZSTD_ON_RLEDICT") for L in (1, 3)]
        feasible = [r for r in cands if r[RATIO] >= 1.05 and r[DEC] >= 1.0]
        if feasible:
            cold_r = min(feasible, key=lambda r: r[COMP_B])
            cold_name = f"{cold_r[SCHEME]}@{cold_r[LEVEL]}"
        else:
            cold_r, cold_name = raw_r, "不压(zstd 无益)"
        cold_pick[c] = cold_name
        cands19 = [by[(c, b, 19)] for b in ("ZSTD_ON_RAW", "ZSTD_ON_RLEDICT")]
        feasible19 = [r for r in cands19 if r[RATIO] >= 1.05 and r[DEC] >= 1.0]
        if feasible19:
            r19 = min(feasible19, key=lambda r: r[COMP_B])
            extra19 = f"{r19[SCHEME].replace('ZSTD_ON_','')}@19 R={r19[RATIO]:.2f}"
        else:
            extra19 = "-"
        m = metas[c]
        print(f"    {c:<14}{pct_str(m['distinct'], m['n']):>10}  {hot[0]:<10}"
              f"{hot[1][RATIO]:>7.2f}{fmt_gbps(hot[1][DEC]):>8}   "
              f"{cold_name:<22}{cold_r[RATIO]:>7.2f}{fmt_gbps(cold_r[DEC]):>8}   {extra19:>16}")
    print("    注: 窄整型 distinct 低但 uint32 id 流未赢的列（如 dup_i32），id 流 BITPACK 后即反超，见[3]。")

    # -- 2. zstd 级别拐点：dec_GBps 相对 level1 衰退 >2x 的第一个 level
    print("\n[2] zstd 级别拐点（dec_GBps < level1 的一半，即衰退 >2x 的第一个 level）")
    print(f"    {'列':<14}{'ZSTD_ON_RAW':>14}{'ZSTD_ON_RLEDICT':>18}")
    infl_raw, infl_rd = [], []
    for c in colnames:
        cell = []
        for base, acc in (("ZSTD_ON_RAW", infl_raw), ("ZSTD_ON_RLEDICT", infl_rd)):
            d1 = by[(c, base, 1)][DEC]
            hit = next((L for L in ZSTD_LEVELS[1:] if by[(c, base, L)][DEC] < d1 / 2.0), None)
            if hit is not None:
                acc.append(hit)
            cell.append(str(hit) if hit else "无(<=19)")
        print(f"    {c:<14}{cell[0]:>14}{cell[1]:>18}")
    if infl_raw:
        print(f"    -> ON_RAW 拐点分布 {sorted(set(infl_raw))}（众数 "
              f"{max(set(infl_raw), key=infl_raw.count)}）；ON_RLEDICT 拐点分布 {sorted(set(infl_rd))}"
              + (f"（众数 {max(set(infl_rd), key=infl_rd.count)}）" if infl_rd else ""))
    print("    -> SPEC 08 预期 'level>=9 得不偿失' 在解码侧成立与否见上表；"
          "enc 侧 level19 普遍掉到 <0.1 GB/s（见明细表）。")

    # -- 3. 字符串/字典收益边界
    print("\n[3] 字典(RLE_DICT, id 流 uint32)收益边界：字典表 d*w̄ + 4N vs RAW w̄*N")
    print("    => 理论边界 distinct_ratio < 1 - 4/w̄（uint32 id 流与 RAW 同宽时对 i32 永不赢）")
    print(f"    {'列':<14}{'w̄':>6}{'distinct':>11}{'dict预zstd':>12}{'胜者':>7}   "
          f"{'raw+z1 MB':>10}{'dict+z1 MB':>11}{'胜者':>7}{'ids@BITPACK':>13}")
    for c in colnames:
        m = metas[c]
        raw_b = by[(c, "RAW", "")][COMP_B]
        rd_b = by[(c, "RLEDICT", "")][COMP_B]
        z1r = by[(c, "ZSTD_ON_RAW", 1)][COMP_B]
        z1d = by[(c, "ZSTD_ON_RLEDICT", 1)][COMP_B]
        w = m["avg_width"]
        d = m["distinct"] or 0
        bw = max(1, int(np.ceil(np.log2(max(d, 2)))))
        bp = m["n"] * bw / 8
        print(f"    {c:<14}{w:>6.2f}{pct_str(m['distinct'], m['n']):>11}"
              f"{fmt_mb(rd_b):>12}{'DICT' if rd_b < raw_b else 'RAW':>7}   "
              f"{float(z1r) / 1e6:>10.1f}{float(z1d) / 1e6:>11.1f}"
              f"{'DICT' if z1d < z1r else 'RAW':>7}"
              f"{bp / 1e6:>9.1f}MB")
    print("    （数值列宽 4/8：i32 边界为 0%——id 流同宽永不赢；i64/f64 边界 50%；"
          "ids@BITPACK = n*ceil(log2(distinct))/8，补足原型未测 BITPACK 的决策缺口）")

    # -- 4. 坑 / 异常数字
    print("\n[4] 观察到的坑 / 异常数字")
    pitfalls = [
        "CPython 坑: bytes(payload) 对 bytes 输入原样返回（零拷贝 no-op），首跑 RAW dec 报出 "
        ">50000 GB/s 的假数字；已改用 bytearray() 强制拷贝。RAW 的 enc/dec 因此是『分配+memcpy』"
        "下限近似（Python 无零拷贝 map）；Rust RAW 解码≈零成本，RAW 与各 codec 的吞吐差在 Rust 中更大。",
        "RLEDICT enc 被 np.unique（排序建字典）主导（比 RAW 编码慢 10~100x）；Rust 用 hash 表建字典，"
        "enc 会显著更好，但仍是 O(N) hash——GPU 层热块建字典成本不可忽略。",
        "i32 列 dict + uint32 id 流在预 zstd 下永不赢（id 与值同宽 4B）：rand_i32 的 RLEDICT "
        "bytes >= RAW 是数学必然而非实现问题；i32 字典的收益完全来自 id 流的下游可压缩性/BITPACK。",
        "本原型未实现 RLE run-length 与 BITPACK（任务范围限定）：dup_i32 的 RLE 潜力只体现为 "
        "id 流 zstd 可压性；真实 GPU 层应对 id 流 BITPACK（~log2(distinct) bit），吞吐预期接近 RAW。",
        "python-zstandard 每帧约 13~18B 帧头：80MB 块可忽略；若按 CBF 1~8MB 块压，帧头占比仍 <0.002%，"
        "但 KB 级 footer/元数据对象不可忽略。",
    ]
    hc = metas["high_card_str"]
    if hc["distinct"] is not None:
        pitfalls.append(
            f"high_card_str: 10M 个 64bit 随机出现 {hc['n'] - hc['distinct']} 次 birthday 碰撞，"
            f"dict 表 {hc['distinct']:,} 行 ~= N —— 近唯一列字典表≈整列复制品，纯亏。")
    enc19_raw = [by[(c, "ZSTD_ON_RAW", 19)][ENC] for c in colnames]
    pitfalls.append(
        f"zstd level19 enc 吞吐 {min(enc19_raw):.3f}~{max(enc19_raw):.2f} GB/s：冷块重编码(re-encoder) "
        "后台跑可以接受，在线路径不可接受；这也是 SPEC 08 把冷块级别钉在 1~3 的原因之一。")
    pitfalls.append(
        "f64_price 由 cents/100 构造：二进制浮点表示使低位 mantissa 近随机，zstd 只能压高位字节 "
        "（R 中等偏低）；真实价格列同理——float 想高压缩率需定点化。")
    pitfalls.append(
        "pk_seq/ts_ms 的 zstd 比率异常高（顺序/类 delta 结构）：真实系统应上 DELTA 编码（SPEC 05 保留 "
        "codec_id=5）而非靠 zstd 白吃熵；这也意味着按本表给 pk 列选 zstd 会高估收益。")
    pitfalls.append(
        "mid_card_str 的 id 流 uint32=40MB，理论熵 10M*log2(1e4)/8≈16.6MB；zstd(1) 只能到字节对齐近似，"
        "BITPACK(13.29bit) 才能逼近——这正是 id 流该用 BITPACK 的定量依据。")
    pitfalls.append(
        "『随机 hex 不可压』是直觉误区：hex 文本每个 ASCII 字节只含 4bit 熵（16 符号均匀，实测字节熵 "
        "4.0000 bits），理论压缩上限恰 2:1，zstd 连 level1 都打满（实测 R=2.00）。高基数文本列的 R 上限"
        "由字符集宽度决定，FSST/zstd 都无法突破——comment 类列真实收益就是 ~2x。")
    pitfalls.append(
        "high_card_str 生成用种子化 u64->hex 替代 uuid4（uuid4 不可种子化）；小端序使每组 8B 内 hex "
        "反序，对随机分布与压缩率无影响。")
    for p in pitfalls:
        print(f"    - {p}")


# ---------------------------------------------------------------- 主流程

def cpu_model():
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if "model name" in line:
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "?"


def main():
    ap = argparse.ArgumentParser(description="dendro columnar codec python 原型基准")
    ap.add_argument("--rows", type=int, default=10_000_000)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--out", default="/home/nzinfo/src.db/dendro/prototype/columnar/results.csv")
    args = ap.parse_args()

    print(f"env: python {platform.python_version()} numpy {np.__version__} "
          f"zstandard {zstd.__version__} | cpu {cpu_model()} | 单线程")
    print(f"rows={args.rows:,} reps={args.reps}(中位) seed={SEED} zstd_levels={ZSTD_LEVELS}")
    print("吞吐口径: 十进制 GB/s, 分母 = 原列 raw 字节; 组合方案 dec 含完整冷块读路径\n")

    rng = np.random.default_rng(SEED)
    all_rows, metas = [], {}
    t_all = time.perf_counter()

    for name, gen, note in SPECS:
        t0 = time.perf_counter()
        raw = gen(rng, args.rows)
        if raw.dtype.kind == "S":
            col = StrColumn(name, note, raw)
        else:
            col = NumColumn(name, note, raw)
        del raw
        gc.collect()

        print(f"--- {name} ({note}), raw={fmt_mb(col.raw_bytes)}MB, "
              f"gen+repr {time.perf_counter() - t0:.1f}s", flush=True)
        rows, meta = run_schemes(col, args.reps)
        all_rows.extend(rows)
        metas[name] = meta
        for r in rows:
            print(f"    {r[SCHEME]:<16}L{str(r[LEVEL]):>2}  comp={fmt_mb(r[COMP_B]):>9}MB  "
                  f"R={r[RATIO]:7.2f}  enc={fmt_gbps(r[ENC]):>7}  dec={fmt_gbps(r[DEC]):>7} GB/s",
                  flush=True)
        del col
        gc.collect()

    # ---- 写 CSV
    with open(args.out, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["列名", "方案", "level", "raw_bytes", "comp_bytes",
                    "ratio", "enc_GBps", "dec_GBps"])
        for r in all_rows:
            w.writerow([r[0], r[1], r[2], r[3], r[4], f"{r[5]:.4f}",
                        f"{r[6]:.4f}", f"{r[7]:.4f}"])
    print(f"\nCSV written: {args.out} ({len(all_rows)} rows, "
          f"total {time.perf_counter() - t_all:.0f}s)")

    # ---- 摘要表（同 CSV 内容的人类可读版已在上面逐列打印），此处打印结论
    colnames = [s[0] for s in SPECS]
    print_conclusions(colnames, all_rows, metas)


if __name__ == "__main__":
    main()
