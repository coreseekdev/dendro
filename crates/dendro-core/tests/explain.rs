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
    // O-2b：join 形态的计划块必须可 parse 回来（round-trip）
    let r2 = c
        .query("EXPLAIN SELECT a.id FROM a JOIN b ON a.id = b.id WHERE a.id = 1")
        .unwrap();
    let lines: Vec<String> = r2
        .rows
        .iter()
        .map(|row| match &row[0] {
            dendro_core::types::SqlValue::Utf8(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let ir = lines.join("\n");
    let start = ir.find("dendro.ir v1").expect("计划块：{ir}");
    let block = &ir[start..];
    let parsed = dendro_core::ir::plan::parse_plan(block);
    assert!(parsed.is_some(), "EXPLAIN 计划块必须可 parse：{block}");
    assert!(dendro_core::ir::plan::verify_plan(&parsed.unwrap()));
    // 无 WHERE：仅扫描行
    let r2 = c.query("EXPLAIN SELECT id FROM a").unwrap();
    assert_eq!(r2.rows.len(), 1);
    assert!(format!("{:?}", r2.rows).contains("Seq Scan on a"));
}

// ---------- EXPLAIN ANALYZE（O-2c+：逐节点实际行数 + 子树墙钟） ----------

#[test]
fn explain_analyze_node_metrics() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE o (id BIGINT PRIMARY KEY, total BIGINT)").unwrap();
    c.execute("CREATE TABLE c (id BIGINT PRIMARY KEY, region TEXT)").unwrap();
    c.execute("INSERT INTO o VALUES (1, 100), (2, 250), (3, 50)").unwrap();
    c.execute("INSERT INTO c VALUES (1, 'EU'), (2, 'US')").unwrap();
    let r = c
        .query("EXPLAIN ANALYZE SELECT o.id FROM o JOIN c ON o.id = c.id WHERE o.total > 60")
        .unwrap();
    let text: Vec<String> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            dendro_core::types::SqlValue::Utf8(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let joined = text.join("\n");
    // 下推注记 + 逐节点行数（精确）+ 树缩进
    assert!(joined.contains("optimizer: pushdown 1 conjunct(s)"), "{joined}");
    assert!(joined.contains("actual: scan o rows=3"), "{joined}");
    assert!(joined.contains("actual: scan c rows=2"), "{joined}");
    // filter 下推在 scan 上（缩进 1 层）+ join 行数精确（total>60: id 1,2）
    assert!(joined.contains("  ! actual: filter rows=2"), "{joined}");
    assert!(joined.contains("actual: join inner rows=2"), "{joined}");
    assert!(joined.contains("actual: project rows=2"), "{joined}");
    // 时间量纲存在（非负微秒——机器相关不固化数值）
    assert!(joined.contains("time="), "{joined}");
}

#[test]
fn explain_analyze_shapes_and_limits() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    // LIMIT + 排序（Sort/Limit 节点行数）
    let r = c
        .query("EXPLAIN ANALYZE SELECT id FROM t ORDER BY v DESC LIMIT 2")
        .unwrap();
    let joined = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            dendro_core::types::SqlValue::Utf8(s) => s.clone(),
            _ => panic!(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("actual: sort rows=2"), "{joined}");
    assert!(joined.contains("actual: limit rows=2"), "{joined}");
    // 聚合形态
    let r = c.query("EXPLAIN ANALYZE SELECT count(*) FROM t").unwrap();
    let joined = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            dendro_core::types::SqlValue::Utf8(s) => s.clone(),
            _ => panic!(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("actual: aggregate rows=1"), "{joined}");
    // 非 SELECT 诚实拒绝
    let e = c
        .execute("EXPLAIN ANALYZE INSERT INTO t VALUES (9, 9)")
        .unwrap_err();
    assert!(e.message.contains("SELECT"), "{e}");
}
