# Dendro 深度教程

> 教程风格的设计文档：从一次 SQL 提交出发，逐层拆到字节。
> 所有格式描述与 `crates/` 下的实现逐字段对齐（2026-09-07，v0.1）。

## 目录

| 章 | 内容 | 你将看到 |
|----|------|----------|
| [01 总览](01-总览与一次提交的旅程.md) | 分层架构、一条 INSERT 的完整旅程 | 全链路时序图 |
| [02 对象存储层](02-对象存储层与manifest.md) | ObjStore trait、目录布局、manifest JSON 逐字段 | 磁盘/桶布局图、乐观提交时序 |
| [03 内容寻址与行编码](03-内容寻址与行编码.md) | SHA-512/160、base32、保序键、行编码 | 逐字节编码示例 |
| [04 prolly 树](04-prolly树与磁盘格式.md) | weibull 分裂、节点二进制布局、CoW 写路径 | 节点字节布局图、树生长动画（字符画）|
| [05 WAL 日志](05-WAL日志格式与组提交.md) | 24B 帧头、TXN/CHECKPOINT 帧、段尾、组提交、恢复 | 帧字节布局、组提交时序、崩溃恢复流程 |
| [06 内存事务引擎](06-内存事务引擎OCC.md) | 64 分片、版本链、OCC 验证、epoch 回收 | 数据结构图、冲突时序 |
| [07 列存 CBF](07-列存CBF格式.md) | 64B 块头逐字段、5 种 codec 的字节布局、footer、增量分段 | 块布局图、剪枝示例、S3 上的段列表 |
| [08 分支与合并](08-分支与合并.md) | catalog 树、commit 对象、三方合并 | 分支fork图、合并冲突示例 |
| [09 网络协议](09-网络协议PG-MySQL.md) | PG v3 消息流、MySQL 握手、二进制编码 | 消息时序图（逐消息字节）|
| [10 云原生实战](10-云原生S3实战.md) | S3 适配器、不确定写消解、读缓存、慢网络 | RustFS 实测数据、网络字节审计 |

## 约定

- 所有整数**小端**（LE）除特别标注（PG 协议与 PG 二进制值为大端 BE）
- "chunk" = 内容寻址对象（首字节是类型标签，见 03 章）
- 代码引用格式 `文件路径:函数`，均可直接跳转
- 字节图：每格 1 字节，`|00 01 ..|` 形式；多字节字段标注端序

## 10 分钟先跑起来

```bash
cargo build --release -p dendro-server
./target/release/dendro serve                    # 本地目录后端，PG:5432 / MySQL:13306
# 另一个终端：
./target/release/dendro smoke "CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT); \
  INSERT INTO t VALUES (1,'hi'); SELECT * FROM t"
```
