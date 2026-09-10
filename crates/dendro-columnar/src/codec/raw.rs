//! RAW codec（id=0）— SPEC 05 §3：定宽列 rows × width 连续（GPU 直接出定宽
//! buffer）；变宽列 offsets(u32)×(rows+1) + bytes；bool 按 1 字节 0/1（见 lib.rs
//! 偏差 2）。null 槽位值已归零/空（见 `chunk_of`）。

use super::{ChunkPart, ColumnValues, Layout};
use crate::{Error, Result};

pub(crate) fn encode(part: &ChunkPart, out: &mut Vec<u8>) -> usize {
    let l0 = out.len();
    match &part.vals {
        ColumnValues::Fixed { width, values, .. } => {
            for &v in values {
                out.extend_from_slice(&v.to_le_bytes()[..*width]);
            }
        }
        ColumnValues::Var { offsets, bytes } => {
            for &o in offsets {
                out.extend_from_slice(&o.to_le_bytes());
            }
            out.extend_from_slice(bytes);
        }
    }
    out.len() - l0
}

pub(crate) fn decode(
    data: &[u8],
    raw_len: usize,
    rows: usize,
    layout: &Layout,
) -> Result<ColumnValues> {
    if data.len() != raw_len {
        return Err(Error::Corrupt(format!(
            "raw data_len {} != raw_len {}",
            data.len(),
            raw_len
        )));
    }
    match layout {
        Layout::Int { .. } | Layout::Float64 => {
            let width = layout.width_bytes();
            let is_float = matches!(layout, Layout::Float64);
            let need = rows * width;
            if data.len() != need {
                return Err(Error::Corrupt(format!(
                    "raw fixed expect {need} bytes, got {}",
                    data.len()
                )));
            }
            let mut values = Vec::with_capacity(rows);
            for i in 0..rows {
                let b = &data[i * width..(i + 1) * width];
                let mut v = 0u64;
                for &byte in b.iter().rev() {
                    v = (v << 8) | byte as u64;
                }
                values.push(v);
            }
            Ok(ColumnValues::Fixed {
                width,
                is_float,
                values,
            })
        }
        Layout::Var => {
            let off_bytes = (rows + 1) * 4;
            if data.len() < off_bytes {
                return Err(Error::Corrupt("raw var offsets truncated".into()));
            }
            let mut offsets = Vec::with_capacity(rows + 1);
            for i in 0..=rows {
                let b = &data[i * 4..i * 4 + 4];
                offsets.push(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            }
            let last = *offsets.last().unwrap() as usize;
            if last != data.len() - off_bytes {
                return Err(Error::Corrupt("raw var bytes length mismatch".into()));
            }
            Ok(ColumnValues::Var {
                offsets,
                bytes: data[off_bytes..].to_vec(),
            })
        }
    }
}
