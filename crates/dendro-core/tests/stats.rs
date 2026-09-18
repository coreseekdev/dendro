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
        Output::Rows(rs) => rs.text_rows().first().unwrap()[0]
            .clone()
            .map(|t| {
                t.parse::<i64>()
                    .ok()
                    .map(SqlValue::Int64)
                    .unwrap_or(SqlValue::Utf8(t))
            })
            .unwrap(),
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
    assert!(text.contains("est="), "下推扫描应带估算：{text}");
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
    s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    s.exec("INSERT INTO t VALUES (1, 1)").unwrap();
    let text = analyze_text(&db, "EXPLAIN ANALYZE SELECT id FROM t WHERE v > 0");
    assert!(!text.contains("est="), "无段无统计：{text}");
}

// ---------- ANALYZE：等高直方图 + MCV + 精确 NDV（性能批 P1） ----------

/// 带列存接线 + CHECKPOINT 的单表夹具（est 面依赖段 footer）
fn fixture_db_with_table(name: &str, n: usize, cgen: impl Fn(usize) -> i64) -> Arc<Database> {
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec(&format!(
        "CREATE TABLE {name} (id BIGINT PRIMARY KEY, c BIGINT)"
    ))
    .unwrap();
    let mut vals = Vec::new();
    for i in 0..n {
        vals.push(format!("({i}, {})", cgen(i)));
        if vals.len() == 1000 {
            s.exec(&format!("INSERT INTO {name} VALUES {}", vals.join(",")))
                .unwrap();
            vals.clear();
        }
    }
    if !vals.is_empty() {
        s.exec(&format!("INSERT INTO {name} VALUES {}", vals.join(",")))
            .unwrap();
    }
    s.exec("CHECKPOINT").unwrap();
    db
}

/// 端到端：ANALYZE 后偏斜等值谓词的 est 从 uniform 的灾难性低估
/// 修正到 MCV 精确频次
#[test]
fn analyze_mcv_fixes_skewed_eq_est() {
    let d = fixture_db_with_table("sk", 10_000, |i| {
        if i % 10 < 9 {
            7
        } else {
            1000 + (i % 100) as i64
        }
    });
    let mut s = d.new_session();
    // ANALYZE 前后各取 est（EXPLAIN ANALYZE 的 est 行）
    let est = |s: &mut dendro_core::engine::Session, sql: &str| -> String {
        let out = s.exec(sql).unwrap();
        let t = match &out[0] {
            dendro_core::types::Output::Rows(rs) => rs
                .text_rows()
                .iter()
                .map(|r| r[0].clone().unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!(),
        };
        t.lines()
            .find(|l| l.contains("scan sk"))
            .and_then(|l| l.split("est=").nth(1))
            .map(|x| x.trim().to_string())
            .unwrap_or_default()
    };
    let sql = "EXPLAIN ANALYZE SELECT count(*) FROM sk WHERE c = 7";
    let before = est(&mut s, sql);
    s.exec("ANALYZE sk").unwrap();
    let after = est(&mut s, sql);
    let b: f64 = before.parse().unwrap_or(0.0);
    let a: f64 = after.parse().unwrap_or(0.0);
    // 偏斜主值：MCV 精确 ≈ 9000；uniform 1/1100 ≈ 9——千倍级修正
    assert!(
        a > 8000.0 && a < 10000.0,
        "MCV est 应 ≈9000（got {a}；pre-analyze {b}）"
    );
    assert!(b < 100.0, "uniform 基线应严重低估（got {b}）");
}

/// 等高直方图：非均匀分布的范围谓词 est 比 uniform 明显更准
#[test]
fn analyze_histogram_range_est() {
    let d = fixture_db_with_table("h", 20_000, |i| {
        if i % 10 < 9 {
            (i % 1000) as i64
        } else {
            1000 + ((i * 7919) % 99_000) as i64
        }
    });
    let mut s = d.new_session();
    let est = |s: &mut dendro_core::engine::Session, sql: &str| -> String {
        let out = s.exec(sql).unwrap();
        let t = match &out[0] {
            dendro_core::types::Output::Rows(rs) => rs
                .text_rows()
                .iter()
                .map(|r| r[0].clone().unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!(),
        };
        t.lines()
            .find(|l| l.contains("scan h"))
            .and_then(|l| l.split("est=").nth(1))
            .map(|x| x.trim().to_string())
            .unwrap_or_default()
    };
    let sql = "EXPLAIN ANALYZE SELECT count(*) FROM h WHERE c < 1000";
    let before = est(&mut s, sql); // uniform: 1000/99000 ≈ 1% → ~200
    s.exec("ANALYZE h").unwrap();
    let after = est(&mut s, sql); // 直方图：首桶区间 ≈ 90%
    let b: f64 = before.parse().unwrap_or(0.0);
    let a: f64 = after.parse().unwrap_or(0.0);
    assert!(
        a > 15_000.0 && a < 19_000.0,
        "直方图 est 应 ≈18000（got {a}；uniform {b}）"
    );
    assert!(b < 2000.0, "uniform 基线应严重低估（got {b}）");
}

/// ANALYZE 语句合同：非显式事务外可用；事务内拒绝
#[test]
fn analyze_txn_guard() {
    let d = fixture_db_with_table("g", 1, |_| 1i64);
    let mut s = d.new_session();
    s.exec("ANALYZE g").unwrap();
    s.exec("BEGIN").unwrap();
    let e = s.exec("ANALYZE g").unwrap_err();
    assert_eq!(e.state, "25001", "{e}");
    s.exec("ROLLBACK").unwrap();
    s.exec("ANALYZE g").unwrap(); // 回滚后可用
}

/// T1/T2：ANALYZE 精确 NDV 与未列值启发（HTAP 差异批）
#[test]
fn analyze_exact_ndv_and_unlisted_heuristic() {
    let d = fixture_db_with_table("nd", 20_000, |i| if i < 19_000 { 5 } else { i as i64 });
    let mut s = d.new_session();
    s.exec("ANALYZE nd").unwrap();
    // 精确 NDV：1001（5 + 19000..19999 各一）
    let an = dendro_core::sql::stats::load_analyze(&d, &s, "nd").unwrap();
    let c_ndv = an.cols[1].ndv;
    assert_eq!(c_ndv, 1001, "精确 NDV（got {c_ndv}）");
    // 未列值启发：c = 5 命中 MCV（19000/20000 = 0.95）；
    // c = 7（未列）≈ (1-0.95)/(1001-32) ≈ 5.1e-5（远低于 1/1001）
    let mut est = |sql: &str| -> f64 {
        let out = s.exec(sql).unwrap();
        let t = match &out[0] {
            dendro_core::types::Output::Rows(rs) => rs
                .text_rows()
                .iter()
                .map(|r| r[0].clone().unwrap())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!(),
        };
        t.lines()
            .find(|l| l.contains("scan nd"))
            .and_then(|l| l.split("est=").nth(1))
            .and_then(|x| x.trim().parse().ok())
            .unwrap_or(0.0)
    };
    let e_main = est("EXPLAIN ANALYZE SELECT count(*) FROM nd WHERE c = 5");
    let e_unlisted = est("EXPLAIN ANALYZE SELECT count(*) FROM nd WHERE c = 7");
    assert!(e_main > 18000.0, "MCV 主值 est ≈19000（got {e_main}）");
    assert!(
        (0.0..=2.0).contains(&e_unlisted),
        "未列值 est 应 ≈1（启发 (1-Σf)/(ndv-|mcv|)；got {e_unlisted}）"
    );
}
