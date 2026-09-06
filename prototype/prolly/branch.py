"""git 式分支 / 提交 / 三方合并 — dendro 存储格式原型。

dolt 对应:
  go/store/chunks/chunk_store.go           commit 对象 chunk 化 (tag 0x02)
  dolt datasets (go/libraries/dolt/core/dataset)  refs/branch 目录
    — spec 03 §4 的简化: catalog = prolly map {branch_name -> commit_addr},
      本身就是一棵 prolly tree ⇒ 加一个分支 = catalog map 一次 O(1) 变更,
      不同分支的指针结构共享, 数据树 chunk 零复制。
  go/store/prolly/tree/patch_generator.go  diff (prolly.diff)
  三方归并语义见 spec 03 §6:
    仅左改 → 应用左; 仅右改 → 应用右; 同改同值 → 收敛(非冲突);
    同改不同值 / 一删一改 → 行级冲突; 无冲突 ⇒ 基于 left 应用 right patches
    生成新根, 新 commit 带双亲。
"""
from __future__ import annotations

import random
import struct
import time
from dataclasses import dataclass, field

if __package__ in (None, ""):  # 允许 python3 prototype/prolly/branch.py 直接运行
    from hash import Hash20
    import prolly as pl
else:
    from .hash import Hash20
    from . import prolly as pl

_BRANCH_SEP = b"\x00" * 20  # commit.root 为空(全删空树)时的占位根


# ---------------------------------------------------------------------------
# commit 对象 (chunk tag 0x02)
# ---------------------------------------------------------------------------

class Commit:
    """COMMIT chunk: {root, parents[], height, ts_ms, branch, message}。"""

    __slots__ = ("addr", "root", "parents", "height", "ts_ms", "branch", "message")

    def __init__(self, addr, root, parents, height, ts_ms, branch, message):
        self.addr = addr
        self.root = root          # Hash20 | None (None = 空树)
        self.parents = parents    # list[Hash20]; 多亲 = merge
        self.height = height
        self.ts_ms = ts_ms
        self.branch = branch
        self.message = message

    def __repr__(self):
        return (f"<Commit {self.addr} branch={self.branch!r} h={self.height} "
                f"root={self.root} parents={len(self.parents)}>")

    def __eq__(self, other):
        # 内容寻址: 同一 addr 即同一 commit (checkout 每次解码新对象)
        return isinstance(other, Commit) and self.addr == other.addr

    def __hash__(self):
        return hash(self.addr)


def encode_commit(root, parents, height: int, ts_ms: int, branch: str, message: str) -> bytes:
    out = bytearray()
    out += bytes(root) if root is not None else _BRANCH_SEP
    out.append(len(parents))
    for p in parents:
        out += bytes(p)
    out += struct.pack("<Qq", height, ts_ms)
    nb = branch.encode()
    out += struct.pack("<H", len(nb)) + nb
    mb = message.encode()
    out += struct.pack("<H", len(mb)) + mb
    return bytes(out)


def decode_commit(addr: Hash20, data: bytes) -> Commit:
    if data[0] != pl.TAG_COMMIT:
        raise ValueError(f"chunk {addr} 不是 COMMIT (tag={data[0]:#x})")
    body = memoryview(data[1:])
    root_raw = bytes(body[0:20])
    root = None if root_raw == _BRANCH_SEP else Hash20(root_raw)
    off = 20
    n_parents = body[off]
    off += 1
    parents = [Hash20(body[off + 20 * i:off + 20 * (i + 1)]) for i in range(n_parents)]
    off += 20 * n_parents
    height, ts_ms = struct.unpack_from("<Qq", body, off)
    off += 16
    (lbl,) = struct.unpack_from("<H", body, off)
    off += 2
    branch = bytes(body[off:off + lbl]).decode()
    off += lbl
    (lms,) = struct.unpack_from("<H", body, off)
    off += 2
    message = bytes(body[off:off + lms]).decode()
    return Commit(addr, root, parents, height, ts_ms, branch, message)


# ---------------------------------------------------------------------------
# 三方合并
# ---------------------------------------------------------------------------

@dataclass
class MergeResult:
    ok: bool
    commit: Commit | None           # 成功时的新 commit (FF 时为被指向的既有 commit)
    conflicts: list                 # [(key, left_new|None, right_new|None)]
    fast_forward: bool
    left_patches: int
    right_patches: int
    new_chunks: int                 # 合并过程新建 chunk 数 (数据节点 + commit)
    diff_stats: dict = field(default_factory=dict)


def merge_commits(store: pl.ChunkStore, base: Commit, left: Commit, right: Commit,
                  branch: str | None = None, ts_ms: int | None = None,
                  message: str | None = None) -> MergeResult:
    """三方合并 merge(base_commit, left, right) (spec 03 §6)。

    left 是合并目标侧, right 是来源侧; 成功时新 commit 的双亲 = [left, right]。
    """
    # 平凡情形
    if left.root == right.root:
        return MergeResult(True, left, [], False, 0, 0, 0)
    if left.root == base.root:  # fast-forward: 目标没动, 直接指向 right
        return MergeResult(True, right, [], True, 0, 0, 0)
    if right.root == base.root:  # 来源没动: no-op, 目标原地保持
        return MergeResult(True, left, [], False, 0, 0, 0)

    n0 = len(store)
    st_l = pl.DiffStats()
    lp = pl.diff(store, base.root, left.root, st_l)
    st_r = pl.DiffStats()
    rp = pl.diff(store, base.root, right.root, st_r)

    # 归并两路 patch 流 (key 有序)
    mutations: list = []
    conflicts: list = []
    i = j = 0
    while i < len(lp) and j < len(rp):
        (k1, _, n1), (k2, _, n2) = lp[i], rp[j]
        if k1 < k2:
            mutations.append((k1, n1))
            i += 1
        elif k2 < k1:
            mutations.append((k2, n2))
            j += 1
        else:
            if n1 == n2:  # 同改同值(含双删) → 收敛, 非冲突
                mutations.append((k1, n1))
            else:         # 同改不同值 / 一删一改 → 冲突
                conflicts.append((k1, n1, n2))
            i += 1
            j += 1
    mutations.extend((k, n) for k, _, n in lp[i:])
    mutations.extend((k, n) for k, _, n in rp[j:])

    if conflicts:
        return MergeResult(False, None, conflicts, False, len(lp), len(rp),
                           len(store) - n0,
                           {"diff_left": st_l, "diff_right": st_r})

    # 无冲突: 基于 left 应用 right patches → 新根 → 双亲 commit
    merged_root = pl.apply_mutations(store, left.root, mutations)
    branch = branch or left.branch
    msg = message if message is not None else f"merge '{right.branch}' into '{left.branch}'"
    commit = _write_commit(store, merged_root, [left.addr, right.addr],
                           max(left.height, right.height) + 1, branch, msg, ts_ms)
    return MergeResult(True, commit, [], False, len(lp), len(rp),
                       len(store) - n0, {"diff_left": st_l, "diff_right": st_r})


def _write_commit(store, root, parents, height, branch, message, ts_ms=None) -> Commit:
    ts = int(time.time() * 1000) if ts_ms is None else ts_ms
    data = encode_commit(root, parents, height, ts, branch, message)
    addr, _ = store.put_object(pl.TAG_COMMIT, data)
    return Commit(addr, root, list(parents), height, ts, branch, message)


# ---------------------------------------------------------------------------
# Repo: catalog = prolly map {branch_name -> commit_addr}
# ---------------------------------------------------------------------------

class Repo:
    """catalog(分支表)本身是一棵 prolly map ⇒ 创建分支只是写一条指针。

    对应 spec 03 §4/§5: CREATE BRANCH = catalog 写一条 (O(1), 数据树零复制);
    commit = 新 COMMIT chunk + catalog CAS (原型里无并发, 直接写)。
    """

    def __init__(self, store: pl.ChunkStore | None = None):
        self.store = store if store is not None else pl.ChunkStore()
        self.catalog_root: Hash20 | None = None

    # --- catalog 读写 ---
    def head(self, branch: str) -> Commit | None:
        raw = pl.get(self.store, self.catalog_root, branch.encode())
        if raw is None:
            return None
        return decode_commit(Hash20(raw), self.store.get(Hash20(raw)))

    def checkout(self, branch: str) -> Commit:
        c = self.head(branch)
        if c is None:
            raise KeyError(f"分支不存在: {branch!r}; 现有分支: {self.branches()}")
        return c

    def branches(self) -> list[str]:
        return [k.decode() for k, _ in pl.iter_items(self.store, self.catalog_root)]

    def _set_head(self, branch: str, c: Commit) -> None:
        self.catalog_root = pl.apply_mutations(
            self.store, self.catalog_root, [(branch.encode(), bytes(c.addr))])

    # --- 分支操作 ---
    def commit(self, branch: str, root, message: str = "", ts_ms=None) -> Commit:
        parent = self.head(branch)
        parents = [parent.addr] if parent else []
        height = parent.height + 1 if parent else 1
        c = _write_commit(self.store, root, parents, height, branch, message, ts_ms)
        self._set_head(branch, c)
        return c

    def create_branch(self, name: str, from_branch: str = "main",
                      message: str | None = None, ts_ms=None) -> Commit:
        """CREATE BRANCH: 只新建 1 个 commit chunk (空差异) + catalog 指针节点,
        不复制任何数据 chunk (O(1))。"""
        src = self.checkout(from_branch)
        msg = message if message is not None else f"branch '{name}' created from '{from_branch}'"
        c = _write_commit(self.store, src.root, [src.addr], src.height + 1, name, msg, ts_ms)
        self._set_head(name, c)
        return c

    def merge(self, source: str, target: str, base_commit: Commit,
              ts_ms=None, message=None) -> MergeResult:
        """MERGE BRANCH source INTO target; base_commit 为分叉点 commit。"""
        left = self.checkout(target)
        right = self.checkout(source)
        res = merge_commits(self.store, base_commit, left, right, target, ts_ms, message)
        if res.ok and res.commit is not None:
            self._set_head(target, res.commit)
        return res


# ---------------------------------------------------------------------------
# 场景与统计
# ---------------------------------------------------------------------------

def _fresh_repo(n_rows: int, seed: int = 1):
    """建 n_rows 行单表 + main 初始 commit。返回 (store, repo, base_commit, oracle)。"""
    store = pl.ChunkStore()
    repo = Repo(store)
    rng = random.Random(seed)
    items = []
    for i in range(n_rows):
        kb = i.to_bytes(8, "big")
        items.append((kb, b"row|" + rng.randbytes(12)))
    root = pl.build_map(store, items)
    base = repo.commit("main", root, "init")
    return store, repo, base, dict(items)


def _branch_and_edit(repo, base, name, edits, message):
    repo.create_branch(name, "main")
    root = pl.apply_mutations(repo.store, base.root, sorted(edits.items()))
    return repo.commit(name, root, message)


def scenario_disjoint_merge(n_rows: int = 10_000) -> dict:
    """两分支各改不相交 100 行 → merge 成功零冲突, 结果 = 并集。"""
    store, repo, base, oracle = _fresh_repo(n_rows)
    keys = sorted(oracle)
    idx_a = [(37 * i) % n_rows for i in range(100)]
    idx_b = [(n_rows // 2 + 37 * i) % n_rows for i in range(100)]
    assert len(set(idx_a)) == 100 and len(set(idx_b)) == 100 and not set(idx_a) & set(idx_b), \
        "场景前提: 两组行索引各 100 个且不相交"
    edit_a = {keys[i]: b"edit-A|" + keys[i] for i in idx_a}
    edit_b = {keys[i]: b"edit-B|" + keys[i] for i in idx_b}

    c1 = _branch_and_edit(repo, base, "agent1", edit_a, "A: 100 rows")
    c2 = _branch_and_edit(repo, base, "agent2", edit_b, "B: 100 rows")
    res = merge_commits(store, base, c1, c2, branch="main")
    assert res.ok, f"不相交修改不应冲突: {res.conflicts[:5]}"
    assert not res.fast_forward, "双侧都有修改, 不应走 fast-forward"
    assert res.commit.parents == [c1.addr, c2.addr], "merge commit 应带双亲"

    merged = dict(pl.iter_items(store, res.commit.root))
    expected = dict(oracle)
    expected.update(edit_a)
    expected.update(edit_b)
    assert merged == expected, \
        f"合并结果 ≠ base+A+B: {len(merged)} vs {len(expected)}"
    return {
        "n_rows": n_rows,
        "left_patches": res.left_patches,
        "right_patches": res.right_patches,
        "conflicts": len(res.conflicts),
        "new_chunks": res.new_chunks,
        "total_chunks": len(store),
        "subtree_skips": (res.diff_stats["diff_left"].subtree_skips,
                          res.diff_stats["diff_right"].subtree_skips),
        "leaf_pairs_compared": (res.diff_stats["diff_left"].leaf_pairs_compared,
                                res.diff_stats["diff_right"].leaf_pairs_compared),
    }


def scenario_conflicts() -> dict:
    """同键同值收敛; 同键不同值冲突; 一删一改冲突。"""
    out = {}
    keys = sorted(_fresh_repo(500, seed=2)[3])
    k43 = keys[43]

    # (1) 同改不同值 → 冲突
    store, repo, base, _ = _fresh_repo(500, seed=2)
    c1 = _branch_and_edit(repo, base, "b1", {k43: b"value-from-b1"}, "b1")
    c2 = _branch_and_edit(repo, base, "b2", {k43: b"value-from-b2"}, "b2")
    res = merge_commits(store, base, c1, c2)
    assert not res.ok, "同键不同值必须报冲突"
    assert res.conflicts == [(k43, b"value-from-b1", b"value-from-b2")], \
        f"冲突键不符: {res.conflicts}"
    assert res.commit is None, "冲突时不应产生 merge commit"
    out["modify_modify"] = res.conflicts

    # (2) 同改同值 → 收敛, 不冲突
    store, repo, base, oracle = _fresh_repo(500, seed=2)
    c1 = _branch_and_edit(repo, base, "b1", {k43: b"converged"}, "b1")
    c2 = _branch_and_edit(repo, base, "b2", {k43: b"converged"}, "b2")
    res = merge_commits(store, base, c1, c2)
    assert res.ok, f"同键同值应收敛: {res.conflicts}"
    assert pl.get(store, res.commit.root, k43) == b"converged"
    assert len(res.conflicts) == 0
    out["converge_same_value"] = True
    out["converge_new_chunks"] = res.new_chunks

    # (3) 一删一改 → 冲突
    store, repo, base, _ = _fresh_repo(500, seed=2)
    c1 = _branch_and_edit(repo, base, "b1", {k43: None}, "b1 deletes")
    c2 = _branch_and_edit(repo, base, "b2", {k43: b"b2-wins?"}, "b2 modifies")
    res = merge_commits(store, base, c1, c2)
    assert not res.ok and res.conflicts == [(k43, None, b"b2-wins?")], \
        f"一删一改应冲突: {res.conflicts}"
    out["delete_modify"] = res.conflicts

    # (4) 双删 → 收敛删除
    store, repo, base, oracle = _fresh_repo(500, seed=2)
    c1 = _branch_and_edit(repo, base, "b1", {k43: None}, "b1 deletes")
    c2 = _branch_and_edit(repo, base, "b2", {k43: None}, "b2 deletes")
    res = merge_commits(store, base, c1, c2)
    assert res.ok and pl.get(store, res.commit.root, k43) is None, "双删应收敛为删除"
    out["delete_delete"] = True
    return out


def scenario_fast_forward() -> dict:
    """目标分支无新提交 → fast-forward; 来源无新提交 → no-op。"""
    store, repo, base, oracle = _fresh_repo(1_000, seed=4)
    repo.create_branch("feature", "main")
    fk = (2_000).to_bytes(8, "big")
    froot = pl.apply_mutations(store, base.root, [(fk, b"feature-row")])
    fc = repo.commit("feature", froot, "feature work")

    # main 未动: left.root == base.root ⇒ FF 到 feature
    main_head = repo.checkout("main")
    res = merge_commits(store, base, main_head, fc)
    assert res.ok and res.fast_forward and res.commit is fc, "main 未动应 fast-forward 到 feature"
    assert res.new_chunks == 0, "fast-forward 不应产生任何 chunk"

    # feature 未动 (main 反向合并) : right.root == base.root ⇒ no-op, 保持 fc
    res2 = merge_commits(store, base, fc, main_head)
    assert res2.ok and not res2.fast_forward and res2.commit is fc, \
        "来源未动应识别为 no-op (目标保持不变)"
    assert res2.new_chunks == 0, "no-op 不应产生任何 chunk"
    return {"ff_ok": True, "ff_new_chunks": res.new_chunks, "noop_ok": res2.ok}


def scenario_branch_o1() -> dict:
    """CREATE BRANCH O(1): 新 chunk = 1 个 commit 对象 + ≤3 个 catalog 节点;
    数据树 chunk 零复制 (新 commit 的 root == 父 root)。"""
    store, repo, base, _ = _fresh_repo(5_000, seed=6)
    n0 = len(store)
    commits0 = store.count_by_tag(pl.TAG_COMMIT)
    c = repo.create_branch("agent/session-1", "main")
    n1 = len(store)
    new_commits = store.count_by_tag(pl.TAG_COMMIT) - commits0
    assert new_commits == 1, f"create_branch 应恰好新建 1 个 commit chunk, 实际 {new_commits}"
    assert 1 <= n1 - n0 <= 4, \
        f"create_branch 应 O(1) (commit 1 + catalog 节点 ≤3), 实际新建 {n1 - n0}"
    assert c.root == base.root, "新分支 commit 的 root 必须复用父 root (数据 chunk 零复制)"
    # checkout O(1): 只是 catalog 点查 + 单 chunk 解码
    assert repo.checkout("agent/session-1") == c
    assert repo.checkout("main") == base
    # 两分支头 root 相同 ⇒ diff 为空 ⇒ 后续 merge 平凡
    assert pl.diff(store, repo.checkout("main").root, c.root) == []
    return {"new_chunks_total": n1 - n0, "new_commit_chunks": new_commits,
            "catalog_nodes": n1 - n0 - new_commits}


_CACHE: dict = {}


def report() -> str:
    if "report" in _CACHE:
        return _CACHE["report"]
    d = scenario_disjoint_merge(10_000)
    cf = scenario_conflicts()
    ff = scenario_fast_forward()
    o1 = scenario_branch_o1()
    lines = [
        "== branch/merge ==",
        f"  1e4 行表, 两分支各改不相交 100 行: patches L/R = "
        f"{d['left_patches']}/{d['right_patches']}, 冲突 {d['conflicts']}, "
        f"merge 新建 chunk {d['new_chunks']}/{d['total_chunks']}",
        f"    diff 子树整跳 (左,右) = {d['subtree_skips']}, 叶对逐条比较 = {d['leaf_pairs_compared']}",
        f"  同键不同值 → 冲突 {cf['modify_modify']}; 一删一改 → 冲突 {cf['delete_modify']}",
        f"  同键同值 → 收敛 (0 冲突, 新建 {cf['converge_new_chunks']} chunk); 双删 → 收敛删除",
        f"  fast-forward: ok={ff['ff_ok']} 新建 {ff['ff_new_chunks']} chunk",
        f"  create_branch O(1): 新建 {o1['new_chunks_total']} chunk "
        f"(commit {o1['new_commit_chunks']} + catalog 节点 {o1['catalog_nodes']}), 数据树零复制",
    ]
    _CACHE["report"] = "\n".join(lines)
    return _CACHE["report"]


# ---------------------------------------------------------------------------
# 单元测试
# ---------------------------------------------------------------------------

def test_commit_codec_roundtrip():
    store = pl.ChunkStore()
    root = Hash20.of(b"tree-root")
    parents = [Hash20.of(b"p1"), Hash20.of(b"p2")]
    data = encode_commit(root, parents, 7, 1725600000000, "main", "hello merge")
    addr, _ = store.put_object(pl.TAG_COMMIT, data)
    c = decode_commit(addr, store.get(addr))
    assert c.root == root and c.parents == parents, f"commit roundtrip 失败: {c}"
    assert (c.height, c.ts_ms, c.branch, c.message) == (7, 1725600000000, "main", "hello merge")
    tagged = bytes([pl.TAG_COMMIT]) + encode_commit(None, [], 1, 0, "b", "")
    c2 = decode_commit(Hash20.of(tagged), tagged)
    assert c2.root is None, "空树根 (全零占位) 应解码为 None"


def test_branch_lifecycle_o1():
    r = scenario_branch_o1()
    assert r["new_commit_chunks"] == 1, f"commit 层应恰好 1 个新 chunk: {r}"


def test_scenario_disjoint_merge():
    r = scenario_disjoint_merge(10_000)
    assert r["conflicts"] == 0, f"不相交修改应零冲突: {r}"
    assert r["left_patches"] == 100 and r["right_patches"] == 100, \
        f"各 100 行修改应产生恰好 100 条 patch: {r['left_patches']}/{r['right_patches']}"
    assert r["new_chunks"] < 0.05 * r["total_chunks"], \
        f"合并新建 chunk {r['new_chunks']} 应远小于总量 {r['total_chunks']} (结构共享)"


def test_scenario_conflicts():
    r = scenario_conflicts()
    assert len(r["modify_modify"]) == 1 and r["converge_same_value"] and r["delete_delete"], f"{r}"


def test_scenario_fast_forward():
    r = scenario_fast_forward()
    assert r["ff_ok"] and r["ff_new_chunks"] == 0 and r["noop_ok"], f"{r}"


def test_merge_applied_to_left_uses_structural_sharing():
    """合并 = 基于 left 应用 right patches ⇒ 未受影响子树地址复用。
    构造一个大表, 两分支各改 1 行, 验证 merge 后大部分 chunk 与 left 分支共享。"""
    store, repo, base, _ = _fresh_repo(50_000, seed=9)
    keys = sorted(pl.iter_items(store, base.root))  # 直接取 (k, v) 流
    k1 = keys[10_000][0]
    k2 = keys[40_000][0]
    c1 = _branch_and_edit(repo, base, "b1", {k1: b"one"}, "b1")
    c2 = _branch_and_edit(repo, base, "b2", {k2: b"two"}, "b2")
    left_levels = pl.collect_levels(store, c1.root)
    left_addrs = {a for recs in left_levels.values() for _, a, _ in recs}
    res = merge_commits(store, base, c1, c2)
    assert res.ok, f"应零冲突: {res.conflicts}"
    merged_levels = pl.collect_levels(store, res.commit.root)
    merged_addrs = [a for recs in merged_levels.values() for _, a, _ in recs]
    shared = sum(1 for a in merged_addrs if a in left_addrs)
    assert shared / len(merged_addrs) > 0.95, \
        f"merge 结果应与 left 共享 >95% chunk: {shared}/{len(merged_addrs)}"
    assert pl.get(store, res.commit.root, k1) == b"one"
    assert pl.get(store, res.commit.root, k2) == b"two"


TESTS = [
    test_commit_codec_roundtrip,
    test_branch_lifecycle_o1,
    test_scenario_disjoint_merge,
    test_scenario_conflicts,
    test_scenario_fast_forward,
    test_merge_applied_to_left_uses_structural_sharing,
]

if __name__ == "__main__":
    for t in TESTS:
        t()
        print(f"PASS {t.__name__}")
    print(report())
