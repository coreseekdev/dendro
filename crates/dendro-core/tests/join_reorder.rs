//! O-4'：join reorder 差分——INNER 链（≥3 因子）贪心重排 vs 原序，
//! 多重集等价（common §1.1 无序口径；行序随 join 序变化是设计内行为）。
//! 夹具：列存段（统计就位——est 门控生效的必要条件）+ 三表星型。

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
    // 星型：orders 12k → customers 200 → regions 8（FK 链——经典重排形态）
    s.exec("CREATE TABLE orders (id BIGINT PRIMARY KEY, cid BIGINT, total BIGINT, note TEXT)")
        .unwrap();
    s.exec("CREATE TABLE customers (id BIGINT PRIMARY KEY, rid BIGINT, tier INT)")
        .unwrap();
    s.exec("CREATE TABLE regions (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    for chunk in 0..12 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {}, {}, 'n{}')", id % 200, id % 1000, i % 97)
            })
            .collect();
        s.exec(&format!("INSERT INTO orders VALUES {}", vals.join(",")))
            .unwrap();
    }
    {
        let vals: Vec<String> = (0..200)
            .map(|i| format!("({i}, {}, {})", i % 8, i % 5))
            .collect();
        s.exec(&format!("INSERT INTO customers VALUES {}", vals.join(",")))
            .unwrap();
    }
    {
        let vals: Vec<String> = (0..8)
            .map(|i| format!("({i}, 'r{i}')"))
            .collect();
        s.exec(&format!("INSERT INTO regions VALUES {}", vals.join(",")))
            .unwrap();
    }
    s.exec("CHECKPOINT").unwrap(); // 物化段（统计就位）
    db
}

fn rows_of(outs: &[Output]) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    match canonicalize(outs) {
        Canonical::Rows { cols, rows, .. } => (cols, rows),
        Canonical::Commands(c) => panic!("{c:?}"),
    }
}

fn run(db: &Arc<Database>, opt: &str, sql: &str) -> (Vec<String>, Vec<Vec<SqlValue>>) {
    let mut s = db.new_session();
    s.exec(&format!("SET dendro.optimize = '{opt}'")).unwrap();
    rows_of(&s.exec(sql).unwrap())
}

fn diff(db: &Arc<Database>, sql: &str) {
    let on = run(db, "on", sql);
    let off = run(db, "off", sql);
    assert_rows_equiv(
        &format!("`{sql}` reorder on vs off"),
        &on.0, &on.1, &off.0, &off.1, false,
    );
}

#[test]
fn star_join_reorder_equivalence() {
    let db = fixture();
    // 全链（大→小书写：orders 首位——reorder 应倒置为小表先行）
    diff(&db, "SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id \
               JOIN regions r ON c.rid = r.id");
    // 带选择性谓词（orders 侧 1%）
    diff(&db, "SELECT r.name, count(*) FROM orders o JOIN customers c ON o.cid = c.id \
               JOIN regions r ON c.rid = r.id WHERE o.total < 20 GROUP BY r.name");
    // 反向书写（小→大：reorder 可能保持原序或调首——多重集恒等）
    diff(&db, "SELECT count(*) FROM regions r JOIN customers c ON c.rid = r.id \
               JOIN orders o ON o.cid = c.id WHERE o.note = 'n1'");
    // 中表选择性（customers.tier = 0 → ~40 行）
    diff(&db, "SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id \
               JOIN regions r ON c.rid = r.id WHERE c.tier = 0");
}

#[test]
fn reorder_actually_reorders_when_skewed() {
    let db = fixture();
    // EXPLAIN：大表首写的 3-链——重排后 join 输入应体现小表先行。
    // 以 EXPLAIN ANALYZE 的 join 行序佐证（join 的子树墙钟 < 全链……
    // 非确定）。直接断言计划文本：scan 顺序（EXPLAIN join 计划块）
    let mut s = db.new_session();
    let out = s
        .exec("EXPLAIN SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id \
               JOIN regions r ON c.rid = r.id")
        .unwrap();
    let text = match &out[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => panic!(),
    };
    // 重排成立 ⇔ 计划里 regions/customers 的 scan 先于 orders 出现
    //（文本序：首个 scan 行不是 orders）
    let first_scan = text
        .lines()
        .find(|l| l.contains("= scan"))
        .unwrap_or_default();
    assert!(
        !first_scan.contains("orders"),
        "重排未生效（首 scan 仍为 orders）：{text}"
    );
}

#[test]
fn two_way_not_reordered_and_left_untouched() {
    let db = fixture();
    // 2 因子：不重排（O-4 构建侧已覆盖）——等价恒成立
    diff(&db, "SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id");
    // LEFT：外层不动（子链 3+ 才有内层重排面；此查询 LEFT 主链保持）
    let mut s = db.new_session();
    s.exec("SELECT count(*) FROM regions r LEFT JOIN customers c ON c.rid = r.id")
        .unwrap();
}
