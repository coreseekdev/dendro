----------------------------- MODULE DendroMerge -----------------------------
(***************************************************************************)
(* DendroMerge — TLA+ model of dendro's three-way merge visibility and     *)
(* the segment-only aggregation gate.                                      *)
(*                                                                          *)
(* Motivated by the P0-era correctness work (2026-09-19): the Arrow        *)
(* global-aggregation shortcut reads ONLY immutable columnar segments;     *)
(* it is sound iff the un-materialized delta (memtx overlay / explicit     *)
(* txn writes / col_deletes) is empty. The engine gate (try_arrow_global_ *)
(* agg) implements this as has_visible_rows + col_deletes + txn checks.    *)
(*                                                                          *)
(* This spec formalizes:                                                    *)
(*   1. visible(k): merge priority txn > overlay > newer segment > older   *)
(*      segment; tombstones hide; col_deletes suppress segment rows only   *)
(*      (tail re-inserts survive) — the MainPlusDeltaSource contract.      *)
(*   2. InvGateSound: gate_open => ∀k: seg_only_visible(k) = visible(k)    *)
(*      (segments-only aggregation equals full-state aggregation).         *)
(*   3. InvGateClosedWithDelta: any visible delta ⇒ gate must be closed    *)
(*      (its contrapositive use; enforced as a state invariant).           *)
(*                                                                          *)
(* Negation check: run with GateIgnoresDelta = TRUE to see                 *)
(* InvGateSound violated (delta dropped ⇒ wrong aggregate) — the bug the   *)
(* differential tests lock at the Rust level.                              *)
(***************************************************************************)

EXTENDS Naturals, TLC, FiniteSets

CONSTANTS Keys,         \* {k1, k2}
          GateIgnoresDelta   \* FALSE = sound gate (engine); TRUE = bug model

VARIABLES
    segOld,        \* segOld[k] ∈ {"absent", "v1"}         (older segment)
    segNew,        \* segNew[k] ∈ {"absent", "v2"}         (newer segment)
    overlay,       \* overlay[k] ∈ {"none", "put", "del"}  (memtx snapshot)
    txn,           \* txn[k] ∈ {"none", "put", "del"}      (explicit txn writes)
    colDeletes     \* set of keys suppressed in SEGMENT sources only

TypeOK ==
    /\ segOld ∈ [Keys → {"absent", "v1"}]
    /\ segNew ∈ [Keys → {"absent", "v2"}]
    /\ overlay ∈ [Keys → {"none", "put", "del"}]
    /\ txn ∈ [Keys → {"none", "put", "del"}]
    /\ colDeletes ⊆ Keys

(***************************************************************************)
(* Segment-side visibility: newest-wins within segments; colDeletes        *)
(* suppress segment rows (tail re-inserts are NOT segments — unaffected).  *)
(***************************************************************************)
SegVal(k) ==
    IF k \notin colDeletes
       \/ (segNew[k] = "absent" /\ segOld[k] = "absent")
    THEN IF segNew[k] # "absent" THEN "v2"
         ELSE IF segOld[k] # "absent" THEN "v1"
         ELSE "absent"
    ELSE "absent"     \* suppressed by colDeletes

(***************************************************************************)
(* Full visibility: txn > overlay > segments (tombstone = "absent").       *)
(***************************************************************************)
Visible(k) ==
    IF txn[k] = "put" THEN "txn"
    ELSE IF txn[k] = "del" THEN "absent"
    ELSE IF overlay[k] = "put" THEN "overlay"
    ELSE IF overlay[k] = "del" THEN "absent"
    ELSE SegVal(k)

(***************************************************************************)
(* The gate: open iff no visible delta (engine: no overlay rows visible,   *)
(* no txn writes on the table, no col_deletes).                            *)
(***************************************************************************)
HasDelta ==
    \/ ∃ k ∈ Keys : overlay[k] # "none" \/ txn[k] # "none"
    \/ colDeletes # {}

GateOpen == IF GateIgnoresDelta THEN TRUE ELSE ~HasDelta

(***************************************************************************)
(* Invariants. InvGateSound: when the gate opens, aggregates computed on   *)
(* segments alone (SegVal) coincide with the full state (Visible).         *)
(* Note "absent" equality matters for COUNT/EXISTS-style aggregates;       *)
(* label equality keeps the model honest for value aggregates too.         *)
(***************************************************************************)
InvGateSound ==
    GateOpen => ∀ k ∈ Keys : SegVal(k) = Visible(k)

InvGateClosedWithDelta ==
    HasDelta => ~GateOpen

(***************************************************************************)
(* State machine: writers mutate segments/overlay/txn/deletes; the gate    *)
(* recomputes each step (functional — no separate variable needed).       *)
(***************************************************************************)
WriteSegOld(k) ==
    /\ segOld' = [segOld EXCEPT ![k] = "v1"]
    /\ UNCHANGED <<segNew, overlay, txn, colDeletes>>

WriteSegNew(k) ==
    /\ segNew' = [segNew EXCEPT ![k] = "v2"]
    /\ UNCHANGED <<segOld, overlay, txn, colDeletes>>

OverlayPut(k) ==
    /\ overlay' = [overlay EXCEPT ![k] = "put"]
    /\ UNCHANGED <<segOld, segNew, txn, colDeletes>>

OverlayDel(k) ==
    /\ overlay' = [overlay EXCEPT ![k] = "del"]
    /\ UNCHANGED <<segOld, segNew, txn, colDeletes>>

TxnPut(k) ==
    /\ txn' = [txn EXCEPT ![k] = "put"]
    /\ UNCHANGED <<segOld, segNew, overlay, colDeletes>>

TxnDel(k) ==
    /\ txn' = [txn EXCEPT ![k] = "del"]
    /\ UNCHANGED <<segOld, segNew, overlay, colDeletes>>

ColDelete(k) ==
    /\ colDeletes' = colDeletes ∪ {k}
    /\ UNCHANGED <<segOld, segNew, overlay, txn>>

ClearDelta ==
    /\ overlay' = [k ∈ Keys ↦ "none"]
    /\ txn' = [k ∈ Keys ↦ "none"]
    /\ colDeletes' = {}
    /\ UNCHANGED <<segOld, segNew>>

Init ==
    /\ segOld = [k ∈ Keys ↦ "absent"]
    /\ segNew = [k ∈ Keys ↦ "absent"]
    /\ overlay = [k ∈ Keys ↦ "none"]
    /\ txn = [k ∈ Keys ↦ "none"]
    /\ colDeletes = {}

Next ==
    ∃ k ∈ Keys :
        WriteSegOld(k) \/ WriteSegNew(k)
        \/ OverlayPut(k) \/ OverlayDel(k)
        \/ TxnPut(k) \/ TxnDel(k) \/ ColDelete(k)
    \/ ClearDelta

Spec == Init /\ [][Next]_<<segOld, segNew, overlay, txn, colDeletes>>

=============================================================================
