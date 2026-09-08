//! 帧编解码 — PG wire 协议 v3 的字节 ↔ 消息（SPEC 06 §2）。
//!
//! 与 neon `pq_proto` 的职责一致，但传输层用 `std::io::Read/Write`
//! （SPEC 10 §7 推荐的 std::net + std::io 方案），不依赖 tokio。
//!
//! - startup 包（SPEC 06 §2.1）：无类型字节，i32 len + i32 request_code；
//!   SSLRequest/GSSENCRequest/CancelRequest 用特殊 code 区分。
//! - 普通消息（SPEC 06 §2.2）：1 字节 tag + i32 len（含自身不含 tag）+ body。
//! - 服务端消息（BeMessage）：按 PG 消息格式文档序列化。

#![deny(unsafe_code)]

use bytes::{BufMut, BytesMut};
use std::collections::HashMap;
use std::io::{self, Read, BufReader, Write};

/// protocol 3.0（196608 = 3 << 16 | 0）
pub const PROTOCOL_3_0: i32 = 196_608;
/// SSLRequest：1234 << 16 | 5679
pub const SSL_REQUEST_CODE: i32 = 80877103;
/// GSSENCRequest：1234 << 16 | 5680
pub const GSSENC_REQUEST_CODE: i32 = 80877104;
/// CancelRequest：1234 << 16 | 5678
pub const CANCEL_REQUEST_CODE: i32 = 80877102;
/// 保留主版本号（1234.x 都是特殊请求，不是 StartupMessage）
const RESERVED_MAJOR: i32 = 1234;
/// startup 包上限（与 PG 一致：pqcomm.h MAX_STARTUP_PACKET_LENGTH）
pub const MAX_STARTUP_PACKET_LENGTH: usize = 10_000;
/// 普通消息 body 上限（1 GB，防御性）
const MAX_MESSAGE_LENGTH: usize = 1 << 30;

/// 协议违规/坏包 IO 错误（InvalidInput）
pub(crate) fn protocol_error(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("protocol error: {}", msg.into()))
}

// ---------------------------------------------------------------------------
// Startup 阶段
// ---------------------------------------------------------------------------

/// startup 阶段读到的包（SPEC 06 §2.1）
#[derive(Debug, Clone, PartialEq)]
pub enum StartupPacket {
    SslRequest,
    GssEncRequest,
    CancelRequest {
        pid: i32,
        secret: i32,
    },
    Startup {
        protocol: i32,
        params: HashMap<String, String>,
    },
}

/// 解析 startup 参数区：PG 的格式是交替的 C 字符串对
/// `k1\0v1\0k2\0v2\0...\0`（libpq/tokio-postgres/psql 均此格式；
/// 参照 neon pq_proto `StartupMessageParams::iter` 的 tuples 解析）
fn parse_params(bytes: &[u8]) -> io::Result<HashMap<String, String>> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| protocol_error("startup message params: invalid utf-8"))?;
    let s = s
        .strip_suffix('\0')
        .ok_or_else(|| protocol_error("startup message params: missing null terminator"))?;
    let mut map = HashMap::new();
    let mut it = s.split_terminator('\0');
    while let Some(k) = it.next() {
        // 宽松：值缺失按空串处理
        let v = it.next().unwrap_or("");
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// 前端消息（ established 后）
// ---------------------------------------------------------------------------

/// 前端 → 服务端消息（解析后）
#[derive(Debug, Clone, PartialEq)]
pub enum FeMessage {
    /// 'Q' 简单查询
    Query(String),
    /// 'P' Parse
    Parse { name: String, sql: String, param_oids: Vec<u32> },
    /// 'B' Bind
    Bind {
        portal: String,
        stmt: String,
        param_formats: Vec<i16>,
        /// None = NULL
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    /// 'D' Describe（kind: b'S' 语句 / b'P' portal）
    Describe { kind: u8, name: String },
    /// 'E' Execute
    Execute { portal: String, max_rows: i32 },
    /// 'C' Close（kind: b'S' / b'P'）
    Close { kind: u8, name: String },
    /// 'H' Flush
    Flush,
    /// 'S' Sync
    Sync,
    /// 'X' Terminate
    Terminate,
    /// 'p' PasswordMessage（认证阶段）
    PasswordMessage(String),
    /// 'd'/'c'/'f' COPY 子协议消息 — v1 不支持（SPEC 06 §2.2 → 0A000）
    Copy { tag: u8 },
}

/// 消息 body 顺序读取器（带边界检查）
struct BodyReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BodyReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn need(&self, n: usize, what: &str) -> io::Result<()> {
        if self.remaining() < n {
            Err(protocol_error(format!("malformed message: missing {what}")))
        } else {
            Ok(())
        }
    }
    fn take(&mut self, n: usize, what: &str) -> io::Result<&'a [u8]> {
        self.need(n, what)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self, what: &str) -> io::Result<u8> {
        Ok(self.take(1, what)?[0])
    }
    fn i16(&mut self, what: &str) -> io::Result<i16> {
        let b = self.take(2, what)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }
    fn i32(&mut self, what: &str) -> io::Result<i32> {
        let b = self.take(4, what)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u32(&mut self, what: &str) -> io::Result<u32> {
        Ok(self.i32(what)? as u32)
    }
    /// 读 C 字符串（到 \0），UTF-8
    fn cstr(&mut self, what: &str) -> io::Result<String> {
        let rest = &self.buf[self.pos..];
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| protocol_error(format!("malformed message: unterminated {what}")))?;
        let s = std::str::from_utf8(&rest[..end])
            .map_err(|_| protocol_error(format!("invalid utf-8 in {what}")))?;
        self.pos += end + 1;
        Ok(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// 服务端消息（序列化）
// ---------------------------------------------------------------------------

/// RowDescription 中的一个字段描述
#[derive(Debug, Clone, PartialEq)]
pub struct RowField {
    pub name: String,
    pub type_oid: u32,
    pub typlen: i16,
    pub typmod: i32,
    pub format: i16,
}

/// 服务端 → 前端消息
#[derive(Debug, Clone, PartialEq)]
pub enum BeMessage {
    AuthenticationOk,
    AuthenticationCleartextPassword,
    BackendKeyData { pid: i32, secret: u32 },
    ParameterStatus { name: &'static str, value: &'static str },
    /// ReadyForQuery，参数为事务状态字节 I/T/E
    ReadyForQuery(u8),
    ParseComplete,
    BindComplete,
    CloseComplete,
    EmptyQueryResponse,
    CommandComplete(String),
    /// 单元格 None = NULL
    DataRow(Vec<Option<Vec<u8>>>),
    RowDescription(Vec<RowField>),
    NoData,
    ParameterDescription(Vec<u32>),
    ErrorResponse { severity: &'static str, code: &'static str, message: String },
}

/// 在 buf 上写一个带长度前缀的消息 body（len 含自身 4 字节）
fn write_body(buf: &mut BytesMut, f: impl FnOnce(&mut BytesMut)) {
    let base = buf.len();
    buf.extend_from_slice(&[0; 4]);
    f(buf);
    let n = (buf.len() - base) as i32;
    buf[base..base + 4].copy_from_slice(&n.to_be_bytes());
}

/// 写 C 字符串（协议中的 String）
fn write_cstr(buf: &mut BytesMut, s: &str) {
    buf.put_slice(s.as_bytes());
    buf.put_u8(0);
}

/// 序列化服务端消息到输出缓冲
pub fn write_be_message(buf: &mut BytesMut, msg: &BeMessage) {
    match msg {
        BeMessage::AuthenticationOk => {
            buf.put_u8(b'R');
            write_body(buf, |b| b.put_i32(0));
        }
        BeMessage::AuthenticationCleartextPassword => {
            buf.put_u8(b'R');
            write_body(buf, |b| b.put_i32(3)); // AuthenticationCleartextPassword
        }
        BeMessage::BackendKeyData { pid, secret } => {
            buf.put_u8(b'K');
            write_body(buf, |b| {
                b.put_i32(*pid);
                b.put_i32(*secret as i32);
            });
        }
        BeMessage::ParameterStatus { name, value } => {
            buf.put_u8(b'S');
            write_body(buf, |b| {
                write_cstr(b, name);
                write_cstr(b, value);
            });
        }
        BeMessage::ReadyForQuery(status) => {
            buf.put_u8(b'Z');
            write_body(buf, |b| b.put_u8(*status));
        }
        BeMessage::ParseComplete => {
            buf.put_u8(b'1');
            write_body(buf, |_| {});
        }
        BeMessage::BindComplete => {
            buf.put_u8(b'2');
            write_body(buf, |_| {});
        }
        BeMessage::CloseComplete => {
            buf.put_u8(b'3');
            write_body(buf, |_| {});
        }
        BeMessage::EmptyQueryResponse => {
            buf.put_u8(b'I');
            write_body(buf, |_| {});
        }
        BeMessage::CommandComplete(tag) => {
            buf.put_u8(b'C');
            write_body(buf, |b| write_cstr(b, tag));
        }
        BeMessage::DataRow(cells) => {
            buf.put_u8(b'D');
            write_body(buf, |b| {
                b.put_u16(cells.len() as u16);
                for cell in cells {
                    match cell {
                        Some(v) => {
                            b.put_i32(v.len() as i32);
                            b.put_slice(v);
                        }
                        None => b.put_i32(-1),
                    }
                }
            });
        }
        BeMessage::RowDescription(fields) => {
            buf.put_u8(b'T');
            write_body(buf, |b| {
                b.put_i16(fields.len() as i16);
                for f in fields {
                    write_cstr(b, &f.name);
                    b.put_i32(0); // table oid
                    b.put_i16(0); // attnum
                    b.put_u32(f.type_oid);
                    b.put_i16(f.typlen);
                    b.put_i32(f.typmod);
                    b.put_i16(f.format);
                }
            });
        }
        BeMessage::NoData => {
            buf.put_u8(b'n');
            write_body(buf, |_| {});
        }
        BeMessage::ParameterDescription(oids) => {
            buf.put_u8(b't');
            write_body(buf, |b| {
                b.put_i16(oids.len() as i16);
                for oid in oids {
                    b.put_u32(*oid);
                }
            });
        }
        BeMessage::ErrorResponse { severity, code, message } => {
            buf.put_u8(b'E');
            write_body(buf, |b| {
                b.put_u8(b'S');
                write_cstr(b, severity);
                b.put_u8(b'V'); // non-localized severity
                write_cstr(b, severity);
                b.put_u8(b'C');
                write_cstr(b, code);
                b.put_u8(b'M');
                write_cstr(b, message);
                b.put_u8(0); // 字段终止符
            });
        }
    }
}

// ---------------------------------------------------------------------------
// PgStream — 读写缓冲封装
// ---------------------------------------------------------------------------

/// 一条连接上的缓冲读写器。
///
/// 读侧用 `BufReader`（阻塞 read_exact），写侧聚合到 `wbuf`，
/// 由 `flush()` 一次性写出（对应协议中的 Flush/Sync 时机）。
pub struct PgStream<T> {
    r: BufReader<T>,
    wbuf: BytesMut,
}

impl<T: Read + Write> PgStream<T> {
    pub fn new(inner: T) -> Self {
        Self {
            r: BufReader::with_capacity(16 * 1024, inner),
            wbuf: BytesMut::with_capacity(16 * 1024),
        }
    }

    /// 读一个字节；干净 EOF（消息边界处）返回 None
    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut b = [0u8; 1];
        loop {
            match self.r.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(b[0])),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.r.read_exact(buf)
    }

    /// 读 startup 包（无 tag 字节）。干净 EOF 返回 None。
    pub fn read_startup(&mut self) -> io::Result<Option<StartupPacket>> {
        let mut lenb = [0u8; 4];
        let Some(first) = self.read_byte()? else {
            return Ok(None);
        };
        lenb[0] = first;
        self.read_exact(&mut lenb[1..])?;
        let len = i32::from_be_bytes(lenb) as usize;
        if !(8..=MAX_STARTUP_PACKET_LENGTH).contains(&len) {
            return Err(protocol_error(format!("invalid startup packet length {len}")));
        }
        let mut body = vec![0u8; len - 4];
        self.read_exact(&mut body)?;
        if body.len() < 4 {
            return Err(protocol_error("startup packet too short"));
        }
        let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        let pkt = match code {
            SSL_REQUEST_CODE => StartupPacket::SslRequest,
            GSSENC_REQUEST_CODE => StartupPacket::GssEncRequest,
            CANCEL_REQUEST_CODE => {
                if body.len() != 12 {
                    return Err(protocol_error("CancelRequest is malformed"));
                }
                StartupPacket::CancelRequest {
                    pid: i32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                    secret: i32::from_be_bytes([body[8], body[9], body[10], body[11]]),
                }
            }
            c if (c >> 16) == RESERVED_MAJOR => {
                return Err(protocol_error(format!("unrecognized request code {}", (c & 0xffff) as u16)));
            }
            protocol => StartupPacket::Startup {
                protocol,
                params: parse_params(&body[4..])?,
            },
        };
        Ok(Some(pkt))
    }

    /// 读一条前端消息。干净 EOF（消息边界）返回 None；
    /// 消息中途断开返回 UnexpectedEof 错误。
    pub fn read_message(&mut self) -> io::Result<Option<FeMessage>> {
        let Some(tag) = self.read_byte()? else {
            return Ok(None);
        };
        let mut lenb = [0u8; 4];
        self.read_exact(&mut lenb)?;
        let len = i32::from_be_bytes(lenb) as usize;
        if !(4..=MAX_MESSAGE_LENGTH).contains(&len) {
            return Err(protocol_error(format!("invalid message length {len}")));
        }
        let mut body = vec![0u8; len - 4];
        self.read_exact(&mut body)?;
        let mut rd = BodyReader::new(&body);
        let msg = match tag {
            b'Q' => FeMessage::Query(rd.cstr("query")?),
            b'p' => FeMessage::PasswordMessage(rd.cstr("password")?),
            b'P' => {
                let name = rd.cstr("statement name")?;
                let sql = rd.cstr("query")?;
                let n = rd.i16("param count")?;
                let n = n.max(0) as usize;
                let mut param_oids = Vec::with_capacity(n);
                for _ in 0..n {
                    param_oids.push(rd.u32("param oid")?);
                }
                FeMessage::Parse { name, sql, param_oids }
            }
            b'B' => {
                let portal = rd.cstr("portal name")?;
                let stmt = rd.cstr("statement name")?;
                let nf = rd.i16("param format count")?.max(0) as usize;
                let mut param_formats = Vec::with_capacity(nf);
                for _ in 0..nf {
                    param_formats.push(rd.i16("param format")?);
                }
                let np = rd.i16("param count")?.max(0) as usize;
                let mut params = Vec::with_capacity(np);
                for _ in 0..np {
                    let len = rd.i32("param length")?;
                    if len < 0 {
                        params.push(None);
                    } else {
                        params.push(Some(rd.take(len as usize, "param value")?.to_vec()));
                    }
                }
                let nr = rd.i16("result format count")?.max(0) as usize;
                let mut result_formats = Vec::with_capacity(nr);
                for _ in 0..nr {
                    result_formats.push(rd.i16("result format")?);
                }
                FeMessage::Bind { portal, stmt, param_formats, params, result_formats }
            }
            b'D' => {
                let kind = rd.u8("describe kind")?;
                let name = rd.cstr("name")?;
                FeMessage::Describe { kind, name }
            }
            b'E' => {
                let portal = rd.cstr("portal name")?;
                let max_rows = rd.i32("max rows")?;
                FeMessage::Execute { portal, max_rows }
            }
            b'C' => {
                let kind = rd.u8("close kind")?;
                let name = rd.cstr("name")?;
                FeMessage::Close { kind, name }
            }
            b'H' => FeMessage::Flush,
            b'S' => FeMessage::Sync,
            b'X' => FeMessage::Terminate,
            b'd' | b'c' | b'f' => FeMessage::Copy { tag },
            other => {
                return Err(protocol_error(format!(
                    "unknown message tag: {}",
                    char::from(other)
                )));
            }
        };
        Ok(Some(msg))
    }

    /// 写一个原始字节（SSLRequest 的 'N'/'S' 应答没有长度前缀）
    pub fn write_raw(&mut self, bytes: &[u8]) {
        self.wbuf.extend_from_slice(bytes);
    }

    /// 追加一条服务端消息到输出缓冲（不冲刷）
    pub fn send(&mut self, msg: &BeMessage) {
        write_be_message(&mut self.wbuf, msg);
    }

    /// 把输出缓冲写到流并冲刷
    pub fn flush(&mut self) -> io::Result<()> {
        if !self.wbuf.is_empty() {
            let wbuf = std::mem::take(&mut self.wbuf);
            self.r.get_mut().write_all(&wbuf)?;
        }
        self.r.get_mut().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Cursor<Vec<u8>> 同时实现 Read + Write（写追加到内部 vec，读从头开始）
    type TestStream = Cursor<Vec<u8>>;

    fn msg_bytes(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        v.extend_from_slice(&((body.len() as i32 + 4).to_be_bytes()));
        v.extend_from_slice(body);
        v
    }

    fn startup_bytes(protocol: i32, params: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&protocol.to_be_bytes());
        // 参数区：交替 k\0v\0，最后一个额外 \0 终止
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

    fn read_one_startup(bytes: &[u8]) -> io::Result<Option<StartupPacket>> {
        PgStream::new(TestStream::new(bytes.to_vec())).read_startup()
    }

    #[test]
    fn startup_ssl_request() {
        // 80877103 = 0x04D2162F
        let pkt = read_one_startup(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap().unwrap();
        assert_eq!(pkt, StartupPacket::SslRequest);
    }

    #[test]
    fn startup_gssenc_request() {
        let pkt = read_one_startup(&[0, 0, 0, 8, 4, 210, 22, 48]).unwrap().unwrap();
        assert_eq!(pkt, StartupPacket::GssEncRequest);
    }

    #[test]
    fn startup_cancel_request() {
        let mut b = vec![0, 0, 0, 16];
        b.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
        b.extend_from_slice(&42i32.to_be_bytes());
        b.extend_from_slice(&7777i32.to_be_bytes());
        let pkt = read_one_startup(&b).unwrap().unwrap();
        assert_eq!(pkt, StartupPacket::CancelRequest { pid: 42, secret: 7777 });
    }

    #[test]
    fn startup_full_parse() {
        let raw = startup_bytes(PROTOCOL_3_0, &[("user", "alice"), ("database", "db0")]);
        let pkt = read_one_startup(&raw).unwrap().unwrap();
        match pkt {
            StartupPacket::Startup { protocol, params } => {
                assert_eq!(protocol, 196_608);
                assert_eq!(params.get("user").map(String::as_str), Some("alice"));
                assert_eq!(params.get("database").map(String::as_str), Some("db0"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn startup_empty_and_odd_params() {
        // 零参数
        let raw = startup_bytes(PROTOCOL_3_0, &[]);
        let pkt = read_one_startup(&raw).unwrap().unwrap();
        assert!(matches!(pkt, StartupPacket::Startup { params, .. } if params.is_empty()));

        // 奇数个 token（缺值的 key）宽松处理：值按空串
        let mut body = PROTOCOL_3_0.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0");
        body.push(0);
        let mut raw = ((body.len() as i32 + 4).to_be_bytes()).to_vec();
        raw.extend_from_slice(&body);
        let pkt = read_one_startup(&raw).unwrap().unwrap();
        match pkt {
            StartupPacket::Startup { params, .. } => {
                assert_eq!(params.get("user").map(String::as_str), Some(""));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn startup_rejects_bad_length() {
        assert!(read_one_startup(&[0, 0, 0, 3]).is_err());
        assert!(read_one_startup(&[0, 0, 0, 0]).is_err());
        // 超过上限
        let mut b = vec![0x00, 0xFF, 0xFF, 0xFF];
        b.resize(16, 0);
        assert!(read_one_startup(&b).is_err());
    }

    #[test]
    fn startup_rejects_reserved_major() {
        // 1234.9 是保留 code
        let mut b = vec![0, 0, 0, 8];
        b.extend_from_slice(&((RESERVED_MAJOR << 16 | 9).to_be_bytes()));
        b.extend_from_slice(&[0, 0, 0, 0]);
        assert!(read_one_startup(&b).is_err());
    }

    #[test]
    fn startup_eof_at_boundary() {
        let mut pg = PgStream::new(TestStream::new(vec![]));
        assert!(pg.read_startup().unwrap().is_none());
    }

    #[test]
    fn message_query_roundtrip() {
        let raw = msg_bytes(b'Q', b"SELECT 1\0");
        let mut pg = PgStream::new(TestStream::new(raw));
        assert_eq!(
            pg.read_message().unwrap(),
            Some(FeMessage::Query("SELECT 1".into()))
        );
        assert!(pg.read_message().unwrap().is_none()); // 干净 EOF
    }

    #[test]
    fn message_parse_roundtrip() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0"); // unnamed
        body.extend_from_slice(b"SELECT $1\0");
        body.extend_from_slice(&2i16.to_be_bytes());
        body.extend_from_slice(&20i32.to_be_bytes());
        body.extend_from_slice(&25i32.to_be_bytes());
        let raw = msg_bytes(b'P', &body);
        let mut pg = PgStream::new(TestStream::new(raw));
        assert_eq!(
            pg.read_message().unwrap(),
            Some(FeMessage::Parse {
                name: String::new(),
                sql: "SELECT $1".into(),
                param_oids: vec![20, 25],
            })
        );
    }

    #[test]
    fn message_bind_roundtrip() {
        use bytes::BufMut as _;
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"stmt1\0");
        body.put_i16(2); // param formats
        body.put_i16(0);
        body.put_i16(1);
        body.put_i16(2); // 2 params
        body.put_i32(3);
        body.extend_from_slice(b"abc");
        body.put_i32(-1); // NULL
        body.put_i16(1); // result formats
        body.put_i16(1);
        let raw = msg_bytes(b'B', &body);
        let mut pg = PgStream::new(TestStream::new(raw));
        assert_eq!(
            pg.read_message().unwrap(),
            Some(FeMessage::Bind {
                portal: String::new(),
                stmt: "stmt1".into(),
                param_formats: vec![0, 1],
                params: vec![Some(b"abc".to_vec()), None],
                result_formats: vec![1],
            })
        );
    }

    #[test]
    fn message_misc_roundtrip() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&msg_bytes(b'D', &[b'S', 0x00]));
        raw.extend_from_slice(&msg_bytes(b'D', &[b'P', b'p', b'1', 0x00]));
        raw.extend_from_slice(&msg_bytes(b'E', b"\0")); // Execute {portal:"", max_rows 缺失 → err}
        let mut exec_body = b"\0".to_vec();
        exec_body.extend_from_slice(&0i32.to_be_bytes());
        raw.extend_from_slice(&msg_bytes(b'E', &exec_body));
        raw.extend_from_slice(&msg_bytes(b'C', &[b'S', b's', b'1', 0x00]));
        raw.extend_from_slice(&msg_bytes(b'H', &[]));
        raw.extend_from_slice(&msg_bytes(b'S', &[]));
        raw.extend_from_slice(&msg_bytes(b'X', &[]));
        raw.extend_from_slice(&msg_bytes(b'p', b"secret\0"));
        raw.extend_from_slice(&msg_bytes(b'd', &[1, 2, 3]));

        let mut pg = PgStream::new(TestStream::new(raw));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Describe { kind: b'S', name: String::new() }));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Describe { kind: b'P', name: "p1".into() }));
        assert!(pg.read_message().is_err()); // Execute 缺 max_rows
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Execute { portal: String::new(), max_rows: 0 }));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Close { kind: b'S', name: "s1".into() }));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Flush));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Sync));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Terminate));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::PasswordMessage("secret".into())));
        assert_eq!(pg.read_message().unwrap(), Some(FeMessage::Copy { tag: b'd' }));
        assert!(pg.read_message().unwrap().is_none());
    }

    #[test]
    fn message_unknown_tag_rejected() {
        let raw = msg_bytes(b'~', &[]);
        let mut pg = PgStream::new(TestStream::new(raw));
        assert!(pg.read_message().is_err());
    }

    #[test]
    fn eof_mid_message_is_error() {
        // 只给一半长度
        let raw = [b'Q', 0, 0, 0, 20, b'S'];
        let mut pg = PgStream::new(TestStream::new(raw.to_vec()));
        assert!(pg.read_message().is_err());
    }

    #[test]
    fn be_authentication_ok_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(&mut buf, &BeMessage::AuthenticationOk);
        assert_eq!(&buf[..], &[b'R', 0, 0, 0, 8, 0, 0, 0, 0]);
    }

    #[test]
    fn be_cleartext_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(&mut buf, &BeMessage::AuthenticationCleartextPassword);
        assert_eq!(&buf[..], &[b'R', 0, 0, 0, 8, 0, 0, 0, 3]);
    }

    #[test]
    fn be_command_complete_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(&mut buf, &BeMessage::CommandComplete("SELECT 1".into()));
        assert_eq!(
            &buf[..],
            &[b'C', 0, 0, 0, 13, b'S', b'E', b'L', b'E', b'C', b'T', b' ', b'1', 0]
        );
    }

    #[test]
    fn be_data_row_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(&mut buf, &BeMessage::DataRow(vec![Some(b"1".to_vec()), None]));
        // len=15: ncol(2) + len(4)+val(1) + null(4) + len 前缀自身(4)
        assert_eq!(
            &buf[..],
            &[
                b'D', 0, 0, 0, 15, // len = 11 body + 4
                0, 2, // ncols
                0, 0, 0, 1, b'1', // "1"
                0xFF, 0xFF, 0xFF, 0xFF, // NULL
            ]
        );
    }

    #[test]
    fn be_ready_for_query_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(&mut buf, &BeMessage::ReadyForQuery(b'I'));
        assert_eq!(&buf[..], &[b'Z', 0, 0, 0, 5, b'I']);
    }

    #[test]
    fn be_row_description_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(
            &mut buf,
            &BeMessage::RowDescription(vec![RowField {
                name: "x".into(),
                type_oid: 20,
                typlen: 8,
                typmod: -1,
                format: 0,
            }]),
        );
        // len = 4 + 2 + (1+1 + 4 + 2 + 4 + 2 + 4 + 2) = 26
        assert_eq!(
            &buf[..],
            &[
                b'T', 0, 0, 0, 26, // len
                0, 1,   // 字段数
                b'x', 0, // name
                0, 0, 0, 0, // table oid
                0, 0,   // attnum
                0, 0, 0, 20, // type oid (int8)
                0, 8,   // typlen
                0xFF, 0xFF, 0xFF, 0xFF, // typmod = -1
                0, 0,   // format
            ]
        );
    }

    #[test]
    fn be_error_response_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(
            &mut buf,
            &BeMessage::ErrorResponse {
                severity: "ERROR",
                code: "42P01",
                message: "no such table".into(),
            },
        );
        let expected: Vec<u8> = {
            let mut b = vec![b'E'];
            let body: Vec<u8> = {
                // 字段 = 类型字节 + cstring；最后字段终止符
                let mut b = Vec::new();
                b.push(b'S');
                b.extend_from_slice(b"ERROR\0");
                b.push(b'V');
                b.extend_from_slice(b"ERROR\0");
                b.push(b'C');
                b.extend_from_slice(b"42P01\0");
                b.push(b'M');
                b.extend_from_slice(b"no such table\0");
                b.push(0);
                b
            };
            b.extend_from_slice(&((body.len() as i32 + 4).to_be_bytes()));
            b.extend_from_slice(&body);
            b
        };
        assert_eq!(&buf[..], &expected[..]);
        // SQLSTATE 必须出现在 body 中
        assert!(buf.windows(5).any(|w| w == b"42P01"));
    }

    #[test]
    fn be_backend_key_data_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(&mut buf, &BeMessage::BackendKeyData { pid: 7, secret: 0xDEADBEEF });
        assert_eq!(
            &buf[..],
            &[b'K', 0, 0, 0, 12, 0, 0, 0, 7, 0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn be_empty_variants_bytes() {
        let cases: &[(BeMessage, u8)] = &[
            (BeMessage::ParseComplete, b'1'),
            (BeMessage::BindComplete, b'2'),
            (BeMessage::CloseComplete, b'3'),
            (BeMessage::EmptyQueryResponse, b'I'),
            (BeMessage::NoData, b'n'),
        ];
        for (msg, tag) in cases {
            let mut buf = BytesMut::new();
            write_be_message(&mut buf, msg);
            assert_eq!(&buf[..], &[*tag, 0, 0, 0, 4]);
        }
    }

    #[test]
    fn be_parameter_description_bytes() {
        let mut buf = BytesMut::new();
        write_be_message(&mut buf, &BeMessage::ParameterDescription(vec![23, 20]));
        // len = 4 + 2 + 4 + 4 = 14
        assert_eq!(
            &buf[..],
            &[b't', 0, 0, 0, 14, 0, 2, 0, 0, 0, 23, 0, 0, 0, 20]
        );
    }
}
