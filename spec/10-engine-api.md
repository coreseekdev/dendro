# SPEC 10 — 引擎门面 API 契约（wire 层 ↔ core）

状态：**定稿 v1**（本文件是 dendro-pgwire / dendro-mywire / slt / bench 的开发契约）

## 1. 入口

```rust
use dendro_core::{Database, DbOptions, StoreConfig, Durability};

let db = Database::open(DbOptions {
    store: StoreConfig::LocalDir("/tmp/dendro-data".into()), // 或 StoreConfig::Memory
    wal_flush_interval_ms: 50,
    wal_segment_bytes: 32 << 20,
    durability: Durability::Group,
})?;
let mut sess = db.new_session();                 // 每连接一个；!Send 由使用方保证
let outs: Vec<Output> = sess.exec("SELECT 1")?;  // 多语句→多 Output；纯注释→空 Vec
```

## 2. 预编译（PG extended 协议映射）

```rust
// Parse:
let meta: PrepareMeta = sess.prepare(name, sql, param_hint)?;
//   meta.param_types    — 参数类型（推断顺序：hint 优先，否则语句内推断，int4 兜底）
//   meta.result_columns — Describe 'S' 用
// Bind+Execute:
let out: Output = sess.exec_prepared(name, &params)?;   // v1 无 PortalSuspended
sess.close_prepared(name);                              // Close 'S'；未知名静默
```

约定：
- 未知名 exec_prepared → SqlError "26000"（invalid_sql_statement_name）
- 参数个数/类型不符 → "08P01"；UNNAMED 语句重复 Parse 覆盖旧的
- Describe 未知名 → "26000"；Describe 只在 prepare 后有效（不单列 portal 元数据，
  portal 与 statement 同列集）

## 3. Output 形态

```rust
pub enum Output {
    Command { tag: String, affected: u64 },   // tag 已按 PG 口径组好(如 "INSERT 0 1")
    Rows(RecordSet),                          // 列元数据 + Arrow 批
}
pub struct RecordSet { pub columns: Vec<ColumnMeta>, pub batches: Vec<RecordBatch> }
// RecordSet::text_rows() -> Vec<Vec<Option<String>>>  — text 协议行；None=NULL
// RecordSet::total_rows() -> usize
```

## 4. 错误

`SqlError { state: SQLSTATE, message }` — 直接映射 ErrorResponse；
severity 统一 ERROR（致命断连类由 wire 层自判）。

## 5. 会话态约定（wire 层需要的）

- `sess.exec("USE BRANCH x")` / `CREATE BRANCH ...` 等（SPEC 03 §5）走普通 exec
- 事务语句 BEGIN/COMMIT/ROLLBACK 走普通 exec；wire 层 ReadyForQuery 的事务状态：
  由最近一次 exec 的输出推断——`Output::Command.tag == "BEGIN"` → T 状态；
  错误 → E（失败事务，后续语句返回 25P02 直到 ROLLBACK——由引擎内部处理，
  wire 层只需在 ErrorResponse 后保持连接）。引擎后续提供 `sess.txn_status()`。

## 6. 多语句切分

`exec` 内部用 sqlparser 分割（含 dollar-quoted 字符串感知）。wire 层不要自行按 `;` 切。

## 7. 线程模型

- `Database: Send + Sync`（Arc 共享）
- `Session` 单线程使用；wire 层：每连接 `tokio::task::spawn_blocking` 独占线程，
  socket IO 用 channel 交给 async 侧（或简化：连接处理整体放 blocking 线程，
  std::net + std::io —— **推荐后者**，v1 用 std::net::TcpListener + 线程池即可，
  tokio 非必须）

## 8. wire 层各自职责（不许越界）

| | dendro-pgwire | dendro-mywire |
|---|---|---|
| 帧协议 | PG v3 全部(SPEC 06 §2) | MySQL v10 握手/text 结果集(SPEC 06 §3) |
| 语句切分 | 交给 sess.exec | 交给 sess.exec（CLIENT_MULTI_STATEMENTS） |
| 参数编码 | 文本+二进制 → SqlValue | 文本 → SqlValue |
| 结果编码 | text 优先（format=0） | lenenc 文本 |
| 类型 OID | ColumnMeta.ty.pg_oid() | 映射 MySQL 类型码 |
| 认证 | trust / cleartext | mysql_native_password |
| 不支持 | COPY→0A000 | COM_STMT_PREPARE→ERR |
