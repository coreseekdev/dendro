# pgrust 与机会式求值 JIT 设计调研（2026）

> 调研时间：2026-09-18。触发：用户指定
> <https://github.com/malisper/pgrust>（参考设计）与
> <https://arxiv.org/abs/2405.11361>（参考 JIT 设计）。两者同源——论文
> 一作 Stephen Mell 即 pgrust 作者（GitHub: malisper）。结论先行：
>
> 1. **pgrust = Postgres 的完整 Rust 重写**（线协议 + SQL 方言兼容，
>    46,066/46,066 回归全过；ClickBench 比 ClickHouse 快 18.5%、比
>    PG 快数百倍；sysbench-oltp 只读快 30%）。核心重构件：向量化
>    push 型 **JIT 编译执行器**、线程化并发模型、查询隔离调度器、
>    内建 OOM killer。对 dendro 最直接的参照是它的**表达式 JIT 设计**：
>    copy-and-patch + 手写 AArch64 模板、无 LLVM 依赖、JIT 与解释器
>    **逐 Step 可互换**、全面 fail-open——这是 dendro ScalarProgram
>    步进 VM 向 JIT 演进的现成配方。
> 2. **论文（OOPSLA'25）是 JIT 之上的求值语义框架**：λO 三步语义
>    （归约/派发/解决）把"外部调用即到即并行派发 + 结果流式回填"
>    形式化，confluence 定理保证任意派发序等价。对 dendro 的意义：
>    执行被 S3 GET 等外部调用支配，机会式派发是并行读/预取/流水线
>    化的**语义依据**（任意交错安全 ⟺ 差分可枚举）。
> 3. **意外收获：proofs/ 目录是 Kani/CBMC 机器检查的 C≡Rust 双执行
>    等价证明**（1,086 个 SQL 内建函数对逐字 PG C 证明等价，顺带挖出
>    8 个 pgrust bug + 4 个 PG 上游 bug）——dendro 差分文化的方法论
>    升级方向（P1）。
> 4. 诚告：AGPL-3.0——**只读设计，不抄代码**；性能数字是 Graviton4
>    特化（neoverse-v2 构建），不可直接比；官方明言尚非生产就绪。

## 1. pgrust 概况

| 维度 | 事实 |
|------|------|
| 定位 | "2026 年视角重建的 Postgres"，Rust 全量重写（对照 PG 18.6 源码逐函数移植） |
| 兼容 | 线协议 + SQL 方言兼容；**46,066/46,066** PG 回归测试全过 |
| 性能 | ClickBench 综合：比 ClickHouse 快 18.5%（用自带列存 pgrcolumnar）、比 PG 快数百倍；sysbench-oltp 只读 300GB 快 30%（fsync 开）；数字经 Greg Smith 独立评审 |
| License | **AGPL-3.0**（传染性——设计参考可以，代码搬运不行） |
| 成熟度 | 自述"尚不建议生产"；#1 优先级 = 测试与可靠性 |
| 规模 | 40 万+文件（大量是 proofs/ 证明工程与 fuzz 目标） |

四大重构件：① 向量化 push 型 JIT 编译执行器；② 线程化并发模型；③
查询调度器（防单查询拖垮全库）；④ 内建 OOM killer（内存压力下自查
自控，而非被内核 OOM 击杀）。

## 2. 表达式 JIT 设计深读（execexpr::jit，本次核心）

pgrust 的表达式求值与 dendro 同构：先编译为 **Step 步列表 VM**
（`steps.rs`，2,482 行——对应 dendro `scalar.rs` 的 ScalarStep，
同样"大负载进侧表、步内只存索引"），解释执行（`interp.rs`）。
JIT 是其上的 copy-and-patch 层（`jit.rs`，1,766 行 +
`jit_deform`，580 行）：

| 设计点 | 做法 | 对 dendro 的启示 |
|--------|------|------------------|
| 代码生成 | **手写 AArch64 模板**（u32 指令字直出），无 LLVM/Cranelift 依赖；发射器 = `Vec<u32>` + 分支 fixup 表 + 字面量池；**分支位移溢出 → 整体放弃编译回落解释器**（不装截断目标的内核） | 编译延迟极小（无外部编译器进程/库）； dendro 若做 x86_64 需自写模板族，aarch64 可直接对照此设计 |
| 覆盖策略 | 每个 Step opcode **要么开编码模板、要么 `bl jitq_step` 调解释器自己的臂**——JIT 与解释器**逐 Step 可互换**（内核中途落回解释器再回来） | 关键架构决策：JIT 是纯加速层而非第二语义面——**不产生"两条可执行路径"**（对照 dendro 评审 checklist 第 9 条：单一语义面） |
| 状态模型 | 程序状态 = 解释器自己的状态（out 单元/fcinfo/anynull 草稿），**地址稳定（mcx 钉址）→ 模板直接烘焙绝对地址** | dendro 的 ScalarProgram 侧表天然可钉址（Arc 稳定）；行值数组需 SoA 化或地址稳定化 |
| 内核 ABI | `extern "C" fn(ctx: *mut JitCtx, start_step: u32) -> i64`，返回码 DONE_RETURN / DONE_NORETURN / **ERR** / **SUSPEND**；挂起（SubPlan）经标签表重入续跑 | 挂起重入设计值得抄：dendro 的 L2 相关子查询迭代求值同构于 SubPlan 挂起 |
| 边界纪律 | panic 在 `extern "C"` 边界捕获、Rust 侧续抛（JIT 帧无 unwind info）；错误经 env stash 返回 | 无展开假设下保持 Rust panic 语义——dendro 直接适用 |
| 内存纪律 | W^X arena（线程本地分块，时间性 W↔X）；跨线程代码缓存用 `SharedCode`（**只在安装窗口可写，之后不可变 RX**——比 arena 更强，故可多线程并发执行） | 两条分配面分工清晰；安全论证写进注释的方式值得学 |
| 门控与退路 | 会话旗标（PGJIT_PERFORM/EXPR）+ estate 归属校验 + 环境开关（`PGRUST_JIT_DEFORM=0`）+ arena 满 → **全部 fail-open 回解释器** | fail-open 与 dendro compile-or-fallback 同哲学 |
| 覆盖证据 | `JitStats{compiled, refused, arena_full, runs}` 常驻计数；**落地门槛 = 回归语料 JIT 强开下零拒绝**；另有 fuzz 对拍（`fuzz_parity_vs_interpreter`——"Miri 无法跑生成代码，fuzz 对拍即证据标准"） | 度量先行 + 明确落地门槛——dendro SLT 全过即天然对拍语料 |

**tuple deform JIT**（jit_deform）：按 TupleDesc 生成解形内核，Row
ABI（values/isnull 双数组）与 Soa ABI（列主存跨步）双出口；内核以
`Rc::ptr_eq` 钉死发射期描述符身份（"identity 比较永不 ABA"）。这是
PG LLVM-deform 的 copy-and-patch 替身。

**lane 向量化层**（lanereg/laneexec）：OID → 批执行层（tier）的
**单一准入注册表**（const 静态表、零热路径开销），各消费者（AOT 比较
位图/JIT 内联算术/聚合折叠/SIMD 缝合）统一查询；**覆盖漂移**由一致性
测试钉死（消费者表与注册表不符即 fail）——与 dendro"单一语义面"同
一问题的另一种解法（他们用注册表防多表漂移，我们用计划 IR 收口）。

**AI 经济学**：作者的核心论点（博客 "How AI Changes the Economics
of JIT Compilers"）——手写模板历来成本高企，LLM 代码生成把"逐指令
写模板 + 对拍验证"的成本压到值得做的水平，copy-and-patch 因此在
2026 年复兴。这对 dendro 的直接含义：**模板族可以 AI 辅助生成 +
fuzz 对拍验证**，工作量不再是阻止因素。

## 3. 论文：机会式并行 λ 演算（λO / Opal）

问题设定：async/await 手写并行正确但维护成本高；自动并行（如
Rayon/ForkJoin）只对 CPU 任务有效。λO 面向**执行时间被外部调用
（IO/网络/DB）支配**的程序——恰好是对象存储数据库的画像。

- **三步语义**（ANF 语法 + 语句标号保证可结合性）：
  1. *归约*（内部计算）；
  2. *派发*：外部调用就绪即**立即派发**，原地换 `TASK` 占位符；
  3. *解决*：`TASK → 结果`，结果可以是**含洞的部分项**（PARTIAL）。
- **定理**：*confluence*（任意派发/解决交错终态唯一——顺序自由 =
  差分可枚举 = 调度器自由）；*公平性*（每个可达外部调用终将执行——
  无饿死）；*可靠性*（终止程序下与顺序求值同值）。
- **流式**：Church 编码的部分数据，洞可在**中段**（不止尾部）——
  比"增量 append"更一般的流式语义；消费者用偏序比较在洞补齐前
  部分推进。
- **排序**：数据依赖显式重化（handle 线性传递），副作用顺序不靠
  求值序而靠依赖边——与 dendro 的 commit_mu/两段式提交同思想。
- **结果**：Opal（Python）对真实 IO 稠密负载 6.2× 运行时 / 12.7×
  延存改进；与**手写 async Rust 的差距 1.3–18.5%**——自动派发接近
  手工优化的下界。实现要点：图表示（语句/变量双顶点 + 双向映射）。

## 4. 对 dendro 的对照与借鉴清单

| 维度 | pgrust | dendro 现状 | 评 |
|------|--------|-------------|-----|
| 表达式执行 | Step VM + copy-and-patch 模板（逐 Step 解释器退路） | ScalarStep VM（侧表化步指令 + compile-or-fallback + 文本 IR round-trip） | 同构起点；dendro 已具备 JIT 化的全部前置（小步指令/侧表/回落合同） |
| 批量执行 | lane SIMD 层 + 注册表准入 | ROW_BATCH=1024 push 管线（FilterOp/AggOp/Sink） | dendro 批式已对齐；SIMD 层未做 |
| 验证 | Kani C≡Rust 双执行证明 + fuzz 对拍 | 差分轴（optimize/force_source/force_agg）+ SLT + golden | 方法论可升级（见 P1-1） |
| 求值语义框架 | ——（论文层：机会式派发 + confluence） | 同步扫描；S3 读在路径上串行等待 | 机会式派发 = 并行读的语义依据（见 P1-2） |
| 容错 | 全面 fail-open + kill switch + stats | compile-or-fallback + 毒化终态 | 同哲学 |

### P0（JIT 落地路径，若立项）

1. **以 ScalarProgram 为语义锚的 copy-and-patch 层**：
   - 决策一：目标架构。pgrust 只做 AArch64（Graviton4）；dendro 的
     部署面（对象存储云原生）两架构都要——建议 aarch64 先行（有
     pgrust 设计可对照），x86_64 次之；
   - 决策二：模板 vs Cranelift。模板：零依赖、编译延迟 µs 级、
     AI 辅助生成可行（pgrust 论点）；Cranelift：覆盖快但引入重依赖
     与编译延迟。倾向模板 + 逐 Step 解释器退路（覆盖零缺口 by
     construction）；
   - 合同三条直接沿用 pgrust 验证过的形状：**挂起重入**（标签表 +
     SUSPEND 返回码——L2 相关子查询同构）、**分支位移溢出整体
     放弃**、**W^X/不可变 RX 双分配面**；
   - 落地门槛同款：**SLT 34 文件 JIT 强开零拒绝** + 解释器/JIT
     fuzz 对拍（每步值位级相等）+ `dendro.jit=off` 开关 + stats。
2. **前置项**：ScalarProgram 状态钉址审计（侧表 Arc 化）、行值
   SoA 化评估（deform 同款 Row/Soa 双 ABI 的取舍）。

### P1（方法论，小切口）

1. **Kani 双执行等价证明试点**：dendro 没有逐字 C 参照，但有同构
   双实现——`expr.rs eval`（AST 直接求值）vs `scalar.rs`
   ScalarProgram 解释执行。对纯函数子集（比较/算术/NULL 语义）建
   Kani harness 符号输入双执行等价，替代/加固现有附录 A 行为基准；
   pgrust 的证据分级值得照抄（`proved(bounds)` > `tested(differential)`
   > 无证据）。
2. **机会式派发框架用于 S3 并行读**：与 ScopeDB 调研的"确定性 I/O
   规划 + 并发 range read"合流——论文给的增量是**语义合同**：
   独立读即到即派发、任意交错 confluence（⟹ 差分轴天然成立）、
   中洞部分数据允许下游部分推进（prelly 节点边到边用）。落到
   dendro 即 table_scan 的 range 并发预取 + `TableView` 部分构造；
   P2' 多节点时同一框架升级为跨节点派发。
3. **查询调度器 / OOM killer**（pgrust 四骑士之二）：dendro 已有
   stmt_deadline/cancel_token；补"单查询资源隔离"与"内存压力自查"
   的设计参考，进 P2 观察清单。

### P2（跟踪）

- sqe 模板查询引擎（参数化模板族 + 条件缓存 + spill）——ClickBench
  数字的来源，但与 dendro 的 prolly/列存路线耦合度低，仅跟踪；
- pgrust 的 proofs/ 工程化细节（SUITE.tsv 台账、覆盖审计、账本）。

## 5. 诚告边界

- **AGPL-3.0**：本调研只取设计思想；任何代码级借鉴都会触发传染——
  禁止抄代码，模板手写（AI 生成 + 对拍）；
- 性能数字均来自 Graviton4 特化构建（`-Ctarget-cpu=neoverse-v2`），
  通用构建不可复现；官方自述非生产就绪、"仍有许多 bug"；
- pgrust 是单机页存储 Postgres 形态——**无对象存储/内容寻址/分支
  语义**，其 WAL/存储层与 dendro 无对照价值；可借鉴面集中在
  表达式 JIT、验证工程、执行器形状三处；
- 论文的 Opal 实现是 Python 级（graph 表示），性能数字对照的是
  Python 串行与手写 Rust——量级参考，非 DB 场景直接证据。

## 6. 信源

- 仓库：<https://github.com/malisper/pgrust>（clone 实读，main 分支，
  2026-09-18）：`crates/backend/executor/execexpr/src/{jit.rs,steps.rs,
  interp.rs,compile.rs}`、`crates/_support/jit_deform/src/lib.rs`、
  `crates/backend/executor/lanereg/src/lib.rs`、`crates/backend/
  executor/sqe/`、`proofs/README.md`
- 论文：Mell, Kallas, Zdancewic, Bastani.
  *Opportunistically Parallel Lambda Calculus*. OOPSLA 2025.
  <https://arxiv.org/abs/2405.11361>（HTML 全文实读）
- 作者博客（README 指引，未逐一核读）："How AI Changes the
  Economics of JIT Compilers"、"Rebuilding Postgres for 300x faster
  analytics"、"pgrust: rebuilding Postgres in Rust with AI"
- dendro 侧对照：`crates/dendro-core/src/sql/scalar.rs`（ScalarStep
  合同）、`crates/dendro-core/src/exec/pipeline.rs`（ROW_BATCH/drive）、
  `docs/research/ScopeDB调研.md`（并发 range read 先行结论）、
  `docs/design/设计评审checklist.md`（单一语义面/fail-open 条款）
