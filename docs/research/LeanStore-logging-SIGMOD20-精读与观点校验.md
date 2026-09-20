# SIGMOD'20《Rethinking Logging, Checkpoints, and Recovery》精读与观点校验

> 论文：Haubenschild, Sauer, Neumann, Leis（`LeanStore-logging-SIGMOD-2020.pdf`，
> CC BY 4.0）。校验方法三层：**论文内证据 → LeanStore 代码实况**
> （全克隆 master/latency 分支）→ **后续论文修订**（SIGMOD'25、
> PVLDB'24、BtrLog VLDB'26）。结论先行：**十项核心主张九项成立**，
> 但论文有三处必须加注的诚实性缺口，且其首选设计的硬件前提
> （Optane 持久内存）已消亡——思想的存活形态是它的"备用方案"。

## 一、论文在做什么（一段话）

为 SSD 优化型存储引擎（LeanStore 这类"接近内存性能 + 可超内存"）
重建恢复栈：ARIES 功能全但集中日志在多核不扩展，内存数据库日志
（Silo/Hekaton）轻量但不能超内存——论文给出一套**分布式两级日志 +
RFA 低延迟提交 + 连续检查点 + 页供给闭环**的完整方案，声称保住
ARIES 的全部特性（生理日志/模糊检查点/steal/索引恢复）同时开销
低一个量级（TPC-C 日志开销 19k 指令 vs Shore 的 200k+）。

## 二、十项主张逐条校验

| # | 主张 | 论文内证据 | 代码实况 | 后续论文 | 判定 |
|---|------|-----------|---------|---------|------|
| C1 | ARIES 集中日志在快引擎里过早成瓶颈；内存式日志不能超内存——需要中间形态 | ARIES 4 线程即峰（123k），基线引擎 1.4M txn/s | master 仍无全局日志 | PVLDB'24 主体继承 | **成立**（已成业界共识） |
| C2 | per-thread 日志 + GSN 近线性扩展 | 857k @40T（20.9×） | latency 分支 LogWorker/线程 ✓ | 同 | **成立** |
| C3 | 朴素分布式日志把 GSN 偏序线性化回全序 → 每次提交要冲刷全部远端日志（关键问题陈述） | No-RFA 变体 690k vs RFA 854k | latency 代码保留该对比路径 ✓ | — | **成立**（本文最有洞察的一段） |
| C4 | RFA：多数事务无逻辑/物理依赖，页 GSN + GSN_flushed + L_last 三要素可零簿记检测；独立即免远端冲刷 | 低争用下 92%→4.5% 免除 | latency 分支 `needs_remote_flush` 旗标 + BufferFrame GSN 字段 ✓ | SIGMOD'25 的 RFA 队列仍在用 | **成立，附条件**（见 §三-1） |
| C5 | RFA 延迟≈理论最优（p50 不变，p99 113→124µs）；组提交使 YCSB 中位翻倍（7 vs 2.8µs） | Fig 11 | — | **SIGMOD'25 反攻组提交侧**（批间隔=延迟下限） | 成立，但被后来者推进 |
| C6 | 连续检查点：按 WAL 体积分片轮转、GSN 表取 min 截断日志——有界恢复、无写尖峰、无时间旋钮 | Fig 9 WAL 稳定在限额；对照 WiredTiger 剧烈波动 | master Partition 分片 ✓ | — | **成立**（对 dendro 最有行动价值的一条，见 §五） |
| C7 | 页供给是闭环流系统：单线程串起 unswizzle→writeBack→evict，FIFO 两拍，"最迟写出"避免写放大 | §3.5 流量图（89/10/1% 稳态分布） | PageProviderThread ✓ | ssd-waf 分支持续研究 | 成立（设计论断，非对照实验） |
| C8 | 恢复三阶段全并行，2.6 GB/s 重放，比 SiloR 报告快 5×（38s vs 211s） | §4.6 | recovery 部分演进为 CRMG | — | 成立**但不可比**（硬件/日志量都不同：SiloR 180GB 日志 vs 本系统 100GB 限额） |
| C9 | 超内存场景比 Aether 快 2×；"经典磁盘架构不适合快 SSD" | Fig 9 右列 | — | PVLDB'24 复述 | 成立**但加注**（竞争者均为作者在自系统内重实现，非原版） |
| C10 | 保住 ARIES 特性集（生理/模糊检查点/steal/索引恢复）且 19k 指令/txn | Table 1（47k→66k 逐项拆解） | WALMacros/生理条目 ✓ | — | 成立（这是它对内存式方案的关键差异化） |

内部一致性小瑕疵：摘要区称日志开销"15k 指令"，Table 1 实际拆出
18-19k（66k−47k）——不同配置口径，引用时以 Table 1 为准。

## 三、诚实性审查（引用本文前必须知道的三件事）

1. **全部实验跑在 read uncommitted**。原话："Because our system does
   not yet implement full transaction isolation, we effectively run all
   experiments in read uncommitted mode." RFA 的免冲刷率（92%→4.5%）
   与争用退化曲线（θ≥1 收敛）都是在**无隔离执法**下测的——真实
   SI/2PL 下事务足迹更长、页争用更密，免冲刷率可能低于论文值。
   这是最大的适用性缺口。
2. **一级日志的硬件（Optane DCPMM）自己就是瓶颈**：同配置把一级
   从 PM 挪到 DRAM，40 线程 857k→960k（22.6× vs 20.9×）。作者
   诚实记录了，但这也预告了下一节的问题。
3. **对照组是"自家重实现"**：SiloR/Aether/ARIES 都在 LeanStore 内
   重写（理由充分——控制变量），WiredTiger 对比**关闭了 fsync**
   （"fair comparison"）。结论方向可信，数字差距须打折。

## 四、硬件前提的消亡与思想的存活（时间线校验）

论文首选设计 = **持久内存放 WAL 尾部 + SSD 放二级**（RFA 免组提交，
即时提交）。Intel 2022 年停产 Optane——首选设计的硬件没了。
存活路线恰是论文自己写的备选（§3.2 末）："一级留在 DRAM、冲刷到
SSD 才算持久，配 RFA 优化的组提交"。**五年后 SIGMOD'25 打的就是
这个备选方案里的组提交**（批间隔=延迟下限 → 自主提交），再五年
BtrLog（VLDB'26）把二级搬到对象存储/云盘。校验结论：本文的
**分布式日志 + GSN + RFA 骨架全部存活**且被 latency 分支原样继承
（`needs_remote_flush`、`global_min_gsn_flushed` 即当年的
`GSN_flushed`）；死掉的只有"持久内存一级"这一层硬件假设。

## 五、dendro 映射复核（对照既有分析，增补与修正）

既有映射（技术分析 §3）"GSN ≙ epoch<<32|seq、结构同源"复核**成立**。
本文精读新增四点：

1. **RFA 三要素在 dendro 单写者下平凡成立**：每分支单写者 = 单日志，
   `needsRemoteFlush ≡ false`——我们天然处于 RFA 的理想态。依赖只
   在**跨分支 merge**时出现（I-D2 三方合并）；若未来 P3 多写者，
   RFA 是现成的免远端冲刷协议（比全量依赖闭包便宜）。
2. **连续检查点 = 行动项**：我们的 WAL 前缀回收/manifest 保留 16 版
   是"历史界"但**触发与日志体积耦合度未成文**（I-G2 假设未成文正
   是此缺口）。论文的"checkpoint 增量由 1/S × WAL 限额触发 +
   分片 GSN 表取 min"是把这个耦合**写成算法**的范本——落 dendro
   即：checkpoint 推进与 (branch WAL 字节, manifest 版本保留数)
   联动的明确公式，可入 I-G 目录。
3. **检查点防尖峰的页级细节**（"一次只 latch 一页、拷贝后写入、IO
   期间不持锁"）：dendro checkpoint 物化列存块时同构适用——
   我们目前 checkpoint 与 flush_loop 的交错策略未按此审视过。
4. **反面对照确认 dendro 的架构红利**：论文花大力气保 ARIES 特性
   （模糊检查点/steal/索引恢复/undo）；dendro 的 append-only +
   CAS + prolly 让这些**结构性免费**（无脏页、无 undo、恢复即重放
   已提交帧）。C10 的整条特性清单是我们"不需要做"的清单。

## 六、可引用口径（给后续文档引用本文时的建议措辞）

- 可直接引用：C3 偏序线性化问题陈述、C6 连续检查点算法、C4 RFA
  三要素（注明"RU 模式下实测"）；
- 须加注引用：免冲刷率数字（§三-1）、2.6 GB/s 重放（§三-3）；
- 作为历史定位引用：分布式日志 + GSN + RFA 骨架的出处（latency
  分支与 BtrLog 的直系源头）。

## 信源

- 论文存档：`LeanStore-logging-SIGMOD-2020.pdf`（CC BY 4.0，
  DOI 10.1145/3318464.3389716）
- 代码验证：master（GroupCommiter×3 变体 = 论文"四种设计"的实验
  残留；BufferFrame.GSN；Partition 分片）、latency 分支
  （`needs_remote_flush` / `global_min_gsn_flushed` = RFA 存活证据）
- 修订链：[LeanStore候选分支深入分析.md](LeanStore候选分支深入分析.md)
  §一（SIGMOD'25）、§四（BtrLog）、[LeanStore技术分析.md](LeanStore技术分析.md) §3
