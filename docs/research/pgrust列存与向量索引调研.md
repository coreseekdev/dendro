# pgrust 列存与向量索引调研（2026）

> 调研时间：2026-09-18。触发：用户问询 pgrust 是否有向量索引（搜索）
> 与列存实现。结论先行：
>
> 1. **两者都有，且列存有两代**。列存 v1（`pgrcolumnar`）：scan-only
>    表 AM，mmap 读 + 直写 pwrite，zone map / bloom / HLL / 字典 / LZ4
>    ——ClickBench"比 ClickHouse 快 18.5%"数字的载体。列存 v2
>    （`pgrcolumnar2`，13 个子 crate 重设计）：不可变 sealed part +
>    **堆 delta store**（涓流 DML 行侧落地、删除走 tombstone 关系、
>    可见性直接复用堆 MVCC）+ 世代化 manifest 崩溃安全发布 +
>    **分析后编码选举**（确定性选举/≥10% 胜出门槛/编码时 round-trip
>    验证/拒绝降级 VERBATIM）+ 出生即并行原生扫描（claim 面）。
> 2. **向量 = pgvector 0.8.5 的忠实移植**：vector 类型（f32）+ 全套
>    距离/算术函数 + **HNSW 索引 AM**（内存 bump arena 建图 →
>    maintenance_work_mem 刷盘 → 逐元组在盘插入；ef_search 等 GUC
>    同语义）。**未移植**：ivfflat、halfvec、sparsevec、bit 量化
>    opclass。
> 3. **关键否定性发现：HNSW 与列存互不打通**——pgrcolumnar2 类型化
>    拒绝 TID 扫描/位图扫描（exactness 姿态 C12："干净报错，绝不给
>    非法 TID"），而 HNSW 正是产出 TID 的索引 AM。pgrust 的两个
>    旗舰能力（向量搜索、列存）**不能组合使用**，向量搜索只在堆表上。
> 4. 对 dendro 最有价值的单项借鉴：**"输入可判定的编码选举 ⇒ 字节
>    确定性 part"法则**——这正是 dendro 内容寻址列存段（哈希即身份）
>    的先决条件（P0 建议）。

## 1. 列存 v1：pgrcolumnar（scan-only，ClickBench 载体）

| 维度 | 设计 |
|------|------|
| 定位 | 分析专章的只扫描表 AM（"scan-only columnar table AM for the analytics charter"） |
| I/O | **绕缓冲池**（mmap 读 part 文件；写 = 直接 pwrite + fsync；主 fork 存在性仍归普通 smgr） |
| 剪枝 | zone map（`ZoneQual`/`ZoneCmp`）+ **bloom** + **HLL**（`dict_frame_stats` 字典帧统计） |
| 压缩 | LZ4 + 字典 |
| 写入 | 并行灌入（`begin_parallel_ingest`/`ParallelIngestPlan`）、排序键探查（`ColOrderProbe`/`loadsort`/`sortkey`——按排序聚簇改善剪枝） |
| 配套 | `CbScanDescData` 的元数据聚合扫描（`MetaAggScan`——zone/bloom 命中统计）、part 缓存 |

## 2. 列存 v2：pgrcolumnar2（重设计，工程化程度极高）

13 个子 crate：`format`（冻结字节格式）/`am`（表 AM 胶水）/`write`/
`scan`/`read`/`delta`/`codec`/`batch`/`claim`/`meta`/`ingest_par`/
`qa`/格式内嵌模块。治理方式值得注意：**字节级格式冻结在
`pgrc2-format.md`，设计裁决（O-2/O-3/O-8…）编号入册"不再重审"**——
任何修改是对两份文档的 A 巷道 PR（与 dendro 的"定案/审视报告"文化
同构，但把裁决编号化到了字节级）。

### 2.1 存储模型：sealed part + 堆 delta + tombstone

- sealed part **不可变**；COPY/CTAS/matview 走批量写入口，发布点 =
  `finish_bulk_insert`；M3 期涓流 DML **类型化拒绝**（干净 0A000，
  M5 起 delta store 接管）；
- **delta store**（M5-B）：涓流 INSERT 进堆 delta 关系（同表列
  schema）；删除 sealed 行 = 向**堆 tombstone 关系**追加一条 int8
  RowId；delta 居留行的删除 = 普通堆删除（**LAW：delta 行永不写
  tombstone**，位图/墓碑两侧对 delta-tagged RowId 类型化拒绝）；
  UPDATE sealed 行 = tombstone + delta-insert（同事务、顺序钉死
  tombstone-first）；
- **删除可见性 = 堆 MVCC on tombstone 行**——子事务/组合 cid/EPQ
  "免费且可证 C-精确"（O-2 裁决的设计要点）；扫描一次性构建
  per-part 可见墓碑位图，相交进批选择；
- **发布**：世代化 manifest（append-only 状态），可见性判据 =
  `XidInMVCC`；单节点串行化 = 进程内每表发布互斥（pgrust 线程/
  进程模型下充分）；崩溃残留由**首次开目录时的 recover_and_clean**
  回收（"崩溃后纯 SELECT 绝不因 ManifestMissing 拒扫健康表"）。

### 2.2 编码选举（"analyze-then-elect"四重奏）

- **输入可判定**：选举是流粒度输入的纯函数——同输入 ⇒ 同选举 ⇒
  同字节（**字节确定性 part 法则依赖于此**）；
- **≥10% 胜出门槛（LAW）**：快布局必须赢基线 1/10 字节否则基线
  出货；不可压缩守卫 = 与原始像比较（赢不过 raw 永不存储）；
- **编码时 round-trip 验证**：每个当选粒度解码回读比对规范字节
  后才可发布；
- **拒绝即降级，绝不归一化**：所有拒绝是带原因的类型化
  `Demotion`，落点恒为 VERBATIM（字节精确 by construction）。

编码族：FSST（一等公民，字符串）、FFOR/bytefor/deltafor（帧基准/
字节基准/增量帧基准）、alpc、dictcodes、packednum、arraydual、
boolbm、jsonbshred、LZ4/Zstd 包装层。FFOR 只在**融合姿态**下参选
（FFOR 赢融合输平铺——选举为其定价）。

### 2.3 其他要点

- **并行原生扫描**（pgrc2_scan）："并行原生出生"是绑定法则（dop=1
  是确定性参照而非独立路径）；粒度序号 span + `pgrc2_claim` 认领、
  认领结束按构造释放 part 钉、字典惰性帧（DictHandle 只触达被引用
  帧字典）、RowId 32/19/13 位宽档；
- **RowId**：打包列存 RowId，10B 行位预算静态断言；
- **目录生命周期**：每表数据住独立目录（主 fork 路径的 sibling），
  事务性创建/级联删除/子事务重挂接镜像 smgr 语义；
- **表 AM 注册** = 闭集名探针（pg_am.amname == "pgrcolumnar2"），
  handler 从不被调用；
- **边车槽位预留**：谓词缓存/memo/trgm 的 sidecar 槽在格式里
  RESERVED（先占位后实现）。

## 3. 向量索引：pgvector 0.8.5 忠实移植

| 维度 | 事实 |
|------|------|
| 类型 | `vector`（f32 定长数组，dim 校验/typmod），**未移植** halfvec/sparsevec |
| 函数 | l2/内积/余弦/l1/球面距离（含平方变体）、加减乘/拼接/子向量/l2_normalize/binary_quantize、dims/norm、比较序 |
| 索引 | **仅 HNSW**（`pgvector_hnsw` 盘上 AM + `pgvector_hnsw_build` 建图）；**ivfflat 未移植** |
| HNSW 构建 | 内存图 = bump arena，u32 元素句柄镜像 C 的 graphCtx 指针共享；达 `maintenance_work_mem` 刷盘，其后逐元组盘上插入；无并行构建（C 无 worker 时也回落串行——**差异已记录**） |
| HNSW 查询 | ef_search=40 默认；iterative_scan / max_scan_tuples=20000 / scan_mem_multiplier GUC 全套（thread-local 单元格背书） |
| 诚实边界 | 迭代扫描内存上限以逐元组估算近似 C 的 MemoryContextMemAllocated；层级 RNG 用移植的 pg_global_prng（同生成器、每后端播种） |

**与列存的组合：不通**。pgrcolumnar2 对 TID 扫描/位图扫描/WHERE
CURRENT OF/采样扫描一律类型化拒绝（"干净 0A000，绝不给非法 TID"），
HNSW 作为 TID 生产者只服务堆表。向量 × 列存（现代向量库的标配组合）
在 pgrust 中缺席——这不是移植债，而是"列存 RowId ≠ 堆 TID"的模型
断层，pgrc2 的 O-8 裁决（打包 RowId + 类型化 TID 拒绝）把这条路
显式封死，等未来边车方案。

## 4. 与 dendro 的对照

| 维度 | pgrust | dendro 现状 | 评 |
|------|--------|-------------|-----|
| 段模型 | 不可变 sealed part + 世代 manifest + 墓碑 GC | 不可变 CBF 段 + manifest 墓碑 + retention GC（GC 定案） | **同构**（append-only 不可变段 + 原子发布 + 墓碑回收三件套两边一致） |
| 增量 | 堆 delta store（涓流 DML 行侧）+ tombstone 关系 | memtx/WAL 承载增量；列存段 checkpoint **全量重建**（materialize_delta） | dendro 模型更简（增量有唯一驻地）；pgrc2 的"删除可见性 = 墓碑行的堆 MVCC"是堆引擎的优雅解，对 dendro 参考价值有限 |
| 编码选择 | 分析后选举四重奏（流粒度、确定性、门槛、验证、降级） | `decide_codecs`：首 64K 行采样、**每列每文件一次**、可注入 `codec_choice` 覆盖钩子 | dendro 粒度粗且有钩子但缺纪律三件（门槛/roundtrip 验证/降级落点）——见 P0 |
| 编码族 | FSST/FFOR/deltafor/bytefor/alpc/dict/packednum + LZ4/Zstd 包装 | RAW/BITPACK/RLE_DICT/ZSTD/FSST/DELTA | 重叠大；dendro 缺 FFOR 族与包装层组合律 |
| 剪枝 | zone + bloom + HLL + PSMA 窗口 + 字典帧统计 | 逐块 min/max（块头内）+ 段统计（stats.rs） | dendro 有核无面（bloom/HLL/PSMA 未做） |
| 并行扫描 | claim 面 span 认领、并行原生出生 | 串行扫描（S3 range 并行读在 ScopeDB 调研挂 P1） | 参见机会式求值调研 P1-2 |
| 字节确定性 | **显式法则**（输入可判定选举 ⇒ 字节确定 part） | 采样决策事实上确定（输入序确定），但**未成律**：无验证、无门槛、跨版本无稳定性合同 | **P0 借鉴点**：dendro 段若入 CAS（内容寻址），字节确定性从"巧合"升格为"合同" |
| 向量索引 | pgvector/HNSW（堆表专用） | **无** | 见 P2 |

## 5. 可借鉴点清单

### P0（确定性编码选举成律——内容寻址列存的前提）

1. 把 pgrc2 的选举四重奏写进 dendro 列存写入端合同：
   - **输入可判定**：`decide_codecs` 对同一（schema, 数据序, 版本）
     必须产出同一编码序列——将采样窗、估算公式、并列裁决全部钉死
     为版本化常量（版本号入段 footer）；
   - **≥10% 门槛 + 不可压缩守卫**：候选赢不过 RAW/基线 1/10 就
     RAW 出货（当前 dendro 无门槛，弱压缩可能负收益）；
   - **编码时 round-trip 验证**：debug/test 构建解码回读比对（生产
     可开关）；
   - **拒绝降级 RAW，绝不静默换 codec**。
   动机：dendro 的内容寻址路线下，列存段哈希即身份——字节确定性
   从优化性质升格为**正确性合同**（同数据 ⇒ 同哈希 ⇒ 分支共享/
   去重成立）。pgrc2 已验证该法则可工程化。
2. FFOR 族评估：dendro 有 BITPACK/DELTA 但无帧基准（FOR）变体；
   pgrc2 实证"FFOR 赢融合姿态输平铺"——若做 JIT/融合内核（见
   JIT 调研 P0），FFOR 值得同批引入。

### P1（观察项）

3. 剪枝面扩展（bloom/HLL/PSMA 窗口）：CBF 块头已有 min/max 骨架，
   段级 bloom（等值高频列）与 HLL（count distinct 加速）是低风险
   增量；pgrc2 把这些放"判定面"（verdict plane）与扫描统计归因
   （XC-5 census）的分层值得照抄。
4. 格式裁决编号化：dendro 的 CBF SPEC 05 已有"已文档化偏差"节；
   借 pgrc2 的做法把每个不可重审决策编号入册（O-N），修改 = 对
   文档的显式 PR。

### P2（向量索引方向性记录）

5. dendro 向量索引为空白。pgvector HNSW 的**堆/TID 模型与 dendro
   内容寻址 + 分支模型不兼容**（图 mutation = 段重写；pgrust 用
   类型化拒绝封死了列存上的向量索引，同类断层在 dendro 更深）。
   可行方向（需独立调研立项）：内容寻址 HNSW（图分块 + 追加式
   邻接表、epoch 分代图 + 合并）、或段内暴力扫描 + SIMD 距离内核
   + 段级 ANN 剪枝（IVF 风格质心）。**不建议直接照搬 pgvector
   页式图**。

## 6. 诚告边界

- AGPL-3.0：只读设计，不抄代码（同 JIT 调研诚告）；
- pgrcolumnar2 是活跃重设计（lane 报告驱动、M3/M4/M5 分块推进），
  读到的是过程态而非终态；v1 与 v2 并存（v2"design-only donor"
  语义下 v1 仍是 ClickBench 载体）；
- HNSW 是 C 忠实移植而非新设计——其价值在"移植面清单 + 已记录
  差异"，不在算法创新。

## 7. 信源

- 仓库：<https://github.com/malisper/pgrust>（clone 实读，2026-09-18）：
  `crates/backend/access/pgrcolumnar/src/`（lib/format/bloom/hll/
  scan/writer 等 12 文件）、`crates/backend/access/pgrcolumnar2/`
  （pgrc2_{format,am,delta,scan,codec,claim,...} 头注与关键实现）、
  `crates/contrib/pgvector{,_hnsw,_hnsw_build}/src/`、
  `crates/backend/executor/sqe/src/bank.rs`
- dendro 侧对照：`crates/dendro-columnar/src/`（lib.rs CBF 布局、
  writer.rs decide_codecs、integrate.rs codec_choice 钩子、stats.rs）、
  `docs/design/GC定案.md`（列存段生命周期）、
  `docs/research/pgrust与JIT设计调研.md`（执行器/JIT 面）
