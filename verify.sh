#!/bin/bash
# 验证链路一键门禁（basalt verify.sh 同款模式）
# 用法：bash verify.sh
# 退出码 0 = 全部通过
set -e
cd "$(dirname "$0")"

PASS=0; FAIL=0
report() {
    if [ $1 -eq 0 ]; then
        echo "  ✅ $2"
        PASS=$((PASS+1))
    else
        echo "  ❌ $2"
        FAIL=$((FAIL+1))
    fi
}

echo "=== dendro 验证链路 ==="

echo "[1/7] clippy..."
cargo clippy --workspace --all-targets -- -D warnings > /dev/null 2>&1
report $? "clippy -D warnings"

echo "[2/7] 测试..."
cargo test --workspace > /tmp/verify_test.txt 2>&1
T=$(grep -E 'test result: ok\.' /tmp/verify_test.txt | sed 's/[^0-9]*\([0-9]*\) passed.*/\1/' | awk '{s+=$1} END {print s}')
F=$(grep -cE 'FAILED' /tmp/verify_test.txt || true)
if [ "$F" -eq 0 ] && [ "$T" -gt 0 ]; then report 0 "workspace tests ($T passed)"
else report 1 "workspace tests ($T passed, $F failed)"; fi

echo "[3/7] slt..."
cargo build -p slt > /dev/null 2>&1
./target/debug/slt run tests/slt/dendro > /tmp/verify_slt.txt 2>&1
if grep -q "failed: 0" /tmp/verify_slt.txt; then report 0 "sqllogictest ($(grep -oP '\d+(?=,)' /tmp/verify_slt.txt | head -1) files)"
else report 1 "sqllogictest"; fi

echo "[4/7] TLC..."
cd spec
make check > /tmp/verify_tlc_safety.txt 2>&1
report $? "TLC safety (CommitPipeline)"
make liveness > /tmp/verify_tlc_live.txt 2>&1
report $? "TLC liveness (StallFreedom)"
cd ..

echo "[5/7] Kani（真源编译版 dendro-kani，B2 绑定）..."
# codec.rs 真源经 #[path] 编译进证明（账本 #28，取代旧镜像副本）。
# CRC 以 crc32c-soft 规范模型顶替（真 crate SIMD/asm 不可验），
# 模型等价由下一步差分测试实证。
K=0; KH=10
rm -f /tmp/verify_kani_full.txt
# txn_record_roundtrip 不入门禁：嵌套 Vec drop 建模爆炸（见 harness 注释）
for h in frame_iter_arbitrary_input_never_panics frame_roundtrip_exact_all_frame_types truncated_header_ends_cleanly tiny_frame_at_segment_end_decodes crc_corruption_rejected bad_magic_rejected bad_version_rejected bad_frame_type_rejected payload_len_exceeds_rejected segment_trailer_stops_iteration; do
    if timeout 600 cargo kani --manifest-path verification/kani/Cargo.toml --harness "$h" >> /tmp/verify_kani_full.txt 2>&1; then
        K=$((K+1))
    else
        echo "    harness $h FAILED（/tmp/verify_kani_full.txt）"
    fi
done
if [ "$K" -eq "$KH" ]; then report 0 "Kani L1 ($K/$KH harnesses，真源 codec)"; else report 1 "Kani L1 ($K/$KH harnesses)"; fi

echo "[6/7] CRC 模型差分（crc32c-soft ≡ 真 SIMD 实现）..."
if cargo test --manifest-path verification/crc32c-soft/Cargo.toml > /tmp/verify_crc_diff.txt 2>&1; then
    report 0 "crc32c-soft 差分（绑定缝隙闭合）"
else
    report 1 "crc32c-soft 差分（/tmp/verify_crc_diff.txt）"
fi

echo "[7/7] 验证覆盖率报告（度量定义见 docs/VERIFICATION.md §5）..."
if python3 scripts/verif-coverage.py --kani-log /tmp/verify_kani_full.txt; then
    report 0 "覆盖率报告已输出（不变式 / vLOC / cover）"
else
    report 1 "覆盖率报告失败（manifest 漂移？）"
fi

echo ""
echo "=== 验证结果: $PASS 通过 / $FAIL 失败 ==="
if [ $FAIL -gt 0 ]; then exit 1; fi
