--------------------------- MODULE RosterSampling ---------------------------
EXTENDS Integers, FiniteSets, TLC

CONSTANTS Processes, MaxRevision, Mode, SplitObjects, ReuseKeys,
          LateEnable, AllowIncomplete

NoProcess == "none"
NoVersion == -1
States == {"cold", "unarmed", "active", "carrier", "ended"}
Host(p) == IF p \in {"A0", "A1"} THEN "A" ELSE p
MemberKey(p) == IF ReuseKeys THEN Host(p) ELSE p

VARIABLES revision, leaseRevision, holder, expired, observed,
          observedLease, observedHolder, observedExpired, localVersion,
          listed, collected, publishedCount, badFence, badIncomplete,
          state, decision, everCarrier, members, pendingCleanup, badCleanup

vars == <<revision, leaseRevision, holder, expired, observed,
          observedLease, observedHolder, observedExpired, localVersion,
          listed, collected, publishedCount, badFence, badIncomplete,
          state, decision, everCarrier, members, pendingCleanup, badCleanup>>

LeaderVars == <<revision, leaseRevision, holder, expired, observed,
               observedLease, observedHolder, observedExpired, localVersion,
               listed, collected, publishedCount, badFence, badIncomplete>>
LifeVars == <<state, decision, everCarrier, members, pendingCleanup, badCleanup>>

Init ==
  /\ revision = 0
  /\ leaseRevision = 0
  /\ holder = NoProcess
  /\ expired = TRUE
  /\ observed = [p \in Processes |-> NoVersion]
  /\ observedLease = [p \in Processes |-> NoVersion]
  /\ observedHolder = [p \in Processes |-> NoProcess]
  /\ observedExpired = [p \in Processes |-> FALSE]
  /\ localVersion = [p \in Processes |-> NoVersion]
  /\ listed = [p \in Processes |-> "none"]
  /\ collected = [p \in Processes |-> {}]
  /\ publishedCount = NoVersion
  /\ badFence = FALSE
  /\ badIncomplete = FALSE
  /\ state = [p \in Processes |-> "cold"]
  /\ decision = [p \in Processes |-> "undecided"]
  /\ everCarrier = {}
  /\ members = {}
  /\ pendingCleanup = {}
  /\ badCleanup = FALSE

\* A GET is the only way to confirm ownership after a lost acknowledgement.
Read(p) ==
  /\ observed' = [observed EXCEPT ![p] = revision]
  /\ observedLease' = [observedLease EXCEPT ![p] = leaseRevision]
  /\ observedHolder' = [observedHolder EXCEPT ![p] = holder]
  /\ observedExpired' = [observedExpired EXCEPT ![p] = expired]
  /\ localVersion' = [localVersion EXCEPT
       ![p] = IF holder = p /\ ~expired THEN revision ELSE NoVersion]
  /\ listed' = [listed EXCEPT ![p] = "none"]
  /\ collected' = [collected EXCEPT ![p] = {}]
  /\ UNCHANGED <<revision, leaseRevision, holder, expired, publishedCount,
                 badFence, badIncomplete, LifeVars>>

\* Both known and lost acknowledgements commit the same atomic document.
Claim(p, acknowledged) ==
  /\ observedExpired[p] \/ observedHolder[p] = NoProcess
  /\ IF SplitObjects
     THEN observedLease[p] = leaseRevision /\ leaseRevision < MaxRevision
     ELSE observed[p] = revision /\ revision < MaxRevision
  /\ revision' = IF SplitObjects THEN revision ELSE revision + 1
  /\ leaseRevision' = IF SplitObjects THEN leaseRevision + 1 ELSE leaseRevision
  /\ holder' = p
  /\ expired' = FALSE
  /\ localVersion' = [localVersion EXCEPT
       ![p] = IF acknowledged THEN revision' ELSE NoVersion]
  /\ listed' = [listed EXCEPT ![p] = "none"]
  /\ collected' = [collected EXCEPT ![p] = {}]
  /\ UNCHANGED <<observed, observedLease, observedHolder, observedExpired,
                 publishedCount, badFence, badIncomplete, LifeVars>>

Expire ==
  /\ holder # NoProcess /\ ~expired
  /\ expired' = TRUE
  /\ UNCHANGED <<revision, leaseRevision, holder, observed, observedLease,
                 observedHolder, observedExpired, localVersion, listed,
                 collected, publishedCount, badFence, badIncomplete, LifeVars>>

\* A paused former leader may continue listing. Its final CAS still fences it.
ListPartial(p) ==
  /\ localVersion[p] # NoVersion /\ listed[p] = "none"
  /\ listed' = [listed EXCEPT ![p] = "partial"]
  /\ collected' = [collected EXCEPT ![p] = {}]
  /\ UNCHANGED <<revision, leaseRevision, holder, expired, observed,
                 observedLease, observedHolder, observedExpired, localVersion,
                 publishedCount, badFence, badIncomplete, LifeVars>>

ListComplete(p) ==
  /\ listed[p] = "partial"
  /\ listed' = [listed EXCEPT ![p] = "complete"]
  /\ collected' = [collected EXCEPT ![p] = Processes]
  /\ UNCHANGED <<revision, leaseRevision, holder, expired, observed,
                 observedLease, observedHolder, observedExpired, localVersion,
                 publishedCount, badFence, badIncomplete, LifeVars>>

Publish(p, acknowledged) ==
  /\ localVersion[p] # NoVersion
  /\ listed[p] = "complete" \/ (AllowIncomplete /\ listed[p] = "partial")
  /\ localVersion[p] = revision /\ revision < MaxRevision
  /\ revision' = revision + 1
  /\ expired' = FALSE
  /\ publishedCount' = Cardinality(collected[p])
  /\ badFence' = (badFence \/ holder # p)
  /\ badIncomplete' = (badIncomplete \/ listed[p] # "complete")
  /\ localVersion' = [localVersion EXCEPT
       ![p] = IF acknowledged THEN revision' ELSE NoVersion]
  /\ listed' = [listed EXCEPT ![p] = "none"]
  /\ collected' = [collected EXCEPT ![p] = {}]
  /\ UNCHANGED <<leaseRevision, holder, observed, observedLease,
                 observedHolder, observedExpired, LifeVars>>

RejectPublication(p) ==
  /\ localVersion[p] # NoVersion /\ localVersion[p] # revision
  /\ localVersion' = [localVersion EXCEPT ![p] = NoVersion]
  /\ listed' = [listed EXCEPT ![p] = "none"]
  /\ collected' = [collected EXCEPT ![p] = {}]
  /\ UNCHANGED <<revision, leaseRevision, holder, expired, observed,
                 observedLease, observedHolder, observedExpired, publishedCount,
                 badFence, badIncomplete, LifeVars>>

FailList(p) ==
  /\ listed[p] # "none"
  /\ listed' = [listed EXCEPT ![p] = "none"]
  /\ collected' = [collected EXCEPT ![p] = {}]
  /\ UNCHANGED <<revision, leaseRevision, holder, expired, observed,
                 observedLease, observedHolder, observedExpired, localVersion,
                 publishedCount, badFence, badIncomplete, LifeVars>>

\* The startup boolean abstracts a rank comparison or the caller's fallback.
Start(p, enabled) ==
  /\ state[p] = "cold"
  /\ state' = [state EXCEPT ![p] = IF enabled THEN "active" ELSE "unarmed"]
  /\ decision' = [decision EXCEPT ![p] = IF enabled THEN "on" ELSE "off"]
  /\ UNCHANGED <<everCarrier, members, pendingCleanup, badCleanup, LeaderVars>>

Heartbeat(p) ==
  /\ state[p] \in {"unarmed", "active", "carrier"}
  /\ members' = members \cup {p}
  /\ UNCHANGED <<state, decision, everCarrier, pendingCleanup, badCleanup,
                 LeaderVars>>

Trim(p) ==
  /\ state[p] = "active"
  /\ state' = [state EXCEPT ![p] = "carrier"]
  /\ everCarrier' = everCarrier \cup {p}
  /\ UNCHANGED <<decision, members, pendingCleanup, badCleanup, LeaderVars>>

End(p) ==
  /\ state[p] \in {"unarmed", "active", "carrier"}
  /\ state' = [state EXCEPT ![p] = "ended"]
  /\ pendingCleanup' = pendingCleanup \cup {p}
  /\ UNCHANGED <<decision, everCarrier, members, badCleanup, LeaderVars>>

Cleanup(p) ==
  /\ p \in pendingCleanup
  /\ LET victims == {q \in members: MemberKey(q) = MemberKey(p)}
     IN /\ members' = members \ victims
        /\ badCleanup' = (badCleanup \/ (victims \ {p}) # {})
  /\ pendingCleanup' = pendingCleanup \ {p}
  /\ UNCHANGED <<state, decision, everCarrier, LeaderVars>>

EnableLater(p) ==
  /\ LateEnable /\ state[p] \in {"unarmed", "carrier"}
  /\ state' = [state EXCEPT ![p] = "active"]
  /\ UNCHANGED <<decision, everCarrier, members, pendingCleanup, badCleanup,
                 LeaderVars>>

LeaderNext ==
  Expire \/ (\E p \in Processes:
    Read(p) \/ ListPartial(p) \/ ListComplete(p) \/ FailList(p)
    \/ RejectPublication(p)
    \/ (\E acknowledged \in BOOLEAN:
        Claim(p, acknowledged) \/ Publish(p, acknowledged)))

LifeNext ==
  \E p \in Processes:
    Heartbeat(p) \/ Trim(p) \/ End(p) \/ Cleanup(p) \/ EnableLater(p)
    \/ (\E enabled \in BOOLEAN: Start(p, enabled))

Next == IF Mode = "leader" THEN LeaderNext ELSE LifeNext
Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ revision \in 0..MaxRevision
  /\ leaseRevision \in 0..MaxRevision
  /\ holder \in Processes \cup {NoProcess}
  /\ expired \in BOOLEAN
  /\ state \in [Processes -> States]
  /\ members \subseteq Processes
  /\ pendingCleanup \subseteq Processes
  /\ everCarrier \subseteq Processes
  /\ publishedCount \in {NoVersion} \cup 0..Cardinality(Processes)
FencedPublication == ~badFence
CompletePassOnly == ~badIncomplete
NoLateEnable == \A p \in Processes: decision[p] = "off" => state[p] # "active"
TerminalTrim == \A p \in everCarrier: state[p] \in {"carrier", "ended"}
IncarnationIsolation == ~badCleanup
=============================================================================
