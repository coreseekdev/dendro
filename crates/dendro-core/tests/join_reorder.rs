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

// ---------- 评审修复回归（P0/P1 盲区清单） ----------

#[test]
fn p0_distinct_limit_semantics() {
    let db = fixture();
    // DISTINCT + LIMIT：语义序 = dedup→sort→limit（曾 sort(topN)→limit→dedup
    // → [a,a,a,b] LIMIT 2 得 [a] 而非 [a,b]——P0）
    let mut s = db.new_session();
    s.exec("SET dendro.optimize = 'on'").unwrap();
    let r = s
        .exec("SELECT DISTINCT cid FROM orders WHERE id <= 8 ORDER BY cid LIMIT 3")
        .unwrap();
    let vs = match &r[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap())
            .collect::<Vec<_>>(),
        _ => panic!(),
    };
    // id 1..8 → cid = id%200 = 1..8（全不同）——8 行 DISTINCT 全留，取前 3
    assert_eq!(vs, vec!["1", "2", "3"], "{vs:?}");
    // 带重复：cid % 4 有 0..3 四值，前 12 个 id → DISTINCT {1,2,3,4,5..12%4}
    let r2 = s
        .exec("SELECT DISTINCT cid % 4 FROM orders WHERE id <= 12 ORDER BY 1 LIMIT 2")
        .unwrap();
    let vs2 = match &r2[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap())
            .collect::<Vec<_>>(),
        _ => panic!(),
    };
    assert_eq!(vs2, vec!["0", "1"], "{vs2:?}");
}

#[test]
fn p0_qualified_wildcard_columns() {
    let db = fixture();
    // SELECT o.* 前缀过滤语义（曾按纯通配 → 两列全出——P0）
    let mut s = db.new_session();
    let r = s
        .exec("SELECT o.* FROM orders o JOIN customers c ON o.cid = c.id WHERE o.id = 1")
        .unwrap();
    match &r[0] {
        Output::Rows(rs) => {
            // o 的 4 列（id, cid, total, note）——非两侧 6 列
            assert_eq!(rs.columns.len(), 4, "o.* 只出 o 列");
            assert_eq!(rs.total_rows(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn p0_wildcard_column_order_stable() {
    let db = fixture();
    // SELECT *：列序 = FROM 序（重排不改变输出 schema——P0）
    let (c_on, r_on) = (
        run(&db, "on", "SELECT * FROM regions r JOIN customers c ON c.rid = r.id JOIN orders o ON o.cid = c.id WHERE o.id = 1"),
        run(&db, "off", "SELECT * FROM regions r JOIN customers c ON c.rid = r.id JOIN orders o ON o.cid = c.id WHERE o.id = 1"),
    );
    assert_eq!(c_on.0, r_on.0, "列名序（客户端 schema）必须一致");
    assert_rows_equiv("SELECT* 列序", &c_on.0, &c_on.1, &r_on.0, &r_on.1, false);
}

#[test]
fn p1_residual_and_bare_name_gating() {
    let db = fixture();
    // 三因子 ON（residual 消歧）+ 中表谓词（曾裸末段使 a.x=b.y 同名
    // 自比较恒真）+ 裸名引用（歧义门控——放弃重排但结果须正确）
    diff(&db, "SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id \
               JOIN regions r ON c.rid = r.id AND c.tier = 0 WHERE o.total < 5");
    // 裸名引用形态（门控放弃重排——结果仍须等价）
    diff(&db, "SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id \
               JOIN regions r ON c.rid = r.id WHERE total < 3");
    // LEFT 下 INNER 链（左侧重排后 LEFT 键仍须正确——hash_join_left 布局修）
    diff(&db, "SELECT count(*) FROM regions r LEFT JOIN customers c ON c.rid = r.id \
               AND c.id < 100 WHERE r.id = 0");
}

#[test]
fn p1_scan_est_with_pushdown_now_effective() {
    // estimate_filter_rows 限定名匹配修——WHERE o.total < 20 的 est 应
    // 显著小于 12000（原恒 12000：CompoundIdentifier 不匹配）
    let db = fixture();
    let mut s = db.new_session();
    let out = s
        .exec("EXPLAIN ANALYZE SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id \
               JOIN regions r ON c.rid = r.id WHERE o.total < 20")
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
    let est: u64 = text
        .lines()
        .find(|l| l.contains("scan orders") && l.contains("est="))
        .and_then(|l| l.split("est=").nth(1))
        .and_then(|e| e.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|e| e.parse().ok())
        .unwrap_or(0);
    assert!(
        est > 0 && est < 12000,
        "限定名匹配修后 est 应反映谓词选择率：est={est}"
    );
}

// ---------- SOTA P2：Statistics Propagation（等值 join 区间传播） ----------

#[test]
fn stat_prop_injects_range_on_wider_side() {
    // orders.cid ∈ [0, 199]（id%200）join customers.id ∈ [0, 199]
    // → 交集 = [0,199]（无收紧——两区间相同）。构造不对称：
    // orders 前 6000 行（cid ∈ [0,199]）× customers 仅前 50 行
    //（id ∈ [0,49]）→ 交集 [0,49] 收紧 orders 侧
    let db = Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap();
    db.set_columnar(Arc::new(dendro_columnar::integrate::CbfColumnar {
        row_group_rows: 4096,
    }));
    let mut s = db.new_session();
    s.exec("CREATE TABLE big (id BIGINT PRIMARY KEY, ref_id BIGINT, v BIGINT)").unwrap();
    s.exec("CREATE TABLE small (id BIGINT PRIMARY KEY, w BIGINT)").unwrap();
    // big: 12k 行 ref_id ∈ [0, 199]
    for chunk in 0..12 {
        let vals: Vec<String> = (0..1000)
            .map(|i| {
                let id = chunk * 1000 + i + 1;
                format!("({id}, {}, {})", id % 200, i)
            })
            .collect();
        s.exec(&format!("INSERT INTO big VALUES {}", vals.join(",")))
            .unwrap();
    }
    // small: 50 行 id ∈ [0, 49]
    {
        let vals: Vec<String> = (0..50).map(|i| format!("({i}, {i})")).collect();
        s.exec(&format!("INSERT INTO small VALUES {}", vals.join(",")))
            .unwrap();
    }
    s.exec("CHECKPOINT").unwrap();
    // 差分：等值 join 的结果两路径等价（传播产生的过滤器不改语义）
    let mut s_on = db.new_session();
    s_on.exec("SET dendro.optimize = 'on'").unwrap();
    let on = rows_of(&s_on.exec("SELECT count(*) FROM big b JOIN small s ON b.ref_id = s.id").unwrap());
    let mut s_off = db.new_session();
    s_off.exec("SET dendro.optimize = 'off'").unwrap();
    let off = rows_of(&s_off.exec("SELECT count(*) FROM big b JOIN small s ON b.ref_id = s.id").unwrap());
    // 期望：每 small.id（50 个）× big 中 ref_id=i 的行数（12000/200=60）= 3000
    assert_rows_equiv("stat_prop join equivalence", &on.0, &on.1, &off.0, &off.1, false);
    // EXPLAIN 验证：big 侧的 scan 应出现传播产生的范围过滤（est 更小）
    let text = {
        let r = s_on
            .exec("EXPLAIN ANALYZE SELECT count(*) FROM big b JOIN small s ON b.ref_id = s.id")
            .unwrap();
        match &r[0] {
            Output::Rows(rs) => rs
                .text_rows()
                .iter()
                .map(|r| r[0].clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!(),
        }
    };
    // 传播后 big 的 est 应显著小于 12000（受 [0,49] 交集收紧）
    let est: u64 = text
        .lines()
        .find(|l| l.contains("scan big"))
        .and_then(|l| l.split("est=").nth(1))
        .and_then(|e| e.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|e| e.parse().ok())
        .unwrap_or(u64::MAX);
    assert!(est < 12000, "传播后 est 应收紧：est={est} (full=12000)\n{text}");
}

// ---------- SOTA P3：等值谓词复制下推 ----------

#[test]
fn eq_copy_pushes_constant_through_join() {
    let db = fixture();
    // WHERE o.total = 500 → ON o.cid = c.id → 复制 c.id = 500 不正确
    //（o.total 与 c.id 无等值对）。构造正确场景：
    // WHERE o.cid = 5 → ON o.cid = c.id → 复制 c.id = 5 到 c 侧
    let mut s = db.new_session();
    s.exec("SET dendro.optimize = 'on'").unwrap();
    let r = s
        .exec("SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id WHERE o.cid = 5")
        .unwrap();
    let on = match &r[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    };
    // 差分
    let mut s_off = db.new_session();
    s_off.exec("SET dendro.optimize = 'off'").unwrap();
    let r_off = s_off
        .exec("SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id WHERE o.cid = 5")
        .unwrap();
    let off = match &r_off[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    };
    assert_eq!(on, off, "eq_copy 差分");
    // 期望：12000 行中 cid=5 的有 60 行（12000/200）
    assert_eq!(on.parse::<i64>().unwrap(), 60, "cid=5 匹配 60 行");
    // EXPLAIN ANALYZE：c 侧 est 应显著收紧（=5 → 200 行中 1 行）
    let text = {
        let r = s
            .exec("EXPLAIN ANALYZE SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id WHERE o.cid = 5")
            .unwrap();
        match &r[0] {
            Output::Rows(rs) => rs
                .text_rows()
                .iter()
                .map(|r| r[0].clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!(),
        }
    };
    let c_est: u64 = text
        .lines()
        .find(|l| l.contains("scan customers") && l.contains("est="))
        .and_then(|l| l.split("est=").nth(1))
        .and_then(|e| e.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|e| e.parse().ok())
        .unwrap_or(u64::MAX);
    assert!(
        c_est < 200,
        "eq_copy 后 customers est 应收紧：est={c_est} (full=200)\n{text}"
    );
}

#[test]
fn eq_copy_range_predicates() {
    let db = fixture();
    // WHERE o.cid >= 190 → ON o.cid = c.id → 复制 c.id >= 190
    let mut s_on = db.new_session();
    s_on.exec("SET dendro.optimize = 'on'").unwrap();
    let r = s_on
        .exec("SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id WHERE o.cid >= 190")
        .unwrap();
    let on = match &r[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    };
    let mut s_off = db.new_session();
    s_off.exec("SET dendro.optimize = 'off'").unwrap();
    let r_off = s_off
        .exec("SELECT count(*) FROM orders o JOIN customers c ON o.cid = c.id WHERE o.cid >= 190")
        .unwrap();
    let off = match &r_off[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap(),
        _ => panic!(),
    };
    assert_eq!(on, off, "范围复制差分");
}
