//! v2c-1 差分测试（ADR-5）：强制派发——同一 SQL 走不同数据源，结果集
//! 必须等价（tests/common §1.1 结果等价正式定义：多重集模式）。
//! force_source 仅调试/测试构建可 SET（本测试在 debug 下运行）。

mod common;

use common::{assert_rows_equiv, canonicalize, Canonical};
use dendro_core::types::{Output, SqlValue};
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn rows_of(outs: &[Output]) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match canonicalize(outs) {
        Canonical::Rows { cols, rows, .. } => (cols, rows),
        Canonical::Commands(c) => panic!("期望 Rows 输出：{c:?}"),
    }
}

fn fixture() -> Arc<Database> {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    // 列存引擎必须显式接线（core 不静态依赖 columnar；不设置则 AP 路径
    // 静默不可用——ap_tp_differential 曾因此空转，见该文件修复注记）
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    // 12k 行（≥1 万触发列存物化），分批插入
    for chunk in 0..12 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {id})",)
            })
            .collect();
        s.exec(&format!("INSERT INTO t VALUES {}", vals.join(",")))
            .unwrap();
    }
    db.checkpoint_branch("main").unwrap(); // 物化列存段
    s.exec("INSERT INTO t VALUES (999999, 7)").unwrap(); // overlay 尾巴
    s.exec("DELETE FROM t WHERE id = 5").unwrap(); // overlay 删除
    db
}

fn run(db: &Arc<Database>, force: &str, sql: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    let mut s = db.new_session();
    s.exec(&format!("SET dendro.force_source = '{force}'"))
        .unwrap();
    let outs = s.exec(sql).unwrap();
    rows_of(&outs)
}

fn diff(db: &Arc<Database>, sql: &str, paths: &[&str]) {
    let mut ref_: Option<(Vec<String>, Vec<Vec<SqlValue>>)> = None;
    for p in paths {
        let r = run(db, p, sql);
        match &ref_ {
            None => ref_ = Some(r),
            Some(base) => {
                assert_rows_equiv(
                    &format!("`{sql}` force={p} vs 基线"),
                    &base.0,
                    &base.1,
                    &r.0,
                    &r.1,
                    false,
                );
            }
        }
    }
}

#[test]
fn point_query_all_paths_equivalent() {
    let db = fixture();
    diff(
        &db,
        "SELECT v FROM t WHERE id = 500",
        &["auto", "delta", "fallback"],
    );
}

#[test]
fn range_query_main_vs_fallback() {
    let db = fixture();
    diff(
        &db,
        "SELECT id, v FROM t WHERE id > 11995",
        &["auto", "main", "fallback"],
    );
}

#[test]
fn full_scan_main_vs_fallback_with_overlay() {
    let db = fixture();
    // 全表（含 overlay 尾巴 999999 与删除 5）——Main+Delta 归并 vs 行路径
    diff(&db, "SELECT id, v FROM t", &["auto", "main", "fallback"]);
    // 覆盖两处 overlay 特征值
    let (_, rows) = run(&db, "main", "SELECT v FROM t WHERE id = 999999");
    assert_eq!(rows.len(), 1, "overlay 插入行必须可见：{rows:?}");
    let (_, rows2) = run(&db, "main", "SELECT v FROM t WHERE id = 5");
    assert_eq!(rows2.len(), 0, "overlay 删除行必须不可见");
}

#[test]
fn forced_structural_impossibilities_error() {
    let db = fixture();
    // 无版本子句强制 prolly → 报错（不静默回落）
    let mut s = db.new_session();
    s.exec("SET dendro.force_source = 'prolly'").unwrap();
    assert!(s.exec("SELECT v FROM t WHERE id = 1").is_err());
    // 无段表强制 main → 报错
    let mut s2 = db.new_session();
    s2.exec("CREATE TABLE small (id BIGINT PRIMARY KEY)")
        .unwrap();
    s2.exec("SET dendro.force_source = 'main'").unwrap();
    assert!(s2.exec("SELECT * FROM small").is_err());
    // 无 pk 谓词强制 delta → 报错
    let mut s3 = db.new_session();
    s3.exec("SET dendro.force_source = 'delta'").unwrap();
    assert!(s3.exec("SELECT * FROM t WHERE v > 5").is_err());
    // auto 恢复
    let mut s4 = db.new_session();
    s4.exec("SET dendro.force_source = 'auto'").unwrap();
    assert!(s4.exec("SELECT v FROM t WHERE v > 5").is_ok());
}

#[test]
fn explain_shows_dispatch() {
    let db = fixture();
    let mut s = db.new_session();
    let outs = s.exec("EXPLAIN SELECT v FROM t WHERE id = 500").unwrap();
    let (cols, rows) = rows_of(&outs);
    assert_eq!(cols, vec!["QUERY PLAN"]);
    let joined = format!("{rows:?}");
    assert!(joined.contains("Seq Scan on t"), "{joined}");
}
