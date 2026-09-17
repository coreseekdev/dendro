# Lance / LanceDB 实现调研（2026-09）

> 调研时间：2026-09-18。触发：用户指定深读 Lance/LanceDB 可借鉴实现
> （本地 clone 实读）。结论先行：
>
> 1. **仓库已拆分**（重要情报）：lancedb/lancedb 只剩嵌入 DB 薄层与
>    多语言绑定；v13 起 25 个核心 crate 迁至独立仓库
>    **lance-format/lance**（版本 13.0.0-beta.4，Apache-2.0）。深读
>    对象是后者。
> 2. **Lance 与 dendro 在"不可变数据 + 版本化 manifest + 墓碑删除 +
>    LIST 权威读"上同构**，且在三个 dendro 已定案的点上给出了同一
>    答案：LIST 为恢复权威路径（dendro P1-F 定案）、不确定错误读回
>    消解（dendro manifest putid 同构）、"可能不可见的 manifest 所
>    引用的文件不得删"保守面（dendro retention 窗口语义）。
> 3. **Lance 在 S3 提交上反而落后于 dendro**：无原生条件写时的默认
>    是 **UnsafeCommitHandler（不做任何检查直接写）**——诚实但危险
>    的默认；可选 DynamoDB 外部真源。dendro 的 put_if_absent CAS +
>    epoch fencing 是更强的默认。这印证 dendro 提交协议的定位。
> 4. **四项高价值可借鉴**：①事务 Operation 枚举携带**删除谓词**做
>    读写冲突检测；②compaction 时的**索引 remap 协议**（RowAddrRemap
>    ——向量索引与不可变段共存的核心难题，dendro 未来向量索引的
>    关键先例）；③Fragment 的 **overlay 层**（cell 级更新不重写
>    base 文件的 LSM-lite）；④CommitHandler 的两个保守性**契约位**
>    （not-found 是否终局 / 错误后读回成功是否仍报错）。

## 1. 版本与提交协议（lance-table/src/io/commit.rs）

事务提交 = 写下一个编号 manifest（`_versions/N.manifest`）。并发写者
竞争由 `CommitHandler` trait 策略族解决：

| 策略 | 机制 | 适用 |
|------|------|------|
| RenameCommitHandler | 临时路径 + `rename_if_not_exists` | 多数对象存储（原子）——**S3 不支持** |
| **UnsafeCommitHandler** | 直接写最终路径，**无任何检查** | **AWS S3 的默认**（文档诚实声明） |
| ExternalManifestCommitHandler | DynamoDB 做外部位点真源 | 需要严格多写者的部署 |
| CommitLock | 更简单的锁语义替代 | 自定义 |

两个值得抄的**契约位**（trait 默认值即设计表态）：
- `is_version_not_found_definitive()` 默认 false——事件一致/外部真源
  下，"not found"不代表提交失败，**调用方不得立刻删除新 manifest 可能
  引用的文件**（与 dendro retention 窗口防"GC 删掉滞后读者引用"同一
  保守面）；
- `propagate_commit_error_after_success()` 默认 true——错误后读回
  验证 manifest 已落地时，自定义 handler 仍保留错误（内置对象存储
  handler 覆写为不报，因为其错误多为"结果未知的传输失败"）——与
  dendro manifest putid 内嵌反查消解同构，但**做成了可配置契约**。

版本解析：LIST 为权威路径（带 hint 优化：`list_manifests_since_version
_with_hint` 失败才全量 LIST 倒序）——与 dendro P1-F"LIST 为权威、
探测有洞歧义"定案一致。另有 V1→V2 manifest 命名迁移的兼容层。

## 2. 数据集模型（lance-table/src/format/）

**Manifest**：schema / version / **branch（数据集级浅分支）** /
fragments（按 id 排序、允许空洞）/ writer_version / 读写 feature
flags / max_fragment_id / 事务文件路径 / tag。

**Fragment**（≈ dendro 列存段，但更新面更细）：
- `files`：数据文件（v2 编码，多文件可并存）；
- `deletion_file`：已删**局部行偏移**文件（Array / Bitmap 两型）——
  不可变文件上的删除层（≈ dendro 墓碑，但粒度是段内行偏移而非 key）；
- `overlays`：**cell 级覆盖层**——不改 base 文件补充新值，顺序即新旧
  （LSM-lite：读时按序解析）；
- `row_id_meta` + FLAG_STABLE_ROW_IDS：稳定行 id（索引/流式消费锚点）；
- `physical_rows`（legacy 可空）。

**事务 Operation 枚举**（transaction/operation.rs）：Append /
Delete{updated_fragments, deleted_fragment_ids, **predicate**} /
Overwrite{fragments, schema, config} / Merge…。两个亮点：
- **Delete 携带谓词原文**进事务元数据——冲突检测器可用"谓词相交性"
  判读写冲突（而非只看写集 key 相交）；
- Overwrite 的 id 铸造规则成文（"铸 id 与携带 deletion file 互斥"——
  deletion 路径内嵌 fragment id，不能跟着换 id）。

## 3. 编码层（lance-encoding）

- `FieldEncodingStrategy` 按字段建编码器树：结构层（struct/list/
  fixed_size_list 的 validity+offsets）+ 物理层
  （bitpacking/fsst/byte_stream_split/constant/rle/packed/value/
  binary/block）；
- 页缓冲最小对齐 8B（对照 dendro CBF 的 **64B GPU 对齐**——dendro
  的选择更强，面向 GPU/SIMD 直读）；
- miniblock vs full-zip 两种页形态；protobuf 描述编码树
  （`pb::ArrayEncoding` 语法树——计划方言的编码版）；
- 有策略 trait 但**未见 pgrc2 式选举纪律**（≥10% 门槛/roundtrip
  验证/降级落点）—— dendro 若落《pgrust列存调研》P0，纪律面可超
  越 Lance。

## 4. 向量索引（lance-index/src/vector/）

全家族：ivf（**v3 新架构**：shuffler + subindex，训练数据洗牌分布式
化）/ graph（stage-wise HNSW 构建与 I/O）/ pq / sq / **bq（二值量化 +
rotation + 距离表量化——RaBitQ 谱系）** / flat / residual / kmeans /
transform；查询参数自适应（minimum/maximum_nprobes）。

**对 dendro 最关键的先例：索引 remap 协议**。compaction 重写 fragment
后行地址全变，索引不重建而是 `remap_index_file(RowAddrRemap)`——
按映射表改写索引文件中的行地址。这是"不可变段 + 辅助索引共存"的
通用解（代价敏感：映射稀疏时重写贵，触发阈值成调参点）。dendro 未来
向量索引（见《向量搜索库调研》P2 路线）与 prolly 段的共存可直接
借用该模式：**段重写 ≠ 索引重建，只做地址 remap**。

## 5. GC 与 I/O

- `cleanup_old_versions(before_time)`：版本时间窗清理；提交时记录
  deletion vector（本事务删的文件集）供 GC 消费——与 dendro 墓碑
  同构但**按事务粒度**（dendro 按 manifest 版本原子登记，更强）。
- lance-io：调度器（SPSC I/O 循环 + 背压水位，5s 后才告警防刷屏）+
  lite 变体 + **io_uring 模块**（current_thread/thread 两型 reader 与
  future 封装——与 zvec v0.7 的 io_uring 选型互相印证）+ 本地页缓存 +
  `CachedFileSize`（避免 stat 往返）。

## 6. 与 dendro 的对照

| 维度 | Lance v13 | dendro | 评 |
|------|-----------|--------|-----|
| S3 提交 | Rename / **Unsafe 默认** / DynamoDB 外源 | put_if_absent CAS + putid 消解 | **dendro 更强**（Lance 的 Unsafe 默认是行业痛点实况） |
| 版本解析 | LIST 权威 + hint | LIST 权威（P1-F 定案） | 同一答案，双方互证 |
| 删除 | 段内行偏移 deletion file | memtx/ prolly 层 tombstone key | 粒度与层次不同；Lance 面向"不可变文件内删行"，dendro 面向 MVCC |
| 更新 | overlay 层（cell 级） | memtx + checkpoint 全量重建 | Lance 免重写面更细；dendro 模型更简 |
| 分支 | manifest 内 branch/tag（浅） | **真分支**（prolly 内容寻址 + epoch） | dendro 强项 |
| 冲突检测 | Operation 谓词 + conflict resolver | OCC 写集（memtx key） | Lance 的谓词相交检测值得借 |
| 编码 | 策略树 + FSST 族 | CBF 六 codec + 64B GPU 对齐 | 对齐面 dendro 强；纪律面两边都缺（P0 机会） |
| 向量 | IVF v3 + graph + bq 全家 + **remap 协议** | 无 | 主要差距与主要借鉴源 |
| I/O | io_uring + 调度器背压 | 同步 + 记忆中 S3 直读 | 借鉴面（与机会式派发 P1 合流） |

## 7. 可借鉴点清单

### P0（小切口，直接落）

1. **谓词进事务元数据**：dendro 的 UPDATE/DELETE 事务把谓词规范化
   文本记入事务记录——冲突检测从"写集相交"扩展到"谓词相交"
   （`DELETE WHERE x>5` 与并发 `INSERT x=7` 的读写冲突今天在
   OCC 快照下如何判？）。先做**语义审计**：确认现状行为并成文，
   再决定是否引入谓词冲突位。
2. **CommitHandler 式契约位自查**：对照 dendro 的 manifest 提交，
   把两问写成注释/文档合同——"什么条件下 NotFound 是终局"、
   "错误后读回成功是否仍报错"（dendro putid 已隐式回答；显式成文
   防回归）。顺带审计 S3 层条件写探测（put_if_absent 在不支持
   conditional-put 的存储上的降级行为是否诚实——Lance 的 Unsafe
   教训）。

### P1（设计先例，待立项引用）

3. **索引 remap 协议**：作为未来向量索引/二级索引与段 compaction
   共存的设计先例记入《向量搜索库调研》P2 的实现方案段
   （RowAddrRemap 模式：映射表 + 索引文件地址改写，而非重建）。
4. **overlay 层**：CBF 段若未来需要免重写的局部更新（大段 + 小
   UPDATE），Fragment.overlay 的"顺序即新旧、读时解析"是现成形态；
   与 dendro"checkpoint 全量重建"的取舍（重建简单性 vs overlay
   读放大）在列存专项里对表。
5. **deletion vector 按事务记录**：dendro 墓碑当前在 manifest 版本
   内原子登记（更强，保留）；借鉴的是 Lance 把"本事务删除集"与
   "版本清理"分离的**运维面**（cleanup_old_versions 的 before_time
   API 形态，dendro gc-retention 的用户面可对齐）。

### P2（跟踪）

- IVF v3 的 shuffler/subindex（分布式索引训练）——P2' 多节点时取；
- io_uring（与 zvec v0.7 互证，S3 客户端层收益存疑，本地盘/缓存层
  先行）；
- DynamoDB 外部真源（dendro 单写者 fencing 已覆盖其需求场景）。

## 8. 信源

- <https://github.com/lancedb/lancedb>（clone 实读：workspace 拆分
  事实、rust/lancedb 薄层）
- <https://github.com/lance-format/lance>（clone 实读，v13.0.0-beta.4）：
  `rust/lance-table/src/io/commit.rs`（CommitHandler 策略族与契约位）、
  `format/{manifest,fragment,transaction}.rs`、
  `transaction/operation.rs`（Operation 枚举）、
  `lance-encoding/src/encoder.rs` 与 `encodings/`、
  `lance-index/src/vector/`（ivf v3/graph/bq/remap）、
  `lance-io/src/{scheduler.rs,uring/}`、
  `lance/src/dataset.rs`（cleanup_old_versions）
- dendro 侧对照：`docs/design/GC定案.md`（墓碑/retention）、
  `crates/dendro-core/src/objstore/manifest.rs`（编号 manifest +
  putid）、`docs/research/pgrust列存与向量索引调研.md`（编码选举
  P0）、`docs/research/向量搜索库调研.md`（向量路线 P2）、
  `docs/research/S3_WAL设计调研.md`（LIST 权威/保守面谱系）
