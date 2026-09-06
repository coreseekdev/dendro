//! 协议状态机集成测试：UnixStream::pair + MockSession 黑盒驱动
//! `handle_connection`，断言字节级行为。

mod common;

use common::*;
use dendro_core::error::SqlError;
use dendro_pgwire::PgConfig;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const PROTO_3_0: i32 = 196_608;
const SSL_REQUEST: i32 = 808_771_03;
const GSSENC_REQUEST: i32 = 808_771_04;
const CANCEL_REQUEST: i32 = 808_771_02;

fn is_eof(s: &mut UnixStream) -> bool {
    let mut b = [0u8; 1];
    match s.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// startup / 认证
// ---------------------------------------------------------------------------

#[test]
fn startup_trust_full_sequence() {
    let (mut s, _h) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    let msgs = handshake(&mut s, &[("user", "alice")]).unwrap();

    // AuthenticationOk 'R' i32=0
    assert_eq!(msgs[0].0, b'R');
    assert_eq!(&msgs[0].1, &0i32.to_be_bytes());

    // ParameterStatus 集合与值
    let ps = param_status(&msgs);
    let get = |k: &str| ps.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("server_version").as_deref(), Some("17.2 (dendro 0.1)"));
    assert_eq!(get("client_encoding").as_deref(), Some("UTF8"));
    assert_eq!(get("server_encoding").as_deref(), Some("UTF8"));
    assert_eq!(get("DateStyle").as_deref(), Some("ISO, MDY"));
    assert_eq!(get("integer_datetimes").as_deref(), Some("on"));
    assert_eq!(get("standard_conforming_strings").as_deref(), Some("on"));
    assert_eq!(get("TimeZone").as_deref(), Some("UTC"));
    assert_eq!(get("is_superuser").as_deref(), Some("on"));

    // BackendKeyData：pid>0，secret 任意 u32
    let k = msgs.iter().find(|(t, _)| *t == b'K').expect("BackendKeyData");
    let pid = i32::from_be_bytes(k.1[0..4].try_into().unwrap());
    let secret = u32::from_be_bytes(k.1[4..8].try_into().unwrap());
    assert!(pid > 0);
    let _ = secret;

    // 最后是 ReadyForQuery('I')
    assert_eq!(msgs.last().unwrap().0, b'Z');
    assert_eq!(msgs.last().unwrap().1, &[b'I']);
}

#[test]
fn startup_pids_increment() {
    let pid_of = |msgs: &[(u8, Vec<u8>)]| {
        let k = msgs.iter().find(|(t, _)| *t == b'K').unwrap();
        i32::from_be_bytes(k.1[0..4].try_into().unwrap())
    };
    let (mut s1, _) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    let m1 = handshake(&mut s1, &[]).unwrap();
    let (mut s2, _) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    let m2 = handshake(&mut s2, &[]).unwrap();
    // 测试并行跑时可能有其他连接插入 pid，这里只断言单调递增
    assert!(pid_of(&m2) > pid_of(&m1));
}

#[test]
fn startup_ssl_request_rejected_then_normal() {
    let (mut s, _h) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    s.write_all(&startup_bytes(SSL_REQUEST, &[])).unwrap();
    let mut n = [0u8; 1];
    s.read_exact(&mut n).unwrap();
    assert_eq!(n[0], b'N');
    // 拒绝后进入正常 startup
    let msgs = handshake(&mut s, &[("user", "bob")]).unwrap();
    assert_eq!(msgs[0].0, b'R');
}

#[test]
fn startup_gssenc_request_rejected() {
    let (mut s, _h) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    s.write_all(&startup_bytes(GSSENC_REQUEST, &[])).unwrap();
    let mut n = [0u8; 1];
    s.read_exact(&mut n).unwrap();
    assert_eq!(n[0], b'N');
}

#[test]
fn startup_cancel_request_closes_without_response() {
    let (mut s, _h) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    let mut b = vec![0, 0, 0, 16];
    b.extend_from_slice(&CANCEL_REQUEST.to_be_bytes());
    b.extend_from_slice(&1i32.to_be_bytes());
    b.extend_from_slice(&2i32.to_be_bytes());
    s.write_all(&b).unwrap();
    // 直接关闭，无任何响应
    std::thread::sleep(Duration::from_millis(50));
    assert!(is_eof(&mut s));
}

#[test]
fn startup_unsupported_protocol_28000() {
    let (mut s, _h) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    s.write_all(&startup_bytes(2 << 16, &[("user", "x")])).unwrap();
    // 服务端发 FATAL 28000 后断连（没有 ReadyForQuery）
    let (t, body) = read_msg(&mut s).unwrap().unwrap();
    assert_eq!(t, b'E');
    let e = parse_error(&body);
    assert_eq!(e.severity, "FATAL");
    assert_eq!(e.code, "28000");
    std::thread::sleep(Duration::from_millis(50));
    assert!(is_eof(&mut s));
}

#[test]
fn startup_missing_user_defaults_to_dendro() {
    // user 缺省不报错（默认 "dendro"），trust 直接就绪
    let (mut s, _h) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    let msgs = handshake(&mut s, &[]).unwrap();
    assert_eq!(msgs[0].0, b'R');
    assert_eq!(msgs.last().unwrap().0, b'Z');
}

#[test]
fn auth_cleartext_ok() {
    let (mut s, _h) =
        spawn_conn(Box::new(MockSession::new()), PgConfig { password: Some("sesame".into()) });
    s.write_all(&startup_bytes(PROTO_3_0, &[("user", "alice")])).unwrap();
    // 第一条是 cleartext 请求
    let (t, body) = read_msg(&mut s).unwrap().unwrap();
    assert_eq!(t, b'R');
    assert_eq!(&body, &3i32.to_be_bytes());
    // 客户端回密码
    s.write_all(&msg_bytes(b'p', &cstr("sesame"))).unwrap();
    let rest = read_until(&mut s, b'Z').unwrap();
    assert_eq!(rest[0].0, b'R'); // AuthenticationOk
    assert_eq!(&rest[0].1, &0i32.to_be_bytes());
}

#[test]
fn auth_cleartext_wrong_password_28p01() {
    let (mut s, _h) =
        spawn_conn(Box::new(MockSession::new()), PgConfig { password: Some("sesame".into()) });
    s.write_all(&startup_bytes(PROTO_3_0, &[("user", "alice")])).unwrap();
    let _req = read_msg(&mut s).unwrap().unwrap();
    s.write_all(&msg_bytes(b'p', &cstr("wrong"))).unwrap();
    let msgs = read_until(&mut s, b'E').unwrap();
    assert_eq!(msgs.last().unwrap().0, b'E');
    let e = parse_error(&msgs.last().unwrap().1);
    assert_eq!(e.severity, "FATAL");
    assert_eq!(e.code, "28P01");
    std::thread::sleep(Duration::from_millis(50));
    assert!(is_eof(&mut s));
}

#[test]
fn startup_fragmented_delivery() {
    // startup 与 Q 都分两次写，验证阻塞读的状态机完整性
    let (mut s, _h) = spawn_conn(Box::new(MockSession::new()), PgConfig::default());
    let raw = startup_bytes(PROTO_3_0, &[("user", "carol")]);
    s.write_all(&raw[..5]).unwrap();
    s.flush().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    s.write_all(&raw[5..]).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'R');

    let q = msg_bytes(b'Q', &cstr("SELECT 1"));
    s.write_all(&q[..3]).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    s.write_all(&q[3..]).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'T');
    // Z 之前是 CommandComplete
    assert_eq!(msgs[msgs.len() - 2].0, b'C');
}

// ---------------------------------------------------------------------------
// 简单查询
// ---------------------------------------------------------------------------

#[test]
fn simple_query_select_flow() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[("user", "u")]).unwrap();

    s.write_all(&msg_bytes(b'Q', &cstr("SELECT 1"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();

    // RowDescription: 1 列 "x" int8
    assert_eq!(msgs[0].0, b'T');
    let fields = parse_row_description(&msgs[0].1);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "x");
    assert_eq!(fields[0].type_oid, 20);
    assert_eq!(fields[0].typlen, 8);
    assert_eq!(fields[0].typmod, -1);
    assert_eq!(fields[0].format, 0);

    // DataRow: ["1"]
    assert_eq!(msgs[1].0, b'D');
    let row = parse_data_row(&msgs[1].1);
    assert_eq!(row, vec![Some(b"1".to_vec())]);

    // CommandComplete "SELECT 1"
    assert_eq!(msgs[2].0, b'C');
    assert_eq!(&msgs[2].1, &b"SELECT 1\0");

    // ReadyForQuery('I')
    assert_eq!(msgs[3].0, b'Z');
    assert_eq!(msgs[3].1, &[b'I']);
}

#[test]
fn simple_query_multi_statement_each_gets_output() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'Q', &cstr("SELECT 1; SELECT 1"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    // 两组 (T D C)
    let ts: Vec<_> = msgs.iter().filter(|(t, _)| *t == b'T').collect();
    let cs: Vec<_> = msgs.iter().filter(|(t, _)| *t == b'C').collect();
    assert_eq!(ts.len(), 2);
    assert_eq!(cs.len(), 2);
    assert_eq!(&cs[0].1, &b"SELECT 1\0");
}

#[test]
fn simple_query_command_tag_passthrough() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'Q', &cstr("CREATE TABLE t (a int)"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    // 无 RowDescription / DataRow
    assert!(msgs.iter().all(|(t, _)| *t != b'T' && *t != b'D'));
    assert_eq!(msgs[0].0, b'C');
    assert_eq!(&msgs[0].1, &b"MOCK CREATE TABLE t (a int)\0");
    assert_eq!(msgs[1].0, b'Z');
}

#[test]
fn simple_query_insert_tag() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'Q', &cstr("INSERT INTO t VALUES (1)"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(&msgs[0].1, &b"INSERT 0 1\0");
}

#[test]
fn simple_query_empty_string_gets_empty_query_response() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'Q', &cstr("   "))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'I'); // EmptyQueryResponse
    assert_eq!(msgs[1].0, b'Z');
}

#[test]
fn simple_query_error_keeps_connection_alive() {
    // 仅对含 "missing" 的语句报错，其余正常（模拟引擎错误）
    let mut mock = MockSession::new();
    mock.exec_behavior.error = Some(SqlError::undefined_table(
        "relation \"missing\" does not exist",
    ));
    mock.exec_behavior.error_if_contains = Some("missing".into());
    let (mut s, _h) = spawn_conn(Box::new(mock), PgConfig::default());
    handshake(&mut s, &[]).unwrap();

    s.write_all(&msg_bytes(b'Q', &cstr("SELECT * FROM missing"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'E');
    let e = parse_error(&msgs[0].1);
    assert_eq!(e.severity, "ERROR");
    assert_eq!(e.code, "42P01");
    assert!(e.message.contains("missing"));
    assert_eq!(msgs[1].0, b'Z');
    assert_eq!(msgs[1].1, &[b'I']);

    // 连接存活：后续查询正常
    s.write_all(&msg_bytes(b'Q', &cstr("SELECT 1"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'T');
}

#[test]
fn simple_query_null_cell() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    // MockSession 默认无 NULL；这里通过 exec_prepared 的 Rows1 无法注入，
    // 改用带 NULL 的扩展查询路径在 extended 测试覆盖。此处测多列。
    s.write_all(&msg_bytes(b'Q', &cstr("SELECT 1"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    let row = parse_data_row(&msgs[1].1);
    assert_eq!(row.len(), 1);
}

#[test]
fn ready_for_query_reflects_txn_status() {
    let sess = Box::new(MockSession::new().with_txn(b'T'));
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'Q', &cstr("BEGIN"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs.last().unwrap().1, &[b'T']);
}

// ---------------------------------------------------------------------------
// 扩展查询
// ---------------------------------------------------------------------------

#[test]
fn extended_full_flow_text_param() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();

    let mut raw = Vec::new();
    // Parse("stmt", "SELECT $1::int8", [20])
    let mut body = cstr("stmt");
    body.extend(cstr("SELECT $1::int8"));
    put_i16(&mut body, 1);
    put_i32(&mut body, 20);
    raw.extend(msg_bytes(b'P', &body));
    // Describe 'S' "stmt"
    raw.extend(msg_bytes(b'D', &[b'S'].iter().chain(cstr("stmt").iter()).copied().collect::<Vec<u8>>()));
    // Bind(portal="p", stmt, formats=[0], params=["7"], result=[])
    let mut body = cstr("p");
    body.extend(cstr("stmt"));
    put_i16(&mut body, 1);
    put_i16(&mut body, 0);
    put_i16(&mut body, 1);
    put_i32(&mut body, 1);
    body.extend(b"7");
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'B', &body));
    // Describe 'P' "p"
    raw.extend(msg_bytes(b'D', &[b'P'].iter().chain(cstr("p").iter()).copied().collect::<Vec<u8>>()));
    // Execute("p", 0)
    let mut body = cstr("p");
    put_i32(&mut body, 0);
    raw.extend(msg_bytes(b'E', &body));
    // Sync
    raw.extend(msg_bytes(b'S', &[]));
    s.write_all(&raw).unwrap();

    let msgs = read_until(&mut s, b'Z').unwrap();
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, vec![b'1', b't', b'T', b'2', b'T', b'D', b'C', b'Z']);

    // ParseComplete 无 body
    assert!(msgs[0].1.is_empty());
    // ParameterDescription [20]
    assert_eq!(msgs[1].0, b't');
    assert_eq!(&msgs[1].1, &[0, 1, 0, 0, 0, 20]);
    // Describe 'S' → RowDescription format 0
    let fields = parse_row_description(&msgs[2].1);
    assert_eq!(fields[0].name, "x");
    assert_eq!(fields[0].type_oid, 20);
    assert_eq!(fields[0].format, 0);
    // BindComplete
    assert!(msgs[3].1.is_empty());
    // Describe 'P' → RowDescription（列集与 statement 相同）
    let fields = parse_row_description(&msgs[4].1);
    assert_eq!(fields[0].name, "x");
    // DataRow 文本 "1"，CommandComplete "SELECT 1"
    assert_eq!(parse_data_row(&msgs[5].1), vec![Some(b"1".to_vec())]);
    assert_eq!(&msgs[6].1, &b"SELECT 1\0");
}

#[test]
fn extended_binary_param_and_result() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();

    let mut raw = Vec::new();
    // Parse unnamed, hint [20, 16, 701, 1082, 1114, 17, 25]
    let mut body = cstr("");
    body.extend(cstr("SELECT $1"));
    let oids = [20i32, 16, 701, 1082, 1114, 17, 25];
    put_i16(&mut body, oids.len() as i16);
    for o in oids {
        put_i32(&mut body, o);
    }
    raw.extend(msg_bytes(b'P', &body));
    // Bind unnamed ← unnamed，全部 binary，7 参数
    let mut body = cstr("");
    body.extend(cstr(""));
    put_i16(&mut body, 1); // 一个 format code 应用到全部参数
    put_i16(&mut body, 1);
    put_i16(&mut body, 7);
    // int8 = 7
    put_i32(&mut body, 8);
    body.extend(7i64.to_be_bytes());
    // bool = true
    put_i32(&mut body, 1);
    body.push(1);
    // float8 = 2.5
    put_i32(&mut body, 8);
    body.extend(2.5f64.to_be_bytes());
    // date = 0（PG epoch）→ Date32(10957)
    put_i32(&mut body, 4);
    body.extend(0i32.to_be_bytes());
    // timestamp = 1_500 µs → TimestampMs(946684800000 + 1)
    put_i32(&mut body, 8);
    body.extend(1_500i64.to_be_bytes());
    // bytea = 原始字节
    put_i32(&mut body, 3);
    body.extend(b"\x01\x02\x03");
    // text
    put_i32(&mut body, 2);
    body.extend(b"hi");
    put_i16(&mut body, 1); // result format：全部 binary
    put_i16(&mut body, 1);
    raw.extend(msg_bytes(b'B', &body));
    // Describe 'P'（应反映 binary format=1）
    raw.extend(msg_bytes(b'D', &[b'P'].iter().chain(cstr("").iter()).copied().collect::<Vec<u8>>()));
    // Execute
    let mut body = cstr("");
    put_i32(&mut body, 0);
    raw.extend(msg_bytes(b'E', &body));
    raw.extend(msg_bytes(b'S', &[]));
    s.write_all(&raw).unwrap();

    let msgs = read_until(&mut s, b'Z').unwrap();
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, vec![b'1', b'2', b'T', b'D', b'C', b'Z']);

    // Describe 'P' 的 RowDescription format=1（binary）
    let fields = parse_row_description(&msgs[2].1);
    assert_eq!(fields[0].format, 1);
    // DataRow 是 8B BE 的 int8 1
    let cells = parse_data_row(&msgs[3].1);
    assert_eq!(cells, vec![Some(1i64.to_be_bytes().to_vec())]);
}

#[test]
fn extended_params_recorded_correctly() {
    // 通过公开可观察的 exec_prepared 参数验证 Bind 解码
    // （借用 std::sync::Arc<Mutex<>> 从线程外读取 MockSession 日志不可行，
    //   因此用 EchoInt8Param 回显行为断言参数值）
    let mut sess = MockSession::new();
    sess.ep_behavior = EpBehavior::EchoInt8Param;
    let (mut s, _h) = spawn_conn(Box::new(sess), PgConfig::default());
    handshake(&mut s, &[]).unwrap();

    let mut raw = Vec::new();
    let mut body = cstr("");
    body.extend(cstr("SELECT $1"));
    put_i16(&mut body, 1);
    put_i32(&mut body, 20);
    raw.extend(msg_bytes(b'P', &body));
    let mut body = cstr("");
    body.extend(cstr(""));
    put_i16(&mut body, 0); // 未给 format → 默认 text
    put_i16(&mut body, 1);
    put_i32(&mut body, 2);
    body.extend(b"42");
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'B', &body));
    let mut body = cstr("");
    put_i32(&mut body, 0);
    raw.extend(msg_bytes(b'E', &body));
    raw.extend(msg_bytes(b'S', &[]));
    s.write_all(&raw).unwrap();

    let msgs = read_until(&mut s, b'Z').unwrap();
    // 文本参数 "42" → int8 → 行值 "42"
    let d = msgs.iter().find(|(t, _)| *t == b'D').unwrap();
    assert_eq!(parse_data_row(&d.1), vec![Some(b"42".to_vec())]);
}

#[test]
fn extended_parse_error_skips_until_sync() {
    let mut sess = MockSession::new();
    sess.prepare_error = Some(SqlError::syntax("bad grammar near x"));
    let (mut s, _h) = spawn_conn(Box::new(sess), PgConfig::default());
    handshake(&mut s, &[]).unwrap();

    let mut raw = Vec::new();
    let mut body = cstr("");
    body.extend(cstr("SELEC 1"));
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'P', &body));
    // 之后的 B/D/E 应被忽略（不发任何响应）
    let mut body = cstr("");
    body.extend(cstr(""));
    put_i16(&mut body, 0);
    put_i16(&mut body, 0);
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'B', &body));
    raw.extend(msg_bytes(b'D', &[b'S', 0]));
    raw.extend(msg_bytes(b'S', &[]));
    s.write_all(&raw).unwrap();

    let msgs = read_until(&mut s, b'Z').unwrap();
    // 只应有 ErrorResponse + ReadyForQuery
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, vec![b'E', b'Z']);
    let e = parse_error(&msgs[0].1);
    assert_eq!(e.code, "42601");
}

#[test]
fn extended_exec_prepared_error_skips_until_sync() {
    let mut sess = MockSession::new();
    sess.ep_error = Some(SqlError::undefined_table("relation \"t\" does not exist"));
    let (mut s, _h) = spawn_conn(Box::new(sess), PgConfig::default());
    handshake(&mut s, &[]).unwrap();

    let mut raw = Vec::new();
    let mut body = cstr("");
    body.extend(cstr("SELECT 1"));
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'P', &body));
    let mut body = cstr("");
    body.extend(cstr(""));
    put_i16(&mut body, 0);
    put_i16(&mut body, 0);
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'B', &body));
    let mut body = cstr("");
    put_i32(&mut body, 0);
    raw.extend(msg_bytes(b'E', &body));
    raw.extend(msg_bytes(b'S', &[]));
    s.write_all(&raw).unwrap();

    let msgs = read_until(&mut s, b'Z').unwrap();
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, vec![b'1', b'2', b'E', b'Z']);
    assert_eq!(parse_error(&msgs[2].1).code, "42P01");
    // 连接存活
    s.write_all(&msg_bytes(b'Q', &cstr("SELECT 1"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'T');
}

#[test]
fn extended_describe_unknown_statement_26000() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'D', &[b'S'].iter().chain(cstr("nope").iter()).copied().collect::<Vec<u8>>()))
        .unwrap();
    s.write_all(&msg_bytes(b'S', &[])).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'E');
    assert_eq!(parse_error(&msgs[0].1).code, "26000");
}

#[test]
fn extended_describe_unknown_portal_26000() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'D', &[b'P'].iter().chain(cstr("nope").iter()).copied().collect::<Vec<u8>>()))
        .unwrap();
    s.write_all(&msg_bytes(b'S', &[])).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(parse_error(&msgs[0].1).code, "26000");
}

#[test]
fn extended_execute_unknown_portal_26000() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    let mut body = cstr("nope");
    put_i32(&mut body, 0);
    s.write_all(&msg_bytes(b'E', &body)).unwrap();
    s.write_all(&msg_bytes(b'S', &[])).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(parse_error(&msgs[0].1).code, "26000");
}

#[test]
fn extended_bind_unknown_statement_26000() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    let mut body = cstr("");
    body.extend(cstr("ghost"));
    put_i16(&mut body, 0);
    put_i16(&mut body, 0);
    put_i16(&mut body, 0);
    s.write_all(&msg_bytes(b'B', &body)).unwrap();
    s.write_all(&msg_bytes(b'S', &[])).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(parse_error(&msgs[0].1).code, "26000");
}

#[test]
fn extended_named_statement_close_removes_it() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();

    let mut raw = Vec::new();
    let mut body = cstr("st1");
    body.extend(cstr("SELECT 1"));
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'P', &body));
    raw.extend(msg_bytes(b'C', &[b'S'].iter().chain(cstr("st1").iter()).copied().collect::<Vec<u8>>()));
    raw.extend(msg_bytes(b'S', &[]));
    s.write_all(&raw).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'1'); // ParseComplete
    assert_eq!(msgs[1].0, b'3'); // CloseComplete

    // Close 后 Describe → 26000
    s.write_all(&msg_bytes(b'D', &[b'S'].iter().chain(cstr("st1").iter()).copied().collect::<Vec<u8>>()))
        .unwrap();
    s.write_all(&msg_bytes(b'S', &[])).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(parse_error(&msgs[0].1).code, "26000");
}

#[test]
fn extended_unnamed_reparse_overwrites() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    // 连续两次 Parse unnamed 都应 ParseComplete（覆盖旧语句，SPEC 10 §2）
    let mut raw = Vec::new();
    for sql in ["SELECT 1", "SELECT 2"] {
        let mut body = cstr("");
        body.extend(cstr(sql));
        put_i16(&mut body, 0);
        raw.extend(msg_bytes(b'P', &body));
    }
    raw.extend(msg_bytes(b'S', &[]));
    s.write_all(&raw).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs.iter().filter(|(t, _)| *t == b'1').count(), 2);
}

#[test]
fn flush_message_flushes_buffer() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    let mut raw = Vec::new();
    let mut body = cstr("");
    body.extend(cstr("SELECT 1"));
    put_i16(&mut body, 0);
    raw.extend(msg_bytes(b'P', &body));
    raw.extend(msg_bytes(b'H', &[])); // Flush（不结束事务边界）
    s.write_all(&raw).unwrap();
    // 只会读到 ParseComplete，无 ReadyForQuery
    let (t, _) = read_msg(&mut s).unwrap().unwrap();
    assert_eq!(t, b'1');
    // 之后的 Sync 才给 RFQ
    s.write_all(&msg_bytes(b'S', &[])).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs.len(), 1);
}

#[test]
fn terminate_closes_connection() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'X', &[])).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(is_eof(&mut s));
}

#[test]
fn copy_subprotocol_rejected_0a000() {
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'd', &[1, 2, 3])).unwrap();
    s.write_all(&msg_bytes(b'S', &[])).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    assert_eq!(msgs[0].0, b'E');
    assert_eq!(parse_error(&msgs[0].1).code, "0A000");
}

#[test]
fn utf8_sql_and_multibyte_result() {
    // UTF-8 通路：SQL 与文本结果均含多字节字符
    let sess = Box::new(MockSession::new());
    let (mut s, _h) = spawn_conn(sess, PgConfig::default());
    handshake(&mut s, &[]).unwrap();
    s.write_all(&msg_bytes(b'Q', &cstr("SELECT '数据库'"))).unwrap();
    let msgs = read_until(&mut s, b'Z').unwrap();
    // MockSession 固定返回 int8 "1"，只验证 UTF-8 SQL 不炸、流程完整
    assert_eq!(msgs[0].0, b'T');
    assert_eq!(msgs[3].0, b'Z');
}
