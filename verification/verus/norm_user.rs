//! L3 证明 #3：标识符 ASCII 折叠（镜像 sql/privs.rs `norm_user` 的
//! to_ascii_lowercase——GRANT/REVOKE/user/owner 四处统一小写的语义面）。
//!
//! 证：
//! 1. **幂等**：lower(lower(c)) == lower(c)——重复折叠稳定
//!    （SET user × 多次 norm 不变性）
//! 2. **界不变**：c 可打印 ASCII ⇒ lower(c) 可打印 ASCII
//!    （折叠不越界——无 UTF-8 破坏）
//! 3. **定点集**：已小写/非字母字符是定点
//!
//! 运行：scripts/verus.sh verification/verus/norm_user.rs

use builtin::*;
use builtin_macros::*;

verus! {
    /// spec：ASCII 小写（Rust char::to_ascii_lowercase 字节语义镜像）
    pub open spec fn lower(c: u8) -> u8 {
        (if 65 <= c <= 90 { (c as int) + 32 } else { c as int }) as u8
    }

    /// ① 幂等（exec 可执行体）
    pub fn lower_exec(c: u8) -> (r: u8)
        ensures
            r == lower(c),
            lower(r) == r,
    {
        if 65 <= c && c <= 90 { c + 32 } else { c }
    }

    /// ② 界不变：可打印 ASCII（0x20..=0x7E）封闭
    pub proof fn lower_printable_closed(c: u8)
        ensures 0x20 <= c <= 0x7e ==> 0x20 <= lower(c) <= 0x7e
    {
        // 'A'=65..'Z'=90 → 97..122（'a'..'z'）；其余恒等
    }

    /// ③ 定点刻画：lower(c) == c ⟺ c ∉ 'A'..'Z'
    pub proof fn lower_fixed_point(c: u8)
        ensures (lower(c) == c) <==> !(65 <= c <= 90)
    {
    }

    fn main() {}
}
