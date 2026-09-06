//! ZSTD codec（id=3）— SPEC 05 §3：整块 zstd，只用于冷块（SPEC 05 §3 GPU 保留
//! 机制 5：热/GPU 路径永不过熵解码器）。级别存于块头 flags 高位（>>2），本库
//! 统一用级别 3（SPEC 08 §5：编码吞吐 3→9 掉 70-80%，L3 为性价比甜点）。

use crate::{Error, Result};

pub(crate) fn compress(input: &[u8], level: i32) -> Result<Vec<u8>> {
    zstd::bulk::compress(input, level).map_err(|e| Error::Zstd(e.to_string()))
}

pub(crate) fn decompress(input: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let out = zstd::bulk::decompress(input, raw_len).map_err(|e| Error::Zstd(e.to_string()))?;
    if out.len() != raw_len {
        return Err(Error::Corrupt(format!(
            "zstd output {} != raw_len {raw_len}",
            out.len()
        )));
    }
    Ok(out)
}
