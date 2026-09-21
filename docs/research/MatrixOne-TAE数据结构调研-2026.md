# MatrixOne TAE 数据结构调研（2026-09-22）

> 对象：ref-projects/matrixone（Go，云原生 HTAP）pkg/vm/engine/tae。
> 目的：为 dendro（memtx 紧凑布局 / WAL Group 提交 / checkpoint 全停
> 物化 / prolly 树）找可参考实现。方法：Explore agent 全量深读
> （86 次检索），本文为证据引用 + **结合 dendro 现状的落地判定**
> （非照单全收）。

## 一、TAE 总体形态（与 dendro 的拓扑对照）

**TAE 没有 skiplist，也没有自研 ART**：memtable = 追加式列向量
Batch + append-only 有序版本元数据切片（MVCCSlice）+ 外部 ART 库
（plar/go-adaptive-radix-tree v1.0.5）做 PK 索引，对象级 RWMutex
保护——与 dendro 的"sorted 数组 + delta 追加 + 哈希索引"**拓扑同构**，
佐证 dendro P1 选型方向。

## 二、六个子系统的证据与判定

### 1. memtable / 增量表
- 三层写路径：txn 私有 workspace（containers.Batch + map 索引）→
  全局可追加对象 memoryNode（`tables/mnode.go:38-46`）→ flush 后
  持久节点。删除/更新 = **独立墓碑对象**（`mnode.go:392-428`，列
  rowid/pk/commitTS/abort——"删除是往墓碑表 append"）。
- **MVCC = 区间节点有序数组**：`AppendMVCCHandle` 持
  `MVCCSlice[*AppendNode]`（`tables/updates/mvcc.go:50-64`），
  AppendNode{startRow,maxRow,TxnMVCCNode{Start,Prepare,End,Aborted}}
  （`append.go:29-37`）；同 txn 连续追加只改 maxRow（`mvcc.go:286-304`）；
  可见性 = 二分（`mvccslice.go:177-211`）；**回滚 = 留洞**
  （`append.go:240-248` 注释：删节点会令行无属主——索引 Positions
  倒序找非 aborted）。

**dendro 判定：增量小**。dendro 的 (key,ts) Cell 排序数组 +
`partition_point(ts<=snapshot)` 已实现同等 O(log n) 可见性且更简；
aborted-hole 语义 dendro 不需要（两段式提交 install 在 durable 后，
无中间可见态）。

### 2. 索引（ART/Bloom/ZM）
- ART 外部库 + **外层一把对象锁**（锁与 MVCC 共享，`tables/base.go:
  49-74`）——工业界在该吞吐量级选择粗锁+批量操作，不做树内并发。
- 查询顺序固定：**ZoneMap 先剪枝 → ART 精确**（`indexwrapper/
  mutindex.go:84-140`）。
- "Bloom" 实为 **Binary Fuse Filter**（xorfilter.BinaryFuse8，
  `index/filter.go:61-105`）：静态集合 ~9bit/键、查询 3 次访存。
- ZM 定长 64B（`index/zm.go:36-55`）。

**dendro 判定**：ART 换哈希索引的收益已被本轮 upper 缓存实测否定
（LRU/哈希占点查 <3%）；fuse filter 适用面是**静态不可变集合的
存在性测试**——dendro 树节点内容寻址（找到即对的）+ 段级已有
zone-map，增量小。**保留为远期候选**（若未来列存段需要更强剪枝）。

### 3. WAL（logstore）
- 分层：StoreImpl（LSN 分配 + SafeQueue 流水线）→ 可插拔 driver。
- **SafeQueue**（`logstore/sm/safeq.go:32-104`）：chan + 消费者一次
  drain 到 batchSize(10000) 再回调——组提交基础设施。
- V1 本地文件：5 级流水线，**每批一次 fsync**（MaxBatchSize=500,
  SyncDuration=2ms）；帧格式 = 一个 record 打包一批 entries +
  meta.addr 偏移索引（`logservicedriver/v1.go:84-114`）。
- **checkpoint entry 自身先写 WAL、durable 后才 truncate**
  （`logstore/wal/checkpoint.go:54-173`）——截断意图先落盘的顺序保证。

**dendro 判定**：内存攒批 + Group + 自主提交已有等价；"checkpoint
意图先落盘再截断"的顺序与 dendro WAL ck 帧一致（已有）；V1 的
"每批一次 fsync"是 LocalObjStore append_at+fdatasync 现状。
**增量小，方法论互证。**

### 4. checkpoint（⭐ dendro 最有价值参考）
- **非全停，两级**：ICKP（增量：时间 5min + 事务数 10000 双阈值，
  只收集元数据差量）+ GCKP（每 40 个 ICKP 全局合并）。
  官方 Q&A（`db/checkpoint/runner.go:118-196`）："1. 选 checkpoint
  ts → 2. 等此前事务全部提交 → **3. 等此前脏块全部 flush** → …
  全程不停写入"。
- **事务化 flush**：`fireFlushTableTail`（`flusher.go:711-800`）以
  400 对象为批调度 flush 任务；`PrepareCompact`（`tables/aobj.go:
  111-149`）要求 freeze + 全部 append 已提交 + refcount==0，失败
  重试（ErrTxnNeedRetry）；**freeze 后新写入走新对象**
  （freezelock，`aobj.go:68-74`）；flush 期间收到的删除用
  **transfer map** 按 rowid 重定向（`txnentries/flushTableTail.go:
  199-330`）。`tables/jobs/flushTableTail.go:96-135` 注释完整记录
  flush 起始 ts 与 append prepare 的竞态设计——**照读**。

**dendro 判定：⭐⭐ 这是对 dendro 残余最大瓶颈（commit_mu 全停
~200ms/次物化）的成熟解法蓝图**。dendro 平移形态（待立项）：
物化做成事务——冻结 delta 区（新写进新区，读写分流）→ 后台把旧区
并入 prolly 树（chunker 攒批已就位）→ 期间删除/更新进新 delta →
完成后原子换 head/发布新根。 prolly 内容寻址天然支持"构建期间旧树
照常服务、新树原子切换"（Arc swap）——比 MO 的 transfer map 更简。
需 TLA+（DendroMerge.tla 扩展）+ owner 评审（架构级）。

### 5. 容器与 6. 归并（简记）
- 容器：定长槽 + area 堆 + roaring null 位图，变长 ≤23B 内联——
  与 Arrow 同构；**版本元数据即列**（commitTS/abort 当普通列，
  `mnode.go:171-208`）。
- 归并：手写小顶堆 k 路 + 堆推进时跳墓碑；**transferSlab**（连续
  内存 + 池化 + checkpoint 后 drain）；两阶段（各段并行排序→串行
  归并）。dendro 的 merge_shard/流式三路归并已有等价；transferSlab
  若做非阻塞 checkpoint 时可参考。

## 三、行动对照表

| MO 参考 | dendro 现状 | 行动 |
|---|---|---|
| MVCCSlice/AppendNode/回滚留洞 | (key,ts) Cell + partition_point | ✅ 已有等价（更简） |
| ART + 外层锁 | 哈希索引 + upper 直存 | ✅ 方向互证；不换 |
| Binary Fuse Filter | 段 zone-map | 远期候选 |
| SafeQueue 组提交 | 内存 buf + Group + 自主提交 | ✅ 已有等价 |
| **非阻塞 checkpoint（freeze+transfer）** | **commit_mu 全停物化** | ⭐ **立项评审**（下一战役设计输入） |
| checkpoint entry 先落盘 | WAL ck 帧 | ✅ 已一致 |
| 手写堆归并/transferSlab | merge_shard/三路归并 | 做非阻塞 cp 时参考 |

## 四、结论

TAE 调研的**净增量 = 非阻塞 checkpoint 蓝图**（freeze + 等flush +
元数据原子切换，MO 把竞态坑注释在源码里）；其余子系统与 dendro
现状互证同构（含 dendro 自研的紧凑布局/组提交/归并——独立收敛到
同一组解，双向佐证）。下一步若立项非阻塞 checkpoint，先读
`tables/jobs/flushTableTail.go:96-135` 与 `runner.go:118-196`，
再写 TLA+ 扩展。
