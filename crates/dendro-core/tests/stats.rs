//! 列统计（join reorder 前置）：CBF footer 聚合（稀疏读）+ order 域
//! 选择率 + EXPLAIN ANALYZE est 呈现。已知数据 → 精确断言。

use dendro_core::types::{Output, SqlValue};
use dendro_core::{Database, DbOptions, StoreConfig};
use std::sync::Arc;

fn fixture() -> Arc<Database> {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    // 列存引擎显式接线（embed 不设——统计面依赖段 footer）
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE w (id BIGINT PRIMARY KEY, a INT, b BIGINT, note TEXT)")
        .unwrap();
    for chunk in 0..12 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {}, {}, 'n')", id % 97, 12_000 - id)
            })
            .collect();
        s.exec(&format!("INSERT INTO w VALUES {}", vals.join(",")))
            .unwrap();
    }
    s.exec("CHECKPOINT").unwrap(); // 物化段
    db
}

fn analyze_text(db: &Arc<Database>, sql: &str) -> String {
    let mut s = db.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => panic!(),
    }
}

fn query_one(db: &Arc<Database>, sql: &str) -> SqlValue {
    let mut s = db.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => {
            rs.text_rows().first().unwrap()[0]
                .clone()
                .map(|t| t.parse::<i64>().ok().map(SqlValue::Int64).unwrap_or(SqlValue::Utf8(t)))
                .unwrap()
        }
        _ => panic!(),
    }
}

#[test]
fn stats_exact_min_max_rows() {
    // a ∈ [0, 96]（id%97 首行 id=1 → 1；id=97 → 0）；b ∈ [1, 11999]
    let c = fixture();
    // 全扫描形态（点查经 CurrentPoint 短路——rows=1 非 12000）
    let text = analyze_text(&c, "EXPLAIN ANALYZE SELECT id FROM w WHERE id >= 1");
    assert!(text.contains("scan w rows=12000"), "{text}");
    // order 域：min(a)=0 → u64 域。断言经 est 呈现面：WHERE a > 48 →
    // 选择率 ≈ (96-48)/96 = 0.5 → est ≈ 6000
    let text = analyze_text(&c, "EXPLAIN ANALYZE SELECT id FROM w WHERE a > 48");
    // 下推到 scan 的过滤（单表无 join——Filter{Scan} 捷径）
    assert!(text.contains("scan w rows=12000"), "{text}");
    assert!(
        text.contains("est="),
        "下推扫描应带估算：{text}"
    );
    // est 值：12000 × (1 - (48-0)/(96-0)) = 6000（uniform 假设精确命中
    // 均匀数据——id%97 在 12000 行上近乎均匀）
    let est: u64 = text
        .split("est=")
        .nth(1)
        .unwrap()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let off = (est as i64 - 6000).abs();
    assert!(off < 600, "est={est} 应≈6000（±10%）");
}

#[test]
fn stats_half_open_range_and_actual_agreement() {
    let c = fixture();
    // b ∈ [1, 11999] 递减数列——WHERE b > 6000 选择率 = (11999-6000)/11998
    let text = analyze_text(&c, "EXPLAIN ANALYZE SELECT id FROM w WHERE b > 6000");
    assert!(text.contains("est="), "{text}");
    let est: u64 = text
        .split("est=")
        .nth(1)
        .unwrap()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // 线性数据 uniform 估计精确：行数 = 11999-6000 = 5999
    let off = (est as i64 - 5999).abs();
    assert!(off < 600, "est={est} 应≈5999");
    // actual 行数精确
    assert_eq!(
        query_one(&c, "SELECT count(*) FROM w WHERE b > 6000"),
        SqlValue::Int64(5999)
    );
}

#[test]
fn stats_absent_without_segments() {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    // 接列存但无段（未 checkpoint）——统计面 None
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)").unwrap();
    s.exec("INSERT INTO t VALUES (1, 1)").unwrap();
    let text = analyze_text(&db, "EXPLAIN ANALYZE SELECT id FROM t WHERE v > 0");
    assert!(!text.contains("est="), "无段无统计：{text}");
}
