# 05 · WAL 日志格式与组提交

> 代码：`crates/dendro-core/src/wal.rs`
> 设计立场：对象存储没有 append，所以 WAL 的原子单位不是"文件内追加"，
> 而是**段 = 一个不可变对象**。一次 PUT 一个段，段内所有事务同生共死。

## 1. 段与帧的全景

```
{root}/wal/{branch}/00000000000000000001.wal      ← 段（不可变对象，8–64MB）

段内容 = 帧 × N + 段尾 32B：

┌────────────┬────────────┬────────────┬─────┬──────────────┐
│ Frame #0   │ Frame #1   │ Frame #2   │ …   │ Trailer 32B  │
└────────────┴────────────┴────────────┴─────┴──────────────┘

Frame（帧头 24B + payload）：
偏移  大小  字段          值/含义
────────────────────────────────────────────────────────────
0     4    magic         0x4F524E44 (LE)   ← ASCII "DRNO" 倒读，肉眼可辨
4     2    version       1
6     2    frame_type    1=TXN  2=CHECKPOINT  3=FENCE  4=SEAL
8     8    seq           u64 分支内单调（TXN=事务提交序号）
16    4    len           payload 字节数
20    4    crc32c        只覆盖 payload（头与载荷独立失效可分别处理）
24    …    payload
────────────────────────────────────────────────────────────

Trailer（32B，全部帧写完后追加）：
0     4    magic         0x4C415345 ("ESAL")
4     8    frame_count   u64
12    8    min_seq       u64
20    8    max_seq       u64
28    4    crc32c        覆盖段尾前 28B
────────────────────────────────────────────────────────────
段级完整性：段尾 CRC 校验通过 = 段完整（对象 PUT 本身原子，段尾是冗余防线）
```

为什么 CRC 只盖 payload：头部损坏和载荷损坏是不同性质的故障
（前者多为截断，后者多为位翻转），分开校验能定位问题且省一次大范围哈希。

## 2. TXN 帧 payload——一个事务的全部行变更

```
TXN payload = N 条表记录：

┌ n_tables u32 ┐
│ table_id u32 │ n_ops u32 │ op × n_ops            ← 每张表一段
│   op: key_len u32 │ key bytes │                 ← 键（03 章保序编码）
│        val_len u32 │ val bytes                ← val_len = 0xFFFFFFFF 表示 DELETE
└ table_id … ┘
示例：单表单行插入 "v" 到 key 0x10 80..2A
  01 00 00 00                       1 张表
  07 00 00 00  01 00 00 00          table_id=7, 1 个操作
  09 00 00 00  10 80 00 00 00 00 00 00 2A      key（9B）
  01 00 00 00  76                   val_len=1, "v"
```

回放即重演：TXN 帧的每个 op 重新 `install` 进 memtx 并登记 pending
（幂等——同一 seq 的同一变更装出同一状态）。

## 3. CHECKPOINT 帧 payload——树版本的"存证"

```
┌ catalog_root 20B ┐┌ commit_addr 20B ┐┌ seq_covered 8B ┐
```

- `catalog_root`：checkpoint 产生的 catalog 树根（05/08 章）
- `commit_addr`：对应的 commit 对象
- `seq_covered`：**该树已包含的最高事务 seq**——恢复时跳过更老 TXN 帧的依据

## 4. 组提交——把 RTT 摊薄到一整个批次

单事务等一次 PUT = 1 RTT，太贵。组提交把"等待"变成"凑批"：

```
事务线程 T1 T2 T3 …                    WAL flush 线程（每分支 1 个）
    │ append(帧, seq)                      │ 每 flush_interval 醒一次
    │  └ 缓冲追加 + pending_frames++       │ pending>0 ?
    │  └ await_durable(seq)：              │   ├ 抓 max_seq
    │      Condvar.wait_for(interval)     │   ├ flush_now(): 段对象 PUT
    │        │ 超时？→ 自己 flush_now()    │   └ advance_durable(seg, max_seq)
    │        ▼                            │       durable_seq = max(…, max_seq)
    │      durable_seq ≥ seq ? 返回 : 继续等│      Condvar.notify_all()
    │◀────────────────────────────────────┘
```

实测（LocalDir，20 并发无、单连接串行、200 样本）：

```
durability=group   间隔 1ms → p50 1.07ms   p99 1.15ms
                   间隔 5ms → p50 5.09ms   p99 5.16ms
                   间隔 50ms→ p50 50.2ms  p99 50.2ms     ← p99≈p50，尾延迟压平
durability=no_wait 间隔任意 → p50 0.006ms
S3 + 注入 100ms RTT → 单行 p50 76ms（流水线吸收，非 2×RTT 放大）
```

三档 durability 的语义契约：

| 档 | ACK 时机 | 崩溃保证 |
|----|----------|----------|
| `NoWait` | 帧入缓冲即返回 | 丢最后未 flush 段内的事务（窗口 = interval）|
| `Group`（默认）| 所在段 PUT 成功后 | **已 ACK 零丢失** |
| `Always` | 本事务单独触发 PUT 成功后 | 已 ACK 零丢失，延迟最高 |

## 5. 恢复——HEAD 探测，绝不 LIST

崩溃后可能存在"已上传但未记入 manifest"的段（manifest 落盘晚于 WAL PUT）。
恢复三步（`recovery.rs` + `wal::probe_tail`）：

```
已知：manifest.refs[b].wal_seg = F（checkpoint 时记录），covered_seq = C

① 探测尾部（指数+二分，O(log N) 次 HEAD）：
   HEAD seg F+1 → 404 ⇒ 没有新段
   否则 F+1, F+2, F+4, F+8 … 探到第一个 404，再在区间二分
        ┌─ HEAD ─┬─ HEAD ─┬─ HEAD ─┐
      F+1      F+2      F+4      F+8(404)     ⇒ 尾在 [F+4, F+8) 二分
② 顺序回放 (F, tail] 的每一帧：
   TXN 帧     seq > C ? → decode_txn → install 进 memtx + 登记 pending
              seq ≤ C ? → 跳过（已在树里）
   CHECKPOINT → 校验 commit chunk 存在性，推进 max_seq
③ branch.restore_seq(max_seq)：seq 计数器与可见水位归位
```

实测：2 万未物化事务（NoWait 模拟崩溃）恢复 8ms，行数 19,867/20,000——
差额正是 NoWait 最后未 flush 段的固有丢失窗口；Group 模式下已 ACK 事务零丢失。

前提不变量：**段号存在性对删除单调**——GC 只从低段号删且带 min_age，
探测才不会把"GC 删过"误判为"从未存在"。

## 6. OSS 友好清单（本层的自查表）

- [x] 段是唯一写入单元，PUT 一次成型（无 append、无 rewrite）
- [x] 单调编号命名，恢复零 LIST
- [x] 组提交摊薄 RTT；间隔是延迟预算旋钮
- [x] 段大小（默认 32MiB）× 间隔（50ms）平衡 恢复下载量 与 请求数
- [x] CRC 每帧 + 段尾双重校验；对象不可变
- [ ] v2：段内块级 range-GET 回放（当前整段 GET）
