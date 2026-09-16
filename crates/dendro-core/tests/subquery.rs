//! P0：子查询内联（非相关 → 常量/InList/Bool——PG SubLink→InitPlan 同构）
//! 标量子查询 / IN 子查询 / EXISTS / NOT EXISTS / 相关子查询拒绝

use dendro_core::{Database, DbOptions, Output, StoreConfig};
use std::sync::Arc;

fn db() -> Arc<Database> {
    Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap()
}

fn setup(d: &Arc<Database>) {
    let mut s = d.new_session();
    s.exec("CREATE TABLE orders (id BIGINT PRIMARY KEY, cid BIGINT, total BIGINT)").unwrap();
    s.exec("CREATE TABLE customers (id BIGINT PRIMARY KEY, region TEXT, vip BOOLEAN)").unwrap();
    s.exec("INSERT INTO orders VALUES (1, 1, 100), (2, 2, 250), (3, 1, 50), (4, 3, 80)").unwrap();
    s.exec("INSERT INTO customers VALUES (1, 'EU', true), (2, 'US', false), (3, 'APAC', true)").unwrap();
}

fn count(d: &Arc<Database>, sql: &str) -> i64 {
    let mut s = d.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap().parse().unwrap(),
        _ => panic!(),
    }
}

#[test]
fn scalar_subquery_in_where() {
    let d = db(); setup(&d);
    // WHERE total > (SELECT avg(total) FROM orders)
    let n = count(&d, "SELECT count(*) FROM orders WHERE total > (SELECT avg(total) FROM orders)");
    // avg = (100+250+50+80)/4 = 120 → total > 120: 只有 250
    assert_eq!(n, 1);
}

#[test]
fn in_subquery_in_where() {
    let d = db(); setup(&d);
    // WHERE cid IN (SELECT id FROM customers WHERE vip = true)
    let n = count(&d, "SELECT count(*) FROM orders WHERE cid IN (SELECT id FROM customers WHERE vip = true)");
    // vip customers: 1, 3 → orders with cid ∈ {1,3}: id 1, 3, 4 → 3 行
    assert_eq!(n, 3);
}

#[test]
fn not_in_subquery() {
    let d = db(); setup(&d);
    let n = count(&d, "SELECT count(*) FROM orders WHERE cid NOT IN (SELECT id FROM customers WHERE vip = true)");
    // 非 vip: cid=2 → orders id 2 → 1 行
    assert_eq!(n, 1);
}

#[test]
fn exists_subquery() {
    let d = db(); setup(&d);
    // EXISTS (SELECT FROM customers WHERE vip = true) → true → 全部行
    let n = count(&d, "SELECT count(*) FROM orders WHERE EXISTS (SELECT id FROM customers WHERE vip = true)");
    assert_eq!(n, 4);
}

#[test]
fn not_exists_subquery() {
    let d = db(); setup(&d);
    let n = count(&d, "SELECT count(*) FROM orders WHERE NOT EXISTS (SELECT id FROM customers WHERE region = 'XX')");
    // 无 region='XX' → NOT EXISTS = true → 4 行
    assert_eq!(n, 4);
}

#[test]
fn nested_scalar_subquery() {
    let d = db(); setup(&d);
    // 双层嵌套：(SELECT max(total) FROM orders WHERE cid = (SELECT id FROM customers WHERE region = 'EU'))
    let n = count(&d, "SELECT count(*) FROM orders WHERE total >= (SELECT max(total) FROM orders WHERE cid = (SELECT id FROM customers WHERE region = 'EU'))");
    // EU → cid=1 → max(total) where cid=1 = max(100,50)=100 → total>=100: id 1,2 → 2 行
    assert_eq!(n, 2);
}

#[test]
fn scalar_subquery_with_aggregate_eq() {
    let d = db(); setup(&d);
    // WHERE total = (SELECT max(total) FROM orders) → 只有 250
    let n = count(&d, "SELECT count(*) FROM orders WHERE total = (SELECT max(total) FROM orders)");
    assert_eq!(n, 1);
}

#[test]
fn empty_in_subquery() {
    let d = db(); setup(&d);
    let n = count(&d, "SELECT count(*) FROM orders WHERE cid IN (SELECT id FROM customers WHERE region = 'XX')");
    assert_eq!(n, 0);
}

#[test]
fn correlated_subquery_rejected() {
    let d = db(); setup(&d);
    let mut s = d.new_session();
    // 真相关子查询：子查询自身 FROM customers，但引用外层 orders.total
    //（customers 无 total 列 → 独立求值失败 → 相关 → not_supported）
    let e = s
        .exec("SELECT count(*) FROM orders WHERE total > (SELECT avg(total) FROM customers WHERE customers.id = orders.cid)")
        .unwrap_err();
    assert!(
        e.message.contains("correlated") || e.message.contains("not supported"),
        "{e}"
    );
}
