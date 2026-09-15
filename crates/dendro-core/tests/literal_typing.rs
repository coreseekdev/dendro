//! 账本 #26：字符串字面量类型语义（PG unknown-literal）
//! - 表达式语境：引号串恒为 text（'1' 不再数值化）
//! - INSERT 语境：按目标列采纳（'1'→BIGINT 列=1；1→TEXT 列='1'）

use dendro_core::embed::Connection;
use dendro_core::types::SqlValue;

#[test]
fn text_column_preserves_numeric_looking_strings() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, k TEXT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (1, 'x'), (2, '1'), (3, '1.5')")
        .unwrap();
    let r = c.query("SELECT id, k FROM t ORDER BY id").unwrap();
    assert_eq!(r.rows[0][1], SqlValue::Utf8("x".into()));
    assert_eq!(
        r.rows[1][1],
        SqlValue::Utf8("1".into()),
        "'1' 在 TEXT 列必须是文本（曾 Int64/Null）"
    );
    assert_eq!(r.rows[2][1], SqlValue::Utf8("1.5".into()), "'1.5' 同");
    // 文本比较找回：数字串与数字不跨型相等（ cmp_values 语义不动，此处只固化存储类型）
}

#[test]
fn insert_adopts_column_type() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE n (id BIGINT PRIMARY KEY, v BIGINT, d DOUBLE, b BOOLEAN)")
        .unwrap();
    c.execute("INSERT INTO n VALUES (1, '42', '1.5', 'true')")
        .unwrap();
    let r = c.query("SELECT v, d, b FROM n").unwrap();
    assert_eq!(r.rows[0][0], SqlValue::Int64(42), "'42'→BIGINT 列按列采纳");
    assert_eq!(r.rows[0][1], SqlValue::Float64(1.5));
    assert_eq!(r.rows[0][2], SqlValue::Bool(true));
    // 反向：数值 → TEXT 列文本化
    c.execute("CREATE TABLE s (id BIGINT PRIMARY KEY, k TEXT)")
        .unwrap();
    c.execute("INSERT INTO s VALUES (1, 7)").unwrap();
    let r2 = c.query("SELECT k FROM s").unwrap();
    assert_eq!(r2.rows[0][0], SqlValue::Utf8("7".into()));
}

#[test]
fn insert_bad_literal_errors() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE n (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    let e = c.execute("INSERT INTO n VALUES (1, 'xyz')").unwrap_err();
    assert!(format!("{e}").contains("invalid input syntax"), "{e}");
}

#[test]
fn expression_context_quoted_is_text() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    // SELECT '1' 是 text；与文本拼接正常
    let r = c.query("SELECT '1' || 'x' AS s FROM t").unwrap();
    assert_eq!(r.rows[0][0], SqlValue::Utf8("1x".into()));
}
