# Dendro

**只写(append-only) · 内容寻址 · 分支化 · 云原生 的 AI 原生 SQL 数据库。**

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
  RAW/BITPACK/RLE_DICT 可被 GPU kernel 直接解码，块 64B 对齐、zone map 在 footer，
  GPU 侧先剪枝后搬运。zstd 只用于冷块。
- **SQL 必须可用**：sqlparser-rs 解析(PG/MySQL 方言) + 自研执行器，
  sqllogictest 基线在 `tests/slt/`。

## 调研：多节点 TP 事务

[docs/research/多节点TP事务调研.md](docs/research/多节点TP事务调研.md) —
把"多节点处理 TP 事务"拆成 R/W1/W2/B 四个问题，逐一对照
Aurora/Neon/Socrates、CockroachDB/TiKV、FoundationDB、Aurora DSQL、
Calvin、ForkBase 的参考架构，给出 Dendro 的四阶段演进路线
（读副本 → 分支租约 fencing → 日志服务 → 分布式 OCC）。

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
cargo run -p dendro-server -- --data /tmp/dendro-data
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
| M7 slt 基线 | ✅ | tests/slt 7/7 语料全绿 + BASELINE.md |

**核心数字**（进程内引擎天花板，详见 benches/results/README.md）：

- oltp_insert 157k txn/s（p50 4.9µs）；点查 162k txn/s（p50 5.0µs，PK 下推直查）
- 组提交延迟 p50 ≈ flush_interval，p99≈p50+0.1ms（尾延迟压平）
- CREATE BRANCH 225µs @1 万行（与数据量无关）；MERGE 306µs @千行 diff
- 崩溃恢复 2 万事务 8ms（HEAD 探测，无 LIST）
- CBF：DELTA 顺序列 R=8 解码 2.3GB/s；低基数文本 RLE_DICT 48×

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
cargo build -p slt && ./target/debug/slt run tests/slt/dendro   # SQL 基线（7 文件）
```
