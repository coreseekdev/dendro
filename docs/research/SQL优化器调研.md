# SQL 优化器调研——Rust 生态现有方案与 dendro 路线

> 2026-09 · 对照 Limbo/Turso、Apache DataFusion、Egg e-graph 三个项目，
> 评估 dendro 的 SQL 优化路线。

## 1. 三大参照系

| 项目 | 架构 | 优化策略 | 语言 | 规模 |
|------|------|---------|------|------|
| **Limbo/Turso** | VDBE 字节码 VM（SQLite 同构） | 无独立优化 pass——字节码生成时内联 | Rust | ~30k LOC |
| **DataFusion** | 逻辑计划 → 规则优化 → 物理计划 → 执行 | 规则+成本双轨（谓词下推/常量折叠/连接重排序） | Rust | ~200k LOC |
| **Egg (RisingLight)** | AST → e-graph 等价饱和 → 提取最优计划 | 规则重写 + 成本函数提取 | Rust <1k LOC | 实验级 |

## 2. 关键洞察

### Limbo/Turso（VDBE 字节码）
- SQL 编译为字节码指令序列（Op::Init → Op::Rewind → Op::Column → Op::Next → Op::ResultRow → Op::Halt）
- 预编译语句 = 编译后的 Program 对象，可反复执行
- 指令级取消/进度回调天然支持（每条指令间检查）
- **代价**：需要完整的字节码编译器 + 寄存器分配 + 游标管理（数千行）

### DataFusion（规则+成本）
- 两阶段：逻辑优化（常量折叠/谓词下推/投影裁剪/简化）→ 物理优化（join 选择/排序）
- 可嵌入：`datafusion-optimizer` crate 独立发布
- **代价**：需要逻辑计划 IR（dendro 当前直接走 AST，无逻辑计划层）

### Egg（等价饱和）
- e-graph 紧凑表示所有等价形式，提取最低成本计划
- 规则：谓词下推/连接交换/连接结合/常量折叠/HashJoin 转换
- **代价**：多表 TPC-H join 有组合爆炸风险；需分阶段规则
- **优势**：<1000 行即可实现原型；RisingLight 已验证可行

## 3. dendro 路线决策

**当前约束**：
- 已有直接 AST 解释执行器（`sql/expr.rs eval` + `sql/scan.rs`）
- 302 测试全绿，SQL 面（SELECT/JOIN/聚合/子查询）已可用
- SQL 重解析是性能瓶颈（每次 ~2-3µs），但**数据链路**优化（本项目重点）
  已在大表点查/范围扫描/组提交上取得 2-780× 改善

**决策：三步走**

| 阶段 | 内容 | 理由 | 状态 |
|------|------|------|------|
| v1（本轮） | AST 级规则优化（常量折叠/谓词简化/死代码消除）+ embed API | 最小代价最大收益；不改执行架构 | ✅ |
| v2 | prepared statement 缓存（SQL → AST 编译一次反复执行） | 消除重解析 2-3µs/次 | ⬜ |
| v3 | 字节码 VM 或逻辑计划 IR | 完整优化器 | ⬜ 长期 |

**v1 本轮实现的规则**：

| 规则 | 示例 | 效果 |
|------|------|------|
| 常量折叠 | `1 + 2` → `3`；`'a' \|\| 'b'` → `'ab'` | 消除运行时计算 |
| 布尔简化 | `TRUE AND x` → `x`；`FALSE OR x` → `x` | 消除无效分支 |
| 比较简化 | `1 < 2` → `TRUE`；`NULL = NULL` → `NULL` | 消除运行时比较 |
| NOT 消除 | `NOT (a > b)` → `a <= b` | 消除取反开销 |
| 算术恒等 | `x + 0` → `x`；`x * 1` → `x`；`x * 0` → `0` | 消除无效运算 |
| 谓词传递 | `a = b AND b = 5` → `a = 5 AND b = 5 AND a = b` | 谓词传递闭包 |

## 4. SQLite Embed API 参照

| SQLite C API | dendro embed API | 说明 |
|-------------|-----------------|------|
| sqlite3_open | Connection::open / memory | 连接生命周期 |
| sqlite3_exec | Connection::execute | DDL/DML |
| sqlite3_prepare_v2 | Connection::prepare | SQL → Statement |
| sqlite3_bind_* | Statement::bind(&[Value]) | 参数绑定 |
| sqlite3_step | Statement::step | 逐行产出 |
| sqlite3_column_* | Row::get_i64/get_str/... | 类型化列访问 |
| sqlite3_finalize | Statement::finalize | 释放 |
| sqlite3_close | Connection::drop | 关闭 |

