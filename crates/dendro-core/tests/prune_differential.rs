//! 优化器 O-3 差分测试：投影裁剪（AP 列存段跳列解码）。
//! `SET dendro.optimize` on/off 两路径结果必须等价——裁剪只在确定
//! 无损时发生（fail-open：通配/未知名/不可解析 → 不裁剪）。
//! 关键陷阱形态：WHERE 引用未投影列、ORDER BY 未投影列、CASE 列、
//! 通配误裁（裁掉列变 NULL 即刻暴露）。

mod common;

use common::{assert_rows_equiv, canonicalize, Canonical};
use dendro_core::types::{Output, SqlValue};
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn fixture() -> Arc<Database> {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    // 5 列宽表：裁剪空间大（id pk, a, b, c, note）
    s.exec("CREATE TABLE w (id BIGINT PRIMARY KEY, a INT, b BIGINT, c DOUBLE, note TEXT)")
        .unwrap();
    for chunk in 0..12 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {id} % 7, {id} * 2, {id}.5, 'n{id}')")
            })
            .collect();
        s.exec(&format!("INSERT INTO w VALUES {}", vals.join(",")))
            .unwrap();
    }
    db.checkpoint_branch("main").unwrap(); // 物化列存段（12k 行 ≥ 阈值）
    s.exec("INSERT INTO w VALUES (999999, 1, 2, 3.5, 'tail')")
        .unwrap(); // overlay 尾巴
    db
}

fn rows_of(outs: &[Output]) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match canonicalize(outs) {
        Canonical::Rows { cols, rows, .. } => (cols, rows),
        Canonical::Commands(c) => panic!("期望 Rows：{c:?}"),
    }
}

fn run(db: &Arc<Database>, opt: &str, sql: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    let mut s = db.new_session();
    s.exec(&format!("SET dendro.optimize = '{opt}'")).unwrap();
    let outs = s.exec(sql).unwrap();
    rows_of(&outs)
}

fn diff(db: &Arc<Database>, sql: &str) {
    let on = run(db, "on", sql);
    let off = run(db, "off", sql);
    assert_rows_equiv(
        &format!("`{sql}` prune on vs off"),
        &on.0,
        &on.1,
        &off.0,
        &off.1,
        false,
    );
}

#[test]
fn prune_projection_subset() {
    let db = fixture();
    // 纯子集投影（2/5 列 + pk）
    diff(&db, "SELECT id, a FROM w WHERE id <= 100");
    diff(&db, "SELECT note FROM w WHERE a = 3 AND id < 500");
    diff(
        &db,
        "SELECT b, c FROM w WHERE b > 100 ORDER BY c DESC LIMIT 20",
    );
}

#[test]
fn prune_where_references_unprojected_column() {
    let db = fixture();
    // 陷阱：WHERE 引用 b/c 但投影只有 a——b/c 必须进位图（漏裁 NULL 判假）
    diff(
        &db,
        "SELECT a FROM w WHERE b = 4 AND c < 100.0 AND id <= 2000",
    );
}

#[test]
fn prune_order_by_unprojected_column() {
    let db = fixture();
    // ORDER BY 未投影列（has_input 回退读输入行列）
    diff(
        &db,
        "SELECT id FROM w WHERE id <= 300 ORDER BY b DESC, id LIMIT 10",
    );
}

#[test]
fn prune_case_and_group_having() {
    let db = fixture();
    // CASE 引用列（expr_idents 的 Case 臂——原缺失会静默错值）
    diff(
        &db,
        "SELECT CASE WHEN a > 3 THEN 'hi' ELSE note END FROM w WHERE id <= 50",
    );
    // GROUP BY + HAVING（组键/聚合参数列进位图）
    diff(
        &db,
        "SELECT a, count(*), max(b) FROM w GROUP BY a HAVING sum(b) > 100 ORDER BY a",
    );
}

#[test]
fn prune_like_family_references() {
    // q21 教训（ClickBench 10M 'URL does not exist'）：列只出现在
    // LIKE 谓词时 expr_idents 曾漏收 → 掩码丢列。差分 on/off 都会对
    // 比行集，另钉非空结果（null 填充期此形态是静默 0 行）
    diff(&db_like(), "SELECT count(*) FROM wl WHERE u LIKE '%zz%'");
    diff(
        &db_like(),
        "SELECT id FROM wl WHERE u NOT LIKE 'plain%' LIMIT 3",
    );
    //（ILIKE 运行时未实现——其掩码收集覆盖在 ident_completeness）
    // LIKE 列 + 未投影组合（掩码 = {pk, u}，投影 note 未引用则不可达——
    // 用 a 列组合确保两列都在掩码）
    diff(
        &db_like(),
        "SELECT a FROM wl WHERE u LIKE '%zz%' AND a > 0 ORDER BY id LIMIT 5",
    );
}

/// LIKE 夹具：note/zz 高区分文本（u=plain_N / zz_N 交替）
fn db_like() -> Arc<Database> {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE wl (id BIGINT PRIMARY KEY, a INT, u TEXT)")
        .unwrap();
    for chunk in 0..6 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!(
                    "({id}, {}, '{}')",
                    id % 7,
                    if id % 4 == 0 {
                        format!("zz_{id}")
                    } else {
                        format!("plain_{id}")
                    }
                )
            })
            .collect();
        s.exec(&format!("INSERT INTO wl VALUES {}", vals.join(",")))
            .unwrap();
    }
    db.checkpoint_branch("main").unwrap();
    // 非空钉子：LIKE 命中恰 1500 行（6000/4）——静默错值回归即刻红
    let (_, rows) = run(&db, "on", "SELECT count(*) FROM wl WHERE u LIKE '%zz%'");
    assert!(
        matches!(&rows[0][0], SqlValue::Int64(1500)),
        "LIKE 命中应为 1500（静默 0 行回归）：{:?}",
        rows[0]
    );
    db
}

#[test]
fn wildcard_not_pruned() {
    let db = fixture();
    // 通配 → 不裁剪（若误裁，SELECT * 的裁掉列变 NULL——差分即刻红）
    diff(&db, "SELECT * FROM w WHERE id <= 30");
    let (_, rows) = run(&db, "on", "SELECT * FROM w WHERE id = 42");
    assert_eq!(rows[0].len(), 5, "通配必须全列");
    assert!(
        matches!(&rows[0][4], SqlValue::Utf8(_)),
        "note 列不得被裁成 NULL：{:?}",
        rows[0]
    );
}

#[test]
fn prune_overlay_tail_and_count() {
    let db = fixture();
    // overlay 尾巴行（行路径解码，不经过掩码）与段行混合
    diff(&db, "SELECT a, note FROM w WHERE id = 999999 OR id = 5");
    diff(&db, "SELECT count(*), sum(b) FROM w WHERE a IN (1, 2, 3)");
}

#[test]
fn row_path_unaffected() {
    // 无列存的小表（行路径）：裁剪仅 AP 面——行路径行为恒等
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT, b INT)")
        .unwrap();
    s.exec("INSERT INTO t VALUES (1, 10, 20), (2, 30, 40)")
        .unwrap();
    diff(&db, "SELECT a FROM t WHERE b > 25");
    diff(&db, "SELECT * FROM t");
}

#[test]
fn prune_ap_path_forced_main() {
    // 强制 MainPlusDelta：掩码的 CBF 跳列解码必然执行（不依赖派发浮动）
    let db = fixture();
    for sql in [
        "SELECT id, a FROM w WHERE b > 23000",
        "SELECT note FROM w WHERE c > 10000.0 ORDER BY id LIMIT 5",
        "SELECT a, count(*) FROM w WHERE b < 100 GROUP BY a ORDER BY a",
    ] {
        let mut on = db.new_session();
        on.exec("SET dendro.optimize = 'on'").unwrap();
        on.exec("SET dendro.force_source = 'main'").unwrap();
        let r_on = rows_of(&on.exec(sql).unwrap());
        let mut off = db.new_session();
        off.exec("SET dendro.optimize = 'off'").unwrap();
        off.exec("SET dendro.force_source = 'main'").unwrap();
        let r_off = rows_of(&off.exec(sql).unwrap());
        assert_rows_equiv(
            &format!("forced-main `{sql}` prune on vs off"),
            &r_on.0,
            &r_on.1,
            &r_off.0,
            &r_off.1,
            false,
        );
    }
}
