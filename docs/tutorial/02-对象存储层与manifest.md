# 02 · 对象存储层与 manifest

> 代码：`crates/dendro-core/src/objstore/`（trait 定义 `mod.rs`，S3 适配 `s3.rs`，
> 读缓存 `cached.rs`，元数据根 `manifest.rs`）

## 1. ObjStore trait——五个动词定义一切

```rust
pub trait ObjStore: Send + Sync + 'static {
    fn get(&self, path: &str) -> ObjResult<Bytes>;
    fn get_range(&self, path: &str, off: u64, len: usize) -> ObjResult<Bytes>;
    fn put(&self, path: &str, data: Bytes) -> ObjResult<()>;
    fn put_if_absent(&self, path: &str, data: Bytes) -> ObjResult<()>;   // ★
    fn delete(&self, path: &str) -> ObjResult<()>;
    fn head(&self, path: &str) -> ObjResult<Option<HeadInfo>>;
    fn list_prefix(&self, prefix: &str) -> ObjResult<Vec<String>>;        // 仅兜底
    fn copy(&self, from: &str, to: &str) -> ObjResult<()>;
}
```

`put_if_absent` 是整个数据库事务性的基石：

```
实现映射：
  Local   → open(O_CREAT|O_EXCL)               原子性来自 POSIX
  Memory  → BTreeMap 插入前查重                 原子性来自 RwLock
  S3      → PutObject + If-None-Match: "*"     原子性来自服务端条件写
            RustFS/MinIO 冲突返回 412 → Err(Exists)
```

错误模型只有四类，调用方据此决定重试语义：

| 错误 | 含义 | 调用方动作 |
|------|------|-----------|
| `Exists` | 条件写冲突 | 乐观提交：重读→改→重试 |
| `NotFound` | 对象不存在 | 常规业务分支 |
| `Transient` | 明确失败（4xx/解析失败）| 重试或报错 |
| `Uncertain` | **可能成功可能失败**（超时/断连）| 见 §4 消解协议 |

## 2. 存储根布局（磁盘=桶，一字不差）

```
{root}/
├── manifest/00000000000000000001.json     ← 元数据根，版本单调递增
├── objects/{首字符}/{其余 31 字符}.chunk   ← 内容寻址（03 章）
├── wal/{branch}/{seg:020}.wal             ← WAL 段（05 章）
├── col/{table}/{hash12}.cbf               ← 列存段（07 章）
└── (fence/{branch}                       ← 跨进程写者 fencing，v2)
```

三条设计规则：

1. **编号一律 20 位零填充**：`00000000000000000042` 的字典序=数值序，
   `list_prefix` 浏览有序、人眼可扫。
2. **路径字符白名单** `[A-Za-z0-9_./-]`，禁 `..`——`validate_path()` 在
   Memory/Local 上强制，杜绝路径逃逸。
3. **对象不可变**。写入后没有任何代码路径会改写它；删除只发生在 GC
   （延迟删除，v2）。不可变让读端永远不需要锁，也让 HTTP/CDN 缓存语义天然成立。

## 3. manifest——数据库的大脑（逐字段）

manifest 是**唯一的可变引用点**：分支头、表目录指针、GC 水位全在里面。
JSON 编码（v1；对象量小、人类可调试优先），真实样例：

```json
{
  "version": 3,                          // 单调递增；文件名含同值
  "writer_putid": "3-1f2a8c",            // 本次提交的消解凭证（§4）
  "format_version": 1,
  "refs": {
    "main": {
      "commit":     "k7q2...",           // 分支头 commit chunk 地址（base32）
      "wal_seg":    44,                  // 已 durable 的最高 WAL 段号
      "parent":     null,                // 父分支（分支树）
      "fork_commit":"k7q2...",           // fork 点（CREATE BRANCH 时定格）
      "fork_wal_seg": 44,
      "epoch":      0,                   // 跨进程写者 fence（v2）
      "covered_seq": 118230              // 已物化进树的最高事务 seq
    },
    "agent42": { "commit": "m81x…", "wal_seg": 3, "parent": "main",
                 "fork_commit": "k7q2…", "fork_wal_seg": 44, … }
  },
  "tables": {},                          // v1 表目录在树内 catalog（08 章），此字段备用
  "next_table_id": 2,
  "gc_last_sweep_ver": 0,
  "created_ms": 1788750785107
}
```

文件名即版本：`manifest/{version:020}.json`。版本链：

```
manifest/…0001.json ──▶ manifest/…0002.json ──▶ manifest/…0003.json ──▶ …
        │                      │                      │
     (不可变)               (不可变)                (不可变)
```

**读最新版 = 常数次 HEAD，不靠 LIST**：

```
缓存版本 c=5:
  HEAD manifest/…0006 → 404 ?  ⇒ 5 就是最新
                       → 200 ?  ⇒ 继续 0007…（最多探测 32 个）
  32 次未收敛 ⇒ list_prefix("manifest/") 兜底（运维场景）
```

### 3.1 乐观提交协议（所有"改 manifest"的唯一路径）

```
     客户端 A                客户端 B                  对象存储
        │                      │                        │
        ├─ GET  ver=7 ─────────┼────────────────────────▶│
        ├─ 修改(m′ based 7)    ├─ 修改(m″ based 7)       │
        ├─ PUT-IF-ABSENT ver=8 │                        │
        │                      ├─ PUT-IF-ABSENT ver=8    │
        │◀──── Ok(8) ──────────┼───────────┐             │
        │                      │◀── 412 Exists ──────────│  ← B 输了
        │                      ├─ GET ver=8 → 7+m′ 重改  │
        │                      ├─ PUT-IF-ABSENT ver=9 ──▶│
        │                      │◀──── Ok(9) ─────────────│
```

`database.update_manifest(f)` 把"GET→改→PUT-IF-ABSENT"包成带 64 次重试的
闭包调用；单写者/分支模型下几乎永不冲突。

## 4. 不确定写消解——"结果未知"的工程学

对象存储的 PUT 在网络超时后有一个狰狞的中间态：**请求可能已经成功**。
盲目重试可能写出两个不同内容；放弃则可能丢数据。

Dendro 的消解（S3 适配器返回 `ObjError::Uncertain` 后）：

```
对象类型            消解方式
─────────────────────────────────────────────────────────────
manifest/*.json    payload 内嵌 writer_putid 字段；
                   Uncertain → GET 该版本 → putid 相等？
                              ├─ 是 ⇒ 其实成功了 ⇒ Ok
                              └─ 否 ⇒ 别人写赢了 ⇒ Exists
objects/*.chunk    内容寻址，同内容同地址同字节 ⇒ 同字节重试幂等
wal/*.wal          段号由 fence 持有者独占 ⇒ 同字节重试幂等
col/*.cbf          路径含内容哈希前缀 ⇒ 幂等
```

manifest 的消解凭证就是这么来的（`manifest.rs::commit`）：

```rust
let putid = format!("{}-{:x}", cur + 1, rand_u64());
new.writer_putid = Some(putid.clone());           // 写进 JSON 本身
match obj.put_if_absent(&path, payload) {
    Err(ObjError::Uncertain(_)) => match self.read_version(cur + 1) {
        Ok(m) if m.writer_putid == Some(putid) => Ok(cur + 1),  // 其实成功了
        Ok(_) => Err(Exists),                                    // 别人写赢了
        …
    }
}
```

> 为什么不用 S3 用户元数据？slatedb 靠元数据反查，但 `object_store` 的
> typed API 不透传用户元数据；payload 内嵌是零成本等价方案。

## 5. 读路径缓存——真 OSS 的可用性前提

`CachedObjStore`（`objstore/cached.rs`）包在 S3 外面：

```
        引擎读 get/get_range
               │
               ▼
        ┌─ 本地缓存命中？ ──是──▶ 直接返回（NVMe 速度，~µs）
        │        │ 否
        │        ▼
        │  远端 GET ──▶ 写缓存文件(tmp+rename 原子) ──▶ 返回
        └──────────────────────────────────────────────┘
缓存文件名：{fnv64(path):016x}-{off}-{len}.c
淘汰：字节预算（默认 1GiB），近似 LRU（atime 最小者先出）
统计：hits / misses 原子计数器
```

RustFS 实测（10 章）：12k 行全生命周期仅 9 次远端 GET——其余全部命中。

## 6. 延迟注入——把"慢 OSS"装进测试

`ThrottledObjStore` 包任意实现，注入每次操作的延迟（均值±20% 抖动）与并发上限。
`DENDRO_S3_RTT_MS=100` 环境变量即可把真 S3 栈变成"跨地域 OSS"，用于验证
组提交对慢网络的吸收能力（实测 p50 76ms 单行提交，见 10 章 §4）。
