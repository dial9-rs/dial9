/* Deterministic model checks, independent of the browser rendering. */
const assert = require("node:assert/strict");
const S = require("./simulator.js");

assert.deepEqual(S.tracking(15, 0), { bits: 0, fraction: 1 });
assert.deepEqual(S.tracking(15, 100000), { bits: 7, fraction: 1 / 128 });
assert.equal(S.tracking(15, 479).fraction, 1);
assert.equal(S.tracking(15, 960).fraction, 1 / 2);

const cold = S.simulate({ scenario: "cold", hosts: 1000, seed: 9 });
assert.equal(cold.summary.processes, 1000);
assert.equal(cold.summary.hosts, 1000);
assert.equal(cold.summary.profiling, 0, "original disabled cohort never enables");
assert.equal(cold.summary.everSelected, 0);
assert(cold.archives.some(o => o.settled));
assert(cold.archives.every(o => o.noSupport === 1000 && o.unarmed === 1000));
assert.equal(cold.summary.projected, 1000, "full clean membership census has exact population");
assert.deepEqual(cold, S.simulate({ scenario: "cold", hosts: 1000, seed: 9 }));

const bootstrap = S.simulate({ scenario: "cold", hosts: 1000, fallback: "assume", assumed: 1000, seed: 9 });
assert(bootstrap.summary.profiling > 0);
assert(bootstrap.summary.expanded > 0);
assert.equal(bootstrap.summary.unsupported, 0);
assert.equal(bootstrap.summary.projected, 1000);
assert(bootstrap.archives.every(o => o.noSupport === 0));

const rolling = S.simulate({ scenario: "rolling", hosts: 500, seed: 4 });
assert.equal(rolling.summary.hosts, 500);
assert.equal(rolling.summary.processes, 500);
assert(rolling.snapshots.some(r => r.processes > r.hosts), "overlapping incarnations count separately");
assert.equal(rolling.summary.projected, 500);

const minute = S.simulate({ scenario: "minute", hosts: 20, seed: 4 });
const first = minute.snapshots.find(r => r.time === 0);
assert.equal(first.hosts, 1);
assert.equal(minute.summary.hosts, 20);
assert(minute.summary.everSelected > 0, "later starts can use a settled cutoff");

const containers = S.simulate({ scenario: "containers", hosts: 60, containers: 12, seed: 12, crash: 0 });
assert.equal(containers.summary.hosts, 60);
assert.equal(containers.summary.processes, 720);
assert(containers.summary.profiledHosts <= containers.summary.profiling);
assert.equal(containers.summary.projected, 720);

const ghosts = S.simulate({ scenario: "cold", hosts: 200, lifetime: 60, crash: 1, duration: 720 });
assert(ghosts.snapshots.some(r => r.projected > r.processes), "crash ghosts create non-Gaussian overcount");
assert(ghosts.summary.processes === 200);
assert(ghosts.snapshots.some(r => r.ghosts > 0));

const outage = S.simulate({ scenario: "cold", hosts: 100, lifetime: 90, fallback: "assume", assumed: 100,
  outageStart: 180, outageDuration: 180, crash: 0, duration: 900 });
assert(outage.snapshots.some(r => r.outage && r.age > 60));
assert(outage.archives.every(o => o.time < 180 || o.time >= 360));

const full = S.distributions(10000, 15, 1, 7, 3000);
assert.equal(full.censusSdPercent, 0);
assert(full.census.every(e => e === 0));
assert(Math.abs(full.profilerMoments.mean - 15) < .5);
assert(Math.abs(full.profilerMoments.sd - Math.sqrt(15 * (1 - 15 / 10000))) < .3);
const sparse = S.distributions(10000, 15, 1 / 16, 7, 3000);
assert(Math.abs(sparse.censusMoments.mean) < .5);
assert(Math.abs(sparse.censusMoments.sd - sparse.censusSdPercent) < .3);
assert(sparse.profilerApproxSd > sparse.referenceSd);
const overshoot = S.simulate({ scenario: "cold", hosts: 1000, fallback: "assume", assumed: 100, seed: 9 });
assert(overshoot.summary.everStopped > 0);
assert(overshoot.archives.some(o => o.round !== null));
assert(overshoot.summary.expanded > overshoot.summary.profiling * 100 / 15,
  "survivor weight is multiplied by inverse thinning probability");
assert(overshoot.summary.profiling > 0, "a seeded independent round retains some profilers");
for (const observation of overshoot.archives) {
  if (observation.active <= overshoot.cap) assert.equal(observation.round, null);
}
const churnThinning = S.simulate({ scenario: "containers", hosts: 100, containers: 10,
  fallback: "assume", assumed: 100, lifetime: 30, crash: .5, seed: 13 });
for (const unit of churnThinning.weightExamples) {
  assert(unit.appliedEpochs.every(epoch => epoch > unit.startEpoch), "new joiners skip old rounds");
  assert.equal(new Set(unit.appliedEpochs).size, unit.appliedEpochs.length, "no duplicate thinning");
  assert(unit.history.every(([t,w]) => Number.isFinite(t) && Number.isFinite(w) && w >= 0));
  assert(unit.history.slice(1).every(([t],i) => t >= unit.history[i][0]), "chronological weight changes");
}
assert(overshoot.weightExamples.some(p => p.history.some(([,weight]) => weight === 0)),
  "a failed local thinning coin records terminal zero");
assert(Math.abs(full.expandedMoments.mean - 10000) < 300);
console.log("Passed: tracking, startup freeze, census/no-support history, overlap, slow rollout, containers, ghosts, outage, independent draws, thinning epochs, terminal weights.");
