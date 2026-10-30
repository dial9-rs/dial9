#!/usr/bin/env python3
"""Exact independent probability enumeration for time-slice flamegraph weights.

No sampling implementation is imported. Bernoulli branches use Fraction, with
heterogeneous startup probabilities, exogenous lifetimes and CPU workloads,
two control decisions adaptive to the selected cohort, and source-born trim
exemptions. Coins are independent conditional on the earlier branch state.
"""
from dataclasses import dataclass, replace
from fractions import Fraction as F
from itertools import product
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent


@dataclass(frozen=True)
class Unit:
    name: str
    born: int
    dies: int
    startup_p: F

    def alive(self, now):
        return self.born <= now < self.dies

    def cpu(self, now):
        return {'a': 4 if now < 180 else 7,
                'b': 3,
                'c': 5 if now < 240 else 2,
                'd': 9}[self.name]


UNITS = (Unit('a', 0, 300, F(1, 2)),
         Unit('b', 0, 170, F(1, 3)),
         Unit('c', 80, 260, F(2, 3)),
         Unit('d', 190, 330, F(1, 4)))
WINDOW = 60


@dataclass(frozen=True)
class State:
    probability: F
    selected: tuple[bool, ...]
    weight: tuple[F, ...]
    missing: tuple[F, ...]
    doubled: tuple[F, ...]


def startup():
    for coins in product([False, True], repeat=len(UNITS)):
        probability = F(1)
        weights = []
        for unit, coin in zip(UNITS, coins):
            probability *= unit.startup_p if coin else 1 - unit.startup_p
            weights.append(1 / unit.startup_p if coin else F(0))
        weights = tuple(weights)
        yield State(probability, coins, weights, weights, weights)


def adaptive_trim(states, *, now, source_window, first):
    output = []
    for state in states:
        eligible = tuple(i for i, (unit, selected) in enumerate(zip(UNITS, state.selected))
                         if selected and unit.alive(now)
                         and unit.born // WINDOW <= source_window)
        # Adaptive probabilities depend on earlier selected members. Later coins
        # remain independent of this control decision. Both trims are nontrivial.
        q = (F(1, 2) if len(eligible) >= 2 else F(3, 4)) if first else (
            F(1, 3) if len(eligible) >= 2 else F(2, 3))
        for outcomes in product([False, True], repeat=len(eligible)):
            probability = state.probability
            selected = list(state.selected)
            actual, missing, doubled = map(list, (state.weight, state.missing, state.doubled))
            for i, survives in zip(eligible, outcomes):
                probability *= q if survives else 1 - q
                selected[i] = survives
                if survives:
                    actual[i] /= q
                    if not first:
                        missing[i] /= q
                    doubled[i] /= q * q if first else q
                else:
                    actual[i] = missing[i] = doubled[i] = F(0)
            for i in set(range(len(UNITS))) - set(eligible):
                assert selected[i] == state.selected[i]
                assert actual[i] == state.weight[i]
                assert missing[i] == state.missing[i]
                assert doubled[i] == state.doubled[i]
            output.append(State(probability, tuple(selected), tuple(actual),
                                tuple(missing), tuple(doubled)))
    assert sum((state.probability for state in output), F(0)) == 1
    return output


def observe(states, now):
    truth = sum(unit.cpu(now) for unit in UNITS if unit.alive(now))
    population = sum(unit.alive(now) for unit in UNITS)
    moments = dict(actual=F(0), missing_first_q=F(0), double_first_q=F(0),
                   roster_count_substitution=F(0))
    per_unit = [F(0)] * len(UNITS)
    for state in states:
        active = [i for i, (unit, selected) in enumerate(zip(UNITS, state.selected))
                  if selected and unit.alive(now)]
        for key, weights in [('actual', state.weight), ('missing_first_q', state.missing),
                             ('double_first_q', state.doubled)]:
            value = sum((weights[i] * UNITS[i].cpu(now) for i in active), F(0))
            moments[key] += state.probability * value
        # Even an exactly correct current population is insufficient to replace
        # the actual unequal selection probabilities with a population/count ratio.
        if active:
            moments['roster_count_substitution'] += (
                state.probability * F(population, len(active))
                * sum(UNITS[i].cpu(now) for i in active))
        for i in active:
            per_unit[i] += state.probability * state.weight[i] * UNITS[i].cpu(now)
    assert moments['actual'] == truth
    for unit, value in zip(UNITS, per_unit):
        assert value == (unit.cpu(now) if unit.alive(now) else 0)
    return dict(time_slice_seconds=now, true_live_processes=population,
                true_cpu_work=truth, exact_means={k: str(v) for k, v in moments.items()},
                exact_per_unit_actual_cpu_work={u.name: str(v) for u, v in zip(UNITS, per_unit)})


def main():
    states = list(startup())
    rows = [observe(states, 110)]
    states = adaptive_trim(states, now=120, source_window=0, first=True)
    rows += [observe(states, 130), observe(states, 200)]
    states = adaptive_trim(states, now=240, source_window=2, first=False)
    rows += [observe(states, 250), observe(states, 310)]
    for mutant in ['missing_first_q', 'double_first_q', 'roster_count_substitution']:
        assert any(F(row['exact_means'][mutant]) != row['true_cpu_work'] for row in rows)
    result = dict(scope='Exact independent probability oracle; aligned time slices, '
                  'positive support, independent conditional coins, exogenous '
                  'lifetimes/workloads, and complete trace delivery. No network/Rust '
                  'refinement or finite-sample closeness claim.',
                  units=[dict(name=u.name, born=u.born, dies=u.dies, startup_p=str(u.startup_p))
                         for u in UNITS],
                  adaptive_trims=[dict(effective_second=120, source_window=0),
                                  dict(effective_second=240, source_window=2)],
                  branch_probability_mass=str(sum((s.probability for s in states), F(0))),
                  final_branch_count=len(states), snapshots=rows)
    (HERE / "weight-lineage-results.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
