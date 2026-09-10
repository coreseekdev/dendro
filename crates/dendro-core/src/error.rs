use thiserror::Error;

/// SQL 层错误：携带 SQLSTATE（PG/MySQL 协议层共用映射，SPEC 06 §2.4）
#[derive(Error, Debug, Clone)]
#[error("{message}")]
pub struct SqlError {
    pub state: &'static str,
    pub message: String,
}

impl SqlError {
    pub fn new(state: &'static str, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
        }
    }
    pub fn syntax(msg: impl Into<String>) -> Self {
        Self::new("42601", msg)
    }
    pub fn undefined_table(msg: impl Into<String>) -> Self {
        Self::new("42P01", msg)
    }
    pub fn undefined_column(msg: impl Into<String>) -> Self {
        Self::new("42703", msg)
    }
    pub fn duplicate_table(msg: impl Into<String>) -> Self {
        Self::new("42P07", msg)
    }
    pub fn duplicate_key(msg: impl Into<String>) -> Self {
        Self::new("23505", msg)
    }
    pub fn serialization(msg: impl Into<String>) -> Self {
        Self::new("40001", msg)
    }
    pub fn not_supported(msg: impl Into<String>) -> Self {
        Self::new("0A000", msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new("XX000", msg)
    }
    pub fn invalid_text(msg: impl Into<String>) -> Self {
        Self::new("22P02", msg)
    }
    pub fn datatype_mismatch(msg: impl Into<String>) -> Self {
        Self::new("42804", msg)
    }
    pub fn division_by_zero() -> Self {
        Self::new("22012", "division by zero")
    }
    pub fn undefined_branch(msg: impl Into<String>) -> Self {
        Self::new("3D000", msg)
    }
    pub fn io(msg: impl Into<String>) -> Self {
        Self::new("58030", msg)
    }
}

pub type Result<T> = std::result::Result<T, SqlError>;

impl From<std::io::Error> for SqlError {
    fn from(e: std::io::Error) -> Self {
        SqlError::io(e.to_string())
    }
}

impl From<crate::objstore::ObjError> for SqlError {
    fn from(e: crate::objstore::ObjError) -> Self {
        SqlError::io(e.to_string())
    }
}
