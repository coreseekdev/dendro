//! v2b B1：标量层 ScalarStep 步列表（ir-spec 03 号 + 附录 A 行为基准）。
//!
//! 合同：
//! - **叶语义零改动**：一切运算调用 expr.rs 既有 pub 助手（binop/arith/
//!   cmp_values/as_bool/as_i64/as_f64/cast_value）——步进只改变控制流
//!   与绑定方式。附录 A（03a）是对拍基准，与代码冲突以代码为准。
//! - **eager null-first 复刻**：AND/OR/比较先求两侧再判 NULL（附录 A
//!   Q17），**无任何求值短路**；`false AND 1/0=1` 必须照常报错。
//! - **compile-or-fallback**：编译器遇不支持的形态返回 Err（含
//!   TryCast——它吞内层求值错误，寄存器机无法事后吞错，留给 AST 路径）。
//!   调用方（B2）对编译失败的谓词/表达式整体回落现行 eval——迁移安全。
//! - 指令大负载（常量/操作符/类型）进侧表，步只存索引（步尺寸纪律）。
//! - NULL 边界语义表见 03 §5.5：`Qual`=谓词终结（NULL→false 丢行）；
//!   `JumpIfNotTrue`=searched-CASE 跳转（NULL→报错，附录 A 怪癖）；
//!   `JumpIfTrue`=simple-CASE 跳转（NULL→不命中继续）。

use crate::error::{Result, SqlError};
use crate::types::SqlValue;
use sqlparser::ast::Value as PV;
use sqlparser::ast::{BinaryOperator as BO, DataType as PD, Expr};

/// 步：大负载进侧表（consts/binops/casts），步内只存索引——全部变体
/// ≤ 10 字节量级（03 §3 步尺寸纪律）。
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarStep {
    /// consts[i] → dst
    Const {
        dst: u16,
        c: u16,
    },
    /// row[i] → dst（越界 = 内部错误，非 panic）
    Col {
        dst: u16,
        idx: u16,
    },
    /// params[i] → dst；未绑定 = 08P01（与 expr.rs:13-15 同码同义）
    Param {
        dst: u16,
        idx: u16,
    },
    /// binops[op](regs[a], regs[b]) → dst：比较族（binop 的 null-first
    /// 与 cmp_values 语义原样）
    Cmp {
        dst: u16,
        op: u16,
        a: u16,
        b: u16,
    },
    /// 算术族（+ - * / %，含类型提升与除零错误）
    Arith {
        dst: u16,
        op: u16,
        a: u16,
        b: u16,
    },
    /// ||（StringConcat：to_text 拼接，null-first）
    Concat {
        dst: u16,
        a: u16,
        b: u16,
    },
    /// AND（eager：两侧已求值；null-first；as_bool）
    And {
        dst: u16,
        a: u16,
        b: u16,
    },
    /// OR（同上）
    Or {
        dst: u16,
        a: u16,
        b: u16,
    },
    /// NOT：as_bool 语义（Null → internal error，附录 A Q16）
    Not {
        dst: u16,
        src: u16,
    },
    /// 一元负号：Int32/Int64/Float64 取负，Null→Null，其余 Err（expr.rs:42-48）
    Neg {
        dst: u16,
        src: u16,
    },
    IsNull {
        dst: u16,
        src: u16,
    },
    IsNotNull {
        dst: u16,
        src: u16,
    },
    /// IsTrue：matches!(as_bool(v), Ok(true))——as_bool 错误吞为 false
    IsTrue {
        dst: u16,
        src: u16,
    },
    IsFalse {
        dst: u16,
        src: u16,
    },
    /// casts[i] 普通转换（错误传播；TryCast 不编译，见合同）
    Cast {
        dst: u16,
        src: u16,
        ty: u16,
    },
    /// Between 合成步：三操作数已**全部求值**（eager，错误先传播），
    /// 任一 Null → Null；否则 lo≤v≤hi（附录 A：不得短路 desugar）
    Between {
        dst: u16,
        v: u16,
        lo: u16,
        hi: u16,
        negated: bool,
    },
    /// InList 单项测试：Null 任一 → state=1（null_seen，继续）；
    /// 相等 → state=2（found，吸收）；否则保持。**求值次序 = 表项序**，
    /// found 后由 JumpIfInFound 跳过剩余项（expr.rs:95-97 的 break 复刻）
    InTest {
        v: u16,
        item: u16,
        state: u16,
    },
    /// InList 终结：2→true；1→Null；0→false；再 `!= negated`
    InFinish {
        dst: u16,
        state: u16,
        negated: bool,
    },
    /// simple CASE 命中测试：任一 Null → Bool(false)（不命中继续，无错）；
    /// 否则 cmp_values(operand, when)==Eq（恒等比较，无 binop 语义——
    /// B4 实证修正：曾名 op 造成"寄存器 vs binop 索引"二义）
    CaseHit {
        dst: u16,
        operand: u16,
        when: u16,
    },
    Mov {
        dst: u16,
        src: u16,
    },
    Jump(u32),
    /// Bool(true) → 跳；Bool(false)/Null → 顺序（simple CASE / 通用真跳）
    JumpIfTrue {
        reg: u16,
        tgt: u32,
    },
    /// Bool(false) → 跳；Bool(true) → 顺序；**Null → internal error**
    /// （searched CASE 的 as_bool(Null) 怪癖复刻，附录 A Q2）
    JumpIfNotTrue {
        reg: u16,
        tgt: u32,
    },
    /// InList state==2（found）→ 跳
    JumpIfInFound {
        state: u16,
        tgt: u32,
    },
    /// 谓词终结：Bool(true)→true；其余（含 Null）→false——EEOP_QUAL 同构
    Qual {
        src: u16,
    },
    /// 投影输出：regs[src] → out
    Out {
        src: u16,
    },
}

/// 编译产物：步列表 + 三个侧表 + 寄存器数（03 §3：编译期定长，
/// verifier/未来文本 IR 消费）。
#[derive(Debug, Clone, Default)]
pub struct ScalarProgram {
    pub steps: Vec<ScalarStep>,
    pub consts: Vec<SqlValue>,
    pub binops: Vec<BO>,
    pub casts: Vec<PD>,
    pub n_regs: usize,
    pub n_cols: usize,
    /// 列名侧表（B4）：Col 步 idx → 名字（反汇编/文本 IR 可读性）。
    /// 由调用方（scan.rs 传 tv.names）提供；空 = 未知（反汇编退化为索引）。
    pub col_names: Vec<String>,
}

/// 步数上限（B5；06 §4 单条目尺寸上界的步数形态）
pub const SCALAR_STEP_CAP: usize = 1024;

pub struct CompiledPredicate {
    pub prog: ScalarProgram,
}

/// 编译谓词（终结步 = Qual）。`cols` 是名字→列偏移解析器（绑定层提供）。
pub fn compile_predicate(
    e: &Expr,
    cols: &dyn Fn(&str) -> Option<usize>,
    n_cols: usize,
) -> Result<CompiledPredicate> {
    compile_predicate_named(e, cols, n_cols, &[])
}

/// 同上，携带列名侧表（反汇编用）。
pub fn compile_predicate_named(
    e: &Expr,
    cols: &dyn Fn(&str) -> Option<usize>,
    n_cols: usize,
    names: &[String],
) -> Result<CompiledPredicate> {
    let mut c = Ctx::new(n_cols);
    c.prog.col_names = names.to_vec();
    let r = c.expr(e, cols)?;
    c.emit(ScalarStep::Qual { src: r });
    // B5（S-3/06 §4）：步数硬断——防恶意巨型表达式撑爆解释循环与
    // （将来的）程序缓存；超限拒绝编译（调用方回落 AST 路径，行为可接受
    // 而非无界）
    if c.prog.steps.len() > SCALAR_STEP_CAP {
        return Err(SqlError::not_supported(format!(
            "expression too large ({} steps > {SCALAR_STEP_CAP})",
            c.prog.steps.len()
        )));
    }
    Ok(CompiledPredicate { prog: c.prog })
}

/// 编译投影表达式（终结步 = Out）
pub fn compile_expr(
    e: &Expr,
    cols: &dyn Fn(&str) -> Option<usize>,
    n_cols: usize,
) -> Result<ScalarProgram> {
    let mut c = Ctx::new(n_cols);
    let r = c.expr(e, cols)?;
    c.emit(ScalarStep::Out { src: r });
    Ok(c.prog)
}

struct Ctx {
    prog: ScalarProgram,
    /// 编译期常量寄存器（折叠用）：reg idx → 值
    const_regs: std::collections::HashMap<u16, SqlValue>,
    next_reg: u16,
    depth: u32,
}

/// 递归深度上限（B5 实证：深链编译先于步数上限爆栈——SIGABRT）。
/// 128 深 ≫ 任何合法用户表达式；超限 = 恶意/失控生成，编译期拒绝。
const SCALAR_COMPILE_DEPTH: u32 = 128;

impl Ctx {
    fn new(n_cols: usize) -> Self {
        Self {
            prog: ScalarProgram {
                n_cols,
                ..Default::default()
            },
            const_regs: Default::default(),
            next_reg: 0,
            depth: 0,
        }
    }
    fn reg(&mut self) -> u16 {
        let r = self.next_reg;
        self.next_reg += 1;
        self.prog.n_regs = self.next_reg as usize;
        r
    }
    fn emit(&mut self, s: ScalarStep) {
        self.prog.steps.push(s);
    }
    fn konst(&mut self, v: SqlValue) -> u16 {
        let dst = self.reg();
        let c = self.prog.consts.len() as u16;
        self.prog.consts.push(v.clone());
        self.emit(ScalarStep::Const { dst, c });
        self.const_regs.insert(dst, v);
        dst
    }
    fn opidx(&mut self, op: &BO) -> u16 {
        if let Some(i) = self.prog.binops.iter().position(|o| o == op) {
            i as u16
        } else {
            self.prog.binops.push(op.clone());
            (self.prog.binops.len() - 1) as u16
        }
    }

    /// 编译表达式 → 寄存器。子表达式步序 = 求值序（eager 复刻的根基）。
    fn expr(&mut self, e: &Expr, cols: &dyn Fn(&str) -> Option<usize>) -> Result<u16> {
        self.depth += 1;
        if self.depth > SCALAR_COMPILE_DEPTH {
            return Err(SqlError::not_supported(
                "expression nesting too deep (>128)",
            ));
        }
        let r = self.expr_inner(e, cols);
        self.depth -= 1;
        r
    }

    fn expr_inner(&mut self, e: &Expr, cols: &dyn Fn(&str) -> Option<usize>) -> Result<u16> {
        match e {
            Expr::Value(v) => {
                if let PV::Placeholder(id) = &v.value {
                    // 数字占位符 $n → Param；命名占位符编译期即 08P01
                    let n: Option<u16> = id.strip_prefix('$').and_then(|s| s.parse().ok());
                    return match n {
                        Some(idx) => {
                            let dst = self.reg();
                            self.emit(ScalarStep::Param { dst, idx });
                            Ok(dst)
                        }
                        None => Err(SqlError::new("08P01", format!("unbound parameter {id}"))),
                    };
                }
                Ok(self.konst(crate::sql::expr::value_from_parser(v.value.clone())))
            }
            Expr::Identifier(id) => self.col(&id.value, cols),
            Expr::CompoundIdentifier(parts) => {
                let name = parts.last().map(|p| p.value.clone()).unwrap_or_default();
                self.col(&name, cols)
            }
            Expr::Nested(inner) => self.expr(inner, cols),
            Expr::UnaryOp { op, expr } => {
                use sqlparser::ast::UnaryOperator as UO;
                match op {
                    UO::Plus => self.expr(expr, cols),
                    UO::Not | UO::Minus => {
                        let mark = self.prog.steps.len();
                        let src = self.expr(expr, cols)?;
                        // 常量折叠（R3）：仅当子式是编译期常量且运算**成功**——
                        // 折叠失败（如 Not(Null) 报错）保留原步，运行期同错。
                        // 成功则截断子式死码（子树恒全 Const 步，无跳转，安全）。
                        if let Some(v) = self.const_regs.get(&src).cloned() {
                            let folded = match op {
                                UO::Not => {
                                    crate::sql::expr::as_bool(&v).map(|b| SqlValue::Bool(!b))
                                }
                                _ => negate(v),
                            };
                            if let Ok(fv) = folded {
                                self.prog.steps.truncate(mark);
                                return Ok(self.konst(fv));
                            }
                        }
                        let dst = self.reg();
                        self.emit(if matches!(op, UO::Not) {
                            ScalarStep::Not { dst, src }
                        } else {
                            ScalarStep::Neg { dst, src }
                        });
                        Ok(dst)
                    }
                    _ => Err(SqlError::not_supported("unary op")),
                }
            }
            Expr::BinaryOp { left, op, right } => {
                let mark = self.prog.steps.len();
                let a = self.expr(left, cols)?;
                let b = self.expr(right, cols)?;
                // 常量折叠（03 §2：编译期一次；仅折叠**无错**运算——
                // `false AND 1/0` 类错误保留到运行期，eager 红线）。成功则
                // 截断子式死码（两侧子树恒全 Const 步，无跳转，安全）。
                if let (Some(l), Some(r)) = (
                    self.const_regs.get(&a).cloned(),
                    self.const_regs.get(&b).cloned(),
                ) {
                    if let Ok(v) = crate::sql::expr::binop(op.clone(), l, r) {
                        self.prog.steps.truncate(mark);
                        return Ok(self.konst(v));
                    }
                }
                let dst = self.reg();
                let opi = self.opidx(op);
                let s = match op {
                    BO::Eq | BO::NotEq | BO::Lt | BO::LtEq | BO::Gt | BO::GtEq => {
                        ScalarStep::Cmp { dst, op: opi, a, b }
                    }
                    BO::And => ScalarStep::And { dst, a, b },
                    BO::Or => ScalarStep::Or { dst, a, b },
                    BO::Plus | BO::Minus | BO::Multiply | BO::Divide | BO::Modulo => {
                        ScalarStep::Arith { dst, op: opi, a, b }
                    }
                    BO::StringConcat => ScalarStep::Concat { dst, a, b },
                    _ => return Err(SqlError::not_supported(format!("operator {op}"))),
                };
                self.emit(s);
                Ok(dst)
            }
            Expr::IsNull(inner) => self.is_test(inner, cols, IsKind::Null),
            Expr::IsNotNull(inner) => self.is_test(inner, cols, IsKind::NotNull),
            Expr::IsTrue(inner) => self.is_test(inner, cols, IsKind::True),
            Expr::IsFalse(inner) => self.is_test(inner, cols, IsKind::False),
            Expr::Cast {
                kind,
                expr,
                data_type,
                ..
            } => {
                match kind {
                    sqlparser::ast::CastKind::TryCast | sqlparser::ast::CastKind::SafeCast => {
                        // 吞内层求值错误 → 寄存器机无法事后吞（合同），回落 AST 路径
                        Err(SqlError::not_supported("try_cast: fallback to AST path"))
                    }
                    _ => {
                        let src = self.expr(expr, cols)?;
                        let ty = self.prog.casts.len() as u16;
                        self.prog.casts.push(data_type.clone());
                        let dst = self.reg();
                        self.emit(ScalarStep::Cast { dst, src, ty });
                        Ok(dst)
                    }
                }
            }
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                // 求值序 = v, low, high（expr.rs:115-117）；全部求值后才判 Null
                let v = self.expr(expr, cols)?;
                let lo = self.expr(low, cols)?;
                let hi = self.expr(high, cols)?;
                let dst = self.reg();
                self.emit(ScalarStep::Between {
                    dst,
                    v,
                    lo,
                    hi,
                    negated: *negated,
                });
                Ok(dst)
            }
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let v = self.expr(expr, cols)?;
                let state = self.konst(SqlValue::Int32(0)); // 0=none 1=null 2=found
                let end = self.prog.steps.len() as u32; // 占位：回填见下
                let _ = end;
                // found 跳点在全部项之后；先记位置，逐项发射
                let mut jump_slots: Vec<usize> = Vec::new();
                for item in list {
                    let it = self.expr(item, cols)?;
                    self.emit(ScalarStep::InTest { v, item: it, state });
                    let slot = self.prog.steps.len();
                    self.emit(ScalarStep::JumpIfInFound {
                        state,
                        tgt: 0, // 回填
                    });
                    jump_slots.push(slot);
                }
                let dst = self.reg();
                // found 跳转目标 = InFinish 步**本身**（差分测试实证：跳到
                // 其后会跳过终结步，dst 残留 Null → true 变 false）
                let after = self.prog.steps.len() as u32;
                self.emit(ScalarStep::InFinish {
                    dst,
                    state,
                    negated: *negated,
                });
                for slot in jump_slots {
                    if let ScalarStep::JumpIfInFound { tgt, .. } = &mut self.prog.steps[slot] {
                        *tgt = after;
                    }
                }
                Ok(dst)
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                let out = self.reg();
                let op_reg = match operand {
                    Some(o) => Some(self.expr(o, cols)?),
                    None => None,
                };
                let mut end_slots: Vec<usize> = Vec::new();
                for cw in conditions {
                    let r = self.expr(&cw.condition, cols)?;
                    match op_reg {
                        Some(opr) => {
                            let hit = self.reg();
                            self.emit(ScalarStep::CaseHit {
                                dst: hit,
                                operand: opr,
                                when: r,
                            });
                            let skip = self.prog.steps.len();
                            self.emit(ScalarStep::JumpIfTrue { reg: hit, tgt: 0 });
                            // 命中 → 求值结果 → out → 跳 end（不命中跳点在
                            // 结果体之后回填为下一分支起点）
                            let res = self.expr(&cw.result, cols)?;
                            self.emit(ScalarStep::Mov { dst: out, src: res });
                            let j = self.prog.steps.len();
                            self.emit(ScalarStep::Jump(0));
                            end_slots.push(j);
                            // 回填"不命中跳到此处"（下一分支起点）
                            let here = self.prog.steps.len() as u32;
                            if let ScalarStep::JumpIfTrue { tgt, .. } = &mut self.prog.steps[skip] {
                                *tgt = here;
                            }
                        }
                        None => {
                            // searched：Null 条件 = internal error（JumpIfNotTrue 语义）
                            let skip = self.prog.steps.len();
                            self.emit(ScalarStep::JumpIfNotTrue { reg: r, tgt: 0 });
                            let res = self.expr(&cw.result, cols)?;
                            self.emit(ScalarStep::Mov { dst: out, src: res });
                            let j = self.prog.steps.len();
                            self.emit(ScalarStep::Jump(0));
                            end_slots.push(j);
                            let here = self.prog.steps.len() as u32;
                            if let ScalarStep::JumpIfNotTrue { tgt, .. } =
                                &mut self.prog.steps[skip]
                            {
                                *tgt = here;
                            }
                        }
                    }
                }
                // else / 默认 Null
                match else_result {
                    Some(e) => {
                        let res = self.expr(e, cols)?;
                        self.emit(ScalarStep::Mov { dst: out, src: res });
                    }
                    None => {
                        let n = self.konst(SqlValue::Null);
                        self.emit(ScalarStep::Mov { dst: out, src: n });
                    }
                }
                let after = self.prog.steps.len() as u32;
                for j in end_slots {
                    if let ScalarStep::Jump(t) = &mut self.prog.steps[j] {
                        *t = after;
                    }
                }
                Ok(out)
            }
            other => {
                let t = other.to_string();
                let t: String = t.chars().take(60).collect();
                Err(SqlError::not_supported(format!("scalar compile: {t}")))
            }
        }
    }

    fn col(&mut self, name: &str, cols: &dyn Fn(&str) -> Option<usize>) -> Result<u16> {
        match cols(name) {
            Some(i) => {
                let dst = self.reg();
                self.emit(ScalarStep::Col { dst, idx: i as u16 });
                Ok(dst)
            }
            None => Err(SqlError::undefined_column(format!(
                "column \"{name}\" does not exist"
            ))),
        }
    }

    fn is_test(
        &mut self,
        inner: &Expr,
        cols: &dyn Fn(&str) -> Option<usize>,
        kind: IsKind,
    ) -> Result<u16> {
        let src = self.expr(inner, cols)?;
        let dst = self.reg();
        self.emit(match kind {
            IsKind::Null => ScalarStep::IsNull { dst, src },
            IsKind::NotNull => ScalarStep::IsNotNull { dst, src },
            IsKind::True => ScalarStep::IsTrue { dst, src },
            IsKind::False => ScalarStep::IsFalse { dst, src },
        });
        Ok(dst)
    }
}

enum IsKind {
    Null,
    NotNull,
    True,
    False,
}

fn negate(v: SqlValue) -> Result<SqlValue> {
    Ok(match v {
        SqlValue::Int32(i) => SqlValue::Int32(-i),
        SqlValue::Int64(i) => SqlValue::Int64(-i),
        SqlValue::Float64(f) => SqlValue::Float64(-f),
        SqlValue::Null => SqlValue::Null,
        _ => return Err(SqlError::datatype_mismatch("cannot negate")),
    })
}

/// eval_row：TP 行后端（03 §3）。寄存器文件 = 定长 Vec（跨批复用留给
/// Ctx 接线，B2）。终结步（Out/Qual）写 out。
pub fn eval_row(
    p: &ScalarProgram,
    row: &[SqlValue],
    params: &[SqlValue],
    out: &mut SqlValue,
) -> Result<()> {
    let mut regs: Vec<SqlValue> = vec![SqlValue::Null; p.n_regs];
    let mut pc: usize = 0;
    let n = p.steps.len();
    while pc < n {
        let s = &p.steps[pc];
        pc += 1;
        macro_rules! bin {
            ($dst:expr, $a:expr, $b:expr, $f:expr) => {{
                let v = $f(regs[$a as usize].clone(), regs[$b as usize].clone())?;
                regs[$dst as usize] = v;
            }};
        }
        match s {
            ScalarStep::Const { dst, c } => regs[*dst as usize] = p.consts[*c as usize].clone(),
            ScalarStep::Col { dst, idx } => {
                let i = *idx as usize;
                if i >= row.len() {
                    return Err(SqlError::internal("scalar: column index out of range"));
                }
                regs[*dst as usize] = row[i].clone();
            }
            ScalarStep::Param { dst, idx } => {
                let i = *idx as usize;
                regs[*dst as usize] = params.get(i).cloned().ok_or_else(|| {
                    SqlError::new("08P01", format!("unbound parameter ${}", i + 1))
                })?;
            }
            ScalarStep::Cmp { dst, op, a, b } => {
                let o = p.binops[*op as usize].clone();
                bin!(*dst, *a, *b, |l, r| crate::sql::expr::binop(o, l, r));
            }
            ScalarStep::Arith { dst, op, a, b } => {
                let o = p.binops[*op as usize].clone();
                bin!(*dst, *a, *b, |l, r| crate::sql::expr::binop(o, l, r));
            }
            ScalarStep::Concat { dst, a, b } => {
                bin!(*dst, *a, *b, |l, r| {
                    crate::sql::expr::binop(BO::StringConcat, l, r)
                });
            }
            ScalarStep::And { dst, a, b } => {
                bin!(*dst, *a, *b, |l, r| crate::sql::expr::binop(BO::And, l, r));
            }
            ScalarStep::Or { dst, a, b } => {
                bin!(*dst, *a, *b, |l, r| crate::sql::expr::binop(BO::Or, l, r));
            }
            ScalarStep::Not { dst, src } => {
                let v = crate::sql::expr::as_bool(&regs[*src as usize])?;
                regs[*dst as usize] = SqlValue::Bool(!v);
            }
            ScalarStep::Neg { dst, src } => {
                regs[*dst as usize] = negate(regs[*src as usize].clone())?;
            }
            ScalarStep::IsNull { dst, src } => {
                regs[*dst as usize] = SqlValue::Bool(regs[*src as usize].is_null());
            }
            ScalarStep::IsNotNull { dst, src } => {
                regs[*dst as usize] = SqlValue::Bool(!regs[*src as usize].is_null());
            }
            ScalarStep::IsTrue { dst, src } => {
                regs[*dst as usize] = SqlValue::Bool(matches!(
                    crate::sql::expr::as_bool(&regs[*src as usize]),
                    Ok(true)
                ));
            }
            ScalarStep::IsFalse { dst, src } => {
                regs[*dst as usize] = SqlValue::Bool(matches!(
                    crate::sql::expr::as_bool(&regs[*src as usize]),
                    Ok(false)
                ));
            }
            ScalarStep::Cast { dst, src, ty } => {
                let dt = p.casts[*ty as usize].clone();
                regs[*dst as usize] =
                    crate::sql::expr::cast_value(regs[*src as usize].clone(), &dt)?;
            }
            ScalarStep::Between {
                dst,
                v,
                lo,
                hi,
                negated,
            } => {
                let (vv, lv, hv) = (
                    regs[*v as usize].clone(),
                    regs[*lo as usize].clone(),
                    regs[*hi as usize].clone(),
                );
                if vv.is_null() || lv.is_null() || hv.is_null() {
                    regs[*dst as usize] = SqlValue::Null;
                } else {
                    let inside = crate::sql::expr::cmp_values(&vv, &lv)?
                        != std::cmp::Ordering::Less
                        && crate::sql::expr::cmp_values(&vv, &hv)? != std::cmp::Ordering::Greater;
                    regs[*dst as usize] = SqlValue::Bool(inside != *negated);
                }
            }
            ScalarStep::InTest { v, item, state } => {
                let (vv, iv) = (regs[*v as usize].clone(), regs[*item as usize].clone());
                let st = regs[*state as usize].clone();
                // found 吸收；null 置 1（若未 found）；否则保持
                let newst = match st {
                    SqlValue::Int32(2) => SqlValue::Int32(2),
                    _ => {
                        if iv.is_null() || vv.is_null() {
                            SqlValue::Int32(1)
                        } else if crate::sql::expr::cmp_values(&vv, &iv)?
                            == std::cmp::Ordering::Equal
                        {
                            SqlValue::Int32(2)
                        } else {
                            st
                        }
                    }
                };
                regs[*state as usize] = newst;
            }
            ScalarStep::InFinish {
                dst,
                state,
                negated,
            } => {
                let res = match regs[*state as usize] {
                    SqlValue::Int32(2) => true,
                    SqlValue::Int32(1) => {
                        regs[*dst as usize] = SqlValue::Null;
                        continue;
                    }
                    _ => false,
                };
                regs[*dst as usize] = SqlValue::Bool(res != *negated);
            }
            ScalarStep::CaseHit { dst, operand, when } => {
                let (ov, wv) = (
                    regs[*operand as usize].clone(),
                    regs[*when as usize].clone(),
                );
                let hit = !ov.is_null()
                    && !wv.is_null()
                    && crate::sql::expr::cmp_values(&ov, &wv)? == std::cmp::Ordering::Equal;
                regs[*dst as usize] = SqlValue::Bool(hit);
            }
            ScalarStep::Mov { dst, src } => regs[*dst as usize] = regs[*src as usize].clone(),
            ScalarStep::Jump(t) => pc = *t as usize,
            ScalarStep::JumpIfTrue { reg, tgt } => {
                if matches!(regs[*reg as usize], SqlValue::Bool(true)) {
                    pc = *tgt as usize;
                }
            }
            ScalarStep::JumpIfNotTrue { reg, tgt } => {
                let b = crate::sql::expr::as_bool(&regs[*reg as usize])?; // Null → Err（怪癖复刻）
                if !b {
                    pc = *tgt as usize;
                }
            }
            ScalarStep::JumpIfInFound { state, tgt } => {
                if matches!(regs[*state as usize], SqlValue::Int32(2)) {
                    pc = *tgt as usize;
                }
            }
            ScalarStep::Qual { src } => {
                *out = match &regs[*src as usize] {
                    SqlValue::Bool(true) => SqlValue::Bool(true),
                    SqlValue::Bool(false) => SqlValue::Bool(false),
                    _ => SqlValue::Bool(false), // NULL → false 丢行（EEOP_QUAL）
                };
                return Ok(());
            }
            ScalarStep::Out { src } => {
                *out = regs[*src as usize].clone();
                return Ok(());
            }
        }
    }
    Err(SqlError::internal("scalar: program missing terminator"))
}



// ---------------------------------------------------------------------------
// B4：反汇编与再解析（EXPLAIN 步列表段 / R4 round-trip；09 文本表示的
// 标量方言前奏——正式 dendro.ir 格式随 v2c-1 落地，此处先立 round-trip
// 合同本体：disassemble(reparse(disassemble(p))) == disassemble(p)）。
// ---------------------------------------------------------------------------

/// 常量的文本表示（确定性：Float64 用 IEEE 位模式十六进制——禁十进制
/// 往返，09 §3 规则 2）
fn fmt_const(v: &SqlValue) -> String {
    match v {
        SqlValue::Null => "null".into(),
        SqlValue::Bool(b) => format!("bool({b})"),
        SqlValue::Int32(i) => format!("i32({i})"),
        SqlValue::Int64(i) => format!("i64({i})"),
        SqlValue::Float64(f) => format!("f64(0x{:016x})", f.to_bits()),
        SqlValue::Utf8(s) => format!("str({})", escape_str(s)),
        SqlValue::Bytes(b) => format!("bytes(0x{})", hex(b)),
        SqlValue::Date32(d) => format!("date32({d})"),
        SqlValue::TimestampMs(t) => format!("tsms({t})"),
    }
}

fn parse_const(t: &str) -> Option<SqlValue> {
    if t == "null" {
        return Some(SqlValue::Null);
    }
    let (tag, body) = t.split_once('(')?;
    let body = body.strip_suffix(')')?;
    Some(match tag {
        "null" => SqlValue::Null,
        "bool" => SqlValue::Bool(body.parse().ok()?),
        "i32" => SqlValue::Int32(body.parse().ok()?),
        "i64" => SqlValue::Int64(body.parse().ok()?),
        "f64" => SqlValue::Float64(f64::from_bits(
            u64::from_str_radix(body.strip_prefix("0x")?, 16).ok()?,
        )),
        "str" => SqlValue::Utf8(unescape_str(body)?),
        "bytes" => SqlValue::Bytes(unhex(body.strip_prefix("0x")?)),
        "date32" => SqlValue::Date32(body.parse().ok()?),
        "tsms" => SqlValue::TimestampMs(body.parse().ok()?),
        _ => return None,
    })
}

fn escape_str(s: &str) -> String {
    // JSON 转义子集（\n \" \\），非 ASCII 原样（源是 UTF-8）
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .inserted_quotes()
}

trait InsertQuotes {
    fn inserted_quotes(self) -> String;
}
impl InsertQuotes for String {
    fn inserted_quotes(self) -> String {
        format!("\"{self}\"")
    }
}

fn unescape_str(s: &str) -> Option<String> {
    let s = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next()? {
                'n' => out.push('\n'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

fn colref(p: &ScalarProgram, idx: u16) -> String {
    p.col_names
        .get(idx as usize)
        .map(|n| format!("#{idx}:{n}"))
        .unwrap_or_else(|| format!("#{idx}"))
}

/// 反汇编：每步一行 `<pc>: <step>`。确定性（无时间/地址）。
pub fn disassemble(p: &ScalarProgram) -> String {
    let mut out = String::new();
    if !p.col_names.is_empty() {
        out.push_str(&format!(
            "cols {}
",
            p.col_names.join(",")
        ));
    }
    for (i, s) in p.steps.iter().enumerate() {
        let line = match s {
            ScalarStep::Const { dst, c } => {
                format!("Const {} -> r{dst}", fmt_const(&p.consts[*c as usize]))
            }
            ScalarStep::Col { dst, idx } => format!("Col {} -> r{dst}", colref(p, *idx)),
            ScalarStep::Param { dst, idx } => format!("Param ${} -> r{dst}", idx + 1),
            ScalarStep::Cmp { dst, op, a, b } => {
                format!("Cmp[{}](r{a}, r{b}) -> r{dst}", p.binops[*op as usize])
            }
            ScalarStep::Arith { dst, op, a, b } => {
                format!("Arith[{}](r{a}, r{b}) -> r{dst}", p.binops[*op as usize])
            }
            ScalarStep::Concat { dst, a, b } => format!("Concat(r{a}, r{b}) -> r{dst}"),
            ScalarStep::And { dst, a, b } => format!("And(r{a}, r{b}) -> r{dst}"),
            ScalarStep::Or { dst, a, b } => format!("Or(r{a}, r{b}) -> r{dst}"),
            ScalarStep::Not { dst, src } => format!("Not(r{src}) -> r{dst}"),
            ScalarStep::Neg { dst, src } => format!("Neg(r{src}) -> r{dst}"),
            ScalarStep::IsNull { dst, src } => format!("IsNull(r{src}) -> r{dst}"),
            ScalarStep::IsNotNull { dst, src } => format!("IsNotNull(r{src}) -> r{dst}"),
            ScalarStep::IsTrue { dst, src } => format!("IsTrue(r{src}) -> r{dst}"),
            ScalarStep::IsFalse { dst, src } => format!("IsFalse(r{src}) -> r{dst}"),
            ScalarStep::Cast { dst, src, ty } => {
                format!("Cast[{}](r{src}) -> r{dst}", p.casts[*ty as usize])
            }
            ScalarStep::Between {
                dst,
                v,
                lo,
                hi,
                negated,
            } => format!("Between(neg={negated})(r{v}, r{lo}, r{hi}) -> r{dst}"),
            ScalarStep::InTest { v, item, state } => format!("InTest(r{v}, r{item}) -> r{state}"),
            ScalarStep::InFinish {
                dst,
                state,
                negated,
            } => format!("InFinish(neg={negated})(r{state}) -> r{dst}"),
            ScalarStep::CaseHit { dst, operand, when } => {
                format!("CaseHit(r{operand}, r{when}) -> r{dst}")
            }
            ScalarStep::Mov { dst, src } => format!("Mov(r{src}) -> r{dst}"),
            ScalarStep::Jump(t) => format!("Jump L{t}"),
            ScalarStep::JumpIfTrue { reg, tgt } => format!("JumpIfTrue(r{reg}) L{tgt}"),
            ScalarStep::JumpIfNotTrue { reg, tgt } => format!("JumpIfNotTrue(r{reg}) L{tgt}"),
            ScalarStep::JumpIfInFound { state, tgt } => format!("JumpIfInFound(r{state}) L{tgt}"),
            ScalarStep::Qual { src } => format!("Qual(r{src})"),
            ScalarStep::Out { src } => format!("Out(r{src})"),
        };
        out.push_str(&format!("{i}: {line}\n"));
    }
    out
}

/// 再解析（R4）：反汇编文本 → ScalarProgram。失败 = None（fail-closed，
/// 不做宽容解析）。侧表（consts/binops/casts）按再出现序重建——
/// round-trip 后与原程序**结构等价**（disassemble 输出逐字节相等）。
pub fn reparse(text: &str) -> Option<ScalarProgram> {
    let mut p = ScalarProgram::default();
    // 头行（可选）：cols 名单（round-trip 保列名）
    let mut lines = text.lines();
    let mut first = lines.next()?.trim().to_string();
    if let Some(names) = first.strip_prefix("cols ") {
        if !names.is_empty() {
            p.col_names = names.split(',').map(|s| s.to_string()).collect();
        }
        first = lines.next()?.trim().to_string();
    }
    let rest: Vec<&str> = lines.map(|l| l.trim()).collect();
    let all: Vec<&str> = std::iter::once(first.as_str()).chain(rest).collect();
    for line in all {
        let line = line.trim();
        let (pc_s, body) = line.split_once(": ")?;
        let _pc: usize = pc_s.parse().ok()?;
        let s = parse_step(body, &mut p)?;
        p.steps.push(s);
    }
    // n_regs 扫描重建
    let mut maxr = 0u16;
    fn scan(s: &ScalarStep, maxr: &mut u16) {
        use ScalarStep::*;
        macro_rules! r {
            ($x:expr) => {
                *maxr = (*maxr).max(*$x)
            };
        }
        match s {
            Const { dst, .. } | Col { dst, .. } | Param { dst, .. } => r!(dst),
            Cmp { dst, a, b, .. }
            | Arith { dst, a, b, .. }
            | Concat { dst, a, b }
            | And { dst, a, b }
            | Or { dst, a, b } => {
                r!(dst);
                r!(a);
                r!(b);
            }
            Not { dst, src }
            | Neg { dst, src }
            | IsNull { dst, src }
            | IsNotNull { dst, src }
            | IsTrue { dst, src }
            | IsFalse { dst, src }
            | Cast { dst, src, .. } => {
                r!(dst);
                r!(src);
            }
            Between { dst, v, lo, hi, .. } => {
                r!(dst);
                r!(v);
                r!(lo);
                r!(hi);
            }
            InTest { v, item, state } => {
                r!(v);
                r!(item);
                r!(state);
            }
            InFinish { dst, state, .. } => {
                r!(dst);
                r!(state);
            }
            CaseHit { dst, operand, when } => {
                r!(dst);
                r!(operand);
                r!(when);
            }
            Mov { dst, src } => {
                r!(dst);
                r!(src);
            }
            JumpIfTrue { reg, .. } | JumpIfNotTrue { reg, .. } => r!(reg),
            JumpIfInFound { state, .. } => r!(state),
            Qual { src } | Out { src } => r!(src),
            Jump(_) => {}
        }
    }
    for s in &p.steps {
        scan(s, &mut maxr);
    }
    p.n_regs = if p.steps.is_empty() {
        0
    } else {
        maxr as usize + 1
    };
    Some(p)
}

fn parse_step(body: &str, p: &mut ScalarProgram) -> Option<ScalarStep> {
    use ScalarStep::*;
    // 尾部 " -> rN" 统一剥
    let (head, dst) = if let Some((h, r)) = body.split_once(" -> r") {
        (h, Some(r.parse::<u16>().ok()?))
    } else {
        (body, None)
    };
    let opidx = |p: &mut ScalarProgram, repr: &str| -> Option<u16> {
        // binops 侧表按 Display 串比对（round-trip 内自洽）
        if let Some(i) = p.binops.iter().position(|o| o.to_string() == repr) {
            Some(i as u16)
        } else {
            // 常见比较/算术由 Display 反查 BO
            let bo = match repr {
                "=" => BO::Eq,
                "!=" => BO::NotEq,
                "<" => BO::Lt,
                "<=" => BO::LtEq,
                ">" => BO::Gt,
                ">=" => BO::GtEq,
                "+" => BO::Plus,
                "-" => BO::Minus,
                "*" => BO::Multiply,
                "/" => BO::Divide,
                "%" => BO::Modulo,
                _ => return None,
            };
            p.binops.push(bo);
            Some((p.binops.len() - 1) as u16)
        }
    };
    let rr = |s: &str| -> Option<u16> { s.strip_prefix('r')?.parse().ok() };
    Some(match head {
        h if h.starts_with("Const ") => {
            let v = parse_const(h.strip_prefix("Const ")?)?;
            let c = p.consts.len() as u16;
            p.consts.push(v);
            Const { dst: dst?, c }
        }
        h if h.starts_with("Col #") => {
            let idx = h.strip_prefix("Col #")?.split(':').next()?.parse().ok()?;
            let _ = p.col_names.get(idx as usize); // 名字不重建（结构等价即可）
            Col { dst: dst?, idx }
        }
        h if h.starts_with("Param $") => {
            let n: u16 = h.strip_prefix("Param $")?.parse().ok()?;
            Param {
                dst: dst?,
                idx: n - 1,
            }
        }
        h if h.starts_with("Cmp[") => {
            let close = h.find("](")?;
            let opi = opidx(p, &h[4..close])?;
            let (a, b) = args2(&h[close + 2..h.len() - 1], &rr)?;
            Cmp {
                dst: dst?,
                op: opi,
                a,
                b,
            }
        }
        h if h.starts_with("Arith[") => {
            let close = h.find("](")?;
            let opi = opidx(p, &h[6..close])?;
            let (a, b) = args2(&h[close + 2..h.len() - 1], &rr)?;
            Arith {
                dst: dst?,
                op: opi,
                a,
                b,
            }
        }
        h if h.starts_with("Concat(") => {
            let (a, b) = args2(strip(h, "Concat("), &rr)?;
            Concat { dst: dst?, a, b }
        }
        h if h.starts_with("And(") => {
            let (a, b) = args2(strip(h, "And("), &rr)?;
            And { dst: dst?, a, b }
        }
        h if h.starts_with("Or(") => {
            let (a, b) = args2(strip(h, "Or("), &rr)?;
            Or { dst: dst?, a, b }
        }
        _ => {
            // 无 dst 的控制流/终结步
            if let Some(t) = head.strip_prefix("Jump L") {
                Jump(t.parse().ok()?)
            } else if let Some(t) = head.strip_prefix("JumpIfTrue(r") {
                let close = t.find(") L")?;
                JumpIfTrue {
                    reg: t[..close]
                        .strip_prefix('r')
                        .unwrap_or(&t[..close])
                        .parse()
                        .ok()?,
                    tgt: t[close + 3..].parse().ok()?,
                }
            } else if let Some(t) = head.strip_prefix("JumpIfNotTrue(r") {
                let close = t.find(") L")?;
                JumpIfNotTrue {
                    reg: t[..close].parse().ok()?,
                    tgt: t[close + 3..].parse().ok()?,
                }
            } else if let Some(t) = head.strip_prefix("JumpIfInFound(r") {
                let close = t.find(") L")?;
                JumpIfInFound {
                    state: t[..close].parse().ok()?,
                    tgt: t[close + 3..].parse().ok()?,
                }
            } else if let Some(t) = head.strip_prefix("Qual(r") {
                Qual {
                    src: t.strip_suffix(')')?.parse().ok()?,
                }
            } else if let Some(t) = head.strip_prefix("Out(r") {
                Out {
                    src: t.strip_suffix(')')?.parse().ok()?,
                }
            } else if head.starts_with("Between(") {
                let inner = head.strip_prefix("Between(")?.strip_suffix(')')?;
                let neg = inner.starts_with("neg=true");
                let regs = &inner[inner.find(")(").map(|i| i + 2).unwrap_or(8)..];
                let parts: Vec<&str> = regs.split(", ").collect();
                if parts.len() != 3 {
                    return None;
                }
                Between {
                    dst: dst?,
                    v: rr(parts[0])?,
                    lo: rr(parts[1])?,
                    hi: rr(parts[2])?,
                    negated: neg,
                }
            } else if head.starts_with("InTest(") {
                let inner = head.strip_prefix("InTest(")?.strip_suffix(')')?;
                let (a, b) = args2(inner, &rr)?;
                InTest {
                    v: a,
                    item: b,
                    state: dst?,
                }
            } else if head.starts_with("InFinish(") {
                let inner = head.strip_prefix("InFinish(")?.strip_suffix(')')?;
                let neg = inner.starts_with("neg=true");
                // inner 形如 "neg=false)(r2"：find(")(r")+3 已越过 'r'
                let reg = inner[inner.find(")(r").map(|i| i + 3).unwrap_or(0)..]
                    .parse()
                    .ok()?;
                InFinish {
                    dst: dst?,
                    state: reg,
                    negated: neg,
                }
            } else if head.starts_with("CaseHit(") {
                let (operand, when) = args2(strip(head, "CaseHit("), &rr)?;
                CaseHit {
                    dst: dst?,
                    operand,
                    when,
                }
            } else if let Some(s) = head.strip_prefix("Not(r").and_then(|t| t.strip_suffix(')')) {
                Not {
                    dst: dst?,
                    src: s.parse().ok()?,
                }
            } else if let Some(s) = head.strip_prefix("Neg(r").and_then(|t| t.strip_suffix(')')) {
                Neg {
                    dst: dst?,
                    src: s.parse().ok()?,
                }
            } else if let Some(s) = head
                .strip_prefix("IsNull(r")
                .and_then(|t| t.strip_suffix(')'))
            {
                IsNull {
                    dst: dst?,
                    src: s.parse().ok()?,
                }
            } else if let Some(s) = head
                .strip_prefix("IsNotNull(r")
                .and_then(|t| t.strip_suffix(')'))
            {
                IsNotNull {
                    dst: dst?,
                    src: s.parse().ok()?,
                }
            } else if let Some(s) = head
                .strip_prefix("IsTrue(r")
                .and_then(|t| t.strip_suffix(')'))
            {
                IsTrue {
                    dst: dst?,
                    src: s.parse().ok()?,
                }
            } else if let Some(s) = head
                .strip_prefix("IsFalse(r")
                .and_then(|t| t.strip_suffix(')'))
            {
                IsFalse {
                    dst: dst?,
                    src: s.parse().ok()?,
                }
            } else if let Some(s) = head.strip_prefix("Mov(r").and_then(|t| t.strip_suffix(')')) {
                Mov {
                    dst: dst?,
                    src: s.parse().ok()?,
                }
            } else if head.starts_with("Cast[") {
                let close = head.find("](r")?;
                let repr = &head[5..close];
                let ty = if let Some(i) = p.casts.iter().position(|c| c.to_string() == repr) {
                    i as u16
                } else {
                    // 反查常见类型（round-trip 内自洽：失败 None）
                    let dt = match repr {
                        "INT" | "INTEGER" => PD::Int(None),
                        "BIGINT" => PD::BigInt(None),
                        "TEXT" | "VARCHAR" => PD::Varchar(None),
                        "DOUBLE" => PD::Double(sqlparser::ast::ExactNumberInfo::None),
                        "BOOLEAN" | "BOOL" => PD::Boolean,
                        _ => return None,
                    };
                    p.casts.push(dt);
                    (p.casts.len() - 1) as u16
                };
                let src = head[close + 3..].strip_suffix(')')?.parse().ok()?;
                Cast { dst: dst?, src, ty }
            } else {
                return None;
            }
        }
    })
}

fn strip<'a>(h: &'a str, prefix: &str) -> &'a str {
    h.strip_prefix(prefix)
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or("")
}
fn args2(s: &str, rr: &dyn Fn(&str) -> Option<u16>) -> Option<(u16, u16)> {
    let (a, b) = s.split_once(", ")?;
    Some((rr(a.trim())?, rr(b.trim())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::expr::eval;
    use sqlparser::ast::{BinaryOperator as BO, Expr, Ident, Value as PV};

    fn cols(_n: &str) -> Option<usize> {
        None
    }
    fn num(s: &str) -> Expr {
        Expr::Value(PV::Number(s.into(), false).into())
    }
    fn bval(b: bool) -> Expr {
        Expr::Value(PV::Boolean(b).into())
    }
    fn null() -> Expr {
        Expr::Value(PV::Null.into())
    }
    fn strv(s: &str) -> Expr {
        Expr::Value(PV::SingleQuotedString(s.into()).into())
    }
    fn bin(l: Expr, op: BO, r: Expr) -> Expr {
        Expr::BinaryOp {
            left: Box::new(l),
            op,
            right: Box::new(r),
        }
    }
    fn cmp_out(e: &Expr) -> (Result<SqlValue>, Result<SqlValue>) {
        let p = compile_predicate(e, &cols, 0).expect("compile");
        let mut out = SqlValue::Null;
        let r2 = eval_row(&p.prog, &[], &[], &mut out).map(|_| out);
        (
            r2,
            crate::sql::expr::eval(e, &[], &cols).map(|v| {
                // 行路径 Filter 终结语义：Bool(true) 放行，其余 false
                match v {
                    SqlValue::Bool(true) => SqlValue::Bool(true),
                    _ => SqlValue::Bool(false),
                }
            }),
        )
    }
    fn diff(e: &Expr) {
        let (a, b) = cmp_out(e);
        match (&a, &b) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "结果不等价：{e:?}"),
            (Err(x), Err(y)) => assert_eq!(
                x.message, y.message,
                "错误信息不等价：{e:?}\n  scalar={x}\n  eval={y}"
            ),
            _ => panic!("Ok/Err 分叉：{e:?}\n  scalar={a:?}\n  eval={b:?}"),
        }
    }

    /// R2：AND/OR/比较/算术/|| 的 9×9 值域全组合差分（含 NULL 三值与
    /// 类型混算），错误逐消息对拍
    #[test]
    fn r2_truth_table_differential() {
        let vals = || {
            vec![
                null(),
                bval(true),
                bval(false),
                num("1"),
                num("2"),
                num("3"),
                num("2.5"),
                strv("x"),
                strv("1"),
            ]
        };
        let ops = vec![
            BO::And,
            BO::Or,
            BO::Eq,
            BO::NotEq,
            BO::Lt,
            BO::LtEq,
            BO::Gt,
            BO::GtEq,
            BO::Plus,
            BO::Minus,
            BO::Multiply,
            BO::Divide,
            BO::Modulo,
            BO::StringConcat,
        ];
        let vs: Vec<_> = vals();
        let mut n = 0;
        for l in &vs {
            for r in &vs {
                for op in &ops {
                    diff(&bin(l.clone(), op.clone(), r.clone()));
                    n += 1;
                }
            }
        }
        assert_eq!(n, 9 * 9 * 14);
    }

    /// R1：负数字面量（一元负号折叠路径）
    #[test]
    fn r1_negative_literals() {
        let neg = |e: Expr| Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr: Box::new(e),
        };
        diff(&bin(neg(num("3")), BO::Gt, neg(num("5"))));
        diff(&bin(neg(num("1")), BO::Plus, num("2")));
        diff(&neg(neg(num("5"))));
        diff(&bin(neg(neg(num("2"))), BO::Multiply, neg(num("3"))));
        // NOT IN / IN 含负数
        diff(&Expr::InList {
            expr: Box::new(num("1")),
            list: vec![neg(num("1")), num("2")],
            negated: false,
        });
        diff(&Expr::InList {
            expr: Box::new(neg(num("1"))),
            list: vec![neg(num("1")), num("2")],
            negated: true,
        });
    }

    /// R3：常量折叠——单 Const 步且值与逐次求值一致；NOT NOT 不折叠（附录 A）
    #[test]
    fn r3_constant_folding() {
        let p = compile_expr(&bin(num("1"), BO::Plus, num("1")), &cols, 0).unwrap();
        assert_eq!(
            p.steps.len(),
            2, // Const + Out 终结步
            "1+1 应折叠为单 Const 步：{:?}",
            p.steps
        );
        assert!(matches!(p.steps[0], ScalarStep::Const { .. }));
        let p2 = compile_expr(
            &Expr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Minus,
                expr: Box::new(Expr::UnaryOp {
                    op: sqlparser::ast::UnaryOperator::Minus,
                    expr: Box::new(num("5")),
                }),
            },
            &cols,
            0,
        )
        .unwrap();
        assert_eq!(p2.steps.len(), 2, "-(-5) 应折叠为单 Const：{:?}", p2.steps);
        assert!(matches!(p2.steps[0], ScalarStep::Const { .. }));
        // 折叠失败保留运行期错误：1/0 不折叠（运行期同错——由 diff 系测试覆盖）
        let p3 = compile_expr(&bin(num("1"), BO::Divide, num("0")), &cols, 0).unwrap();
        assert!(
            p3.steps.len() > 1,
            "除零不得折叠吞错（应保留 Arith 步到运行期报错）：{:?}",
            p3.steps
        );
    }

    /// Between/InList/Case 的 NULL 与控制流怪癖差分（附录 A Q2/Q7/Q8）
    #[test]
    fn null_quirks_differential() {
        // Between：三操作数任一 Null → Null（丢行）
        diff(&Expr::Between {
            expr: Box::new(num("3")),
            negated: false,
            low: Box::new(num("2")),
            high: Box::new(null()),
        });
        diff(&Expr::Between {
            expr: Box::new(strv("x")),
            negated: true,
            low: Box::new(num("2")),
            high: Box::new(num("5")),
        });
        // InList：found 即 break——[3, 1/0] 对 v=3 不得除零
        diff(&Expr::InList {
            expr: Box::new(num("3")),
            list: vec![num("3"), bin(num("1"), BO::Divide, num("0"))],
            negated: false,
        });
        // 未 found：除零错误必须照常发生
        diff(&Expr::InList {
            expr: Box::new(num("9")),
            list: vec![num("3"), bin(num("1"), BO::Divide, num("0"))],
            negated: false,
        });
        // has_null：v=5, list=[NULL,5] → found 优先于 null → true
        diff(&Expr::InList {
            expr: Box::new(num("5")),
            list: vec![null(), num("5")],
            negated: false,
        });
        // has_null 无 found → Null（丢行）
        diff(&Expr::InList {
            expr: Box::new(num("5")),
            list: vec![null(), num("6")],
            negated: false,
        });
        // searched CASE 条件 Null → internal error（两侧同错）
        diff(&Expr::Case {
            case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
            end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
            operand: None,
            conditions: vec![sqlparser::ast::CaseWhen {
                condition: null(),
                result: num("1"),
            }],
            else_result: None,
        });
        // simple CASE 操作数 Null → 不命中 → else
        diff(&Expr::Case {
            case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
            end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
            operand: Some(Box::new(null())),
            conditions: vec![sqlparser::ast::CaseWhen {
                condition: num("1"),
                result: num("1"),
            }],
            else_result: Some(Box::new(num("9"))),
        });
    }

    /// R5：越界列/未绑定参数 = 错误非 panic
    #[test]
    fn r5_errors_not_panics() {
        // 参数未绑定（$2 只给 1 个）——运行期 08P01
        let p = compile_predicate(&num_placeholder("$2"), &cols, 0).unwrap();
        let mut out = SqlValue::Null;
        let r = eval_row(&p.prog, &[], &[SqlValue::Int64(1)], &mut out);
        assert!(r.is_err());
        // 列越界：行短于 n_cols
        let cols1 = |n: &str| (n == "a").then_some(0usize);
        let p2 = compile_predicate(&Expr::Identifier(Ident::new("a")), &cols1, 1).unwrap();
        let mut out2 = SqlValue::Null;
        let r2 = eval_row(&p2.prog, &[], &[], &mut out2);
        assert!(r2.is_err(), "行缺失必须报错非 panic");
        // 未定义列：编译期错误
        assert!(compile_predicate(&Expr::Identifier(Ident::new("zz")), &cols1, 1).is_err());
    }

    /// R4：round-trip——disassemble(reparse(text)) 逐字节恒等
    #[test]
    fn r4_roundtrip() {
        let names: Vec<String> = vec!["id".into(), "v".into()];
        let cols = |n: &str| names.iter().position(|c| c == n);
        let cases: Vec<Expr> = vec![
            bin(Expr::Identifier(Ident::new("v")), BO::Gt, num("10")),
            bin(
                bin(Expr::Identifier(Ident::new("id")), BO::Eq, num("1")),
                BO::And,
                bin(Expr::Identifier(Ident::new("v")), BO::Lt, num("9")),
            ),
            Expr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr: Box::new(Expr::IsNull(Box::new(Expr::Identifier(Ident::new("v"))))),
            },
            Expr::InList {
                expr: Box::new(Expr::Identifier(Ident::new("id"))),
                list: vec![num("1"), num("2"), null()],
                negated: true,
            },
            Expr::Between {
                expr: Box::new(Expr::Identifier(Ident::new("v"))),
                negated: false,
                low: Box::new(num("1")),
                high: Box::new(num("9")),
            },
            Expr::Case {
                case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                operand: Some(Box::new(Expr::Identifier(Ident::new("id")))),
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
                expr: Box::new(Expr::Identifier(Ident::new("v"))),
                data_type: PD::Int(None),
                format: None,
                array: false,
            },
            bin(
                strv("a"),
                BO::StringConcat,
                Expr::Identifier(Ident::new("v")),
            ),
        ];
        for e in &cases {
            let cp = compile_predicate_named(e, &cols, names.len(), &names).unwrap();
            let p = cp.prog;
            let text = disassemble(&p);
            let p2 = reparse(&text).unwrap_or_else(|| panic!("reparse 失败：{text}"));
            let text2 = disassemble(&p2);
            assert_eq!(text, text2, "round-trip 不恒等");
            // 再解析程序与原程序结构等价（steps 逐条相等）
            assert_eq!(p.steps, p2.steps, "steps 结构不等价：{text}");
        }
    }

    /// B5：步数硬断——超深表达式编译拒绝（非 panic、非无界）。
    /// 用列引用链防常量折叠折叠成单步。
    #[test]
    fn b5_step_cap() {
        let colsc = |n: &str| (n == "x").then_some(0usize);
        let x = || Expr::Identifier(Ident::new("x"));
        let mut e = x();
        for _ in 0..1100 {
            e = bin(e, BO::Plus, num("1"));
        }
        assert!(
            compile_predicate(&e, &colsc, 1).is_err(),
            "1100 深链必须被深度守卫（>128）拒绝，不爆栈"
        );
        let mut e2 = x();
        for _ in 0..100 {
            e2 = bin(e2, BO::Plus, num("1"));
        }
        assert!(compile_predicate(&e2, &colsc, 1).is_ok(), "100 深正常编译");
    }

    fn num_placeholder(id: &str) -> Expr {
        Expr::Value(PV::Placeholder(id.into()).into())
    }

    /// IsNull 族与 Not 的差分
    #[test]
    fn is_family_differential() {
        for e in [
            Expr::IsNull(Box::new(null())),
            Expr::IsNull(Box::new(num("1"))),
            Expr::IsNotNull(Box::new(strv(""))),
            Expr::IsTrue(Box::new(bval(true))),
            Expr::IsTrue(Box::new(null())),    // as_bool 错误 → false
            Expr::IsFalse(Box::new(num("1"))), // as_bool 错误 → false
            Expr::UnaryOp {
                op: sqlparser::ast::UnaryOperator::Not,
                expr: Box::new(null()),
            }, // internal error（两侧同）
        ] {
            diff(&e);
        }
    }

    /// 嵌套与列引用（有行场景）
    #[test]
    fn column_and_nested() {
        let cols2 = |n: &str| match n {
            "a" => Some(0usize),
            "b" => Some(1usize),
            _ => None,
        };
        let row = vec![SqlValue::Int64(4), SqlValue::Null];
        let e = bin(
            Expr::Nested(Box::new(Expr::Identifier(Ident::new("a")))),
            BO::GtEq,
            num("4"),
        );
        let p = compile_predicate(&e, &cols2, 2).unwrap();
        let mut out = SqlValue::Null;
        eval_row(&p.prog, &row, &[], &mut out).unwrap();
        assert_eq!(out, SqlValue::Bool(true));
        // 与行路径 eval 同果
        let v = eval(&e, &row, &cols2).unwrap();
        assert_eq!(v, SqlValue::Bool(true));
    }
}
