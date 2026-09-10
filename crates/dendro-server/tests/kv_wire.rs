//! KV RESP wire 对拍：原生 TCP 客户端走 RESP 协议访问 dendro KV 层。
use dendro_core::objstore::ObjStore;
use dendro_core::{Database, DbOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
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
    read_replies(w, 1) // 每条命令恰好一个回复
}

fn read_replies(r: &mut TcpStream, n: usize) -> Vec<String> {
    let mut rd = BufReader::new(r.try_clone().unwrap());
    let mut out = Vec::new();
    for _ in 0..n {
        // 读一行状态/整数/批量头
        let mut line = String::new();
        rd.read_line(&mut line).unwrap();
        if let Some(rest) = line.strip_prefix('$') {
            let len: i64 = rest.trim().parse().unwrap_or(0);
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
    let obj: std::sync::Arc<dyn ObjStore> =
        std::sync::Arc::new(dendro_core::objstore::memory::MemoryObjStore::new());
    let db = Database::open(DbOptions {
        store: dendro_core::StoreConfig::Obj(obj),
        ..DbOptions::default()
    })
    .unwrap();
    let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
    kv.put("k1", b"v1").unwrap();

    kv.begin().unwrap();
    // ① 事务内 GET：值必须是解码后的 v1，而非整行编码
    assert_eq!(
        kv.get("k1").unwrap().as_deref(),
        Some(&b"v1"[..]),
        "事务内 GET 必须返回解码值"
    );
    kv.put("k2", b"v2").unwrap();
    // ② 事务内 SCAN：必须看到自己的写
    let rows = kv.scan(None, None).unwrap();
    let got: Vec<(Vec<u8>, Vec<u8>)> = rows;
    assert!(got.contains(&(b"k1".to_vec(), b"v1".to_vec())));
    assert!(
        got.contains(&(b"k2".to_vec(), b"v2".to_vec())),
        "事务内 SCAN 必须读自己的写"
    );
    // 事务内删除 → SCAN/GET 均消失
    kv.delete("k1").unwrap();
    assert_eq!(kv.get("k1").unwrap(), None);
    assert!(!kv.scan(None, None).unwrap().iter().any(|(k, _)| k == b"k1"));
    kv.commit().unwrap();
    // COMMIT 后持久视图一致
    assert_eq!(kv.get("k1").unwrap(), None);
    assert_eq!(kv.get("k2").unwrap().as_deref(), Some(&b"v2"[..]));
}

#[test]
fn kv_txn_frozen_reads_and_registry_lifecycle() {
    // 第十轮 R10-4：KV 显式事务接入 R7-3/R9-1 机制——
    // ① 冻结根：事务内他人提交+物化后仍不可见（此前隔离翻转）；
    // ② 注册表生命周期：COMMIT/放弃会话都注销（此前永久跳过截断）。
    let obj: std::sync::Arc<dyn ObjStore> =
        std::sync::Arc::new(dendro_core::objstore::memory::MemoryObjStore::new());
    let db = Database::open(DbOptions {
        store: dendro_core::StoreConfig::Obj(obj),
        ..DbOptions::default()
    })
    .unwrap();
    {
        let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
        kv.put("k1", b"a").unwrap();
    }
    let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
    kv.begin().unwrap();
    assert_eq!(kv.get("k1").unwrap().as_deref(), Some(&b"a"[..]));
    // 并发提交新键 + checkpoint（物化 + covered_min 推进尝试）
    {
        let mut kv2 = dendro_core::kv::Kv::open(&db, "main").unwrap();
        kv2.begin().unwrap();
        kv2.put("k2", b"b").unwrap();
        kv2.commit().unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    // 冻结读：事务内仍只有 k1
    assert_eq!(kv.get("k2").unwrap(), None, "冻结读被 checkpoint 击穿");
    let rows = kv.scan(None, None).unwrap();
    assert!(rows.iter().all(|(k, _)| k != b"k2"), "SCAN 冻结读被击穿");
    // COMMIT：注销注册表 → 该分支截断恢复
    kv.commit().unwrap();
    let b = db.branch("main").unwrap();
    assert!(
        b.active_snaps.lock().is_empty(),
        "COMMIT 后活跃快照必须注销（否则截断永久跳过）"
    );
    // 新会话可见
    let kv3 = dendro_core::kv::Kv::open(&db, "main").unwrap();
    assert_eq!(kv3.get("k2").unwrap().as_deref(), Some(&b"b"[..]));
}

#[test]
fn kv_use_branch_inside_txn_rejected() {
    // 第十一轮收口②（R9-6）：KV 层事务内切分支此前静默丢弃事务且不注销
    let obj: std::sync::Arc<dyn ObjStore> =
        std::sync::Arc::new(dendro_core::objstore::memory::MemoryObjStore::new());
    let db = Database::open(DbOptions {
        store: dendro_core::StoreConfig::Obj(obj),
        ..DbOptions::default()
    })
    .unwrap();
    {
        let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
        kv.put("k", b"v").unwrap();
    }
    let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
    kv.begin().unwrap();
    let e = match kv.use_branch("b2") {
        Ok(_) => panic!("事务内 use_branch 应被拒"),
        Err(e) => e,
    };
    assert_eq!(e.state, "25001", "{e}");
    kv.rollback();
}

#[test]
fn kv_use_branch_success_path_isolation() {
    // 第十二轮 P12-6 转正：use_branch 成功路径此前零覆盖（探针转正）
    let obj: std::sync::Arc<dyn ObjStore> =
        std::sync::Arc::new(dendro_core::objstore::memory::MemoryObjStore::new());
    let db = Database::open(DbOptions {
        store: dendro_core::StoreConfig::Obj(obj),
        ..DbOptions::default()
    })
    .unwrap();
    db.create_branch("b2", "main").unwrap(); // use_branch 不自动建分支（自动建语义在 kv_resp 的 BRANCH 命令）
    {
        let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
        kv.put("k", b"main-v").unwrap(); // 自动提交（无显式事务）
    }
    {
        let mut kv = dendro_core::kv::Kv::open(&db, "b2").unwrap();
        kv.put("k", b"b2-v").unwrap(); // b2 上的自动提交
    }
    let mut kv = dendro_core::kv::Kv::open(&db, "main").unwrap();
    assert_eq!(
        kv.get("k").unwrap().as_deref(),
        Some(&b"main-v"[..]),
        "分支隔离：main 不受 b2 写影响"
    );
    kv.use_branch("b2").unwrap();
    assert_eq!(
        kv.get("k").unwrap().as_deref(),
        Some(&b"b2-v"[..]),
        "b2 可见自己的写"
    );
    kv.use_branch("main").unwrap();
    assert_eq!(
        kv.get("k").unwrap().as_deref(),
        Some(&b"main-v"[..]),
        "切回 main 原值仍在"
    );
}
