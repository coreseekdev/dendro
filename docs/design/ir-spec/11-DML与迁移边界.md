# 11 DML 与迁移边界

> 评审 P1-4 补充：UPDATE/DELETE 是派发链（table_scan）的另一个消费者，
> spec 原稿完全未提——留白即永久双读路径。本文件定边界。

## 1. 现状

`exec_delete` / `update_impl`（ddl.rs 581-691）调 `table_scan_by_name`
→ `table_scan`，**不传 selection**：无 pk 下推、无范围下推、无 AP
剪枝——全表物化后逐行过滤。DML 的 affected-rows 计数语义：对 Null
谓词行跳过；求值错误即语句失败（错误中断 = 失败，不是部分生效）。

## 2. v1 边界定案（Q13 建议案）

- **DML 不进派发器**：v1 UPDATE/DELETE 复用 Source 层的
  RowFallback/Current 路径（即现状行为），派发器只服务 SELECT；
- **差分语料加入 DML 用例**：DML 前后的 SELECT 走全 force_source
  对拍——DML 本身行为不变，但其产物（可见行集）必须在所有路径一致；
- **旧 eval_select 的删除条件**：推迟到 DML 扫描迁移完成（v2c-2 或
  独立里程碑 M-DML：DML 的 WHERE 谓词走 ScalarProgram + Current 范围
  下推——这条白捡的优化与 SELECT 共享全部机制）；
- 双路径存续期间的看护：M-DML 合入前，table_scan 旧路径**冻结**
  （只修 bug 不加功能），新增扫描特性一律走 Source 层。

## 3. M-DML 里程碑（v2c-2 后）

| 项 | 内容 |
|----|------|
| D1 | DELETE/UPDATE 的 WHERE 编译为 ScalarProgram（eval_row）|
| D2 | pk 范围提取接入（Current 范围下推——消灭全表物化）|
| D3 | affected-rows 等价测试（Null 跳过/错误中断语义逐字对拍）|
| D4 | 删除旧 eval_select if-else 链（至此单路径达成）|

## 4. 实现前必须回答

1. DML 语句内嵌子查询（`DELETE WHERE id IN (SELECT ..)`）现状是否
   支持？若支持，M-DML 的谓词求值须复用 Subquery 节点（10 号 spec）。
2. TRUNCATE 走墓碑路径（ddl.rs truncate_impl），不涉及扫描——确认
   无 IR 化需求，维持现状。
