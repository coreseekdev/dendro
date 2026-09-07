#![allow(clippy::all)]
//! 备份回归（M-1）：
//! - 一致性点：备份 = 快照时刻 manifest 版本的一致状态；备份打开后数据与源一致
//! - 幂等：重复备份同路径对象跳过（append-only ⇒ 同路径同内容）
//! - 非 dendro 目录备份 → 明确报错

use dendro_core::{Database, DbOptions};
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dendro-bk-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn count(db: &std::sync::Arc<Database>, sql: &str) -> String {
    let mut s = db.new_session();
    let out = s.exec(sql).unwrap();
    match &out[0] {
        dendro_core::Output::Rows(rs) => rs.text_rows()[0][0].clone().unwrap_or_default(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn backup_restore_roundtrip() {
    let src = tmpdir("src");
    let dst = tmpdir("dst");
    // 源库：两批写入（中间一次 checkpoint）
    {
        let db = Database::open(DbOptions { store: dendro_core::StoreConfig::LocalDir(src.clone()), ..DbOptions::default() }).unwrap();
        let mut s = db.new_session();
        s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)").unwrap();
        s.exec("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();
        db.checkpoint_branch("main").unwrap();
        s.exec("INSERT INTO t VALUES (3, 'c')").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    let (objects, bytes) = dendro_server::backup::backup_dir(&src, &dst).unwrap();
    assert!(objects > 0 && bytes > 0);

    // 备份目录作为数据根打开：一致性点状态完整可读
    let restored = Database::open(DbOptions { store: dendro_core::StoreConfig::LocalDir(dst.clone()), ..DbOptions::default() }).unwrap();
    assert_eq!(count(&restored, "SELECT count(*) FROM t"), "3");
    assert_eq!(count(&restored, "SELECT max(id) FROM t"), "3");

    // 幂等：重复备份不在备份目录创建任何新文件（同路径跳过）
    let dst_files = |root: &std::path::Path| -> usize {
        std::fs::read_dir(root.join("objects")).map(|d| d.count()).unwrap_or(0)
            + std::fs::read_dir(root.join("manifest")).map(|d| d.count()).unwrap_or(0)
    };
    let before = dst_files(&dst);
    dendro_server::backup::backup_dir(&src, &dst).unwrap();
    assert_eq!(dst_files(&dst), before, "幂等重备不新增文件");

    // 源库继续写 → 新提交不在旧快照内（一致性点语义）；再次备份后可见
    {
        let db = Database::open(DbOptions { store: dendro_core::StoreConfig::LocalDir(src.clone()), ..DbOptions::default() }).unwrap();
        let mut s = db.new_session();
        s.exec("INSERT INTO t VALUES (4, 'd')").unwrap();
        db.checkpoint_branch("main").unwrap();
    }
    let restored2 = Database::open(DbOptions { store: dendro_core::StoreConfig::LocalDir(dst.clone()), ..DbOptions::default() }).unwrap();
    assert_eq!(count(&restored2, "SELECT count(*) FROM t"), "3", "旧快照不含后续提交");
    dendro_server::backup::backup_dir(&src, &dst).unwrap();
    let restored3 = Database::open(DbOptions { store: dendro_core::StoreConfig::LocalDir(dst.clone()), ..DbOptions::default() }).unwrap();
    assert_eq!(count(&restored3, "SELECT count(*) FROM t"), "4", "再次备份后新提交进入快照");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dst);
}

#[test]
fn backup_rejects_non_dendro_dir() {
    let empty = tmpdir("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = tmpdir("out");
    let err = dendro_server::backup::backup_dir(&empty, &out).unwrap_err();
    assert!(err.to_string().contains("not a dendro data root"), "{err}");
    let _ = std::fs::remove_dir_all(&empty);
    let _ = std::fs::remove_dir_all(&out);
}
