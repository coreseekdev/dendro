//! 账本 #20：单列 pk 表 `NOT IN` 曾被点查下推资格判定放行（取键空集）
//! → 静默返回 0 行。修复后回落通用谓词路径，语义正确。

use dendro_core::embed::Connection;

#[test]
fn not_in_returns_correct_rows() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    for i in 1..=5 {
        c.execute(&format!("INSERT INTO t VALUES ({i}, {i})"))
            .unwrap();
    }
    let r = c
        .query("SELECT id FROM t WHERE id NOT IN (1, 2) ORDER BY id")
        .unwrap();
    let ids: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            dendro_core::types::SqlValue::Int64(v) => *v,
            other => panic!("非 Int64：{other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![3, 4, 5], "NOT IN 必须排除命中值而非返回空集");
}

#[test]
fn in_still_uses_point_path() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    for i in 1..=5 {
        c.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let r = c
        .query("SELECT id FROM t WHERE id IN (2, 4) ORDER BY id")
        .unwrap();
    assert_eq!(r.row_count(), 2, "IN（非 negated）点查路径不受影响");
}

#[test]
fn not_in_with_null_semantics() {
    // 附录 A A.4：InList NULL 三值——NOT IN 含 NULL 时无行匹配（NULL 比较
    // 未定）。此处只固化"不静默空集于无 NULL 情形"；含 NULL 行为另据
    // 附录 A Q 表逐条对拍（v2b 编译期单测承接）。
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    for i in 1..=3 {
        c.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let r = c.query("SELECT id FROM t WHERE id NOT IN (9)").unwrap();
    assert_eq!(r.row_count(), 3);
}
