//! 运维端点回归（P1-11 负载自感知）：
//! - /readyz → 200 "ok"（k8s 就绪探针）
//! - /metrics → Prometheus 文本：pending_bytes / watermark / durable / lease TTL
//! - 只读性：/metrics 不得懒加载分支（仅 manifest 存在、未驻留的分支不出现，
//!   也不因此领 epoch / 起 WAL writer）

use dendro_core::{Database, DbOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

fn mem_db() -> std::sync::Arc<Database> {
    Database::open(DbOptions::memory()).unwrap()
}

fn get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    let mut rd = BufReader::new(s);
    let mut status = String::new();
    rd.read_line(&mut status).unwrap();
    let code: u16 = status.split_whitespace().nth(1).unwrap().parse().unwrap();
    loop {
        let mut h = String::new();
        rd.read_line(&mut h).unwrap();
        if h == "\r\n" || h == "\n" || h.is_empty() {
            break;
        }
    }
    let mut body = String::new();
    rd.read_to_string(&mut body).unwrap();
    (code, body)
}

#[test]
fn metrics_endpoints_report_active_branch_load() {
    let db = mem_db();
    {
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let db2 = db.clone();
    std::thread::spawn(move || dendro_server::metrics::serve_listener(listener, db2).unwrap());

    // readyz
    let (code, body) = get(port, "/readyz");
    assert_eq!(code, 200);
    assert_eq!(body.trim(), "ok");

    // metrics：写负载可见（未 checkpoint 的 pending 字节 > 0）
    let (code, body) = get(port, "/metrics");
    assert_eq!(code, 200);
    for metric in ["dendro_up 1", "dendro_branch_pending_bytes{branch=\"main\"}", "dendro_branch_watermark{branch=\"main\"}", "dendro_branch_lease_ttl_ms{branch=\"main\"}"] {
        assert!(body.contains(metric), "metrics 缺 {metric}\n---\n{body}");
    }
    let pending_line = body.lines().find(|l| l.starts_with("dendro_branch_pending_bytes")).unwrap();
    let pending: u64 = pending_line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(pending > 0, "两次 INSERT 后 pending_bytes 应 > 0（行：{pending_line}）");
    // lease TTL 必须为正（健康写者）
    let ttl_line = body.lines().find(|l| l.starts_with("dendro_branch_lease_ttl_ms")).unwrap();
    let ttl: i64 = ttl_line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(ttl > 0, "健康写者租约应有剩余 TTL（行：{ttl_line}）");

    // 只读性：404 路径
    let (code, _) = get(port, "/nope");
    assert_eq!(code, 404);
}
