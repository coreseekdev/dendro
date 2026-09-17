//! EXPLAIN 输出（L4 统一）：全部 SELECT 形态同出 dendro.ir v1 计划方言
//!（单表 Seq Scan/dispatch/标量块形态已退役）；可 parse round-trip。

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
    // L4 统一：dendro.ir v1 计划方言（scan/filter/project 节点 + round-trip）
    let joined = text.join("\n");
    assert!(joined.contains("dendro.ir v1"), "版本头：{joined}");
    assert!(
        joined.contains("table \"t\""),
        "扫描形状（表节点）：{joined}"
    );
    assert!(joined.contains("filter"), "过滤节点：{joined}");
    let ir_start = joined.find("dendro.ir v1").unwrap();
    let ir_end = joined.rfind('}').unwrap() + 1;
    let plan = dendro_core::ir::plan::parse_plan(&joined[ir_start..ir_end]);
    assert!(plan.is_some(), "EXPLAIN 计划块必须可 parse：{joined}");
    assert!(dendro_core::ir::plan::verify_plan(&plan.unwrap()));
}

#[test]
fn explain_fallback_shapes_stay_honest() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE a (id BIGINT PRIMARY KEY)").unwrap();
    c.execute("CREATE TABLE b (id BIGINT PRIMARY KEY)").unwrap();
    // 逗号多 FROM：v1 计划不覆盖（reject_multi_from 同口径）——诚实回落
    let r = c
        .query("EXPLAIN SELECT a.id FROM a, b WHERE a.id = b.id")
        .unwrap();
    let joined = format!("{:?}", r.rows);
    assert!(
        joined.contains("not supported") || joined.contains("Query (plan build"),
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
    // 无 WHERE 单表：纯 scan + project 计划块（可 parse）
    let r2 = c.query("EXPLAIN SELECT id FROM a").unwrap();
    let t2: Vec<String> = r2
        .rows
        .iter()
        .map(|row| match &row[0] {
            dendro_core::types::SqlValue::Utf8(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let j2 = t2.join("\n");
    assert!(j2.contains("table \"a\""), "扫描节点：{j2}");
    let p2 = dendro_core::ir::plan::parse_plan(&j2[j2.find("dendro.ir v1").unwrap()..]);
    assert!(p2.is_some(), "单表计划块必须可 parse：{j2}");
}

// ---------- EXPLAIN ANALYZE（O-2c+：逐节点实际行数 + 子树墙钟） ----------

#[test]
fn explain_analyze_node_metrics() {
    let mut c = Connection::memory().unwrap();
    c.execute("CREATE TABLE o (id BIGINT PRIMARY KEY, total BIGINT)")
        .unwrap();
    c.execute("CREATE TABLE c (id BIGINT PRIMARY KEY, region TEXT)")
        .unwrap();
    c.execute("INSERT INTO o VALUES (1, 100), (2, 250), (3, 50)")
        .unwrap();
    c.execute("INSERT INTO c VALUES (1, 'EU'), (2, 'US')")
        .unwrap();
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
    assert!(
        joined.contains("optimizer: pushdown 1 conjunct(s)"),
        "{joined}"
    );
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
    c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
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
