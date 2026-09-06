# SPEC 06 — Wire 协议：PG 优先，MySQL 次之

状态：**定稿 v1**（协议与实现分离的落点：两个协议层只做"字节 ↔ 内部 AST/结果集"，无业务逻辑）

## 1. 协议无关核心

```rust
// dendro-core::session — 两个协议层共同面向的接口
pub struct Session { /* 当前分支、参数、事务态、预编译语句缓存 */ }
impl Session {
    pub fn exec(&mut self, stmt: &str, params: &[Value]) -> Result<ExecOutput>;
}
pub enum ExecOutput {
    Command { tag: String, rows_affected: u64 },      // DDL/DML
    Rows(RecordSet),                                   // SELECT
    CopyIn/...                                        // v1 报错
}
```

方言差异在 translate 层吸收：
| 差异 | PG | MySQL | 内部统一 |
|------|----|-------|---------|
| 占位符 | `$1..$n` | `?` | 内部 `Placeholder(usize)`，协议层注入映射 |
| 标识符引用 | `"..."` | `` `...` `` | sqlparser 方言解析后统一为裸标识符 |
| LIMIT 语法 | `LIMIT n OFFSET m` | `LIMIT m, n` 兼容 | 同一 Limit{skip,take} |
| 布尔字面量 | `true/false` | `1/0`、`TRUE` | Value::Bool |

## 2. PostgreSQL wire (v3) — dendro-pgwire

实现参照：neon `libs/postgres_backend/src/lib.rs`（单文件最小服务端）、`libs/pq_proto`（帧编解码）。

### 2.1 生命周期
```
startup → SSLRequest?(拒绝, 'N') / GSSEnc?(拒绝) / ProtocolVersion(3.x)
        → AuthenticationOk (trust) / AuthenticationCleartextPassword (配密码时)
        → ParameterStatus×N (server_version=17.2-dendro, client_encoding=UTF8,
          DateStyle=ISO,MDY, standard_conforming_strings=on, integer_datetimes=on)
        → BackendKeyData{pid, secret} → ReadyForQuery{status: I/T/E}
loop: Q/ P/B/D/E/S/F/H/C/X ...（见 2.2）
```

### 2.2 消息处理矩阵
| 消息 | 处理 |
|------|------|
| Query (Q) | 多语句按 `;` 分隔逐条执行，每条 CommandComplete/EmptyQueryResponse 或 ErrorResponse(后继续跑)；事务态由语句决定 |
| Parse (P) | 翻译并缓存 prepared(name→plan, 参数类型)；未知类型按 PG 类型 OID 推断 |
| Bind (B) | 参数二进制/文本格式解码 → portal |
| Describe (D) | 'S'/'P' → ParameterDescription + RowDescription(NoData) |
| Execute (E) | 执行 portal，行数受 max_rows 限制，PortalSuspended |
| Close (C) / Flush (H) / Sync (S) | 释放 / 冲刷 / 事务边界结算（隐式事务回滚若出错） |
| Terminate (X) | 关闭 |
| CopyIn/Out (D/f) | ErrorResponse `0A000` feature_not_supported |
| CancelRequest | 按 BackendKeyData 定位会话置取消标志（执行器协作点检查） |

### 2.3 类型映射（文本/二进制编解码）
| 内部类型 | PG OID | 文本 | 二进制 |
|----------|--------|------|--------|
| Bool | 16 | t/f | 1B |
| Int32 | 23 | 十进制 | BE 4B |
| Int64 | 20 | 十进制 | BE 8B |
| Float64 | 701 | shortest repr | BE 8B IEEE754 |
| Utf8 | 25 | 原文 | 原文 |
| Bytes | 17 | `\xhex` | 原文 |
| Date | 1082 | ISO 8601 | i32 days-since-2000-01-01 BE |
| Timestamp(µs) | 1114 | ISO | i64 µs-since-2000-01-01 BE |
| Null | — | 空串(列长度 -1) | 长度 -1 |

### 2.4 错误与 SQLSTATE
内部错误 → ErrorResponse 三字段映射（severity, code, message）：
冲突 40001、未定义表 42P01、语法 42601、类型 42804、除零 22012、
唯一冲突 23505（v1 有 PK ⇒ 主键重复）、分支不存在 3D000 等。
NOTICE 用于 merge 冲突清单提示。

### 2.5 catalog 仿真（客户端工具兼容）
- `pg_catalog` 最小视图：`pg_tables`、`pg_class`(relkind)、`pg_type`(常用 OID)、
  `pg_namespace`、`pg_settings`(可读)——psql `\d`、JDBC 元数据可用的下限
- `SHOW server_version` 等；`SET` 全部接受并记录会话参数（值有效性宽松）

## 3. MySQL wire — dendro-mywire

实现参照：vitess go/mysql 包结构、ClickHouse `src/Server/MySQLHandler.cpp`、dolt server。

```
握手: Server Greeting v10 {version "8.0.36-dendro", conn_id, auth-plugin-data 20B,
     capability: LONG_PASSWORD|PROTOCOL_41|TRANSACTIONS|SECURE_CONNECTION|
                 PLUGIN_AUTH|DEPRECATE_EOF|CONNECT_WITH_DB, charset utf8mb4(45),
     auth_plugin: mysql_native_password}
认证: HandshakeResponse41 → native password 校验(SHA1 异或式) → OK/ERR
     （caching_sha2_password 依赖 TLS，v1 不宣告）
命令: COM_QUIT/COM_INIT_DB/COM_PING/COM_FIELD_LIST/COM_STATISTICS + COM_QUERY
COM_QUERY: text resultset: column_count lenenc → ColumnDefinition41×N
     → (Row: lenenc 文本列, NULL=0xFB)×M → EOF 或 OK(DEPRECATE_EOF)
     多语句: CLIENT_MULTI_STATEMENTS 协商后按 ; 拆分
错误包: code(2B LE) + '#' + SQLSTATE(5B) + message —— SQLSTATE 与 PG 层共用映射表
不支持: COM_STMT_PREPARE → ERR(ER_UNKNOWN_COM_ERROR)，文档注明 JDBC 需
     useServerPrepStmts=false + useLocalSessionState=true（v2 再补二进制协议）
```

## 4. 传输与装配

- tokio;每连接一个 task；执行在会话绑定的专用阻塞线程池（防大查询饿死 IO）
- `dendro-server` 同时监听 PG(默认 5432)/MySQL(默认 3306)，可关
- 取消：PG CancelRequest / MySQL KILL QUERY → 会话原子标志，执行器循环检查

## 5. 与实现的映射

| SPEC 条目 | 代码 |
|-----------|------|
| PG 帧编解码 | `dendro-pgwire/src/codec.rs` |
| PG 会话循环 | `dendro-pgwire/src/server.rs` |
| PG extended query | `dendro-pgwire/src/extended.rs` |
| MySQL 包编解码 | `dendro-mywire/src/codec.rs` |
| MySQL 会话循环 | `dendro-mywire/src/server.rs` |
| SQLSTATE 映射 | `dendro-core/src/sql/error.rs` |
