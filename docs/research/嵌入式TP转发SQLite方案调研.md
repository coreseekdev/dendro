# 嵌入式形态：TP 转发 SQLite 的方案调研（2026-09-19）

> 触发：嵌入式应用中，不排除 TP 部分转发给 SQLite（或 SQLite 兼容
> 库）的架构。生态主流模式与 dendro 的三个选项。

## 生态主流（2025-2026）：双层嵌入式 HTAP

**DuckDB sqlite_scanner 模式**（事实标准）：SQLite 文件 = 持久行存
/OLTP 引擎；DuckDB attach 只读做列存分析（扫描比 SQLite 行存快
~900× 的公开口径）。pg_duckdb 把同模式搬到 Postgres（"medium 规模
HTAP"）。原生派：`sqliteai/sqlite-columnar` 把列存做进 SQLite 扩展。
共同叙事：**零 ETL、单文件嵌入、事务与分析各用所长**。

## dendro 的三个选项

### A. 现状：dendro-sqlite（dendro 即引擎，SQLite C ABI 兼容）

宿主按 SQLite API 用 dendro——TP/AP/分支一体。
- 优：功能完整（分支/CAS/时间旅行）、无同步问题
- 劣：嵌入足迹（运行时 ~100MB 级二进制 + jemalloc 元数据）、
  TP 单行路径 vs SQLite 数十年打磨仍有差距、宿主迁移信任成本

### B. 混合：SQLite 为 TP 主存 + dendro 为 AP/物化层（用户提议方向）

SQLite 持有行存真源（WAL 模式），dendro 从 SQLite **变更流**增量
物化列存段（CAS 段不可变特性天然支持追加式 ingest），分析查询走
dendro 段；dendro-sqlite ABI 层做路由（TP 语句直通 SQLite，AP
形态走段）。
- 优：嵌入足迹骤降（AP 段按需驻留）、TP 用最成熟的引擎、宿主
  已有 SQLite 数据可零迁移接入（sqlite_scanner 式读 + 我们的段
  化）；dendro 的增量 ingest 正是 CAS 追加语义的强项
- 劣：**一致性边界**（段物化滞后于 SQLite 提交——需快照边界协议：
  SQLite WAL 帧序号 → 段版本；分析查询看到的是"最近物化点"）、
  双写路径的事务语义（TP 内的 DDL/分析函数路由复杂度）、分支
  语义跨引擎的映射（dendro 分支 vs SQLite 无分支——分支只在 AP 侧）
- 关键构件：WAL frame 监听（read_mark 之后的增量段化）或
  periodical ATTACH 重扫 + rowid 水位；段化复用现有 CBF writer

### C. 只读 attach：dendro 直接读 SQLite 文件做段化（无转发）

sqlite_scanner 式：dendro 分析层直接 attach SQLite 文件 → 一次性
段化入 CAS → 分析查询走段。**无 TP 转发**，TP 全留宿主。
- 优：最简单，一致性 = 每次段化的快照点；与 B 共享段化构件
- 劣：非增量（全量重段化或 rowid 水位增量——SQLite rowid 单调，
  增量可行但有删除空洞问题）

## 判定

1. **C 是 B 的子集与前置**：先做"SQLite 文件 → CBF 段"的段化器
   （复用 CBF writer + rowid 水位），即可验证端到端价值（分析
   提速 + 足迹）。
2. B 的触发条件：真实嵌入方要求"单 API 双引擎"且接受物化滞后
   语义——届时 dendro-sqlite ABI 做语句路由（TP→SQLite 直通）。
3. A 与 B/C 不互斥：A 面向"dendro 全功能嵌入"，B/C 面向"轻足迹
   分析增强"——按宿主诉求选择，`dendro_sqlite_open` 的 flags
   区分模式（v2 ABI 预留位已有）。

## 信源

- DuckDB SQLite extension（读写 SQLite 文件）：
  https://duckdb.org/docs/stable/extensions/sqlite
- MotherDuck：How to choose an OLAP database（pg_duckdb HTAP 模式，
  2026-07）：https://motherduck.com
- sqliteai/sqlite-columnar（SQLite 原生列存扩展）：GitHub
- Tinybird：OLAP databases 2026（嵌入式分析叙事）：
  https://www.tinybird.co
