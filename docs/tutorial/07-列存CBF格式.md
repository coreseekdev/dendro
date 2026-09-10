# 07 · 列存 CBF 格式

> 代码：`crates/dendro-columnar/`（writer/reader/footer/codec）
> CBF = **C**olumn **B**lock **F**ormat。设计目标：**GPU 可解码的热块** +
> 对象存储友好的段文件 + 互通用的 parquet 导出（feature）。
> 调研结论（SPEC 05）：tonbo/lance/influxdb3 无一以 GPU 解码为一等目标——这是格式层面的差异化。

## 1. 文件总览

```
col/{table}/{hash12}.cbf                       ← 一个不可变对象 = 一个"段"

┌────────────────────────────────────────────┐
│ RowGroup 0                                  │ 1M 行/组（默认）
│   ColChunk c0 (pk)  = Block×1..n + validity │
│   ColChunk c1 (…)   = …                     │
│   ColChunk c2 (…)   = …                     │
│ RowGroup 1 …                                │
│ ⋮      （数据区全部 64B 对齐）                 │
├─────────────────────────────────────────────┤
│ Footer body（二进制自描述）                    │
│ [footer_len u32 LE][magic u32 LE]           │ ← 文件尾 8B 定位
└─────────────────────────────────────────────┘
```

列块（ColChunk）按列组织——同列的字节连续存放，这正是列存的定义；
块（Block，1–8MB）是**解码与传输的最小单元**，块与块之间也是 64B 对齐。

## 2. Block 头（64B，逐字段）

```c
struct BlockHeader {            // 52 字节字段 + 12B 零填充 = 64B
  u16 magic        = 0xCB71     // [0..2)   LE，视觉校验
  u16 version      = 1          // [2..4)
  u8  codec                     // [4]      见 §3
  u8  flags                     // [5]      bit0 all_non_null（无 validity 区）
                                           // bit1 sorted；bits2-7 zstd level
  u32 rows                      // [6..10)
  u32 null_count                // [10..14)
  u64 data_len                  // [14..22)  编码后数据区长度
  u64 raw_len                   // [22..30)  解码后长度
  u64 min                       // [30..38)  zone map（order 域定点，§4）
  u64 max                       // [38..46)
  u16 reserved                  // [46..48)
  u32 crc32c                    // [48..52)  只覆盖 data 区
  u8  pad[12]                   // [52..64)  凑齐 64B cache line
}
```

64B 对齐 + 头部自带统计 ⇒ **GPU/CPU 拿到裸指针就能剪枝、就能解码**，
不需要先解析容器。

## 3. 六种 codec 的字节布局

| id | 名 | 字节布局 | 适用 | GPU 可解 |
|----|----|----------|------|:---:|
| 0 | RAW | 定宽：值连续；变宽：offsets(u32×(n+1))+bytes | 高基数/热块 | ✅ 指针算术 |
| 1 | BITPACK | `bit_width u8` + LSB-first 位流 | 窄整数 | ✅ 位搬运 |
| 2 | RLE_DICT | 定宽：`dict_len u32`,`width u8`,字典 RAW,id 流 BITPACK；变宽：`dict_len u32`,`width=0`,`bytes_len u32`,offsets,bytes,id 流 | 低中基数 | ✅ gather |
| 3 | ZSTD | 整块 zstd（level 在 flags 高位） | 冷块 | ❌ 熵解码 |
| 4 | FSST | `符号表 2312B` + `每行码偏移 u32×(n+1)` + FSST 码流 | 高基数文本 | ✅ 查表展开 |
| 5 | DELTA | `baseline u64 LE` + Σ varint(zigzag(v[i]−v[i−1])) | 顺序 pk/ts | ✅ 前缀扫描 |

DELTA 编码示例（顺序列 100, 102, 105）：

```
unsigned 化域上回绕差分： [100, +2, +3]
zigzag: 0→0, +2→4, +3→6
字节： 64 00 00 00 00 00 00 00 | 04 | 06
       └ baseline 100 (LE)      └varint└varint     顺序 i64 每值 ~1 字节
```

FSST（id=4）值得单独说两句：它不压"重复值"（那是 RLE_DICT 的活），压的是
**字符级的重复模式**——"http://"、"SELECT "、邮箱后缀这种跨行共享的字节片段。
编码器先采样 ≤16KB 训练一张 255 个符号（1–8 字节长）的码本，每字节查最长
匹配符号 → 1 字节码；不在码本里的字节走 escape(255)+原字节，**保证无损**。
解码就是逐码查表展开，无熵解码依赖，字节对齐——所以热层可用，GPU 侧码本
放共享内存即可。自然文本约 2R；输入 <32KB 自动退化为原样拷贝（码本里
switch=0），小块无收益也无损失。符号表内联在块里（2312B），块保持自描述。

选择策略（`choose_codec`，采样首行组前 64K 行）：

```
整型  distinct<10%          → RLE_DICT
整型  单调（pk/ts）          → DELTA            ← 实测 R=8、解码 2.3GB/s
整型  其他                   → RAW
f64                         → RAW              （mantissa 低位近随机，压不动）
字符串 distinct<20%          → RLE_DICT         （实测低基数 R≈48）
字符串 高基数 & 均长≥6B      → FSST             ← 自然文本 ≈2R，无熵解码
字符串 其他（短串高基数）    → RAW
Bytes 高基数                → RAW              （blob 常不可压，FSST 可显式指定）
ZSTD 只由调用方显式指定（冷块再编码，v2）
```

## 4. zone map 的定点解释（min/max 怎么算）

min/max 是 **u64 order 域**——把任意类型的"可比较性"压进定长整数：

```
整型     i64 值 ^ i64::MIN           （符号翻转，保序）
float64  IEEE754 全序变换             （负数取反位/正数置符号位）
字符串   前 8 字节大端序列             （近似：只影响剪枝精度，不影响正确性）
```

因此"谓词 vs 块统计"的比较是纯整数比较——GPU 上就是一次向量比较指令。

## 5. validity 位图

独立 64B 对齐区，LSB-first（与 Arrow 同构），**1 = 有效**。
块头 `flags.bit0 = 1`（全非空）时整区省略。`null_count = rows − popcount(位图)`。
解码时直接作为 Arrow `ArrayData` 的 null buffer 零拷贝挂载。

## 6. Footer——一次 range-GET 拿到全文件索引

```
Footer body（二进制）：
  schema（内嵌 arrow Schema：列名+列型=物化时的真实类型）
  rg_count u32, total_rows u64
  pk_min u64, pk_max u64          ← 第 0 列 order 域（文件级剪枝）
  rgs[]：每行组
     first_row u64, rows u32
     cols[]：每列 ChunkMeta
        blocks[]：每块 { offset u64, rows u32, null_count u32,
                         codec u8, flags u8, min u64, max u64 }
        validity_offset u64, validity_len u64
文件尾 8B：[footer_len u32 LE][magic u32 LE]
```

**scan 的第 0 次远端 IO** = 文件尾 8B + footer（通常 <1MB 的 range GET）。
之后每个要读的块都是一次独立 range GET——列裁剪、行组裁剪、块裁剪三层
都发生在"发出请求"之前。

```
WHERE id BETWEEN 1e6 AND 2e6 的剪枝旅程：
  footer.pk_range 与谓词不相交      → 整文件跳过（0 IO）
  某 RowGroup 的 pk min/max 不相交  → 整组跳过（0 IO）
  某块 zone map 不相交              → 整块跳过（0 IO）
  剩余块                            → range-GET 解码
```

## 7. 增量分段——OSS 友好（对比"全量重建"）

checkpoint 的列存物化是**增量分段**，不是整表重写：

```
checkpoint #1（12k 行）：        checkpoint #2（又写 1 行）：   checkpoint #N：
  PUT col/t/aaaa.cbf (12k)       PUT col/t/bbbb.cbf (1 行)     PUT col/t/cccc.cbf (…)
  col_segments=[aaaa]            col_segments=[aaaa,bbbb]      col_segments=[…,cccc]
```

- 增量行来自 **memtx overlay（纯内存）**——写段过程零树扫描、零远端读、一次 PUT
- 扫描时新段优先按 pk 去重（UPDATE= 新段新值胜出）、`col_deletes`（hex 键）
  抑制被删行、再叠 memtx overlay（WAL 尾部）
- 段数 ≥ 8 或删除数 > 1 万 → 全量重建一次（把 N 段合并成 1 段，删旧对象）

**为什么这是慢/贵网络的正确姿势**：每 checkpoint 的上传量 = 增量本身；
下载量 = 查询真正触碰的块。对比"整表重建"：1 亿行表每次 checkpoint 重传
GB 级——这里只传 KB 级增量。

## 8. 实测数字（1e6 行/列，release）

| 列 | codec | 编码 MB/s | 解码 MB/s | R |
|----|-------|----------|----------|---|
| i64 顺序 | DELTA | 163 | **2,272** | **8.00** |
| i64 顺序 | ZSTD3 | 133 | 1,124 | 7.60 |
| utf8 低基数(8 值) | RLE_DICT | 590 | 2,279 | **47.96** |
| i64 随机 | RAW | 148 | 3,618 | 1.00 |

结论同 Python 原型（SPEC 08 §5）：**DELTA 热层免熵解码**（GPU 可直解），
ZSTD3 解码吞吐减半 → 只配冷块；字典化是低基数字符串的唯一正解。
