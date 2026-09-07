# TASK.md — 待办任务清单

> 来源：架构评审（`docs/discussions/2026-09-07-架构评审-首轮.md`）+ SOTA 调研修订路线。
> 状态标记：⬜ 待做 · 🔧 进行中 · ✅ 已完成
> 优先级：P0（正确性）> P1（完整性）> P2（质量）> P3（远期）

---

## P0 — 正确性

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| ~~P0-1~~ | ~~WAL flush 失败丢帧 + 挂死~~ | ✅ | flush_now 改为 clone→PUT→成功才消费 |
| ~~P0-2~~ | ~~decode_row/FrameIter 对坏数据 panic~~ | ✅ | 定宽字段长度守卫已加 |
| ~~P0-3~~ | ~~列存旧段删除时序（崩溃窗口）~~ | ✅ | 延迟到 manifest 发布后 |
| ~~P0-4~~ | ~~P1 fencing 文档诚实化~~ | ✅ | 设计文档/代码注释/测试头已修正 |
| ~~P0-5~~ | ~~GitHub Actions CI~~ | ✅ | 见 `.github/workflows/ci.yml` |

## P1 — 完整性

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| P1-1 | 只读打开模式（`read_only: bool`，不领 epoch/不起 WAL writer） | ⬜ | `engine.rs::Database::open` |
| P1-2 | fencing 运行时拒写（commit 前检查租约过期/epoch 匹配） | ⬜ | `engine.rs::commit_tx` |
| P1-3 | fencing 续期线程（TTL/3 周期 renew） | ⬜ | `fence.rs::renew` 接线 |
| P1-4 | GC：manifest 旧版本 + 旧 chunk + 旧 WAL 段回收 | ⬜ | `ManifestStore::retained` 接入 |
| P1-5 | SQL 语义修复（S1–S5，见评审 §1.3） | ⬜ | WHERE 吞错/键碰撞/溢出/单 INSERT 重复 pk |
| P1-6 | JOIN/派生表测试（scan.rs hash_join 零覆盖） | ⬜ | 补 slt + 集成测试 |
| P1-7 | 多线程 OCC 并发测试（同 key 冲突 + 不冲突 key 均成功） | ⬜ | `tests/concurrent.rs` |
| P1-8 | 真 kill 崩溃恢复测试（子进程 SIGKILL） | ⬜ | 替代 `drop(db)` 模拟 |
| P1-9 | fencing 安全性质测试（旧实例 epoch 被抢后写被拒） | ⬜ | 补 multi_node.rs 断言 |
| P1-10 | time travel SQL 入口（`AS OF` / `FOR SYSTEM_TIME`） | ⬜ | 数据层已支持，缺 SQL 面 |
| P1-11 | `/metrics` `/readyz` 端点（负载自感知 M1/M2） | ⬜ | k8s 探针依赖 |

## P2 — 质量 / 性能 / 证据链

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| P2-1 | slt 语料扩展（接入 sqllogictest-corpus，哪怕 1%） | ⬜ | `tests/slt/` |
| P2-2 | AP 向量化执行器（或修订 SPEC 措辞为"行式解释器"） | ⬜ | SPEC 00 G4 |
| P2-3 | 基准证据链整改（环境指纹/中位数/恢复率断言/README CI 生成） | ⬜ | `benches/results/` |
| P2-4 | 共识选型文档修订（消除"不从 Raft 开始"vs"锁定 raft-rs"矛盾） | ⬜ | 决策理由已补，正文需同步 |
| P2-5 | 死代码清理（`new_putid`/`retry()`/`RootView`/`chunk_seen` 等） | ⬜ | `cargo fix` + 手动 |
| P2-6 | 性能：`Node.key()` 去分配 / NodeStore 真正 LRU / commit_mu 与 flush 解耦 | ⬜ | 热路径优化 |
| P2-7 | GRAMMAR.md 修正（WITH ⬜、CHECKPOINT ✅、differential 目录删除） | ⬜ | 与代码对齐 |
| P2-8 | AGENTS.md 架构清单补 kv/journal/consensus/fence | ⬜ | 文档同步 |

## P3 — 远期

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| P3-1 | Adjudicator + Journal 分布式实施（openraft 3 副本） | ⬜ | SOTA 调研 §3 |
| P3-2 | multi-region 强一致（Journal 多 Region 2+1） | ⬜ | 依赖基础设施 |
| P3-3 | 向量化列式执行器（Arrow 列式 filter/agg） | ⬜ | AP 性能 |
| P3-4 | criss-cross merge 修复（common_ancestor 遍历多父） | ⬜ | 低频场景 |
| P3-5 | blob 外置（大 value 不进 prolly 叶层） | ⬜ | 参考 lance blob |

---

## 当前冲刺目标（本周）

1. ~~P0 三项修复 + 测试~~ ✅
2. ~~CI 工作流~~ ✅
3. ~~文档诚信整改~~ ✅
4. P1-1 只读模式
5. P1-2 fencing 运行时拒写
6. P1-5 SQL 语义修复（S1–S5）
7. P1-6 JOIN 测试
