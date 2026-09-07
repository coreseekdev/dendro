# SPEC 01 — 对象存储层 (objstore)

状态：**定稿 v1**

## 1. 目标

所有持久化状态（WAL、chunk、列存、manifest）都以**不可变对象**形式存放在对象存储上。
本层提供：统一 `ObjStore` trait、本地/内存/S3 兼容实现、延迟注入（基准用）、
内容寻址 chunk 存储、manifest 乐观提交、无 LIST 恢复、GC。

设计参照：
- slatedb `wal/slatedb/store.rs`（WAL 对象编号与 HEAD 二分探测）
- slatedb `retrying_object_store.rs`（不确定写消解：put-id 元数据反查）
- slatedb-txn-obj `object_store.rs`（manifest 单调 id + Create 条件写）
- neon `remote_timeline_client/index.rs`（单 index 列全量对象，避免 LIST）

## 2. ObjStore trait

```rust
pub trait ObjStore: Send + Sync + 'static {
    fn get(&self, path: &str) -> Result<Vec<u8>>;             // 整对象
    fn get_range(&self, path: &str, off: u64, len: usize) -> Result<Bytes>;
    fn put(&self, path: &str, data: Bytes) -> Result<()>;
    /// 条件写：不存在才成功(即 Create / if-none-match)。
    /// 返回 Err(Exists) 表示已被他人写入 —— fencing/乐观提交的基石。
    fn put_if_absent(&self, path: &str, data: Bytes) -> Result<()>;
    fn delete(&self, path: &str) -> Result<()>;
    fn head(&self, path: &str) -> Result<Option<HeadInfo>>;
    /// 仅本地实现需要；S3 上等价于 list。恢复路径不得依赖本方法。
    fn list_prefix(&self, prefix: &str) -> Result<Vec<String>>;
    fn copy(&self, from: &str, to: &str) -> Result<()>;
}
```

实现：
- `LocalObjStore`：目录布局 `path` 直接映射文件；`put_if_absent` 用 `open(O_CREAT|O_EXCL)`
  + 临时文件 rename。`head` 用 stat。
- `MemoryObjStore`：`RwLock<BTreeMap<String, Bytes>>`（测试/原型）。
- `ThrottledObjStore`：包装任意实现，注入每次调用的延迟分布(基准，SPEC 09)。
  注：热路径调用方传 `&dyn ObjStore`；trait 对象虚调用每请求一次，可忽略。

对象命名全局约定（可读、可扫、前缀即语义）：

```
{root}/objects/{hash[0:2]}/{hash[2:]}.chunk    内容寻址数据（prolly 节点、commit、行块）
{root}/wal/{branch_id}/{seg:020}.wal           分支 WAL 段(SPEC 02)
{root}/col/{table_id}/{gen:020}.cbf            列存投影(SPEC 05)
{root}/col/{table_id}/{gen:020}.cbf.idx        CBF footer/索引(可与上合并，见 SPEC 05)
{root}/manifest/{ver:020}.json                 元数据根(SPEC §5)
{root}/fence/{branch_id}                       写者 fence 对象(零字节)
```

规则：
- 所有编号字段 20 位零填充 ⇒ 字典序 = 数值序，LIST/浏览有序（slatedb paths.rs:119）
- chunk 路径按哈希两位前缀分桶 ⇒ 大库下目录可管理
- 对象一经写入**不可变**；删除只发生在 GC

## 3. 错误模型与重试

- 瞬时错误（网络/超时/5xx）：调用方可见 `Err::Transient`，上层重试策略统一放
  `RetryPolicy { max_retries, base_delay_ms: 100, max_delay_ms: 1_000 }`，指数退避+jitter。
- 确定性错误不重试：`Exists`（put_if_absent 冲突）、`NotFound`、`Corrupt(CRC)`。
- **不确定写消解**（照抄 slatedb）：`put_if_absent` 前给对象附加随机写入 id 元数据
  （本地实现用 sidecar `.putid` 文件；S3 实现用对象 metadata）。超时后先 `head` 反查
  putid 是否等于本次 ⇒ "其实成功了"；否则重试。把 at-most-once 变成 effectively-once。
- CRC：所有对象内容尾部 4B crc32c（chunk 格式内建；manifest/wal 段同理）。

## 4. 无 LIST 恢复（WAL 尾部探测）

恢复分支 `b` 的 WAL：manifest 记录 `wal_flushed_seg`（最后完整段号），
崩溃场景下未提交 manifest 的段号可能更大——用**指数探测+二分 HEAD**：

```
已知 seg lo 存在；探测 lo+1, lo+2, lo+4, ... 直到 head=NotFound ⇒ hi；
在 [lo, hi) 二分找最大存在的 seg。并发 HEAD 数 8。
前提不变量：编号存在性对删除单调（GC 只从低处删 + min_age）。
```

复杂度 O(log N) 次 HEAD，RTT 200ms 的 OSS 上恢复万段 < 3s。
（slatedb `wal/slatedb/store.rs:313-389` 同款。）

## 5. Manifest —— 元数据根与乐观提交

```jsonc
// manifest/{ver:020}.json  （ver 单调递增；v1 用 JSON+并行二进制，v2 可换 flatbuffer）
{
  "version": 128,
  "format_version": 1,
  "refs": {                       // 分支表（v2 迁入 prolly map；v1 直接 JSON）
    "main":  { "commit": "b32hash...", "wal_seg": 44, "parent": null, "fork_at": null },
    "agent42": { "commit": "...", "wal_seg": 3, "parent": "main", "fork_at": {"commit":"...", "wal_seg":44} }
  },
  "tables": {                     // 表目录
    "t": { "id": 1, "schema": <chunk_hash_of_schema>, "col_gen": 17,
           "col_rows": 12000000, "col_seg_range": [3, 17] }
  },
  "gc": { "min_obj_age_s": 300, "last_sweep_ver": 120 },
  "writer": { "epoch": 3 }        // fence epoch，多写者互斥(SPEC 02 §6)
}
```

提交协议（乐观并发，支持多客户端并发提交同一库）：

```
loop:
  ver = 读到的最新 manifest.version          // 读路径: 从缓存版本起 head 探测 +1，
                                            // 超 32 次才 list_prefix（slatedb 同款）
  new = rebase(my_changes, ver)              // 冲突时上层决策(单写者/分支下几乎无冲突)
  put_if_absent(manifest/{ver+1:020}.json, new)
  on Exists → 读最新 manifest，回到 loop（有界重试，默认 64）
```

- **读最新版本以 LIST 为权威路径**（v1.1 修订）：GC 删除使版本号空间存在
  任意空洞，探测无法区分"已是最新"与"撞洞"（后者会放行停滞写者写进已删
  版本号 → 影子谱系）。manifest 版本数 ≤17（GC 保留窗口），每次
  `load_latest` 恰 1 个 LIST；写者发布后自采纳缓存（不再额外 LIST）。
- manifest 对象写入后不可变 ⇒ 读者拿到 (version) 即拿到一致快照，无需读锁。
- 快照引用计数：分支引用的 commit/wal 段被 GC 保护；`branch create` 时在 fork 处
  记 `fork_at`，父分支 GC 不得越过任何子分支 fork 点（neon retain_lsns 同思路）。

## 6. GC

**已实现（v1，机制 = 墓碑 + 保留窗口，权威口径见 `docs/design/GC定案.md`）**：

- 列存段（全量重建被替换）/ WAL 旧 epoch 目录 / WAL 当前 epoch 前缀段：
  墓碑随"新 manifest 停止引用"**同一版本原子发布**，`gc_retention_ms`
  （默认 24h）后由 `engine.rs::gc_sweep` 物理删除（checkpoint 尾部与打库
  各一次；单批 ≤256）。恢复路径经 `BranchHead.wal_first_seg` 容忍 WAL
  前缀空洞。
- manifest 版本：保留最近 **K=16** 个（`ManifestStore::retained`），
  其余直接删除；`load_latest` 探测遇空洞回落 LIST（影子谱系防护）。
- **CAS chunk（prolly 节点 / commit 对象）v1 不回收**（time travel 依赖 +
  全根可达性分析成本），v2 候选：引用计数入 manifest 快照。旧标记-清扫
  设计（min_age + 引用集）保留为 v2 方案参考。

## 7. 本地缓存（读路径）

`CachedObjStore`：包装底层 store，LRU(字节配额, 默认 2GiB)缓存 byte-range GET 与整对象；
对 CBF/parquet 的 footer、prolly 节点命中友好。本地盘目录 `{cache_root}`，
崩溃可丢。写路径**不**经过缓存（对象存储写直传；WAL 可选 local-staged 见 SPEC 02 §7）。

## 8. S3 兼容实现

v1 以 Local/Memory 为主，S3 兼容实现（`s3` feature，`object_store` crate 或直连
S3 REST）遵循同一 trait：`put_if_absent` = `PutObject` + `If-None-Match: *`；
bucket 需开 versioning 关闭（对象不可变）。put-id 反查用对象 metadata。

## 9. 与实现的映射

| SPEC 条目 | 代码 |
|-----------|------|
| trait/实现 | `dendro-core/src/objstore/{mod,local,memory,throttled,cached}.rs` |
| CAS chunk | `dendro-core/src/objstore/cas.rs` |
| manifest | `dendro-core/src/objstore/manifest.rs` |
| 恢复探测 | `dendro-core/src/objstore/probe.rs` |
| GC | `dendro-core/src/objstore/gc.rs`（v1: 标记+延迟清扫）|
