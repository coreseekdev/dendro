----------------------------- MODULE DendroWAL -----------------------------
(***************************************************************************)
(* DendroWAL — a TLA+ model of dendro's WAL safety core.                    *)
(*                                                                          *)
(* Modeled after the style of Vanlightly's s3-wal-collection. dendro is a   *)
(* single-writer-per-branch WAL on object storage with EPOCH FENCING:       *)
(*                                                                          *)
(*   - every writer generation claims an epoch via a conditional lease     *)
(*     object  fence/{branch}/{epoch}.json  (put_if_absent, unique);        *)
(*   - frames carry composite timestamps ts = epoch<<32 | seq. Segments     *)
(*     live under  wal/{branch}/e{epoch}/{seg}.wal;                          *)
(*   - recovery replays epochs in ascending order; a stale writer's late    *)
(*     PUTs land in its OLD epoch path and are suppressed by ts comparison  *)
(*     (brain-split safe);                                                  *)
(*   - checkpoint advances `covered` and retires WAL prefixes. The retire   *)
(*     bound: a segment may only be retired/GC'd when ALL its frames are    *)
(*     installed (their ts <= covered). This is the audit-R3-P0 invariant:  *)
(*     retiring a segment that still holds durable-but-unacked frames makes *)
(*     restart replay skip ACKED commits.                                   *)
(*                                                                          *)
(* Simplifications (documented, safety-preserving):                         *)
(*   - the object store is atomic (no torn tails — dendro handles those     *)
(*     separately with sealed/unsealed trailer semantics);                  *)
(*   - one frame == one segment (the retire bound per frame is             *)
(*     "ts <= covered"; segments are batches of frames and add nothing      *)
(*     structural here);                                                    *)
(*   - the retention window is collapsed (tombstone+sweep = one step);      *)
(*     the SAFETY-relevant bound is *what* may be deleted, not when;        *)
(*   - lease expiry is adversarial-nondeterministic (a writer may become    *)
(*     fenced at any time), and a fenced writer may still complete PUTs it  *)
(*     had already enqueued — recovery must suppress those by epoch order.  *)
(*                                                                          *)
(* The two invariants (the local, decidable forms used by the Rust debug    *)
(* assertions in recovery.rs):                                              *)
(*                                                                          *)
(*   InvAcked     — prefix/durability: every ACKED ts is either <= covered  *)
(*                  (materialized into the tree) or still present in the    *)
(*                  live WAL. Breaking the checkpoint/GC bound breaks this. *)
(*   InvRecovered — rebuildability: a completed recovery yields, per key,   *)
(*                  the value of the max-ts live frame (epoch-ordered       *)
(*                  replay with ts suppression == latest-live).             *)
(***************************************************************************)

(***************************************************************************)
(* VERIFIED (TLC 1.8.0, 2026-09-17):                                        *)
(*   main spec : 1,361,163 states / 170,748 distinct — TypeOK,              *)
(*               InvCoveredGrounded, InvAcked, InvRecovered all hold.       *)
(*   Neg1 (GcFrame retire bound weakened to TRUE)                           *)
(*               -> InvAcked violated: an ACKed ts > covered is deleted     *)
(*                  from liveWal — the audit-R3-P0 commit-loss class.       *)
(*   Neg2 (Checkpoint precondition RecoverableUpTo removed)                 *)
(*               -> InvCoveredGrounded violated: covered advances over an   *)
(*                  un-ACKed frame, which is then GC-able — the             *)
(*                  materialization claim is ungrounded.                    *)
(*   Run: java -cp tla2tools.jar tlc2.TLC -deadlock DendroWAL.tla           *)
(***************************************************************************)
EXTENDS Naturals, TLC

CONSTANTS Writers,     \* two writers suffice to exhibit takeover/fencing
          Keys,        \* key universe (single key keeps state small and is
                       \* enough to exhibit stale-epoch suppression)
          Values,      \* value universe
          MaxEpoch,    \* epoch ceiling (state-space bound)
          MaxSeq       \* per-epoch seq ceiling (state-space bound)

VARIABLES
    leaseEpochs,   \* set of claimed epochs (fence objects present)
    wState,        \* writer -> {"idle", "ready", "fenced"}
    wEpoch,        \* writer -> its epoch (0 = none)
    wSeq,          \* writer -> next seq in its epoch
    wBuf,          \* writer -> frames enqueued, not yet PUT (in-flight)
    wAcked,        \* writer -> set of ts whose durability was ACKed
    liveWal,       \* set of durable frames [epoch | seq | ts | key | val]
    covered,       \* checkpoint watermark (materialized into the tree)
    recOn,         \* recovery process running
    recSnap,       \* recovery's input snapshot (live frames > covered at start)
    recRemain,     \* frames recovery has not applied yet
    recVal,        \* key -> value recovered so far
    recTs          \* key -> max ts recovered so far

TS(e, s) == e * 1000000 + s

FramesOf(fs) == {f.ts : f \in fs}

AckedAny == UNION {wAcked[w] : w \in Writers}

(* 帧 ts 值域（含缓冲/已删——水位只会推进到某个帧的 ts） *)
AllFrameTs ==
    FramesOf(liveWal) \cup FramesOf(UNION {wBuf[w] : w \in Writers}) \cup AckedAny \cup {0}

MaxOf(S) == CHOOSE x \in S : \A y \in S : y <= x

\* (epoch, seq) 字典序（TLA+ 无元组序）
FrameLe(f, g) == f.epoch < g.epoch \/ (f.epoch = g.epoch /\ f.seq <= g.seq)

(* the frame recovery should end up with per key: the max-ts LIVE frame *)
LatestLiveTs(key) == MaxOf({f.ts : f \in {g \in liveWal : g.key = key}})

(* ------------------------------------------------------------------ *)
(* Writer actions                                                      *)
(* ------------------------------------------------------------------ *)

(* Claim a fresh epoch = max(claimed)+1. Uniqueness is by construction in   *)
(* this interleaving model; in dendro it is the put_if_absent on the lease  *)
(* object. A claim FENCES every writer holding a lower epoch (adversarial   *)
(* lease expiry — the takeover does not wait for the old lease to lapse).   *)
ClaimEpoch(w) ==
    /\ wState[w] = "idle"
    /\ wEpoch[w] = 0
    /\ LET e == (MaxOf(leaseEpochs \cup {0})) + 1
       IN  e <= MaxEpoch
           /\ leaseEpochs' = leaseEpochs \cup {e}
           /\ wEpoch' = [wEpoch EXCEPT ![w] = e]
           /\ wState' = [x \in Writers |->
                IF x = w THEN "ready"
                ELSE IF wEpoch[x] = 0 THEN wState[x]
                ELSE IF wEpoch[x] < e THEN "fenced"
                ELSE wState[x]]
           /\ wSeq' = [wSeq EXCEPT ![w] = 1]
           /\ wBuf'  = [wBuf EXCEPT ![w] = {}]
           /\ wAcked' = [wAcked EXCEPT ![w] = {}]
           /\ UNCHANGED <<liveWal, covered, recOn, recSnap, recRemain, recVal, recTs>>

(* Enqueue a frame (under commit_mu, before the durability wait). Allowed   *)
(* while "fenced": an expired writer that already enqueued keeps flushing — *)
(* its frames land in its OLD epoch path; recovery must suppress them.      *)
Append(w) ==
    /\ wState[w] # "idle"
    /\ wSeq[w] <= MaxSeq
    /\ wBuf' = [wBuf EXCEPT ![w] = wBuf[w] \cup {
          [epoch |-> wEpoch[w], seq |-> wSeq[w], ts |-> TS(wEpoch[w], wSeq[w]),
           key |-> CHOOSE k \in Keys : TRUE,
           val |-> CHOOSE v \in Values : TRUE] }]
    /\ wSeq' = [wSeq EXCEPT ![w] = wSeq[w] + 1]
    /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wAcked, liveWal, covered,
                   recOn, recSnap, recRemain, recVal, recTs>>

(* The PUT completes: the frame becomes durable in the writer's epoch path. *)
Flush(w) ==
    /\ wBuf[w] # {}
    /\ LET f == CHOOSE x \in wBuf[w] : TRUE
       IN  liveWal' = liveWal \cup {f}
           /\ wBuf' = [wBuf EXCEPT ![w] = wBuf[w] \ {f}]
           /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wSeq, wAcked, covered,
                          recOn, recSnap, recRemain, recVal, recTs>>

(* Durability observed by the waiting committer -> ACK (+ local install).   *)
Ack(w) ==
    /\ \E f \in liveWal : f.epoch = wEpoch[w] /\ f.ts \notin wAcked[w]
    /\ LET f == CHOOSE x \in liveWal : x.epoch = wEpoch[w] /\ x.ts \notin wAcked[w]
       IN  wAcked' = [wAcked EXCEPT ![w] = wAcked[w] \cup {f.ts}]
           /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wSeq, wBuf, liveWal,
                          covered, recOn, recSnap, recRemain, recVal, recTs>>

(* ------------------------------------------------------------------ *)
(* Checkpoint / GC                                                      *)
(* ------------------------------------------------------------------ *)

(* dendro's checkpoint precondition, distilled: `covered` may advance to t  *)
(* only when EVERY frame below t is ACKed+installed (the two-phase commit  *)
(* frontier). The commit chunk materializes that state into the tree.      *)
RecoverableUpTo(t) ==
    \A f \in liveWal \cup (UNION {wBuf[w] : w \in Writers}) :
        f.ts <= t => f.ts \in (UNION {wAcked[w] : w \in Writers})

Checkpoint ==
    /\ \E t \in AllFrameTs :
          t > covered /\ RecoverableUpTo(t)
          /\ covered' = t
          /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wSeq, wBuf, wAcked,
                         liveWal, recOn, recSnap, recRemain, recVal, recTs>>

(* Retire + sweep (collapsed): a durable frame may leave the live WAL only *)
(* when its ts <= covered — dendro's segment retire bound (audit R3-P0):   *)
(* a frame below covered is materialized, replay will skip it anyway.      *)
GcFrame ==
    /\ \E f \in liveWal : f.ts <= covered
    /\ LET f == CHOOSE x \in liveWal : x.ts <= covered
       IN  liveWal' = liveWal \ {f}
           /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wSeq, wBuf, wAcked,
                          covered, recOn, recSnap, recRemain, recVal, recTs>>

(* ------------------------------------------------------------------ *)
(* Recovery (epoch-ascending, ts-suppressing replay)                    *)
(* ------------------------------------------------------------------ *)

(* Snapshot the live WAL and start replay. `covered` frames are already    *)
(* materialized; replaying them would be harmless but the real system      *)
(* starts from the covered base — model that by seeding the cursor with    *)
(* frames above covered only.                                            *)
RecoverStart ==
    /\ recOn = FALSE
    /\ recSnap' = {f \in liveWal : f.ts > covered}
    /\ recRemain' = recSnap'
    /\ recOn' = TRUE
    /\ recVal' = [k \in Keys |-> CHOOSE v \in Values : TRUE]
    /\ recTs' = [k \in Keys |-> 0]
    /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wSeq, wBuf, wAcked, liveWal,
                   covered>>

(* Apply the (epoch, seq)-minimal remaining frame; ts-suppression: a frame *)
(* whose ts does not exceed the recovered ts of its key is skipped.        *)
RecoverStep ==
    /\ recOn = TRUE
    /\ recRemain # {}
    /\ LET f == CHOOSE x \in recRemain :
                 \A y \in recRemain : FrameLe(x, y)
       IN  recRemain' = recRemain \ {f}
           /\ recTs' = [k \in Keys |-> IF k = f.key /\ f.ts > recTs[k]
                                          THEN f.ts ELSE recTs[k]]
           /\ recVal' = [k \in Keys |-> IF k = f.key /\ f.ts > recTs[k]
                                          THEN f.val ELSE recVal[k]]
           /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wSeq, wBuf, wAcked,
                          liveWal, covered, recOn, recSnap>>

RecoverDone ==
    /\ recOn = TRUE
    /\ recRemain = {}
    /\ UNCHANGED <<leaseEpochs, wState, wEpoch, wSeq, wBuf, wAcked, liveWal,
                   covered, recOn, recSnap, recRemain, recVal, recTs>>

Next ==
    \E w \in Writers :
        ClaimEpoch(w) \/ Append(w) \/ Flush(w) \/ Ack(w)
    \/ Checkpoint \/ GcFrame
    \/ RecoverStart \/ RecoverStep \/ RecoverDone

Init ==
    /\ leaseEpochs = {}
    /\ wState = [w \in Writers |-> "idle"]
    /\ wEpoch = [w \in Writers |-> 0]
    /\ wSeq = [w \in Writers |-> 1]
    /\ wBuf = [w \in Writers |-> {}]
    /\ wAcked = [w \in Writers |-> {}]
    /\ liveWal = {}
    /\ covered = 0
    /\ recOn = FALSE
    /\ recSnap = {}
    /\ recRemain = {}
    /\ recVal = [k \in Keys |-> CHOOSE v \in Values : TRUE]
    /\ recTs = [k \in Keys |-> 0]

Spec == Init /\ [][Next]_<<leaseEpochs, wState, wEpoch, wSeq, wBuf, wAcked,
                          liveWal, covered, recOn, recSnap, recRemain, recVal, recTs>>

(* ------------------------------------------------------------------ *)
(* Invariants                                                           *)
(* ------------------------------------------------------------------ *)

TypeOK ==
    /\ \A w \in Writers : wAcked[w] \subseteq AllFrameTs

(* Inv0 — covered 推进有据：水位锚定在一个已 ack 或仍存活的帧上。
   这是 Checkpoint 前置条件（RecoverableUpTo）的可检后果——削弱
   前置条件（Neg2 否定性验证）即违反：covered 越过未 ack 帧推进，
   该帧随后被 GC，"已物化"声称失据 *)
InvCoveredGrounded ==
    covered = 0 \/ covered \in (AckedAny \cup FramesOf(liveWal)
                              \cup FramesOf(UNION {wBuf[w] : w \in Writers}))

(* Inv1 — prefix/durability: an ACKed commit is materialized (<= covered)
   or still live in the WAL. This is exactly what the segment retire bound
   protects; weakening RecoverableUpTo or the GcFrame guard violates it. *)
InvAcked ==
    \A t \in AckedAny : (t <= covered) \/ (t \in FramesOf(liveWal))

(* Inv2 — rebuildability: a completed recovery reproduces, per key, the
   max-ts LIVE frame (covered base + epoch-ordered ts-suppressing replay). *)
SnapFrames(k) == {g \in recSnap : g.key = k}

(* 对恢复自身输入的断言：步进式 epoch 升序 + ts 抑制回放 ≡ 快照内每键
   max-ts 帧。恢复完成后的新写入不属于本次恢复（liveWal 可再变） *)
InvRecovered ==
    recOn = TRUE /\ recRemain = {}
    => \A k \in Keys :
          SnapFrames(k) # {}
          => /\ recTs[k] = MaxOf({f.ts : f \in SnapFrames(k)})
             /\ recVal[k] = (CHOOSE f \in SnapFrames(k) :
                                 f.ts = MaxOf({g.ts : g \in SnapFrames(k)})).val

(***************************************************************************)
(* Negation harness: remove either safety guard and TLC must produce a     *)
(* counterexample trace — the model doubles as executable documentation of *)
(* WHY the guards exist.                                                   *)
(*                                                                         *)
(*   NoRetireBound == GcFrame with `f.ts <= covered` weakened to TRUE      *)
(*   NoCkptBound   == Checkpoint with RecoverableUpTo(t) weakened to TRUE  *)
(***************************************************************************)
=============================================================================
