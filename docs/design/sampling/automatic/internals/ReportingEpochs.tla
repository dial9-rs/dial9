-------------------------- MODULE ReportingEpochs --------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS MixFractions, EarlyPromotion, ReporterOnlyRecovery, UnfencedCAS,
          ReapplyTrim, LateEnable, StalledRecovery
Processes == {"a", "b"}
None == 4
Eligible(f) == IF f = 1 THEN Processes ELSE {"a"}

VARIABLES active, fraction, pending, pendingFraction, ticks, members, live,
          version, readVersion, stalePublished, observationEpoch,
          observationFraction, promotionReady, startup, profiling, stopped,
          applied, weightPower, zeroSeen
vars == <<active, fraction, pending, pendingFraction, ticks, members, live,
          version, readVersion, stalePublished, observationEpoch,
          observationFraction, promotionReady, startup, profiling, stopped,
          applied, weightPower, zeroSeen>>

Init ==
  /\ active = 0 /\ fraction = 1 /\ pending = None /\ pendingFraction = 1
  /\ ticks = 0 /\ members = [e \in 0..3 |-> IF e = 0 THEN Processes ELSE {}]
  /\ live = Processes /\ version = 0 /\ readVersion = 0
  /\ stalePublished = FALSE /\ observationEpoch = 0
  /\ observationFraction = 1 /\ promotionReady = TRUE
  /\ startup \in SUBSET Processes /\ profiling = startup /\ stopped = {}
  /\ applied = {} /\ weightPower = [p \in Processes |-> 0] /\ zeroSeen = FALSE

Propose ==
  /\ active = 0 /\ pending = None /\ version < 4
  /\ pending' = 1 /\ pendingFraction' = 2 /\ ticks' = 0
  /\ version' = version + 1
  /\ UNCHANGED <<active, fraction, members, live, readVersion, stalePublished,
        observationEpoch, observationFraction, promotionReady, startup,
        profiling, stopped, applied, weightPower, zeroSeen>>

Tick ==
  /\ pending # None /\ ticks < 2 /\ ticks' = ticks + 1
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, members, live,
        version, readVersion, stalePublished, observationEpoch,
        observationFraction, promotionReady, startup, profiling, stopped,
        applied, weightPower, zeroSeen>>

Report(p) ==
  /\ pending # None /\ p \in live
  /\ p \in Eligible(pendingFraction) \cup profiling \cup stopped
  /\ p \notin members[pending]
  /\ members' = [members EXCEPT ![pending] = @ \cup {p}]
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, ticks, live,
        version, readVersion, stalePublished, observationEpoch,
        observationFraction, promotionReady, startup, profiling, stopped,
        applied, weightPower, zeroSeen>>

Ready == ticks = 2
Promote ==
  /\ pending # None /\ version < 4
  /\ (Ready \/ EarlyPromotion)
  /\ Cardinality(members[pending] \cap Eligible(pendingFraction)) > 0
  /\ active' = pending /\ fraction' = pendingFraction /\ pending' = None
  /\ observationEpoch' = IF MixFractions THEN active ELSE pending
  /\ observationFraction' = pendingFraction
  /\ promotionReady' = Ready /\ version' = version + 1
  /\ UNCHANGED <<pendingFraction, ticks, members, live, readVersion,
        stalePublished, startup, profiling, stopped, applied, weightPower, zeroSeen>>

Leave(p) ==
  /\ p \in live /\ live' = live \ {p}
  /\ members' = [e \in 0..3 |-> members[e] \ {p}]
  /\ profiling' = profiling \ {p}
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, ticks, version,
        readVersion, stalePublished, observationEpoch, observationFraction,
        promotionReady, startup, stopped, applied, weightPower, zeroSeen>>

ObserveZero ==
  /\ active = 1 /\ members[active] \cap Eligible(fraction) = {} /\ ~zeroSeen
  /\ zeroSeen' = TRUE
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, ticks, members,
        live, version, readVersion, stalePublished, observationEpoch,
        observationFraction, promotionReady, startup, profiling, stopped,
        applied, weightPower>>

ProposeReduction ==
  /\ active = 1 /\ pending = None /\ version = 2
  /\ pending' = 2 /\ pendingFraction' = 2 /\ ticks' = 0
  /\ version' = version + 1
  /\ UNCHANGED <<active, fraction, members, live, readVersion, stalePublished,
        observationEpoch, observationFraction, promotionReady, startup,
        profiling, stopped, applied, weightPower, zeroSeen>>

RecoveryEnabled == zeroSeen /\ live # {} /\
  (pending = None \/ (~StalledRecovery /\ pendingFraction # 1)) /\
  (~ReporterOnlyRecovery \/ live \cap Eligible(fraction) # {})
Recover ==
  /\ RecoveryEnabled /\ version < 4
  /\ pending' = 3 /\ pendingFraction' = 1 /\ ticks' = 0
  /\ version' = version + 1
  /\ UNCHANGED <<active, fraction, members, live, readVersion, stalePublished,
        observationEpoch, observationFraction, promotionReady, startup,
        profiling, stopped, applied, weightPower, zeroSeen>>

StaleCAS ==
  /\ version # readVersion /\ ~stalePublished /\ UnfencedCAS
  /\ stalePublished' = TRUE
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, ticks, members,
        live, version, readVersion, observationEpoch, observationFraction,
        promotionReady, startup, profiling, stopped, applied, weightPower, zeroSeen>>

ApplyTrim(p) ==
  /\ p \in profiling /\ (p \notin applied \/ ReapplyTrim) /\ weightPower[p] < 2
  /\ applied' = applied \cup {p}
  /\ weightPower' = [weightPower EXCEPT ![p] = @ + 1]
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, ticks, members,
        live, version, readVersion, stalePublished, observationEpoch,
        observationFraction, promotionReady, startup, profiling, stopped, zeroSeen>>

Stop(p) ==
  /\ p \in profiling /\ profiling' = profiling \ {p} /\ stopped' = stopped \cup {p}
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, ticks, members,
        live, version, readVersion, stalePublished, observationEpoch,
        observationFraction, promotionReady, startup, applied, weightPower, zeroSeen>>

Enable(p) ==
  /\ LateEnable /\ p \in live \ stopped /\ p \notin profiling
  /\ profiling' = profiling \cup {p}
  /\ UNCHANGED <<active, fraction, pending, pendingFraction, ticks, members,
        live, version, readVersion, stalePublished, observationEpoch,
        observationFraction, promotionReady, startup, stopped, applied, weightPower, zeroSeen>>

Next == Propose \/ ProposeReduction \/ Tick \/ Promote \/ ObserveZero \/ Recover \/ StaleCAS \/
  (\E p \in Processes: Report(p) \/ Leave(p) \/ ApplyTrim(p) \/ Stop(p) \/ Enable(p))
Spec == Init /\ [][Next]_vars

EpochCorrect == observationEpoch = active /\ observationFraction = fraction
NoPrematurePromotion == promotionReady
QuietRecovery == zeroSeen /\ (pending = None \/ pendingFraction # 1) /\
  live # {} /\ version < 4 => RecoveryEnabled
CASFenced == ~stalePublished
OnceOnly == \A p \in Processes: weightPower[p] <= 1
NoLateEnable == profiling \subseteq startup /\ profiling \cap stopped = {}
=============================================================================
