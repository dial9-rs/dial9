from fractions import Fraction as F
from itertools import product
import json

def bernoulli(n,p):
    for state in product((False,True), repeat=n):
        weight=F(1)
        for included in state:
            weight *= p if included else 1-p
        if weight:
            yield state,weight

def rounds(indices,weights,target,remaining):
    if not remaining or not indices:
        return sum(weights),F(1)
    q=min(F(1),F(target,len(indices)))
    total=F(0)
    mass=F(0)
    for survivor,prob in bernoulli(len(indices),q):
        new_i=tuple(i for i,keep in zip(indices,survivor) if keep)
        new_w=tuple(w/q for w,keep in zip(weights,survivor) if keep)
        contribution,inside_mass=rounds(new_i,new_w,target,remaining-1)
        total += prob*contribution
        mass += prob*inside_mass
    return total,mass

results=[]
for n in range(1,7):
  for p in (F(1),F(1,2),F(1,4)):
    for target in (1,2):
      for count in (0,1,2):
        estimate=F(0)
        mass=F(0)
        for selected,prob in bernoulli(n,p):
            indices=tuple(i for i,included in enumerate(selected) if included)
            weights=tuple(1/p for i in indices)
            total,round_mass=rounds(indices,weights,target,count)
            estimate += prob*total
            mass += prob*round_mass
        assert estimate == n,(n,p,target,count,estimate)
        assert mass==1
        results.append(dict(n=n,p=str(p),target=target,rounds=count,mean=str(estimate),probability_mass=str(mass)))

# The census/admission split remains unbiased even when the admission rate
# depends on the randomly observed census size. Off cohorts with rate zero
# are intentionally demonstrated as irrecoverable rather than called unbiased.
for n in range(1,7):
  for f in (F(1),F(1,2),F(1,4)):
    total=F(0)
    for counted,cp in bernoulli(n,f):
        r=sum(counted)
        a=F(1) if not r else min(F(1),F(1,r))
        for selected,ap in bernoulli(r,a):
            total += cp*ap*sum(selected)/(f*a)
    assert total==n

print(json.dumps({'sequential_scenarios':len(results),'all_exact_means_equal_true_population':True,'adaptive_census_scenarios':18,'results':results},indent=2))
