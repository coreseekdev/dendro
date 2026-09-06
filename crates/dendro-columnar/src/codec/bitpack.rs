//! BITPACK codec（id=1）— SPEC 05 §3：`bit_width u8 + 位压缩流`，定宽整数
//! 无符号化后压缩。实现为简化的**逐位 LSB-first 打包**（位 i 属于值 i/width，
//! 在流内偏移 i*width+b，b 为值内位序）——任务允许简化，但解码保证精确。
//! 窄整数解码接近 RAW 吞吐（SPEC 08 §2 假设表：位搬运）。

use crate::{Error, Result};

/// 计算覆盖全部值所需的位宽（1..=64）
pub(crate) fn width_of(values: &[u64]) -> u8 {
    let m = values.iter().copied().max().unwrap_or(0);
    if m == 0 {
        1
    } else {
        (64 - m.leading_zeros()) as u8
    }
}

/// LSB-first 位打包：追加到 out 末尾（不含 bit_width 头）
pub(crate) fn pack(values: &[u64], width: u8, out: &mut Vec<u8>) {
    debug_assert!(width >= 1);
    let w = width as usize;
    let mut buf = vec![0u8; (values.len() * w + 7) / 8];
    let mut pos = 0usize;
    for &v in values {
        for b in 0..w {
            if (v >> b) & 1 == 1 {
                let p = pos + b;
                buf[p >> 3] |= 1 << (p & 7);
            }
        }
        pos += w;
    }
    out.extend_from_slice(&buf);
}

/// 流式编码：`bit_width u8 + packed stream`（SPEC 05 §3 BITPACK）
pub(crate) fn encode(values: &[u64], out: &mut Vec<u8>) {
    let width = width_of(values);
    out.push(width);
    pack(values, width, out);
}

/// 解码 n 个值。流形如 encode。
pub(crate) fn decode(data: &[u8], n: usize) -> Result<Vec<u64>> {
    if data.is_empty() {
        return Err(Error::Corrupt("bitpack stream empty".into()));
    }
    let w = data[0] as usize;
    if !(1..=64).contains(&w) {
        return Err(Error::Corrupt(format!("bitpack width {w} invalid")));
    }
    let need = (n * w + 7) / 8;
    if data.len() < 1 + need {
        return Err(Error::Corrupt("bitpack stream truncated".into()));
    }
    let bytes = &data[1..1 + need];
    let mut values = Vec::with_capacity(n);
    let mut pos = 0usize;
    for _ in 0..n {
        let mut v = 0u64;
        for b in 0..w {
            let p = pos + b;
            if (bytes[p >> 3] >> (p & 7)) & 1 == 1 {
                v |= 1 << b;
            }
        }
        values.push(v);
        pos += w;
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_widths() {
        for &w in &[1u8, 3, 7, 8, 13, 17, 31, 32, 33, 63, 64] {
            let mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let vals: Vec<u64> = (0..257u64)
                .map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15)) & mask)
                .chain([0, mask, 0, mask])
                .collect();
            let mut out = Vec::new();
            encode(&vals, &mut out);
            let got = decode(&out, vals.len()).unwrap();
            assert_eq!(got, vals, "width {w}");
        }
    }

    #[test]
    fn width_of_basics() {
        assert_eq!(width_of(&[0, 0]), 1);
        assert_eq!(width_of(&[1]), 1);
        assert_eq!(width_of(&[255]), 8);
        assert_eq!(width_of(&[256]), 9);
        assert_eq!(width_of(&[u64::MAX]), 64);
    }
}
