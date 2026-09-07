# 调研：多节点处理 TP 侧事务的架构方案

> 面向 Dendro 的现状（对象存储原生、分支模型、memtx OCC、组提交 WAL），
> 系统梳理"多个 node 处理 TP 事务"的架构选项、可参考的实现，以及推荐演进路线。
> 2026-09 · 调研性质文档，不含实现承诺。

---

## 0. 先把问题问对：三种"多节点 TP"是三个不同的问题

| 缩写 | 问题 | 难度 | 例子 |
|------|------|------|------|
| **R** | 读扩展：多节点分担读负载 | ★ | 只读副本 |
| **W1** | 写扩展：不同数据落到不同节点（分片/分区） | ★★★ | 分库分表、TiDB 的 Region |
| **W2** | 同一份数据多节点并发写（multi-master）| ★★★★★ | Aurora Multi-Master（已退役）|

外加 Dendro 特有的第四种：

| **B** | 分支并行：不同 Agent/租户各自分支上写 | ★（已实现）| git 分支、Agent 沙箱 |

绝大多数"我需要多节点 TP"的真实需求是 **R + W1 + B**，
W2 在业界几乎所有尝试里都被证明得不偿失（见 §2.E）。
**先确认你要的是哪一种，再选架构**——这是本次调研最重要的结论。

---

## 1. Dendro 现状盘点：手上的资产与约束

### 1.1 可复用的资产

| 资产 | 对多节点的价值 |
|------|----------------|
| **分支 = 一致性单元** | 写并行的天然分片键：分支之间零冲突，不需要 2PC |
| **manifest CAS（条件写）** | 天然的"单点串行化器"：一次条件写 = 一次提交投票 |
| **OCC 已实现**（memtx 读写集、first-committer-wins）| 分布式 OCC 只差"验证阶段的分布化" |
| **WAL = 不可变段流**（分支内单调编号）| 天然的复制日志；读副本 = 追段 |
| **列存段 = 派生数据**（col_segments）| 与 TP 分布正交：AP 层完全不感知节点数 |
| **存储根共享**（桶即数据库）| 任意新节点指向同一根即可恢复/读——读扩展已经成立 |

### 1.2 结构性约束（不可回避）

1. **OSS RTT（1–100ms）不适合当共识日志盘**。Raft/Paxos 每次提交要 1–2 轮
   多数派往返，OSS 延迟与抖动会让单分支提交吞吐塌到个位数 TPS。
   ⇒ "存储主体在 OSS" 与 "单分支内多节点强一致写" 存在结构性张力，
   工业界的一致解法是**插入第三层：低延迟日志服务**（见架构 A/C）。
2. **分支内单写者目前是进程内互斥**（commit_mu），跨节点需要
   租约/fencing（SPEC 02 §6 的 `fence/{branch}` + epoch，已设计未实现）。
3. **跨分支事务**是反模式：分支模型的意义就在冲突局部化；
   跨分支的"事务"应该表达为 merge（已实现），而不是 2PC。

---

## 2. 五类参考架构逐一拆解

### A. 共享存储 + 单写多读（disaggregated storage）

```
        ┌── 计算节点 1（writer: branch A,C）──┐
客户端 ─┤                                    ├──▶ 日志服务（3×NVMe 多数派，快速追加）
        └── 计算节点 2（writer: branch B,D）──┘         │
        ┌── 只读节点 N（tail 日志 + 缓存）──────────────┘
        ▼
   对象存储 OSS（版本化页/段、长期存储、备份）      ← 慢，但只承受批量顺序写
```

- **代表**：Amazon Aurora（SIGMOD'17）、Azure Socrates/Hyperscale（SIGMOD'20）、
  **Neon**（Rust 实现！safekeeper + pageserver，与 Dendro 同栈）、
  PolarDB（VLDB'21，PolarFS）
- 关键机制：**单写者**（writer 持有日志序列权）+ **日志即数据库**
  （只有 redo log 过低延迟日志服务；页/版本由后台从日志物化到共享存储）
  + 读节点追日志提供服务
- 对多节点 TP 的回答：**W1 靠"每节点负责一部分分支"，W2 不做**。
  单分支仍然是单写者，但节点可以承载任意多个分支，故障时分支迁移。
- 对 Dendro 的适配度：**最高**。OSS 保留"长期版本存储"身份（不破坏"桶即数据库"
  的最终语义），新增的日志服务只承担 WAL 的快速追加与复制。
  Neon 是 Rust 实现，safekeeper/日志协商的代码可直接研读。

### B. 共享无 + Raft 分组 + 分布式 2PC（shared-nothing NewSQL）

```
   节点1        节点2        节点3        节点4
  [Range A]   [Range B]    [Range C]   [Range D]     ← 每 Range = 一组 3 副本 Raft
  [Range C]   [Range A]    [Range D]   [Range B]

  事务跨 Range ⇒ 2PC（coordinator = 首个参与者）
  时间戳：TrueTime(Spanner) / HLC(CRDB) / 全局时间戳服务(OceanBase TSO)
```

- **代表**：Spanner（OSDI'12/CIDR'17，TrueTime）、CockroachDB（SIGMOD'20，
  并行 commit + 不确定性恢复）、TiDB+TiKV（VLDB'20，Percolator 血统的 per-key 锁）、
  YugabyteDB、OceanBase（SIGMOD'22，GTS 全局时间戳）
- 关键机制：数据按 Range 分片、每分片 Raft 多数派（**本地 NVMe 日志**）、
  跨分片事务走 2PC + 集中式或混合时间戳
- 对多节点 TP 的回答：W1（分片内单写，多分片并行）+ 有限 W2（跨分片冲突用锁）
- 对 Dendro 的适配度：**低**。Raft 日志要求本地低延迟盘，与"OSS 原生"定位冲突；
  若强行把 Raft 日志放 OSS，单分片吞吐 ≈ 1/RTT，不可用。且工程量是重写级。
  **可借鉴的是组件设计而非整体**：Range 划分、TSO、并行 commit 论文。

### C. 确定性数据库（deterministic）

```
  节点1 ──┐
  节点2 ──┼──▶ 全局日志（Sequencer 按读集/写集排序）──▶ 各节点按同一顺序确定性执行
  节点N ──┘
```

- **代表**：Calvin（SIGMOD'12，FaunaDB 的底座）
- 关键机制：事务执行前先全局定序，之后每个副本独立重放必然一致——
  无 2PC、无分布式锁、故障恢复是重放
- 代价：需要预知读写集（存储过程式事务）；交互式事务要乐观预执行+失败重试
- 对 Dendro 的适配度：中。OCC 写集已经是"预知写集"的形态；但读集依赖快照，
  Calvin 式排序要求把读集也先声明，交互式 SQL 体验差。**其"先定序后执行"
  的思想被 Aurora DSQL 继承（batch sequencer），可只学思想。**

### D. 分布式 OCC（乐观并发 + 集中式验证）—— 与 Dendro 血缘最近

```
  节点1 ──┐   ① 本地乐观执行（收集读写集）
  节点2 ──┼─▶ ② 提交：把读写集送到 Sequencer/Resolver
  节点N ──┘   ③ Resolver 验证冲突（OCC 验证的分布化）+ 授序
             ④ 写集落日志（低延迟层）──▶ 异步物化到 OSS
```

- **代表**：
  - **FoundationDB**（SIGMOD'21）：unbundled 设计——Sequencer、Proxies、
    **Resolvers**（专职 OCC 冲突检测）、Transaction Logs；存储层可插拔。
    "把事务系统当独立服务"的最佳教材。
  - **Aurora DSQL**（re:Invent 2024，SIGMOD'25 论文）：**OCC + batch
    sequencer/adjudicator + S3 存证**——与 Dendro 的形态惊人相似：
    事务本地乐观执行 → 批序器定序并裁决冲突 → 提交批次写 S3 →
    只读节点从 S3 回放；前端有热/冷块缓存吸收读流量。
    DSQL 证明了"对象存储当 TP 事实源 + 分布式 OCC"是成立的生产路线。
  - **Percolator**（OSDI'10）：直接在 Bigtable（共享存储）上做 2PC——
    write lock 就是表里的一个特殊记录。Dendro 的"OSS 上的 intent 对象"
    与之同构。
  - **Granola**（NSDI'24，Meta）：coordinator-commitment，降低多协调者
    事务的延迟放大。
- 对 Dendro 的适配度：**高（同分支多写者的正确答案）**。memtx 的 OCC
  读写集已经存在，缺的是：验证服务 + 全局授序 + 写集日志。

### E. 多主合并（multi-master + merge）

```
  节点1 ─┐
  节点2 ─┼── 各自独立接受写 ──▶ 异步交换 ──▶ 冲突解决（LWW/CRDT/应用层合并）
  节点N ─┘
```

- **代表**：git/Dolt remote（push/pull）、离线优先数据库（Automerge/Yjs，CRDT）、
  **ForkBase**（VLDB'18：分支/分叉友好的存储引擎，直接相关）、
  CouchDB/riak（LWW/CRDT）、Aurora Multi-Master（**商业上退役**——冲突率×
  性能损失不划算的公开教训）
- 对 Dendro 的适配度：**这就是 Dendro 已有的模型**！分支+merge 就是
  异步多主的冲突消解协议，只是冲突解决是"显式的、应用可见的"（行级冲突列表）
  而非 LWW 静默覆盖。对 Agent 场景这是 feature 不是 bug：
  冲突显式化 = 可审计。ForkBase 值得精读其存储层设计。

---

## 3. 与 Dendro 场景的匹配度分析

### 3.1 Agent 工作负载的真实形态

```
N 个 Agent ──▶ N 个沙箱分支 ──▶ 各自读写 ──▶ 验证后 merge 回主干
```

- 写并行度天然 = 分支数，**分支之间零冲突**——W1 的分片键是现成的
- 同一分支内的高频并发写（同一 Agent 热改同一批 key）在 TP 总负载中占比很小
- 读：主干数据被所有 Agent 读——R（读扩展）是刚需

⇒ 结论：**B（分支并行，已有）+ R（读副本，几乎免费）+ W1-按分支放置（租约）
覆盖绝大部分需求；同分支多写（D）是最后才需要的重武器。**

### 3.2 演进路线建议

```
P0（已完成）        读副本：任意节点挂同一存储根，追 WAL 段 + col_segments 提供读
                    └ 已成立的架构事实，补一个"副本 follower 进程"即可

P1（短期，~1-2 周） 分支放置 + 租约 fencing + 提交转发：
                    - 分支→节点 的放置表（进 manifest；一致性哈希或手工）
                    - 写者租约：fence/{branch} 条件写 + epoch 续租（SPEC 02 §6 落地）
                    - 非写者节点的写请求转发给当前写者（commit forwarding）
                    - 效果：任意节点可承载任意分支的写；分支故障秒级漂移
                    参考：Aurora 的 writer/reader、Neon 的 compute attach

P2（中期）          日志服务层（Socrates/Neon 形态）：
                    - 3 节点 openraft(Rust) 多数派，只存"WAL 帧流"（快速追加）
                    - 分支提交序列号由日志服务授序（替代"OSS CAS 当 sequencer"）
                    - OSS 退为：版本化物化结果 + 长期存储（异步追日志）
                    - 效果：单分支提交延迟从 OSS-RTT 降到 LAN-RTT；
                      分支迁移 = 日志重放，秒级
                    参考：Neon safekeeper（Rust）、Socrates 日志服务、FDB transaction log

P3（远期，仅当需要）同分支多写者 = 分布式 OCC：
                    - 节点本地乐观执行，提交时把读写集送 Resolver（FDB 式）
                      或批序裁决（DSQL 式）
                    - memtx 已产出读写集，验证逻辑即现有 OCC 验证的分布化
                    - 需要全局授序服务 + 写集日志（复用 P2 日志服务）
                    参考：FoundationDB resolver、Aurora DSQL、Calvin（思想）

不做：W2 多主静默合并（LWW/CRDT）——与分支显式冲突的哲学冲突，
     且 Aurora MM 的商业教训在前。跨分支事务不做 2PC；用 merge 表达。
```

### 3.3 每阶段的改动清单（对照现有代码）

| 阶段 | 改动 | 触及模块 |
|------|------|----------|
| P0 | follower 进程：只读打开（现有 recovery）+ 禁写 guard + 段位追新 | `engine.rs`（小）|
| P1 | `fence/{branch}` + epoch 租约；提交转发 RPC；放置表入 manifest | `objstore/`、`engine.rs`、新增 `cluster/`（中）|
| P2 | openraft 日志服务 crate；WAL writer 双写 OSS/日志；恢复改为"日志优先，OSS 兜底" | 新增 `wal-service/`（大）|
| P3 | Resolver 服务；提交协议从"本地 OCC"改"读写集上送" | `memtx/txn`、`engine/commit`（大）|

---

## 4. 参考实现与论文清单

### Rust 同栈（可直接读代码/复用组件）

| 项目 | 拿什么 |
|------|--------|
| [neon](https://github.com/neondatabase/neon) | safekeeper（日志多数派）、pageserver（从日志物化到 S3）、compute attach/lease——**与 Dendro 同语言同形态，第一参考** |
| [tikv](https://github.com/tikv/tikv) | multi-raft、Percolator 血统的事务键锁、并行提交 |
| [openraft](https://github.com/databendlabs/openraft) | 可嵌入的 Raft 库（Databend 系），P2 日志服务的共识底座 |
| [apache/opendal](https://github.com/apache/opendal) | 统一对象存储访问层（备选 object_store）|

### 论文/设计文档

| 系统 | 文献 | 对本题的核心启发 |
|------|------|------------------|
| Aurora | SIGMOD'17 "Design Considerations for High Throughput Cloud-Native DB" | 日志即数据库；单写多读 |
| Socrates/Hyperscale | SIGMOD'20 | 三层拆分：compute / log service / page servers |
| Aurora DSQL | re:Invent'24 / SIGMOD'25 | **OCC + batch sequencer + S3 存证**——对象存储当 TP 事实源的生产证明 |
| FoundationDB | SIGMOD'21 | unbundled 事务：sequencer/proxy/resolver/log 各司其职 |
| Calvin | SIGMOD'12 | 确定性定序免 2PC（思想）|
| Percolator | OSDI'10 | 共享存储上的锁记录/意图对象 |
| Spanner | OSDI'12/CIDR'17 | TrueTime；Range+Paxos+2PC 的完整范式 |
| CockroachDB | SIGMOD'20 | 并行 commit、不确定性恢复 |
| TiDB | VLDB'20 | HTAP：行存+列存副本（TiFlash ↔ Dendro 的 CBF 段同构）|
| OceanBase | SIGMOD'22 | 全局时间戳服务的工程形态 |
| ForkBase | VLDB'18 | 分支/分叉友好的存储引擎（与 Dendro 目标重合）|
| Granola | NSDI'24 | 多协调者事务的延迟优化 |

### 汇总对照表

| 架构 | W1 写扩展 | W2 同数据多写 | 跨节点延迟 | 对 Dendro 改动 | 代表 |
|------|:---:|:---:|:---:|:---:|------|
| A 共享存储单写 | 按分支/库 | ✗ | LAN（日志层）| 中 | Aurora/Neon/Socrates |
| B 共享无 Raft+2PC | ✓ 分片 | 部分（跨片用锁）| LAN | 重写级 | CRDB/TiKV/Spanner |
| C 确定性 | ✓ | ✓（定序换）| LAN | 大 | Calvin/Fauna |
| D 分布式 OCC | ✓ | ✓（冲突回滚）| LAN（日志）| 大但渐进 | FDB/DSQL |
| E 多主合并 | ✓ | ✓（异步冲突）| 无要求 | **已有（分支/merge）** | git/ForkBase/Dolt |

---

## 5. 结论

1. **R（读扩展）今天已经成立**：共享存储根 + 恢复器 = 任意多只读节点；
   补一个只读守卫与段位追新即可产品化。
2. **W1 的 Dendro 答案是"分支即分片"**：P1 的租约 fencing + 提交转发，
   工程量小且不破坏"桶即数据库"；这正贴合 Agent 场景（沙箱天然按分支切）。
3. **若需要突破"单分支单写者"的提交延迟上限**：引入 P2 日志服务层
   （3×NVMe 多数派，openraft），OSS 退居长期版本存储——这是 Neon/Socrates
   验证过的形态，也与 Aurora DSQL"OCC + 序列化 + S3 存证"的路线汇合。
4. **同分支多写（P3/D 类）只在明确需要时再做**：FDB resolver + DSQL 批序
   是最贴近现有 OCC 资产的参考；跨分支事务则永远用 merge 表达——这是
   Dendro 与传统分布式 TP 最大的差异点，也是最大的简洁性来源。
