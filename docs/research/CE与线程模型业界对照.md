# CE 与线程模型：业界对照及 dendro 改动范围评估（2026-09-18）

> 触发：ANALYZE（54ac903）落地后的两个追问——"CE 错误是什么 /
> 线程模型决策是什么"的延伸选型分析。评估口径 = 改动范围（模块/
> 文件面），不含工时。

## 1. 基数估计（CE）：四家对照

| 维度 | SQLite | DuckDB | PostgreSQL | MySQL | dendro 现状 |
|------|--------|--------|------------|-------|-------------|
| 统计存储 | sqlite_stat1（索引行数/组合 NDV 粗估）+ stat4（每索引 ~24 条样本行） | 段/列级 min/max + 行数（**刻意少统计**）+ ART 采样 | pg_statistic：NDV（精确/比例）、MCV+频次、等高直方图（默认 100 桶）、correlation；PG10+ 扩展统计（列组 NDV/MCV/函数依赖） | InnoDB 持久统计（采样页 → 每索引前缀 NDV）+ 8.0 直方图（纯等高，≤102 桶，JSON 存 column_stats）；**无 MCV** | ✅ CAS 侧车：精确 NDV + MCV(32) + 等高直方图(64) |
| 采集方式 | ANALYZE 全扫，写入侧不自动 | 写入时增量（段 footer） | **随机采样**（statistics_target×300×行为上限）+ autoanalyze 阈值触发 | 采样页（默认 20 页/表）+ 手动 ANALYZE | 全扫 + **手动**（v1 合同） |
| 等值估算 | 无索引时硬编码常数（~10%）+ stat4 样本 | 1/NDV 式 | MCV 命中 → 精确频次；未命中 → (1-ΣMCV)/(NDV-|MCV|) 的"未列值低于均值"启发 | 索引列 = 索引俯冲（B 树 dives 精确）；非索引 = 直方图桶频 | MCV → 1/NDV → 1/区间宽 ✅（PG 同构） |
| 范围估算 | stat4 样本边界计数 | min/max 区间 uniform（无直方图） | 直方图插值 + 桶内均匀 | 直方图插值（等高） | 直方图插值 → footer uniform ✅ |
| 对 CE 错误的补救 | —（靠手工 REINDEX/ANALYZE） | **动态 join filter 下推**（sideways information passing：执行期用 build 侧键集过滤 probe 侧扫描——绕开静态 CE）+ 运行时自适应（perfect hash join 等） | 扩展统计缓解跨列独立性假设；真实系统普遍靠 hint/人工 | optimizer_switch 开关人工干预 | SemiJoin（IN 子查询静态版）已具备同构雏形 |
| 弱项 | 样本太少、偏斜弱 | 静态 CE 弱（论文自述 almost no statistics）| 未列值低估/独立性假设 | 无 MCV、非索引列弱 | 陈旧性手动、大表全扫贵 |

**SOTA 判读**：PG 的 MCV+等高+NDV 三件套仍是生产骨架（dendro 已对齐），
增量方向是**采样采集 + 自动陈旧管理**；而现代趋势（DuckDB 路线）是
**少依赖静态 CE、多运行时自适应**——动态 join filter 用 build 侧真实
键集修正 probe 侧，天然免疫 CE 错误。对 dendro 含义：统计面已够用，
下一分钱花在动态过滤比堆更多统计值钱。

## 2. 线程模型：四家对照

| 维度 | SQLite | DuckDB | PostgreSQL | MySQL | dendro 现状 |
|------|--------|--------|------------|-------|-------------|
| 单位 | 连接即线程；**无查询内并行** | **morsel 驱动**（HyPer 血统）：work-stealing 线程池 = 核数；任务 = morsel（行块）；扫描共享原子游标、并行哈希 join（并行 build + 分区 probe）、分区聚合 | 进程/连接 + 查询内并行（9.6+）：leader + background **worker 进程**，Gather/Gather Merge 为边界；并行感知算子（并行顺扫共享块区间、并行哈希 join 11+、partial agg） | 线程/连接（可选线程池插件）；InnoDB 并行读线程（8.0.14+）仅限 CHECK TABLE/COUNT 等特定操作，**社区版无通用查询内并行**（AP 走 HeatWave 独立引擎） | 同步单线程、逐算子物化（exec_plan → TableView） |
| 切分 | — | morsel（动态工作窃取） | 静态块区间（无窃取） | 表空间级（仅扫描） | — |
| 取消/错误 | — | 算子内检查中断标志 | 信号 + flag | kill 信号 | cancel_token/deadline 单线程检查点 |
| 结果边界 | 全物化 | 流式 chunk（QueryResult） | Gather 汇聚物化 | 全物化 | 全物化 |
| 定位 | 嵌入式 OLTP（并行 = 多连接） | **嵌入式 OLAP 并行参照实现** | 服务端 OLTP/AP 混合并行 | OLTP 为主 | — |

**SOTA 判读**：DuckDB 的 morsel 驱动是嵌入式 OLAP 的事实参照（dendro
的 exec/pipeline.rs push 算子雏形正对该方向）；PG 的 Gather 模型是为
进程模型付出的折衷（DSM 分配 + worker 进程启动成本），dendro 无此
包袱，不必学。

## 3. dendro 改动范围评估

### 3.1 CE 侧（按业界对照列增量）

| 项 | 参照 | 改动范围 | 大小 |
|----|------|----------|------|
| 精确 NDV 接入 join 估算 | PG | stats.rs `col_ndv`/`ndv_range` 改读 analyze 产物（现仍用区间宽上界）——**已有一切，只差接线** | 小 |
| MCV 未列值启发 | PG | eq_selectivity 未命中分支改 (1-Σfreq)/(ndv-|mcv|) | 小 |
| 采样式 ANALYZE | PG/MySQL | analyze_impl 大表（行数 > 阈值）改随机采样口径；eval_query 加采样限制或 memtx 直接抽样 | 中 |
| 自动陈旧（autoanalyze） | PG | 写路径记 n_mod_since_analyze（TableEntry 或内存）+ 阈值触发后台重分析——涉及 catalog 写面与调度点选择 | 中 |
| **动态 join filter**（运行时 SemiJoin 推到 probe 扫描） | DuckDB（sideways IP） | hash_join build 完成后把键集注入 probe 侧 Scan 的扫描提示/位图——scan_table 与 join.rs 接线；与 SemiJoin 探测器复用 | 中大（执行层，价值最高） |
| 扩展统计（列组 NDV） | PG10+ | 分析模型扩列组维度 | 远期 |

### 3.2 线程侧（三档路线，改动面递增）

| 档 | 内容 | 改动范围 | 大小 |
|----|------|----------|------|
| A. I/O 并行（S3 预取） | 后台线程预取下一段/下一 range 到缓冲，扫描线程到时即热——**不动执行模型** | objstore 层（cached.rs 或新 prefetch.rs）+ table_scan_opt 接线；机会式派发语义框架（调研已成文） | 小中 |
| B. Scan 分片 + partial agg（PG Gather 精神、无进程包袱） | 顺扫按块区间分片多线程，Filter 随片并行；聚合改 partial/combine 两段；join 仍串行 | scan/scan_table.rs（分片游标）、exec/pipeline.rs（AggOp 分区合并）、plan_exec Scan/Filter 臂；cancel_token 跨线程传播 | 中 |
| C. 全 morsel 化（DuckDB 路线） | push 管线接管全部算子（并行 hash join 分区 probe、流式结果、工作窃取池） | exec/** 全面扩展 + plan_exec 各臂改流式 + TableView 物化边界上移到顶层 + 会话/查询线程配额策略（pgrust 调研的查询调度器问题） | 大 |

**推荐序**：CE 侧先做两小项（NDV 接线 + 未列值启发）+ 动态 join filter
（中大但价值最高）；线程侧按 A → B → C 三档走，A 与 S3 场景收益直接
挂钩且零架构风险，C 在有真实负载数据前不立项。

## 4. 信源

- DuckDB：[Join Order Optimization with (Almost) No Statistics
  (Ebergen, 2022)](https://blobs.duckdb.org/papers/tom-ebergen-msc-thesis-join-order-optimization-with-almost-no-statistics.pdf)、
  [Optimizers: The Low-Key MVP (2024)](https://duckdb.org/2024/11/14/optimizers.html)、
  [Query Rewriting and Optimization slides（join filter pushdown =
  sideways information passing，执行期动态）](https://blobs.duckdb.org/slides/DiDi-08.pdf)、
  [Join Operations](https://duckdb.org/docs/lts/guides/performance/join_operations.html)、
  [DuckDB internals: join reordering（DPccp）](https://www.alibabacloud.com/blog/duckdb-internals---part-7-join-reordering-optimization_602899)
- PG / MySQL / SQLite 架构事实为稳定公开知识（pg_statistic/
  autoanalyze、innodb_stats_persistent + 8.0 直方图/索引俯冲、
  sqlite_stat1/4）；延续《优化器SOTA调研》《pgrust与JIT设计调研》
  既有结论
- dendro 侧：`crates/dendro-core/src/sql/stats.rs`（ANALYZE/估算）、
  `crates/dendro-core/src/exec/pipeline.rs`（push 算子雏形）、
  `crates/dendro-columnar/src/footer.rs`（zone map）
