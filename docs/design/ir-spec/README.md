# TP/AP 统一 IR 与执行层——设计 Spec

- 状态：**评审稿 v1**（2026-09-15）。本文集是实施前的权威设计；
  与其冲突的早期讨论以本文集为准（演进史见父文档
  `docs/design/TP_AP统一IR与动态派发.md`）。
- 评审要求：**慎重**。每份 spec 末尾列出"实现前必须回答"的问题；
  汇总见 `08-里程碑风险与待决.md`。

## 阅读顺序

| 文件 | 内容 | 读者 |
|------|------|------|
| [01-决策记录.md](01-决策记录.md) | 六条 ADR：选了什么、拒了什么、重开条件 | 所有人，必读 |
| [02-逻辑IR与放置属性.md](02-逻辑IR与放置属性.md) | 逻辑计划、placement/coverage 属性、类型格 | 核心 |
| [03-标量层ScalarStep.md](03-标量层ScalarStep.md) | 表达式步列表：编译、双后端求值、NULL/collation 语义 | 核心 |
| [04-执行层ChunkPlane.md](04-执行层ChunkPlane.md) | Chunk、push 管线协议、算子、四类 Source、Sink | 核心 |
| [05-派发与EXPLAIN.md](05-派发与EXPLAIN.md) | coverage 推理、派发规则、代价 v0、EXPLAIN | 核心 |
| [06-缓存与失效.md](06-缓存与失效.md) | 两级计划缓存、键设计、失效矩阵、内存界 | 核心 |
| [07-测试与不变量.md](07-测试与不变量.md) | 差分测试（force_source）、回归清单、验收线 | 测试/评审 |
| [08-里程碑风险与待决.md](08-里程碑风险与待决.md) | v2b/v2c-1..3、风险表、待决问题（需拍板） | 决策者 |

## 一句话架构

**一个逻辑 IR**（PlanNode + placement×coverage 属性束）
**→ 唯一执行方言**（ChunkPlane：push 管线，可变执行 Chunk，边界转 RecordBatch）
**→ 三层数据源**（MemtxSource / CbfSource / ProllySource，+MainPlusDelta 归并源）
**→ 派发 = coverage 推理纯函数**；点查 = Source→Sink 退化管线。
**标量层**：ScalarStep 步列表（PG11 EEOP 同构），prepare 编译一次，
`eval_row` / `eval_chunk` 双后端。

## 存储模型立场（为什么这样设计）

TP 与 AP **不是一份数据的两种形态**。三层放置：Delta（memtx+WAL 尾巴，
行式，小工作集）/ Main（CBF 列存段，bulk 权威）/ History（prolly 全历史，
只服务分支/合并/时间旅行，不占派发热路径）。经典 HTAP（TiFlash 等）
双完整副本的代价被明确拒绝。

## 硬约束（所有设计决策的边界）

1. **行为等价迁移**：v2b/v2c-1 不改任何查询语义；差异只允许出现在
   性能，不允许出现在结果。
2. **TP 验收线**：点查吞吐 ≥ 现状 325k txn/s 的 85%（276k）。
3. **既有不变量不得破坏**：gap-free watermark、Q-14 显式事务可见性、
   时间旅行门控（AS OF 不静默读当前——R6 P0）、I-H1 差分、I-C4 段退休界。
4. **资源纪律（S-3）**：计划缓存有界；步列表缓存有界；管线执行受
   语句超时/取消/内存守卫约束。
