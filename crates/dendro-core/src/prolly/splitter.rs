//! chunk 边界分裂器（SPEC 03 §2.1，dolt keySplitter 同参数）。
//!
//! 确定性：boundary = f(已见条目序列) —— size 由前缀决定，h 只依赖 key 与层盐，
//! 故同一子树内容 ⇒ 同一边界集 ⇒ 同地址。正式哈希 xxh3-32（64 位机取低 32 位）。

use sha2::{Digest, Sha512};
use xxhash_rust::xxh3::xxh3_64;

pub const MIN_CHUNK: usize = 512;
pub const TARGET_CHUNK: usize = 4096;
pub const MAX_CHUNK: usize = 16384;
const WEIBULL_K: f64 = 4.0;

/// 每层独立盐：同一数据在不同层产生独立边界（父子边界不相关）
pub fn salt_for_level(level: u8) -> [u8; 8] {
    let mut h = Sha512::new();
    h.update((level as u64).to_le_bytes());
    let out = h.finalize();
    let mut s = [0u8; 8];
    s.copy_from_slice(&out[..8]);
    s
}

#[derive(Clone)]
pub struct KeySplitter {
    salt: [u8; 8],
}

impl KeySplitter {
    pub fn new(level: u8) -> Self {
        Self {
            salt: salt_for_level(level),
        }
    }

    /// 追加本条目后，当前缓冲是否应在此条目后切分。
    /// `prev_size`/`size` = 本条目追加前/后的缓冲字节数。
    ///
    /// 增量形式用**条件风险**（prototype/prolly 验证的关键点）：
    /// `p_i = 1 - exp(-Δ[(size/λ)^k])`（对字节精确差分），
    /// 此时 Π(1-p_i) = exp(-(s/λ)^k)，节点尺寸精确服从 Weibull(k=4, λ=4096)。
    /// 若用无条件 CDF 阈值 `h < F(size)`，分布会系统性偏移。
    pub fn crossed_boundary(&self, prev_size: usize, size: usize, key: &[u8]) -> bool {
        if size < MIN_CHUNK {
            return false;
        }
        if size >= MAX_CHUNK {
            return true;
        }
        // h: key 的 32 位指纹（seed=层盐）。xxh3 雪崩充分，顺序主键无网格伪影
        // （原型实证 FNV-1a 顺序键会走出等差格 → 必须 fmix32 终结化）
        let seed = u64::from_le_bytes(self.salt);
        let h = (xxh3_64(key) ^ seed) & 0xFFFF_FFFF;
        let x = size as f64 / TARGET_CHUNK as f64;
        let xp = prev_size as f64 / TARGET_CHUNK as f64;
        let delta = x.powf(WEIBULL_K) - xp.powf(WEIBULL_K);
        let p = 1.0 - (-delta).exp();
        (h as f64 / 4_294_967_296.0) < p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(i: usize) -> Vec<u8> {
        format!("key/{i:08}").into_bytes()
    }

    #[test]
    fn deterministic() {
        let s = KeySplitter::new(0);
        // 同一 (size, key) 判定稳定
        assert_eq!(
            s.crossed_boundary(1968, 2000, b"abc"),
            s.crossed_boundary(1968, 2000, b"abc")
        );
        assert!(!s.crossed_boundary(68, 100, b"abc"));
        assert!(s.crossed_boundary(MAX_CHUNK + 1, MAX_CHUNK + 2, b"abc"));
    }

    #[test]
    fn size_distribution() {
        // 10000 条目流过 splitter，统计节点尺寸分布
        let s = KeySplitter::new(0);
        let mut sizes = Vec::new();
        let mut cur = 0usize;
        for i in 0..50_000 {
            let k = key_of(i);
            let item = 32; // 模拟 key+value 平均字节数
            let prev = cur;
            cur += item;
            if s.crossed_boundary(prev, cur, &k) {
                sizes.push(cur);
                cur = 0;
            }
        }
        sizes.sort_unstable();
        let n = sizes.len();
        assert!(n > 300, "too few chunks: {n}");
        let med = sizes[n / 2];
        let p99 = sizes[n * 99 / 100];
        // 中位接近 target 量级，p99 不爆
        assert!(med > 1000 && med < 12_000, "median {med}");
        assert!(p99 < 20_000, "p99 {p99}");
    }

    #[test]
    fn level_salts_differ() {
        assert_ne!(salt_for_level(0), salt_for_level(1));
    }
}
