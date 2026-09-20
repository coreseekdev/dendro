# dendro × SQLite TP 对比基线（2026-09-19 首测）

> 目标（owner 设定）：TP 磁盘路径不弱于 SQLite；MemTx 远超其
> `:memory:`。首测结论：**均未达成**——差距定位与追赶路径如下。

## 口径

- SQLite：`prepare_cached` + 参数绑定（其最佳实践）；磁盘臂
  WAL + synchronous=NORMAL；mem 臂 `:memory:`
- dendro：SQL 文本（形状缓存自动 prepare——我们的等价机制）；
  磁盘臂 = embedded 预设 + CHECKPOINT 后树驻留；mem 臂 = memtx
  驻留（无 checkpoint）
- 1M 行 (id BIGINT PK, v BIGINT, tag TEXT)；点查/范围100/随机
  UPDATE/INSERT OR REPLACE；median of 5×1s（`dendro sqlite-cmp`）

## 实测（ops/s）

| 场景 | sqlite.disk | dendro.disk | 比 | sqlite.mem | dendro.mem | 比 |
|------|------------|-------------|----|-----------|------------|----|
| point | 473,445 | 107,890 | **0.23** | 500,675 | 114,869 | **0.23** |
| range100 | 138,638 | 18,423 | 0.13 | 128,818 | 17,663 | 0.14 |
| update | 150,802 | 56,045 | 0.37 | 258,591 | 61,682 | 0.24 |
| insert | 202,141 | 196,431 | **0.97** | 453,679 | 164,027 | 0.36 |
| load | 0.4s | 5.2s | — | 0.3s | 4.7s | — |

## 判定（诚实）

- 磁盘"不弱于"：**未达**（insert 0.97 持平；point 0.23 / range
  0.13 / update 0.37 落后）
- MemTx"远超"：**未达**（全面落后 4-5×）
- SQLite 的 2.1µs/点查（含 rusqlite 绑定开销）是其数十年打磨的
  prepared 路径——单行小值负载正是其最优场景

## 差距解剖（perf 框架归因）

dendro 点查 mem 臂 ~8.7µs：parse 已消（形状缓存）；剩余 =
AST 深克隆 + substitute（~2µs，prepare/形状缓存共用路径）+
resolve 2× + eval/exec 机器 ~5µs。SQLite 同场景 = 绑定 + B-tree
步进 + 列取，无任何 AST/文本层。

**结构性差距**：我们的每次执行都经 SQL 文本→模板→AST 克隆→
代入→执行；SQLite prepared 是 绑定→原生执行。**追赶的正确
路径不是继续微调文本路径，而是 prepared 快路径的克隆消除**。

## 追赶路径（按 ROI）

1. **P0 prepared 免克隆**：`exec_prepared` 的 AST 深克隆改为
   原位代入 + 执行后还原（参数槽复用）或绑定环境（执行器直接
   读参数槽，AST 不动）——预计 mem 点查 8.7→~3µs（>SQLite
   disk 473k 的量级追赶）。形状缓存同享此收益。
2. P1 range100：范围扫描的 MainPlusDeltaSource 启动成本
   （TableView 物化）——流式返回首批（C 档前置）。
3. P2 update/insert：WAL 组提交批量化（当前每条一个 txn 帧）。
4. load：分段批量已并入 P2。

## 复现

```bash
dendro sqlite-cmp --data /tmp/dendro-sqlitecmp --rows 1000000 \
  --out benches/results/sqlite_cmp.json
```
