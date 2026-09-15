# TP/AP 统一 IR 与执行层——设计 Spec

- 状态：**评审稿 v1.2**（2026-09-15；v1.1 经双 agent 对照审视——
  DuckDB/Polars 本地源码（7D）+ PG/SQLite/MySQL 机制（7M），两处
  正确性缺口（prepared 失效、CatalogCache 新鲜度）与执行层接口缺口
  （finish/EOS）已全部回写——清单见 08 §0.1/§0.2）。本文集是实施前的权威设计；
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
| [09-文本表示.md](09-文本表示.md) | LLVM/MLIR 式纯文本 IR：round-trip/确定性/可审阅，golden 测试 | 核心 |
| [10-绑定与名字解析.md](10-绑定与名字解析.md) | 视图展开/派生表/伪表/列名 lenient 回退（评审 P1-2）| 核心 |
| [11-DML与迁移边界.md](11-DML与迁移边界.md) | UPDATE/DELETE 走向、双路径看护、旧路径删除条件（评审 P1-4）| 核心 |

## 一句话架构

**一个逻辑 IR**（PlanNode + placement×coverage 属性束）
**→ 唯一执行方言**（ChunkPlane：push 管线，可变执行 Chunk，边界转 RecordBatch）
**→ 三层数据源**（MemtxSource / CbfSource / ProllySource，+MainPlusDelta 归并源）
**→ 派发 = coverage 推理纯函数**；点查 = Source→Sink 退化管线。
**标量层**：ScalarStep 步列表（PG11 EEOP 同构），prepare 编译一次，
`eval_row` / `eval_chunk` 双后端。
**文本表示**：dendro.ir v1（LLVM/MLIR 式），round-trip + 确定性 + golden
测试（09）——IR 可 diff、可审阅、可跨进程检视。

## 存储模型立场（为什么这样设计）

TP 与 AP **不是一份数据的两种形态**。三层是**对象放置**模型：
Delta（memtx 活跃尾巴，行式，小工作集）/ Main（CBF 列存段，bulk 权威）/
History（prolly 全历史，服务分支/合并/时间旅行）。**注意（评审 P0-2
修正）**：prolly 当前树同时是"当前读"的组成部分（点查四段合成的第三
段）——"History 不占派发热路径"指时间旅行语义，不是说当前读绕开树。

## 硬约束（所有设计决策的边界）

1. **行为等价迁移**：v2b/v2c-1 不改任何查询语义；差异只允许出现在
   性能，不允许出现在结果。
2. **TP 验收线**：点查吞吐 ≥ 现状 325k txn/s 的 85%（276k）。
3. **既有不变量不得破坏**：gap-free watermark、Q-14 显式事务可见性、
   时间旅行门控（AS OF 不静默读当前——R6 P0）、I-H1 差分、I-C4 段退休界。
4. **资源纪律（S-3）**：计划缓存有界；步列表缓存有界；管线执行受
   语句超时/取消/内存守卫约束。
