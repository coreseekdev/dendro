# dendro vs SQLite / MySQL / PostgreSQL 差异评估（2026-09-16）

> 基于 48k 行 Rust 源码 + 486 测试 + 31 slt + 三层形式化验证的现状。
> 评估目的：识别差距 → 产出任务清单 → 指导下一阶段方向。

## 0. 项目画像

| | dendro | SQLite | MySQL 8 | PostgreSQL 16 |
|---|---|---|---|---|
| 定位 | 云原生、追加 only、分支化分析数据库 | 嵌入式单文件 OLTP | 通用 OLTP + 主从复制 | 通用 OLTP/AP + 扩展生态 |
| 语言 | Rust（48k 行 src） | C（150k+ 行） | C/C++（1M+ 行） | C（1M+ 行） |
| 成熟度 | 研究原型→工程化 | 20+ 年生产、万亿部署 | 25+ 年生产 | 30+ 年生产 |
| 存储模型 | CAS + prolly 树 + memtx + CBF 列存 | B-tree 单文件 | InnoDB B+tree | heap table + 索引 |
| 协议 | pgwire + mywire | C API（无网络） | MySQL protocol | PG wire v3 |

## 1. 架构与存储

| 维度 | dendro | SQLite | MySQL | PG |
|------|--------|--------|-------|-----|
| 写入模型 | 追加 only（WAL→memtx→prolly→CBF） | 原地 B-tree | 原地 undo/redo | 原地 MVCC tuple |
| 内容寻址 | ✓ 全量 CAS | ✗ | ✗ | ✗ |
| 列存 | ✓ CBF（FSST+zstd，zone map，稀疏读） | ✗ | ✗（HeatWave 独立） | ✗（外挂扩展） |
| HTAP 单引擎 | ✓ Delta/Main/History 三层 | ✗ | ✗ | ✗ |
| 分支化 | ✓ Git 语义 | ✗ | ✗ | ✗ |
| 并行查询 | ✗ | ✗ | ✓ | ✓ |
| buffer pool | ✗（memtx + OS cache） | ✗（page cache） | ✓（LRU/flush） | ✓（shared buffer） |

**dendro 独有**：CAS + 分支化 + 单引擎 HTAP（三大数据库均无）
**dendro 落后**：无原地更新（UPDATE 写放大大）；无 buffer pool；无并行查询

## 2. SQL 面

| 特性 | dendro | SQLite | MySQL 8 | PG 16 |
|------|--------|--------|---------|-------|
| SELECT/JOIN | ✓ INNER/LEFT | ✓ 全部 | ✓ 全部 | ✓ 全部 |
| 集合操作 | ✓ UNION/EXCEPT/INTERSECT | ✓ | ✓（缺 EXCEPT/INTERSECT） | ✓ |
| 聚合 | ✓ 基础五函数 | ✓ 丰富 | ✓ 丰富 | ✓ 极丰富 |
| **窗口函数** | **✗** | ✓ | ✓ | ✓ |
| **CTE/WITH** | **✗** | ✓ 递归 | ✓ 递归 | ✓ 递归 |
| **WHERE 子查询** | **派生表 ✓；标量/IN ✗** | ✓ | ✓ | ✓ |
| 视图 | ✓ 简单 | ✓ | ✓ 物化 | ✓ 完整 |
| 索引 | ✓ 主键 | ✓ B-tree/RTree/FTS | ✓ B+tree/Hash/FTS | ✓ B-tree/GIN/GiST/BRIN |
| **外键约束** | **✗** | ✓ | ✓ | ✓ |
| **触发器** | **✗** | ✓ | ✓ | ✓ |
| **存储过程** | **✗** | ✗ | ✓ | ✓ |
| 时间旅行 | ✓ FOR SYSTEM_TIME AS OF | ✗ | ✗ | ✗ |
| GRANT/REVOKE | ✓ 表级 | ✗ | ✓ 细粒度 | ✓ 细粒度 |
| 类型 | 7 类（INT/BIGINT/TEXT/DOUBLE/BOOL/DATE/TIMESTAMP） | 动态 affinity | 完整含 JSON | 极完整含自定义 |
| **JSON 操作符** | **✗** | ✓ json1 | ✓ | ✓ JSONB |
| **全文搜索** | **✗** | ✓ FTS5 | ✓ | ✓ tsvector |
| **数组/范围类型** | **✗** | ✗ | ✗ | ✓ |

**关键缺口**（生产硬门槛）：窗口函数、CTE、WHERE 标量子查询、外键、触发器、JSON、全文搜索

## 3. 查询处理与优化器

| 维度 | dendro | SQLite | MySQL 8 | PG 16 |
|------|--------|--------|---------|-------|
| 框架 | 规则管线 + 计划执行 | 规则 rewrite 链 | 代价 hypergraph | 代价 bottom-up |
| 计划 IR | ✓ 逻辑 Plan + dendro.ir 文本 round-trip | VDBE 字节码 | Hypergraph | Plan tree |
| EXPLAIN | ✓ 计划块 + 派发行 + golden 锁定 | QUERY PLAN | JSON/TREE | JSON/TREE |
| EXPLAIN ANALYZE | ✓ 逐节点 actual + est | ✗ | ✓ | ✓ |
| 谓词下推 | ✓ 单源合取 | ✓ | ✓ | ✓ |
| 等值复制 | ✓ | ✗ | ✓ | ✓ |
| 统计传播 | ✓ [min,max] 交集 | ✗ | ✓ | ✓ |
| join 重排 | ✓ 贪心（≥3 因子） | ✓ 贪心 | ✓ exhaustive | ✓ exhaustive/GEQO |
| 投影裁剪 | ✓ CBF 跳列 + 稀疏 IO | ✗ | ✗ | ✗ |
| top-N | ✓ 有界堆 | ✓ | ✓ | ✓ |
| **代价估算** | **uniform + NDV 近似** | 表行数 | **直方图 + index dive** | **直方图 + extended stats** |
| **并行执行** | **✗** | ✗ | ✓ | ✓ |
| **计划缓存** | ✓ 16-shard + schema_version | ✓ | ✓ | ✓ |
| **差分验证** | ✓ 三轴 + §1.1 形式定义 | ✗ | ✗ | ✗ |

**dendro 独有**：dendro.ir round-trip + golden + 差分体系 + CBF 列存原生优化
**dendro 落后**：无直方图、无并行、贪心 vs exhaustive、无自适应计划切换

## 4. 事务与并发

| 维度 | dendro | SQLite | MySQL | PG |
|------|--------|--------|-------|-----|
| 隔离 | SI + MVCC | SERIALIZABLE（库级锁） | RC/RR | RC/SI/SS |
| 并发模型 | 单写者 per branch + 多读者 | 库级写锁 | MVCC + 2PL | MVCC |
| **死锁** | **不可能** | 不可能 | 可能（间隙锁） | 可能（SSI） |
| WAL 持久性 | ✓ Group/Always/NoWait | journal/WAL | redo + binlog | WAL |
| 提交管线 | ✓ 两段式 + **TLA+ 验证** | 单步 | 组提交 | 组提交 |
| **fencing** | ✓ manifest CAS + epoch | ✗ | ✗（GTID） | ✗（timeline） |
| 共识 | ✓ Raft | ✗ | Group Replication | streaming replication |
| **并发写吞吐** | **单写者限制** | 更差（库级） | ✓ 行级锁 | ✓ 行级锁 |

**dendro 独有**：不可能死锁 + TLA+ 验证 + CAS fencing
**dendro 落后**：单写者 per branch 限制并发写吞吐

## 5. 验证体系

| 维度 | dendro | SQLite | MySQL | PG |
|------|--------|--------|-------|-----|
| 测试 | 486 Rust + 31 slt | ~700k 行 | 数百万 MTR | 数百万 regression |
| **差分测试** | ✓ 三轴系统性 | ✗ | ✗ | ✗ |
| **形式化** | TLA+ / Kani / Verus | ✗ | ✗（内部 TLA+ 未公开） | ✗ |
| **计划锁定** | ✓ golden + round-trip | ✗ | ✗ | ✗ |
| **语义清单** | ✓ 附录 A 294 行 | 隐含 | 隐含 | 隐含 |

**dendro 核心差异化**——验证深度超过三大数据库同阶段水平

## 6. 生态成熟度

| | dendro | SQLite | MySQL | PG |
|---|---|---|---|---|
| 驱动/ORM | ✗ | 全语言 | 全语言 | 全语言 |
| 迁移工具 | ✗ | ✓ | ✓ | ✓ |
| 监控/告警 | ✓ Prometheus | ✗ | ✓ | ✓ |
| 社区 | 无 | 巨大 | 巨大 | 巨大 |
| 文档 | 内部 design docs | 极完善 | 极完善 | 极完善 |

## 7. 总结

| 维度 | 评分 | 说明 |
|------|------|------|
| 架构设计 | ★★★★☆ 领先 | CAS + 分支化 + 单引擎 HTAP 是下一代设计 |
| SQL 完整性 | ★★☆☆☆ | 缺窗口函数/CTE/子查询/外键/触发器/JSON——**生产硬门槛** |
| 优化器 | ★★★☆☆ | 规则管线 + 保守门控 + 差分验证体系领先；缺直方图/并行 |
| 事务 | ★★★★☆ | SI + 单写者 + TLA+ + fencing 设计优秀；并发写受限 |
| 存储效率 | ★★★★☆ | CBF 3.6:1 压缩 + 投影裁剪 + 稀疏 IO；追加 only 写放大 |
| 可观测性 | ★★★★☆ | EXPLAIN ANALYZE 逐节点 + dendro.ir 可审 + est vs actual |
| 验证体系 | ★★★★★ 领先 | 差分 + TLA+/Kani/Verus + golden + round-trip |
| 生态成熟度 | ★☆☆☆☆ | 48k 行 vs 数百万行；无驱动/ORM/社区 |

**一句话**：dendro 在**架构设计**和**验证体系**上领先三大数据库同阶段水平，在 **SQL 完整性**和**生态成熟度**上有显著差距。适合定位为"下一代分析数据库的研究原型，核心差异在分支化 + 单引擎 HTAP + 形式化验证"。

---

## 8. 任务清单（按优先级，从差距识别产出）

### P0：SQL 生产硬门槛（不补无法替代现有系统）

| 任务 | 预估规模 | 依赖 | 说明 |
|------|---------|------|------|
| 窗口函数（ROW_NUMBER/RANK/DENSE_RANK/NTILE/LAG/LEAD/FIRST_VALUE/LAST_VALUE + OVER(PARTITION BY...ORDER BY...)) | 大 | ORDER BY 已有 | 执行层 SortOp 已有分区能力雏形；解析层 sqlparser 原生支持 |
| CTE / WITH（非递归 + 递归） | 中 | 派生表已有 | 非递归 = 内联展开；递归 = 迭代不动点 |
| WHERE 标量子查询 / IN 子查询 / EXISTS | 中 | ScalarProgram 已有 | 子查询 = 相关/非相关求值；非相关可一次求值 |
| 外键约束（REFERENCES / ON DELETE CASCADE） | 中 | DDL 层 | INSERT/UPDATE/DELETE 时检查父表存在性 |
| DISTINCT on 单列 GROUP BY 语义统一 | 已有 ✓ | — | — |

### P1：性能与可伸缩性

| 任务 | 预估规模 | 依赖 | 说明 |
|------|---------|------|------|
| 并行查询（parallel scan / parallel aggregate） | 大 | ChunkPlane 已有 | rayon 或手线程池；Scan 产批→多 worker 聚合 |
| 直方图（等高/等频 + most-common-values） | 中 | CBF footer 已有 | 写入时收集；ANALYZE 命令；estimate_filter_rows 接入 |
| Filter Pull-Up 跨表传播 | 中 | Filter{Join} 重写 | DuckDB pull-up + push-down 协同 |
| 自适应计划切换（执行中途降级 join 策略） | 远期 | — | 参考 MySQL hash→BNL 降级 |
| 列存字典编码（低基数列） | 中 | CBF 格式 | FSST 已有；补 RLE/dictionary for INT |

### P2：类型与功能

| 任务 | 预估规模 | 依赖 | 说明 |
|------|---------|------|------|
| JSON 类型 + 操作符（->> / jsonb_extract / @>） | 中 | types.rs | 解析层 sqlparser 支持；存储为 TEXT + 索引 |
| 数组类型（INT[] / TEXT[] + ANY/ALL） | 中 | types.rs | PG 兼容语法 |
| 全文搜索（倒排索引 + MATCH AGAINST / tsvector） | 大 | 索引层 | FTS5 / tsvector 方案 |
| 触发器（BEFORE/AFTER INSERT/UPDATE/DELETE） | 中 | 执行层钩子 | DDL 定义 + DML 时回调 |
| 存储过程 / 用户自定义函数 | 大 | 解析+执行 | PL/pgSQL 子集 |
| UPSERT / ON CONFLICT DO UPDATE | 小 | INSERT 已有 | PG 语法 |
| CHECK 约束 | 小 | DDL 层 | INSERT/UPDATE 时求值 |
| ENUM / SET 类型 | 小 | types.rs | 静态枚举 |

### P3：生态与运维

| 任务 | 预估规模 | 依赖 | 说明 |
|------|---------|------|------|
| 驱动（Rust / Go / Python / Node.js） | 中 | pgwire 已有 | 用 psql 兼容协议即可复用现有 PG 驱动 |
| 数据迁移工具（mysqldump/pg_dump → dendro） | 中 | — | CSV / COPY 协议 |
| 慢查询日志（log_min_duration） | 小 | 执行层 | 超时记录 SQL + 计划 |
| 在线 DDL 扩展（DROP COLUMN / RENAME / 修改类型） | 中 | DDL 层 | 当前仅 ADD COLUMN |
| 系统视图扩充（pg_catalog 兼容子集） | 中 | — | pg_tables / pg_columns / pg_indexes |
| 用户文档（SQL 参考 / 架构指南 / 快速上手） | 中 | — | 从 design docs 提炼 |

### P4：验证深化（现有体系扩展）

| 任务 | 预估规模 | 依赖 | 说明 |
|------|---------|------|------|
| sqllogictest 官方语料适配（SQLite origin 测试集） | 中 | slt runner 已有 | 已有 runner；需要适配非覆盖特性跳过 |
| Verus 证明扩展（hash_join 键消歧 / SortOp 稳定序 / dedup_rows） | 中 | L3 框架已有 | 镜像文件 + 漂移守护模式 |
| Kani harness 扩展（CBF 编解码 / columnar scan / prolly cursor） | 中 | L1 框架已有 | — |
| TLA+ 模型扩展（多 branch 并发合并 / Raft 共识） | 大 | L2 已有单 writer | — |
| 基准测试套件（TPC-H 子集 / JOB / Stack Overflow） | 中 | — | 与 SQLite/PG/DuckDB 对比 |

---
*生成方式：基于本 session 对 dendro 全部 48k 行源码的开发与评审经验 +
SQLite/MySQL/PG 公开文档的对比分析。任务清单从差距识别直接产出，
按生产可用性优先级排序。*
