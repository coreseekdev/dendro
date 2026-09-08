# TASK.md — 待办任务清单

> 来源：架构评审（首轮 + 第二轮，`docs/discussions/`）+ SOTA 调研修订路线。
> 状态标记：⬜ 待做 · 🔧 进行中 · ✅ 已完成
> 优先级：P0（正确性）> P1（完整性）> P2（质量）> P3（远期）
> **完成三要件**（见 discussions/README.md）：代码 + 回归测试 + 文档同步，同一提交；完成必须附证据。

---

## P0 — 正确性

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| ~~P0-1~~ | ~~WAL flush 失败丢帧 + 挂死~~ | ✅ | `92dcb35`/`f1bca77`：restore+retry；本提交收尾——计数器"成功清零/失败归还"、Bytes 去全量克隆、TRAILER_LEN 常量；**测试捕获真 bug：段号空洞**（失败也推进 cur_seg → probe_tail 连续性假设丢数据），已改为成功后才推进。回归：`tests/wal_corruption.rs::{flush_put_failure_no_frame_loss_no_hang, transient_flush_failure_self_heals}` |
| ~~P0-2~~ | ~~恢复路径对坏数据 panic~~ | ✅ | `92dcb35` FrameIter len 守卫 + underflow 守卫；回归：`tests/wal_corruption.rs`（坏 len / 坏 CRC / 截断 payload / 截断帧头 / ≤32B 撕尾容忍 / e2e open fail-fast），`decode_row` 定宽守卫随 `68de926` |
| ~~P0-3~~ | ~~列存旧段删除时序（崩溃窗口）~~ | ✅ | `68de926` 止血（删除推迟）；真删除时序已随 P1-4 GC 落地（墓碑随 manifest 原子发布，见 `docs/design/GC定案.md`）|
| ~~P0-4~~ | ~~P1 fencing 文档诚实化~~ | ✅ | `92dcb35` 设计文档/multi_node 头；本提交：fence.rs 模块注释随**运行时拒写实现**改写（不再是注释先行） |
| ~~P0-5~~ | ~~GitHub Actions CI~~ | ✅ | `53f2c74`；`f1bca77` 起 clippy 非阻塞过渡；本提交 clippy --all-targets 清零，恢复 `-D warnings` |

## 二轮评审收尾（2026-09-07 晚）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R2-1 | clippy 清零（107→0，含 --all-targets） | ✅ | 本提交；`cargo clippy --workspace --all-targets -- -D warnings` 绿 |
| R2-2 | S2 收尾（25P02 拒绝失败事务内语句） | ✅ | `92dcb35`/`f1bca77`：`sql/mod.rs` exec_batch 入口检查 |
| R2-3 | S6（WITH → 0A000，不再静默丢弃） | ✅ | `92dcb35`：`sql/mod.rs` `Statement::Query` with 检查 |
| R2-4 | S4 SPEC 07 偏离记录（整数升宽 Int64 / 溢出 22003） | ✅ | 本提交：`spec/07-sql-surface.md` §1 已知偏离段 |
| R2-5 | P0 配套回归测试（此前零交付） | ✅ | 本提交：`tests/wal_corruption.rs` 10 测试（含 flush 失败注入）+ `tests/multi_node.rs` 2 个 fencing 新测试 |

## P1 — 完整性

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| ~~P1-2~~ | ~~fencing 运行时拒写~~ | ✅ | 本提交：`engine.rs::Branch::fence_gate`——三个写入口（commit_tx / write_branch_commit / checkpoint_branch）在 commit_mu 内检查；过期 → 40001。回归：`tests/multi_node.rs::{fence_expired_writer_rejected, fence_renew_keeps_healthy_writer_writing}` |
| ~~P1-3~~ | ~~fencing 续期~~ | ✅ | 本提交：惰性续期（commit 路径，每 ttl/3 ≤1 次 PUT，失败仅告警下次重试；无后台线程——文档口径已同步） |
| ~~P1-1~~ | ~~只读打开模式~~ | ✅ | 本提交：`DbOptions.read_only` + `serve --read-only`。不领 epoch（零 fence 对象）、不起 WAL writer、空存储拒绝打开；写路径经 fence_gate 拒绝（25006）。回归：`tests/multi_node.rs::{read_only_open_does_not_pollute_epoch_sequence, read_only_open_missing_store_errors}` |
| ~~P1-4~~ | ~~GC：旧段删除时序定案 + manifest 旧版本 + WAL 段回收~~ | ✅ | 本提交：墓碑随 manifest 原子发布（P0-3 正式修复）+ `gc_retention_ms` 保留窗口 + `gc_sweep`（checkpoint 尾部/打库各一次，单批 ≤256）；WAL 旧 epoch 目录与当前 epoch 前缀段回收，`BranchHead.wal_first_seg` 保证恢复容忍前缀空洞；manifest 旧版本保留 16。CAS chunk GC 明确划入 v2（`docs/design/GC定案.md` §4.4）。回归：`crates/dendro-server/tests/gc.rs`（3 测试） |
| ~~P1-5~~ | ~~SQL 语义修复（S1–S6）~~ | ✅ | `53f2c74`/`92dcb35`/`f1bca77`；S4 偏离记录见上 |
| ~~P1-6~~ | ~~JOIN/派生表测试（hash_join 零覆盖）~~ | ✅ | 本提交：`tests/slt/dendro/008_join.slt`（INNER/LEFT/NULL 键/一对多/三表链/复合键/JOIN+GROUP BY/派生表）。语料当场暴露真 bug：sqlparser 0.62 把裸 `JOIN`(Join) 与 `INNER JOIN`(Inner) 分为不同枚举——标准写法 `A JOIN B` 直接报 not_supported，hash_join 此前经由该路径**不可达**。已修（scan.rs eval_from 匹配 Join/Inner、Left/LeftOuter） |
| P1-7 | 多线程 OCC 并发测试 | ⬜ | `tests/concurrent.rs` |
| P1-8 | 真 kill 崩溃恢复测试（子进程 SIGKILL） | ⬜ | 替代 `drop(db)` 模拟 |
| ~~P1-9~~ | ~~fencing 安全性质测试~~ | ✅ | 本提交：`fence_expired_writer_rejected` 即评审要的"旧实例写被拒"断言 |
| P1-10 | time travel SQL 入口（`AS OF` / `FOR SYSTEM_TIME`） | ⬜ | 数据层已支持，缺 SQL 面 |
| ~~P1-11~~ | ~~`/metrics` `/readyz` 端点~~ | ✅ | 本提交：`dendro-server/src/metrics.rs`（serve `--metrics-port`，默认 9469）。/metrics 暴露每驻留分支 pending_bytes（扩容信号）/watermark/durable 水位/lease 剩余 TTL；`Database::active_branches()` 只读快照，绝不懒加载分支。回归：`tests/metrics_endpoint.rs` |

## P2 — 质量 / 性能 / 证据链

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| P2-1 | slt 语料扩展（接入 sqllogictest-corpus，哪怕 1%） | ⬜ | `tests/slt/` |
| P2-2 | AP 向量化执行器（或修订 SPEC 措辞为"行式解释器"） | ⬜ | SPEC 00 G4 |
| P2-3 | 基准证据链整改（环境指纹/中位数/恢复率断言/README CI 生成） | ⬜ | `benches/results/` |
| P2-4 | 共识选型文档修订（消除正文与决策的矛盾） | ⬜ | 决策理由已补，正文需同步 |
| P2-5 | 死代码清理 | 🔧 | clippy 清零已带走大部分；余 `retry()`/`RootView` 等 |
| P2-6 | 性能：`Node.key()` 去分配 / NodeStore 真正 LRU / commit_mu 与 flush 解耦 | ⬜ | 热路径优化 |
| P2-7 | GRAMMAR.md 修正（WITH ⬜、CHECKPOINT ✅、differential 目录删除） | ⬜ | 与代码对齐 |
| P2-8 | AGENTS.md 架构清单补 kv/journal/consensus/fence | ⬜ | 文档同步 |

## 四轮评审修复（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R4-P0D | WAL 错误语义定案：**写者毒化**（append 拒 40003 / flush 停 / reopen 恢复） | ✅ | `wal.rs`（poisoned 标志 + append/await_durable/flush_now 三处检查）；`wal_corruption::{wal_failure_poisons_writer_until_reopen, transient_flush_failure_self_heals_via_reopen}`（毒化即时生效、无幽灵行、reopen 自愈） |
| R4-P1E | put_batch 无界并发（160 chunk 峰值 160 并发 PUT） | ✅ | 逐组 join，并发上界 PAR=8 + 首错早停；`cas.rs::put_batch` |
| R4-P1F | load_latest 常态 LIST（探测死重）+ 三处文档反向 | ✅ | 删探测循环，LIST 权威（模块 doc/函数注释/SPEC 01 §5 同步）；update_manifest 自发布 + 无变更路径也采纳缓存 |
| R4-P2B | gc_last_sweep_ver 独立发布使版本推进 ×2 | ✅ | 并入墓碑压缩发布（无压缩不发布）；`engine.rs::gc_sweep` |
| R4-P2 | LeaseKeeper 续期 PUT 持锁（RTT 阻塞提交/readyz/metrics） | ✅ | 锁内 clone → 锁外 PUT → 写回取 max 防倒退；`engine.rs::LeaseKeeper::renew_if_due` |
| R4-杂 | 缩进×2 / SPEC 01 §7 标题 / metrics read_only HELP / write_branch_commit in-doubt 注释 / s3_cloud 显式 SKIPPED / **PG cleartext 认证接线** | ✅ | pgwire `serve_with_config` + main.rs 传入 password（trust/cleartext，SPEC 10 §8） |

## 五轮评审修复（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R5-P1 | 毒化无可达恢复路径（keepalive 续租堵死接管 + 无 reopen 入口） | ✅ | flush_loop 保活移到毒化检查后（毒化分支租约自然过期可接管）；`engine.rs::reopen_branch`（驱逐旧 writer + 重新领 epoch + 恢复回放） |
| R5-P1 | backup 撕裂写 × 幂等固化截断 | ✅ | tmp+rename 原子拷贝 + 尺寸不符重拷守卫；`backup.rs` |
| R5-P2 | NoWait × 毒化 ack 后静默丢失 | ✅ | 契约显式化：SPEC 02 §3.5 + `Durability::NoWait` 文档 |
| R5-P2 | 40003 MySQL 退化 HY000 | ✅ | `map_state("40003") => (1105, "40003")` sql_state 透传 + 单测 |
| R5-P2 | backup 失败退出码 0 | ✅ | 失败 `exit(1)` |
| R5-P2 | 毒化语义零 SPEC 化 | ✅ | 新增 SPEC 02 §3.5（40003/毒化/保活停止/reopen/Uncertain 对账/NoWait/双协议映射） |
| R5-P2 | cached 零读者死状态 + "cached 不再只写"表述失实 | ✅ | 字段与 adopt() 删除；空洞测试改写为 `list_authoritative_reads_survive_gc_holes`；注释修正；本回应 §3 更正 |

## 六轮评审修复（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R6-P0 | **回归（研发自引入）**：第五轮修复把 flush_loop 保活调用整体删除而非移位——空闲超过 TTL 即永久 40001，第三轮修复被静默撤销，196 测试全绿放行（无空闲场景回归） | ✅ | keepalive 恢复至毒化检查之后 + `multi_node::idle_writer_stays_writable` 防线（2.5×TTL 空闲后必须可写）；回应文件含根因分析 |
| R6-P1 | reopen_branch 零调用方（40003 指引的操作对 SQL 客户端不存在） | ✅ | `REOPEN BRANCH [name]` 语句（缺省当前分支）；回归 `reopen_branch_sql_recovers_poisoned_writer` |
| R6-P1 | reopen_branch 并发边界（双 epoch / 孤儿写者 / 驱逐健康写者 / 42P01 窗口） | ✅ | 每-名字打开互斥（branch 创建全程持锁，双检）+ 仅允许驱逐毒化写者（poisoned() 访问器投入使用）+ 驱逐前 commit_mu 清空在途 |
| R6-P1 | 毒化分支上 commit 路径 fence_gate 仍续租 | ✅ | fence_gate 毒化快速失败（40003）且不续租；SPEC 02 §3.5 机制描述修正（接管本不依赖过期，停止续租的意义是诚实/审计） |
| R6-P2 | backup 撕裂守卫零回归覆盖（"由 roundtrip 覆盖"为虚假声称） | ✅ | `backup_repairs_truncated_files`（预置截断文件 → 重备修复 → 恢复可开） |
| R6-P2 | SPEC 02 "§4.1" 幻影引用 ×4 + §3.5 疑问句未定稿 | ✅ | 全部改指 §3.5；疑问句改为对账语义定案（主键覆盖天然幂等 + 业务键提示） |
| R6-P2 | 毒化无 metric | ✅ | `dendro_branch_poisoned` gauge + HELP/TYPE |

### 全新扫描登记（第六轮 §新视野，此前未覆盖面）

| # | 任务 | 优先级 | 备注 |
|---|------|:----:|------|
| S-1 | 启动全量打开所有分支（每分支线程+租约+fence 写）——万级分支不可行 | P1 | 惰性打开（会话/查询触达时 branch()）+ load_open_branches 仅恢复有租约分支 |
| S-2 | DROP BRANCH 永久泄漏该分支 WAL/fence 对象 | P1 | 删除分支时墓碑化其全部对象（复用 GC 机制） |
| S-3 | 资源上界：连接数 / 分支数 / 单事务大小无守卫 | P2 | 配置上限 + 超限错误码 |
| S-4 | SQL 面缺口：UNION / 视图 / 权限（GRANT/REVOKE） | P2 | GRAMMAR 已列 ⬜ |
| M-5 | metrics 直方图（commit/flush/manifest 延迟） | P2 | 未动（前轮登记） |
| M-4 | GPU/CBF 解码对拍 | P2 | 未动（前轮登记） |
| M-3 | G6 压缩量化衰退 | P2 | 未动（前轮登记） |

## 七轮评审修复（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R7-P0 | checkpoint 后 UPDATE/DELETE 读路径失真（UPDATE 双行 / DELETE 复活 / 点查墓碑回退树 / 空表 count 0 行） | ✅ | 归并预刷 `<=`→`<` + 等键分支输出 overlay 值；点查 latest_ts 墓碑判定；全局聚合空输入一行。语料 `009_checkpoint_visibility.slt`（checkpoint→变更→查询 组合首次入 corpus）+ INSERT 重插墓碑键墓碑感知（ddl 23505） |
| R7-P1 | 只读副本可执行 DROP BRANCH（manifest CAS 在副本成功） | ✅ | update_manifest 单一咽喉 25006 守卫 + RO writer append 25006（DDL 的 WAL 帧先于 manifest）；回归 `r7_2_read_only_rejects_catalog_writes` |
| R7-P1 | 显式事务隔离混合（overlay 按 BEGIN 快照、树按当前 head） | ✅ | BEGIN 冻结 catalog 根（Txn.head_root）；table_scan/点查下推以冻结根解析；回归 `r7_3_explicit_txn_read_visibility_frozen` |
| R7-P2 | has_agg_expr 不递归 Cast（count(*)::text 报错） | ✅ | Cast 分支补齐（与 collect_agg_calls 对称） |
| R7-P2 | SPEC 02 §4.1 幻影引用 ×4 | ✅ | 全部改指 §3.5 |

### 第七轮登记（读路径与产品面——前六轮盲区）

| # | 任务 | 优先级 | 备注 |
|---|------|:----:|------|
| Q-1 | 读路径内存上界 + LIMIT 下推 + 游标（DECLARE CURSOR/FETCH；执行器全程物化、LIMIT 全扫后截断） | P1 | 与 S-3 同族但独立列；"用户 90% 交互是读" |
| Q-2 | 跨进程 DROP 不驱逐外部写者（僵尸写者 ack + 对象泄漏 + 高 epoch 复活） | P2 | 方向：DROP 领新 epoch + fence_gate 校验租约存在性/manifest ref |
| Q-3 | 启动 O(分支数) HEAD 校验 → 抽样/懒校验；NoWait × DROP ack 丢失文档化 | P2 | |
| Q-4 | 二级索引缺失作为产品决策入册（非 pk 谓词恒全表扫） | P2 | SPEC 07 明示 v1 无二级索引或立任务 |
| Q-5 | SET 参数静默 OK 无效果（isolation/timezone 假象） | P2 | 至少返回 not_supported 或真实生效 |
| Q-6 | pseudo_tables 恒读 main 分支（非 main 会话 information_schema 显示错表） | P2 | |
| Q-7 | stop_cp 死字段（无写者） | P3 | Database::close 收口时处理 |
| ~~Q-8~~ | ~~P1-7 并发测试首个用例~~ | ✅ | r7_3 单线程版已有；**P1-7 真并发已交付**（`tests/concurrent.rs`：Barrier 同快照 8 线程同键恰一赢家 40001、异键全成、autocommit 40001-or-win） |

## 八轮评审修复（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R8-P1 | 显式事务不读自己的写（SQL 读路径从不合并 sess.txn.writes） | ✅ | table_scan 可见性归并**收敛为单一抽象**（树→overlay→会话事务写 三层 map 覆盖——评审建议的结构性方案，键序错误无处可写）+ 点查路径自身写覆盖。回归 `sql_semantics::r8_1_explicit_txn_reads_own_writes`（INSERT/DELETE/UPDATE/COMMIT 全链） |
| R8-P1 | 服务端日志为零（17 处 tracing 无 subscriber） | ✅ | main 入口 tracing_subscriber fmt + EnvFilter（RUST_LOG 可调，默认 info） |
| R8-P2 | 监听器 bind 失败照常 ready 且永不退出 | ✅ | 全部 handle join，任一失败 exit(1)；此前 join 顺序 + panic 吞噬 |
| R8-P2 | OCC 冲突检测窗口止于 checkpoint | ✅（定案） | 跨越 checkpoint 的显式事务提交**显式 40001**（盲区拒绝，客户端重试获得完整视图）——静默丢失更新消除。`Branch.covered_min` + commit_tx 检查；回归 `q9_txn_spanning_checkpoint_rejected_not_silent`。Q-9 保留"validate 回退树版本链"为 v2 增强 |
| R8-P2 | 事务内 DDL 立即生效且 ROLLBACK 不撤销 | ⬜ 入册 Q-10 | 需 catalog 写集事务化（v2） |
| R8-P2 | BEGIN 后 USE BRANCH 不设防 | ✅ | 事务内 USE → 25001；回归 `q11_use_branch_inside_txn_rejected` |

### 第八轮登记

| # | 任务 | 优先级 | 备注 |
|---|------|:----:|------|
| Q-9 | OCC 冲突检测窗口越过 checkpoint（30s 必然发生 → 40001 失效） | P1 | 修法：validate 回退树版本链 vs truncate 保留窗口 |
| Q-10 | 事务内 DDL 事务化（catalog 写集进 Txn，ROLLBACK 可撤销） | P2 | |
| Q-11 | 显式事务内 USE BRANCH 拒绝（25001） | P2 | 一行守卫 |
| Q-12 | 优雅关闭（SIGTERM → 停监听 → flush → exit 0）+ /readyz live/readiness 分离 | P2 | k8s 终止语义 |

## 九轮评审修复（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R9-P0 | 冻结读被 checkpoint 截断击穿（BEGIN 前提交的行在事务内静默消失） | ✅ | 截断水位尊重活跃快照：Branch.active_snaps 注册表（BEGIN 注册 / COMMIT/ROLLBACK/Drop 注销），存在活跃快照即不截断（保守 v1 口径；精细化保留列 Q-13）。回归 `sql_semantics::r9_1_frozen_reads_survive_checkpoint` |
| R9-P1 | 事务内 CHECKPOINT/CREATE/DROP/MERGE/REOPEN 自伤（提交时刻撞 Q-9 40001） | ✅ | exec_branch_statement 入口 25001 拒绝；回归 `r9_2_checkpoint_and_branch_ddl_rejected_inside_txn`（PG aborted 语义逐事务验证） |
| R9-P1 | R8-6 修复默认配置无效（bind 在线程内 + join 首位永不返回） | ✅ | 全部监听器**前置 bind**（任一冲突 exit(1) 且不打印 ready）+ pgwire/mywire/kv_resp 增 serve_listener/bind_on 变体；端到端验证：占用端口的第二实例 exit=1 |
| R9-P2 | KV 层双洞（事务内 GET 返回整行编码 / SCAN 不合并写集） | ✅ | GET 解码取值列；SCAN 增加写集层（最后覆盖，删除生效）。回归 `kv_wire::kv_txn_reads_own_writes_and_decoded_values` |

### 第九轮登记

| # | 任务 | 优先级 | 备注 |
|---|------|:----:|------|
| Q-13 | memtx 版本保留精细化（活跃快照存在即全量保留 → 内存随最老事务增长） | P2 | 按版本/per-key 保留；与 Q-9 的 40001 口径联动 |
| Q-14 | AP 列存路径（≥1 万行）读自己的写 + 冻结根（R9-3，本轮未完成） | P1 | 显式事务内大表 AP 查询的可见性与行路径不一致 |
| Q-15 | SPEC 03/04 + tutorial 同步 Q-9/Q-11/毒化语义（DoD #3） | P2 | |
| Q-16 | 事务内重复 INSERT 同一新键 → 23505（当前静默覆盖） | P3 | 十五轮确认不阻 G5 |
| Q-17 | slt runner 显式设置 checkpoint_interval_s=0（避免环境泄漏） | P3 | |

## 十轮评审修复（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R10-P0 | 活跃快照只在 Session::drop 注销（COMMIT/ROLLBACK 缺失）→ 截断永久跳过 / memtx 无界 | ✅ | COMMIT/ROLLBACK/failed-COMMIT 三路径注销（键引用计数递减）；回归 `kv_wire.rs::kv_txn_reads_own_writes_and_decoded_values`（值解码/SCAN 写集；注册表断言在 `kv_txn_frozen_reads_and_registry_lifecycle`）（断言 COMMIT 后注册表空）+ `q9/q11/r9_x` 全组仍绿 |
| R10-P0 | 前置 bind 装错端口（FIFO 顺序与消费序错位 → PG/MySQL 端口互换） | ✅ | listeners 改 **HashMap 按名存取**；进程级验证：PG 端口回 AuthenticationOk('R')，MySQL 端口回 8.0.36 握手 |
| R10-P1 | BTreeSet 去重：同 watermark 双事务共占一槽，先结束者连带摘除他人保护 | ✅ | `active_snaps` 改 `BTreeMap<u64, usize>` 引用计数（BEGIN +1 / 结束 -1 / 归零摘除） |
| R10-P1 | KV 显式事务游离于 R7-3/R9-1 机制外（读不冻结、写照拒） | ✅ | Kv::begin 注册 + 冻结根；commit/rollback/Drop 注销；kv_entry 以冻结根解析。回归 `kv_txn_frozen_reads_and_registry_lifecycle` |
| R10-P1 | clippy 门槛失守（main.rs 两处 unused） | ✅ | 已修；`cargo clippy --workspace --all-targets -- -D warnings` 零输出 |
| R10-P2 | R9-2 守卫误伤只读 SHOW BRANCHES | ✅ | Show 放行；其余分支语句仍 25001 |
| R10-P2 | 截断跳过时 covered_min 照进（保留窗口内提交被误拒 40001 且消息失实） | ✅ | covered_min 仅在**真截断**时推进；Q-9 的 40001 现在只出现在真盲区（语义更准：盲区外的跨 checkpoint 事务可正常提交，冲突由完整 memtx 历史检测） |

### 第十轮登记

| # | 任务 | 优先级 | 备注 |
|---|------|:----:|------|
| ~~Q-14~~ | ~~AP 列存路径读自己的写 + 冻结根~~ | ✅（证据勘误 R13-1） | 归并抽象补齐 AP 路径（冻结 catalog 根 + 第三层会话事务写覆盖）。**R13-1 教训**：q14 初版在 core 侧且未 set_columnar——AP 短路全程走行路径的空转测试（评审探针：还原修复仍绿）。真实化：`dendro-server/tests/ap_txn.rs`（set_columnar + col_rows≥1 万断言 + 事务矩阵）|
| ~~Q-18~~ | ~~装配层测试~~ | ✅ | `tests/assembly.rs`：spawn 真实 dendro 进程，断言**端口↔协议对应**（PG Startup→'R'、MySQL 握手、RESP PING→+PONG、readyz 200），随 CI 跑 |
| Q-19 | "线程不持强 Arc 睡觉/自环"红线入 AGENTS.md（Weak 化三连的通用化） | P2 | |
| Q-20 | ~~kv use_branch 静默丢事务~~ ✅（第十一轮收口，25001）∥ R9-9 runner 多语句比对 ∥ R9-11 协议小项 | P3 | 剩余两项保留 |

## 十一轮收口（2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R11-1 | SHOW BRANCHES 事务内放行（第十轮声称已做实际未落地——replace 静默失败，评审探针实锤） | ✅ | 守卫排除 Show + 回归 `sql_semantics::r10_show_branches_allowed_and_snapshot_lifecycle`（含 R10-1/R10-3 的 SQL 侧常驻回归：COMMIT 注销 + 引用计数） |
| R11-2 | Kv::use_branch 事务内静默丢事务且不注销（R10-4 后升级为注册表泄漏） | ✅ | 25001 拒绝（与 SQL 侧 Q-11 同口径）；回归 `kv_wire::kv_use_branch_inside_txn_rejected` |
| R11-3 | Q-18 装配层测试固化 | ✅ | `tests/assembly.rs`（进程级端口↔协议对应，见 Q-18 行） |
| R11-4 | 记账勘误（README 7/7→9/9、基线计数、kv.rs SPEC 11 缺号标注、M-1/M-2 交付入账） | ✅ | 本提交 |

## 十二轮收尾复评（2026-09-08）——**主干正确性防线收敛：终局确认**

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R12-1 | TASK.md 幽灵测试名（勘误批自身漏项）+ 聚合计数脚本承诺落地 | ✅ | 测试名更正；`scripts/count-tests.sh`（回应基线从此脚本化） |
| R12-2 | 守卫注释引用错误测试名（sql/mod.rs:260） | ✅ | 更正为 `r10_show_branches_allowed_and_snapshot_lifecycle` |
| R12-3 | assembly.rs blanket allow（防守性豁免即门槛漏洞） | ✅ | 删除；clippy -D warnings 仍零输出 |
| R12-4 | assembly readiness 判定弱于注释 + 空响应越界 | ✅ | 轮询真判 HTTP/1.1 200；空响应先断言 |
| R12-5 | 默认组合（kv=0 三端口）未自动化 | ✅ | `assembly_default_combo_three_ports`（README 快速开始路径） |
| R12-6 | 两条探针转正（REOPEN-in-txn / use_branch 成功路径隔离） | ✅ | r9_2 用例清单 + kv_wire 隔离测试（顺带勘误：use_branch 不自动建分支） |

**G1–G7 终评（第十二轮）**：G1 ✅（Q-18 入 CI 条件满足）、G2 ✅、G3–G7 🟡
（残余全部第二阶段）。

## 评审机制（第十二轮收尾建议，已采纳）

- **停止固定轮次**，转"里程碑聚焦复评 + 例外专项"双触发：
  - 里程碑复评：Q-14 + P1-7 完成后的轻量复评（第 13 轮预约）；
  - 例外专项：任何 P0/P1 级发现随时发起。
- **DoD 增补三条**（第十二轮 §4.4）：
  - 第 4 条：证据引用一律以 grep 命中为准（杜绝虚构测试名）；
  - 第 5 条：基线计数以 `scripts/count-tests.sh` 为准；
  - 第 6 条：代码级批量编辑必须带 assert（replace 静默失败三次的教训）。

## P1-7 真并发（第二阶段，2026-09-08）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| ~~P1-7~~ | ~~真并发 OCC~~ | ✅ | `tests/concurrent.rs`：①同键 8 线程真 Barrier 同快照并发提交 → **恰一赢家 + 7×40001**（first-committer-wins）；②异键并发全成；③autocommit 同键 40001-or-win 不变式。1.2 万行 AP 路径事务矩阵见 q14 |

## 十三轮轻量复评修复（2026-09-08，里程碑：Q-14 + P1-7）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R13-1（P1） | q14 回归空转（未 set_columnar → AP 短路 → 全程行路径，"还原修复仍绿"） | ✅ | 移至 `dendro-server/tests/ap_txn.rs`（列存接线 + col_rows≥1 万门断言 + 事务矩阵全链） |
| R13-2（P3） | commit message 基线计数失准（215 实为 218） | ✅ 勘误 | 已成文的 DoD 第 5 条（脚本计数）为本类问题的永久防线；历史提交不改 |
| R13-3（P3） | concurrent.rs 重新引入 blanket allow（P12-3 反模式回归） | ✅ | 已删；clippy -D warnings 零输出 |
| R13-4（P3） | README "SQL 基线（7 文件）" 勘误漏项 | ✅ | 9 文件 |
| R13-5（P3） | P1-7 用例①的 COMMIT 由主线程串行发出（"并发提交"表述过强） | ✅ | COMMIT 移入线程（barrier 后真并发）；不变式不变 |

## 十四轮轻量复评修复（2026-09-08，例外触发：R13-1 验收变异失败）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R14-1（P1） | q14 的断言全被 pk 下推截走（走行路径 R8-1），AP 归并层③禁用后测试仍绿 | ✅ | 补两条**非 pk 断言**（`WHERE v='upd'`→1、`WHERE v='v6'`→0，评审配方）+ **变异自检通过**（禁用 AP 层③ → 测试红在目标断言；还原 → 绿） |
| R14-2（P3） | ap_txn.rs 新建时复制 blanket allow（复制模板反模式第三次）；全仓 13 个测试文件带豁免 | ✅ | **一次性清扫**全部测试文件 blanket allow + clippy 修正（useless format/strip_prefix/let-and-return 等）——测试代码自此纳入 clippy -D warnings 门槛 |

## 十五轮轻量复评修复（2026-09-08，触发：验证 R14 + G5 终审）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R15-1 | **G5 终判 ✅**（Q-14 证据门槛经评审独立变异复跑成立；Q-16/Q-5 不阻门） | ✅ | 第十五轮轻量复评报告 |
| R15-2 | R15-1（P3）：src 侧 7 个 crate 级 blanket allow——clippy 门槛只罩测试不罩生产代码 | ✅ | 一次性移除 + 清偿 37 条警告（机械项 auto-fix；merge.rs 8 参数定点豁免并注明 v2 收敛方向）；`cargo clippy --workspace --all-targets -- -D warnings` 零输出 |

### Q-12 系列余项（第十/十六轮登记转正为独立行）

| # | 任务 | 优先级 | 备注 |
|---|------|:----:|------|
| Q-12b | 停监听（SIGTERM 后先关 listener 再 flush）+ /readyz live/readiness 分离 | P2 | 现状：直接 exit(0)，监听由进程退出回收（可接受但非最优） |
| Q-12c | shutdown join checkpoint 线程（Weak 化后 upgrade 窗口外 join 可行） | P3 | 观察项：manifest 原子性使风险低 |

### Q-12 优雅关闭（2026-09-08，第二阶段交付）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| Q-12 | 优雅关闭：SIGTERM/SIGINT → `Database::shutdown`（停 checkpoint 线程 + 全驻留分支 WAL `close_graceful`）→ exit(0) | ✅ | `wal.rs::close_graceful`（毒化时跳过上传——错误语义保留）+ `engine.rs::shutdown` + main 信号线程（signal-hook）。回归 `wal_corruption::close_graceful_flushes_no_wait_tail`（NoWait 缓冲尾经优雅关闭持久）|

## 十六轮里程碑复评修复（2026-09-08，触发：R13–R15 + Q-12 里程碑）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| R16-1（P1） | Q-12 回归空转（用 Group 持久级——exec 返回前缓冲必空，"NoWait 缓冲尾"从未执行；评审变异 3/3 仍绿） | ✅ | 测试改 NoWait 持久级（缓冲真实存在）+ **变异自检**（禁用 close_graceful 的 flush → 红；还原 → 绿）。`wal_corruption::close_graceful_flushes_no_wait_tail` |
| R16-2（P3） | Q-12 注册范围静默收窄 | ✅ 登记 | Q-12b：停监听 + live/readiness 分离（见下 Q-12b 行）；"Q-12e"为笔误 |
| R16-3（P3） | shutdown 对 checkpoint 线程只置标志不 join | ✅ 登记 | Q-12c（见下） |
| ~~P1-8~~ | ~~真崩溃（SIGKILL 子进程）~~ | ✅ | `dendro-server/tests/crash.rs`：spawn 真实 serve 进程 → PG 线协议写入（半 checkpoint 半 WAL）→ **SIGKILL** → 重启 → ack 数据全可见 + 可继续写。G3 的真崩溃缺口关闭 |

## P2' — 提交管线（2026-09-08 起动）

| # | 任务 | 状态 | 证据 |
|---|------|:----:|------|
| ~~P2'-1~~ | ~~管线顺序：裁决→持久化→安装→水位~~ | ✅（口径修正） | 进程内 in-doubt 消除（`engine.rs::commit_tx` + `memtx::{validate_only, install}`）；恢复边界语义由 R4-P0D 毒化定案：WAL 失败 = 40003 + 毒化 + reopen——原表述超前，四轮 §2.5 更正 |
| P2'-2 | Adjudicator/Journal trait 化（替换 Phase 2；含读路径时间戳 + 裁决器 HA/fencing 设计补全） | ⬜ | 设计文档已注状态；实现待 P3-1 |
| P2'-3 | 抽象收敛（journal.rs/consensus/ 标 EXPERIMENTAL 已做；最终删除或并入唯一 trait 集） | 🔧 | 标记完成，收敛随 P2'-2 |

## 遗漏任务登记（三轮 §7 清单，四轮复核后正式入册——此前回应声称登记实际未登记，本轮补上）

| # | 任务 | 优先级 | 备注 |
|---|------|:----:|------|
| ~~M-1~~ | ~~备份/快照手册~~ | ✅ | `dendro backup`（b9161e6：一致性点物理备份 + 幂等 + 原子拷贝 + 运行手册入模块文档）；`--gc-retention-ms -1` 支持 |
| ~~M-2~~ | ~~格式版本兼容守卫~~ | ✅ | b9161e6：manifest format_version 前向守卫（拒绝未来版本，防 serde default 吞未知字段）；WAL FRAME_VERSION 已有校验 |
| M-3 | G6 压缩量化衰退持续测量（zstd 级别 × 列类型 Q 曲线进 `dendro bench`） | P2 | SPEC 08 承诺 |
| M-4 | G4 GPU/CBF 解码对拍（native SIMD 等价实现 + 逐位对拍） | P2 | GPU 保留不能停留在格式注释 |
| M-5 | metrics 直方图（commit/flush/manifest-CAS 延迟）+ 慢查询日志 | P2 | 支撑弹性调度故事 |
| M-6 | fence 对象 GC（每分支每 open 一个，持续累积；load_open_branches 放大） | P2 | 墓碑机制可复用 |
| M-7 | PG 认证 trust/cleartext ✅（四轮已接线）；SCRAM-SHA-256 + TLS | P1→P2 | cleartext 本提交完成；SCRAM/TLS 列 P2 |
| M-8 | s3_cloud 显式 SKIPPED ✅（四轮已改）；部署 yaml 入库 | P2 | yaml 随 k8s 部署文档落地 |

## P3 — 远期

| # | 任务 | 状态 | 备注 |
|---|------|:----:|------|
| P3-1 | Adjudicator + Journal 分布式实施（openraft 3 副本） | ⬜ | SOTA 调研 §3；~~seam 已留~~（概念 seam，非编译期 seam——三轮评审 §5.2 更正）；journal.rs/consensus/ 已标 EXPERIMENTAL，P2' 动工时收敛为唯一 trait 集 |
| P3-2 | multi-region 强一致（Journal 多 Region 2+1） | ⬜ | 依赖基础设施 |
| P3-3 | 向量化列式执行器（Arrow 列式 filter/agg） | ⬜ | AP 性能 |
| P3-4 | criss-cross merge 修复（common_ancestor 遍历多父） | ⬜ | 低频场景 |
| P3-5 | blob 外置（大 value 不进 prolly 叶层） | ⬜ | 参考 lance blob |

---

## 当前冲刺目标

1. ~~P0 修复 + 回归测试~~ ✅（含二轮收尾）
2. ~~fencing 运行时拒写 + 续期~~ ✅
3. ~~P1-6 JOIN 测试（评审最看重）~~ ✅
4. ~~P1-1 只读模式~~ ✅
5. ~~P1-4 GC 定案（含 P0-3 真删除时序）~~ ✅
6. 下一批：P2-1 slt 语料扩展 / P2-3 基准证据链 / P2' Journal+Adjudicator 深化
