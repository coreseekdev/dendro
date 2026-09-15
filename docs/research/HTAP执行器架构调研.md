# HTAP 执行器架构调研——Umbra/Limbo/DataFusion 全景分析

> 2026-09-15 · 参考 Neumann CIDR 2020 (Umbra) + VLDB 2022 (Tidy Tuples) + PVLDB 2022 (MVCC)
> + Limbo/Turso + DataFusion + Databend + Polars
> 论文全列表：https://umbra-db.com/#publications

## 1. Umbra 核心论文谱系（Neumann / TUM）

| 论文 | 年份 | 核心贡献 | dendro 可借鉴 |
|------|------|---------|-------------|
| HyPer (SIGMOD 2011) | 2011 | LLVM JIT 编译 + MVCC 快照隔离 = HTAP | 编译思路 |
| **Adaptive Execution** (ICDE 2018, Best Paper) | 2018 | 自适应编译：冷→解释，热→升级编译 | 计划缓存+升级策略 |
| **Umbra** (CIDR 2020) | 2020 | 变长页缓冲管理器 + 磁盘级内存性能 + 轻量 IR | 存储层分离 |
| **Tidy Tuples / Flying Start** (VLDB 2022) | 2022 | 统一编译+向量化执行；快速启动 | 执行器架构 |
| **Memory-Optimized MVCC for Disk-Based** (PVLDB 2022) | 2022 | 磁盘级 MVCC：版本信息内存驻留，缓冲管理器透明维护 | 版本链/水位管理 |
| **UmbraPerf** (PVLDB 2025) | 2025 | DBMS 开发者专用性能分析器 | 性能分析工具 |

## 2. Umbra 三大执行模型演进

### HyPer（2011）→ 整体编译
```
SQL → LLVM JIT → 机器码 → 执行
问题：每查询都编译，便宜查询编译时间 > 执行时间（最高 29×）
```

### Umbra v1（2020）→ 自适应编译 + 轻量 IR
```
SQL → 自定义轻量 IR → 字节码解释执行
                          │
                     进度追踪 → 代价高？ → 升级为 LLVM JIT
```
- 解决了 HyPer 的编译开销陷阱
- 但仍是**整体编译**（一个 pipeline = 一段代码），非模块化

### Umbra v2（VLDB 2022 "Tidy Tuples"）→ 模块化 + 统一
```
SQL → 轻量 IR → [Tidy Tuples 紧凑元组 + Flying Start 快速启动]
      │
      ├── 解释路径（冷查询）
      └── 编译路径（热查询，升级）
```
- **Tidy Tuples**：紧凑元组表示（非 Arrow 宽表），SIMD 友好
- **Flying Start**：新查询立即用解释器跑，后台异步编译，下次用编译版

## 3. 对 dendro 的可操作洞察

### 3.1 已有 = 正确的骨架

dendro 已有 HTAP 双引擎骨架：
- TP：prolly 树 + memtx overlay → PK 下推 6µs
- AP：CBF 列存 + zone map → 200k 聚合 75ms
- 路由：try_pk_pushdown → TP / try_ap_scan → AP

### 3.2 缺口 1：无预编译缓存（影响所有查询）

每次执行同一 SQL 重新解析（~2-3µs）。
**修法**：SQL hash → 解析后 AST 缓存（LRU），列索引预解析。

### 3.3 缺口 2：结果集全量物化（影响大结果集）

TableView 全行物化在内存后才返回。
**修法**：Arrow RecordBatch 分批产出（`embed_stream.rs` 草案已有）。

### 3.4 缺口 3：无向量化（影响 AP 吞吐 10-100×）

逐行 `eval(Expr, row, cols)` vs 向量化 `eval_batch(Expr, RecordBatch)`。
**修法**：Arrow compute kernels（`arrow::compute::kernels`）。

## 4. 参考来源

- Umbra: https://umbra-db.com/
- Umbra CIDR 2020: Neumann & Freitag
- Tidy Tuples VLDB 2022: Neumann et al.
- MVCC PVLDB 2022: Freitag et al.
- Limbo/Turso: https://github.com/tursodatabase/turso
- DataFusion: https://datafusion.apache.org/
- RisingLight: https://github.com/risinglightdb/risinglight
- Egg SQL 优化: https://rustmagazine.org/issue-2/write-a-sql-optimizer-using-egg
