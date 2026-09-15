# SQL 解析测试集调研（分层：SQL → AST → IR）

- 日期：2026-09-15
- 背景：解析步骤独立成层——`SQL → AST（可选，可缓存/可绕过）→ 正式 IR`；
  查询改写/算子下沉重组是后续优化器的另一部分，不在本文范围。
- 结论先行：**按层配测试集**——P 层（SQL→AST）用 sqlparser-rs 语料 +
  PG 回归语料做 parse-only 冒烟 + 属性 fuzz；L 层（AST→IR）用 golden IR
  （09 号 spec 的三合同）；端到端继续用 slt（已在用）。跨层差分
  （AST 路径 vs IR 路径同果）是迁移期主护舷。

## 1. 测试集全景（按层归属）

### P 层：SQL → AST

| 测试集 | 规模/形态 | 对 dendro 的用法 |
|--------|----------|-----------------|
| **sqlparser-rs YAML 语料**（apache/datafusion-sqlparser-rs，tests/sqlparse_*）| 方言标注的 SQL→AST 期望 + **round-trip 测试**（AST 序列化回 SQL 再解析恒等）| **直接复用**：我们就是用这个 parser——升级版本时跑它的语料=上游回归；另可抽取其 PG/MySQL 方言子集做我们包装层（DendroTimeTravelDialect）的回归 |
| **PostgreSQL 回归语料**（src/test/regress，.sql/.out）| 数万语句；新 parser 常以"能解析多少"为验收（有项目 4.5 万语句 99.6% 通过）| **parse-only 冒烟**：只测 `parse_batch` 不执行——量化"不支持语法占比"并随版本收敛；期望输出不用管 |
| **MySQL MTR（mysql-test）** | 量大但执行器耦合深 | 低优先：wire 兼容 MySQL 时再取其语法子集 |
| **NIST SQL92 套件** | 历史标准一致性，老旧 | 仅作参照，不引入 |
| **5 个 Rust parser × 8300 真实 PG 查询**（社区基准）| 误报/召回对比 | 方法可借鉴：真实负载（非合成）测 parser，dendro 可在 pgwire 场景收集脱敏失败样本 |

**P 层 fuzz**：cargo-fuzz 对 `split_statements + parse_batch`（含
DendroTimeTravelDialect 回退路径）——不可信输入不 panic（与 WAL 帧的
Kani H1 同一纪律）；种子用 sqlparser-rs 语料。

### L 层：AST → IR（降级/绑定）

| 手段 | 依据 |
|------|------|
| **golden IR 文件**（09 号 spec P1/P2/P3 三合同）| 语料每查询 dump `.ir` 入库；降级器改动 = diff 可审阅 |
| **slt 语料双跑**：现有 26 文件全部走"解析→降级→IR dump"，golden 化 | 行为等价的静态证明（语义不变 ⇒ IR 只随降级器实现变）|
| **verifier 负测试**：构造非法 IR（坏跳转/类型错配/列越界）构造期即拒 | 09 §4 |
| 附录 A（03 §7.5）Expr→现状语义清单 | 对拍基准，差分红了以它裁决 |

### 端到端（执行正确性，已在用）

- **sqllogictest**：SQLite 原版语料 ~6.7M 语句/4.26M 测试（体量太大，
  我们自建 26 文件走格式；如需扩量可抽样原版语料的子集）；
- DuckDB 语料（本地 ref-projects/duckdb/test，数千 .slt/.test）：
  取语法子集做 parse/L 冒烟可行，期望值不直接用（方言差异大）；
- **SQLancer**（TLP/noREC 差分）：语义层，等 IR 化完成后接入——
  对 dendro 尤其适合（force_source 各路径 × 变换查询正是天然差分对）；
- **SQLRight / SQLaser / ParserFuzz（2025）/ Griffin / QTRAN**：fuzz
  研究前沿（语法引导变异/子句引导/从 DBMS 语法定义自动抽文法）——
  观察旗；dendro 的等价物是 dante 约束式生成器思路（ref-projects/
  readyset/dante 是现成参照）。

### 学术参照

- **Understanding and Reusing Test Suites Across Database Systems**
  （arXiv 2410.21731）：对比 SLT / MySQL MTR / PG regress / DuckDB
  四套语料的跨引擎复用研究——"复用他库语料"的方法论依据。

## 2. 对"AST（可选）"层的测试含义

AST 是**可缓存（L1 已做）/可绕过**的中间态，IR 是正式契约：
- 每个边界一个 round-trip/等价合同：SQL→AST（parse 幂等，miss 路径
  与命中路径同果——plan_cache.rs 已固化）；AST→IR（降级确定性）；
  IR→text（09 P1/P2）；
- **迁移期差分**：同一 SQL 经"AST 直评旧路径"与"降级 IR 新路径"
  执行结果全等（07 号 spec 的强制派发差分就是它的执行层形态）；
- 文本 IR（09）使"解析快照"可入库：失败样本粘 .ir 即可离线重放
  L 层——不需要带库重放（agent 场景报障友好）。

## 3. 落地清单（并入 ir-spec 07）

1. **B0-a**：vendor sqlparser-rs PG/MySQL 方言语料子集 →
   `tests/parser_corpus/`（P 层回归；升级 parser 版本的验收门）；
2. **B0-b**：PG 回归语料 parse-only 冒烟脚本（量化不支持语法占比，
   输出收敛曲线进 docs）；
3. **B0-c**：cargo-fuzz parse_batch 短 fuzz（clean panic 断言）；
4. **L 层随 v2b/v2c-1**：slt 语料 golden IR 化（26 文件先跑通流程）；
5. SQLancer/SQLRight 接入排 IR 化完成之后（v2c-2+）。

## 引用来源

- [sqlite.org/sqllogictest](https://sqlite.org/sqllogictest) · [HN：4,259,065 tests](https://news.ycombinator.com/item?id=33070247) · [Reddit：6.7M statements](https://www.reddit.com/r/SQL/comments/dln2jk/testing_sql_engine_correctness_with_sqllogictests/)
- [DuckDB sqllogictest 文档](https://duckdb.org/docs/lts/dev/sqllogictest/intro.html) · [duckdb-sqllogictest-python](https://github.com/duckdb/duckdb-sqllogictest-python)
- [跨引擎测试集复用研究（arXiv 2410.21731）](https://arxiv.org/html/2410.21731v1)
- [apache/datafusion-sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs) · [5 个 Rust parser × 8300 真实 PG 查询](https://www.reddit.com/r/rust/comments/1repqh1/5_rust_sql_parsers_on_8300_real_postgresql/) · [Bytebase parser 综述（PG 回归语料 4.5 万语句验收先例）](https://www.bytebase.com/blog/top-open-source-sql-parsers/)
- [SQLancer](https://github.com/sqlancer/sqlancer) · [SQLRight (ISSTA'24)](https://huhong789.github.io/papers/liang:sqlright.pdf) · [ParserFuzz (2025)](https://arxiv.org/html/2503.03893v1) · [SQLaser (2025)](https://arxiv.org/html/2407.04294v1) · [Griffin (ASE'22)](http://www.wingtecher.com/themes/WingTecherResearch/assets/papers/ASE22-Griffin.pdf) · [QTRAN (2025)](https://dl.acm.org/doi/10.1145/3728908)
- [Databend sqllogictest 实践](https://medium.com/@databend/sqllogictest-illustrated-2807a92e1149) · [Embucket SLT 方言兼容案例](https://embucket.com/blog/how_we_tested_embuckets_snowflake_compatibility_with_sql_logic_tests-slt)
