# SPEC 05 — 列存 CBF (Dendro Block Format)：GPU 友好的列式投影

状态：**定稿 v1**（调研结论：tonbo/lance/influxdb3 无一以 GPU 解码为一等目标——这是差异化空间）

## 1. 定位

- 列存是**提交物化的投影**（derived），不是权威数据；权威=prolly 行树(SPEC 03)
- **增量分段物化（✅ 已实现，OSS 友好）**：checkpoint 只把 memtx 增量
  （纯内存读，零树扫描、零远端读）写成一个新的不可变 CBF 段对象并 PUT；
  `TableEntry.col_segments[]` 按时间序累积，末尾=最新
- 扫描端：段级 pk 剪枝（每段记录 order 域 min/max）→ 行组级 zone map 剪枝
  → 列块解码；pk 去重（新段优先）+ `col_deletes` 删除抑制 + memtx overlay 合并
- 段数 ≥ 8 或删除数 > 1 万时触发**全量重建**（一次写放大换长期读放大，
  替代"每次 checkpoint 重传整表"——慢/贵网络下的关键设计）
- AP 查询走 CBF；TP 点查走行树/memtx；物化滞后窗口 = checkpoint 间隔
- 分支结构共享延伸到列存：子分支 fork 时引用父分支的段列表（零复制）

## 2. 文件布局

```
{root}/col/{table_id}/{gen:020}.cbf
┌─────────────────────────────┐
│ RowGroup 0                  │  每个 RowGroup 固定 1,048,576 行(tonbo 同款)
│   ColChunk(pk, i64)         │  每个 ColChunk = N 个 Block(1–8MB 解码单元)
│   ColChunk(v, f64)          │
│   ColChunk(s, utf8)         │
│ RowGroup 1 ...              │
│ ...                         │
│ [块数据区全部 64B 对齐]        │
├─────────────────────────────┤
│ Footer (自描述二进制)          │
│   RG 元数据表: 每 RG 每列      │
│     {block offsets[], rows,  │
│      null_count, min, max,   │
│      codec_id, first_row}    │
│   文件级: {table_id, schema  │
│     hash, row_range(min/max  │
│     pk), rg_count, key cols} │
│   footer_len u32 + magic u32 │  文件尾 8B 定位 footer
└─────────────────────────────┘
```

- **顺序即布局**：row group 依 (checkpoint, 主键) 排序 ⇒ 主键范围列式有序，
  range 剪枝直接用 RG 级 min/max(pk)
- footer 单对象读（通常 < 1MB），加载即得全文件索引——**scan 前 1 次 range GET**

## 3. Block 格式（GPU 可解的核心）

```
Block 头 64B (与 GPU segment/cache line 对齐):
  magic        u16 = 0xCB71
  version      u16 = 1
  codec_id     u8    // 0 RAW  1 BITPACK  2 RLE_DICT  3 ZSTD  (4 FSST 保留 5 DELTA 保留)
  flags        u8    // bit0: all_non_null(无 validity 区) bit1: sorted
  rows         u32
  null_count   u32
  data_len     u64   // 本块数据区字节数(编码后)
  raw_len      u64   // 解码后字节数
  min          u64   // 定点解释(数值=物理值；字符串=前 8B 序列)
  max          u64
  reserved     u16   // 对齐填充
  crc32c       u32   // 覆盖 data 区

data 区按 codec:
  RAW      定宽列: rows × width 连续；变宽列: offsets(u32)×(rows+1) + bytes
  BITPACK  bit_width u8 + packed stream (定宽整数字典化后无符号化)
  RLE_DICT dict 页: {dict_len u32}{width u8}{字典值 RAW}{id 流 BITPACK}
           变宽字典: 字典区 offsets+bytes，id 流纯 u32 ⇒ GPU gather 理想
  ZSTD     整块 zstd(可带级别在 flags 高位)；只用于冷块
validity  位图区(64B 对齐，LSB-first，Arrow 同构)——flags.bit0=1 时省略
```

**GPU 解码保留机制**（对应需求 #4）：
1. codec_id ∈ {RAW, BITPACK, RLE_DICT} 的块：解码是**纯数据并行指针算术**，
   无熵解码依赖，CUDA/ROCm kernel 一遍出 Arrow 定宽 buffer
2. 64B 对齐 + validity 与数据分离 ⇒ 显存拷贝无需重排
3. footer 的 (min,max,null_count) 使 GPU 端可先剪枝后搬运（zone map 常驻）
4. `decode_plan`（footer 内）：每列的 codec 序列与输出 buffer 形状描述，
   供 kernel 生成器/未来 CPU SIMDMaterialize 使用
5. zstd 只允许出现在冷块 ⇒ 热/GPU 路径永不过熵解码器（tonbo 默认全 zstd 是
   其 GPU 不友好根因——调研结论）

## 4. 编码器选择策略（列级自适应）

```
采样列值(每 RG 前 64K 行):
  定宽整数: distinct_ratio < 0.1 → RLE_DICT else if 值域窄 → BITPACK else RAW(+ZSTD 冷)
  浮点:     RAW(+ZSTD 冷)；时间戳: DELTA(保留)/RAW
  字符串:   distinct_ratio < 0.2 → RLE_DICT(字典字符串) else ZSTD(冷)/RAW(热)
  排序检测: 前缀有序 → flags.sorted (供 binary search / PGM)
```

## 5. Arrow 内存表示

- CBF 块解码直接产出 Arrow 数组（arrow-rs `ArrayData`，offset 0 零拷贝构造）
- 读取聚合粒度 = Block(1–8MB)；scan 以 RG 为调度单元，块级并行
- parquet 导出（feature `parquet`）：Arrow batch → parquet crate 写出（duckdb/pyarrow
  互通验证），不参与运行时热路径

## 6. 删除/更新与 deletion vector

- 列存不可变；行删除/更新以 **deletion vector**（位图对象，内容寻址 chunk）挂在
  manifest `tables[t].col_delvs`：`{cbf_gen → bitset chunk addr}`
- merge/branch 引用的 row group 同样引用删除位图 ⇒ 一切仍只写

## 7. 统计与剪枝管线

```
谓词含 pk 或列 c:
  RG 剪枝: footer (min,max) → 跳过整 RG
  Block 剪枝: 块级 (min,max) → 跳过块(解压前判定)
  块选择率 < 阈值 且 codec=ZSTD → 解压后谓词下推
无谓词全扫: 流水线按 RG→Block 分发到工作窃取线程池
```

## 8. 与实现的映射

| SPEC 条目 | 代码 |
|-----------|------|
| CBF 读写 | `dendro-columnar/src/cbf/{writer,reader,footer}.rs` |
| 编码器 | `dendro-columnar/src/codec/{raw,bitpack,rledict,zstd}.rs` |
| 统计/剪枝 | `dendro-columnar/src/stats.rs` |
| parquet 导出 | `dendro-columnar/src/parquet_export.rs` (feature) |
| 物化器 | `dendro-core/src/materializer.rs` |
