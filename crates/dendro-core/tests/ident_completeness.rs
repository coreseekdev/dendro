//! 标识符收集完备性黄金用例（q21 教训固化）。
//!
//! `expr_idents_pub` 是列掩码（O-3/P0 窄行）的列引用事实源——漏收
//! 任何形态 = 掩码丢列 = 查询错值/报错。历史缺陷：手写 match 漏
//! LIKE 家族（ClickBench 10M q21；null 填充期静默错值，窄行期响亮
//! 报错）。实现已重写到 sqlparser 派生 Visit（按构造完备）；本用例
//! 集把**语义**钉死：每种表达式形态的期望 ident 集（黄金值），连同
//! 子查询边界（子查询内标识符不得泄漏到本层）。
//!
//! 新增 Expr 形态时在此补一行黄金用例（比差分更能钉住语义）。

use dendro_core::sql::optimize::expr_idents_pub;
use sqlparser::ast::Statement;

fn idents_of(expr_sql: &str) -> Vec<String> {
    // 形态统一从 SELECT 投影位取出被测表达式（覆盖各种嵌套）
    let q = format!("SELECT ({expr_sql}) FROM t");
    let dialect = sqlparser::dialect::GenericDialect {};
    let stmts = sqlparser::parser::Parser::parse_sql(&dialect, &q).expect("parse");
    let Statement::Query(query) = &stmts[0] else {
        panic!("not a query");
    };
    let body = &*query.body;
    let sqlparser::ast::SetExpr::Select(sel) = body else {
        panic!("not select");
    };
    let sqlparser::ast::SelectItem::UnnamedExpr(e) = &sel.projection[0] else {
        panic!("not unnamed expr");
    };
    // SELECT (expr) 解析为 Nested(expr)——剥一层
    let sqlparser::ast::Expr::Nested(inner) = e else {
        panic!("expect nested");
    };
    let mut out = Vec::new();
    expr_idents_pub(inner, &mut out);
    let mut set: Vec<String> = out.into_iter().collect();
    set.sort();
    set.dedup();
    set
}

#[track_caller]
fn expect(expr_sql: &str, want: &[&str]) {
    let got = idents_of(expr_sql);
    let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    assert_eq!(got, want, "expr: {expr_sql}");
}

#[test]
fn like_family_all_collected() {
    // 历史缺陷本体：列只出现在 LIKE 谓词 → 曾漏收
    expect("u LIKE '%x%'", &["u"]);
    expect("u NOT LIKE '%x%'", &["u"]);
    expect("u ILIKE '%x%'", &["u"]);
    expect("u SIMILAR TO '%x%'", &["u"]);
    expect("u LIKE pat_col", &["pat_col", "u"]); // 模式为列引用
}

#[test]
fn classic_forms_still_collected() {
    expect("a + b", &["a", "b"]);
    expect("a BETWEEN b AND c", &["a", "b", "c"]);
    expect("a IN (b, 1, c)", &["a", "b", "c"]);
    expect("CASE WHEN a > 1 THEN b ELSE c END", &["a", "b", "c"]);
    expect("CAST(a AS INT)", &["a"]);
    expect("a IS NULL", &["a"]);
    expect("-a", &["a"]);
    expect("t.col", &["t.col"]); // 限定名全路径（消费端 rsplit 取末段）
    expect("f(a, b)", &["a", "b"]);
}

#[test]
fn deep_nesting_forms() {
    // 旧收集器未显式覆盖但派生 Visit 应自动到达的形态
    expect("SUBSTRING(u FROM p FOR l)", &["l", "p", "u"]);
    expect("TRIM(BOTH 'x' FROM u)", &["u"]);
    expect("EXTRACT(YEAR FROM ts)", &["ts"]);
    expect("(a, b)", &["a", "b"]); // 元组
    expect("COALESCE(a, b, c)", &["a", "b", "c"]);
    expect("f(x => a)", &["a"]); // 命名参数内的列
}

#[test]
fn subquery_boundary() {
    // 子查询内的标识符属于其自身计划——不得泄漏到本层掩码
    expect(
        "a IN (SELECT x FROM other WHERE y = 1)",
        &["a"], // x/y 不得出现
    );
    // 注意：相关引用（外层列出现在子查询内）当前同样不收集——
    // 已知缺口：相关子查询 × AP 窄扫描若该列仅子查询引用会丢列
    //（v1 相关子查询经 L2 逐行代入，修复需因子键上下文，另行立项）
    expect(
        "EXISTS (SELECT 1 FROM other WHERE other.k = t.col2)",
        &[], // 子查询边界整体隔离（含相关引用——现状语义）
    );
    expect("(SELECT MAX(z) FROM other) + a", &["a"]);
}
