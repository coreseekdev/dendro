//! S-4：UNION / UNION ALL 基础支持（v1：两侧子查询独立求值 → 拼接；
//! UNION 去重保首见序；UNION ALL 保留重复）

use dendro_core::embed::Connection;

fn setup() -> Connection {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE a (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    c.execute("CREATE TABLE b (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    c.execute("INSERT INTO a VALUES (1, 'x'), (2, 'y')")
        .unwrap();
    c.execute("INSERT INTO b VALUES (3, 'z'), (1, 'x')")
        .unwrap();
    c
}

#[test]
fn union_all_preserves_duplicates() {
    let mut c = setup();
    let r = c
        .query("SELECT v FROM a UNION ALL SELECT v FROM b ORDER BY v")
        .unwrap();
    assert_eq!(r.row_count(), 4, "UNION ALL 保留重复：{:?}", r.rows);
    let vs: Vec<String> = r
        .rows
        .iter()
        .filter_map(|r| match &r[0] {
            dendro_core::types::SqlValue::Utf8(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(vs, vec!["x", "x", "y", "z"]);
}

#[test]
fn union_deduplicates() {
    let mut c = setup();
    let r = c
        .query("SELECT v FROM a UNION SELECT v FROM b ORDER BY v")
        .unwrap();
    assert_eq!(r.row_count(), 3, "UNION 去重：{:?}", r.rows);
    let vs: Vec<String> = r
        .rows
        .iter()
        .filter_map(|r| match &r[0] {
            dendro_core::types::SqlValue::Utf8(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(vs, vec!["x", "y", "z"], "重复 'x' 去掉");
}

#[test]
fn union_explicit_distinct() {
    let mut c = setup();
    let r = c
        .query("SELECT v FROM a UNION DISTINCT SELECT v FROM b ORDER BY v")
        .unwrap();
    assert_eq!(r.row_count(), 3, "UNION DISTINCT 等价 UNION");
}

#[test]
fn union_column_mismatch_errors() {
    let mut c = setup();
    let e = c
        .execute("SELECT id, v FROM a UNION SELECT v FROM b")
        .unwrap_err();
    assert!(format!("{e}").contains("mismatch"), "{e}");
}

#[test]
fn union_with_types() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE s1 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("CREATE TABLE s2 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("INSERT INTO s1 VALUES (1, 1), (2, 2)").unwrap();
    c.execute("INSERT INTO s2 VALUES (1, 2), (2, 3)").unwrap();
    let r = c
        .query("SELECT v FROM s1 UNION SELECT v FROM s2 ORDER BY v")
        .unwrap();
    let vs: Vec<i64> = r
        .rows
        .iter()
        .filter_map(|r| match r[0] {
            dendro_core::types::SqlValue::Int64(v) => Some(v),
            dendro_core::types::SqlValue::Int32(v) => Some(v as i64),
            _ => None,
        })
        .collect();
    assert_eq!(vs, vec![1, 2, 3]);
}

#[test]
fn union_nested() {
    let mut c = setup();
    // 三路 UNION（链式）
    let r = c
        .query("SELECT v FROM a UNION SELECT v FROM b UNION SELECT 'w' AS v ORDER BY v")
        .unwrap();
    assert_eq!(r.row_count(), 4, "三路：x,y,z,w");
}

#[test]
fn except_removes_right_from_left() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE s1 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("CREATE TABLE s2 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("INSERT INTO s1 VALUES (1, 1), (2, 2), (3, 3)")
        .unwrap();
    c.execute("INSERT INTO s2 VALUES (1, 2), (2, 4)").unwrap();
    let r = c
        .query("SELECT v FROM s1 EXCEPT SELECT v FROM s2 ORDER BY v")
        .unwrap();
    let vs: Vec<i64> = r
        .rows
        .iter()
        .filter_map(|r| match r[0] {
            dendro_core::types::SqlValue::Int32(v) => Some(v as i64),
            dendro_core::types::SqlValue::Int64(v) => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(vs, vec![1, 3], "EXCEPT: {vs:?}（去掉 2 和 4）");
}

#[test]
fn intersect_common_rows() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE s1 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("CREATE TABLE s2 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("INSERT INTO s1 VALUES (1, 1), (2, 2), (3, 3)")
        .unwrap();
    c.execute("INSERT INTO s2 VALUES (1, 2), (2, 4)").unwrap();
    let r = c
        .query("SELECT v FROM s1 INTERSECT SELECT v FROM s2 ORDER BY v")
        .unwrap();
    let vs: Vec<i64> = r
        .rows
        .iter()
        .filter_map(|r| match r[0] {
            dendro_core::types::SqlValue::Int32(v) => Some(v as i64),
            dendro_core::types::SqlValue::Int64(v) => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(vs, vec![2], "INTERSECT: {vs:?}（仅 2 在两侧）");
}

#[test]
fn except_all_keeps_dups() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE s1 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("CREATE TABLE s2 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("INSERT INTO s1 VALUES (1, 1), (2, 1), (3, 2)")
        .unwrap();
    c.execute("INSERT INTO s2 VALUES (1, 1)").unwrap();
    let r = c
        .query("SELECT v FROM s1 EXCEPT ALL SELECT v FROM s2 ORDER BY v")
        .unwrap();
    let vs: Vec<i64> = r
        .rows
        .iter()
        .filter_map(|r| match r[0] {
            dendro_core::types::SqlValue::Int32(v) => Some(v as i64),
            dendro_core::types::SqlValue::Int64(v) => Some(v),
            _ => None,
        })
        .collect();
    // EXCEPT ALL: 左侧 [1,1,2] 减右侧 [1]（一次）→ [1,2]
    assert_eq!(vs, vec![1, 2], "EXCEPT ALL 保留重复：{vs:?}");
}

#[test]
fn intersect_empty() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE s1 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("CREATE TABLE s2 (id BIGINT PRIMARY KEY, v BIGINT)")
        .unwrap();
    c.execute("INSERT INTO s1 VALUES (1, 1)").unwrap();
    c.execute("INSERT INTO s2 VALUES (1, 2)").unwrap();
    let r = c
        .query("SELECT v FROM s1 INTERSECT SELECT v FROM s2")
        .unwrap();
    assert_eq!(r.row_count(), 0, "无交集");
}
