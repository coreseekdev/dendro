# 查询优化器 SOTA 补充调研：agentic 强化学习分支（2026-09）

> 调研时间：2026-09-18。触发：用户指定入口
> <https://rohanbansal.com/qorl>（"Training a 4B model to produce 81%
> faster query plans than Postgres"，2026-09）。定位：**补充**——
> 经典 Cascades/DP 与学习型（Bao/Neo/Balsa/Lero）谱系及 Leis 2025
> 重评已见《[优化器SOTA调研](优化器SOTA调研.md)》（2026-09-16），
> 本文聚焦其后新起的 **LLM 智能体 × 强化学习** 分支，并重排各分支
> 的可信度。结论先行：
>
> 1. **qorl 的结果是真的但口径要读细**：JOB 113 查询 best-of-15
>    几何均值 **1.81×**（总和延迟降 44.7%）；对照组是 **Postgres
>    默认计划**，且 best-of-15 = 采样 15 个计划取最快（推理成本
>    ~分钟级/查询）。它的价值不在"模型会优化"而在**工程方法论**：
>    测量去噪纪律（fooled rate）、anchored GRPO、hint 空间选择。
> 2. **模型学到的不是新优化理论**：胜分主要来自 **leading-tree 形
>    状 + 物理修正（扫描类型/并行度）**——恰是 Leis 2015/2025 与
>    Bao 系工作早已指出 PG 计划缺口的同两个位置（连接顺序错 +
>    代价模型钝感）；Rows 修正类 hint 基本没学会用。LLM 智能体在
>    "重新发现经典结论"，而非超越。
> 3. **2025-26 批判线持续加强**：Leis 等十年重评（PVLDB v18）对
>    学习型优化器整体持保留态度；2025 年"简单自适应（LIP/AJA）
>    在 JOB-Slow 上胜过 Balsa/Bao"进一步压低学习型优先级。**基线
>    工程质量（CE 质量、测量纪律）仍是最高杠杆**。
> 4. 对 dendro：**零 ML 也能拿走 qorl 的三样东西**（P0 测量纪律
>    升级 / P1 hint 面标准化 / 差分轴即智能体工具面雏形）——dendro
>    的 force 轴 + EXPLAIN ANALYZE est/actual 与 qo-agent 的六工具
>    几乎一一对应。

## 1. qorl 案例复盘（入口文章）

### 1.1 设置

- 模型：Qwen 蒸馏 4B（基模型对 JOB 113 查询中 99 个**连合法计划都
  产不出**）；
- SFT：从 GPT-6 Astra 蒸馏轨迹（off-policy）；
- RL：自研 **anchored GRPO**（自定义 GRPO 变体）；
- 动作空间：**pg_hint_plan 提示**（join 顺序/方法、Rows 修正、扫描
  类型、并行度、leading 表）——不改 PG 内核；
- 智能体骨架 qo-agent 六工具：`inspect_relation` / `get_column_stats`
  / `get_plan` / `evaluate_candidate`（执行候选计划取实测） /
  `keep_default` / `finish`；
- 基准：JOB（Leis 2015 引入的 113 查询）。

### 1.2 结果与口径

| 数字 | 口径 |
|------|------|
| 1.81× 几何均值 | best-of-15（每查询采样 15 计划取最快）；对 PG **默认**计划 |
| 44.7% | 总和延迟削减（同上口径） |
| 训练前 | 99/113 无法产出合法计划 |

诚告：best-of-N + 分钟级/查询的评估预算，与生产 OLTP/交互场景无关；
其适用画像是"重报表、跑一次值回等待"的分析负载——Bao 论文早已
给出同口径的 bandit 解法（更便宜）。

### 1.3 四个可迁移的工程点（本文真正的贡献）

1. **测量去噪纪律**（最重要）：
   - 预热至 SHB/SRB 缓冲计数器稳定再测；
   - shared_buffers 提到 2GB 后"愚弄率"从 5% → 1.3%（所谓愚弄 =
     缓存状态随机性把慢计划测成快计划）；
   - **(候选, 默认) 交替配对** + 每侧中位数-of-3；
   - 平局区 ±5% 内不算赢；
   - **以 fooled rate（而非变异系数 CV）为度量质量指标**——CV
     衡量方差不衡量方向性错误，愚弄率直接衡量"奖励信号可信度"。
2. **anchored GRPO**：优势锚定到 default 而非组均值——朴素 GRPO
   会强化"与默认等价"的计划（组内全是平局时梯度指向默认邻域）；
   锚定后只有"赢过默认"才获正优势。**对任何"以现有系统为基线做
   RL 改进"的任务通用**。
3. **奖励设计教训**：每次 evaluate 收 -3 固定费 → 模型恐惧评估、
   过早 finish（学出"保守平庸"策略）；改软费后探索恢复。
4. **动作空间的隐性知识**：hint 空间 = "物理修正 + 连接形状"是
   正确刀口——与 Leis 2015 的结论（PG 的两个痛点）精确重合。

### 1.4 模型实际学到什么

leading 树重排 + 扫描方式修正 + **Parallel 提示**（PG 默认不启用
per-query 并行）赢得大头；Rows（基数修正）hint 几乎未学会。解读：
LLM 智能体在**重新发现 2015 年以来的经典结论**，且受益最多的
Parallel 提示属于"把已知好东西打开"而非优化智慧。

## 2. 分支谱系与可信度重排（2026-09 视角）

| 分支 | 代表 | 状态判读 |
|------|------|----------|
| 经典搜索 | Cascades/Volcano 系（Orca/Calcite/SQL Server）、PG DP+GEQO、DuckDB 规则管线 | 生产主流；dendro 现处此列（规则链 + 保守门控） |
| 批判重评 | [Leis 2025 Still Asking](http://www.vldb.org/pvldb/vol18/p5531-viktor.pdf)、[Learned Cost Models 评测 (SIGMOD 2025)](https://github.com/DataManagementLab/lcm-eval)、[简单自适应 vs 学习型 (2025)](https://link.springer.com/article/10.1007/s00778-025-00936-6) | CE 错误仍是万恶之源；LIP/AJA 简单自适应在 JOB-Slow 胜 Balsa/Bao——**学习型未兑现**的证据在积累 |
| 学习型（预 LLM） | [Bao](https://people.csail.mit.edu/tatbul/publications/bao_sigrec22.pdf)（bandit 选 hint 集）、[Balsa](https://zongheng.me/pubs/balsa-sigmod2022.pdf)（无教师）、[Lero](https://dl.acm.org/doi/10.14778/3583140.3583160)（学习排序）、Neo/HybridQO/LOGGER | Bao 形态（上层 hint 集合 + 汤普森采样）是唯一生产化较深者；[期望行为审计](https://arxiv.org/html/2309.01551v2)揭示常见退化 |
| LLM 智能体 | **qorl**（本文）、各类 GPT-4o/Claude 试 PG 计划调优 | 2025-26 新起；结果口径普遍 best-of-N + 重分析负载；方法论贡献 > 优化能力贡献 |
| 综述 | [Query Optimization in the Wild (2025)](https://arxiv.org/html/2510.20082v2) | 生产现实与趋势总览 |

判读：**qorl 与 Bao 是同构的**（都在 hint 空间上搜索、都以默认计划
为锚）——qorl 把 Bao 的 bandit 换成了 LLM+RL，代价是分钟级推理与
4B 模型运维。在 dendro 尺度上，Bao 形态（甚至更简单的 paired-bandit）
已足够承接同一收益位。

## 3. 与 dendro 的对照

| qorl 组件 | dendro 现状 | 评 |
|-----------|-------------|-----|
| pg_hint_plan 动作空间 | `SET dendro.force_source / force_agg`（debug 构建）+ `optimize=off` 六规则开关 | dendro 已有微型 hint 面（差分轴）；语义 = 物理/规则修正，与 qorl 赢分位（物理修正）同刀口 |
| qo-agent 六工具 | `resolve_table`（≈inspect_relation）、`stats::table_stats`（≈get_column_stats）、`EXPLAIN`（≈get_plan）、`EXPLAIN ANALYZE` est/actual（≈evaluate_candidate） | **一一对应**——dendro 的可观测面天然是智能体工具面雏形 |
| 测量纪律 | SLT 固化 + 差分轴（optimize on/off、force 两轴）；无延迟类指标的去噪协议 | P0 升级点（下） |
| CE 质量 | 段统计 + `estimate_filter_rows`（EXPLAIN ANALYZE 的 est 可见） | 与 Leis 结论对齐：est/actual 差值即 CE 质量的常驻监控，**已有** |
| 学习型/RL 本体 | 无 | 与《优化器SOTA调研》结论一致：远期 |

## 4. 可借鉴点清单

### P0（零 ML，测量纪律直接搬）

1. **A/B 评测协议标准化**：任何优化规则的效果主张走 qorl 纪律——
   预热（缓冲/缓存计数稳定）、(开启, 关闭) 交替配对、每侧中位数-of-3、
   ±5% 平局区、报告 **fooled rate** 而非方差。落点： benches/ 下
   新增一个 `bench_pair` 小工具（配对交替 + 中位数 + 愚弄率输出），
   供规则 PR 附带数字；EXPLAIN ANALYZE 文档同步声明 est/actual 的
   测量噪声边界。
2. **Parallel/物理默认值审计**（qorl 赢分头的启发）：dendro 侧的
   对应问题是——列存扫描的批并行、S3 range 预取是否默认开启？
   （与机会式派发 P1 合流）确认"已知好东西没有默认关着"。

### P1（hint 面标准化，为 bandit/智能体留钩）

3. **force 轴泛化为 hint-set**：`force_source/force_agg` 从调试
   SET 泛化为统一 hint 词汇（扫描方式/聚合路径/连接顺序开关/并行度），
   release 也可见但默认空。收益：① 差分轴更可枚举；② 未来 Bao 式
   paired-bandit 或 LLM 智能体的动作空间**免费就位**（qo-agent 的
   六工具 dendro 已有对应物）。注意保持"hint 只影响物理选择，不
   改语义"的合同（现 force 轴已守）。
4. **est/actual 差值的产品化监控**：EXPLAIN ANALYZE 已输出
   est=…；把 JOB 风格自测语料的 est/actual 比值纳入回归报告——
   CE 质量退化即优化器退化的前导指标（Leis 两篇的核心主张）。

### P2（远期跟踪）

5. 学习型优化器本体：维持《优化器SOTA调研》判断——先 CE 与简单
   自适应（LIP/AJA 类算子内联/早停在 dendro 的等价物），学习型留
   到有真实 workload 反馈回路（P2' 多节点/云服务形态）之后；
6. qorl 类 LLM 智能体：作为**离线计划审查工具**（CI 里对 SLT 慢
   查询跑 best-of-N hint 搜索，产出回归修正）比在线优化现实——
   dendro 的 SLT + 差分轴 + EXPLAIN 面就是现成的智能体环境。

## 5. 落地记录（2026-09-18）

| 建议项 | 状态 |
|--------|------|
| P0-1 评测协议 | ✅ `dendro-server bench-pair`（预热 W3 / 交替配对 R7 先手翻转 / 每侧中位数 / ±5% 平局区 / 愚弄率；`pair_stats` 纯函数 3 单测锁定口径）。**首跑即验证方法 论价值**：join_pushdown 判 WIN 0.800× 且愚弄率 0（规则 20% 加速的干净信号）；range_sort_limit 判 TIE 但愚弄率 0.429（测量不可信的典型形态——方差指标看不见、愚弄率直接点名）；global fooled rate 0.171 为基线。规则效果主张自此附本工具数字 |
| P0-2 物理默认值审计 | ✅ optimize 默认 on；server 主路径默认接 CbfColumnar（AP 激活）；Durability 默认 Group。**无已知好物默认关着**；未启用的（并行扫描/S3 range 预取/SIMD）属未建非关闭——qorl 的 Parallel 型免费赢面在 dendro 对应"先建后默认开" |
| P1-3 hint-set 泛化 | 未做（force 轴仍 debug-only 双轴） |
| P1-4 est/actual 产品化 | 未做 |

## 6. 信源

- 入口：Rohan Bansal, *Training a 4B model to produce 81% faster
  query plans than Postgres*.
  <https://rohanbansal.com/qorl>（2026-09，全文实读）
- 十年重评：Leis et al., *Still Asking: How Good Are Query
  Optimizers, Really?* PVLDB v18 (2025).
  <http://www.vldb.org/pvldb/vol18/p5531-viktor.pdf>
- 学习型谱系：Bao
  <https://people.csail.mit.edu/tatbul/publications/bao_sigrec22.pdf>；
  Balsa <https://zongheng.me/pubs/balsa-sigmod2022.pdf>；Lero
  <https://dl.acm.org/doi/10.14778/3583140.3583160>
- 批判线：学习型优化器行为审计
  <https://arxiv.org/html/2309.01551v2>；简单自适应 vs 学习型
  (2025) <https://link.springer.com/article/10.1007/s00778-025-00936-6>；
  学习型代价模型评测 (SIGMOD 2025)
  <https://github.com/DataManagementLab/lcm-eval>
- 综述：*Query Optimization in the Wild: Realities and Trends*
  (2025). <https://arxiv.org/html/2510.20082v2>
- dendro 侧：`docs/research/优化器SOTA调研.md`（经典+学习型+生产
  系统对照，本文上篇）、`crates/dendro-core/src/sql/stats.rs`、
  `crates/dendro-core/src/sql/optimize.rs`（六规则）、
  EXPLAIN ANALYZE est/actual（sql/mod.rs）
