------------------------------ MODULE OffByOne ------------------------------
EXTENDS Integers, FiniteSets
VARIABLE selected
Ranks == {0, 1, 2}
Target == 2
\* The target-th rank is one; a strict comparison excludes that member.
Init == selected = {r \in Ranks: r < 1}
Next == UNCHANGED selected
Spec == Init /\ [][Next]_selected
DesiredCount == Cardinality(selected) = Target
=============================================================================
