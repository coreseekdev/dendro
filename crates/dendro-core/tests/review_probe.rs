//! 评审探针（2026-09-18 review）：SemiJoin/L2 边界形态
use dendro_core::{Database, DbOptions, Output, StoreConfig};
use std::sync::Arc;

fn db() -> Arc<Database> {
    Database::open(DbOptions {
        store: StoreConfig::Memory,
        ..Default::default()
    })
    .unwrap()
}
fn setup(d: &Arc<Database>) {
    let mut s = d.new_session();
    s.exec("CREATE TABLE o (id BIGINT PRIMARY KEY, cid BIGINT, total BIGINT)")
        .unwrap();
    s.exec("CREATE TABLE c (id BIGINT PRIMARY KEY, region TEXT)")
        .unwrap();
    s.exec("INSERT INTO o VALUES (1,1,100),(2,2,250),(3,1,50),(4,3,80),(5,NULL,10)")
        .unwrap();
    s.exec("INSERT INTO c VALUES (1,'EU'),(2,'US'),(3,'APAC')")
        .unwrap();
}
fn rows(d: &Arc<Database>, sql: &str) -> String {
    let mut s = d.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| c.clone().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(" | "),
        _ => panic!(),
    }
}

#[test]
fn probe_p1_paren_groups() {
    let d = db();
    setup(&d);
    // 括号 AND 群组被 flatten_and 溶解——语义保持
    // EU={1}：total∈(20,200) ∧ cid=1 → 行 1(100)/3(50) → 2
    assert_eq!(rows(&d, "SELECT count(*) FROM o WHERE (total > 20 AND total < 200) AND cid IN (SELECT id FROM c WHERE region = 'EU')"), "2");
    // 双重括号包裹的 IN
    assert_eq!(
        rows(
            &d,
            "SELECT count(*) FROM o WHERE ((cid IN (SELECT id FROM c)))"
        ),
        "4"
    );
    // NOT 包裹 + 括号
    assert_eq!(
        rows(
            &d,
            "SELECT count(*) FROM o WHERE NOT (cid IN (SELECT id FROM c)) AND total > 20"
        ),
        "0"
    );
}

#[test]
fn probe_p2_multi_in_chain() {
    let d = db();
    setup(&d);
    // 双 SemiJoin 链：EU={1} ∩ {2,3} = ∅ → 0
    assert_eq!(rows(&d, "SELECT count(*) FROM o WHERE cid IN (SELECT id FROM c WHERE region = 'EU') AND cid IN (SELECT id FROM c WHERE id > 1)"), "0");
    // 同一子查询文本出现两次（计划构建两次、各自求值）
    assert_eq!(
        rows(
            &d,
            "SELECT count(*) FROM o WHERE cid IN (SELECT id FROM c) AND cid IN (SELECT id FROM c)"
        ),
        "4"
    );
}

#[test]
fn probe_p3_order_limit_over_semijoin() {
    let d = db();
    setup(&d);
    // EU={1} → 行 1(100)/3(50)；total DESC → id 序 [1,3]
    assert_eq!(rows(&d, "SELECT id FROM o WHERE cid IN (SELECT id FROM c WHERE region = 'EU') ORDER BY total DESC LIMIT 2"), "1 | 3");
    assert_eq!(
        rows(
            &d,
            "SELECT id FROM o WHERE cid IN (SELECT id FROM c) ORDER BY id LIMIT 3 OFFSET 1"
        ),
        "2 | 3 | 4"
    );
}

#[test]
fn probe_p4_mixed_mechanisms() {
    let d = db();
    setup(&d);
    // SemiJoin 合取 + OR 位 InList 内联 + 相关 EXISTS 迭代——三机制同句
    assert_eq!(rows(&d, "SELECT count(*) FROM o WHERE cid IN (SELECT id FROM c) AND (total > 900 OR EXISTS (SELECT 1 FROM c WHERE c.id = o.cid))"), "4");
}

#[test]
fn probe_p5_empty_antijoin() {
    let d = db();
    setup(&d);
    // 空 build 集：正 IN 全弃；NOT IN 全保（除 NULL probe）
    assert_eq!(
        rows(
            &d,
            "SELECT count(*) FROM o WHERE cid IN (SELECT id FROM c WHERE region = 'XX')"
        ),
        "0"
    );
    assert_eq!(
        rows(
            &d,
            "SELECT count(*) FROM o WHERE cid NOT IN (SELECT id FROM c WHERE region = 'XX')"
        ),
        "4"
    );
}

#[test]
fn probe_p6_subst_positions() {
    let d = db();
    setup(&d);
    // 相关子查询藏在 BETWEEN / CASE 容器内
    assert_eq!(rows(&d, "SELECT count(*) FROM o WHERE o.total BETWEEN (SELECT max(c.id) FROM c WHERE c.id = o.cid) AND 1000"), "4");
    assert_eq!(rows(&d, "SELECT count(*) FROM o WHERE CASE WHEN EXISTS (SELECT 1 FROM c WHERE c.id = o.cid) THEN total > 40 ELSE total > 900 END"), "4");
}

#[test]
fn probe_p7_nested_semijoin_in_correlated() {
    let d = db();
    setup(&d);
    // 相关子查询体内部又含 IN 子查询（嵌套 SemiJoin）：
    // 每行 max(同 cid 且 cid∈c 的 total)：row3(50) < 100 → 仅 1 行
    assert_eq!(rows(&d, "SELECT count(*) FROM o WHERE o.total < (SELECT max(o2.total) FROM o o2 WHERE o2.cid IN (SELECT id FROM c WHERE c.id = o.cid))"), "1");
}

#[test]
fn probe_p9_cross_type_error_contract() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // 探测键 BIGINT vs build 侧 TEXT：线性 cmp_values 路径——跨型比较错误照旧上抛
    let e = s
        .exec("SELECT count(*) FROM o WHERE cid IN (SELECT region FROM c)")
        .unwrap_err();
    assert!(
        e.message.contains("compare") || e.message.contains("cannot"),
        "跨型必须响亮报错：{e}"
    );
}

#[test]
fn probe_p10_update_subquery_unchanged() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // UPDATE 谓词子查询本就不支持——确认无新破损面（仍是响亮错误）
    let e = s
        .exec("UPDATE o SET total = 1 WHERE cid IN (SELECT id FROM c)")
        .unwrap_err();
    assert!(!e.message.is_empty());
}

#[test]
fn probe_p8_explain_semijoin() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    let out = s
        .exec("EXPLAIN SELECT id FROM o WHERE cid IN (SELECT id FROM c)")
        .unwrap();
    let t = match &out[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => panic!(),
    };
    assert!(t.contains("semijoin"), "{t}");
    let p = dendro_core::ir::plan::parse_plan(&t[t.find("dendro.ir v1").unwrap()..]);
    assert!(p.is_some(), "{t}");
    // ANALYZE 可执行
    let out2 = s
        .exec("EXPLAIN ANALYZE SELECT count(*) FROM o WHERE cid IN (SELECT id FROM c)")
        .unwrap();
    let t2 = match &out2[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => panic!(),
    };
    assert!(t2.contains("semijoin"), "{t2}");
}

#[test]
fn probe_p11_union_select_arm_correlation() {
    let d = db();
    setup(&d);
    // 集合操作 Select 直臂内的相关引用（评审修复回归——此前只下穿
    // Query 包裹臂）：UNION 右臂 WHERE c.id = o.cid 必须被代入
    let r = rows(&d, "SELECT count(*) FROM o WHERE o.total > (SELECT max(x.v) FROM (SELECT total AS v FROM o WHERE cid = o.cid UNION SELECT c.id * 60 FROM c WHERE c.id = o.cid) x)");
    // 内层按行：cid=1 → {100,50}∪{60} max=100；cid=2 → {250,50}∪{120} max=250；
    // cid=3 → {80}∪{180} max=180；NULL → 空 → NULL。
    // 外层 total > 该值：row1 100>100✗ row2 250>250✗ row3 50>100✗ row4 80>180✗ → 0？
    // 注：左臂 cid=o.cid 汇总两行（1,3 同 cid=1）——逐行代入后如上 → 0
    assert_eq!(r, "0");
    // 正向可证形态：右臂单独命中
    let r2 = rows(&d, "SELECT count(*) FROM o WHERE o.total > (SELECT max(v) FROM (SELECT 0 AS v UNION SELECT c.id * 60 FROM c WHERE c.id = o.cid) x)");
    // cid=1→60 cid=2→120 cid=3→180 NULL→0；total: 100>60✓ 250>120✓ 50>60✗ 80>180✗ 10>0✓ → 3
    assert_eq!(r2, "3");
}
