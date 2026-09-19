# 分配器引入评估：jemalloc（optional feature）与替代方案（2026-09-19）

## 结论

- **P0（零依赖，立即做）**：不换分配器——把 **glibc 自省 API**
  （`mallinfo2`/`malloc_info`/`malloc_trim`，glibc ≥ 2.33）接进 memprof。
  我们主平台（Linux）的 arena/碎片/可用统计白拿，`unattributed`
  差分解成"arena 滞留 vs 真增长"。
- **P1（feature 门控，A/B 后定）**：`jemalloc` feature（tikv-jemallocator
  + tikv-jemalloc-ctl），**默认 off**，仅 server 二进制形态启用评估。
  选择理由：唯一具备深度内省（mallctl/arena/bin/heap prof）+ 活调参
  （decay/background_thread）的成熟 Rust 绑定——直接治我们实测过的
  "释放后滞留棘轮"（10M ANALYZE 期 RSS 只涨不还）。
- **不引入**：epoch-GC（另有判定准则，见 memprof 讨论）；全局换
  mimalloc/tcmalloc 作为默认（见下）。

## jemalloc（tikv-jemallocator）优劣

**优**
1. 内省最全：`tikv-jemalloc-ctl` 暴露 allocated/retained/mapped、
   碎片比、按 arena/bin 明细、heap profiling（prof build）——分配器级
   物理归因，与 memprof 语义归因互补。
2. 活调参：`dirty_decay_ms`/`muzzy_decay_ms`/`background_thread`——
   **主动 madvise 归还**，针对我们的 RSS 有界目标（glibc 只有被动
   trim_threshold）。
3. 数据库系谱：TiKV/TiDB 生产多年；社区在 2025-2026 仍活跃建议
   数据库负载用它（原 jemallocator 停维，tikv 分支 2026-05 仍在更新）。
4. 碎片治理：size-class arena 对我们"106 列 × 海量小 SqlValue 分配"
   的形态友好。

**劣**
1. **空闲基线 RSS 更高**（元数据 ~9MB 级 vs mimalloc <4MB——2025-12
   dev.to 对比）；decay 配错会放大滞留（与目标相反）。
2. 绑定维护度中等（OpenSSF 5/10）；构建链重（C 源编译）。
3. **发布形态约束**：dendro 有三种形态——server bin / embed 库 /
  dendro-sqlite **cdylib（宿主进程内加载）**。`#[global_allocator]`
  在 cdylib 内是模块作用域（宿主 malloc 不受影响），但会带来进程内
  **第二套分配器状态**（双份元数据/双碎片域），且跨 ABI 边界的
  alloc/free 纪律必须严格（我们 C ABI 全拷贝、无跨界 free——现状
  合规，需测试锁定该不变量）。⇒ feature 只在 **server bin** 评估启用。
4. 收益未经我们自己的负载验证——文献分歧真实存在（见 Meilisearch
   案例：jemalloc 内省查案，最终却换 mimalloc v3 解决滞留）。

## 替代方案

| 方案 | 内省 | 碎片/滞留 | 成熟度/绑定 | 判定 |
|------|------|-----------|-------------|------|
| **glibc + mallinfo2/malloc_info**（现状分配器） | arena 级统计够用；malloc_trim 主动归还 | 中（trim_threshold 粗粒度） | 零依赖，主平台白拿 | **P0 立即接 memprof** |
| **tikv-jemallocator** | 最强（arena/bin/prof/活调参） | 强（decay 可控，默认偏高） | TiKV 生产谱系 | **P1 feature，A/B 后定** |
| **mimalloc v3**（crate `mimalloc`） | 弱（mi_stats 汇总，无 arena 级） | **默认低碎片**、线程间共享好（Meilisearch 2026-03 换它解决 1.5 年"泄漏"实为滞留） | MS 出品，绑定简单 | 备选 feature（若 A/B 输给 jemalloc） |
| tcmalloc（crate 老旧） | gperftools 级 | 高并发好，空闲 RSS 最高（~13MB） | 绑定维护弱 | 不引 |
| snmalloc（snmalloc-rs） | 弱 | 好 | 绑定维护停滞风险 | 不引 |
| bumpalo/typed-arena | —（非全局，作用域） | — | 成熟 | 热批缓冲**局部**复用（P2 性能项，非全局） |
| epoch-GC（crossbeam） | —（回收机制非管理器） | **延迟回收**与内存上界相悖 | 成熟 | 仅当 memtx 去锁化时重评 |

## A/B 方法学（P1 判定准则）

同一 ClickBench 10M 管线 × {glibc, jemalloc(tuned), mimalloc} 三臂：
1. 装载/ANALYZE 后稳态 RSS（decay 归还有效性——针对实测棘轮）
2. 查询峰值 VmHWM 与查询后 RSS 回落比
3. q01-q23 时延中位数（±5% 平局区）
4. memprof `unattributed` 占比（分配器滞留可解释性）
通过标准：RSS 治理显著改善（≥20% 稳态下降）且时延不退——否则
保持 glibc 基线。

## 信源（2026 现状核查）

- tikv/jemallocator（原 jemallocator 后继，2026-05 仍在更新）
- Meilisearch "The good, the bad, and the leaky"（2026-03）：
  jemalloc 内省定位滞留 → 最终 mimalloc v3 解决
- "Rust allocator: jemalloc vs mimalloc vs tcmalloc"（2026-07）：
  jemalloc 调参与可见性最强，mimalloc 低碎片默认最佳
- dev.to 分配器对比（2025-12）：空闲 RSS mimalloc<4MB / jemalloc~9MB
  / tcmalloc~13MB；负载主导结论，需自测
- "Heap Fragmentation in Rust"（2026-03）：jemalloc 快 ~17% 但
  drop 后滞留显著（配比不当的实证）
