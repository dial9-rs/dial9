------------------------- MODULE BernoulliControl -------------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS Workers, MaxEpochs, IgnoreEpochGuard, IgnoreCAS
VARIABLES version, holder, currentRound, rounds, pending,
          started, active, terminal, offAtStartup, startEpoch, lastEpoch,
          applied, factors, weight, staleCommit

vars == <<version, holder, currentRound, rounds, pending,
          started, active, terminal, offAtStartup, startEpoch, lastEpoch,
          applied, factors, weight, staleCommit>>

Round == [id : 0..MaxEpochs, factor : {1, 2}, kept : SUBSET Workers]
EmptyRound == [id |-> 0, factor |-> 1, kept |-> Workers]
EmptyPending == [ready |-> FALSE, base |-> 0, epoch |-> 0,
                 round |-> EmptyRound]
RECURSIVE Product(_)
Product(s) == IF Len(s) = 0 THEN 1 ELSE Head(s) * Product(Tail(s))
Unique(s) == Len(s) = Cardinality({s[i] : i \in 1..Len(s)})

Init ==
    /\ version = 0 /\ holder = "none" /\ currentRound = 0
    /\ rounds = [e \in 1..MaxEpochs |-> EmptyRound]
    /\ pending = [p \in Workers |-> EmptyPending]
    /\ started = [p \in Workers |-> FALSE]
    /\ active = [p \in Workers |-> FALSE]
    /\ terminal = [p \in Workers |-> FALSE]
    /\ offAtStartup = [p \in Workers |-> FALSE]
    /\ startEpoch = [p \in Workers |-> 0]
    /\ lastEpoch = [p \in Workers |-> 0]
    /\ applied = [p \in Workers |-> <<>>]
    /\ factors = [p \in Workers |-> <<>>]
    /\ weight = [p \in Workers |-> 0]
    /\ staleCommit = FALSE

\* A complete census may retain or clear a round, or seal a new one.
\* Its fresh salted outcomes and numerical factor are immutable after CAS.
Prepare(p) ==
    /\ ~pending[p].ready /\ version < MaxEpochs
    /\ \E r \in Round :
        /\ (r = EmptyRound
            \/ (currentRound > 0 /\ r = rounds[currentRound])
            \/ (r.id = version + 1
                /\ (r.factor = 2 \/ r.kept = Workers)))
        /\ pending' = [pending EXCEPT ![p] =
            [ready |-> TRUE, base |-> version, epoch |-> version + 1,
             round |-> r]]
    /\ UNCHANGED <<version, holder, currentRound, rounds,
         started, active, terminal, offAtStartup, startEpoch, lastEpoch,
         applied, factors, weight, staleCommit>>

\* Leadership and round publication share this one conditional write.
\* The publisher may pause after Prepare while another publisher takes over.
Commit(p) ==
    /\ pending[p].ready
    /\ LET attempt == pending[p] IN
       /\ IF attempt.base = version \/ IgnoreCAS
          THEN /\ version' = attempt.epoch /\ holder' = p
               /\ currentRound' = attempt.round.id
               /\ rounds' = IF attempt.round.id = attempt.epoch
                            THEN [rounds EXCEPT ![attempt.epoch] = attempt.round]
                            ELSE rounds
          ELSE /\ UNCHANGED <<version, holder, currentRound, rounds>>
       /\ pending' = [pending EXCEPT ![p] = EmptyPending]
       /\ staleCommit' = (staleCommit \/ (IgnoreCAS /\ attempt.base # version))
    /\ UNCHANGED <<started, active, terminal, offAtStartup,
         startEpoch, lastEpoch, applied, factors, weight>>

\* A new invocation freezes its startup choice and skips existing rounds.
Start(p) ==
    /\ ~started[p]
    /\ \E enabled \in BOOLEAN :
        /\ started' = [started EXCEPT ![p] = TRUE]
        /\ active' = [active EXCEPT ![p] = enabled]
        /\ offAtStartup' = [offAtStartup EXCEPT ![p] = ~enabled]
        /\ weight' = [weight EXCEPT ![p] = IF enabled THEN 1 ELSE 0]
    /\ startEpoch' = [startEpoch EXCEPT ![p] = version]
    /\ lastEpoch' = [lastEpoch EXCEPT ![p] = version]
    /\ UNCHANGED <<version, holder, currentRound, rounds, pending,
         terminal, applied, factors, staleCommit>>

\* Only the latest observed round is applied. Earlier missed rounds are not
\* invented or multiplied into a weight: the process never drew those rounds.
Apply(p) ==
    /\ started[p] /\ active[p] /\ currentRound > 0
    /\ (currentRound > lastEpoch[p] \/ IgnoreEpochGuard)
    /\ Len(applied[p]) < MaxEpochs + 1
    /\ LET r == rounds[currentRound] IN
       /\ lastEpoch' = [lastEpoch EXCEPT ![p] = currentRound]
       /\ applied' = [applied EXCEPT ![p] = Append(@, currentRound)]
       /\ IF p \in r.kept
          THEN /\ factors' = [factors EXCEPT ![p] = Append(@, r.factor)]
               /\ weight' = [weight EXCEPT ![p] = @ * r.factor]
               /\ UNCHANGED <<active, terminal>>
          ELSE /\ active' = [active EXCEPT ![p] = FALSE]
               /\ terminal' = [terminal EXCEPT ![p] = TRUE]
               /\ weight' = [weight EXCEPT ![p] = 0]
               /\ UNCHANGED factors
    /\ UNCHANGED <<version, holder, currentRound, rounds, pending,
         started, offAtStartup, startEpoch, staleCommit>>

\* Application enable() leaves the sampler's choice unchanged. Recorder
\* enable/disable state is outside this model; explicit override is allowed.
EnableRequest(p) == /\ terminal[p] /\ UNCHANGED vars

Next == \E p \in Workers : Prepare(p) \/ Commit(p) \/ Start(p)
                              \/ Apply(p) \/ EnableRequest(p)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ version \in 0..MaxEpochs /\ holder \in Workers \cup {"none"}
    /\ currentRound \in 0..version
    /\ rounds \in [1..MaxEpochs -> Round]
    /\ \A p \in Workers :
        /\ started[p] \in BOOLEAN /\ active[p] \in BOOLEAN
        /\ terminal[p] \in BOOLEAN /\ offAtStartup[p] \in BOOLEAN
        /\ startEpoch[p] \in 0..MaxEpochs /\ lastEpoch[p] \in 0..MaxEpochs
        /\ applied[p] \in Seq(1..MaxEpochs)
        /\ factors[p] \in Seq({1, 2}) /\ weight[p] \in Nat
OnceOnly == \A p \in Workers : Unique(applied[p])
TerminalNeverEnable == \A p \in Workers : terminal[p] => ~active[p]
StartupOffNeverEnable == \A p \in Workers : offAtStartup[p] => ~active[p]
StartupSkipsOld == \A p \in Workers :
    \A i \in 1..Len(applied[p]) : applied[p][i] > startEpoch[p]
SealedRound == currentRound = 0 \/ rounds[currentRound].id = currentRound
WeightsMatchApplied == \A p \in Workers :
    IF active[p] THEN weight[p] = Product(factors[p]) ELSE weight[p] = 0
SealedFactorApplied == \A p \in Workers :
    /\ Len(factors[p]) <= Len(applied[p])
    /\ \A i \in 1..Len(factors[p]) :
        factors[p][i] = rounds[applied[p][i]].factor
CASFencing == ~staleCommit
\* A stronger, deliberately invalid claim used only to obtain a missed-round
\* witness: the implementation is allowed to miss a round while not polling.
NoMissedRound == \A p \in Workers :
    \A e \in 1..MaxEpochs :
        (rounds[e].id = e /\ e > startEpoch[p] /\ e < lastEpoch[p])
        => e \in {applied[p][i] : i \in 1..Len(applied[p])}
=============================================================================
