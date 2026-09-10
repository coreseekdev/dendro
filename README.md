# Dendro

**只写(append-only) · 内容寻址 · 分支化 · 云原生的 AI 原生 SQL 数据库。**

> 经历 20 轮架构评审（Review + Fix/Impl 循环），**主干正确性防线已收敛**
> （连续 8 轮零新增 P0），234 个测试 / 10 个 slt 文件全绿，clippy -D warnings 全域零输出。

名字取自 dendrochronology(树轮年代学)：数据库每次提交就是一圈年轮——只追加、不可变、
可回溯；分支像树一样生长——为 Agent 操作数据库/管理系统提供 O(1) 的沙箱分支。

## 一句话架构

> 对外说 PostgreSQL(优先)/MySQL 协议；事务在内存中做 OCC MVCC(HANA 式 delta)；
> 提交以 group-commit WAL 直写对象存储(OSS)；数据以内容寻址 prolly tree 版本化(dolt 式 git 语义)；
> 后台把提交物化为 GPU 友好的列存块(CBF)，AP 查询走向量化列式执行。

## 特性

- **协议与实现分离**：`pgwire`(PG v3) / `mywire`(MySQL 客户端协议) 是薄适配层，
  内部 AST/执行器与协议无关。PG 优先。
- **git 式版本化**：`CREATE BRANCH / MERGE BRANCH / SHOW BRANCHES`，
  分支创建 O(1)（只写一条 ref），合并基于 prolly tree 的 chunk 级 diff，
  行级冲突检测——天然适配 Agent 沙箱：每个 Agent 开一个分支随便折腾，验证后合并或丢弃。
- **数据只写**：没有 update/delete 物理操作，只有追加新版本。全历史可查。
- **云原生**：TP 侧 WAL 与 AP 侧列存全部落在对象存储（本地目录/S3 兼容）；
  计算节点无状态，重启从 manifest + WAL 恢复；分支 = 元数据指针，跨计算节点共享存储。
- **列存为 GPU 保留优化机制**：CBF(Cambium/Dendro Block Format) 块级 codec 分层，
  RAW/BITPACK/RLE_DICT/FSST 可被 GPU kernel 直接解码（FSST 无熵解码、码本共享内存），
  块 64B 对齐、zone map 在 footer，GPU 侧先剪枝后搬运。zstd 只用于冷块。
- **SQL 必须可用**：sqlparser-rs 解析(PG/MySQL 方言) + 自研执行器，
  sqllogictest 基线在 `tests/slt/`（10 文件，含 JOIN / 游标 / checkpoint 可见性）。
- **多写者安全**：分支租约 fencing（epoch CAS + 运行时拒写 40001 + 惰性续期）、
  只读打开模式、毒化语义（WAL 失败 → 40003 + close_graceful 恢复）。
- **GC 与生命周期**：墓碑随 manifest 原子发布 + 保留窗口 + WAL 前缀/旧 epoch 回收；
  manifest 版本保留 16（CAS 影子谱系防护：读路径不信任本地记忆）。
- **可观测**：/metrics（commit/flush 延迟、pending_bytes、lease TTL、毒化标志）、
  /readyz（停止中 → 503）、dendro backup（一致性点物理备份）。
- **运维**：SIGTERM/SIGINT 优雅关闭（close_graceful per branch）、
  k8s 部署 yaml（deploy/k8s.yaml：非 root + probes + preStop）、
  PG cleartext 认证、MySQL 认证、备份工具。

## 运维：本地 k8s 部署（已验证）

[docs/ops/local-k8s.md](docs/ops/local-k8s.md) — 本地 k8s 选型对比（kind/k3d/minikube/microk8s/k3s）、
microk8s 部署手册（镜像源受限网络解法）、dendro on k8s 已验证用例、踩坑索引。

## 调研：SOTA 分布式事务技术（2024–2026）

[docs/research/SOTA分布式事务技术调研-2026.md](docs/research/SOTA分布式事务技术调研-2026.md) —
Aurora DSQL（OCC + Adjudicator + Journal）、FoundationDB（sequencer/resolver）、
Granola、Slatedb/Delta（对象存储原生事务）、Unistore/SingleStore/HaSiS（HTAP 单存储）
的 SOTA 拆解，及其对 Dendro P2'/P3 路线的修订（裁决与授序合流、Journal 即事实源）。

## 调研：多节点 TP 事务

[docs/research/多节点TP事务调研.md](docs/research/多节点TP事务调研.md) —
把"多节点处理 TP 事务"拆成 R/W1/W2/B 四个问题，逐一对照
Aurora/Neon/Socrates、CockroachDB/TiKV、FoundationDB、Aurora DSQL、
Calvin、ForkBase 的参考架构，给出 Dendro 的四阶段演进路线
（读副本 → 分支租约 fencing → 日志服务 → 分布式 OCC）。

## KV 接口层（分支化的版本键值存储）

[docs/design/kv-接口层.md](docs/design/kv-接口层.md) — 把 memtx + prolly 树
统一暴露为分支化的版本 KV：Rust API（get/scan/cas/显式事务）+ RESP wire
（Redis 客户端直连，BRANCH = checkout -b）。"不标准"点 = 核心特性：
值带版本、键空间可 fork/merge、追加式、快照读。

## 选型：共识实现对比（Raft vs Quorum Log）

[docs/research/共识实现选型.md](docs/research/共识实现选型.md) —
tikv/raft-rs vs openraft vs hashicorp/raft vs Kafka KRaft 的工业验证对比、
Dendro Journal 的三层演进（单节点 → Quorum Append → Raft 升级路径）、
以及"Aurora 洞察：有外部单写者时 Raft 的 leader 选举是多余的"。

## 设计：多节点 memtx 事务一致性

[docs/design/多节点memtx一致性.md](docs/design/多节点memtx一致性.md) —
现状一致性模型的精确刻画（不变量 I1-I5、持久性三线）、双写同一分支的具体
损坏路径分析、P1 租约 fencing / P2 日志服务 / P3 分布式 OCC 的协议设计与
memtx 一致性保证矩阵、与现有代码的差距清单。

## 设计：负载自感知与弹性调度

[docs/research/负载自感知与弹性调度.md](docs/research/负载自感知与弹性调度.md) —
引擎负载信号目录（组提交队列/checkpoint 积压/冲突率…）、/metrics 与 /readyz
暴露设计、K8s 三种执行器映射（HPA 读副本 · DendroOperator 分支再均衡 ·
scale-to-zero 沙箱）、反压与负载卸载次序、实施路线 M1-M5。

## 深度教程（推荐从 01 章读起）

[docs/tutorial/](docs/tutorial/README.md) — 教程风格的逐层拆解，颗粒度到字节：
对象存储与 manifest 逐字段 · 内容寻址与保序键编码（逐字节示例）· prolly 树节点
二进制布局 · WAL 24B 帧头与组提交时序 · memtx OCC · CBF 列存 64B 块头与 5 种
codec 字节布局 · 分支合并 · PG/MySQL 消息流 · S3 云原生实战（RustFS 实测）。

## 目录

```
spec/          详细 SPEC（11 篇，从对象存储到基准方法学）
prototype/     Python/Go 原型（prolly tree、分支模型、压缩基准）
crates/
  dendro-core     内容寻址、prolly tree、WAL、manifest、内存事务引擎、SQL 引擎
  dendro-columnar CBF 列存格式 + 压缩 + 统计剪枝 + parquet 导出
  dendro-pgwire   PostgreSQL wire 协议
  dendro-mywire   MySQL wire 协议
  dendro-server   服务器装配 + CLI
tests/slt/     sqllogictest 语料与基线
benches/       基准（TP 微基准、AP 列式、压缩衰退、OSS 延迟注入）
```

## 快速开始

```bash
cargo run -p dendro-server -- serve --data /tmp/dendro-data
# 另一个终端（任何 PG 客户端）：
psql "host=127.0.0.1 port=5432 user=dendro dbname=cambium"
```

```sql
CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT);
INSERT INTO t VALUES (1, 'hello');
CREATE BRANCH dev FROM main;
USE BRANCH dev;
INSERT INTO t VALUES (2, 'from agent sandbox');
MERGE BRANCH dev INTO main;
SELECT * FROM t;

-- 游标（分批读取大批量结果）
DECLARE c CURSOR FOR SELECT * FROM t ORDER BY id;
FETCH 10 FROM c;
CLOSE c;
```

## 备份

```bash
cargo run -p dendro-server -- backup --data /tmp/dendro-data --out /tmp/dendro-backup
# 恢复 = 把备份目录作为数据根启动
cargo run -p dendro-server -- serve --data /tmp/dendro-backup
```

## 只读副本

```bash
cargo run -p dendro-server -- serve --data /tmp/dendro-data --read-only --pg-port 5433
# /readyz 503 = 写者租约过期；/metrics 包含 pending_bytes / lease_ttl
```

## 状态（2026-09-07，M0–M7 全部达成）

| 里程碑 | 状态 | 证据 |
|--------|------|------|
| M0 SPEC + 骨架 | ✅ | spec/00–10 共 11 篇 |
| M1 Python 原型 | ✅ | prototype/prolly（25/25 测试，节点分布≈理论 Weibull）；prototype/columnar（压缩衰退实证）|
| M2 对象层+prolly+WAL+manifest | ✅ | 24 单测 |
| M3 memtx OCC+SQL+pgwire | ✅ | e2e 5 测；tokio-postgres 真客户端对拍 |
| M4 mywire+分支 SQL+merge | ✅ | mysql 真客户端对拍；分支/合并/冲突 e2e |
| M5 CBF 列存+AP 执行+物化 | ✅ | columnar 12 测；AP 集成测试（CBF+WAL overlay 合并）|
| M6 基准 | ✅ | benches/results/*.json（TP/组提交/分支/恢复/AP）|
| M7 slt 基线 | ✅ | tests/slt 10/10 语料全绿（含 008 JOIN、009 checkpoint 可见性、010 游标/事务）+ BASELINE.md |

**核心数字**（进程内引擎天花板，详见 benches/results/README.md）：

- oltp_insert 157k txn/s（p50 4.9µs）；点查 162k txn/s（p50 5.0µs，PK 下推直查）
- 组提交延迟 p50 ≈ flush_interval，p99≈p50+0.1ms（尾延迟压平）
- CREATE BRANCH 225µs @1 万行（与数据量无关）；MERGE 306µs @千行 diff
- 崩溃恢复 2 万事务 8ms（HEAD 探测，无 LIST）
- CBF：DELTA 顺序列 R=8 解码 2.3GB/s；低基数文本 RLE_DICT 48×；
  高基数文本 FSST R=4 解码 126 Mrow/s（≈2.2× zstd，热层免熵解码）

## 云原生（✅ 真实对象存储验证）

存储主体（prolly 行树 chunk、WAL 段、CBF 列存段、manifest）**全部落在
S3 兼容对象存储**，计算节点无状态。已用 Docker 里的
[RustFS](https://github.com/rustfs/rustfs) 完成端到端验证：

- 条件写（`If-None-Match: *`）支撑 manifest 乐观提交与分支 CAS
- 杀进程 → 全新计算节点从 S3 完整恢复（数据+分支+合并结果）
- **增量分段列存**：checkpoint 只 PUT memtx 增量（零树扫描零远端读），
  段级+行组级双重剪枝，段数超阈值才全量压缩——慢/贵网络友好
- 读路径磁盘 LRU 缓存：RustFS 实测 12k 行全生命周期仅 9 次远端 GET
- 慢网络（注入 100ms RTT）下单行提交 p50 76ms，组提交+流水线吸收、尾延迟压平

```bash
# 云原生模式启动
docker run -d --name rustfs -p 9000:9000 \
  -e RUSTFS_ACCESS_KEY=key -e RUSTFS_SECRET_KEY=secret rustfs/rustfs:latest
aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://dendro
./target/release/dendro serve --s3-endpoint http://127.0.0.1:9000 \
  --s3-bucket dendro --s3-access-key key --s3-secret-key secret
# DENDRO_S3_RTT_MS=100 可模拟慢网络
```

## 测试

```bash
cargo test --workspace          # 全部单元/集成/e2e 测试
cargo build -p slt && ./target/debug/slt run tests/slt/dendro   # SQL 基线（9 文件）
```
