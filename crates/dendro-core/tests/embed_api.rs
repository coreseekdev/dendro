//! 嵌入式 API 集成测试

use dendro_core::embed::{Connection, Value};
use dendro_core::types::SqlValue;

#[test]
fn embed_basic_crud() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'hello')").unwrap();
    let n = conn.execute("INSERT INTO t VALUES (2, 'world')").unwrap();
    assert_eq!(n, 1);
    let r = conn.query("SELECT id, name FROM t ORDER BY id").unwrap();
    assert_eq!(r.row_count(), 2);
    assert_eq!(r.get_i64(0, 0), Some(1));
    assert_eq!(r.get_string(0, 1), Some("hello".to_string()));
    assert_eq!(r.get_string(1, 1), Some("world".to_string()));
}

#[test]
fn embed_prepared_with_bind() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    let mut stmt = conn.prepare("INSERT INTO t VALUES ($1, $2)").unwrap();
    for i in 0..5 {
        stmt.execute(&[Value::Integer(i), Value::Text(format!("v{}", i))])
            .unwrap();
    }
    drop(stmt);
    let r = conn.query("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.rows().len(), 1, "count(*) 返回 1 行");
    assert_eq!(r.rows()[0][0], SqlValue::Int64(5), "应恰好 5 行已插入");
}

#[test]
fn embed_transaction_rollback_on_drop() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();
    {
        let mut tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO t VALUES (2)").unwrap();
        // Drop without commit → auto ROLLBACK
    }
    let r = conn.query("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.rows().len(), 1, "未 commit 的行不应可见");
}

#[test]
fn embed_transaction_commit() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();
    let mut tx = conn.transaction().unwrap();
    tx.execute("INSERT INTO t VALUES (42)").unwrap();
    tx.commit().unwrap();
    let r = conn.query("SELECT count(*) FROM t").unwrap();
    assert_eq!(r.rows().len(), 1);
}

#[test]
fn embed_file_persistence() {
    let dir = std::env::temp_dir().join(format!("embed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut conn = Connection::open(&dir).unwrap();
        conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    {
        let mut conn = Connection::open(&dir).unwrap();
        let r = conn.query("SELECT count(*) FROM t").unwrap();
        assert_eq!(r.rows().len(), 1, "文件持久化：reopen 后数据仍可见");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn embed_query_scalar() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (42)").unwrap();
    let v = conn.query_scalar_i64("SELECT count(*) FROM t").unwrap();
    assert_eq!(v, Some(1));
}

#[test]
fn embed_branch_operations() {
    let conn = Connection::memory().unwrap();
    {
        let mut s = conn.database().new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        s.exec("INSERT INTO t VALUES (1, 'base')").unwrap();
    }
    conn.create_branch("dev", "main").unwrap();
}

/// embed 默认 SQLite 方言：`?` 位置参数开箱可用（与 dendro-sqlite
/// C ABI 同一语义面）；`$1` 兼容不变
#[test]
fn embed_sqlite_dialect_default_positional() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE q (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    let mut ins = conn.prepare("INSERT INTO q VALUES (?, ?)").unwrap();
    ins.execute(&[Value::Integer(1), Value::Text("a".into())])
        .unwrap();
    ins.execute(&[Value::Integer(2), Value::Text("b".into())])
        .unwrap();
    drop(ins);
    let mut sel = conn.prepare("SELECT v FROM q WHERE id = ?").unwrap();
    let r = sel.query(&[Value::Integer(2)]).unwrap();
    assert_eq!(r.get_string(0, 0).as_deref(), Some("b"));
    drop(sel);
    // $1 兼容（SQLiteDialect 亦接受 PG 数字占位符）
    let mut sel2 = conn.prepare("SELECT v FROM q WHERE id = $1").unwrap();
    let r2 = sel2.query(&[Value::Integer(1)]).unwrap();
    assert_eq!(r2.get_string(0, 0).as_deref(), Some("a"));
}

/// SQLite rowid 语义：INTEGER PRIMARY KEY 列即 rowid 别名——
/// last_insert_rowid 真值 + 查询引用（裸/限定/谓词）
#[test]
fn embed_rowid_semantics() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (10, 'a')").unwrap();
    assert_eq!(c.last_insert_rowid(), 10);
    c.execute("INSERT INTO t VALUES (25, 'b')").unwrap();
    assert_eq!(c.last_insert_rowid(), 25);
    // 查询引用：SELECT rowid / WHERE rowid = N / 限定 r.rowid
    let r = c.query("SELECT rowid, v FROM t WHERE rowid = 25").unwrap();
    assert_eq!(r.get_i64(0, 0), Some(25));
    assert_eq!(r.get_string(0, 1).as_deref(), Some("b"));
    let r2 = c
        .query("SELECT r._rowid_ FROM t r WHERE r.oid = 10")
        .unwrap();
    assert_eq!(r2.get_i64(0, 0), Some(10));
    // 多行插入记末行
    c.execute("INSERT INTO t VALUES (31, 'c'), (32, 'd')")
        .unwrap();
    assert_eq!(c.last_insert_rowid(), 32);
    // 非整数 PK 表：rowid 引用响亮报错、记账不动
    c.execute("CREATE TABLE s (k TEXT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO s VALUES ('x')").unwrap();
    assert_eq!(c.last_insert_rowid(), 32, "非整数 PK 不改写 last_rowid");
    let e = c
        .query("SELECT rowid FROM s")
        .err()
        .expect("非整数 PK 表 rowid 必须响亮报错");
    assert!(
        e.message.contains("rowid") || e.message.contains("does not exist"),
        "{e}"
    );
}

/// rowid 三方言对齐（档案行为面）：SQLite 全集 / MySQL _rowid /
/// PG 空集（undefined column——ctid 是物理位置非身份，官方不建议
/// 当长期标识；映射到 PK 是语义谎言，保持 PG 忠实）
#[test]
fn rowid_dialect_alignment() {
    use dendro_core::sql::SqlDialect;
    // MySQL：_rowid 生效（官方兼容名），rowid 不生效
    let mut m = Connection::memory().unwrap();
    m.set_dialect(SqlDialect::MySql);
    m.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    m.execute("INSERT INTO t VALUES (7, 'x')").unwrap();
    let r = m.query("SELECT _rowid FROM t").unwrap();
    assert_eq!(r.get_i64(0, 0), Some(7));
    let e = m
        .query("SELECT rowid FROM t")
        .err()
        .expect("MySQL 无 rowid 名");
    assert!(e.message.contains("does not exist"), "{e}");
    // PG：ctid 生效（迁移指南语义——逻辑行身份 = PK，append-only
    // 下比原生物理位置 ctid 更稳定）；rowid/_rowid 非系统列
    let mut p = Connection::memory().unwrap();
    p.set_dialect(SqlDialect::Pg);
    p.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    p.execute("INSERT INTO t VALUES (7, 'x')").unwrap();
    let rc = p.query("SELECT ctid FROM t WHERE ctid = 7").unwrap();
    assert_eq!(rc.get_i64(0, 0), Some(7));
    let e1 = p.query("SELECT rowid FROM t").err().expect("PG 无 rowid");
    let e2 = p.query("SELECT _rowid FROM t").err().expect("PG 无 _rowid");
    assert!(e1.message.contains("does not exist"), "{e1}");
    assert!(e2.message.contains("does not exist"), "{e2}");
    // PG：声明真实 rowid 列自然可用（无改写冲突）
    p.execute("CREATE TABLE r2 (rowid INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    p.execute("INSERT INTO r2 VALUES (1, 'real')").unwrap();
    let r3 = p.query("SELECT rowid FROM r2").unwrap();
    assert_eq!(r3.get_i64(0, 0), Some(1));
    // last_insert_rowid 记账与方言无关（引擎级元数据——mywire OK 包
    // 的 last_insert_id / sqlite3_last_insert_rowid 各取所需）
    assert_eq!(p.last_insert_rowid(), 1);
}

/// 行身份统一约束：无 PK 表建表即拒（存储约束前移——行的寻址键
/// 就是 PK 编码；此前延迟到 INSERT/CHECKPOINT 报错）
#[test]
fn pk_required_at_ddl() {
    let mut c = Connection::memory().unwrap();
    let e = match c.execute("CREATE TABLE nopk (a BIGINT, b TEXT)") {
        Ok(_) => panic!("无 PK 表必须建表即拒"),
        Err(e) => e,
    };
    assert!(e.message.contains("PRIMARY KEY required"), "{e}");
    // 零新列原则：行身份别名（rowid/ctid/_rowid）是查询层名字，
    // 表结构不增列——SELECT * 输出形状不含任何合成列
    c.execute("CREATE TABLE ok (id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    c.execute("INSERT INTO ok VALUES (1, 'x')").unwrap();
    let r = c.query("SELECT * FROM ok").unwrap();
    assert_eq!(r.column_count(), 2, "无合成列泄漏");
}
