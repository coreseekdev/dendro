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

echo "[1/5] clippy..."
cargo clippy --workspace --all-targets -- -D warnings > /dev/null 2>&1
report $? "clippy -D warnings"

echo "[2/5] 测试..."
cargo test --workspace > /tmp/verify_test.txt 2>&1
T=$(grep -E 'test result: ok\.' /tmp/verify_test.txt | sed 's/[^0-9]*\([0-9]*\) passed.*/\1/' | awk '{s+=$1} END {print s}')
F=$(grep -cE 'FAILED' /tmp/verify_test.txt || true)
if [ "$F" -eq 0 ] && [ "$T" -gt 0 ]; then report 0 "workspace tests ($T passed)"
else report 1 "workspace tests ($T passed, $F failed)"; fi

echo "[3/5] slt..."
cargo build -p slt > /dev/null 2>&1
./target/debug/slt run tests/slt/dendro > /tmp/verify_slt.txt 2>&1
if grep -q "failed: 0" /tmp/verify_slt.txt; then report 0 "sqllogictest ($(grep -oP '\d+(?=,)' /tmp/verify_slt.txt | head -1) files)"
else report 1 "sqllogictest"; fi

echo "[4/5] TLC..."
cd spec
make check > /tmp/verify_tlc_safety.txt 2>&1
report $? "TLC safety (CommitPipeline)"
make liveness > /tmp/verify_tlc_live.txt 2>&1
report $? "TLC liveness (StallFreedom)"
cd ..

echo "[5/5] Kani..."
# 账本 #19 后真跑：H1 任意输入不 panic / H2 编解码对偶 /
# H3 撕尾容忍 / H4 小帧段尾回归（总耗时 ≈30s）
K=0
for h in frame_roundtrip_exact tiny_frame_at_segment_end_decodes truncated_header_ends_cleanly frame_iter_arbitrary_input_never_panics; do
    if timeout 300 kani verification/kani/wal_frame.rs --harness "$h" > "/tmp/verify_kani_$h.txt" 2>&1; then
        K=$((K+1))
    else
        echo "    harness $h FAILED（/tmp/verify_kani_$h.txt）"
    fi
done
if [ "$K" -ge 4 ]; then report 0 "Kani L1 ($K/4: H1 arbitrary / H2 roundtrip / H3 torn / H4 tiny-frame)"
else report 1 "Kani L1 ($K/4 harnesses)"; fi

echo ""
echo "=== 验证结果: $PASS 通过 / $FAIL 失败 ==="
if [ $FAIL -gt 0 ]; then exit 1; fi
