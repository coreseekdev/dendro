//! 差分测试：软模型（本包）≡ 真 crc32c crate（SIMD）。
//!
//! 这是 dendro-kani 绑定链上唯一的模型缝隙的闭合机制：
//! Kani 证明里 crc32c 调用解析到软模型；本测试实证两实现对任意
//! 输入逐字节一致（已知向量 + 0..=256 全长度 LCG 伪随机扫描 +
//! 1KiB 随机块）。纳入 verify.sh 门禁。

#[test]
fn soft_matches_real_crc32c() {
    // 已知向量（RFC 3720 系）
    let edges: [&[u8]; 6] = [
        b"", b"a", b"123456789", &[0u8; 32], &[0xFFu8; 32], b"the quick brown fox",
    ];
    for v in edges {
        assert_eq!(
            crc32c_soft::crc32c(v),
            crc32c::crc32c(v),
            "已知向量不一致 len={}",
            v.len()
        );
    }

    // 全长度扫描（0..=256）× LCG 伪随机内容
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for len in 0..=256usize {
        let buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        assert_eq!(
            crc32c_soft::crc32c(&buf),
            crc32c::crc32c(&buf),
            "len={len} 不一致"
        );
    }

    // 大块（跨 SIMD 分块边界 8×3=24 / 尾部处理）
    for len in [55usize, 56, 1024] {
        let buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        assert_eq!(
            crc32c_soft::crc32c(&buf),
            crc32c::crc32c(&buf),
            "大块 len={len} 不一致"
        );
    }
}
