# 04 · prolly 树与磁盘格式

> 代码：`crates/dendro-core/src/prolly/`（splitter / node / chunker / cursor / diff）
> 一个表 = 一棵 prolly 风格的内容寻址树；key = 保序主键编码（03 章），value = 行编码。

## 1. 为什么是 prolly 树

普通 B-tree 改一个字节要重写一条路径且无法和历史共享；纯 log 无法随机读。
prolly tree（probabilistic B-tree）用**内容决定切分**换来了三个性质：

1. **切分点只由数据决定** ⇒ 相同子树内容必然得到相同节点字节 ⇒ 相同地址
2. 地址相同 ⇒ **结构共享**：改一行，只有受影响路径是新对象
3. 地址相同 ⇒ **diff 可以整棵跳过**（合并的基础，08 章）

```
结构共享示意（10 万行树，改 1 个 key）：

      before:                    after:
        ROOT(旧)                   ROOT'(新)
       /      \                   /      \
   N1(旧)     N2(旧)          N1(旧)     N3(新)   ← 只有含改动 key 的路径
  /    \                          /    \           是新对象；其余地址原样复用
L1(旧) L2(旧)                  L1(旧) L4(新)

新建 chunk ≈ 3 个（复用率 99.7%，prototype/prolly 实测）
```

## 2. 什么时候切分——weibull 分裂器

节点攒条目时，每追加一条就问一次"该切了吗"（`splitter.rs`）：

```
size = 当前缓冲字节数（自上个边界起）

size < 512            → 不切（避免碎块）
size ≥ 16384          → 强制切（防爆块）
否则：
  h = (xxh3_64(key) ^ seed(level)) & 0xFFFFFFFF     // key 的 32 位指纹
  p  = 1 − exp( −Δ[(size/4096)^4] )                 // ★ 条件风险增量
  h/2³² < p → 切分
```

两个精妙处（都是 Python 原型踩坑换来的）：

1. **条件风险而不是 CDF 阈值**。增量形式 `p_i = 1−exp(−Δ[(s/λ)^k])` 对字节
   精确差分后，∏(1−p_i) = exp(−(s/λ)^k)——节点尺寸**精确服从**
   Weibull(k=4, λ=4096)。若用朴素的 `h < CDF(size)`，分布系统性偏移。
2. **每层独立盐**：`seed(level) = SHA-512(level as u64 LE)[..8]`。同一批数据
   在叶层和上层产生独立边界，父子切分互不相关——这是"改一个 key 只扰动
   一条路径"的前提。

原型实测 vs 理论（10000 条目）：

```
        min     p50     mean    p99     max
实测    1026    3672    3724    6210    6318
理论    ——      3737    3713    6000    ——
```

> **v1 工程决策**：批量建树用上述分裂器；**增量更新走路径复制 CoW +
> 溢出按字节中点重分裂**（不是实时重切）。内容寻址/共享/diff 全保留，
> 点更新写放大 O(树高)。完整"实时重切+对齐"算法在 prototype 验证，v2 切换。

## 3. 节点二进制布局（磁盘格式，逐字段）

叶节点和内部节点**同构**——内部节点的 value 是"20B 子地址 + 8B 子树计数"：

```
PROLLY_NODE chunk（payload，LE；总长任意，尾 4B CRC）：

偏移      字段                    大小
──────────────────────────────────────────────────────────────
0        level                   u8      0=叶, 1..=内部层
1        count                   u32     条目数 N
5        key_lens[N]             u16×N   每 key 长度
5+2N     key_bytes[Σlen]         变长    key 顺序拼接
…        key_offs[N]             u32×N   在 key_bytes 内的偏移（二分用）
…        val_lens[N]             u32×N   每 value 长度
…        val_bytes[Σlen]         变长    value 顺序拼接
…        val_offs[N]             u32×N   在 val_bytes 内的偏移
…        addr_slots_len          u32     内嵌地址数 S（=N，内部层）
…        addr_slots[S]           u32×N   val_bytes 内 20B 子地址的偏移
                                          （GC 直接扫描，不解码值）
…        crc32c                  u32     覆盖以上全部
──────────────────────────────────────────────────────────────
value 内容：
  叶层     = 行编码字节（03 章）
  内部层   = [child_addr 20B][subtree_count u64 LE]    恰 28B
```

**为什么平行数组而不是交错 KV**：二分查找只碰 key 区（顺序读友好的紧凑段），
值区完全跳过；`key_offs` 让二分不做任何前缀解析。整节点解析零拷贝——
`Node` 只持 `Arc<Vec<u8>>`，字段访问是偏移量读数。

地址的组成：`addr = SHA-512( 0x01 ‖ payload )[..20]`——类型标签参与哈希
（03 章），因此树根地址即整棵树的加密校验和。

## 4. 批量建树（build）

```
items（有序 KV）─────────▶ splitter(level=0) 切叶 ──▶ 叶节点…
叶节点 entries (last_key, addr, count) ─▶ splitter(level=1) 切 ──▶ L1 节点…
…直到某层只剩 1 个节点 = 根

例：5000 行的树（均值 4KB/节点 ≈ 每节点 60~90 条目）
  L0: ~70 叶        L1: ~2 内部节点     L2: 1 根       树高 3
```

`subtree_count` 自底向上累计，供 OFFSET/统计的 O(log n) 序数定位
（`cursor::tree_count` 直接读根的子树计数和）。

## 5. 增量写路径（apply）——CoW 路径复制

`chunker::apply(root, mutations)`（mutations 按 key 有序，同 key 取最后）：

```
apply(根, [k100=UPDATE, k9999=INSERT, k5=DELETE])：

              … 递归下探到叶 …
  叶节点：有序归并(旧条目, 变更) ──▶ 新条目集
       │ 超过 16KB？──是──▶ 从中点分裂为 2 个叶
       ▼
  父节点：被触及的子槽位换成新地址；未触及子槽位**原样复用**
       │ 条目集尺寸 > 上限？──▶ 分裂
       ▼
  …直到根；根分裂则加一层；根只剩 1 个内部子则收缩一层

  全部新节点 = dirty chunks（本例 5~40 个，视树高与分裂）
```

要点：

- **未触及子树零复制**：父节点里 untouched child 的 (key, addr, count) 原样保留
- 叶 merge 的三种情形：插入（新 key）、更新（同 key 覆盖）、删除（直接略过，
  空叶从父层剔除）
- 单测断言：3000 行树改 3 个 key，dirty < 40 chunk（实际 ~10）

## 6. 读路径

```
点查 lookup(root, key)：            范围扫描 range_scan / TreeIter：
  node = 根                          TreeIter 持"根→叶"路径栈
  loop {                               next_item():
    i = node.lower_bound(key)             叶[idx] 取条目; idx++
    if i 越界 → None                      叶尽 → 上卷父链找未尽的
    内层 → addr = child[i] 继续           下探到下一个叶
    叶层 → key相等? Some(v) : None      seek(key): 路径构建时每层
  }                                      lower_bound(key) 定位
```

复杂度：树高 h（1 亿行 ≈ 4~5 层），点查 = h 次节点访问（缓存后为本地读）。
迭代器全程只持 Arc，读侧零锁（06 章 memtx 同款哲学）。

## 7. diff——地址即摘要的合并基建

```
diff(root_a, root_b)：双游标并行走树
  两个子节点 addr 相等？──是──▶ 整棵子树跳过（内容必然相同）
  叶层归并 → Change{key, old, new}
```

一万行的树，两分支各改 100 个不相交 key：diff 恰好产出 100+100 条 patch，
子树级跳过 48/49 次——这是 08 章三方合并 O(diff) 的来源。
