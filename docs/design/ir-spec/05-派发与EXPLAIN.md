# 05 派发与 EXPLAIN

> 派发是**纯函数**：`(逻辑计划, catalog, snapshot) → 物理管线描述`。
> 纯函数 ⇒ 可测试性（同输入恒同输出）⇒ 差分测试可枚举。

## 1. coverage 推理

对每个 Scan 节点，按固定优先级产出 ScanAlt 候选集：

```
fn scan_alternatives(req, cat, snap) -> Vec<ScanAlt> {
    if req.version.is_some()  { return vec![HistoryScan] }        // R6 门控
    let mut alts = vec![RowFallback];                              // 恒在（兜底）
    if req.point_keys.is_some()           { alts.push(DeltaPoint) }
    if req.pk_range.is_some()             { alts.push(DeltaRange) }
    if cat.col_rows(table) >= THRESHOLD   { alts.push(MainScan / MainPlusDelta) }
    alts
}
```

- `THRESHOLD = 10_000`（沿用现行 col_rows 门槛，行为等价）；
- MainPlusDelta 只在单列 pk 时进入候选（ADR-6 边界）；
- 候选集生成后由**选择规则**定夺（下一节）——候选集可枚举是差分测试
  的基础（ADR-5 的 force_source 直接映射到"跳过选择规则，指定候选"）。

## 2. 选择规则（v0 规则式，无统计）

按序取第一条命中的规则：

1. version 子句 → HistoryScan；
2. 显式事务且涉及本表的读 → DeltaPoint/DeltaRange/RowFallback
   （**不派 Main**：Q-14 要求事务读见自身写，Main 无事务尾巴——保守
   正确；事务内大表查询走 RowFallback 与今日行为一致）；
3. pk 等值 → DeltaPoint；
4. Main Full 覆盖（checkpoint 已覆盖至 snap，无尾巴）→ MainScan；
5. Main+Delta 联合可 Full → MainPlusDelta；
6. 其余 → RowFallback。

**无代价比较**：规则序即优先序（v0）。代价模型（zone map 选择性估计）
v1 记录不启用——只在 EXPLAIN 中输出"剪枝掉的段数"作为未来特征依据。

## 3. Join/多表的层约束（v1）

- 同层 join：两侧同一 ScanAlt 变体 → 正常装配；
- 跨层 join（如 Delta×Main）：**整体降级**为 RowFallback×RowFallback
  （走现行行路径 join）——保守正确，性能损失有界（等价今日）；
- 层约束的检查在派发器，管线装配器无需感知。

## 4. 装配

选择 ScanAlt 后装配管线（见 04 §5）：

```
物理管线 = Source(selected) → Filter* → Project → Sink
```

- Filter 谓词来自逻辑计划的 PredId（ScalarProgram，见 03）；
- 谓词**先于** Source 的部分（pk_range 提取）已在候选生成期用掉——
  剩余谓词一律 Filter 算子（不做 Source 内隐式过滤，除 CbfSource 的
  段剪枝——那是分区消除不是谓词求值）。

## 5. EXPLAIN（v1 输出格式）

```
QUERY PLAN
Scan t [MainPlusDelta segs=3/7 pruned=4 tail=yes]
  Filter (v > 10)  [rows→?]
  Project (id, v)
expr:                                  ; 见 03 §7
  0: Col(1) ...
dispatch: rule#4 (main full coverage, threshold=10000)
```

- 输出**派发理由**（rule# + 关键数字）——规则式代价的可观测化；
- `force_source` 生效时标注 `[forced: delta]`（调试可辨识）。

## 6. 实现前必须回答

1. 规则 4 的"无尾巴"判定：checkpoint 完成后 col_rows 与树行数如何比对？
   （现 try_ap_scan 无此判定、靠 overlay 合并兜底——v1 同样用
   MainPlusDelta 兜底即可，规则 4 可先省略，减少判定错误面。）
2. `force_source` 的实现位置：派发器参数（推荐）还是会话变量？需要
   与 Session 序列化/连接复用语义对齐（仅调试构建暴露）。
3. 派发器单测需要"catalog 快照夹具"（同一数据不同 checkpoint 状态）——
   测试基建是否已有等价物？（ap_tp_differential 可扩展，见 07。）
