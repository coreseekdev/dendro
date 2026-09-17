//! 文本 IR（dendro.ir v1，spec 09）：标量方言。
//!
//! 三条性质合同：**P1 round-trip**（parse(print(p)) 与 p 全字段结构相等
//! ——侧表重建语义与编译器一致：consts/casts 按出现序追加、binops 按
//! 首见去重）、**P2 确定性**（同一程序恒同字节：无时间/地址、f64 位
//! 模式十六进制、属性固定序）、**P3 人可读**（SSA 名 + 显式类型）。
//!
//! fail-closed（§4）：未知版本/操作/类型 → None/Err，不做尽力解析。
//! verifier 与构造期共用（compile_predicate_named 出口调用）。

use crate::error::{Result, SqlError};
use crate::sql::scalar::{ScalarProgram, ScalarStep, SCALAR_STEP_CAP};
use crate::types::SqlValue;
use sqlparser::ast::{BinaryOperator as BO, DataType as PD};

// ---------------------------------------------------------------------------
// printer
// ---------------------------------------------------------------------------

/// 打印标量程序（`scalar @name { … }` 块；name 仅标识别途，不入程序本体）。
/// 不支持的侧表内容（未知 cast 类型等）→ Err（fail-closed 双向）。
pub fn print_scalar(name: &str, p: &ScalarProgram) -> Result<String> {
    let mut out = String::new();
    out.push_str("dendro.ir v1\n");
    out.push_str(&format!("scalar @{name} {{\n"));
    if !p.col_names.is_empty() {
        let names: Vec<String> = p.col_names.iter().map(|n| json_escape(n)).collect();
        out.push_str(&format!("  cols = [{}]\n", names.join(", ")));
    }
    out.push_str(&format!("  n_cols = {}\n", p.n_cols));
    out.push_str(&format!("  n_regs = {}\n", p.n_regs));
    for s in &p.steps {
        out.push_str("  ");
        out.push_str(&print_step(s, p)?);
        out.push('\n');
    }
    out.push_str("}\n");
    Ok(out)
}

fn reg(r: u16) -> String {
    format!("%r{r}")
}

fn print_step(s: &ScalarStep, p: &ScalarProgram) -> Result<String> {
    use ScalarStep::*;
    Ok(match s {
        Const { dst, c } => {
            let v = p
                .consts
                .get(*c as usize)
                .ok_or_else(|| bad("const 池索引越界"))?;
            format!("{} = const {}", reg(*dst), print_const(v)?)
        }
        Col { dst, idx } => format!("{} = col {idx}", reg(*dst)),
        Param { dst, idx } => format!("{} = param ${}", reg(*dst), idx + 1),
        Cmp { dst, op, a, b } => {
            let o = p
                .binops
                .get(*op as usize)
                .ok_or_else(|| bad("binop 池索引越界"))?;
            format!(
                "{} = cmp.{} {}, {}",
                reg(*dst),
                cmp_name(o)?,
                reg(*a),
                reg(*b)
            )
        }
        Arith { dst, op, a, b } => {
            let o = p
                .binops
                .get(*op as usize)
                .ok_or_else(|| bad("binop 池索引越界"))?;
            format!(
                "{} = arith.{} {}, {}",
                reg(*dst),
                arith_name(o)?,
                reg(*a),
                reg(*b)
            )
        }
        Concat { dst, a, b } => format!("{} = concat {}, {}", reg(*dst), reg(*a), reg(*b)),
        And { dst, a, b } => format!("{} = and {}, {}", reg(*dst), reg(*a), reg(*b)),
        Or { dst, a, b } => format!("{} = or {}, {}", reg(*dst), reg(*a), reg(*b)),
        Not { dst, src } => format!("{} = not {}", reg(*dst), reg(*src)),
        Neg { dst, src } => format!("{} = neg {}", reg(*dst), reg(*src)),
        IsNull { dst, src } => format!("{} = is_null {}", reg(*dst), reg(*src)),
        IsNotNull { dst, src } => format!("{} = is_not_null {}", reg(*dst), reg(*src)),
        IsTrue { dst, src } => format!("{} = is_true {}", reg(*dst), reg(*src)),
        IsFalse { dst, src } => format!("{} = is_false {}", reg(*dst), reg(*src)),
        Cast { dst, src, ty } => {
            let t = p
                .casts
                .get(*ty as usize)
                .ok_or_else(|| bad("cast 池索引越界"))?;
            format!("{} = cast.{} {}", reg(*dst), cast_name(t)?, reg(*src))
        }
        Between {
            dst,
            v,
            lo,
            hi,
            negated,
        } => format!(
            "{} = between {}, {}, {}{}",
            reg(*dst),
            reg(*v),
            reg(*lo),
            reg(*hi),
            if *negated { " neg" } else { "" }
        ),
        InTest { v, item, state } => {
            format!("{} = in.test {}, {}", reg(*state), reg(*v), reg(*item))
        }
        InFinish {
            dst,
            state,
            negated,
        } => format!(
            "{} = in.finish {}{}",
            reg(*dst),
            reg(*state),
            if *negated { " neg" } else { "" }
        ),
        CaseHit { dst, operand, when } => {
            format!("{} = case.hit {}, {}", reg(*dst), reg(*operand), reg(*when))
        }
        Mov { dst, src } => format!("{} = mov {}", reg(*dst), reg(*src)),
        Jump(t) => format!("jump L{t}"),
        JumpIfTrue { reg: r, tgt } => format!("jump_if_true {}, L{tgt}", reg(*r)),
        JumpIfNotTrue { reg: r, tgt } => format!("jump_if_not_true {}, L{tgt}", reg(*r)),
        JumpIfInFound { state, tgt } => {
            format!("jump_if_in_found {}, L{tgt}", reg(*state))
        }
        Qual { src } => format!("qual {}", reg(*src)),
        Out { src } => format!("out {}", reg(*src)),
    })
}

/// 常量打印（§3-2/3-6）：f64 位模式十六进制（NaN/Inf 确定性）、str 走
/// JSON 转义、bytes hex、类型标签全集 i32/i64/f64/bool/str/bytes/
/// date32/timestamp_ms/null
fn print_const(v: &SqlValue) -> Result<String> {
    Ok(match v {
        SqlValue::Null => "null".into(),
        SqlValue::Bool(b) => format!("bool {b}"),
        SqlValue::Int32(i) => format!("i32 {i}"),
        SqlValue::Int64(i) => format!("i64 {i}"),
        SqlValue::Float64(f) => format!("f64 0x{:016x}", f.to_bits()),
        SqlValue::Utf8(s) => format!("str {}", json_escape(s)),
        SqlValue::Bytes(b) => {
            if b.is_empty() {
                "bytes 0x".into()
            } else {
                format!(
                    "bytes 0x{}",
                    b.iter().map(|x| format!("{x:02x}")).collect::<String>()
                )
            }
        }
        SqlValue::Date32(d) => format!("date32 {d}"),
        SqlValue::TimestampMs(t) => format!("timestamp_ms {t}"),
    })
}

fn cmp_name(o: &BO) -> Result<&'static str> {
    Ok(match o {
        BO::Eq => "eq",
        BO::NotEq => "ne",
        BO::Lt => "lt",
        BO::LtEq => "le",
        BO::Gt => "gt",
        BO::GtEq => "ge",
        _ => return Err(bad("非比较 binop 出现在 Cmp 步")),
    })
}

fn arith_name(o: &BO) -> Result<&'static str> {
    Ok(match o {
        BO::Plus => "add",
        BO::Minus => "sub",
        BO::Multiply => "mul",
        BO::Divide => "div",
        BO::Modulo => "mod",
        _ => return Err(bad("非算术 binop 出现在 Arith 步")),
    })
}

fn cast_name(t: &PD) -> Result<&'static str> {
    Ok(match t {
        PD::Int(None) => "int",
        PD::BigInt(None) => "bigint",
        PD::Varchar(None) => "text",
        PD::Double(sqlparser::ast::ExactNumberInfo::None) => "double",
        PD::Boolean => "boolean",
        // 评审 P2：常见可执行 cast 原映射缺失 → print Err → EXPLAIN 退化
        PD::Date => "date",
        PD::Timestamp(None, sqlparser::ast::TimezoneInfo::None) => "timestamp",
        _ => return Err(bad("未知 cast 类型（文本 IR v1 未覆盖）")),
    })
}

fn bad(msg: &str) -> SqlError {
    SqlError::internal(format!("dendro.ir: {msg}"))
}

/// SQL 文本片段的 IR 呈现（JSON 转义包裹——谓词/JOIN ON 的确定性
/// 文本形态；`;` 经 json_escape 内的 \u003b 分支已出注释域）
pub(crate) fn escape_sql_text(s: &str) -> String {
    json_escape(s)
}

/// JSON 字符串转义（§2 约定；含 ASCII 控制字符与引号/反斜杠）
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // `;` 是行注释起始符（strip_comment 不辨引号内外）——转义
            // 出文本域（评审 P1：含分号常量 print 成功 parse 失败）
            ';' => out.push_str("\\u003b"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// parser（fail-closed）
// ---------------------------------------------------------------------------

/// 解析 dendro.ir v1 标量块 → ScalarProgram。任何未知 token/结构 → None。
/// 侧表重建语义与编译器一致（consts/casts 追加、binops 首见去重）⇒
/// P1 全字段结构相等。
pub fn parse_scalar(text: &str) -> Option<ScalarProgram> {
    let mut lines = text
        .lines()
        .map(strip_comment)
        .map(|l| l.trim().to_string());
    let header = lines.next()?;
    if header != "dendro.ir v1" {
        return None; // 未知版本 fail-closed（§4）
    }
    let sig = lines.next()?;
    let name_part = sig.strip_prefix("scalar @")?;
    if !name_part.contains('{') || !sig.ends_with('{') {
        return None;
    }
    let mut p = ScalarProgram::default();
    let mut closed = false;
    // fail-closed 收紧（评审 P2）：属性恰好一次且先于步；闭括号后无内容
    let mut seen_cols = false;
    let mut seen_ncols = false;
    let mut seen_nregs = false;
    for line in lines.by_ref() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "}" {
            closed = true;
            break;
        }
        if closed {
            return None; // 闭括号后仍有内容（嵌套第二块等）
        }
        if let Some(rest) = line.strip_prefix("cols = [") {
            if seen_cols || !p.steps.is_empty() {
                return None; // 重复属性 / 属性行不得在步之后
            }
            seen_cols = true;
            let inner = rest.strip_suffix(']')?;
            if !inner.is_empty() {
                p.col_names = split_json_strings(inner)?;
            }
            continue;
        }
        if let Some(n) = line.strip_prefix("n_cols = ") {
            if seen_ncols || !p.steps.is_empty() {
                return None;
            }
            seen_ncols = true;
            p.n_cols = n.parse().ok()?;
            continue;
        }
        if let Some(n) = line.strip_prefix("n_regs = ") {
            if seen_nregs || !p.steps.is_empty() {
                return None;
            }
            seen_nregs = true;
            p.n_regs = n.parse().ok()?;
            continue;
        }
        let s = parse_step(line, &mut p)?;
        p.steps.push(s);
    }
    if !closed {
        return None;
    }
    // `}` 后遗留非空内容拒绝（break 后未消费的行）
    if lines.any(|l| !l.trim().is_empty()) {
        return None;
    }
    verify(&p).ok()?;
    Some(p)
}

fn strip_comment(l: &str) -> &str {
    match l.find(';') {
        Some(i) => &l[..i],
        None => l,
    }
}

pub(crate) fn split_json_strings(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let item = rest.strip_prefix('"')?;
        let end = find_str_end(item)?;
        out.push(json_unescape(&item[..end])?);
        let tail = item[end + 1..].trim_start();
        if tail.is_empty() {
            break; // 末项（无尾随逗号）
        }
        rest = tail.strip_prefix(',')?.trim_start();
    }
    Some(out)
}

/// 找未转义引号的位置
pub(crate) fn find_str_end(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

pub(crate) fn json_unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'u' => {
                let hex: String = it.by_ref().take(4).collect();
                if hex.len() != 4 {
                    return None; // 短转义 fail-closed（评审 P2）
                }
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// 类型标签长分派链——question_mark 改写（层层 strip_prefix → ?）反而
/// 破坏标签一览性，按例外放行
#[allow(clippy::question_mark)]
fn parse_const(body: &str, p: &mut ScalarProgram) -> Option<u16> {
    let v = if body == "null" {
        SqlValue::Null
    } else if let Some(t) = body.strip_prefix("bool ") {
        SqlValue::Bool(t == "true")
    } else if let Some(t) = body.strip_prefix("i32 ") {
        SqlValue::Int32(t.parse().ok()?)
    } else if let Some(t) = body.strip_prefix("i64 ") {
        SqlValue::Int64(t.parse().ok()?)
    } else if let Some(t) = body.strip_prefix("f64 0x") {
        let bits = u64::from_str_radix(t, 16).ok()?;
        SqlValue::Float64(f64::from_bits(bits))
    } else if let Some(t) = body.strip_prefix("str ") {
        let t = t.strip_prefix('"')?;
        let end = find_str_end(t)?;
        SqlValue::Utf8(json_unescape(&t[..end])?)
    } else if let Some(t) = body.strip_prefix("bytes 0x") {
        if t.is_empty() {
            SqlValue::Bytes(vec![])
        } else {
            let mut b = Vec::with_capacity(t.len() / 2);
            let tb = t.as_bytes();
            let mut i = 0;
            while i + 1 < tb.len() {
                let hi = (tb[i] as char).to_digit(16)?;
                let lo = (tb[i + 1] as char).to_digit(16)?;
                b.push((hi * 16 + lo) as u8);
                i += 2;
            }
            if i != tb.len() {
                return None; // 奇数长度 hex
            }
            SqlValue::Bytes(b)
        }
    } else if let Some(t) = body.strip_prefix("date32 ") {
        SqlValue::Date32(t.parse().ok()?)
    } else if let Some(t) = body.strip_prefix("timestamp_ms ") {
        SqlValue::TimestampMs(t.parse().ok()?)
    } else {
        return None;
    };
    let c = p.consts.len() as u16;
    p.consts.push(v);
    Some(c)
}

fn parse_step(line: &str, p: &mut ScalarProgram) -> Option<ScalarStep> {
    use ScalarStep::*;
    // "%rD = …" 与裸操作两类
    if let Some(rest) = line.strip_prefix("%r") {
        let (dst_s, body) = rest.split_once(" = ")?;
        let dst: u16 = dst_s.parse().ok()?;
        return parse_assign(dst, body.trim(), p);
    }
    // 跳转/终结步
    if let Some(t) = line.strip_prefix("jump L") {
        return Some(Jump(t.parse().ok()?));
    }
    if let Some(t) = line.strip_prefix("jump_if_true %r") {
        let (r, l) = t.split_once(", L")?;
        return Some(JumpIfTrue {
            reg: r.parse().ok()?,
            tgt: l.parse().ok()?,
        });
    }
    if let Some(t) = line.strip_prefix("jump_if_not_true %r") {
        let (r, l) = t.split_once(", L")?;
        return Some(JumpIfNotTrue {
            reg: r.parse().ok()?,
            tgt: l.parse().ok()?,
        });
    }
    if let Some(t) = line.strip_prefix("jump_if_in_found %r") {
        let (r, l) = t.split_once(", L")?;
        return Some(JumpIfInFound {
            state: r.parse().ok()?,
            tgt: l.parse().ok()?,
        });
    }
    if let Some(t) = line.strip_prefix("qual %r") {
        return Some(Qual {
            src: t.parse().ok()?,
        });
    }
    if let Some(t) = line.strip_prefix("out %r") {
        return Some(Out {
            src: t.parse().ok()?,
        });
    }
    None
}

fn parse_assign(dst: u16, body: &str, p: &mut ScalarProgram) -> Option<ScalarStep> {
    use ScalarStep::*;
    let rr = |s: &str| -> Option<u16> { s.strip_prefix("%r")?.parse().ok() };
    // 可选 " neg" 后缀（between / in.finish）
    let (core, neg) = match body.strip_suffix(" neg") {
        Some(c) => (c, true),
        None => (body, false),
    };
    if let Some(t) = core.strip_prefix("const ") {
        let c = parse_const(t, p)?;
        return Some(Const { dst, c });
    }
    if let Some(t) = core.strip_prefix("col ") {
        return Some(Col {
            dst,
            idx: t.parse().ok()?,
        });
    }
    if let Some(t) = core.strip_prefix("param $") {
        let n: u32 = t.parse().ok()?;
        let idx = n.checked_sub(1)?;
        if idx > u16::MAX as u32 {
            return None; // $65536+ 越界（评审 P2）
        }
        return Some(Param {
            dst,
            idx: idx as u16,
        });
    }
    for (prefix, is_cmp) in [("cmp.", true), ("arith.", false)] {
        if let Some(t) = core.strip_prefix(prefix) {
            let (name, args) = t.split_once(' ')?;
            let mut it = args.split(", ");
            let a = rr(it.next()?)?;
            let b = rr(it.next()?)?;
            if it.next().is_some() {
                return None;
            }
            let bo = if is_cmp {
                match name {
                    "eq" => BO::Eq,
                    "ne" => BO::NotEq,
                    "lt" => BO::Lt,
                    "le" => BO::LtEq,
                    "gt" => BO::Gt,
                    "ge" => BO::GtEq,
                    _ => return None,
                }
            } else {
                match name {
                    "add" => BO::Plus,
                    "sub" => BO::Minus,
                    "mul" => BO::Multiply,
                    "div" => BO::Divide,
                    "mod" => BO::Modulo,
                    _ => return None,
                }
            };
            let op = binop_index(p, bo);
            return Some(if is_cmp {
                Cmp { dst, op, a, b }
            } else {
                Arith { dst, op, a, b }
            });
        }
    }
    let two_reg = |t: &str| -> Option<(u16, u16)> {
        let mut it = t.split(", ");
        let a = rr(it.next()?)?;
        let b = rr(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        Some((a, b))
    };
    if let Some(t) = core.strip_prefix("concat ") {
        let (a, b) = two_reg(t)?;
        return Some(Concat { dst, a, b });
    }
    if let Some(t) = core.strip_prefix("and ") {
        let (a, b) = two_reg(t)?;
        return Some(And { dst, a, b });
    }
    if let Some(t) = core.strip_prefix("or ") {
        let (a, b) = two_reg(t)?;
        return Some(Or { dst, a, b });
    }
    for name in [
        "not ",
        "neg ",
        "is_null ",
        "is_not_null ",
        "is_true ",
        "is_false ",
        "mov ",
    ] {
        if let Some(t) = core.strip_prefix(name) {
            let src = rr(t)?;
            return Some(match name {
                "not " => Not { dst, src },
                "neg " => Neg { dst, src },
                "is_null " => IsNull { dst, src },
                "is_not_null " => IsNotNull { dst, src },
                "is_true " => IsTrue { dst, src },
                "is_false " => IsFalse { dst, src },
                _ => Mov { dst, src },
            });
        }
    }
    if let Some(t) = core.strip_prefix("cast.") {
        let (name, arg) = t.split_once(' ')?;
        let src = rr(arg)?;
        let dt = match name {
            "int" => PD::Int(None),
            "bigint" => PD::BigInt(None),
            "text" => PD::Varchar(None),
            "double" => PD::Double(sqlparser::ast::ExactNumberInfo::None),
            "boolean" => PD::Boolean,
            "date" => PD::Date,
            "timestamp" => PD::Timestamp(None, sqlparser::ast::TimezoneInfo::None),
            _ => return None,
        };
        let ty = p.casts.len() as u16;
        p.casts.push(dt);
        return Some(Cast { dst, src, ty });
    }
    if let Some(t) = core.strip_prefix("between ") {
        let mut it = t.split(", ");
        let v = rr(it.next()?)?;
        let lo = rr(it.next()?)?;
        let hi = rr(it.next()?)?;
        if it.next().is_some() {
            return None;
        }
        return Some(Between {
            dst,
            v,
            lo,
            hi,
            negated: neg,
        });
    }
    if let Some(t) = core.strip_prefix("in.test ") {
        let (v, item) = two_reg(t)?;
        return Some(InTest {
            v,
            item,
            state: dst,
        });
    }
    if let Some(t) = core.strip_prefix("in.finish ") {
        let state = rr(t)?;
        return Some(InFinish {
            dst,
            state,
            negated: neg,
        });
    }
    if let Some(t) = core.strip_prefix("case.hit ") {
        let (operand, when) = two_reg(t)?;
        return Some(CaseHit { dst, operand, when });
    }
    None
}

/// binops 侧表首见去重（与编译器 opidx 语义一致——P1 全字段相等的关键）
fn binop_index(p: &mut ScalarProgram, bo: BO) -> u16 {
    if let Some(i) = p.binops.iter().position(|o| *o == bo) {
        i as u16
    } else {
        p.binops.push(bo);
        (p.binops.len() - 1) as u16
    }
}

// ---------------------------------------------------------------------------
// verifier（§4：parse 与构造共用）
// ---------------------------------------------------------------------------

fn jump_ok(t: u32, n_steps: usize) -> Result<()> {
    if (t as usize) >= n_steps {
        return Err(bad(&format!("Jump 目标 L{t} 越界（步数 {n_steps}）")));
    }
    Ok(())
}

/// 结构校验：步数上限、寄存器/列/侧表索引界内、跳转目标界内。
pub fn verify(p: &ScalarProgram) -> Result<()> {
    if p.steps.len() > SCALAR_STEP_CAP {
        return Err(bad("步数超上限"));
    }
    use ScalarStep::*;
    macro_rules! chk {
        ($r:expr, $what:expr) => {
            if (*$r as usize) >= p.n_regs {
                return Err(bad($what));
            }
        };
    }
    for s in p.steps.iter() {
        match s {
            Const { dst, c } => {
                chk!(dst, "Const dst 越界");
                if (*c as usize) >= p.consts.len() {
                    return Err(bad("Const 池索引越界"));
                }
            }
            Col { dst, idx } => {
                chk!(dst, "Col dst 越界");
                if (*idx as usize) >= p.n_cols {
                    return Err(bad(&format!("Col idx {idx} ≥ n_cols {}", p.n_cols)));
                }
            }
            Param { dst, .. } => chk!(dst, "Param dst 越界"),
            Cmp { dst, op, a, b } => {
                chk!(dst, "Cmp dst 越界");
                chk!(a, "Cmp a 越界");
                chk!(b, "Cmp b 越界");
                if (*op as usize) >= p.binops.len() {
                    return Err(bad("binop 池索引越界"));
                }
            }
            Arith { dst, op, a, b } => {
                chk!(dst, "Arith dst 越界");
                chk!(a, "Arith a 越界");
                chk!(b, "Arith b 越界");
                if (*op as usize) >= p.binops.len() {
                    return Err(bad("binop 池索引越界"));
                }
            }
            Concat { dst, a, b } | And { dst, a, b } | Or { dst, a, b } => {
                chk!(dst, "dst 越界");
                chk!(a, "a 越界");
                chk!(b, "b 越界");
            }
            Not { dst, src }
            | Neg { dst, src }
            | IsNull { dst, src }
            | IsNotNull { dst, src }
            | IsTrue { dst, src }
            | IsFalse { dst, src }
            | Mov { dst, src } => {
                chk!(dst, "dst 越界");
                chk!(src, "src 越界");
            }
            Cast { dst, src, ty } => {
                chk!(dst, "dst 越界");
                chk!(src, "src 越界");
                if (*ty as usize) >= p.casts.len() {
                    return Err(bad("cast 池索引越界"));
                }
            }
            Between { dst, v, lo, hi, .. } => {
                chk!(dst, "dst 越界");
                chk!(v, "v 越界");
                chk!(lo, "lo 越界");
                chk!(hi, "hi 越界");
            }
            InTest { v, item, state } => {
                chk!(v, "v 越界");
                chk!(item, "item 越界");
                chk!(state, "state 越界");
            }
            InFinish { dst, state, .. } => {
                chk!(dst, "dst 越界");
                chk!(state, "state 越界");
            }
            CaseHit { dst, operand, when } => {
                chk!(dst, "dst 越界");
                chk!(operand, "operand 越界");
                chk!(when, "when 越界");
            }
            Jump(t) => jump_ok(*t, p.steps.len())?,
            JumpIfTrue { reg, tgt } => {
                chk!(reg, "jump_if_true 条件越界");
                jump_ok(*tgt, p.steps.len())?;
            }
            JumpIfNotTrue { reg, tgt } => {
                chk!(reg, "jump_if_not_true 条件越界");
                jump_ok(*tgt, p.steps.len())?;
            }
            JumpIfInFound { state, tgt } => {
                chk!(state, "jump_if_in_found 条件越界");
                jump_ok(*tgt, p.steps.len())?;
            }
            Qual { src } | Out { src } => chk!(src, "终结步源越界"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::scalar::{compile_predicate_named, ScalarProgram};
    use sqlparser::ast::{BinaryOperator as BO, Expr, Ident};

    fn names2() -> Vec<String> {
        vec!["id".into(), "v".into()]
    }
    fn cols2(n: &str) -> Option<usize> {
        names2().iter().position(|c| c == n)
    }
    fn num(s: &str) -> Expr {
        Expr::Value(sqlparser::ast::ValueWithSpan {
            value: sqlparser::ast::Value::Number(s.into(), false),
            span: sqlparser::tokenizer::Span::empty(),
        })
    }
    fn bin(l: Expr, op: BO, r: Expr) -> Expr {
        Expr::BinaryOp {
            left: Box::new(l),
            op,
            right: Box::new(r),
        }
    }
    fn idn(n: &str) -> Expr {
        Expr::Identifier(Ident::new(n))
    }

    /// P1/P2：编译 → print → parse → 全字段相等 + 字节恒等（全步类语料）
    #[test]
    fn roundtrip_full_field_all_step_kinds() {
        let names = names2();
        let strv = |s: &str| {
            Expr::Value(sqlparser::ast::ValueWithSpan {
                value: sqlparser::ast::Value::SingleQuotedString(s.into()),
                span: sqlparser::tokenizer::Span::empty(),
            })
        };
        let cases: Vec<Expr> = vec![
            bin(idn("v"), BO::Gt, num("10")),
            bin(
                bin(idn("id"), BO::Eq, num("1")),
                BO::And,
                bin(idn("v"), BO::Lt, num("9")),
            ),
            bin(bin(idn("id"), BO::Plus, num("2")), BO::Modulo, num("3")),
            bin(strv("a\"b\\c\n"), BO::StringConcat, idn("v")),
            Expr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr: Box::new(Expr::IsNull(Box::new(idn("v")))),
            },
            Expr::InList {
                expr: Box::new(idn("id")),
                list: vec![num("1"), num("2")],
                negated: true,
            },
            Expr::Between {
                expr: Box::new(idn("v")),
                negated: true,
                low: Box::new(num("1")),
                high: Box::new(num("9")),
            },
            Expr::Case {
                case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                operand: Some(Box::new(idn("id"))),
                conditions: vec![
                    sqlparser::ast::CaseWhen {
                        condition: num("1"),
                        result: num("10"),
                    },
                    sqlparser::ast::CaseWhen {
                        condition: num("2"),
                        result: num("20"),
                    },
                ],
                else_result: Some(Box::new(num("0"))),
            },
            Expr::Cast {
                kind: sqlparser::ast::CastKind::Cast,
                expr: Box::new(idn("v")),
                data_type: sqlparser::ast::DataType::Int(None),
                format: None,
                array: false,
            },
            // param 引用
            bin(idn("v"), BO::Gt, {
                Expr::Value(sqlparser::ast::ValueWithSpan {
                    value: sqlparser::ast::Value::Placeholder("$1".into()),
                    span: sqlparser::tokenizer::Span::empty(),
                })
            }),
        ];
        for e in &cases {
            let cp = compile_predicate_named(e, &cols2, names.len(), &names).unwrap();
            let text = print_scalar("pred", &cp.prog).unwrap();
            let p2 = parse_scalar(&text).unwrap_or_else(|| panic!("parse 失败：{text}"));
            // P2：同一程序 print 两次字节恒等
            assert_eq!(print_scalar("pred", &cp.prog).unwrap(), text);
            // P1：全字段结构相等
            assert_eq!(cp.prog.steps, p2.steps, "{text}");
            assert_eq!(cp.prog.consts, p2.consts, "{text}");
            assert_eq!(cp.prog.binops, p2.binops, "{text}");
            assert_eq!(cp.prog.casts, p2.casts, "{text}");
            assert_eq!(cp.prog.n_regs, p2.n_regs);
            assert_eq!(cp.prog.n_cols, p2.n_cols);
            assert_eq!(cp.prog.col_names, p2.col_names);
            // 再 print 仍恒等（idempotence）
            assert_eq!(print_scalar("pred", &p2).unwrap(), text);
        }
    }

    /// 常量类型全集（§3-6）round-trip：i32/i64/f64(位模式)/bool/str/bytes/
    /// date32/timestamp_ms/null
    #[test]
    fn const_type_full_set() {
        let p = ScalarProgram {
            steps: vec![
                ScalarStep::Const { dst: 0, c: 0 },
                ScalarStep::Const { dst: 1, c: 1 },
                ScalarStep::Const { dst: 2, c: 2 },
                ScalarStep::Const { dst: 3, c: 3 },
                ScalarStep::Const { dst: 4, c: 4 },
                ScalarStep::Const { dst: 5, c: 5 },
                ScalarStep::Const { dst: 6, c: 6 },
                ScalarStep::Const { dst: 7, c: 7 },
                ScalarStep::Const { dst: 8, c: 8 },
                ScalarStep::Qual { src: 0 },
            ],
            consts: vec![
                SqlValue::Null,
                SqlValue::Bool(true),
                SqlValue::Int32(-7),
                SqlValue::Int64(i64::MIN),
                SqlValue::Float64(f64::NAN),
                SqlValue::Utf8("引号\"反斜\\换行\n\t控\u{1}制".into()),
                SqlValue::Bytes(vec![0x00, 0xff, 0x10]),
                SqlValue::Date32(19700),
                SqlValue::TimestampMs(1726400000123),
            ],
            binops: vec![],
            casts: vec![],
            n_regs: 9,
            n_cols: 1,
            col_names: vec![],
        };
        let text = print_scalar("k", &p).unwrap();
        // NaN 必须位模式（P2：禁十进制）
        assert!(text.contains("f64 0x7ff8000000000000"), "{text}");
        let p2 = parse_scalar(&text).unwrap();
        // NaN 位等（f64 PartialEq 下 NaN≠NaN——按位比较）
        for (a, b) in p.consts.iter().zip(&p2.consts) {
            let eq = match (a, b) {
                (SqlValue::Float64(x), SqlValue::Float64(y)) => x.to_bits() == y.to_bits(),
                (x, y) => x == y,
            };
            assert!(eq, "{a:?} ≠ {b:?}：{text}");
        }
        assert_eq!(p.steps, p2.steps);
        assert_eq!(print_scalar("k", &p2).unwrap(), text, "idempotence");
    }

    /// fail-closed（§4）：未知版本 / 未知操作 / 坏结构
    #[test]
    fn fail_closed() {
        let good =
            "dendro.ir v1\nscalar @p {\n  n_cols = 1\n  n_regs = 1\n  %r0 = col 0\n  qual %r0\n}\n";
        assert!(parse_scalar(good).is_some());
        assert!(
            parse_scalar(&good.replace("v1", "v2")).is_none(),
            "未知版本"
        );
        assert!(
            parse_scalar(&good.replace("%r0 = col 0", "%r0 = frob 0")).is_none(),
            "未知操作"
        );
        assert!(parse_scalar(&good.replace("}\n", "")).is_none(), "未闭合");
        assert!(
            parse_scalar(&good.replace("n_cols = 1", "n_cols = 0")).is_none(),
            "verifier：col 越界必须拒"
        );
        assert!(
            parse_scalar(&good.replace("qual %r0", "jump L9")).is_none(),
            "verifier：跳转越界必须拒"
        );
        assert!(
            parse_scalar(&good.replace("n_regs = 1", "n_regs = 0")).is_none(),
            "verifier：寄存器越界必须拒"
        );
    }

    /// 评审修复回归：含 `;` 的字符串/列名 round-trip（原 strip_comment
    /// 引号内截断 → parse 失败）
    #[test]
    fn semicolon_in_strings_roundtrips() {
        let names = vec!["a;b".to_string(), "v".to_string()];
        let cols = |n: &str| names.iter().position(|c| c == n);
        let strv = |t: &str| {
            Expr::Value(sqlparser::ast::ValueWithSpan {
                value: sqlparser::ast::Value::SingleQuotedString(t.into()),
                span: sqlparser::tokenizer::Span::empty(),
            })
        };
        // 谓词常量含分号 + 注释样式内容
        let e = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("v"))),
            op: BO::Eq,
            right: Box::new(strv("x;y ; z")),
        };
        let cp = crate::sql::scalar::compile_predicate_named(&e, &cols, 2, &names).unwrap();
        let text = print_scalar("pred", &cp.prog).unwrap();
        assert!(text.contains("\\u003b"), "分号必须转义出文本域：{text}");
        let p2 = parse_scalar(&text).unwrap();
        assert_eq!(cp.prog.steps, p2.steps);
        assert_eq!(cp.prog.consts, p2.consts);
        assert_eq!(p2.col_names, names);
    }

    /// 评审修复回归：常量折叠截 consts 池（原孤儿池项破坏全字段相等）
    #[test]
    fn fold_truncates_const_pool() {
        let names = vec!["id".to_string(), "v".to_string()];
        let cols = |n: &str| names.iter().position(|c| c == n);
        // -5 = UnaryOp Minus(5) → 折叠后池应只含 [-5]（原 [5, -5]）
        let e = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("v"))),
            op: BO::Gt,
            right: Box::new(Expr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Minus,
                expr: Box::new(num("5")),
            }),
        };
        let cp = crate::sql::scalar::compile_predicate_named(&e, &cols, 2, &names).unwrap();
        assert_eq!(
            cp.prog.consts.len(),
            1,
            "折叠后池不得留孤儿：{:?}",
            cp.prog.consts
        );
        let text = print_scalar("pred", &cp.prog).unwrap();
        let p2 = parse_scalar(&text).unwrap();
        assert_eq!(cp.prog.steps, p2.steps);
        assert_eq!(cp.prog.consts, p2.consts);
    }

    /// 评审修复回归：fail-closed 收紧（闭括号后内容/重复属性/属性后置）
    #[test]
    fn fail_closed_structure_tightened() {
        let good =
            "dendro.ir v1\nscalar @p {\n  n_cols = 1\n  n_regs = 1\n  %r0 = col 0\n  qual %r0\n}\n";
        assert!(parse_scalar(good).is_some());
        // 闭括号后多余内容
        assert!(parse_scalar(&format!("{good}scalar @x {{\n}}\n")).is_none());
        // 重复 n_cols
        let dup = good.replace("n_regs = 1", "n_cols = 1\n  n_regs = 1");
        assert!(parse_scalar(&dup).is_none());
        // 属性出现在步之后
        let late = good.replace("qual %r0", "qual %r0\n  n_cols = 9");
        assert!(parse_scalar(&late).is_none());
        // 短 \u 转义
        let bad_u = good.replace("col 0", "const str \"a\\u0\"");
        assert!(parse_scalar(&bad_u).is_none());
    }

    /// 评审修复回归：date/timestamp cast 的 print/parse（原映射缺失）
    #[test]
    fn cast_date_timestamp_prints() {
        let names = vec!["id".to_string(), "v".to_string()];
        let cols = |n: &str| names.iter().position(|c| c == n);
        for dt in [
            sqlparser::ast::DataType::Date,
            sqlparser::ast::DataType::Timestamp(None, sqlparser::ast::TimezoneInfo::None),
        ] {
            let e = Expr::Cast {
                kind: sqlparser::ast::CastKind::Cast,
                expr: Box::new(Expr::Identifier(Ident::new("v"))),
                data_type: dt.clone(),
                format: None,
                array: false,
            };
            let cp = crate::sql::scalar::compile_predicate_named(&e, &cols, 2, &names).unwrap();
            let text = print_scalar("pred", &cp.prog).unwrap();
            let p2 = parse_scalar(&text).unwrap();
            assert_eq!(cp.prog.steps, p2.steps, "{text}");
            assert_eq!(cp.prog.casts, p2.casts, "{text}");
        }
    }

    /// verifier 负测试（构造侧；§4 共用）
    #[test]
    fn verify_negatives() {
        let mut p = ScalarProgram {
            steps: vec![
                ScalarStep::Col { dst: 0, idx: 0 },
                ScalarStep::Qual { src: 0 },
            ],
            consts: vec![],
            binops: vec![],
            casts: vec![],
            n_regs: 1,
            n_cols: 1,
            col_names: vec![],
        };
        assert!(verify(&p).is_ok());
        p.n_cols = 0;
        assert!(verify(&p).is_err(), "col 越界");
        p.n_cols = 1;
        p.steps.insert(0, ScalarStep::Jump(99));
        assert!(verify(&p).is_err(), "跳转越界");
    }
}
