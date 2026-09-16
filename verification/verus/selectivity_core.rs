//! L3 证明 #2：选择率整数核（镜像 sql/stats.rs `range_selectivity` 的
//! 差值整数段——O-3+ 精度修复后的核心不变量）。
//!
//! 证：
//! 1. **clamp 定界**（exec 可执行体）：diff_clamp 输出 == spec 且
//!    ≤ span——frac ≤ 1 的整数核（f64 frac 的精度安全前提）
//! 2. **估算不放大**（乘法界）：total·below ≤ total·span——
//!    est = total·below/span ≤ total 的本质内容（below/span ≤ 1
//!    的加权形态）。**机械缺口（如实记录）**：最后整除一步
//!    （t·b ≤ t·s ∧ s>0 ⇒ t·b/s ≤ t）依赖欧几里得除法定义展开，
//!    本 Verus 版（0.2026.09）对变量分母×非线性被除数的整除公理
//!    不自动实例化（by(nonlinear_arith) 亦不含整除）；数学上一步
//!    平凡（除法定义），vstd int::div_mod 引理可闭合——待 vstd
//!    链接修复后升级本 ensures。
//!
//! 运行：scripts/verus.sh verification/verus/selectivity_core.rs

use builtin::*;
use builtin_macros::*;

verus! {
    /// spec：差值 clamp（stats.rs `below.min(span)` 镜像）
    pub open spec fn diff_clamp_spec(lo: u64, hi: u64, p: u64) -> u64 {
        let span = (hi as int) - (lo as int);
        let raw = if p >= lo { (p as int) - (lo as int) } else { 0int };
        (if raw < span { raw } else { span }) as u64
    }

    /// ① clamp 定界（exec——证明对执行代码成立）
    pub fn diff_clamp(lo: u64, hi: u64, p: u64) -> (below: u64)
        requires hi > lo
        ensures
            below == diff_clamp_spec(lo, hi, p),
            below <= (hi as int) - (lo as int),
    {
        let span = hi - lo;
        let raw = p.saturating_sub(lo);
        if raw < span { raw } else { span }
    }

    /// ② 估算不放大（乘法界）：t·below ≤ t·span（below ≤ span 加权）
    pub proof fn est_no_amplify(total: u64, lo: u64, hi: u64, p: u64)
        requires hi > lo
        ensures
            (total as int) * (diff_clamp_spec(lo, hi, p) as int)
                <= (total as int) * ((hi as int) - (lo as int)),
    {
        let span = (hi as int) - (lo as int);
        let below = diff_clamp_spec(lo, hi, p) as int;
        assert(0 <= below <= span);
        assert(span > 0);
        assert((total as int) >= 0);
        // 差分解（分配律——nonlinear_arith 职责域）：
        // t·span − t·below = t·(span−below) ≥ 0
        assert((total as int) * span - (total as int) * below
            == (total as int) * (span - below)) by(nonlinear_arith);
        assert((total as int) * (span - below) >= 0);
    }

    fn main() {}
}
