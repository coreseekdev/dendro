# TiDB X Shared SST 机制调研（对照 dendro）

- 日期：2026-09-15
- 触发：siddontang（PingCAP 联合创始人）"Inside TiDB X" 系列推文
- 结论先行：**TiDB X 的 shared SST 与 dendro 的存储货币（内容寻址不可变对象 + manifest 发布）机制同构**——dendro 天生就是 shared-SST 形状，且在内容去重维度走得更远。真正的差异在**复制货币与 HA 形态**，不在存储。近期可落地借鉴只有 3 个小 delta（只读计算挂载、CAS 增量备份承诺化、compaction 出进程 seam），多写者 HA 是远期设计记录。

## 1. 机制来源

推文原文（Inside TiDB X 系列，2026-09）：

> **Shared SSTs**：Three replicas, one shared set of immutable SSTs.
> Raft replicates **logs and file-version changes**; each replica keeps its own state and caches.
> Benefit: less duplicated SST storage than a design with a separate SST set per replica.

> **Write**：Client → Raft commit → MemTable Apply → **ACK**.
> SST creation, upload and **file publication run on a separate background path** — writes don't wait for their SSTs to reach object storage.

官方架构文档（docs.pingcap.com/tidbcloud/tidb-x-architecture）补充：

| 维度 | TiDB X 做法 |
|------|------------|
| 持久层 | 对象存储（S3）= 唯一事实源；本地盘只承载 Raft log（ack 点）与缓存 |
| 写路径 | Raft log 先落**本地盘**（前台绝不碰 S3）→ memtable apply → ack；SST 上传/发布全后台 |
| 复制货币 | Raft 复制的是 **日志 + 文件版本变更**，不是数据文件——三副本共享同一份不可变 SST |
| 读路径 | 轻请求走本地缓存/盘；重查询下推远端弹性 coprocessor |
| compaction | 独立弹性服务（"compute 与 compute 分离"）；完成后**通知节点加载新 SST**——还是文件版本发布 |
| 存储引擎 | LSM forest：每 Region 独立 LSM 树，消除 RocksDB 单树全局 mutex（>6TiB / 30 万 SST 后即瓶颈）|
| 扩容 | 新节点 attach 对象存储、按需加载，**不拷数据**（比物理拷 SST 快 5-10×）|
| 备份 | 元数据驱动：增量 Raft 日志 + S3 元数据，秒级、与数据量无关 |

## 2. dendro 对照：天生同构的部分

dendro 的存储设计在机制层几乎逐条对应，不需要"引入"shared SST——它已经是：

| TiDB X 机制 | dendro 对应物 | 备注 |
|------------|--------------|------|
| 一份共享不可变 SST | 内容寻址 chunk / CBF / prolly node（CAS）| **dendro 更进一步**：TiDB X 只在"同一数据的副本间"共享；CAS 把共享做到内容级——跨分支、跨版本结构去重（git 语义），这是 TiDB X 没有的维度 |
| file-version changes（发布货币）| manifest 版本化目录（CAS 提交）| 同构：数据文件不可变，"变更"只是元数据指针替换 |
| Raft log → ack；SST 上传/发布后台化 | 两段式提交：Pass1 WAL enqueue（持久点）→ Pass2 install+watermark（发布）| 同构。gap-free watermark = 发布前沿；NoWait/Group 模式下"写不等 OSS"由 durability 契约显式声明（TiDB X 用本地盘隐藏延迟，dendro 用组提交摊销——同一谱系两端）|
| memtable flush → SST 上传 | checkpoint 线程：memtx → 不可变 chunk + manifest swap | 已是独立后台线程 |
| per-replica state and caches | memtx（每分支 BTreeMap）+ NodeStore 16-shard LRU + cached.rs 块缓存 | 同构 |
| 每 Region 独立 LSM 树（去全局 mutex）| per-table partition + prolly tree，天然分片无全局锁 | 验证了 dendro 分区选择；RocksDB 单树 30 万 SST 的 mutex 瓶颈 dendro 结构上不存在 |
| 弹性扩容 attach S3 不拷数据 | `read_only` 打开 + S-1 惰性分支打开 | 机制已有一半，缺"跟随前沿"（见 §3-A）|
| 元数据驱动秒级备份 | append-only + manifest 后拷（backup.rs）；目标已存在同路径对象即跳过 | v1 文档自认"全量拷贝"，但跳过语义使**对既有目标重复备份已天然增量**（见 §3-B）|
| 段/文件 GC 需跨副本引用追踪 | 墓碑 GC + retire_bound（分支引用即引用方）| dendro 的分支/版本引用追踪已是同构机制，且更一般化 |

## 3. 可借鉴的 delta（按性价比排序）

### A. 只读计算挂载（attach，近期，中等工作量）
TiDB X 的"扩容不拷数据"在 dendro 的等价物：多个只读计算进程 attach 同一 objstore 的同一分支。`read_only` 打开已保证不打 lease；缺的是**跟随 manifest 前沿**——周期轮询 manifest 版本号（或 S3 惯性通知），前进时失效 NodeStore/块缓存中受影响前缀并重建快照。S-1 惰性分支打开 + ArcSwap<DbSnapshot> 是现成接缝。价值：AP/BI 副本横向扩容零拷贝；也是未来"计算层无状态化"叙事的地基。

### B. CAS 增量备份承诺化（近期，纯文档+一个小特性）
backup.rs 已实现"目标存在即跳过"的幂等拷贝——对同一目标重复备份实际只传增量字节。升级两件事：① 把"增量"从实现细节升格为承诺（写进 SPEC/运行手册，标注语义：目标已有的对象视为已备份）；② 利用 CAS，向"已有旧备份的任意目录/S3 桶"备份时体积 ∝ 增量。对标 TiDB X"备份与数据量无关"叙事的 dendro 版：**与（相对目标的）增量成正比**，且因内容寻址，跨备份链去重免费。

### C. compaction/checkpoint 出进程（中期，seam 已存在）
TiDB X 把 compaction 拆成弹性服务（重 I/O 不与 OLTP 抢 CPU/IO，索引构建快 5×）。dendro 的对应重活：checkpoint 全量投影、AP 列存重建、GC sweep。seam 已经存在：这些操作都产出**不可变 chunk + manifest swap**，本身无共享可变状态。出进程方案 = manifest CAS 做工作租约（claim 一个 `{branch, target}` 工作项写入 manifest，完成后 publish）——与分支创建/DDL 同一套 CAS 纪律，无需新协调服务。触发条件：实测 checkpoint 影响前台 p99 时再做，现在不做。

### D. 多写者 HA：复制货币 = manifest 增量（远期设计记录）
若 dendro 需要单分支多写者高可用，TiDB X 给了直接可抄的形状：**共识日志只复制 WAL + manifest 增量（文件版本变更），数据文件共享，memtx/缓存各副本本地重建**。映射到 dendro：commit_tx 的 Pass1（WAL enqueue）经共识复制 → 各副本各自 Pass2 安装 → 各自维护水位。当前 fencing 单写者是刻意取舍（简单、OSS 上足够），此条仅记录"要 HA 时的最小改动路径"，不启动。

### E. ack 点契约（无动作，文档对齐）
TiDB X：ack 在本地盘 Raft log，S3 异步——低延迟但实例级故障依赖副本。dendro：ack 在 objstore durability 模式（Group = 段 fsync），单副本即持久。两者是"本地盘+副本"与"对象存储+契约"谱系的两端；dendro 的云原生定位（OSS RTT 主导、免管实例）下**不需要**追本地盘方案，现有两段式 + 组提交已是该谱系内的正确解。此条仅防"性能对比时口径混淆"。

## 4. 不照抄的部分

- **3 副本共享 SST 的动机**是消除经典三副本 3× 存储放大；dendro 的 CAS 已把共享做到内容级（跨分支去重），不存在同构放大问题，无需为共享而引入副本概念。
- **shared cache 独立层**（行存+列存缓存作为计算与 S3 之间的服务）：dendro 规模下是过度设计，cached.rs 块缓存 + NodeStore LRU 已覆盖热路径。
- **分支/时间旅行缺失**：TiDB X 文档未覆盖 snapshot 分支语义（只有元数据备份）——这是 dendro 的差异化维度，对照时注意谁向谁学：存储形状学 TiDB X 的运维分离，语义层面 dendro 是超集。

## 引用来源

- [siddontang on X — Inside TiDB X | Shared SSTs](https://x.com/siddontang/status/2099418842892312849)
- [siddontang on X — Inside TiDB X | Write](https://x.com/siddontang/status/2099670387198173399)
- [siddontang on X — Inside TiDB X Series](https://x.com/siddontang/status/2099679899267346791)
- [TiDB X Architecture | TiDB Docs](https://docs.pingcap.com/tidbcloud/tidb-x-architecture/)
- [Ten Years, Starting Again: My Journey with TiDB — siddontang (Medium)](https://medium.com/@siddontang/ten-years-starting-again-my-journey-with-tidb-d017139331a3)
- [Ten Years On, We Set Off Again — PingCAP blog](https://www.pingcap.com/blog/ten-years-on-we-set-off-again-my-journey-with-tidb/)
