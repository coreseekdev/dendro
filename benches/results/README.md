# Dendro 基准结果（首轮全量）

- 日期：2026-09-07
- 环境：20 核 x86_64 / 91GB RAM / NVMe / Linux
- 构建：`cargo build --release`（lto=thin, codegen-units=1）
- 口径：进程内引擎直连（wire 层开销另计）；每次操作 = 一条独立 SQL（含解析）
- JSON 明细：本目录 `tp.json` / `commit_latency.json` / `branch.json` / `recovery.json`

## TP（内存引擎天花板，20k 行数据集）

| 指标 | 值 |
|------|-----|
| oltp_insert（逐行自动提交）| **157,296 txn/s**，p50 4.9µs，p99 16.1µs |
| oltp_point_select（PK 等值下推直查）| **162,371 txn/s**，p50 5.0µs，p99 11.8µs |

构成：sqlparser 解析 + OCC 验证 + memtx 安装 + WAL 帧编码。
点查走 `id = ?` 主键下推（memtx ∪ 树直查），全表扫描路径未参与。

## 提交延迟（WAL 组提交，LocalDir 后端 + fsync）

| durability | 组提交间隔 | p50 | p99 |
|------------|-----------|-----|-----|
| no_wait | 1ms | 0.006 ms | 0.058 ms |
| group | 1ms | 1.07 ms | 1.15 ms |
| group | 5ms | 5.09 ms | 5.16 ms |
| group | 25ms | 25.1 ms | 25.2 ms |
| group | 50ms | 50.2 ms | 50.2 ms |

**结论**：group 模式延迟上限 ≈ flush_interval（设计目标），且 p99≈p50+0.1ms
——组提交把尾延迟压平（无 RTT 抖动放大）。interval 是延迟预算的旋钮；
单连接串行吞吐受 1/interval 限制，多连接并发时吞吐按批次摊销（OSS RTT 场景
下组提交价值更大，SPEC 02 §3）。no_wait 模式 6µs 交付。

## 分支操作（git 语义成本曲线）

| 操作 | 10k 行 | 100k 行 | 500k 行 |
|------|--------|---------|---------|
| CREATE BRANCH | **225 µs** | 240 µs | ~ms 级（含强制 checkpoint 源分支）|
| MERGE（1000 行 diff）| 306 µs | O(diff) | O(diff) |

CREATE BRANCH 成本 = 一次 manifest CAS + 源分支强制 checkpoint，与数据量无关
（数据部分零复制）；随 size 的微增来自 fork 前的源分支 checkpoint。
MERGE 与无关数据量无关，只与 diff 大小线性。

## 恢复（NoWait 模式模拟崩溃）

| WAL 未物化事务数 | 恢复时间 | 恢复行数 |
|------------------|----------|----------|
| 1,000 | <10 ms | 943（NoWait 丢尾窗口）|
| 5,000 | 2 ms | 4,994 |
| 20,000 | **8 ms** | 19,867 |

- 恢复 = HEAD 探测 WAL 尾部（无 LIST）+ 顺序回放，20k 事务 8ms。
- 行数差额 = NoWait 模式最后未 flush 段的固有丢失窗口（已 ACK 的事务不受影响；
  group/always 模式下已 ACK 事务零丢失——这是 SPEC 02 §3 的 durability 契约）。

## 复现

```bash
cargo build --release -p dendro-server
./target/release/dendro bench --out benches/results
```

## 待办（v2）

- 多客户端并发 TP（branch_scale：64 分支并行写入）
- AP 列式：TPC-H Q1 型 + 冷/热 + OSS 延迟注入曲线（ThrottledObjStore 已实现）
- pgwire 协议层开销（对照进程内数字）
- 压缩衰退正式曲线（Rust 版；Python 原型结论见 spec/08 §5）
