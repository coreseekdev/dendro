fn main() {
    let dir = std::env::temp_dir().join(format!("memwal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut c = dendro_core::embed::Connection::open_with(
        &dir,
        dendro_core::DbOptions::embedded(dendro_core::StoreConfig::Memory),
    ).unwrap();
    match c.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, payload TEXT, tag TEXT)") {
        Ok(_) => println!("create ok"),
        Err(e) => { println!("create err: {e:?}"); return; }
    }
    for round in 0..40 {
        let vals: Vec<String> = (0..2000)
            .map(|i| format!("({}, '{}', 't')", round * 2000 + i + 1, "p".repeat(190)))
            .collect();
        match c.execute(&format!("INSERT INTO t VALUES {}", vals.join(","))) {
            Ok(_) => {}
            Err(e) => { println!("round {round} err: {e:?}"); break; }
        }
    }
    println!("batches done");
    match c.execute("CHECKPOINT") {
        Ok(_) => println!("checkpoint ok"),
        Err(e) => println!("checkpoint err: {e:?}"),
    }
    match c.execute("INSERT INTO t VALUES (999999, 'z', 'z')") {
        Ok(_) => println!("post-ckpt insert ok"),
        Err(e) => println!("post-ckpt err: {e:?}"),
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    match c.execute("INSERT INTO t VALUES (2, 'y', 'y')") {
        Ok(n) => println!("insert2 ok {n}"),
        Err(e) => println!("insert2 err: {e:?}"),
    }
}
