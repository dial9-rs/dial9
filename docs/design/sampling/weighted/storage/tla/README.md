# Bernoulli thinning control safety

`BernoulliControl.tla` models the schema-3 sampler's control behavior with two
workers and two committed roster epochs. A candidate round chooses an immutable
survival factor and a set of surviving workers before its conditional publish.
The accepted epoch stands for a fresh salt; all later readers of that round see
the same outcomes and factor. Nondeterministic survivor sets cover every coin
outcome, without asserting a probability distribution in TLA+.

The one roster version fences both leadership changes and round publication.
A publisher can pause with an old version while another takes over, then lose
its CAS. Workers can start after a round was already published, repeatedly read
one round, or miss an earlier round while not polling. A survivor applies only
the latest eligible epoch, exactly once, and records its factor. A failed coin
records terminal zero and cannot resume, including an explicit enable request.
An initially off startup remains off. New startups initialize their last epoch
from the roster and skip all already-published rounds.

The safety configuration checks typing, once-only decisions, no terminal or
startup-off resurrection, skipping old rounds, sealed round identity, factors
matching their applied sealed rounds, weight products, and CAS fencing. No
count/cap invariant is present: Bernoulli trimming has a probabilistic count.

The model deliberately abstracts storage timeouts, clock skew, guarded census
pagination, membership ghosts, cleanup, cooldown, startup selection rates,
cryptographic entropy, and trace checkpoint ordering. It overapproximates which
publisher may attempt a write; lease expiry is not needed for its CAS safety
property. The production tests and prior roster/census models cover the
storage/timing concerns. This is bounded safety exploration, not a liveness
proof, an unbiasedness proof, or a model of every Rust interleaving. See
`../../statistics/weighting-proof.md`, `../exact_moments.py`, and `../control_model.py` for the
statistical derivation and independent checks.

Run from this directory with any compatible local `tla2tools.jar`:

```sh
python3 run_tlc.py /absolute/path/to/tla2tools.jar
```

The runner checks exit codes and expected invariant messages and records exact
commands and state counts in `results.json`. The direct invocation is:

```sh
java -XX:+UseParallelGC -Xmx2g -cp /absolute/path/to/tla2tools.jar \
  tlc2.TLC -workers 2 -config safety.cfg BernoulliControl.tla
```

Controls:

- `negative-duplicate-epoch.cfg` removes the last-observed-epoch guard. TLC must
  violate `OnceOnly` after applying a survivor round twice.
- `negative-stale-cas.cfg` accepts conditional writes with stale versions. TLC
  must violate `CASFencing` when a paused contender overwrites the winner.
- `missed-round-witness.cfg` keeps both guards and checks the deliberately
  stronger claim that no previously published round is missed. Its expected
  counterexample demonstrates a worker applying round 2 without round 1;
  missing that round is allowed, and the weight must omit its factor.

All three counterexamples are expected validation outcomes, not findings in
the production implementation. Logs retain the full textual traces. Generated
TLC state files and binary trace-exploration files are not review resources.
