# SPEC 03 — 版本层：内容寻址、prolly tree、分支与合并

状态：**定稿 v1**（参数以 dolt 实证值为默认，Python 原型验证见 prototype/prolly/）

## 1. 内容寻址 chunk

- **哈希**：SHA-512 取前 20 字节（`Hash20`）；文本编码 base32 自定义字母表 `0-9a-v`
  （排序后文本序=字节序）。理由见 dolt `go/store/hash/hash.go`。
- **Chunk** = `{addr: Hash20, data: Bytes}`；`addr = H(data)`。
- 存储：SPEC 01 §2 路径规则；写入去重：写入集合先 `head` 抽样判定（内存含
  hasCache：本会话已写地址集合），重复内容地址相同，重复 PUT 无害但浪费——
  批量写入前对批量内 + 会话缓存去重。
- 所有对象（prolly 节点、行块、commit、schema）都是 chunk；`data` 首字节是类型标签：

```
0x01 PROLLY_NODE   0x02 COMMIT   0x03 SCHEMA   0x04 ROWBLOCK(大行块，可选)
```

## 2. prolly tree（概率 B-tree）

### 2.1 何时分裂（keySplitter，默认）

对每个 (key, value) 条目追加进节点缓冲时：

```
size = buf.len()
if size < MIN(512):     不分裂
elif size >= MAX(16384): 强制分裂
else:                    h = xxh3_32(key, seed=salt(level))
                         // 条件风险增量（prototype/prolly 实证：必须用此形式）
                         p = 1 - exp(-Δ[(size/4096)^4])   // Δ 对字节精确差分
                         if h/2^32 < p: 分裂
                         // 注：无条件 CDF 阈值 h < F(size) 会使分布系统性偏移
salt(level) = sha512(level as u64 le)[0..8]
```

- 每层不同 salt ⇒ 同一数据在各层独立切分，父边界不相关（dolt weibullCheck, K=4）
- 期望节点 ~4KB，分布近似 Weibull（钟形，少碎块少巨块）
- **确定性与位置无关**：边界只依赖"已见内容+层号" ⇒ 同一子树内容相同 ⇒
  地址相同 ⇒ 结构共享自动成立

### 2.2 节点编码（叶与内部同构，平行数组）

```
PROLLY_NODE chunk:
  level u8 (=0 叶)
  count u32
  keys:    [key_len u16 + key bytes] × count      // 叶=key；内部=子树末 key
  key_off: u32 × count                             // 前缀偏移(二分定位)
  values:  叶 = [val_len u32 + val bytes] × count
           内部 = [child_addr 20B + subtree_count u64] × count
  val_off: u32 × count
  addr_slots: {区段偏移}×k                          // values 内嵌 chunk 地址的位置表
                                                   // (GC WalkAddresses 直接扫描，不解析值)
  尾: crc32c
```

节点本体不压缩（对象存储侧可选整体 zstd，SPEC 08）；解析零拷贝（借用视图）。

### 2.3 Rust v1 实现决策：批量建树用分裂器，增量更新走 CoW 路径复制

**v1 权衡**（原型验证完整 prolly 增量算法；Rust v1 采用简化变体）：
- 批量构建：weibull 分裂器切节点（与 prolly 布局完全一致）
- 增量更新：**路径复制 + 溢出按字节中点重分裂**（CoW B-tree）
- 保留：内容寻址、结构共享（未触及子树地址复用）、chunk 级 diff、O(1) 分支
- 让步：边界不随数据流重新对齐 ⇒ 相同内容经不同更新路径可能产生不同节点边界
  （diff 需多下一层，正确性不受影响）；点更新写放大 = O(树高)，优于全量重切
- v2 升级路径：prototype/prolly 已验证"前缀复用+重切+实时对齐"算法
  （关键坑：条件风险增量、splitter 重置、跨分支 chunk 共享时按位置对账判变更）

### 2.3 写路径 chunker（增量应用变更）

```
apply(base_root, sorted_mutations):
  打开叶 chunker，cursor 定位到第一个变更 key
  复制 base 条目直到变更点 → 应用变更 → 继续
  每次越界(CrossedBoundary)：缓冲序列化为节点 → addr 写入父 chunker 缓冲
  变更点之后的复制在新层盐下重新切分（代价：边界扰动 ~1.5%/字节 @4K，dolt 实测口径）
返回 (new_root, dirty_chunks)
```

**读放大/写放大关键数字**（对 Agent 场景友好）：单 key 写只重写受影响路径 +
扰动边界后的一段，其余子树整棵地址复用（chunk 级去重使重复导入/同分支副本近零成本）。

### 2.4 读路径

- `NodeCursor`：节点内 `key_off` 二分；内部节点 `subtree_count` 前缀和做
  O(log n) 序数定位（支持 OFFSET/行号 seek）
- 点查：根 → 逐层二分 → 叶值；缓存节点(读缓存 LRU)
- 迭代：叶层顺序；`range(start..end)` 两 cursor 夹逼
- **PGM 实验模块**（读优化结构）：对已排序 run 的块索引建立分段线性模型
  （每 64 块一条 (slope, intercept)），点查先模型预测块号再局部二分——
  基准验证 vs 纯二分（SPEC 09 §5；预期大型 run 上比较次数 3-5x 减少）

## 3. 行编码（叶值格式）

```
row payload: [schema_ver u16][col_count u16]
             {col_value: tag u8 + bytes}×col_count     // tag: 0 NULL,1 I32,2 I64,
                                                       // 3 F64,4 UTF8(len u32),5 BOOL,6 BYTES,7 TS_MS i64
键 = 主键元组编码为有序字节序(大端+类型感知)，保证字节序=逻辑序
```

- 有序字节序键使 prolly map 直接服务 `ORDER BY pk`、范围扫描
- 不做行内压缩（压缩在列存层做，SPEC 08）；行块(ROWBLOCK)实验：同表行聚块
  zstd，供冷数据降本

## 4. commit 对象

```
COMMIT chunk:
  root       20B    // 库级根 = refs 所在 map 的地址？不——见下"两级根"
  parents    [20B]×k // 多父 = merge
  height     u64
  branch     utf8    // 冗余便于审计
  ts_ms      i64, author utf8, message utf8
```

**两级根**（dolt 的 datasets AddressMap 简化）：
- 库级 `catalog map`: `{branch_name → commit_addr}` —— 本身是 prolly map，
  即 refs 表；manifest 只存 catalog map 地址 + 各分支 wal 高水位
- 分支级 commit.root = **该分支数据根**（table 目录 map: `{table_name → (schema_addr, table_root_addr)}`，
  同为 prolly map）⇒ **不同分支的表目录也是结构共享的**（加一张表不动别的分支）
- 合并冲突域因此覆盖 DDL 与 DML：表目录 merge = catalog merge 同算法

## 5. 分支 SQL 语义

```sql
CREATE BRANCH [IF NOT EXISTS] name [FROM main [AS OF commit_id]]  -- O(1)：catalog 写一条
DROP BRANCH [IF EXISTS] name
USE BRANCH name                -- 会话级当前分支；未设置=main
SHOW BRANCHES                  -- → catalog map 扫描
MERGE BRANCH src INTO target   -- 见 §6
SELECT * FROM cambium.branches           -- 系统视图
SELECT * FROM cambium.commit_log('main') -- 提交历史(parent 链回溯)
-- time travel（P1-10，§5.1）
SELECT * FROM t FOR SYSTEM_TIME AS OF '<epoch_ms | ISO8601 | commit_hash>' [AS alias]
```

### 5.1 Time Travel（提交快照语义）

```sql
SELECT * FROM t FOR SYSTEM_TIME AS OF '2099-01-01'            -- 时间戳（ISO8601 UTC）
SELECT * FROM t FOR SYSTEM_TIME AS OF 1736000000000           -- epoch 毫秒
SELECT * FROM t FOR SYSTEM_TIME AS OF '<commit_hash>'         -- 提交哈希精读
```

- **精度 = 提交粒度**：commit = checkpoint = 完整物化树；未 CHECKPOINT 的事务
  不在任何提交里，AS OF 不可见（append-only 模型的自然推论）。
- 时间戳沿分支**第一父链**找 `ts_ms ≤ 目标` 的最近提交；链跨 fork 边界
  （分支继承源提交），故可回溯到 fork 之前。早于首个提交 → SQLSTATE 22023。
- 哈希命中 CAS 即用（跨分支快照读允许）；历史树 chunk 不可变 ⇒ 快照稳定可重复。
- 只读：无 memtx overlay、无会话写；谓词/LIMIT/JOIN（各表可独立 AS OF）正常组合。
- 方言注：sqlparser 对 PG 关闭 `supports_table_versioning`；含该子句的语句
  经 `DendroTimeTravelDialect`（版本子句开）重解析兜底（`sql/mod.rs`）。
  已知残留：`E''` 转义串与版本子句同句时兜底方言不识别（42703）——
  sqlparser 的 E'' 支持是类型级门控，自定义方言无法继承；罕见组合。
- **快路径门控**（审计 R2 P0 修复）：PK 直查（`try_pk_pushdown`，读 memtx ∪
  当前树）与 AP 列存（`try_ap_scan`，读当前物化段）都无历史根概念——带版本
  子句的查询必须回落 time travel 路径；视图引用（无提交链）显式 0A000
  （此前两者静默按当前态求值）。列存历史快照 v2。
- 时间字面量校验：年份 |y| ≤ 300_000（无界年份 i64 溢出：debug panic /
  release 回绕）；月长 + 闰年校验；分数秒任意位宽（截断到 ms）。
- 显式事务内 AS OF 以**当前分支 head** 为解析起点（非 BEGIN 冻结根）——
  快照本身不可变，事务内重复 AS OF 在并发 checkpoint 下可能解析到不同提交。

- 分支内事务提交 = 新 commit 节点 + catalog CAS（乐观；冲突重读重试）
- `USE BRANCH` 只是会话态；跨会话并发写同一分支由 commit CAS 串行化（first-writer-wins
  于 catalog 版本；业务冲突见 §6）

## 6. 合并（merge）

```
merge(base_commit, left_root, right_root):
  if left_root == base:  fast-forward（target 直接指向 right）→ 完成
  if right_root == base: 已是最新 → no-op
  对 (base,right) 与 (base,left) 各建 patch_generator（地址相同子树整棵跳过）
  归并两路 patch 流（key 有序）:
    仅左改   → 应用左
    仅右改   → 应用右
    两边同改同值 → 应用（收敛，非冲突）
    两边同改不同值 → CONFLICT(key, left_val, right_val)  // 行级
                     同 key 一删一改 → CONFLICT
    表级(目录 patch)冲突同算法（CREATE 同名表/同表 schema 演化不一致）
  冲突非空：merge 失败，返回冲突清单（SELECT * FROM cambium.last_merge_conflicts）
  否则：基于 left 树增量应用 right patches → 新根 → 新 commit(两父)
```

复杂度：O(diff)；无关表/无关行零成本（结构共享）。
Agent 场景典型用法：分支从 main 分出 → Agent 随意写 → 合并；若冲突，
Agent 可重基于 main 再放（`MERGE` 失败后重开分支 FROM main 重放）。

## 7. checkpoint（WAL → 树）

- 触发：分支 memtx 未物化字节 ≥ `checkpoint_threshold`(默认 64MiB) 或
  距上次 ≥ `checkpoint_interval`(默认 60s) 或显式 `CHECKPOINT`
- 过程：对该分支自上次 checkpoint 以来的 TXN 帧重放排序 → chunker 增量应用到
  上版树根 → dirty chunks 批量上传 → CHECKPOINT 帧(新根+chunk 引用) → catalog CAS
- 幂等：重放同输入产生同 chunk 地址（确定性），崩溃重跑无副作用

## 8. 与实现的映射

| SPEC 条目 | 代码 |
|-----------|------|
| Hash20/base32 | `dendro-core/src/format/hash.rs` |
| splitter/chunker/cursor/patch | `dendro-core/src/prolly/{splitter,chunker,cursor,patch}.rs` |
| 节点编解码 | `dendro-core/src/prolly/node.rs` |
| 行编码 | `dendro-core/src/format/row.rs` |
| commit/catalog | `dendro-core/src/versioned/{commit,catalog}.rs` |
| merge | `dendro-core/src/versioned/merge.rs` |
| PGM(实验) | `dendro-core/src/prolly/pgm.rs` |
