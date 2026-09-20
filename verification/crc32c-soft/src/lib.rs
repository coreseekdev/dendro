//! CRC-32C（iSCSI，反射多项式 0x1EDC6F41 / 0x82F63B78）的**规范定义**
//! 逐位实现——Kani 符号执行用的数学模型。
//!
//! ## 角色（绑定链）
//!
//! 生产 codec（`crates/dendro-core/src/wal/codec.rs`）调用真 `crc32c`
//! crate（SSE4.2 SIMD + `cpuid` 运行时检测——内联汇编，Kani 不可验证，
//! 615 项 UNDETERMINED 实证）。dendro-kani 包以依赖别名
//! `crc32c = { package = "crc32c-soft" }` 顶替之，使**生产源码原样编译**
//! 进入证明。
//!
//! 因此本实现与真 crate 的等价性是绑定链上唯一的模型缝隙：
//! `tests/diff_real.rs` 差分测试（已知向量 + LCG 伪随机全长度扫描）
//! 实证两实现逐字节一致。改动任一侧必须重跑差分。

/// CRC-32C 校验和（逐位，无查表——符号执行友好）
pub fn crc32c(data: &[u8]) -> u32 {
    const POLY: u32 = 0x82F6_3B78; // 0x1EDC6F41 的反射形式
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mut bit = crc & 1;
            bit = bit.wrapping_neg(); // 0 或 0xFFFF_FFFF（无分支掩码）
            crc = (crc >> 1) ^ (POLY & bit);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    /// RFC 3720 / 工业已知向量
    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"a"), 0xC1D0_4330);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
    }
}
