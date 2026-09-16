# 优化器 SOTA 调研（2024-2025）

> 调研时间：2026-09-16。触发：join reorder（O-4'）落地后对代价模型/
> 鲁棒性的方向确认。结论先行：**当前保守门控设计与 SOTA 方向一致**
> （鲁棒性 > 完美代价模型），非等值 d^(2/3) 与 Statistics Propagation
> 记为可落地增强点。

## 1. 论文要点

### 1.1 "Debunking the Myth of Join Ordering: Toward Robust SQL Analytics"
**SIGMOD 2025** | [arXiv:2502.15181](https://arxiv.org/abs/2502.15181) | [ACM](https://dl.acm.org/doi/10.1145/3725283) | 36+ 引用

- **核心论点**：join order 本身常被高估——对估算**误差的鲁棒性**
  比找到"最优" join order 更重要
- **方法**：Robust Predicate Transfer——在查询间传递选择率信息，
  使计划在估算误差下仍保持合理（plan bouquet 方向的延伸）
- **对 dendro 的启示**：
  - 保守门控（无统计不重排 / 裸名门控 / DISTINCT 门控）正是
    "宁可不优化也不优化错"的实践 ✓
  - 未来方向：当同一 workload 反复执行时，从历史实际行数（EXPLAIN
    ANALYZE 的 actual rows）反馈校正选择率——plan feedback loop

### 1.2 "Still Asking: How Good Are Query Optimizers, Really?"
**PVLDB Vol. 18, 2025** | [PDF](http://www.vldb.org/pvldb/vol18/p5531-viktor.pdf) | Leis et al.

- 2015 年 JOB (Join Order Benchmark) 论文的十年回顾
- **结论**：基数估算误差仍是代价优化器的**核心痛点**，即使现代
  系统（DuckDB、HyPer）也未根本解决
- **对 dendro 的启示**：
  - est 精度永远不完美——差分体系保证结果正确性比 est 精度更重要 ✓
  - EXPLAIN ANALYZE 的 est vs actual 同行呈现（已落地）正是持续
    验证估算质量的面板

### 1.3 "Join Order Optimization with (Almost) No Statistics"
**MSc Thesis, VU Amsterdam 2022** | [PDF](https://blobs.duckdb.org/papers/tom-ebergen-msc-thesis-join-order-optimization-with-almost-no-statistics.pdf) | Tom Ebergen

DuckDB 的 join order 估算器设计（**最直接可参考的实现**）：

- **分母法**：card = numerator / denominator，不用乘法链
- **等值谓词**：denominator ×= effective_d（等值列的 distinct 数）
- **非等值谓词**：denominator ×= effective_d^(2/3)——不等式过滤
  不会像等值那样强（不消重），也不会完全无效（d^0 = 无效果），
  2/3 是经验指数
- **FK-PK 假设**：等值 join 假设遵循最常见的 FK-PK 模式
- **效果**：Parquet 上端到端 ~18%，其他 ~25%
- **对 dendro 的启示**：
  - 我们当前用 `|A|·|B|/max(ndv_l, ndv_r)`（经典 System R 形式）；
    DuckDB 的分母法在**多谓词叠加**时更稳定（连乘变分母连加的
    逻辑等价但溢出控制更好）
  - **非等值 d^(2/3)** 是可直接落地的增强——当前 `range_selectivity`
    的 uniform 假设可改为此式（记为待做）

### 1.4 CardBench: Benchmark for Learned Cardinality Estimation
**Google, 2024** | [OpenReview](https://openreview.net/forum?id=Nu8b9C1xcr)

- 20 个真实数据库、数千查询的学习型基数估算基准
- **启示**：学习型估算是研究热点但生产落地仍少；当前统计面
  （min/max/rows/null_count）+ uniform 假设是务实的 v1 基线

### 1.5 "Extensible Query Optimizers in Practice"
**Microsoft Research, Dec 2024** | [PDF](https://www.microsoft.com/en-us/research/wp-content/uploads/2024/12/Extensible-Query-Optimizers-in-Practice.pdf)

- Bao、Lero、FORCE ORDER 等生产实践综述
- **启示**：生产系统趋向"混合经典/学习"而非纯学习——dendro 的
  规则框架 + 保守门控正是经典路径；学习型留为远期

### 1.6 VLDB 2023 Tutorial: Join Order Selection with Deep RL
**Yan et al.** | [PDF](https://www.vldb.org/pvldb/vol16/p3882-yan.pdf)

- 覆盖 Neo、Bao、Balsa、HybridQO、Lero、LOGGER 等学习型方法
- **启示**：远期参考——dendro 的 EXPLAIN ANALYZE metrics（NodeMetric）
  已预留了训练数据采集接口

## 2. 生产系统参考实现

### 2.1 DuckDB（本地有源码：`/home/nzinfo/src.db/ref-projects/duckdb`）

**优化器管线**（blog 2024-11 + 源码）：

| 规则 | 作用 | dendro 对应 |
|------|------|------------|
| Expression Rewriter | 常量折叠/move constants（`x+1=6` → `x=5`）| optimize.rs 表达式规则（v1 基础） |
| Filter Pushdown | 下推至首次引入列的算子 + **等值条件复制到另一列** | O-1 谓词下推（单源限定名，无等值复制——可借鉴） |
| Filter Pull-Up | 上拉过 join 再下推实现跨表谓词传播 | 无——需 Filter{Join} 重写框架增强 |
| Join Order | 消 cross product、少统计估算器 | O-4'（贪心 INNER 链 + 待决边）✓ |
| TopN | ORDER BY + LIMIT → O(M+N log N) | O-5 top-N 有界堆 ✓ |
| IN Clause Rewriter | 单值→`=`；小范围→范围；大→MARK/HASH | 无（v1 IN 走步列表） |
| **Statistics Propagation** | 等值 join 两侧 min/max 传播在对侧生成新过滤器 | **无——可落地**（stats 面已有 min/max） |
| Reorder Filters | 廉价谓词先执行 | 无（谓词序 = 合取项序） |
| **Join Filter Pushdown** | build 侧 join 键 min/max 作为 probe 侧扫描过滤器 | **无——与 Statistics Propagation 同族** |

**基数估算器源码**（`src/optimizer/join_order/cardinality_estimator.cpp`，
1003 行）：

```cpp
// 比较类型比率（核心公式——可直接移植 dendro stats.rs）
static double ApplyComparisonRatio(double base_denom, ExpressionType type, double effective_d) {
    switch (type) {
    case COMPARE_EQUAL:
        return base_denom * effective_d;          // 等值：分母 ×= ndv
    case COMPARE_LESSTHAN: // ... 非等值族
        return base_denom * pow(effective_d, 2.0 / 3.0);  // 指数 2/3
    }
}
```

**对 dendro 的可落地公式**（stats.rs `range_selectivity` 增强）：
```
非等值谓词 d^(2/3) 公式（替代 uniform 假设）：
sel_non_eq = 1 - (effective_d)^(2/3) / effective_d
           = 1 - effective_d^(-1/3)
其中 effective_d = 区间宽 [min, max] 的 ndv 近似
```

### 2.2 Snowflake Optima Planning
**Blog** | [链接](https://www.snowflake.com/en/blog/engineering/snowflake-optima-planning-query-performance/)

- 从查询执行历史学习改进计划（plan feedback loop）
- **启示**：dendro 的 EXPLAIN ANALYZE actual rows 可作为反馈源
  （远期：自动校正 scan_est 选择率）

### 2.3 CMU optd（Cascades 研究框架）
**GitHub** | [cmu-db/optd-original](https://github.com/cmu-db/optd-original)

- Rust Cascades 优化器，与 DataFusion 集成
- **启示**：dendro 的规则框架（固定顺序 pass）是简化版；Cascades
  的 memo + 探索式搜索是远期方向（当前贪心够用且可验证）

## 3. 与 dendro 现状的对照

| 维度 | SOTA 方向 | dendro 现状 | 差距 |
|------|----------|-----------|------|
| join order | 鲁棒性 > 最优性 | 保守门控（无统计/裸名/DISTINCT 均不重排）| 一致 ✓ |
| 基数估算 | 分母法 + d^(2/3) | 乘法链 + uniform | **可落地**：非等值 d^(2/3) |
| 谓词传播 | Statistics Propagation / Join Filter Pushdown | 仅单源合取下推 | **可落地**：等值列 min/max 传播 |
| 估算验证 | est vs actual 可观测 | EXPLAIN ANALYZE 同行呈现 | 一致 ✓ |
| 学习型 | 混合（经典为主+反馈） | 纯经典 | 远期：plan feedback |
| 验证体系 | 各家测试基准（JOB/CardBench） | 三重锁定（差分/golden/round-trip） | 自有体系，更强 |

## 4. 待做清单（从 SOTA 提炼）

| 优先级 | 项 | 来源 | 说明 |
|--------|---|------|------|
| P1 | 非等值选择率 d^(2/3) | DuckDB Ebergen | `range_selectivity` 的 uniform 假设替换——更保守 |
| P2 | Statistics Propagation | DuckDB blog | 等值 join 侧 min/max → 对侧扫描过滤器（stats 面已有） |
| P2 | Join Filter Pushdown | DuckDB blog | build 侧键 min/max → probe 侧扫描过滤 |
| P3 | 等值谓词复制下推 | DuckDB Filter Pushdown | `a.id = b.id AND a.x = 5` → b.id 已知可推 b 侧过滤 |
| P3 | Filter Pull-Up 跨表传播 | DuckDB blog | 需 Filter{Join} 重写框架增强 |
| 远期 | Plan Feedback Loop | Snowflake Optima / Robust Predicate Transfer | EXPLAIN ANALYZE actual → 选择率校正 |
| 远期 | Cascades 探索式搜索 | CMU optd | 当前贪心够用且可验证 |

---
*生成方式：WebSearch + WebFetch + 本地 ref-projects/duckdb 源码阅读。
本地 DuckDB 路径：`/home/nzinfo/src.db/ref-projects/duckdb`（仅参考，
MIT 协议，不复制代码）。*
