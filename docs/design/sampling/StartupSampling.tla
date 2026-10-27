-------------------------- MODULE StartupSampling --------------------------
EXTENDS Integers, FiniteSets, TLC
CONSTANTS N, K, LeaseTicks, MaxToken, ContinueProfiling, ProbabilityFallback
Hosts == 1..N
Slots == 1..K
VARIABLES state, frozen, terminal, holder, token, left, request, response, owned
vars == <<state, frozen, terminal, holder, token, left, request, response, owned>>
Empty == [slot |-> 0, expected |-> 0, op |-> "none"]
Init ==
  /\ state = [h \in Hosts |-> "preparing"]
  /\ frozen = [h \in Hosts |-> FALSE]
  /\ terminal = [h \in Hosts |-> FALSE]
  /\ holder = [i \in Slots |-> 0]
  /\ token = [i \in Slots |-> 0]
  /\ left = [i \in Slots |-> 0]
  /\ request = [h \in Hosts |-> Empty]
  /\ response = [h \in Hosts |-> "none"]
  /\ owned = [h \in Hosts |-> 0]

Claim(h, i) ==
  /\ state[h] = "preparing" /\ ~frozen[h]
  /\ request[h].op = "none"
  /\ (holder[i] = 0 \/ left[i] = 0)
  /\ request' = [request EXCEPT ![h] = [slot |-> i, expected |-> token[i], op |-> "claim"]]
  /\ UNCHANGED <<state, frozen, terminal, holder, token, left, response, owned>>

Apply(h) ==
  /\ request[h].op # "none" /\ response[h] = "none"
  /\ LET r == request[h] IN
     IF token[r.slot] = r.expected /\ token[r.slot] < MaxToken
        /\ (r.op = "claim" \/ holder[r.slot] = h)
     THEN /\ holder' = [holder EXCEPT ![r.slot] = h]
          /\ token' = [token EXCEPT ![r.slot] = @ + 1]
          /\ left' = [left EXCEPT ![r.slot] = LeaseTicks]
          /\ response' = [response EXCEPT ![h] = "ok"]
     ELSE /\ response' = [response EXCEPT ![h] = "conflict"]
          /\ UNCHANGED <<holder, token, left>>
  /\ UNCHANGED <<state, frozen, terminal, request, owned>>

Ack(h) ==
  /\ response[h] # "none"
  /\ LET i == request[h].slot IN
     IF state[h] = "preparing" /\ ~frozen[h] /\ response[h] = "ok" /\ holder[i] = h /\ left[i] > 1
     THEN /\ state' = [state EXCEPT ![h] = "on"]
          /\ frozen' = [frozen EXCEPT ![h] = TRUE]
          /\ owned' = [owned EXCEPT ![h] = i]
     ELSE /\ UNCHANGED <<state, frozen, owned>>
  /\ request' = [request EXCEPT ![h] = Empty]
  /\ response' = [response EXCEPT ![h] = "none"]
  /\ UNCHANGED <<terminal, holder, token, left>>

Timeout(h) ==
  /\ state[h] = "preparing" /\ ~frozen[h]
  /\ state' = [state EXCEPT ![h] = IF ProbabilityFallback THEN "on" ELSE "off"]
  /\ frozen' = [frozen EXCEPT ![h] = TRUE]
  \* An outstanding write survives the waiter's timeout and can still apply.
  /\ UNCHANGED <<terminal, holder, token, left, request, response, owned>>

Renew(h) ==
  /\ state[h] = "on" /\ owned[h] > 0
  /\ request[h].op = "none" /\ holder[owned[h]] = h
  /\ request' = [request EXCEPT ![h] = [slot |-> owned[h], expected |-> token[owned[h]], op |-> "renew"]]
  /\ UNCHANGED <<state, frozen, terminal, holder, token, left, response, owned>>

Tick ==
  /\ left' = [i \in Slots |-> IF left[i] > 0 THEN left[i] - 1 ELSE 0]
  \* Strict mode assumes its local stop is enforced before server reuse.
  /\ state' = [h \in Hosts |->
       IF state[h] = "on" /\ owned[h] > 0
          /\ (~ContinueProfiling)
          /\ (left[owned[h]] <= 1 \/ holder[owned[h]] # h)
       THEN "stopped" ELSE state[h]]
  /\ terminal' = [h \in Hosts |-> terminal[h] \/ state'[h] = "stopped"]
  /\ UNCHANGED <<frozen, holder, token, request, response, owned>>

Exit(h) ==
  /\ state[h] # "dead"
  /\ state' = [state EXCEPT ![h] = "dead"]
  /\ terminal' = [terminal EXCEPT ![h] = TRUE]
  /\ UNCHANGED <<frozen, holder, token, left, request, response, owned>>

Next == Tick \/ (\E h \in Hosts:
    Timeout(h) \/ Apply(h) \/ Ack(h) \/ Renew(h) \/ Exit(h)
    \/ (\E i \in Slots: Claim(h, i)))
Spec == Init /\ [][Next]_vars
NoBounceBack == \A h \in Hosts: terminal[h] => state[h] # "on"
FrozenOff == \A h \in Hosts: frozen[h] /\ state[h] = "off" => owned[h] = 0
CoordinatedCap == Cardinality({holder[i] : i \in {j \in Slots: holder[j] # 0 /\ left[j] > 0}}) <= K
ActualCap == Cardinality({h \in Hosts: state[h] = "on"}) <= K
=============================================================================
