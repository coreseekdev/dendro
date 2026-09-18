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

## 3. 全量路径瓶颈链（测量产出 + 一次重大修正）

1. ~~SQL parse 3.6K rows/s~~ → COPY（release **72.9K rows/s**，全量 ≈ 23 min ✓）
2. ~~CHECKPOINT 1.1K rows/s~~——**修正：初测为 debug 构建伪影**
   （294µs/节点的 Node::build 仅在无优化构建成立）。真正瓶颈是
   逐 chunk put_batch 的逐文件 fsync（release 12.8s/40K）；已修
   （Chunker 攒批 + put_batch 批末单次 syncfs，提交 2315236，
   A/B 12.84s → **1.44s** = 8.9×）。现 checkpoint ≈ 27.8K rows/s
   （全量外推 ~60 min ✓）。**教训入册：性能测量一律 release
   构建**（已补进设计评审 checklist 待办）。
3. 查询全量外推：release 40K 单查询 ~10-40ms 量级 → 全量线性
   外推 ~10-100s/条 × 39 × (warmup1+median3)——全量跑改用
   cold1+warm1 口径或分批。

## 3.5 下载器（运维记录）

自制 curl 分块下载器被服务端限速退化（单连接 ~2KB/s）后弃用；
改 **aria2c**（16 连接 -x16 -s16、无限重试、.aria2 控制文件断点
续传）：稳定 ~800KB/s（与服务端 32 连接实测封顶 718KB/s 一致），
ETA ~5.5h。教训：长传输用成熟工具，自制脚本只做启动验证。

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
