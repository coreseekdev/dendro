//! 前置2 自测：结果等价正式定义（tests/common）的行为合同。
//! 多重集模式、行序模式、Float 1-ULP 容差、NaN 位等。

mod common;

use common::{assert_rows_equiv, value_equiv};
use dendro_core::embed::Connection;
use dendro_core::types::SqlValue;

#[test]
fn multiset_mode_ignores_row_order() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v DOUBLE)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (1, 1.0),(2, 2.0),(3, 3.0)")
        .unwrap();
    let a = c.query("SELECT id, v FROM t").unwrap();
    let b = c.query("SELECT id, v FROM t ORDER BY id DESC").unwrap();
    assert_rows_equiv("multiset", &a.columns, &a.rows, &b.columns, &b.rows, false);
}

#[test]
#[should_panic(expected = "列 0 不等价")]
fn ordered_mode_detects_order_difference() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    let a = c.query("SELECT id FROM t").unwrap();
    let b = c.query("SELECT id FROM t ORDER BY id DESC").unwrap();
    assert_rows_equiv("ordered", &a.columns, &a.rows, &b.columns, &b.rows, true);
}

#[test]
fn float_ulp_and_nan_semantics() {
    let x = 0.1f64 + 0.2; // 0.30000000000000004
    let y = 0.30000000000000004f64; // 位等
    let z = 0.3f64; // 差 1 ULP
    let w = 0.5f64; // 差远
    let f = |v: f64| SqlValue::Float64(v);
    assert!(value_equiv(&f(x), &f(y), 1));
    assert!(value_equiv(&f(x), &f(z), 1), "1 ULP 内必须等价");
    assert!(!value_equiv(&f(x), &f(w), 1));
    assert!(!value_equiv(&f(x), &f(z), 0), "位精确模式下 1 ULP 不等价");
    assert!(
        value_equiv(&f(f64::NAN), &f(f64::NAN), 0),
        "NaN 位等（两侧皆 NaN）"
    );
    assert!(!value_equiv(&f(f64::NAN), &f(0.0), 0));
}

#[test]
#[should_panic(expected = "行数不同")]
fn row_count_mismatch_panics() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    let a = c.query("SELECT id FROM t").unwrap();
    let b = c.query("SELECT id FROM t WHERE id = 1").unwrap();
    assert_rows_equiv("count", &a.columns, &a.rows, &b.columns, &b.rows, false);
}
