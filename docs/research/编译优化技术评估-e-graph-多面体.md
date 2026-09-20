# 编译优化技术采纳评估：e-graph（equality saturation）与多面体（2026-09-19）

> 触发：评估现代编译器优化技术（egg/e-graph、多面体）在 dendro
> 优化器的采纳机会。纪律：先记录后实现——本文只评估不给票。

## 一、e-graph / equality saturation

### 现状（2025-2026 核查）

- **工业采纳加速但不在 RDBMS**：equality saturation 的生产落地在
  编译器/EDA/张量程序（egg CACM 2026 综述；EuroLLVM 2026 有生产
  编译器采纳报告）。**主流 RDBMS 无生产采纳**——数据库侧仍是
  学术原型（Cambridge MPhil 2023、Rust Magazine 教程、learned
  rewrite arXiv 2407.12794）；2025 工业界查询优化叙事由 learned/AI
  方法主导。
- **egg/egglog 生态活跃**：egglog（Datalog×EqSat 统一，2023）、
  **Oatlog（PLDI 2025，AOT 编译的 e-graph 引擎——性能量级改善）**、
  Relational E-matching（用数据库 join 技术加速 e-matching——
  有趣的数据库↔编译器双向流动）。
- 经典对照（egg Discussion #189）：e-graph ≈ Cascades memo 的
  泛化推广；Cascades 更"聪明/专用"，EqSat 更通用（规则序无关、
  无 phase 分层）。

### dendro 的适配面（诚实）

现状：优化器 = 六条**启发式重写链**（0.2µs 总耗时——perf 实测），
序敏感（改序即变计划），无代价搜索（join_order 是贪心）。

| 维度 | EqSat 能带来什么 | dendro 现实 |
|------|-----------------|-------------|
| 重写序问题 | 规则并发饱和，序无关 | 六条链固定序——**问题真实存在但量级小**（规则少，序已手工调好） |
| 连接序枚举 | e-graph 天然承载 DP/枚举 + 代价提取 | 现贪心在 ≥3 表 join 有改进空间——**最实份额** |
| 规则正确性 | 等价类构造即保证（重写=等式） | 现有 canonical form 差分已提供等价验证——互补 |
| 开销 | e-graph 构建+饱和 µs-ms 级 | **TP 点查 25µs 预算下不可接受**；仅复杂 AP 查询可负担 |

### 采纳判定（建议）

1. **不做全量替换**（六条链 → EqSat）：规则集小、序已调优、
   0.2µs 的现状没有痛点；全量替换引入 egg 依赖 + 饱和开销，
   收益不成比例。
2. **定点采纳——join_order 的 EqSat 化**（≥3 表时）：e-graph 承载
   连接序/交换/结合的等价类 + 代价函数提取（替代贪心）。门槛：
   `optimize=on` 且 join 数 ≥3（TP/点查零开销）。egg 或 egglog
   皆可（Rust 生态原生）。**这是有真实份额的一块**。
3. **配套先建**：①代价模型可信（stat_prop 已有基数估计基础）
   ②`cambium.perf_stages` 增加 `optimize` 的分查询形态记录
   （复杂查询的优化耗时观测）③差分轴（optimize on/off）已就绪。
4. 长期（≥v2）：若规则集增长到 15+ 条/序维护成本显性化，重评
   全量 EqSat（egglog/Oatlog 的 AOT 路线值得跟踪）。

## 二、多面体模型（polyhedral）

### 现状

- 数据库侧：**无主流引擎采纳**——检索未发现 bridging 工作
  （DuckDB 向量化引擎与多面体分属两条线）。
- 编译器侧成熟（ISL/Pluto/MLIR affine）：affine 循环变换、tiling、
  依赖导向并行化/向量化。

### dendro 的适配面（诚实）

多面体的前提是 **affine 循环嵌套 + 依赖分析**——对应到我们
**运行时生成代码**的场景，而非 SQL 重写：

1. **JIT 方向（真正的接口）**：此前 pgrust/JIT 调研（arXiv
   2405.11361）指的表达式/算子编译。若落地 JIT，生成循环
   （扫描-过滤-投影融合核）可用 MLIR affine/polly 做平铺+向量化。
   **多面体是 JIT 的下游优化，不是独立采纳项**。
2. **静态 Rust 代码**（ScalarProgram 解释器、CBF 解码循环）：
   已由 rustc/LLVM 自动向量化覆盖；多面体增益的典型来源
   （跨迭代依赖的 tiling）在解码循环里不成立（FSST 字典访问
   非 affine）。
3. **向量化执行（DuckDB 式 batch 列算）**：与多面体正交——
   我们 P0 的窄列/Arrow 批已在此路上，无需多面体。

### 采纳判定（建议）

**现在不采纳**；作为 **JIT 立项的子项**记录（JIT → MLIR →
affine/polly 平铺向量化，仅对生成的算子核）。JIT 本身按
ClickBench 数据再决策（当前瓶颈在 SQL 层/parse，不在执行核）。

## 三、其他现代技术（顺带评估）

- **learned 优化（AI 基数估计/计划选择）**：2025 工业主流叙事，
  但依赖查询负载语料——dendro 阶段过早；stat_prop+直方图是
  正确的地基（已在）。
- **Relational E-matching**：反向流动（DB 技术→e-graph 加速）——
  若采纳 egg 可白拿，非独立项。

## 信源

- egg（CACM 2026 综述）：https://cacm.acm.org
- egg Discussion #189（Calcite×EqSat×Cascades）：
  https://github.com/egraphs-good/egg/discussions/189
- egglog（Better Together, 2023）/ Oatlog（PLDI 2025）：
  https://pldi25.sigplan.org
- Learned rewrite（arXiv 2407.12794）
- 向量化 vs 代码生成（Kersten VLDB 2018）：https://www.vldb.org
- 多面体向量化（INRIA 2009 综述）：https://inria.hal.science
- 验证的多面体代码生成（2021）：https://xavierleroy.org
