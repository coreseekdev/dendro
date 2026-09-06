"""prolly tree (概率 B-tree) + 内容寻址 chunk — dendro 存储格式原型。

dolt 对应文件:
  go/store/prolly/tree/node.go            节点编码 (叶/内部同构, 平行数组)
  go/store/prolly/tree/node_splitter.go   概率分裂 (见 splitter.py)
  go/store/prolly/tree/chunker.go         增量写路径: cursor + 路径复制 + 边界重切
  go/store/prolly/tree/node_cursor.go     读路径: 节点内二分 + 叶间顺序推进
  go/store/prolly/tree/patch_generator.go 双树 diff: 地址相同子树整棵跳过

与正式实现 (spec 03) 的已知差异:
  * 节点尾部校验用 zlib.crc32 (spec 为 crc32c; 原型只保留完整性语义)。
  * apply_mutations 的结构共享 = "前缀整节点复用 + 规范化重切 + chunk 去重":
      - 变更点之前、last_key < 变更起点的子树按整节点地址复用
        (与 dolt chunker 的 skipBelow 等价);
      - 变更点之后重新扫描切分, 边界扰动窗口内的节点被重新序列化;
        其中与基树字节相同的节点靠内容寻址去重兜底 (地址相同 ⇒ 不产生新 chunk)。
    由于切分由"层盐 + 已见内容"决定, 产出的 chunk 集合与 dolt chunker 一致,
    差异仅在 CPU (dolt 用游标对齐免掉未变节点的重序列化)。
  * key/value 是原始 bytes; 行编码、两级根、PGM 索引不在本原型范围。

节点编码 (chunk data 首字节为类型标签, addr = H(tag + payload)):
  tag u8 = 0x01 | level u8 | count u32 | key_blob_len u32
        | key_off u32 x count | val_off u32 x count
        | key_blob  (u16 len + key) x count
        | val_blob  叶: (u32 len + val) x count
                    内部: (child_addr 20B + subtree_count u64) x count
        | crc32 u32
"""
from __future__ import annotations

import bisect
import itertools
import random
import struct
import zlib
from dataclasses import dataclass

if __package__ in (None, ""):  # 允许 python3 prototype/prolly/prolly.py 直接运行
    from hash import Hash20
    from splitter import KeySplitter
else:
    from .hash import Hash20
    from .splitter import KeySplitter

# chunk 类型标签 (spec 03 §1)
TAG_PROLLY_NODE = 0x01
TAG_COMMIT = 0x02
TAG_SCHEMA = 0x03
TAG_ROWBLOCK = 0x04

_KEY_HDR = 2     # u16 key_len
_VAL_HDR = 4     # u32 val_len
_CHILD_ENT = 28  # 内部节点 value = child_addr 20B + subtree_count u64


# ---------------------------------------------------------------------------
# chunk 存储
# ---------------------------------------------------------------------------

class ChunkStore:
    """内存 chunk 存储: addr = H(tag + payload), PUT 幂等去重。

    对应 dolt go/store/chunks (MemoryStoreView)。重复 PUT 无害;
    is_new 用于统计"结构共享率" (spec 03 §2.3)。
    """

    def __init__(self):
        self._chunks: dict[Hash20, bytes] = {}
        self.puts_new = 0
        self.puts_dup = 0

    def put(self, data: bytes) -> tuple[Hash20, bool]:
        addr = Hash20.of(data)
        if addr in self._chunks:
            self.puts_dup += 1
            return addr, False
        self._chunks[addr] = bytes(data)
        self.puts_new += 1
        return addr, True

    def put_object(self, tag: int, payload: bytes) -> tuple[Hash20, bool]:
        return self.put(bytes([tag]) + payload)

    def get(self, addr: Hash20) -> bytes:
        try:
            return self._chunks[addr]
        except KeyError:
            raise KeyError(f"chunk 不存在: {addr}") from None

    def load_node(self, addr: Hash20) -> "Node":
        return decode_node(addr, self.get(addr))

    def count_by_tag(self, tag: int) -> int:
        return sum(1 for d in self._chunks.values() if d[0] == tag)

    def __len__(self) -> int:
        return len(self._chunks)


# ---------------------------------------------------------------------------
# 节点编解码
# ---------------------------------------------------------------------------

class Node:
    """解码后的节点视图。values: 叶=list[bytes]; 内部=list[(child_addr, subtree_count)]。"""

    __slots__ = ("addr", "level", "keys", "values")

    def __init__(self, addr: Hash20, level: int, keys: list, values: list):
        self.addr = addr
        self.level = level
        self.keys = keys
        self.values = values

    @property
    def is_leaf(self) -> bool:
        return self.level == 0

    def __repr__(self) -> str:
        return f"<Node {self.addr} level={self.level} n={len(self.keys)}>"


def encode_node(level: int, keys: list, values: list) -> bytes:
    """序列化节点 payload (不含类型标签); keys 升序, values 与 keys 平行。"""
    n = len(keys)
    key_off: list[int] = []
    key_parts: list[bytes] = []
    kpos = 0
    for k in keys:
        key_off.append(kpos)
        key_parts.append(struct.pack("<H", len(k)) + k)
        kpos += _KEY_HDR + len(k)
    val_off: list[int] = []
    val_parts: list[bytes] = []
    vpos = 0
    if level == 0:
        for v in values:
            val_off.append(vpos)
            val_parts.append(struct.pack("<I", len(v)) + v)
            vpos += _VAL_HDR + len(v)
    else:
        for child_addr, cnt in values:
            val_off.append(vpos)
            val_parts.append(bytes(child_addr) + struct.pack("<Q", cnt))
            vpos += _CHILD_ENT
    body = struct.pack("<BII", level, n, kpos)
    body += struct.pack(f"<{n}I", *key_off) + struct.pack(f"<{n}I", *val_off)
    body += b"".join(key_parts) + b"".join(val_parts)
    # spec 03 §2.2 尾部是 crc32c; 原型用 zlib.crc32 (CRC-32/ISO-HDLC), 语义等价
    return body + struct.pack("<I", zlib.crc32(body) & 0xFFFFFFFF)


def decode_node(addr: Hash20, data: bytes) -> Node:
    if data[0] != TAG_PROLLY_NODE:
        raise ValueError(f"chunk {addr} 不是 PROLLY_NODE (tag={data[0]:#x})")
    body = data[1:-4]
    (crc_stored,) = struct.unpack("<I", data[-4:])
    if (zlib.crc32(body) & 0xFFFFFFFF) != crc_stored:
        raise ValueError(f"节点 CRC 校验失败: addr={addr}")
    level, n, key_blob_len = struct.unpack("<BII", body[:9])
    off = 9
    key_off = struct.unpack_from(f"<{n}I", body, off)
    off += 4 * n
    val_off = struct.unpack_from(f"<{n}I", body, off)
    off += 4 * n
    key_blob = body[off:off + key_blob_len]
    off += key_blob_len
    val_blob = body[off:]
    keys = []
    for o in key_off:
        (klen,) = struct.unpack_from("<H", key_blob, o)
        keys.append(key_blob[o + _KEY_HDR:o + _KEY_HDR + klen])
    values: list = []
    if level == 0:
        for o in val_off:
            (vlen,) = struct.unpack_from("<I", val_blob, o)
            values.append(val_blob[o + _VAL_HDR:o + _VAL_HDR + vlen])
    else:
        for o in val_off:
            values.append((Hash20(val_blob[o:o + 20]),
                           struct.unpack_from("<Q", val_blob, o + 20)[0]))
    return Node(addr, level, keys, values)


def _item_size(level: int, key: bytes, payload) -> int:
    """条目编码字节数 — 分裂器统计的 size 单位 (与 encode_node 的帧装一致)。"""
    if level == 0:
        return _KEY_HDR + len(key) + _VAL_HDR + len(payload)
    return _KEY_HDR + len(key) + _CHILD_ENT


# ---------------------------------------------------------------------------
# 分层切分器: 把有序 item 流切成节点记录 (build 与 apply 共用)
# ---------------------------------------------------------------------------

def _chunk_records(make_items, level: int, base_records, dirty_lo, dirty_hi, store):
    """把有序 item 流 (key, payload) 切分为本层节点记录 [(last_key, addr, count)]。

    make_items(boundary): 返回 key > boundary 的 item 迭代器 (boundary=None 从头)。
    base_records: 基树该层的节点记录, 空表 = 全量构建 (build)。
    dirty_lo/dirty_hi: 本层流中变化的 key 区间 [lo, hi]; None = 全量构建。
    返回 (records, first_dirty_key, last_dirty_key):
      与基树记录按位置锁步对账得到的"变化节点" key 边界, 供上一层定界;
      与基树完全一致时 records == base_records。

    结构共享三件套 (对应 dolt chunker):
      1. 前缀复用: last_key < dirty_lo 的基节点整片沿用, 不重序列化;
      2. 变更后重切: KeySplitter(层盐) 从基节点边界重新扫描 (内容决定 ⇒ 可对齐);
      3. 后缀拼接: 变更全部应用后, 一旦某新节点的 last_key 恰为某基节点边界,
         其后内容与基树逐字节相同 ⇒ 余下基节点整片地址复用。

    坑 (本原型实测踩过): "节点是否变化"只能与基树记录锁步对账, 不能看
    chunk 在 store 里是否首次出现 —— store 跨分支共享, 另一分支可能已写入
    内容相同的 chunk, 把它误判成 no-op 会把变更整个丢掉。
    """
    base_by_key = {rec[0]: i for i, rec in enumerate(base_records)}
    out: list = []

    # --- 1. 前缀整体复用 ---
    prefix_len = 0
    if dirty_lo is not None:
        while prefix_len < len(base_records) and base_records[prefix_len][0] < dirty_lo:
            prefix_len += 1
        if prefix_len == len(base_records) and base_records:
            # 坑: 变更越过树尾 (纯追加) 时, 最后一片必须"重开"以吸纳新条目,
            # 否则会留下一个只有新条目的碎片叶。
            prefix_len -= 1
        out.extend(base_records[:prefix_len])
        boundary = base_records[prefix_len - 1][0] if prefix_len else None
    else:
        boundary = None

    items = make_items(boundary)
    sp = KeySplitter(level)
    keys: list = []
    vals: list = []
    n_items = 0  # 子树条目计数 (内部节点累加孩子的 count)
    bi = prefix_len  # 与基树记录锁步对账的游标
    first_dirty = last_dirty = None

    def flush() -> bool:
        """落盘当前缓冲为一个节点; 若满足后缀对齐条件则拼接基树余下节点。"""
        nonlocal n_items, bi, first_dirty, last_dirty
        payload = encode_node(level, keys, vals)
        addr, _ = store.put_object(TAG_PROLLY_NODE, payload)
        last_key = keys[-1]
        out.append((last_key, addr, n_items))
        # 锁步对账: 位置对上但 addr/count 不同 ⇒ 该节点变了;
        # last_key 错位 ⇒ 进入边界扰动窗口, 其后全部按脏处理 (保守但安全)
        if bi < len(base_records) and base_records[bi][0] == last_key:
            if base_records[bi][1] != addr or base_records[bi][2] != n_items:
                if first_dirty is None:
                    first_dirty = last_key
                last_dirty = last_key
            bi += 1
        else:
            if first_dirty is None:
                first_dirty = last_key
            last_dirty = last_key
        keys.clear()
        vals.clear()
        n_items = 0
        sp.reset()  # 切分后缓冲清零 (对应 dolt keySplitter 的 count 重置)
        # --- 3. 后缀对齐检查: 变更已全部应用且落在基树边界上 ⇒ 整片复用余下子树
        if (dirty_hi is not None and last_key >= dirty_hi
                and last_key in base_by_key):
            out.extend(base_records[base_by_key[last_key] + 1:])
            return True
        return False

    for key, val in items:
        keys.append(key)
        vals.append(val)
        n_items += 1 if level == 0 else val[1]
        if sp.append(key, _item_size(level, key, val)):
            if flush():
                return out, first_dirty, last_dirty
    if keys:
        flush()
    return out, first_dirty, last_dirty


# ---------------------------------------------------------------------------
# 全量建树
# ---------------------------------------------------------------------------

def build_map(store: ChunkStore, items) -> Hash20 | None:
    """全量建树: items 为 (key, value) 升序流。空输入返回 None (空树)。

    层级切分复用 _chunk_records (base_records 空 ⇒ 纯规范化切分);
    直到某层只剩 1 个节点即为根。
    """
    items = list(items)
    for i in range(1, len(items)):
        if not items[i - 1][0] < items[i][0]:
            raise ValueError(f"build_map 输入必须严格升序: #{i-1}={items[i-1][0]!r} >= #{i}={items[i][0]!r}")
    if not items:
        return None

    def leaf_items(_boundary=None):
        return iter(items)

    records, _, _ = _chunk_records(leaf_items, 0, [], None, None, store)
    level = 1
    while len(records) > 1:
        recs = records

        def parent_items(_boundary=None, _recs=recs):
            # 上一层记录 (last_key, addr, count) → 本层 item (key, (addr, count))
            return iter((r[0], (r[1], r[2])) for r in _recs)

        records, _, _ = _chunk_records(parent_items, level, [], None, None, store)
        level += 1
    return records[0][1]


# ---------------------------------------------------------------------------
# 增量修改: apply(base_root, sorted_mutations)
# ---------------------------------------------------------------------------

def _check_mutations(mutations) -> None:
    for i in range(1, len(mutations)):
        if not mutations[i - 1][0] < mutations[i][0]:
            raise ValueError(
                f"mutations 必须按 key 严格升序且去重: #{i-1}={mutations[i-1][0]!r} >= #{i}={mutations[i][0]!r}")


def _merged_entries(store, leaf_records, start_after_key, mutations):
    """基树叶条目 (从 start_after_key 之后那片叶起) 与变更的有序归并。

    变更语义: (key, value) 覆盖/插入, (key, None) 删除。
    """
    if start_after_key is None:
        p = 0
    else:
        p = bisect.bisect_right([r[0] for r in leaf_records], start_after_key)
    mi = 0
    n_mut = len(mutations)
    for rec in leaf_records[p:]:
        leaf = store.load_node(rec[1])
        for k, v in zip(leaf.keys, leaf.values):
            while mi < n_mut and mutations[mi][0] < k:
                mk, mv = mutations[mi]
                if mv is not None:
                    yield mk, mv
                mi += 1
            if mi < n_mut and mutations[mi][0] == k:
                mv = mutations[mi][1]
                if mv is not None:
                    yield k, mv
                mi += 1
            else:
                yield k, v
    for mk, mv in mutations[mi:]:
        if mv is not None:
            yield mk, mv


def apply_mutations(store: ChunkStore, base_root: Hash20 | None, mutations) -> Hash20 | None:
    """增量应用一组有序变更, 返回新根 (结构共享, 见 _chunk_records docstring)。

    * 未受影响子树地址整体复用 ⇒ 单 key 写只新建"受影响路径 + 扰动边界窗口"
      数量的 chunk (spec 03 §2.3);
    * 变更集与现值完全一致 (no-op) ⇒ 不产生任何新 chunk, 原根原样返回;
    * 删空 ⇒ 返回 None。
    """
    if not mutations:
        return base_root
    _check_mutations(mutations)
    if base_root is None:
        return build_map(store, [(k, v) for k, v in mutations if v is not None])

    levels = collect_levels(store, base_root)
    leaf_records = levels.get(0, [])
    mut_lo, mut_hi = mutations[0][0], mutations[-1][0]

    def leaf_items(boundary=None, _leaves=leaf_records, _muts=mutations, _store=store):
        return _merged_entries(_store, _leaves, boundary, _muts)

    records, lo, _hi = _chunk_records(leaf_items, 0, leaf_records, mut_lo, mut_hi, store)
    if not records:
        return None  # 全部删空
    if records == leaf_records:
        return base_root  # 与基树逐记录一致 ⇒ 树逐字节未变 (no-op)

    level = 1
    while len(records) > 1:
        base_records = levels.get(level, [])
        recs = records

        def parent_items(boundary=None, _recs=recs):
            # 上一层记录 (last_key, addr, count) → 本层 item (key, (addr, count))
            gen = ((r[0], (r[1], r[2])) for r in _recs)
            if boundary is None:
                return gen
            return itertools.dropwhile(lambda it, _b=boundary: it[0] <= _b, gen)

        records, lo, _hi = _chunk_records(parent_items, level, base_records, lo, _hi, store)
        if not records:
            return None
        if records == base_records:
            return base_root  # 本层及以上全部复用
        level += 1
    return records[0][1]


# ---------------------------------------------------------------------------
# 读路径: get / range_scan / iter
# ---------------------------------------------------------------------------

def get(store: ChunkStore, root: Hash20 | None, key: bytes) -> bytes | None:
    """点查: 根 → 逐层二分 (fence key = 子树最大 key) → 叶值。"""
    if root is None:
        return None
    node = store.load_node(root)
    while not node.is_leaf:
        i = bisect.bisect_left(node.keys, key)
        if i == len(node.keys):
            return None  # key 大于树内最大 key
        node = store.load_node(node.values[i][0])
    i = bisect.bisect_left(node.keys, key)
    if i < len(node.keys) and node.keys[i] == key:
        return node.values[i]
    return None


def _leftmost_cursor(store: ChunkStore, root: Hash20) -> list:
    node = store.load_node(root)
    stack = [[node, 0]]
    while stack[-1][0].level > 0:
        top = stack[-1][0]
        stack.append([store.load_node(top.values[stack[-1][1]][0]), 0])
    return stack  # stack = [(node, child_idx), ..., (leaf, entry_idx)]


def _seek_cursor(store: ChunkStore, root: Hash20, key: bytes):
    """定位第一个 key' >= key 的条目; key 超过树内最大 key 时返回 None。"""
    node = store.load_node(root)
    stack = []
    while not node.is_leaf:
        i = bisect.bisect_left(node.keys, key)
        if i == len(node.keys):
            return None
        stack.append([node, i])
        node = store.load_node(node.values[i][0])
    i = bisect.bisect_left(node.keys, key)
    stack.append([node, i])
    if i == len(node.keys) and not _advance(store, stack):
        return None
    return stack


def _advance(store: ChunkStore, stack: list) -> bool:
    """cursor 前进一个条目; 耗尽返回 False (stack 弹空)。"""
    while stack:
        node, idx = stack[-1]
        if node.is_leaf:
            if idx + 1 < len(node.keys):
                stack[-1][1] = idx + 1
                return True
            stack.pop()
            continue
        # 内部节点: idx 指向当前孩子; 换下一个孩子并降到叶
        if idx + 1 < len(node.keys):
            stack[-1][1] = idx + 1
            child = store.load_node(node.values[idx + 1][0])
            stack.append([child, 0])
            while stack[-1][0].level > 0:
                top = stack[-1][0]
                stack.append([store.load_node(top.values[0][0]), 0])
            return True
        stack.pop()
    return False


def iter_items(store: ChunkStore, root: Hash20 | None):
    """全序迭代 (key, value); 空树产出空序列。"""
    if root is None:
        return
    stack = _leftmost_cursor(store, root)
    while stack:
        leaf, i = stack[-1]
        yield leaf.keys[i], leaf.values[i]
        if not _advance(store, stack):
            return


def range_scan(store: ChunkStore, root: Hash20 | None, start: bytes | None, end: bytes | None):
    """范围扫描 [start, end) (半开区间); start/end 为 None 表示不设界。"""
    if root is None:
        return
    if start is None:
        stack = _leftmost_cursor(store, root)
    else:
        stack = _seek_cursor(store, root, start)
        if stack is None:
            return
    while stack:
        leaf, i = stack[-1]
        k = leaf.keys[i]
        if end is not None and k >= end:
            return
        yield k, leaf.values[i]
        if not _advance(store, stack):
            return


def count(store: ChunkStore, root: Hash20 | None) -> int:
    """子树条目总数 (校验 subtree_count 记账用)。"""
    if root is None:
        return 0
    node = store.load_node(root)
    if node.is_leaf:
        return len(node.keys)
    return sum(c for _, c in node.values)


def collect_levels(store: ChunkStore, root: Hash20) -> dict[int, list]:
    """按层枚举节点记录 {level: [(last_key, addr, subtree_count)]}, 层内从左到右。"""
    levels: dict[int, list] = {}

    def walk(addr, node):
        recs = levels.setdefault(node.level, [])
        if node.is_leaf:
            recs.append((node.keys[-1], addr, len(node.keys)))
            return
        total = 0
        for child_addr, cnt in node.values:
            walk(child_addr, store.load_node(child_addr))
            total += cnt
        recs.append((node.keys[-1], addr, total))

    walk(root, store.load_node(root))
    return levels


# ---------------------------------------------------------------------------
# diff: 双树并行 diff, 地址相同的子树整棵跳过
# ---------------------------------------------------------------------------

@dataclass
class DiffStats:
    subtree_skips: int = 0        # 整棵子树地址相同被跳过 (含根相等)
    leaf_pairs_skipped: int = 0   # 叶级地址相同被跳过
    leaf_pairs_compared: int = 0  # 真正逐条目比较的叶对
    entries_scanned: int = 0


class _Seg:
    """子树片段: addr 的子树限制在 (lo, hi] 内 (None = 不设界)。"""

    __slots__ = ("addr", "lo", "hi", "is_leaf")

    def __init__(self, addr, lo, hi, is_leaf):
        self.addr = addr
        self.lo = lo
        self.hi = hi
        self.is_leaf = is_leaf

    def __repr__(self):
        return f"_Seg({self.addr}, ({self.lo!r},{self.hi!r}], leaf={self.is_leaf})"


def _seg_before(a: _Seg, b: _Seg) -> bool:
    """a 整体位于 b 左侧 (a.hi <= b.lo)?"""
    return a.hi is not None and b.lo is not None and a.hi <= b.lo


def _clipped_segments(store: ChunkStore, node: Node, lo, hi) -> list[_Seg]:
    """节点的孩子片段限制在 (lo, hi] 内; 叶则返回其自身 (裁剪到窗口)。"""
    if node.is_leaf:
        return [_Seg(node.addr, lo, hi, True)]
    segs = []
    prev = None  # exclusive 左界
    for fence, (child_addr, _cnt) in zip(node.keys, node.values):
        c_hi_ok = fence is not None and lo is not None and fence <= lo
        c_lo_ok = prev is not None and hi is not None and prev >= hi
        if not c_hi_ok and not c_lo_ok:  # (prev, fence] 与 (lo, hi] 有交集
            seg_lo = prev if (lo is None or (prev is not None and prev > lo)) else lo
            seg_hi = fence if (hi is None or (fence is not None and fence < hi)) else hi
            segs.append(_Seg(child_addr, seg_lo, seg_hi, False))
        prev = fence
    return segs


def _iter_range_le(store: ChunkStore, addr: Hash20, lo, hi):
    """子树内 (lo, hi] 的条目 (diff 单侧 dump 用)。"""
    node = store.load_node(addr)
    if node.is_leaf:
        walker = iter([node])
    else:
        walker = _leaf_walk(store, node)
    for leaf in walker:
        i0 = bisect.bisect_right(leaf.keys, lo) if lo is not None else 0
        i1 = bisect.bisect_right(leaf.keys, hi) if hi is not None else len(leaf.keys)
        for i in range(i0, i1):
            yield leaf.keys[i], leaf.values[i]


def _leaf_walk(store: ChunkStore, node: Node):
    """按序产出子树内的叶节点 (Node 对象), 每片叶恰好一次。

    注意与 iter_items 的区别: _advance 是"条目级"推进 (叶内索引 +1 也算一步),
    这里只要叶对象没换就不重复产出。
    """
    if node.is_leaf:
        yield node
        return
    stack = [[node, 0]]
    while stack[-1][0].level > 0:
        top = stack[-1][0]
        stack.append([store.load_node(top.values[stack[-1][1]][0]), 0])
    prev = None
    while stack:
        leaf = stack[-1][0]
        if leaf is not prev:
            yield leaf
            prev = leaf
        if not _advance(store, stack):
            return


def _dump(store: ChunkStore, seg: _Seg, from_b: bool, out: list) -> None:
    """单侧片段 → patch 流。from_b=False: 仅 A 有 (k, v, None); True: 仅 B 有 (k, None, v)。"""
    for k, v in _iter_range_le(store, seg.addr, seg.lo, seg.hi):
        out.append((k, None, v) if from_b else (k, v, None))


def _merge_scan(store: ChunkStore, la: Node, lb: Node, lo, hi, stats: DiffStats) -> list:
    """两片叶在 (lo, hi] 内的归并 diff (patch_generator.go 的叶级情形)。"""
    ka, kb = la.keys, lb.keys
    ia = bisect.bisect_right(ka, lo) if lo is not None else 0
    ea = bisect.bisect_right(ka, hi) if hi is not None else len(ka)
    ib = bisect.bisect_right(kb, lo) if lo is not None else 0
    eb = bisect.bisect_right(kb, hi) if hi is not None else len(kb)
    stats.leaf_pairs_compared += 1
    stats.entries_scanned += (ea - ia) + (eb - ib)
    out: list = []
    i, j = ia, ib
    while i < ea and j < eb:
        if ka[i] < kb[j]:
            out.append((ka[i], la.values[i], None))
            i += 1
        elif kb[j] < ka[i]:
            out.append((kb[j], None, lb.values[j]))
            j += 1
        else:
            if la.values[i] != lb.values[j]:
                out.append((ka[i], la.values[i], lb.values[j]))
            i += 1
            j += 1
    while i < ea:
        out.append((ka[i], la.values[i], None))
        i += 1
    while j < eb:
        out.append((kb[j], None, lb.values[j]))
        j += 1
    return out


def _diff_range(store: ChunkStore, a_addr, b_addr, lo, hi, out: list, stats: DiffStats) -> None:
    """比较两棵子树在 (lo, hi] 内的差异; 两侧子树都必须覆盖该区间。

    双 cursor 并走: 片段两指针归并; 区间不等时对交集递归收窄、
    交集外按单侧 dump; 片段地址相同 ⇒ 整片(整棵子树)跳过。
    """
    if a_addr == b_addr:
        stats.subtree_skips += 1
        return
    a = store.load_node(a_addr)
    b = store.load_node(b_addr)
    if a.is_leaf and b.is_leaf:
        out.extend(_merge_scan(store, a, b, lo, hi, stats))
        return

    sa = _clipped_segments(store, a, lo, hi)
    sb = _clipped_segments(store, b, lo, hi)
    i = j = 0
    while i < len(sa) and j < len(sb):
        A, B = sa[i], sb[j]
        if _seg_before(A, B):
            _dump(store, A, False, out)
            i += 1
            continue
        if _seg_before(B, A):
            _dump(store, B, True, out)
            j += 1
            continue
        # 有交集: 先把左侧未对齐的部分按单侧处理, 使两段左端一致
        # (lo=None 视为 -inf; 两侧都从 -inf 开始则已对齐)
        a_starts_left = A.lo is None or (B.lo is not None and A.lo < B.lo)
        b_starts_left = B.lo is None or (A.lo is not None and B.lo < A.lo)
        if a_starts_left and not b_starts_left:
            _dump(store, _Seg(A.addr, A.lo, B.lo, A.is_leaf), False, out)
            A = _Seg(A.addr, B.lo, A.hi, A.is_leaf)
        elif b_starts_left and not a_starts_left:
            _dump(store, _Seg(B.addr, B.lo, A.lo, B.is_leaf), True, out)
            B = _Seg(B.addr, A.lo, B.hi, B.is_leaf)
        ov_hi = A.hi if (B.hi is None or (A.hi is not None and A.hi < B.hi)) else B.hi
        # 交集 (A.lo, ov_hi]: 地址相同整片跳过; 叶对直接归并; 否则递归收窄
        if A.addr == B.addr:
            if A.is_leaf and B.is_leaf:
                stats.leaf_pairs_skipped += 1
            else:
                stats.subtree_skips += 1
        elif A.is_leaf and B.is_leaf:
            out.extend(_merge_scan(store, store.load_node(A.addr),
                                   store.load_node(B.addr), A.lo, ov_hi, stats))
        else:
            _diff_range(store, A.addr, B.addr, A.lo, ov_hi, out, stats)
        # 推进: 右端先到头的一侧换下一片段, 另一侧裁剪剩余区间
        if ov_hi is None or (A.hi is not None and A.hi <= ov_hi):
            i += 1
        else:
            sa[i] = _Seg(A.addr, ov_hi, A.hi, A.is_leaf)
        if ov_hi is None or (B.hi is not None and B.hi <= ov_hi):
            j += 1
        else:
            sb[j] = _Seg(B.addr, ov_hi, B.hi, B.is_leaf)
    while i < len(sa):
        _dump(store, sa[i], False, out)
        i += 1
    while j < len(sb):
        _dump(store, sb[j], True, out)
        j += 1


def diff(store: ChunkStore, root_a: Hash20 | None, root_b: Hash20 | None,
         stats: DiffStats | None = None) -> list:
    """双树 diff, 返回 patch 流 [(key, old|None, new|None)], 按 key 升序。

    None 表示该侧不存在 (插入/删除)。地址相同的子树整棵跳过 ⇒
    复杂度 O(差异路径), 与无关子树大小无关 (spec 03 §6)。
    """
    stats = stats if stats is not None else DiffStats()
    if root_a is None and root_b is None:
        return []
    if root_a is None:
        return [(k, None, v) for k, v in iter_items(store, root_b)]
    if root_b is None:
        return [(k, v, None) for k, v in iter_items(store, root_a)]
    if root_a == root_b:
        stats.subtree_skips += 1
        return []
    out: list = []
    _diff_range(store, root_a, root_b, None, None, out, stats)
    return out


# ---------------------------------------------------------------------------
# 结构共享统计报告
# ---------------------------------------------------------------------------

_CACHE: dict = {}


def structure_report(n_keys: int = 100_000, seed: int = 42) -> dict:
    """建 n_keys 行的树, 做单 key 覆写 / no-op / 尾追加 / 头插入,
    报告"新建 chunk 数 / 总 chunk 数" (结构共享率)。结果按参数缓存。"""
    key = ("structure", n_keys, seed)
    if key in _CACHE:
        return _CACHE[key]

    store = ChunkStore()
    rng = random.Random(seed)
    items = []
    for i in range(n_keys):
        kb = i.to_bytes(8, "big")
        items.append((kb, b"v|" + rng.randbytes(20)))
    root = build_map(store, items)
    levels = collect_levels(store, root)
    total_chunks = sum(len(v) for v in levels.values())
    per_level = {lv: len(recs) for lv, recs in sorted(levels.items())}

    # 1) 单 key 覆写 (值长度不同 ⇒ 字节相位偏移, 触发边界扰动)
    mid_key = (n_keys // 2).to_bytes(8, "big")
    n0 = len(store)
    root2 = apply_mutations(store, root, [(mid_key, b"OVERWRITTEN-VALUE")])
    new_1write = len(store) - n0

    # 2) no-op (同值覆写) ⇒ 必须零新 chunk 且根不变
    cur = get(store, root2, mid_key)
    n1 = len(store)
    root3 = apply_mutations(store, root2, [(mid_key, cur)])
    noop_reused = root3 == root2 and len(store) == n1

    # 3) 尾追加 (越过最大 key; 验证最后一片被重开而非整体拼接)
    tail_key = (n_keys + 5).to_bytes(8, "big")
    n2 = len(store)
    root4 = apply_mutations(store, root3, [(tail_key, b"appended-tail")])
    new_append = len(store) - n2
    # 承接追加的叶应 ≥ 2 个条目 (重开并吸收新条目, 而非碎片叶)
    leaf_of_tail = None
    node = store.load_node(root4)
    while not node.is_leaf:
        node = store.load_node(node.values[-1][0])
    leaf_of_tail = node
    tail_leaf_entries = len(leaf_of_tail.keys)

    # 4) 头插入 (最小 key 之前)
    head_key = b"\x00" * 8
    n3 = len(store)
    root5 = apply_mutations(store, root4, [(head_key, b"prepend")])
    new_prepend = len(store) - n3

    rep = {
        "n_keys": n_keys,
        "total_chunks": total_chunks,
        "per_level": per_level,
        "new_after_1_write": new_1write,
        "share_after_1_write": 1 - new_1write / total_chunks,
        "noop_root_reused": noop_reused,
        "new_after_append": new_append,
        "tail_leaf_entries": tail_leaf_entries,
        "new_after_prepend": new_prepend,
    }
    _CACHE[key] = rep
    return rep


def report(n_keys: int = 100_000) -> str:
    r = structure_report(n_keys)
    lvl = "  ".join(f"L{lv}:{n}" for lv, n in r["per_level"].items())
    return (
        f"== prolly: 结构共享 (build {r['n_keys']} 行, 单 key 覆写) ==\n"
        f"  树 chunk 总数 {r['total_chunks']}  ({lvl})\n"
        f"  单 key 覆写新建 chunk {r['new_after_1_write']}  → 结构共享率 "
        f"{r['share_after_1_write']:.2%}\n"
        f"  no-op 同值覆写: 根复用={r['noop_root_reused']} (0 新 chunk)\n"
        f"  尾追加新建 {r['new_after_append']} (承接叶含 {r['tail_leaf_entries']} 条目, 无碎片叶);"
        f"  头插入新建 {r['new_after_prepend']}"
    )


# ---------------------------------------------------------------------------
# 单元测试
# ---------------------------------------------------------------------------

def _oracle_map(n, seed):
    rng = random.Random(seed)
    return {i.to_bytes(8, "big"): b"v|" + rng.randbytes(20) for i in range(n)}


def test_node_codec_roundtrip():
    store = ChunkStore()
    for level, values in (
        (0, [b"", b"short", b"x" * 100]),
        (1, None),
    ):
        keys = [f"key{i:04d}".encode() for i in range(3)]
        vals = values if values is not None else [
            (Hash20.of(b"child"), i * 7) for i in range(3)]
        payload = encode_node(level, keys, vals)
        addr, _ = store.put_object(TAG_PROLLY_NODE, payload)
        node = decode_node(addr, store.get(addr))
        assert node.level == level and node.keys == keys, f"roundtrip 失败 level={level}: {node}"
        if level == 0:
            assert node.values == values
        else:
            assert [(a, c) for a, c in node.values] == vals, f"内部节点 roundtrip 失败: {node.values}"
    # CRC 破坏检测
    payload = bytearray(encode_node(0, [b"k"], [b"v"]))
    payload[5] ^= 0xFF
    try:
        decode_node(Hash20.of(bytes(payload)), bytes([TAG_PROLLY_NODE]) + bytes(payload))
    except ValueError:
        pass
    else:
        raise AssertionError("损坏的节点应触发 CRC 校验失败")


def test_build_get_iter_range():
    store = ChunkStore()
    oracle = _oracle_map(10_000, seed=1)
    root = build_map(store, sorted(oracle.items()))
    assert count(store, root) == len(oracle), \
        f"subtree_count 记账错误: {count(store, root)} != {len(oracle)}"
    # 点查: 命中 / 未命中 / 边界
    for k in [b"\x00" * 8, (0).to_bytes(8, "big"), (9999).to_bytes(8, "big"),
              (5000).to_bytes(8, "big"), (12345).to_bytes(8, "big")]:
        assert get(store, root, k) == oracle.get(k), f"点查不一致 key={k!r}"
    # 全序迭代
    got = list(iter_items(store, root))
    assert got == sorted(oracle.items()), "全序迭代与 oracle 不一致"
    # 范围扫描 (半开区间 + None 边界)
    s, e = (100).to_bytes(8, "big"), (200).to_bytes(8, "big")
    got = list(range_scan(store, root, s, e))
    want = [(k, v) for k, v in sorted(oracle.items()) if s <= k < e]
    assert got == want, f"range_scan [100,200) 不一致: {len(got)} vs {len(want)}"
    assert list(range_scan(store, root, e, s)) == [], "start>end 应为空"
    assert list(range_scan(store, root, None, s)) == \
        [(k, v) for k, v in sorted(oracle.items()) if k < s], "range(None,s) 不一致"


def test_apply_batches_match_oracle():
    """随机批量 insert/update/delete, 每轮与 dict oracle 全量对账。"""
    store = ChunkStore()
    rng = random.Random(7)
    oracle = _oracle_map(5_000, seed=3)
    root = build_map(store, sorted(oracle.items()))
    all_keys = [i.to_bytes(8, "big") for i in range(8_000)]
    for round_no in range(25):
        muts = {}
        while len(muts) < 40:
            k = rng.choice(all_keys)
            op = rng.random()
            if op < 0.35 and k in oracle:
                muts[k] = None  # 删除
            elif op < 0.7 and k in oracle:
                muts[k] = b"upd|" + rng.randbytes(12)  # 覆写 (长度扰动)
            else:
                muts[k] = b"ins|" + rng.randbytes(9)  # 插入/覆盖
        muts = sorted(muts.items())
        root = apply_mutations(store, root, muts)
        for k, v in muts:
            if v is None:
                oracle.pop(k, None)
            else:
                oracle[k] = v
        assert root is not None, f"round {round_no}: 树不应为空"
        assert count(store, root) == len(oracle), \
            f"round {round_no}: subtree_count {count(store, root)} != oracle {len(oracle)}"
        got = dict(iter_items(store, root))
        if got != oracle:
            diff_keys = [k for k in set(got) | set(oracle) if got.get(k) != oracle.get(k)][:5]
            raise AssertionError(f"round {round_no}: 树与 oracle 不一致, 例: {diff_keys}")
        k = rng.choice(all_keys)
        assert get(store, root, k) == oracle.get(k), f"round {round_no}: 点查不一致 {k!r}"


def test_apply_noop_reuses_root():
    store = ChunkStore()
    oracle = _oracle_map(2_000, seed=5)
    root = build_map(store, sorted(oracle.items()))
    k, v = next(iter(oracle.items()))
    n0 = len(store)
    root2 = apply_mutations(store, root, [(k, v)])
    assert root2 == root, "同值覆写应原样返回根"
    assert len(store) == n0, f"no-op 不应产生新 chunk: +{len(store)-n0}"
    assert apply_mutations(store, root, []) == root, "空变更集应原样返回根"


def test_apply_edges():
    store = ChunkStore()
    oracle = _oracle_map(1_000, seed=11)
    root = build_map(store, sorted(oracle.items()))
    # 头插入 (最小 key 之前)
    hk = b"\x00" * 8
    root = apply_mutations(store, root, [(hk, b"head")])
    oracle[hk] = b"head"
    assert get(store, root, hk) == b"head", "头插入后应可点查"
    # 尾追加: 新条目应并入重开的最后一片叶, 不产生碎片叶
    tk = (999_999).to_bytes(8, "big")
    root = apply_mutations(store, root, [(tk, b"tail")])
    oracle[tk] = b"tail"
    node = store.load_node(root)
    while not node.is_leaf:
        node = store.load_node(node.values[-1][0])
    assert len(node.keys) >= 2, \
        f"尾追加后最后一片叶应 ≥2 条目 (无碎片叶), 实际 {len(node.keys)}: {node.keys[-3:]}"
    assert list(iter_items(store, root)) == sorted(oracle.items()), "头插+尾追后全序迭代不一致"
    # 删空
    muts = [(k, None) for k in sorted(oracle)]
    root2 = apply_mutations(store, root, muts)
    assert root2 is None, "全部删除应返回空树 (None)"
    assert list(iter_items(store, root2)) == [], "空树迭代应为空"
    # 空树上插入
    root3 = apply_mutations(store, None, [(b"k1", b"v1"), (b"k2", b"v2")])
    assert get(store, root3, b"k1") == b"v1" and get(store, root3, b"k2") == b"v2", \
        "空树 apply 应等价于 build"


def test_structure_sharing():
    r = structure_report(100_000)
    assert r["total_chunks"] > 500, f"1e5 行的树应远大于 500 chunk: {r['total_chunks']}"
    budget = max(20, int(0.02 * r["total_chunks"]))
    assert r["new_after_1_write"] <= budget, \
        f"单 key 覆写新建 {r['new_after_1_write']} chunk 超预算 {budget} (共享率 {r['share_after_1_write']:.2%})"
    assert r["noop_root_reused"], "no-op 覆写必须零新 chunk 且根不变"
    assert r["tail_leaf_entries"] >= 2, "尾追加应并入重开的最后一片叶"


def test_diff_matches_oracle():
    rng = random.Random(23)
    base = _oracle_map(2_500, seed=13)
    keys = sorted(base)

    def mutate(m: dict, n_upd, n_del, n_ins, tag):
        a = dict(m)
        for _ in range(n_upd):
            k = rng.choice(keys)
            a[k] = tag + rng.randbytes(8)
        for _ in range(n_del):
            a.pop(rng.choice(keys), None)
        for _ in range(n_ins):
            a[int(rng.getrandbits(63)).to_bytes(8, "big")] = tag + rng.randbytes(8)
        return a

    a = mutate(base, 60, 40, 30, b"A|")
    b = mutate(base, 80, 30, 50, b"B|")
    store = ChunkStore()
    ra = build_map(store, sorted(a.items()))
    rb = build_map(store, sorted(b.items()))
    d = diff(store, ra, rb)
    expected = []
    for k in sorted(set(a) | set(b)):
        va, vb = a.get(k), b.get(k)
        if va != vb:
            expected.append((k, va, vb))
    assert d == expected, \
        f"diff 与 oracle 不一致: got {len(d)} patches, want {len(expected)}; 首3个差异 {d[:3]} vs {expected[:3]}"
    # 空树 / 相等树
    assert diff(store, None, ra) == [(k, None, v) for k, v in sorted(a.items())], "空树 diff 应为全量插入"
    assert diff(store, ra, ra) == [], "同根 diff 应为空"


def test_leaf_walk_yields_each_leaf_once():
    """回归: _leaf_walk 曾按"条目步进"重复产出同一叶, 导致内部节点的
    单侧 dump (diff 的插入/删除段) 把条目复制多份。"""
    store = ChunkStore()
    oracle = _oracle_map(20_000, seed=17)
    root = build_map(store, sorted(oracle.items()))
    levels = collect_levels(store, root)
    assert max(levels) >= 2, "测试前提: 至少 3 层树"
    walked = list(_leaf_walk(store, store.load_node(root)))
    assert len(walked) == len(levels[0]), \
        f"_leaf_walk 应产出每叶一次: {len(walked)} vs {len(levels[0])}"
    assert len({n.addr for n in walked}) == len(walked), "_leaf_walk 有重复叶"
    # 内部节点片段的单侧 dump: 条目数必须正好等于子树条目数 (无重复)
    out: list = []
    _dump(store, _Seg(root, None, store.load_node(root).keys[-1], False), False, out)
    assert len(out) == len(oracle) == len({k for k, _, _ in out}), \
        f"单侧 dump 条目重复: {len(out)} vs {len(oracle)}"


TESTS = [
    test_node_codec_roundtrip,
    test_build_get_iter_range,
    test_apply_batches_match_oracle,
    test_apply_noop_reuses_root,
    test_apply_edges,
    test_structure_sharing,
    test_diff_matches_oracle,
    test_leaf_walk_yields_each_leaf_once,
]

if __name__ == "__main__":
    for t in TESTS:
        t()
        print(f"PASS {t.__name__}")
    print(report())
