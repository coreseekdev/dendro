//! RLE_DICT codec（id=2）— SPEC 05 §3 字典页：
//!
//! ```text
//! 定宽: {dict_len u32}{width u8 = 字节宽}{字典值 RAW 定宽}{id 流 BITPACK}
//! 变宽: {dict_len u32}{width u8 = 0 标记}{bytes_len u32}
//!       {(dict_len+1) × u32 offsets}{bytes}{id 流 BITPACK}
//! ```
//!
//! id 流用 BITPACK（SPEC 08 §5 实证：17bit id 流 = 53% 原始大小）；解码端
//! gather 是纯数据并行（GPU 理想，SPEC 05 §3）。SPEC 08 §1：distinct<20% 必开。

use super::bitpack;
use super::{ColumnValues, Layout};
use crate::{Error, Result};
use std::collections::HashMap;

/// 定宽字典编码。全空/零行时填充一个 dummy 值 0，保证 id 流位宽 ≥1 且路径统一。
pub(crate) fn encode_fixed(width: usize, values: &[u64], out: &mut Vec<u8>) -> Result<()> {
    let mut dict: Vec<u64> = Vec::new();
    let mut map: HashMap<u64, u64> = HashMap::new();
    let ids: Vec<u64> = values
        .iter()
        .map(|&v| {
            *map.entry(v).or_insert_with(|| {
                dict.push(v);
                (dict.len() - 1) as u64
            })
        })
        .collect();
    if dict.is_empty() {
        dict.push(0); // dummy（rows=0 或全 null）
    }
    out.extend_from_slice(&(dict.len() as u32).to_le_bytes());
    out.push(width as u8);
    for &d in &dict {
        out.extend_from_slice(&d.to_le_bytes()[..width]);
    }
    bitpack::encode(&ids, out);
    Ok(())
}

/// 变宽（utf8/binary）字典编码：字典条目以 offsets+bytes 存储。
pub(crate) fn encode_var(offsets: &[u32], bytes: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let n = offsets.len().saturating_sub(1);
    let mut dict: Vec<&[u8]> = Vec::new();
    let mut map: HashMap<&[u8], u64> = HashMap::new();
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            let s = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
            *map.entry(s).or_insert_with(|| {
                dict.push(s);
                (dict.len() - 1) as u64
            })
        })
        .collect();
    if dict.is_empty() {
        dict.push(&[]); // dummy 空串
    }
    let dict_bytes: usize = dict.iter().map(|s| s.len()).sum();
    out.extend_from_slice(&(dict.len() as u32).to_le_bytes());
    out.push(0u8); // width=0 ⇒ 变宽字典标记
    out.extend_from_slice(&(dict_bytes as u32).to_le_bytes());
    let mut doff = 0u32;
    out.extend_from_slice(&doff.to_le_bytes());
    for s in &dict {
        doff += s.len() as u32;
        out.extend_from_slice(&doff.to_le_bytes());
    }
    for s in &dict {
        out.extend_from_slice(s);
    }
    bitpack::encode(&ids, out);
    Ok(())
}

pub(crate) fn decode(data: &[u8], rows: usize, layout: &Layout) -> Result<ColumnValues> {
    let mut cur = data;
    let dict_len = take_u32(&mut cur, "dict_len")? as usize;
    let width = take_u8(&mut cur, "dict width")?;
    if width == 0 {
        // 变宽字典
        let bytes_len = take_u32(&mut cur, "dict bytes_len")? as usize;
        let mut doffs = Vec::with_capacity(dict_len + 1);
        for _ in 0..=dict_len {
            doffs.push(take_u32(&mut cur, "dict offset")?);
        }
        if cur.len() < bytes_len {
            return Err(Error::Corrupt("dict bytes truncated".into()));
        }
        let dbytes = &cur[..bytes_len];
        cur = &cur[bytes_len..];
        if *doffs.last().unwrap() as usize != bytes_len {
            return Err(Error::Corrupt("dict offsets overrun bytes".into()));
        }
        let ids = bitpack::decode(cur, rows)?;
        let mut offsets = Vec::with_capacity(rows + 1);
        let mut obytes = Vec::new();
        offsets.push(0u32);
        for &id in &ids {
            let s = dict_entry_var(&doffs, dbytes, id as usize)?;
            obytes.extend_from_slice(s);
            offsets.push(obytes.len() as u32);
        }
        Ok(ColumnValues::Var { offsets, bytes: obytes })
    } else {
        // 定宽字典
        let need = dict_len.checked_mul(width as usize).ok_or_else(|| {
            Error::Corrupt("dict size overflow".into())
        })?;
        if cur.len() < need {
            return Err(Error::Corrupt("dict entries truncated".into()));
        }
        let mut dict = Vec::with_capacity(dict_len);
        for i in 0..dict_len {
            let b = &cur[i * width as usize..(i + 1) * width as usize];
            let mut v = 0u64;
            for &byte in b.iter().rev() {
                v = (v << 8) | byte as u64;
            }
            dict.push(v);
        }
        cur = &cur[need..];
        let ids = bitpack::decode(cur, rows)?;
        let mut values = Vec::with_capacity(rows);
        for &id in &ids {
            let v = *dict.get(id as usize).ok_or_else(|| {
                Error::Corrupt(format!("dict id {id} out of range"))
            })?;
            values.push(v);
        }
        let is_float = matches!(layout, Layout::Float64);
        let w = layout.width_bytes();
        Ok(ColumnValues::Fixed {
            width: if w == 0 { width as usize } else { w },
            is_float,
            values,
        })
    }
}

fn dict_entry_var<'a>(doffs: &[u32], dbytes: &'a [u8], id: usize) -> Result<&'a [u8]> {
    if id + 1 >= doffs.len() {
        return Err(Error::Corrupt(format!("dict id {id} out of range")));
    }
    let s = doffs[id] as usize;
    let e = doffs[id + 1] as usize;
    dbytes
        .get(s..e)
        .ok_or_else(|| Error::Corrupt("dict entry range".into()))
}

fn take_u8(cur: &mut &[u8], what: &str) -> Result<u8> {
    if cur.is_empty() {
        return Err(Error::Corrupt(format!("rledict {what} truncated")));
    }
    let v = cur[0];
    *cur = &cur[1..];
    Ok(v)
}

fn take_u32(cur: &mut &[u8], what: &str) -> Result<u32> {
    if cur.len() < 4 {
        return Err(Error::Corrupt(format!("rledict {what} truncated")));
    }
    let v = u32::from_le_bytes([cur[0], cur[1], cur[2], cur[3]]);
    *cur = &cur[4..];
    Ok(v)
}
