//! 端到端 wire 对拍：真实客户端（tokio-postgres / mysql crate）连 dendro 服务器。

use dendro_core::{Database, DbOptions, StoreConfig};
use mysql::prelude::*;
use std::sync::Arc;

fn open_temp(tag: &str) -> Arc<Database> {
    let dir = std::env::temp_dir().join(format!("dendro-wire-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Database::open(DbOptions {
        store: StoreConfig::LocalDir(dir),
        wal_flush_interval_ms: 5,
        ..Default::default()
    })
    .unwrap();
    db
}

fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

#[test]
fn pg_client_end_to_end() {
    let db = open_temp("pg");
    let addr = free_addr();
    {
        let db2 = db.clone();
        std::thread::spawn(move || dendro_pgwire::serve(addr, db2).unwrap());
    }
    // 等监听就绪
    std::thread::sleep(std::time::Duration::from_millis(100));

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async move {
        let (client, conn) = tokio_postgres::connect(
            &format!("host=127.0.0.1 port={} user=dendro dbname=cambium", addr.port()),
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        let conn = tokio::spawn(async move { conn.await });

        // DDL
        client
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT, score DOUBLE)", &[])
            .await
            .unwrap();
        // DML（简单协议）
        client.execute("INSERT INTO t VALUES (1, 'a', 1.5)", &[]).await.unwrap();
        // 预编译（扩展协议）
        client.execute("INSERT INTO t VALUES ($1, $2, $3)", &[&4i64, &"d", &4.25f64]).await.unwrap();
        // 查询
        let rows = client
            .query("SELECT id, v, score FROM t ORDER BY id", &[])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for c in rows[0].columns() {
            println!("wire col {} type={:?}", c.name(), c.type_());
        }
        let id: i64 = rows[0].get(0);
        let v: &str = rows[0].get(1);
        let score: f64 = rows[0].get(2);
        assert_eq!((id, v, score), (1, "a", 1.5));

        // 分支 SQL
        client.execute("CREATE BRANCH dev FROM main", &[]).await.unwrap();
        client.execute("USE BRANCH dev", &[]).await.unwrap();
        client.execute("INSERT INTO t VALUES (7, 'dev-only', 0.5)", &[]).await.unwrap();
        let n: i64 = client.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get(0);
        assert_eq!(n, 3);
        client.execute("USE BRANCH main", &[]).await.unwrap();
        let n: i64 = client.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get(0);
        assert_eq!(n, 2);
        // 合并
        client.execute("MERGE BRANCH dev INTO main", &[]).await.unwrap();
        let n: i64 = client.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get(0);
        assert_eq!(n, 3);

        // 错误传播（未定义表）
        let err = client.query("SELECT * FROM nope", &[]).await.unwrap_err();
        assert_eq!(err.code().map(|c| c.code()), Some("42P01"));

        drop(client);
        let _ = conn.await;
    });
}

#[test]
fn mysql_client_end_to_end() {
    let db = open_temp("my");
    let addr = free_addr();
    {
        let db2 = db.clone();
        std::thread::spawn(move || {
            dendro_mywire::serve(&addr.to_string(), db2, dendro_mywire::MyConfig::default()).unwrap()
        });
    }
    std::thread::sleep(std::time::Duration::from_millis(100));

    let url = format!("mysql://dendro@127.0.0.1:{}/cambium", addr.port());
    let opts = mysql::Opts::from_url(&url).unwrap();
    let mut conn = mysql::Conn::new(opts).unwrap();

    conn.query_drop("CREATE TABLE m (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
    conn.query_drop("INSERT INTO m VALUES (1, 'x'), (2, 'y')").unwrap();
    let selected: Vec<(i64, String)> = {
        let result = conn.query("SELECT id, v FROM m ORDER BY id").unwrap();
        result
            .into_iter()
            .map(|r: mysql::Row| {
                let (a, b) = mysql::from_row::<(i64, String)>(r);
                (a, b)
            })
            .collect()
    };
    assert_eq!(selected, vec![(1, "x".into()), (2, "y".into())]);
    // 聚合
    let cnt: i64 = conn
        .query_first("SELECT count(*) FROM m")
        .unwrap()
        .unwrap();
    assert_eq!(cnt, 2);
}
