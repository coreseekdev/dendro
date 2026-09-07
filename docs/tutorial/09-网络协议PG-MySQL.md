# 09 · 网络协议：PG v3 与 MySQL

> 代码：`crates/dendro-pgwire/`、`crates/dendro-mywire/`
> 协议层是纯适配器：字节 ↔ `WireSession` trait（exec/prepare/exec_prepared），
> 不含任何业务逻辑。方言差异（`$1` vs `?`、引号风格）由 sqlparser 在解析层吸收。

## 1. PostgreSQL wire（v3）

### 1.1 启动握手

```
客户端                                    服务端
  │ startup: len u32 | 196608 u32 |        │
  │ "user\0dendro\0database\0cambium\0\0"  │   ← 参数是 C 字符串对（不是 k=v！）
  ├───────────────────────────────────────▶│
  │        （若先发 SSLRequest 80877103）    │ ← 'N'（不支持 TLS，v1 明文）
  │◀────── AuthenticationOk (R, 0)         │
  │◀────── ParameterStatus × 8             │  server_version / client_encoding /
  │                                        │  DateStyle / integer_datetimes /
  │                                        │  standard_conforming_strings / …
  │◀────── BackendKeyData(pid, secret)     │  ← CancelRequest 用
  │◀────── ReadyForQuery('I')              │  ← I 空闲 / T 事务中 / E 失败事务
```

> 实现中修过的真坑：startup 参数区是交替 C 字符串对；libpq/tokio-postgres
> 都按此格式。参照 neon `pq_proto::tuples()` 的解析。

### 1.2 简单查询（'Q'）

```
Q("SELECT id, v FROM t; INSERT …")          ← 整串交给 sess.exec（多语句切分在引擎）
  │
  ├─ RowDescription { id:Int8, v:Text }     ← tylen：定宽=字节数，变长=-2
  ├─ DataRow ("1", "hello")                 ← 全 text 格式（format code 0）
  ├─ CommandComplete "SELECT 1"
  ├─ CommandComplete "INSERT 0 1"           ← 第二条语句的响应
  └─ ReadyForQuery('I')
错误：某条语句失败 → ErrorResponse{SQLSTATE, message} → 继续后续语句
```

### 1.3 扩展查询（psycopg/JDBC 默认路径）

```
Parse("", "INSERT INTO t VALUES ($1,$2)", oids=[])   → ParseComplete
Describe('S', "")                                    → ParameterDescription [int8,text]
                                                     → RowDescription / NoData
Bind("", "", 格式码, 参数 text/binary)                → BindComplete
Describe('P', "")                                    → RowDescription
Execute("", 0)                                       → DataRow… / CommandComplete
Sync                                                 → ReadyForQuery
```

要点：

- **参数类型推断**：hint 缺失时按语句上下文推（INSERT → 目标列类型，
  `WHERE col = $1` → 列类型）。推断错了 tokio-postgres 会拒绝绑定
  （实测：0 参数语句曾被兜底成 1 参数类型 → Parameters(0,1) 错误）
- **Bind 可要求 binary 结果**（result_formats=[1]）：`encode_cell` 按列型
  做 PG 二进制编码（int8=BE8B、float8=BE8B IEEE754、text=裸 UTF-8、
  date=自 2000 天数 BE4、timestamp=自 2000 µs BE8）
- **列型对齐**：Execute 发送的数据列型必须以 Parse 时的描述为准——
  执行期按数据推断会把 `id=1` 物化成 Int32，二进制编码 4B，客户端按
  int8 读 8B → 必炸。实测踩过。
- 错误后到 Sync 之前：跳过所有消息只回 ReadyForQuery（PG 语义）

### 1.4 SQLSTATE 映射（与 MySQL 层共用一张表）

| 事件 | SQLSTATE |
|------|----------|
| 语法/不支持构造 | 42601 |
| 未定义表 | 42P01 |
| 主键冲突 | 23505 |
| 合并冲突/序列化 | 40001 |
| 未定义分支 | 3D000 |
| COPY / 二进制 prepared(MySQL) | 0A000 / 1047 |

## 2. MySQL 客户端协议（v10）

### 2.1 握手与认证

```
服务端 ──▶ Greeting: protocol=10, version="8.0.36-dendro", conn_id,
                scramble 20B 随机, capability flags（无 SSL/无 DEPRECATE_EOF）,
                charset=45(utf8mb4), status=AUTOCOMMIT, plugin="mysql_native_password"
客户端 ──▶ HandshakeResponse41: capability, max_packet, charset, username,
                auth_response(scramble 参与 SHA1 运算), [database], [plugin]
服务端 ──▶ Ok | AuthSwitchRequest | Err(1045,"28000")

native_password 校验：SHA1(p) XOR SHA1(scramble ‖ SHA1(SHA1(p)))
```

### 2.2 COM_QUERY 与 text 结果集

```
COM_QUERY("SELECT id, v FROM m")
  ──▶ column_count (lenenc 2)
  ──▶ ColumnDefinition41 × 2（类型码：Int64→LONGLONG(8), Utf8→VAR_STRING(253), …）
  ──▶ EOF
  ──▶ Row: lenenc"1" | lenenc"x"      （NULL = 0xFB 单字节）
  ──▶ Row: …
  ──▶ EOF
```

命令覆盖：COM_QUERY/COM_PING/COM_QUIT/COM_INIT_DB/COM_FIELD_LIST/COM_STATISTICS。
`COM_STMT_PREPARE` → ERR 1047（v1 不做二进制协议；JDBC 需
`useServerPrepStmts=false`）。错误包 = code u16 + `#` + 5 字节 SQLSTATE +
message——SQLSTATE 与 PG 层共用映射，PG→MySQL 码再翻一层（42P01→1146）。

帧规则：3B LE 长度 + 1B seq；>0xFFFFFF 自动拆帧；超 max_allowed_packet →
ERR 1153 后断连。seq 纪律：握手 0→响应 2，命令后从 1 起（mysql crate 严格校验）。

## 3. 双协议会话的统一

```
pgwire ─┐
        ├─ Box<dyn WireSession> ──▶ Session（同引擎、同分支模型、同 SQL）
mywire ─┘        ▲
                 └─ MyDialect / PostgreSqlDialect 只在 parse 处分流
```

新增协议 = 新写一个适配 crate + 实现 WireSession 适配；引擎零改动。
这就是"协议与实现分离"的可验证含义。
