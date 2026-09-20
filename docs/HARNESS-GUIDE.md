# 形式化验证 Harness 实施手册（人 / AI Agent 双读者）

> 定位：[VERIFICATION.md](VERIFICATION.md)（验证总纲与账本）的实施侧配套——
> **怎么设计、怎么写、怎么判断遇到墙、怎么记账**。总纲回答"验证什么"
> （§1 不变式目录），本手册回答"怎么做与做到什么程度"。
> 读者：工程师；以及被指派"给 X 写形式化验证"的 coding agent——
> 规则带编号（K/W/D/P 系列）供引用，违反 K 系列的"完成"声明无效
> （同 AGENTS.md 口径）。

## 0. 一页速览（Agent 先读这段）

1. 先分类再动手：性质 → 层（Kani/Verus/TLC/fuzz）→ 绑定级 → 输入空间（§1）。
2. 证明对象必须是**真源编译**（B2），禁止镜像副本（K1，历史上三连坑）。
3. 每个 harness：显式 `#[kani::unwind(N)]` + 注释推导 N（K4）。
4. 符号数据用栈数组不用符号 Vec；调用次数固定不 while-let（K5/K6）。
5. cover! 放输入整形侧；assume 声明输入域（K7/K8）。
6. 超时 ≥10 分钟 = 设计问题不是算力问题：按 §3 墙识别表处置，允许停泊，
   停泊必须记账（W0）。
7. 完成的定义 = §5 DoD 清单全勾，含 verify.sh 接入与账本/度量更新。

---

## 1. 第一步：需求分类（不是所有"验证"都该写 harness）

接到"验证 X"的需求，按顺序回答四个问题：

### 1.1 性质是什么类型？

| 性质类型 | 例（本仓库实证） | 层 | 工具 |
|---|---|---|---|
| 任意输入不 panic / 无 UB | FrameIter 对任意字节（I-C6） | L1 | Kani |
| 编解码对偶（roundtrip） | encode_frame ↔ FrameIter | L1 | Kani |
| 拒绝路径完备（坏输入必拒） | 魔数/版本/帧型/len/CRC ×5 | L1 | Kani |
| 协议设计正确（安全+活性） | 水位不停滞（I-A6 StallFreedom） | L2 | TLA+ TLC |
| 函数级数学性质 | order 域对合/保序（stats.rs） | L3 | Verus |
| 端到端语义（真 OSS/wire/tokio） | crash 后 ack 数据可见 | 外壳 | opfuzz + e2e |
| 性能 | — | 不可证 | bench |

**预期管理**：L1 可承诺"全输入空间"（这是它区别于 fuzz 的全部价值）；
L2 可承诺"模型内全状态空间"，但模型↔代码的保真度要单独交代；
外壳层**不可形式化**，只能采样（opfuzz/e2e）——不要对需求方过度承诺。

### 1.2 对象绑定到哪份代码？（绑定级，vLOC 度量的轴）

| 级 | 含义 | 本仓库现状 |
|---|---|---|
| B2 真源编译 | harness 经 `#[path]`/`#[cfg(kani)]` 编译生产文件 | wal/codec.rs（dendro-kani） |
| B1 镜像 | 手工副本 + 同步测试 | Verus 3 文件（待升级） |
| B0 自由镜像 | 无同步机制 | 已灭绝（#19 教训后） |
| 模型级 | TLA+ 抽象模型 | CommitPipeline.tla |

### 1.3 输入空间多大？
符号变量数 × 每变量位宽 × 触达循环深度 = 成本主项（§4 预期表）。
40 字节缓冲 + unwind 17 ≈ 90s 是已实证的舒适区上限附近。

### 1.4 值不值得做？
优先级 = 不可信输入边界 > 协议状态机 > 纯函数核心 > IO 胶水/wire 壳。
判据：该模块是否挂在 §1 不变式目录的某条 I-XX 上。没有对应不变式的，
先立目录条目（性质定义）再写 harness——**先承诺后证明**，顺序不可反。

---

## 2. Kani harness 设计规则（K 系列）

### K1 绑定：真源编译，禁止镜像副本
- harness 所在包（`verification/kani/`）用 `#[path = "..."]` 直接编译
  生产模块。改生产代码 = 改证明对象，无需任何同步机制。
- **历史三连坑**（全部真实发生，见 §6 表）：FRAME_MAGIC 副本漂移
  （#19）；镜像手写 CRC 而真实现是 SIMD——CRC 从未被证明过；
  真 FrameType 四变体副本只写了两——Fence/Seal 帧零探索。
- 镜像的唯一残余合法性：Verus 旧文件（B1），新工作禁止再产生。

### K2 依赖面：验证对象模块依赖必须最小
被 `#[path]` 编译的模块只允许：纯数据类型 + 无副作用的构造器 +
已建模型的原语。新增依赖（尤其 tokio/object_store/arrow 系）会使
整个包在 Kani 工具链下不可编译或不可达——**改 codec 依赖 = 同 PR
评估绑定**（codec.rs 头注释有同款警告）。

### K3 原语墙：SIMD/asm/cpuid 用"规范模型 + 差分测试"闭合
- 实证：crc32c crate 经 `is_x86_feature_detected` → `__cpuid_count`
  内联汇编，Kani 报 615 项 UNDETERMINED，整包不可验。
- 模式（已落地，可复制）：
  1. 写**规范定义**实现（按标准文档逐位/逐公式，非抄优化版）：
     `verification/crc32c-soft/src/lib.rs`；
  2. 验证包里以依赖别名顶替：`crc32c = { package = "crc32c-soft" }`，
     生产源码原样编译；
  3. **差分测试**实证 模型 ≡ 真实现：已知向量 + 0..=256 全长度扫描
    + 跨分块边界大块（`tests/diff_real.rs`，<1s），入门禁。
- 缝隙口径：这是绑定链上唯一以测试闭合的环节，必须在账本"残余缝隙"
  清单里申报（VERIFICATION.md §5）。

### K4 unwind：每个 harness 显式定界并推导
- `#[kani::unwind(N)]` 必写，注释给出推导：N ≥ 触达最深循环的
  迭代数 + 守卫轮。例：软 CRC 内循环恰 8、外循环 ≤16B 载荷 → 17；
  encode_trailer 内 CRC 28B → 32。
- **无属性的默认界展开不可复现**：实证 payload_len harness 无属性时
  symex 把 CRC 循环展开到 282+ 轮不收敛，显式 `unwind(9)` 后 6s 过。
- N 是该证明的**完备性上界**（类比测试"跑到/没跑到"）：缩小 N 须
  说明覆盖损失，unwinding assertion 失败 = 覆盖悬崖，不许调大 N 硬压。

### K5 符号数据：栈数组优先，警惕堆
- 符号字节进堆分配（`vec![kani::any(),..]`、`Box`）会引入分配器
  建模噪声，本工具链（Kani 0.67 + nightly-2025-11-21）曾产生**虚假
  反例**（旧镜像 H2 注释存证）。用 `[u8; N] = kani::any()`。
- `Vec::clone` 的分配建模会让下游循环长度符号化 → symex 爆炸
  （payload_len 实证：去 clone 即愈）。
- **嵌套 Vec（Vec<结构体{Vec, Option<Vec>}>）是当前工具链的墙**：
  drop/dealloc 路径 symex 爆炸（3800+ aborting paths，480s 不收敛，
  去 clone 无效）——H5 txn 往返因此停泊。此类对象只能：具体向量
  回归兜底 + 等堆模型改进（W3）。

### K6 迭代结构：固定次数，不 while-let
- 符号长度输入上的 `while let Some(..) = it.next()` = 无界迭代 ×
  符号长度 → 展开空间乘法爆炸（旧 H1 实证 >9min）。
- 模式：`for _ in 0..3 { it.next_frame() }` 固定推进次数，次数 =
  界内可完整解析的最大帧数 + 1。

### K7 输入域：assume 显式声明，漏了就是真反例
- 符号判别值进 `unwrap` 前必须 assume 定义域。实证：H2 漏
  `assume(tv >= 1 && tv <= 4)`，`from_u16(tv).unwrap()` 被打出的
  反例 tv=0——**这是 harness bug 不是产品 bug，但反例是真的**，
  说明证明器在认真工作。
- assume 只能约束"输入本就满足的域"（构造合法性的表达），禁止
  assume 结论（= delete-to-pass 的形式化版，AGENTS.md §4 同罪）。

### K8 cover：路径覆盖的声明与放置
- `kani::cover!(cond, "Pxx 中文描述")`：Kani 证可达性 + 给见证输入；
  UNSATISFIED 必须解释（路径死 = 发现死代码；过约束 = harness bug）。
- **放输入整形侧，不放任意缓冲侧**：在任意符号缓冲上证明"帧成功
  产出"可达 = 求解器反解 CRC（GF(2) 级联 128 级 XOR），实证 19min
  不收敛；移到"先 encode 再 iterate"的整形 harness 后免费。
- 原因级 cover（区分 magic/version/type/len/CRC 拒绝）用**输入整形**
  实现（构造只可能触发该原因的输入），不做错误消息字符串比较。
- 命名规范：P + 序号 + 动宾短语；在 verif-coverage.py 输出里按此聚合。

### K9 性质模板（复制起点）

```rust
// 对偶（H2 式）：符号化定义域内的全量往返
#[kani::proof]
#[kani::unwind(9)] // 推导：CRC 内 8 + 外 ≤2
fn xx_roundtrip() {
    let t: u16 = kani::any();
    kani::assume(/* 定义域 */);
    let enc = encode(...);
    kani::cover!(true, "P2 对偶");
    assert_eq!(decode(&enc).unwrap(), /* 期望 */);
}

// 拒绝路径（P3 式）：整形到只可能因该原因失败
#[kani::proof]
fn xx_bad_yyy_rejected() {
    let mut m = encode(GOOD);
    let v: u16 = kani::any();
    kani::assume(v != GOOD_VAL);
    /* 只改 v 字段 */
    kani::cover!(true, "P3x 该原因拒绝");
    assert!(matches!(decode(&m), Some(Err(_))));
}

// 任意输入不 panic（H1 式）：栈缓冲 + 符号长度 + 固定推进
#[kani::proof]
#[kani::unwind(17)]
fn xx_arbitrary_never_panics() {
    let buf: [u8; 40] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 40);
    let mut it = XIter::new(&buf[..len]);
    for _ in 0..3 { let _ = it.next(); }
}
```

---

## 3. 墙识别与处置（W 系列）——超时是信号不是失败

**W0 总则**：单 harness ≥10 分钟不收敛 = 设计问题，禁止硬磕/加 timeout
硬等。按下表识别 → 改设计 → 仍不通则**停泊**（harness 保留 + 注释
写明墙与待解条件 + 不入门禁 + 账本记录 + 用回归测试兜底性质）。

| # | 症状（日志特征） | 根因 | 处置 | 首次实证 |
|---|---|---|---|---|
| W1 | `Unwinding loop ... iteration N` 持续增长（数百轮） | 无显式 unwind，默认界行为不可复现 | 加显式 `#[kani::unwind(N)]`（K4） | payload_len：282+ 轮 |
| W2 | 同上，但发生在 `drop_in_place`/`deallocate`，大量 `aborting path on assume(false)` | Vec/clone 的分配建模使循环长度符号化 | 去 clone / 改栈数组（K5） | payload_len |
| W3 | symex 阶段（未到求解器）大量 aborting path，输入含嵌套 Vec | drop 胶水路径组合爆炸 | **停泊**：当前工具链墙；具体向量回归兜底 | H5 txn 往返：3800+ paths |
| W4 | 求解器阶段长时不归（cover SAT 查询） | 任意输入上反解密码学/线性级联原语 | cover 移到整形侧（K8） | P2 on 任意缓冲：19min |
| W5 | 大片 `UNDETERMINED`，check 位于 cpuid/asm/intrinsics | 原语不可符号执行 | 规范模型 + 差分（K3） | crc32c：615 项 |
| W6 | `unwinding assertion loop N` FAILURE | N 小于真实循环深度 | 按失败位置的循环重推 N（含 encode 侧！trailer 28B CRC 教训） | segment_trailer：11→32 |
| W7 | unwrap/panic 反例，位置在 harness 自身 | 漏 assume 定义域 | 补 assume（K7），不是产品 bug | H2 from_u16 |

---

## 4. 成本预期表（对需求方的报价单）

已在本仓库实测的数字（Kani 0.67 / nightly-2025-11-21 / 本机）：

| 工作项 | 实测 | 备注 |
|---|---|---|
| 无循环 harness（头部字段拒绝 ×3） | 2-3s | 最便宜档 |
| 2B 载荷 CRC 相关（roundtrip/小帧/CRC 拒绝） | 5-15s | CRC 不等式 8s |
| 28B CRC（trailer） | ~9s | 界要给足（W6） |
| 任意输入 40B + unwind 17 + 3 推进 | ~90s | 舒适区上限附近 |
| 任意缓冲上的 cover SAT | >19min 不收敛 | 禁（W4） |
| 嵌套 Vec 对象 | >480s 不收敛 | 墙（W3） |
| 差分测试（全长度扫描） | <1s | 模型缝隙闭合 |
| TLC CommitPipeline（安全+活性） | 分钟级（781 状态） | L2 |
| 门禁 [5-7/7] 全步（10 harness+差分+报告） | ~3min | verify.sh |
| **人的时间**：新 harness 从写到绿 | 0.5-2h | 遇墙另计，识别≤10min |

报价口径：接需求时按"每个性质一个 harness × 上表时长 + 接门禁/记账
0.5h"估；若对象含嵌套堆结构或 SIMD 原语，先声明 W3/W5 处置成本。

---

## 5. 验收清单（Definition of Done）

一个 harness/验证工作算"完成"当且仅当：

- [ ] 真源编译（B2），或显式申报绑定级与残余缝隙（K1/K3）；
- [ ] 显式 unwind + 推导注释（K4）；
- [ ] 无符号堆数据、无 while-let（K5/K6）；assume 仅声明输入域（K7）；
- [ ] cover 有 Pxx 命名；UNSAT（如有）已解释（K8）；
- [ ] 模型缝隙（如有）有差分测试且入门禁（K3）；
- [ ] verify.sh [5/7] harness 清单已含（或声明停泊 + 原因，W0）；
- [ ] VERIFICATION.md：账本条目 + §1 不变式目录检测列更新；
- [ ] `python3 scripts/verif-coverage.py` 数字变化可在账本中解释
  （不变式覆盖率 / vLOC / cover 各维度）；
- [ ] 生产代码若为此重构（抽模块等）：全 workspace 测试绿。

## 6. Agent 标准工作流（被指派"验证 X"时执行）

```
1. 定性质：查/立 §1 不变式目录条目（先承诺后证明）
2. 定对象：抽依赖最小模块（如 codec.rs 先例）；审计依赖面（K2）
3. 遇原语墙 → 规范模型 + 差分测试（K3）
4. 写 harness：从 §2 K9 模板起手，逐条过 K4-K8
5. 跑：单 harness 带超时（300-600s）；失败按 §3 W 表分诊
   （日志在 /tmp/verify_kani_full.txt 等门禁路径）
6. 全绿后：入门禁清单 → 跑 [5-7/7] → 更新账本/目录/度量
7. 汇报：引用规则编号与 W 编号说明每个决策（如"停泊依 W3"）
```

汇报模板（Agent 产出必须含）：
> 新增 harness N 个（层/绑定级）；unwind 界与推导；cover 声明 n 条
> 可达 n 条；遇墙 W# 处置；不变式覆盖率 a/30 → b/30；vLOC 变化；
> 残余缝隙清单变化。

## 7. TLC / Verus 侧要点（简）

**TLC（L2）**：
- 建模对象是协议状态机，不是代码；每条模型不变式挂 §1 目录 I-XX；
- 警惕**机制锁**：把要证的性质写进模型动作的原子性里 = 恒真空洞
  （I-A1 曾如此：模型内 ack 与安装同动作，无独立判别力——须拆开
  或降级标注，AGENTS.md §1 形式化 Review 的职责之一）；
- 活性（liveness）反例是高价值产出：R8-WM 水位停滞由 TLC 反例
  驱动修复后 781 状态全绿——反例 → 修 → 全绿 → 回归测试四步走；
- 模型↔代码绑定是 vLOC 之外的独立问题：当前靠 opfuzz 采样对拍，
  迹回放（trace validation）是升级方向（总纲 §5 绑定链）。

**Verus（L3）**：
- 现存 3 文件为 B1 镜像（升级 B2 是待办）；
- 实证经验：位事实以**自足重言式**整体送 `by(bit_vector)`（蕴含
  前提一并进位向量查询）；含跨整型 cast 的混合式不自动位爆炸，
  需拆层——跨层推导放 SMT 整数算术（order_domain.rs 注释存证）。

## 8. 历史坑全集（证据索引）

| 坑 | 后果 | 解法 | 落点 |
|---|---|---|---|
| 镜像常量漂移（FRAME_MAGIC 0x4C… vs 0x4F…） | 证明对象与产品脱钩 | B2 真源编译（K1） | 账本 #19/#28 |
| 镜像手写 CRC ≠ 真 SIMD CRC | CRC 从未被证明 | 模型+差分（K3） | #28 |
| FrameType 四变体镜像写两 | Fence/Seal 零探索 | 真源编译+定义域符号化 | #28 |
| 无显式 unwind | symex 282+ 轮不收敛 | K4 | #28 |
| Vec::clone | 循环长度符号化 | K5 | #28 |
| 嵌套 Vec drop | 3800+ paths 爆炸 | W3 停泊 | #28（H5） |
| 任意缓冲 cover SAT | 19min 不收敛 | K8 整形侧 | #28 |
| 符号字节进堆 | 虚假反例 | K5 栈数组 | 旧镜像 H2 注释 |
| while-let 符号长度 | >9min 展开爆炸 | K6 固定次数 | 旧镜像 H1 注释 |
| 24B 探索界太小 | 解码体从未被探索 | 界=头+载荷并注释来历 | #19（40B 由来） |
| 漏 assume 定义域 | unwrap 反例（harness bug） | K7 | #28（H2） |
| trailer CRC 28B 界给 11 | unwinding FAILURE | W6 重推 N | #28 |
| TLC 机制锁 | 恒真不变式 | 拆动作/降级标注 | I-A1 注 |
| （对照）纯读码评审 | 两轮未检出三缺陷 | 探针实证纪律 | AGENTS.md §1 |

> 维护纪律：新坑入表 + 账本；本手册与 VERIFICATION.md §5 的度量
> 定义同步演化。改本手册须在账本留条目。
