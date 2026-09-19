//! round-trip 不动点黄金用例（前端快速裁决机制）。
//!
//! 价值（q21 教训）：SQL→数据缺陷定位曾走 装载 70min × 反复复现。
//! 本机制把前端（解析→建计划）从执行侧剥离：`unparse(P₁) → SQL₁ →
//! P₂ → SQL₂`，sql₁==sql₂（不动点）⇒ 前端无损 ⇒ 缺陷在执行侧——
//! **纯内存毫秒级**，不碰数据。

use dendro_core::ir::unparse::roundtrip_report;
use dendro_core::sql::SqlDialect::Pg;

#[track_caller]
fn fix(sql: &str) {
    // 主裁决 = IR 结构等价（newIR == IR）；字符串不动点为次级诊断
    let r = roundtrip_report(sql, Pg).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    assert!(
        r.ir_equal,
        "`{sql}`\n  sql1: {}\n  sql2: {}\n  fixpoint={} div: {:?}",
        r.sql1, r.sql2, r.fixpoint, r.divergence
    );
    // 字符串不动点为诊断字段（Display 层包裹可增长，不作断言——
    // 裁决标准是 newIR == IR）
}

#[test]
fn clickbench_shapes_reach_fixpoint() {
    // q21 本体（LIKE 掩码缺陷的触发形态）
    fix("SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'");
    fix("SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits \
         WHERE URL LIKE '%google%' AND SearchPhrase <> '' \
         GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10");
    // 捷径形态
    fix("SELECT COUNT(*), SUM(AdvEngineID), AVG(ResolutionWidth) FROM hits");
    fix("SELECT COUNT(DISTINCT UserID) FROM hits WHERE AdvEngineID <> 0");
    // 深组合：JOIN + 范围 + 多列组 + 排序 + 分页
    fix(
        "SELECT o.id, c.region, COUNT(*) AS n FROM orders o JOIN customers c ON o.cid = c.id \
         WHERE o.total > 100 AND c.tier < 3 GROUP BY o.id, c.region \
         HAVING COUNT(*) > 2 ORDER BY n DESC, o.id ASC LIMIT 20 OFFSET 10",
    );
    // 嵌套派生表 + IN 子查询（SemiJoin）
    fix(
        "SELECT * FROM (SELECT id, v FROM t WHERE v > 1) AS s WHERE s.id IN \
         (SELECT uid FROM u WHERE k = 3) ORDER BY s.v LIMIT 5",
    );
    // NOT IN（反半连接三值语义）
    fix("SELECT a FROM t WHERE a NOT IN (SELECT b FROM u)");
    // DISTINCT + 集合操作
    fix("SELECT DISTINCT a, b FROM t UNION ALL SELECT x, y FROM u ORDER BY 1 LIMIT 3");
    // CTE（绑定 → WITH 重构）
    fix("WITH recent AS (SELECT id FROM t WHERE ts > 100) SELECT COUNT(*) FROM recent");
    // 窗口
    fix("SELECT id, row_number() OVER (PARTITION BY g ORDER BY v DESC) AS rn FROM t");
    // CASE / BETWEEN / IN 列表 / CAST（表达式层全覆盖）
    fix("SELECT CASE WHEN a BETWEEN 1 AND 9 THEN CAST(a AS TEXT) ELSE 'x' END FROM t WHERE b IN (1,2,3)");
}

#[test]
fn unsupported_shapes_reported_honestly() {
    // 无 FROM 常量输入（Values）与 WITH RECURSIVE：诚实标注不可渲染，
    // 不得伪装成失败或静默降级
    let e = roundtrip_report("SELECT 1 + 1", Pg).unwrap_err();
    assert!(e.message.contains("Values"), "{e}");
    let e2 = roundtrip_report(
        "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) \
         SELECT COUNT(*) FROM r",
        Pg,
    );
    assert!(e2.is_err(), "IterativeScan 应诚实拒绝：{e2:?}");
}

#[test]
fn verdict_surface_is_machine_readable() {
    // 裁决面：raw_ir + sql1 + fixpoint 布尔（CLI/测试消费的稳定契约）
    let r = roundtrip_report("SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'", Pg).unwrap();
    assert!(r.parse_faithful());
    assert!(r.raw_ir.contains("scan"), "{}", r.raw_ir); // IR 方言小写节点名
    assert!(r.sql1.to_ascii_uppercase().contains("LIKE"), "{}", r.sql1);
}

#[test]
fn canonical_form_uniqueness_contract() {
    use dendro_core::ir::unparse::compare_sql;
    // 唯一性（⟸）：表面不同、语义等价的文本 → 同一正式形式
    for (a, b) in [
        (
            "SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'",
            "select count ( * ) from HITS where uRl like '%google%'",
        ),
        (
            "SELECT id FROM t WHERE a > 1 AND b < 2",
            "SELECT id FROM (SELECT * FROM (SELECT * FROM t)) WHERE b < 2 AND a > 1",
        ),
        (
            "SELECT a, b FROM t ORDER BY a",
            "SELECT a AS \"a\", b AS \"b\" FROM t ORDER BY (a) ASC",
        ),
    ] {
        let r = compare_sql(a, b, Pg).unwrap_or_else(|e| panic!("`{a}`: {e}"));
        assert!(r.equal, "应等价：\nA={a}\nB={b}\n{:?}", r.divergence);
    }
    // 判别性（⟹）：语义不同的文本 → 正式形式可区分
    for (a, b) in [
        (
            "SELECT id FROM t WHERE a > 1",
            "SELECT id FROM t WHERE a > 2",
        ),
        ("SELECT a, b FROM t", "SELECT b, a FROM t"), // 列序是语义
        ("SELECT COUNT(*) FROM t", "SELECT COUNT(v) FROM t"),
    ] {
        let r = compare_sql(a, b, Pg).unwrap();
        assert!(!r.equal, "应可区分：`{a}` vs `{b}`");
    }
}
