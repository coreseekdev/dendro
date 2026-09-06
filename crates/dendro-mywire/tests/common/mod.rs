//! 测试共享件：手写迷你 MySQL 客户端 + MockSession（SPEC 10 契约的最小实现）。
//! 迷你客户端按协议自行解析服务端响应，用于状态机级断言。
//! 注意：本模块会被每个测试二进制分别编译，部分条目在个别二进制中未用到。
#![allow(dead_code)]

use dendro_core::engine::WireSession;
use dendro_core::error::{Result, SqlError};
use dendro_core::types::{ColType, ColumnMeta, Output, RecordSet};
use dendro_mywire::MyConfig;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

// —— 常量（与 codec 对应，测试侧独立声明以防实现漂移时测试失真）——
pub const OK: u8 = 0x00;
pub const EOF: u8 = 0xFE;
pub const ERR: u8 = 0xFF;
pub const COM_QUIT: u8 = 0x01;
pub const COM_INIT_DB: u8 = 0x02;
pub const COM_QUERY: u8 = 0x03;
pub const COM_FIELD_LIST: u8 = 0x04;
pub const COM_STATISTICS: u8 = 0x09;
pub const COM_PING: u8 = 0x0e;
pub const COM_STMT_PREPARE: u8 = 0x16;

pub const CLIENT_PROTOCOL_41: u32 = 1 << 9;
pub const CLIENT_SECURE_CONNECTION: u32 = 1 << 15;
pub const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
pub const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
pub const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
pub const CLIENT_DEPRECATE_EOF: u32 = 1 << 24;

/// 迷你客户端：持有 UnixStream::pair 的客户端端
pub struct MiniClient {
    pub stream: UnixStream,
}

impl MiniClient {
    /// 起一个服务端线程跑 handle_connection，返回（客户端, 线程句柄）
    pub fn start(
        sess: Box<dyn WireSession>,
        cfg: MyConfig,
    ) -> (MiniClient, std::thread::JoinHandle<std::io::Result<()>>) {
        let (server_side, client_side) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || {
            dendro_mywire::handle_connection(server_side, sess, cfg)
        });
        let stream = client_side;
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        (MiniClient { stream }, handle)
    }

    /// 读一帧，返回 (seq, payload)
    pub fn read_frame(&mut self) -> (u8, Vec<u8>) {
        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr).expect("read frame header");
        let len = hdr[0] as usize | ((hdr[1] as usize) << 8) | ((hdr[2] as usize) << 16);
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).expect("read frame payload");
        (hdr[3], payload)
    }

    /// 读到对端关闭（COM_QUIT / 认证失败断开的验证）。
    /// 服务端收到未读完的包就断开时客户端会看到 RST，同样视为「已关闭」；
    /// 超时（WouldBlock）说明对端还活着，属于失败。
    pub fn read_eof(&mut self) {
        use std::io::ErrorKind;
        let mut b = [0u8; 1];
        match self.stream.read(&mut b) {
            Ok(0) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                ) => {}
            Err(e) => panic!("expected connection close, got io error: {e}"),
            Ok(n) => panic!("expected connection close, got {n} byte(s)"),
        }
    }

    pub fn write_frame(&mut self, seq: u8, payload: &[u8]) {
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes()[..3]);
        frame.push(seq);
        frame.extend_from_slice(payload);
        self.stream.write_all(&frame).expect("write frame");
    }

    /// 发送命令包（客户端命令 seq 恒 0）
    pub fn cmd(&mut self, cmd: u8, body: &[u8]) {
        let mut p = vec![cmd];
        p.extend_from_slice(body);
        self.write_frame(0, &p);
    }

    /// 主动断开（服务端阻塞在 read 上，join 前必须断开）
    pub fn close(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// 读 ColumnDefinition41×N + EOF（COM_FIELD_LIST 响应：无 column_count 帧）
    pub fn expect_column_defs(&mut self) -> Vec<Column> {
        let mut columns = Vec::new();
        loop {
            let (_seq, p) = self.read_frame();
            if p[0] == EOF {
                return columns;
            }
            columns.push(parse_column_def(&p));
        }
    }

    // —— 解析辅助 ——

    /// 读 Server Greeting v10
    pub fn read_greeting(&mut self) -> Greeting {
        let (seq, p) = self.read_frame();
        assert_eq!(seq, 0, "greeting must be seq 0");
        assert_eq!(p[0], 10, "protocol version");
        let mut i = 1;
        let nul = p[i..].iter().position(|&b| b == 0).unwrap();
        let version = String::from_utf8_lossy(&p[i..i + nul]).into_owned();
        i += nul + 1;
        let conn_id = u32::from_le_bytes(p[i..i + 4].try_into().unwrap());
        i += 4;
        let mut scramble = p[i..i + 8].to_vec();
        i += 8;
        assert_eq!(p[i], 0, "filler");
        i += 1;
        let caps_lo = u16::from_le_bytes(p[i..i + 2].try_into().unwrap());
        i += 2;
        let charset = p[i];
        i += 1;
        let status = u16::from_le_bytes(p[i..i + 2].try_into().unwrap());
        i += 2;
        let caps_hi = u16::from_le_bytes(p[i..i + 2].try_into().unwrap());
        i += 2;
        let auth_len = p[i] as usize;
        assert_eq!(auth_len, 21, "auth-plugin-data len = 20 + NUL");
        i += 1;
        i += 10; // reserved
        let part2 = &p[i..i + (auth_len - 8)];
        assert_eq!(part2[12], 0, "part-2 ends with NUL");
        scramble.extend_from_slice(&part2[..12]);
        i += auth_len - 8;
        let nul = p[i..].iter().position(|&b| b == 0).unwrap();
        let plugin = String::from_utf8_lossy(&p[i..i + nul]).into_owned();
        let capabilities = (caps_lo as u32) | ((caps_hi as u32) << 16);
        Greeting { version, conn_id, scramble, plugin, capabilities, charset, status }
    }

    /// 发送 HandshakeResponse41（SECURE_CONNECTION 1B 长度前缀形态 + PLUGIN_AUTH）
    pub fn send_handshake(
        &mut self,
        g: &Greeting,
        username: &str,
        password: Option<&str>,
        db: Option<&str>,
    ) {
        let mut caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
        if db.is_some() {
            caps |= 1 << 3; // CLIENT_CONNECT_WITH_DB
        }
        let mut p = Vec::new();
        p.extend_from_slice(&caps.to_le_bytes());
        p.extend_from_slice(&16_777_216u32.to_le_bytes()); // max packet
        p.push(45); // utf8mb4_general_ci
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(username.as_bytes());
        p.push(0);
        match password {
            Some(pw) => {
                let token = dendro_mywire::auth::scramble_token(pw, &g.scramble);
                p.push(token.len() as u8);
                p.extend_from_slice(&token);
            }
            None => p.push(0),
        }
        if let Some(db) = db {
            p.extend_from_slice(db.as_bytes());
            p.push(0);
        }
        p.extend_from_slice(b"mysql_native_password");
        p.push(0);
        self.write_frame(1, &p);
    }

    /// 认证/命令后的 OK 包 → (seq, OkPacket)
    pub fn expect_ok(&mut self) -> (u8, OkPacket) {
        let (seq, p) = self.read_frame();
        assert_eq!(p[0], OK, "expected OK packet, got {:02x?} ({})", p[0], String::from_utf8_lossy(&p));
        let mut i = 1;
        let affected = read_lenenc(&p, &mut i);
        let last_insert_id = read_lenenc(&p, &mut i);
        let status = u16::from_le_bytes(p[i..i + 2].try_into().unwrap());
        i += 2;
        let warnings = u16::from_le_bytes(p[i..i + 2].try_into().unwrap());
        i += 2;
        let info = if i < p.len() {
            // info 为 lenenc-str（现代 MySQL 风格）
            String::from_utf8_lossy(read_bytes(&p, &mut i)).into_owned()
        } else {
            String::new()
        };
        (seq, OkPacket { affected, last_insert_id, status, warnings, info })
    }

    /// ERR 包 → (seq, code, sqlstate, message)
    pub fn expect_err(&mut self) -> (u8, u16, String, String) {
        let (seq, p) = self.read_frame();
        assert_eq!(p[0], ERR, "expected ERR packet, got {:02x?}", p);
        let code = u16::from_le_bytes(p[1..3].try_into().unwrap());
        assert_eq!(p[3], b'#');
        let state = String::from_utf8_lossy(&p[4..9]).into_owned();
        let msg = String::from_utf8_lossy(&p[9..]).into_owned();
        (seq, code, state, msg)
    }

    /// EOF 包 → (seq, warnings, status)
    pub fn expect_eof(&mut self) -> (u8, u16, u16) {
        let (seq, p) = self.read_frame();
        assert_eq!(p[0], EOF, "expected EOF packet, got {:02x?}", p);
        (seq, u16::from_le_bytes(p[1..3].try_into().unwrap()), u16::from_le_bytes(p[3..5].try_into().unwrap()))
    }

    /// 读完整 text 结果集：column_count → coldef×N → EOF → 行×M → EOF
    pub fn expect_result_set(&mut self) -> ResultSet {
        let (first_seq, head) = self.read_frame();
        let mut i = 0usize;
        let column_count = read_lenenc(&head, &mut i) as usize;
        let mut columns = Vec::new();
        for _ in 0..column_count {
            let (_s, p) = self.read_frame();
            columns.push(parse_column_def(&p));
        }
        let (_s, _w, _st) = self.expect_eof(); // 列定义后的 EOF
        let mut rows = Vec::new();
        loop {
            let (_s, p) = self.read_frame();
            if p[0] == EOF {
                let status = u16::from_le_bytes(p[3..5].try_into().unwrap());
                return ResultSet { first_seq, column_count, columns, rows, final_status: status };
            }
            let mut r = 0usize;
            let mut row = Vec::new();
            while r < p.len() {
                if p[r] == 0xFB {
                    row.push(None);
                    r += 1;
                } else {
                    row.push(Some(String::from_utf8_lossy(read_bytes(&p, &mut r)).into_owned()));
                }
            }
            rows.push(row);
        }
    }
}

#[derive(Debug)]
pub struct Greeting {
    pub version: String,
    pub conn_id: u32,
    pub scramble: Vec<u8>,
    pub plugin: String,
    pub capabilities: u32,
    pub charset: u8,
    pub status: u16,
}

#[derive(Debug)]
pub struct Column {
    pub name: String,
    pub charset: u16,
    pub column_length: u32,
    pub type_code: u8,
}

#[derive(Debug)]
pub struct OkPacket {
    pub affected: u64,
    pub last_insert_id: u64,
    pub status: u16,
    pub warnings: u16,
    pub info: String,
}

#[derive(Debug)]
pub struct ResultSet {
    pub first_seq: u8,
    pub column_count: usize,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Option<String>>>,
    pub final_status: u16,
}

fn read_lenenc(p: &[u8], i: &mut usize) -> u64 {
    let first = p[*i];
    *i += 1;
    match first {
        0xFC => { let v = u16::from_le_bytes(p[*i..*i + 2].try_into().unwrap()) as u64; *i += 2; v }
        0xFD => { let v = (p[*i] as u64) | ((p[*i + 1] as u64) << 8) | ((p[*i + 2] as u64) << 16); *i += 3; v }
        0xFE => { let v = u64::from_le_bytes(p[*i..*i + 8].try_into().unwrap()); *i += 8; v }
        v => v as u64,
    }
}

fn read_bytes<'a>(p: &'a [u8], i: &mut usize) -> &'a [u8] {
    let n = read_lenenc(p, i) as usize;
    let s = &p[*i..*i + n];
    *i += n;
    s
}

/// 解析单个 ColumnDefinition41 帧
fn parse_column_def(p: &[u8]) -> Column {
    let mut r = 0usize;
    let _catalog = read_bytes(p, &mut r);
    let _schema = read_bytes(p, &mut r);
    let _table = read_bytes(p, &mut r);
    let _org_table = read_bytes(p, &mut r);
    let name = read_bytes(p, &mut r);
    let _org_name = read_bytes(p, &mut r);
    assert_eq!(read_lenenc(p, &mut r), 0x0C, "fixed fields len");
    let charset = u16::from_le_bytes(p[r..r + 2].try_into().unwrap());
    r += 2;
    let column_length = u32::from_le_bytes(p[r..r + 4].try_into().unwrap());
    r += 4;
    let type_code = p[r];
    Column { name: String::from_utf8_lossy(&name).into_owned(), charset, column_length, type_code }
}

// ——————————————————————————— MockSession ———————————————————————————

/// 最小 WireSession 实现（SPEC 10 §1/§3）：
/// `SELECT 1` → 单列 int8 值 1 的 Rows；INSERT → Command；BOOM → Err 传播。
#[derive(Default)]
pub struct MockSession;

fn int64_set(name: &str, vals: &[i64]) -> RecordSet {
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    let schema = Arc::new(Schema::new(vec![Field::new(name, DataType::Int64, false)]));
    let batch = arrow::record_batch::RecordBatch::try_new(
        schema,
        vec![Arc::new(Int64Array::from(vals.to_vec()))],
    )
    .unwrap();
    RecordSet {
        columns: vec![ColumnMeta { name: name.into(), ty: ColType::Int64 }],
        batches: vec![batch],
    }
}

fn utf8_null_set(name: &str) -> RecordSet {
    use arrow::array::StringArray;
    use arrow::datatypes::{DataType, Field, Schema};
    let schema = Arc::new(Schema::new(vec![Field::new(name, DataType::Utf8, true)]));
    let batch = arrow::record_batch::RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![None::<String>]))],
    )
    .unwrap();
    RecordSet {
        columns: vec![ColumnMeta { name: name.into(), ty: ColType::Utf8 }],
        batches: vec![batch],
    }
}

fn users_meta() -> RecordSet {
    RecordSet {
        columns: vec![
            ColumnMeta { name: "id".into(), ty: ColType::Int64 },
            ColumnMeta { name: "name".into(), ty: ColType::Utf8 },
        ],
        batches: vec![],
    }
}

impl WireSession for MockSession {
    fn exec(&mut self, sql: &str) -> Result<Vec<Output>> {
        match sql.trim() {
            "SELECT 1" => Ok(vec![Output::Rows(int64_set("1", &[1]))]),
            "SELECT 2" => Ok(vec![Output::Rows(int64_set("2", &[2]))]),
            "SELECT 1; SELECT 2" => Ok(vec![
                Output::Rows(int64_set("1", &[1])),
                Output::Rows(int64_set("2", &[2])),
            ]),
            "SELECT NULL" => Ok(vec![Output::Rows(utf8_null_set("n"))]),
            "INSERT INTO t VALUES (1),(2)" => Ok(vec![Output::Command {
                tag: "INSERT 0 2".into(),
                affected: 2,
            }]),
            s if s.starts_with("SELECT * FROM `") && s.ends_with("` LIMIT 0") => {
                let t = &s["SELECT * FROM `".len()..s.len() - "` LIMIT 0".len()];
                if t == "users" {
                    Ok(vec![Output::Rows(users_meta())])
                } else {
                    Err(SqlError::undefined_table(format!("table 'cambium.{t}' doesn't exist")))
                }
            }
            "BOOM" => Err(SqlError::undefined_table("table 'cambium.boom' doesn't exist")),
            other => Err(SqlError::syntax(format!("mock session: unexpected sql '{other}'"))),
        }
    }

    fn prepare(
        &mut self,
        _name: &str,
        _sql: &str,
        _hint: &[ColType],
    ) -> Result<dendro_core::engine::PrepareMeta> {
        Err(SqlError::not_supported("mock session: prepare unsupported"))
    }

    fn exec_prepared(&mut self, _name: &str, _params: &[dendro_core::types::SqlValue]) -> Result<Output> {
        Err(SqlError::not_supported("mock session: exec_prepared unsupported"))
    }

    fn close_prepared(&mut self, _name: &str) {}

    fn txn_status(&self) -> u8 {
        b'I'
    }
}

use std::sync::Arc;

/// MockSession 工厂（serve_with 用）
pub fn mock_factory(
) -> dendro_mywire::SessionFactory {
    Arc::new(|| -> Box<dyn WireSession> { Box::new(MockSession) })
}
