# AGENTS.md — Dendro

只写（append-only）· 内容寻址 · 分支化 · 云原生的 AI 原生 SQL 数据库。
Rust workspace，edition 2021，无 CI、无 rust-toolchain/clippy/rustfmt 配置（用工具链默认值）。

## 作者意图（原始立项 prompt 的硬性要求）

1. 协议与实现分离：对外 MySQL + PostgreSQL 双协议，**PG 优先**；协议层是薄适配层。
2. 数据只写、git 式版本化：分支即沙箱，服务于 **Agent 操作数据库/管理系统** 这一
   AI 原生场景（每个 Agent 开分支折腾，验证后合并或丢弃）。
3. 云原生：TP 侧 WAL、AP 侧列存全部落 OSS；事务用内存引擎（HANA 式 OCC MVCC），
   内存事务的并发度与"WAL 对 OSS 友好"是专项调研过的设计点。
4. 列存为 GPU 优化保留机制（CBF 块 codec 分层、64B 对齐、footer zone map 先剪枝后搬运），
   尽可能复用/参考现有组件与设计。
5. SQL 是必备能力：必须有语法测试基线（tests/slt + tests/pg_grammar，PG 代码已作参考克隆）。
6. 只写库会很大，**压缩是必须的**，且要持续测量压缩/性能的衰退情况（SPEC 08）。
7. **性能绝对优先**：禁止 Rc/RefCell 等拖慢性能的方案（除非确实必须）；鼓励内存池等
   高性能结构；只读数据结构可采用带部分元数据的改进型 B-tree 等高级结构
   （Strange Loop 有专题汇报的方向）。

## 参考实现：`../readonly.refer/`（只读，禁止修改）

仓库外的 `/home/nzinfo/src.db/readonly.refer/` 收录了立项时筛选的参考源码
（均为 `--depth 1` 浅克隆，目录内 README.md 有完整清单与许可表）。按关注点查阅：

- **git 式版本化 / prolly tree**：`dolt/`、`doltgresql/`（内容寻址 prolly tree、
  分支/合并语义；其官方性能对标数据是 dendro 的对照系：Dolt 读延迟约为 PG 6.3×、
  写 3.6×——dendro 的内存事务路线正是为了避开这个开销）
- **OSS 原生存储**：`slatedb/`（WAL+SST 全落对象存储的最小完整实现）、
  `neon/`（计算/存储分离）、`delta-rs/`、`iceberg/`、`iceberg-rust/`（append-only
  事务日志、manifest 树、快照与时间旅行）
- **列存 / Arrow 栈 / GPU 方向**：`influxdb3_core/`（Arrow+DataFusion+Parquet）、
  `databend/`（云原生数仓；注意 `src/**/ee`、`src/bendsave` 为 Elastic-2.0 许可）、
  `lancedb/`（lance 列存格式）
- **嵌入式 KV / 树结构**：`redb/`（COW B-tree）、`fjall/`（LSM）、`sled/`（仅架构参考）
- **Rust 全栈 SQL 引擎**：`limbo/`（Turso，io_uring 异步 I/O、确定性仿真测试）、
  `gluesql/`（可换存储后端）、`tikv/`、`greptimedb/`、`qdrant/`
- **SQL 测试基线语料**：`sqllogictest-corpus/`、`postgresql/`（PG 17.6 回归测试，
  tests/pg_grammar/GRAMMAR.md 的勾选清单来源）

**许可纪律**：`readonly.refer/restricted-license/`（immudb BSL、endb AGPL）仅留档，
**不得复制其代码**；databend 的 ee 目录同理。参考其余仓库时读设计、读算法，
抄思路不抄代码。同级的 `../risingwave/`（Apache-2.0）也可参考；`../cozo/`（MPL）、
`../materialize/`（BSL）不在许可白名单内。

## 架构速览

对外说 PostgreSQL（优先）/MySQL 协议；事务在内存中做 OCC MVCC；提交以 group-commit
WAL 直写对象存储；数据以内容寻址 prolly tree 版本化（git 语义）；后台物化为列存块
CBF 供 AP 向量化执行。设计决策以 `spec/00–10`（11 篇 SPEC）为准，代码注释频繁引用
SPEC 章节（如 `SPEC 06 §2.4`）——**改动语义前先读对应 SPEC**。

```
crates/
  dendro-core      format → objstore → prolly → versioned → wal → memtx → sql → engine
                   （分层单向依赖，见 src/lib.rs 顶部注释；顶层 re-export Database/Session/SqlError）
  dendro-columnar  CBF 列存：writer/reader/footer/stats + codec/{raw,bitpack,rledict,delta,zstd}
                   `#![forbid(unsafe_code)]`
  dendro-pgwire    PG v3 协议薄适配层，`#![deny(unsafe_code)]`，把 SqlError 映射为 ErrorResponse
  dendro-mywire    MySQL 协议，仅依赖 std::net + std::thread（SPEC 10 §7），禁止引入 tokio/bytes
  dendro-server    bin `dendro`：serve / bench / smoke 三个子命令（clap 4 derive）
  slt              bin `slt`：sqllogictest runner，进程内驱动引擎，每文件独立内存库
prototype/         Python 原型（prolly tree、压缩基准），改动引擎语义时可参考其测试
tests/slt/dendro/  SQL 基线 7 个 .slt 文件；tests/pg_grammar/GRAMMAR.md 是 PG 语法覆盖 roadmap
benches/results/   基准结果 JSON + README（没有 criterion，见下文基准命令）
```

## 常用命令

```bash
cargo test --workspace                                      # 全部单元/集成/e2e 测试
cargo build -p slt && ./target/debug/slt run tests/slt/dendro   # SQL 基线（7 文件，必须全绿）
cargo test -p dendro-columnar --release --test bench_cbf -- --ignored --nocapture  # CBF 微基准
cargo build --release -p dendro-server && ./target/release/dendro bench --out benches/results
./target/debug/dendro serve --data /tmp/dendro-data         # 起服务（serve 子命令不可省略）
./target/debug/dendro smoke [sql]                           # 引擎自检
```

## 硬性纪律

1. **SLT 回归红线**（tests/slt/BASELINE.md）：任何引擎变更后必须重跑
   `slt run tests/slt/dendro`，当前基线 7/7 全过，通过数下降禁止合入。
2. **语义常量**（改动会破基线，须同步更新 slt 与 BASELINE.md）：
   `ORDER BY DESC` 默认 NULLS FIRST；PK 冲突 SQLSTATE 23505；未定义表 42P01；
   合并冲突 40001。
3. **性能红线**（SPEC 00 §6）：热路径禁止 Rc/RefCell，共享仅限不可变 Arc；
   并发用 parking_lot + arc-swap。
4. **unsafe**：全仓库无 unsafe。columnar 是 `forbid`、pgwire 是 `deny`，不得移除。
5. **同步优先**：引擎主体是同步代码（std::thread + std::net）。tokio 只允许出现在
   objstore/s3.rs（object_store 需要 runtime）、server、slt 与 wire 集成测试中。

## 代码约定

- **依赖**：集中在根 `[workspace.dependencies]`，成员 crate 一律 `xxx.workspace = true`，
  新增依赖先加到根再引用。
- **错误处理**：thiserror 2。core 统一用 `SqlError { state: &'static str, message }`
  （`dendro-core/src/error.rs`，携带 SQLSTATE，构造器如 `syntax()`/`undefined_table()`）
  和 `pub type Result<T>`；columnar 用 thiserror 枚举 `Error`；wire 层不定义自己的错误，
  只映射 `SqlError` → 协议错误响应。
- **注释与文档**：中文（模块级 `//!`、doc、行内注释），并引用 SPEC 章节；
  面向用户的错误消息、SQL 文本用英文。
- **commit message**：英文短句，小写前缀 + 冒号（如 `final: live dual-protocol verification`）。
- **日志**：tracing，但埋点极少——不要为了"完整性"到处加日志，跟随现有密度。
- **测试**：单元测试内联 `#[cfg(test)]`（每文件一个 test module）；集成/e2e 在各 crate
  `tests/` 目录；真实客户端对拍用 dev-dependencies 里的 tokio-postgres / mysql crate。

## 基准与证据

性能声明必须有据可查：`benches/results/*.json` 是当前数字（如 oltp_insert 157k txn/s、
CREATE BRANCH 225µs @1 万行）。改了热路径就重跑 `dendro bench` 并更新结果与 README 表格。
方法学见 `spec/09-bench-plan.md`。

## 已知文档偏差

README "快速开始" 的 `cargo run -p dendro-server -- --data ...` 缺少 `serve` 子命令，
正确形式见上文"常用命令"。
