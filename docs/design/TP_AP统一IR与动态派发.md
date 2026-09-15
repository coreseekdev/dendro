# TP/AP 统一 IR 与动态派发（设计提案 v0）

- 日期：2026-09-15
- 状态：提案（待评审；实施从 v2c-1 起，行为等价重构）
- 命题来源：架构定向——**TP 侧数据可能不足 AP 侧的 1/10，不能认为 TP & AP 是
  "一份数据的两种形态"**；目标是统一 IR，使查询派发成为可推理的动态决策。

## 0. 核心立场：三层放置模型（placement tiers），不是双副本

经典 HTAP（TiDB TiFlash）把 TP/AP 当作**同一份数据的两种完整形态**，代价是
2× 存储 + 副本同步。dendro 的命题相反：

| 层 | 内容 | 形态 | 体量 | 服务对象 |
|----|------|------|------|---------|
| **Delta（TP 层）** | memtx + WAL 可见尾巴 | 行式 | **小**（活跃工作集） | 点查、小范围、最新读、事务内读 |
| **Main（AP 层）** | CBF 列存段（col_segments + col_deletes） | 列式（FSST/Delta/ZSTD 压缩） | **大**（历史 bulk） | 大扫描、聚合、投影 |
| **History（Git 层）** | prolly 树全历史 | 行式、内容寻址、跨分支去重 | 大但去重后增量小 | 分支/合并/时间旅行——**不在 TP/AP 派发热路径上** |

代码事实核对（2026-09-15）：这个分层在读取路径**已存在**——`try_ap_scan`
（scan.rs:652）就是"CBF 段扫描（段级 pk 剪枝、新段优先去重）→ memtx overlay
→ 事务写 overlay（Q-14）"，且 `col_rows < 10_000` 的表纯 TP 侧。**缺的不是
分层，是统一表示与派发规则**：现在派发是 `eval_select` 里的 if-else 链
（try_ap_scan → try_pk_pushdown → time_travel_scan → 行扫描），决策标准
（版本子句、行数阈值、谓词形状）散落且不可组合。

## 1. 统一 IR（v1 最小形态）

采用 SOTA 调研 §6.4 + optd 泛形（§4）：

```rust
pub struct PlanNode<T: RelOp, D: Attr> {
    pub typ: T,                       // Scan / Filter / Project / Join / Agg …
    pub children: Vec<Arc<PlanNode<T, D>>>,
    pub data: Arc<D>,                 // 属性束（见下）
}
```

属性束 D 首批字段（每一条都对应一种派发/改写能力，不多不少）：

| 字段 | 类型 | 解锁的能力 |
|------|------|-----------|
| `schema` | 唯一列 ID（查询内全局唯一）→ 列索引 | 谓词下推/投影改写不漂移（§6.4-①） |
| **`placement`** | `Tier { Delta, Main, History, Any }` × **coverage 区间** | **动态派发的核心**（见 §2） |
| `snapshot` | u64（gap-free watermark ts） | 一致快照跨层合并的正确性依据 |
| `order` | Main 段按 pk 有序 | 区间谓词下推 + 归并合并（v2） |
| `pipeline` | 边界标记（v2） | 向量化融合判据（§6.4-③） |

**Scan 的物理替身集合**（一个逻辑 Scan 展开为多个可执行形态）：

```rust
enum ScanImpl {
    DeltaPoint   { keys: PkSet },          // memtx pk 直查
    DeltaRange   { range: PkRange },       // memtx 范围
    MainScan     { segs: Vec<SegId>, pruned: bool },  // CBF 段（footer zone map 剪枝后）
    MainPlusDelta{ segs, tail: bool },      // Main + Delta 尾巴合并（现 overlay 语义）
    HistoryScan  { as_of: u64 },           // prolly 时间旅行/分支点
    RowFallback,                           // 现行表扫描（小表/兜底）
}
```

dendro 特有语义（分支、快照 ts、AS OF）全部放属性 D，**不进算子枚举**——
换引擎/加算子不动语义载体（optd 教训）。

## 2. 派发算法：coverage 推理 + 规则式代价

`coverage(tier, snapshot) → Full | Partial(lo, hi) | Empty`：

1. **单层 Full** → 直接派发（小表 Delta Full；checkpoint 后无尾巴的表 Main Full；
   AS OF ≤ checkpoint 的 History Full）。
2. **Main+Delta 联合覆盖** → 两侧 pk 均有序（Main 段 pk_min/max 已有），
   归并去重替换现在的 `HashSet seen`（O(n) 且顺序稳定）；谓词两侧下沉
   （Main 走 footer zone map——已有；Delta 走 memtx 范围——已有）。
3. **不覆盖** → History / RowFallback 兜底。
4. **代价模型 v0 = 规则式**：保留 `col_rows ≥ 10_000` 行数阈值；选择性用
   zone map min/max 粗估；点查恒 Delta/History。learned 统计是 v3 的事
   （统计进 catalog 带版本，不进 IR——§6.4 禁令）。

派发是**纯函数**：`(logical plan, catalog, snapshot) → physical plan`。
可测试性：同输入恒同输出，差分测试（I-H1）直接护航。

## 3. 存储效率主张的落点

现状大表**双重持有**：prolly（全历史行式）+ CBF（col_gen 投影，目前按表
全量物化）≈ 2× 行式 + 1× 列式。把"TP ≪ AP"从口号落成两个可验收目标：

- **a. TP 派发路径只碰 Delta**：TP 点查/小范围绝不因拉全历史而行式扫描全表
  （`try_pk_pushdown` 已做到点查；范围查询仍可能走全表行扫——IR 化后
  Main+Delta 归并承接范围）。
- **b. Main 成为 bulk 权威**：col_segments 已是段列表——checkpoint 改为
  **按 pk 切段增量物化**（只重投影新段，避免全表重投影）；prolly 保留为
  git 语义权威（分支/合并/时间旅行），但"当前读"不再必经 prolly。
- **数字口径**：1/10 是设计目标不是既定事实。验收 = bench：行式（memtx
  + prolly 窗口）vs 列式（CBF）字节比、Delta 尾巴占比、派发器切换点
  （10k 阈值）两侧的 p50/p99。bench 基线：`ap_load_rows_s`（现 0.771）、
  `ap_group_agg_200000_ms`（现 185）。

## 4. 演进路径（行为等价重构起步，不推翻现有实现）

| 步骤 | 内容 | 护航 |
|------|------|------|
| **v2c-1** | 把 eval_select 的 if-else 派发链改写为 IR 枚举 + coverage 派发器（**行为等价**） | I-H1 差分（AP 路径=行路径）+ 26 slt + 026 null edge |
| **v2c-2** | Main+Delta 归并替换 hash 去重；Delta 尾巴谓词下沉 | ap_tp_differential 3 测试扩展 overlay 用例 |
| **v2c-3** | checkpoint 按 pk 切段增量物化；存储字节比 bench | checkpoint 一致性测试 + I-C4 段退休界 |
| v3 | order/partitioning 属性、pipeline 边界、hash join、代价模型升级 | 另立设计 |

**不变量（全部已有机制，IR 化不得破坏）**：gap-free watermark（跨层合并的
一致性地基）、Q-14 显式事务可见性、时间旅行门控（AS OF 不静默读当前——
R6 P0 教训）、I-H1 差分、I-C4 段退休界。

## 5. SOTA 锚点

| 系统 | 与本提案的关系 |
|------|---------------|
| SAP HANA delta/main | 同形：TP 写入小 delta 行存，merge 进大列存 main——"两层数据量级悬殊"的原始出处 |
| Snowflake Unistore（hybrid tables） | 统一 SQL 跨行存/列存派发 + 跨层 join——"动态派发"的产品化先例 |
| SingleStore universal storage | 表内行存+列存混存、按查询路由 |
| TiDB TiFlash | **反例**：两份完整副本 + Raft 同步——正是命题否定的"一份数据两种形态" |
| ClickHouse MergeTree parts | 段=part、pk 序、段级剪枝与去重合并——col_segments 的同构参照 |
| Umbra | 自适应执行谱系；HyPer pipeline 模型（v2c-4+ 的 pipeline 属性依据） |

## 6. 待决问题（评审输入）

1. Delta 尾巴的**上界**：overlay 无界增长（从不 checkpoint 的大事务/长尾巴）
   时 Main+Delta 归并退化为行式——是否给 Delta 尾巴设硬上限（超限强制
   checkpoint）？涉及 I-C4 与 checkpoint 时机，需单独评审。
2. 多列主键：col_segments 的 pk_min/max 目前是单列序——IR 的 order 属性
   在多列 pk 下如何定义（v2c-2 前必须定）。
3. col_gen 投影代与段增量的交互：部分段重投影后 col_rows/col_deletes 的
   一致性口径（v2c-3 前必须定）。
4. 跨表 join 的 placement 传播：Delta×Main join = 3 种层组合 × pipeline
   边界——v1 先限制"join 两侧同层"，跨层 join 归入 v3。
