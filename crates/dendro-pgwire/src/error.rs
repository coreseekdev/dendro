//! ErrorResponse 构造（SPEC 06 §2.4：内部 SqlError → severity/code/message 三字段）。
//!
//! severity 统一 ERROR（SPEC 10 §4）；连接致命类（认证失败/协议不支持）
//! 由 wire 层自判为 FATAL 并断连。

use crate::codec::BeMessage;
use dendro_core::error::SqlError;

/// 语句执行错误：severity=ERROR，连接保持
pub fn error_response(e: &SqlError) -> BeMessage {
    BeMessage::ErrorResponse {
        severity: "ERROR",
        code: e.state,
        message: e.message.clone(),
    }
}

/// 致命错误（startup/认证/协议违规）：severity=FATAL，调用方随后断开连接
pub fn fatal_response(code: &'static str, message: impl Into<String>) -> BeMessage {
    BeMessage::ErrorResponse {
        severity: "FATAL",
        code,
        message: message.into(),
    }
}

/// 常用 SQLSTATE（SPEC 06 §2.4 之外、协议层自产的错误码）
pub mod codes {
    /// protocol_violation
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    /// invalid_authorization_specification（协议版本不支持）
    pub const INVALID_AUTHORIZATION: &str = "28000";
    /// invalid_password
    pub const INVALID_PASSWORD: &str = "28P01";
    /// invalid_sql_statement_name（未知 prepared statement / portal）
    pub const INVALID_STATEMENT_NAME: &str = "26000";
    /// feature_not_supported（COPY 等）
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
}
