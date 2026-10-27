-------------------------- MODULE BoundaryCensus ----------------------------
EXTENDS Integers, TLC
CONSTANTS BoundaryGuard, EmptyGuard
VARIABLES bucket, safePhase, startBucket, startSafe, stage, count, usableArm,
          badBoundary, badEmpty
vars == <<bucket, safePhase, startBucket, startSafe, stage, count, usableArm,
          badBoundary, badEmpty>>
Init ==
  /\ bucket = 0 /\ safePhase = FALSE
  /\ startBucket = 0 /\ startSafe = FALSE /\ stage = "idle" /\ count = 0
  /\ usableArm = FALSE /\ badBoundary = FALSE /\ badEmpty = FALSE
Begin ==
  /\ stage = "idle"
  /\ startBucket' = bucket /\ startSafe' = safePhase /\ stage' = "listing"
  /\ UNCHANGED <<bucket, safePhase, count, usableArm, badBoundary, badEmpty>>
Finish ==
  /\ stage = "listing"
  /\ stage' = "complete" /\ count' \in {0, 1}
  /\ UNCHANGED <<bucket, safePhase, startBucket, startSafe, usableArm, badBoundary, badEmpty>>
Publish ==
  /\ stage = "complete"
  /\ LET safe == startSafe /\ startBucket = bucket /\ safePhase
         arm == (~BoundaryGuard \/ safe) /\ (~EmptyGuard \/ count > 0)
     IN /\ usableArm' = (usableArm \/ arm)
        /\ badBoundary' = (badBoundary \/ (arm /\ ~safe))
        /\ badEmpty' = (badEmpty \/ (arm /\ count = 0))
  /\ stage' = "idle"
  /\ UNCHANGED <<bucket, safePhase, startBucket, startSafe, count>>
Phase ==
  /\ safePhase' \in BOOLEAN
  /\ UNCHANGED <<bucket, startBucket, startSafe, stage, count, usableArm, badBoundary, badEmpty>>
Rollover ==
  /\ bucket = 0 /\ bucket' = 1 /\ safePhase' = FALSE
  /\ UNCHANGED <<startBucket, startSafe, stage, count, usableArm, badBoundary, badEmpty>>
Next == Begin \/ Finish \/ Publish \/ Phase \/ Rollover
Spec == Init /\ [][Next]_vars
GuardedCensus == ~badBoundary
NonemptyArm == ~badEmpty
=============================================================================
