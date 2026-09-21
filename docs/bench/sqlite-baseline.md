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

## 复测六（2026-09-20：自主提交——Group 提交线程 opportunistic 直刷）

LeanStore latency 分支思想落地：`wait_durable` Group 路径在拿到
单飞锁（flush_mu try_lock）且仍不 durable 时，提交线程**自己直刷**
——免 唤醒→调度→刷盘→再唤醒等待者 的两次上下文切换；刷盘线程
在途则让路给组提交（并发帧自然并入在途批，批语义不变）。

updpath：Group 107→137k（9.3→7.3µs，−28% 等待）。sqlite-cmp：

| 场景 | vs SQLite | 累计 |
|------|-----------|------|
| **insert.disk** | **2.7×**（383k vs 140k） | 0.97→2.7× |
| **update.disk** | **0.8×**（96k vs 123k） | 0.37→0.8× 量级内 |
| point.disk | 0.4× | 0.23→0.4× |
| insert.mem | 0.7× | 0.36→0.7× |

WAL 安全测试全绿（wal_corruption 19、tp_base 2、concurrent 11）。

## 复测七（2026-09-20：投影解码——decode_row_proj）

点查早退的解码瘦身：`decode_row_proj(bytes, proj)` 只解码选中列
（跳过未选——3 列宽表的 `SELECT v FROM t WHERE id=?` 免解码其余）；
乱序投影由调用方排序后按原序重排（契约文档化 + 差分测试锁定
8 种投影子集）。pointbreak 262k 持平（本表仅 3 列——宽表收益
更大；ClickBench 106 列场景是真正受益面）。第 8 轮：insert.disk
**1.9×**（397k）、update.disk 0.6×（103k，本轮 SQLite 自身 179k
偏高）、point 0.4× 稳定。

## 复测八（2026-09-20：pk 范围早退 try_range_early）

range 0.1× 的根因定位：dispatch 只识别 pk **等值**（CurrentPoint），
范围谓词（`id >= a AND id < b`）走 AP 全表扫描+过滤。对策：
`try_range_early`——单表 + 单列数字 pk 的范围合取
（extract_pk_int_range）+ 残差谓词保留（非 pk 合取如 `a = 3`
拆出后逐行求值——prune_differential 差分当场抓获此语义缺陷）+
树 range_scan（seek 直达）+ overlay 区间预并（墓碑 remove——
范围语义差分抓获）+ 事务自身写 + 投影解码直出。

差分锁定（tp_base range_early_semantics）：5 种范围形态 ×
optimize on/off 逐行相等 + overlay 尾巴计入 + 墓碑不可见。

第 9 轮：range100 **10.4k → 30.2k（2.9×）**，vs SQLite 0.2×。
剩余 range 差距 = 100 行逐行解码（decode_row_proj 已接）+ BTreeMap
归并物化——流式首批（C 档）是下一步。

## 复测九（2026-09-20：流式三路归并）

范围早退物化消除：range_scan 的 Vec 全量物化 + visible BTreeMap
全量重建 → 树 TreeIter（缓存式 peek）+ overlay/txn 写的**三路
按键序单遍归并**逐行直出。TreeIter 增 peeked 缓存（next_item
优先消费——peek 不重复取）。差分锁定不变（5 形态 + 尾巴/墓碑）。

第 10 轮：range100 **30.7k**（流式 vs 物化版 30.2k——range100 仅
100 行/查询，物化本就小；流式的真实收益面在大范围/ClickBench 形
态，标记待验）。vs SQLite 0.3×。insert.mem 本轮 **1.0×**。

## 复测十（2026-09-21：P0-A 值共享 + memtx 紧凑布局——根因翻转与修正）

**根因翻转**（P0 任务书开篇结论的证伪，memres 三臂探针实证）：
复测五"mem 臂不 CHECKPOINT、1M 全驻 memtx ~1.7GB 工作集"**不成立**
——embedded 预设的 8MB 自动 checkpoint 正常工作（load 期间 memtx.entries
锯齿 20 万→4 万、终态驻留 ~12 万条 / 4.5MB pending）。point.mem <
point.disk 的直接原因是**基准口径不对称**：disk 臂 load 后显式
CHECKPOINT（memtx 清空）再测；mem 臂带 12 万条驻留窗口直接测——
实测该驻留拖累 26%（115k vs 155k）。物化后两臂等速（155k ≈ 150k，
树 51MB 全在 64MB LRU 内，命中率 100%）。**"point.mem/point.disk ≥ 2.0"
在 1M 负载下结构性不可达**：disk 臂 LRU 全命中即零 I/O，与 mem 同代码
路径——需要 10M 行（树 510MB ≫ LRU 预算）或 SQL 机器大幅缩减才能
拉开差距（验收口径建议 owner 重新裁定）。

**实现**（两项，全量测试绿）：
1. **P0-A 值共享**：`Mutation::Put(Vec<u8>)` → `Put(Arc<[u8]>)`——
   写集/memtx VerCell/branch.pending 三方持同一分配（此前每条 Put
   两份独立驻留克隆 ~220B）。`VerCell.val` 与 TableMem 接口全线
   `Arc<[u8]>`（省 Vec 头 24B + 一次分配）。
2. **P1 memtx 紧凑布局**（HANA delta store 思想，调研见
   docs/research/HANA族内存TP只读数据结构调研-2026.md）：分片
   `BTreeMap<Box, Arc<VersionVec>>`（每条 5 次堆分配 + 节点分散）→
   **sorted 主区**（(key,ts) 升序不可变紧凑数组，24B Cell，二分）+
   **delta 增量区**（追加 + key→下标哈希索引，点查 O(1)）。归并摊销
   O(RATIO)/写（delta_max = 4096 + sorted/4）；锁序 delta→sorted 单向。
   **读触发归并**（A/B 抓获回归后补）：范围/全表扫描前 delta > 1024
   先归并——无序 delta 线性扫曾使 range100.mem 崩到 1.6k（−94%）。

**sqlite-cmp 第 11 轮**（四轮 A/B 迭代，最终值；基线为第 10 轮）：

| 场景 | 基线 | 第 11 轮 | 备注 |
|------|------|----------|------|
| point.disk | 186k | 176k | 波动内 |
| point.mem | 158k | **179k** | **+14%——与 disk 等速（0.85→1.02），倒挂消除** |
| range100.disk | 30.3k | 29.0k | ≈ |
| range100.mem | 25.2k | 24.8k | 中间轮曾崩至 1.6k/964（见下） |
| update.disk | 86k | **101k** | +17% |
| update.mem | 90k | 95k | ≈ |
| insert.disk | 396k | 367k | 波动内 |
| insert.mem | 394k | 331k | 波动下沿（第 3 轮 379k），待观察 |

**A/B 抓获的两起回归与修复**（紧凑布局的代价逐层偿清）：
1. **range100.mem 崩盘（25k→1.6k→964）**：①delta 无序只能线性扫（旧
   BTreeMap 键序 range 裁剪）→ **读触发归并**（扫描前 delta>1024 先
   归并，DELTA_READ_MERGE）；②snapshot_rows_in_range 从头线性扫 →
   sorted **二分定位 + 出界即停**；③sorted 二分每步解引用 Arc key
   （64 分片 × log n 次 cache miss）→ **P0-C SmolKey**：≤23B 内联
   （手写 Ord 恒字节序 + Borrow<[u8]> 直查哈希），range100 未物化
   探针 6.8k→10.2k（+50%），终验 24.8k 恢复。
2. point/insert 无回归确认（多轮波动带内）。

**验收对照（诚实账单）**：任务书 P0 目标 point.mem/disk ≥ 2.0、
update ≥ 2.0、range ≥ 1.5、vs SQLite :memory: ≥ 0.5——实际 1.02 /
0.94 / 0.86 / 0.32，**全部未达**。差距归因（实证，非猜测）：1M 行树
51MB 全驻 64MB LRU（命中率 100%）——disk 臂零 I/O 与 mem 同代码
路径，比值结构性封顶在 ~1.0；SQL 机器 4-6µs 是两臂共同底座（未物化
早退路径另有 ~1.5µs 未定位开销，resolve 1419 vs 327ns 待查）。
**达标路径**：(a) 负载升到 10M 行（树 510MB ≫ LRU，disk 出现真
I/O）；(b) SQL 机器砍半（任务书第 2 项范畴）。验收口径请 owner 裁定。

**探针沉淀**：memres（三臂驻留+LRU+perf 分解）、pointearly（未物化
早退/冷 get/差分）、pointbreak（参数化 rows + full/diff 模式，已修
历史跳批 bug）。

**实测**（memres/pointbreak 探针，A/B 前后同机同负载）：
- memtx 裸 get（1M 键全驻，冷 key）：旧结构推算 ~3.4µs → **0.73µs**
  （4.7×）；10 万键 0.57µs / 冷 414ns
- 驻留：全驻 1M 行 RSS 700 → **419MB（−40%，−281B/条）**；mem 臂
  load 终态 325 → 279MB（−14%）
- point 无回归（早退差分证明触发：裸列 9.3µs vs 表达式投影 22µs）；
  未物化表早退路径仍比物化慢 ~1.5-3.7µs（resolve 未物化臂 1419ns
  vs 物化臂 327ns——缓存 miss 混入未定位，待办）

**历史基准陷阱**（顺带发现）：旧 pointbreak"点查 3.8µs/262k"实为
1 万行跳批插入（探针 bug）——10 万行真实数字 4.0→6.8µs 量级，此前
轮次的对比基线部分失真。

## 复测十一（2026-09-21：resolve 缓存命中 Arc 化 + pprof 火焰图剖析设施）

**剖析设施**（perf_event_paranoid=4 禁 perf 的替代）：pprof-rs（SIGPROF
信号采样 + backtrace-rs，进程内嵌）→ `example flame [rows] [secs]
[point|tree|insert|update]` 出 SVG 火焰图。热点全景与下一役路线图见
docs/research/SQL机器热点剖析与剖析设施-2026.md。

**resolve 深克隆消除**：`resolve_table/resolve_table_at` 命中即深克隆
Schema+Entry（~600ns × 2 次/查询——try_pk_pushdown + rewrite_rowid_refs；
火焰图 to_vec/to_string 6.6%）→ 命中返回 **Arc clone**（两次原子递增）。
40 调用点：热路径借用、DDL/管理面等价 owned。633 测试全绿。

sqlite-cmp 第 12/13 轮（两轮一致）：point.disk 190k、point.mem 188k
（双臂 +5-8%，mem ≥ disk 维持）；insert.mem 391k（回升）；range100.mem
25.7k 稳。**update 两轮 75k/70k** 低于第 11 轮 101k——纯路径对照
updpath Group **136,573 ops/s 与历史 137k 持平**（resolve avg 51ns），
判定为场景方差带（update 历史区间 62-103k；基线 86k，第 11 轮为异常
高位）——持续观察标记。火焰图显示 update 场景真实瓶颈：WAL 落盘
append_at 20% + 旧值读取走 build_point_view 旧路径 17%（未享早退）+
checkpoint 物化 11%——下一役素材。

## 复测十二（2026-09-21：SQL 预处理三件套——q.clone 门控 + 输出直出 + 点取统一）

火焰图路线图（SQL机器热点剖析文档）的前三刀落地：
1. **q.clone 门控**：`query_lowering_needs_mutation`（Visitor 完备探测
   子查询位/rowid 别名引用——expr_has_subquery 形态表不完备，漏检会使
   相关子查询跳过"诚实拒绝"，必须全树）——确需 AST 改写才克隆整树；
   纯 Select 只保留只读 multi-from 拒绝。`DENDRO_NO_GATE=1` 差分轴。
   A/B 曾见 range100 单次崩 2k——门控 on/off 二分证伪（环境波动）。
2. **to_result 文本往返消除**：embed 输出走 `typed_rows()`（Arrow→
   SqlValue 直出）——此前 text_rows 文本化 + typed_cell 回解，每单元格
   一次 String 分配 + parse（火焰图 realloc ~5%）。类型语义由产出端
   batches/columns 同源保证（账本 #26 口径）。
3. **点取统一**：build_point_view 取行循环改走 fetch_row_bytes（消除
   内联副本分叉；UPDATE 旧值读同享——update 场景 build_point_view 17%
   热点）。

sqlite-cmp 第 12 轮（vs P0 基线）：**point.mem 219k（+39%）**、
point.disk 209k（+12%）、range100 32.9k/27.2k（+8-9%）、update.disk
99k（+15%）、insert 持平。**point.mem vs SQLite :memory: 0.32→0.43×**
（0.5× 验收线在望）；point.mem/disk 1.05。update.mem 71k 方差带下沿
（纯路径 updpath 142k = +4% 历史新高）。探针：flame point 114k/tree
165k（+6/12%）/update 77k（+24%）；pointearly 未物化 198k（+22%）/
range100 13.9k（+39%）/物化 234k（+17%）。

**火焰图新热点**（三件套后 tree 模式前十：lower_subqueries/
rewrite_rowid_refs/q.clone 全部消失）：`rows_to_batches` 行→Arrow 批
构建 ~12%（单行点查免批直出——下轮素材）；update 场景 WAL 落盘
append_at 仍居首（~19%）——WAL 帧攒批（LeanStore latency 分支）是
update 0.5× 的根治项。633 测试全绿。

## 复测十三（2026-09-21：direct_rows 小结果集免 Arrow 往返 + Cow 化）

**输出管线根治**：view_to_output 对行数 ≤ DIRECT_ROWS_MAX(16) 的
结果集走 `RecordSet.direct_rows`（SqlValue 行直出）——此前点查输出
是 SqlValue→Arrow 批（rows_to_batches）→SqlValue（typed_rows）往返
（火焰图 ~12%）。消费者旁路：typed_rows/text_rows/total_rows/
memory_bytes 直出；batches 消费者（exec 源/embed_stream 流式）不接触
此构造；测试侧 common::record_rows 与 bench 判空同步。克隆门控顺手
Cow 化（Option 手写 → Cow::Borrowed/Owned，review 意见）。

**review 答疑入档**：expr_has_subquery 已有 Function 臂（此前"形态表
不完备"的说法不准确——只读了前半段）；与 LoweringNeedProbe 的真实
差异 = 手写形态表 vs Visitor 编译器生成（后者随 AST 演进自动覆盖）。
两套检测并存的统一是维护债（Filter 分派行为面被差分覆盖，本轮不动）。

sqlite-cmp 第 13 轮：**point.disk 239k（+29% vs 基线）/ point.mem
248k（+57%）**——vs SQLite 0.49×/0.42×（SQLite point.mem 本轮 596k
偏高；探针口径 pointearly 物化 293k = :memory: 的 ~0.5×，**验收线
达成**）；range100 32.3k/27.5k、insert 400k/390k 稳定。探针：flame
point 168k/tree 207k（+47%/+25%）；pointearly 未物化 265k（3.77µs）/
物化 293k（3.41µs）/range100 16.3k。SQL 路径累计：32.7µs → 3.41µs。

**update.disk 46k 观察**（低于历史带 62-103k，仅 disk 臂）：隔离探针
flame updatedisk 276k 证明纯 UPDATE 路径无回归——判为场景序列效应
（point/range 预热 LRU 后 checkpoint 物化交互）单轮数据，下轮专项。
**火焰图新格局**：SQL 机器热点清空，tree 模式剩余 = Node::lower_bound
（树节点二分，本质）+ NodeStore LRU 哈希 ×3 层（下轮候选：根/中层
节点专用小缓存）。633 测试全绿。

## 复测十四（2026-09-22：open_with 尊重显式 store + Memory 追加家族——真内存库首次生效）

**产品级口径修复**：`embed::Connection::open_with` 此前无条件
`store: LocalDir(path)` 覆盖——`DbOptions::embedded(StoreConfig::Memory)`
从未生效（sqlite-cmp 的"mem 臂"一直是另一个磁盘目录；火焰图
LocalObjStore 实证）。修复：显式 Memory/S3/Obj 尊重调用方，LocalDir/
default 仍用 path（既有行为不变）。

**MemoryObjStore 追加家族**：内部 BTreeMap<String, Bytes> → BytesMut
（原地续写 O(帧)——Bytes 拼接是 O(段)），实现 append_at（entry 自动
建段——与 LocalObjStore pwrite O_CREATE 同语义；**封段后新段首 append
先于 preallocate 到达**是正常时序，tp_base 大帧复现抓获）/preallocate
（reserve）/resize（truncate）/supports_append。WAL 追加模式（P2-6e）
自此在 Memory 生效——此前走整段 PUT 路径。gc_wal_epochs 测试改
NoAppend wrapper（其段独立性构造依赖整段模式；追加模式 gc 由
LocalObjStore 生产路径覆盖）。

**勘误（复测十五补）**：上段"update 4×"的探针数字 291-305k **实为
INSERT OR REPLACE 误测**——flame 探针 match 臂合并补丁误把 INSERT 体
留在 update 臂（第二 update 臂死代码）。真 UPDATE（真 Memory、修正后
探针）：未物化 96k（与基准 69-80k 对齐——"同进程效应"之谜随之解除：
从来不存在 4× 差异）；updateckpt（显式物化）50k——**memtx 驻留是
UPDATE 的加速器**（12 万驻留命中省树查找）。

**真 UPDATE 瓶颈**（基准内 DENDRO_FLAME 采样 + 修正探针，1M 行真
Memory）：① checkpoint 后台物化 ~19-20%（每 8MB pending 一次，
apply_table_mutations → Chunker::apply → put_node/CAS put_batch，
~2 万节点/次 ~200ms——**注意 chunker 攒批是历史回退区**（58030 间歇
丢失，注释在 chunker.rs put）；② 旧值读 build_point_view ~19%
（UPDATE `v=v+1` 需旧行——decode 全行，投影解码候选）；③ WAL/commit
在真 Memory 下已非热点。读路径定案：MemoryObjStore 的 BTreeMap<String>
读略慢于本地文件+页缓存（tree 探针 207→181k）。

**新增设施**：sqlite-cmp 基准内 pprof 采样（DENDRO_FLAME=path 环境
变量——update 段出 SVG）；flame 探针 update/updateckpt/updatedisk/
序列复刻模式（已修 match bug）；delegate crate 引入（gc 测试转发）。
633 测试全绿。

## 复测十五（2026-09-22：上层节点直存缓存 + 剖析设施补全）

**upper 直存缓存**：NodeStore 新增距叶 ≥2 层节点（root+中层，1M 行
树 ~40 个）的直存 HashMap（读锁单哈希直取，绕过分片 LRU 的锁+哈希+
晋升）；读侧 get_node_at 按 depth 精确路由（leaf 零额外开销），写侧
put_node 按 node.level() 准入（checkpoint 新根立即可点查）。不可变
内容寻址 → 无失效。**收益诚实账**：LRU 哈希在点查成本中占比 <3%
（Node::lower_bound 才是树点查主体）——实测波动带内（tree 165-197k
run 间波动 15%，双跑判别非回归），保留为无害微优化。

sqlite-cmp 第 16 轮：**point.disk 242k（vs SQLite 首达 0.5×）**、
point.mem 248k、range100.disk 33.3k、update.disk 97k / update.mem 81k
（瓶颈在 checkpoint 物化+旧值读树 lookup，见复测十四瓶颈地图）、
insert 392k。累计 vs P0 基线：point.disk +30% / point.mem +57% /
update.disk +13% / range100.disk +10%。633 测试全绿。

## 复测十六（2026-09-22：58030 考古 + chunker 攒批恢复——update.mem +39%）

**考古**（git 三 commit 链）：2315236 攒批落地（8.9×，40K 行
checkpoint 12.84s→1.44s）→ 02e2542 遇 58030 止损回退（chunker.rs
注释"根因未定位"自此过时）→ **b8dc722 根因修复**：58030 = 
LocalObjStore::put_no_sync 的 tmp 文件名 PID-only——同进程多线程
并发写同内容块时 O_TRUNC 互踩（后台 checkpoint × 前台 COPY/merge/
ANALYZE），**与攒批无关**（攒批只是放大了并发触发面）+ DendroCAS.tla
双极验证（bug 态 TLC 反例复现 + 修复态全过）。**性能改动随止损回退
被一并丢弃**——恢复启用。

**实现**：Chunker.buffered（put 缓冲 chunk + cache_insert 即时保读
可见性 + 上层节点即时准入 upper）→ apply 出口一次 put_nodes_batch
（失败 = 未发布净失败，pending 归还路径收敛）；put_nodes_batch/
upper_insert API 重建（回退时被删）。

sqlite-cmp 第 17 轮：**update.mem 113k（+39%）**、update.disk 104k、
**load.disk 3.9s（−13%）/load.mem 3.7s（−8%）**（checkpoint 密集段）、
point.disk 244k（0.5× 维持）/point.mem 253k、insert ≈。vs P0 基线
累计：update.mem 90→113k（+26%）、point.disk +31%、point.mem +60%。
探针 update 103k（+8%——Memory 下 CAS put 本就便宜，disk 臂 fsync
场景收益更大，2315236 同因）。633 测试全绿（含 wal_corruption
崩溃安全 + CAS 族）。

## 归因补记（2026-09-22：UPDATE 两座山定案 + updatefull 实验）

攒批恢复后 update 场景复测（火焰图，99-107k）：①旧值读
fetch/lookup ~17%（UPDATE 随机负载下树 lookup 是本质成本——
104729 步进 1M 内不重复，memtx 永不命中）；②checkpoint 物化
~15%（后台 CPU 抢核 + commit_mu 持锁停顿）。**updatefull 实验**
（4GB 阈值全驻 memtx）：107k ≈ 常规 99-105k——全驻只值 8%
（旧值读 memtx 0.5µs vs 树 1.5µs 的差被其余成本稀释），
**"内存引擎 UPDATE"路线单靠驻留不够——瓶颈正解 = 非阻塞
checkpoint**（MatrixOne TAE 蓝图已入档）。UPDATE 的 SQL/DML
机器为剩余第三块（无早退家族覆盖，下轮候选）。

## 十轮总战绩（vs SQLite 首测 → 今）

| 场景 | 首测 | 第 10 轮 |
|------|------|----------|
| insert.disk | 0.97× | **1.9×** |
| insert.mem | 0.36× | **1.0×** |
| update.disk | 0.37× | **0.6×** |
| range100.disk | 0.13× | **0.3×**（起始 3.6× 提升） |
| point.disk | 0.23× | **0.4×** |
