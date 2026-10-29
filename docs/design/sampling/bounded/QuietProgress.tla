-------------------------- MODULE QuietProgress --------------------------
EXTENDS Naturals, TLC
CONSTANTS ReporterOnlyReads, RedrawEachInterval
VARIABLES now, due, stage, seenExpired, owner
vars == <<now, due, stage, seenExpired, owner>>

(* Finite healthy-path abstraction: the process is alive, quiet and disabled.
   A tick is a minute, one GET completes within one tick, scheduling services a
   due request before passing its deadline, and election has no live contender.
   Fair completion is an assumption, not a model of failed/suspended S3 calls. *)
Init ==
  /\ now = 0 /\ due \in 1..5 /\ stage = "idle"
  /\ seenExpired = FALSE /\ owner = "none"

Tick ==
  /\ now < 6
  /\ (now < due \/ stage # "idle")
  /\ (stage # "pending" \/ now < due + 1)
  /\ now' = now + 1
  /\ due' = IF RedrawEachInterval /\ stage = "idle" THEN due + 1 ELSE due
  /\ UNCHANGED <<stage, seenExpired, owner>>

StartRead ==
  /\ ~ReporterOnlyReads /\ now >= due /\ stage = "idle"
  /\ stage' = "pending"
  /\ UNCHANGED <<now, due, seenExpired, owner>>

CompleteRead ==
  /\ stage = "pending"
  /\ stage' = "complete" /\ seenExpired' = TRUE
  /\ UNCHANGED <<now, due, owner>>

Claim ==
  /\ seenExpired /\ owner = "none"
  /\ owner' = "quiet"
  /\ UNCHANGED <<now, due, stage, seenExpired>>

Next == Tick \/ StartRead \/ CompleteRead \/ Claim
Spec ==
  Init /\ [][Next]_vars /\ WF_vars(Tick) /\ WF_vars(StartRead) /\
  WF_vars(CompleteRead) /\ WF_vars(Claim)
QuietEventuallyReads == <> seenExpired
QuietEventuallyClaims == <> (owner = "quiet")
=============================================================================
