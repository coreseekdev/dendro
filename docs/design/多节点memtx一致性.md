# 设计：多节点 memtx 事务一致性——现状边界与演进协议

> 回答两个问题：**① 当前实现里，memtx 的事务一致性到底由什么保证、保证到什么范围？**
> **② 多个 node 处理事务时，memtx 的一致性如何达成？**
> 代码：`engine.rs::commit_tx`、`memtx.rs`、`recovery.rs`、`wal.rs`。关联：
> [多节点 TP 调研](../research/多节点TP事务调研.md)、[WAL 格式](../tutorial/05-WAL日志格式与组提交.md)、[OCC 引擎](../tutorial/06-内存事务引擎OCC.md)。
> 状态：现状部分为**已实现事实**；P1/P2/P3 为**协议设计**（未实现处均如实标注）。

---

## 0. 结论速览（TL;DR）

| 问题 | 答案 |
|------|------|
| 单节点内 memtx 一致性？ | ✅ 已实现：OCC（first-committer-wins）+ `commit_mu` 全序化，分支内可串行化 |
| 跨节点读？ | ✅ 安全：对象不可变，任意节点挂同一存储根读（新鲜度=追段延迟）|
| **跨节点写同一分支？** | **❌ 当前不支持，且不安全**——两个进程写同一分支会损坏状态（§3 给出具体损坏路径）|
| 兜底纪律 | **一个分支同一时刻只允许一个写节点**（当前靠部署纪律保证；`fence`/epoch 机制已设计未实现）|
| 多节点的一致性路线 | P1 租约 fencing（单写者的分布化）→ P2 日志服务（memtx 变为可重放派生态）→ P3 分布式 OCC（读写集上送裁决）|

**关键认知**：memtx 从设计上就是**不可共享的易失状态**——它不是被"复制"到多节点，
而是每个节点**从同一条 WAL 全序独立重放**出来的派生态。多节点一致性问题的本质
因此转化为：**谁有权追加 WAL（写者唯一性）+ 追加的全序如何达成（授序）**。
这两点解决了，memtx 一致性自动成立。

---

## 1. 当前实现：单节点内的一致性模型（精确描述）

### 1.1 数据结构与角色

```
Session ──(写集缓存)──▶ Txn { snapshot, writes: BTreeMap<(table,key), Mutation> }
                              │ COMMIT
                              ▼
Branch（每分支一个运行态，进程内唯一）
  commit_mu: Mutex<()>        ← 分支内提交全序化的锁
  next_seq:  AtomicU64        ← 提交序号分配器（alloc_seq）
  watermark: AtomicU64        ← 可见性水位（读快照来源）
  pending:   Mutex<HashMap<table, BTreeMap<key, Mutation>>>   ← checkpoint 原料
memtx = BranchMem { 64 分片 HashMap<key, Arc<VersionVec>> }   ← 版本链（06 章）
```

### 1.2 提交协议与正确性论证（单节点）

`commit_tx` 五步（全部在 `commit_mu` 临界区内，除最后 ACK 等待）：

```
① seq = alloc_seq()                 锁内 → 分支内全序，无空洞
② 验证: ∀key∈写集: latest_ts(key) ≤ txn.snapshot
       不满足 → 40001（first-committer-wins）
③ 安装: 分片内追加 VerCell{ts=seq}   （读侧无锁，写侧短锁）
④ pending 登记 + WAL 帧编码入组提交缓冲
⑤ durable 后 watermark=seq；对客户端 ACK
```

成立所依赖的不变量（后续多节点设计必须逐一保住的版本）：

| # | 不变量 | 当前保证者 |
|---|--------|-----------|
| **I1** | 分支内 seq 严格全序、无重复 | `commit_mu`（进程内互斥）|
| **I2** | 读快照 = watermark，且 install 后立即对后续读可见 | 锁内 watermark.store |
| **I3** | OCC 验证与安装原子（验证通过到安装之间无人插队）| 同一把 `commit_mu` |
| **I4** | WAL 帧序 = seq 序；段号由唯一写者单调分配 | flush 线程 + 唯一进程 |
| **I5** | checkpoint 的 `covered_seq` 单调；恢复跳过 `seq ≤ covered` | manifest CAS |

对外可见的一致性等级：**分支内可串行化的写入 + 快照读**（验证覆盖写写冲突；
读写冲突依赖快照读，即快照隔离语义，见 SPEC 04 的诚实标注）。

### 1.3 持久性三线（ACK / 可见 / 权威）

```
时间 ─────────────────────────────────────────────────▶
  ③install      ⑤durable         checkpoint
────┬──────────────┬─────────────────┬──────────▶
    可见(新快照)    ACK 给客户端        权威转移给树
    （崩溃即消失）   （崩溃后重放恢复）    （恢复跳过 ≤covered_seq）
```

"已 ACK = 已在对象存储"（Group 档）是持久性承诺；install 与 durable 之间的
可见窗口只在本进程存活期内有效——崩溃后以 WAL 重放为准。这正是 memtx
可以"无锁读 + 乐观写"的根本原因：**它不是真相，是真相的缓存**。

---

## 2. 跨节点现状边界（必须诚实面对的部分）

### 2.1 今天安全的操作

| 操作 | 安全性 | 原因 |
|------|--------|------|
| 多节点**读**同一存储根 | ✅ | 对象不可变；读端只有本地缓存 |
| 多节点各自写**不同分支**（无 fence 时靠人工保证放置） | ✅* | 分支间状态天然隔离；`*` 指段对象路径按分支隔离，无交集 |
| 单节点写 + 多节点读 | ✅ | 读副本追 WAL 段（新鲜度=追段延迟）|

### 2.2 今天**不安全**的操作：两个节点写同一分支

具体损坏路径（不是理论风险，是代码事实）：

```
节点 A（会话 sa）            节点 B（会话 sb）           共享存储
├─ open: 各自 branch("main")
│   next_seq=10（各自恢复）  next_seq=10（相同！）
├─ sa: INSERT → seq=11 安装进 A 的 memtx
│      WAL 帧入 A 的缓冲     …
├─ sb: INSERT → seq=11 安装进 B 的 memtx      ← 各自独立分配，无感知
│      WAL 帧入 B 的缓冲
├─ flush: A PUT wal/main/…0011.wal (内容α)
├─ flush: B PUT wal/main/…0011.wal (内容β)    ← 同段号不同内容，PUT 直接覆盖
│                                             ⇒ 段内容互相吞没，重放结果随机
└─ 之后各自 checkpoint：
   A: manifest CAS → main.commit = 树A(只含 A 视角)
   B: manifest CAS → 重试后 main.commit = 树B(只含 B 视角)   ← A 的提交被静默丢弃
```

三个层面全部失守：**I1 破坏**（seq 双方各自分配）、**I4 破坏**（段号重叠、
PUT 覆盖）、**I5 失效**（后者的 checkpoint 静默抹掉前者）。这不是 bug，
是当前一致性模型**定义上就不含跨节点**——因此：

> **部署纪律（当前版本必须执行）：一个分支同一时刻只允许一个写节点。**
> 读节点无限多。违反纪律 = 数据损坏。

### 2.3 把纪律变成机制的路径（§3-§5）

分布化的推进不改变 memtx 的角色定义，只改变"谁有权充当唯一写者"和
"授序在哪里发生"：

```
P1  租约 fencing      →  写者唯一性由对象存储条件写保证（机制替代纪律）
P2  日志服务          →  授序由 3 副本多数派承担（延迟 + 可用性升级）
P3  分布式 OCC        →  多节点同时乐观执行，提交时集中裁决（真正的分布式写）
```

---

## 3. P1 —— 租约 fencing：把"单写者"从纪律变成机制

> **状态：✅ 已实现（2026-09-07）。** `objstore/fence.rs::FenceStore` +
> `engine.rs::branch`（打开分支即领取新 epoch）+ `recovery.rs`（按 epoch 升序
> 重放，复合时间戳 ts = epoch<<32 | seq，陈旧写抑制）。
> 实测：双实例接管 e2e 通过（A 写入 → 掉线 → B 接管 epoch+1 写入 →
> 第三实例恢复后两个世代的数据全部可见）；fence 并发竞争测试通过
> （同 epoch 竞争者只有一个 CAS 成功）。

### 3.1 协议

```
节点 X 想成为分支 b 的写者：
  ① put_if_absent(fence/b, {epoch: E+1, holder: X, ttl: 30s})    抢租约
     └ Exists → 读当前 epoch → 等租约过期后 E+2 重试（带随机退避防活锁）
  ② 持有者每 ttl/3 心跳：put(fence/b, epoch=E, 续期时间戳)
     （覆盖写——只有 epoch 持有者会写这个对象，写者唯一 ⇒ 幂等安全）
  ③ 每次提交：WAL 段对象名携带 epoch（seg 路径不变，帧头 FENCE 帧先于写）；
     checkpoint 的 manifest 更新携带 epoch —— 不匹配即被 CAS 拒绝
  ④ 旧持有者：任何一次写发现 epoch 不符 → 立即停止、丢弃 memtx（其已 ACK
     事务早已 durable 于旧 epoch 段，不丢失）
```

### 3.2 memtx 一致性在 P1 下如何保持

```
接管者 Y 赢得租约(epoch=E) 后：
  ① 读 manifest.refs[b] → (commit, wal_seg=F, covered_seq=C)
  ② HEAD 探测 WAL 尾部（05 章 §5）→ 回放 (F, tail] 中 seq > C 的 TXN
     → 在 Y 的 memtx 中按 seq 序重放 install（确定性：同输入同状态）
  ③ restore_seq(max_seq) → Y 的 memtx 从此刻起就是唯一权威 delta
旧持有者 X（若还活着）：
  任何写操作的 epoch 校验失败 → 事务返回 40001 → 客户端重路由到 Y
```

memtx 一致性 = **"单写者 + 全序重放"**：任一时刻只有一个 memtx 被写；
接管时新 memtx 由全序日志唯一确定。**不需要 memtx 同步协议**——
这正是把 memtx 设计成"可从 WAL 完全重建的派生态"的回报。

### 3.3 多实例接管 e2e（已实现，tests/multi_node.rs）

```
实例 A（E1）：建表 + 2 行 → drop（模拟崩溃）
实例 B（E2）：A 掉线后打开 → epoch 自动 +1 → A 数据可见 → 写 1 行 → drop
实例 C（E3）：打开 → 3 行全部可见（跨 epoch 恢复 ✓）
```

同时验证：TTL 过期是接管的前提（B 需等 A 的 600ms 租约过期后才接管）。

### 3.3 脑裂分析

| 场景 | 结果 |
|------|------|
| X 活着但网络分区，Y 抢到 epoch+1 | X 的写被 fence 校验拒绝；X 已 durable 的事务保留在旧段，Y 重放读入 |
| X 与 Y 同时认为自己持有（时钟/缓存） | 段路径与 manifest CAS 都携带 epoch —— 物化提交只有一个能赢 |
| X 在租约过期瞬间正好在写段 | 段对象不可变；Y 重放时以 epoch 大者的段为准（帧内 FENCE 帧带 epoch 可判）|

残余风险（如实标注）：X 在**失去租约后、感知前**写出的最后一个段与 Y 的
重放并存——已实现的消解规则：**epoch 升序重放 + 复合时间戳抑制**（低 epoch
的同 key 写入被高 epoch 已有版本抑制；恢复器 `replay_branch` 逐 key 检查）。
非冲突键的旧 epoch 已提交数据完整保留（它是真实提交，不是垃圾）。

### 3.4 提交转发（客户端无感的写路由）

非写者节点收到写事务 → 转发给当前租约持有者执行（memtx/OCC 全在写者侧）→
返回结果。读仍本地。这样客户端面对的仍是"一个逻辑分支"。

---

## 4. P2 —— 日志服务：memtx 变为"按序重放的派生态"

P1 之后单分支仍是单写者。若要**多个节点同时提交同一分支**：

```
        节点1 ─┐ ①提交请求(读写集/帧)
        节点2 ─┼─▶ 日志服务（3×NVMe, openraft 多数派）
        节点3 ─┘        │ ②全局授序 seq ←── 分支内仍全序
                        │ ③多数派持久 = durable
                        ▼
        所有节点按日志重放 ──▶ 各自 memtx 收敛到同一状态（确定性重放）
                        │
                        ▼ 异步物化（树 + 列存段 + manifest）──▶ OSS（长期权威）
```

memtx 一致性的定义升级为：**每个节点的 memtx 都是同一条全局日志的确定性
重放投影**。冲突裁决在授序点完成（提前于安装），节点本地不再需要
"验证-安装"的原子域——`commit_mu` 的职责被日志的全序取代。

- 延迟：ACK 在多数派持久（LAN 毫秒级），OSS 物化异步
- 冲突处理：授序点做 OCC 裁决（需要节点上送读集——见 P3）或
  先到先得 + 冲突回滚（写写冲突简单版）
- 故障：节点崩溃 = 日志重放；日志服务本身 = Raft 容错

## 5. P3 —— 分布式 OCC（读写集上送裁决）

在 P2 之上把裁决做完整（FDB resolver / DSQL 批序同族）：

```
提交请求 = {读集摘要, 写集, base_seq}
Resolver:
  对写集 key 检查"最近提交的读写集"是否与我的读集相交
    相交 → 40001 回滚（或等冲突者完成后再判）
    否则 → 记录本次读写集 → 授 seq → 写日志
```

- memtx 分片所有权两种变体：
  - **全副本重放**（简单）：每节点重放全部分支日志，查询本地读——适合分支小
  - **分片归属**（可扩展）：key range 按节点划分，节点只持有本分片 memtx；
    跨分片事务由 Resolver 协调（回到 2PC 域）
- 读集摘要：memtx 读路径需记录读键（当前 Txn 只有写集——P3 的主要改造点）

## 6. 一致性保证矩阵（现状 vs 演进后）

| 维度 | 现状 | P1 | P2 | P3 |
|------|------|-----|-----|-----|
| 分支内隔离级别 | 快照隔离+FFW | 同 | 同 | 同（读写集裁决补全）|
| 同分支写节点数 | 1（纪律）| 1（机制）| N（授序）| N（裁决）|
| memtx 权威 | 进程内 | 租约持有者 | 日志重放投影 | 分片归属/重放投影 |
| 分支故障恢复 | 进程重启重放 | 租约转移+重放 | 日志重放（秒级）| 同 P2 |
| 脑裂 | 靠部署纪律 | epoch+CAS 消解 | Raft 免脑裂 | 同 P2 |
| 跨分支事务 | merge | merge | merge | merge（不变）|

## 7. 与现有代码的差距清单（按优先级）

| # | 缺口 | 涉及模块 | 规模 |
|---|------|----------|------|
| 1 | `fence/{branch}` 租约对象 + 心跳/接管协议（帧头 epoch 字段已预留 FrameType::Fence）| `objstore/`、`wal.rs`、`engine.rs` | 中 |
| 2 | 提交转发 RPC（非写者 → 写者）| 新增 `cluster/` 或复用 wire 层 | 中 |
| 3 | 恢复器 epoch 消解规则（同段号不同 epoch 时取大）| `recovery.rs` | 小 |
| 4 | 只读打开模式（读副本守卫，禁写）| `engine.rs` | 小 |
| 5 | Txn 读集记录（P3 前置）| `memtx.rs::Txn` | 中 |
| 6 | 日志服务（openraft 3 副本）| 新 crate | 大 |

## 8. P1 实施状态

| 项 | 状态 | 代码 |
|----|------|------|
| FenceStore（epoch CAS 租约 + 并发竞争）| ✅ | `objstore/fence.rs` |
| WAL 段路径 epoch 化 | ✅ | `wal.rs::seg_path` |
| 复合时间戳（epoch<<32 \| seq）| ✅ | `recovery.rs::composite_ts` |
| 多 epoch 恢复回放（陈旧写抑制）| ✅ | `recovery.rs::replay_branch` |
| 分支打开即领租约（epoch 自动 +1）| ✅ | `engine.rs::branch` |
| 双实例接管 + 跨 epoch 恢复 e2e | ✅ | `tests/multi_node.rs` |
| 提交转发 RPC（非写者→写者）| ⬜ P2 | — |
| 心跳续期后台线程（当前依赖打开时新鲜度）| ⬜ P2 | — |

## 参考

同[多节点 TP 调研](../research/多节点TP事务调研.md) §4。与本文直接对应的机制：
FoundationDB 的 Resolver（P3 裁决）、Neon safekeeper（P2 日志多数派）、
Aurora DSQL 的 sequencer/adjudicator（P2+P3 合体形态）、
slatedb 的 fencing（P1 的对象存储实现）。
