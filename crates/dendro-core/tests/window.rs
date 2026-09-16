//! P0：窗口函数——OVER (PARTITION BY / ORDER BY) + 排名 + 裸聚合窗口

use dendro_core::{Database, DbOptions, Output, StoreConfig};
use std::sync::Arc;

fn db() -> Arc<Database> {
    Database::open(DbOptions { store: StoreConfig::Memory, ..Default::default() }).unwrap()
}

fn setup(d: &Arc<Database>) {
    let mut s = d.new_session();
    s.exec("CREATE TABLE w (id BIGINT PRIMARY KEY, grp TEXT, v INT)").unwrap();
    s.exec("INSERT INTO w VALUES (1,'a',10),(2,'a',20),(3,'a',30),(4,'b',100),(5,'b',200)").unwrap();
}

fn rows(d: &Arc<Database>, sql: &str) -> Vec<Vec<String>> {
    let mut s = d.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => rs.text_rows().iter().map(|r| r.iter().map(|c| c.clone().unwrap_or_default()).collect()).collect(),
        _ => panic!(),
    }
}

#[test]
fn row_number_global() {
    let d = db(); setup(&d);
    let r = rows(&d, "SELECT row_number() OVER (ORDER BY v DESC) FROM w ORDER BY 1");
    assert_eq!(r.len(), 5);
    assert_eq!(r[0][0], "1");
    assert_eq!(r[4][0], "5");
}

#[test]
fn row_number_partition() {
    let d = db(); setup(&d);
    let r = rows(&d, "SELECT grp, row_number() OVER (PARTITION BY grp ORDER BY v) FROM w ORDER BY grp, v");
    // a 组 3 行 rank 1-3；b 组 2 行 rank 1-2
    assert_eq!(r.len(), 5);
    assert_eq!(r[0], vec!["a", "1"]);
    assert_eq!(r[2], vec!["a", "3"]);
    assert_eq!(r[3], vec!["b", "1"]);
    assert_eq!(r[4], vec!["b", "2"]);
}

#[test]
fn rank_and_dense_rank() {
    let d = db(); setup(&d);
    // 值 [10,20,30,100,200]——全不同所以 rank = dense_rank = row_number
    let r = rows(&d, "SELECT rank() OVER (ORDER BY v), dense_rank() OVER (ORDER BY v) FROM w ORDER BY v");
    assert_eq!(r[0], vec!["1", "1"]);
    assert_eq!(r[4], vec!["5", "5"]);
}

#[test]
fn sum_over_partition() {
    let d = db(); setup(&d);
    let r = rows(&d, "SELECT grp, sum(v) OVER (PARTITION BY grp) FROM w ORDER BY grp, v");
    // a 组 sum = 10+20+30 = 60；b 组 sum = 100+200 = 300
    assert_eq!(r[0], vec!["a", "60"]);
    assert_eq!(r[2], vec!["a", "60"]);
    assert_eq!(r[3], vec!["b", "300"]);
    assert_eq!(r[4], vec!["b", "300"]);
}

#[test]
fn sum_over_running() {
    let d = db(); setup(&d);
    // v1：无 frame → 整分区同值（running 需 ROWS frame——v2）
    // ORDER BY 仍影响排名函数但不影响聚合 frame（v1 简化）
    let r = rows(&d, "SELECT sum(v) OVER (ORDER BY v) FROM w ORDER BY v");
    assert_eq!(r[0][0], "360");
    assert_eq!(r[4][0], "360");
}

#[test]
fn count_over_partition() {
    let d = db(); setup(&d);
    let r = rows(&d, "SELECT grp, count(*) OVER (PARTITION BY grp) FROM w ORDER BY grp, v");
    assert_eq!(r[0], vec!["a", "3"]);
    assert_eq!(r[3], vec!["b", "2"]);
}

#[test]
fn avg_min_max_over() {
    let d = db(); setup(&d);
    let r = rows(&d, "SELECT min(v) OVER (PARTITION BY grp), max(v) OVER (PARTITION BY grp) FROM w WHERE grp = 'a' ORDER BY v");
    assert_eq!(r[0], vec!["10", "30"]);
    assert_eq!(r[2], vec!["10", "30"]);
}

#[test]
fn window_returns_all_rows_not_collapsed() {
    let d = db(); setup(&d);
    // 核心回归：sum() OVER() 不塌缩为 1 行（每行出值）
    let r = rows(&d, "SELECT sum(v) OVER () FROM w");
    assert_eq!(r.len(), 5, "窗口每行出值（原全局聚合塌缩 1 行）");
    assert_eq!(r[0][0], "360");
}
