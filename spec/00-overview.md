# SPEC 00 — 总体架构与设计决策

状态：**定稿 v1**（随实现演进，变更需在文末记录）

## 1. 目标与非目标

### 1.1 目标

| # | 目标 | 度量 |
|---|------|------|
| G1 | 协议与实现分离，PG 协议优先，MySQL 协议其次 | 两个协议层共用同一 AST/执行器；协议层不包含存储逻辑 |
| G2 | 数据只写 + git 式版本化，分支创建 O(1)，行级冲突合并 | `CREATE BRANCH` 不复制数据；merge 用 chunk 级 diff |
| G3 | 云原生：WAL 与列存都在对象存储；事务在内存 | 计算节点本地盘只作缓存/可选写缓冲 |
| G4 | 列存为 GPU 保留解码机制，最大化复用现有设计 | 块头 64B、codec 分层、zone map；Arrow 内存布局 |
| G5 | SQL 是一等公民，有语法/语义测试基线 | sqllogictest runner + .slt 语料；PG 语法清单 |
| G6 | 压缩必须，并量化性能衰退 | zstd 级别 × 列类型 × 压缩率/解码吞吐曲线 |
| G7 | 性能绝对优先 | 热路径无 Rc/RefCell；Arc 只用于不可变共享；内存池化 |

### 1.2 非目标（v1）

- 不做分布式多写（单写者/分支 + 对象存储条件写乐观提交；Raft 属后续工作）
- 不实现 PG 全部语法（覆盖矩阵见 SPEC 07；未覆盖的给出清晰错误）
- 不做二级索引（主键 prolly tree 之外，二级索引留待 v2）
- 不做细粒度权限/认证体系（trust + cleartext + SCRAM-SHA-256 最小实现）
- 不做列存的实时更新（列存是提交物的化投影，更新=新 row group + deletion vector）

## 2. 分层架构

```
        PostgreSQL 客户端            MySQL 客户端
              │ pgwire(PG v3)            │ mywire(client proto v10)
              └────────────┬─────────────┘
                           ▼
                ┌─────────────────────┐
                │ session / router    │  会话态：当前分支、参数、事务
                │ sqlparser-rs 解析    │  方言: PostgreSql / MySql
                │ 内部 AST (protocol 无关)
                └───────┬─────┬───────┘
              TP 路径   │     │   AP 路径
        (点查/DML/短查询)│     │  (扫描/聚合/大范围)
                ┌───────▼──┐ ┌▼──────────────────────┐
                │ 内存事务  │ │ AP 执行器（v1 行式解释器， │
                │ OCC MVCC │ │ Arrow 仅输出边界；向量化  │
                │ HANA delta│ │ 列式执行器为 v2，见 SPEC  │
                └───────┬──┘ │ 00 §2/G4 与 TASK P2-2）  │
                        │    └───────▲──────┬─────────┘
                 commit │            │      │
                ┌───────▼────────┐   │   列存投影(物化)
                │ commit pipeline │   │      │
                │ 1. OCC 验证     │   │      │
                │ 2. install 内存 │   │      │
                │ 3. group-commit │   │      │
                │    WAL → OSS   │──┼──────┘ (checkpoint 触发物化)
                │ 4. durable 回调 │   │
                └───────┬────────┘   │
                        ▼            │
        ┌─────────────────────────────────────────────┐
        │ 版本层 (dolt 式)                              │
        │  refs map {branch→commit} ─ commit{root,parents}
        │      └── prolly tree (行存快照, 内容寻址)      │
        ├─────────────────────────────────────────────┤
        │ 对象存储层 (SPEC 01)                          │
        │  objects/{xx}/{hash}.chunk  内容寻址数据       │
        │  wal/{branch}/{seg:020}.wal 提交日志          │
        │  col/{table}/{gen}.cbf     列存投影           │
        │  manifest/{ver:020}.json   元数据根(单调+CAS)  │
        └─────────────────────────────────────────────┘
                 ObjStore trait: Local / InMemory / S3兼容 / 注入延迟
```

## 3. 关键决策与来源

| 决策 | 选择 | 理由与参考 |
|------|------|-----------|
| 内容哈希 | SHA-512 截断 160bit，自定义 base32 | dolt `go/store/hash/hash.go`：64 位机 sha512 比 sha256 快，20B 平衡碰撞与扇出 |
| 版本结构 | prolly tree + keySplitter(weibull) | dolt `go/store/prolly/tree/node_splitter.go`：min 512 / target 4096 / max 16384，xxh3-32 + 每层 salt |
| 分支模型 | refs 本身是 prolly map；分支=一条 ref | dolt `address_map.go`：创建分支=写一条记录，GC 根=全部 ref |
| 合并 | 双 patch 流归并，key 级冲突 | dolt `tree/patch_generator.go`：地址相同的子树整棵跳过 |
| WAL 直写 OSS | 单调编号不可变对象 + Create 条件写 + HEAD 二分恢复 | slatedb `wal/slatedb/store.rs`：不依赖 LIST；fencing 靠条件 PUT |
| 元数据提交 | manifest 单调版本 + Create 条件写(乐观并发) | slatedb-txn-obj；比 neon 的"最后写者赢"更强 |
| 组提交 | 固定间隔/字节阈值触发，段=一个对象 | slatedb flush_interval=100ms 与费用估算；我们默认 50ms/8MB |
| 列存内存表示 | Arrow RecordBatch | tonbo/influxdb3/lance 全部收敛 Arrow；SIMD/GPU 友好 |
| 列存磁盘格式 | 自研 CBF（GPU 可解）+ Parquet 导出 | 三库无 GPU 一等目标（调研结论），差异化空间；互通靠 parquet crate |
| 内存事务 | OCC(Hekaton 式) + 分支级并行 | git 语义天然单写者/分支；读完全无锁；避免多版本写冲突复杂性 |
| SQL 解析 | sqlparser-rs 双方言 + 独立 translate 层 | gluesql 证明 ~11K 行可架全引擎；limbo 自写 2.1 万行仅覆盖 SQLite 方言 |
| PG 协议 | 自研最小实现(参考 neon pq_proto/postgres_backend) | 控制帧级细节；pgwire crate 做后备方案 |
| MySQL 协议 | 自研 handshake v10 + text resultset | vitess go/mysql 的包结构，ClickHouse MySQLHandler 参照 |
| 测试基线 | sqllogictest-rs(AsyncDB) + 自写 .slt + 上游语料 | limbo 弃 slt 改自建 DSL 但工程量大；slt-rs 一份实现吃两方言语料 |

## 4. 模块与 crate 划分

```
crates/dendro-core
  ├── objstore/    ObjStore trait + Local/InMemory/Throttled + CAS 块存储 + manifest
  ├── wal/         帧编码、段管理、组提交、恢复
  ├── prolly/      splitter/chunker/cursor/patch/address map
  ├── format/      行编码、commit 对象、varint、hash
  ├── memtx/       OCC 事务管理器、版本存储、epoch 回收、内存池
  ├── catalog/     库表元数据(自身也在 prolly map 中)
  ├── sql/         translate(planner)/TP 执行器/AP 执行器/表达式求值
  └── engine/      Database 门面：把上述粘起来(会话无关)
crates/dendro-columnar   CBF 读写、编码器、统计、剪枝、parquet 导出
crates/dendro-pgwire     PG v3 服务器协议
crates/dendro-mywire     MySQL 客户端协议服务器
crates/dendro-server     bin: 装配、监听、CLI(dendro serve / dendro bench ...)
```

依赖原则：core 不依赖 tokio（引擎同步）；wire/server 层用 tokio。
列存 crate 依赖 arrow（内存向量）；parquet 导出为可选 feature。

## 5. 数据流举例

### 5.1 一条 INSERT 的完整旅程
1. pgwire 收 `Query` 消息 → sqlparser(PG 方言) → AST → translate 成 `Stmt::Insert`
2. 会话在当前分支上开一个内存事务：写集缓存 `(table, pk, row)` 于线程本地
3. `COMMIT`/自动提交：OCC 验证(读写集与提交水位比较) → install 到 memtx 版本存储
   → 追加 commit 记录进 WAL 缓冲(帧编码) → 等待组提交段落盘/上传(可配 no_wait)
4. durable 后：事务状态 → Committed，版本对读快照可见
5. 后台 checkpoint：把该分支累积的写集用 prolly chunker 应用到上次树根 → 新树根；
   新 chunk(内容寻址)上传 OSS；commit 对象写入；manifest CAS 推进分支头
6. 物化器把两次 checkpoint 之间的行转 Arrow batch → 追加 CBF row group → 上传

### 5.2 一个 Agent 沙箱的旅程
```
CREATE BRANCH agent42 FROM main;   -- manifest CAS：新增 ref，O(1)
USE BRANCH agent42;                -- 会话当前分支切换
... DDL/DML ...                    -- 写只落在 agent42 的 WAL 序列与树
MERGE BRANCH agent42 INTO main;    -- 或 DROP BRANCH agent42; 丢弃
```
合并 = main 树与 agent42 树对 base 树做双 patch 流归并；行级冲突报错并列出冲突键。

## 6. 性能红线（实现守则）

1. 热路径禁止 `Rc/RefCell/Gc`；共享只允许 `Arc<T: Send+Sync>` 的不可变数据
2. 锁粒度：memtx 分 shard；读路径零锁(原子快照指针)；写路径 per-key CAS 安装版本
3. 内存池：行编码缓冲、batch 构造缓冲用 arena/池，禁止热路径每行 alloc
4. 序列化零拷贝优先：prolly 节点、CBF 块解引用用借用视图
5. 所有阻塞 IO 在专用线程池/tokio blocking；引擎内不有隐式 IO
6. 只读数据结构优化（"StrangeLoop 读优化结构"落地）：
   - zone map(块级 min/max) + SuRF 风格前缀过滤器(实验) 做剪枝
   - 排序 run 上建 PGM 式分段线性索引(实验模块 `prolly::pgm`)替代二分
   - immutability 换并发：已发布快照永久不可变，读者无同步成本

## 7. 路线图

- M0 SPEC + 骨架（本文）
- M1 Python 原型：prolly tree/分支/合并 + 压缩基准（prototype/）
- M2 Rust 对象层 + prolly + WAL + manifest（可持久化、可恢复）
- M3 memtx OCC + SQL translate + TP 执行器 + pgwire：psql 能建表插查
- M4 mywire + 分支 SQL + merge
- M5 CBF 列存 + AP 执行器 + checkpoint 物化
- M6 基准：TP/AP/压缩衰退/OSS 延迟注入；报告写入 benches/results/
- M7 sqllogictest 基线扩展 + 文档收尾

## 8. 术语表

| 术语 | 含义 |
|------|------|
| branch | 分支：一条指向 commit 的 ref；Agent 沙箱单位 |
| commit | 一次提交：{树根, 父提交列表, 高度, 元数据} |
| chunk | 内容寻址数据块（prolly 节点/行编码/commit 都以 chunk 形式存储）|
| LSN/seq | 分支内单调递增的日志序号 |
| manifest | 元数据根：refs、表目录、WAL 高水位、GC 水位；单调版本号 |
| CBF | Dendro Block Format：自研 GPU 友好列存块格式(SPEC 05) |
| memtx | 内存事务引擎(OCC MVCC)，HANA 意义上的 delta 存储 |
| checkpoint | 把 memtx 累积写集物化为新 prolly 树版本的过程 |

## 变更记录

- 2026-09-07 v1 初稿
