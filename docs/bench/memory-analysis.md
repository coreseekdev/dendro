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

| 方案 | 做法 | 收益（1M / 10M） | 风险 |
|------|------|------------------|------|
| P0-1 窄行直推（C 档第一步） | 扫描直推批只含掩码列；Filter/AggOp/Project 按掩码索引映射（`MainPlusDeltaSource` 已是批迭代器——"方向 B"的自然延伸） | tv.rows 2.5GB→~50MB；q02/q09/投影类 3.9GB→~1GB / 10M 同降 10× | 中：消费端索引映射，差分测试护航 |
| P0-2 null 占位消除 | 与 P0-1 合并：批只含掩码列（schema 变体+列映射），不再造 null 数组 | 0.68GB→~2MB / ~7GB→~20MB | 随 P0-1 |
| P1 段级惰性解码 | `SegCursor` 按行组拉取（ap.scan 暴露行组迭代器），段驻留 O(全表)→O(活跃行组) | 50 段全驻留→逐段 / 10M 收益最大 | 中：columnar scan API 扩展 |
| P2 SELECT * 免中转 | 无 overlay 且段间键不重叠时输出 RecordSet 直接引用段批（三重→单重） | SELECT * 6.7GB→~3GB | 中：需证段不相交 |
| P3 已落地 | Arrow 全局聚合捷径（0.69GB）；ANALYZE 逐列（~40MB/列）；分段装载（2.5GB）；top-N 有界堆；管线输入惰性批（本批：消除宽行输入的整表预拷贝） | — | — |

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
