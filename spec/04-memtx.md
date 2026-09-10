# SPEC 04 — 内存事务引擎 (memtx)：OCC MVCC + 分支级并行

状态：**定稿 v1**（HANA delta 概念 + Hekaton OCC + git 单写者语义的融合）

## 1. 定位与取舍

云原生 + 分支化改变了"最大并发"的定义：

> **并发单位是分支，不是行锁。**

- 每个分支内：单写者语义（写事务在 commit 点乐观验证，first-committer-wins）
- 跨分支：完全并行（Agent 一人一分支），merge 时才见分晓
- 读：全库无锁快照读（不可变版本 + 原子指针）

这让我们免去分布式行锁/死锁检测的整类问题，同时保留 Hekaton 式内存 OLTP 吞吐。
代价：同分支高频并发写同一 key 的吞吐受 commit 验证串行化限制——这正是
Agent 场景该用多分支而不是疯狂单点更新的原因（设计立场写进文档）。

## 2. 内存数据布局（性能红线执行处）

```
TableMem (每表，每分支活跃集)
  shards: [TableShard; 64]        // 按 pk hash 分片，独占锁域
TableShard
  map: RawTable<Arc<VerNode>>     // hashbrown raw，open addressing，无每节点 alloc
  lock: parking_lot::RwLock       // 写者短暂持锁改头指针；读者走 version 数组

VerNode (不可变，一旦发布永不变)
  key:  KeyBytes (内联小键，堆大键)
  head: AtomicPtr<VerCell>        // 版本链头
VerCell (池化分配)
  ts:   u64          // commit_ts(=seq)；0 表示 deleted(tombstone)
  val:  RowData      // 行编码字节(复用 SPEC 03 §5)；池化 buffer
  next: *mut VerCell // 版本链(新→旧)
```

- 读者：`head` 原子读 → 沿链找 `ts <= snapshot_ts` 的首个版本 → 拿到不可变 RowData
  **全程零锁零原子写**，只有一个 Acquire load
- 写者：本地写集缓冲；commit 时分 shard 短锁 CAS 安装新 VerCell 到链头
- **无 Rc/RefCell**：版本链是 `*mut VerCell` + epoch 回收（crossbeam-epoch 或
  自实现 epoch：全局 AtomicUsize epoch + 每 reader 注册 slot，延迟两代释放）
  —— 自实现优先（可控、无 trait 魔法），API 只在 memtx 内部暴露
- **内存池**：`VerCell` 与行 buffer 从 per-shard arena 分配（`bumpalo` 风格自研：
  定长 class 池 64B/128B/256B/…/4KiB + 大对象直分配），释放回池不还 OS

## 3. OCC 协议（Hekaton 式简化）

事务生命周期：
```
BEGIN (隐式自动提交或显式 BEGIN)
  snapshot_ts = 当前提交水位 committed_watermark()
读: 走 §2 快照读；读到的 (key,ts) 记入读集(仅显式事务记录；自动提交单语句免记)
写: 本地写集 Vec<(table_id, key, row_bytes|Delete)>；同 key 后写覆盖前写
COMMIT:
  1. 获取分支 commit 序号 c = watermark+1 (fetch_max 原子申请，失败=并发提交，重试取号)
  2. 验证: 对写集涉及的分片短锁内——
     对每个读集项: 版本链头 ts 是否 == 读时 ts（不等 = 别人提交过 ⇒ serializable 冲突）
     对每个写集 key: 链头 ts 是否 > snapshot_ts（是 ⇒ 写写冲突）
     冲突 → ROLLBACK(错误 40001 serialization_failure)
  3. 安装: 各分片 CAS 新 VerCell(ts=c)
  4. WAL 帧(组提交, SPEC 02)；durable 后推进 committed_watermark
     （安装先于 durable ⇒ 读者可能见到未持久事务，恢复时以 WAL 为准重建，
       服务端重启即消失 ⇒ 提交 API 仍等 durable 才返回客户端 OK，故对外 ACID 完整）
ROLLBACK: 丢弃写集
```

### 3.1 两段式提交（P2-6 组提交解耦，实现口径）

```
Pass1 (持 commit_mu):
  fence → Q-9 → OCC 裁决（memtx + in-flight 写集求交）→ seq 分配
  → WAL 入队（enqueue_only，无 durable 等待）→ 注册 in-flight{ts → 写集}
durable 等待（锁外）: 并发提交的帧并入同一组刷盘（组大小 = 并发度）
Pass2 (持 commit_mu):
  memtx 安装（VerVec 按 ts **有序插入**——pass2 完成序 ≠ ts 序）
  → pending 登记 → 摘除自身 in-flight
  → watermark = min(installed_max, min(in-flight)−1)   ← 无间隙前沿
```

- **无间隙前沿**：watermark 只推进到"全部更低 ts 均已安装"的位置——
  否则并发读者快照跳过未安装版本，事务内可见性翻转（重复读违约）。
  installed_max 与 min(in-flight) 两个分量都必须持久跟踪（只取自身 ts 会在
  小 ts 后完成时把水位永久压低——回归 `two_pass_repeatable_read_gap_free_watermark`）。
- **裁决有效性**：validate 与 install 之间插入的并发提交都在 in-flight
  注册表内（Pass1 求交 40001）——等待移出锁外不弱化 first-committer-wins。
- **失败语义**：等待失败（毒化 40003）→ InflightGuard 摘除注册 + 错误上抛；
  同帧组内他者成功 = 既有 Uncertain 对账口径（SPEC 02 §3.5，按**段**为不确定域）。
- **段退休安全界**（审计 R3-P0）：checkpoint 的 `wal_first_seg` 推进与段墓碑
  以 `retire_bound(covered)` 为界——最高"段内最大帧 ts ≤ covered"段。旧单段
  设计下"等待+安装同锁"保证 checkpoint 时无 durable-but-uninstalled 帧；
  两段式下必须显式守卫。回归：`wal_corruption::segment_retirement_bounded_by_covered_frontier`。
- **效果**：8 并发写者组提交吞吐 20 → 160 commits/s（8×，线性于并发度）。

- 自动提交（默认）：每语句一事务，验证成本 O(写集)，无读集记账
- 隔离级别：提供 `READ COMMITTED`(每语句新快照) 与 `SERIALIZABLE`(事务快照+验证，
  默认 SERIALIZABLE，名字诚实，因为验证确实是 SSI 的简化 OCC 版)
- 死锁：不存在（无等待锁）

**v1.1 语义增补（第十一/十二轮评审落地，实现为准）**：

- **冻结读**：显式事务内树的可见性以 BEGIN 冻结的 catalog 根为准
  （`Txn.head_root`），memtx overlay 按快照读；写路径仍按当前 head。
- **活跃快照与截断**：显式事务快照注册入 `Branch.active_snaps`
  （引用计数）；存在活跃快照时 checkpoint **跳过 memtx 截断**（冻结读
  依赖其保留的版本），否则截断到 covered。
- **Q-9：事务跨越 checkpoint ⇒ 提交显式 40001**。截断发生后该事务的
  写写冲突检测存在盲区（memtx 版本链被截、树只有最新态），静默
  last-writer-wins 会丢更新——covered_min 只在真截断时推进，显式事务
  快照低于它即拒绝（客户端重试即获得完整视图）。v2 增强：validate
  回退树版本链后可放开此限制。
- **读自己的写**：会话显式事务写集作为读归并的最后覆盖层（table_scan
  / 点查 / AP 列存三路径同一抽象：树 → memtx overlay → 会话事务写）。
- **事务内分支语句**：USE BRANCH / CHECKPOINT / CREATE|DROP|MERGE|REOPEN
  BRANCH → 25001（USE 切换破坏冻结读；CHECKPOINT 推进截断水位自伤；
  catalog 写不可回滚——Q-10 事务化前保守口径）。SHOW BRANCHES 只读放行。
- **事务内 DDL**（Q-10）：CREATE TABLE / CREATE INDEX / ALTER TABLE /
  DROP / TRUNCATE → 25001（catalog 写不经事务写集，立即生效且 ROLLBACK
  不可撤销——R18-2 实证可见性漂移）。DML 不受影响。
- **游标与事务解耦**（Q-1b/R18-5 口径）：游标为 INSENSITIVE 物化——
  DECLARE 时结果集快照，与事务生命周期解耦是有意行为（游标只读，
  写侧隔离不受影响）。会话级游标 map 有上界护栏（Q-1）。
- **读自己的写**：会话显式事务写集作为读归并最后覆盖层（table_scan /
  点查 / AP 三路径同一抽象）。

## 4. 与版本层/WAL 的关系

- memtx 是**未 checkpoint 数据的权威**（内存态）；checkpoint 后数据权威转移到
  prolly 树（OSS）。checkpoint 把 (start_seq, end_seq] 的写集增量应用到树上后，
  memtx 中 ts ≤ end_seq 的版本可释放（读走树路径），内存占用有界
  （≈ checkpoint 间隔内的写入量）——这就是 HANA "delta(内存) + main(列存/落盘)" 的对应物
- 读路径合并：`memtx(snapshot) ∪ prolly_tree(snapshot)`，memtx 优先（新）
- 崩溃恢复 = WAL 重放重建 memtx（SPEC 02 §4）+ 树已物化部分直接加载索引

## 5. DDL 与 catalog

- catalog（表目录/schemas）在版本层（SPEC 03 §4 表目录 map）；memtx 内缓存
  `ArcSwap<CatalogSnapshot>`，DDL = 提交一个特殊事务（写 catalog map），
  读者无锁换快照
- DDL 与 DML 冲突：同分支 DDL 串行（commit CAS 自然串行化）

## 6. 与实现的映射

| SPEC 条目 | 代码 |
|-----------|------|
| 分片版本存储 | `dendro-core/src/memtx/shard.rs` |
| VerCell 池/arena | `dendro-core/src/memtx/pool.rs` |
| epoch 回收 | `dendro-core/src/memtx/epoch.rs` |
| OCC 管理器 | `dendro-core/src/memtx/txn.rs` |
| catalog 快照 | `dendro-core/src/catalog.rs` |
