# 性能优化专项任务书（Agent 交接文档）

> 创建：2026-09-20。前置会话已完成十轮 SQLite 追赶优化；本文档
> 交接**未完成任务**与**核心问题**给接手 Agent。
> 项目：/home/nzinfo/src.db/dendro（Rust，HTAP 内存事务库）。

## 一、核心问题（本任务最高优先级）

### P0：内存路径慢于磁盘路径——不正常，必须修复

**现象**（sqlite-cmp 基准，1M 行）：
- dendro.point.mem ≈ dendro.point.disk（186k ≈ 186k）——内存无优势
- dendro.update.mem < dendro.update.disk（73k < 82k）——内存反而更慢
- dendro.range100.mem ≈ dendro.range100.disk（28k ≈ 30k）

**为什么这不正常**：内存引擎的数据访问应零 I/O、缓存局部性好，
性能应显著优于磁盘路径。对标 **SAP HANA**（内存数据库标杆）：
其内存路径（delta store + main store 均在内存）始终优于任何
disk-based 系统。

**根因分析**（已完成，见 docs/bench/sqlite-baseline.md 复测五）：
基准的 mem 臂不 CHECKPOINT——100 万行全部驻留 memtx，按实测
**26.7× 结构开销**即 ~1.7GB 工作集：
1. `pending` 与 `memtx` **双份值克隆**（未共享 Arc，~220B/条）
2. Arc 层叠（每条 5 次堆分配 + ~250B 固定结构开销）
   - `BTreeMap<Box<[u8]>, Arc<VersionVec>>` 节点摊销
   - `Arc<VerCell>`（16B + ts 8B + Option<Arc> 8B）
   - `Arc<Vec<u8>>`（16B + Vec 24B + 行编码）
3. BTreeMap 节点分散 → **CPU 缓存局部性极差**（对比：CHECKPOINT
   后的紧凑不可变段 + NodeStore LRU 64MB 预算钉死，局部性好）

**修复方向**（按 ROI，见 docs/research/关键数据结构内存分析.md）：

| 优先级 | 改动 | 预期 |
|--------|------|------|
| P0-A | pending×memtx 值 Arc 共享（`Mutation::Put(Arc<Vec<u8>>)`） | −220B/条（~13% 驻留） |
| P0-B | VerCell 瘦身：值 ≤16B inline（SSO）+ ts 复合时间戳 epoch 高位（u32+u32 替代 u64 双份） | −2 次分配/条 |
| P0-C | key 紧凑化：rowid 主键场景 key 恒定 8-16B——`Box<[u8]>` → inline ≤24B | −1 分配/条 |
| P1 | memtx 分片 BTreeMap → **紧凑列存数组**（HANA delta store 思想：插入追加到排序数组而非 BTreeMap 节点） | 局部性质变 |
| P1 | 读取路径：热数据在 memtx 时直接列式扫描（复用 CBF 解码器） | 大范围扫描 |

**对标 HANA 的架构参考**：
- HANA delta store = 内存行存（追加写）+ 周期 merge 到 main store
  （列存）——与我们 memtx→CHECKPOINT→列存段 同构
- HANA 的 merge 是后台增量、不停服——我们的 CHECKPOINT 阻塞提交
  （commit_mu 持有期间）——研究非阻塞 merge
- 内存管理：HANA 用全局 allocator + 按池统计——我们已有 memprof
  三层计量（继续用）

### 验收标准

```
dendro sqlite-cmp --rows 1000000 后：
- point.mem / point.disk ≥ 2.0（内存至少双倍优势）
- update.mem / update.disk ≥ 2.0
- range100.mem / range100.disk ≥ 1.5
- point.mem vs SQLite point.mem（:memory:）≥ 0.5（追平一半）
```

## 二、已完成工作（上下文，勿重复）

### 十轮 SQLite 追赶（master 0af0829）

| 优化 | 效果 |
|------|------|
| 形状缓存（literal 模板化自动 prepare） | parse 4.6µs→0 |
| 会话工作副本（免每执行 AST 深克隆） | point +12% |
| exec_statement 借用化 | insert +31% |
| 点查免中转（fetch_row_bytes + 直出列） | point 阶段 -1.3µs |
| 投影解码（decode_row_proj——只解码选中列） | 宽表收益面 |
| 持久 resolve 缓存（catalog 根键控 LRU） | UPDATE 2.4-4.5× |
| 自主提交（Group 提交线程 opportunistic 直刷） | update -28% 等待 |
| pk 范围早退（try_range_early + 残差谓词） | range 2.9× |
| 流式三路归并（TreeIter peek + 逐行直出） | 大范围收益待验 |

**当前 vs SQLite**：insert.disk 1.9× / insert.mem 1.0× / update 0.6×
/ range 0.3× / point 0.4×。点查 SQL 路径 32.7→3.8µs（8.6×）。

### 基础设施（全部可用）

| 设施 | 位置 | 用途 |
|------|------|------|
| perf 框架（八阶段打点） | `crates/dendro-core/src/perf.rs` | `SELECT * FROM cambium.perf_stages` |
| memprof（meters/包络/采样） | `crates/dendro-core/src/memprof.rs` | `SELECT * FROM cambium.memory_usage` |
| sqlite 对比基准 | `dendro sqlite-cmp` | 公平口径双引擎 |
| TP 基准 | `dendro tp-bench` | 点查/范围/写 |
| ClickBench | `dendro click-bench` | AP 侧（含 --queries-only） |
| 各探针 | `examples/{pointbreak,updpath,stprobe,memstruct}` | 微基准分解 |
| 差分测试体系 | `SET dendro.optimize=off` | 路径等价验证 |

### 关键研究档案

- `docs/bench/sqlite-baseline.md`——十轮优化全记录
- `docs/research/关键数据结构内存分析.md`——26.7× 分解与优化候选
- `docs/research/LeanStore技术分析.md`——技术映射（不可变底线）
- `docs/research/内存优化SOTA调研-2026.md`——MVCC/压缩/感知
- `docs/bench/tp-base-opt1.md`——树底座架构与 A/B
- `docs/bench/memory-analysis.md`——查询内存剖析

## 三、待完成任务清单

### 性能（本任务）

1. **P0 内存路径修复**（见上）——对标 HANA 验收
2. point 0.4× 追赶：SQL 机器剩余 ~3.8µs（会话机器 ~2µs + eval
   核心 ~1.7µs）——进一步 Slim 或接受为文本路径合理下界
3. update 0.6×：WAL 帧攒批（LeanStore latency 分支参考——每
   线程批量而非每事务一帧）
4. range 0.3×：流式首批的 ClickBench 验证（大范围收益面）
5. join_order EqSat 化（egg，≥3 表时——docs/research/
   编译优化技术评估-e-graph-多面体.md）

### 独立项（非本任务，记录防丢）

- SQLite 文件→CBF 段化器（嵌入式 C 选项前置）
- C 档全面流式执行（10M+ 查询侧根治）
- 10M jemalloc 复验
- JIT 立项评估（当前瓶颈在 SQL 层不在执行核）

## 四、工作纪律（必须遵守）

1. **先测后优**：每次改动前跑 sqlite-cmp 或对应探针取基线
2. **perf 归因驱动**：优化前用 cambium.perf_stages 定位热点
3. **差分锁定**：涉及语义的改动必须有 optimize on/off 差分测试
4. **每轮全量测试**：`cargo test --workspace --release` 全绿才提交
5. **诚实入档**：docs/bench/sqlite-baseline.md 按轮次追加
6. **不可变底线**：CAS/append-only/prolly 不可变——任何优化
   不得引入原地页更新（LeanStore 分析文档的不采纳清单）
7. **基准环境**：/tmp 可能被环境重置——重要数据存 /home/nzinfo
   并用 setsid 跑长基准

## 五、快速上手

```bash
cd /home/nzinfo/src.db/dendro
cargo build --release
# 验收基线（约 25 分钟）
rm -rf /tmp/dendro-sqlitecmp && ./target/release/dendro sqlite-cmp \
  --data /tmp/dendro-sqlitecmp --rows 1000000 --out /tmp/base.json
# 微基准（秒级）
target/release/examples/pointbreak   # 点查分层
target/release/examples/updpath     # UPDATE 分解
target/release/examples/memstruct   # 数据结构内存
# 归因
cargo run --release --example pointbreak -- 2>&1 | grep perf -A 10
```
