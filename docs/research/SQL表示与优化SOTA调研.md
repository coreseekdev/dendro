# SQL 表示与优化：SOTA 方案调研（决策用）

- 日期：2026-09-15
- 背景：dendro 现状 = sqlparser-rs AST 直接解释执行 + 轻量规则优化器（optimize.rs 常量折叠/布尔化简）+ PK/范围下推 + 计划缓存 v2a（hash→AST）。此前决策"暂不优化 SQL 解析、聚焦数据链路"已兑现（计划缓存 2.8×）。本文回答下一个问题：**表示层（IR）与优化器往哪走**。
- 结论先行：**表示层走"绑定计划（bound plan）"中间态**（AST → 名字解析/类型检查后的可执行形态，v2b），**不建 VDBE、不上 Cascades**；AP 侧保留自研列存、把 DataFusion 作为增长期的决策点而非现在的依赖；Substrait 只做互操作观察位；JIT（Cranelift）和等式饱和（egg）是研究观察旗，不动。

## 1. 表示层五条路线（SOTA 全景）

| 路线 | 代表 | 优势 | 代价 | 适配 dendro？ |
|------|------|------|------|--------------|
| AST 直接解释 | **dendro 现状**；各嵌入式引擎起步态 | 零 IR 维护成本；执行期实时绑定（DDL 漂移免疫，计划缓存 I-E4 已固化） | 每次执行重复名字解析/类型检查；无算子级优化空间 | ✅ 已在用，是合理起点 |
| VDBE 字节码 VM | SQLite；**Turso/Limbo**（Rust 重写，字节码可移植——Doom 跑在 VDBE 上；单查询 506ns，比 SQLite 快 ~20%）| prepared 语句一次编译；指令流稳定（可做协议级兼容）；async 化可行（io_uring） | 完整 VM 是大工程；优化发生在编译到字节码时，规则空间受指令集约束 | ❌ 战略不匹配：dendro 的 SQLite 兼容仅限 Embed API 语义层，不是字节码兼容；建 VM 的收益（微秒级解释开销）计划缓存已吃掉大半 |
| 逻辑/物理计划 | **DataFusion**（Rust 生态事实标准）；DuckDB | 算子级优化（下推/连接重排/物理选择）；生态成熟（函数库/join 实现/Parquet 等） | 依赖重；快照/事务集成需要自定义 TableProvider + 快照钩子；线程模型需对齐 | 🔶 AP 增长期的**决策点**（见 §4），不是现在的依赖 |
| 绑定计划（bound AST） | SQLite prepare 语义的轻量版；各类 prepared statement 实现 | 一次解析+名字解析+类型检查，N 次执行；保留 AST 结构（增量改动小） | 无算子图，join 优化受限 | ✅ **v2b 推荐**（§3）|
| 自定义 IR + 编译（Umbra 谱系） | HyPer "Flying Start"（ICDE'18：字节码解释 ↔ LLVM 编译按行数自适应）；Umbra "medium-term"（先出字节码、热路径升级机器码，自研编译器）；Dynamic Blocks（ADMS'22）；Cranelift（代码生成比 LLVM 快一个量级，Rust 原生，CGO'24 有编译期开销对比研究） | 极致执行性能；自适应切换是 HTAP 正解形状 | 编译器基础设施是数人年级工程 | ❌ 现阶段明确不做；Cranelift 记为"若做 JIT 的唯一候选后端" |

## 2. 优化器三条流派（2025 共识）

| 流派 | 现状 | 证据 |
|------|------|------|
| **Cascades/Memo** | 生产 SOTA，20+ 年主干：SQL Server（1989 至今）、Calcite、Orca、CockroachDB、Microsoft Fabric（SIGMOD'25 仍以 MEMO 为计划空间核心结构）| 目标导向、代价驱动；短板是终止启发式与计划空间剪枝 |
| **等式饱和（e-graph）** | 研究 SOTA（rewrite 阶段挑战者）：egg/egglog、Relational E-matching（POPL'22）、Aurora（RL 引导，2024）、Incremental Equality Saturation（EGRAPHS'25，对"重复/增量到达的查询"有专门意义）| 非破坏性广度探索，适合逻辑重写；短板是 e-graph 爆炸与代价提取困难；社区共识是"与 Cascades 混合"而非替代 |
| **学习型/反馈闭环** | 趋势而非流派：Microsoft《Query Optimization in the Wild》（SIGMOD Record, 2025-10）总结两大趋势——优化与执行之间**更紧的反馈闭环**（自适应执行）、学习型组件嵌入生产优化器（Databricks predictive optimization 等）| 需要工作负载统计基础设施，dendro 当前不具备，仅观察 |

**对 dendro 的含义**：当前优化需求（常量折叠、谓词下推、PK/范围下推、时间旅行门控）全是规则级，规则优化器 + 启发式足够；join 优化（连接顺序）的前提是先有 hash join 算子——顺序是"先算子后优化器"，Cascades/eqsat 都排不上近期队列。

## 3. dendro 路线建议：分阶段

### v2b（近期推荐，工作量小收益实）：绑定计划
计划缓存 v2a 缓存的是**纯 AST**（执行期实时解析名字/类型——正确性上完美，性能上留了名字解析的重复开销）。v2b 把 prepared statement 与热查询升级为**绑定计划**：解析一次 → 对着当次 catalog 做名字解析（表 ID、列索引）+ 类型检查 → 缓存绑定形态。执行时直接按列索引取值。
- 正确性守卫沿用 I-E4 纪律但方向反转：绑定计划**含** schema 绑定，所以必须有失效机制——catalog 版本号（manifest version / schema chunk 地址）进缓存键，DDL 后自动 miss。这与 v2a 的"DDL 漂移免疫"是互补的两档：未绑定缓存（快、无失效）与绑定计划（更快、版本键控）。
- 这一步做完，TP 热路径的 SQL 开销已接近 SQLite prepare 微路径，**VDBE 的存在理由被进一步消解**。

### v3（AP 增长期决策点）：DataFusion or 自研深化
触发条件（满足其一才启动评估）：join/聚合类 AP 查询占比显著；需要 Parquet 互操作；函数生态成为用户诉求。
- 走 DataFusion：dendro 的分支/快照/水位集成点 = 自定义 `TableProvider`（scan 时钉住 manifest 版本 + 时间旅行门控语义进 custom scan node）；CBF 投影作为其数据源之一。
- 走自研深化：向量化算子（P2-2）+ hash join + hash 聚合，保持零重依赖。
- 决策判据：**HTAP 的差异化在快照/分支语义与存储，不在算子库**——DataFusion 的价值是买算子生态，代价是把执行线程模型和内存管理交给外部。若自研算子在 benchmark 上达到 DataFusion 的 70%+，优先自研（保持单进程嵌入式形态与确定性）。

### 观察旗（不启动，记录判据）
- **Substrait**：跨引擎计划交换标准，DataFusion/DuckDB/Velox 已内置 producer/consumer。对 dendro 的潜在价值：agent 场景下"外部生成计划、dendro 执行"的互操作位。判据：有外部引擎互操作需求才接；接法 = datafusion-substrait 桥，不自研。
- **Cranelift JIT**：仅当热点表达式解释成为实测瓶颈且向量化不足时评估；它是 Rust 原生 JIT 唯一候选后端。
- **等式饱和 / Aurora / 学习型优化器**：需要 join 规模与工作负载统计后才有意义；保持阅读即可。

## 4. 表示层教训（optd 作者亲述，直接可抄）

skyzh（optd 作者）2025-02 复盘：初版用"一个大 enum 包所有关系节点"（Calcite 式），在 Rust 里付出三倍代价——每个节点要定义**树形态、Memo 形态、模式匹配形态**三份结构，加一个节点改三处。教训固化成的设计：

```rust
pub struct RelNode<T: RelNodeType, D: RelAttrType> {
    pub typ: T,                          // 用户自定义节点类型
    pub children: Vec<Arc<RelNode<T, D>>>, // 统一子节点向量
    pub data: Arc<D>,                    // 节点属性（表 ID、列引用等）
}
```

框架只对 T 做比较/哈希/克隆，永不 match 其变体；树↔Memo↔绑定三种表示只换 children 容器类型。**对 dendro 的含义**：若 v3 选择自研计划 IR，从第一天就用这个泛形（把 dendro 特有属性——分支、快照版本、时间旅行门控——放进 D 而非节点变体）；若走 DataFusion，这个教训说明"自定义逻辑节点"（UserDefinedLogicalNode）要用同样纪律。

## 5. 与既有决策的一致性检查

- "暂不优化 SQL 解析、聚焦数据链路"（用户既定决策）：v2b 是数据链路的收尾件（消除执行期名字解析），不是回头优化解析器——一致。
- "追加模式值得"（用户裁定）、内容寻址存储：与任何表示层选择正交，无冲突。
- 计划缓存 v2a（I-E4：纯 AST 无失效需求）：v2b 的绑定计划是**新增一档**，不替换 v2a——未参数化热查询继续走 v2a，prepared/高频查询走 v2b。
- HTAP 双引擎（TP 行式解释 / AP Arrow 列式）：这本身就是粗粒度的"自适应执行"（Umbra 谱系的 HTAP 形状），SOTA 方向一致；细化（表达式级 JIT）排观察旗。

## 引用来源

- [Turso（原 Limbo）——SQLite 的 Rust 重写与 VDBE 字节码](https://turso.tech/blog/introducing-limbo-a-complete-rewrite-of-sqlite-in-rust) / [仓库](https://github.com/tursodatabase/turso)
- [Adaptive Execution of Compiled Queries（HyPer Flying Start, ICDE 2018）](https://15721.courses.cs.cmu.edu/spring2019/papers/19-compilation/kohn-icde2018.pdf) / [Umbra DB](https://umbra-db.com/) / [Dynamic Blocks（ADMS 2022）](https://www.adms-conf.org/2022-camera-ready/ADMS22_schmidt.pdf) / [编译器框架编译期开销对比（CGO 2024）](https://aengelke.net/pubs/2403-cgo.pdf) / [Cranelift](https://cranelift.dev/)
- [Cascades（CMU 15-799 Spring 2025）](https://15799.courses.cs.cmu.edu/spring2025/slides/05-cascades.pdf) / [Query Optimization in the Wild（Microsoft, SIGMOD Record 2025-10）](https://arxiv.org/abs/2510.20082) / [Microsoft Fabric DW（SIGMOD 2025）](https://dl.acm.org/doi/pdf/10.1145/3788853.3803075)
- [Aurora：RL + 等式饱和（2024）](https://arxiv.org/pdf/2407.12794) / [Relational E-matching（POPL 2022）](https://ztatlock.net/pubs/2022-popl-rematch/2022-popl-rematch.pdf) / [Incremental Equality Saturation（EGRAPHS 2025）](https://pldi25.sigplan.org/details/egraphs-2025-papers/4/Incremental-Equality-Saturation) / [egg 社区讨论：Cascades vs eqsat](https://github.com/egraphs-good/egg/discussions/189)
- [cmu-db/optd（2.0 优化器即服务）](https://github.com/cmu-db/optd) / [optd-original（Cascades + DataFusion 集成，已停主线）](https://github.com/cmu-db/optd-original) / [Plan Representation 教训（skyzh, 2025-02）](https://www.skyzh.dev/blog/2025-02-06-optimizer-lesson-01/)
- [Substrait 采用现状](https://www.data-landscape.com/standards/substrait/) / [DataFusion Substrait crate](https://docs.rs/datafusion-substrait) / [一份计划三引擎执行实例](https://medium.com/@omri-levy/one-query-plan-three-different-engines-e5dc74aeb52f)
- [DataFusion 2025 代价模型/自适应优化方向（issue #14373）](https://github.com/apache/datafusion/issues/14373)
