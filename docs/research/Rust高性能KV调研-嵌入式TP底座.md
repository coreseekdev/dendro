# Rust 高性能 KV 调研：嵌入式形态的 TP 底座（2026-09-19）

> 触发：A 选项（全功能嵌入）的痛点 = dendro 默认 in-memory TP——
> memtx 单版本实测 26.7× 载荷驻留（见《关键数据结构内存分析》），
> 嵌入模式内存过大。方向：TP 侧引入磁盘型 KV 底座，最好是
> **不可变/追加式**（与 dendro 的 CAS/append-only 哲学对齐）。

## 候选全景（2026-09 现状）

| KV | 模型 | 不可变程度 | 纯 Rust | 活跃度 | 嵌入足迹 | 关键特性 |
|----|------|-----------|---------|--------|----------|----------|
| **redb 4.x** | CoW B+tree（LMDB 式） | 页级 CoW（旧版本可存活的 MVCC 快照） | ✅ | 高（2026-04 4.1，AI 辅助优化 ~1.5×） | 小单文件 | ACID、非阻塞读、zero-copy 读；基准与 LMDB/RocksDB 同级 |
| **fjall 3.0** | LSM（SSTable 不可变 + compaction） | **SSTable 追加式不可变**（compaction 重写） | ✅ forbid(unsafe) | 高（2026-01 3.0：大数据集性能/内存改进、新盘格式、KV 分离 GC） | 分区目录 | 写密集/大数据集强；读放大需调参 |
| **sanakirja** | 持久化（函数式）CoW B+tree | **全持久**（版本跨事务存活——最接近"不可修改"） | ✅ | 低（Pijul 作者，随 Pijul 演进） | 单文件 | 快照/多版本是一等公民；生态小、性能口径少 |
| LMDB（heed 绑定） | CoW B+tree | 页级 CoW | C 库 + Rust 绑定 | 极高（30 年谱系） | 极小 | 多进程共享、单写多读零锁；非 Rust |
| sled | — | — | ✅ | **休眠**（社区共识弃用，bpfman 2024 公开迁移离开） | — | ❌ 新项目不选 |
| canopydb | B+tree | 页级 | ✅ | 新（2026 涌现） | — | 自述随机写不如 LSM——观察名单 |

## 与 dendro 的对齐分析

dendro TP 侧的既有结构其实已经是一个**不可变 KV**：prolly 树
（内容寻址、不可变节点）+ CAS。memtx 只是它的内存写缓冲。因此
真正的选项比"选哪个外部 KV"更根本：

### 选项 1：自有 prolly+CAS 直做 TP 底座（零新依赖）

memtx 降级为有界写缓冲（如 16MB），到线即 checkpoint 段化/树化
到 CAS——**磁盘为主、内存为缓存**。嵌入式足迹 = 页缓存大小（可配）。
- 优：架构一致（一切皆 CAS chunk）、分支/时间旅行免费、不可变
  哲学完全对齐；checkpoint 已有（10M 装载就是这条路径）
- 劣：TP 点查/短范围扫描走"树路径 + 对象读"，本地盘延迟 vs
  redb/fjall 的 mmap/块缓存有差距——**需 TP 读路径的页缓存层**
  （cache_budget_bytes 已有钩子）；写放大 = checkpoint 粒度
- 度量：现有 benchmark 管线可直接 A/B（memtx 有界 vs 全量）

### 选项 2：外部 KV 做 TP 行存（redb / fjall / sanakirja）

memtx 整层替换为 KV 表（table_id+key → 行字节），WAL/checkpoint
  语义重定义（外部 KV 的 durability 替代 WAL 的物化职能）。
- redb：读强 + CoW 快照与 dendro 分支概念同构（快照 = 分支基线）
- fjall：写密集场景（追加式 SSTable 与 CAS 追加哲学最像）
- sanakirja：**不可变语义最纯**（全持久版本），但活跃度/生态弱
- 劣：引入第二存储宇宙（CAS + KV 双事实源一致性）、WAL→KV 的
  双写或替代协议、分支语义映射（KV 无分支——回到 AP-only 分支）

### 选项 3：混合——自有 prolly 为主，redb 做热缓冲（观察）

不建议先做：两套系统的复杂度在嵌入场景收益不明确。

## 判定

1. **首选选项 1**（自有 prolly 直做底座 + 有界 memtx + TP 页缓存）：
   零依赖、哲学对齐、分支完整；先做 **TP 读路径页缓存 + memtx
   有界化** 的 A/B（现有 1M 管线 + memstruct 探针直接度量）。
2. 外部 KV 中若需引入：**redb**（读密集嵌入、CoW 快照同构）或
   **fjall**（写密集、追加式最像我们）二选一，**不做双后端抽象**
   （社区共识：抽象层增复杂度无用户收益）。sanakirja 语义最合但
   活跃度不足，列观察名单。
3. sled 排除（休眠）；LMDB/heed 作为多进程共享场景的备选。

## 信源

- Fjall 3.0（2026-01）：https://fjall-rs.github.io
- redb（基准/4.1）：https://redb.org 、
  https://github.com/cberner/redb
- Sanakirja（不可变 CoW 设计反思）：https://pijul.org
- 社区选型共识（2026-01，users.rust-lang.org）：
  redb（读/简单）vs fjall（写/大数据），勿做双后端
- sled 休眠与迁移案例（bpfman）：https://github.com/spacejam/sled
