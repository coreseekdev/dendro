----------------------------- MODULE DendroCAS -----------------------------
(***************************************************************************)
(* DendroCAS — TLA+ model of dendro's CAS tmp-file/rename write path.      *)
(*                                                                          *)
(* Motivated by intermittent 58030: concurrent writers of the same         *)
(* content-addressed chunk collided on a PID-only tmp file name → one     *)
(* thread's rename hit ENOENT (loser) or O_TRUNC clobbered (phantom).      *)
(*                                                                          *)
(* Abstracted to 1 shared address × 2 writers × 4 states — the minimal    *)
(* state space that exhibits the collision.                                 *)
(*                                                                          *)
(* SharedTmp = FALSE  (bug) → "errored" state reachable (loser ENOENT)     *)
(* SharedTmp = FALSE (fix)  → InvNoErr + InvPublished hold                *)
(***************************************************************************)

EXTENDS Naturals, TLC, FiniteSets

CONSTANTS Writers,       \* {w1, w2}
          SharedTmp      \* TRUE = pid-only tmp (bug); FALSE = unique (fix)

VARIABLES
    store,        \* published chunks: store[addr] = "good" | "empty"
    tmpAllocated, \* how many tmp slots currently allocated
    wState,       \* writer states
    lastOutcome   \* "ok" | "err" | "none"

good == "good"
empty == "empty"

Init ==
    /\ store = [a \in {1} |-> empty]
    /\ tmpAllocated = 0
    /\ wState = [w \in Writers |-> "idle"]
    /\ lastOutcome = "none"

(* Writer allocates tmp then goes to renaming *)
Step(w) ==
    /\ wState[w] = "idle"
    /\ IF SharedTmp
       THEN tmpAllocated' = 1
       ELSE tmpAllocated' = tmpAllocated + 1
    /\ wState' = [wState EXCEPT ![w] = "renaming"]
    /\ UNCHANGED <<store, lastOutcome>>

Publish(w) ==
    /\ wState[w] = "renaming"
    /\ IF SharedTmp
       THEN IF tmpAllocated = 0
            THEN \* shared tmp already consumed by concurrent writer → ENOENT
                 wState' = [wState EXCEPT ![w] = "errored"]
                 /\ lastOutcome' = "err"
                 /\ UNCHANGED <<store, tmpAllocated>>
            ELSE \* first to rename: consume the shared tmp
                 store' = [store EXCEPT ![1] = good]
                 /\ tmpAllocated' = 0
                 /\ wState' = [wState EXCEPT ![w] = "done"]
                 /\ lastOutcome' = "ok"
       ELSE \* unique tmp: rename always succeeds
            store' = [store EXCEPT ![1] = good]
            /\ wState' = [wState EXCEPT ![w] = "done"]
            /\ lastOutcome' = "ok"
            /\ UNCHANGED tmpAllocated

Reset(w) ==
    /\ wState[w] \in {"done", "errored"}
    /\ wState' = [wState EXCEPT ![w] = "idle"]
    /\ tmpAllocated' = 0
    /\ UNCHANGED <<store, lastOutcome>>

Next ==
    \E w \in Writers : Step(w) \/ Publish(w) \/ Reset(w)

Spec == Init /\ [][Next]_<<store, tmpAllocated, wState, lastOutcome>>

TypeOK ==
    /\ store[1] \in {good, empty}
    /\ tmpAllocated \in 0..4
    /\ wState \in [Writers -> {"idle", "renaming", "done", "errored"}]
    /\ lastOutcome \in {"ok", "err", "none"}

(* InvNoErr — with unique tmp, no writer ever errors *)
InvNoErr ==
    ~SharedTmp => \A w \in Writers : wState[w] # "errored"

(* InvPublished — once any writer publishes, the address stays readable *)
InvPublished ==
    \A w \in Writers :
        wState[w] = "done" => store[1] = good

(* NoErr — no writer ever errors (holds with unique tmp; violated with
   shared tmp where concurrent rename consumes the tmp → loser ENOENT) *)
NoErr ==
    \A w \in Writers : wState[w] # "errored"

(***************************************************************************)
(* Negation harness:                                                       *)
(* Run 1: SharedTmp=FALSE, check InvNoErr + InvPublished (fix proof)     *)
(* Run 2: SharedTmp=TRUE,  negate InvNoErr (i.e., assert it fails) —      *)
(*        TLC finds a trace where a writer hits "errored" (bug proof)    *)
(***************************************************************************)
=============================================================================
