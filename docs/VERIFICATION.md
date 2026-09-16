# 验证账本（dendro）——形式化验证 / 模型检查 / 测试 的总纲

> 落实 AGENTS.md 的验证体系。参考同工作区 basalt 的方法论
> （../stream-db/basalt/docs/VERIFICATION.md），按 dendro 架构（追加-only
> 内容寻址 + 两段式提交 + 分支化）重写。
> 测试回答"跑过的没坏"，验证回答"**没跑到的也不坏**"。

## 0. 状态

- **2026-09-16 v2 后大阶段（执行层收口）**：四方向全绿——
  ① AggOp/ProjectOp 接线 eval_select（聚合/投影消除双路径；伴生修复
  count(文本列) 报错、SUM(DISTINCT) 不去重、混合 int/float 列丢整数
  三处两路径语义缺陷；SET dendro.force_agg 差分轴）；
  ② Source 流式化（MainPlusDelta 三路惰性归并游标，全量 BTreeMap
  物化消除；**账本 #27**：混合增量 checkpoint 丢弃删除键 → AP 复活
  已删行，差分暴露即修；Q-14 事务写在 AP 路径首次有活事务断言）；
  ③ 文本 IR dendro.ir v1（标量方言 print/parse/verifier 同文件，
  P1 全字段 round-trip + P2 字节恒等 + golden 文件；伴生修复编译器
  opidx 对 And/Or/Concat 的 binops 池污染）；
  ④ GRANT/REVOKE 表级 ACL（单点权限门 enforce，exec+prepare 双卡口；
  超户缺省行为不变）。门禁：clippy 0 / 420 测试 / 30 slt 全过。
  下一步：优化器（基于既有逻辑 IR 与 dispatch 纯函数展开）。
- **2026-09-16 优化器 O-2c（计划驱动执行 v1）**：覆盖形状
  （Scan/Filter/Join/Project——无聚合/分组/HAVING/排序/LIMIT/通配/
  版本子句）由 exec_plan 执行重写后计划（计划=执行序，EXPLAIN 与
  执行逐节点对应）；其余回落 AST 路径（差分轴不变）。
  **伴生缺陷修复**：rewrite_pushdown 对 LEFT join 右侧谓词是移动
  而非复制——计划路径 NULL 延展行不再被过滤（差分 6 vs 2 行暴露；
  AST 路径原本加性故未现身）；修为右侧项下推副本 + residual 保留。
  Plan::Project 增 names（别名信息）；Filter{Scan} 谓词下传
  selection 提示（点查/派发判定恢复）。
  门禁：clippy 0 / 463 测试 / 31 slt。
- **2026-09-16 EXPLAIN ANALYZE**（spec 09 §5.5 / 04 §2 D7 预留位
  落地）：ExecCx 参数收敛（masks/top-N 界/指标/深度）；exec_plan
  包装层逐节点采集（实际输出行数 + 子树墙钟——Filter{Scan} 捷径与
  聚合组合内联手记）；输出经 `!` 注解通道（树缩进呈现）。SELECT
  且计划覆盖形态；时间机器相关——Rust 断言 rows 精确 + 标签序列，
  slt 不固化。
- **2026-09-16 计划路径覆盖补全（Limit + 纯通配）**：Plan::Limit
  {limit, offset}（含 OFFSET-only / LIMIT-无-ORDER；Sort 卸 limit
  字段——top-N 界经 sort_hint 父子下传保留有界堆）；Plan::Project
  增 wildcard（纯通配透传；混合通配留 AST）；plan_scan_masks 对
  wildcard 计划 fail-open 整体禁裁剪（差分首跑即抓 null 列）。
  AST 回落清单缩至：混合通配 / DISTINCT on SetOp 等罕见形态。
  门禁：clippy 0 / 465 测试 / 31 slt。
- **2026-09-16 SELECT DISTINCT 实现**（R21-17 的显式拒绝闭环）：
  投影后 first-seen 去重、ORDER BY 前；键 = 类型标签 + 值 debug
  编码（组键同口径防跨类型碰撞）；计划/AST 双路径同点接入。
  语义测试含 DISTINCT ≡ GROUP BY 差分与 WHERE/多列/子查询计数。
- **2026-09-16 优化器 O-2c+（远期清单收口）**：A1/A2 SetOp+Sort
  计划执行（apply_setop 提取共享；Sort{Project} 键回退——镜像
  order_key_value 语义）；A3 Aggregate 组合模式执行（display→
  AggCall 结构化；HAVING/投影映射镜像 eval_select——表达式聚合
  参数与表达式组键在计划路径原生支持）；B Plan::Scan 携带 version
  （display 重建 TableVersion——历史查询上计划路径）。join 顺序
  重排明确不做（无选择率统计前置——见 spec 12 §4）。
  门禁：clippy 0 / 464 测试 / 31 slt。
- **2026-09-16 优化器量化（bench_optimizer）**：O 系列 on/off 对比
  （50k 宽表 × 200 小表，5 轮中位）：下推 1.58× / top-N 1.09×（另有
  内存 O(n)）/ 裁剪+稀疏读 1.16× / 构建侧 1.21×——
  benches/results/optimizer.json。
- **2026-09-16 优化器 O-4 + O-2b + ObjStore 稀疏读**：O-4 INNER
  join 构建侧按实际基数选择（join 时两侧已扫描——rows.len() 即精确
  值；小侧建表 O(min)；输出列序恒 left++right，#28 布局/残留合取
  不变；多重集差分）。O-2b plan 方言 parse/verify 闭环（fail-closed；
  9 语料 parse→verify→再 print 字节恒等——sqlparser Expr
  Display→parse 稳定性锁定；EXPLAIN 计划块可 parse 回放）。稀疏读：
  CBF 掩码扫描经 get_range 取 footer+所需块（footer 拆 tail/body
  解析；decode 走 fetch 闭包；跳列 IO——计数测试：2/5 列掩码
  读字节 < 全读一半且值一致）。
  门禁：clippy 0 / 463 测试 / 31 slt。
- **2026-09-16 优化器 O-3（投影裁剪）**：单表查询列需求位图
  （column_mask：投影/WHERE/GROUP/HAVING/ORDER 全引用面走查 +
  CASE 臂补齐 + pk 恒留 + fail-open——通配/未知名即放弃）；CBF
  段扫描对非需求列零成本 null 占位跳过解码（null 类型取 footer
  物化时真实类型——与当前 schema 分叉时批构造会炸，差分实证即修）。
  差分：prune_differential 8 测试（子集投影/未投影 WHERE/ORDER 回退/
  CASE/通配不裁/overlay 尾巴/行路径恒等/强制 main 臂）。
  门禁：clippy 0 / 458 测试 / 31 slt。
- **2026-09-16 优化器 O-5 + O-2a**：O-5 SortOp top-N 有界堆
  （ORDER BY+LIMIT 内存 O(n)；与全量排序前缀逐字节一致——含并列
  稳定序，单测矩阵差分护航）。O-2a 逻辑计划 IR（ir/plan.rs：Plan
  构建/下推重写/打印；下推决策从 AST 走查迁到计划走查——既有差分
  全绿零行为变化；EXPLAIN join/集合操作形态从 "pending" 升级为
  dendro.ir v1 plan 方言真实计划块；golden tests/golden/plans.ir
  字节级锁定 7 语料）。门禁：clippy 0 / 450 测试 / 31 slt。
- **2026-09-16 优化器 O-1 + 账本 #28**：规则框架落地（spec 12——
  确定性/语义保持/差分可枚举/EXPLAIN 可见四合同）；R1 合取拆分 +
  R2 单源下推（限定名分类，join 前加性过滤，LEFT 右侧下推安全论证）
  + R3 NOT 消除；`SET dendro.optimize` 差分轴。**#28（差分暴露的
  既有缺陷）**：join 后限定名列解析在两侧同名列时错读——
  cols_lookup HashMap 重复键 last-wins + 复合标识符剥前缀取末段，
  使 `o.id` 读到右侧 id 列；修：因子列布局（eval_from 追踪各因子
  列区间）+ 全路径优先解析（expr::eval 与标量编译器双侧）。
  门禁：clippy 0 / 442 测试 / 31 slt。
- **2026-09-16 评审轮（3 agent 并行）**：修复 **3×P0 权限绕过面**
  （派生表子查询、视图两步链 CREATE VIEW→SELECT v、DROP/ALTER 无
  属主检查——require_owner 原写未接线）；P1：GRANT 读改写竞态
  （commit_mu 内 RMW，catalog_commit_locked）、列级权限静默展平为
  全表（改诚实拒绝）、聚合组键 to_text 跨类型碰撞（NULL 与 '' 同组、
  Int64(1) 与 Utf8("1") 判重合并——两路径同步 typed 键）、文本 IR
  含 `;` 常量 parse 失败（\u003b 转义）、常量折叠孤儿 consts 池项；
  P2×9：EXPLAIN 内层走查、用户名 PG 式小写折叠、resolve 瞬态错误
  不吞、compile 出口 verify 接线（曾因文件恢复丢失——构造期校验
  首次真实拦截了测试的元数据不一致装配）、date/timestamp cast
  文本映射、parse fail-closed 收紧（闭括号后内容/重复属性/短 \u）、
  Param $65536 边界、管线 min/max 比较错误传播（与行式口径一致）、
  #27 判重 O(n²)→HashSet。门禁：clippy 0 / 432 测试 / 30 slt。
- **2026-09-15 v2b+v2c 全量完成**：26 条账本缺陷全闭环（#1-#26，每条
  有永久检测机制）；ScalarStep 标量层 + ChunkPlane 执行层 + coverage
  派发器 + MainPlusDelta 归并源 + SortOp/ProjectOp 算子群 + ORDER BY
  管线化 + prepared 重校验 + EXPLAIN 真实输出 + force_source 差分体
  系 + 存储字节比 bench。门禁 7/7（clippy / 373 测试 / 26 slt / TLC
  safety+liveness / Kani 4/4）。

- L1（Kani 无 panic harness）：✅ verification/kani/wal_frame.rs（3 harness：
  任意输入不 panic / 编解码对偶 / 撕尾容忍；运行 `kani --standalone verification/kani/wal_frame.rs`）
- L2（TLA+ 模型检查）：✅ spec/CommitPipeline.tla 首个模型全空间绿
  （1001 状态，5 安全不变式 + StallFreedom 活性），**且已产出实现修复**
  （R8-WM in-flight 摘除不推进水位——TLC 反例→实现修复→回归测试闭环）
- L3（Verus 函数级证明）：⬜ 未开始（verification/verus/ 留位）
- opfuzz：✅ tests/opfuzz.rs（SimObjStore clean 20 + chaos 20 种子全绿）

## 1. 核心不变式目录（模型检查/证明的对象，R8 分析版）

以下为 dendro 核心协议面应检查的不变式全集。每条：定义、违反后果、
当前检测机制、状态。

### I-A 提交管线 / 水位（CommitPipeline.tla，✅ 已建模）

| # | 不变式 | 内容 | 违反后果 | 检测 |
|---|--------|------|---------|------|
| I-A1 | **AckedInstalled** | 已 ack 提交 ⊆ installed——ack 先于安装 = 幽灵/丢失根源 | ack 数据不可见 | TLC ✅（**机制锁**：模型内 ack 与安装同动作原子发生，无独立判别力；适用域 durability ∈ {Group, Always}——NoWait 设计性不成立）+ opfuzz clean |
| I-A2 | **WatermarkVisible** | 快照读到的每行其 ts ≤ watermark 且已安装（实现无持久 lost 集；validate 失败消耗 seq 产生合法空洞） | 事务内可见性翻转（重复读违约，R5 实证） | TLC ✅ + repeatable-read 回归 |
| I-A3 | **WatermarkBound** | watermark ≤ installed_max | 可见性超前 = 未安装读 | TLC ✅ |
| I-A4 | **InFlightSane** | in-flight ∩ (installed ∪ lost) = ∅ | 摘除不完备 | TLC ✅ |
| I-A5 | **Disjoint** | installed ∩ lost = ∅ | 双重结算 | TLC ✅ |
| I-A6 | **StallFreedom**（活性） | installed_max 到顶后 watermark 必追平 | 已 ack 行永久不可见（R8-WM TLC 反例→修复） | TLC liveness ✅（模型 drop 路径回灌后 781 状态全绿）|
| I-A7 | **GapFreeFrontier** | frontier = min(installed_max, min(in-flight)−1)；三处摘除路径同一公式 | 水位停滞/跳过（R4 实证两形态） | 回归（out-of-order + 摘除失败）+ TLC |
| I-A8 | **Pass2 无副作用拒绝** | 等待失败摘除后不得安装 | Uncertain 演变幽灵行 | opfuzz chaos（无幻行断言）|

### I-B OCC / 快照可见性（memtx + 范围下推，⬜ 待模型化）

| # | 不变式 | 内容 | 违反后果 | 检测现状 |
|---|--------|------|---------|---------|
| I-B1 | FirstCommitterWins | 写写交集必恰一方失败（40001），不允许静默丢失 | 丢失更新 | 并发计数器测试（sum==成功数）|
| I-B2 | RepeatableRead | 会话快照内重复读稳定（含并发安装乱序） | 隔离级违约 | concurrent.rs ✅ + R7 缺口修复 |
| I-B3 | 范围下推超集 | 下推区间结果 ⊇ 全扫描过滤结果（区间是收窄不是改写） | 错误结果集 | 范围语义测试（树+overlay+负数混合）✅ |
| I-B4 | 字面量提取完备 | Eq/IN/范围提取对 UnaryOp::Minus 等句法变体不丢字面量 | 静默丢行（R8 实证 IN(-3,4)） | negative_literals_all_surfaces ✅ |
| I-B5 | 时间旅行可见性 | AS OF 读 = 该 commit 物化树快照（无 memtx/会话写混入） | 历史查询错数据 | time_travel.rs ✅ |

### I-C WAL 持久化 / 恢复（opfuzz + Kani，✅ opfuzz / 🚧 Kani）

| # | 不变式 | 内容 | 违反后果 | 检测现状 |
|---|--------|------|---------|---------|
| I-C1 | AckedSurvives | Group ack 提交在 crash+reopen 后完整可见 | 丢 ack 数据（P0） | opfuzz clean ✅ |
| I-C2 | ReopenLegal | 任意故障序列后 reopen 恒成功、帧流 CRC 合法前缀可用 | 打不开库 | opfuzz chaos ✅ |
| I-C3 | NoPhantom | 可见行 ⊆ acked ∪ uncertain-durable（Uncertain = 已持久化但客户端收到错误，reopen 后可见且未 ack——SPEC 02 §3.5） | 幻行 | opfuzz chaos（uncertain 注入档 🚧 待接） |
| I-C4 | 段退休安全 | retire_bound ≤ 全部已安装前沿（在途帧段不可退休） | reopen 丢已 ack 数据（R6 实证） | retirement_bounded ✅ |
| I-C5 | 撕尾合同二分 | 已封段严格 / 未封段容忍 | 两个方向各有一种静默丢 | 双合同测试 ✅ |
| I-C6 | FrameIter 不可信输入 | 任意字节输入不 panic（Err/None 而非 UB）；合法帧必解码；段尾小帧不丢（#19） | 网络可达 DoS / 已确认提交丢失 | 边界测试 ✅ + Kani 0.67 4/4（H1 arbitrary / H2 roundtrip / H3 torn / H4 tiny-frame）✅ |
| I-C7 | 水位停滞免役 | in-flight 全部摘除后（含失败路径）watermark = installed_max | 已 ack 行不可见（R8-WM TLC 反例） | watermark_recovers（精确等值断言）✅ + TLC StallFreedom ✅ |

### I-D 分支 / 合并 / manifest（⬜ 待建模）

| # | 不变式 | 内容 | 检测现状 |
|---|--------|------|---------|
| I-D1 | 分支上限原子 | CAS 重试以最新 manifest 重评估（预检只是快路径） | race 测试 ✅ |
| I-D2 | 三方合并正确 | base 左右三方归并，行冲突显式 40001 | 合并冲突测试 ✅（criss-cross 多父 ⬜）|
| I-D3 | manifest CAS 单调 | 版本号递增、发布原子（失败重试不留半状态） | 隐式（乐观提交重试）⬜ 显式测试 |

### I-F fencing / 租约（⬜ reviewer 补充：I-F3 P0）

| # | 不变式 | 内容 | 检测现状 |
|---|--------|------|---------|
| I-F1 | epoch 唯一单调 | acquire 条件写领取 max+1，无物两主 | multi_node 8 项 ✅（须入账本目录）|
| I-F2 | 过期写者零新副作用 | 租约过期后无新 WAL 段/manifest 推进 | ⚠️ 缺口：flush_loop 不查租约（fence.rs 诚实边界承认靠回放消解）|
| I-F3 | 脑裂收敛 | 双 epoch 并发 ack 的提交恢复后收敛为高 epoch 串行历史 | ⬜ P0 零覆盖（opfuzz 双 Database 扩档）|

### I-G GC 墓碑（⬜）

| # | 不变式 | 内容 | 检测现状 |
|---|--------|------|---------|
| I-G1 | 墓碑与停止引用同版本原子发布 | 滞后读者不读被删对象 | ⬜ 无专项测试 |
| I-G2 | 保留窗口覆盖假设 | retention ≥ 最大读者停顿 ∧ keep-16 ≥ 读者代数 | ⬜ 假设未成文 |

### I-H 列存一致（⬜）

| # | 不变式 | 内容 | 检测现状 |
|---|--------|------|---------|
| I-H1 | AP/TP 同快照一致 | col_segments + col_deletes ≡ 行树同 checkpoint 可见行 | ⬜ 差分对拍缺 |
| I-H2 | col_deletes 与 reinsert 抑制 | 重插 key 不被删抑制 | ⬜ |

### I-E 资源守卫（✅ 已闭合）

连接数（panic 安全/竞态）/ 分支数（CAS 内检查）/ 单事务字节（Pass1 无副作用
拒绝）/ 语句超时（57014 逐语句重置）/ 会话配额（prepared/cursor/结果集）/
计划缓存（4096 满即清空，plan_cache_is_bounded）——见 resource_limits.rs
15 项 + plan_cache.rs 6 项。

计划缓存正确性不变式（I-E4）：缓存对象是**纯 AST**——执行期名字解析、
schema 绑定、快照选取全部逐次执行，因此 DDL 漂移（加列/重建同名列型）
对命中路径即时可见（ddl_drift_does_not_poison_cached_ast 固化）。

## 2. 信任阶梯（dendro 版）

| 层 | 对象 | 工具 | 状态 |
|---|---|---|---|
| L0 语言安全 | 全部 | unsafe deny（部分 crate）、clippy -D warnings | ✅ |
| L1 无 panic | 格式编解码 / WAL FrameIter / 范围提取 | Kani 0.67 | 🚧 harness 起步 |
| L2 设计正确 | 两段式提交 / 水位 / OCC | TLA+ TLC | ✅ CommitPipeline；⬜ OCC/合并模型 |
| L3 函数级正确 | encode_key 保序 / zigzag 类 / CRC 容错 | Verus（引入待定） | ⬜ |
| 外壳 | wire 协议 / 真实 OSS / tokio | e2e 对拍 + opfuzz chaos | ✅ 部分 |

## 3. 故障模型单一事实源

`crates/dendro-core/src/objstore/sim.rs`（SimObjStore）：
pending/committed 二态（写→pending，fsync→committed）、crash() 丢 pending、
torn_write_prob、enospc_after、fail_writes 确定性注入。
消费方：opfuzz、资源回归、（未来）TLA+ 环境动作。
**变更纪律**：改 sim.rs 语义必须同 PR 更新本节 + opfuzz 断言 + 相关回归。

## 4. 工具链

TLC（tools/tla2tools.jar，basalt 同版）、Kani 0.67.0（verification/kani/
standalone harness 口径）、Verus（未引入；触发条件见 basalt §4.1 同款协议）。

## 12. 缺陷 → 检测机制矩阵

> 原则：每个已发生的缺陷必须落一个永久检测机制。只修不加机制 = 未完成。

| # | 缺陷 | 类别 | 抓住它的机制 | 永久机制 |
|---|------|------|-------------|---------|
| 1 | WAL flush 并发同段覆写 | 并发交互 | 探针（P0-A） | flush_mu 单飞 + 并发同段回归 |
| 2 | put_batch 错误吞没（spawn 期检查） | 异步检查时序 | checkpoint 注入探针 | join 后权威检查 + 回归 |
| 3 | 段号空洞（失败也推进 cur_seg） | 持久化序不变量 | 恢复测试 | 成功后推进 + 回归 |
| 4 | checkpoint 失败丢 committed 数据 | 失败结算 | 注入测试 | key-cover 归还 + 回归 |
| 5 | FSST 不可压输入越界 panic | 不可信输入边界 | write_cbf 探针 | 2×+8 预置 + switch-on 回归 |
| 6 | 负数字面量 IN 丢行（R8） | 字面量提取句法盲区 | 全表面探针 | expr_to_literal 统一 + negative_literals 回归 |
| 7 | 负数字面量 Int32 取负失效（R6） | 数值宽度窄化盲区 | 范围探针 | Int32 分支取负 + 回归 |
| 8 | 水位前沿公式双形态错误（R4：自含 in-flight / 以自身 ts 代 installed_max） | 派生状态公式错误 | ap 回归 + 并发计数 | 无间隙前沿统一助手 + out-of-order 回归 |
| 9 | 矛盾范围 BTreeMap panic | 边界判定次序 | 审计探针 | 空判定前置 + 矛盾范围回归 |
| 10 | 分支上限 CAS 竞态 | 检查与写入原子性 | 审计分析 | 闭包内权威复查 + 32 线程竞态回归 |
| 11 | 连接守卫 panic 泄漏 | 恐慌安全缺口 | 审计分析 | ConnSession Drop + catch_unwind 回归 |
| 12 | 段退休含在途帧（R6-P0） | 退休界与安装前沿解耦缺失 | 复核 agent 探针 | retire_bound(covered) + 回归 |
| 13 | 毒化热旋 100% CPU（R4-F1） | 状态机停等缺失 | 复核 agent CPU tick 探针 | 毒化挂起 + CPU tick 回归 |
| 14 | **in-flight 摘除不推进水位（R8-WM）** | 摘除路径前沿缺失 | **TLA+ liveness（StallFreedom）反例** ✅ 首例 | recompute_watermark_on_removal 统一 + watermark_recovers 回归 |
| 15 | ConnGuard exit 非原子 RMW（并发 enter 覆盖 → 慢泄漏 + 伪 53300） | 计数器 RMW 竞态 | 形式化评审 agent 探针（8 线程×30 万次，泄漏 62 + 35901 伪拒绝） | exit 对称 fetch_update + 并发配额测试 |
| 16 | 模型 drop 路径未回灌水位重算（模型-实现精化桥断裂）+ Makefile tail 吞 exit 13 | 精化桥断裂 + 门禁失效 | 形式化评审 agent 复验（账本 ✅ 不实） | drop 动作补 frontier 公式 + Makefile 去 tail |
| 17 | if-let 审视位 MutexGuard 活到 if/else 尾，miss 分支再 lock() 自死锁（P2-6 计划缓存首执行即挂） | Rust 临时值存活期陷阱 | cargo test 0% CPU 挂死复现 | guard 显式落语句（`let hit = …; match hit`）+ plan_cache.rs 全套命中路径测试 |
| 18 | ALTER TABLE ADD COLUMN 用新列空 pk 整体覆盖 schema.pk → 表永久失去主键，后续 INSERT 全部报 "has no primary key" | DDL 目录字段覆盖 | 计划缓存 DDL 漂移行为测试（ALTER 后 INSERT 即暴露） | AddColumn 保持原 pk 不变 + ddl_drift 回归 |
| 19 | **FrameIter 段尾守卫 `remaining ≤ TRAILER_LEN 即停` 把总长 ≤32B 的小帧当 trailer 丢弃 → 封段回放静默丢已确认提交（P0）**；连带发现 Kani 同源副本 FRAME_MAGIC 漂移（副本 0x4C41_5345 vs 真实现 0x4F524E44）长期无告警 | 边界字节数启发式错误 + 同源副本无机制保障 | **Kani H2 对偶反例**（2B 载荷 = 26B 帧 < 32B 被吞；旧 guard 下 H2/H1 从未真正进入解码路径——"3/3 绿"含假阴性覆盖） | next_frame 按魔数识别段尾（DRNO≠ESAL，垃圾尾改报 Err）+ H4 小帧回归 + constants-sync 同步守卫测试 + H1 重编码（整块符号化 + 固定 3 次调用 + unwind 定界：符号长度循环逐轮自动加界曾 >9min 不收敛） |
| 20 | 单列 pk 表 `id NOT IN (…)` 静默返回 0 行：点查下推**资格判定**未拒 negated（scan.rs:1183）而取键已拒（1248）→ 空键集 = 空 TableView | 资格判定与取键条件不对称 | 附录 A 编写 agent slt 实证（ir-spec 03a §A.4） | 资格判定同拒 negated 回落通用谓词 + not_in_regression 3 测试 |
| 21 | LEFT JOIN NULL 键互相匹配：`" NULL"` 哨兵是死代码（to_text(Null)=''），join 键无类型 tag → NULL↔NULL、NULL↔''、Int64(1)↔Utf8("1") 可碰撞（实证 `1\|1\|NULL`） | join 键编码丢失类型信息 | 附录 A agent 实证（03a §A.4 Q3） | **未修**（P1）：join 键改带类型 tag 编码，v2b HashJoin 算子合同一并定；差分语料含碰撞用例 |
| 22 | JOIN ON 的 AND 链中非等值合取被**静默丢弃**（walk 返回值被忽略）→ `ON a.id=b.id AND a.v>1` 不过滤 v | 下推走查未回收残留条件 | 附录 A agent 实证（03a §A.4 Q4） | **已修**（v2c-2 前置）：残留由父 And 收集、逐候选对求值（LEFT：残留全不成立=无匹配→NULL 延展）+ join_semantics 回归 |
| 23 | `count(*) WHERE <恒 NULL 谓词>` 无结果行（常量短路跳过聚合）vs `WHERE 1` 返回 count=0——两条路径不一致 | 常量谓词短路与聚合空集语义分叉 | 附录 A agent 实证（03a §A.4 Q5） | **已修**（v2c-2 前置）：恒假短路仅在无聚合时生效（has_agg 判定前移）；join_semantics #23 用例固化"空输入全局聚合单行"唯一语义 |
| 24 | **AP 路径单边 pk 范围返回 0 行**（`id > N` / `id >= N` 整段被剪）：三处段/行组剪枝用 Option 序比较，`hi=None`（开上限）使 `None <= Some(min)` 恒真；双边界/等值不触发 → 长期潜伏 | Option 排序陷阱（None≠无界语义） | **v2c-1 force_source 差分首跑即抓**（main vs fallback 行数 6:1）；分层探针定位（无 overlay 即错→剪枝层） | `is_some_and` 判定（None=无界）三处同修（scan.rs 段剪枝 + integrate.rs 段级/行组级 zone map）+ dispatch_differential range 差分固化 |
| 26 | **字符串字面量类型系统性丢失**：value_from_parser 对 SingleQuotedString 也走 number_or_string——`'1'`→Int32/Int64、`'1.5'`→Float64（TEXT 列插 '1' 读回 Int64；多行插读回 Null——读路径疑似按列型改型/置空，伴生待查）。PG 语义应为 unknown 字面量按目标列采纳 | 字面量类型启发式越权（引号串≠数字） | v2c-2 join 测试前提被污染顺藤摸瓜（探针实证 text_col=[[1,"x"],[2,Null]]） | **已修**（v2c-2 前置独立 PR）：三处同修——① value_from_parser 引号串恒 Utf8（Number 保持窄化定型）；② INSERT coerce_for_column 按列采纳（Utf8→数值/日期/布尔解析、数值→TEXT 文本化、同族宽/窄化，22P02/22003 错误面）；③ embed to_result 按**列元数据**定型（此前对文本化结果重新猜类型——测试视角污染源）。回归：literal_typing 4 测试；期望更新：plan_cache（SELECT 1 → Int32 窄化定型）、ap_tp（Rust 风格 99_999 下划线字面量曾靠启发式侥幸）、explain（dispatch 行序）；slt 26 文件零改动全绿 |
| 25 | **ap_tp_differential（I-H1）自成立以来空转**：夹具未 `set_columnar` → AP 路径静默不可用 → 两侧恒走行路径，"差分通过"无覆盖 | 差分夹具未验证被测路径真的参与 | v2c-1 新差分首跑暴露（新夹具接线后真差分立即可用） | 夹具补 set_columnar（I-H1 修真）；教训入账本：**差分测试必须断言两路径确实分叉过**（如 force_source 下 EXPLAIN/计数探针） |
