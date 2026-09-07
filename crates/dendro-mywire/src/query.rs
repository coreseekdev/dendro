//! 命令分发与 text 结果集编码 — SPEC 06 §3「命令 / COM_QUERY」。
//!
//! 命令矩阵：COM_QUIT / COM_INIT_DB / COM_QUERY / COM_FIELD_LIST / COM_STATISTICS /
//! COM_PING；COM_STMT_PREPARE 与未知命令 → ERR 1047 "08S01"（ER_UNKNOWN_COM_ERROR）。
//!
//! COM_QUERY：整段 SQL 交给 `sess.exec()`（多语句切分在引擎内，SPEC 10 §6；
//! CLIENT_MULTI_STATEMENTS 下客户端也是逐条发 COM_QUERY）。多个 Output 时
//! 除最后一个外置 SERVER_MORE_RESULTS_EXISTS，让客户端继续读。

use crate::codec::{
    eof_packet, err_packet, ok_packet, write_lenenc_int, write_lenenc_str, Reader, WireIo,
    SERVER_MORE_RESULTS_EXISTS, SERVER_STATUS_AUTOCOMMIT, SERVER_STATUS_IN_TRANS,
};
use crate::column_def::column_def_bytes;
use crate::MyConfig;
use dendro_core::engine::WireSession;
use dendro_core::error::SqlError;
use dendro_core::types::{ColType, ColumnMeta, Output, RecordSet};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

// 命令字节
const COM_QUIT: u8 = 0x01;
const COM_INIT_DB: u8 = 0x02;
const COM_QUERY: u8 = 0x03;
const COM_FIELD_LIST: u8 = 0x04;
const COM_STATISTICS: u8 = 0x09;
const COM_PING: u8 = 0x0e;
const COM_STMT_PREPARE: u8 = 0x16;

const NULL_CELL: u8 = 0xFB;

/// 全局命令计数（COM_STATISTICS 用）
static QUESTIONS: AtomicU64 = AtomicU64::new(0);

/// 分发一个命令包（payload 含命令字节）。返回 `true` = 继续命令循环，
/// `false` = 关闭连接（COM_QUIT / 空包）。
pub fn dispatch<T: Read + Write>(
    io: &mut WireIo<T>,
    sess: &mut dyn WireSession,
    payload: &[u8],
    cfg: &MyConfig,
) -> io::Result<bool> {
    let Some(&cmd) = payload.first() else {
        io.write_packet(&err_packet(1047, "08S01", "empty command packet"))?;
        return Ok(false);
    };
    QUESTIONS.fetch_add(1, Ordering::Relaxed);
    let body = &payload[1..];
    match cmd {
        COM_QUIT => return Ok(false),
        COM_INIT_DB => {
            // v1 单库：一律接受（不校验库名）
            io.write_packet(&ok_packet(0, 0, status(sess, false), None))?;
        }
        COM_QUERY => com_query(io, sess, body, cfg)?,
        COM_FIELD_LIST => field_list(io, sess, body)?,
        COM_STATISTICS => {
            let text = statistics_text();
            io.write_packet(text.as_bytes())?; // 原始字符串（非 OK/ERR 包）
        }
        COM_PING => {
            io.write_packet(&ok_packet(0, 0, status(sess, false), None))?;
        }
        COM_STMT_PREPARE => {
            // SPEC 06 §3「不支持」：JDBC 需 useServerPrepStmts=false + useLocalSessionState=true
            io.write_packet(&err_packet(
                1047,
                "08S01",
                "prepared statements not supported, set useServerPrepStmts=false",
            ))?;
        }
        _ => {
            io.write_packet(&err_packet(1047, "08S01", "unknown command"))?;
        }
    }
    Ok(true)
}

/// 会话当前 status flags：AUTOCOMMIT 基线 + 事务态（SPEC 10 §5）+ 多结果集提示
fn status(sess: &dyn WireSession, more: bool) -> u16 {
    let mut s = SERVER_STATUS_AUTOCOMMIT;
    if sess.txn_status() == b'T' {
        s |= SERVER_STATUS_IN_TRANS;
    }
    if more {
        s |= SERVER_MORE_RESULTS_EXISTS;
    }
    s
}

fn com_query<T: Read + Write>(
    io: &mut WireIo<T>,
    sess: &mut dyn WireSession,
    body: &[u8],
    cfg: &MyConfig,
) -> io::Result<()> {
    let sql = String::from_utf8_lossy(body);
    // 客户端库（mysql rust/JDBC）连接初始化常查 @@ 系统变量 —— 协议层的自有
    // 变量（max_allowed_packet/socket/version 等）直接应答，不经引擎
    let trimmed = sql.trim();
    if let Some(name) = trimmed
        .strip_prefix("SELECT @@")
        .or_else(|| trimmed.strip_prefix("select @@"))
    {
        let name = name.trim();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            if let Some((ty, value)) = sysvar_value(cfg, name) {
                write_sysvar_row(io, sess, name, ty, &value)?;
                return Ok(());
            }
        }
    }
    match sess.exec(&sql) {
        Ok(outs) => {
            if outs.is_empty() {
                // 空串/纯注释 → OK（affected 0）
                io.write_packet(&ok_packet(0, 0, status(sess, false), None))?;
                return Ok(());
            }
            let n = outs.len();
            for (i, out) in outs.iter().enumerate() {
                let more = i + 1 < n;
                match out {
                    Output::Rows(rs) => write_result_set(io, sess, rs, more)?,
                    // tag 为 PG 口径（"INSERT 0 2"），OK 包只携带 affected 数值，
                    // tag 作为 info 字段附带（客户端可用于回显）
                    Output::Command { tag, affected } => {
                        io.write_packet(&ok_packet(*affected, 0, status(sess, more), Some(tag)))?;
                    }
                }
            }
        }
        Err(e) => {
            io.write_packet(&err_from(&e))?;
        }
    }
    Ok(())
}

/// text 结果集：column_count lenenc → ColumnDefinition41×N → EOF → 行×M → EOF
fn write_result_set<T: Read + Write>(
    io: &mut WireIo<T>,
    sess: &dyn WireSession,
    rs: &RecordSet,
    more: bool,
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(9);
    write_lenenc_int(&mut buf, rs.columns.len() as u64);
    io.write_packet(&buf)?;
    for col in &rs.columns {
        io.write_packet(&column_def_bytes(col))?;
    }
    // 未宣告 CLIENT_DEPRECATE_EOF → 列定义之后先发一个 EOF
    io.write_packet(&eof_packet(status(sess, more)))?;
    for row in rs.text_rows() {
        let mut buf = Vec::new();
        for cell in row {
            match cell {
                Some(s) => write_lenenc_str(&mut buf, s.as_bytes()),
                None => buf.push(NULL_CELL),
            }
        }
        io.write_packet(&buf)?;
    }
    io.write_packet(&eof_packet(status(sess, more)))?;
    Ok(())
}

/// 协议层自有系统变量（名称不含 @@ 前缀）。未收录的交引擎处理。
fn sysvar_value(cfg: &MyConfig, name: &str) -> Option<(ColType, String)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "max_allowed_packet" => (ColType::Int64, cfg.max_allowed_packet.to_string()),
        "socket" => (ColType::Utf8, String::new()), // v1 仅 TCP，无 unix socket
        "version" => (ColType::Utf8, cfg.server_version.clone()),
        "version_comment" => (ColType::Utf8, "dendro".into()),
        _ => return None,
    })
}

/// 单行单列系统变量结果集（协议层自有配置直接应答用）
fn write_sysvar_row<T: Read + Write>(
    io: &mut WireIo<T>,
    sess: &dyn WireSession,
    name: &str,
    ty: ColType,
    value: &str,
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4);
    write_lenenc_int(&mut buf, 1);
    io.write_packet(&buf)?;
    io.write_packet(&column_def_bytes(&ColumnMeta { name: name.into(), ty }))?;
    io.write_packet(&eof_packet(status(sess, false)))?;
    let mut row = Vec::with_capacity(value.len() + 9);
    write_lenenc_str(&mut row, value.as_bytes());
    io.write_packet(&row)?;
    io.write_packet(&eof_packet(status(sess, false)))
}

/// COM_FIELD_LIST：按表名列出 ColumnDefinition41×N + EOF；
/// 表不存在 → ERR 1146 "42S02"（ER_NO_SUCH_TABLE）。
/// 列元数据来自 `SELECT * FROM \`t\` LIMIT 0` 的 Rows 输出。
fn field_list<T: Read + Write>(
    io: &mut WireIo<T>,
    sess: &mut dyn WireSession,
    body: &[u8],
) -> io::Result<()> {
    let mut r = Reader::new(body);
    let table = match r.nul_terminated() {
        Ok(t) => String::from_utf8_lossy(t).into_owned(),
        Err(_) => {
            io.write_packet(&err_packet(1047, "08S01", "malformed COM_FIELD_LIST payload"))?;
            return Ok(());
        }
    };
    let table = table.trim_matches('`');
    let sql = format!("SELECT * FROM `{}` LIMIT 0", table.replace('`', "``"));
    match sess.exec(&sql) {
        Ok(outs) => {
            let Some(rs) = outs.into_iter().find_map(|o| match o {
                Output::Rows(rs) => Some(rs),
                _ => None,
            }) else {
                io.write_packet(&err_packet(1105, "HY000", "table metadata unavailable"))?;
                return Ok(());
            };
            for col in &rs.columns {
                io.write_packet(&column_def_bytes(col))?;
            }
            io.write_packet(&eof_packet(status(sess, false)))?;
        }
        Err(e) => io.write_packet(&err_from(&e))?, // 42P01 → 1146/42S02
    }
    Ok(())
}

/// COM_STATISTICS：一行 Tab 分隔的统计文本（Uptime\tThreads\tQuestions\t...）
fn statistics_text() -> String {
    static START: OnceLock<Instant> = OnceLock::new();
    let uptime = START.get_or_init(Instant::now).elapsed().as_secs();
    let q = QUESTIONS.load(Ordering::Relaxed);
    let qps = if uptime == 0 { q as f64 } else { q as f64 / uptime as f64 };
    format!(
        "Uptime\t{}\tThreads\t1\tQuestions\t{}\tSlow queries\t0\tOpens\t0\t\
         Flush tables\t1\tOpen tables\t0\tQueries per second avg\t{qps:.3}",
        uptime, q
    )
}

/// SqlError → MySQL (ER code, SQLSTATE)。SQLSTATE 与 PG 层共用一套内部码
/// （SPEC 06 §3「错误包」：code + '#' + SQLSTATE + message），此处翻译为
/// MySQL 客户端认识的码值/状态；未收录的 state 兜底 ER_UNKNOWN_ERROR。
pub fn map_state(state: &str) -> (u16, &'static str) {
    match state {
        "42P01" => (1146, "42S02"), // ER_NO_SUCH_TABLE
        "42P07" => (1050, "42S01"), // ER_TABLE_EXISTS_ERROR
        "42703" => (1054, "42S22"), // ER_BAD_FIELD_ERROR
        "42601" => (1064, "42000"), // ER_PARSE_ERROR
        "23505" => (1062, "23000"), // ER_DUP_ENTRY
        "40001" => (1213, "40001"), // ER_LOCK_DEADLOCK
        "22012" => (1365, "22012"), // ER_DIVISION_BY_ZERO
        "22P02" => (1366, "HY000"), // ER_TRUNCATED_WRONG_VALUE
        "42804" => (1241, "21000"), // ER_CANT_AGGREGATE_2COLLATIONS 家族
        "0A000" => (1235, "42000"), // ER_NOT_SUPPORTED_YET
        "3D000" => (1049, "42000"), // ER_BAD_DB_ERROR
        "42000" => (1044, "42000"), // ER_DBACCESS_DENIED_ERROR 兜位
        "58030" => (2013, "08S01"), // CR_SERVER_LOST（IO 类）
        // completion_unknown（WAL 毒化，P0-D）：MySQL 无对应错误码，
        // code 用 ER_UNKNOWN_ERROR，但 sql_state 原样携带 40003——客户端
        // 可据此区分"结果未知"与普通错误（语义见 SPEC 02 §4.1）
        "40003" => (1105, "40003"),
        _ => (1105, "HY000"),       // ER_UNKNOWN_ERROR（XX000 等）
    }
}

/// SqlError → ERR 包字节
pub fn err_from(e: &SqlError) -> Vec<u8> {
    let (code, state) = map_state(e.state);
    err_packet(code, state, &e.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_mapping_covers_core_errors() {
        // SPEC 06 §3：表不存在 → 1146 / 42S02
        assert_eq!(map_state("42P01"), (1146, "42S02"));
        assert_eq!(map_state("40003"), (1105, "40003"), "completion_unknown 的 sql_state 必须透传");
        assert_eq!(map_state("42601"), (1064, "42000"));
        assert_eq!(map_state("23505"), (1062, "23000"));
        assert_eq!(map_state("3D000"), (1049, "42000"));
        assert_eq!(map_state("XX000"), (1105, "HY000"));
    }

    #[test]
    fn err_packet_carries_state_and_message() {
        let e = SqlError::undefined_table("no such rel");
        let p = err_from(&e);
        assert_eq!(p[0], crate::codec::ERR_HEADER);
        assert_eq!(&p[1..3], &1146u16.to_le_bytes());
        assert_eq!(p[3], b'#');
        assert_eq!(&p[4..9], b"42S02");
        assert_eq!(&p[9..], b"no such rel");
    }
}
