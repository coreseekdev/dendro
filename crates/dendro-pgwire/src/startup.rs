//! startup / 认证阶段（SPEC 06 §2.1）：
//!
//! ```text
//! SSLRequest? → 'N'（拒绝） / GSSENCRequest? → 'N'（拒绝）
//! CancelRequest → 直接关闭
//! StartupMessage(protocol 3.x) → AuthenticationOk（trust）
//!     或 AuthenticationCleartextPassword → 密码比对 → OK / FATAL 28P01
//! → ParameterStatus×N → BackendKeyData{pid, secret} → ReadyForQuery('I')
//! ```
//!
//! 协议版本非 3.x → FATAL 28000（SPEC 06 §2.1 生命周期要求）。

use std::collections::HashMap;
use std::io;

use crate::codec::{BeMessage, FeMessage, PgStream, StartupPacket};
use crate::error::{self, codes};
use crate::PgConfig;

/// startup 参数（协议层保留给上层/日志用）
#[derive(Debug, Clone)]
pub struct StartupParams {
    pub protocol: i32,
    /// user 必有：startup 参数缺省时为 "dendro"
    pub user: String,
    pub params: HashMap<String, String>,
}

/// 会话参数宣告（任务规格固定集合；SPEC 06 §2.1）
const PARAM_STATUS: &[(&str, &str)] = &[
    ("server_version", "17.2 (dendro 0.1)"),
    ("server_encoding", "UTF8"),
    ("client_encoding", "UTF8"),
    ("DateStyle", "ISO, MDY"),
    ("integer_datetimes", "on"),
    ("standard_conforming_strings", "on"),
    ("TimeZone", "UTC"),
    ("is_superuser", "on"),
];

/// 处理 startup + 认证，直到发出第一条 ReadyForQuery。
///
/// 返回 `Ok(None)` 表示连接应关闭（CancelRequest / 认证失败 / 协议版本
/// 不支持 / 对端断开），错误响应已发出。
pub fn handshake<T: io::Read + io::Write>(
    pg: &mut PgStream<T>,
    cfg: &PgConfig,
) -> io::Result<Option<StartupParams>> {
    // SSL/GSS 协商可能重复出现（客户端在收到 'N' 后发真正的 startup）
    loop {
        let Some(pkt) = pg.read_startup()? else {
            return Ok(None);
        };
        match pkt {
            StartupPacket::SslRequest | StartupPacket::GssEncRequest => {
                // SPEC 06 §2.1：v1 不支持 TLS/GSS 加密，回应拒绝并等待明文 startup
                pg.write_raw(b"N");
                pg.flush()?;
            }
            StartupPacket::CancelRequest { .. } => {
                // SPEC 06 §2.2：v1 直接关闭连接
                return Ok(None);
            }
            StartupPacket::Startup { protocol, params } => {
                if (protocol >> 16) != 3 {
                    pg.send(&error::fatal_response(
                        codes::INVALID_AUTHORIZATION,
                        format!("unsupported frontend protocol {protocol}: server supports 3.x"),
                    ));
                    pg.flush()?;
                    return Ok(None);
                }
                let user = params
                    .get("user")
                    .cloned()
                    .unwrap_or_else(|| "dendro".to_string());

                // 认证：None=trust；Some=cleartext（SPEC 10 §8）
                if let Some(expected) = cfg.password.as_deref() {
                    pg.send(&BeMessage::AuthenticationCleartextPassword);
                    pg.flush()?;
                    match pg.read_message()? {
                        Some(FeMessage::PasswordMessage(pw)) if pw == expected => {}
                        Some(FeMessage::PasswordMessage(_)) => {
                            pg.send(&error::fatal_response(
                                codes::INVALID_PASSWORD,
                                format!("password authentication failed for user \"{user}\""),
                            ));
                            pg.flush()?;
                            return Ok(None);
                        }
                        _ => {
                            pg.send(&error::fatal_response(
                                codes::PROTOCOL_VIOLATION,
                                "expected PasswordMessage ('p')",
                            ));
                            pg.flush()?;
                            return Ok(None);
                        }
                    }
                }

                pg.send(&BeMessage::AuthenticationOk);
                for (name, value) in PARAM_STATUS {
                    pg.send(&BeMessage::ParameterStatus { name, value });
                }
                pg.send(&BeMessage::BackendKeyData {
                    pid: crate::next_backend_pid(),
                    secret: crate::next_backend_secret(),
                });
                pg.send(&BeMessage::ReadyForQuery(b'I'));
                pg.flush()?;
                return Ok(Some(StartupParams { protocol, user, params }));
            }
        }
    }
}
