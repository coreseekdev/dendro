# TP/AP 统一 IR 与动态派发（设计提案 v0）

- 日期：2026-09-15
- 状态：提案（待评审；实施从 v2c-1 起，行为等价重构）
- 命题来源：架构定向——**TP 侧数据可能不足 AP 侧的 1/10，不能认为 TP & AP 是
  "一份数据的两种形态"**；目标是统一 IR，使查询派发成为可推理的动态决策。

> **设计结论（2026-09-15 定稿，经四轮评审：dVBE 双方言 → 统一 ChunkPlane
> → TP 主流口径 → PG 落地）**：
> **一个逻辑 IR**（PlanNode<T,D> + placement×coverage 属性束，§1）
> **↓ 唯一执行方言**：ChunkPlane push 管线（§9，可变执行 Chunk，边界转 RecordBatch）
> **↓ 三层数据源**：MemtxSource / CbfSource / ProllySource（+MainPlusDelta 归并源）
> **↓ 派发 = coverage 推理纯函数**（§2）；点查 = Source→Sink 退化管线
> **标量层**：PG11 式步列表（EEOP 同构），v2b 先行（§10）
> **不做**：JIT、dVBE 字节码（§8 归档）、双引擎、Cascades。
> **护航**：I-H1 差分 / TP ≥325k×85% / gap-free watermark / Q-14 / 时间旅行门控。

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

## 7. 【已取代 → 见 §9】IR 构造：VDBE × Arrow 的双方言合成（dVBE + BatchFlow）

> 追问（2026-09-15）：能否参考 VDBE 与 Arrow 的设计思路构造 IR？
> 结论：**能，且必须分层取用**——VDBE 与 Arrow 不在同一层，它们分别回答
> IR 的两个子问题：VDBE 回答"**执行形态**"（指令如何驱动存储游标），
> Arrow 回答"**数据货币**"（算子间传什么）。合成 = 同一逻辑 IR 的两个
> **执行方言**。

### 7.1 分层图景

```
逻辑 IR（PlanNode<T,D> + placement/coverage 属性束）   ← 派发推理在这里
    │ 降低（lowering）二选一或混合
    ├── TP 方言 dVBE：寄存器字节码 + 游标抽象           ← 取自 VDBE
    └── AP 方言 BatchFlow：RecordBatch 数据流 + 纯内核  ← 取自 Arrow
```

### 7.2 从 VDBE 取什么（四件，及 dendro 对应物）

1. **游标抽象是统一异构存储的关键**——VDBE 用 OpenRead/SeekGE/Next/Column
   一套指令操作 btree 游标，不关心页层细节。dendro 版：`trait Cursor
   { seek(pk); next() -> Row; column(i) }`，按放置层实现三个游标：
   `MemtxCursor`（Delta，snapshot_rows_in_range 已有 BTreeMap range）、
   `CbfCursor`（Main，段批切片）、`ProllyCursor`（History，node 流）。
   **同一套指令跨三层执行——这就是"统一"的落点**：派发器选的是游标
   实例，不是指令集。
2. **寄存器式扁平程序**（非栈式）：prepare 一次编译成线性指令 +
   寄存器数组，执行循环一个 match。对 TP 的分支形计划（点查、索引
   seek、小 join）派发开销最低。v2c-1 先用"计划树解释器 + 游标"
   （≈dVBE-0，即今天 exec 的游标参数化改写）；**拍平成寄存器程序
   （dVBE-1）仅在 profiling 显示树遍历开销显著时做**——避免过早
   引入字节码维护负担。
3. **prepare 期绑定 + schema cookie 失效**：SQLite 靠 schema cookie
   在 schema 变更时 reprepare。与 v2b 绑定计划的"catalog 版本进缓存键"
   完全同构——两个独立设计收敛于同一机制，视为验证。
4. **EXPLAIN 即免费产物**：线性指令流天然可打印（现 EXPLAIN 是占位
   字符串）。子查询/关联子查询用 VDBE 的 coroutine 模式（OP_InitCoroutine）
   而非内联展开。

### 7.3 从 Arrow 取什么（三件）

1. **RecordBatch 是算子间唯一数据货币**——AP 方言的每个算子是
   `RecordBatch → RecordBatch` 纯函数 + selection vector。dendro 已经
   天然对齐：CBF 解码产出 RecordBatch（columnar/reader.rs read_cbf）、
   RecordSet 持 batches（types.rs:147）。**不自造列存格式**。
2. **零拷贝切片**：Arrow buffer+offset 切片让"CbfCursor 把批切片伪装成
   行游标"不付出物化代价——dVBE 读 Main 层时按需 gather，批仍以批流转。
3. **物理类型格**：SqlValue ↔ Arrow 类型已有转换（ColumnMeta/rows_from_batches）。
   把它升格为 IR 的值格（value lattice）——寄存器（dVBE）与批列
   （BatchFlow）是同一值格的两种物理视图，跨方言转换只发生在这里。

### 7.4 桥：物化边界 = pipeline 边界

跨方言只允许在显式边界发生，且方向固定：
- **批→寄存器**（unwind）：仅 LIMIT/点查返回/TP join 的 probe 侧小输入；
- **寄存器→批**（gather）：仅 AP 算子消费 Delta 输出时（如 hash join 的
  build 侧从 memtx 收批）——复用 §7.2-1 的游标批量读。
边界即 pipeline 边界（§6.4-③ 的落地物）：边界内融合（filter 并入 scan）、
边界处物化。**禁止逐行跨界**——那是双引擎 HTAP 的经典性能坟场。

### 7.5 明确不取的部分

| 来源 | 不取 | 理由 |
|------|------|------|
| VDBE | 完整 SQLite opcode 兼容 | Embed API 只需语义层兼容；opcode 兼容锁死指令集演进 |
| VDBE | 栈式机/全局 DB 锁 | 寄存器式已选；并发靠 dendro 两段式提交 |
| Arrow | 把 Arrow 当查询引擎（Acero 直接嵌入） | Acero = Arrow 官方 C++ 流式执行引擎（ExecPlan/ExecNode：Source/Sink/Filter/Project/Aggregate/HashJoin，批间异步推拉，**官方标注 experimental、API 不稳**）。不嵌入的理由：① 快照/分支/时间旅行语义进不了其 SourceNode 契约；② C++ 链接/维护负担；③ dendro 自家 BatchFlow 方言只需几十个内核组合，Arrow compute kernels 已够。可取之处：它的 ExecNode 接口划分与 Substrait 对接是 BatchFlow 方言节点集的对照物 |
| Arrow | dictionary/RLE 全编码集进入 IR | IR 只记逻辑类型；编码是 CBF 存储层私事（footer 决定） |

## 8. 【已取代 → 见 §9】dVBE 重定义（无 SQLite 兼容约束，ISA 自主设计）

> 授权（2026-09-15）：dendro 不要求 SQLite 兼容——dVBE 的指令集可以
> 完全按自家语义重新定义。VDBE 只取**架构思想**（寄存器/游标/prepare
> 绑定/EXPLAIN/coroutine），不取其指令集与编码。

### 8.1 重定义解锁了什么

| 约束解除 | 设计红利 |
|---------|---------|
| 无 opcode 兼容包袱 | 指令集按 dendro 三层游标 + 两段式提交 + watermark 语义从零设计（如 Overlay/MergeTail、WatermarkCheck 可为一等指令，VDBE 里根本没有对应物） |
| 无 SQLite 类型系统 | 寄存器直接承载 SqlValue 值格（Date32/TimestampMs/Bytes 一等公民，无 AFFINITY 强转机制） |
| 无 VDBE Op 结构兼容 | 指令编码自选：**定宽 64 位**（LuaJIT 风格：op:16 + a:16 + b:16 + imm:16 或 op:8+三寄存器+常量池索引），解释循环可用跳表 |
| 无单线程假设 | 超时/取消（deadline_check/cancel_token）作为显式 Checkpoint 指令落在循环边界，而非散布在 C 代码 |

### 8.2 ISA 骨架（v0，目标 ~40 条核心指令 + builtin 表）

```
── 游标族（放置层多态：同一指令，三个游标实现）──
Open      dst=cursor(tier, table, range|keys)   ; tier ∈ {Delta, Main, History}
Seek      cur, reg_pk                            ; 定位
Next      cur                                    ; 前进（段/批/节点流自适应）
Column    reg_dst, cur, col_idx                  ; 值格读
Close     cur

── 寄存器族 ──
LoadI / LoadF / LoadS / LoadN                  ; 常量/NULL → reg
Cmp / Arith(op) / Like(id) / Cast(type)         ; 值格运算（Like/builtin 走表）

── 控制族 ──
Jump / JumpIf / JumpIfNot / Halt
CoroutineInit / CoroutineYield                  ; 子查询（VDBE 模式）

── 聚合族 ──
AggInit(slot, func) / AggStep(slot, reg) / AggFinal(slot, reg)

── 边界族（跨方言唯一通道，§7.4）──
GatherBatch  dst=batch, cur, [pred]             ; 寄存器流 → 批（AP 方言入口）
UnwindRow    reg_dst, batch, i                  ; 批 → 寄存器（LIMIT/返回）
MergeTail    cur_main, cur_delta, out           ; Main+Delta 归并（一等指令！）

── 会话族 ──
Checkpoint  deadline_check / cancel / mem_guard ; 超时/取消/内存守卫落点
ResultRow   regs[..]                            ; 行输出（dVBE 出口）
```

设计纪律：
1. **核心 ISA 封闭**（游标/寄存器/控制/聚合/边界/会话六族），函数类需求
   （字符串/时间/JSON）一律 `Builtin(id, args)` 走表——VDBE 数百条 opcode
   的膨胀教训不重蹈；
2. **MergeTail 是一等指令**而非伪指令序列：三层放置模型的核心操作
   （§2 派发算法-2）值得 ISA 级支撑，其游标多态实现归并/去重细节；
3. 指令流**可序列化**（定宽 + 常量池）→ embed API 的 prepared 语句、
   wire 协议的预编译缓存、EXPLAIN 打印三者同一表示；
4. 每条指令声明**内存/时间上界**（对齐 S-3 会话守卫），Checkpoint 族
   指令由编译器按循环回边自动插入。

### 8.3 与 v2c-1 的关系（防止过度设计）

v2c-1 **不实现字节码**：计划树解释器 + 三层 Cursor（dVBE-0）已满足行为
等价重构；ISA 是 dVBE-0 的"形状承诺"——树解释器的节点划分（六族）与
未来指令一一对应，拍平（dVBE-1）时只是把树遍历换成线性 PC 推进，
逻辑 IR 与语义零改动。**触发条件**：profiling 显示计划树遍历/虚分派
占 TP 点查 p50 的 >15% 时启动 dVBE-1。

### 7.6 降低规则（派发的第二半）

| 计划形状 | 方言 | 理由 |
|---------|------|------|
| 点查/小范围/事务内读 | dVBE + MemtxCursor | 顺序 seek，向量化无收益 |
| AS OF/分支点读 | dVBE + ProllyCursor | 行流天然 |
| 大扫描/聚合/投影 | BatchFlow + CbfCursor(批) | 内核纯函数，SIMD/selection 受益 |
| Main+Delta 归并 | BatchFlow（Main 批）+ dVBE（Delta 尾）在归并边界汇 | 归并点是显式边界 |
| hash join（跨层） | build=Delta gather 成批；probe=Main 批流 | v3（待决问题 4） |

## 9. 【现行方案】修订：统一 ChunkPlane——单一 Arrow 形执行表示（取代 §7/§8 双方言）

> 修订理由（2026-09-15，架构定向）：调研 DuckDB/ClickHouse/Velox/Polars
> 四引擎后确认，push-based 批式向量化管线是执行表示的**行业收敛点**；
> TP/AP 存储不对称（Delta 小行存 / Main 大列存）是**放置层问题，不是
> 执行表示问题**。双方言（寄存器字节码 + 批数据流）引入的跨方言物化
> 边界、ISA 维护、两套算子库，收益不抵复杂度。dVBE 全线砍除。

### 9.1 四引擎执行表示对照（调研结论）

| 引擎 | 顶层表示 | 数据单元 | 并行 | 与 dendro 的关系 |
|------|---------|---------|------|-----------------|
| DuckDB | push-based 管线（操作树推 DataChunk）| **Vector（2048 行）×多编码**（flat/constant/dictionary/RLE/FSST）+ selection vector | morsel/pipeline | 设计母本；"类 Arrow 但为执行而生、与 Velox 共同设计"（Raasveldt CMU 15-721）|
| ClickHouse | 处理器 DAG（显式端口，prepare/work/schedule）| Block（列集） | 端口跨 lane | 显式控制流的参照 |
| Velox | Task → 线性 Pipelines → Drivers（在 exchange 处切开）| RowVector 批 | Driver=管线切片线程 | 管线切分规则参照（hash build/probe 分段）|
| Polars 新流 | 物理节点 DAG + **Morsel**（morsel.rs）| Morsel（批） | async morsel 多线程 | **Rust 同侪**，out-of-core 参照 |
| ~~SQLite/Turso~~ | 寄存器字节码 VM | 寄存器 | 单线程 | 少数派；dendro 不再走此路 |

**共识**：一个引擎一种执行表示（批式向量管线），TP 与 AP 的差异落在
**数据源**（索引 seek vs 段扫描）与**数据量**（1 行 chunk vs 2048 行
chunk），不落在执行模型上。DuckDB 以此同时服务嵌入式 TP 与 OLAP。

### 9.2 统一抽象：ChunkPlane（执行层）

```
逻辑 IR（不变，§1）+ coverage 派发（不变，§2）
    ↓ 降低（唯一方言）
物理管线 = Source → [算子]* → Sink     （push-based，批间传递）
```

- **执行 Chunk（新核心类型）**：列式、**可变、可复用**、带 validity 与
  selection——Arrow 形状但为执行而生（DuckDB 教训：arrow-rs RecordBatch
  不可变、逐算子分配，做执行货币开销大）。只在**边界**转 RecordBatch：
  CBF 解码出口、最终结果集入口。`chunk → RecordBatch` 零拷贝可做；
  反向 gather 需拷贝（小数据可接受）。
- **Source 即放置层多态**（取代 §7 的三游标——同一抽象，批语义）：
  - `MemtxSource`：snapshot 过滤 + 行解码 → 小 chunk（1~64 行）；
    点查退化为 **Source→Sink 退化管线**（零中间算子，等价今日
    try_pk_pushdown 快路径——快路径保住了，只是换了表示）；
  - `CbfSource`：段（zone map 剪枝后）→ 批（零拷贝切片）；
  - `ProllySource`：node 流 → chunk（时间旅行/分支点）；
  - `MainPlusDeltaSource`：归并源（§2 派发-2 的 IR 化）。
- **算子库唯一**：Filter/Project/HashJoin/Aggregate/Sort/Limit——
  一套实现同时服务 TP/AP；SIMD 优化一处受益两处。

### 9.3 对 v2c 路径的影响

| 步骤 | 原计划（§4） | 修订后 |
|------|-------------|--------|
| v2c-1 | 计划树解释器 + 三层 Cursor（dVBE-0）| **逻辑 IR + ChunkPlane 派发**：按 coverage 选 Source，退化为管线的快路径先行；I-H1 差分护航 |
| v2c-2 | 归并替换 hash 去重 | 不变（MainPlusDeltaSource 内部） |
| v2c-3 | 段增量物化 + 字节比 bench | 不变 |
| ~~dVBE-1~~ | 寄存器 ISA 拍平 | **取消**；§8 ISA 骨架归档。重开条件：TP 点查 p50 相比今日快路径劣化 >30%，或进入与 SQLite 正面竞争的亚微米场景 |

### 9.4 风险与缓解

1. **逐查询 chunk 分配开销**（TP 点查本应 ~µs）：线程本地 chunk 池
   复用 + 退化管线（无中间算子）+ 计划缓存/v2b。验收：TP 基准不低于
   现状 325k txn/s 的 85%。
2. **行→chunk 解码成本**（Delta 层）：与今日行路径解码同量级，非新增；
   长期可让 memtx 行格式对齐 chunk 列式布局（v3 评估）。
3. **arrow-rs RecordBatch 不可变与执行 Chunk 的双类型**：边界转换器
   单点维护（`chunk→RecordBatch` 零拷贝；RecordBatch 仅作出口格式），
   防止两套类型逻辑漂移——与 CBF/RecordSet 既有关系一致。

### 9.5 与 TP 主流执行器的关系（MySQL/PG 口径）

**事实**：wire 主流 TP 确实不走向量化管线——MySQL 8.0 是行式迭代执行器
（RowIterator 协议，逐行）；PG 是 tuple-at-a-time 需求驱动管线
（ExecProcNode），标量表达式走 PG11 的步列表解释器（EEOP_* 编译步）
+ 可选 LLVM JIT。**为什么他们行式**：① 行堆 MVCC 存储——可见性
（xmin/xmax）逐元组检查，行式执行与存储同构；② 30 年遗产路径；③
他们的 AP 答案恰恰是**外挂第二引擎**：

| 主流 TP | HTAP 答案 | 形态 |
|---------|----------|------|
| MySQL | HeatWave（内存列存加速集群）| 双引擎外挂 |
| PostgreSQL | Citus / AlloyDB 列存加速器 / pg_duckdb（把 DuckDB 整个嵌进 PG）| 双引擎外挂 |
| TiDB（MySQL wire）| TiFlash 列存副本 | 双引擎外挂 |

**这正是本提案否定的"一份数据两种形态"**——主流 TP 世界用血泪证明
双引擎的代价（同步延迟、一致性口径、运维双倍）。dendro 的差异化恰在
反面：**wire 兼容 ≠ 执行器兼容**（CockroachDB 说 PG wire、ReadySet 说
MySQL/PG wire，内部执行器都是自家形态）；且 dendro 的结果货币本来就是
Arrow——今天每条点查的 `Output::Rows(RecordSet{batches})` 都在构建
RecordBatch，chunk-plane 只是把中间执行层也统一，点查成本结构没有
质变。

**从 PG 学一件真东西（采纳进 v2b）**：PG11 表达式步列表——标量表达式
在 prepare 期编译为 `EEOP_*` 步列表、执行期循环解释、可选 JIT。v2b
绑定计划的 ScalarExpr 形态由此定：**AST → 步列表**（prepare 一次），
步列表在 chunk 列上求值 = 向量化表达式（AP），在 1 行 chunk 上求值 =
行语义（TP）——同一表示两种粒度，与 chunk-plane 正交且互惠。

**验收线重申**：TP 基准 ≥ 现状 325k txn/s 的 85%；退化管线 + 线程本地
chunk 池 + 计划缓存三重压制常数开销。若此线不达，优先优化 chunk 池
而非重开户VBE路线。

## 10. PG（新架构）可参考之处的落地清单

### 10.1 参考映射表

| PG 机制 | 取舍 | dendro 落地 | 里程碑 |
|---------|------|------------|--------|
| **PG11 表达式步列表**（ExprState：Expr → EEOP_* 编译步，执行期 dispatch 循环解释，可选 LLVM JIT）| **取（核心）** | `ScalarStep` 步列表：prepare 期编译一次；`eval_row`（TP，1 行 chunk 语义）+ `eval_chunk`（AP，列上向量化）双后端 | **v2b** |
| prepare/再编译（schema cookie 失效）| 取 | 计划缓存二级键 `xxh3(sql)+catalog_version`，DDL 自动 miss；与 v2a（纯 AST 级，无失效）构成两级缓存 | **v2b** |
| LLVM JIT（表达式编译为本地码）| **不取** | PG 自己的经验：OLTP 上编译延迟常吞掉收益（社区默认建议低代价查询关 JIT）。dendro 的等价物是向量化 eval_chunk；重开条件与 dVBE 相同 | 永久观察 |
| ExecProcNode 需求驱动（Volcano）| **不取** | push-based 已定（§9）；LIMIT/EXISTS 短路由由 Sink 断流实现（push 模型同样可短路） | v2c-1 |
| PG18 异步 I/O（io_uring）| 暂不取 | dendro 存储层是对象存储（ObjStore trait），本地 io_uring 语义作用有限；OSS 客户端层可另行评估 | 远期 |

### 10.2 ScalarStep 落地规格（v2b，独立可交付）

```rust
// 编译期（prepare 一次；catalog_version 进缓存键）
enum ScalarStep {
    Const(u32),          // 常量池索引
    Col(usize),          // 列索引（绑定计划的产物；AST 无此信息）
    Cmp(Op) / Arith(Op) / LogicAnd / LogicOr / Not / IsNull,
    Like(u32) / Cast(Ty) / Builtin(u32, u8),   // 函数走表（§8.2 纪律承袭）
    Jump(u32) / JumpIfFalse(u32),              // 三值逻辑短路
    Out,                 // 结果寄存器
}
// 执行期双后端：同一份步列表
eval_row(&[ScalarStep], &const_pool, row: &[SqlValue]) -> SqlValue          // TP
eval_chunk(&[ScalarStep], &const_pool, chunk: &Chunk, sel) -> Chunk         // AP（v2c-1 接入）
```

- 现有 `optimize.rs` 常量折叠/布尔化简**迁移到编译期**（编译时跑规则，
  步列表即化简产物——规则跑一次而非每次执行）；
- **护航回归**：负数字面量（R8 教训）、NULL 三值逻辑、collation 比较、
  类型提升矩阵——编译器单测逐条对应 eval.rs 现有语义；
- 与 EXPLAIN 的关系：步列表打印即表达式解释（对标 PG `EXPLAIN (VERBOSE)`）。

### 10.3 落地顺序与验收（合成 §4）

| 里程碑 | 内容 | 验收 |
|--------|------|------|
| **v2b** | ScalarStep 编译器 + eval_row + 两级计划缓存 | 现有 SQL 语义回归全绿；点查 p50 不劣于 v2a；EXPLAIN 输出步列表 |
| **v2c-1** | Chunk（可变/线程本地池）+ 三层 Source + coverage 派发器替换 if-else 链；eval_chunk 接入 | I-H1 差分全绿；26 slt 全绿；TP ≥ 276k txn/s（85% 线）|
| v2c-2/3 | 归并源、段增量物化、字节比 bench | 不变（§4）|
