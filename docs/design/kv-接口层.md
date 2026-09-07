# 设计：KV 接口层——分支化的版本键值存储

> 结论先行：**KV 是 Dendro 的对外接口契约，独立于 SQL 存在**。
> 实现上复用全部既有机器（memtx + prolly 树 + OCC + WAL + 分支 + merge），
> 但接口本身是一等公民：Rust 进程内 API + RESP（Redis 协议）wire 对外暴露。
>
> 这一层的 KV "不标准"，不标准点全部来自 Dendro 的核心特性（版本化 + 分支）。

## 1. 对"是否存在 KV 层"的正名

此前的表述（"KV 的职责被 prolly 树 + 对象存储 + 读缓存瓜分"）混淆了两个概念：

| 概念 | 在 Dendro 中的状态 |
|------|--------------------|
| KV **存储引擎**（RocksDB/Sled/Fjall 类）| ❌ 刻意没有：对象存储原生架构不需要本地页引擎（SPEC 00）|
| **KV 数据模型** | ✅ memtx = 内存 MVCC KV；prolly 树 = 持久有序 KV |
| **KV 访问接口** | ✅ 本文档：`dendro_core::kv::Kv` —— 一等公民 API + RESP wire |

**接口层的存在与实现位置无关**：列存、memtx、prolly 树是内部实现，
对外暴露统一的 KV 访问完全成立——这正是本层的定义。

## 2. 接口（`dendro_core::kv::Kv`）

```rust
let mut kv = Kv::open(&db, "main")?;          // 打开某分支上的 KV 视图
kv.use_branch("agent42")?;                     // 切换键空间（分支隔离）

kv.get("key")?;                                // Option<Vec<u8>>，快照读
kv.scan(Some(b"a"), Some(b"z"))?;              // 范围扫描（字节序），[(k, v)]
kv.put("key", b"value")?;                      // 自动提交（OCC + WAL）
kv.delete("key")?;

kv.begin()?;                                   // 显式事务（读己之写）
kv.put("k1", "v1")?; kv.put("k2", "v2")?;
kv.commit()?;                                  // OCC 冲突 → 40001
kv.rollback();

kv.cas("k", Some(b"old"), b"new")?;            // 线性化比较-交换（bool）
kv.use_branch("…") / branch()                  // 键空间
```

### 2.1 "不标准"清单——每一处都是核心特性的映射

| 非标准点 | 来自哪个核心特性 | 对使用者的意义 |
|----------|------------------|----------------|
| 值带版本（MVCC 快照读）| WAL + prolly 版本树 | 旧快照可读（`Txn` 的 snapshot 即时点）|
| 键空间 = 分支，可 fork/merge | manifest refs + 结构共享 | 冲突显式化（行级 40001），不丢数据 |
| 追加式：写 = 新版本，删 = tombstone | 内容寻址不可变对象 | 全历史可审计；"删除"在合并语义下也是可冲突的操作 |
| 提交 = OCC，无锁、无等待 | memtx 分支内单写者 | 高并发读 + 无死锁；热点 key 冲突显式报错 |
| 键序 = 字节序 | prolly 叶层字节序 | 二进制安全；无字符集假设 |
| 表 id / catalog | KV 表是真实的内部表 `__kv` | 与 SQL 层共享同一存储：SQL 可查 KV 数据，KV 读写对 SQL 立即可见 |

## 3. 实现映射（每个 API 走哪条既有路径）

| API | 实现 |
|-----|------|
| `get` | memtx 快照点查（`TableMem::get`）→ 树 lookup（`cursor::lookup`）；显式事务中先查写集（读己之写）|
| `scan` | 树 `TreeIter`（键序）∪ memtx overlay 归并（BTreeMap：覆盖/插入/删除）|
| `put`/`delete` | `Txn` 写集 → `commit_tx`（OCC 验证 → 分片安装 → WAL 帧 → 组提交）|
| `cas` | **begin → get（事务快照）→ 匹配则 put → commit**；OCC 验证保证读-提交之间无并发写 ⇒ 线性化 |
| `use_branch` | 会话切分支 + `ensure_table`（每个分支有独立的 `__kv` 表条目——零复制 fork）|
| 持久化 | 与 SQL 完全一致：组提交 WAL → checkpoint 物化 → S3 |

注意 `cas` 的正确性依赖**事务快照先于读**：先 begin 再读-判-写-提交，
OCC 验证自然覆盖"读和写之间被并发插入"的窗口（若用无事务的 get+put，
快照太新，会漏检——初版实现犯过此错，测试固定了语义）。

## 4. RESP wire（对外暴露，Redis 生态兼容）

`dendro-server/src/kv_resp.rs`（`--kv-port 6380` 启用）：

| Redis 命令 | Dendro 语义 |
|-----------|-------------|
| PING / ECHO | 标准 |
| GET / SET / DEL / EXISTS / DBSIZE | 当前分支键空间 |
| **BRANCH \<name\>** | 切换分支；不存在则**从当前分支创建**（git checkout -b 语义）|
| **BRANCHES** | 列出全部分支 |
| MULTI / EXEC / DISCARD | KV 显式事务（OCC 冲突 → ERR，客户端重试）|

```bash
redis-cli -p 6380 SET agent7/state running      # Agent 直接用 Redis 客户端
redis-cli -p 6380 BRANCH agent7                 # 沙箱分支
redis-cli -p 6380 SET agent7/draft v2
```

局限（如实）：无 TTL/过期、无 pub-sub、无集群模式；键序扫描用 SCAN 的
简化版（DBSIZE/EXISTS 已覆盖常用面）。这些是 v2 的自然扩展点。

## 5. 与 SQL 层的关系

- KV 表 `__kv` 是真实的 catalog 表（`k BLOB PRIMARY KEY, v BLOB`）：
  SQL `SELECT * FROM __kv` 可见 KV 数据；KV 写对 SQL 连接立即按快照可见
- 表 id 冲突已修复（表 id = catalog 现存最大 id + 1，见 ops 文档踩坑表）
- 分支 merge 对 `__kv` 与普通表一视同仁：key 级三方合并，冲突显式 40001

## 6. 测试与验证

- `tests/kv.rs`：基础读写/范围、CAS、显式事务原子性、分支隔离+合并可见性、
  持久化（进程重启）、OCC 冲突（40001）——5/5
- `tests/kv_wire.rs`：RESP 协议端到端（PING/SET/GET/DEL/EXISTS/MULTI-EXEC/
  BRANCH 隔离/DBSIZE）——1/1
- 交叉验证：SQL 写 → KV 读、KV 写 → SQL 读共用 `__kv` 表（实现即证明）

## 7. v2 扩展点

- `get_at(ts/commit)`：历史时点读（数据都在，缺 API）
- KV 专用 wire 的鉴权（当前 trust）
- 大 value 的块外存储（blob 外置，参考 lance 的 blob 字段）
- KV 层的二级索引（值前缀索引等）
