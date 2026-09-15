# 03 标量层：ScalarStep 步列表

> 同构参照：PG11 ExprState/EEOP_*（十年生产验证）。
> 红线：**叶语义零改动**——步列表只改变控制流与绑定方式，运算语义
> 一律调用现有 eval 函数。这是行为等价的根基。

## 1. 指令集（封闭；函数类走表）

```rust
pub enum ScalarStep {
    Const(u32),              // 常量池索引（含 NULL：Const(NULL_IDX)）
    Col(usize),              // 行/块内列偏移（绑定产物）
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

## 4. 三值逻辑的编译模式（NULL 短路的正确性关键）

AND/OR 不能朴素短路（`NULL AND FALSE = FALSE`，须看右侧）。
采用 PG 的 anynull 跟踪模式：

```
a AND b 编译为：
  eval a -> r0
  JumpIfFalse r0 -> L_false          ; a=false ⇒ 整体 false（跳过 b）
  eval b -> r1
  ...                                 ; 汇合点：结合 anynull 标志合成结果
```

合成规则（与现 eval 逐条对拍，进编译器单测）：
AND：见 false ⇒ false；否则见 null ⇒ null；否则 true。
OR 对偶。**禁止**引入"null 当 false"之类的近似——I-H1 红线。

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

## 8. 实现前必须回答

1. `Like` 模式当前语义（转义/大小写）在哪实现？步列表的预解析模式
   必须与其逐字节一致（编译期预解析属于"控制流"改动还是"语义"改动？
   建议：v1 保持 Like 现状（运行时匹配），预解析进 v2b.1）。
2. `Builtin` 表与现有 builtin 函数注册机制（若有）如何对齐？避免双表。
3. Agg 是否纳入步列表？建议：**v1 不纳入**（聚合走算子层 AggCall，
   见 04），步列表只管标量表达式——缩小爆炸半径。
