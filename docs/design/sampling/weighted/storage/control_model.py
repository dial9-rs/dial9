from collections import deque
from itertools import product
import json

# Model the relevant protocol guards, including publishing stale CAS attempts,
# repeated poll delivery, a new process reading an already-published round,
# and terminal stop. Timing-based availability is deliberately abstracted away.
def check(draws):
    # roster=(version,epoch,round), pending writes for two publishers,
    # workers=(started,active,last_round,applied_rounds)
    initial=((0,0,0),(None,None),((False,True,0,()),(False,True,0,())))
    seen={initial}
    todo=deque([initial])
    edges=0
    stale=0
    while todo:
        state=todo.popleft()
        roster,pending,workers=state
        version,epoch,round_epoch=roster
        successors=[]
        for publisher in range(2):
            if pending[publisher] is None and version<3:
                for new_round in (0,round_epoch,epoch+1):
                    change=list(pending)
                    change[publisher]=(version,epoch+1,new_round)
                    successors.append((roster,tuple(change),workers))
            elif pending[publisher] is not None:
                base,new_epoch,new_round=pending[publisher]
                change=list(pending)
                change[publisher]=None
                if version==base:
                    assert new_epoch==epoch+1
                    successors.append(((version+1,new_epoch,new_round),tuple(change),workers))
                else:
                    stale+=1
                    successors.append((roster,tuple(change),workers))
        for i,worker in enumerate(workers):
            started,active,last,applied=worker
            if not started:
                # Startup ignores all pre-existing published rounds.
                updated=(True,True,epoch,())
            elif active and round_epoch>last:
                assert round_epoch not in applied
                keep=draws[(i,round_epoch)]
                updated=(True,keep,round_epoch,applied+(round_epoch,))
            else:
                continue
            change=list(workers)
            change[i]=updated
            successors.append((roster,pending,tuple(change)))
        for successor in successors:
            edges+=1
            for before,after in zip(workers,successor[2]):
                assert before[1] or not after[1], 'terminal process resumed'
                assert len(after[3])==len(set(after[3])), 'duplicate thinning'
                assert after[2]>=before[2], 'last epoch regressed'
            if successor not in seen:
                seen.add(successor)
                todo.append(successor)
    return len(seen),edges,stale

states=edges=stale=0
for bits in product((False,True),repeat=6):
    draws={(i,j):bits[i*3+j-1] for i in range(2) for j in range(1,4)}
    s,e,c=check(draws)
    states+=s;edges+=e;stale+=c
print(json.dumps(dict(draw_assignments=64,total_states=states,total_edges=edges,stale_cas_transitions=stale,once_only=True,terminal_no_resume=True,startup_skips_old_rounds=True,note='Small abstract model of guards, not an exhaustive model of all Rust/storage/timing behavior'),indent=2))
