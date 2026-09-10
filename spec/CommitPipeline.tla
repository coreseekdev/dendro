---- MODULE CommitPipeline ----
(* 两段式提交 + 无间隙水位（S-3/P2-6 规约 v1）。
 *
 * 建模对象：dendro commit_tx 的四阶段管线（裁决→入队→[durable]→安装→水位）
 * 的抽象状态机。seq 即提交序号（epoch 内单增）。
 *
 * 历史缺陷（本模型必须能检出，见 Mutants 注释）：
 * - BUG-WM-STALL（R4 实证）：Pass2 计算前沿时把自身仍算在 in-flight 里
 *   （或以"自身 ts"代替 installed_max）⇒ 水位永久停滞 ⇒ 已安装版本不可见。
 * - BUG-WM-SKIP：水位直接推到自身 ts，跳过未安装的更小 ts ⇒ 事务内
 *   可见性翻转（重复读违约）。
 *
 * 状态空间：MaxSeq=4 时全空间 < 10^5 状态（TLC 秒级）。
 *
 * 环境模型（docs/VERIFICATION.md §3 故障模型）：
 * - AppendFail：入队但永不 durable（写丢失，未 ack，UltimateDrop 可发生）
 * - CrashUncertain：帧已 durable 但客户端收到 40003（Uncertain）——
 *   该 ts 从 in-flight 摘除、永不安装（本进程内）；落盘帧留给 crash-replay
 *   裁决（超出本模型轮次）。
 *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    MaxSeq,     (* 最大提交序号 *)
    NIL         (* 不存在标记；取 0 *)

AgSeq == 1..MaxSeq
Max2(x, y) == IF x >= y THEN x ELSE y
Min2(x, y) == IF x <= y THEN x ELSE y
MinOf(S) == CHOOSE x \in S : \A y \in S : x <= y

VARIABLES
    nextSeq,        (* 下一个分配的提交序号 *)
    inFlight,       (* 已入队未安装的提交序号集 *)
    durable,        (* 已到达持久存储的提交序号集 *)
    installed,      (* 已安装（可见）的提交序号集 *)
    lost,           (* 已丢弃（写丢失，永不安装）的提交序号集 *)
    acked,          (* 已向客户端 ack 的提交序号集 *)
    waterMark,      (* 无间隙可见水位 *)
    installedMax    (* 已安装的最大提交序号 *)

SeqConstraint == ∀ s ∈ AgSeq : TRUE

WatermarkBound ==
    IF inFlight = {} THEN installedMax
    ELSE Min2(installedMax, MinOf(inFlight) - 1)


vars == <<nextSeq, inFlight, durable, installed, lost, acked, waterMark, installedMax>>

Init ==
    /\ nextSeq = 1
    /\ inFlight = {}
    /\ durable = {}
    /\ installed = {}
    /\ lost = {}
    /\ acked = {}
    /\ waterMark = 0
    /\ installedMax = 0

(* Pass1：裁决 + 入队 + 注册 in-flight（同临界区，原子） *)
Enqueue ==
    \E ts \in AgSeq \ (inFlight \union installed \union lost \union durable) :
        /\ nextSeq <= MaxSeq
        /\ ts = nextSeq
        /\ nextSeq' = nextSeq + 1
        /\ inFlight' = inFlight \union {ts}
        /\ UNCHANGED <<durable, installed, lost, acked, waterMark, installedMax>>

(* 环境动作：帧到达持久存储（fsync 完成） *)
MakeDurable ==
    \E ts \in inFlight \ durable :
        /\ durable' = durable \union {ts}
        /\ UNCHANGED <<nextSeq, inFlight, installed, lost, acked, waterMark, installedMax>>

(* Pass2：安装 + 无间隙前沿推进。
 * 正确公式（实现见 engine.rs Pass2）：先摘除自身，再取
 *   frontier = Min(installedMax', min(inFlight') - 1)
 * Mutant M1（BUG-WM-STALL）：frontier = Min(installedMax', min(inFlight ∪ {ts}) - 1)
 *   —— 自身滞留 in-flight ⇒ 水位永久停滞（TLC 活性检出）。
 * Mutant M2（BUG-WM-SKIP）：waterMark' = ts
 *   —— 跳过未安装的更低 ts ⇒ InvWatermarkVisible 失败（TLC 安全检出）。
 *)
Install(ts) ==
    /\ ts \in inFlight
    /\ ts \in durable
    /\ installed' = installed \union {ts}
    /\ installedMax' = Max2(installedMax, ts)
    /\ inFlight' = inFlight \ {ts}
    /\ acked' = acked \union {ts}
    /\ waterMark' = Max2(waterMark,
                          IF inFlight' = {} THEN installedMax'
                          ELSE Min2(installedMax', MinOf(inFlight') - 1))
    /\ nextSeq' = nextSeq
    /\ durable' = durable
    /\ lost' = lost

(* 环境动作：写入丢失（AppendFail / 非持久崩溃）——未 durable 的 in-flight
 * 被丢弃，永不安装、永不 ack（客户端收到错误）。
 * 摘除必须重算水位（R8-WM 修复回灌：任何摘除路径同公式）。 *)
UltimateDrop(ts) ==
    /\ ts \in inFlight
    /\ ts \notin durable
    /\ inFlight' = inFlight \ {ts}
    /\ lost' = lost \union {ts}
    /\ waterMark' = Max2(waterMark,
                          IF inFlight' = {} THEN installedMax
                          ELSE Min2(installedMax, MinOf(inFlight') - 1))
    /\ UNCHANGED <<nextSeq, durable, installed, acked, installedMax>>

(* 环境动作：Uncertain（帧已 durable 但客户端收到错误）——in-flight 摘除，
 * ts 既不安装也不 lost（本进程内"结果未知"；落盘帧留 crash-replay 裁决）。
 * 模型以 uncertainDropped 记账（复用 lost，语义上"未 ack 且未安装"） *)
UncertainDrop(ts) ==
    /\ ts \in inFlight
    /\ ts \in durable
    /\ inFlight' = inFlight \ {ts}
    /\ lost' = lost \union {ts}
    /\ waterMark' = Max2(waterMark,
                          IF inFlight' = {} THEN installedMax
                          ELSE Min2(installedMax, MinOf(inFlight') - 1))
    /\ UNCHANGED <<nextSeq, durable, installed, acked, installedMax>>

Next ==
    \E ts \in AgSeq :
        \/ Enqueue
        \/ MakeDurable
        \/ Install(ts)
        \/ UltimateDrop(ts)
        \/ UncertainDrop(ts)

(* ================= 不变式 ================= *)

(* I1 已 ack 的提交必然已安装——ack 先于安装 = 客户端可见幽灵/丢失的根源 *)
InvAckedInstalled == acked \subseteq installed

(* I2 水位覆盖的每个 seq 都已安装或已明确丢弃——可见性不含未决提交 *)
InvWatermarkVisible ==
    \A s \in 1..waterMark : s \in installed \union lost

(* I3 水位不超过已安装最大值（可见性不超前） *)
InvWatermarkBound == waterMark <= installedMax

(* I4 installed 与 lost 不相交（一个提交不能既可见又丢失） *)
InvDisjoint == installed \cap lost = {}

(* I5 in-flight 与 installed/disjoint 不相交（Pass2 摘除的完备性） *)
InvInFlightSane ==
    inFlight \cap installed = {} /\ inFlight \cap lost = {}

(* 公平性（对应实现保证）：每个 in-flight 提交最终必然 durable（组提交
 * 线程 WF）、最终要么安装要么被丢弃（客户端等待有界/InflightGuard）。
 * 无此假设时环境可让 ts 永久滞留 in-flight，水位合理地不追平
 * （非实现缺陷——StopFreedom 反例即此形态）。 *)
FairDurable == \E ts \in inFlight \ durable : MakeDurable
FairInstall == \E ts \in inFlight \cap durable : Install(ts)
FairDrop == \E ts \in inFlight :
    UltimateDrop(ts) \/ UncertainDrop(ts)

Spec ==
    Init
    /\ [][Next]_vars
    /\ WF_vars(FairDurable)
    /\ WF_vars(FairInstall)
    /\ WF_vars(FairDrop)

(* 活性（liveness.cfg 单独跑）：公平调度下水位最终到达 MaxSeq
 * —— Mutant M1（水位停滞）在 L1 检查中红 *)
(* 停滞检测器（抓 Mutant M1 水位停滞）：一旦 installedMax 到顶，
 * 水位必须最终追平——M1（前沿公式把自身算在 in-flight 里）下
 * installedMax = MaxSeq 而水位永久停在 MaxSeq-1，此处红。
 * 注：合法丢弃（UltimateDrop）使 installedMax 不到顶 ⇒ 前件假，不误报。*)
StallFreedom == [](installedMax = MaxSeq => <>(waterMark = installedMax))

====
