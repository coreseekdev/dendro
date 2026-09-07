#![allow(clippy::all)]
//! KV RESP wire 对拍：原生 TCP 客户端走 RESP 协议访问 dendro KV 层。
use dendro_core::objstore::ObjStore;
use dendro_core::{Database, DbOptions};
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

#[test]
fn kv_txn_reads_own_writes_and_decoded_values() {
    // 第九轮 R9-5：KV 层两洞——①显式事务内 GET 返回未解码整行编码；
    // ②事务内 SCAN 不合并写集（看不到自己的写）。
    let obj: std::sync::Arc<dyn ObjStore> = std::sync::Arc::new(dendro_core::objstore::memory::MemoryObjStore::new());
    let db = Database::open(DbOptions { store: dendro_core::StoreConfig::Obj(obj), ..DbOptions::default() }).unwrap();
    let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
    kv.put("k1", b"v1").unwrap();

    kv.begin().unwrap();
    // ① 事务内 GET：值必须是解码后的 v1，而非整行编码
    assert_eq!(kv.get("k1").unwrap().as_deref(), Some(&b"v1"[..]), "事务内 GET 必须返回解码值");
    kv.put("k2", b"v2").unwrap();
    // ② 事务内 SCAN：必须看到自己的写
    let rows = kv.scan(None, None).unwrap();
    let got: Vec<(Vec<u8>, Vec<u8>)> = rows;
    assert!(got.contains(&(b"k1".to_vec(), b"v1".to_vec())));
    assert!(got.contains(&(b"k2".to_vec(), b"v2".to_vec())), "事务内 SCAN 必须读自己的写");
    // 事务内删除 → SCAN/GET 均消失
    kv.delete("k1").unwrap();
    assert_eq!(kv.get("k1").unwrap(), None);
    assert!(!kv.scan(None, None).unwrap().iter().any(|(k, _)| k == b"k1"));
    kv.commit().unwrap();
    // COMMIT 后持久视图一致
    assert_eq!(kv.get("k1").unwrap(), None);
    assert_eq!(kv.get("k2").unwrap().as_deref(), Some(&b"v2"[..]));
}
