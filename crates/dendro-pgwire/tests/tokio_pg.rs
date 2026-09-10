//! 真实 PG 客户端（tokio-postgres）兼容性集成测试。
//!
//! 说明：`Database::open(StoreConfig::Memory)` 在引擎桩阶段返回
//! XX000 "engine not yet implemented"（dendro-core engine.rs 占位实现），
//! 无法构造 `Arc<Database>` 走 `serve()`；因此这里用 std 线程监听 +
//! MockSession（`Box<dyn WireSession>`）提供同等的服务端路径
//! （`handle_connection`）。引擎就绪后只需把 factory 换成
//! `db.new_session()`。重点验证：连接/认证握手、简单与扩展查询往返、
//! SQLSTATE 透传、出错后连接复用——与真实客户端的字节级兼容。

mod common;

use std::net::TcpListener;
use std::sync::Arc;

use common::MockSession;
use dendro_core::error::SqlError;
use dendro_pgwire::PgConfig;
use tokio_postgres::NoTls;

type Factory = Arc<dyn Fn() -> MockSession + Send + Sync>;

/// 起一个 std::net 监听线程，每连接用 factory 新建会话并跑协议状态机
fn spawn_server(cfg: PgConfig, factory: impl Fn() -> MockSession + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let factory: Factory = Arc::new(factory);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            let factory = Arc::clone(&factory);
            let cfg = cfg.clone();
            std::thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let sess: Box<dyn dendro_core::engine::WireSession> = Box::new(factory());
                if let Err(e) = dendro_pgwire::handle_connection(stream, sess, cfg) {
                    eprintln!("[pgwire-test] connection error: {e}");
                }
            });
        }
    });
    port
}

fn conn_string(port: u16, user: &str, password: Option<&str>) -> String {
    let mut s = format!("host=127.0.0.1 port={port} user={user}");
    if let Some(p) = password {
        s.push_str(&format!(" password={p}"));
    }
    s
}

#[tokio::test]
async fn real_client_connect_simple_and_extended() {
    let port = spawn_server(PgConfig::default(), MockSession::new);
    let (client, connection) = tokio_postgres::connect(&conn_string(port, "postgres", None), NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    // 简单查询（text 协议）：RowDescription + DataRow + CommandComplete + RFQ
    let rows = client.simple_query("SELECT 1").await.expect("simple_query");
    let mut texts = Vec::new();
    for msg in &rows {
        if let tokio_postgres::SimpleQueryMessage::Row(row) = msg {
            texts.push(row.get(0).unwrap().to_string());
        }
    }
    assert_eq!(texts, vec!["1"]);

    // 扩展查询（int8 结果，客户端会请求 binary 格式 → DataRow 8B BE）
    let row = client.query_one("SELECT 1", &[]).await.expect("query_one");
    assert_eq!(row.len(), 1);
    assert_eq!(row.columns()[0].name(), "x");
    assert_eq!(row.get::<_, i64>(0), 1);

    // DML：CommandComplete("INSERT 0 1") → affected rows = 1
    let n = client
        .execute("INSERT INTO t VALUES (1)", &[])
        .await
        .expect("execute");
    assert_eq!(n, 1);

    // 具名 prepared statement：Parse(named)+Describe(S)+Sync → Bind+Execute+Sync
    let stmt = client
        .prepare_typed("SELECT $1", &[tokio_postgres::types::Type::INT8])
        .await
        .expect("prepare");
    assert_eq!(stmt.columns().len(), 1);
    assert_eq!(stmt.columns()[0].name(), "x");
    let row = client.query_one(&stmt, &[&7i64]).await.expect("query stmt");
    assert_eq!(row.get::<_, i64>(0), 1);
}

#[tokio::test]
async fn real_client_param_echo_roundtrip() {
    // int8 参数（binary）Bind → 引擎回显为结果 → 客户端 binary 解码
    let port = spawn_server(PgConfig::default(), || {
        let mut m = MockSession::new();
        m.ep_behavior = common::EpBehavior::EchoInt8Param;
        m
    });
    let (client, connection) = tokio_postgres::connect(&conn_string(port, "postgres", None), NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let row = client
        .query_one("SELECT $1", &[&123_456_789i64])
        .await
        .expect("param query");
    assert_eq!(row.get::<_, i64>(0), 123_456_789);
}

#[tokio::test]
async fn real_client_error_sqlstate_and_connection_reuse() {
    // 仅对含 "missing" 的语句报 42P01，其余正常
    let port = spawn_server(PgConfig::default(), || {
        let mut m = MockSession::new();
        m.ep_error = Some(SqlError::undefined_table(
            "relation \"missing\" does not exist",
        ));
        m.ep_error_if_contains = Some("missing".into());
        m
    });
    let (client, connection) = tokio_postgres::connect(&conn_string(port, "postgres", None), NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    // ErrorResponse 的 SQLSTATE 是 42P01（SqlError.state 透传）
    let err = client
        .query("SELECT * FROM missing", &[])
        .await
        .expect_err("should fail");
    let db_err = err.as_db_error().expect("db error");
    assert_eq!(db_err.code().code(), "42P01");
    assert_eq!(db_err.message(), "relation \"missing\" does not exist");
    assert_eq!(db_err.severity(), "ERROR");

    // 连接存活（同一 client 继续可用）
    let row = client
        .query_one("SELECT 1", &[])
        .await
        .expect("still alive");
    assert_eq!(row.get::<_, i64>(0), 1);
}

#[tokio::test]
async fn real_client_cleartext_password() {
    let port = spawn_server(
        PgConfig {
            password: Some("sesame".into()),
        },
        MockSession::new,
    );

    // 错误密码 → FATAL 28P01
    // （connect 返回的 Ok 侧 Connection 非 Debug，不能 expect_err）
    let err = match tokio_postgres::connect(&conn_string(port, "alice", Some("wrong")), NoTls).await
    {
        Ok(_) => panic!("auth must fail"),
        Err(e) => e,
    };
    let db_err = err.as_db_error().expect("db error");
    assert_eq!(db_err.code().code(), "28P01");

    // 正确密码 → 可用
    let (client, connection) =
        tokio_postgres::connect(&conn_string(port, "alice", Some("sesame")), NoTls)
            .await
            .expect("connect with password");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let row = client.query_one("SELECT 1", &[]).await.expect("query");
    assert_eq!(row.get::<_, i64>(0), 1);
}
