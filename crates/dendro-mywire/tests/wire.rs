//! 状态机测试：UnixStream::pair + 迷你客户端驱动完整握手/命令循环（SPEC 06 §3）。

mod common;

use common::*;
use dendro_mywire::MyConfig;

const AUTOCOMMIT: u16 = 0x0002;
const MORE_RESULTS: u16 = 0x0008;

fn default_cfg() -> MyConfig {
    MyConfig::default()
}

/// 完成无密码握手（含 seq 断言）并返回 (客户端, 句柄)
fn connect_ok(cfg: MyConfig) -> (MiniClient, std::thread::JoinHandle<std::io::Result<()>>) {
    let (mut c, h) = MiniClient::start(Box::new(MockSession), cfg);
    let g = c.read_greeting();
    c.send_handshake(&g, "root", None, None);
    let (seq, ok) = c.expect_ok();
    assert_eq!(
        seq, 2,
        "认证 OK 应为 seq 2（greeting 0 → 客户端 1 → 服务端 2）"
    );
    assert_eq!(ok.affected, 0);
    assert_eq!(ok.status & AUTOCOMMIT, AUTOCOMMIT);
    (c, h)
}

#[test]
fn handshake_greeting_and_ok() {
    let (mut c, h) = {
        let (mut c, h) = MiniClient::start(Box::new(MockSession), default_cfg());
        let g = c.read_greeting();
        // SPEC 06 §3 握手字段
        assert_eq!(g.version, "8.0.36-dendro");
        assert_eq!(g.plugin, "mysql_native_password");
        assert_eq!(g.charset, 45); // utf8mb4_general_ci
        assert_eq!(g.status, AUTOCOMMIT);
        assert_eq!(g.scramble.len(), 20, "auth-plugin-data 共 20B（8 + 12）");
        assert_eq!(
            g.capabilities & CLIENT_DEPRECATE_EOF,
            0,
            "不宣告 DEPRECATE_EOF"
        );
        assert_ne!(g.capabilities & CLIENT_PROTOCOL_41, 0);
        assert_ne!(g.capabilities & CLIENT_SECURE_CONNECTION, 0);
        assert_ne!(g.capabilities & CLIENT_PLUGIN_AUTH, 0);
        assert_ne!(g.capabilities & CLIENT_MULTI_STATEMENTS, 0);
        assert_ne!(g.capabilities & CLIENT_MULTI_RESULTS, 0);
        assert_ne!(g.capabilities & (1 << 3), 0, "CLIENT_CONNECT_WITH_DB");
        assert_ne!(g.capabilities & (1 << 13), 0, "CLIENT_TRANSACTIONS");
        // 无密码 → 直接 OK
        c.send_handshake(&g, "root", None, None);
        c.expect_ok();
        (c, h)
    };
    c.close(); // 握手后客户端关闭，服务端应干净退出
    h.join().unwrap().unwrap();
}

#[test]
fn auth_no_password_accepts_any() {
    let (mut c, h) = connect_ok(default_cfg());
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn auth_correct_password() {
    let mut cfg = default_cfg();
    cfg.password = Some("secret".into());
    let (mut c, h) = MiniClient::start(Box::new(MockSession), cfg);
    let g = c.read_greeting();
    c.send_handshake(&g, "app", Some("secret"), None);
    c.expect_ok();
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn auth_wrong_password_err_1045() {
    let mut cfg = default_cfg();
    cfg.password = Some("secret".into());
    let (mut c, h) = MiniClient::start(Box::new(MockSession), cfg);
    let g = c.read_greeting();
    c.send_handshake(&g, "app", Some("wrong"), None);
    let (_seq, code, state, msg) = c.expect_err();
    assert_eq!(
        (code, state.as_str()),
        (1045, "28000"),
        "ER_ACCESS_DENIED_ERROR"
    );
    assert!(msg.contains("Access denied"), "got: {msg}");
    c.read_eof(); // 认证失败 → 服务端断开
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_query_select_1_full_resultset() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_QUERY, b"SELECT 1");
    let rs = c.expect_result_set();
    assert_eq!(rs.first_seq, 1, "响应帧从命令包 seq 0 + 1 开始");
    assert_eq!(rs.column_count, 1);
    let col = &rs.columns[0];
    assert_eq!(col.name, "1");
    assert_eq!(col.type_code, 8, "Int64 → LONGLONG");
    assert_eq!(col.charset, 63, "数值列 binary");
    assert_eq!(col.column_length, 20);
    assert_eq!(rs.rows, vec![vec![Some("1".to_string())]]);
    assert_eq!(rs.final_status & AUTOCOMMIT, AUTOCOMMIT);
    assert_eq!(rs.final_status & MORE_RESULTS, 0);
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_query_null_cell() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_QUERY, b"SELECT NULL");
    let rs = c.expect_result_set();
    assert_eq!(rs.column_count, 1);
    assert_eq!(rs.rows, vec![vec![None]], "NULL → 0xFB");
    assert_eq!(rs.columns[0].type_code, 253, "Utf8 → VAR_STRING");
    assert_eq!(rs.columns[0].charset, 33, "文本列 utf8_general_ci");
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_query_command_ok_affected() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_QUERY, b"INSERT INTO t VALUES (1),(2)");
    let (_seq, ok) = c.expect_ok();
    assert_eq!(
        ok.affected, 2,
        "OK 包直接携带 affected 数值（tag 为 PG 口径）"
    );
    assert_eq!(ok.last_insert_id, 0);
    assert_eq!(ok.warnings, 0);
    assert_eq!(ok.info, "INSERT 0 2", "tag 作为 info 附带");
    assert_eq!(ok.status & AUTOCOMMIT, AUTOCOMMIT);
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_query_error_propagates() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_QUERY, b"BOOM");
    let (_seq, code, state, msg) = c.expect_err();
    // SqlError(42P01) → MySQL ER_NO_SUCH_TABLE 1146 / 42S02
    assert_eq!((code, state.as_str()), (1146, "42S02"));
    assert_eq!(msg, "table 'cambium.boom' doesn't exist");
    // 错误后连接保持（SPEC 10 §5：wire 层在 ErrorResponse 后保持连接）
    c.cmd(COM_PING, b"");
    c.expect_ok();
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_query_syntax_error_mapping() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_QUERY, b"SELEKT 1");
    let (_seq, code, state, _msg) = c.expect_err();
    // SqlError(42601) → MySQL ER_PARSE_ERROR 1064 / 42000
    assert_eq!((code, state.as_str()), (1064, "42000"));
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_query_multi_results() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_QUERY, b"SELECT 1; SELECT 2");
    let rs1 = c.expect_result_set();
    assert_eq!(rs1.rows, vec![vec![Some("1".into())]]);
    assert_eq!(
        rs1.final_status & MORE_RESULTS,
        MORE_RESULTS,
        "第一个结果集带 MORE_RESULTS_EXISTS"
    );
    let rs2 = c.expect_result_set();
    assert_eq!(rs2.rows, vec![vec![Some("2".into())]]);
    assert_eq!(rs2.final_status & MORE_RESULTS, 0, "最后一个结果集不带");
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_ping_ok() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_PING, b"");
    let (seq, ok) = c.expect_ok();
    assert_eq!(seq, 1);
    assert_eq!(ok.affected, 0);
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_init_db_ok() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_INIT_DB, b"cambium");
    c.expect_ok();
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_statistics_text() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_STATISTICS, b"");
    let (_seq, p) = c.read_frame();
    let text = String::from_utf8(p).unwrap();
    assert!(text.starts_with("Uptime\t"), "got: {text}");
    assert!(text.contains("Questions\t"));
    assert!(text.contains("Queries per second avg\t"));
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_field_list_table() {
    let (mut c, h) = connect_ok(default_cfg());
    // 表存在：users → 2 列 ColumnDefinition41 + EOF（无 column_count 帧）
    c.cmd(COM_FIELD_LIST, b"users\0");
    let cols = c.expect_column_defs();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[0].type_code, 8);
    assert_eq!(cols[1].name, "name");
    assert_eq!(cols[1].type_code, 253);
    // 表不存在：ghost → ERR 1146 / 42S02
    c.cmd(COM_FIELD_LIST, b"ghost\0");
    let (_seq, code, state, _msg) = c.expect_err();
    assert_eq!((code, state.as_str()), (1146, "42S02"), "ER_NO_SUCH_TABLE");
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_quit_closes() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_QUIT, b"");
    c.read_eof();
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn com_stmt_prepare_err_1047() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(COM_STMT_PREPARE, b"SELECT 1");
    let (_seq, code, state, msg) = c.expect_err();
    assert_eq!(
        (code, state.as_str()),
        (1047, "08S01"),
        "ER_UNKNOWN_COM_ERROR"
    );
    assert!(msg.contains("useServerPrepStmts=false"), "got: {msg}");
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn unknown_command_err_1047() {
    let (mut c, h) = connect_ok(default_cfg());
    c.cmd(0x99, b"");
    let (_seq, code, state, _msg) = c.expect_err();
    assert_eq!((code, state.as_str()), (1047, "08S01"));
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn oversized_packet_err_1153() {
    let mut cfg = default_cfg();
    cfg.max_allowed_packet = 64; // 缩小阈值便于测试
    let (mut c, h) = connect_ok(cfg);
    // 100B 逻辑包 > max_allowed_packet=64 → ERR 1153 后断开
    let mut p = vec![COM_QUERY];
    p.extend_from_slice(&[b'A'; 100]);
    c.write_frame(0, &p);
    let (_seq, code, state, _msg) = c.expect_err();
    assert_eq!(
        (code, state.as_str()),
        (1153, "08S01"),
        "ER_NET_PACKET_TOO_LARGE"
    );
    c.read_eof();
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn auth_switch_for_foreign_plugin() {
    let mut cfg = default_cfg();
    cfg.password = Some("secret".into());
    let (mut c, h) = MiniClient::start(Box::new(MockSession), cfg);
    let g = c.read_greeting();
    // 客户端带了别的插件名（如 caching_sha2_password）→ 服务端应回 AuthSwitchRequest
    let mut p = Vec::new();
    let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    p.extend_from_slice(&caps.to_le_bytes());
    p.extend_from_slice(&16_777_216u32.to_le_bytes());
    p.push(45);
    p.extend_from_slice(&[0u8; 23]);
    p.extend_from_slice(b"app");
    p.push(0);
    p.push(32);
    p.extend_from_slice(&[0xEE; 32]); // 原插件形态的 token（服务端不认识该插件）
    p.extend_from_slice(b"caching_sha2_password");
    p.push(0);
    c.write_frame(1, &p);
    // AuthSwitchRequest：0xfe + "mysql_native_password" NUL + scramble 20B + NUL
    let (_seq, sw) = c.read_frame();
    assert_eq!(sw[0], 0xFE, "AuthSwitchRequest header");
    let nul = sw[1..].iter().position(|&b| b == 0).unwrap();
    assert_eq!(&sw[1..1 + nul], b"mysql_native_password");
    let scramble: Vec<u8> = sw[2 + nul..2 + nul + 20].to_vec();
    assert_eq!(scramble, g.scramble, "switch 携带同一 scramble");
    assert_eq!(sw[2 + nul + 20], 0);
    // 用正确口令按 native 算法应答（裸 20B token 包，无命令字节）
    let token = dendro_mywire::auth::scramble_token("secret", &scramble);
    c.write_frame(2, &token);
    c.expect_ok();
    c.close();
    h.join().unwrap().unwrap();
}

#[test]
fn handshake_ssl_request_rejected() {
    let (mut c, h) = MiniClient::start(Box::new(MockSession), default_cfg());
    let _g = c.read_greeting();
    c.write_frame(1, &[0u8; 32]); // SSLRequest 形态（32B 定长；v1 无 TLS）
    let (_seq, code, state, _msg) = c.expect_err();
    assert_eq!((code, state.as_str()), (1043, "08S01"));
    c.read_eof();
    c.close();
    h.join().unwrap().unwrap();
}
