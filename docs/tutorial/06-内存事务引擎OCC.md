# 06 · 内存事务引擎（OCC MVCC）

> 代码：`crates/dendro-core/src/memtx.rs`、`engine.rs::commit_tx`
> 定位：HANA 意义上的 delta 存储——**未 checkpoint 数据的权威**，内存有界。

## 1. 并发模型一句话

> **并发单位是分支，不是行锁。** 分支内：单写者语义 + OCC 验证；
> 分支间：完全并行；读：全库无锁快照读。

这免掉了死锁检测/等待图的整类复杂度，代价是同分支热点 key 的高频并发写
会在 commit 点串行化——Agent 场景的解法本来就是"一人一分支"。

## 2. 内存布局

```
TableMem（每表，分支内活跃数据）
 │
 ├─ shard[0..64)                     ← key 的 xxh3_64 低 6 位选片
 │    └─ RwLock<HashMap<key, Arc<VersionVec>>>
 │
VersionVec = Vec<Arc<VerCell>>       ← 按提交序（ts 升序）
VerCell = { ts: u64, val: Option<Arc<Vec<u8>>> }
                          └ None = tombstone（删除标记）
```

读路径的字节级行为（全程零锁写、一次读锁）：

```
get(key, snapshot):
  lock.read() → 拿 Arc<VersionVec> 副本 → 解锁
  partition_point(|c| c.ts <= snapshot)   ← 二分找第一个 > snapshot 的位置
  idx-1 即"快照可见的最新版本"
```

```
k 的版本链示例（ts=提交序号）：
  [ts=5 "a"] → [ts=8 "b"] → [ts=12 tomb]

  get(k, 4)  → None        (尚不存在)
  get(k, 5)  → "a"
  get(k, 11) → "b"
  get(k, 12) → None        (tombstone 生效)
  get(k, 99) → None
```

**为什么保留整个 VersionVec 而不是只留最新**：快照隔离要求"读旧快照时看到
旧值"。版本会在 checkpoint 时被截断（§4），所以链长有界 ≈ checkpoint 窗口内的
写频率。性能红线：`VerCell` 不可变、Arc 共享、无 Rc/RefCell（SPEC 00 §6）。

## 3. OCC 提交协议（first-committer-wins）

会话事务只是个本地结构：`Txn { snapshot, writes: BTreeMap<(table,key), Mutation> }`。
所有读走快照（memtx ∪ 树），写缓存在写集。COMMIT 时：

```
commit_tx(db, branch, txn)：                    ← 分支 commit_mu 串行化

 seq = branch.alloc_seq()                       ① 取号（分支内单调）

 验证（②③在 commit_mu 保护下原子）：
   for key in 写集:
     latest = 分片里该 key 的最新版本 ts
     if latest > txn.snapshot:                  ② 别人已抢先提交
         return 40001 serialization_failure     （first-committer-wins）

 安装：
   for (key, Put v) in 写集: shard.install(key, ts=seq, Some(v))
   for (key, Delete) in 写集: shard.install(key, ts=seq, None)   ③

 pending 登记（checkpoint 的原料）+ WAL 帧 TXN(seq) ──▶ 组提交（05 章）

 watermark.store(seq)                           ④ 对后续读者可见
 返回客户端：durability=Group 时等到段 durable；NoWait 时立即
```

冲突时序实例：

```
T1(快照=10): UPDATE k=1 → 'A'          T2(快照=10): UPDATE k=1 → 'B'
      │                                      │
      ├─ commit: 链头 ts=10 ≤ 10 ✓           │
      │  install ts=11, WAL, ACK ✓           │
      │                                      ├─ commit: 链头 ts=11 > 10 ✗
      │                                      └─ ERROR 40001（客户端应重试或走分支）
```

> 诚实标注：这套验证等价于 **快照隔离（Snapshot Isolation）**——写写冲突
> first-committer-wins。`SHOW transaction_isolation` 如实报 read committed
> （隐式快照按语句刷新）。SSI 的读写集全验证是 v2。

## 4. checkpoint——内存有界的机关

```
checkpoint_locked(branch)：                     持 commit_mu

 pending = take(branch.pending)                 ① 取走增量（此后新写不受影响）
 对每张表: chunker.apply(树根, pending)          ② prolly 增量写（04 章）
 新 catalog + commit 对象 + chunks 批量上传      ③ 版本层落盘（08 章）
 WAL CHECKPOINT 帧（seq_covered=水位）+ flush    ④ 存证
 manifest CAS：refs[b].commit/…covered_seq      ⑤ 发布（02 章 §3.1）
 mem.truncate_all(covered_seq)                  ⑥ 释放 ts≤covered 的版本
```

第 ⑥ 步就是内存有界的原因：**memtx 常驻量 ≈ 两次 checkpoint 之间的写入量**
（默认阈值 16MB 或 30s）。截断语义：

```
某 key 的版本链 [ts5, ts8, ts12]，checkpoint covered=10：
  最新 ts=12 > 10 → 保留 [ts12]，丢弃 [ts5, ts8]     （老快照读不到了）
某 key 的版本链 [ts5, ts8]，covered=10：
  全部 ≤ 10 → 整键移除（权威已转移给树）
```

checkpoint 之后读路径自动改走树：`读 = memtx(未物化增量) ∪ prolly 树(已物化)`
的键序归并（`scan.rs::table_scan`），两路都是有序流，一次线性归并。

## 5. 与树/WAL 的一致性责任划分

| 状态 | 权威 | 崩溃后果 | 恢复动作 |
|------|------|----------|----------|
| 已 checkpoint | prolly 树（OSS）| 无损 | 直接加载索引 |
| checkpoint 后写入 | WAL 段（OSS）| Group 模式无损 | 回放重建 memtx+pending |
| 未 flush 的 WAL 缓冲 | 仅内存 | NoWait 丢、Group 无已 ACK 丢失 | —— |

三者衔接点就是 `covered_seq`：恢复回放跳过 `seq ≤ covered` 的帧，
既不丢也不重。

## 附录：v1.1 语义增补（与 SPEC 04 同步）

- **冻结读**：显式事务以 BEGIN 时的 catalog 根读树（不随 checkpoint 翻转）。
- **截断保护**：活跃显式事务会阻止 checkpoint 截断 memtx 版本（`Branch.active_snaps` 引用计数）。
- **跨 checkpoint 的写事务**：提交时显式 40001（冲突检测盲区，重试即可）。
- **读自己的写**：事务内 SELECT 能看到本事务的 INSERT/UPDATE/DELETE（table_scan / 点查 / AP 三路径同一归并抽象）。
- **事务内分支语句**：USE/CHECKPOINT/CREATE|DROP|MERGE|REOPEN BRANCH → 25001。
