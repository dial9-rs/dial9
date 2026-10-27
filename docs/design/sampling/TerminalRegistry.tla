------------------------- MODULE TerminalRegistry ---------------------------
EXTENDS TLC
CONSTANT StateFirst
VARIABLES carrierOldBucket, activeNewBucket, observed, badTerminal
vars == <<carrierOldBucket, activeNewBucket, observed, badTerminal>>
Init ==
  /\ carrierOldBucket = FALSE /\ activeNewBucket = FALSE
  /\ observed = "none" /\ badTerminal = FALSE
CarrierWrite ==
  /\ ~carrierOldBucket /\ carrierOldBucket' = TRUE
  /\ UNCHANGED <<activeNewBucket, observed, badTerminal>>
\* The old active request can finish after a backward clock step/carrier PUT.
DelayedActiveWrite ==
  /\ carrierOldBucket /\ ~activeNewBucket /\ activeNewBucket' = TRUE
  /\ UNCHANGED <<carrierOldBucket, observed, badTerminal>>
Census ==
  /\ observed' =
       IF carrierOldBucket /\ (StateFirst \/ ~activeNewBucket) THEN "carrier"
       ELSE IF activeNewBucket THEN "active" ELSE "none"
  /\ badTerminal' = (badTerminal \/ (carrierOldBucket /\ observed' = "active"))
  /\ UNCHANGED <<carrierOldBucket, activeNewBucket>>
Next == CarrierWrite \/ DelayedActiveWrite \/ Census
Spec == Init /\ [][Next]_vars
TerminalClassification == ~badTerminal
=============================================================================
