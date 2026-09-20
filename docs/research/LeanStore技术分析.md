# LeanStore 技术分析（PVLDB 17(12) 2024, Viktor Leis）→ dendro 映射

> 论文：LeanStore: A High-Performance Storage Engine for NVMe SSDs
> （docs/research/LeanStore-PVLDB17-2024.pdf，10 页，已提取）。
> GitHub 仓库部分能力未开封（owner 提示）——以论文为分析基准。
> **约束：dendro 的写入不可变（CAS/append-only/prolly）底线保留**——
> 映射时逐项标注"直接采纳 / 需改造（不可变形态）/ 不采纳"。

## 一、LeanStore 关键技术点（论文 §2）

### 1. 缓冲管理：vmcache（指针置换的替代路线）

- **背景**：指针置换（swizzling，ICDE'18 版 LeanStore）在驱逐页时
  需解引用全部入边、对非树结构困难、正确性微妙。
- **vmcache**：保留**间接层**（逻辑页号 → 指针的映射表），但用
  **虚拟内存硬件**（页表/mmap）让间接层查找几乎免费；DBMS 自己
  控制驱逐（对比 OS mmap 的语义问题： eviction 信号、零页错误等）。
- **驱逐**：两阶段随机采样（先随机 unswizzle 候选、再驱逐其中
  冷者）——访问热页零额外成本（无 LRU 计账）；WATT 等价值模型
  为进阶。
- **4KB 页**：NVMe 随机 I/O 延迟最低、放大最小（16KB→4KB 的演化）。
- **I/O 路径**：去全局 I/O 锁、免小对象动态分配、高并发异步 I/O
  （SSD 内部并行度 > 线程数）、绕 OS 页缓存/文件系统、SPDK 用户态。

**→ dendro 映射**：
- 我们的 prolly 节点 LRU（字节预算）≈ 其热页层；**间接层本就存在**
  （Hash → Node，内容寻址）——比 vmcache 的页号间接更"不可变友好"。
- **可采纳（改造）**：两阶段随机采样驱逐替代 LRU 计账——当前
  lru::LruCache 每次 get 都移动链表节点（写缓存行）；采样式对
  只读热点零写。工作量小，点查热路径直接受益。
- **不采纳**：原地脏页/写回（违背不可变）；SPDK 用户态 I/O
  （本地 LocalObjStore 走 OS 页缓存即可，10M 验证双层缓存 99.1%
  命中已证明此层健康）。

### 2. 同步：乐观版本锁（非 lock-free）

- 每锁带版本计数，读者**乐观读 + 版本校验**（无物理写缓存行）；
  lock-free 因原语受限需映射表/delta 记录反而更慢且易错——明确不用。
- **→ dendro 映射**：memtx 分片 RwLock 的读侧可改乐观版本
  （shard 版本计数 + SeqCst 读校验）——多核点查扩展性项。
  当前单线程基准不受益，列为多核阶段任务（**直接采纳，延后**）。

### 3. 日志：去中心化 + GSN + 组提交

- 生理日志（页号 + 页内逻辑 redo/undo），**每线程独立日志**，
  Lamport 时钟/GSN 维持偏序——去中心化 WAL 消除单点 LSN 瓶颈。
- 提交确认：组提交等待 GSN 依赖闭包（跨日志 flush 的依赖追踪）。
- 模糊检查点、增量恢复。
- **→ dendro 映射**：**结构同源度惊人**——我们的 WAL 本就按
  (branch, epoch) 分目录（epoch=去中心化时钟），复合时间戳
  epoch<<32|seq = GSN 语义（recovery 回放已按此偏序）；组提交
  （Durability::Group + commit_mu 两段式）已有。
  **差距在量级**：每事务一个帧 → LeanStore 每线程批量。可采纳：
  **WAL 帧攒批**（同线程连续事务合并为一次 flush——SQLite 对比
  update 0.37× 的直接对策，P2 已列）。

### 4. MVCC：OSIC + Graveyard

- 快照即时提交（commit log 数组 + worker 缓存他人条目）——快照
  创建 O(1) 无全局屏障；Graveyard 索引解决长快照阻塞版本回收。
- **→ dendro 映射**：我们的 watermark/inflight 语义同向；Graveyard
  思想 = 长读快照的版本回收调度（memtx 版本链 GC 的进阶——
  SOTA 调研中 MVCC GC 已列）。**采纳（改造为不可变版本链形态）**。

### 5. 热/冷分层

- 论文组相关工作（非本文主体）：热页驻内存、冷页 FSST 压缩落
  SSD——**与我们列存段 + memtx 热层的架构同构**；10M 验证的
  双层缓存（dendro LRU + OS 页缓存）是它的本地特例。
- **→ 已在架构中**；追加项：prolly 节点的冷压缩（LRU 驱逐前
  FSST——低优先，页缓存已兜底）。

## 二、对"update/point 也低"的直接回应（SQLite 对比 0.23×）

论文的 15,000 周期预算论（100 核 × 3GHz × 8 SSD × 2.5M IOPS）
正是我们的镜鉴：**TP 点查的全部内部开销必须在一个严格预算内**。
我们的 8.7µs 里 ~6µs 是 SQL 文本层（模板+克隆+代入+机器），
SQLite 的 2.1µs 里几乎全是执行。LeanStore 启示的优先序：

1. **P0（本次）**：prepare 免克隆——AST 原位代入 + Drop 守卫还原
   （执行器视角 = 参数槽）。目标：显式 prepare 与形状缓存共享，
   点查 →3µs 量级。
2. WAL 帧攒批（§3 映射）。
3. LRU→采样驱逐（§1 映射）。
4. memtx 乐观读（§2 映射，多核阶段）。

## 三、不采纳清单（不可变底线）

- 原地更新页/dirty page write-back（生理日志的物理侧）
- B-tree 节点原地分裂/合并（prolly 结构共享即我们的等价物）
- ARIES 集中式 LSN（epoch 复合时间戳已覆盖）

## 信源

- 论文：https://www.vldb.org/pvldb/vol17/p4536-leis.pdf（本仓库存档）
- 代码：https://github.com/leanstore/leanstore（部分未开封，以论文为准）


## 四、全克隆补充：分支地图与未开封能力（owner 提示验证）

全克隆（非 depth-1）后 README 的分支-能力矩阵（master 未合入 =
"未开封"）：

| 分支 | 能力（论文） | master 状态 | dendro 关联 |
|------|-------------|-------------|-------------|
| `master` | vmcache 缓冲管理 + 乐观锁耦合 + OSIC/SI + 分布式日志 + CIDR'21（contention_split/xmerge）+ BTW'23 前缀压缩 | ✅ 主体 | 本档案 §1-4 已析 |
| `latency` | **自主提交**（SIGMOD'25：去组提交——每事务自决 durable 时机，NVMe 队列深度允许即发起，不等批次） | ❌ 未合入 | **直接对标我们的 commit_mu 两段式组提交**——update/insert 提速的深层对策 |
| `io` | 高性能 I/O 栈（VLDB'23：io_uring/SPDK、绕 OS） | ❌ | LocalObjStore/S3 后备阶段参照 |
| `blob` | 变长对象 + 文件系统接口 + 虚存别名（ICDE'24） | ❌ | CAS 对象层设计参照 |
| `WATT`/`ssd-*` | 驱逐价值模型 / SSD 行为研究 | — | LRU→采样驱逐进阶 |
| `mvcc` | OSIC 细节 | 部分在 master | §4 已映射 |

**关键新发现（latency，SIGMOD'25 自主提交）**：组提交的批间隔是
延迟下限；自主提交让每事务在 NVMe 队列深度允许时立即 durable
（"Moving on From Group Commit"）。我们的 WAL Group + 20ms flush
正该论文批判的形态——**WAL 帧攒批（§3 映射）应参考 latency
分支设计**。
