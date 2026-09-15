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

- **len = 逻辑行数**（评审 D4 定死）：`sel=Some(s)` 时 `s.len()==len`
  且 `arrays[i].len() >= len`（物化后相等）——这一句决定全部算子写法；
- **零行不变量**（评审 D3，DuckDB/Polars 同款）：算子不接收也不发送
  0 行 Chunk；Filter 全批滤空时跳过 push 直接 Continue；空结果集的
  schema 由 Sink 出口保证（Chunk 自带 Arc<ChunkSchema>）；
- **v1 实现**：包装 arrow ArrayRef + selection（ADR-4：分配优化推迟，
  藏在类型后）；
- 常量列/字典列等 DuckDB 式多编码 **v1 不做**——CBF 解码出的就是
  flat array；编码优化是 CBF 层私事；
- **边界转换**：`Chunk → RecordBatch`（结果集出口，零拷贝）；
  `RecordBatch → Chunk`（CBF 入口，零拷贝）；**禁止逐行跨界**。

## 2. 管线协议（评审 D1/D2 修正：补 finish/EOS 与停止令牌）

```rust
pub enum FlowControl {
    Continue,
    Stop,                    // LIMIT 满员/取消/超时 → 正常停止
    Err(SqlError),           // 求值错误 → 语句失败（现状语义）
}
pub trait Sink { fn push(&mut self, cx: &mut Ctx, c: Chunk) -> FlowControl; }
pub trait PipeOp {
    fn push(&mut self, cx: &mut Ctx, c: Chunk, out: &mut dyn Sink) -> FlowControl;
    /// 输入耗尽（EOS）后的收尾（评审 D1：DuckDB Sink→Combine→Finalize
    /// / Polars 聚合节点"Sink 态→combine→Source 态"的同构物）。
    /// Aggregate 吐最终组（含"空输入+无分组仍出一行"合同）、Sort 排序后
    /// 吐全部行、HashJoin build 收表——没有此方法三者全部堵死。
    fn finish(&mut self, cx: &mut Ctx, out: &mut dyn Sink) -> FlowControl;
}
```

- 驱动器：Source 迭代器产批推链，None 后**按装配序调用各算子 finish**；
- **HashJoin 由装配器拆两段**：build 子管线（build 输入 → 收集器 Sink）
  + probe 主管线（build 表就绪后 probe 输入流过）——不靠单个算子内部
  状态机承载双输入（DuckDB pipeline 切分同构）；
- **停止令牌随 Ctx 走**（评审 D2，Polars SourceToken 同构）：Ctx 持有
  `stopped: AtomicBool`（v1 可退化为普通 bool），任何算子返回 Stop 时
  驱动器置位，**Source 每次产批前检查**；"不得再 poll"扩展为
  **Err/Stop 后均不得**（LIMIT 满员后不再拉批）；
- Stop（正常停止）与 Err（语句失败）保持二分，不合并；
- **metrics 挂点**（评审 D7，Polars 调度器注入模式同构）：驱动器在
  算子边界统一计数（rows/time，v1 可空实现），算子不感知——为
  EXPLAIN ANALYZE 预留；Ctx 携带：deadline_check/cancel 检查点、
  内存守卫配额、标量程序引用、**参数数组**、**停止令牌**；
- v1 **单线程顺序执行**（morsel 并行是 v3；跨线程后返回值传播失效，
  届时令牌是唯一通道——现在的 Ctx 形状即为其预留）。

## 3. 算子集## 3. 算子集## 3. 算子集（v1 最小集）

| 算子 | 语义 | 备注 |
|------|------|------|
| Filter(pred) | 逐行 eval_row → sel 累积（v1）；列式求值（v2，见 03 §5） | 不物化，传 sel |
| Project(exprs) | 表达式列物化 | 仅末端 |
| HashJoin(build, probe) | build 侧收集 hash 表；probe 逐批 | v1 限同层两侧（ADR-6）|
| Aggregate(groups, aggs) | 分组聚合（现有 agg 语义迁入） | 现 ap_group_agg 的算子化；**strict 聚合遇 NULL 输入跳过转移**写进算子合同（PG EEOP_AGG_STRICT_INPUT_CHECK 对应物，评审 M4）|
| Sort / Limit | 排序 / 截断 | Limit 即 Sink 特例 |

算子**无状态于查询间**（状态在 Ctx/算子实例内）；同一算子类型服务
TP/AP——差异只在批大小（点查路径批=1 行或不产生中间批）。

## 4. 数据源（Source trait = 放置层多态）（评审 P0-2 修正构成）

```rust
pub trait Source {
    fn open(&self, cx: &mut Ctx) -> Box<dyn Iterator<Item = Result<Chunk>>>;
}
```

| Source | 语义（**现有行为的逐段搬运**，含坑位注释） |
|--------|------------------------------------------|
| `CurrentSource::Point` | **四段合成**：memtx get → **墓碑判定**（latest_ts ≤ snapshot 不得回树——R7-1 伴生①，曾出 P0）→ prolly cursor::lookup → txn writes（build_point_view 1258-1297 逐段搬运）。checkpoint 截断后树是主源 |
| `CurrentSource::Range` | prolly range_scan + snapshot_rows_in_range + txn（table_scan 1611-1656；**无纯 memtx 范围路径**） |
| `CbfSource` | 段剪枝（pk_range ∩ [pk_min,pk_max]）→ ap.scan → RecordBatch → Chunk → pk 去重 + col_deletes hex 抑制（try_ap_scan 725-752）|
| `MainPlusDeltaSource` | CbfSource 主流 + 当前读尾巴（overlay 含 txn 写，Q-14）；v1 = 现 `seen: HashSet` 语义原样搬运；v2c-2 = 归并替换 |
| `ProllySource` | time_travel_scan 现逻辑 → chunk |
| `RowFallbackSource` | 现行 `table_scan` 路径 → chunk（**等价性锚点**）|

**已知既有缺陷如实搬运**（评审 P2）：多列 pk 且 ≥10k 行的表今天已走
CBF 路径且无条件以 pk[0] 做去重键（潜在既有 bug）——v1 派发器排除
多列 pk 走 Main；差分若红 = 既有 bug 暴露，**按 bug 修而非按等价修**，
单列已知问题清单。

## 5. 管线装配## 5. 管线装配与退化路径

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
   与现 CBF 行组之间，bench 后定）。**读侧内存守卫是缺口**（评审 P2）：
   max_txn_bytes 只管写集，Sort/HashJoin/Aggregate 仍无界物化——
   v1 明示此缺口，守卫机制另立设计（v3），不写空话。
4. **物化点澄清**（评审 P2）：sel 非连续时 Chunk→RecordBatch 必须
   物化（arrow take/filter），"零拷贝"仅对连续/无 sel 批成立——
   转换器实现须注明物化点与代价。
