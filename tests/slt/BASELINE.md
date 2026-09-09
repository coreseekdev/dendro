# SQL 测试基线（SPEC 07 §5）

刷新方式：`cargo build -p slt && ./target/debug/slt run tests/slt/dendro`

## 当前基线

| 日期 | 引擎 commit 语料 | 文件数 | 通过 | 失败 |
|------|------------------|--------|------|------|
| 2026-09-07 | dendro 0.1.0（M0-M7 全量首版） | 7 | 7 | 0 |
| 2026-09-07 | + JOIN 基线（008，评审 P1-6） | 8 | 8 | 0 |
| 2026-09-08 | + checkpoint 可见性基线（009，评审 R7-P0） | 9 | 9 | 0 |
| 2026-09-08 | + 游标/事务组合基线（010，Q-1b/Q-10） | 10 | 10 | 0 |
| 2026-09-09 | + 可见性矩阵基线（013，Q-1 优化器/R7-P0 回归防线） | 13 | 13 | 0 |
| 2026-09-09 | + 字符串操作 + 视图基线（014/015）+ 事务矩阵 + 分支生命周期（016/017） | 17 | 17 | 0 |

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
| 009_checkpoint_visibility.slt | **checkpoint→变更→查询**（UPDATE/DELETE 后归并、点查墓碑、重插已删键、空表 count、检查点后唯一性）|
| 010_cursors_txn.slt | 游标声明/分批 FETCH/ALL 耗尽/CLOSE；事务内 DML 与读一致性 |
| 011_edge_cases.slt | NULL 传播、空串 ≠ NULL、负数、i64 上界、ORDER BY NULL 筛选 |
| 013_visibility.slt | NULL 三值逻辑、常量短路（WHERE 1=0 / WHERE NULL）、LIMIT 下推、聚合 NULL 计数 |
| 014_string_ops.slt | upper/lower/length/substr、LIKE-free WHERE 比较、空串 ≠ NULL |
| 015_views.slt | CREATE VIEW / DROP VIEW / OR REPLACE / 视图筛选 |
| 016_txn_matrix.slt | 事务矩阵：DML 提交/回滚、空事务、自动提交混合 |
| 017_branch_lifecycle.slt | 分支创建/切换/写入/合并/删除/隔离 |

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
