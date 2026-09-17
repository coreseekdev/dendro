//! golden 文件测试（spec 09 §6）：谓词语料 dendro.ir v1 文本入库
//! （tests/golden/predicates.ir）——语义改动的 PR 中 diff 即计划变更审阅面。
//! 更新纪律：语义改动必须重生成 golden 并人工审阅 diff。
//! 再生成：`UPDATE_GOLDEN=1 cargo test -p dendro-core --test ir_golden`。

use dendro_core::ir::text::print_scalar;
use dendro_core::sql::scalar::compile_predicate_named;
use sqlparser::ast::{BinaryOperator as BO, Expr, Ident};

fn names() -> Vec<String> {
    vec!["id".into(), "v".into()]
}
fn cols(n: &str) -> Option<usize> {
    names().iter().position(|c| c == n)
}
fn num(s: &str) -> Expr {
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value: sqlparser::ast::Value::Number(s.into(), false),
        span: sqlparser::tokenizer::Span::empty(),
    })
}
fn strv(s: &str) -> Expr {
    Expr::Value(sqlparser::ast::ValueWithSpan {
        value: sqlparser::ast::Value::SingleQuotedString(s.into()),
        span: sqlparser::tokenizer::Span::empty(),
    })
}
fn idn(n: &str) -> Expr {
    Expr::Identifier(Ident::new(n))
}
fn bin(l: Expr, op: BO, r: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(l),
        op,
        right: Box::new(r),
    }
}

/// 语料（固定顺序——P2）：每个谓词一节，`# name` 注释分隔
fn corpus() -> Vec<(&'static str, Expr)> {
    vec![
        ("gt_const", bin(idn("v"), BO::Gt, num("10"))),
        (
            "eq_and_lt",
            bin(
                bin(idn("id"), BO::Eq, num("1")),
                BO::And,
                bin(idn("v"), BO::Lt, num("9")),
            ),
        ),
        (
            "arith_mod",
            bin(bin(idn("id"), BO::Plus, num("2")), BO::Modulo, num("3")),
        ),
        ("concat_str", bin(strv("x"), BO::StringConcat, idn("v"))),
        (
            "not_is_null",
            Expr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr: Box::new(Expr::IsNull(Box::new(idn("v")))),
            },
        ),
        (
            "not_in_list",
            Expr::InList {
                expr: Box::new(idn("id")),
                list: vec![num("1"), num("2"), num("3")],
                negated: true,
            },
        ),
        (
            "between_neg",
            Expr::Between {
                expr: Box::new(idn("v")),
                negated: true,
                low: Box::new(num("1")),
                high: Box::new(num("9")),
            },
        ),
        (
            "case_operand",
            Expr::Case {
                case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                operand: Some(Box::new(idn("id"))),
                conditions: vec![sqlparser::ast::CaseWhen {
                    condition: num("1"),
                    result: num("10"),
                }],
                else_result: Some(Box::new(num("0"))),
            },
        ),
        (
            "cast_int",
            Expr::Cast {
                kind: sqlparser::ast::CastKind::Cast,
                expr: Box::new(idn("v")),
                data_type: sqlparser::ast::DataType::Int(None),
                format: None,
                array: false,
            },
        ),
    ]
}

fn render() -> String {
    let names = names();
    let mut out = String::new();
    out.push_str("; dendro.ir v1 golden（生成见 ir_golden.rs；人工审阅后提交）\n");
    for (name, e) in corpus() {
        let cp = compile_predicate_named(&e, &cols, names.len(), &names).unwrap();
        out.push_str(&format!("; corpus: {name}\n"));
        out.push_str(&print_scalar("pred", &cp.prog).unwrap());
    }
    out
}

#[test]
fn golden_predicates_ir() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/predicates.ir");
    let cur = render();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(path, &cur).unwrap();
        return;
    }
    let want = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("golden 缺失（UPDATE_GOLDEN=1 再生成）：{e}"));
    assert_eq!(want, cur, "golden 偏移——语义变更需重生成并人工审阅 diff");
}
