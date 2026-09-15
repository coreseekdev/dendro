# ir-spec 03a · 附录 A：Expr → 现状语义清单

> **地位**：v2b ScalarStep 编译器的**行为基准**（对拍 ground truth，03 §7.5 交付物）。
> 与代码冲突时**以代码为准并更新本清单**。行号以当前工作区为准（dendro-core，sqlparser 0.62）。
> 验证标注：〔实证〕= 2026-09-15 经 crates/slt runner 逐条跑通；〔码读〕= 仅代码推断；
> 〔红〕= 现状与 SQL 标准三值语义不一致，R1/R2 需裁决是照搬还是修复。
> 关键文件：`crates/dendro-core/src/sql/expr.rs`（求值）、`scan.rs`（终结语境/JOIN/下推）、
> `optimize.rs`（折叠）、`agg.rs`（HAVING）、`ddl.rs`（UPDATE/DELETE）、`error.rs`（SQLSTATE）。

## A.0 全局约定与终结语境

- 求值入口 `expr::eval(e, row, cols)`（expr.rs:10）。默认**先递归求子、再合成**，子错误（`?`）即刻上抛。
- eager 通则：`BinaryOp`/`Between`/`nullif`/`mod`/`replace` 等**全部操作数先求值**（先左后右），NULL 判定在求值之后——`NULL AND 1/0` = 22012 而非 Null〔实证〕。
- `binop` NULL-first：任一侧 Null 直接返回 Null（expr.rs:410-412），故 `NULL = NULL` = Null、`NULL AND false` = Null（非 false）〔实证〕。
- 惰性形态：InList 命中即 break（expr.rs:95-98）、CASE 只求命中分支（expr.rs:145-151）、coalesce 逐参短路（expr.rs:341-347）。
- 错误码（error.rs:18-50）：42601 syntax｜42703 undefined column｜42804 datatype mismatch｜0A000 not supported｜22012 division by zero｜22003 integer out of range｜22P02 invalid text｜08P01 unbound parameter｜XX000 internal。

| 终结语境 | 语义（只认 `Bool(true)` 放行） | 位置 |
|---|---|---|
| WHERE(SELECT) | 先 optimize 折叠；无列引用先单次求值：Bool(false)/Null→**直接返回空 TableView**（聚合/投影全跳过）、Bool(true)→免过滤、非布尔→落回逐行**静默丢行**；逐行仅 Bool(true) 放行，Err→语句失败 | scan.rs:143-174 |
| WHERE(DELETE/UPDATE) | 逐行 `eval != Bool(true) → 跳过`；Null/非布尔静默丢行；**不经 optimize** | ddl.rs:613, 659 |
| JOIN ON | **不做布尔求值**：extract_equi 只提取等值列对（见 Q6/Q7） | scan.rs:1808-1988 |
| HAVING | 逐组求值，`!= Bool(true)` 丢弃；仅支持 Function/BinaryOp/Nested/Identifier/Value 子集，其余 0A000 `"HAVING: …"` | scan.rs:189-199, agg.rs:199-234 |

## A.1 Expr 形态主表

| 形态 | 求值顺序 | NULL 行为 | 错误行为 | 位置 | 常量折叠 |
|---|---|---|---|---|---|
| Value::Number/引号字符串 | 字面量 | — | — | expr.rs:12-17, 641-670 | 折叠输入仅认整型 Number（optimize.rs:62-70） |
| Value::Placeholder | — | — | 08P01 `unbound parameter {id}`（prepared 参数已在 mod.rs:1210-1231 替换） | expr.rs:13-15 | 无 |
| Value::Boolean/Null/Hex/Escaped | 字面量 | Null→Null | Hex 解码失败→空 Bytes（unwrap_or_default） | expr.rs:646-655 | 无 |
| Identifier | 查列（大小写不敏感；WHERE 路径含限定名回退 scan.rs:465-484） | 行值原样 | 42703 `column "x" does not exist` | expr.rs:18-23 | 不折 |
| CompoundIdentifier | 取**末段**解析（a.b.c→c） | 同上 | 同上 | expr.rs:24-29 | 不折 |
| Wildcard（表达式位） | — | — | 42601 `wildcard not allowed in expression` | expr.rs:30-32 | 无 |
| BinaryOp{=,!=,<,<=,>,>=} | eager 左→右，null-first 后比较 | 任一 Null→Null | 混族比较→42804 `cannot compare …` | expr.rs:33-37, 414-426, 477-522 | 不折 |
| BinaryOp{AND,OR} | **eager：两侧都求值**（无短路） | 任一 Null→Null〔红：非三值逻辑〕 | as_bool 非布尔→42804；子式错误照抛 | expr.rs:427-428 | 折：true/false 恒等消除（optimize.rs:80-106） |
| BinaryOp{+,-,*,/,%} | eager | 任一 Null→Null | 整数 /0、%0→22012；i64 溢出→22003；Utf8 参算→42804；浮点 %0.0→**NaN 不报错** | expr.rs:429, 435-474 | 折：仅整型字面量 +−×（checked；**不折 / %**）optimize.rs:44-60 |
| BinaryOp{\|\|} | eager | 任一 Null→Null | 无（两侧 to_text 强转） | expr.rs:430 | 不折 |
| BinaryOp 其他（位运算等） | eager 两侧 | **任一 Null→Null（null-first 先于 not_supported）** | 其余 0A000 `operator …` | expr.rs:410-412, 431 | 不折 |
| UnaryOp{NOT} | 子式先求 | 子式 Null→**XX000 internal `NULL bool`** | 非布尔→42804 | expr.rs:41, 524-532 | 不折 |
| UnaryOp{-} | 子式先求 | Null→Null | 非数值→42804 `cannot negate` | expr.rs:42-48 | 折：整型字面量取负、-(-x)→x（optimize.rs:128-147） |
| UnaryOp{+} | 子式先求 | 原样 | 无 | expr.rs:49 | 折：+x→x |
| Nested | 透传 | — | — | expr.rs:53 | 递归（optimize.rs:27） |
| Cast（普通） | 子式错误照抛→cast | Null→Null | 见 Q14 cast 矩阵 | expr.rs:54-70, 556-599 | 不折 |
| Cast{Try/Safe} | 子式错误**也吞** | 一切失败→Null | 无（全吞） | expr.rs:60-66 | 不折 |
| IsNull / IsNotNull | 子式错误照抛 | 永不 Null，输出 Bool | 无 | expr.rs:71-72 | 结构递归（optimize.rs:28-29） |
| IsTrue / IsFalse | 子式错误照抛；as_bool 错误被 `matches!` **吞掉** | 子式 Null→false；非布尔→false（不报错） | 无 | expr.rs:73-80 | 不折 |
| IsNotTrue / IsNotFalse | **不求值** | — | 0A000 `expression: …`（eval 无此臂；has_column_ref 却认得 scan.rs:2116-2121） | expr.rs:195-198 | 不折 |
| InList{negated} | 左值先求；列表逐项：项 Null 或左值 Null→记 has_null 继续；命中即 break（其后项不求值） | found→true；否则 has_null→**Null（在 negation 之前返回）**；否则 false；最后 XOR negated | 与项比较混族→42804 | expr.rs:81-108 | 列表递归（optimize.rs:30-38）；pk 下推怪癖见 Q9 |
| Between{negated} | 三操作数**全 eager**（值→lo→hi），null 判定最后 | 任一 Null→Null（`NULL BETWEEN 1/0 AND 2`=22012 非 Null） | cmp 错误照抛 | expr.rs:109-124 | 不折 |
| Case（searched） | 条件逐个求；命中才求 result；else 仅无命中求 | 条件 Null→**XX000 internal `NULL bool`**；无命中无 else→Null | 条件非布尔→42804 | expr.rs:135-153 | 不折 |
| Case（simple） | operand 一次；条件逐个 | operand Null 或条件 Null→该 WHEN 不命中继续；无 else→Null | cmp 错误照抛 | expr.rs:131-148 | 不折 |
| Function | 实参逐一求（coalesce/greatest/least 惰性） | 见 A.2 | 未知函数 0A000 `function {name}`；命名参数/wildcard 参数→42601 | expr.rs:154, 207-392 | 不折（args 不入折叠） |
| Substring{FROM, FOR} | 串→起点→长度顺序求 | 任一 Null→Null（分位置短路） | 缺 FROM→42601；起点/长度非数值→42804；非串 to_text 强转 | expr.rs:155-190 | 不折 |
| TypedString | 字面量先 cast | — | cast 失败照抛（22P02 等） | expr.rs:191-193 | 不折 |
| Like/ILike/RLike、InSubquery、Exists、标量子查询等 | **不求值子式** | — | 0A000 `expression: …`（短显示 60 字符） | expr.rs:195-205 | 无 |

## A.2 Builtin 函数表（eval_function，expr.rs:207-392；函数名小写匹配）

| 函数 | 参数求值 | NULL 行为 | 错误行为 | 位置 |
|---|---|---|---|---|
| abs | 单参 | Null→Null（Int 保持宽度，Float 保持 Float） | 非数值→42804 `abs` | 240-249 |
| round | 单参 + 可选精度（as_i64） | **Null→42804 `expected number`（非 Null！）** | 输出恒 Float64（半值远离零） | 250-260 |
| floor / ceil / ceiling | 单参 | **Null→42804** | 输出恒 Float64 | 261-262 |
| sqrt | 单参 | **Null→42804** | 负数→NaN（不报错） | 263-266 |
| pow / power | 两参 eager | **Null→42804** | — | 263-269 |
| length / char_length / character_length | 单参 | Null→Null | 非 Utf8→42804；输出 Int32（字符数） | 271-275 |
| upper / lower | 单参 | Null→Null | 非 Utf8→42804 | 276-284 |
| substr / substring（函数形） | 逐参 eager | 串 Null→Null | **非 Utf8→42804（与 Expr::Substring 的 to_text 强转不同！）**；起点/长度非数值→42804 | 285-304 |
| concat | 逐参 eager | **Null 跳过（非传播）**；非串 to_text | 无；零参→`''` | 305-315 |
| trim / ltrim / rtrim | 单参 | Null→Null | 非 Utf8→42804 | 316-324 |
| replace | 三参全 eager | **任一 Null→42804 `replace`（非 Null！）** | arity≠3→42601 | 325-339 |
| coalesce | **逐参惰性短路** | 首个非 Null；全 Null/零参→Null | 已命中后的参数不求值故不报错 | 340-348 |
| nullif | 两参 eager | 任一 Null→返回 x；相等→Null | cmp 混族→42804 | 349-356 |
| greatest / least | 逐参 eager（全参数都求值） | **Null 跳过（非传播）**；全 Null→Null | cmp 混族→42804 | 357-382 |
| mod | 两参 as_i64 eager | **Null→42804（非 Null！）** | y=0→22012；输出 Int64，符号随**除数**（rem_euclid×signum） | 383-389 |

## A.3 字面量与类型系统要点

- `number_or_string`（expr.rs:659-670）：i64 可析且在 i32 域→Int32；否则 i64→Int64；否则 f64→Float64；否则 Utf8。**带引号字符串走同一路径**（expr.rs:643-645）：`'42'`→Int32、`'1.5'`→Float64、`'007'`→Int32(7)〔实证〕；`'4x2'`→Utf8。E'…' 不数值化（expr.rs:654）。
- `as_bool`（expr.rs:524-532）：Bool；Int≠0；**Null→XX000**；其余（含 Float64/Utf8）→42804。
- `as_i64`（expr.rs:534-546）：Int 家族；Bool(0/1)；Date32（天）；TimestampMs（ms）；**Float64→42804（浮点不能转整数！）**〔实证 `CAST(1.5 AS BIGINT)` 42804〕；Utf8→42804。
- `as_f64`（expr.rs:548-552）：Float64 或 as_i64×1.0（Bool/Date/Ts 可入浮点）。
- `cmp_values`（expr.rs:477-522）：数值族（Int32/Int64/Float64/Date32/TimestampMs）跨型可比：任一 Float→as_f64 比较，否则 as_i64 比较。**Date（天）与 Timestamp（ms）按原始单位直比、无归一**〔实证：同日 `DATE < TIMESTAMP` = true〕。Bool↔Bool、Utf8↔Utf8（字节序）、Bytes↔Bytes；其余组合→42804 `cannot compare`（含 Bool↔Int、Utf8↔Int）〔实证〕。Null 输入给确定序（Null=Null Equal），仅当调用方漏检时可达（482-491）。
- `arith`（expr.rs:435-474）：任一 Float64→Float64 运算（`%0.0`→NaN 无检查）；否则 i64 checked（/0、%0→22012；溢出→22003）。**输出一律 Int64**（Int32+Int32→Int64）〔实证 `2147483647+1`→Int64 2147483648〕。Date/Timestamp 可参算（按天数/ms），`DATE '2024-01-05' + 1`→Int64(19728)，**结果不是日期**〔实证〕。
- `to_text`（expr.rs:394-406）：**Null→`''`**（Q7 死哨兵根源）；Float→format_f64；Bytes 逐字节 as char。
- `cast_value`（expr.rs:556-599）：目标 Boolean/Int 家族/BigInt/浮点族/文本族/Bytea/Date/Timestamp；其余目标→0A000。细节怪癖见 Q14。

## A.4 怪癖清单（对拍重点）

- **Q1**〔实证〕AND/OR/比较 eager + null-first：两侧先求值再判 Null。`NULL AND false`=Null
  （标准=FALSE）；`NULL AND 1/0`=22012（标准=Null）。expr.rs:33-37, 410-412, 427-428。
- **Q2**〔实证〕`NOT NULL`→XX000 internal `NULL bool`（as_bool 对 Null 报错，注释自认
  "调用方通常先查 null"）。连带：`NOT (x IN (…,NULL))` 未命中时同样 XX000。expr.rs:41, 529。
- **Q3**〔实证〕searched CASE 条件为 Null→XX000 `NULL bool`（expr.rs:143）；simple CASE
  操作数 Null 或条件 Null→不命中继续（expr.rs:137-142），落到 ELSE/Null。
- **Q4**〔实证〕InList：found 即 break（其后项不求值，`1 IN (1, 1/0)`=true）；has_null 的
  Null 返回发生在 XOR negated **之前**（`1 NOT IN (2,NULL)`=Null，三值正确）。expr.rs:81-108。
- **Q5**〔实证〕终结语境只认 Bool(true)：`WHERE 1`（非布尔）**静默 0 行不报错**；更怪：
  WHERE 常量 false/Null 短路直接返回空 TableView，**聚合也被跳过**——`count(*) WHERE NULL`
  **无结果行**（而 `WHERE 1` 路径反而返回 count=0，两路径不一致）。scan.rs:151-174。
- **Q6**〔实证〕JOIN ON 非布尔求值：AND 链中**非等值合取被静默丢弃**（walk 返回值被忽略，
  scan.rs:1944-1953）；无等值条件→0A000（1979-1986）；Or/非等值顶层→0A000。
- **Q7**〔实证〕LEFT JOIN 死哨兵：`\x00NULL` 检查永不命中（to_text(Null)=`''`，expr.rs:396），
  键又无类型 tag（hash_join_left scan.rs:1887-1908）→ **NULL 键 ↔ NULL 键可匹配**（实证
  `1|1|NULL`，标准应 null-pad）；Int64(1)↔Utf8("1")、NULL↔`''` 同样可碰撞〔码读〕。
  INNER join 正确：mkkey 带 type tag 且 Null 键跳过（scan.rs:1819-1834）。
- **Q8**〔实证〕LIKE 未实现：sqlparser 0.62 中 LIKE 是**独立 Expr::Like 变体**（非
  BinaryOperator），落 eval 兜底 0A000 `expression: …`；**子式不求值，NULL 也不短路**。
  ILike/RLike/Any/All/子查询同。expr.rs:195-198。
- **Q9**〔实证〕单列 pk 表 `id NOT IN (…)`：try_pk_pushdown 不检查 negated（scan.rs:1183-1186）
  → build_point_view 落 `_ => vec![]` 空键集（scan.rs:1248-1256）→ **静默 0 行**。
- **Q10**〔实证〕引号数字字面量被数值化（`'42'`→Int32、`'42'+1`=43、`'007'`→7）。
- **Q11**〔实证〕类型提升要点：算术任一 Float→Float64，否则一律 Int64；数值族跨型比较
  （浮点优先）；Date/Timestamp 属数值族但按原始单位（天 vs ms）参与比较与算术。
- **Q12**负数字面量：eval 对 Int32/Int64/Float64 保持宽度取负（expr.rs:42-48；
  Int32::MIN 取负未设防——release 回绕/dev panic，〔待核对构建 profile〕）；
  optimize 折 `-literal`（optimize.rs:130-136）；pk 下推 expr_to_literal 认 `-literal`
  （scan.rs:1206-1223，S-3 修复）；i64::MIN 字面量解析溢出→Float64〔码读〕。
- **Q13**〔实证〕round/floor/ceil/sqrt/pow/mod/replace 对 NULL 报 42804（非 strict-NULL 语义）。
- **Q14**〔实证〕cast 怪癖：`CAST('42' AS BIGINT)` 成功仅因字面量已数值化，列上Utf8 数字串
  →42804；`CAST(1.5 AS INT/BIGINT)`→42804（as_i64 拒 Float64）；`CAST(3000000000 AS INT)`
  →-1294967296 静默截断；`CAST('true' AS BOOLEAN)`→42804；`CAST(19727 AS DATE)`→42804
  （date+1 结果无法 cast 回日期）；`DATE '2024-13-99'` 不校验月日范围；
  timestamp 秒字段解析失败静默按 0（expr.rs:631-634）。
- **Q15**optimize 只在 WHERE 一处调用（scan.rs:146，全库唯一）；只折整型 +−×、布尔恒等、
  ±号；**不折** / %、比较、函数；折叠产物重走字面量解析（Int32 域）而运行时算术输出 Int64
  ——同值不同类型 tag（投影 `1+2`=Int64 vs WHERE 折后=Int32）〔码读〕。
- **Q16**〔实证〕IS TRUE/IS FALSE 吞 as_bool 错误：`'x' IS TRUE`=false 不报错；但
  IS NOT TRUE/IS NOT FALSE→0A000。
- **Q17**〔码读〕JOIN 输出同名列在 cols_lookup 的 HashMap collect 中后者覆盖前者→
  裸列名解析到**最右**表的列（scan.rs:486-492）。

## A.5 对拍清单（slt 种子，期望值全部〔实证〕；〔红〕处现状=缺陷，v2b 裁决）

```slt
# 字面量（Q10）
query TTTTT
SELECT '42', '1.5', '007', 2147483648, - -5
----
42 1.5 7 2147483648 5

# 比较/逻辑（Q1）
query TTTTT
SELECT NULL = NULL, NULL AND false, 1 AND 2, NOT 0, 1 = 1.0
----
NULL NULL true true true

# 算术/类型提升（Q11）
query TTTTTT
SELECT 2147483647+1, -7/2, 7/-2, mod(7,-3), 1.0 % 0.0, DATE '2024-01-05'+1
----
2147483648 -3 -3 -1 NaN 19728

# InList 三值（Q4）
query TTTTTT
SELECT 1 IN (2,NULL), 1 IN (1,NULL), 3 IN (1,2), NULL IN (1,2), 1 NOT IN (2,NULL), 1 NOT IN (1,2)
----
NULL true false NULL NULL false

# Between / CASE（Q3）
query TTTTT
SELECT NULL BETWEEN 1 AND 3, 3 BETWEEN 1 AND NULL, CASE NULL WHEN 1 THEN 'a' ELSE 'b' END, CASE 2 WHEN NULL THEN 'a' ELSE 'b' END, CASE WHEN true THEN 1 ELSE 1/0 END
----
NULL NULL b b 1

# IS 族（Q16）
query TTTTTT
SELECT NULL IS TRUE, 'x' IS TRUE, NULL IS FALSE, 0 IS FALSE, 3 IS TRUE, NULL IS NOT NULL
----
false false false true true false

# 函数 NULL/惰性（Q13；coalesce 惰性：1/0 不求值）
query TTTTT
SELECT concat('a',NULL,1.5,true), greatest(1,NULL,3), coalesce(1,1/0), substr(123,1,2), '42'+1
----
a1.5true 3 1 12 43

# StringConcat（null-first）
query TT
SELECT 'a'||'b'||NULL, 1||2
----
NULL 12

# CAST 矩阵（Q14）
query TTT
SELECT CAST('42' AS BIGINT), CAST(3000000000 AS INT), CAST(DATE '2024-01-05' AS TEXT)
----
42 -1294967296 2024-01-05

# SUBSTRING 语法形（负起点钳 0；越界钳 len）
query TTTT
SELECT substring('hello' FROM 2 FOR 3), substring('hello' FROM 0), substr('hello', -1), substring('hello' FROM 2 FOR 99)
----
ell hello hello ello

# 错误分支（`query error <消息子串>`；记录间必须空行，注释走 SQL 的 `--`）
query error NULL bool
SELECT NOT NULL -- Q2: XX000

query error NULL bool
SELECT NOT (1 IN (2, NULL)) -- Q2 连带

query error NULL bool
SELECT CASE WHEN NULL THEN 1 ELSE 2 END -- Q3 searched CASE

query error division by zero
SELECT NULL AND 1/0 -- Q1 eager（null-first 在求值后）

query error division by zero
SELECT 1/0

query error integer out of range
SELECT 9223372036854775807 + 1

query error cannot compare
SELECT 'a' < 1 -- cmp 混族

query error expression
SELECT 'a' LIKE 'b' -- Q8 LIKE 未实现

query error expression
SELECT (1 IS NOT TRUE) -- Q16 IsNotTrue 无臂

query error expected number
SELECT round(NULL) -- Q13 数学函数不吃 NULL

query error expected number
SELECT CAST(1.5 AS BIGINT) -- Q14 浮点不能转整数

query error cast to date
SELECT CAST(19727 AS DATE) -- Q14 Int 不能 cast 回 DATE

query error expected boolean
SELECT CAST('true' AS BOOLEAN)

query error abs
SELECT abs('x')

query error does not exist
SELECT no_such_col FROM (SELECT 1 AS x) t -- Identifier 42703

# 终结语境（Q5）
statement ok
CREATE TABLE wt (id BIGINT PRIMARY KEY, v BIGINT)

statement ok
INSERT INTO wt VALUES (1,10),(2,20)

query I
SELECT count(*) FROM wt WHERE 1
----
0

# 〔红〕Q5：现状无结果行（常量短路跳过聚合）；标准：一行 0
query I
SELECT count(*) FROM wt WHERE NULL
----
0

# 〔红〕Q9：现状静默 0 行；标准：返回 id=3
query II
SELECT id, v FROM wt WHERE id NOT IN (1, 2)
----
3 30

# JOIN（Q6/Q7）
statement ok
CREATE TABLE jl (id BIGINT PRIMARY KEY, k BIGINT)

statement ok
INSERT INTO jl VALUES (1, NULL), (2, 5)

statement ok
CREATE TABLE jr (id BIGINT PRIMARY KEY, k BIGINT)

statement ok
INSERT INTO jr VALUES (1, NULL), (2, 5)

# 〔红〕Q7：现状 NULL↔NULL 匹配出 1|1|NULL；标准：1|NULL|NULL（null-pad）
query III
SELECT jl.id, jr.id, jr.k FROM jl LEFT JOIN jr ON jl.k = jr.k ORDER BY jl.id
----
1 NULL NULL
2 2 5

# 〔红〕Q6：现状 2|2（id>id 合取被丢弃）；标准：0 行
query II
SELECT jl.id, jr.id FROM jl JOIN jr ON jl.k = jr.k AND jl.id > jr.id ORDER BY jl.id
----
```

R1/R2 起步建议：A.1/A.2 每行至少一条 slt（上表已覆盖 NULL 与错误分支）；
〔红〕条目先在 08 号文档立项裁决，再决定 v2b 是"忠实搬运"还是"顺手修复"。
