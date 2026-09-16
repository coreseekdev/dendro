//! 优化器 O-1 差分测试（spec 12 §3）：`SET dendro.optimize = 'on'|'off'`
//! 两路径结果必须等价（common §1.1）。默认 on——既有 432 测试日常即
//! 回归。重点：LEFT JOIN 右侧下推的语义陷阱形态。

mod common;

use common::assert_rows_equiv;
use dendro_core::embed::Connection;
use dendro_core::types::SqlValue;

fn setup() -> Connection {
    let mut c = Connection::memory().unwrap();
    c.execute(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, cid BIGINT, total BIGINT, note TEXT)",
    )
    .unwrap();
    c.execute(
        "CREATE TABLE customers (id BIGINT PRIMARY KEY, region TEXT, tier INT)",
    )
    .unwrap();
    c.execute(
        "INSERT INTO orders VALUES \
         (1, 1, 100, 'a'),(2, 1, 250, 'b'),(3, 2, 50, 'c'),(4, 2, 500, 'd'),\
         (5, 3, 75, 'e'),(6, NULL, 10, 'f')",
    )
    .unwrap();
    c.execute(
        "INSERT INTO customers VALUES (1, 'EU', 3),(2, 'US', 1),(3, 'APAC', 2)",
    )
    .unwrap();
    c
}

fn run(c: &mut Connection, opt: &str, sql: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    if opt != "default" {
        c.execute(&format!("SET dendro.optimize = '{opt}'")).unwrap();
    }
    let r = c.query(sql).unwrap();
    (r.columns.clone(), r.rows.clone())
}

fn diff(c: &mut Connection, sql: &str) {
    let on = run(c, "on", sql);
    let off = run(c, "off", sql);
    assert_rows_equiv(
        &format!("`{sql}` optimize on vs off"),
        &on.0,
        &on.1,
        &off.0,
        &off.1,
        false,
    );
}

// ---------- 差分：join 查询 × 优化开关 ----------

#[test]
fn inner_join_both_sides_pushed() {
    let mut c = setup();
    // 双侧可下推：o.total > 60（orders）、c.region = 'EU'（customers）
    diff(
        &mut c,
        "SELECT o.id, c.region FROM orders o JOIN customers c ON o.cid = c.id \
         WHERE o.total > 60 AND c.region = 'EU'",
    );
    // 特征值：join 前过滤后仍正确的行集
    let r = run(
        &mut c,
        "on",
        "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id \
         WHERE o.total > 60 AND c.region = 'EU' ORDER BY o.id",
    );
    let ids: Vec<i64> = r
        .1
        .iter()
        .map(|row| match &row[0] {
            SqlValue::Int64(v) => *v,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 2], "EU 且 total>60 的订单");
}

#[test]
fn left_join_right_side_push_is_safe() {
    let mut c = setup();
    // 语义陷阱形态：LEFT JOIN 右侧谓词下推——NULL 扩展行必须仍被
    // join 后保留的原谓词过滤（spec 12 §2 论证）
    diff(
        &mut c,
        "SELECT o.id, c.id FROM orders o LEFT JOIN customers c ON o.cid = c.id \
         WHERE c.region = 'EU'",
    );
    // 全外行（cid=NULL 的订单 6）：两侧都不可见
    let r = run(
        &mut c,
        "on",
        "SELECT o.id FROM orders o LEFT JOIN customers c ON o.cid = c.id \
         WHERE c.region = 'EU' ORDER BY o.id",
    );
    let ids: Vec<i64> = r
        .1
        .iter()
        .map(|row| match &row[0] {
            SqlValue::Int64(v) => *v,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn mixed_conjuncts_cross_table_stays() {
    let mut c = setup();
    // 跨表谓词（o.total > c.tier）+ 单侧谓词混合
    diff(
        &mut c,
        "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id \
         WHERE o.total > 100 AND c.tier >= 1 AND o.total > c.tier * 20",
    );
}

#[test]
fn unqualified_conjuncts_not_pushed() {
    let mut c = setup();
    // 裸列名（total 无前缀）→ v1 保守不下推（留 join 后）——结果不变
    diff(
        &mut c,
        "SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id \
         WHERE total > 100 AND c.region = 'EU'",
    );
}

#[test]
fn single_table_queries_unaffected() {
    let mut c = setup();
    diff(&mut c, "SELECT id FROM orders WHERE total > 60");
    diff(&mut c, "SELECT count(*) FROM orders WHERE cid = 1");
    // 聚合 + join
    diff(
        &mut c,
        "SELECT c.region, count(*), sum(o.total) FROM orders o \
         JOIN customers c ON o.cid = c.id WHERE o.total > 55 GROUP BY c.region",
    );
}

#[test]
fn three_way_join_chain() {
    let mut c = setup();
    c.execute("CREATE TABLE items (id BIGINT PRIMARY KEY, oid BIGINT, sku TEXT)")
        .unwrap();
    c.execute("INSERT INTO items VALUES (1, 1, 'x'),(2, 2, 'y'),(3, 4, 'z')")
        .unwrap();
    diff(
        &mut c,
        "SELECT i.sku FROM orders o JOIN customers c ON o.cid = c.id \
         JOIN items i ON i.oid = o.id \
         WHERE c.region <> 'US' AND o.total > 60 AND i.sku <> 'y'",
    );
}

// ---------- EXPLAIN 注记（spec 12 §1 合同 4：规则可见） ----------

#[test]
fn explain_shows_pushdown_annotation() {
    let mut c = setup();
    let r = c
        .query(
            "EXPLAIN SELECT o.id FROM orders o JOIN customers c ON o.cid = c.id \
             WHERE o.total > 60 AND c.region = 'EU'",
        )
        .unwrap();
    let text: Vec<String> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            SqlValue::Utf8(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let joined = text.join("\n");
    assert!(
        joined.contains("optimizer: pushdown 2 conjunct(s)"),
        "注记行：{joined}"
    );
    assert!(joined.contains("o(1)"), "orders 因子计数：{joined}");
    assert!(joined.contains("c(1)"), "customers 因子计数：{joined}");
}

#[test]
fn explain_no_annotation_without_join() {
    let mut c = setup();
    let r = c
        .query("EXPLAIN SELECT id FROM orders WHERE total > 60")
        .unwrap();
    let joined = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            SqlValue::Utf8(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!joined.contains("optimizer: pushdown"), "{joined}");
}

// ---------- 账本 #28 回归：join 后限定名跨侧同名列错读 ----------

#[test]
fn ledger28_qualified_name_reads_correct_side() {
    let mut c = setup();
    // orders.id 与 customers.id 同名：o.id 必须读 orders 侧
    let r = c
        .query("SELECT o.id, c.id FROM orders o JOIN customers c ON o.cid = c.id \
                WHERE o.total > 60 AND c.region = 'EU' ORDER BY o.id")
        .unwrap();
    let pairs: Vec<(i64, i64)> = r
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (SqlValue::Int64(a), SqlValue::Int64(b)) => (*a, *b),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(pairs, vec![(1, 1), (2, 1)], "o.id=orders.id / c.id=customers.id");
    // LEFT JOIN + NULL 延展行的限定读取
    let r = c
        .query("SELECT o.id, c.id FROM orders o LEFT JOIN customers c ON o.cid = c.id \
                WHERE o.id = 6")
        .unwrap();
    assert!(
        matches!(&r.rows[0][1], SqlValue::Null),
        "无匹配行 c.id 应为 NULL：{:?}",
        r.rows
    );
}

#[test]
fn ledger28_where_qualified_reads_correct_side() {
    let mut c = setup();
    // WHERE 里的限定名（经谓词编译器路径）
    let r = c
        .query("SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id \
                WHERE c.id = 1 AND o.total >= 100")
        .unwrap();
    assert_eq!(r.rows[0][0], SqlValue::Int64(2), "c.id=1 且 total>=100：订单 1/2");
}
