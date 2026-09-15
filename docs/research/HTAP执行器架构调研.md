# HTAP SQL 执行器架构调研——Umbra/Limbo/DataFusion 对照分析

> 2026-09-15 · 参考 Neumann CIDR 2020 (Umbra) + Limbo/Turso + DataFusion + Databend。
> 回答：dendro 的 SQL 执行器该选什么架构？

## 1. 三种架构核心机制对比

| 维度 | VDBE 字节码 (SQLite/Limbo) | 逻辑→物理计划 (DataFusion) | 自适应编译 (Umbra) |
|------|---------------------------|--------------------------|-------------------|
| 编译模型 | SQL→AST→字节码→VM 循环 | SQL→逻辑计划→物理计划→向量化执行 | SQL→轻量 IR→字节码→（热路径升级 JIT）|
| 预编译 | prepare = 编译一次反复执行 | 不支持（每次重新规划） | 自适应：冷查询解释、热查询升级 |
| 执行粒度 | 逐指令（VM dispatch） | 逐批（Arrow RecordBatch） | 逐步（pipeline→steps→morsels）|
| 取消 | 指令间检查 | 运算符间检查 | 步骤间检查 |
| 优化 | 无独立 pass（生成时内联） | 规则+成本双轨 | 自适应升级 |
| 并发模型 | 单线程 | 多线程+Arrow 并行 | morsel-driven |
| 适合 | 嵌入式/TP | 嵌入式/AP | 通用 HTAP |

## 2. Umbra 三大核心创新（Neumann CIDR 2020）

### 2.1 自适应编译策略
```
新查询 → 轻量 IR → 字节码解释执行
                        │
                   进度追踪 ──── 代价高？──→ 升级为 LLVM JIT 编译
                        │                       │
                   保持解释（低开销）         编译后执行（高性能）
```
- **避免 HyPer 陷阱**：HyPer 每查询都编译 → 便宜查询编译时间 > 执行时间 29×
- Umbra：冷查询解释、热查询升级 JIT——两全其美
- **对 dendro 的启示**：不需要一开始就 JIT。保持解释执行，热路径逐步升级

### 2.2 模块化状态机执行（非整体编译）
```
HyPer: 整个查询 = 一个 LLVM 代码片段（无法暂停/取消/调度）
Umbra: pipeline → steps → 每步一个函数 → 状态机转换

查询: SELECT count(*) FROM supplier GROUP BY s_nationkey
Pipeline 1: [Scan supplier] → [HashAgg] → (输出)
Pipeline 2: [Scan groups] → [Output]
```
- 每步可暂停/恢复/取消（语句取消的架构基础）
- 多线程步骤用 morsel-driven 并行
- **对 dendro 的启示**：执行器检查点（取消/超时）需要步进式执行，不是整体循环

### 2.3 自定义轻量 IR（不直接用 LLVM IR）
- 设计接近 LLVM IR 子集的自定义格式
- 需要 JIT 时线性翻译到 LLVM IR
- 避免不必要的 LLVM 功能开销
- **对 dendro 的启示**：v2 可设计轻量执行 IR，不需要完整 VDBE

## 3. 开源 Rust 实现参考

| 项目 | 架构核心 | Rust 生态 | 可参考组件 |
|------|---------|----------|-----------|
| **Limbo/Turso** | VDBE 字节码 + SQLite 兼容 | 纯 Rust，30k LOC | 指令集设计、cursor 管理、program 缓存 |
| **DataFusion** | 逻辑计划 → 物理计划 → Arrow 向量化 | Rust，Apache 项目 | 优化规则（谓词下推/投影裁剪/常量折叠）、execution plan trait |
| **Databend** | push-based + morsel 驱动 | Rust | pipeline 执行器、processor trait |
| **Polars** | streaming + Arrow 向量化 | Rust | lazy frame、SIMD filter/agg |
| **Daft** | Arrow + Rust 分布式 | Rust | 列式执行器 |
| **RisingLight** | Egg 优化器 + 向量化 | Rust | e-graph 优化原型 |

## 4. dendro 推荐架构（HTAP 双引擎自适应）

```
SQL 文本
  │
  ▼
sqlparser-rs 解析
  │
  ▼
AST → Plan Cache 查找 ──── 命中 ──→ 已编译计划
  │ miss                              │
  ▼                                   │
AST → 规则优化（常量折叠/谓词简化）      │
  │                                   │
  ▼                                   │
查询路由                               │
  ├─ PK 等值/IN ──→ TP 点查路径 ────→ prolly 树直查
  ├─ 小范围 ────→ TP 扫描路径 ────→ 树 range_scan + overlay
  └─ 大扫描/聚合 ──→ AP 列存路径 ──→ CBF + zone map + 向量化
  │
  ▼
执行（含取消/超时检查点）
```

### 4.1 TP 路径（已基本落地，补两件事）

| 缺口 | 修法 | 预期 |
|------|------|------|
| prepared 缓存 | SQL hash → 编译后 AST（消除重解析 2-3µs） | 点查 6→4µs |
| overlay 增量 | install 时增量更新可见集（非每次重建） | 扫描不白做 |

### 4.2 AP 路径（需 Arrow 向量化）

| 缺口 | 修法 | 参考 |
|------|------|------|
| 逐行解码→逐行聚合 | Arrow 列式批处理 filter/sum | Polars/DataFusion 向量化内核 |
| 无 SIMD | Arrow compute kernels（自动 SIMD） | arrow::compute |
| 全量物化中间结果 | push-based（上游推给下游，不整存） | DataFusion push-based exec |

### 4.3 自适应升级（v3，长期）

| 触发条件 | 动作 |
|---------|------|
| 同 SQL 指纹执行 >3 次且耗时 >10ms | 编译为特化代码路径 |
| 扫描行数 > 阈值 | 升级为向量化批处理 |
| JOIN 输入 > 阈值 | 升级为 Arrow 向量化 hash join |

## 5. 推荐路线图

| 阶段 | TP 路径 | AP 路径 | 优先级 |
|------|---------|---------|--------|
| **v2a** ✅ | 两段式提交/取消/超时 | CBF 物化/zone map | 已完成 |
| **v2b** 本轮 | 计划缓存 + overlay 增量 | — | 中 |
| **v2c** 后续 | — | Arrow 向量化 AP 扫描 | 高 |
| **v3** | — | DataFusion 集成（可选） | 低 |

## 6. 参考

- Umbra: Neumann & Freitag, CIDR 2020 — 自适应编译/模块化 pipeline/变长页
- Limbo/Turso: SQLite Rust 重写 — VDBE 字节码
- DataFusion: Apache Arrow 查询引擎 — 规则+成本优化
- Databend: Rust 数据仓库 — push-based morsel 驱动
- Polars: Rust 向量化 DataFrame — lazy evaluation + streaming
- DuckDB: 列式 + 行存储混合 HTAP（单进程嵌入式 HTAP 最佳先例）
