//! L3 证明 #1：zone map / 列统计的 order 域（镜像 dendro-columnar
//! src/stats.rs `order_key` 的 Int 家族臂与 dendro-core scan.rs
//! `order_domain`——两侧同式 `v ^ (1<<63)`）。
//!
//! 证三条：
//! 1. **对合**：flip(flip(v)) == v——编码/解码往返
//! 2. **保序**：i64 序 ⇒ order 域序（zone map 区间剪枝与选择率 uniform
//!    假设的合法性根基——位翻转不改相对序）
//! 3. **定点解码**：enc⁻¹(enc(i)) == i
//!
//! 证明形态：位事实以**自足重言式**整体送 by(bit_vector)（蕴含前提
//! 一并进位向量查询；含跨整型 cast 的混合式不自动位爆炸——实证），
//! 跨层推导在 SMT 整数算术。
//!
//! 运行：scripts/verus.sh verification/verus/order_domain.rs

use builtin::*;
use builtin_macros::*;

verus! {
    pub const C: u64 = 0x8000000000000000u64;

    /// 源镜像：`v ^ (1u64 << 63)`（stats.rs order_key Int64 臂）
    pub open spec fn flip(v: u64) -> u64 {
        v ^ C
    }

    /// i64 → order 域（编码）
    pub open spec fn enc(i: i64) -> u64 {
        (i as u64) ^ C
    }

    /// 可执行镜像（证明对可执行体成立——spec 与 exec 同式）
    pub fn flip_exec(v: u64) -> (r: u64)
        ensures r == flip(v)
    {
        v ^ C
    }

    /// 位恒等式：与 2^63 异或 = 跨 2^63 边界的算术平移
    proof fn xor_shift(v: u64)
        ensures
            (v >= C ==> v ^ C == v - C),
            (v < C ==> v ^ C == v + C),
    {
        assert(v ^ C == (if v >= C { v - C } else { v + C })) by(bit_vector);
    }

    /// ① 对合（flip 是对合——编码/解码共用同一函数的根基）
    pub proof fn flip_involution(v: u64)
        ensures flip(flip(v)) == v
    {
        xor_shift(v);
        xor_shift(v ^ C);
    }

    /// ② 保序：i64 a ≤ b ⇒ enc(a) ≤ enc(b)（zone map 剪枝 + 选择率
    /// uniform 假设的合法性）——定理整体作为位向量重言式
    pub proof fn order_preserving(a: i64, b: i64)
        ensures a <= b ==> enc(a) <= enc(b)
    {
        assert((a <= b) ==> (((a as u64) ^ C) <= ((b as u64) ^ C))) by(bit_vector);
    }

    /// ③ 解码往返（统计面谓词点求域的可逆性）——经 ① 归约到纯
    /// round-trip cast（BV 可解形态；xor→cast 链不自动位爆炸）
    pub proof fn decode_roundtrip(i: i64)
        ensures ((enc(i) ^ C) as i64) == i
    {
        flip_involution(i as u64); // (i as u64 ^ C) ^ C == i as u64
        assert(((i as u64) as i64) == i) by(bit_vector);
    }

    fn main() {}
}
