//! Kani L1 harness——**真源编译版**（绑定级 B2，取代旧镜像副本 B1）。
//!
//! ## 绑定方式
//!
//! 经 `#[path]` 直接编译生产文件，非镜像：
//!   - `crates/dendro-core/src/wal/codec.rs` —— 验证对象本体（WAL 帧
//!     编解码，不可信输入边界）；
//!   - `crates/dendro-core/src/format/hash.rs` —— CheckpointRecord 的
//!     纯数据依赖（真源包含；Sha512 不入证明）。
//!
//! 错误类型以 `mod error` 最小 shim 提供：codec 仅调用
//! `SqlError::internal` 构造错误值、不依赖其内部结构；真 error.rs 若
//! 移除 `internal()` 会直接破坏 dendro-core 编译，shim 缝隙不可静默漂移。
//!
//! ## 对比旧镜像（账本 #19 时代的 verification/kani/wal_frame.rs）的实证缺陷
//!
//! 1. FRAME_MAGIC 曾漂移（0x4C41_5345 vs 0x4F52_4E44），常量同步测试
//!    只是事后补丁——本版不存在副本，源文件即证明对象；
//! 2. CRC 用手写副本而非真 crc32c crate（SIMD/asm 不可验）——本版以
//!    crc32c-soft 规范模型 + 差分测试闭合该缝隙；
//! 3. 真 `FrameType` 有 Txn/Checkpoint/Fence/Seal 四变体，镜像只有两
//!    ——Fence/Seal 帧从未被探索。本版 H2 对 `from_u16` 全定义域证明。
//!
//! 运行：`cargo kani --manifest-path verification/kani/Cargo.toml`
//! （单 harness 加 `--harness <名>`）。

pub mod error {
    //! SqlError 最小面 shim（绑定链缝隙之一，见 lib.rs 文档）。
    #[derive(Debug, Clone)]
    pub struct SqlError {
        pub state: &'static str,
        pub message: String,
    }
    impl SqlError {
        pub fn internal(msg: impl Into<String>) -> Self {
            Self { state: "XX000", message: msg.into() }
        }
    }
    pub type Result<T> = std::result::Result<T, SqlError>;
}

#[path = "../../../crates/dendro-core/src/format/hash.rs"]
pub mod format_hash;

/// 拼出 `crate::format::hash::Hash` 路径（codec.rs 的真源引用）。
pub mod format {
    pub use crate::format_hash as hash;
}

#[path = "../../../crates/dendro-core/src/wal/codec.rs"]
pub mod wal_codec;

#[cfg(kani)]
mod proofs {
    use crate::wal_codec::*;

    // ------------------------------------------------------------------
    // H 系列：性质证明（继承旧镜像四项 + 扩展）
    // ------------------------------------------------------------------

    /// H1：任意输入序列上 FrameIter 恒不 panic（Err/None 而非 UB/越界）。
    /// 界 40 = 帧头 24 + 16B 载荷；固定 3 次调用（无界迭代会展开爆炸，
    /// 旧版实证 >9min 不收敛）。unwind 17 覆盖软 CRC（内循环恰 8、
    /// 外循环 ≤16B 载荷）。
    /// cover 只放便宜路径（P1 终止：len=0 即达；P3 拒绝：坏魔数即达，
    /// 不触 CRC）；P2 成功路径需在任意缓冲上反解 CRC（GF(2) 级联），
    /// 放 H2 的整形输入侧（实证 19min 不收敛）。
    #[kani::proof]
    #[kani::unwind(17)]
    fn frame_iter_arbitrary_input_never_panics() {
        let buf: [u8; 40] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= 40);
        let mut it = FrameIter::new(&buf[..len]);
        for _ in 0..3 {
            match it.next_frame() {
                None => kani::cover!(true, "P1 帧流终止(耗尽/ESAL/半帧头)"),
                Some(Ok(_)) => {}
                Some(Err(_)) => kani::cover!(true, "P3 坏帧显式拒绝"),
            }
        }
    }

    /// H2：合法编码帧必被无错解码（编码/解码对偶）。
    /// 帧型符号化：`from_u16` 全定义域（1..=4：Txn/Checkpoint/Fence/Seal
    /// ——旧镜像只验过 Txn，Fence/Seal 帧零覆盖）。
    /// payload 用栈数组：符号字节进堆分配曾在本工具链产生虚假反例。
    #[kani::proof]
    #[kani::unwind(9)]
    fn frame_roundtrip_exact_all_frame_types() {
        let tv: u16 = kani::any();
        kani::assume(tv >= 1 && tv <= 4); // from_u16 定义域（0/越界由 P3d 证）
        let ty = FrameType::from_u16(tv).unwrap();
        kani::cover!(true, "P2 帧成功产出(编码→解码对偶)");
        kani::cover!(true, "P6 帧型全定义域 roundtrip");
        let seq: u64 = kani::any();
        let payload: [u8; 2] = [kani::any(), kani::any()];
        let frame = encode_frame(ty, seq, &payload);
        let mut it = FrameIter::new(&frame);
        match it.next_frame() {
            Some(Ok((t, s, p))) => {
                assert_eq!(t, ty);
                assert_eq!(s, seq);
                assert_eq!(p, payload.as_slice());
            }
            _ => panic!("合法帧必须无错解码"),
        }
    }

    /// H3：截断帧头（< 24B）干净返回 None（撕尾容忍）
    #[kani::proof]
    fn truncated_header_ends_cleanly() {
        let frame = encode_frame(FrameType::Txn, 1, b"abc");
        let cut: usize = kani::any();
        kani::assume(cut < HEADER_LEN);
        let mut it = FrameIter::new(&frame[..cut]);
        kani::cover!(true, "P4 半帧头视为段尾(撕尾容忍)");
        assert!(it.next_frame().is_none(), "半帧头视为段尾");
    }

    /// H4（账本 #19 回归）：总长 ≤ TRAILER_LEN 的小帧在段尾必须被解码
    /// （旧 guard `remaining <= TRAILER_LEN 即停` 会把它当 trailer 丢弃
    /// ——封段回放静默丢已确认提交）。
    #[kani::proof]
    #[kani::unwind(9)]
    fn tiny_frame_at_segment_end_decodes() {
        let seq: u64 = kani::any();
        let payload: [u8; 2] = [kani::any(), kani::any()]; // 26B 帧 < 32B trailer
        let frame = encode_frame(FrameType::Txn, seq, &payload);
        assert!(frame.len() <= TRAILER_LEN, "前置：确为小帧");
        kani::cover!(true, "P5 段尾小帧不丢(#19 回归)");
        let mut it = FrameIter::new(&frame);
        match it.next_frame() {
            Some(Ok((ty, s, p))) => {
                assert_eq!(ty, FrameType::Txn);
                assert_eq!(s, seq);
                assert_eq!(p, payload.as_slice());
            }
            _ => panic!("小帧必须解码，不得当 trailer 丢弃"),
        }
    }

    /// H5：TXN 记录编解码对偶（encode_txn → decode_txn 恒等往返）。
    /// 符号化：table_id / 键值内容 / 删除(None) 与更新(Some) 两形态。
    /// **已知墙（不入门禁）**：TxnRecord 嵌套 Vec（ops→(Vec,Option<Vec>)）
    /// 的 drop/dealloc 路径使 symex 爆炸（3800+ aborting paths，480s
    /// 不收敛，去 clone 无效）；待 Kani 堆模型改进或 drop stub 后启用。
    /// 往返性质暂由 tests/wal_corruption.rs 的具体向量回归兜底。
    #[kani::proof]
    #[kani::unwind(12)]
    fn txn_record_roundtrip() {
        let table_id: u32 = kani::any();
        let key: [u8; 2] = [kani::any(), kani::any()];
        let is_del: bool = kani::any();
        let val: [u8; 2] = [kani::any(), kani::any()];
        let rec = TxnRecord {
            table_id,
            ops: vec![(
                key.to_vec(),
                if is_del { None } else { Some(val.to_vec()) },
            )],
        };
        let want_key = rec.ops[0].0.clone();
        let want_val = rec.ops[0].1.clone();
        let payload = encode_txn(std::slice::from_ref(&rec));
        let back = decode_txn(&payload).unwrap();
        kani::cover!(is_del, "P7a 删除记录往返");
        kani::cover!(!is_del, "P7b 更新记录往返");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].table_id, table_id);
        assert_eq!(back[0].ops.len(), 1);
        assert_eq!(back[0].ops[0].0, want_key);
        match (&back[0].ops[0].1, &want_val) {
            (None, None) => {}
            (Some(v), Some(w)) => assert_eq!(v, w),
            _ => panic!("删除/更新形态漂移"),
        }
    }

    // ------------------------------------------------------------------
    // P 系列：坏帧拒绝路径（输入整形保证到达该拒绝原因，cover 记录路径）
    // ------------------------------------------------------------------

    /// P3a：载荷任意单字节翻转 → CRC 拒绝（腐坏帧不得静默通过）
    #[kani::proof]
    #[kani::unwind(11)]
    fn crc_corruption_rejected() {
        let payload = [0x5Au8, 0xA5];
        let mut frame = encode_frame(FrameType::Txn, 7, &payload);
        let flip_at: usize = kani::any();
        let bit: u8 = kani::any();
        kani::assume(flip_at >= HEADER_LEN && flip_at < frame.len());
        kani::assume(bit < 8);
        frame[flip_at] ^= 1 << bit;
        let mut it = FrameIter::new(&frame);
        kani::cover!(true, "P3a CRC 损坏拒绝");
        assert!(matches!(it.next_frame(), Some(Err(_))), "载荷腐坏必须 Err");
    }

    /// P3b：非法魔数（≠DRNO ≠ESAL）→ 拒绝
    #[kani::proof]
    fn bad_magic_rejected() {
        let mut frame = encode_frame(FrameType::Txn, 1, b"xy");
        let m: u32 = kani::any();
        kani::assume(m != FRAME_MAGIC && m != SEGMENT_MAGIC);
        frame[..4].copy_from_slice(&m.to_le_bytes());
        let mut it = FrameIter::new(&frame);
        kani::cover!(true, "P3b 非法魔数拒绝");
        assert!(matches!(it.next_frame(), Some(Err(_))), "非法魔数必须 Err");
    }

    /// P3c：版本号 ≠ FRAME_VERSION → 拒绝
    #[kani::proof]
    fn bad_version_rejected() {
        let mut frame = encode_frame(FrameType::Txn, 1, b"xy");
        let v: u16 = kani::any();
        kani::assume(v != FRAME_VERSION);
        frame[4..6].copy_from_slice(&v.to_le_bytes());
        let mut it = FrameIter::new(&frame);
        kani::cover!(true, "P3c 版本不匹配拒绝");
        assert!(matches!(it.next_frame(), Some(Err(_))), "版本不匹配必须 Err");
    }

    /// P3d：帧型 ∉ {1,2,3,4} → 拒绝
    #[kani::proof]
    fn bad_frame_type_rejected() {
        let mut frame = encode_frame(FrameType::Txn, 1, b"xy");
        let t: u16 = kani::any();
        kani::assume(t == 0 || t > 4);
        frame[6..8].copy_from_slice(&t.to_le_bytes());
        let mut it = FrameIter::new(&frame);
        kani::cover!(true, "P3d 非法帧型拒绝");
        assert!(matches!(it.next_frame(), Some(Err(_))), "非法帧型必须 Err");
    }

    /// P3e：len 字段超出剩余字节 → 拒绝（切片前防越界，P0-2 回归）。
    /// 不用 frame.clone()（分配建模使 CRC 循环长度符号化，symex 爆炸）。
    #[kani::proof]
    #[kani::unwind(9)]
    fn payload_len_exceeds_rejected() {
        let mut frame = encode_frame(FrameType::Txn, 1, b"xy");
        let huge: u32 = kani::any();
        kani::assume((huge as usize) > frame.len() - HEADER_LEN);
        frame[16..20].copy_from_slice(&huge.to_le_bytes());
        let mut it = FrameIter::new(&frame);
        kani::cover!(true, "P3e len 越界拒绝(P0-2 回归)");
        assert!(
            matches!(it.next_frame(), Some(Err(_))),
            "len 越界必须 Err 而非越界切片"
        );
    }

    /// P1b：帧流终止之 ESAL 段尾——帧后接 trailer，迭代在 trailer 处停，
    /// 且不再产出（不把 trailer 当帧、也不把帧当 trailer）。
    /// unwind 32 ≥ encode_trailer 内 CRC 的 28 字节外层循环 + 守卫轮。
    #[kani::proof]
    #[kani::unwind(32)]
    fn segment_trailer_stops_iteration() {
        let frame = encode_frame(FrameType::Seal, 3, b"zz");
        let trailer = encode_trailer(1, 3, 3);
        let mut seg = frame.clone();
        seg.extend_from_slice(&trailer);
        let mut it = FrameIter::new(&seg);
        match it.next_frame() {
            Some(Ok((ty, seq, _))) => {
                assert_eq!(ty, FrameType::Seal);
                assert_eq!(seq, 3);
            }
            _ => panic!("trailer 前的帧必须正常解码"),
        }
        kani::cover!(true, "P1b ESAL 段尾识别停止");
        assert!(it.next_frame().is_none(), "ESAL trailer 必须终止迭代");
    }
}
