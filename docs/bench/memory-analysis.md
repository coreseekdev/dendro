# 查询内存剖析与降低方案（1M ClickBench 实测，2026-09-19）

背景：10M 运行查询阶段 TableView 物化外推 ~77GB 把 8GB swap 打满殃及整机。
本文用独立进程探针（`crates/dendro-core/examples/memprobe.rs`，VmHWM 取
真实峰值）对 1M×106 列 hits 逐形态实测，并插桩定位结构归属。

## 1. 实测（VmHWM 峰值 / 输出行数 / 耗时）

| 形态 | 峰值 | 输出 | 耗时 |
|------|------|------|------|
| SELECT *（全宽全量） | 6.72GB | 1,000,000 行 | 8.8s |
| SELECT 单列投影 | 3.89GB | 1,000,000 行 | 2.4s |
| COUNT + WHERE（q02 类） | 3.89GB | **1 行** | 1.6s |
| GROUP BY + LIMIT 10（q09 类） | 3.90GB | **10 行** | 1.9s |
| SELECT * + LIKE + ORDER + LIMIT（q24 类） | 5.00GB | 10 行 | 4.7s |
| Arrow 全局聚合捷径 COUNT(*)（q01 类） | **0.69GB** | 1 行 | 0.19s |

输出 1 行的查询吃 3.9GB——峰值与输出规模无关，由**中间表示**决定。

## 2. 归因（插桩：`arrow_cols=106, arrow_bytes=0.68GB, tv.rows width=106, mask_cols=2`）

非捷径查询 ~3.9GB 峰值的三座山：

1. **`tv.rows` 全宽行物化 ≈ 2.5GB**（最大项）：扫描输出 TableView 把
   1M 行 × 106 列全部收集为 `Vec<Vec<SqlValue>>`。列掩码（O-3）只省
   **解码 IO/CPU**，行结构仍全宽——被裁列以 `SqlValue::Null` 槽位占
   24B/列/行（1M×104×24B ≈ 2.5GB 纯 Null 槽）。掩码 2 列的查询与
   SELECT * 同宽。
2. **null 占位 Arrow 批 ≈ 0.68GB**：O-3 裁剪列在批内做 null 数组保持
   批宽 = schema 宽（"消费端零映射"的设计代价）——空 Utf8 数组仍要
   (n+1)×4B offsets 缓冲，70 个文本列 × 5 段 × 200K 行 ≈ 0.7GB。
3. **段批全量驻留**：`segment_batches` 一次性解码全部段（1M=5 段尚可；
   10M=50 段线性放大至 ~28GB）。真实列数据在掩码生效时很小（~20MB），
   驻留大头是第 2 项的 null 占位。

SELECT * 额外两项：全宽行含真实字符串 ≈ +2.6GB；输出时
`rows_to_batches_typed` 把行**重转换回 Arrow**（三重表示：段 Arrow →
SqlValue 行 → 输出 Arrow）≈ +1GB 瞬时。

排序 + LIMIT 已是有界堆（top-N，`Limit{Sort}` 执行期重建为 n=limit+offset），
q24 的峰值来自过滤前的扫描全量物化，不在排序。

## 3. 降低方案（按收益排序；均为**减工**而非加工，性能不降反升）

### P0 已落地（2026-09-19：活跃列投影——窄行 + 窄批）

`MainPlusDeltaSource` 增活跃列集（源自 O-3 列掩码）：段批转换与 overlay
解码只投影活跃列（键槽重映射），`tv.names` 收窄——下游 FactorLayout/
Project/Filter 按**名字**解析自适应；`CbfColumnar::scan` 掩码列不再造
null 占位数组（批 schema = 活跃字段窄变体）。

| 形态 | P0 前 | P0 后 | 内存 | 速度 |
|------|-------|-------|------|------|
| SELECT 单列投影 | 3.89GB / 2.4s | 0.19GB / 0.40s | **20×** | **6.0×** |
| COUNT + WHERE（q02 类） | 3.89GB / 1.6s | 0.13GB / 0.23s | **30×** | **7.0×** |
| GROUP BY + LIMIT（q09 类） | 3.90GB / 1.9s | 0.14GB / 0.40s | **28×** | **4.8×** |
| Arrow 捷径 COUNT(*) | 0.69GB / 0.19s | 0.02GB / 0.01s | **35×** | **19×** |
| SELECT *（全宽合理） | 6.72GB / 8.8s | 6.89GB / 8.4s | — | — |

速度提升来自不再解码/分配被裁列（104/106 列是死重）；"性能不受影响"
的约束被超越。10M 外推：q02/q09 类 ≈ 1.3GB——查询套件可完整运行
（SELECT \* 类 10M ≈ 50GB 仍需软上限优雅截断或 P2）。

落地中修复的连带缺陷：`estimate_filter_rows_ex` 曾用窄 `tv.names` 对
全宽 `stats.cols` 定位（列错位 → est=0）；sparse_read 合同翻新为
窄批（裁剪列缺席而非 null）。

### 后续（未落地）

| 方案 | 做法 | 收益 | 风险 |
|------|------|------|------|
| P1 段级惰性解码 | `SegCursor` 按行组拉取（ap.scan 暴露行组迭代器），段驻留 O(全表)→O(活跃行组) | SELECT \* 10M 50GB→~段流 | 中 |
| P2 SELECT * 免中转 | 无 overlay 且段间键不重叠时输出 RecordSet 直接引用段批（三重→单重） | SELECT \* 6.9GB→~3GB | 中 |
| P3 已落地 | Arrow 全局聚合捷径；ANALYZE 逐列；分段装载；top-N 有界堆；管线输入惰性批 | — | — |

结论：q02/q09 类"小输出大扫描"查询的根治是 **P0-1+P0-2+P1**（合起来即
流式执行 C 档的扫描侧），完成后峰值 ≈ O(活跃行组 × 掩码列宽)，与输出
和表规模解耦。SELECT * 全量输出在 v1 wire 语义下仍需物化，P2 可再省一半。

## 4. 运维防线（已上线）

- `--max-rss-mb`（click-bench）：装载逐段/查询逐条**间**检查，超限标
  `skipped[memcap]` 写出已完成部分后优雅退出
- `memguard.sh <pid> <maxGB>`：5s 轮询，超限 SIGKILL 进程组（兜单条
  查询内部失控）
- 跑批脚本自挂双层（现行：软 16GB / 硬 24GB）

## 5. 复现

```bash
cargo build --release --example memprobe -p dendro-core
target/release/examples/memprobe <0-8> [force]   # 每查询独立进程
```
