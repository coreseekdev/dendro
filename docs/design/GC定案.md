# GC 定案——对象生命周期与回收机制（P1-4 / P0-3 收口）

- 日期：2026-09-08
- 状态：✅ 已实现（`engine.rs::gc_sweep` + manifest 墓碑）
- 回归：`crates/dendro-server/tests/gc.rs`（3 测试）
- 关联：评审 P0-3（列存旧段删除时序）、评审 A6（append-only 无 GC）、SPEC 01 §5

---

## 0. 一句话

**墓碑（tombstone）随"新 manifest 停止引用"同一版本原子发布，保留窗口过后由
GC pass 删除；恢复路径通过 manifest 记录的 WAL 起始段号容忍前缀空洞。**

## 1. 背景

Dendro 是 append-only 存储：列存段、WAL 段、manifest 版本只增不删。
不做 GC 的后果（评审 A6）：

- 每次列存全量重建（≥8 段触发）留下 8+ 个孤儿段——只写库存储增长加速；
- WAL 段永不删除——长寿命写者线性增长；
- manifest 每次提交一个 JSON——版本数无限增长。

但"直接删"是错的（评审 P0-3 指出的崩溃窗口）：删除必须与新引用的发布协调，
且必须容忍滞后的读者（读副本 / k8s 中滚动重启的实例 / 读路径缓存）。

## 2. 对象生命周期总表

| 对象 | 何时成为垃圾 | 何时登记墓碑 | 何时物理删除 | 机制 |
|------|------------|------------|------------|------|
| 列存段（全量重建被替换）| `materialize_delta` 全量重建返回 old_paths | 与新 checkpoint 的 manifest **同一版本原子发布** | `at_ms + retention` 后的 gc_sweep | 墓碑 |
| WAL 旧 epoch 目录（epoch < 当前）| 接管者首次 checkpoint：covered_seq ≥ 旧 epoch 全部 ts | 同上（首次 checkpoint 时逐段 LIST 枚举登记） | 同上 | 墓碑 |
| WAL 当前 epoch 前缀段（seg < ckpt 帧所在段）| 每次 checkpoint：checkpoint 帧之前的段全部 covered | 同上（批量为 [first, seg_now)） | 同上 | 墓碑 + `BranchHead.wal_first_seg` 推进 |
| manifest 旧版本 | 版本 ≤ latest − 16 | 不需要（非引用语义，见 §4.3） | 每次 gc_sweep 直接删 | `retained(latest, 16)` 接入 |
| CAS chunk（prolly 节点 / commit 对象）| — | — | **v2**（诚实声明见 §5） | — |

## 3. 核心不变式

1. **不删被引用对象**：墓碑登记发生在 `update_manifest`（乐观 CAS）内部——
   与"新版本停止引用该对象"是**同一个 JSON 对象的一次原子 PUT**。
   任何崩溃窗口内，旧 manifest 引用的段都未被登记，更未被删除。
2. **保留窗口覆盖滞后读者**：`gc_retention_ms`（默认 24h，`<0` 禁用；
   serve `--gc-retention-ms`）。持有旧 manifest 的副本只要在窗口内追平，
   其引用的对象全部仍在。
3. **恢复完整性**：
   - 当前 epoch：checkpoint 帧所在段之后的段绝不被回收（回收集合 =
     `[wal_first_seg, ckpt_seg)`，帧序保证其内全部 ts ≤ covered_seq）；
   - 前缀空洞安全：`replay_branch` 对当前 epoch 从 `head.wal_first_seg`
     起探测（`probe_tail(lo)` 语义：lo 不存在 → 该 epoch 无段）；
   - 旧 epoch 目录整体消失：replay 对不存在目录回放 0 帧（探测返回 0）。
4. **GC 有界**：单次 sweep 至多物理删除 256 个墓碑对象（大批量分多轮），
   不阻塞提交路径；checkpoint 尾部与打库时各跑一次。

## 4. 机制细节

### 4.1 墓碑清单

```rust
// Manifest（serde default，旧版本 manifest 兼容）
pub tombstones: Vec<Tombstone>   // { path, at_ms }
```

清单随 manifest 版本化——天然持久、可并发（乐观 CAS）、崩溃一致。
登记时按 path 去重；物理删除成功后从清单压缩移除（又一次 manifest 提交，
冲突重试由 `update_manifest` 内建）。

### 4.2 WAL 段回收与恢复的配合

- `BranchHead.wal_first_seg`（serde default 0 → 视作 1）记录当前 epoch
  的有效起始段，与墓碑登记同一版本发布；
- `WalWriter.first_seg`（AtomicU64）是写者进程内的同一事实，
  checkpoint 时二者一起推进（`fetch_max`，单调）；
- 段内帧序 = seq 序 ⇒ checkpoint 帧之前的段必然全部 covered
  （`commit_mu` 串行化保证 checkpoint 时无并发 append）。

### 4.3 manifest 旧版本

manifest 版本是"单调版本号 + Create 条件写"，不是引用语义：
旧版本只被 `load_latest` 的探测/LIST 路径看到，不构成数据引用。
保留最近 16 个版本（滞后读者兜底），其余直接删除。
`load_latest`：进程内 cached 探测兼容空洞；缓存失效后 LIST 兜底天然兼容。

### 4.4 CAS chunk（v2 范围，诚实声明）

prolly 树节点与 commit 对象是内容寻址、跨分支共享的：
- 回收需要"从全部保留根做可达性分析"（跨分支、跨历史），成本高；
- 且 commit 历史是 time travel（`AS OF`，P1-10）的数据来源。
**v1 不回收任何 CAS chunk**。v2 方案候选：引用计数入 manifest 快照 /
每 N 个 checkpoint 做一次全根标记。在实现前，全历史 chunk 持续累积——
这是当前已知且明示的存储增长来源。

## 5. 运维面

| 参数 | 默认 | 含义 |
|------|------|------|
| `--gc-retention-ms` / `DbOptions.gc_retention_ms` | 24h | 墓碑保留窗口；`<0` 禁用回收 |
| 保留 manifest 版本数 | 16 | 硬编码（`gc_sweep`） |
| 单批删除上限 | 256 | 硬编码（有界延迟） |

## 6. 回归证明（tests/gc.rs）

| 测试 | 证明 |
|------|------|
| `gc_columnar_segments_after_retention_window` | 9 次 checkpoint 触发全量重建；**窗口内旧段原样存在**（P0-3 崩溃窗口保证）；窗口后仅剩 1 段；数据完整 |
| `gc_wal_epochs_prefix_and_recovery` | 旧 epoch 目录：窗口内不删、窗口后删除、**删除后重开恢复完整**；当前 epoch 前缀段（含 ckpt 帧的段之前的段）：登记 → 删除 → 从 `wal_first_seg` 恢复完整 |
| `gc_manifest_versions_keep_recent` | 25 次 checkpoint 后版本数 ≤17、最老版本已删、最新数据完整 |
