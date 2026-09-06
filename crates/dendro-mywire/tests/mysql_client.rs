//! （可选）mysql rust 客户端集成测试：真实 TCP 上用官方协议栈对拍。
//! 覆盖：连接 + 认证（对/错口令）→ COM_QUERY 全流程 → COM_STMT_PREPARE → ERR。

mod common;

use common::mock_factory;
use dendro_mywire::{MyConfig, MyServer};
use mysql::prelude::Queryable;
use std::net::SocketAddr;
use std::time::Duration;

/// 起一个 MockSession 后端，返回 127.0.0.1 随机端口
fn start_server(cfg: MyConfig) -> SocketAddr {
    let server = MyServer::bind("127.0.0.1:0", cfg, mock_factory()).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || server.run());
    addr
}

fn opts(addr: SocketAddr, user: &str, pw: &str) -> mysql::Opts {
    mysql::OptsBuilder::new()
        .ip_or_hostname(Some(addr.ip().to_string()))
        .tcp_port(addr.port())
        .user(Some(user.to_string()))
        .pass(Some(pw.to_string()))
        .db_name(Some("cambium".to_string()))
        .tcp_connect_timeout(Some(Duration::from_secs(5)))
        .into()
}

fn pw_cfg() -> MyConfig {
    MyConfig { password: Some("pw".into()), ..Default::default() }
}

#[test]
fn mysql_crate_connect_and_query() {
    let addr = start_server(pw_cfg());
    let mut conn = mysql::Conn::new(opts(addr, "dendro", "pw")).expect("connect + native auth");
    // COM_QUERY text 结果集全流程
    let v: Option<i64> = conn.query_first("SELECT 1").expect("query");
    assert_eq!(v, Some(1));
    // 命令路径
    conn.query_drop("INSERT INTO t VALUES (1),(2)").expect("command ok");
    // 断连后服务端线程干净退出（不做断言，依赖 5s 超时兜底）
}

#[test]
fn mysql_crate_stmt_prepare_rejected() {
    let addr = start_server(pw_cfg());
    let mut conn = mysql::Conn::new(opts(addr, "dendro", "pw")).expect("connect");
    let err = conn.prep("SELECT 1").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("useServerPrepStmts=false"),
        "expected prepare rejection, got: {msg}"
    );
}

#[test]
fn mysql_crate_wrong_password() {
    let addr = start_server(pw_cfg());
    let err = mysql::Conn::new(opts(addr, "dendro", "bad-pass")).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("Access denied"), "got: {msg}");
}

#[test]
fn mysql_crate_no_password_needed() {
    let addr = start_server(MyConfig::default());
    let opts = mysql::OptsBuilder::new()
        .ip_or_hostname(Some(addr.ip().to_string()))
        .tcp_port(addr.port())
        .user(Some("anyone".to_string()))
        .tcp_connect_timeout(Some(Duration::from_secs(5)));
    let mut conn = mysql::Conn::new(opts).expect("connect without password");
    let v: Option<i64> = conn.query_first("SELECT 1").expect("query");
    assert_eq!(v, Some(1));
}
