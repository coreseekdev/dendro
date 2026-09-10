//! mysql_native_password 认证 — SPEC 06 §3「认证」。
//!
//! 口令校验式（SHA1 异或式）：客户端发回的 token = `SHA1(p) XOR SHA1(scramble ‖ SHA1(SHA1(p)))`。
//! 服务端持有明文口令（cfg.password），直接重算期望 token 比对。

use sha1::{Digest, Sha1};

/// 本服务端唯一宣告/接受的认证插件名
pub const NATIVE: &str = "mysql_native_password";

/// 计算客户端应回的 token：`SHA1(p) XOR SHA1(scramble ‖ SHA1(SHA1(p)))`
pub fn scramble_token(password: &str, scramble: &[u8]) -> [u8; 20] {
    let p1 = Sha1::digest(password.as_bytes()); // SHA1(p)
    let p2 = Sha1::digest(p1); // SHA1(SHA1(p))
    let mut h = Sha1::new();
    h.update(scramble);
    h.update(p2);
    let s = h.finalize(); // SHA1(scramble ‖ SHA1(SHA1(p)))
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = p1[i] ^ s[i];
    }
    out
}

/// 校验客户端 token 是否与 cfg.password 匹配
pub fn verify_native(password: &str, scramble: &[u8; 20], token: &[u8]) -> bool {
    token == scramble_token(password, scramble)
}

/// 生成 20B 随机 scramble（握手 auth-plugin-data-part-1 8B + part-2 12B + 0x00）。
/// 优先 /dev/urandom；不可用时退回「时间熵 + 地址熵」的 xorshift。
pub fn new_scramble() -> [u8; 20] {
    use std::io::Read;
    let mut buf = [0u8; 20];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        ^ (&buf as *const _ as u64);
    for b in buf.iter_mut() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *b = (s >> 24) as u8;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_known_vectors() {
        // RFC 3174 / NIST 已知向量
        assert_eq!(
            hex(&Sha1::digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&Sha1::digest(b"secret")),
            "e5e9fa1ba31ecd1ae84f75caaa474f3a663f05f4"
        );
    }

    #[test]
    fn native_password_known_vector() {
        // 外部（python hashlib）按 RFC 公式预算的期望值：
        // p = "secret", scramble = bytes(range(20))
        let scramble: [u8; 20] = std::array::from_fn(|i| i as u8);
        let token = scramble_token("secret", &scramble);
        assert_eq!(hex(&token), "21b3ff405f32cbe4aafff291396046ea29fa3a4d");
        assert!(verify_native("secret", &scramble, &token));
        // 错口令 / 错 scramble / 空包 → 拒绝
        assert!(!verify_native("wrong", &scramble, &token));
        assert!(!verify_native("secret", &[0u8; 20], &token));
        assert!(!verify_native("secret", &scramble, &token[..8]));
        assert!(!verify_native("secret", &scramble, &[]));
    }

    fn hex(d: &[u8]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }
}
