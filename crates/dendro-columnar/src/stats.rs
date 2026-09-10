//! 列统计与 zone map 定点解释（SPEC 05 §3 min/max、§4 采样、§7 剪枝管线）。

use crate::codec::{chunk_of, ChunkPart, Layout};
use crate::Result;
use arrow::array::ArrayRef;

/// 列/块统计。`min`/`max` 为**定点解释（order 域）**（SPEC 05 §3）：
/// - 数值列：保序变换后的物理值（i64/i32 符号位翻转；f64 IEEE754 全序）；
/// - 字符串列：前 8 字节大端序列（>8B 公共前缀退化为等值键，剪枝精度近似）。
#[derive(Debug, Clone, PartialEq)]
pub struct ColStats {
    pub rows: usize,
    pub null_count: usize,
    /// 非空值去重计数（chunk 内精确值，非估计）
    pub distinct: usize,
    pub min: u64,
    pub max: u64,
    /// 非空序列非递减（前缀有序 → 块 flags.sorted，供 binary search / PGM）
    pub monotonic: bool,
    /// 变宽列非空值平均字节长（FSST 决策用：过短串摊不平符号表查找）；
    /// 定宽列恒 0.0
    pub avg_len: f64,
}

impl ColStats {
    /// 对整个数组做精确统计（codec 决策采样时由 writer 截取 ≤64K 行切片）。
    pub fn of(array: &ArrayRef) -> Result<ColStats> {
        let layout = crate::codec::layout_of(array.data_type())?;
        let part = chunk_of(array)?;
        Ok(Self::from_chunk(&part, &layout))
    }

    pub(crate) fn from_chunk(part: &ChunkPart, layout: &Layout) -> ColStats {
        let rows = part.rows();
        let mut st = ColStats {
            rows,
            null_count: 0,
            distinct: 0,
            min: 0,
            max: 0,
            monotonic: true,
            avg_len: 0.0,
        };
        let mut min_set = false;
        let mut prev_key = 0u64;
        let mut prev_bytes: &[u8] = &[];
        let mut var_total_len = 0usize;
        match &part.vals {
            crate::codec::ColumnValues::Fixed { values, .. } => {
                let mut seen = std::collections::HashSet::with_capacity(values.len() / 4 + 1);
                for (i, &v) in values.iter().enumerate() {
                    if !part.is_valid(i) {
                        continue;
                    }
                    let k = order_key(layout, v);
                    seen.insert(v);
                    if !min_set {
                        min_set = true;
                        st.min = k;
                        st.max = k;
                    } else {
                        st.min = st.min.min(k);
                        st.max = st.max.max(k);
                        if k < prev_key {
                            st.monotonic = false;
                        }
                    }
                    prev_key = k;
                }
                st.distinct = seen.len();
            }
            crate::codec::ColumnValues::Var { offsets, bytes } => {
                let n = offsets.len().saturating_sub(1);
                let mut seen = std::collections::HashSet::with_capacity(n / 4 + 1);
                for i in 0..n {
                    if !part.is_valid(i) {
                        continue;
                    }
                    let s = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                    seen.insert(s);
                    var_total_len += s.len();
                    let k = order_key_bytes(s);
                    if !min_set {
                        min_set = true;
                        st.min = k;
                        st.max = k;
                    } else {
                        st.min = st.min.min(k);
                        st.max = st.max.max(k);
                        // 变宽排序按完整字节序判定（order key 仅前 8B 近似）
                        if s < prev_bytes {
                            st.monotonic = false;
                        }
                    }
                    prev_bytes = s;
                }
                st.distinct = seen.len();
            }
        }
        st.null_count = part.null_count();
        let valid = st.rows.saturating_sub(st.null_count);
        st.avg_len = if matches!(layout, Layout::Var) && valid > 0 {
            var_total_len as f64 / valid as f64
        } else {
            0.0
        };
        st
    }

    /// distinct / 非空行数（全空列记 0，使规则自然落入 RLE_DICT/全 null 路径）
    pub fn distinct_ratio(&self) -> f64 {
        let non_null = self.rows.saturating_sub(self.null_count).max(1);
        self.distinct as f64 / non_null as f64
    }
}

/// 数值列 order 域 key（保序）：i64/i32 符号位翻转；f64 IEEE754 全序；
/// bool 0/1 原样。NaN 排序位置未定义（见 lib.rs 偏差 4）。
pub(crate) fn order_key(layout: &Layout, v: u64) -> u64 {
    match layout {
        Layout::Int { width } => match width {
            1 => v,
            4 => ((v as u32 as i32 as i64) as u64) ^ (1 << 63),
            _ => v ^ (1 << 63),
        },
        Layout::Float64 => {
            if v & (1 << 63) == 0 {
                v | (1 << 63)
            } else {
                !v
            }
        }
        Layout::Var => v, // 不走此路径；变宽用 order_key_bytes
    }
}

/// 字符串列 order 域 key：前 8 字节大端，零填充（SPEC 05 §3 定点解释）。
pub(crate) fn order_key_bytes(b: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let n = b.len().min(8);
    buf[..n].copy_from_slice(&b[..n]);
    u64::from_be_bytes(buf)
}

/// 供 footer/块头写出的 min/max/sorted（跳过 null；全空 → min=max=0, sorted=true）
pub(crate) fn order_minmax(part: &ChunkPart, layout: &Layout) -> (u64, u64, bool) {
    let st = ColStats::from_chunk(part, layout);
    (st.min, st.max, st.monotonic)
}
