# SQL 测试基线（SPEC 07 §5）

刷新方式：`cargo build -p slt && ./target/debug/slt run tests/slt/dendro`

## 当前基线

| 日期 | 引擎 commit 语料 | 文件数 | 通过 | 失败 |
|------|------------------|--------|------|------|
| 2026-09-07 | dendro 0.1.0（M0-M7 全量首版） | 7 | 7 | 0 |
| 2026-09-07 | + JOIN 基线（008，评审 P1-6） | 8 | 8 | 0 |

## 语料矩阵

| 文件 | 覆盖 |
|------|------|
| 001_ddl_crud.slt | CREATE/INSERT 主键冲突/UPDATE/DELETE/DROP/未定义表 42P01 |
| 002_expr.slt | 算术/abs/upper/substr/concat/length/coalesce/case/round |
| 003_agg_group.slt | sum/avg/max/count/group by/having |
| 004_branch.slt | CREATE/USE/MERGE/DROP BRANCH、分支隔离、CHECKPOINT |
| 005_tx.slt | BEGIN/COMMIT/ROLLBACK（显式事务原子性）|
| 006_null_order.slt | NULL 三值逻辑、IS (NOT) NULL、ORDER BY DESC NULLS FIRST（PG 语义）|
| 007_system.slt | cambium.branches / cambium.commit_log 系统视图 |
| 008_join.slt | INNER/LEFT JOIN、NULL 键不匹配、一对多放大、三表链、复合键（AND）、JOIN+GROUP BY、派生表 |

## 语义注记（与 PG 对齐的行为）

- `ORDER BY DESC` 默认 NULLS FIRST（PG 行为）
- 主键冲突 → 23505；未定义表 → 42P01
- 分支合并冲突 → 40001（serialization_failure）
- 空表 count(*) = 0；count(col) 跳过 NULL

## 语料来源

- `tests/slt/dendro/`：自写基线（引擎直连，进程内）
- `readonly.refer/sqllogictest-corpus/test/`：上游 sqlite 官方 .slt（方言子集，v2 接入）
- `readonly.refer/postgresql/src/test/regress/sql/`：PG 回归测试语法清单（roadmap 勾选表，v2 接入）

## 回归纪律

任何引擎变更后运行上述命令；通过数下降即性能/功能衰退，禁止合入。
