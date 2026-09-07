#![allow(clippy::type_complexity)]
//! dendro-columnar — CBF(Dendro Block Format) GPU 友好列存（SPEC 05）。
//!
//! 文件布局（SPEC 05 §2）：
//!
//! ```text
//! [RowGroup 0: ColChunk(col0), ColChunk(col1), ...]
//! [RowGroup 1 ...]
//! [块数据区全部 64B 对齐]
//! [Footer（自描述二进制）: RG 元数据表 + 文件级统计]
//! [footer_len u32][magic u32]      ← 文件尾 8B
//! ```
//!
//! Block 头 64B（SPEC 05 §3）：`magic u16=0xCB71, version u16=1, codec_id u8,
//! flags u8, rows u32, null_count u32, data_len u64, raw_len u64, min u64,
//! max u64, reserved u16, crc32c u32` + 12B 零填充。crc32c 覆盖 data 区。
//! validity 位图独立成区（64B 对齐、LSB-first、Arrow 同构），flags.bit0=1 时省略。
//!
//! codec（SPEC 05 §3 + SPEC 08 §5 实证）：
//! `RAW=0 / BITPACK=1 / RLE_DICT=2 / ZSTD=3 / DELTA=5`。
//!
//! # v1 相对 SPEC 05 的已文档化偏差/简化
//!
//! 1. 每 (row group, column) 写**单个 Block**（SPEC 允许 1..N 块；footer 保留
//!    `blocks[]` 数组与逐块 offset/rows/min/max，多块只是写入端扩展点）。
//!    默认行组 1,048,576 行 ⇒ i64 块恰为 SPEC 的 8MB 解码单元上限。
//! 2. Bool 在 Fixed 值域按 1 字节 0/1 存储（RAW raw_len = rows 字节），解码时
//!    再打包为 Arrow 位图——按位写扩散对 CPU/GPU 都不友好。
//! 3. 字符串 min/max 用前 8 字节大端序列（SPEC 05 §3 定点解释）。局限：>8B
//!    公共前缀的串会退化为相等键，只降低 zone map 剪枝精度，不影响正确性。
//!    （UTF-8 字节序 == 码点序，故 utf8 本身无序问题。）
//! 4. f64 min/max 采用 IEEE754 全序编码（符号位翻转）；NaN 的排序位置未定义。
//! 5. SPEC §2 文件级的 table_id / schema hash：本库无 catalog 上下文，改为
//!    **内嵌完整 schema**（列名 + Arrow 类型 + nullable），超集且自描述。
//!    pk min/max 取第 0 列（对应 SPEC §2 “顺序即布局”，列 0 视为 pk/排序列）。
//!
//! # GPU 友好性（SPEC 05 §3 保留机制）
//!
//! - RAW/BITPACK/RLE_DICT 解码是纯数据并行指针算术，无熵解码依赖；
//! - 64B 对齐 + validity 与数据分离 ⇒ 显存拷贝无需重排；
//! - footer (min,max,null_count) 常驻内存做 zone map 剪枝；
//! - ZSTD 只经 `codec_choice` 回调进入（冷块，SPEC 08 §1/§5）。

#![forbid(unsafe_code)]

#![allow(clippy::type_complexity)]

#![allow(clippy::all)]
pub mod codec;
pub mod footer;
pub mod reader;
pub mod stats;
pub mod writer;
pub mod integrate;

pub use codec::{choose_codec, CodecId};
pub use footer::{BlockMeta, CbfFooter, ChunkMeta, RgMeta};
pub use stats::ColStats;

/// 便利 re-export：codec 决策回调签名中的列类型来自 dendro-core。
pub use dendro_core::types::ColType;

pub type Result<T> = std::result::Result<T, Error>;

/// Block 头 magic（SPEC 05 §3）
pub const BLOCK_MAGIC: u16 = 0xCB71;
/// Block 头版本（SPEC 05 §3）
pub const BLOCK_VERSION: u16 = 1;
/// Block 头长度：52B 字段 + 12B 零填充（与 GPU segment / cache line 对齐，SPEC 05 §3）
pub const BLOCK_HEADER_LEN: usize = 64;
/// 块数据区 / validity 区 / footer 的对齐单位（SPEC 05 §2、§3）
pub const ALIGN: usize = 64;
/// 默认行组行数（SPEC 05 §2，tonbo 同款 1,048,576）
pub const DEFAULT_ROW_GROUP_ROWS: usize = 1_048_576;
/// codec 选择采样上限：首行组前 64K 行（SPEC 05 §4）
pub const SAMPLE_ROWS: usize = 65_536;
/// 冷块 zstd 默认级别（SPEC 08 §5：解码吞吐对 level 不敏感，L3 为性价比甜点）
pub const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("corrupt CBF data: {0}")]
    Corrupt(String),
    #[error("unsupported arrow type: {0}")]
    UnsupportedArrowType(String),
    #[error("codec {0:?} not applicable to this column type")]
    CodecNotApplicable(CodecId),
    #[error("zstd: {0}")]
    Zstd(String),
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
}

/// 把一组 Arrow RecordBatch（同一 schema）编码为一个 CBF 文件字节串。
///
/// - `row_group_rows`：每个行组行数（传 0 = 默认 1_048_576，见 [`DEFAULT_ROW_GROUP_ROWS`]）。
/// - `codec_choice`：每列的 codec 决策回调（列名 / ColType / 采样统计 → CodecId）；
///   `None` = 内置自适应策略 [`choose_codec`]（SPEC 05 §4 + SPEC 08 §5 实证规则）。
///   采样口径：首行组前 64K 行（[`SAMPLE_ROWS`]）；决策对全文件该列生效。
pub fn write_cbf(
    batches: &[RecordBatch],
    row_group_rows: usize,
    codec_choice: Option<&dyn Fn(&str, ColType, &ColStats) -> CodecId>,
) -> Result<Vec<u8>> {
    writer::write_cbf(batches, row_group_rows, codec_choice)
}

/// 读取 CBF 字节串 → 全部行组（含 footer 与每块 crc32c 校验）。v1 整文件解码；
/// range-get 友好的部分读取见 [`read_footer`] + [`read_column_chunk`]。
pub fn read_cbf(data: &[u8]) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    reader::read_cbf(data)
}

/// 只读 footer/元数据（文件尾 range get 即可），供剪枝：返回每行组每列的
/// (row_group_idx, rows, min, max, null_count, codec_id)（见 [`CbfFooter`]）
/// 与文件级 row range（`pk_min`/`pk_max`，第 0 列定点解释）。
pub fn read_footer(data: &[u8]) -> Result<CbfFooter> {
    reader::read_footer(data)
}

/// 解码指定行组的指定列（zone map 剪枝后的按需读取）。
pub fn read_column_chunk(data: &[u8], footer: &CbfFooter, rg: usize, col: usize) -> Result<ArrayRef> {
    reader::read_column_chunk(data, footer, rg, col)
}

use arrow::array::{ArrayRef, RecordBatch};
use arrow::datatypes::SchemaRef;
