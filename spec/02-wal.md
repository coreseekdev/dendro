# SPEC 02 — WAL：只写日志直写对象存储

状态：**定稿 v1**

## 1. 设计立场

与传统 WAL 的差异：对象存储**没有 append**，一次 PUT 一个不可变对象。
因此 WAL 的单位不是"文件内追加"，而是**段(segment) = 一个不可变对象**，
事务提交的原子性由"一个段对象整体存在或不存在"保证（slatedb `wal/mod.rs:209-221` 同款契约）。

WAL 的 OSS 友好四原则：
1. **段是唯一写入单元**：8–64MB/段，PUT 一次成型；小事务靠组提交摊薄 RTT
2. **单调编号命名**：`{branch_id}/00000000000000000042.wal`，无 LIST 可寻址
3. **恢复只靠 HEAD**（SPEC 01 §4），LIST 仅兜底
4. **无 rewrite/无原地更新**：修正=新对象；删除只有 GC

## 2. 帧格式（WAL 段内部）

段 = 帧序列 + 段尾。帧（参照 tonbo `wal/frame.rs` 的 24B 头 + CRC-payload-only 决策）：

```
帧头 24B (LE):
  magic      u32 = 0x4F524E44  ("DRNO" 之 LE，视觉可辨)
  version    u16 = 1
  frame_type u16 = 1 TXN | 2 CHECKPOINT | 3 FENCE | 4 SEAL
  seq        u64  // 分支内单调：TXN=事务序号；CHECKPOINT=对应树版本高度
  len        u32  // payload 字节数
  crc32c     u32  // 只覆盖 payload（头与 payload 独立失效可分别处理）
payload:
  TXN        = 行变更批(行编码，SPEC 03 §6)：[table_id u32][n_ops u32]
               {op u8: 0 INSERT/1 DELETE; row_key_len+row_key; row_payload} × n_ops
               + [txn_id u64][commit_ts u64][读集摘要 hash u64×k(OCC 验证凭证)]
  CHECKPOINT = {tree_root_hash 20B}{chunk_refs: 变更 chunk 地址列表(供预取/GC)}
  FENCE      = {epoch u64}  // 接管写权时首帧
段尾 32B:
  {magic u32}{frame_count u64}{min_seq u64}{max_seq u64}{crc32c u32}(覆盖段尾自身)
  段尾在全部帧写完后计算——段是完整对象，段尾可校验整段完整性
```

写入顺序即 seq 顺序（无 key 排序）——WAL 是回放日志不是查询结构。

## 3. 组提交（commit pipeline）

```
事务线程                    WAL 管道(每分支一个 writer task)
────────                    ──────────────────────────────
OCC 验证+install (内存)  ─┐
append 帧 → seq 申请      │→ MPMC 队列 ─→ 聚帧器(当前段缓冲)
等待 durable(seq) Latch ──┘                │ 触发条件任一:
                                           │   段字节 ≥ seg_max_bytes(默认 32MiB)
                                           │   flush_interval 到期(默认 50ms)
                                           │   显式 flush
                                           ▼
                                    上传对象 {branch}/seg.wal
                                           ▼
                              manifest 推进 wal_flushed_seg
                                           ▼
                        唤醒 Latch: durable_seq = 段内最大 seq
```

- `durability` 三档（会话/服务器可配）：
  - `no_wait`：帧入队即返回（最快，崩溃丢窗口内事务）
  - `group`（默认）：等待所在段 durable —— 延迟上限 ≈ flush_interval + RTT
  - `always`：每事务独立触发 flush（OSS 上 = 每事务 ≥1 RTT，仅低频关键写用）
- Latch 表：`DashMap<seq, broadcast>` 或原子 bitmap + condvar；实现用 `parking_lot`。
- 写放大控制：CHECKPOINT 段(树物化)合并 TXN 段上传时机，避免双写抖动。

## 4. 恢复

```
读 manifest: refs[b].wal_flushed_seg = F, refs[b].commit = C
1. HEAD 探测 {b}/{F+1..} 找到实际尾部 S（SPEC 01 §4）
2. 顺序回放 [F+1..S]：TXN 帧 → 重放进 memtx（重算 install，幂等）；
   CHECKPOINT 帧 → 校验 chunk 存在性(抽样 HEAD)，更新树根候选
3. 高于最后 CHECKPOINT 的未物化 TXN 重新物化（chunker 增量应用）
4. 推进 manifest
恢复时间目标：T_recover ≈ 段数 × RTT + 回放速率；段越大段数越少
（注意权衡：seg_max_bytes ↑ ⇒ 恢复下载量 ↑；默认 32MiB 平衡，基准验证 SPEC 09）
```

## 5. 分支与 WAL

- **每分支独立 seq 空间与段序列**：分支=独立日志流，天然隔离，merge 在版本层做
- 分支创建：不复制任何段；manifest 记 `fork_at{commit, wal_seg}`，
  恢复子分支 = 父分支 checkpoint(树根) 即可，父分支 WAL 段由父分支自己的 GC 逻辑管
- 子分支的段号从 0 开始；`fence/{branch}` 对象实现单写者互斥（见 §6）

## 6. 写者 fencing（多客户端写同一分支）

照抄 slatedb 两步接管：
1. `put_if_absent(fence/{branch}, epoch+1)`；成功者获写权，旧写者后续 `put_if_absent`
   的段路径携带 epoch 前缀，与新 fence 不符即被拒
2. 会话持有租约（默认 30s，后台心跳续期）；会话退出释放
（简化：v1 单机单进程内天然互斥，fencing 机制实现但主要服务多进程/多节点场景）

## 7. local-staged 模式（低延迟选项）

`wal.mode = direct_oss`（默认，纯云原生）| `local_staged`：
- 帧先写本地 NVMe journal 文件（POSIX append + fdatasync，组提交内聚）
- 后台按段切出并上传 OSS；上传确认前本地 journal 保留
- 崩溃恢复：OSS 段 + 本地 journal 尾部合并回放
- 取舍：延迟从 `interval+RTT` 降到 `fdatasync` 级；代价是计算节点不再无状态
（HANA/neon 的本地缓冲同思路；作为可选，默认关闭）

## 8. 与实现的映射

| SPEC 条目 | 代码 |
|-----------|------|
| 帧编解码 | `dendro-core/src/wal/frame.rs` |
| 段写入/聚帧器 | `dendro-core/src/wal/writer.rs` |
| 组提交 Latch | `dendro-core/src/wal/commit_latch.rs` |
| 回放/恢复 | `dendro-core/src/wal/replay.rs` |
| fence | `dendro-core/src/objstore/fence.rs` |
