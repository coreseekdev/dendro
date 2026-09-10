# AGENTS.md —— Agent 工作规则（强约束，任何 Agent 会话必须遵守）

> 本文件是 coding agent 在本仓库工作的最高优先级流程约束。
> 违反本文件的"完成"声明无效。验证账本：[docs/VERIFICATION.md](docs/VERIFICATION.md)。
> 架构三约束：云原生（OSS + OSS 优化存储）、高性能（架构优化 + 测试）、
> 高可靠（形式化验证 + 模型检查）——所有工作不得违背。
> **追加-only 不可变（架构根约束）**：任何代码路径不得改写/删除已持久化
> 内容——物理删除仅经 GC 墓碑保留窗口；对 append-only 的任何"优化"
> （原地重写、截断复用）都是设计变更，须走审计。

## 1. 三类 Review Agent（定期 + 事件触发，必须启动）

| Agent | 触发条件 | 职责 |
|---|---|---|
| **Code Review** | 每轮存储/引擎/协议的实质性变更合并前 | 实现与理想的差距、隐藏 bug。**必须探针实证**（写临时测试跑通后删除），不接受纯读码结论 |
| **形式化验证 Review** | spec/*.tla 变更后必须；平时定期 | 不变式合理性/完整性（对照 docs/VERIFICATION.md §1 验收不变式）、环境模型合理性、活性缺口；识别"构造恒真"的空洞不变式并降级标注 |
| **性能 Review** | 定期 + 热路径变更后 | 分配/拷贝/syscall/索引/锁五个维度，结论须含验证方法（基准/perf）与 A/B 方案 |

Review 结论必须落盘入库（docs/review-*.md），作为后续工作的任务书。
**实证先例**（本仓库已发生）：矛盾范围 BTreeMap panic、分支上限竞态、
毒化热旋 100% CPU——三者均为 review agent **写探针跑通**后才发现，
纯读码两轮未检出。

## 2. 顺序约束（重要：Code Review 先于形式化验证）

```
code review agent（发现缺陷/语义澄清）
  → 修复 + 回归测试（红转绿）
  → 形式化验证跟进：不变式补充 / harness 扩展 / 规约 vN+1
  → 不变式 review agent 校验
```

**禁止并行启动 code review 与形式化验证评审**：code review 的结论
（缺陷模式、边界语义、故障模型修正）是形式化验证的输入——并行会使
规约与实现各自演化、精化桥断裂。本仓库依据：两轮审计中"负数字面量
Int32 取负失效""段退休无界"等缺陷模式直接改写了范围下推与两段式
提交的规约需求——若规约先行即冻结在错误语义上。

形式化验证内部的 review（不变式评审）可与 code review 的**修复实现**
并行，但规约修订必须在 code review 结论入库之后。

## 3. 缺陷 → 检测机制纪律（docs/VERIFICATION.md §12 矩阵）

**每个已发生的缺陷必须落一个永久检测机制**，并在账本 §12 矩阵登记
错误类别。机制分层：
SimStore 故障注入（环境模型采样）→ opfuzz（状态机交互采样）→
一致性/边界表/属性测试（确定性边界与表示不变式）→ TLC（协议语义）
→ Kani（不可信输入边界）。只修不加机制 = 未完成。

## 4. WIP 纪律

- 未全绿的验证测试标 `#[ignore]` + WIP 注释（复现序列、疑点、修法方向
  必须写入），**不计入账本已验证集合**；
- 禁止 delete-to-pass：不得通过删除断言/弱化规约使测试变绿；
- 账本状态必须与事实一致（✅/🚧/⬜），诚实标注证据边界。

## 5. 工具与版本锁定

- tla2tools.jar 已入库（spec/tools/）；Kani 0.67.0 / Verus（引入时记录）；
- 升级验证工具必须同步账本工具版本并全量重跑对应验证。

## 6. 提交纪律

- 原子提交：一个逻辑变更一笔（缺陷修复 / 机制新增 / 文档 分开）；
- 提交前：涉及 crate 的全部测试 + 验证链路绿（见账本各 Claim 的复验命令）；
- 中文 conventional commits；工作区保持干净。

## 7. 参考索引

- 验证总纲与账本：docs/VERIFICATION.md（§12 缺陷→机制矩阵）
- 规约目录：spec/（CommitPipeline.tla 等；`make -C spec check`）
- opfuzz：crates/dendro-core/tests/opfuzz.rs（SimStore 随机操作序列）
- 故障模型单一事实源：crates/dendro-core/src/objstore/sim.rs
- 同工作区先例：../stream-db/basalt（AGENTS.md / VERIFICATION.md 全套）
