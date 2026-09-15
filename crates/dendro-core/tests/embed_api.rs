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
