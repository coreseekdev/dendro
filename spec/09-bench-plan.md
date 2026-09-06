# SPEC 09 — 基准方法学（TP / AP / 版本操作 / 恢复 / OSS 延迟）

状态：**定稿 v1**

## 0. 环境记录义务

每次基准记录：CPU 型号/核数、内存、盘、OS、构建 profile(release/lto)、数据规模、
日期与 commit。结果不带环境 = 无效结果。

## 1. TP 微基准（内存事务引擎 + WAL）

仿 sysbench oltp_*，Rust 驱动（经 pgwire，协议级）：

| 用例 | 压力点 |
|------|--------|
| oltp_insert | 1 行/事务 × N 线程，吞吐+P99 |
| oltp_point_select | 主键点查 100% |
| oltp_read_write | 10 点查 + 2 更新/事务（OCC 冲突率记录）|
| branch_scale | 64 分支各写各的（分支级并行声明验证）|

自比基线：`--engine memory`（无 WAL 等待）隔离持久化成本；`durability=no_wait|group`。
外部参照口径：dolt 自报 MySQL 1.1X（README），doltgresql 自报 PG 6.3X 读/3.6X 写
（readonly.refer/README.md）——v1 不对齐真 MySQL/PG（环境无），报告写明"内部口径"。

## 2. AP 列式基准

- Q1 型聚合（lineitem 1M/10M 行：filter+group+agg+sort）
- 点查型 take（单行/百行 by pk）验证列存随机访问
- 冷/热：冷=清空块缓存+首次拉对象；热=页缓存命中
- `SELECT ... WHERE` 选择性 {0.01%,1%,10%,100%} × RG 剪枝开/关

## 3. 版本操作基准（git 语义成本曲线）

| 操作 | 曲线 x 轴 |
|------|-----------|
| CREATE BRANCH | 库大小 {1e6,1e7,1e8 行} → 应平坦 O(1) |
| commit(写 1k 行) | 历史长度 {100..10000 commits} → 应平坦 |
| MERGE 无冲突 | 分叉后双方写入量 → O(diff) 线性 |
| 结构共享率 | fork 后子分支改 x% 行 → 上传字节/x%（预期 <<1）|

## 4. 恢复与 OSS 延迟注入

- ThrottledObjStore 注入 RTT {0,1,10,50,200}ms（均值+抖动 20%，8 并发上限）
- 曲线：commit P50/P99（组提交吸收验证）、恢复时间 vs WAL 段数、
  AP 首查延迟 vs RTT（footer+首块 RTT 下限）
- 结论目标：group 模式下 P99 增幅 ≈ RTT 而非 2×RTT（流水线证据）

## 5. 只读数据结构实验（读优化结构落地验证）

- prolly 点查 vs PGM 索引点查（1e7 键）：比较次数与纳秒
- 块级 zone map 剪枝命中率（TPC-H Q6 型谓词）
- SuRF 风格前缀过滤器（实验，v2）：存在性剪枝的假阳性率 vs 内存

## 6. harness 形态

`dendro bench <suite> [--out results.json]`；JSON 结果 + Python 绘图脚本
`benches/plot.py`（无第三方依赖强制，matplotlib 可用时出图，否则文本表）。
BASELINE.md 汇总每次跑分，防性能衰退回归（需求 #6 的另一层含义）。

## 7. 与实现的映射

- `crates/dendro-server/src/bench/`（各 suite）
- `benches/results/`（结果归档，git 提交）
