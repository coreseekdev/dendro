//! KV RESP wire 对拍：原生 TCP 客户端走 RESP 协议访问 dendro KV 层。
use dendro_core::{Database, DbOptions, StoreConfig};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

fn conn(port: u16) -> TcpStream {
    TcpStream::connect(("127.0.0.1", port)).unwrap()
}

fn cmd(w: &mut TcpStream, args: &[&str]) -> Vec<String> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend(format!("${}\r\n{}\r\n", a.len(), a).into_bytes());
    }
    w.write_all(&buf).unwrap();
    w.flush().unwrap();
    read_replies(w, 1)   // 每条命令恰好一个回复
}

fn read_replies(r: &mut TcpStream, n: usize) -> Vec<String> {
    let mut rd = BufReader::new(r.try_clone().unwrap());
    let mut out = Vec::new();
    for _ in 0..n {
        // 读一行状态/整数/批量头
        let mut line = String::new();
        rd.read_line(&mut line).unwrap();
        if line.starts_with('$') {
            let len: i64 = line[1..].trim().parse().unwrap_or(0);
            if len < 0 {
                out.push("NIL".into());
                continue;
            }
            let mut buf = vec![0u8; len as usize + 2];
            rd.read_exact(&mut buf).unwrap();
            buf.truncate(len as usize);
            out.push(String::from_utf8_lossy(&buf).to_string());
        } else {
            out.push(line.trim_end().to_string());
        }
    }
    out
}

#[test]
fn kv_resp_wire_end_to_end() {
    let db = Database::open(DbOptions::memory()).unwrap();
    let addr = free_addr();
    {
        let db2 = db.clone();
        std::thread::spawn(move || {
            dendro_server::kv_resp::serve(&addr.to_string(), db2, "main").unwrap()
        });
    }
    std::thread::sleep(std::time::Duration::from_millis(80));

    let mut s = conn(addr.port());
    // PING
    assert_eq!(cmd(&mut s, &["PING"])[0], "+PONG");
    // SET/GET/DEL/EXISTS
    assert_eq!(cmd(&mut s, &["SET", "k", "v"])[0], "+OK");
    assert_eq!(cmd(&mut s, &["GET", "k"])[0], "v");
    assert_eq!(cmd(&mut s, &["EXISTS", "k"])[0], ":1");
    assert_eq!(cmd(&mut s, &["DEL", "k"])[0], ":1");
    assert_eq!(cmd(&mut s, &["GET", "k"])[0], "NIL");
    // MULTI/EXEC 原子写
    assert_eq!(cmd(&mut s, &["MULTI"])[0], "+OK");
    cmd(&mut s, &["SET", "a", "1"]);
    cmd(&mut s, &["SET", "b", "2"]);
    assert_eq!(cmd(&mut s, &["EXEC"])[0], "+OK");
    assert_eq!(cmd(&mut s, &["GET", "a"])[0], "1");
    assert_eq!(cmd(&mut s, &["GET", "b"])[0], "2");
    // 分支隔离（自定义命令）
    assert_eq!(cmd(&mut s, &["BRANCH", "sandbox"])[0], "+OK");
    assert_eq!(cmd(&mut s, &["SET", "feat", "on-sandbox"])[0], "+OK");
    assert_eq!(cmd(&mut s, &["GET", "feat"])[0], "on-sandbox");
    assert_eq!(cmd(&mut s, &["BRANCH", "main"])[0], "+OK");
    assert_eq!(cmd(&mut s, &["GET", "feat"])[0], "NIL");
    // DBSIZE 只统计当前键空间
    let sz = cmd(&mut s, &["DBSIZE"]);
    assert!(!sz.is_empty());
}
