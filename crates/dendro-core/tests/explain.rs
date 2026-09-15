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
    // v2c-1 后行序：Scan / dispatch / Filter / 步列表——按下标断言过时
    assert!(text.iter().any(|l| l.starts_with("dispatch: ")), "{text:?}");
    assert!(text.iter().any(|l| l.starts_with("Filter:")), "{text:?}");
    // dendro.ir v1 标量块（spec 09）：版本头 + SSA 步 + round-trip
    let joined = text.join("\n");
    assert!(joined.contains("dendro.ir v1"), "版本头：{joined}");
    assert!(joined.contains("%r0 = col 1"), "col 步（SSA 形式）：{joined}");
    assert!(joined.contains("cols = [\"id\", \"v\"]"), "列名侧表：{joined}");
    let ir_start = joined.find("dendro.ir v1").unwrap();
    // 尾部 } 收口（嵌在多行单元格里）
    let ir_end = joined.rfind('}').unwrap() + 1;
    let prog = dendro_core::ir::text::parse_scalar(&joined[ir_start..ir_end]);
    assert!(prog.is_some(), "EXPLAIN 的 IR 段必须可再解析：{joined}");
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
