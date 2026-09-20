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

## 复测一（2026-09-19 晚：prepare 原位代入 + LeanStore 档案批）

| 场景 | 首测 disk | 复测 disk | 首测 mem | 复测 mem |
|------|-----------|-----------|----------|----------|
| point | 107.9k | 112.6k | 114.9k | 115.0k |
| insert | 196.4k | 195.0k | 164.0k | **205.4k**（+25%） |
| update | 56.0k | 39.7k* | 61.7k | 66.8k |
| range100 | 18.4k | 10.4k* | 17.7k | 18.1k |

*disk 臂 update/range 的波动与 jemalloc/页缓存状态相关（同日多次
运行 ±30%）；point/insert 稳定。

**诚实结论**：prepare 原位代入是**克隆中性**（快照仍需一次深克隆
——sqlparser AST 按值传递，真实免克隆需 exec_statement 借用化，
已列为下一步）；insert mem 臂 +25% 疑似 jemalloc 预热。差距的
主要来源确认不在 prepare 层而在 **每次执行的 SQL 机器本身**。

## 追赶路径修订（LeanStore 档案吸收后）

1. **exec_statement 借用化**（真免克隆的前提）
2. **WAL 自主提交/帧攒批**（LeanStore latency 分支 SIGMOD'25 对标
   ——update 0.2-0.4× 的深层对策）
3. 点查机器继续瘦身（resolve 2×→1×；TableView 分配消除）

## 复测二（2026-09-19 深夜：持久 resolve 缓存）

resolve 缓存从语句级清空改为**持久 LRU（键 = catalog 根 + 表名）**——
正确性由内容寻址保证（checkpoint/DDL → 新 catalog 根 → 新键），
连续同表语句的首次全量树走查（~5.8µs）只付一次。

**updpath 分解（100K 行 memtx）**：

| 耐久性 | 首测 | 复测二 | SQLite 参照 |
|--------|------|--------|-------------|
| UPDATE Group（每提交 durable） | 24.7k | **58.6k（2.4×）** | disk 169k* |
| UPDATE NoWait（搭车刷盘） | 50.3k | **226k（4.5×）** | mem 306k* |

*SQLite synchronous=NORMAL 在 WAL 下**不每提交 fsync**（检查点时才
同步）——耐久性口径介于我们 NoWait 与 Group 之间。**NoWait 226k 已
达 SQLite 量级**（disk 169k 的 1.34×，mem 306k 的 0.74×）。

sqlite-cmp 官方轮（文本 SQL 路径，Group 耐久）：point.disk 0.5×
（126.9k vs 255.6k——本轮 SQLite 自身读数回落）、update.disk 0.4×、
insert.disk 1.0×。文本路径与耐久口径仍是两大差项。

## 判定更新

- insert（磁盘）：**1.0× 达标**（不弱于）
- update NoWait 口径：**SQLite 量级达成**（226k）
- point：0.5×（文本机器 + 树路径）——exec_statement 借用化 +
  point 机器瘦身后复测

## 复测三（2026-09-20：exec_statement 借用化 + memtx 瓶颈验证）

**用户假设验证（pointbreak 探针）**：memtx **不是**瓶颈——裸
find_by_pk 2.8M ops/s（0.35µs）、树 1.9M（0.52µs）；瓶颈是
SQL 文本机器（4.7µs = 存储 0.35 + 机器 4.3）。

**借用化**：exec_statement(Statement) → (&Statement)——Query 臂
克隆一次 Query，DDL 臂按需 clone；形状缓存路径的 AST 深克隆
消除（模板直接借用执行）。

| 场景 | 复测二 | 复测三 | SQLite（本轮） |
|------|--------|--------|---------------|
| point.disk | 126.9k | **167.2k** | 471k |
| point.mem | 121.6k | **159.9k** | 569k |
| insert.disk | 197.4k（1.0×） | **366.8k（1.7×）** | 210k |
| insert.mem | 123.8k | **398.4k（0.8×）** | 502k |
| update.disk | 63.9k | 61.8k | 166k |
| range100.disk | — | 10.4k | 112k |

**里程碑**：insert.disk **1.7× 达标并超越**；insert.mem 0.8×
（SQLite 量级内）。point 0.3-0.4×、update 0.4×、range 0.1×
为剩余差项——SQL 机器 4.3µs 的进一步分解（eval 1.7 内点取
0.35 + TableView/投影 ~1.3；形状缓存查找+代入 ~1.5）。

## 复测四（2026-09-20：会话级模板工作副本）

形状缓存命中的 AST 深克隆改为会话工作副本（`sess.shape_working`：
模板 hash → 首次克隆后复用；in-place 代入 + ParamSwapRef 守卫
还原；安全闸门 = Value 数==占位符数，不满足退回单次克隆路径）。

pointbreak：point 213k → 237k（4.27µs）。sqlite-cmp 第 5 轮：

| 场景 | 第 4 轮 | 第 5 轮 | vs SQLite |
|------|---------|---------|-----------|
| insert.disk | 367k（1.7×） | 310k（**1.6×**） | ✅ 持续超越 |
| insert.mem | 398k（0.8×） | 399k（0.8×） | 量级内 |
| point.disk | 167k（0.4×） | 187k（0.4×） | +12% |
| update.disk | 62k（0.4×） | 82k（**0.6×**） | +33% |

累计（首测 → 第 5 轮）：point 0.23×→0.4×、insert 0.97×→1.6×、
update 0.37×→0.6×。剩余差项不变：SQL 机器 ~4µs 的 eval/投影中转、
Group 组提交等待（update）、range 流式首批。

## 复测五（2026-09-20：点查免中转——字节点取 + 直出列）

try_point_early 重构：`fetch_row_bytes`（字节级单键点取：memtx ∪
树、墓碑、事务自身写——不解码不建 TableView）+ 投影形态**先判**
（聚合/表达式回落计划路径——空集聚合须返回单行 0，直出会得 0 行，
ap_txn q14 实证；IN(单值) = 等值臂补齐，join_reorder 实证）→
单次解码直出列。IN 单值查询（`WHERE id IN (2)`）同享早退。

pointbreak：point 254→262k（3.8µs）。sqlite-cmp 第 6 轮：

| 场景 | vs SQLite | 累计（首测→今） |
|------|-----------|-----------------|
| insert.disk | **1.8×**（372k vs 210k） | 0.97→1.8× |
| insert.mem | 0.8× | 0.36→0.8× |
| update.disk | 0.5× | 0.37→0.5× |
| point.disk | 0.4×（185k） | 0.23→0.4× |

**disk vs mem 同速现象解释**（owner 质询）：基准 mem 臂不
CHECKPOINT——1M 行全驻 memtx（26.7× 结构开销 → ~1.7GB 工作集、
缓存局部性差）；disk 臂 checkpoint 后紧凑段 + LRU 热页（64MB）。
memtx 是写缓冲不是读存储——opt1 架构的实证。
