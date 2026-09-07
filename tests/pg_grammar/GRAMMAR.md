# PostgreSQL 语法清单（roadmap）

来源：`readonly.refer/postgresql/src/test/regress/sql/`（PG 17.6 回归测试，224 个 .sql 文件）

语句首关键词分布（回归测试中出现次数）——作为 dendro SQL 能力覆盖的勾选基线：

| 关键词 | PG 回归中出现次数 | dendro v1 |
|---|---|---|
| SELECT | 19113 | ✅ 部分/全量 |
| CREATE | 8102 | ✅ 部分/全量 |
| INSERT | 5403 | ✅ 部分/全量 |
| DROP | 3007 | ✅ 部分/全量 |
| ALTER | 2991 | ✅ 部分/全量 |
| EXPLAIN | 1769 | ✅ 部分/全量 |
| SET | 1725 | ✅ 部分/全量 |
| BEGIN | 1119 | ✅ 部分/全量 |
| UPDATE | 1014 | ✅ 部分/全量 |
| RESET | 576 | ⬜ v2+ |
| DELETE | 499 | ✅ 部分/全量 |
| ROLLBACK | 420 | ✅ 部分/全量 |
| WITH | 382 | ⬜ v2（静默丢弃→待改 0A000） |
| DECLARE | 343 | ⬜ v2+ |
| COPY | 338 | ⬜ v2+ |
| EXECUTE | 337 | ⬜ v2+ |
| GRANT | 333 | ⬜ v2+ |
| MERGE | 301 | ⬜ v2+ |
| ANALYZE | 283 | ⬜ v2+ |
| FETCH | 260 | ⬜ v2+ |
| COMMIT | 256 | ✅ 部分/全量 |
| VACUUM | 180 | ⬜ v2+ |
| VALUES | 145 | ⬜ v2+ |
| REINDEX | 145 | ⬜ v2+ |
| DO | 143 | ⬜ v2+ |
| TRUNCATE | 135 | ✅ 部分/全量 |
| REVOKE | 133 | ⬜ v2+ |
| COMMENT | 122 | ⬜ v2+ |
| SHOW | 118 | ✅ 部分/全量 |
| SAVEPOINT | 114 | ⬜ v2+ |
| PREPARE | 98 | ⬜ v2+ |
| LOCK | 60 | ⬜ v2+ |
| CALL | 43 | ⬜ v2+ |
| CLOSE | 43 | ⬜ v2+ |
| REFRESH | 38 | ⬜ v2+ |
| DEALLOCATE | 32 | ⬜ v2+ |
| CLUSTER | 30 | ⬜ v2+ |
| RELEASE | 18 | ⬜ v2+ |
| CHECKPOINT | 7 | ⬜ v2+ |
| IMPORT | 4 | ⬜ v2+ |
| DISCARD | 4 | ⬜ v2+ |
| LISTEN | 2 | ⬜ v2+ |
| UNLISTEN | 2 | ⬜ v2+ |
| NOTIFY | 1 | ⬜ v2+ |

覆盖策略：先 SELECT/INSERT/UPDATE/DELETE/CREATE（dendro v1 ✅），
事务/SET/EXPLAIN 次之；GRANT/COPY/MERGE/CALL 等显式 0A000 报错。

正式差分测试（同语句对拍真 PG）脚本见 `tests/differential/`（v2）。
