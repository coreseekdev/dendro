//! P0：外键约束 + NOT NULL 执法

use dendro_core::{Database, DbOptions, Output, StoreConfig};
use std::sync::Arc;

fn db() -> Arc<Database> {
    Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap()
}

fn count(d: &Arc<Database>, sql: &str) -> String {
    let mut s = d.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    }
}

fn err(d: &Arc<Database>, sql: &str) -> dendro_core::SqlError {
    let mut s = d.new_session();
    s.exec(sql).unwrap_err()
}

fn setup(d: &Arc<Database>) {
    let mut s = d.new_session();
    s.exec("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT NOT NULL)").unwrap();
    s.exec("CREATE TABLE orders (id BIGINT PRIMARY KEY, cid BIGINT REFERENCES customers(id), note TEXT)").unwrap();
    s.exec("INSERT INTO customers VALUES (1, 'alice'), (2, 'bob')").unwrap();
}

#[test]
fn fk_insert_valid() {
    let d = db(); setup(&d);
    // FK 指向存在的父行 → 成功
    let mut s = d.new_session();
    s.exec("INSERT INTO orders VALUES (1, 1, 'o1')").unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM orders"), "1");
}

#[test]
fn fk_insert_violation_rejected() {
    let d = db(); setup(&d);
    // FK 指向不存在的父行 → 23503
    let e = err(&d, "INSERT INTO orders VALUES (1, 99, 'o1')");
    assert_eq!(e.state, "23503", "{e}");
    assert!(e.message.contains("foreign key"), "{e}");
    // 表无残留
    assert_eq!(count(&d, "SELECT count(*) FROM orders"), "0");
}

#[test]
fn fk_null_allowed() {
    let d = db(); setup(&d);
    // FK 列 NULL → 跳过检查（SQL 外键语义）
    let mut s = d.new_session();
    s.exec("INSERT INTO orders VALUES (1, NULL, 'o1')").unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM orders"), "1");
}

#[test]
fn not_null_rejected() {
    let d = db(); setup(&d);
    // name NOT NULL → INSERT NULL 报 23502
    let e = err(&d, "INSERT INTO customers VALUES (3, NULL)");
    assert_eq!(e.state, "23502", "{e}");
    assert!(e.message.contains("not-null"), "{e}");
}

#[test]
fn fk_table_level_syntax() {
    let d = db(); setup(&d);
    // 表级 FOREIGN KEY 语法
    let mut s = d.new_session();
    s.exec("CREATE TABLE items (id BIGINT PRIMARY KEY, oid BIGINT, FOREIGN KEY (oid) REFERENCES orders(id))").unwrap();
    s.exec("INSERT INTO orders VALUES (10, 1, 'ten')").unwrap();
    s.exec("INSERT INTO items VALUES (1, 10)").unwrap();
    // FK 违反
    let e = err(&d, "INSERT INTO items VALUES (2, 99)");
    assert_eq!(e.state, "23503", "{e}");
}

#[test]
fn fk_nonexistent_parent_table() {
    let d = db(); setup(&d);
    // REFERENCES 不存在的表 → 建表时报错
    let e = err(&d, "CREATE TABLE bad (id BIGINT PRIMARY KEY, x BIGINT REFERENCES nonexistent(id))");
    assert!(
        e.message.contains("non-existent") || e.message.contains("not exist"),
        "{e}"
    );
}

#[test]
fn fk_multiple_violations_batch() {
    let d = db(); setup(&d);
    // 批量 INSERT 混合合法/非法 → 整条失败
    let e = err(&d, "INSERT INTO orders VALUES (1, 1, 'ok'), (2, 99, 'bad')");
    assert_eq!(e.state, "23503", "{e}");
    assert_eq!(count(&d, "SELECT count(*) FROM orders"), "0"); // 原子性
}

// ---------- UNIQUE 约束 ----------

#[test]
fn unique_insert_violation() {
    let d = db();
    let mut s = d.new_session();
    s.exec("CREATE TABLE u (id BIGINT PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    s.exec("INSERT INTO u VALUES (1, 'a@x.com')").unwrap();
    let e = err(&d, "INSERT INTO u VALUES (2, 'a@x.com')");
    assert_eq!(e.state, "23505", "{e}");
    assert!(e.message.contains("unique"), "{e}");
}

#[test]
fn unique_null_allowed_multiple() {
    let d = db();
    let mut s = d.new_session();
    s.exec("CREATE TABLE u (id BIGINT PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    s.exec("INSERT INTO u VALUES (1, NULL), (2, NULL)").unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM u"), "2");
}

#[test]
fn unique_table_level() {
    let d = db();
    let mut s = d.new_session();
    s.exec("CREATE TABLE u (id BIGINT PRIMARY KEY, a INT, b INT, UNIQUE(a, b))").unwrap();
    s.exec("INSERT INTO u VALUES (1, 10, 20)").unwrap();
    let e = err(&d, "INSERT INTO u VALUES (2, 10, 20)");
    assert_eq!(e.state, "23505", "{e}");
    s.exec("INSERT INTO u VALUES (3, 10, 30)").unwrap();
    assert_eq!(count(&d, "SELECT count(*) FROM u"), "2");
}
