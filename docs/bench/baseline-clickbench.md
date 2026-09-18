# ClickBench 基线（1M 已完成，2026-09-18 深夜更新）

> 数据：官方 hits.csv.gz 全量 15.5GB（1 亿行）已下载。
> 本页入档 **1M 完整基线**（39/43 查询 + 4 跳过）+ 内存治理效果。

## 1. 环境与口径

- 20 核 / 91GB RAM；LocalDir 存储（NVMe）；release 构建
- harness：`dendro-server click-bench`（分段 COPY + 段间 CHECKPOINT →
  全量 CHECKPOINT → ANALYZE 逐列 → 43 查询 warmup1+median3）
- 适配清单（诚实）：+合成主键 rid；DATE/TIMESTAMP→TEXT；lossy UTF-8；
  跳过 4 查询（extract/REGEXP_REPLACE×2/DATE_TRUNC）；GROUP BY 1→显式列；
  GROUP BY 别名（q41 CASE AS Src 已修）；LIKE 已实现（含 %lit% 快路径）；
  SUM/AVG i128 累加（UserID 近 i64 上限溢出已修）

## 2. 1M 完整基线（2026-09-18 深夜，含全修复链）

| 指标 | 值 |
|------|-----|
| **装载** | 314s（3,184 rows/s）——分段 5×200K 行 COPY+CHECKPOINT |
| **末次 CHECKPOINT** | ~0s（分段已增量物化） |
| **ANALYZE** | 202s（逐列 106 次 SELECT，~40MB/列峰值） |
| **查询中位数** | 1,430ms |
| **查询总和** | 74s（39 查询 × warmup+3 计时） |
| **RSS 装载期** | **~2.5GB**（分段装载生效——原 12GB） |
| **RSS 查询期** | ~7.7GB（查询物化 TableView 1M×106 列中间行） |

### 逐查询（median of 3，1M 行）

| 查询 | median_ms | 行数 | 说明 |
|------|-----------|------|------|
| q01 COUNT(*) | 1224 | 1 | 全扫描 |
| q02 COUNT WHERE | 953 | 1 | |
| q03 SUM+COUNT+AVG | 1384 | 1 | |
| q04 AVG(UserID) | 1240 | 1 | |
| q05 COUNT(DISTINCT UserID) | 1426 | 1 | |
| q06 COUNT(DISTINCT SearchPhrase) | 1385 | 1 | |
| q07 MIN/MAX(EventDate) | 1500 | 1 | |
| q08 GROUP BY + ORDER | 947 | 5 | |
| q09 GROUP BY DISTINCT + TOP 10 | 1565 | 10 | |
| q10 GROUP BY 多聚合 TOP 10 | 1698 | 10 | |
| q11 GROUP BY + WHERE | 960 | 10 | |
| q12-18 GROUP BY 各种形态 | 980-1761 | 10 | |
| q20 COUNT LIKE '%google%' | 925 | 0 | LIKE 已实现 |
| q21 MIN+COUNT LIKE | 967 | 1 | |
| q22 多条件 LIKE | 1833 | 1 | |
| q23 LIKE+NOT LIKE | 2542 | 10 | |
| q24 SELECT * LIKE + ORDER | 4629 | 10 | 最慢——全列扫描+排序 |
| q25-27 ORDER BY + LIMIT | 1100-1320 | 10 | |
| q30 90 个 SUM | 6868 | 1 | 最重——90 列算术 |
| q31-36 GROUP BY + JOIN 形态 | 1243-3010 | 10 | |
| q37-39 CounterID=62 日期范围 | 1437-2709 | 10 | |
| q41-42 窄范围 | 1351-1428 | 0-10 | |

### Arrow 原生全局聚合（P0，2026-09-18 追加）

`SELECT agg(...) FROM t`（无 WHERE/GROUP BY/JOIN/ORDER BY/LIMIT）直接在
列存 Arrow 列上计算（i128 累加 SUM/AVG、类型化 downcast MIN/MAX），免
rows_from_batches 行式转换（1M×106 列 ≈ 1.06 亿 SqlValue 中间对象）。
列掩码只读引用列（sparse get_range），COUNT(*) 只读 pk 列。

| 查询 | 行式 median_ms | Arrow median_ms | 加速 |
|------|---------------|-----------------|------|
| q01 COUNT(*) | 1132 | 53 | **21.4×** |
| q03 SUM+COUNT+AVG | 1332 | 83 | **16.0×** |
| q04 AVG(UserID) | 1176 | 62 | **19.0×** |
| q07 MIN/MAX(EventTime) | 1480 | 91 | **16.3×** |

非覆盖形态（COUNT(DISTINCT)、SUM(col+expr)、带 WHERE）差分安全回落
行式。对照注意：本对照跑整机慢 ~15%（load 314→396s 可证非代码回归），
上表加速比在两个运行内各自成立。

差分安全门（`try_arrow_global_agg`，7 项集成测试锁定）：

- DISTINCT 修饰 → 回落（**缺陷修复**：捷径曾忽略 DISTINCT 语义算成
  plain count——1M 样本上 UserID 全唯一、SearchPhrase 非空值恰全唯一，
  数值巧合掩盖；有重复值的判别用例已入 `arrow_agg_differential.rs`）
- COUNT(*) 通配符 → count_star（曾因 Wildcard 参数形态误判回落）
- memtable overlay / 显式事务写 / col_deletes 非空 → 回落三路归并
  （未物化增量丢行/多数防线，`has_visible_rows` O(1) 首键早退探测）
- `SET dendro.optimize = off` 在 release 也生效（原 debug-only cfg 使
  release 下差分对照失效——A/B 旋钮必须在产物二进制可复现）

## 3. 内存治理效果（核心成果）

| 阶段 | 原（单 COPY+单 CHECKPOINT+SELECT * ANALYZE） | 现（分段+逐列） |
|------|---------------------------------------------|-----------------|
| 装载 | 12GB RSS（pending 全量积压 + CHECKPOINT 全量 decode） | **2.5GB** |
| ANALYZE | +12GB（SELECT * 全表物化） | **~40MB/列**（逐列） |
| 查询 | — | 7.7GB（TableView 1M×106 列中间行——待惰性化） |
| **全量重建** | 每 8 段触发 write_full → 18.8GB（10M 实证） | COMPACT=512 消除 |

## 4. 已修复的 ClickBench 发现的缺陷

| 缺陷 | 修复 |
|------|------|
| COPY 不支持引号内嵌换行 | in_open_quote 引号感知拼接 |
| COPY 单事务超 256MB → 54000 | 分批 commit（max/2） |
| GROUP BY 不支持投影别名 | build_select 别名→表达式替换 |
| LIKE 未实现 | expr 层 + %lit%→contains 快路径 |
| SUM/AVG i64 溢出 | i128 累加双路径 |
| CHECKPOINT 逐 chunk fsync → 12.8s/40K | Chunker 攒批 + syncfs（8.9×） |
| ANALYZE SELECT * → 12GB | 逐列查询（~300× 内存降） |
| 后台 ckpt × COPY 并发 → 58030 | 装载期关后台 + tmp 唯一化 |
| CAS tmp PID-only 碰撞 → 58030 | unique_tmp + rename 幂等 |

## 5. 全量 100M 计划

- 数据 15.5GB 已下载；磁盘 ~180GB 可用（估算 120GB 数据+WAL → 够）
- 管线就绪：分段 COPY（~1.5h）→ ANALYZE → 39 查询 cold1+warm1
- 瓶颈：查询物化 7.7GB（1M） → 100M 时 TableView 不可能全物化——
  需惰性游标 v2 全覆盖（当前仅单表 SELECT 形态）或流式执行 C 档
- **建议**：全量 100M 跑在查询侧需要 C 档就位，或按 10M/50M 分级跑

## 6. 信源

- 官方 hits.csv.gz：datasets.clickhouse.com（15.5GB，1 亿行）
- 1M 样本 = gz 流前缀（引号感知逻辑记录提取）
- dendro 修复链：6ebb06b（dry-run 三修）→ 02e2542（内存治理）→
  b8dc722（58030 根因）→ 本提交（COMPACT 512）

### 内存防护（2026-09-19 追加：10M 运行 OOM 事故后）

10M 查询阶段 TableView 物化（1M 已 7.7GB → 10M 线性外推 ~77GB）曾把
swap 打满殃及整机。此后跑批双层防护：

1. **进程内软上限** `--max-rss-mb N`（click-bench 参数）：装载逐段/
   查询逐条**之间**检查 RSS，超限把剩余步骤标 `skipped[memcap]` 写出
   **已完成部分**的 JSON 后优雅退出；每查询完成打点耗时+RSS 到日志
   （即使被硬杀也可从 tee 日志恢复部分基线）
2. **外层硬看门狗** `memguard.sh <pid> <maxGB>`：5s 轮询，超限
   SIGKILL 进程组——兜住单条查询**内部**的失控（软上限步间才检查）

现行口径：10M 跑批 = `--max-rss-mb 16384` + memguard 24GB。
查询侧根治（10M+ 不全物化 TableView）仍需 C 档流式执行。
