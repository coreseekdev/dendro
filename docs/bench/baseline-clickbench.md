# ClickBench 基线（进行中，2026-09-18）

> 数据：官方 hits.csv.gz 全量 15.5GB（1 亿行）后台分块下载中。
> 本页先入档 **100K 真实数据端到端验证** + 全链瓶颈测量——全量
> 数字待下载完成后补（装载 ~1.5h + 检查点见 §3 瓶颈）。

## 1. 环境与口径

- 20 核 / 91GB RAM；LocalDir 存储；Durability=Group（默认）
- harness：`dendro-server click-bench`（装载 COPY → CHECKPOINT →
  ANALYZE → 43 查询 warmup1+中位数 of 3）
- 适配（诚实清单）：+合成主键 rid（dendro 表必须 PK）；DATE/TIMESTAMP
  → TEXT（ISO 序 = 时间序）；lossy UTF-8（官方数据文本列含二进制
  垃圾）；跳过 4 查询（extract/REGEXP_REPLACE×2/DATE_TRUNC）；
  GROUP BY 1 → 显式列
- 样本 = 官方 gz 流前缀（前 53,280 行——时间序数据的窄窗口，
  聚合类分布有代表性、日期范围类（q37-43）不代表）

## 2. 100K 验证数字（绝对值仅作全链验证，非基线）

| 阶段 | 值 |
|------|-----|
| 装载（INSERT 路径，改 COPY 前） | 3,567 rows/s（parse-bound） |
| 装载（COPY FROM） | **18,291 rows/s（5.1×）** |
| CHECKPOINT | ~1.1K rows/s（见 §3） |
| ANALYZE | 53K 行 4.2s |
| 查询（39 条） | 250–440ms @ 53K 行（全表线性扫描量级） |

## 3. 全量路径瓶颈链（本次测量产出）

1. ~~SQL parse 3.6K rows/s~~ → COPY 18.3K rows/s（全量装载 ≈ 91 min ✓）
2. **CHECKPOINT ~1.1K rows/s（宽行线性）——新关键路径**：
   10K 行 9.4s / 40K 行 34.6s（3.7×，线性）；外推 1 亿行单次检查点
   ≈ **25 小时**。全量基线被此阻塞——checkpoint 物化路径（prolly
   提交 + CBF 段写 + memtx 截断）需专项 profile（下一工作项，
   疑点：逐行 encode/tree 点插/每段 codec 试编码）。
3. 查询全量外推：线性扫描 ~500s/条 × 39 × 4 runs（需减轮次或分批）。

## 4. 全量执行计划（下载完成后）

zcat 全量 → 前置行号（~36GB 平 csv）→ COPY（~91min）→ CHECKPOINT
（**阻塞：先修 §3-2**）→ ANALYZE → 43 查询（cold 1 + warm 1 口径）
→ 数字补入本页。

## 5. 伴生产出（本轮代码）

- LIKE 实现（expr 层 + 双指针回溯匹配器 + 8 单测）——补 SQL 面缺口
- SUM/AVG i64 溢出修复（i128 累加双路径——ClickBench UserID 近
  i64 上限，两个相加即溢；100K 首跑即爆）
- COPY FROM CSV（产品功能 + 装载官方路径；.gz 自动解压为基准便利
  扩展）；引号感知 CSV 解析进 core
