# 前端快速裁决：ir / unparse / compare（2026-09-19）

## 动机

q21 定位链教训：SQL → 数据的缺陷定位曾走 装载 70 分钟 × 反复复现。
本机制把**前端**（解析 → 建计划）从执行侧剥离，毫秒级裁决、不碰数据。

## 三个正交原语（CLI）

```bash
# 1. dump：SQL → 未优化 IR（parse → build_plan，无降级无重写）
dendro ir --sql "SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'"

# 2. unparse：IR → 重构 SQL'（链式融合——Project/Aggregate/Filter/
#    Sort/Limit 折叠为单条 SELECT，与 build_plan 分解严格互逆）
dendro unparse --sql "..."

# 3. compare：两段 SQL → IR 正式形式等价裁决（不限于 roundtrip——
#    任意两段 SQL 的语义等价判定；不等价时打印两侧正式形式差异）
dendro compare --a "..." --b "..."
```

roundtrip = `compare sql "$(unparse sql)"`（shell 组合）；库级便捷口
`ir::unparse::roundtrip_report`（结构化报告，测试消费）。

## IR 正式形式（canonical form）

比较的前提是唯一性：**等价 IR ⟹ 同一正式形式**（`ir/canonical.rs`，
自底向上、确定性）。保范规则：

1. 括号剥除（`((x)) ≡ x`——表面语法）
2. 标识符/函数名/输出列名/表名大小写归一（解析层处处不区分）
3. 匿名派生表透明化（`SubqueryScan{key:""}` 是纯包裹）
4. 恒等投影剥除（`SELECT *` 透传层）
5. Filter 堆叠折叠 + 合取项排序（合取可交换）
6. Project∘Project 内联（SELECT 列表合并的逆）
7. 混合通配投影 × 窗口消解（`SELECT *, win()` 的必然表面形态）

判别性保持：不同谓词/列序/节点类型不折叠——正式形式是等价关系，
不是语义包含。

## 裁决流程

```
SQL → IR₁ → unparse → SQL' → IR₂；判 canonical(IR₁) == canonical(IR₂)
  YES ⇒ 前端无损（解析/建计划稳定）——缺陷在执行侧（掩码/管线/存储）
  NO  ⇒ 前端缺陷——报告给出两侧正式形式的差异定位
```

## 测试锁定

- `roundtrip_unparse.rs`：ClickBench 全形态黄金用例（LIKE/聚合/JOIN/
  SemiJoin/CTE/窗口/集合操作）达 `newIR == IR`；不可渲染形态（无 FROM
  / WITH RECURSIVE）诚实拒绝
- 唯一性契约双向：表面不同语义等价（大小写/空白/包裹/合取序/列别名）
  ⟹ 相等；语义不同（谓词/列序/聚合参数）⟹ 可区分

---

# 内存监控三表面（memprof，2026-09-19 同批）

SQL：`SELECT * FROM cambium.memory_usage`（kind/name/bytes/items/detail
——meter 行 + rss/hwm/unattributed 包络行 + 分配器明细行）
HTTP：`/metrics`（Prometheus 文本——dendro_mem_* 族 + 峰值归因）
embed：`Connection::memory_snapshot_json()`

分层：语义 meters（memtx.pending / columnar.scan_active / query.rows，
RAII 作用域）+ RSS 包络（unattributed = 计量盲区持续自检）+ 分配器
物理明细（jemalloc stats.allocated/retained，默认 feature；Linux 无
feature 时 glibc mallinfo2 兜底）+ 后台采样环（--mem-sample-ms，默认
1s，600 帧窗口 + RSS 峰值帧归因）。
