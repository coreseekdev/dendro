//! 内容寻址哈希：SHA-512 截断 160bit，base32("0-9a-v") 文本编码。
//! 参考 dolt go/store/hash/hash.go：20 字节平衡碰撞抗性与树扇出。

use sha2::{Digest, Sha512};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash([u8; 20]);

/// base32 字母表：'0'-'9','a'-'v'（排序后文本序 = 字节序）
const B32: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";

impl Hash {
    pub fn of(data: &[u8]) -> Self {
        let mut h = Sha512::new();
        h.update(data);
        let out = h.finalize();
        let mut a = [0u8; 20];
        a.copy_from_slice(&out[..20]);
        Hash(a)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// 分片/布桶用 64 位视图（SHA-512 前缀，均匀分布；非内容寻址身份）
    pub fn as_u64(&self) -> u64 {
        u64::from_be_bytes(self.0[..8].try_into().unwrap())
    }

    pub fn from_bytes(a: [u8; 20]) -> Self {
        Hash(a)
    }

    pub fn to_base32(&self) -> String {
        // 标准 base32 位打包（MSB first），160 bit = 32 字符整除
        let mut out = String::with_capacity(32);
        let mut buf: u64 = 0;
        let mut bits = 0u32;
        for &b in &self.0 {
            buf = (buf << 8) | b as u64;
            bits += 8;
            while bits >= 5 {
                let idx = ((buf >> (bits - 5)) & 0x1f) as usize;
                out.push(B32[idx] as char);
                bits -= 5;
            }
        }
        debug_assert_eq!(out.len(), 32);
        out
    }

    pub fn from_base32(s: &str) -> Option<Self> {
        if s.len() != 32 {
            return None;
        }
        let mut a = [0u8; 20];
        let mut buf: u64 = 0;
        let mut bits = 0u32;
        let mut bi = 0usize;
        for c in s.bytes() {
            let v = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'v' => c - b'a' + 10,
                _ => return None,
            };
            buf = (buf << 5) | v as u64;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                if bi >= 20 {
                    return None;
                }
                a[bi] = (buf >> bits) as u8;
                bi += 1;
            }
        }
        if bi != 20 {
            return None;
        }
        Some(Hash(a))
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 20]
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_base32())
    }
}
impl std::fmt::Debug for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hash({})", self.to_base32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = Hash::of(b"hello dendro");
        let s = h.to_base32();
        assert_eq!(s.len(), 32);
        assert_eq!(Hash::from_base32(&s).unwrap(), h);
        assert_eq!(h.to_string(), s);
    }

    #[test]
    fn order_matches_text() {
        // 文本序 = 字节序
        let a = Hash::from_base32(&("0".repeat(31) + "0")).unwrap();
        let b = Hash::from_base32(&("0".repeat(31) + "1")).unwrap();
        assert!(a < b);
        assert!(a.to_base32() < b.to_base32());
    }

    #[test]
    fn known_vector() {
        // 自洽性：同内容同哈希，不同内容不同哈希
        assert_eq!(Hash::of(b"x"), Hash::of(b"x"));
        assert_ne!(Hash::of(b"x"), Hash::of(b"y"));
    }
}
