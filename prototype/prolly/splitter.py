"""keySplitter — 概率分裂 (内容定义分块, prolly tree 的核心)。

dolt 对应实现: go/store/prolly/tree/node_splitter.go
  - min = 512 / max = 16384 / target = 4096, weibullCheck(k=4)
  - salt(level) = sha512(uint64 little-endian(level))[:8]
    每层不同盐 ⇒ 同一数据在各层独立切分, 父子层边界不相关;
  - dolt 的哈希 = xxh3-32(key, seed=salt_u64)。纯 Python 标准库没有 xxh3,
    这里用 FNV-1a 32bit 并把 8 字节 salt 前置混入哈希链
    (等价于"按 level 换一个偏移基"), 统计均匀性足够验证算法;
    正式 Rust 实现必须用 xxh3-32: 与 dolt 字节兼容且快约两个数量级。

分裂判定 (增量形式, dolt weibullCheck 的逐条目离散化):
  size 进入 [MIN, MAX) 后, 对新追加条目:
      p = 1 - exp(-( Δ (size/λ)^k )),   Δ (size/λ)^k = (size/λ)^k - (prev/λ)^k
      split ⇔ H(key)/2^32 < p
  * size < MIN: 不判; size >= MAX: 强制分裂 (防碎片/巨块的双向夹逼)。
  * 这是 Weibull 的"条件风险"离散化: P(节点 > s) = Π(1-p_j)
    = exp(-Σ Δ(x_j)) = exp(-(s/λ)^k), 即节点大小精确服从
    Weibull(k=4, λ=4096): mean ≈ λ·Γ(1+1/k) ≈ 3712B, p50 ≈ 3737B。
    (若用无条件增量 F(s)-F(prev) 当作每条目概率, 尾部会指数化地堆积到
    MAX 强制分裂点 —— 实测 mean 会飘到 ~9000B, 是个真坑。)
  * 确定性与位置无关: 边界只依赖"层盐 + 已见内容" ⇒ 内容相同的子树切分相同
    ⇒ 地址相同 ⇒ 结构共享自动成立 (spec 03 §2.1)。
"""
from __future__ import annotations

import hashlib
import math
import random

MIN_SIZE = 512
MAX_SIZE = 16384
TARGET_SIZE = 4096.0
WEIBULL_K = 4.0

_FNV_OFFSET = 0x811C9DC5
_FNV_PRIME = 0x01000193
_U32 = 0xFFFFFFFF
_INV_2POW32 = 1.0 / 4294967296.0


def salt_for_level(level: int) -> bytes:
    """salt(level) = sha512(level as u64 little-endian)[:8] (dolt node_splitter.go)。"""
    return hashlib.sha512(level.to_bytes(8, "little")).digest()[:8]


def fnv1a32(data: bytes, salt: bytes = b"") -> int:
    """纯 FNV-1a 32bit (salt 前置混入)。保留为可独立测试的原语 (标准测试向量)。"""
    h = _FNV_OFFSET
    for b in salt:
        h = ((h ^ b) * _FNV_PRIME) & _U32
    for b in data:
        h = ((h ^ b) * _FNV_PRIME) & _U32
    return h


def _fmix32(h: int) -> int:
    """murmur3 的 fmix32 终结器 (强制雪崩)。"""
    h &= _U32
    h ^= h >> 16
    h = (h * 0x85EBCA6B) & _U32
    h ^= h >> 13
    h = (h * 0xC2B2AE35) & _U32
    h ^= h >> 16
    return h


def hash32(key: bytes, salt: bytes = b"") -> int:
    """分裂器条目哈希 = fmix32(FNV-1a(salt + key))。

    坑 (原型实测): 裸 FNV-1a 对"共享前缀的顺序键"(如自增整数的大端编码)
    有网格伪影 —— 同一 256 键前缀块内 h/2^32 沿 step=P/2^32≈0.0039 的等差格
    行走, 相邻键的分裂判定强相关, 实测节点均值从 ~3.7KB 膨胀到 ~6.8KB。
    fmix32 终结器恢复雪崩后与随机键统计一致。
    正式 Rust 实现用 xxh3-32(key, seed=salt_u64) (dolt node_splitter.go),
    天然无此问题且与 dolt 字节兼容。
    """
    return _fmix32(fnv1a32(key, salt))


def weibull_cdf(x: float) -> float:
    """Weibull(k=WEIBULL_K, λ=TARGET_SIZE) 的 CDF: 1 - exp(-(x/λ)^k)。"""
    if x <= 0:
        return 0.0
    return 1.0 - math.exp(-((x / TARGET_SIZE) ** WEIBULL_K))


def _split_probability(prev_size: float, size: float) -> float:
    """该条目的分裂概率 = 1 - exp(-Δ[(size/λ)^k]) (条件风险, 见模块 docstring)。"""
    if size <= prev_size:
        return 0.0
    dx = (size / TARGET_SIZE) ** WEIBULL_K - (prev_size / TARGET_SIZE) ** WEIBULL_K
    return 1.0 - math.exp(-dx)


class KeySplitter:
    """有状态分裂器: 逐条目 append, 返回是否应在该条目后切分节点。

    对应 dolt keySplitter: 追加条目后调 crossedBoundary()。
    状态只有 (size, prev_size), 切分后 reset ⇒ "位置无关、内容决定"。
    """

    __slots__ = ("level", "salt", "size", "_prev_size")

    def __init__(self, level: int = 0):
        self.level = level
        self.salt = salt_for_level(level)
        self.size = 0        # 当前缓冲区的编码字节数
        self._prev_size = 0  # 上一条目追加后的字节数 (CDF 增量的下端)

    def append(self, key: bytes, entry_size: int) -> bool:
        """追加一个条目 (entry_size = 其编码字节数), 返回是否越过边界。"""
        self._prev_size = self.size
        self.size += entry_size
        if self.size < MIN_SIZE:
            return False
        if self.size >= MAX_SIZE:
            return True
        h = hash32(key, self.salt)
        p = _split_probability(self._prev_size, self.size)
        return h * _INV_2POW32 < p

    def reset(self) -> None:
        self.size = 0
        self._prev_size = 0


# ---------------------------------------------------------------------------
# 节点大小分布统计
# ---------------------------------------------------------------------------

def synthetic_items(n: int, seed: int = 0x5EED, key_size: int = 16, value_size: int = 32):
    """合成 KV 流: key = uuid 形状的 16 随机字节, value = 32 随机字节。"""
    rng = random.Random(seed)
    for _ in range(n):
        yield rng.randbytes(key_size), rng.randbytes(value_size)


_KEY_HDR = 2  # u16 key_len
_VAL_HDR = 4  # u32 val_len


def node_size_distribution(n: int = 10_000, seed: int = 0x5EED, level: int = 0,
                           key_size: int = 16, value_size: int = 32) -> list[int]:
    """模拟叶层切分, 返回每个"节点"的编码字节数。

    条目编码大小 = 2B key_len + key + 4B val_len + value (与 prolly.encode_node 一致)。
    触发越界的条目计入该节点 (与 dolt chunker 的切法一致), 之后缓冲清零重来。
    """
    sizes: list[int] = []
    sp = KeySplitter(level)
    entry = _KEY_HDR + key_size + _VAL_HDR + value_size
    for key, _val in synthetic_items(n, seed, key_size, value_size):
        if sp.append(key, entry):
            sizes.append(sp.size)
            sp.reset()
    if sp.size:
        sizes.append(sp.size)
    return sizes


def summarize(sizes: list[int]) -> dict:
    ss = sorted(sizes)
    n = len(ss)

    def q(p):
        return ss[min(n - 1, int(p * n))] if n else 0

    return {
        "nodes": n,
        "min": ss[0] if n else 0,
        "p50": q(0.50),
        "mean": (sum(ss) / n) if n else 0.0,
        "p99": q(0.99),
        "max": ss[-1] if n else 0,
    }


def render_histogram(sizes: list[int], bucket: int = 1024) -> str:
    counts: dict[int, int] = {}
    for s in sizes:
        counts[s // bucket] = counts.get(s // bucket, 0) + 1
    if not counts:
        return "(空)"
    width = max(counts.values())
    lines = []
    for b in range(min(counts), max(counts) + 1):
        c = counts.get(b, 0)
        bar = "#" * max(1, round(40 * c / width)) if c else ""
        lines.append(f"  [{b * bucket:5d},{(b + 1) * bucket:5d}) {c:4d} {bar}")
    return "\n".join(lines)


def report(n: int = 10_000) -> str:
    sizes = node_size_distribution(n)
    s = summarize(sizes)
    theo_mean = TARGET_SIZE * math.gamma(1 + 1 / WEIBULL_K)             # λ·Γ(1+1/k)
    theo_p50 = TARGET_SIZE * math.log(2) ** (1 / WEIBULL_K)             # λ·(ln2)^{1/k}
    theo_p99 = TARGET_SIZE * (-math.log(0.01)) ** (1 / WEIBULL_K)       # λ·(-ln0.01)^{1/k}
    return (
        f"== splitter: 节点大小分布 (n={n} 合成KV, 条目≈{2 + 16 + 4 + 32}B, level=0) ==\n"
        f"  节点数 {s['nodes']}  min {s['min']}  p50 {s['p50']:.0f}  mean {s['mean']:.0f}"
        f"  p99 {s['p99']}  max {s['max']}  (bytes)\n"
        f"  理论 Weibull(k=4, λ=4096): mean≈{theo_mean:.0f}  p50≈{theo_p50:.0f}  p99≈{theo_p99:.0f}\n"
        f"  直方图 (1024B/桶):\n{render_histogram(sizes)}"
    )


# ---------------------------------------------------------------------------
# 单元测试
# ---------------------------------------------------------------------------

def test_fnv1a32_known_vectors():
    # 标准 FNV-1a 32bit 测试向量 (无 salt, 无终结器)
    assert fnv1a32(b"") == 0x811C9DC5, f"FNV-1a('') 偏移基错误: {fnv1a32(b''):#x}"
    assert fnv1a32(b"a") == 0xE40C292C, f"FNV-1a('a') 错误: {fnv1a32(b'a'):#x}"


def test_sequential_keys_get_weibull_sizes():
    """回归: 顺序整数字节键 (共享前缀) 下裸 FNV-1a 有网格伪影,
    hash32 (加 fmix32) 后节点分布必须仍符合 Weibull。"""
    rng = random.Random(5)
    sizes = []
    sp = KeySplitter(0)
    for i in range(20_000):
        entry = 2 + 8 + 4 + 22
        if sp.append(i.to_bytes(8, "big"), entry):
            sizes.append(sp.size)
            sp.reset()
    s = summarize(sizes)
    theo_mean = TARGET_SIZE * math.gamma(1 + 1 / WEIBULL_K)
    assert abs(s["mean"] - theo_mean) < 350, \
        f"顺序键下节点均值 {s['mean']:.0f} 偏离理论 {theo_mean:.0f} (网格伪影未消除?)"


def test_salt_is_deterministic_and_level_dependent():
    assert salt_for_level(3) == salt_for_level(3), "同层 salt 必须确定"
    assert salt_for_level(0) != salt_for_level(1) != salt_for_level(2), "不同层 salt 应不同"
    assert len(salt_for_level(0)) == 8, "salt 应为 8 字节"


def test_split_bounds_and_determinism():
    sizes_a = node_size_distribution(10_000, seed=99)
    sizes_b = node_size_distribution(10_000, seed=99)
    assert sizes_a == sizes_b, "同种子同输入必须得到相同切分 (确定性)"
    entry_max = 2 + 16 + 4 + 32
    bad = [s for s in sizes_a if not (MIN_SIZE <= s <= MAX_SIZE + entry_max - 1)]
    assert not bad, f"节点大小越界 [512, {MAX_SIZE}+条目-1]: 前5个 {bad[:5]}"


def test_distribution_moments():
    sizes = node_size_distribution(10_000)
    s = summarize(sizes)
    theo_mean = TARGET_SIZE * math.gamma(1 + 1 / WEIBULL_K)
    assert abs(s["mean"] - theo_mean) < 350, f"均值 {s['mean']:.0f} 偏离理论 {theo_mean:.0f} 超过 350B"
    assert 3200 <= s["p50"] <= 4100, f"中位数 {s['p50']} 应在 [3200, 4100]"
    assert s["p99"] <= 7500, f"p99 {s['p99']} 过大 (Weibull k=4 尾部应短)"
    assert s["min"] >= MIN_SIZE, f"min {s['min']} 低于 MIN_SIZE"
    # 分布应集中: 绝大多数节点落在 2K-6K
    frac = sum(1 for x in sizes if 2048 <= x <= 6144) / len(sizes)
    assert frac > 0.9, f"落在 2K-6K 的比例 {frac:.2%} 应 > 90%"


def test_level_salt_changes_cuts():
    s0 = node_size_distribution(4_000, seed=7, level=0)
    s1 = node_size_distribution(4_000, seed=7, level=1)
    assert s0 != s1, "不同层盐应产生不同切分 (父子层边界不相关)"
    assert summarize(s1)["mean"] > 3000, f"level=1 均值异常: {summarize(s1)}"


TESTS = [
    test_fnv1a32_known_vectors,
    test_sequential_keys_get_weibull_sizes,
    test_salt_is_deterministic_and_level_dependent,
    test_split_bounds_and_determinism,
    test_distribution_moments,
    test_level_salt_changes_cuts,
]

if __name__ == "__main__":
    for t in TESTS:
        t()
        print(f"PASS {t.__name__}")
    print(report())
