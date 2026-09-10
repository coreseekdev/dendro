//! 扩展查询协议：Parse/Bind/Describe/Execute/Close/Flush/Sync
//! （SPEC 06 §2.2 P/B/D/E/C/H/S 行；SPEC 10 §2 映射约定）。
//!
//! 状态：
//! - `prepared`: name → PrepareMeta（Parse 时由引擎返回并缓存）
//! - `portals`:  name → Portal（Bind 产物；portal 与 statement 同列集，SPEC 10 §2）
//! - `in_error`: PG 语义的“出错跳过”模式——ErrorResponse 发出后，
//!   到 Sync 之前的 P/B/D/E/C/H 一律忽略，Sync 时只发 ReadyForQuery。
//!
//! Execute 忽略 max_rows 分片（SPEC 10 §2：v1 无 PortalSuspended），全量执行。

use std::collections::HashMap;
use std::io;

use dendro_core::engine::{PrepareMeta, WireSession};
use dendro_core::error::SqlError;
use dendro_core::types::{ColType, Output, SqlValue};

use crate::codec::{BeMessage, FeMessage, PgStream};
use crate::error::{self, codes};
use crate::param;
use crate::simple_query;

/// 协议层识别的 PG 类型 OID（SPEC 06 §2.3）；其余宽松处理为无 hint
fn oid_to_col_type(oid: u32) -> Option<ColType> {
    Some(match oid {
        16 => ColType::Bool,
        20 => ColType::Int64,
        23 => ColType::Int32,
        25 => ColType::Utf8,
        17 => ColType::Bytes,
        701 => ColType::Float64,
        1082 => ColType::Date32,
        1114 => ColType::TimestampMs,
        _ => return None, // 宽松：未知 oid 不给 hint（含 0 = unspecified）
    })
}

/// Bind 建立的 portal（Execute 时消费；可重复执行）
struct Portal {
    stmt: String,
    params: Vec<SqlValue>,
    result_formats: Vec<i16>,
}

/// 扩展协议状态（每连接一份）
#[derive(Default)]
pub struct ExtendedState {
    prepared: HashMap<String, PrepareMeta>,
    portals: HashMap<String, Portal>,
    pub(crate) in_error: bool,
}

impl ExtendedState {
    fn unknown_statement(name: &str) -> SqlError {
        SqlError::new(
            codes::INVALID_STATEMENT_NAME,
            format!("prepared statement \"{name}\" does not exist"),
        )
    }
    fn unknown_portal(name: &str) -> SqlError {
        SqlError::new(
            codes::INVALID_STATEMENT_NAME,
            format!("portal \"{name}\" does not exist"),
        )
    }
}

/// 单条消息处理结果：Continue 正常，Close 表示客户端请求断开（Terminate）
pub(crate) enum Flow {
    Continue,
    Close,
}

pub(crate) fn handle_message<T: io::Read + io::Write>(
    pg: &mut PgStream<T>,
    sess: &mut dyn WireSession,
    st: &mut ExtendedState,
    msg: FeMessage,
) -> io::Result<Flow> {
    // 出错跳过模式：直到 Sync 之前只吞掉消息（SPEC 06 §2.2）
    if st.in_error {
        return Ok(match msg {
            FeMessage::Sync => {
                st.in_error = false;
                pg.send(&BeMessage::ReadyForQuery(sess.txn_status()));
                pg.flush()?;
                Flow::Continue
            }
            FeMessage::Terminate => Flow::Close,
            _ => Flow::Continue, // P/B/D/E/C/H/Q? 全部忽略
        });
    }

    match msg {
        FeMessage::Parse {
            name,
            sql,
            param_oids,
        } => {
            // 参数类型 hint：只认 SPEC 06 §2.3 的 OID，未知宽松跳过
            let hint: Vec<ColType> = param_oids
                .iter()
                .filter_map(|o| oid_to_col_type(*o))
                .collect();
            match sess.prepare(&name, &sql, &hint) {
                Ok(meta) => {
                    // UNNAMED 语句重复 Parse 覆盖旧的（SPEC 10 §2）；具名同理
                    st.prepared.insert(name, meta);
                    pg.send(&BeMessage::ParseComplete);
                }
                Err(e) => {
                    st.in_error = true;
                    pg.send(&error::error_response(&e));
                }
            }
        }
        FeMessage::Bind {
            portal,
            stmt,
            param_formats,
            params,
            result_formats,
        } => {
            let types = st.prepared.get(&stmt).map(|m| m.param_types.clone());
            let Some(types) = types else {
                st.in_error = true;
                pg.send(&error::error_response(&ExtendedState::unknown_statement(
                    &stmt,
                )));
                return Ok(Flow::Continue);
            };
            // 参数格式：0=text / 1=binary；未指定列按 hint 类型（缺省 text）
            let mut vals = Vec::with_capacity(params.len());
            let mut decode_err = None;
            for (i, raw) in params.iter().enumerate() {
                let ty = types.get(i).copied().unwrap_or(ColType::Utf8);
                match param::decode_param(
                    simple_query::fmt_for(&param_formats, i),
                    ty,
                    raw.as_deref(),
                ) {
                    Ok(v) => vals.push(v),
                    Err(e) => {
                        decode_err = Some(e);
                        break;
                    }
                }
            }
            match decode_err {
                Some(e) => {
                    st.in_error = true;
                    pg.send(&error::error_response(&e));
                }
                None => {
                    st.portals.insert(
                        portal,
                        Portal {
                            stmt,
                            params: vals,
                            result_formats,
                        },
                    );
                    pg.send(&BeMessage::BindComplete);
                }
            }
        }
        FeMessage::Describe { kind, name } => match kind {
            b'S' => match st.prepared.get(&name) {
                Some(meta) => {
                    pg.send(&BeMessage::ParameterDescription(
                        meta.param_types.iter().map(|t| t.pg_oid()).collect(),
                    ));
                    // Describe 'S' 结果固定 text 格式（客户端 Bind 时再协商）
                    pg.send(&simple_query::row_description(&meta.result_columns, &[]));
                }
                None => {
                    // SPEC 10 §2：Describe 未知名 → 26000
                    st.in_error = true;
                    pg.send(&error::error_response(&ExtendedState::unknown_statement(
                        &name,
                    )));
                }
            },
            b'P' => {
                let described = st.portals.get(&name).and_then(|p| {
                    st.prepared
                        .get(&p.stmt)
                        .map(|m| (m.result_columns.clone(), p.result_formats.clone()))
                });
                match described {
                    Some((cols, formats)) => {
                        pg.send(&simple_query::row_description(&cols, &formats));
                    }
                    None => {
                        st.in_error = true;
                        pg.send(&error::error_response(&ExtendedState::unknown_portal(
                            &name,
                        )));
                    }
                }
            }
            other => {
                st.in_error = true;
                pg.send(&error::error_response(&SqlError::new(
                    codes::PROTOCOL_VIOLATION,
                    format!("invalid Describe kind: {}", char::from(other)),
                )));
            }
        },
        FeMessage::Execute {
            portal,
            max_rows: _,
        } => {
            // v1 忽略 max_rows 分片：全量执行 + CommandComplete（SPEC 10 §2）
            let Some(p) = st.portals.get(&portal) else {
                st.in_error = true;
                pg.send(&error::error_response(&ExtendedState::unknown_portal(
                    &portal,
                )));
                return Ok(Flow::Continue);
            };
            let stmt: &str = &p.stmt;
            match sess.exec_prepared(stmt, &p.params) {
                Ok(Output::Command { tag, .. }) => {
                    pg.send(&BeMessage::CommandComplete(tag));
                }
                Ok(Output::Rows(mut rs)) => {
                    // 列型对齐：以 Parse 时的描述口径为准（执行期按数据推断会窄化，
                    // 如 i64 值 1 落成 Int32 → binary 编码 4B，客户端按 int8 解必炸）
                    if let Some(meta) = st.prepared.get(&p.stmt) {
                        if meta.result_columns.len() == rs.columns.len() {
                            rs.columns = meta.result_columns.clone();
                        }
                    }
                    simple_query::emit_rows(pg, &rs, &p.result_formats)?;
                }
                Err(e) => {
                    st.in_error = true;
                    pg.send(&error::error_response(&e));
                }
            }
        }
        FeMessage::Close { kind, name } => {
            match kind {
                b'S' => {
                    // SPEC 10 §2：close_prepared 未知名静默
                    sess.close_prepared(&name);
                    st.prepared.remove(&name);
                }
                b'P' => {
                    st.portals.remove(&name);
                }
                _ => {} // 其他 kind 容错：仍然 CloseComplete
            }
            pg.send(&BeMessage::CloseComplete);
        }
        FeMessage::Flush => {
            pg.flush()?;
        }
        FeMessage::Sync => {
            // 事务边界结算点：发 ReadyForQuery（txn 状态由引擎给出，SPEC 10 §5）
            pg.send(&BeMessage::ReadyForQuery(sess.txn_status()));
            pg.flush()?;
        }
        FeMessage::Terminate => return Ok(Flow::Close),
        FeMessage::PasswordMessage(_) => {
            // established 后不应再出现认证消息 → 致命协议错误
            pg.send(&error::fatal_response(
                codes::PROTOCOL_VIOLATION,
                "unexpected PasswordMessage in established state",
            ));
            pg.flush()?;
            return Ok(Flow::Close);
        }
        FeMessage::Copy { tag } => {
            // SPEC 06 §2.2：CopyIn/Out → 0A000 feature_not_supported
            let _ = tag;
            pg.send(&error::error_response(&SqlError::not_supported(
                "COPY sub-protocol is not supported",
            )));
        }
        FeMessage::Query(sql) => {
            // 防御路径（lib.rs 直接路由 'Q'，正常不会到这里）
            simple_query::handle_query(pg, sess, &sql)?;
        }
    }
    Ok(Flow::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_oids_map_to_col_types() {
        assert_eq!(oid_to_col_type(16), Some(ColType::Bool));
        assert_eq!(oid_to_col_type(20), Some(ColType::Int64));
        assert_eq!(oid_to_col_type(23), Some(ColType::Int32));
        assert_eq!(oid_to_col_type(25), Some(ColType::Utf8));
        assert_eq!(oid_to_col_type(17), Some(ColType::Bytes));
        assert_eq!(oid_to_col_type(701), Some(ColType::Float64));
        assert_eq!(oid_to_col_type(1082), Some(ColType::Date32));
        assert_eq!(oid_to_col_type(1114), Some(ColType::TimestampMs));
        assert_eq!(oid_to_col_type(0), None); // unspecified
        assert_eq!(oid_to_col_type(9999), None); // 宽松：未知不报 08P01
    }
}
