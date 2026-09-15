# 04 执行层：ChunkPlane

> push-based 批管线。参照：DuckDB（Vector 多编码 + selection）、
> Polars 新流（Morsel/async）、Velox（管线切分）。

## 1. Chunk（执行数据单元）

```rust
pub struct Chunk {
    schema: Arc<ChunkSchema>,      // 列类型（稳定引用，跨算子共享）
    arrays: Vec<ArrayRef>,         // arrow 列（CBF 解码产物可零拷贝借用）
    len: usize,
    sel: Option<Box<[u32]>>,       // selection vector（过滤不物化）
}
```

- **v1 实现**：包装 arrow ArrayRef + selection（ADR-4：分配优化推迟，
  藏在类型后）；
- 常量列/字典列等 DuckDB 式多编码 **v1 不做**——CBF 解码出的就是
  flat array；编码优化是 CBF 层私事；
- **边界转换**：`Chunk → RecordBatch`（结果集出口，零拷贝）；
  `RecordBatch → Chunk`（CBF 入口，零拷贝）；**禁止逐行跨界**。

## 2. 管线协议（push-based）

```rust
pub trait Sink { fn push(&mut self, cx: &mut Ctx, c: Chunk) -> FlowControl; }
pub enum FlowControl { Continue, Stop }   // LIMIT 满员/取消/超时 → Stop

pub trait PipeOp { fn push(&mut self, cx: &mut Ctx, c: Chunk, out: &mut dyn Sink); }
```

- Source 顺序产 Chunk 推入算子链尾端的 Sink；
- `Ctx` 携带：会话句柄（deadline_check/cancel——**每算子 push 前检查**，
  超时/取消即时传播）、内存守卫配额、标量程序引用；
- **Stop 沿 push 链逆向传播**：Sink 停止后算子即不再向下游推（LIMIT
  短路的实现点；EXISTS 半连接同机制）；
- v1 **单线程顺序执行**（不做 morsel 并行——AP 200k 行 185ms 的瓶颈
  不是并行度；并行是 v3，避免一次引入调度复杂度）。

## 3. 算子集（v1 最小集）

| 算子 | 语义 | 备注 |
|------|------|------|
| Filter(pred) | 逐行 eval_row → sel 累积（v1）；列式求值（v2，见 03 §5） | 不物化，传 sel |
| Project(exprs) | 表达式列物化 | 仅末端 |
| HashJoin(build, probe) | build 侧收集 hash 表；probe 逐批 | v1 限同层两侧（ADR-6）|
| Aggregate(groups, aggs) | 分组聚合（现有 agg 语义迁入） | 现 ap_group_agg 的算子化 |
| Sort / Limit | 排序 / 截断 | Limit 即 Sink 特例 |

算子**无状态于查询间**（状态在 Ctx/算子实例内）；同一算子类型服务
TP/AP——差异只在批大小（点查路径批=1 行或不产生中间批）。

## 4. 数据源（Source trait = 放置层多态）

```rust
pub trait Source {
    fn open(&self, cx: &mut Ctx) -> Box<dyn Iterator<Item = Result<Chunk>>>;
}
```

| Source | 语义（全部为**现有行为的搬运**，非新逻辑） |
|--------|------------------------------------------|
| `MemtxSource`（Point/Range）| memtx `get`/`snapshot_rows_in_range` → 行解码 → chunk（点查=1 行 chunk 或直接 Sink 产 ResultSet）|
| `CbfSource` | col_segments 段剪枝（pk_range ∩ [pk_min,pk_max]）→ ap.scan → RecordBatch → Chunk（零拷贝）→ pk 去重 + col_deletes 抑制 |
| `MainPlusDeltaSource` | CbfSource 主流 + memtx 尾巴：v1 = 现 `seen: HashSet` 去重语义原样搬运（行为等价）；v2c-2 = pk 有序归并替换 |
| `ProllySource` | time_travel_scan 现逻辑 → chunk |
| `RowFallbackSource` | 现行 `table_scan` 路径 → chunk（**等价性锚点**：任何派发决策可与之对拍）|

**可见性规则**（所有 Source 一致）：snapshot 过滤（memtx）/段快照
（CBF）/事务冻结根（Q-14：显式事务读 `txn_head`，事务自身写以 overlay
并入——现 try_ap_scan 语义逐条搬运）。

## 5. 管线装配与退化路径

- 派发器（见 05）产出：`Source + [算子] + Sink` 的具体序列；
- **退化管线**：点查 = MemtxSource(Point) → ProjectSink（无中间算子，
  零 Chunk 中转——源直接产 1 行结果，等价今日 try_pk_pushdown 快路径）；
- 事务内（Q-14）：Delta/RowFallback 才可见事务自身写——Main/Prolly
  天然不含未提交数据，无需处理；MainPlusDelta 的尾巴含事务写
  （与现 overlay 一致）。

## 6. 实现前必须回答

1. `Chunk.sel` 与 arrow `filter_record_batch` 的关系：v1 建议 sel 只在
   Filter→Project 相邻时物化（filter_record_batch），跨多算子保留
   sel——需要小实验定夺（性能差异可能 <5%，不值得复杂化则统一物化）。
2. 错误传播：Source 中途错误（CBF 段 CRC 坏）→ 管线中止 + 错误上抛
   （现语义）；不允许"跳过坏段"（静默丢数据红线）。
3. 内存量（S-3）：Chunk 批大小上限（建议 4096 行，介于 DuckDB 2048
   与现 CBF 行组之间，bench 后定）；管线内存守卫并入现有
   max_txn_bytes 同款机制。
