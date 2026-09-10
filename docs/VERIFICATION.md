# 验证账本（dendro）——形式化验证 / 模型检查 / 测试 的总纲

> 落实 AGENTS.md 的验证体系。参考同工作区 basalt 的方法论
> （../stream-db/basalt/docs/VERIFICATION.md），按 dendro 架构（追加-only
> 内容寻址 + 两段式提交 + 分支化）重写。
> 测试回答"跑过的没坏"，验证回答"**没跑到的也不坏**"。

## 0. 状态

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
| I-A6 | **StallFreedom**（活性） | installed_max 到顶后 watermark 必追平 | 已 ack 行永久不可见（R8-WM TLC 反例→修复） | TLC liveness ✅ |
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
| I-C6 | FrameIter 不可信输入 | 任意字节输入不 panic（Err/None 而非 UB） | 网络可达 DoS | 边界测试 ✅ + Kani harness 🚧 |
| I-C7 | 水位停滞免役 | in-flight 全部摘除后（含失败路径）watermark = installed_max | 已 ack 行不可见（R8-WM TLC 反例） | watermark_recovers ✅ + TLC StallFreedom ✅ |

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
拒绝）/ 语句超时（57014 逐语句重置）/ 会话配额（prepared/cursor/结果集）——
见 resource_limits.rs 15 项。

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
