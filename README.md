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

## 状态

工程阶段：SPEC 完成、Python 格式原型完成、Rust 核心实现中。
详见 `spec/00-overview.md` 的路线图。
