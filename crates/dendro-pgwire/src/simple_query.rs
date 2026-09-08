//! 简单查询协议（'Q'，SPEC 06 §2.2 Query 行）。
//!
//! 整个查询串交给 `sess.exec()`（SPEC 10 §6：多语句切分由引擎负责，
//! wire 层不自行按 `;` 切）。每个 Output 发一组响应：
//! - `Output::Rows` → RowDescription + DataRow×N + CommandComplete("SELECT N")
//!   （引擎不带 tag，协议层按 PG 口径生成 "SELECT {total_rows}"）
//! - `Output::Command` → CommandComplete(tag)
//! - 空 Vec → EmptyQueryResponse
//! - 错误 → ErrorResponse 后照常 ReadyForQuery（连接保持，SPEC 10 §5）。

use std::io;

use dendro_core::types::{ColumnMeta, ColType, Output, RecordSet};

use crate::codec::{BeMessage, PgStream, RowField};
use crate::error;
use crate::param;

/// RowDescription 的 typlen（任务规格固定表；-2 = 变长）
pub(crate) fn typlen(ty: ColType) -> i16 {
    match ty {
        ColType::Bool => 1,
        ColType::Int32 => 4,
        ColType::Int64 => 8,
        ColType::Float64 => 8,
        ColType::Utf8 | ColType::Bytes => -2,
        ColType::Date32 => 4,
        ColType::TimestampMs => 8,
    }
}

/// 第 i 列的 format code：空/单元素列表应用到全部列，否则按位取
pub(crate) fn fmt_for(formats: &[i16], i: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(i).copied().unwrap_or(formats[0]),
    }
}

pub(crate) fn row_description(cols: &[ColumnMeta], formats: &[i16]) -> BeMessage {
    BeMessage::RowDescription(
        cols.iter()
            .enumerate()
            .map(|(i, c)| RowField {
                name: c.name.clone(),
                type_oid: c.ty.pg_oid(),
                typlen: typlen(c.ty),
                typmod: -1,
                format: fmt_for(formats, i),
            })
            .collect(),
    )
}

/// 把一行文本单元格编码为 DataRow 消息（按列 format）
pub(crate) fn data_row(cols: &[ColumnMeta], row: &[Option<String>], formats: &[i16]) -> BeMessage {
    let cells = row
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            let ty = cols.get(i).map(|c| c.ty).unwrap_or(ColType::Utf8);
            param::encode_cell(ty, cell.as_deref(), fmt_for(formats, i))
        })
        .collect();
    BeMessage::DataRow(cells)
}

/// 发送 DataRow×N + CommandComplete("SELECT {n}")。
///
/// 注意：RowDescription 只随 Describe/简单查询发送——扩展协议 Execute
/// 的行描述由 Describe 提供（PG 协议语义），这里不发。
pub(crate) fn emit_rows<T: io::Read + io::Write>(
    pg: &mut PgStream<T>,
    rs: &RecordSet,
    formats: &[i16],
) -> io::Result<()> {
    let n = rs.total_rows();
    for row in rs.text_rows() {
        pg.send(&data_row(&rs.columns, &row, formats));
    }
    // 引擎 Output::Rows 不带 tag，SELECT 的 tag 由协议层生成
    pg.send(&BeMessage::CommandComplete(format!("SELECT {n}")));
    Ok(())
}

/// 简单查询语境下的完整 Output 发送（Rows 带 RowDescription，format=0）
pub(crate) fn emit_output<T: io::Read + io::Write>(
    pg: &mut PgStream<T>,
    out: Output,
) -> io::Result<()> {
    match out {
        Output::Command { tag, .. } => {
            pg.send(&BeMessage::CommandComplete(tag));
            Ok(())
        }
        Output::Rows(rs) => {
            pg.send(&row_description(&rs.columns, &[]));
            emit_rows(pg, &rs, &[])
        }
    }
}

/// 处理一条 'Q'：执行 → 每语句响应 → ReadyForQuery → flush
pub(crate) fn handle_query<T: io::Read + io::Write>(
    pg: &mut PgStream<T>,
    sess: &mut dyn dendro_core::engine::WireSession,
    sql: &str,
) -> io::Result<()> {
    match sess.exec(sql) {
        Ok(outs) => {
            if outs.is_empty() {
                // 空串/纯注释（SPEC 10 §1：exec 空 Vec）
                pg.send(&BeMessage::EmptyQueryResponse);
            }
            for out in outs {
                emit_output(pg, out)?;
            }
        }
        Err(e) => {
            // ErrorResponse 后连接保持，最后仍 ReadyForQuery（SPEC 10 §5）
            pg.send(&error::error_response(&e));
        }
    }
    pg.send(&BeMessage::ReadyForQuery(sess.txn_status()));
    pg.flush()
}
