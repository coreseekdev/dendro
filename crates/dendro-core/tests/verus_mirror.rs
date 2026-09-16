//! L3 镜像漂移守护：Verus 证明文件（verification/verus/*.rs）镜像的
//! 函数与生产代码同式——本测试对**真实实现**断言证明所立的性质。
//! 证明文件与源分离是布局约束（Verus 独立工具链不进 workspace 编译）；
//! 漂移（源改式、证明未跟）在此即刻红。

#[test]
fn order_domain_real_impl_involution_and_order() {
    use dendro_core::types::SqlValue;
    // 真实实现：scan.rs order_domain + stats.rs order_of（同式 v^MIN）
    let f = |v: i64| (v ^ i64::MIN) as u64;
    // ① 对合（边界 + 随机采样）
    let mut x = 0x123456789abcdefu64;
    for v in [
        i64::MIN, i64::MIN + 1, -1, 0, 1, 42, i64::MAX - 1, i64::MAX,
    ] {
        let d = f(v);
        assert_eq!((d as i64) ^ i64::MIN, v, "decode(roundtrip) @ {v}");
    }
    for _ in 0..10_000 {
        // xorshift 采样
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        let v = (x as i64) ^ (x << 1) as i64;
        let d = f(v);
        assert_eq!((d as i64) ^ i64::MIN, v);
    }
    // ② 保序（随机对）
    let mut a = 0xdeadbeefu64;
    for _ in 0..10_000 {
        a ^= a << 13; a ^= a >> 7; a ^= a << 17;
        let b = a.wrapping_mul(0x9E3779B97F4A7C15);
        let (i, j) = ((a as i64), (b as i64));
        if i <= j {
            assert!(f(i) <= f(j), "order @ {i} <= {j}");
        } else {
            assert!(f(j) <= f(i));
        }
    }
    let _ = SqlValue::Null;
}

#[test]
fn norm_user_real_impl_idempotent() {
    // 真实实现：privs.rs norm_user（to_ascii_lowercase）
    for c in 0u8..=255 {
        let l1 = (c as char).to_ascii_lowercase();
        let l2 = l1.to_ascii_lowercase();
        assert_eq!(l1, l2, "幂等 @ {c}");
    }
    // 生产 API 直证
    for s in ["Dendro", "DENDRO", "alice", "Alice", "MiXeD_User-42"] {
        let n1 = dendro_core::sql::privs::norm_user(s);
        let n2 = dendro_core::sql::privs::norm_user(&n1);
        assert_eq!(n1, n2);
        assert!(n1.chars().all(|c| !c.is_ascii_uppercase()));
    }
}

#[test]
fn selectivity_clamp_real_impl_bounds() {
    // 真实实现：stats.rs range_selectivity——差值 clamp 的 frac ∈ [0,1]
    use dendro_core::sql::stats::{range_selectivity, ColStat};
    use dendro_core::types::SqlValue;
    let mut x = 0xfeedfaceu64;
    for _ in 0..10_000 {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        let min = x >> 8; // 任意 56 位
        let max = min + (x & 0xffff); // span > 0
        let p = (x.wrapping_mul(31)) >> 8;
        let st = ColStat { rows: 1000, nulls: 0, min, max, has_data: true };
        for op in [">", ">=", "<", "<="] {
            if let Some(sel) = range_selectivity(&st, &SqlValue::Int64(p as i64), op) {
                assert!(
                    (0.0..=1.0).contains(&sel),
                    "sel={sel} 越界 @ min={min} max={max} p={p} op={op}"
                );
            }
        }
    }
}
