# HelixDB 数据结构调研（2026-09-22）

> 对象：ref-projects 同级 /home/nzinfo/src.db/helix-db（YC 公司，Rust
> graph-vector 数据库，graph+vector+KV+doc+relational）。方法：Explore
> agent 深读（49 检索）。本文为证据引用 + **dendro 落地判定**。

## 一、总体：正相反的选型

HelixDB **没有自研存储引擎**——整体架在 fork 的 SlateDB（LSM +
object_store）上，图/向量/全文编码为 KV 行，事务/WAL/memtable/
compaction 全部委托（`crates/db/Cargo.toml` git 依赖）。**买引擎卖
语义层 vs dendro 造引擎**——可参考的不是存储结构，而是编码层、
事务边界、缓存-提交耦合、索引治理。

## 二、六个关注点

### 1. KV 编码与 delta 代数（⭐ 直接可抄）
- key = 单字节前缀 + 定长 ID，`KeyPrefix` 枚举集中管理（Adjacency
  0x00 / EdgeProperty 0x01 / …，`encoding/v2/keys/data.rs:37-51`）；
  **类型化 codec 使非法 key 不可构造**（AGENTS.md："structs that make
  building invalid keys impossible"）。
- 邻接 = 每节点一行 `RoaringTreemap` 双向位图（`values/adjacency.rs:
  80-84`）；更新三档：EAGER（RMF，低度数）/ LAZY（delta append，
  高度数免读放大）/ ADAPTIVE（**Morris counter 估度数逐节点选择**，
  `adjacency.rs:53-60`）。
- **delta 是可结合代数对象**：`BitmapMembershipDelta{additions,
  removals}.compose()`（`values/indexes/equality.rs:16-54`），LSM
  merge operator 整批 compose 后与基值合并（`merge_operator.rs:
  117-140`）——合并次序无关、可部分应用。
- **dendro 判定**：CBF 段 delta/二级索引维护可统一到"加集/删集+
  compose"；eager/lazy/adaptive 三档是现成调参框架。**直接可抄**。

### 2. 事务边界与调度铁律
- MVCC 委托 SlateDB（OCC+序列号）；**读视图能力编码进类型**：
  Reader=可 Clone 的 DbSnapshot（并行）/ Writer=move-only 事务
  （串行），"impossible to build a request view with the wrong
  storage capability"（`read_view.rs:1-6`）。
- 进程内 gate（`index_lifecycle/scope_gate.rs`）：类型化 permit
  guard（drop 即释放），锁只用于 DDL/发布类独占变更——**MVCC 版本
  能保护的读者不加锁**。与 dendro 三段式 checkpoint 同构且更进一步。
- **提交边界**：`commit_write_tx`（`tx.rs:229-293`）先 stage 全部
  延迟变更 → commit → **拿到 seqnum 后才按 seq 精确失效内存缓存**；
  拿不到 seq 而缓存有脏 = InvariantViolation（fail closed）。
  dendro checkpoint 期间哈希索引/upper 缓存失效可照此规则。

### 3. 索引读校验与成本 fallback（⭐ 正确性铁律）
- `UNIQUE_EQUALITY_FIX_PLAN.md`：`IN(v1..vN)` 打 unique 索引曾退化
  全扫（4 值起劣化，300ms）。修复两原则：① 索引读出的 owner 必须
  **同一快照回权威数据校验**（missing=miss，stale/malformed=fail
  closed）；② 无统计信息时，小基数字面量集合折叠为一次 multi_get
  **永不劣于扫描**（修复后 1.2ms，250×）。
- **dendro 判定**：二级索引/向量索引一旦有 stale entry 可能，此为
  正确性铁律；成本模型无统计时的安全 fallback 直接可抄。

### 4. 向量索引（HNSW）
- 整图存 KV 行（Core/Hot/Layer0 三 lane，前缀分键，`storage_prefixes.
  rs:6-13`）——与图事务同一原子域。
- **分层驻留**：只有 upper 层进内存缓存，layer0 走块缓存；**有界
  部分缓存天然正确**——缺行回落稳定快照（`cache/store.rs:1-13`）。
- **影子执行→捕获写集→单批应用**：未提交事务里试跑确定性 HNSW
  插入、测量、准入，然后一次性应用最终写集——"avoids running the
  deterministic graph algorithm twice"（`storage/transaction.rs:1-16`）。
  dendro 的 OCC 验证+提交可借这个形状。
- 数值防御：Candidate 拒 NaN/负距离；UnalignedVector 零拷贝不对齐
  安全；SimHash 预过滤 + 每查询 prefetch 预算。

### 5. 值语义（精品小 crate）
- `CanonicalNumber`：i64/f32/f64 归一 `odd_significand × 2^exp` 精确
  表示，`42 == 42.0`、`-0.0 == 0.0`、NaN→None（`value-semantics/
  lib.rs:19-98`）；跨类型精确比较不经浮点（floor_log2+significand）。
  **dendro SQL 语义（整数键=浮点键）的最小正确实现，直接可抄**。
- 行编码 rkyv 零拷贝 + 对齐一次复制；有符号 i64 索引串 sign-mask
  前缀 + 20 位零填充（`property/row.rs:27-56`）。

### 6. 工程实践清单（可立即采用）
- **slice 偏移显式 const 相加**（AGENTS.md："always keep the
  additions of offsets explicit"）——dendro CBF 解析器立此规矩。
- doc-test 即契约 + 模块头写边界契约（"must not create database
  keys, inspect raw bytes, or perform I/O"）。
- fail-closed 不变量错误消息写物理含义；`let-else` 优先；clippy
  -D warnings + wildcard match deny；IdAllocator（lease 型 ID 分配，
  lock-free 快路径 + 后台续租，`id_allocator.rs`）可整体照搬。

## 三、行动对照

| HelixDB | dendro 现状 | 行动 |
|---|---|---|
| delta-compose 代数 + adaptive 三档 | delta 追加区（区间合并） | ⭐ CBF/二级索引 delta 统一到 compose |
| seqnum 耦合的缓存失效 | checkpoint truncate 锁外已做 | 缓存失效规则可照抄（如有内存索引） |
| 索引读回权威校验 | 无二级索引 stale 面 | 未来铁律入档 |
| 影子执行→写集捕获→单批应用 | OCC 验证+提交 | 形状可借 |
| CanonicalNumber | SqlValue 比较 | SQL 语义候选（远期） |
| 类型化 key codec / let-else / doc-test 契约 | — | 工程规范可立即吸收 |

## 四、反面教材（警示）

1. **内容寻址对象无回收协议**：TEXT_INDEX_SNAPSHOT_RETENTION.md 明写
   "Storage grows monotonically until a separate reclamation protocol
   is designed"——用"不删"买正确性，GC 留成债。**dendro 已有
   tombstone/gc_sweep 优于之**，但做非阻塞 checkpoint 第二步时
   引用回收必须同步设计。
2. keyspace 前缀复用导致整个 migrations/ 目录买单——**前缀表一次
   定死集中管理**。
3. 缓存精确 seqnum 匹配换正确性但牺牲稳定性（无关写也触发
   rehydration）——用代际（generation）而非逐 seq 匹配更稳。
