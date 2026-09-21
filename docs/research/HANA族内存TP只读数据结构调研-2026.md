# HANA 族内存 TP：只读数据结构 SOTA 调研（2026-09）

> 背景：P0 内存路径战役——memtx 从 BTreeMap 改为 delta 追加 + sorted
> 不可变主区（紧凑数组 + 哈希索引点查）。本档案核对业界形态、
> 找出差距与可借鉴点。检索日 2026-09-21。

## 一、各家的读路径数据结构

| 系统 | 写路径（热） | 读路径（冷/只读） | 版本/MVCC |
|------|--------------|-------------------|-----------|
| **SAP HANA** | L1 delta（行式、内存、不压缩）→ L2 delta（列式、字典压缩） | main store（字典压缩列存，**read-optimized 不可变**）；merge = 构建新 main，读者不阻塞；smart merge 自动权衡 merge 成本 vs 读性能 | 行级 MVCC（CSN） |
| **Hyper/Umbra（TUM）** | 原地更新 + 独立版本存储（commit-ts 版本向量）；Umbra 加 buffer manager（磁盘介质、内存性能） | Data Blocks：冷 chunk 压缩为**不可变列存块** + delta 热结构 | 原地 + 版本分离（**与 dendro 不可变底线冲突，不采纳**） |
| **LeanStore（TUM）** | 内存的 B-tree，指针 swizzling（冷页换出） | 同一 B-tree（热页全内存） | — |
| **SingleStore(MemSQL)** | 内存 rowstore：**skiplist** 索引（写吞吐优，官方自认扫描弱） | columnstore 段（不可变）+ rowstore 热层 | MVCC 时间旅行副本 |
| **SQL Server Hekaton** | Bw-tree（latch-free，页映射表间接层）+ hash 索引 | — | 行版本链 |
| **DuckDB** | ART（持久化布局 2022） | ART 约束索引 + 列存段 | — |

## 二、只读结构的关键结论

1. **"immutable sorted array + delta" 是读优化极简形态**（Brown/H-Store
   的 hybrid indexes：小 delta 收写、周期归并进不可变主索引——与
   本轮 memtx 改造完全同型，验证方向正确）。
2. **局部性 > 扇出**（ADMS'24 Kowalski 等，ART/B+/trie 实验对比）：
   读性能主导因素是访存局部性而非树扇出——**与 memres 实证一致**
   （BTreeMap 100 万键 get ~3µs 全是节点 miss；紧凑数组热 key 483ns）。
3. **ART 是读重负载标杆**（Leis et al.，695 引用）：高度 = 键长
   （O(k)，定长 8B 键高 2-3）、Node4/16/48/256 自适应小节点全 cache
   驻留，胜过 CSB+-tree 与红黑树。dendro rowid 8B 定长键下排序数组
   二分已够；**若未来键形态多样（长文本/复合），delta 索引可换 ART**。
4. **merge 不停服是 HANA 的核心工程卖点**：dendro CHECKPOINT 持
   commit_mu 阻塞提交（任务书"研究非阻塞 merge"项）——HANA 形态 =
   新 main 构建期间旧 main+delta 继续服务，构建完成原子切换。
   与 dendro 的 prolly 树快照切换（Arc 交换）天然契合。
5. **两级 delta（L1 行式→L2 列式）**：HANA 在"追加写"与"读优压缩"
   之间放了一个中间层。dendro 现状 delta（cells+哈希索引）≈ L1；
   sorted 主区 ≈ 已排序 main（但仍是行值字节）。**缺 L2 等价物：
   sorted 主区可列化**（CBF 段化解码器已有）——远期项。

## 三、2025-2026 动态

- TUM 组（Neumann/Leis）近期主线在 SQL 语义与编译（"A Critique of
  Modern SQL"）与 Umbra 演进，**存储结构无新范式**——delta+不可变
  主区仍是主流答案。
- 2025-03 arXiv：memory-optimized B+-tree latch-free 变体（并发写
  优化）——dendro memtx 单写者（commit_mu 串行）用不上。
- PermART（多版本持久 ART）：点查/更新胜 NVM 索引——NVM 线，非主线。

## 四、对 dendro 的行动对照

| 借鉴点 | 树/dendro 现状 | 行动 |
|--------|---------------|------|
| delta+不可变主区 | 本轮 P1 已落（delta 追加 + sorted 二分 + 哈希索引） | ✅ 方向与 SOTA 一致 |
| 局部性优先 | 紧凑 Cell 数组已落；**裸 get 全驻 1M 键 483ns** | ✅ 验证达成 |
| 非阻塞 merge | CHECKPOINT 持 commit_mu | 待办（任务书既有项，prolly 快照切换天然支持） |
| 键形态多样时 ART | rowid 定长键场景数组够用 | 条件触发项 |
| sorted 主区列化（L2） | 值仍为行字节 | 远期（衔接 CBF） |
| Hyper 原地更新+版本分离 | — | ❌ 违反不可变底线，不采纳 |

来源：[SAP HANA 内存管理（官方）](https://help.sap.com)、
[Sikka et al. Efficient Transaction Processing in SAP HANA (CMU 15-721)](https://15799.courses.cs.cmu.edu)、
[Data Blocks (TUM)](https://db.in.tum.de/downloads/publications/datablocks.pdf)、
[Hybrid Indexes (Brown)](https://web.eecs.umich.edu)、
[Wu et al. An Empirical Evaluation of In-Memory MVCC (VLDB'17)](https://www.vldb.org)、
[Neumann et al. Fast Serializable MVCC (TUM)](https://db.in.tum.de)、
[Freitag & Neumann Memory-Optimized MVCC (VLDB'22)](https://www-db.cs.tum.de)、
[Leis et al. ART (TUM)](https://db.in.tum.de)、
[DuckDB ART 持久化](https://duckdb.org)、
[Kowalski et al. Locality matters more than fanout (ADMS)](https://adms-conf.org)、
[latch-free B+-tree (arXiv 2025)](https://arxiv.org)、
[TUM 发表页](https://db.in.tum.de)、
[SingleStore skiplist 讨论](https://news.ycombinator.com)。
