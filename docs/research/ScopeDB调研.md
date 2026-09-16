# ScopeDB 调研（2025–2026）

> 调研时间：2026-09-16。触发：与 dendro 定位类似（Rust、对象存储原生 SQL
> 数据库）的竞品分析。结论先行：**ScopeDB 主引擎闭源**（GitHub org 只开源
> 外围组件，Apache-2.0），**没有版本控制/分支模型**（无 time travel、无
> snapshot、无 branch），其核心可借鉴价值在 **S3 读路径的稀疏访问设计**
> （确定性 I/O 规划 + 并发 range read + 短暂缓存）与**语义索引/聚类布局**；
> 与 dendro 的 prolly tree 版本化是**错位竞争**而非同质竞争。

## 1. 项目概况与开源状态

| 维度 | 事实 |
|------|------|
| 定位 | Serverless 事件分析数据库（可观测性/行为分析/AI Agent 遥测），口号 "Insight In No Time, Schema On The Fly" |
| 形态 | **纯云服务（ScopeDB Cloud）**，闭源单二进制；无自托管选项 |
| 语言 | Rust（"single, stateless Rust binary"，依赖 300+ crate） |
| 查询语言 | ScopeQL（自研管道式关系语言，非 SQL） |
| 存储 | 任意商用对象存储（AWS/GCP/Azure），shared-disk |
| License | **主引擎闭源**；外围仓库全部 Apache-2.0 |
| 团队 | 2–10 人创业公司（LinkedIn），广州 |

**GitHub org（github.com/scopedb，12 个仓库，62 followers）全量清单**（2026-09-16，
GitHub API 实测）：

| 仓库 | 语言 | Stars | License | 最近推送 | 说明 |
|------|------|-------|---------|----------|------|
| percas | Rust | 118 | Apache-2.0 | 2026-08-22 | 分布式持久缓存服务（NVMe SSD 优化） |
| scopedb-sdk（**已归档**） | Python | 21 | Apache-2.0 | 2026-08-23 | 旧 SDK monorepo |
| community | — | 11 | 无 | 2025-11-20 | 反馈讨论 |
| telescope | Go | 10 | Apache-2.0 | 2026-09-09 | 开发者 Agent 遥测 runtime |
| cache2 | Rust | 4 | Apache-2.0 | 2026-09-16 | 有界 RAM+SSD 缓存 |
| scopeql | Rust | 3 | Apache-2.0 | 2026-08-22 | CLI/REPL（非语言本体） |
| scopedb-client / goscopedb / scopedb-js | Rust/Go/TS | 各 1 | Apache-2.0 | 2026-09-16 | 三语客户端 |
| scopedb-docs | MDX | 2 | 无 | 2026-08-08 | 官网文档源 |
| scopedb-cli / orrery | Go | 0 | Apache-2.0 | 2026-09-16 | 云 CLI / 未公开说明 |

要点：

- **`github.com/scopedb/casql` 不存在**（HTTP 404，搜索引擎亦无任何"casql"痕迹）。
  传闻中的主引擎仓库从未公开。
- **是否允许借鉴**：主引擎**无代码可借鉴**，只能借鉴公开文档/博客描述的思路
  （思路不受版权保护）；外围仓库（percas、cache2、scopeql、三语 client）
  均为 Apache-2.0，**可自由复用代码**（保留 NOTICE/版权声明即可）。
- 团队将适合独立发布的公共库放在 Fast 组织（fastrace 分布式追踪、
  logforth 日志），亦为 Apache-2.0 系。

## 2. 团队、成熟度与活跃度

- **团队**：Co-founder/CEO Yu Lei（广州，中山大学，数据库研发背景）、
  Co-founder Chen；核心工程团队含 **tison**（Rust/Apache 社区知名人物，
  fastrace/logforth 作者，2025 年加入）；Alexander Reelsen（前 Elastic
  15 年分布式搜索工程师）在外部活跃传播，角色不明。
- **时间线**：2024 年底启动开发，**四个月**通过测试后逐步上生产
  （tison 2025-09 博客《原地起飞：基于 Rust 在四个月内开发出服务全球的
  云数据库》，RustChinaConf 2025 演讲）；2025 年初开始密集发布博客与
  Reddit/LinkedIn 推广。
- **成熟度**：宣称生产实例遍布全球多个可用区；客户案例（可观测性平台）
  声称综合成本降低 70%+，"少数低配节点每秒百万行以上写入"。
- **活跃度**：org 内 5 个仓库**当天（2026-09-16）仍有推送**，持续活跃；
  但社区声量小（最高单仓库 118 stars，org 62 followers）。
- **形态判断**：早期创业公司 + 小众闭源云产品，非开源社区项目，
  不存在"抄代码"的可能性，只有"抄设计"的价值。

## 3. 整体架构

ScopeDB 是**shared-disk 列式分析库**，与 dendro 同属"对象存储原生"阵营，
但取舍激进得多：

- **存储计算彻底分离**：S3 是唯一事实源（single source of truth），
  计算节点**零本地盘**、完全对等、无状态、**无选主、无数据复制**——
  持久化与容错全部委托给对象存储本身。
- **无 ETL**：写入直接落 S3、成功即可查询，宣称消灭 OLTP+CDC+Kafka+
  数仓+Flink 流水线（称其占洞察方案成本 50%+）。
- **纯读写分离**：写入走独立资源组（小规格常驻），查询分即席
  （常驻资源池）与历史/重型查询（临时资源池，可用 Spot 实例）。
- **Serverless Compute Slots**：按资源组隔离工作负载，支持
  scale-to-zero 与分钟级扩缩容（无 data rebalance）。
- **执行引擎**：向量化 + 分布式 MPP——ScopeQL 解析优化后生成可并发
  物理计划任务，分派到各无关节点独立计算、交换必要数据。
- 优化器细节**未公开**（无任何 join reorder/代价模型博客）。

与 dendro 对照：dendro 是 TP(OCC MVCC + WAL) + AP(列存) 的 HTAP 形态，
ScopeDB 是**纯 AP、无事务语义公开、无点查更新模型**（append-only 事件
+ retention 过期）。

## 4. 存储模型（对象存储布局 / 索引 / compaction）

物理布局细节公开度低，已知信息拼图：

### 4.1 数据组织层级

```
表 → 分区(partition) → 聚类(cluster, CLUSTER BY 布局提示) → 段(segment)
     → 列式块(按类型压缩, 自适应表达式索引挂在 segment 级做剪枝)
```

- **列式 + 按类型压缩**：查询只读引用列，跳过大 message/半结构化列。
- **CLUSTER BY**（`ALTER TABLE events CLUSTER BY service, time`）：
  **布局提示而非约束**——把相近键值的行聚到同分区同 segment，让
  剪枝更有效。官方明确"不改变查询结果、不保证顺序"，键序敏感
  （高频过滤表达式放最前）、键集要小而稳。
- **segment 级索引剪枝**：剪枝只是性能优化，**不参与正确性**
  （"not a requirement for query correctness"）——多索引交集后
  剩余候选 segment 全量评估。

### 4.2 四类语义索引（"索引是数据模型的一部分"）

| 索引 | 谓词形状 | 示例 | 备注 |
|------|---------|------|------|
| POINT | 等值 / IN | `CREATE POINT INDEX ON events (service)` | 挂 object 列时可索引**任意 JSON 路径**（一个索引覆盖 `var['gateway']::string='x'` 等所有路径），也可对单热路径建表达式索引 |
| RANGE | 范围 | `CREATE RANGE INDEX ON events (time)` | 时间窗过滤主力 |
| SEARCH | 文本匹配 | `CREATE SEARCH INDEX ON events (message)` | 全文检索 |
| MATERIALIZED | 预计算 | `CREATE MATERIALIZED INDEX ON events (var['http']['url']::string)` | 物化昂贵半结构化抽取，重复查询免解析 |

方法论是 **query-first**：先看真实查询哪些字段收窄结果集，再建索引；
明确警告不要对 `var` 里每个路径都建索引。

### 4.3 半结构化（Schema On The Fly）

- 摄入记录先落为**单 object 列**（嵌套字段），建表可只部分定义 schema；
  `variant`/`ANY` 类型 + `PARSE_JSON(var)` 按需抽取 cast。
- 摄入管道（data cable）在写入时用 ScopeQL 做 reshape：数据质量过滤
  （时间窗内、`name IS NOT NULL`）、字段抽取 cast，然后 `INSERT INTO`
  目标表。

### 4.4 compaction 与生命周期

- 后台 compaction **合并高频小文件为大文件**，降 S3 存储成本、加速查询
  ——策略细节（分层？尺寸触发？并发？）**未公开**。
- retention：表级保留窗口，数据**异步过期**。
- 无公开的 manifest/Catalog 文件格式；元数据由"事务性 metadata 服务"
  承载（见 §6）。

### 4.5 内容寻址

**完全未提及**。没有 CID/hash 命名、去重、内容寻址块的任何公开描述。
这是 dendro 的差异化点（dendro：sha2/xxhash 内容寻址 prolly tree）。

## 5. 版本控制 / 分支模型：没有

多方交叉验证（官方文档全部页面、三篇架构博客、RustChinaConf 演讲、
搜索引擎）均**无 branch / time travel / snapshot / versioning 能力**：

- 无 `CREATE BRANCH` 类语句；无 `AS OF` 查询；无快照谱系。
- 数据模型是**事件流 + retention 单向过期**，不是版本树。
- 博客对版本化只字未提，路线图亦无迹象。

三家对比：

| 维度 | git/Dolt | **dendro** | **ScopeDB** |
|------|----------|-----------|-------------|
| 版本化单位 | cell 级 MVCC | 行级 prolly tree chunk | **无** |
| 分支 | ref 指针 | ref 指针，O(1) 创建，chunk 级 diff + 行级冲突检测 | **无** |
| merge | 三方 | prolly tree chunk diff | **无** |
| 历史查询 | time travel | 全历史可查（append-only） | 仅 retention 窗口内"当前"数据 |
| 内容寻址 | 有 | 有（CBF 块 CID） | **无（未公开）** |
| 去重 | chunk 级 | chunk 级 | 无 |

结论：**dendro 的 git 式分支 + 内容寻址是 ScopeDB 完全没有的能力**，
二者在"版本化"轴上不构成竞争；ScopeDB 的取舍是把全部复杂度预算花在
"对象存储上的吞吐与剪枝"。

## 6. 与 AWS S3 的集成（本调研最有价值部分）

官方博客《Cloud Elasticity》给出四项工程原则，dendro 可直接对标：

1. **Deterministic I/O Planning（确定性 I/O 规划）**
   - 一个**事务性 metadata 服务**记录每个数据块的**精确位置、统计信息、
     启发式轻量索引**；
   - planner 在执行前就知道要读哪些对象、哪些**字节范围**；
   - **永远不用 S3 LIST**；分区下载退化为 byte-range fetch
     （这就是其 sparse read 机制）。
2. **Throughput over Latency（吞吐优先于延迟）**
   - 向量化引擎对 S3 发**几十个并发异步 range 读**，流水线化以
     饱和网络带宽，掩盖 S3 的 TTFB，喂饱 CPU 核。
3. **Aggressive Data Pruning（激进剪枝）**
   - 自适应表达式索引 + 列式布局跳过无关文件；
   - range request **只取查询需要的列和 page**。
4. **Ephemeral Caching（短暂缓存）**
   - 多级缓存**可选**（opt-in）、只为热数据亚秒延迟；
   - **缓存不承担正确性职责**——节点挂掉零丢失，替身节点直接从 S3
     起服务、后台预热；
   - 对应开源组件：**percas**（分布式持久缓存，去中心化无协调者，
     NVMe 优化，HTTP PUT/GET/DELETE 极简接口）、**cache2**
     （单机有界 RAM+SSD 缓存）。

成本叙事：PB 级 S3 $21,550/月 vs EBS $80k–125k/月，~10x；
客户低谷 1 节点/高峰多节点，综合降本 70%+。

## 7. ScopeQL 查询语言设计

**不是 SQL 方言**，是管道式关系语言（灵感：PRQL、SaneQL、GoogleSQL
pipe syntax 论文 *SQL Has Problems. We Can Fix Them: Pipe Syntax in SQL*；
社区比作 KQL/BigQuery pipe syntax）。

设计原则：语法序 = 执行序（线性管道）；一切皆表；一致性（一个 `WHERE`
随处过滤，消灭 HAVING/QUALIFY）；`GROUP BY` 降格为 `AGGREGATE`/`WINDOW`
的修饰符；表达式复用免子查询。

```
[ FROM ... [ SAMPLE n PERCENT ] | VALUES ... ]
[ [INNER|LEFT|RIGHT|FULL] JOIN ... [ON ...] ]
[ SELECT ... ] [ WHERE ... ] [ DISTINCT ON ... BY ... ]
[ [GROUP BY ...] [WITHIN GROUP ORDER BY ...] WINDOW/AGGREGATE ... ]
[ ORDER BY ... ] [ LIMIT n [OFFSET m] ]
```

示例（语法序 = 逻辑序，两级聚合无需子查询——TPC-H Q13 风格）：

```
FROM r JOIN s ON r.a = s.b
WHERE r.c < 15
GROUP BY r.d AGGREGATE SUM(r.e) AS sum
WHERE sum > 3                       -- 原地过滤聚合结果，无 HAVING
GROUP BY sum ORDER BY sum DESC AGGREGATE COUNT(*) AS cnt
```

其它特性：`SELECT` 可省略/可后置；`DISTINCT ON a BY b DESC` 顺序受控
去重；`FROM t SAMPLE 10 PERCENT` 源头采样；**倒装插入**
`VALUES (...) INSERT INTO sales`；`variant` 求和类型替代多表 join。
查询引擎为向量化 + 分布式 MPP；**优化器设计未公开**。

生态代价（官方自认）：BI 工具等 SQL 生态需要 SQL→ScopeQL 转译器
（他们论证可行，因为共享关系代数基础）。

## 8. 与 dendro 现状的对照

| 维度 | ScopeDB | dendro 现状 | 差异评注 |
|------|---------|-----------|---------|
| 形态 | 闭源云服务 | 开源 Apache-2.0 | 不同赛道 |
| 协议 | 私有 HTTP API + ScopeQL | pgwire/mywire，标准 SQL | **dendro 生态位更稳**（BI/工具零适配） |
| 事务 | 无公开事务模型 | OCC MVCC + group-commit WAL + 分支租约 fencing | dendro 强 |
| 版本化 | **无** | prolly tree + git 分支 + chunk diff merge | **dendro 独有** |
| 内容寻址 | 未提及 | CID 命名 CBF 块 | dendro 独有（天然去重/manifest） |
| S3 读路径 | 确定性 I/O 规划 + 并发 range read + 激进剪枝 | manifest + WAL 恢复；读路径并发稀疏读待查 | **ScopeDB 领先，值得学** |
| 二级索引 | 四类语义索引 + CLUSTER BY 聚类 | zone map（CBF footer）+ 结构化查询 | **ScopeDB 领先，值得学** |
| 缓存 | percas/cache2（正确性与缓存解耦） | 进程内 lru | ScopeDB 领先，值得学 |
| 半结构化 | variant + 路径索引/物化索引 | 无（列存 schema 固定） | 按需借鉴 |
| AP 执行 | 向量化 + MPP 分布式 | 向量化列式（单机） | dendro 单机先行，分布式远期 |
| 压缩 | 按类型（细节未公开） | CBF 分层 codec（RAW/BITPACK/RLE_DICT/FSST，GPU 可解码） | dendro 更精细且公开 |

## 9. 对 dendro 的可借鉴点清单

| 优先级 | 项 | 来源 | 说明 |
|--------|---|------|------|
| **P0** | 确定性 I/O 规划（执行前定 byte-range、**永不 LIST**） | blog/cloud-elasticity | dendro 的 manifest + CID 已具备全部前提（manifest 记录块位置即"事务性 metadata 服务"的等价物）；应把"planner 输出含对象内字节范围、扫描阶段零 LIST 零 HEAD"定为 S3 后端读路径的**硬性验收项** |
| **P0** | 并发异步 range read 流水线（吞吐优先） | blog/cloud-elasticity | S3 TTFB 高，需并发几十个 range GET 饱和带宽；dendro 对象存储读路径应做并发预取 + 向量化消费的流水线（tokio 并发读 CBF 块），而非串行逐块拉取 |
| **P1** | segment 级语义索引（zone map 之上加 point/range 剪枝位图或 bloom） | docs/guides/add-indexes | CBF footer 已有 zone map；可对高频等值列（如 branch_id、tenant_id）加块级 bloom/位图，挂 manifest 索引区，query-first 原则选列 |
| **P1** | CLUSTER BY 式布局提示 | docs/guides/storage-architecture | dendro 后台物化/compaction 时按聚类键重写 CBF 段（append-only 天然支持重排 = 新 commit）；布局提示不改语义，只影响剪枝效率，实现成本低收益高 |
| **P1** | 缓存与正确性解耦 + SSD 层 | percas / cache2（**Apache-2.0，可直接复用代码**） | dendro 当前进程内 lru；cache2（bounded RAM+SSD）可作为 CBF 块缓存的 SSD 层参考实现；原则：节点重启缓存全失效也不影响正确性（dendro 无状态恢复已满足，补缓存层即可） |
| **P2** | MATERIALIZED INDEX ≡ 物化分支 | docs/guides/add-indexes | dendro 有 ScopeDB 没有的杀手锏：**物化视图 = 一个自动维护的分支**（ prolly tree 快照 + 增量重算）；把"表达式物化"映射为分支族，语义上比 ScopeDB 的物化索引更强（可 time travel） |
| **P2** | 摄入管道（写入时 reshape） | blog/insight-in-no-time | dendro 若做 bulk ingest API，可借鉴"data cable"形态：一次 HTTP POST + 声明式转换（过滤/cast）在服务端摄入时完成，而非先落库再 ETL |
| **P2** | retention 异步过期 | docs/guides/data-retention | dendro 已有 GC（墓碑 + manifest 原子发布 + 保留窗口），语义已覆盖；只需确认过期是**异步**不阻塞读写（ScopeDB 同款承诺） |
| **远期** | pipe syntax 作为可选扩展 | blog/scopeql-origins + GoogleSQL pipe 论文 | dendro 走 PG/MySQL 兼容路线，**不应**换语言；但可评估 PG 语法扩展（`FROM t \| WHERE ...`）作为 Agent 友好的简写——dendro 的 AI 原生定位与管道语法（组合性好、生成容错高）契合 |
| **远期** | MPP 分布式执行 | tison 演讲 | dendro 单机向量化先行；ScopeDB 的"计划任务分派到无关节点"依赖其无状态 shared-disk，dendro 的分支租约 + 共享对象存储同样成立，列为 P3/多节点路线参考 |

**反面结论（不借鉴）**：

- **不借鉴其无版本化模型**——dendro 的 git 分支/内容寻址是核心差异化，
  ScopeDB 在此轴上为零，保持错位优势；
- **不借鉴自研查询语言**——dendro 的 pgwire/mywire 生态位是护城河，
  ScopeQL 的生态代价（BI 转译器）官方自己都承认；
- **无代码可抄主引擎**——闭源，只吸收公开设计；可复用代码仅限
  percas/cache2/scopeql-parser 等外围（Apache-2.0）。

---
*生成方式：WebSearch + WebFetch（scopedb.io 三篇架构博客、docs.scopedb.io
storage-architecture/add-indexes/stmt-query、GitHub org API 全量 12 仓库、
tisonkun.org 演讲稿）。casql 仓库实测 404。所有对外宣称的性能/成本数字
（10x、70% 降本、百万行/秒）均未经独立验证。*
