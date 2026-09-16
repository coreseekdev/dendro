# SQL 层架构审视：推翻重设计 vs 渐进统一（2026-09-16）

> 触发：用户曾表示"接受整个架构推翻重新设计"；此后 P0 特性
> （WHERE 子查询/CTE/外键/UNIQUE/UPSERT/CHECK/窗口函数/递归 CTE）
> 已在现架构上全部落地，530 workspace Rust 测试 + 34 SLT 全绿
> （实测 `cargo test --workspace` 530 passed；SLT 034 个文件）。
> 本报告基于实现过程暴露的结构性问题做裁决。
>
> 方法：通读 sql/mod.rs（1791 行）、scan.rs（5100 行）、optimize.rs
> （1825 行）、ir/plan.rs（1142 行）、ddl.rs（1274 行）的 dispatch 与
> 关键函数体；对照《解析器架构评审-2026-09-16》《SQL表示与优化SOTA调研》
> 《ScopeDB调研》三份既有研究。所有结论附代码位置。

## 结论先行

**不推翻，渐进统一到计划路径（方案 b）。** 理由压缩为四点：

1. 存储引擎层（prolly/memtx/WAL/列存/分支/fencing）健康且与 SQL 层
   正交——"推翻"能动的只有 SQL 层，而 SQL 层恰好已有目标架构的
   ~70% 骨架（Plan IR + exec_plan + dispatch 派发器 + 差分测试三轴）。
2. 五个结构问题中四个（双路径、文本回环、错误吞噬、AST 注入）的
   根因是同一个：**Plan IR 表达力不足，导致特性要么回落 AST 路径、
   要么以 AST 重写注入**。补 IR（Window/Distinct/子查询/结构化
   AggCall）比分期偿还好，但远小于重写。
3. 530 测试 + 34 SLT 是重写无法带走资产的最大部分——每个测试名
   （R9-1/Q-9/#23/#28…）都是一次真实 bug 的教训，重写等于把教训
   重新买一遍。
4. 全盘重设计的等待期内双路径继续共存，比今天更糟；渐进方案每阶段
   可验证、测试永不红。

估算：完整统一 40–60 人日（约 6 周，单人）；可在阶段 2 后停获利。

---

## 1. 现状架构与依赖分析

### 1.1 执行管线全景（文本图）

```
pgwire / mywire / embed
   │
   ▼
exec_batch (mod.rs:127) ── 分支语句文本拦截 ── 游标文本拦截 (mod.rs:1682)
   │                        （DECLARE/FETCH/CLOSE 按空白切 token 识别）
   ▼
计划缓存 v2a：hash(SQL)→AST，桶清 (mod.rs:153)
   │
   ▼
exec_statement (mod.rs:596) ── 权限门 privs::enforce ── DDL 事务门
   │
   ├─ Query ──► scan::exec_query (scan.rs:23) ──► eval_query (scan.rs:92, ~700 行)
   │                │
   │                ├─ reject_offset_comma / reject_multi_from（诚实拒绝）
   │                ├─ CTE 展开
   │                │    ├─ 递归：eval_recursive_cte (scan.rs:4912)
   │                │    │     不动点迭代 + VALUES 字面量注入 AST
   │                │    └─ 非递归：optimize::expand_ctes (optimize.rs:1714)
   │                │          纯 AST 替换 Table→Derived
   │                ├─ VALUES 直接求值
   │                ├─ 集合操作：计划尝试(scan.rs:142-167) ║ AST 回落(递归两侧)
   │                ├─ WHERE 子查询内联 optimize::inline_subqueries
   │                │     （标量→常量 / IN→InList 字面量 / EXISTS→Bool；
   │                │       仅非相关——is_correlated 结构判定 + 42703 嗅探兜底）
   │                │
   │                ├─【计划路径】optimize_enabled 且 build_plan 成功：
   │                │     build_plan (plan.rs:135) → 6 重写
   │                │     (in_list/pushdown/stat_prop/eq_copy/join_order/filter_order)
   │                │     → plan_exec_covered? (scan.rs:1654) ── 覆盖判定
   │                │     → plan_scan_masks → exec_plan (scan.rs:1931)
   │                │        节点：Values/Scan/Filter/Join/Aggregate/
   │                │              Project/Sort/Limit/SetOp（无 Window/
   │                │              Distinct/子查询/CTE 节点）
   │                │        Project{[Filter]Aggregate} 组合模式 →
   │                │        exec_aggregate_composite（镜像 AST 聚合分支）
   │                │
   │                └─【AST 路径】eval_from (scan.rs:1132)
   │                     │  首因子扫描 + join 链（hash_join / hash_join_left）
   │                     │  下推合取项回灌 apply_predicates
   │                     ▼
   │                WHERE：apply_predicates_q (scan.rs:818)
   │                     三机制：ScalarProgram+FilterOp(>64行) / 紧循环 / AST 回落
   │                     ▼
   │                窗口：collect_window_calls + eval_windows (scan.rs:4658/4717)
   │                     合成列 __wN + 投影重写为 Identifier（仅此路径）
   │                     ▼
   │                GROUP BY：AggOp 管线 ║ group_aggregate 行式（force_agg 轴）
   │                     ▼ HAVING → project → DISTINCT(dedup_rows) →
   │                ORDER BY(SortOp/top-N) → LIMIT/OFFSET
   │
   ├─ Insert ──► ddl::exec_insert (ddl.rs:533)
   │                VALUES 逐行 / INSERT..SELECT 走 eval_query
   │                逐行：handle_conflict(UPSERT，三段冲突检查)
   │                      → insert_row (ddl.rs:850)
   │                         PK NULL → CHECK(重 parse 文本) → NOT NULL
   │                         → FK 点查 check_fk_parents → UNIQUE 全表扫
   │
   ├─ Update/Delete ──► table_scan_by_name + 谓词过滤 + 重写
   ├─ CreateTable ──► 约束文本化入 catalog（check_exprs: Vec<String>）
   ├─ CreateView ──► m.views[name] = query.to_string()（文本）
   └─ 视图解析：table_scan_opt (scan.rs:2339) 每次执行重 parse 视图文本
        + 递归求值（thread_local 深度 8）
```

物理扫描统一入口 table_scan_opt → dispatch::dispatch_scan（纯函数，
scan.rs:2290/dispatch.rs:91）四替身：CurrentPoint（PK 点查）/
MainPlusDelta（CBF 列存+增量）/ HistoryScan（time travel）/
RowFallback（行路径）。`force_source`/`force_agg`/`optimize` 三个
调试 SET 构成差分测试三轴。

### 1.2 依赖分析（谁依赖谁，耦合点在哪）

| 模块 | 依赖方向 | 耦合问题 |
|------|---------|---------|
| `ir::plan`（Plan IR） | 依赖 sqlparser AST（节点内表达式全是 `Expr`；`aggs: Vec<String>` 是 display 文本） | IR 不是自足的：语义回捞依赖 scan.rs 的 `parse_plan_agg` 重解析 |
| `sql::optimize` | **双靶**：AST 靶（inline_subqueries/expand_ctes/column_mask/optimize 常量折叠）+ Plan 靶（5 个 rewrite_*） | 同一模块服务两条路径；inline_subqueries 无条件运行，不受 optimize 门控（评审 P1-6：差分轴失真） |
| `sql::scan::eval_query` | 依赖 optimize（重写）+ plan（构建/执行）+ scalar（谓词编译）+ exec/pipeline（FilterOp/SortOp/AggOp）+ dispatch | 全仓库最高扇入点；投影/聚合/窗口/谓词/排序全流程在一个函数体（92-794 行） |
| `exec_plan` | 依赖 eval_from 同源的 table_scan_opt、apply_predicates_q、project_exprs、agg::group_aggregate | 与 AST 路径**共享叶子**（这是优点：叶子语义单点）但**复制中段**（聚合组合段镜像 AST 分支，scan.rs:1341 注释自认"镜像 eval_select 聚合分支"） |
| `ddl` | 依赖 scan::eval_query（INSERT..SELECT/子查询）、scan::table_scan_by_name（UPDATE）、parse_batch（CHECK 文本重解析） | 约束执法与求值核心互相纠缠；CHECK 每行一次 parse_batch |
| 叶子层 expr::eval / scalar::ScalarProgram | 被 AST 路径、计划路径、DDL 共用 | 叶子语义单点（好）；但同一谓词存在三套执行机制（管线/紧循环/AST 直评） |

**关键判断**：叶子（expr/scalar 的行级语义、table_scan 四替身、
pipeline 算子）是健康的单点；**分叉发生在中段**（eval_query 的
装配逻辑 vs exec_plan 的节点解释 + exec_aggregate_composite 的
镜像分支）。这是"渐进统一"可行的结构性依据——要统一的是中段，
不动叶子。

### 1.3 计划路径当前覆盖面

`plan_exec_covered`（scan.rs:1654）+ `plan_nodes_exec_ok` 决定走向，
当前**显式排除**（carve-out 清单，单调增长中）：

1. 窗口函数（Plan 无 Window 节点——scan.rs:477 硬跳过）
2. DISTINCT + ORDER BY/LIMIT 共存（去重在 eval 出口做，与计划
   top-N/Limit 次序冲突——scan.rs:1665-1668）
3. QualifiedWildcard（`o.*` 前缀过滤语义——scan.rs:1699）
4. 混合通配（`SELECT *, x`——scan.rs:1696）
5. SetOp 分支内 SELECT DISTINCT（评审 P1-2：计划路径丢去重）
6. build_plan 失败的一切形态（派生表在 FROM 首位、CTE 未先展开、
   GROUP BY ALL 等）

评审文档口径"计划路径 ~90% 形态"可信，但**剩余 10% 恰是新特性
聚集地**（窗口/CTE/复杂投影），carve-out 是特性的"负资产清单"：
每加一个特性要么双实现、要么加 carve-out，没有第三条路——这是
双路径问题的可观测症状。

---

## 2. 五个结构问题的严重度评估

评分制：高/中/低，三维度（正确性风险 = 静默错误结果的概率×危害；
可维护性 = 每特性边际成本；性能 = 用户可感知开销）。

### 问题 1：双路径求值 —— 正确性：高 | 可维护性：高 | 性能：中

**证据**：
- 同一 SELECT 语义两处实现：计划路径（scan.rs:452-497）与 AST 路径
  （scan.rs:503-789）；集合操作还有第三处小双路径（scan.rs:140-168
  计划尝试 + AST 回落，`apply_setop` 共享是止损）。
- 聚合语义双实现：eval_query 聚合分支（607-695）vs
  exec_aggregate_composite（1344-1391，注释自认"镜像"）。
- 谓词过滤三机制：apply_predicates_q 内 FilterOp 管线（>64 行）/
  eval_row 紧循环 / AST 直评回落（818-877）。
- 差分测试三轴（optimize on/off、force_source、force_agg）的存在
  本身就是双路径的税：optimizer_differential.rs / dispatch_differential.rs /
  prune_differential.rs / agg_pipeline.rs 全部为此而生。
- 已实付的正确性账单：P0-1（build_plan 消费未内联 AST → count=0）、
  P1-2（SetOp 分支 DISTINCT 丢弃）、窗口+计划路径组合（曾把
  `sum() OVER` 当全局聚合塌缩——评审静默丢弃审计第 1 条）。

**判断**：这是五问题中唯一"高×高"的。它不是已炸的 bug，而是
**bug 发生器**：每一次"计划路径覆盖不到→回落"的决策点都是一次
语义分叉机会。窗口函数落在 AST 路径（且 v1 不支持窗口+GROUP BY）
不是能力上限，是双路径税的直接体现。

### 问题 2：文本回环 —— 正确性：中高 | 可维护性：中 | 性能：中

四处，性质不同：

| 位置 | 形态 | 危害 |
|------|------|------|
| CHECK 约束（ddl.rs:203/285/866-886） | 存 `expr.to_string()`；INSERT **每行每约束** `parse_batch("SELECT {}")` 重解析 | parse 失败被 `if let Ok` 静默吞 → **约束静默失效**（活的正确性洞）；热 INSERT 路径每行一次 tokenize+parse |
| Plan::Aggregate `aggs: Vec<String>`（plan.rs:42-46） | display 文本进 IR；`parse_plan_agg`（scan.rs:1313）重解析；投影映射按 `e.to_string()` 相等（scan.rs:1375-1377） | sqlparser 的 Display 格式成为**语义身份**——格式化变体（空格/大小写/嵌套括号）即错配 → 假"column must appear in GROUP BY"或错值 |
| 视图（mod.rs:739 存文本；scan.rs:2339-2352 重 parse） | 每次执行重解析视图体 | 与 PG 行为同形（PG 也存文本）——**可接受**，但应入计划缓存键 |
| Plan::Scan `version: Option<String>`（plan.rs:29-31） | 版本子句 display 文本 | 低危，但同属"IR 不自足" |

**判断**：CHECK 的静默失效是当前**最紧迫的单点**（5 行可修）；
`aggs: Vec<String>` 是 IR 层最刺眼的设计债，也是"IR 信息密度"
不足的标本（对照 SOTA 调研 §6.4 的命题）。

### 问题 3：错误吞噬模式 —— 正确性：高（残留点）| 可维护性：中 | 性能：无

**证据**：
- 已修：apply_predicates_q 回退分支（scan.rs:859-871，P0-2 修——
  "求值错误 → 语句失败不静默吞"已成文档承诺）；相关性检测 42703
  嗅探改结构判定（P0-3 修）。
- **残留**：CHECK 执法 `if let Ok(mut ss) = parse_batch(...)` 
  （ddl.rs:870）——解析失败 = 约束跳过，无任何告警。
- 结构性根因：**吞噬是双路径的伴生现象**。"计划路径失败→回落
  AST"的合法模式，会自然诱发"失败→回落→吞错"的非法变体
  （P0-2 的原形态正是如此）。只要回落路径存在，这个模式就会
  被重新发明。

**判断**：单点已大幅收敛（评审后修了 3 个 P0），但**模式滋生的
土壤（回落路径）还在**。这是把问题 3 归并进问题 1 治理的理由。

### 问题 4：AST 注入式特性 —— 正确性：中 | 可维护性：中高 | 性能：中

**证据**：
- 子查询内联（optimize.rs:1472-1629）：非相关标量→常量、IN→
  **InList 全量字面量**（子查询结果集整体物化进 AST——大表 IN
  子查询 = 内存与 AST 尺寸双炸）、EXISTS→Bool。相关子查询诚实
  拒绝（"v2: iterative evaluation"）。仅覆盖 WHERE（投影/HAVING/
  JOIN ON 不内联——评审 P1-1）。
- CTE 展开（optimize.rs:1714-1788）：`Table{cte}` → `Derived{clone}`
  ——多次引用的 CTE 每处克隆整棵查询，引用²次展开。
- 递归 CTE（scan.rs:4912-4988）：每轮把**全部累积行**转字面量
  Expr 注入 `Derived(VALUES ...)` 再整棵克隆求值——O(n²) 行克隆；
  `MAX_ROWS=1000` 截断是**静默的**（合法递归结果 >1000 行 →
  静默少数据，活的正确性悬崖）。
- 与计划路径的组合事故实证：P0-1（内联改了 `sel.selection`，
  build_plan 拿到未内联的 q → count=0）——注入式重写与计划构建
  的交错顺序是显式踩过的坑。

**判断**：注入式实现让"特性已落地"（SLT 绿）但**天花板极低**：
相关子查询、LATERAL、CTE 物化、窗口+GROUP BY 都被这层挡住。
它也是问题 1 的输入端（注入产物必须两条路径都能消化）。

### 问题 5：scan.rs 巨石 —— 正确性：低（间接）| 可维护性：高 | 性能：无

5100 行承载：求值核心（eval_query ~700 行巨函数）+ hash join 两个
+ 点查/范围提取 + time travel + 伪表 + 行↔batch 转换 + 窗口求值
+ 递归 CTE。混了四个抽象层（SQL 语义/物理派发/指标采集/列存适配）。
残留痕迹如 `let _ = (has_window, select_with_window);`（scan.rs:599）
显示快速迭代的 vestigial 代码已在积累。

**判断**：纯可维护性问题，但它**放大**问题 1-4 的修复成本
（任何改动都在 5100 行里做），也是 review agent 效率的直接税。

### 严重度总表

| # | 问题 | 正确性风险 | 可维护性 | 性能 | 趋势 |
|---|------|-----------|---------|------|------|
| 1 | 双路径求值 | **高**（bug 发生器） | **高**（特性×2） | 中 | 恶化（carve-out 单调涨） |
| 2 | 文本回环 | 中高（CHECK 活洞） | 中 | 中（每行 parse） | 稳定 |
| 3 | 错误吞噬 | 高→中（残留 CHECK） | 中 | — | 收敛中，土壤尚在 |
| 4 | AST 注入 | 中（1000 行悬崖） | 中高 | 中（O(n²)/物化） | 恶化（新特性首选歪路） |
| 5 | scan.rs 巨石 | 低（间接） | **高** | — | 恶化 |

**根因归纳**：1/3/4 同根——Plan IR 表达力不足（无 Window/Distinct/
子查询/CTE 节点、Aggregate 携带文本而非结构），特性被迫走 AST 侧，
于是有了回落、注入、吞噬。2 是 IR 自足性欠债的同一枚硬币。
**治本 = 补 IR；治标 = 继续打补丁。**

---

## 3. 三方案对比与推荐

### 方案 a：维持现状 + 局部修补

内容：继续现模式——新特性"AST 路径实现 + carve-out"或双实现；
CHECK 吞错修 5 行；递归 CTE 上限改报错。

- 优点：零迁移风险；每步小。
- 缺点：**特性边际成本不降反升**。窗口+GROUP BY、QUALIFY、相关
  子查询、LATERAL、物化 CTE、GROUPING SETS 每个都要回答"计划路径
  怎么办"——要么 carve-out 扩张（覆盖面萎缩），要么双实现（税翻倍）。
  carve-out 清单已证明单调增长，架构在复杂度拐点上。
- 适合场景：如果项目转入维护期、特性冻结——可行。但与 dendro
  路线图（v2 事务化 catalog、v3 AP 增长、物化视图=分支）相悖：
  每个后续里程碑都压在 SQL 层上。

### 方案 b：渐进统一到计划路径（推荐）

内容：AST 只做 parse→lowering（CTE 展开/子查询下沉为计划构建步骤），
特性在计划层实现；AST 求值路径（eval_from 及 eval_query 装配段）
逐步退役。**不重写叶子**（expr/scalar/table_scan 四替身/pipeline
算子/物理派发全保留）。

- 优点：
  - 直击根因：IR 补齐后 carve-out 清单清零、注入式特性有正路、
    吞错土壤消失（无回落可吞）、`aggs: Vec<String>` 换结构化
    AggCall、CHECK 一次性绑定。
  - 已有 70% 骨架：Plan IR/exec_plan/dispatch/差分三轴/EXPLAIN
    管道都是目标架构的组件，且 exec_plan 与 AST 路径共享叶子，
    统一是"删中段"不是"建新楼"。
  - 差分三轴把迁移变成可测的枚举过程（这正是 v2c-1 派发器设计的
    初衷——"派发可枚举 ⇒ 差分可枚举"）。
- 缺点：需要 40-60 人日的专注投入；阶段 3（子查询）有语义风险；
  期间要克制并行特性开发（或新特性直接以计划节点形态进入）。
- 详见 §3.1 迁移路径。

### 方案 c：全盘重设计（含新 IR / 换 DataFusion / 换语言栈）

内容：丢弃 sql 层（或整体），从 parse 起建新管线；变体包括引入
DataFusion 作为执行层。

- 优点：绿地自由度；DataFusion 变体可"买"算子生态。
- 缺点：
  - **测试资产不可迁移**：530 Rust 测试 + 34 SLT 里编码的教训
    （并发可见性、fencing、UPSERT 墓碑语义、NULL 排序、time travel
    快照冻结……）大部分是 dendro 特有语义，新架构要全部重证。
    按本仓库 21 轮评审的 bug 密度，重写后的再稳定期以季度计。
  - 与存储层的接缝（快照/分支/内容寻址/列存段）是 dendro 独有
    语义，任何现成引擎（含 DataFusion）都需要重写 TableProvider
    级集成——SOTA 调研 §3 已论证这是 v3 决策点而非现在。
  - 双架构共存期比双路径更糟。
- 适合场景：仅当存储层也要推翻时才连带成立——而存储层恰恰是
  健康的。

### 对比表

| 维度 | a 维持修补 | **b 渐进统一** | c 全盘重设计 |
|------|-----------|---------------|-------------|
| 正确性风险消除 | 部分（打地鼠） | 根因消除（回落/注入消失） | 根因消除（但引入新根因） |
| 特性边际成本 | 上升 | **下降**（计划节点即特性） | 最低（长期） |
| 测试资产 | 保留 | **保留并增强**（差分轴即迁移工具） | 大部分作废 |
| 中断期风险 | 无 | 低（每阶段全绿） | 高（季度级双架构） |
| 工作量 | 持续税 | 40-60 人日一次性 | 4-6 个月+ 再稳定 |
| 可回退性 | — | 高（阶段可停，阶段 2 后已获利） | 低 |

### 3.1 推荐方案 b 的迁移路径（分阶段、每阶段可验证、不破测试）

总原则：**每阶段结束时 `cargo test --workspace` + 全部 SLT 绿**；
optimize on/off 差分轴在阶段 5 前保留为"新旧行为对拍器"——off =
旧 AST 路径（还在），on = 新计划路径，差分绿 = 迁移正确。这把
"重构"变成和 v2c-1 派发器同款的枚举差分问题。

#### 阶段 0：止血与拆石（约 5 人日，纯行为保持或变好）

1. scan.rs 机械拆分（纯移动零语义变更）：
   `scan/`（eval 核心）、`scan/join.rs`、`scan/point.rs`（点查/PK
   范围）、`scan/history.rs`（time travel）、`scan/pseudo.rs`、
   `scan/window.rs`、`scan/cte.rs`。mod.rs 的 EXPLAIN 段同步拆出。
   验证：编译 + 全测试绿 + git diff 仅移动。
2. CHECK 吞错修复（ddl.rs:870）：parse 失败 → 23514 级错误（约束
   文本损坏是 DDL 期问题，运行期必须响亮）。回归：损坏 CHECK
   文本（手工改 manifest）→ INSERT 报错测试。
3. 递归 CTE `MAX_ROWS=1000` 静默截断 → 到达上限时报错
   （与差分轴的"强制不可行→报错"约定同源）。回归：1001 行递归
   CTE 报错测试。
4. 差分语料扩容：optimizer_differential 加窗口/DISTINCT/递归 CTE
   形态（先固定现状——阶段 2 的对拍基准）。

#### 阶段 1：IR 自足化（约 6-8 人日）

1. `Plan::Aggregate { aggs: Vec<AggCall> }`（结构化，替代
   `Vec<String>`）；AggCall 增加可序列化形态。`parse_plan_agg`
   删除。print_plan 的文本格式保持（display 从 AggCall 渲染）——
   ir_golden 测试只允许声明式更新。
2. `Plan::Distinct { input }` 节点；`Plan::Window { calls, input }`
   节点（calls 携带函数/参数/partition/order/合成列名——从
   WindowCall 结构升格）。
3. `Plan::Scan.version` 文本 → 结构化枚举 `{ AsOfHash(h) | AsOfTime(ms) }`。
4. exec_plan 为 Distinct/Window 接线：**实现体直接调用现有
   `dedup_rows`/`eval_windows`**（从 AST 路径提升为共享助手——
   零新语义，窗口仍在此阶段不走计划覆盖判定）。
   
   验证：新节点只经 EXPLAIN/golden 与单测暴露；执行行为不变。

#### 阶段 2：覆盖判定翻转（约 8-12 人日，核心获利点）

按 carve-out 逐项把 `plan_exec_covered` 的排除改为接受，每次翻转：
- 差分轴护航（off = 旧 AST 路径产出，on = 计划路径产出，逐查询
  对拍）；SLT 全绿后**当场删除**对应 AST 分支（不留三路径）。
- 翻转顺序：① DISTINCT+Sort/Limit（Distinct 节点置于 Sort 之上，
  语义序 dedup→sort→limit 修复）② 窗口（Window 节点在 Project 下、
  Filter(谓词) 上——顺带解锁窗口+GROUP BY 的实现位）③
  QualifiedWildcard（Project 携带前缀掩码）④ SetOp 分支 DISTINCT
  （P1-2 随之关闭）。
- 期间 EXPLAIN 输出窗口/去重节点（可观测性同步升级）。

   验证：差分对拍矩阵（每翻转项 × optimize on/off）+ 扩容的 SLT。

#### 阶段 3：子查询与 CTE 进计划（约 12-20 人日，风险最高段）

1. 非相关子查询：inline_subqueries 的字面量注入改为计划构建期
   下沉——build_plan 把 WHERE 里的 Subquery/InSubquery/Exists 转为
   `Plan::Join { kind: "cross-single" }` / `Plan::SemiJoin`，或
   InitPlan 形态（独立小计划，执行前一次求值，值绑定进主计划）。
   **消灭 InList 全量物化**（半连接哈希 Probe 替代）。差分：对拍
   旧内联结果。
2. 相关子查询（可选、可延后）：NestedLoopJoin + 外层行绑定（
   `Plan::Join { kind: "lateral" }`）。有了计划层位置后这是增量
   而非新架构——但建议独立排期，不与迁移混线。
3. CTE：expand_ctes 从"求值期 AST 改写"移到 build_plan 入口
   （计划构建消费 WITH）——多次引用的 CTE 在计划层共享为同一
   子树（Arc），克隆展开消失。MATERIALIZED 语义留钩子。
4. 递归 CTE：`Plan::IterativeScan { base, recursive, name }`——
   执行器驱动不动点（复用现有循环但操作 TableView 而非重建 AST），
   VALUES 注入与 O(n²) 克隆消失；上限行为已在阶段 0 修为报错。

   验证：subquery.rs / not_in_regression.rs / 032_cte.slt 扩容；
   差分轴 off 仍走旧注入路径直到本阶段末。

#### 阶段 4：约束与视图收口（约 6-9 人日，与阶段 3 可并行）

1. CHECK：CREATE 时 parse 一次 → 存储 AST 结构（serde）或绑定
   ScalarProgram，键控 schema_version（B3 失效纪律同源）；
   INSERT 热路径零 parse。旧文本格式兼容读（catalog 版本迁移）。
2. UNIQUE：insert 语句级批处理——每条 INSERT 语句对每个 unique
   set 做一次扫描构建哈希集，而非每行全扫（v1 全扫 O(n²) → 语句级
   O(n)）；v2 唯一索引另议。
3. 视图：视图体 parse 结果入计划缓存（键 = schema_version +
   视图文本 hash），执行期零重解析。

#### 阶段 5：AST 求值路径退役（约 3-5 人日）

eval_query 收缩为：`CTE/子查询 lowering（build_plan 内）→ 6 重写
→ exec_plan`。eval_from、eval_query 装配段（scan.rs:503-789 的
FROM/WHERE/窗口/聚合/投影/排序/limit 流水）、exec_aggregate_composite
镜像分支删除。optimize on/off 差分轴退役（重写保留开关另议）；
force_source/force_agg 保留（物理轴，与本次迁移正交）。
验证：全测试绿 + KPI 达标（见下）。

**KPI（迁移完成的可度量定义）**：
- `plan_exec_covered` carve-out 数 = 0（函数本身删除）；
- SLT + 差分语料中 AST 路径命中次数 = 0（加计数器断言）；
- INSERT 路径 parse_batch 调用 = 0（CHECK/视图已绑定）；
- scan/ 各文件 ≤ 1200 行；eval_query 残体 ≤ 100 行。

---

## 4. 借鉴 ScopeDB 调研的适用结论

ScopeDB 主引擎闭源且无版本化模型，可借鉴的是**原则**而非代码：

1. **"剪枝/索引不参与正确性"（ScopeDB §4.1-4.2）→ 迁移期总不变式**。
   dendro 的差分三轴（optimize/force_source/force_agg）正是这一原则
   的执行机构：优化与物理派发永不改变语义。渐进统一全程押注这条
   不变式——每阶段翻转覆盖判定时，off 轴即"无剪枝参照物"。
2. **确定性 I/O 规划（ScopeDB §6.1）→ 计划是读什么的前置合同**。
   ScopeDB 的 planner 在执行前确定字节范围、永不 LIST；dendro 的
   manifest+CID 已具备等价前提。统一后的 Plan 应成为**唯一**的
   "本次查询读哪些对象"决策点（dispatch_scan 的四替身选择进计划
   注解/物理计划层）——这正是把 dispatch 从 table_scan_opt 内部
   提升到计划层的长期方向，方案 b 为它铺路，方案 a 则让它继续
   埋在扫描函数里。
3. **缓存与正确性解耦（ScopeDB §6.4 percas/cache2）→ 计划缓存
   纪律不变**。统一后计划缓存可升级为"绑定计划"（SOTA 调研 v2b
   建议：schema_version 入键）——CHECK 绑定（阶段 4）即其首个
   实例。缓存任何时刻失效都不影响正确性。
4. **MATERIALIZED INDEX ≡ 物化分支（ScopeDB 调研 §9 P2）→ 物化
   视图的实现前提是单计划 IR**。dendro 的杀手锏"物化视图 = 自动
   维护的分支"要求增量重算在计划层可表达（子计划 diff/重放）——
   双路径下无从做起；这是选择方案 b 的一个远期但战略性的理由。
5. **反面结论维持**：不借鉴 ScopeQL（pgwire/mywire 生态位是护城河）；
   不借鉴无版本化模型。与本报告无关的 S3 并发 range read 等结论
   归存储层路线，不在此展开。

另引《SQL表示与优化SOTA调研》两条直接相关结论：**optd 教训**
（§4：节点三份结构的代价——Plan enum 扩员时保持变体纪律，分支
属性进节点数据而非变体）；**"先算子后优化器"**（§2：顺序原则——
阶段 1/2 是算子补齐，代价模型/连接重排深化放后，现有 6 重写
规则级优化器在统一后自然受益于更大的作用域）。

---

## 5. 工作量估算与风险

### 5.1 工作量（人日，单人专注口径）

| 阶段 | 内容 | 估算 | 累计 | 可停点价值 |
|------|------|------|------|-----------|
| 0 | 拆分+止血+差分扩容 | 4-5 | 5 | 独立价值（CHECK 洞关闭） |
| 1 | IR 自足化（AggCall/Distinct/Window 节点） | 6-8 | 13 | 结构债清偿 |
| 2 | 覆盖翻转 + AST 分支删除 | 8-12 | 25 | **主获利点**（窗口/DISTINCT 入计划路径） |
| 3 | 子查询/CTE 进计划（含相关子查询） | 12-20 | 45 | 相关子查询解锁（可只做非相关 8-12 人日） |
| 4 | CHECK/UNIQUE/视图收口 | 6-9 | 54 | INSERT 热路径优化（可与 3 并行） |
| 5 | AST 路径退役 + KPI 收口 | 3-5 | 59 | 长期维护成本落底 |

- **最小可行**（阶段 0-2）：约 25 人日，已消除 carve-out 与双实现
  税的大头；
- **完整**（0-5）：40-60 人日（约 6 周）；
- 对照：方案 c 全盘重写同特性面估算 4-6 个月开发 + 1-2 个季度
  再稳定（按本仓库评审轮次的 bug 发现率外推），且期间 530 测试
  的语义教训需重证。

### 5.2 风险与缓解

| 风险 | 等级 | 缓解 |
|------|------|------|
| 阶段 3 子查询语义分叉（半连接 NULL 语义、IN 含 NULL） | 高 | 差分轴对拍旧内联结果；not_in_regression/subquery 测试先行扩容；相关子查询独立排期不混线 |
| 翻转期间三路径共存（计划新节点 / AST 分支 / 差分 off 轴） | 中 | 纪律：每次翻转 green 后**当场删** AST 分支；KPI 监控 eval_query 行数单调下降 |
| print_plan/golden 大面积 churn | 低 | display 从结构渲染保持旧格式；golden 更新声明式单列 |
| 并行特性开发与迁移冲突 | 中 | 冻结非关键 SQL 特性；或约定新特性只能以计划节点进入（阶段 1 后即可） |
| sqlparser 0.62 升级打断（~52 处表达式 match） | 中 | 与本次迁移正交但叠加放大；迁移期锁版本，完成后单独评估 |
| 阶段 0 拆分引入微妙借用/行为变化 | 低 | 纯移动 PR 独立评审；`git diff -M` 验证移动 |

### 5.3 明确不做（本次）

- 不换解析器/不换语言栈/不引 DataFusion（SOTA 调研 v3 决策点条件
  未触发：AP 查询占比、Parquet 互操作、函数生态诉求均未到期）；
- 不动存储层（prolly/memtx/WAL/CBF/分支/fencing）与协议层；
- 不做表达式 JIT / Cascades / e-graph（观察旗维持）；
- optimize 六重写的代价模型化（统一后作用域自然扩大，另行立项）。

---

## 附：证据索引（代码位置速查）

| 事实 | 位置 |
|------|------|
| 双路径分发（计划 vs AST） | scan.rs:452-497（计划）, 503-789（AST） |
| 集合操作小双路径 | scan.rs:140-168 |
| 窗口仅 AST 路径 | scan.rs:477-482（硬跳过）, 545-605（合成列+投影重写） |
| 聚合语义镜像双实现 | scan.rs:607-695 vs 1344-1391（注释自认"镜像 eval_select"） |
| 谓词三机制 | scan.rs:818-877（FilterOp>64 行/紧循环/AST 回落） |
| carve-out 清单 | scan.rs:1654-1712（plan_exec_covered） |
| CHECK 文本存储+逐行重解析+吞错 | ddl.rs:203/285（存文本）, 866-886（`if let Ok` 吞 parse 失败） |
| Plan::Aggregate 文本 aggs | plan.rs:42-46；重解析 scan.rs:1313；文本匹配 scan.rs:1375-1377 |
| 视图文本回环 | mod.rs:731-741（存）, scan.rs:2339-2352（重解析+递归求值，深度 8） |
| 子查询字面量注入 | optimize.rs:1472-1629（InList 全量物化；相关→拒绝） |
| 递归 CTE VALUES 注入 + 静默 1000 行截断 | scan.rs:4912-4988（MAX_ROWS=1000） |
| CTE 克隆展开 | optimize.rs:1714-1788 |
| 差分三轴定义 | mod.rs:1025-1112（force_source/force_agg/optimize 的 SET）；dispatch.rs 全文 |
| 错误吞噬已修承诺 | scan.rs:796-804, 859-871 |
| 评审已修/残留账本 | docs/research/解析器架构评审-2026-09-16.md（P0-1..5 状态） |
