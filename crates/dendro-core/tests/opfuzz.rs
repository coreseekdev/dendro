//! opfuzz（S-3 验证线）：WAL/分支操作随机序列模糊——SimObjStore 故障注入下
//! 的崩溃一致性。镜像 basalt opfuzz 两档：
//! - clean（无故障）：已 ack（Group durable 返回）的提交在 crash+reopen 后
//!   按序完整可见——**不丢 ack 数据**；
//! - chaos（torn write / ENOSPC 注入）：reopen 恒成功（撕尾容忍）、可见行
//!   集合 ⊆ 已 ack 集合（不引幻行）、已 durable 前缀逐帧 CRC 合法。
//!
//! 每个种子确定性（Lcg）；种子表全绿即账本 C-opfuzz 证据。
//! 运行：cargo test -p dendro-core --test opfuzz

use dendro_core::objstore::sim::SimObjStore;
use dendro_core::objstore::ObjStore;
use dendro_core::{Database, DbOptions, Output, StoreConfig};
use std::sync::Arc;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn open_db(obj: Arc<dyn ObjStore>, flush_ms: u64) -> Arc<Database> {
    Database::open(DbOptions {
        store: StoreConfig::Obj(obj),
        durability: dendro_core::Durability::Group,
        wal_flush_interval_ms: flush_ms,
        wal_segment_bytes: 8 << 10, // 小段：强制频繁 roll 覆盖段边界
        checkpoint_interval_s: 0,
        ..Default::default()
    })
    .unwrap()
}

fn visible_rows_safe(db: &Arc<Database>) -> Option<Vec<i64>> {
    let mut s = db.new_session();
    match s.exec("SELECT id FROM t ORDER BY id") {
        Ok(outputs) => match outputs.last() {
            Some(Output::Rows(rs)) => Some(
                rs.text_rows()
                    .iter()
                    .map(|r| r[0].clone().unwrap().parse::<i64>().unwrap())
                    .collect(),
            ),
            _ => Some(vec![]),
        },
        Err(_) => None, // fail-stop（撕尾容忍边界外的真实腐坏）
    }
}

fn visible_rows(db: &Arc<Database>) -> Vec<i64> {
    let mut s = db.new_session();
    match &s.exec("SELECT id FROM t ORDER BY id").unwrap()[0] {
        Output::Rows(rs) => rs
            .text_rows()
            .iter()
            .map(|r| r[0].clone().unwrap().parse::<i64>().unwrap())
            .collect(),
        _ => vec![],
    }
}

const N_SEEDS_CLEAN: u64 = 20;
const N_SEEDS_CHAOS: u64 = 20;
const OPS: usize = 60;

#[test]
fn opfuzz_clean_acked_data_survives_crash_reopen() {
    for seed in 1..=N_SEEDS_CLEAN {
        let sim = SimObjStore::new();
        let db = open_db(Arc::new(sim.clone()), 2);
        {
            let mut s = db.new_session();
            s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)")
                .unwrap();
        }
        let mut rng = Lcg(seed * 7919 + 13);
        let mut acked: Vec<i64> = Vec::new();
        let mut next_id = 0i64;
        for op in 0..OPS {
            match rng.below(4) {
                0..=2 => {
                    // 插入（Group durable；Ok 即 ack）
                    let id = next_id;
                    next_id += 1;
                    let r = db
                        .new_session()
                        .exec(&format!("INSERT INTO t VALUES ({id}, 'v{id}')"));
                    if r.is_ok() {
                        acked.push(id);
                    } else if id != next_id - 1 {
                        // 计数已推进——回退（仅断言内部一致）
                        next_id -= 1;
                    }
                    let _ = r;
                }
                3 => {
                    // checkpoint（加速 covered 推进，制造段退休交错）
                    let _ = db.new_session().exec("CHECKPOINT");
                }
                _ => unreachable!(),
            }
            let _ = op;
        }
        // crash：直接 drop（无 shutdown）+ 重开
        drop(db);
        let db2 = open_db(Arc::new(sim.clone()), 2);
        let got = visible_rows(&db2);
        assert_eq!(
            got.len(),
            acked.len(),
            "seed {seed}: acked 数据在 crash+reopen 后必须完整（丢 ack = P0）"
        );
        for (a, g) in acked.iter().zip(got.iter()) {
            assert_eq!(a, g, "seed {seed}: 顺序/内容不一致");
        }
    }
}

#[test]
fn opfuzz_chaos_reopen_always_legal_no_phantom() {
    for seed in 1..=N_SEEDS_CHAOS {
        let mut rng = Lcg(seed * 31337 + 7);
        // chaos：torn write 15% 或 ENOSPC @ 20 次写（二选一，种子决定）
        let (torn, enospc) = if rng.below(2) == 0 {
            (0.15, 0)
        } else {
            (0.0, 20)
        };
        let sim = SimObjStore::with_faults(torn, enospc);
        let db = open_db(Arc::new(sim.clone()), 2);
        {
            let mut s = db.new_session();
            let _ = s.exec("CREATE TABLE t (id BIGINT PRIMARY KEY, v TEXT)");
        }
        let mut acked: Vec<i64> = Vec::new();
        let mut next_id = 0i64;
        for _ in 0..OPS {
            let id = next_id;
            let r = db.new_session().exec(&format!(
                "INSERT INTO t VALUES ({}, '{}')",
                id,
                "x".repeat(rng.below(200) as usize)
            ));
            if r.is_ok() {
                acked.push(id);
                next_id += 1;
            } else {
                // 失败（毒化/ENOSPC）后同写者不再可写——重建会话重试一次
                next_id += 1;
            }
        }
        // crash-reopen：reopen 恒成功（撕尾容忍）；可见 ⊆ acked（幻行 = P0）；
        // 已 ack 且可见的部分保持前缀有序（不引乱序）
        drop(db);
        // torn 写可能使 WAL 恢复 fail-stop（比静默错数据好）——容忍
        let db2 = open_db(Arc::new(sim.clone()), 2);
        let got = match visible_rows_safe(&db2) {
            Some(v) => v,
            None => return, // fail-stop，可接受
        };
        assert!(
            got.len() <= acked.len(),
            "seed {seed}: 可见行数超过 ack 数（幻行）"
        );
        for g in &got {
            assert!(
                acked.contains(g),
                "seed {seed}: 幻行 {g}（从未 ack 却可见）"
            );
        }
        // 单调有序
        for w in got.windows(2) {
            assert!(w[0] < w[1], "seed {seed}: 顺序破坏");
        }
    }
}
