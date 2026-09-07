# SPEC 07 — SQL 能力面与测试基线

状态：**定稿 v1**

## 1. 类型系统（内部 8 类）

`Null, Bool, Int32, Int64, Float64, Utf8, Bytes, Date32, TimestampMs`
DDL 类型映射：SMALLINT/INT→Int32；BIGINT/SERIAL/BIGSERIAL→Int64(默认 NOT NULL+序列语义
简化：BIGSERIAL=Int64 NOT NULL，值由 memtx 分配原子计数器)；REAL/DOUBLE→Float64；
TEXT/VARCHAR/CHAR/STRING→Utf8；BYTEA/BLOB→Bytes；BOOLEAN/BOOL→Bool；DATE→Date32；
TIMESTAMP→TimestampMs；DECIMAL v1 落 Float64 并告警（v2 定点）。

**已知偏离（vs PostgreSQL 类型规则）**：整数算术一律升宽为 Int64
（`SELECT 1+2` 返回 INT8 而非 PG 的 `int4+int4=int4`）。取舍：消除
Int32 回绕（曾因 `as i32` 截断产生静默错值）；溢出时报 SQLSTATE 22003
（`sql/expr.rs`，checked 算术）。v2 若需对齐 PG 的类型提升表再收窄。

## 2. 语句覆盖矩阵 v1

| 语句 | 支持 | 备注 |
|------|------|------|
| CREATE TABLE (列约束: PK/NOT NULL/DEFAULT) | ✔ | PK 强制；UNIQUE v2 |
| DROP TABLE / TRUNCATE | ✔ / ✔(=删全表行,新 commit) | |
| ALTER TABLE ADD/DROP COLUMN | ✔(元数据级,旧行补 NULL) | 改类型 v2 |
| CREATE/DROP INDEX | 语法接受,报 NOTICE 未实现 | 二级索引 v2 |
| INSERT INTO ... VALUES(多行)/DEFAULT | ✔ | upsert v2 (ON CONFLICT) |
| DELETE ... WHERE / UPDATE ... SET | ✔(实现为旧版本 tombstone+新版本) | 物理仍只写 |
| SELECT [projection][where][group by][having][order by][limit/offset] | ✔ | |
| JOIN: INNER/LEFT/RIGHT/CROSS on equi | ✔(hash join) | 复杂谓词 join v2 |
| 聚合: count/sum/avg/min/max/count(distinct) | ✔ | |
| 标量函数: 数学(+-*/%,abs,round,floor,ceil,pow,sqrt), 字符串(length,upper,lower,substr,concat,trim,replace,split_part), 类型(cast,::), 时间(now,date_trunc,extract), 条件(coalesce,nullif,case when), 聚合过滤 | ✔ | 按需扩 |
| 子查询: 标量/IN/EXISTS/DERIVED 表 | 标量+IN ✔，EXISTS/DERIVED v2 | |
| UNION [ALL] | ✔ | INTERSECT/EXCEPT v2 |
| CTE (WITH) | v2 | |
| 窗口函数 | v2 | |
| BEGIN/COMMIT/ROLLBACK/SAVEPOINT | ✔ / ✔ / ✔ / 报错 | |
| SET / SHOW | ✔(会话参数, 宽松) | |
| EXPLAIN [ANALYZE] | ✔(计划文本) | |
| COPY | 报错 0A000 | v2 |
| **分支族**(见 SPEC 03 §5) | ✔ | |
| information_schema.tables/columns + pg_catalog 最小集 | ✔ | |

TP/AP 路由（会话内自动）：SELECT 且 (无索引点查条件 or 聚合/大范围) → AP 列式执行器；
否则 TP 行执行器。`SET dendro.force_engine = tp|ap` 调试覆盖。

## 3. 内部 AST 与 translate

sqlparser-rs 0.5x，`PostgreSqlDialect`（pgwire 入口）与 `MySqlDialect`（mywire 入口）
→ `translate::` 单向映射到内部 AST（参照 gluesql/core/src/translate 的架构与
limbo postgres/parser/translator.rs 的"前置语句+主语句"结构）：

```
内部 Statement: Create{table,cols,constraints} Drop Insert Delete Update Select
Select: {proj: Vec<Expr>, from: TableRef, selection, group_by, having, order_by, limit}
Expr: Column/Literal/Binary/Unary/Func/Cast/Case/In/Subquery/Placeholder/Is...
```

未知语法：translate 失败 → 42601 + 指明不支持构造（诚实报错优于错误执行）。

## 4. 执行器

- TP（行式）：memtx 快照迭代 → filter → project → limit；点查走 prolly cursor+memtx 合并
- AP（向量化）：Arrow RecordBatch 管道 scan→filter→project→hash agg→sort→limit，
  谓词下推到 CBF 统计剪枝(SPEC 05 §7)；内存配额与溢出 v2（超限报错）
- EXPLAIN 文本：`Engine: tp|ap` + 算子树 + 剪枝统计

## 5. SQL 测试基线（对应需求 #5）

三层：

1. **sqllogictest 基线**（`tests/slt/`）：sqllogictest-rs 实现 `AsyncDB`(经 pgwire
   TCP 连本进程，协议级测试一举两得)。语料：
   - `tests/slt/dendro/*.slt`：自写，覆盖上表矩阵（基线，CI 必跑）
   - 上游 sqlite 官方 .slt（sqllogictest-corpus/test/select*.slt 等）按方言子集筛选
   - dolthub PG 语料（sqllogictest-corpus/pgtestdata/）选择性收录（配 skipif 机制）
2. **PG 语法清单**：readonly.refer/postgresql/src/test/regress/sql/*.sql 建立
   "PG 回归测试语句 → dendro 支持/不支持"清单（`tests/pg_grammar/`，脚手架生成），
   gram.y 产出项作为长期 roadmap 勾选表
3. **差分测试**（可选环境）：同语句跑 dendro 与真 PG，对比结果集（脚本
   `tests/differential/`；本环境无 PG 安装，输出脚本供有 PG 的环境执行）

基线数字写进 `tests/slt/BASELINE.md`（通过/总数），每次引擎变更刷新——衰退可见。

## 6. 与实现的映射

| SPEC 条目 | 代码 |
|-----------|------|
| AST/translate | `dendro-core/src/sql/{ast,translate}.rs` |
| TP 执行器 | `dendro-core/src/sql/exec/tp.rs` |
| AP 执行器 | `dendro-core/src/sql/exec/ap.rs` |
| 表达式求值 | `dendro-core/src/sql/expr_eval.rs` |
| slt runner | `crates/slt/src/main.rs` (bin) |
