-------------------------- MODULE BoundedCensus --------------------------
EXTENDS Integers, FiniteSets, TLC

CONSTANTS RerollCurrent, UncappedList, ShortPageExhaustion, DoubleTrim,
          UnfencedResult, UnfencedElection, ReservationFreshens, LateEnable
Processes == {"a", "b"}
Channels == {"census", "profilers"}
NoWindow == -1
MaxVersion == 4

VARIABLE s
vars == <<s>>

Init ==
  s = [window |-> 1, version |-> 0, owner |-> "none", expired |-> TRUE,
       readVersion |-> [p \in Processes |-> 0],
       holderToken |-> [p \in Processes |-> 0],
       lastReserved |-> NoWindow,
       reservation |-> [p \in Processes |-> NoWindow],
       reserveVersion |-> [p \in Processes |-> 0],
       listMask |-> [p \in Processes |-> {}],
       listCount |-> [w \in 0..1 |-> 0],
       truncated |-> [p \in Processes |-> FALSE],
       populationSource |-> NoWindow,
       control |-> 0, cache |-> [p \in Processes |-> 0],
       frozen |-> [p \in Processes |-> 0],
       snapshot |-> [p \in Processes |-> 0],
       rank |-> [p \in Processes |-> IF p = "a" THEN 0 ELSE 1],
       rankSnapshot |-> [p \in Processes |-> IF p = "a" THEN 0 ELSE 1],
       staleResult |-> FALSE, staleElection |-> FALSE,
       fabricatedEmpty |-> FALSE, fabricatedFresh |-> FALSE,
       trimPublished |-> FALSE, trimKnown |-> {},
       applied |-> {}, weightPower |-> 0,
       startup |-> {"a"}, profiling |-> {"a"}, stopped |-> {}]

(* A read is available to every process, including the initially disabled b.
   Periodic clock bounds are checked separately; network/scheduler fairness
   is not asserted by this finite safety abstraction. *)
Read(p) ==
  s' = [s EXCEPT !.readVersion[p] = s.version, !.cache[p] = s.control,
        !.frozen[p] = IF RerollCurrent THEN s.control ELSE @,
        !.rank[p] = IF RerollCurrent /\ s.control # s.cache[p]
                   THEN 1 - @ ELSE @,
        !.trimKnown = IF s.trimPublished /\ s.window = 1
                      THEN @ \cup {p} ELSE @]

Expire ==
  /\ ~s.expired /\ s.version < MaxVersion
  /\ s' = [s EXCEPT !.expired = TRUE]

Claim(p) ==
  /\ s.expired /\ s.version < MaxVersion
  /\ (s.readVersion[p] = s.version \/ UnfencedElection)
  /\ s' = [s EXCEPT !.owner = p, !.expired = FALSE,
        !.version = @ + 1, !.holderToken[p] = s.version + 1,
        !.staleElection = @ \/ (s.readVersion[p] # s.version)]

Reserve(p) ==
  /\ p = s.owner /\ ~s.expired /\ s.holderToken[p] = s.version
  /\ s.lastReserved < s.window - 1 /\ s.version < MaxVersion
  /\ s' = [s EXCEPT !.version = @ + 1,
        !.holderToken[p] = s.version + 1,
        !.lastReserved = s.window - 1,
        !.reservation[p] = s.window - 1,
        !.reserveVersion[p] = s.version + 1, !.listMask[p] = {},
        !.truncated[p] = FALSE,
        !.populationSource = IF ReservationFreshens THEN s.window - 1 ELSE @,
        !.fabricatedFresh = @ \/ ReservationFreshens]

(* Local requests may finish after takeover and after a later source is reserved.
   A durable monotonic reservation prevents another owner reissuing this source. *)
List(p, channel, short) ==
  /\ s.reservation[p] # NoWindow
  /\ (channel \notin s.listMask[p] \/ UncappedList)
  /\ s.listCount[s.reservation[p]] < 3
  /\ s' = [s EXCEPT
        !.listCount[s.reservation[p]] = @ + 1,
        !.listMask[p] = @ \cup {channel},
        !.truncated[p] = IF channel = "census" THEN short ELSE @,
        !.fabricatedEmpty = @ \/
            (channel = "census" /\ short /\ ShortPageExhaustion)]

Result(p) ==
  /\ s.reservation[p] # NoWindow /\ s.listMask[p] = Channels
  /\ s.version < MaxVersion
  /\ ((p = s.owner /\ s.reserveVersion[p] = s.version) \/ UnfencedResult)
  /\ s' = [s EXCEPT !.version = @ + 1,
        !.holderToken[p] = s.version + 1,
        !.reserveVersion[p] = 0,
        !.staleResult = @ \/ (p # s.owner \/ s.reserveVersion[p] # s.version),
        !.control = IF s.truncated[p] THEN @ ELSE 1,
        !.populationSource = IF s.truncated[p] THEN @ ELSE s.reservation[p]]

NextWindow ==
  /\ s.window = 1
  /\ \E ranks \in [Processes -> 0..1]:
      s' = [s EXCEPT !.window = 2, !.frozen = s.cache, !.snapshot = s.cache,
            !.rank = ranks, !.rankSnapshot = ranks]

PublishTrim ==
  /\ s.window = 1 /\ ~s.trimPublished
  /\ s' = [s EXCEPT !.trimPublished = TRUE]

SurviveTrim ==
  /\ s.window = 2 /\ s.trimPublished /\ "a" \in s.trimKnown
  /\ "a" \in s.profiling /\ ("a" \notin s.applied \/ DoubleTrim)
  /\ s.weightPower < 2
  /\ s' = [s EXCEPT !.applied = @ \cup {"a"}, !.weightPower = @ + 1]

Stop(p) ==
  /\ p \in s.profiling
  /\ s' = [s EXCEPT !.profiling = @ \ {p}, !.stopped = @ \cup {p}]

Enable(p) ==
  /\ LateEnable /\ p \notin s.profiling
  /\ s' = [s EXCEPT !.profiling = @ \cup {p}]

Next ==
  Expire \/ NextWindow \/ PublishTrim \/ SurviveTrim \/
  (\E p \in Processes: Read(p) \/ Claim(p) \/ Reserve(p) \/ Result(p)
     \/ Stop(p) \/ Enable(p)
     \/ (\E channel \in Channels, short \in BOOLEAN: List(p, channel, short)))

Spec == Init /\ [][Next]_vars
FrozenLottery == s.frozen = s.snapshot /\ s.rank = s.rankSnapshot
BoundedLists == \A w \in 0..1: s.listCount[w] <= 2
NoFalseExhaustion == ~s.fabricatedEmpty
CASFenced == ~s.staleResult /\ ~s.staleElection
ReservationIsNotPopulation == ~s.fabricatedFresh
OnceOnly == s.weightPower <= 1
NoReenable == s.profiling \subseteq s.startup /\ s.profiling \cap s.stopped = {}
=============================================================================
