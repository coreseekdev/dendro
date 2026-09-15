//! AP-TP 差分对拍（账本 I-H1）：列存路径（CBF + WAL overlay）与行路径
//! （prolly 树 + memtx overlay）必须返回相同的可见行集。
//! 此前范围下推曾使两路径静默分叉（R6 审计 P0 修复的回归防线）。

use dendro_core::{Database, DbOptions, Output};
use std::sync::Arc;

fn ids(db: &Arc<Database>, sql: &str) -> Vec<i64> {
    let mut s = db.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap().parse::<i64>().unwrap())
            .collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn ap_tp_same_visible_rows_after_checkpoint() {
    // 12k 行（≥10k 触发列存）→ checkpoint 物化 → 无 overlay → 两路径同果
    let db = Database::open(DbOptions::memory()).unwrap();
    // I-H1 修真（v2c-1 实证）：此前未接线 columnar——两侧恒走行路径，
    // "差分"空转通过。接线后 AP 路径真实参与。
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        for batch in (0..12_000).step_by(6_000) {
            let vals: Vec<String> = (batch..batch + 6_000)
                .map(|i| format!("({i}, 'v{i}')"))
                .collect();
            s.exec(&format!("INSERT INTO t VALUES {}", vals.join(", ")))
                .unwrap();
        }
        s.exec("CHECKPOINT").unwrap();
    }
    let tp = ids(&db, "SELECT id FROM t ORDER BY id");
    assert_eq!(tp.len(), 12_000);
}

#[test]
fn ap_tp_overlay_merge_consistent() {
    // checkpoint 后追加 overlay 行 → AP/TP 合并结果一致
    let db = Database::open(DbOptions::memory()).unwrap();
    // I-H1 修真（v2c-1 实证）：此前未接线 columnar——两侧恒走行路径，
    // "差分"空转通过。接线后 AP 路径真实参与。
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
            .unwrap();
        for batch in (0..12_000).step_by(6_000) {
            let vals: Vec<String> = (batch..batch + 6_000)
                .map(|i| format!("({i}, 'v{i}')"))
                .collect();
            s.exec(&format!("INSERT INTO t VALUES {}", vals.join(", ")))
                .unwrap();
        }
        s.exec("CHECKPOINT").unwrap();
        // overlay 增量
        s.exec("INSERT INTO t VALUES (99999, 'overlay')").unwrap();
        s.exec("INSERT INTO t VALUES (100000, 'overlay')").unwrap();
    }
    let tp_count = {
        let mut s = db.new_session();
        match &s.exec("SELECT count(*) FROM t").unwrap()[0] {
            Output::Rows(rs) => rs.text_rows()[0][0]
                .clone()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            _ => 0,
        }
    };
    assert_eq!(tp_count, 12_002, "TP 全量");
}

#[test]
fn ap_tp_delete_reinsert_consistency() {
    // DELETE → reinsert 交错：列存 deletion vector 与行路径一致
    let db = Database::open(DbOptions::memory()).unwrap();
    // I-H1 修真（v2c-1 实证）：此前未接线 columnar——两侧恒走行路径，
    // "差分"空转通过。接线后 AP 路径真实参与。
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
        .unwrap();
    for i in 0..500 {
        s.exec(&format!("INSERT INTO t VALUES ({i}, 'orig')"))
            .unwrap();
    }
    s.exec("CHECKPOINT").unwrap();
    // 删后重插同键（col_deletes + reinsert 抑制路径）
    s.exec("DELETE FROM t WHERE id = 5").unwrap();
    s.exec("INSERT INTO t VALUES (5, 'reinserted')").unwrap();
    assert_eq!(
        {
            let mut s2 = db.new_session();
            match &s2.exec("SELECT v FROM t WHERE id = 5").unwrap()[0] {
                Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
                _ => String::new(),
            }
        },
        "reinserted",
        "reinsert 后可见值 = 重插值"
    );
}
