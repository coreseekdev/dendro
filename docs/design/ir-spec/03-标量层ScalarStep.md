# 03 标量层：ScalarStep 步列表

> 同构参照：PG11 ExprState/EEOP_*（十年生产验证）。
> 红线：**叶语义零改动**——步列表只改变控制流与绑定方式，运算语义
> 一律调用现有 eval 函数。这是行为等价的根基。

## 1. 指令集（封闭；函数类走表）

```rust
pub enum ScalarStep {
    Const(u32),              // 常量池索引（含 NULL：Const(NULL_IDX)）
    Col(usize),              // 行/块内列偏移（绑定产物）
    Param(u16),              // 参数槽位 $n（评审 P0-3：prepared 模板编译必需）
    Cmp(BinaryOperator),     // < <= = != >= like 等比较（语义=现 eval）
    Arith(BinaryOperator),   // + - * / %（含负数字面量已折 Const）
    LogicAnd / LogicOr,      // 三值逻辑短路（见 §4）
    Not,
    IsNull,
    Like(u32),               // 编译后的模式（常量池：预解析 LikePattern）
    Cast(Ty),
    Builtin(u32, u8),        // builtin 表 id + 实参个数（栈式取参）
    Jump(u32) / JumpIfFalse(u32) / JumpIfNull(u32),
    Out,                     // 结果落输出槽
}
pub struct ScalarProgram {
    steps: Vec<ScalarStep>,
    consts: Vec<SqlValue>,       // 常量池
    n_cols: usize,               // 绑定列数（越界检查）
}
```

**纪律**（承袭父文档 §8.2）：核心指令封闭于上表；新函数 = builtin 表
新条目，不加指令。目标：指令数 ≤ 20，builtin 表不限。

## 2. 编译（Expr → ScalarProgram）

1. **绑定**：列名 → ColId → 物理列偏移（绑定缓存命中则跳过）；
2. **常量折叠**：现有 optimize.rs 规则**迁移至编译期**执行一次
   （含负数字面量 `UnaryOp::Minus` → Const(-n)，R8 教训）；
3. **谓词 lowering**：`a AND b AND c` → 短路跳转结构（§4）；
   比较两侧常量提升进常量池；
4. **跳转回填**：两遍编译（先收集待回填跳转）。

编译期不做：collation 推导、统计推断、跨语句优化。

## 3. 后端一：eval_row（TP）

```rust
pub fn eval_row(p: &ScalarProgram, row: &[SqlValue], out: &mut SqlValue)
```

- 寄存器 = `SmallVec<[SqlValue; 8]>`（栈上，无堆分配——TP 热路径）；
- 逐行调用，供 DeltaPoint/RowFallback/退化管线使用；
- 越界/类型错 = 返回内部错误（不 panic；P0-2 同款纪律）。

## 4. 求值序与 NULL 语义（评审 P0-1 修正：现状复刻，非 SQL 标准）

**现状是唯一基准**（行为等价红线）：现行 `expr.rs binop`（408-433 行）的
AND/OR/比较是 **eager + null-first**：两侧**都先求值**，任一侧 NULL 即
返回 NULL，无短路；`NOT NULL` 今天是 internal error（as_bool(Null)
报错），**不是**返回 NULL；`SELECT (false AND NULL)` 今天返回 NULL
（不是 FALSE）。

因此 v1 编译模式为**忠实复刻**（禁止 Kleene 化）：

```
a AND b 编译为：            ; 与现 binop 逐步同构
  eval a -> r0              ; 总是求值（无跳转——见下）
  eval b -> r1              ; 总是求值（eager：右侧除零仍须报错！）
  And                       ; 合成：任一 Null ⇒ Null；否则 bool_and
```

- **v1 不引入任何求值短路**：现状 eager 求值使 `WHERE x=1 AND 1/0=1`
  今天对每行报 division_by_zero——JumpIfFalse 短路会把"错误"变"空集"，
  属可见行为变化，**禁止**。Jump/JumpIfFalse/JumpIfNull 指令保留在
  指令集中（Case/InList 内部控制流用），但 AND/OR 不用它们；
- **NOT 的现状**：`Not(Null)` = internal error（非 Null）。复刻为
  Not 步遇 Null 输入返回同样错误；
- **语义修正（Kleene 化）是明确的未来变更**：单列"已知语义偏差清单"
  （SQL 标准三值逻辑 + NOT NULL=NULL + 短路求值），作为独立 PR 评审、
  重生成受影响 slt 期望，并从行为等价护航中显式豁免——本 spec 不做。

## 5. 后端二：eval_chunk（AP，v2c-1 接入）

- v1 实现：对 selection 内的行**循环调用 eval_row**，结果写入输出列
  ——先正确后向量化（分配不是瓶颈，见 ADR-4）；
- v2 优化：对**无跳转的直线段**（纯 map：Col/Arith/Cmp 序列）做列式
  求值（列寄存器批处理）；含 Jump 的子图保持行循环。识别在编译期
  完成（步列表切分为 `Segment::Map(..) | Segment::Control(..)`）。

## 6. 缓存与失效（详见 06）

- `ScalarProgram` 缓存键 = `(xxh3(sql), dialect, catalog_version)`；
- DDL（CREATE/DROP/ALTER/TRUNCATE）推进 catalog_version ⇒ 自动 miss；
- 有界：与 v2a 同池不同表，或独立 LRU（上限 1024）——06 定稿。

## 7. EXPLAIN 与文本表示

步列表的**规范文本形式 = 09 号 spec 的标量方言**（SSA 名 + 标签 +
版本头，round-trip/确定性/golden 测试三合同），EXPLAIN 输出其二段。
此处示例为简式（规范格式以 09 为准）：

```
expr:
  0: Col(1)            ; v
  1: Const(0)          ; 42
  2: Cmp(=)
  3: Out
```

## 7.5 附录 A：Expr → ScalarStep 全映射表（B1 前置交付物）

评审 P1-8 结论：现有表达式面远大于指令集的直观覆盖（Between 含
negated/NULL、InList 三值 found/has_null 且 found 即 break、Case 的
operand 形式与条件 NULL、IsTrue/IsFalse、TryCast/SafeCast、TypedString、
Substring FROM-FOR 语法、StringConcat、约 20 个 builtin 含变参）。
**B1 开工前必须交付附录 A**（独立文档或本文件扩充）：逐 Expr 形的
步序列、求值顺序、错误码、NULL 行为，与 expr.rs 行号互链；R1/R2
单测按此表逐行生成。此表同时是对拍基准——差分红了以它裁决。

## 8. 实现前必须回答

1. **LIKE 现状根本未实现**（评审 P1-8 更正：全库无 Expr::Like 处理，
   落在 not_supported 兜底）——它不是"语义搬运"而是**新功能**：Builtin
   表新增条目 + 新语义定义（转义/大小写规则需拍板）+ 新增测试，不进
   等价性叙事。Q3 作废。
2. `Builtin` 表与现有 builtin 函数注册机制（若有）如何对齐？避免双表。
3. Agg 是否纳入步列表？建议：**v1 不纳入**（聚合走算子层 AggCall，
   见 04），步列表只管标量表达式——缩小爆炸半径。
