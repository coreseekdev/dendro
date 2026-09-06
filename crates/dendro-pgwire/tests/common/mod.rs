//! 测试共享：MockSession（`Box<dyn WireSession>` 注入桩）+ 协议字节助手。

#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;

use arrow::array::{ArrayRef, Int64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use dendro_core::engine::{PrepareMeta, WireSession};
use dendro_core::error::SqlError;
use dendro_core::types::{ColumnMeta, ColType, Output, RecordSet, SqlValue};

use dendro_pgwire::PgConfig;

// ---------------------------------------------------------------------------
// MockSession
// ---------------------------------------------------------------------------

/// exec 的行为（简单查询）
#[derive(Default)]
pub struct ExecBehavior {
    /// 返回给匹配语句的错误（测试错误传播）
    pub error: Option<SqlError>,
    /// Some(子串) → 只对包含该子串的 sql 报错；None → 对所有非空语句报错
    pub error_if_contains: Option<String>,
}

/// exec_prepared 的行为
#[derive(Default)]
pub enum EpBehavior {
    /// 返回单列 int8、值为 1 的 Rows（默认）
    #[default]
    Rows1,
    /// 回显第一个 int 参数为单行结果（测参数端到端）
    EchoInt8Param,
    /// 返回 Command{tag}
    Command(String),
}

/// 可配置的 WireSession 桩。
///
/// 默认行为（任务规格要求）：`exec("SELECT 1")` 返回单列 int8 值 1 的
/// Rows；其余 SELECT 返回同样的 Rows（每条语句一个）；非 SELECT 返回
/// Command。prepare/exec_prepared 记录调用。
pub struct MockSession {
    pub exec_log: Vec<String>,
    pub exec_behavior: ExecBehavior,
    pub prepare_log: Vec<(String, String, Vec<ColType>)>,
    pub prepare_error: Option<SqlError>,
    pub exec_prepared_log: Vec<(String, Vec<SqlValue>)>,
    /// exec_prepared 错误注入
    pub ep_error: Option<SqlError>,
    /// Some(子串) → 只对语句 SQL 包含该子串的执行报错（按 prepare_log 反查）
    pub ep_error_if_contains: Option<String>,
    pub ep_behavior: EpBehavior,
    pub close_log: Vec<String>,
    pub txn: u8,
}

impl Default for MockSession {
    fn default() -> Self {
        Self {
            exec_log: Vec::new(),
            exec_behavior: ExecBehavior::default(),
            prepare_log: Vec::new(),
            prepare_error: None,
            exec_prepared_log: Vec::new(),
            ep_error: None,
            ep_error_if_contains: None,
            ep_behavior: EpBehavior::default(),
            close_log: Vec::new(),
            txn: b'I',
        }
    }
}

impl MockSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_txn(mut self, status: u8) -> Self {
        self.txn = status;
        self
    }

    pub fn with_exec_error(mut self, e: SqlError) -> Self {
        self.exec_behavior.error = Some(e);
        self
    }
}

/// 构造单列 int8 Rows（含可选 NULL 掩码）
pub fn rows_output_int8(name: &str, vals: &[i64], nulls: &[bool]) -> Output {
    let array: Int64Array = vals
        .iter()
        .enumerate()
        .map(|(i, &v)| if nulls.get(i).copied().unwrap_or(false) { None } else { Some(v) })
        .collect();
    let schema = Arc::new(Schema::new(vec![Field::new(name, DataType::Int64, true)]));
    let batch =
        RecordBatch::try_new(schema, vec![Arc::new(array) as ArrayRef]).expect("batch build");
    Output::Rows(RecordSet {
        columns: vec![ColumnMeta { name: name.to_string(), ty: ColType::Int64 }],
        batches: vec![batch],
    })
}

fn is_select(sql: &str) -> bool {
    sql.trim_start().to_ascii_uppercase().starts_with("SELECT")
}

/// 数 SQL 里的 `$n` 占位符个数（模拟引擎参数推断）
fn infer_param_count(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut max = 0usize;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && bytes[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            let mut v = 0usize;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                v = v * 10 + (bytes[j] - b'0') as usize;
                j += 1;
            }
            if v > max {
                max = v;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    max
}

fn is_insert(sql: &str) -> bool {
    sql.trim_start().to_ascii_uppercase().starts_with("INSERT")
}

impl WireSession for MockSession {
    fn exec(&mut self, sql: &str) -> Result<Vec<Output>, SqlError> {
        self.exec_log.push(sql.to_string());
        if sql.trim().is_empty() {
            return Ok(vec![]);
        }
        let error_applies = match &self.exec_behavior.error_if_contains {
            Some(pat) => sql.contains(pat.as_str()),
            None => true,
        };
        if error_applies {
            if let Some(e) = &self.exec_behavior.error {
                return Err(e.clone());
            }
        }
        if is_select(sql) {
            // 模拟引擎多语句切分：每条语句一个 Output
            let n = sql.split(';').filter(|s| !s.trim().is_empty()).count();
            Ok((0..n).map(|_| rows_output_int8("x", &[1], &[])).collect())
        } else if is_insert(sql) {
            Ok(vec![Output::Command { tag: "INSERT 0 1".into(), affected: 1 }])
        } else {
            Ok(vec![Output::Command { tag: format!("MOCK {sql}"), affected: 0 }])
        }
    }

    fn prepare(&mut self, name: &str, sql: &str, hint: &[ColType]) -> Result<PrepareMeta, SqlError> {
        self.prepare_log.push((name.to_string(), sql.to_string(), hint.to_vec()));
        if let Some(e) = &self.prepare_error {
            return Err(e.clone());
        }
        // 模拟引擎推断：hint 优先，语句里有 $n 占位则按个数补齐（int8 兜底）
        let mut param_types = hint.to_vec();
        let n = infer_param_count(sql);
        while param_types.len() < n {
            param_types.push(ColType::Int64);
        }
        let result_columns = if is_select(sql) {
            vec![ColumnMeta { name: "x".to_string(), ty: ColType::Int64 }]
        } else {
            vec![]
        };
        Ok(PrepareMeta { param_types, result_columns })
    }

    fn exec_prepared(&mut self, name: &str, params: &[SqlValue]) -> Result<Output, SqlError> {
        self.exec_prepared_log.push((name.to_string(), params.to_vec()));
        // portal 执行时只拿得到语句名，用 prepare_log 反查 SQL 做过滤
        let sql = self
            .prepare_log
            .iter()
            .rev()
            .find(|(n, _, _)| n == name)
            .map(|(_, s, _)| s.clone());
        let error_applies = match &self.ep_error_if_contains {
            Some(pat) => sql.as_deref().map_or(false, |s| s.contains(pat.as_str())),
            None => true,
        };
        if error_applies {
            if let Some(e) = &self.ep_error {
                return Err(e.clone());
            }
        }
        match &self.ep_behavior {
            EpBehavior::Rows1 => Ok(rows_output_int8("x", &[1], &[])),
            EpBehavior::EchoInt8Param => {
                let v = match params.first() {
                    Some(SqlValue::Int64(v)) => *v,
                    Some(SqlValue::Int32(v)) => *v as i64,
                    _ => 1,
                };
                Ok(rows_output_int8("x", &[v], &[]))
            }
            EpBehavior::Command(tag) => Ok(Output::Command { tag: tag.clone(), affected: 1 }),
        }
    }

    fn close_prepared(&mut self, name: &str) {
        self.close_log.push(name.to_string());
    }

    fn txn_status(&self) -> u8 {
        self.txn
    }
}

// ---------------------------------------------------------------------------
// 协议字节助手
// ---------------------------------------------------------------------------

/// 无类型 startup 包字节（len + code + 参数区：交替 k\0v\0 + 终止 \0）
pub fn startup_bytes(protocol: i32, params: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&protocol.to_be_bytes());
    for (k, v) in params {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut v = ((body.len() as i32 + 4).to_be_bytes()).to_vec();
    v.extend_from_slice(&body);
    v
}

/// 普通 frontend 消息字节（tag + len + body）
pub fn msg_bytes(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    v.extend_from_slice(&((body.len() as i32 + 4).to_be_bytes()));
    v.extend_from_slice(body);
    v
}

/// cstr 追加
pub fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// i16 BE 追加
pub fn put_i16(v: &mut Vec<u8>, x: i16) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// i32 BE 追加
pub fn put_i32(v: &mut Vec<u8>, x: i32) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// 读一条 backend 消息；EOF 返回 None
pub fn read_msg(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut tag = [0u8; 1];
    match r.read(&mut tag) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb)?;
    let len = i32::from_be_bytes(lenb) as usize;
    assert!(len >= 4, "bad message length {len}");
    let mut body = vec![0u8; len - 4];
    if !body.is_empty() {
        r.read_exact(&mut body)?;
    }
    Ok(Some((tag[0], body)))
}

/// 一直读到指定 tag（含），返回途中全部消息
pub fn read_until(r: &mut impl Read, tag: u8) -> io::Result<Vec<(u8, Vec<u8>)>> {
    let mut out = Vec::new();
    loop {
        match read_msg(r)? {
            Some(m) => {
                let done = m.0 == tag;
                out.push(m);
                if done {
                    return Ok(out);
                }
            }
            None => panic!("EOF before message with tag {}", tag as char),
        }
    }
}

/// 从 startup 到 ReadyForQuery 的一次完整握手（trust 模式）
pub fn handshake(s: &mut UnixStream, params: &[(&str, &str)]) -> io::Result<Vec<(u8, Vec<u8>)>> {
    s.write_all(&startup_bytes(196_608, params))?;
    read_until(s, b'Z')
}

/// 提取 ErrorResponse 字段
pub struct ErrFields {
    pub severity: String,
    pub code: String,
    pub message: String,
}

pub fn parse_error(body: &[u8]) -> ErrFields {
    let mut severity = String::new();
    let mut code = String::new();
    let mut message = String::new();
    for field in body.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let kind = field[0];
        let val = String::from_utf8_lossy(&field[1..]).to_string();
        match kind {
            b'S' | b'V' => severity = val,
            b'C' => code = val,
            b'M' => message = val,
            _ => {}
        }
    }
    ErrFields { severity, code, message }
}

/// 提取所有 ParameterStatus 到 (name, value) 列表
pub fn param_status(msgs: &[(u8, Vec<u8>)]) -> Vec<(String, String)> {
    msgs.iter()
        .filter(|(t, _)| *t == b'S')
        .map(|(_, body)| {
            // body = name\0value\0
            let nul = body.iter().position(|&b| b == 0).unwrap();
            let name = String::from_utf8_lossy(&body[..nul]).to_string();
            let value = String::from_utf8_lossy(&body[nul + 1..body.len() - 1]).to_string();
            (name, value)
        })
        .collect()
}

/// 解析 RowDescription
pub struct RowDescField {
    pub name: String,
    pub type_oid: u32,
    pub typlen: i16,
    pub typmod: i32,
    pub format: i16,
}

pub fn parse_row_description(body: &[u8]) -> Vec<RowDescField> {
    let n = i16::from_be_bytes([body[0], body[1]]);
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        let end = body[pos..].iter().position(|&b| b == 0).unwrap() + pos;
        let name = String::from_utf8_lossy(&body[pos..end]).to_string();
        pos = end + 1;
        let u32_at = |p: usize| u32::from_be_bytes(body[p..p + 4].try_into().unwrap());
        let i16_at = |p: usize| i16::from_be_bytes(body[p..p + 2].try_into().unwrap());
        let _table_oid = u32_at(pos);
        let attnum = i16_at(pos + 4);
        let type_oid = u32_at(pos + 6);
        let typlen = i16_at(pos + 10);
        let typmod = i32::from_be_bytes(body[pos + 12..pos + 16].try_into().unwrap());
        let format = i16_at(pos + 16);
        assert_eq!(attnum, 0);
        pos += 18;
        out.push(RowDescField { name, type_oid, typlen, typmod, format });
    }
    out
}

/// 解析 DataRow 单元格
pub fn parse_data_row(body: &[u8]) -> Vec<Option<Vec<u8>>> {
    let n = u16::from_be_bytes([body[0], body[1]]);
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        pos += 4;
        if len < 0 {
            out.push(None);
        } else {
            out.push(Some(body[pos..pos + len as usize].to_vec()));
            pos += len as usize;
        }
    }
    out
}

/// 用 UnixStream::pair 起一个连接处理线程（协议状态机黑盒驱动）
pub fn spawn_conn(sess: Box<dyn WireSession>, cfg: PgConfig) -> (UnixStream, JoinHandle<io::Result<()>>) {
    let (server, client) = UnixStream::pair().expect("unix pair");
    let handle = std::thread::spawn(move || dendro_pgwire::handle_connection(server, sess, cfg));
    (client, handle)
}
