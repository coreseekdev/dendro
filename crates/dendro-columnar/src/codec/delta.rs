//! DELTA codec（id=5）— SPEC 05 §3（保留）；SPEC 08 §5 发现 3：DELTA 对 pk/ts
//! 列的价值被定量确认（zstd 独占 delta 熵 3.9-10.8R），Rust 实现内置 DELTA 以
//! 摆脱对 zstd 的隐性依赖。
//!
//! 格式：`baseline u64 LE + Σ varint(zigzag(v[i] - v[i-1]))`。差分在无符号化
//! 值域上回绕计算，zigzag 保序编码为 varint——顺序 i64 每值约 1 字节。

use super::{read_varint, write_varint, zigzag_decode, zigzag_encode};
use crate::{Error, Result};

pub(crate) fn encode(values: &[u64], out: &mut Vec<u8>) {
    let Some(&first) = values.first() else {
        return; // rows == 0：空流
    };
    out.extend_from_slice(&first.to_le_bytes());
    let mut prev = first;
    for &v in &values[1..] {
        let d = v.wrapping_sub(prev);
        write_varint(out, zigzag_encode(d));
        prev = v;
    }
}

pub(crate) fn decode(data: &[u8], rows: usize) -> Result<Vec<u64>> {
    if rows == 0 {
        return Ok(Vec::new());
    }
    if data.len() < 8 {
        return Err(Error::Corrupt("delta baseline truncated".into()));
    }
    let mut cur = data;
    let mut cur_v = u64::from_le_bytes([
        cur[0], cur[1], cur[2], cur[3], cur[4], cur[5], cur[6], cur[7],
    ]);
    cur = &cur[8..];
    let mut values = Vec::with_capacity(rows);
    values.push(cur_v);
    for _ in 1..rows {
        let z = read_varint(&mut cur)?;
        cur_v = cur_v.wrapping_add(zigzag_decode(z));
        values.push(cur_v);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_seq_and_wrap() {
        // 顺序列
        let seq: Vec<u64> = (0..1000).map(|i| i * 3 + 7).collect();
        let mut out = Vec::new();
        encode(&seq, &mut out);
        assert_eq!(decode(&out, seq.len()).unwrap(), seq);
        // 负数（无符号化域回绕）：i64 连续负值
        let neg: Vec<u64> = (-500i64..=500).map(|v| v as u64).collect();
        let mut out2 = Vec::new();
        encode(&neg, &mut out2);
        assert_eq!(decode(&out2, neg.len()).unwrap(), neg);
        // 单行 / 空列
        let mut out3 = Vec::new();
        encode(&[42], &mut out3);
        assert_eq!(decode(&out3, 1).unwrap(), vec![42]);
        assert!(decode(&[], 0).unwrap().is_empty());
    }

    #[test]
    fn zigzag_symmetry() {
        for v in [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN] {
            assert_eq!(zigzag_decode(zigzag_encode(v as u64)) as i64, v);
        }
    }
}
