# S3 WAL 设计调研：Vanlightly s3-wal-collection（2026）

> 调研时间：2026-09-16。触发：用户指定参考
> <https://github.com/Vanlightly/s3-wal-collection>，评估其对 dendro WAL
> 及其他部分的参考意义。结论先行：
>
> 1. 该仓库是 Jack Vanlightly 的 **TLA+ 规格合集**（不是实现代码）：把业界
>    "S3 上做 WAL"的设计收进一套统一建模框架（对象存储 + 条件写 + 写者
>    状态机 + GC），逐个抽出协议本质并用模型检查验证。
> 2. dendro 的 WAL 与其中 **SlateDB WAL（单写者 + 围栏）同族**，且 dendro
>    已落地该族的关键机制：epoch 入段路径（旧写者写不进新世代）、租约
>    围栏、组提交、durable 水位、段退休安全界。机制层面的直接缺口很小。
> 3. **最大差距在方法论而非机制**：dendro 靠人肉审计发现的两个 P0（在途
>    帧 vs 段退休 R3-P0、GC 复活已删分支 fence 对象）恰好都是这套规格
>    的不变量/性质检查所针对的竞态类别。为 dendro WAL 写一份 TLA+ 规格
>    是本调研最值得落地的产出（P0 建议）。
> 4. 多写者线（OSWALD / Conflux / LogDrive）对应 dendro
>    [提交管线重构](../design/提交管线重构.md) 中 P2'/P3 的多节点扩展路径，
>    当前仅作跟踪，不引入。

## 1. 仓库概况

| 维度 | 事实 |
|------|------|
| 作者 | Jack Vanlightly（前 Confluent/Kafka 圈，近年专注用 TLA+ 分析分布式系统协议） |
| 定位 | WAL-on-S3 设计的 **TLA+ 规格合集与分析笔记**，非可运行实现 |
| License | MIT |
| 成熟度 | 早期（2026 年中起步，十几次提交；36 stars） |
| 内容 | 4 个已建模设计（含 1 个变体）+ 各自 notes.md 状态机图与不变量说明 |

收录标准（README 原文归纳）：**S3 是唯一真源**（允许软状态优化，如缓存
预读）；不做 SSD 持久数据；不依赖复杂元数据服务（单条件写/版本化对象即可）。
这三条与 dendro 的"直写对象存储 + 无本地盘"路线完全同频。

### 分类与收录状态

| 类别 | 设计 | 状态 |
|------|------|------|
| 单写者 + 写者围栏 | SlateDB WAL 协议 | ✅ `slatedb/SlateDBWAL.tla` |
| 单写者 + 写者围栏 | SlateDB WAL 的 CAS-manifest 变体 | ✅ `slatedb/SlateDBWAL_CAS.tla` |
| 单写者 + 写者围栏 | OpenData Buffer 改造为 WAL | ✅ `opendata/BufferAsWAL.tla` |
| not-quite-WAL | OpenData Buffer（多写者单消费者队列） | ✅ `opendata/Buffer.tla` |
| 多写者多主 | OSWALD（nvartolomei） | ✅ `oswald/Oswald.tla` |
| 多写者多主 | Conflux（Virtual Consensus / LogDrive） | ⏳ TODO（规格暂在其个人仓库 `log-drive-specs`） |
| 待评估 | git3、Cursor Continuity、OpenData Log、UnisonDB WAL、s2c、objwal | 📋 期望投稿 |
| 排除 | BtrLog（非 S3-only）、S2.dev（闭源）、S2 Lite（WAL 委托给 SlateDB） | ❌ |

**跟踪价值**：这个仓库正在成为 "S3 WAL 设计"的半官方综述点——待评估清单
（UnisonDB、s2c、objwal、Continuity）值得按月回头看增补。

## 2. 统一建模框架（读这套规格的钥匙）

四个规格共享同一套骨架，这也是 Vanlightly 想凸出的"S3 WAL 协议的公因子"：

1. **存储原语只有四种**：不可变对象 PUT、（条件写或版本化 CAS 的）元数据
   对象、LIST、DELETE。没有rename、没有事务。
2. **写者是一个状态机**：初始化（领权/恢复/重放）→ `READY`（追加+定期
   快照）；任何条件写冲突 → `FENCED`/`PREEMPTED` **终态**——建模上失势
   写者永久停写，liveness 检查才能写得简单。
3. **两条核心不变量**（各规格措辞略异、实质相同）：
   - *前缀性*：每个写者本地已应用的数据是全局成功写历史的**前缀**（最新
     写者持有全部，陈旧写者只有一段前缀）；
   - *可重建性*：任意时刻，"当前元数据 + 存活的 WAL 对象 + 最新状态机
     快照"足以重建正确状态。
4. **GC 是第一竞态源**：陈旧写者可能把对象写进"刚被 GC 删除"的地址
   （写成功但语义无效），所以所有规格都要求**关键写之后重新校验**，并把
   回收下界（boundary / watermark / replayAfter）作为协议一等公民。

> 对照 dendro：`poisoned()`（上传失败后一律 40003、强制 reopen）就是
> "FENCED 是终态"的工程对应；`retire_bound`（段内最大 seq 的 ts ≤
> covered 才可退休）就是"GC 安全线"的工程对应。dendro 已经在按这套框架
> 的结论做事，只是没有把框架本身写下来。

## 3. 已收录设计的协议本质

### 3.1 SlateDB WAL——编号 manifest + boundary 文件 + fence 条目

元数据：**编号递增的 manifest 序列**（每代一个对象）、**GC boundary
文件**、WAL 对象（`DATA` / `FENCE` 两类）、状态机数据快照。

写者初始化的关键序列（notes.md 状态机的要点）：

```
claim epoch（manifest 条件写，失败重试更高 epoch）
  → boundary 校验（领到的 manifest 代号若 ≤ GC boundary，说明该地址已被
    回收、可能换过主人 → 整个初始化重来）
  → 写 FENCE WAL 条目（占位 + 排斥旧写者）
  → validate-epoch-before-replay（重放前再校验一次）
  → 载入最新快照
  → 从 manifest.replayAfterWalId 起重放 DATA → READY
```

**三个"validate-before"**（before-retry / before-replay / before-not-found）
是本规格最值得学的细节：陈旧写者的条件写可能"成功"落在已被 GC 的地址上
（对象不存在≠没写进去），所以 **fence 写成功也不可信，必须回读 manifest
再校验 epoch**。GC 侧配套规则：只删 `replayAfterWalId` 以下的 `DATA`、
**FENCE 对象永不删**、最后一个对象永不删。

### 3.2 SlateDB WAL CAS 变体——单版本化 manifest + CAS

把"编号 manifest 序列 + boundary 文件"换成**单个带版本号的 manifest
对象，CAS 更新**。notes.md 给出的账目：

- 免了：manifest 写后校验、manifest GC、boundary 文件及其校验（三免）；
- 免不了：**fence 写之后的校验**——陈旧写者写进已回收地址的窗口依然
  存在，仍需回读 manifest 检查 `replayAfterWalID`。

取舍：CAS 版牺牲"编号 manifest 天然保留历史读能力"，换来元数据面的大幅
简化。SlateDB 生产形态接近此变体。

### 3.3 OSWALD——多写者多主

- **chunk 地址空间无空洞**：写者 CAS 领下一个 chunk 位置，尾部竞争 =
  条件写冲突 → 转入 **Catchup Recovery**（补读最新 chunk、发现新尾部、
  再战）。
- **GC 水位以下允许不一致**：陈旧写者可能把 chunk 写进刚被 GC 删除的
  位置。两道校验兜住：读路径校验（读到无效 chunk 的写者不得进 READY，
  转 snapshot recovery）+ **写后校验**（写了无效 chunk 的写者不得把它
  应用进本地状态机）。与 3.1 的 "validate-after-write" 同一洞察的多写者版。
- 快照 + manifest 更新同样走 CAS，冲突即重走 catchup。

### 3.4 OpenData Buffer / BufferAsWAL——两步入队与 GC 的经典竞态

Buffer 是"多生产者 PUT ULID 批对象 → CAS 入队 manifest → 单消费者按
cursor 消费"的队列（把 dequeue 视作日志 GC 就是 not-quite-WAL）。

- **GC 在无 grace period 时是不安全的**（notes.md 给了完整反例 trace）：
  w1 写完对象 ts1 尚未入队 manifest，w2 写 ts2 并先入队，GC 以 manifest
  里最老的 ts2 为 floor、把磁盘上的 ts1 删掉——**把还在途的合法批删了**。
  规格里用开关 `EnableGC` 显式暴露：打开即触发安全性质违例。grace
  period（按 ULID 时间戳留宽限期）是补丁，单调确认点才是机制。
- 消费者围栏 = **双 CAS 次序敏感**：先读 manifest、读 cursor，再依次
  CAS manifest（epoch+1）、CAS cursor（version+1）；低 epoch 读者仍可能
  抢到 cursor 但抢不到 manifest 剪裁权——安全性靠"两把锁不同时放"。
- BufferAsWAL：把 Buffer 限成单写者（manifest 版本 CAS 领权 + 快照恢复
  + 逐批 catchup），展示"队列改 WAL"的最小改动面。

## 4. 与 dendro WAL 的逐项对照

dendro 现状依据：`crates/dendro-core/src/wal.rs`、
`crates/dendro-core/src/objstore/fence.rs`、
[提交管线重构](../design/提交管线重构.md)、[GC 定案](../design/GC定案.md)。

| 机制 | s3-wal-collection 形态 | dendro 现状 | 评 |
|------|------------------------|-------------|-----|
| 单写者围栏 | manifest 内 epoch 声明 + FENCE WAL 条目 + 写后校验 | `fence/{branch}/{epoch}.json` 租约（条件写领权）+ **epoch 入段路径** `wal/{b}/e{epoch}/{seg}` + `ts=epoch<<32\|seq` | 同族。dendro 的路径命名空间化让旧写者**根本写不进**新世代路径（阻断更早）；SlateDB 靠"写得进但校验后自认无效" |
| 失势写者 | FENCED/PREEMPTED 终态，永久停写 | `poisoned` → 一律 40003，强制 reopen | 一致 |
| 组提交 | TLA+ 不建模延迟，无对应物 | `enqueue_only`/`flush_now` + NoWait/Group/Always 三档 + 事件驱动刷盘线程 | dendro 有而规格无；属实现层，不构成差距 |
| 崩溃恢复 | 载快照 + 从 `replayAfterWalId` 重放 | Checkpoint 帧 + prolly 提交为恢复锚 + `probe_tail` 指数/二分找尾 | 同构（快照=checkpoint，重放起点=covered 界） |
| 段封口语义 | 对象即边界；DATA/FENCE 分类 | 段尾 trailer：已封段损坏=真腐坏，未封段=撕尾容忍、回放 durable 前缀 | dendro 更细（单对象多帧），语义等价 |
| GC 安全线 | boundary / GC watermark / `replayAfterWalId` | `retire_bound`（段内 max seq 的 ts ≤ covered 才退休）+ 墓碑 + `first_seg` | 同一问题的两解。dendro 的界正是审计 R3-P0 的产物；OSWALD 的"写后校验"提示还可以在**写侧**补一道防线（见 P1-3） |
| 围栏痕迹保留 | FENCE 对象永不删 | 租约对象随分支 GC 删除（曾出"复活已删分支 fence 对象"问题，靠 flush 线程持 Weak 修复） | **dendro 弱项**：规则分散在注释里，未成文（见 P1-4） |
| 快照发布 | 写快照对象 → manifest CAS 两步发布 | pending 登记 → prolly commit 两段式 | 同构 |
| 多写者 | OSWALD（chunk CAS + catchup） | 无（单写者；Journal seam 预留 quorum append / openraft） | 跟踪 P2'/P3 时再取 |

总评：**机制对齐度约八成**，且 dendro 在围栏阻断点（路径命名空间）、
撕裂容忍（trailer）两处比规格建模更精细；弱项集中在"规则成文"与
"写后校验"两点，见下节。

## 5. 对 dendro 的可借鉴点清单

### P0（方法论，直接落地）✅ 2026-09-17 收口

1. ✅ **DendroWAL.tla 已建并经 TLC 模型检查**（见同目录
   `DendroWAL.tla` / `DendroWAL.cfg`；TLC 1.8.0，136 万状态 /
   17 万 distinct 全过）。建模面：多 epoch 写者（领权即围栏旧世代、
   fenced 写者仍可完成在途 PUT）+ 异步刷盘 + checkpoint 水位
   （RecoverableUpTo 前置 = 两段式提交前沿）+ 帧级退休界 + 恢复
   （epoch 升序 + ts 抑制回放）。三条不变量：
   - InvCoveredGrounded（covered 推进有据）、
   - InvAcked（前缀性：ack ⊆ covered ∨ 存活 WAL）、
   - InvRecovered（可重建性：步进回放 ≡ 快照内 max-ts）。
   **否定性验证**证明两守卫必要：削弱退休界 → InvAcked 违例
   （R3-P0 丢提交类反例）；削弱 checkpoint 前置 → InvCoveredGrounded
   违例（covered 越过未 ack 帧推进）。
2. ✅ **不变量落为回归断言**：前缀性 = `replay_branch` 末端
   debug/test 断言（`BranchMem::debug_check_ts_bound`，含断言器
   负例测试）；可重建性 = `reopen_rebuilds_identical_state`
   （同存储重开逐行等价，含 UPDATE/DELETE 语义）。

### P1（机制，小改动）✅ 2026-09-17 收口

3. ✅ **写后校验已落地**：`LeaseKeeper::renew_if_due` 在续期 PUT 成功
   后以 manifest 权威读回校验 refs——死分支立即 `delete_lease` 撤销
   复活对象（epoch 单调保证只命中自己世代）并自毒化本地租约（check
   40001，FENCED 终态）；`branch_locked` 领权位同校验（refs 读取与
   put_if_absent 之间的并发 DROP 窗口）。回归：
   `engine::fence_validate_tests`（死分支撤销+围栏 / 健康分支放行）。
4. ✅ **围栏对象回收规则成文**：GC 定案补总表两行（存活期永不回收
   ——epoch 高水位语义；DROP 分支墓碑化）+ 不变式 5（不复活：写后
   校验 / Weak 线程），回归对照行同步。
5. ✅ **Buffer 教训入典**：`docs/design/设计评审checklist.md` 第 1 条
   （时间戳不得做回收下界——EnableGC 反例 trace 摘要），另沉淀
   单一语义面/递归有界/写后校验/失败终态等 10 条。

### P2（远期，跟踪）

6. **多写者模板**：若走 P2'（quorum append），OSWALD 的
   `READY→(冲突)→CATCHUP→VALIDATE→READY` 写者端状态机是现成模板；
   若走 P3（openraft），Conflux/LogDrive 的"从共识日志派生 per-object
   日"（规格在 Vanlightly 的 `log-drive-specs` 仓库，2026-08 有配套
   博文）与 Raft 快照/日志分层互补，值得在选型时对读。
7. **订阅/读副本的 cursor 管理**：Buffer 的"版本化 cursor 寄存器 +
   双 CAS 次序敏感"分析，对 dendro 未来做变更订阅（CDC）或读副本
   追尾时的消费者围栏设计有直接参考。

## 6. 非 WAL 部分的参考意义（顺带收益）

- **两段式发布的通用形态**：写数据对象 → CAS 发布指针，在四个规格里
  反复出现（快照发布、manifest 更新、批入队）。dendro 的
  pending→prolly commit、WAL Checkpoint 帧、分支头推进全部同构——
  说明 dendro 已在用这条 S3 上的"事务原语"，可放心继续依赖。
- **状态机快照 vs 日志回放的恢复代价权衡**：SlateDB 规格明确把
  "LSM flush 换成状态机快照文件"作为建模前提，与 dendro
  "prolly 提交为恢复锚、WAL 只回放增量"的取舍一致，佐证该取舍在
  S3-first 系统中是主流。

## 7. 落地记录（2026-09-17）

| 建议项 | 产出 |
|--------|------|
| P0-1 TLA+ 规格 | `docs/research/DendroWAL.tla`（+cfg）：TLC 全过 + 双守卫否定性验证 |
| P0-2 不变量断言 | `recovery.rs` 回放末端 debug 断言 + `reopen_rebuilds_identical_state` 重开等价回归 |
| P1-3 写后校验 | `engine.rs` LeaseKeeper.alive 探针 + renew 撤销/自毒化 + acquire 窗口守卫；2 测试 |
| P1-4 回收规则成文 | `docs/design/GC定案.md` 总表 + 不变式 5 + 回归对照 |
| P1-5 评审 checklist | `docs/design/设计评审checklist.md`（10 条） |
| P2 多写者/LogDrive | 跟踪不引入（对应提交管线重构 P2'/P3 时再取） |

## 8. 信源

- 仓库：<https://github.com/Vanlightly/s3-wal-collection>（clone 实读，
  2026-09-16，main 分支）
- 各设计规格与笔记：`slatedb/SlateDBWAL.tla`（896 行）+
  `SlateDBWAL_notes.md`、`slatedb/SlateDBWAL_CAS.tla` + notes、
  `oswald/Oswald.tla` + `notes.md`、`opendata/Buffer.tla` / 
  `BufferAsWAL.tla` + 各自 notes
- OSWALD 原始实现：<https://github.com/nvartolomei/oswald>
- OpenData Buffer RFC：
  <https://github.com/opendata-oss/opendata/blob/main/buffer/rfcs/0001-stateless-buffer.md>
- Conflux/LogDrive 规格（未入本仓库）：
  <https://github.com/Vanlightly/log-drive-specs>；
  配套博文 "The LogDrive: Flexible Composition Through Abstraction in
  Shared Logs"（jack-vanlightly.com，2026-08-25）
- dendro 侧对照材料：`crates/dendro-core/src/wal.rs`（1007 行）、
  `crates/dendro-core/src/objstore/fence.rs`、
  `docs/design/提交管线重构.md`、`docs/design/GC定案.md`
