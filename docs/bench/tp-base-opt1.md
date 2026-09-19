# opt1-prolly-tp-base：prolly 树作 TP 底座（分支工作档案）

> 决策来源：docs/research/Rust高性能KV调研-嵌入式TP底座.md 选项 1。
> 分支：opt1-prolly-tp-base（自 master@f15f390）。

## 落地构件

1. **节点页缓存字节预算化**：NodeStore 从硬编码 4096 节点（≈16MB
   恒定，cache_budget_bytes 形同虚设）升级为字节预算 LRU（预算/分片，
   Arc 共享去重计账，超界逐最久未用）；`cache_budget_bytes` 真接线；
   census 三件套（hits/misses/驻留字节）进 memprof。
2. **有界 memtx**：提交路径越 `checkpoint_threshold_bytes` 即时踢醒
   检查点线程（Condvar；轮询退化为兜底——此前 30s 轮询窗口内写入
   风暴无界）。内存上界 = 阈值 + 检查点进行期在途。
3. **`DbOptions::embedded()` 预设**：8MB 写缓冲 + 64MB 页缓存 +
   紧凑资源上界（连接 16 / 分支 64 / txn 32MB / 游标 8MB）。
4. **DML pk 下推**（TP 基准暴露的既有缺陷）：UPDATE/DELETE 的单列
   pk 等值/IN 谓词此前走全表扫描 + 全物化（1M 行实测 ~340ms/条），
   现复用点查路径（memtx ∪ 树 + 墓碑判定 + 事务自身写）。
5. **TP 基准**（`dendro tp-bench`）：装载→物化→点查/短范围/随机写
   × {default, embedded} 双臂，median of 5×1s + RSS + memprof。

## A/B 实测（1M 行，checkpoint 后树驻留）

| 场景 | embedded | default（memtx 惯性） |
|------|----------|----------------------|
| point（pk 等值） | 28.4k ops/s | 28.1k ops/s |
| range100 | 11.0k ops/s | 11.2k ops/s |
| write（随机 UPDATE） | **29.8k ops/s**（修复前 2.9） | 31.1k ops/s |
| 终局 RSS | 0.2GB | 0.2GB |

结论：**树底座的 TP 与 memtx 惯性形态持平**（页缓存命中后点查/写
均 ~30k ops/s），embedded 预设提供内存上界（缓冲 8MB + 页缓存
64MB 可配）——选项 1 的可行性得到数据背书。DML 下推带来写路径
**10000×**（340ms → 0.03ms/条，既有缺陷修复）。

## 测试锁定

- `tp_base.rs`：DML 下推正确性（等值/IN/非 pk 回落/混合谓词/DELETE/
  不存在键 affected=0）+ 有界 memtx（40 轮 burst 阈值断言 + 显式
  CHECKPOINT 归零）；全局 meter 并行互扰以串行锁防护。
- 624/624（+2 新测试）。

## 后续（分支外）

- 10M/更大表的页缓存命中率曲线（census 已可观测）
- KV 调研中外部后端（redb/fjall）的对照臂——仅在树底座出现短板时
- memtx 26.7× 结构开销的 P0（pending×memtx Arc 共享）仍独立有效
