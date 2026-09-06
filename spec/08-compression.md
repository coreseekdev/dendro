# SPEC 08 — 压缩策略与性能衰退研究

状态：**定稿 v1**（对应需求 #6：只读大数据库压缩必须 + 量化性能衰退）

## 1. 压缩分层（哪里压、压什么）

| 层 | 机制 | 默认 |
|----|------|------|
| prolly 节点(chunk) | 不压；对象整体可选 zstd(1) | 关（节点小且热点，解压税不划算）|
| WAL 帧 | TXN 批内行编码已是紧凑格式；段级可选 zstd | 关（延迟敏感）|
| CBF 热块 | codec=RAW/BITPACK/RLE_DICT（GPU 可解，SPEC 05）| 自适应(SPEC 05 §4) |
| CBF 冷块 | codec=ZSTD(level 1..3) | 物化时按 age 策略 |
| 字符串字典 | RLE_DICT 字典字符串 | distinct<20% 必开 |
| 整库再编码 | 后台 re-encoder：热块→冷块迁移重编码 | age>7d 触发(v2) |

## 2. "性能衰退"的量化定义（研究问题形式化）

P(codec, column_type, level, selectivity) 四元曲线，指标：

1. **压缩率** R = raw/compressed（越高越好）
2. **解码吞吐** T = GB/s 单线程（衰退主指标）
3. **查询时间放大** Q = scan 耗时(codec) / scan 耗时(RAW)
4. **收益-成本平衡点**：R/T 比值（每 GB 每秒节省）

衰退假设（待基准证实/证伪）：
- zstd level ↑ ⇒ R 缓升、T 骤降 ⇒ Q 恶化存在拐点（预期 level≥9 后得不偿失）
- 字典列（低基数）RLE_DICT 在 GPU/CPU 双侧皆优（解码即 gather）
- BITPACK 窄整数 T 接近 RAW（位搬运）
- 冷块 ZSTD(1~3) 是性价比甜点（tonbo/influxdb3 同款选择：tonbo 默认 ZSTD、
  influxdb3 zstd(1)——调研一致）

## 3. 基准设计

数据：TPC-H lineitem 风格合成数据（生成器 `benches/gen/`），
列型覆盖：顺序 int64(pk)、随机 int32、稀疏 int32(大量重复)、浮点、
低基数 utf8(5 值)、中基数 utf8(1e4 值)、高基数 utf8(近乎唯一)、
定长时间戳、可空列(null 0/10/50%)。
规模：每列 10M 行（~80MB 定宽），保证曲线稳定。

协议：
```
对每 (codec, level, column_type, null_ratio):
  编码 10 次 → 中位编码吞吐
  解码 10 次 → 中位解码吞吐 + 验证逐字节等于原列
  记录 R, T_enc, T_dec
列存 AP 查询(Q1 聚合, 选择性 {0.01%, 1%, 100%}) 跑 RAW/ZSTD{1,3,9,19} → Q 曲线
输出: benches/results/compression/{csv, png 由脚本生成, BASELINE.md 摘要}
```

原型先跑 Python（zstandard，快速出趋势），Rust 实现后跑正式曲线
（Rust 数字为报告口径，Python 数字仅原型参考）。

## 4. 与整体性能的关系

- 压缩收益的兑现条件：解压并行度 ≥ 传输/存储节省 ⇒ 块级并行解码（块 1–8MB，
  线程池分发）+ 对象存储按 block range GET（CBF block 边界即 range 边界）
- OSS 延迟注入下重跑 Q 曲线：RTT ↑ ⇒ 大块/少请求策略优（验证 SPEC 05 的 8MB 上限）

## 5. Python 原型实证结论（prototype/columnar，2026-09-07，10M 行×8 列型）

| 列型 | 热/GPU 层 | 冷层 | 依据 |
|------|-----------|------|------|
| 顺序/时间戳 int | RAW | zstd(3) | zstd 白吃 delta 熵，pk_seq R=7.9@L1 |
| 随机 int32（无重复）| RAW | **不压** | zstd 无益甚至负收益 |
| 低重复 int32 | RLE_DICT+BITPACK(id) | zstd(3) | id 流 BITPACK 17bit = 53% 原始 |
| f64 | RAW | zstd(3)（期望有限）| mantissa 低位近随机，R 仅 1.3-1.9 |
| 低基数字符串 | RLE_DICT | dict+zstd(3) | R=17.7 |
| 中基数字符串 | RLE_DICT | dict+zstd(3) | R=15 vs raw+z 的 2.6（10 倍差距）|
| 高基数字符串 | RAW | zstd(1) | hex 字符集 4bit/字节熵 → 上限 2:1 |

**核心发现**：
1. **解码吞吐对 zstd level 不敏感**；编码吞吐在 L1→3 掉 0-10%，3→9 掉 70-80%，
   9→19 掉 95%+ ⇒ **冷块默认 zstd(3)**（比 L1 压缩率显著更高、解码几乎同价）
2. 字典收益实用边界：post-zstd 下 mid_card(dict) 13.8R vs raw 2.6R；高基数连 zstd
   后仍亏 ⇒ SPEC 05 §4 的 distinct<20% 规则成立且保守
3. DELTA codec（id=5）对 pk/ts 列的价值被定量确认（zstd 独占 delta 熵 3.9-10.8R），
   Rust 实现应内置 DELTA 以摆脱对 zstd 的隐性依赖
4. 测量陷阱：CPython bytes(b) 零拷贝假数字；S-dtype 不剥尾部 NUL 虚增压缩率

## 6. 与实现的映射

- 压缩基准 bin：`benches/compression/`（cargo bench 或 `dendro bench compression`）
- 结果表：`benches/results/compression/README.md`
- 默认策略落地：`dendro-columnar/src/codec/mod.rs` 的 choose_codec()
