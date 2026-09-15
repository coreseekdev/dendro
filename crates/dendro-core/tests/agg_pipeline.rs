//! v2c-3 差分测试：聚合执行路径（AggOp 管线 vs group_aggregate 行式）。
//! `SET dendro.force_agg`（调试面，ADR-5 同源）枚举两路径，结果必须等价
//! （common §1.1 正式定义）。另固化 v2c-3 语义合同的缺陷修复：
//! count(文本列) 计数（原 as_i64 报错）、DISTINCT 去重对 sum/avg 生效
//! （原仅 count）、混合 int/float 列 SUM 不丢整数部分。

mod common;

use common::assert_rows_equiv;
use dendro_core::embed::Connection;
use dendro_core::types::SqlValue;

fn setup() -> Connection {
    let mut c = Connection::memory().unwrap();
    c.execute(
        "CREATE TABLE g (id BIGINT PRIMARY KEY, grp TEXT, name TEXT, \
         v BIGINT, f DOUBLE)",
    )
    .unwrap();
    c.execute(
        "INSERT INTO g VALUES \
         (1,'a','foo',10,1.5),(2,'a','bar',20,2.0),(3,'a','foo',10,NULL),\
         (4,'b','baz',100,0.5),(5,'b',NULL,NULL,1.0),(6,NULL,'qux',7,7.0)",
    )
    .unwrap();
    c
}

fn run(c: &mut Connection, force: &str, sql: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    c.execute(&format!("SET dendro.force_agg = '{force}'"))
        .unwrap();
    let r = c.query(sql).unwrap();
    (r.columns.clone(), r.rows.clone())
}

fn diff(c: &mut Connection, sql: &str) {
    let row = run(c, "row", sql);
    let pipe = run(c, "pipeline", sql);
    assert_rows_equiv(
        &format!("`{sql}` pipeline vs row"),
        &row.0,
        &row.1,
        &pipe.0,
        &pipe.1,
        false,
    );
}

// ---------- 差分：资格内查询，两路径结果等价 ----------

#[test]
fn diff_group_by_text_key_all_aggs() {
    let mut c = setup();
    diff(
        &mut c,
        "SELECT grp, count(*), count(v), count(name), sum(v), avg(v), min(v), max(v), \
         min(name), max(name) FROM g GROUP BY grp",
    );
}

#[test]
fn diff_global_aggregate() {
    let mut c = setup();
    diff(&mut c, "SELECT count(*), count(v), sum(v), avg(v), min(v), max(v) FROM g");
}

#[test]
fn diff_distinct_variants() {
    let mut c = setup();
    diff(&mut c, "SELECT count(DISTINCT v), sum(DISTINCT v), avg(DISTINCT v) FROM g");
    diff(&mut c, "SELECT grp, count(DISTINCT name) FROM g GROUP BY grp");
}

#[test]
fn diff_having_and_empty_groups() {
    let mut c = setup();
    diff(&mut c, "SELECT grp, sum(v) FROM g GROUP BY grp HAVING sum(v) > 15");
    diff(&mut c, "SELECT grp FROM g GROUP BY grp"); // 纯分组无聚合
}

#[test]
fn diff_empty_input_global_agg_one_row() {
    let mut c = setup();
    c.execute("CREATE TABLE e (id BIGINT PRIMARY KEY, v BIGINT)").unwrap();
    // 空输入全局聚合：两路径都出一行（count=0, sum=NULL）
    diff(&mut c, "SELECT count(*), count(v), sum(v), avg(v), min(v) FROM e");
}

#[test]
fn diff_group_by_empty_input_no_rows() {
    let mut c = setup();
    c.execute("CREATE TABLE e (id BIGINT PRIMARY KEY, v BIGINT)").unwrap();
    diff(&mut c, "SELECT v, count(*) FROM e GROUP BY v"); // 分组 + 空输入 → 0 行
}

#[test]
fn diff_mixed_int_float_sum() {
    let mut c = setup();
    // f 列含 NULL；[1.5,2.0,0.5,1.0,7.0] sum=12.0（float 路径）；
    // 混合验证：v 列 sum=147（纯 int）
    diff(&mut c, "SELECT sum(f), avg(f) FROM g");
    diff(&mut c, "SELECT sum(v), sum(f) FROM g");
}

// ---------- 语义合同固化（v2c-3 缺陷修复） ----------

#[test]
fn count_over_text_column_counts_not_errors() {
    let mut c = setup();
    // 原：Accum::push 对所有函数 as_i64 → count(文本) 报 42804
    let r = run(&mut c, "row", "SELECT count(name) FROM g");
    assert_eq!(r.1, vec![vec![SqlValue::Int64(5)]]);
    let r = run(&mut c, "pipeline", "SELECT count(name) FROM g");
    assert_eq!(r.1, vec![vec![SqlValue::Int64(5)]]);
}

#[test]
fn sum_distinct_dedups_in_both_paths() {
    let mut c = setup();
    // v 列非空值 [10,20,10,100,7]：DISTINCT 后 {10,20,100,7} sum=137
    for force in ["row", "pipeline"] {
        let r = run(&mut c, force, "SELECT sum(DISTINCT v) FROM g");
        assert_eq!(
            r.1,
            vec![vec![SqlValue::Int64(137)]],
            "force={force}"
        );
    }
}

#[test]
fn sum_over_text_errors_in_both_paths() {
    let mut c = setup();
    for force in ["row", "pipeline"] {
        c.execute(&format!("SET dendro.force_agg = '{force}'")).unwrap();
        let e = c.query("SELECT sum(name) FROM g").err().unwrap();
        assert!(
            e.to_string().contains("expected number"),
            "force={force}: {e}"
        );
    }
}

// ---------- 路径选择：资格边界 ----------

#[test]
fn expression_group_key_falls_back_and_forces_error() {
    let mut c = setup();
    // 表达式组键（upper(grp)）→ 资格外，auto 走行式（静默、结果正确）
    let r = c
        .query("SELECT upper(grp), count(*) FROM g GROUP BY upper(grp)")
        .unwrap();
    assert_eq!(r.row_count(), 3); // A / B / NULL（upper(NULL)=NULL 一组）
    // 强制 pipeline + 资格外 → 报错（静默回落会让差分失义）
    c.execute("SET dendro.force_agg = 'pipeline'").unwrap();
    let e = c
        .query("SELECT upper(grp), count(*) FROM g GROUP BY upper(grp)")
        .err()
        .unwrap();
    assert!(e.to_string().contains("not plain column refs"), "{e}");
}

#[test]
fn expression_agg_arg_falls_back() {
    let mut c = setup();
    // 聚合参数是表达式（v+1）→ 资格外，行式求值
    let r = c.query("SELECT grp, sum(v + 1) FROM g GROUP BY grp").unwrap();
    assert_eq!(r.row_count(), 3);
    c.execute("SET dendro.force_agg = 'pipeline'").unwrap();
    let e = c.query("SELECT grp, sum(v + 1) FROM g GROUP BY grp").err().unwrap();
    assert!(e.to_string().contains("not plain column refs"), "{e}");
}

#[test]
fn nested_paren_column_group_is_eligible() {
    let mut c = setup();
    // Nested(Identifier) 仍是纯列引用 → 管线可走（不报错即过）
    c.execute("SET dendro.force_agg = 'pipeline'").unwrap();
    let r = c.query("SELECT (grp), count(*) FROM g GROUP BY (grp)").unwrap();
    assert_eq!(r.row_count(), 3);
}
