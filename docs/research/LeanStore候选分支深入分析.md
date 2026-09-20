# LeanStore 候选分支深入分析（latency / io / blob）→ dendro 映射

> [LeanStore技术分析.md](LeanStore技术分析.md) §四分支地图的展开：README
> "Implemented Features" 未勾选的三项对应三个未合入分支，本文逐一开箱。
> 方法：全克隆 worktree 代码解剖（非 depth-1）+ 四篇论文存档
> （见文末信源，均在 docs/research/）。
> **约束不变**：dendro 追加-only/CAS/prolly 底线保留，映射逐项标注
> 直接采纳 / 需改造（不可变形态）/ 不采纳。
>
> 开箱结论先行：**latency 分支价值最高**（自主提交直接对标我们的
> commit_mu 两段式）；blob 分支提供 prolly 大值分层的设计蓝本；
> io 分支对云原生路径不急迫（单机裸块设备档位再回来）。

## 一、latency 分支——自主提交（SIGMOD'25，价值最高）

### 1.1 问题定义：组提交的批间隔就是延迟下限

论文《Moving on From Group Commit: Autonomous Commit Enables High
Throughput and Low Latency on NVMe SSDs》（Nguyen, Alhomssi, Ziegler,
Leis）的出发点：

- 磁盘时代：单次写昂贵 → 必须攒大批摊薄 → 组提交是正确答案；
- **企业级 NVMe 的 4KB 写延迟已低至 ~11µs**（论文实测）——写日志本身
  不再是瓶颈，**等批的排队时间成了提交延迟的主体**；
- 组提交把"吞吐"和"延迟"绑成一个旋钮（批大小），二者不可兼得是
  设备时代的遗产，不是本质。

回答："让每个 worker 自己决定何时写日志"——NVMe 队列深度足够
（每 worker 私有 io_uring），小批多次写不再惩罚吞吐。

### 1.2 代码解剖（src/ 全重写超集）

该分支是**全新代码基**（`src/` 布局取代 master 的 `backend/frontend`），
且是超集：自主提交（SIGMOD'25）+ blob（ICDE'24）+ vmcache（SIDMOG'23）
+ RFA 分布式日志（SIGMOD'20）+ OLC（IEEE'19）+ BTW'23 B-tree 并存。

**四种提交协议共存**（`src/config.cc` 的 `txn_commit_variant`，
对照实验即论文的实验矩阵）：

| 值 | 协议 | 机制 |
|---|------|------|
| 0 | BASELINE_COMMIT | 传统组提交：commit 时 TriggerGroupCommit 唤醒专职线程 |
| 1 | FLUSH_PIPELINING | 批 N 的落盘与批 N+1 的收集流水重叠（默认值） |
| 2 | WORKERS_WRITE_LOG | worker 自写日志：攒够 worker_write_batch_size 才写 |
| 3 | **WILO_STEAL** | 自主提交：自写 + **偷同伴日志**（README 示例即此） |

**核心数据结构**（`src/include/recovery/`）：

- `LogWorker`：每 worker 一个**环形日志缓冲**（`wal_cursor` 写入位/
  `write_cursor` 已落盘位，回绕用 `CARRIAGE_RETURN` padding 帧填缝）
  + **私有 io_uring ring（QD=8）**；
- `WorkerConsistentState`：三元组 `(last_wal_cursor, last_gsn,
  precommitted_tx_commit_ts)` + HybridLatch——组提交/偷日志者收集的
  一致性快照，`Clone()` 本身带 mfence 语义（先见状态必先见缓冲内容）；
- `parking_lot_log_flush`：in-flight 落盘窗口内的状态停车场；
- `LogManager`：全局 GSN 时钟、`w_offset_`（**fetch_sub 预留递减**的无锁
  文件偏移分配）、`global_min_gsn_flushed`（RFA 依据）。

**自主提交的决策与偷日志**（`log_worker.cc`，论文 §3 的实现）：

```
ShouldStealLog() 三态（pending = wal_cursor - write_cursor）：
  pending ≥ worker_write_batch_size        → TO_WRITE_LOCALLY（攒够自写）
  pending ≥ batch_size / 组位宽            → TO_STEAL（值得开一次 IO）
  否则                                      → NOTHING（先不写）
```

`WorkerStealsLog` 的完整动作：

1. 在 stealing group（`wal_stealing_group_size` 个相邻 worker）内逐 peer
   `TryStealLogs`：快照 peer 一致性状态 + memcpy peer 的未写字节进
   自己缓冲（前提：本方连续空闲区装得下 peer 待写量 + 一个 CR 帧）；
2. **一次 `PersistLog`（io_uring write）覆盖全组日志**——偷日志者成为
   本组一次性的"临时组提交者"，把 N 个小写合并为一个 4K 对齐写；
3. 落盘后 `TryPublishCommitState` 逐 peer 发布持久性状态——被偷者的
   事务随之可 ack；
4. 兜底：commit 时以 `1/worker_count` 概率也触发全局 GroupCommit
   （防低流量事务无限滞留）。

**仍在的组提交协调器**（`group_commit.cc`，五阶段/round）：
P1 收集全部 worker 一致性状态 + WAL 写准备（io_uring QD=16）→
P2 submit + `fdatasync` + 发布 write_cursor → P3 blob extent 写 +
EvictExtent → P4 RFA 感知的队列 ack。工程亮点
`txn_collect_state_during_flush`：P1 与日志落盘**重叠执行**（注释：
100 核机器上 P1 每 round 贡献 ~10µs）——投机收集可能略陈旧，正确性
不受损（最多漏 ack 几个已提交事务，下轮补上）。

**RFA（Remote Flush Avoidance，SIGMOD'20 遗产）**：`needs_remote_flush
= false` 的事务（依赖闭包全在本地日志）只等自己日志持久即可 ack
（`TryCommitRFATxns`），不等 `global_min_gsn_flushed`。

### 1.3 → dendro 映射

| 项 | 判定 | 说明 |
|---|------|------|
| 批间隔是延迟下限 | **直接采纳（论断层）** | 我们的 `Durability::Group` + flush_loop 20ms 定时正是论文批判的形态；SQLite 对比 update 0.37× 的深层原因之一 |
| 帧攒批（同分支连续事务合并 flush） | **直接采纳（近期）** | 单写者下的第一步：事务提交不逐帧 flush，写满阈值或 lease tick 立即写——去掉"定时器空等" |
| 阈值触发替代纯定时 | **直接采纳** | `pending ≥ batch_size 即写`的 ShouldStealLog 骨架可平移到 WalShared（pending_bytes 已有！只差"写满即触发"） |
| WILO_STEAL 多日志偷写 | **P3 阶段参照** | dendro 分支租约 = 每分支单写者，无 per-worker 日志；**P3 分布式 OCC / Journal 服务化时**这就是提交协议蓝本（多日志 + 偷写合并 + RFA 本地依赖 ack） |
| RFA 本地依赖即 ack | 已天然具备 | 单分支内 durable 即 ack；跨分支 merge 才引入依赖闭包——届时 RFA 是参照 |
| GSN ≙ epoch<<32\|seq | 已同构 | 技术分析 §3 已映射（recovery 回放按复合时间戳偏序） |
| fdatasync | 不适用 | 对象存储 PUT 全有或全无，无 fsync 面 |
| io_uring 私有 ring | 不采纳（当前） | S3/本地目录路径无裸块设备；单机高性能档位与 io 分支一起再评估 |

## 二、io 分支——高性能 I/O 栈（VLDB'23，暂不急迫）

### 2.1 论文要点

《What Modern NVMe Storage Can Do, And How To Exploit It》（Haas &
Leis, PVLDB 16）：系统测量现代 NVMe 的真实能力（并行单元数、队列
深度行为、admin/log page 可观测面），并给出存储引擎 I/O 栈的
落地建议。

### 2.2 代码解剖（backend/leanstore/io/）

- **两层抽象**：`IoEnvironment`/设备层（四后端：`LibaioImpl` /
  `LiburingImpl` / `SpdkImpl`（用户态 NVMe 驱动）/ `XnvmeImpl`）×
  `IoChannel` 通道层（`Raid0Channel` / `Raid5Channel` **软 RAID**，
  64KB chunk 交错；`RequestStack` 预分配请求池免热路径分配）；
- `LiburingImpl` 细节：`IORING_SETUP_SQE128/CQE32`（为 NVMe
  passthrough `IORING_OP_URING_CMD` 留大 SQE）、`IORING_SETUP_IOPOLL`
  轮询模式、`IORING_SETUP_SQPOLL` 内核线程提交、`ATTACH_WQ` 多 ring
  共享 worker 池、`__io_uring_peek_cqe` 批量收割；
- `NvmeLog.hpp`：libnvme 读 **SMART/health log page**——物理介质写入
  量（PMUW，写放大实测）、坏块、热节流事件等——论文"看穿 SSD 内部"
  的工具；`SSDCounters` 接入性能剖析；
- `iob` 独立基准器（README 示例：8 盘 RAID0、IO_DEPTH=128、IOPOLL）。

### 2.3 → dendro 映射

| 项 | 判定 | 说明 |
|---|------|------|
| io_uring/SPDK 栈 | **不采纳（当前）** | 云原生路径（OSS/S3）不踩块设备；本地 LocalObjStore 走 OS 页缓存已被 10M 双层缓存 99.1% 命中验证健康 |
| 多盘 RAID0 64KB chunk | 改造观察 | S3 侧等价物 = 多 prefix/多连接并行 + 分段上传——已在对象存储原生能力内，无需自建 |
| SMART 计量（PMUW 写放大） | **采纳（运维启发）** | /metrics 增加存储健康维度：对象 PUT 字节 vs 逻辑提交字节的放大率监控（我们已有 pending_bytes/commit 延迟，缺放大率） |
| 单机裸块档位 | 记档 | dendro backup / 高性能单机模式若引入 `--block_device`，回来抄此分支 |

## 三、blob 分支——变长对象（ICDE'24，prolly 大值分层蓝本）

### 3.1 论文要点

《Why Files If You Have a DBMS?》（Nguyen & Leis）：大对象进文件
系统 vs 进 DBMS 的系统对比；结论是**新 BLOB 分配 + 日志设计（每个
大对象恰好异步写一次）在大小对象混合负载下同时超过文件系统与其他
DBMS**——文件系统的目录项/页缓存/日志层对大对象全是税。

### 3.2 代码解剖（src/storage/blob/）

- **BlobState 内联进 B-tree 值**（`blob_state.h`，设计精华）：
  - `blob_prefix[32]`：前缀内联——**有序/范围算子直接在索引层完成**，
    不触大对象；
  - `sha2_val[32]` + `sha256_intermediate[4]`：SHA-256 摘要 + **可恢复
    中间态**（增量计算，追加时续算）——哈希算子同样不出索引层；
  - `ExtentList`：127 个 extent 上限（7 bit，注释原话"Beautiful
    number"）、15 层 × 8 扇出的幂次分层表（理论容量 5.76×10^17 YB）；
    `BlobID = 首 extent 的 page id`；
  - 尺寸预算：BlobState 最大 1136B，保证**每页至少 3 个 BlobState**
    （索引密度约束先于容量约束）。
- `BlobManager` 生命周期：`AllocateBlob`（新分配 / `ExtendExistingBlob`
  尾 extent 追加——增长型 blob 只重写尾块，`MoveTailExtent` 处理
  追加越界）/ `LoadBlob`（按需加载 + `PageAliasGuard` **虚存别名**把
  离散 extent 映射成连续 VA——调用方拿到"连续大对象"假象）；
  `RemoveBlob` 进 free page manager；
- **提交管线集成**（latency 分支的 `PhaseThree`）：blob extent 的
  持久化挂在 WAL 落盘**之后**（日志先于数据的原则保持），extent 写
  完成后 `EvictExtent` 才允许驱逐——"恰好一次异步写"的顺序保障；
- 对照实验面（`benchmark/`）：`filesystem_adapter` 把 ext4
  (data=ordered/journal)、f2fs、btrfs 当"数据库"跑同负载；
  `test/fuse/fuse.cc` 反向把 LeanStore 挂成 FUSE 文件系统
  （path→key，getattr/read）。

### 3.3 → dendro 映射

| 项 | 判定 | 说明 |
|---|------|------|
| 前缀+哈希内联索引层 | **采纳（改造）——prolly 大值分层蓝本** | dendro 行值无上限但 prolly chunker 有预算：超阈值行可引入 blob tier（值 → 内容寻址 chunk 引用列表）；**前缀换我们的 20B Hash + 定长前缀字节**，排序与内容校验留在树层 |
| 增量 SHA 中间态 | 采纳（改造） | append-only 追加场景续算 Hash（SHA-512 截断 160b 已是我们的 Hash——中间态可保留在 blob 元数据） |
| 尾 extent 追加 | 已有等价物 | append-only + chunk 边界稳定（prolly 的本质）；MoveTailExtent 的问题在不可变世界里不存在 |
| 恰好一次异步写 + 先日志后 extent | 已同构 | WAL→checkpoint 物化即此序（I-A1 AckedInstalled 家族） |
| 每 extent zstd | 已有 | CBF 块级 codec（RAW/BITPACK/RLE_DICT/FSST）更细 |
| FUSE/文件系统接口 | 记档（远期想象力） | [kv-接口层.md](design/kv-接口层.md) 的进一步：分支化 FS = Agent 沙箱的 `worktree`；"DBMS 当文件系统"与"文件系统当 DBMS"（Git）对偶 |
| BlobID=首块 pid | 不采纳 | 与内容寻址冲突（我们的身份 = Hash，非位置）；仅作对照 |

## 四、加送：BtrLog（VLDB'26，同作者后续）

下载时发现同第一作者的《BtrLog: Low-Latency Logging for Cloud
Database Systems》（Kuschewski, Nguyen, Jasny, Ziegler, Leis, El-Hindi，
已存档 `BtrLog-VLDB-2026.pdf`）——**云数据库低延迟日志**，与我们
[S3_WAL设计调研.md](S3_WAL设计调研.md) / Journal 服务化路线（P2'）
正面相关：自主提交思想在对象存储/云盘日志上的延续。建议作为
Journal 设计的下一份精读材料（本文不展开）。

## 五、其余开放分支深入（本轮一并开箱）

### 5.1 WATT 分支——写感知页替换（VLDB'23，驱逐进阶参照）

论文《Write-Aware Timestamp Tracking: Effective and Efficient Page
Replacement for Modern Hardware》（Vöhringer & Leis，PVLDB 16，
FAU 时期工作，`LeanStore-WATT-VLDB16-2023.pdf`）。

- **动机**：LRU/CLOCK 系驱逐只优化命中率，把脏页写回当成免费；
  SSD 上驱逐脏页 = 一次真实写——替换算法应该是**写感知成本模型**；
- **机制**（`BufferFrame.hpp`，+198 行集中改动）：每页维护 SIMD 可扫的
  **压缩时间戳数组**（`simd_getFreq`、U8/U16 位打包时间戳）近似访问
  频率/新近度，驱逐评分纳入"脏则需写回"的代价项；`WATT_LOG`
  backlog 与 `FLAGS_watt_history` 支持离线回放调参；多核协同设计
  （时间戳数组避免 LRU 链表的共享缓存行写）；
- **→ dendro 映射**：我们 prolly LRU（字节预算）当前是纯计数；
  技术分析已列"LRU→两阶段随机采样"，**WATT 是再下一步**——但
  dendro 页不可变（无脏页写回！），WATT 的写感知项在我们这里
  结构性消失，只剩"访问频率感知驱逐"仍有意义。**判定：记档，
  优先级低于采样驱逐**（不可变架构恰好豁免了 WATT 要解的问题）。

### 5.2 svcc 分支——OSIC 论文的实验基础设施（对比 SVCC vs MVCC）

17 个独有提交，`src/transaction/svcc/`（wait_die_lock.h + lock_manager）
——对应论文《Scalable and Robust Snapshot Isolation for
High-Performance Storage Engines》（Alhomssi & Leis，PVLDB 16(6):
1426-1438，`LeanStore-OSIC-VLDB16-2023.pdf`）。

提交史即论文实验搭建过程：WaitDieLock 基础设施 → **SVCC（单版本
并发控制：wait-die 2PL，可串行化基线）** 集成 → Hyper 式 MVCC
（B-tree 节点内 per-tuple 时间戳 + 私有 undo 缓冲 + OCC 校验 +
低水位 GC）→ "Fix both SVCC and MVCC run-time for comparison"。

代码要点：

- `WaitDieLock`：时间戳降序排序的 owner/waiter 双列表（降序插入
  使 wait-die 判定 O(1)——年轻者遇老者等待、反之夭折），移植自
  rotaki/tpcc-runner 并改造；
- `LockManager`：`tbb::concurrent_hash_map` 全局锁表 + **thread-local
  读写集**（提交/回滚快速释放）+ TryLock 携带 undo_ts/undo_payload
  （写进私有 undo 缓冲，abort 时逆序回放）；诚实 TODO：冷元组锁的
  内存回收未做；
- 论文结论方向：MVCC-SI 在长读下版本堆积崩溃，OSIC（顺序快照即时
  提交）+ Graveyard 才是 out-of-memory 的可扩展形态。

**→ dendro 映射**：wait-die 2PL 是 dendro **悲观可串行化档位**的
现成参照（当前只有 OCC）；thread-local 读写集 + 全局锁表的结构与
我们 memtx 写集同构；"SVCC vs MVCC 对比基础设施"的实验方法论
（同一代码库两种 CC 可切）值得 memtx 演进时借鉴（`--isolation`
开关已具备此形态）。**判定：P2 后多隔离级档位的设计参照**。

### 5.3 ssd-latency / ssd-waf 分支——实验分支（无独立论文）

- **ssd-latency**（Haas，2025-05，2 独有提交）：scheduler + ycsb
  workload 改造——**开环到达率调度**（固定速率注入而非闭环尽力压）
  的延迟测量方法论基建。这是 SIGMOD'25 延迟研究的方法前提：闭环
  压测看不出排队延迟。**→ dendro 映射**：我们 benches 全是闭环；
  未来做提交延迟对比时需同款开环注入器（列bench 待办）。
- **ssd-waf**（老代码基小 diff）：profiling 线程追记
  `consumedPages()/maxPid` 随时间曲线——**稳态 TPC-C 下库体积
  增长**即内部写放大观察。与 io 分支的 SMART PMUW 互补（逻辑侧
  vs 物理侧）。**→ dendro 映射**：/metrics 已有 pending_bytes；
  补"逻辑提交字节 vs 对象 PUT 字节"曲线即我们版的 WAF 观察。

### 5.4 latency-j / no-exmap / release——变体分支

- **latency-j**：SIGMOD'25 工件重打包（Initial commit 式干净历史，
  README cite 同一论文）——工件评审用，无增量内容；
- **no-exmap**：superset 代码基的**无内核模块变体**（"Deprecate
  exmap" 删 4087 行 + OOM 负载修复）——无 root/无 exmap.kf 环境的
  可移植构建；
- **release**（ahead 0，已并干）："remove si_commit_protocol option"
  ——发布清理。

### 5.5 已合并历史分支（ahead 0 = master 祖先，无开放内容）

| 分支 | 对应论文 | 存档 |
|------|---------|------|
| `btw` | 《The Evolution of LeanStore》（Alhomssi, Haubenschild, Leis，BTW'23 LNI P-331:259-281）——系统全览 + 变长 K/V B-tree（fences/前缀压缩/head 提取/hints） | `LeanStore-evolution-BTW-2023.pdf` |
| `mvcc` | OSIC 论文线（同 §5.2） | `LeanStore-OSIC-VLDB16-2023.pdf` |
| `cidr` | 《Contention and Space Management in B-Trees》（Alhomssi & Leis，CIDR'21——contention split + xmerge） | `LeanStore-cidr21-contention.pdf` |

### 5.6 全分支总表（14 分支终局地图）

| 分支 | 论文 | 状态 | dendro 价值 |
|------|------|------|------------|
| latency | SIGMOD'25 自主提交 | 开放 | ★★★ P3 提交协议蓝本 + 近期帧攒批 |
| blob | ICDE'24 变长对象 | 开放 | ★★★ prolly 大值分层蓝本 |
| io | VLDB'23 NVMe I/O | 开放 | ★☆ 单机块设备档位记档 |
| WATT | VLDB'23 写感知替换 | 开放 | ★ 不可变架构豁免其核心问题 |
| svcc | OSIC（VLDB'23） | 开放 | ★★ 悲观可串行化档位参照 |
| ssd-latency/ssd-waf | —（实验） | 开放 | ★ 开环基准方法论 + WAF 观察 |
| latency-j/no-exmap/release | —（变体） | 开放 | — |
| btw/mvcc/cidr | BTW'23/OSIC/CIDR'21 | 已合并 | 论文存档价值 |
| master | PVLDB'24 主体 | — | 见技术分析 |

**论文族谱补遗**：README 反复引用却未展开的 [SIGMOD'20] =
《Rethinking Logging, Checkpoints, and Recovery for High-Performance
Storage Engines》（Haubenschild, Sauer, Neumann, Leis，SIGMOD'20
pp.877-892，**CC BY 4.0 金色开放获取**，已存档
`LeanStore-logging-SIGMOD-2020.pdf`）——**分布式 per-thread 日志 +
RFA + 增量/模糊检查点 + 恢复**的出处，latency 分支的 RFA 与五阶段
协议皆源于此，也是 README "Recovery [ ]" 未完成项的对应文献。
DOI: 10.1145/3318464.3389716（ACM 站有 JS 挑战墙，开放副本经
Unpaywall/Semantic Scholar 确认 CC BY 后归档）；与我们 recovery.rs /
WAL 设计正面相关，**BtrLog（§四）是其 2026 年的云化续作——两篇连读**。
**已精读并逐条校验十项主张**（九项成立 + 三处诚实性加注：全实验
read-uncommitted、Optane 平台自身饱和、对照组为自系统重实现）：
[LeanStore-logging-SIGMOD20-精读与观点校验.md](LeanStore-logging-SIGMOD20-精读与观点校验.md)。

## 信源（本仓库存档）

- `LeanStore-latency-SIGMOD-2025.pdf` —— 自主提交（对应 latency 分支；
  代码 = 本文 §1 解剖）
- `LeanStore-io-VLDB16-2023.pdf` —— NVMe I/O（io 分支）
- `LeanStore-blob-ICDE-2024.pdf` —— 变长对象/FS 接口（blob 分支）
- `BtrLog-VLDB-2026.pdf` —— 加送（云日志，Journal 路线精读材料）
- `LeanStore-WATT-VLDB16-2023.pdf` —— 写感知页替换（WATT 分支）
- `LeanStore-OSIC-VLDB16-2023.pdf` —— 可扩展 SI/OSIC（svcc + mvcc 分支）
- `LeanStore-evolution-BTW-2023.pdf` —— 系统全览/变长 B-tree（btw 分支）
- `LeanStore-cidr21-contention.pdf` —— 争用与空间管理（cidr 分支）
- `LeanStore-logging-SIGMOD-2020.pdf` —— 分布式日志/RFA/检查点/恢复
  （latency 分支 RFA 与五阶段协议的出处；CC BY 4.0；与 BtrLog 连读）
- （既有）`LeanStore-PVLDB17-2024.pdf` —— master 主体，见
  [LeanStore技术分析.md](LeanStore技术分析.md)
- 代码：/home/nzinfo/src.db/leanstore 全克隆（14 分支全部 worktree 开箱）
