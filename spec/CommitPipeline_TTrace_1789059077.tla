---- MODULE CommitPipeline_TTrace_1789059077 ----
EXTENDS Sequences, TLCExt, CommitPipeline, Toolbox, Naturals, TLC

_expression ==
    LET CommitPipeline_TEExpression == INSTANCE CommitPipeline_TEExpression
    IN CommitPipeline_TEExpression!expression
----

_trace ==
    LET CommitPipeline_TETrace == INSTANCE CommitPipeline_TETrace
    IN CommitPipeline_TETrace!trace
----

_prop ==
    ~<>[](
        durable = ({3, 4})
        /\
        installed = ({4})
        /\
        installedMax = (4)
        /\
        lost = ({1, 2, 3})
        /\
        inFlight = ({})
        /\
        nextSeq = (5)
        /\
        waterMark = (2)
        /\
        acked = ({4})
    )
----

_init ==
    /\ inFlight = _TETrace[1].inFlight
    /\ nextSeq = _TETrace[1].nextSeq
    /\ waterMark = _TETrace[1].waterMark
    /\ installedMax = _TETrace[1].installedMax
    /\ durable = _TETrace[1].durable
    /\ installed = _TETrace[1].installed
    /\ lost = _TETrace[1].lost
    /\ acked = _TETrace[1].acked
----

_next ==
    /\ \E i,j \in DOMAIN _TETrace:
        /\ \/ /\ j = i + 1
              /\ i = TLCGet("level")
        /\ inFlight  = _TETrace[i].inFlight
        /\ inFlight' = _TETrace[j].inFlight
        /\ nextSeq  = _TETrace[i].nextSeq
        /\ nextSeq' = _TETrace[j].nextSeq
        /\ waterMark  = _TETrace[i].waterMark
        /\ waterMark' = _TETrace[j].waterMark
        /\ installedMax  = _TETrace[i].installedMax
        /\ installedMax' = _TETrace[j].installedMax
        /\ durable  = _TETrace[i].durable
        /\ durable' = _TETrace[j].durable
        /\ installed  = _TETrace[i].installed
        /\ installed' = _TETrace[j].installed
        /\ lost  = _TETrace[i].lost
        /\ lost' = _TETrace[j].lost
        /\ acked  = _TETrace[i].acked
        /\ acked' = _TETrace[j].acked

\* Uncomment the ASSUME below to write the states of the error trace
\* to the given file in Json format. Note that you can pass any tuple
\* to `JsonSerialize`. For example, a sub-sequence of _TETrace.
    \* ASSUME
    \*     LET J == INSTANCE Json
    \*         IN J!JsonSerialize("CommitPipeline_TTrace_1789059077.json", _TETrace)

=============================================================================

 Note that you can extract this module `CommitPipeline_TEExpression`
  to a dedicated file to reuse `expression` (the module in the 
  dedicated `CommitPipeline_TEExpression.tla` file takes precedence 
  over the module `CommitPipeline_TEExpression` below).

---- MODULE CommitPipeline_TEExpression ----
EXTENDS Sequences, TLCExt, CommitPipeline, Toolbox, Naturals, TLC

expression == 
    [
        \* To hide variables of the `CommitPipeline` spec from the error trace,
        \* remove the variables below.  The trace will be written in the order
        \* of the fields of this record.
        inFlight |-> inFlight
        ,nextSeq |-> nextSeq
        ,waterMark |-> waterMark
        ,installedMax |-> installedMax
        ,durable |-> durable
        ,installed |-> installed
        ,lost |-> lost
        ,acked |-> acked
        
        \* Put additional constant-, state-, and action-level expressions here:
        \* ,_stateNumber |-> _TEPosition
        \* ,_inFlightUnchanged |-> inFlight = inFlight'
        
        \* Format the `inFlight` variable as Json value.
        \* ,_inFlightJson |->
        \*     LET J == INSTANCE Json
        \*     IN J!ToJson(inFlight)
        
        \* Lastly, you may build expressions over arbitrary sets of states by
        \* leveraging the _TETrace operator.  For example, this is how to
        \* count the number of times a spec variable changed up to the current
        \* state in the trace.
        \* ,_inFlightModCount |->
        \*     LET F[s \in DOMAIN _TETrace] ==
        \*         IF s = 1 THEN 0
        \*         ELSE IF _TETrace[s].inFlight # _TETrace[s-1].inFlight
        \*             THEN 1 + F[s-1] ELSE F[s-1]
        \*     IN F[_TEPosition - 1]
    ]

=============================================================================



Parsing and semantic processing can take forever if the trace below is long.
 In this case, it is advised to uncomment the module below to deserialize the
 trace from a generated binary file.

\*
\*---- MODULE CommitPipeline_TETrace ----
\*EXTENDS IOUtils, CommitPipeline, TLC
\*
\*trace == IODeserialize("CommitPipeline_TTrace_1789059077.bin", TRUE)
\*
\*=============================================================================
\*

---- MODULE CommitPipeline_TETrace ----
EXTENDS CommitPipeline, TLC

trace == 
    <<
    ([durable |-> {},installed |-> {},installedMax |-> 0,lost |-> {},inFlight |-> {},nextSeq |-> 1,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {},installed |-> {},installedMax |-> 0,lost |-> {},inFlight |-> {1},nextSeq |-> 2,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {},installed |-> {},installedMax |-> 0,lost |-> {1},inFlight |-> {},nextSeq |-> 2,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {},installed |-> {},installedMax |-> 0,lost |-> {1},inFlight |-> {2},nextSeq |-> 3,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {},installed |-> {},installedMax |-> 0,lost |-> {1, 2},inFlight |-> {},nextSeq |-> 3,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {},installed |-> {},installedMax |-> 0,lost |-> {1, 2},inFlight |-> {3},nextSeq |-> 4,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {},installed |-> {},installedMax |-> 0,lost |-> {1, 2},inFlight |-> {3, 4},nextSeq |-> 5,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {4},installed |-> {},installedMax |-> 0,lost |-> {1, 2},inFlight |-> {3, 4},nextSeq |-> 5,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {3, 4},installed |-> {},installedMax |-> 0,lost |-> {1, 2},inFlight |-> {3, 4},nextSeq |-> 5,waterMark |-> 0,acked |-> {}]),
    ([durable |-> {3, 4},installed |-> {4},installedMax |-> 4,lost |-> {1, 2},inFlight |-> {3},nextSeq |-> 5,waterMark |-> 2,acked |-> {4}]),
    ([durable |-> {3, 4},installed |-> {4},installedMax |-> 4,lost |-> {1, 2, 3},inFlight |-> {},nextSeq |-> 5,waterMark |-> 2,acked |-> {4}])
    >>
----


=============================================================================

---- CONFIG CommitPipeline_TTrace_1789059077 ----
CONSTANTS
    MaxSeq = 4
    NIL = 0

PROPERTY
    _prop

CHECK_DEADLOCK
    \* CHECK_DEADLOCK off because of PROPERTY or INVARIANT above.
    FALSE

INIT
    _init

NEXT
    _next

CONSTANT
    _TETrace <- _trace

ALIAS
    _expression
=============================================================================
\* Generated on Fri Sep 11 00:51:18 CST 2026