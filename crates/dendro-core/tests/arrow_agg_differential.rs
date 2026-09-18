//! P0 Arrow 全局聚合捷径差分测试：`SELECT agg(...) FROM t`（无 WHERE/
//! GROUP BY/JOIN）在列存段上的捷径必须与行式路径结果一致——**尤其是
//! 存在未物化增量时**（overlay 尾巴/墓碑/显式事务写/col_deletes），
//! 捷径必须回落而非漏算段外行。对照方式：SET dendro.optimize=off 关
//! 捷径（走行式三路归并）比对。

mod common;

use common::{canonicalize, Canonical};
use dendro_core::types::Output;
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn fixture() -> (Arc<Database>, dendro_core::Session) {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE h (id BIGINT PRIMARY KEY, v BIGINT, t TEXT)")
        .unwrap();
    for chunk in 0..4 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {}, 'x{id}')", id * 3)
            })
            .collect();
        s.exec(&format!("INSERT INTO h VALUES {}", vals.join(",")))
            .unwrap();
    }
    db.checkpoint_branch("main").unwrap(); // 物化 4000 行进列存段
    (db, s)
}

fn one(outs: &[Output]) -> String {
    match canonicalize(outs) {
        Canonical::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "全局聚合恰一行：{rows:?}");
            rows[0]
                .iter()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join("|")
        }
        Canonical::Commands(c) => panic!("非行输出：{c:?}"),
    }
}

fn query(s: &mut dendro_core::Session, sql: &str) -> String {
    one(&s.exec(sql).unwrap())
}

#[test]
fn shortcut_matches_row_path_on_pure_segments() {
    let (db, mut s) = fixture();
    // 捷径生效面：纯段（checkpoint 后无增量）
    assert_eq!(
        query(
            &mut s,
            "SELECT COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM h"
        ),
        "Int64(4000)|Int64(24006000)|Float64(6001.5)|Int64(3)|Int64(12000)",
    );
    s.exec("SET dendro.optimize = off").unwrap();
    assert_eq!(
        query(
            &mut s,
            "SELECT COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM h"
        ),
        "Int64(4000)|Int64(24006000)|Float64(6001.5)|Int64(3)|Int64(12000)",
    );
    let _ = db; // fixture 持有
}

#[test]
fn overlay_tail_must_not_be_missed() {
    let (db, mut s) = fixture();
    // checkpoint 后追加 overlay：捷径必须回落并入尾巴行
    s.exec("INSERT INTO h VALUES (999999, 7, 'tail')").unwrap();
    let with_tail = query(&mut s, "SELECT COUNT(*), SUM(v), MIN(v) FROM h");
    s.exec("SET dendro.optimize = off").unwrap();
    assert_eq!(
        with_tail,
        query(&mut s, "SELECT COUNT(*), SUM(v), MIN(v) FROM h")
    );
    assert!(with_tail.contains("Int64(4001)"), "尾巴行计入：{with_tail}");
    let _ = db;
}

#[test]
fn overlay_delete_must_not_overcount() {
    let (db, mut s) = fixture();
    // 段内行删除（DELETE 后未 checkpoint）：捷径若不回落会多数
    s.exec("DELETE FROM h WHERE id <= 1000").unwrap();
    let after_del = query(&mut s, "SELECT COUNT(*), MAX(v) FROM h");
    s.exec("SET dendro.optimize = off").unwrap();
    assert_eq!(after_del, query(&mut s, "SELECT COUNT(*), MAX(v) FROM h"));
    assert!(
        after_del.starts_with("Int64(3000)"),
        "删除已生效：{after_del}"
    );
    let _ = db;
}

#[test]
fn explicit_txn_writes_bypass_shortcut() {
    let (db, mut s) = fixture();
    s.exec("BEGIN").unwrap();
    s.exec("INSERT INTO h VALUES (5000, 5, 'txn')").unwrap();
    s.exec("DELETE FROM h WHERE id = 1").unwrap();
    // 事务内可见：4000 + 1 - 1 = 4000，SUM = 24006000 + 5 - 3
    let in_txn = query(&mut s, "SELECT COUNT(*), SUM(v) FROM h");
    s.exec("COMMIT").unwrap();
    let committed = query(&mut s, "SELECT COUNT(*), SUM(v) FROM h");
    assert_eq!(in_txn, committed, "提交前后同结果：{in_txn} vs {committed}");
    s.exec("SET dendro.optimize = off").unwrap();
    assert_eq!(
        committed,
        query(&mut s, "SELECT COUNT(*), SUM(v) FROM h"),
        "与行式路径一致"
    );
    let _ = db;
}

#[test]
fn count_col_vs_star_and_null_semantics() {
    let (db, mut s) = fixture();
    s.exec("INSERT INTO h VALUES (6000, NULL, 'n')").unwrap();
    s.exec("CHECKPOINT").unwrap();
    // COUNT(v) 跳过 NULL；COUNT(*) 计全行
    let got = query(&mut s, "SELECT COUNT(*), COUNT(v), SUM(v) FROM h");
    s.exec("SET dendro.optimize = off").unwrap();
    assert_eq!(
        got,
        query(&mut s, "SELECT COUNT(*), COUNT(v), SUM(v) FROM h")
    );
    assert!(
        got.starts_with("Int64(4001)|Int64(4000)|"),
        "NULL 语义：{got}"
    );
    let _ = db;
}

#[test]
fn count_distinct_must_fall_back_not_plain_count() {
    // 1M ClickBench 样本上 UserID 全唯一、SearchPhrase 非空值恰好全唯一，
    // count(col) == count(DISTINCT col) 数值巧合掩盖过缺陷——用**有重复值**
    // 的列做强判别：distinct=10 ≠ 非 null 计数 4000
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE d (id BIGINT PRIMARY KEY, g BIGINT)")
        .unwrap();
    for chunk in 0..4 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {})", id % 10)
            })
            .collect();
        s.exec(&format!("INSERT INTO d VALUES {}", vals.join(",")))
            .unwrap();
    }
    db.checkpoint_branch("main").unwrap();
    let on = query(&mut s, "SELECT COUNT(DISTINCT g), SUM(DISTINCT g) FROM d");
    s.exec("SET dendro.optimize = off").unwrap();
    let off = query(&mut s, "SELECT COUNT(DISTINCT g), SUM(DISTINCT g) FROM d");
    assert_eq!(on, off, "DISTINCT 两路径必须一致");
    assert!(
        on.starts_with("Int64(10)|Int64(45)"),
        "distinct=10（0+1+…+9=45），非 plain count/sum：{on}"
    );
}

#[test]
fn ineligible_shapes_fall_back_cleanly() {
    let (db, mut s) = fixture();
    // WHERE / GROUP BY / 表达式参数 / ORDER BY 均不覆盖——行式路径
    // 正常作答而非报错
    // v = id*3，v>100 ⇔ id≥34 → 4000-33 行
    assert_eq!(
        query(&mut s, "SELECT COUNT(*) FROM h WHERE v > 100"),
        "Int64(3967)"
    );
    let g = s.exec("SELECT COUNT(*) FROM h GROUP BY t").unwrap();
    match canonicalize(&g) {
        Canonical::Rows { rows, .. } => assert_eq!(rows.len(), 4000, "GROUP BY 逐组"),
        Canonical::Commands(c) => panic!("{c:?}"),
    }
    let _ = query(&mut s, "SELECT SUM(v + 1) FROM h");
    let _ = query(&mut s, "SELECT AVG(id) FROM h ORDER BY 1");
    let _ = db;
}
