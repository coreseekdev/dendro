# TASK.md — 待办任务清单

> 来源：架构评审（首轮 + 第二轮，`docs/discussions/`）+ SOTA 调研修订路线。
> 状态标记：⬜ 待做 · 🔧 进行中 · ✅ 已完成
> 优先级：P0（正确性）> P1（完整性）> P2（质量）> P3（远期）
> **完成三要件**（见 discussions/README.md）：代码 + 回归测试 + 文档同步，同一提交；完成必须附证据。

---

## P0 — 正确性

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| ~~P0-1~~ | ~~WAL flush 失败丢帧 + 挂死~~ | ✅ | `92dcb35`/`f1bca77`：restore+retry；本提交收尾——计数器"成功清零/失败归还"、Bytes 去全量克隆、TRAILER_LEN 常量；**测试捕获真 bug：段号空洞**（失败也推进 cur_seg → probe_tail 连续性假设丢数据），已改为成功后才推进。回归：`tests/wal_corruption.rs::{flush_put_failure_no_frame_loss_no_hang, transient_flush_failure_self_heals}` |
| ~~P0-2~~ | ~~恢复路径对坏数据 panic~~ | ✅ | `92dcb35` FrameIter len 守卫 + underflow 守卫；回归：`tests/wal_corruption.rs`（坏 len / 坏 CRC / 截断 payload / 截断帧头 / ≤32B 撕尾容忍 / e2e open fail-fast），`decode_row` 定宽守卫随 `68de926` |
| ~~P0-3~~ | ~~列存旧段删除时序（崩溃窗口）~~ | ✅（止血） | `68de926` 删除推迟；真删除时序（发布新 manifest 后删本次替换段）并入 P1-4 GC 一并定案，勿两处口径 |
| ~~P0-4~~ | ~~P1 fencing 文档诚实化~~ | ✅ | `92dcb35` 设计文档/multi_node 头；本提交：fence.rs 模块注释随**运行时拒写实现**改写（不再是注释先行） |
| ~~P0-5~~ | ~~GitHub Actions CI~~ | ✅ | `53f2c74`；`f1bca77` 起 clippy 非阻塞过渡；本提交 clippy --all-targets 清零，恢复 `-D warnings` |

## 二轮评审收尾（2026-09-07 晚）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R2-1 | clippy 清零（107→0，含 --all-targets） | ✅ | 本提交；`cargo clippy --workspace --all-targets -- -D warnings` 绿 |
| R2-2 | S2 收尾（25P02 拒绝失败事务内语句） | ✅ | `92dcb35`/`f1bca77`：`sql/mod.rs` exec_batch 入口检查 |
| R2-3 | S6（WITH → 0A000，不再静默丢弃） | ✅ | `92dcb35`：`sql/mod.rs` `Statement::Query` with 检查 |
| R2-4 | S4 SPEC 07 偏离记录（整数升宽 Int64 / 溢出 22003） | ✅ | 本提交：`spec/07-sql-surface.md` §1 已知偏离段 |
| R2-5 | P0 配套回归测试（此前零交付） | ✅ | 本提交：`tests/wal_corruption.rs` 10 测试（含 flush 失败注入）+ `tests/multi_node.rs` 2 个 fencing 新测试 |

## P1 — 完整性

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| ~~P1-2~~ | ~~fencing 运行时拒写~~ | ✅ | 本提交：`engine.rs::Branch::fence_gate`——三个写入口（commit_tx / write_branch_commit / checkpoint_branch）在 commit_mu 内检查；过期 → 40001。回归：`tests/multi_node.rs::{fence_expired_writer_rejected, fence_renew_keeps_healthy_writer_writing}` |
| ~~P1-3~~ | ~~fencing 续期~~ | ✅ | 本提交：惰性续期（commit 路径，每 ttl/3 ≤1 次 PUT，失败仅告警下次重试；无后台线程——文档口径已同步） |
| P1-1 | 只读打开模式（不领 epoch、不起 WAL writer） | ⬜ | `engine.rs::Database::open`；读副本前置 |
| P1-4 | GC：旧段删除时序定案（P0-3 并入）+ manifest 旧版本 + 旧 chunk + 旧 WAL 段回收 + 孤儿段墓碑 | ⬜ | `ManifestStore::retained` 接入 |
| ~~P1-5~~ | ~~SQL 语义修复（S1–S6）~~ | ✅ | `53f2c74`/`92dcb35`/`f1bca77`；S4 偏离记录见上 |
| ~~P1-6~~ | ~~JOIN/派生表测试（hash_join 零覆盖）~~ | ✅ | 本提交：`tests/slt/dendro/008_join.slt`（INNER/LEFT/NULL 键/一对多/三表链/复合键/JOIN+GROUP BY/派生表）。语料当场暴露真 bug：sqlparser 0.62 把裸 `JOIN`(Join) 与 `INNER JOIN`(Inner) 分为不同枚举——标准写法 `A JOIN B` 直接报 not_supported，hash_join 此前经由该路径**不可达**。已修（scan.rs eval_from 匹配 Join/Inner、Left/LeftOuter） |
| P1-7 | 多线程 OCC 并发测试 | ⬜ | `tests/concurrent.rs` |
| P1-8 | 真 kill 崩溃恢复测试（子进程 SIGKILL） | ⬜ | 替代 `drop(db)` 模拟 |
| ~~P1-9~~ | ~~fencing 安全性质测试~~ | ✅ | 本提交：`fence_expired_writer_rejected` 即评审要的"旧实例写被拒"断言 |
| P1-10 | time travel SQL 入口（`AS OF` / `FOR SYSTEM_TIME`） | ⬜ | 数据层已支持，缺 SQL 面 |
| ~~P1-11~~ | ~~`/metrics` `/readyz` 端点~~ | ✅ | 本提交：`dendro-server/src/metrics.rs`（serve `--metrics-port`，默认 9469）。/metrics 暴露每驻留分支 pending_bytes（扩容信号）/watermark/durable 水位/lease 剩余 TTL；`Database::active_branches()` 只读快照，绝不懒加载分支。回归：`tests/metrics_endpoint.rs` |

## P2 — 质量 / 性能 / 证据链

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| P2-1 | slt 语料扩展（接入 sqllogictest-corpus，哪怕 1%） | ⬜ | `tests/slt/` |
| P2-2 | AP 向量化执行器（或修订 SPEC 措辞为"行式解释器"） | ⬜ | SPEC 00 G4 |
| P2-3 | 基准证据链整改（环境指纹/中位数/恢复率断言/README CI 生成） | ⬜ | `benches/results/` |
| P2-4 | 共识选型文档修订（消除正文与决策的矛盾） | ⬜ | 决策理由已补，正文需同步 |
| P2-5 | 死代码清理 | 🔧 | clippy 清零已带走大部分；余 `retry()`/`RootView` 等 |
| P2-6 | 性能：`Node.key()` 去分配 / NodeStore 真正 LRU / commit_mu 与 flush 解耦 | ⬜ | 热路径优化 |
| P2-7 | GRAMMAR.md 修正（WITH ⬜、CHECKPOINT ✅、differential 目录删除） | ⬜ | 与代码对齐 |
| P2-8 | AGENTS.md 架构清单补 kv/journal/consensus/fence | ⬜ | 文档同步 |

## P3 — 远期

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| P3-1 | Adjudicator + Journal 分布式实施（openraft 3 副本） | ⬜ | SOTA 调研 §3；seam 已留（`629d155`/`e3b0557`） |
| P3-2 | multi-region 强一致（Journal 多 Region 2+1） | ⬜ | 依赖基础设施 |
| P3-3 | 向量化列式执行器（Arrow 列式 filter/agg） | ⬜ | AP 性能 |
| P3-4 | criss-cross merge 修复（common_ancestor 遍历多父） | ⬜ | 低频场景 |
| P3-5 | blob 外置（大 value 不进 prolly 叶层） | ⬜ | 参考 lance blob |

---

## 当前冲刺目标

1. ~~P0 修复 + 回归测试~~ ✅（含二轮收尾）
2. ~~fencing 运行时拒写 + 续期~~ ✅
3. ~~P1-6 JOIN 测试（评审最看重）~~ ✅
4. P1-1 只读模式
5. P1-4 GC 定案（含 P0-3 真删除时序）
