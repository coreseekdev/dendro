# opt1-prolly-tp-base：prolly 树作 TP 底座（分支工作档案）

> 决策来源：docs/research/Rust高性能KV调研-嵌入式TP底座.md 选项 1。
> 分支：opt1-prolly-tp-base（自 master@f15f390）。

## 落地构件

1. **节点页缓存字节预算化**：NodeStore 从硬编码 4096 节点（≈16MB
   恒定，cache_budget_bytes 形同虚设）升级为字节预算 LRU（预算/分片，
   Arc 共享去重计账，超界逐最久未用）；`cache_budget_bytes` 真接线；
   census 三件套（hits/misses/驻留字节）进 memprof。
2. **有界 memtx**：提交路径越 `checkpoint_threshold_bytes` 即时踢醒
   检查点线程（Condvar；轮询退化为兜底——此前 30s 轮询窗口内写入
   风暴无界）。内存上界 = 阈值 + 检查点进行期在途。
3. **`DbOptions::embedded()` 预设**：8MB 写缓冲 + 64MB 页缓存 +
   紧凑资源上界（连接 16 / 分支 64 / txn 32MB / 游标 8MB）。
4. **DML pk 下推**（TP 基准暴露的既有缺陷）：UPDATE/DELETE 的单列
   pk 等值/IN 谓词此前走全表扫描 + 全物化（1M 行实测 ~340ms/条），
   现复用点查路径（memtx ∪ 树 + 墓碑判定 + 事务自身写）。
5. **TP 基准**（`dendro tp-bench`）：装载→物化→点查/短范围/随机写
   × {default, embedded} 双臂，median of 5×1s + RSS + memprof。

## A/B 实测（1M 行，checkpoint 后树驻留）

| 场景 | embedded | default（memtx 惯性） |
|------|----------|----------------------|
| point（pk 等值） | 28.4k ops/s | 28.1k ops/s |
| range100 | 11.0k ops/s | 11.2k ops/s |
| write（随机 UPDATE） | **29.8k ops/s**（修复前 2.9） | 31.1k ops/s |
| 终局 RSS | 0.2GB | 0.2GB |

结论：**树底座的 TP 与 memtx 惯性形态持平**（页缓存命中后点查/写
均 ~30k ops/s），embedded 预设提供内存上界（缓冲 8MB + 页缓存
64MB 可配）——选项 1 的可行性得到数据背书。DML 下推带来写路径
**10000×**（340ms → 0.03ms/条，既有缺陷修复）。

## 测试锁定

- `tp_base.rs`：DML 下推正确性（等值/IN/非 pk 回落/混合谓词/DELETE/
  不存在键 affected=0）+ 有界 memtx（40 轮 burst 阈值断言 + 显式
  CHECKPOINT 归零）；全局 meter 并行互扰以串行锁防护。
- 624/624（+2 新测试）。

## 后续（分支外）

- 10M/更大表的页缓存命中率曲线（census 已可观测）
- KV 调研中外部后端（redb/fjall）的对照臂——仅在树底座出现短板时
- memtx 26.7× 结构开销的 P0（pending×memtx Arc 共享）仍独立有效

## 大表验证（10M 行，全键空间随机——补充 2026-09-19）

修正基准键空间硬编码（%1M → %rows）后：

| 指标 | 10M 行 | 对照 1M |
|------|--------|---------|
| point | **26.3k ops/s** | 28.4k（仅降 7%） |
| range100 | 10.4k ops/s | 11.0k |
| write | 31.2k ops/s | 29.8k |
| dendro LRU 命中率 | **99.1%**（21.6M hit / 192K miss） | ~100% |
| LRU 驻留 | **精确 64.0MB**（预算钉死） | — |
| 装载 | 78.7s（127k 行/s） | 6.7s |

缓存有效性结论：64MB 预算下 10M 键（~300MB 树）命中率 99.1%——
**双层缓存结构**：dendro LRU 持有热的树上层（~64MB），miss 路径
由 OS page cache 兜底（本地 NVMe 热），点查仅降 7%。预算是真实
旋钮（驻留精确=预算），嵌入式足迹可预测。

## A/B 解读修正：SQL 层封顶分析（stprobe，2026-09-19）

> 用户质询"是否 memtx 太差或数据路径太差导致无法凸显区别"——
> stprobe 探针（裸存储 vs SQL 路径分层测量）的回答：

| 路径 | ops/s | µs/op |
|------|-------|-------|
| 裸树点查（绕过 SQL，10M 页缓存热） | **120.6k** | 8.3 |
| SQL 点查 × 树驻留 | 26.8k | 37.3 |
| SQL 点查 × memtx 驻留 | 33.4k | 30.0 |

1. **SQL 层是主瓶颈**：~29µs/条（parse→plan cache→catalog→快照→
   行解码→Result 构造）= 树路径总延迟的 **78%**——tp-bench 的
   A/B 持平确系 SQL 层封顶（存储差异被压缩）。
2. **memtx 不差**：同口径 SQL 层下 memtx 比树快 24%（30.0 vs
   37.3µs；存储真实差距 ~1µs vs 8.3µs）。
3. **opt1 结论坚守 + 优化顺序确立**：树底座在当前 SQL 层下不伤
   TP、内存上界收益不变；**正确优化顺序 = 先 SQL 层（78%）后
   存储层**。SQL 点查快路径（embed pk 直读 / RESP 直驱）可拿
   4.5×（37→8µs 量级），无需动存储。


## embed pk 直读快路径（find_by_pk，2026-09-19 补充）

SQL 层封顶分析的直接行动项：`Connection::find_by_pk(table, pk)` ——
绕过 SQL 管线的单行点取（memtx ∪ 树、墓碑隐藏、事务读自身写——
build_point_view 同语义），返回 `Option<Vec<SqlValue>>` 整行。

stprobe 实测（10M 树驻留）：

| 路径 | ops/s | µs/op |
|------|-------|-------|
| SQL 点查 | 28.8k | 34.7 |
| **find_by_pk** | **107.8k** | **9.3** |
| 裸树（理论上限） | 91.9k* | 10.9 |

*裸树本轮受同进程负载扰动——find_by_pk 与其同量级（差 <15%），
即**快路径已贴近存储层理论上限**（3.7×，预测 4.5× 的下界达成）。

差分锁定（embed_fastpath.rs 2 项）：三形态（memtx 驻留/树驻留/
树+overlay 尾巴）× 7 键与 SQL 点查逐行相等；事务内读自身写、
墓碑不复活、提交后可见性。

## 统一分阶段性能分析框架（perf.rs，2026-09-19 补充）

> 架构修正（owner 定向）：pk 类优化必须在统一 SQL 框架内，不做
> 旁路 API；`find_by_pk` 保留为优化基线（存储上限参照 ~9µs）。

**框架**：`crate::perf` —— 八阶段固定枚举（parse/plan_cache/resolve/
exec/eval/scan/storage_get/output_build），M-5 同型三原子计数
（~20ns/语句/阶段，常开）；SQL 自省表 `cambium.perf_stages`
（name/count/avg_ns/total_ms）——任何负载跑完即可归因；
`perf::reset()` 基准臂间复位。

**首次拆解**（SQL×树点查，10M，124K 样本）：

| 阶段 | avg | 次数/查询 | 判定 |
|------|-----|----------|------|
| resolve | 3.6µs | **8** | **29µs 之谜主体**——dispatch/下推/优化器各查一遍 |
| parse | 5.2µs | 1（键值全异必 miss；参数化则为 0） | 次 |
| storage_get | 1.4µs | 5（catalog 树走查 + 数据点取） | 含在 resolve 内 |
| output_build | 0.7µs | 1 | 小 |

**第一个统一路径优化：语句内 resolve 记忆化**（thread_local +
语句守卫；单语句内 catalog 不可变——正确性平凡）。效果：resolve
8→4 次/查询、**exec 33.4→22.7µs（−32%）**、resolve avg 3.6→1.5µs。
**所有查询形态受益**（join/优化器/点查的重复解析全部消除）。

墙钟持平（~33µs）揭示下一层：embed 包装（to_result/split/format
~10µs）——已列为下一打点与优化对象。剩余栈（诚实账）：parse 4.7
（形状缓存可消）/ eval 核心 ~10（计划构建+优化链）/ 包装 ~5-10。

## 持续优化第一轮（master 直推，2026-09-19）

perf 框架扩展（build_plan/optimize/batch_split/to_result 四阶段）+
 两个统一路径优化：

1. **点查早退**（try_point_early）：pk 等值 + 单表 + 裸列/通配投影 +
   无 GROUP/ORDER/LIMIT/DISTINCT/HAVING/子查询 的最窄安全门，在
   build_plan 前复用 try_pk_pushdown + build_point_view（同一实现，
   语义一致由构造保证）。`SET dendro.optimize=off` 与 `force_source
   ≠ auto` 时关闭（差分轴 + dispatch 合同测试锁定）。
   **统一路径点查 30.6k → 39.4k ops/s（32.7→25.4µs，+29%）**。
2. 拆解更新：build_plan 0.8µs / optimize 0.2µs / batch_split 0.5µs
   / to_result 0.25µs——**均非热点**；剩余 SQL 层 = parse 4.6（键值
   全异必 miss——形状缓存待做）+ point 路径本体 + 会话前导 ~3µs。

find_by_pk 基线参照：6.4µs。统一路径与基线的剩余差距 = parse +
lower/arrow 检查 + 会话机器 ≈ 19µs——形状缓存（literal 模板化
plan cache）是下一个大头。
