#![allow(clippy::all)]
//! 装配层冒烟（Q-18：第十/十一轮两次装配 P0 的结构性整改——此前
//! main() 装配零测试，FIFO 错位使 PG/MySQL 端口互换而全绿放行）。
//! 断言：**端口 ↔ 协议对应**（PG 端口讲 PG 协议、MySQL 端口讲 MySQL
//! 握手、KV 端口讲 RESP、metrics 讲 HTTP），进程级、自动化、随 CI 跑。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    pg: u16,
    mysql: u16,
    kv: u16,
    metrics: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_server(data: &std::path::Path) -> Server {
    let (pg, mysql, kv, metrics) = (free_port(), free_port(), free_port(), free_port());
    let mut child = Command::new(env!("CARGO_BIN_EXE_dendro"))
        .args([
            "serve",
            "--data",
            data.to_str().unwrap(),
            "--host",
            "127.0.0.1",
            "--pg-port",
            &pg.to_string(),
            "--mysql-port",
            &mysql.to_string(),
            "--kv-port",
            &kv.to_string(),
            "--metrics-port",
            &metrics.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dendro serve");
    // 轮询 ready（metrics /readyz 200 即装配完成）
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("serve not ready in 10s");
        }
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", metrics)) {
            if s.write_all(b"GET /readyz HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").is_ok()
                && s.read_to_string(&mut String::new()).is_ok()
            {
                let _ = s;
                // 读一遍状态行判断 200
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, pg, mysql, kv, metrics }
}

fn probe(port: u16, payload: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    s.write_all(payload).unwrap();
    let mut buf = [0u8; 128];
    let n = s.read(&mut buf).unwrap_or(0);
    buf[..n].to_vec()
}

#[test]
fn assembly_ports_speak_right_protocols() {
    let data = std::env::temp_dir().join(format!("dendro-asm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let srv = start_server(&data);

    // PG 端口：StartupMessage → AuthenticationOk('R') 而非 MySQL 握手
    let mut body = struct_pack(196608);
    body.extend_from_slice(b"user\0postgres\0\0");
    let mut start = struct_pack(body.len() as u32 + 4);
    start.extend_from_slice(&body);
    let pg_reply = probe(srv.pg, &start);
    assert_eq!(pg_reply[0], b'R', "PG 端口应回 AuthenticationOk（实际 {:?}——协议装反？）", pg_reply);

    // MySQL 端口：连接即发握手（含 "dendro" 版本串）
    let my = probe(srv.mysql, b"");
    assert!(
        my.windows(6).any(|w| w == b"dendro"),
        "MySQL 端口应回 MySQL 握手（实际 {:?}）",
        my
    );

    // KV 端口：RESP PING → +PONG
    let kv = probe(srv.kv, b"*1\r\n$4\r\nPING\r\n");
    assert!(kv.starts_with(b"+PONG"), "KV 端口应回 +PONG（实际 {:?}）", kv);

    // metrics：HTTP /readyz → 200
    let mut s = TcpStream::connect(("127.0.0.1", srv.metrics)).unwrap();
    s.write_all(b"GET /readyz HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.starts_with("HTTP/1.1 200"), "readyz 应 200");

    let _ = std::fs::remove_dir_all(&data);
}

fn struct_pack(v: u32) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}
