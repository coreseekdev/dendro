#!/usr/bin/env bash
# 测试基线计数（第十二轮 P12-1：回应基线数字以本脚本为准）
set -e
cd "$(dirname "$0")/.."
cargo test --workspace 2>&1 | awk '/test result/ {p+=$4; f+=$6; i+=$8} END {printf "workspace tests: passed=%d failed=%d ignored=%d\n", p, f, i}'
cargo run -q -p slt -- run tests/slt/dendro 2>&1 | tail -1
