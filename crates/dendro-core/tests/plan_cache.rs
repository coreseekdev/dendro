//! P2-6 v2a 计划缓存正确性测试
//!
//! 缓存的是「已解析 AST」（按 SQL 文本 hash 键控），执行期名字解析、
//! schema 绑定、快照读取全部发生在每次执行时——因此天然免疫 DDL 漂移。
//! 本文件用行为测试固化这些不变量。

use dendro_core::embed::Connection;

#[test]
fn cache_hit_replays_identical_results() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT)")
        .unwrap();
    for i in 0..10 {
        conn.execute(&format!("INSERT INTO t VALUES ({i}, {i}*2)"))
            .unwrap();
    }
    // 同一文本执行三次：首次 miss（填充缓存），后两次 hit
    let q = "SELECT count(*), sum(v) FROM t";
    let results: Vec<_> = (0..3)
        .map(|_| {
            let r = conn.query(q).unwrap();
            format!("{:?}", r.rows())
        })
        .collect();
    assert_eq!(results[0], results[1], "命中路径不得改变结果");
    assert_eq!(results[1], results[2]);
    assert!(results[0].contains("10"), "应为 10 行: {}", results[0]);
}

#[test]
fn ddl_drift_does_not_poison_cached_ast() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, a INT)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 100)").unwrap();

    // 首次执行 → AST 入缓存
    let r1 = conn.query("SELECT * FROM t").unwrap();
    assert_eq!(r1.rows()[0].len(), 2, "初始两列");

    // DDL 漂移：加列 + 改数据。缓存的 SELECT * AST 必须按执行期
    // catalog 重新解析列集——绝不允许吐出旧 schema 的形状。
    conn.execute("ALTER TABLE t ADD COLUMN b INT").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 200, 7)").unwrap();
    let r2 = conn.query("SELECT * FROM t").unwrap();
    assert_eq!(
        r2.rows()[1].len(),
        3,
        "缓存 AST 执行时必须看到新列（实时绑定，非陈旧计划）"
    );

    // 更狠的漂移：同名列数不变但类型变化，同样走实时解析
    conn.execute("DROP TABLE t").unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, a TEXT, b TEXT)")
        .unwrap();
    let r3 = conn.query("SELECT * FROM t").unwrap();
    assert_eq!(r3.rows().len(), 0, "重建后空表");
    conn.execute("INSERT INTO t VALUES (1, 'x', 'y')").unwrap();
    let r4 = conn.query("SELECT * FROM t").unwrap();
    assert_eq!(
        r4.rows()[0][1],
        dendro_core::types::SqlValue::Utf8("x".into())
    );
}

#[test]
fn distinct_literal_text_distinct_results() {
    let mut conn = Connection::memory().unwrap();
    // 同构不同字面量 → 必然不同缓存键 → 各自正确
    for n in 1..=5 {
        let r = conn.query(&format!("SELECT {n} AS n")).unwrap();
        assert_eq!(
            r.rows()[0][0],
            dendro_core::types::SqlValue::Int64(n),
            "SELECT {n} 结果错乱"
        );
    }
    // 再跑一遍（命中路径）仍各归各位
    for n in 1..=5 {
        let r = conn.query(&format!("SELECT {n} AS n")).unwrap();
        assert_eq!(r.rows()[0][0], dendro_core::types::SqlValue::Int64(n));
    }
}

#[test]
fn cache_rejects_nothing_on_parse_error_miss() {
    let mut conn = Connection::memory().unwrap();
    // 语法错误：miss → parse 报错，绝不能把错误对象缓存后改变行为
    assert!(conn.query("SELEC 1").is_err());
    assert!(conn.query("SELEC 1").is_err(), "重复报错行为一致");
    // 正确语句不受污染
    let r = conn.query("SELECT 1").unwrap();
    assert_eq!(r.rows()[0][0], dendro_core::types::SqlValue::Int64(1));
}

#[test]
fn plan_cache_is_bounded() {
    // 白盒：越过上限后整体清空，未参数化海量唯一 SQL 不会撑爆内存
    let mut conn = Connection::memory().unwrap();
    for i in 0..4300 {
        conn.query(&format!("SELECT {i}")).unwrap();
    }
    let n = conn.database().plan_cache_len();
    assert!(n <= 4096, "计划缓存无界增长: {n} 条（上限 4096）");
}

#[test]
fn multi_statement_batch_cached_and_correct() {
    let mut conn = Connection::memory().unwrap();
    conn.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .unwrap();
    let batch = "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (3);";
    conn.execute(batch).unwrap();
    // 第二次执行同文本（缓存命中路径）：主键冲突必须照常报错，
    // 证明缓存重放不会跳过执行期约束检查
    assert!(
        conn.execute(batch).is_err(),
        "缓存命中路径必须仍然执行约束检查"
    );
}
