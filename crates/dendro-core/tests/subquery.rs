//! P0：子查询内联（非相关 → 常量/InList/Bool——PG SubLink→InitPlan 同构）
//! 标量子查询 / IN 子查询 / EXISTS / NOT EXISTS / 相关子查询拒绝

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
    s.exec("CREATE TABLE orders (id BIGINT PRIMARY KEY, cid BIGINT, total BIGINT)")
        .unwrap();
    s.exec("CREATE TABLE customers (id BIGINT PRIMARY KEY, region TEXT, vip BOOLEAN)")
        .unwrap();
    s.exec("INSERT INTO orders VALUES (1, 1, 100), (2, 2, 250), (3, 1, 50), (4, 3, 80)")
        .unwrap();
    s.exec("INSERT INTO customers VALUES (1, 'EU', true), (2, 'US', false), (3, 'APAC', true)")
        .unwrap();
}

fn count(d: &Arc<Database>, sql: &str) -> i64 {
    let mut s = d.new_session();
    match &s.exec(sql).unwrap()[0] {
        Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap().parse().unwrap(),
        _ => panic!(),
    }
}

#[test]
fn scalar_subquery_in_where() {
    let d = db();
    setup(&d);
    // WHERE total > (SELECT avg(total) FROM orders)
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE total > (SELECT avg(total) FROM orders)",
    );
    // avg = (100+250+50+80)/4 = 120 → total > 120: 只有 250
    assert_eq!(n, 1);
}

#[test]
fn in_subquery_in_where() {
    let d = db();
    setup(&d);
    // WHERE cid IN (SELECT id FROM customers WHERE vip = true)
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE cid IN (SELECT id FROM customers WHERE vip = true)",
    );
    // vip customers: 1, 3 → orders with cid ∈ {1,3}: id 1, 3, 4 → 3 行
    assert_eq!(n, 3);
}

#[test]
fn not_in_subquery() {
    let d = db();
    setup(&d);
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE cid NOT IN (SELECT id FROM customers WHERE vip = true)",
    );
    // 非 vip: cid=2 → orders id 2 → 1 行
    assert_eq!(n, 1);
}

#[test]
fn exists_subquery() {
    let d = db();
    setup(&d);
    // EXISTS (SELECT FROM customers WHERE vip = true) → true → 全部行
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE EXISTS (SELECT id FROM customers WHERE vip = true)",
    );
    assert_eq!(n, 4);
}

#[test]
fn not_exists_subquery() {
    let d = db();
    setup(&d);
    let n = count(&d, "SELECT count(*) FROM orders WHERE NOT EXISTS (SELECT id FROM customers WHERE region = 'XX')");
    // 无 region='XX' → NOT EXISTS = true → 4 行
    assert_eq!(n, 4);
}

#[test]
fn nested_scalar_subquery() {
    let d = db();
    setup(&d);
    // 双层嵌套：(SELECT max(total) FROM orders WHERE cid = (SELECT id FROM customers WHERE region = 'EU'))
    let n = count(&d, "SELECT count(*) FROM orders WHERE total >= (SELECT max(total) FROM orders WHERE cid = (SELECT id FROM customers WHERE region = 'EU'))");
    // EU → cid=1 → max(total) where cid=1 = max(100,50)=100 → total>=100: id 1,2 → 2 行
    assert_eq!(n, 2);
}

#[test]
fn scalar_subquery_with_aggregate_eq() {
    let d = db();
    setup(&d);
    // WHERE total = (SELECT max(total) FROM orders) → 只有 250
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE total = (SELECT max(total) FROM orders)",
    );
    assert_eq!(n, 1);
}

#[test]
fn empty_in_subquery() {
    let d = db();
    setup(&d);
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE cid IN (SELECT id FROM customers WHERE region = 'XX')",
    );
    assert_eq!(n, 0);
}

#[test]
fn correlated_subquery_iterative() {
    let d = db();
    setup(&d);
    // L2：相关子查询迭代求值（外层列逐行代入 + memo）——三种形态
    // 标量：total >= (SELECT max(c.id) FROM customers c WHERE c.id <= o.cid)
    let n = count(&d, "SELECT count(*) FROM orders o WHERE o.total >= (SELECT max(c.id) FROM customers c WHERE c.id <= o.cid)");
    // o.cid ∈ {1,2,1,3} → max(id) ∈ {1,2,1,3}，全部 total ≥ 之 → 4 行
    assert_eq!(n, 4);
    // EXISTS：相关存在性检测
    let n2 = count(&d, "SELECT count(*) FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = o.cid AND c.vip = true)");
    // vip customers {1,3} → orders cid ∈ {1,3} → id 1,3,4 → 3 行
    assert_eq!(n2, 3);
    // IN：相关成员探测
    let n3 = count(&d, "SELECT count(*) FROM orders o WHERE o.total IN (SELECT c.id * 100 FROM customers c WHERE c.id = o.cid)");
    // (1,100)✓ (2,250 vs 200)✗ (3,50 vs 100)✗ (4,80 vs 300)✗ → 1 行
    assert_eq!(n3, 1);
}

#[test]
fn correlated_unqualified_outer_ref_loud() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // 非限定名外层引用（avg(total) —— total 非 customers 列，PG 会回
    // 溯外层作用域）不在 L2 支持面：限定名代入仅覆盖显式前缀形态。
    // 行为合同 = 响亮 42703（非静默错果）
    let e = s
        .exec("SELECT count(*) FROM orders WHERE total > (SELECT avg(total) FROM customers WHERE customers.id = orders.cid)")
        .unwrap_err();
    assert!(e.message.contains("does not exist"), "{e}");
}

#[test]
fn semi_join_null_semantics() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    s.exec("CREATE TABLE t_null (id BIGINT PRIMARY KEY, k BIGINT)")
        .unwrap();
    s.exec("INSERT INTO t_null VALUES (1, 1), (2, NULL)")
        .unwrap();
    // NOT IN + build 侧含 NULL → 恒非真（三值逻辑——全弃）
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE id NOT IN (SELECT k FROM t_null)",
    );
    assert_eq!(n, 0, "x NOT IN (1, NULL) 恒 NULL/false");
    // 正 IN + build 侧 NULL → 命中照常
    let n2 = count(
        &d,
        "SELECT count(*) FROM orders WHERE id IN (SELECT k FROM t_null)",
    );
    assert_eq!(n2, 1, "仅 id=1 命中");
    // probe 侧 NULL（orders.cid 有 NULL 行）
    let n3 = count(
        &d,
        "SELECT count(*) FROM orders WHERE cid NOT IN (SELECT id FROM customers)",
    );
    // cid ∈ {1,2,1,3,NULL}：非 {1,2,3} 的只有 NULL 行 → NULL NOT IN = NULL → 弃
    assert_eq!(n3, 0);
}

#[test]
fn semi_join_int_width_mixed() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // build 侧 INT（窄）vs probe 侧 BIGINT（宽）——宽度归一哈希命中
    s.exec("CREATE TABLE t_narrow (id BIGINT PRIMARY KEY, k INT)")
        .unwrap();
    s.exec("INSERT INTO t_narrow VALUES (1, 1), (2, 3)")
        .unwrap();
    let n = count(
        &d,
        "SELECT count(*) FROM orders WHERE cid IN (SELECT k FROM t_narrow)",
    );
    // cid ∈ {1,2,1,3} 命中 {1,1,3} → 3 行（旧 join 键宽度分叉同因修复口径）
    assert_eq!(n, 3);
}

#[test]
fn in_subquery_under_or_not_semijoin() {
    let d = db();
    setup(&d);
    // OR 位不落 SemiJoin（InList 内联路径保留）——两种机制共存等价
    let n = count(&d, "SELECT count(*) FROM orders WHERE cid IN (SELECT id FROM customers WHERE region = 'EU') OR total > 200");
    // EU → cid=1 → orders 1,3；total>200 → order 2 → 并集 {1,2,3} → 3
    assert_eq!(n, 3);
}

// ---------- 架构评审止血回归 ----------

#[test]
fn window_function_now_works() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // sum() OVER() —— 每行出值（4 行而非全局塌缩 1 行）
    let out = s
        .exec("SELECT count(*) FROM (SELECT sum(total) OVER () FROM orders) w")
        .unwrap();
    match &out[0] {
        dendro_core::types::Output::Rows(rs) => {
            assert_eq!(rs.text_rows()[0][0].clone().unwrap(), "4");
        }
        _ => panic!(),
    }
    // row_number() OVER(ORDER BY id)
    let out2 = s
        .exec("SELECT count(*) FROM (SELECT row_number() OVER (ORDER BY id) FROM orders) w")
        .unwrap();
    match &out2[0] {
        dendro_core::types::Output::Rows(rs) => {
            assert_eq!(rs.text_rows()[0][0].clone().unwrap(), "4");
        }
        _ => panic!(),
    }
}

#[test]
fn comma_from_rejected_not_silently_dropped() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // FROM t1, t2 曾静默丢 t2——现在是诚实拒绝
    let e = s
        .exec("SELECT count(*) FROM orders, customers")
        .unwrap_err();
    assert!(
        e.message.contains("comma") || e.message.contains("JOIN"),
        "{e}"
    );
}

#[test]
fn predicate_eval_error_propagates_not_silent() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // 非法列引用在 WHERE——原被 apply_predicates 回退分支吞掉返回 0 行
    // 现在应报"column not found"
    let e = s
        .exec("SELECT count(*) FROM orders WHERE nonexistent_col > 5")
        .unwrap_err();
    assert!(
        e.message.contains("column") || e.message.contains("exist"),
        "应报列不存在而非静默空集：{e}"
    );
}

// ---------- 递归 CTE ----------

#[test]
fn recursive_cte_divergent_errors() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    // 无终止条件的递归——必须报错而非静默截断返回部分行
    let r = s.exec(
        "WITH RECURSIVE inf AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM inf) SELECT count(*) FROM inf",
    );
    match r {
        Ok(_) => panic!("发散递归应报错，不应返回部分行"),
        Err(e) => assert!(
            e.message.contains("limit") || e.message.contains("recursive"),
            "应报发散上限错误：{e}"
        ),
    }
}

#[test]
fn recursive_cte_fibonacci() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    let r = s.exec(
        "WITH RECURSIVE fib AS (\
         SELECT 1 AS n, 0 AS a, 1 AS b \
         UNION ALL \
         SELECT n + 1, b, a + b FROM fib WHERE n < 11\
         ) SELECT a FROM fib WHERE n = 11",
    );
    match r {
        Ok(o) => match &o[0] {
            dendro_core::types::Output::Rows(rs) => {
                // 行 n 的 a = fib(n-1)（0 起）：n=11 → fib(10)=55
                let v = rs.text_rows()[0][0].clone().unwrap();
                assert_eq!(v, "55", "fib(10) = 55（0 1 1 2 3 5 8 13 21 34 55）");
            }
            _ => panic!(),
        },
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn recursive_cte_sum() {
    let d = db();
    setup(&d);
    let mut s = d.new_session();
    let r = s.exec(
        "WITH RECURSIVE cnt AS (\
         SELECT 1 AS n \
         UNION ALL \
         SELECT n + 1 FROM cnt WHERE n < 100\
         ) SELECT sum(n) FROM cnt",
    );
    match r {
        Ok(o) => match &o[0] {
            dendro_core::types::Output::Rows(rs) => {
                let v = rs.text_rows()[0][0].clone().unwrap();
                assert_eq!(v, "5050", "sum(1..100) = 5050");
            }
            _ => panic!(),
        },
        Err(e) => panic!("{e}"),
    }
}
