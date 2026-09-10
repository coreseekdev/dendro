//! MySQL 协议帧与 lenenc / OK / ERR / EOF 包编解码 — SPEC 06 §3。
//!
//! 帧格式（MySQL 客户端协议 v10，SPEC 06 §3 首图）：
//! ```text
//! 3B  payload 长度（小端）      ≤ 0xFFFFFF，超出需拆多帧
//! 1B  sequence_id              每帧自增；命令/响应两侧各自延续
//!     payload
//! ```
//! 读侧：校验长度，超 max_allowed_packet 的逻辑包报 [`ReadError::TooLarge`]
//! （上层回 ERR 1153 ER_NET_PACKET_TOO_LARGE 后断开）。
//! 写侧：> 0xFFFFFF 的逻辑包自动拆帧、sequence 自增。

use std::io::{self, Read, Write};

/// 单帧 payload 上限（3B 长度字段所能表达的值）
pub const MAX_FRAME_PAYLOAD: usize = 0x00FF_FFFF;
/// 默认 max_allowed_packet：读侧单逻辑包累计超限 → ERR 1153 "08S01"
pub const DEFAULT_MAX_ALLOWED_PACKET: usize = 16 * 1024 * 1024;

// ———— capability flags（握手协商，SPEC 06 §3）————
pub const CLIENT_LONG_PASSWORD: u32 = 1 << 0;
pub const CLIENT_FOUND_ROWS: u32 = 1 << 1;
pub const CLIENT_LONG_FLAG: u32 = 1 << 2;
pub const CLIENT_CONNECT_WITH_DB: u32 = 1 << 3;
pub const CLIENT_PROTOCOL_41: u32 = 1 << 9;
/// v1 不支持 TLS，不宣告
pub const CLIENT_SSL: u32 = 1 << 11;
pub const CLIENT_TRANSACTIONS: u32 = 1 << 13;
pub const CLIENT_SECURE_CONNECTION: u32 = 1 << 15;
pub const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
pub const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;
pub const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;

/// 服务端宣告的能力。刻意**不含 CLIENT_DEPRECATE_EOF**（SPEC 06 §3 简化：
/// 统一走 EOF 包，各类客户端均兼容；客户端按宣告的能力回退）。
pub const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH;

// ———— status flags ————
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

// ———— payload 首字节 ————
pub const OK_HEADER: u8 = 0x00;
pub const EOF_HEADER: u8 = 0xFE;
pub const ERR_HEADER: u8 = 0xFF;
/// 握手阶段的 AuthSwitchRequest 首字节（上下文与 EOF 区分）
pub const AUTH_SWITCH_HEADER: u8 = 0xFE;

/// 字符集：utf8mb4_general_ci（SPEC 06 §3 握手）
pub const UTF8MB4_GENERAL_CI: u8 = 45;

// ————————————————————————————— lenenc 编码 —————————————————————————————

/// lenenc-int 写入（<251 / 0xFC+2B / 0xFD+3B / 0xFE+8B）
pub fn write_lenenc_int(buf: &mut Vec<u8>, v: u64) {
    if v < 251 {
        buf.push(v as u8);
    } else if v <= 0xFFFF {
        buf.push(0xFC);
        buf.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v <= 0x00FF_FFFF {
        buf.push(0xFD);
        buf.extend_from_slice(&(v as u32).to_le_bytes()[..3]);
    } else {
        buf.push(0xFE);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

/// lenenc-str 写入：lenenc-int 长度 + 原始字节
pub fn write_lenenc_str(buf: &mut Vec<u8>, s: &[u8]) {
    write_lenenc_int(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

// ————————————————————————————— 包解析 —————————————————————————————

/// 定长解包失败的标记（包被截断/格式错误）
#[derive(Debug, PartialEq, Eq)]
pub struct Truncated;

/// 面向 payload 的顺序读取游标
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Truncated> {
        let end = self.pos.checked_add(n).ok_or(Truncated)?;
        if end > self.buf.len() {
            return Err(Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, Truncated> {
        Ok(self.take(1)?[0])
    }

    pub fn u16_le(&mut self) -> Result<u16, Truncated> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32_le(&mut self) -> Result<u32, Truncated> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn skip(&mut self, n: usize) -> Result<(), Truncated> {
        self.take(n).map(|_| ())
    }

    /// NUL 结尾字符串（不含 NUL 本身）
    pub fn nul_terminated(&mut self) -> Result<&'a [u8], Truncated> {
        let rest = self.buf.get(self.pos..).ok_or(Truncated)?;
        let n = rest.iter().position(|&b| b == 0).ok_or(Truncated)?;
        let s = &rest[..n];
        self.pos += n + 1;
        Ok(s)
    }

    /// lenenc-int 读取
    pub fn lenenc_int(&mut self) -> Result<u64, Truncated> {
        let first = self.u8()?;
        Ok(match first {
            0xFC => u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as u64,
            0xFD => {
                let b = self.take(3)?;
                (b[0] as u64) | ((b[1] as u64) << 8) | ((b[2] as u64) << 16)
            }
            0xFE => u64::from_le_bytes(self.take(8)?.try_into().unwrap()),
            v => v as u64,
        })
    }

    /// lenenc-str 读取
    pub fn lenenc_bytes(&mut self) -> Result<&'a [u8], Truncated> {
        let n = self.lenenc_int()?;
        self.take(n as usize)
    }
}

// ————————————————————————————— 帧收发 —————————————————————————————

/// 一个逻辑包（可能由多个 ≤0xFFFFFF 的帧拼接而成）
pub struct Packet {
    pub seq: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub enum ReadError {
    /// 对端关闭（干净 EOF）
    Closed,
    /// 逻辑包超过 max_allowed_packet —— 上层回 ERR 1153 后断开
    TooLarge,
    Io(io::Error),
}

impl From<ReadError> for io::Error {
    fn from(e: ReadError) -> io::Error {
        match e {
            ReadError::Closed => io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"),
            ReadError::TooLarge => io::Error::new(
                io::ErrorKind::InvalidData,
                "packet bigger than max_allowed_packet",
            ),
            ReadError::Io(e) => e,
        }
    }
}

/// 暂存阈值：响应体积超过此值先落盘一次，避免大结果集把整包攒在内存
const STAGE_FLUSH_LIMIT: usize = 1 << 20;

/// 连接级帧读写器。`seq` 既是写侧下一帧的 sequence_id，
/// 也在每次 [`WireIo::read_packet`] 后自动同步为「读到的 seq + 1」，
/// 恰好满足协议的响应侧延续规则（命令 seq 0 → 响应从 seq 1 起）。
pub struct WireIo<T: Read + Write> {
    inner: T,
    seq: u8,
    out: Vec<u8>,
}

impl<T: Read + Write> WireIo<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            seq: 0,
            out: Vec::with_capacity(4096),
        }
    }

    /// 读一个逻辑包（跨帧自动拼接）。`Ok(None)` = 对端已关闭。
    pub fn read_packet(&mut self, max_allowed_packet: usize) -> Result<Option<Packet>, ReadError> {
        let mut payload: Vec<u8> = Vec::new();
        loop {
            let mut hdr = [0u8; 4];
            match self.inner.read_exact(&mut hdr) {
                Ok(()) => {}
                // 首帧头 EOF = 干净关闭；半途 EOF 同样按对端消失处理
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(ReadError::Io(e)),
            }
            let len = hdr[0] as usize | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
            self.seq = hdr[3].wrapping_add(1); // 响应侧 seq 从「客户端帧 seq + 1」起
            if payload.len() + len > max_allowed_packet {
                return Err(ReadError::TooLarge);
            }
            let mut chunk = vec![0u8; len];
            match self.inner.read_exact(&mut chunk) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(ReadError::Io(e)),
            }
            payload.extend_from_slice(&chunk);
            if len < MAX_FRAME_PAYLOAD {
                return Ok(Some(Packet {
                    seq: hdr[3],
                    payload,
                }));
            }
            // 该帧打满 → 后续还有同逻辑包的续帧
        }
    }

    /// 队列一个逻辑包：> 0xFFFFFF 自动拆多帧，sequence 每帧自增（SPEC 06 §3 大结果分包）。
    /// 暂存超过 [`STAGE_FLUSH_LIMIT`] 时自动落盘。
    pub fn write_packet(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_FRAME_PAYLOAD);
            let (chunk, tail) = rest.split_at(n);
            self.out.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
            self.out.push(self.seq);
            self.out.extend_from_slice(chunk);
            self.seq = self.seq.wrapping_add(1);
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
        if self.out.len() >= STAGE_FLUSH_LIMIT {
            self.flush()?;
        }
        Ok(())
    }

    /// 把暂存的帧写出
    pub fn flush(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        self.inner.write_all(&self.out)?;
        self.inner.flush()?;
        self.out.clear();
        Ok(())
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

// ————————————————————————————— 响应包构造 —————————————————————————————

/// OK 包（0x00）：affected + last_insert_id + status + warnings（PROTOCOL_41 形态）。
/// info 非空时按 lenenc-str 编码（现代 MySQL net_send_ok 的做法；
/// EOF 结尾的老式写法会被 mysql rust 等客户端按 lenenc 误读）。
pub fn ok_packet(affected: u64, last_insert_id: u64, status: u16, info: Option<&str>) -> Vec<u8> {
    let mut b = vec![OK_HEADER];
    write_lenenc_int(&mut b, affected);
    write_lenenc_int(&mut b, last_insert_id);
    b.extend_from_slice(&status.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // warnings
    if let Some(info) = info {
        if !info.is_empty() {
            write_lenenc_str(&mut b, info.as_bytes());
        }
    }
    b
}

/// EOF 包（0xfe，PROTOCOL_41 形态）：warnings + status
pub fn eof_packet(status: u16) -> Vec<u8> {
    let mut b = vec![EOF_HEADER];
    b.extend_from_slice(&0u16.to_le_bytes()); // warnings
    b.extend_from_slice(&status.to_le_bytes());
    b
}

/// ERR 包（0xff）：code(2B LE) + '#' + SQLSTATE(5B) + message
pub fn err_packet(code: u16, sql_state: &str, message: &str) -> Vec<u8> {
    let mut b = vec![ERR_HEADER];
    b.extend_from_slice(&code.to_le_bytes());
    b.push(b'#');
    let st: &[u8] = sql_state.as_bytes();
    debug_assert_eq!(st.len(), 5, "SQLSTATE must be 5 chars");
    b.extend_from_slice(&st[..st.len().min(5)]);
    b.extend_from_slice(message.as_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenenc_int_roundtrip() {
        for v in [
            0u64,
            1,
            250,
            251,
            252,
            65535,
            65536,
            0xFF_FFFF,
            0x100_0000,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            write_lenenc_int(&mut buf, v);
            let mut r = Reader::new(&buf);
            assert_eq!(r.lenenc_int(), Ok(v), "roundtrip {v}");
            assert!(r.is_empty());
        }
        // 编码长度断言
        let mut b = Vec::new();
        write_lenenc_int(&mut b, 250);
        assert_eq!(b.len(), 1);
        write_lenenc_int(&mut b, 251);
        assert_eq!(b.len(), 1 + 3);
        write_lenenc_int(&mut b, 0xFF_FFFF);
        assert_eq!(b.len(), 4 + 4);
        write_lenenc_int(&mut b, 0x1_0000_0000);
        assert_eq!(b.len(), 8 + 9);
    }

    #[test]
    fn lenenc_str_roundtrip() {
        let mut buf = Vec::new();
        write_lenenc_str(&mut buf, b"hello");
        write_lenenc_str(&mut buf, b"");
        let mut r = Reader::new(&buf);
        assert_eq!(r.lenenc_bytes(), Ok(&b"hello"[..]));
        assert_eq!(r.lenenc_bytes(), Ok(&b""[..]));
    }

    #[test]
    fn reader_truncated() {
        let mut r = Reader::new(&[0xFC, 0x01]);
        assert_eq!(r.lenenc_int(), Err(Truncated));
        let mut r = Reader::new(&b"abc"[..]);
        assert_eq!(r.nul_terminated(), Err(Truncated));
    }

    #[test]
    fn write_splits_large_packets() {
        // > 0xFFFFFF 的逻辑包 → 两帧（0xFFFFFF + 余量），seq 连续
        let payload = vec![0xABu8; MAX_FRAME_PAYLOAD + 16];
        let mut io = WireIo::new(io::Cursor::new(Vec::new()));
        io.write_packet(&payload).unwrap();
        io.flush().unwrap();
        let bytes = io.into_inner().into_inner();
        // 第一帧头
        assert_eq!(&bytes[..4], &[0xFF, 0xFF, 0xFF, 0]);
        let tail_len = bytes.len() - 4 - MAX_FRAME_PAYLOAD - 4;
        let h2 = &bytes[4 + MAX_FRAME_PAYLOAD..8 + MAX_FRAME_PAYLOAD];
        assert_eq!(&h2[..3], &(tail_len as u32).to_le_bytes()[..3]);
        assert_eq!(h2[3], 1);
        let reassembled = [
            &bytes[4..4 + MAX_FRAME_PAYLOAD],
            &bytes[8 + MAX_FRAME_PAYLOAD..],
        ]
        .concat();
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn read_reassembles_and_enforces_max_packet() {
        // 两帧拼接读回
        let payload = vec![7u8; MAX_FRAME_PAYLOAD + 3];
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0]);
        raw.extend_from_slice(&payload[..MAX_FRAME_PAYLOAD]);
        let rest = MAX_FRAME_PAYLOAD + 3 - MAX_FRAME_PAYLOAD;
        raw.extend_from_slice(&(rest as u32).to_le_bytes()[..3]);
        raw.push(1);
        raw.extend_from_slice(&payload[MAX_FRAME_PAYLOAD..]);
        let mut io = WireIo::new(io::Cursor::new(raw));
        // 0xFFFFFF+3 超过默认 16MB，用更大阈值验证拼接逻辑本身
        let pkt = io.read_packet(32 << 20).unwrap().unwrap();
        assert_eq!(pkt.seq, 1);
        assert_eq!(pkt.payload, payload);
        // 写侧 seq 已同步为最后帧 seq+1
        assert_eq!(io.seq, 2);

        // 超 max_allowed_packet → TooLarge
        let mut raw = Vec::new();
        raw.extend_from_slice(&[10, 0, 0, 0]);
        raw.extend_from_slice(&[0u8; 10]);
        let mut io = WireIo::new(io::Cursor::new(raw));
        assert!(matches!(io.read_packet(8), Err(ReadError::TooLarge)));
    }

    #[test]
    fn read_returns_closed_on_eof() {
        let mut io = WireIo::new(io::Cursor::new(Vec::new()));
        assert!(matches!(
            io.read_packet(DEFAULT_MAX_ALLOWED_PACKET),
            Ok(None)
        ));
    }

    #[test]
    fn packet_builders() {
        let ok = ok_packet(3, 0, SERVER_STATUS_AUTOCOMMIT, Some("INSERT 0 3"));
        assert_eq!(ok[0], OK_HEADER);
        assert_eq!(ok[1], 3); // lenenc affected
        assert_eq!(ok[2], 0); // lenenc last_insert_id
        assert_eq!(&ok[3..5], &2u16.to_le_bytes()); // status
        assert_eq!(&ok[5..7], &0u16.to_le_bytes()); // warnings
        assert_eq!(ok[7], 10); // lenenc info 长度
        assert_eq!(&ok[8..], b"INSERT 0 3");

        let eof = eof_packet(SERVER_STATUS_AUTOCOMMIT | SERVER_MORE_RESULTS_EXISTS);
        assert_eq!(eof, vec![0xFE, 0, 0, 0x0A, 0x00]);

        let err = err_packet(1045, "28000", "Access denied");
        assert_eq!(err[0], ERR_HEADER);
        assert_eq!(&err[1..3], &1045u16.to_le_bytes());
        assert_eq!(err[3], b'#');
        assert_eq!(&err[4..9], b"28000");
        assert_eq!(&err[9..], b"Access denied");
    }
}
