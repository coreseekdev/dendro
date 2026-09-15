//! v2b B4：EXPLAIN 真实输出——扫描形状 + WHERE 步列表反汇编（可 round-trip）。

use dendro_core::embed::Connection;

#[test]
fn explain_outputs_scan_and_steps() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (1, 5)").unwrap();
    let r = c
        .query("EXPLAIN SELECT id FROM t WHERE v > 3 AND id = 1")
        .unwrap();
    let text: Vec<String> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            dendro_core::types::SqlValue::Utf8(s) => s.clone(),
            other => panic!("非文本行：{other:?}"),
        })
        .collect();
    assert_eq!(text[0], "Seq Scan on t", "首行扫描形状：{text:?}");
    assert!(text[1].starts_with("Filter:"), "{text:?}");
    // 步列表段：Col 步带列名、比较步存在
    let joined = text.join("\n");
    assert!(joined.contains("Col #1:v"), "列名标注：{joined}");
    assert!(joined.contains("Cmp["), "比较步：{joined}");
    // 反汇编段可再解析（round-trip）：从 Filter: 下一行起
    let steps_start = joined.find("cols ").unwrap();
    let prog = dendro_core::sql::scalar::reparse(&joined[steps_start..]);
    assert!(prog.is_some(), "EXPLAIN 步列表段必须可再解析：{joined}");
}

#[test]
fn explain_fallback_shapes_stay_honest() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE a (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("CREATE TABLE b (id BIGINT PRIMARY KEY)").unwrap();
    // join 形状：v1 无派发器——诚实标注 pending 而非假装有计划
    let r = c
        .query("EXPLAIN SELECT a.id FROM a, b WHERE a.id = b.id")
        .unwrap();
    let joined = format!("{:?}", r.rows);
    assert!(
        joined.contains("pending") || joined.contains("Seq Scan"),
        "未覆盖形状必须诚实：{joined}"
    );
    // 无 WHERE：仅扫描行
    let r2 = c.query("EXPLAIN SELECT id FROM a").unwrap();
    assert_eq!(r2.rows.len(), 1);
    assert!(format!("{:?}", r2.rows).contains("Seq Scan on a"));
}
