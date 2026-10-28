/* Offline explanatory event model of dial9-sampling with census history and
 * independent admission / weighted Bernoulli thinning (October 28 change).
 * This models population and lifecycle rules, not S3 request concurrency.
 * Uniform seeded ranks stand in for independent SHA-256 rank identities.
 */
(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.SamplingSimulator = api;
})(typeof globalThis === "object" ? globalThis : this, function () {
  "use strict";
  function rng(seed) {
    let s = Number(seed) >>> 0;
    return function () {
      s += 0x6d2b79f5;
      let t = Math.imul(s ^ (s >>> 15), 1 | s);
      t ^= t + Math.imul(t ^ (t >>> 7), 61 | t);
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }
  class Heap {
    constructor() { this.items = []; this.serial = 0; }
    push(time, type, data, priority = 0) {
      const event = { time, type, data, priority, serial: this.serial++ };
      const a = this.items;
      a.push(event);
      let i = a.length - 1;
      while (i) {
        const j = (i - 1) >> 1;
        if (!Heap.less(a[i], a[j])) break;
        [a[i], a[j]] = [a[j], a[i]]; i = j;
      }
    }
    pop() {
      const a = this.items, first = a[0], last = a.pop();
      if (a.length) {
        a[0] = last;
        let i = 0;
        for (;;) {
          let j = i * 2 + 1;
          if (j >= a.length) break;
          if (j + 1 < a.length && Heap.less(a[j + 1], a[j])) j++;
          if (!Heap.less(a[j], a[i])) break;
          [a[i], a[j]] = [a[j], a[i]]; i = j;
        }
      }
      return first;
    }
    static less(a, b) {
      return a.time < b.time || (a.time === b.time &&
        (a.priority < b.priority || (a.priority === b.priority && a.serial < b.serial)));
    }
  }
  function tracking(target, lower) {
    let bits = 0;
    if (target > 0) while (bits < 58 && lower / Math.pow(2, bits + 1) >= 32 * target) bits++;
    return { bits, fraction: Math.pow(2, -bits) };
  }
  function binomial(n, p, random) {
    n = Math.max(0, Math.floor(n));
    if (p <= 0 || n === 0) return 0;
    if (p >= 1) return n;
    if (p > 0.5) return n - binomial(n, 1 - p, random);
    // Geometric gaps give an exact binomial draw, without N Bernoulli draws.
    const logFailure = Math.log1p(-p);
    let position = -1, successes = 0;
    while (true) {
      position += 1 + Math.floor(Math.log(1 - random()) / logFailure);
      if (position >= n) return successes;
      successes++;
    }
  }
  function quantile(values, p) {
    const sorted = [...values].sort((a, b) => a - b);
    return sorted[Math.min(sorted.length - 1, Math.floor(p * sorted.length))];
  }
  function moments(values) {
    const mean = values.reduce((a, b) => a + b, 0) / values.length;
    const variance = values.reduce((a, b) => a + (b - mean) ** 2, 0) / values.length;
    return { mean, sd: Math.sqrt(variance), low: quantile(values, .025), high: quantile(values, .975) };
  }
  function distributions(n, target, f, seed, trials = 1500) {
    const random = rng(seed ^ 0xa81c39), census = [], selected = [], expanded = [], afterThin = [];
    const idealP = Math.min(f, n > 0 ? target / n : 0);
    for (let i = 0; i < trials; i++) {
      census.push(n ? (binomial(n, f, random) / f - n) / n * 100 : 0);
      // Independent next cohort: its admission cutoff comes from another
      // complete cohort. This draw deliberately does not model the fleet path.
      const previousR = binomial(n, f, random);
      const cutoff = previousR ? Math.min(f, target * f / previousR) : 0;
      const admitted = binomial(n, cutoff, random);
      selected.push(admitted);
      const cap = target + Math.ceil(3 * Math.sqrt(target));
      const q = admitted > cap ? target / admitted : 1;
      const survivors = binomial(admitted, q, random);
      afterThin.push(survivors);
      expanded.push(cutoff > 0 ? survivors / (cutoff * q) : 0);
    }
    return {
      trials, census, selected, expanded, afterThin,
      censusMoments: moments(census), profilerMoments: moments(selected),
      expandedMoments: moments(expanded),
      censusSdPercent: n ? 100 * Math.sqrt((1 - f) / (n * f)) : 0,
      referenceMean: n * idealP,
      referenceSd: Math.sqrt(n * idealP * (1 - idealP)),
      // Delta-method extra variance from an independent sparse old census.
      profilerApproxSd: n ? Math.sqrt(n * idealP * (1 - idealP) +
        (n * f >= target ? target * target * (1 - f) / (n * f) : 0)) : 0,
      cap: target + Math.ceil(3 * Math.sqrt(target)),
      aboveCap: selected.filter(a => a > target + Math.ceil(3 * Math.sqrt(target))).length / trials
    };
  }
  const presets = {
    cold: { name: "Cold start", hosts: 1000, containers: 1, duration: 900, rollout: 600,
      lifetime: 0, crash: 0, description: "Every host starts together with an empty roster. Off decisions stay off." },
    rolling: { name: "Rolling start", hosts: 1000, containers: 1, duration: 1200, rollout: 600,
      lifetime: 0, crash: 0, description: "A warm fleet is replaced over ten minutes, with 15 seconds of process overlap." },
    minute: { name: "1 minute per host", hosts: 20, containers: 1, duration: 1800, rollout: 1140,
      lifetime: 0, crash: 0, description: "An empty fleet grows by one physical host every minute. Lifetime can make it churn." },
    containers: { name: "Lots of containers", hosts: 250, containers: 20, duration: 1200, rollout: 600,
      lifetime: 90, crash: .2, description: "Twenty independently ranked containers share each physical host; short lifetimes churn." }
  };
  function simulate(options) {
    const scenario = options.scenario || "cold";
    const cfg = { ...presets[scenario], target: 15, lower: 0, fallback: "disabled",
      assumed: null, seed: 9, outageStart: 360, outageDuration: 0, ...options };
    cfg.hosts = Math.max(1, Math.min(20000, Math.floor(cfg.hosts)));
    cfg.containers = Math.max(1, Math.min(100, Math.floor(cfg.containers)));
    if (cfg.hosts * cfg.containers > 80000) throw new Error("Keep the fleet at 80,000 live processes or fewer.");
    cfg.duration = Math.max(120, Math.min(7200, Number(cfg.duration)));
    const { fraction: f, bits } = tracking(cfg.target, cfg.lower);
    const cap = cfg.target + Math.ceil(3 * Math.sqrt(cfg.target));
    const random = rng(cfg.seed), heap = new Heap();
    const processes = new Map(), members = new Map(), snapshots = [], archives = [], notes = [];
    let roster = null, leader = null, leaderGeneration = 0, censusHistory = [], passAt = null;
    let everStarted = 0, everSelected = 0, everStopped = 0, uniqueSelectedHosts = new Set(), rosterEpoch = 0;
    let sequence = 0, clock = 0;
    const bucketWidth = 300, guard = 11, poll = 60, lease = 186;
    const outageEnd = cfg.outageStart + cfg.outageDuration;
    const isDown = time => cfg.outageDuration > 0 && time >= cfg.outageStart && time < outageEnd;
    const emit = (time, text) => {
      if (notes.length < 70) notes.push({ time, text });
    };
    const planPass = time => {
      if (passAt !== null && passAt <= time) return;
      passAt = time;
      heap.push(time, "census", { generation: leaderGeneration }, 5);
    };
    const visibleMembers = time => {
      const bucket = Math.floor(time / bucketWidth);
      const visible = [];
      for (const member of members.values()) {
        let state = null;
        for (const [b, s] of member.keys) {
          if ((b === bucket || b === bucket - 1) && (!state || s === "carrier")) state = s;
        }
        if (state) visible.push({ ...member, visibleState: state });
      }
      return visible;
    };
    const writeMember = p => {
      const b = Math.floor(clock / bucketWidth);
      let m = members.get(p.id);
      if (!m) { m = { id: p.id, rank: p.rank, host: p.host, keys: new Map() }; members.set(p.id, m); }
      m.keys.set(b, p.state);
      for (const old of [...m.keys.keys()]) if (old < b - 1) m.keys.delete(old);
      p.writtenBucket = b;
    };
    const scheduleHeartbeat = p => {
      const b = Math.floor(clock / bucketWidth);
      const due = b * bucketWidth + p.offset;
      heap.push(due > clock ? due : due + bucketWidth, "heartbeat", p.id, 2);
    };
    const applyTrim = p => {
      if (!p.alive || p.state !== "active" || !roster || isDown(clock)) return;
      if (clock > roster.modified + lease || roster.censusAt === null || clock > roster.censusAt + 190) return;
      const round = roster.thin;
      if (!round || round.id <= p.appliedRound) return;
      p.appliedRound = round.id;
      p.appliedEpochs.push(round.id);
      if (random() >= round.q) {
        p.state = "carrier"; p.profiling = false; everStopped++;
        p.weightHistory.push([clock, 0]);
        writeMember(p);
        const m = members.get(p.id);
        for (const [b, s] of [...m.keys]) if (s === "active") m.keys.delete(b);
      } else {
        p.weight /= round.q;
        p.weightHistory.push([clock, p.weight]);
      }
    };
    const elect = p => {
      if (!p.tracked || isDown(clock)) return;
      if (leader && processes.get(leader)?.alive && roster && clock <= roster.modified + lease) return;
      if (roster && clock <= roster.modified + lease) return;
      leader = p.id; leaderGeneration++; censusHistory = []; rosterEpoch++;
      roster = roster || { arm: f, cutoffSettled: false, settled: false, population: null,
        counted: 0, thin: null, censusAt: null, modified: clock };
      roster.thin = null; roster.settled = false; roster.modified = clock;
      emit(clock, "One tracked process owns the roster; old admission cutoff is retained.");
      planPass(clock + p.rank / f * 2);
    };
    const start = data => {
      const id = String(++sequence).padStart(12, "0"), rank = random();
      const p = { id, host: data.host, seat: data.seat, generation: data.generation || 0,
        rank, tracked: rank < f, alive: true, profiling: false, state: "unarmed",
        admissionDraw: random(), weight: null, probability: 0,
        appliedRound: !isDown(clock) && roster ? rosterEpoch : 0, weightHistory: [],
        startEpoch: !isDown(clock) && roster ? rosterEpoch : 0, appliedEpochs: [],
        offset: random() * bucketWidth, pollInterval: poll * (.9 + .2 * random()),
        writtenBucket: null, end: data.end ?? null };
      processes.set(id, p); everStarted++;
      const usable = !isDown(clock) && roster && roster.cutoffSettled;
      const cutoff = usable ? roster.arm : cfg.fallback === "assume" ?
        Math.min(f, cfg.target / (cfg.assumed || cfg.hosts * cfg.containers)) : 0;
      p.probability = cutoff;
      p.profiling = p.tracked && p.admissionDraw < cutoff / f;
      p.weight = p.profiling ? 1 / cutoff : null;
      p.state = p.profiling ? "active" : cutoff === 0 ? "no_support" : "unarmed";
      if (p.profiling) p.weightHistory.push([clock, p.weight]);
      if (p.profiling) { everSelected++; uniqueSelectedHosts.add(p.host); }
      if (p.tracked) {
        if (!isDown(clock)) writeMember(p);
        heap.push(clock, "poll", id, 3);
        scheduleHeartbeat(p);
      }
      const end = data.end ?? (cfg.lifetime > 0 ? clock + cfg.lifetime : null);
      if (end !== null && end <= cfg.duration) heap.push(end, "end", { id, renew: data.renew !== false }, 1);
    };
    const record = () => {
      let hosts = new Set(), profiledHosts = new Set(), live = 0, profiling = 0, liveTracked = 0, off = 0, carriers = 0, expanded = 0, supported = 0;
      for (const p of processes.values()) {
        if (!p.alive) continue;
        live++; hosts.add(p.host);
        if (p.probability > 0) supported++;
        if (p.tracked) liveTracked++;
        if (p.profiling) { profiling++; profiledHosts.add(p.host); expanded += p.weight; }
        else if (p.state === "carrier") carriers++; else off++;
      }
      const counted = roster?.counted ?? null, visible = visibleMembers(clock).length;
      snapshots.push({ time: clock, hosts: hosts.size, processes: live, profiling,
        profiledHosts: profiledHosts.size, tracked: liveTracked, visible, off, carriers,
        projected: roster?.population ?? null, counted, expanded, supported, unsupported: live - supported,
        censusAt: roster?.censusAt ?? null, age: roster?.censusAt !== null && roster ? clock - roster.censusAt : null,
        settled: !!roster?.settled, cutoffSettled: !!roster?.cutoffSettled,
        arm: roster?.arm ?? null, ghosts: Math.max(0, visible - liveTracked), outage: isDown(clock),
        everStarted, everSelected, everStopped, everProfiledHosts: uniqueSelectedHosts.size });
    };
    // A warm roster is an explicit initial condition for rolling/container cases.
    // Build its cutoff from this initial cohort; admission draws are independent
    // of the tracking ranks that determine the census.
    if (scenario === "rolling" || scenario === "containers") {
      for (let h = 0; h < cfg.hosts; h++) for (let c = 0; c < cfg.containers; c++) {
        clock = 0;
        const replacement = scenario === "rolling" ? 30 + h / cfg.hosts * cfg.rollout : null;
        start({ host: "host-" + h, seat: h * cfg.containers + c,
          end: replacement === null ? (cfg.lifetime > 0 ? 1 + random() * cfg.lifetime : null) : replacement + 15,
          renew: scenario !== "rolling" });
        if (replacement !== null) heap.push(replacement, "start", { host: "host-" + h,
          seat: h * cfg.containers + c }, 0);
      }
      const liveR = [...processes.values()].filter(p => p.tracked).length;
      roster = { arm: liveR ? Math.min(f, cfg.target * f / liveR) : f,
        cutoffSettled: liveR > 0, settled: liveR > 0, population: liveR / f,
        counted: liveR, thin: null, censusAt: 0, modified: 0 };
      everSelected = 0; uniqueSelectedHosts.clear();
      const initialCutoff = liveR > 0 ? roster.arm : cfg.fallback === "assume" ?
        Math.min(f, cfg.target / (cfg.assumed || cfg.hosts * cfg.containers)) : 0;
      for (const p of processes.values()) {
        p.probability = initialCutoff;
        p.profiling = p.tracked && p.admissionDraw < initialCutoff / f;
        p.weight = p.profiling ? 1 / initialCutoff : null;
        p.weightHistory = p.profiling ? [[0, p.weight]] : [];
        p.state = p.profiling ? "active" : initialCutoff === 0 ? "no_support" : "unarmed";
        if (p.profiling) { everSelected++; uniqueSelectedHosts.add(p.host); }
        if (p.tracked) writeMember(p);
      }
      leader = [...processes.values()].filter(p => p.tracked).sort((a, b) => a.rank - b.rank)[0]?.id || null;
      leaderGeneration++; censusHistory = [liveR, liveR, liveR];
      emit(0, liveR > 0 ? "Simulation begins with a compatible, settled roster from the initial live fleet." :
        "No tracked member exists: a warm roster cannot be established; the startup fallback applies.");
      if (leader) planPass(guard);
    } else {
      for (let h = 0; h < cfg.hosts; h++) for (let c = 0; c < cfg.containers; c++) {
        const time = scenario === "minute" ? h * 60 : 0;
        if (time <= cfg.duration) heap.push(time, "start", { host: "host-" + h,
          seat: h * cfg.containers + c }, 0);
      }
      emit(0, cfg.fallback === "disabled" ? "No roster: this cohort starts off and will never turn on." :
        "No roster: rank uses the explicit assumed population for startup.");
    }
    if (cfg.outageDuration > 0) {
      emit(cfg.outageStart, "Storage outage begins. Existing profilers keep running; new startups use fallback.");
      emit(outageEnd, "Storage recovers. Old membership ghosts can persist until bucket expiry.");
    }
    const sampleInterval = Math.max(2, Math.ceil(cfg.duration / 360));
    for (let t = 0; t < cfg.duration; t += sampleInterval) heap.push(t, "sample", null, 9);
    heap.push(cfg.duration, "sample", null, 9);
    while (heap.items.length) {
      const event = heap.pop(); clock = event.time;
      if (clock > cfg.duration) break;
      const p = typeof event.data === "string" ? processes.get(event.data) : null;
      if (event.type === "start") start(event.data);
      else if (event.type === "end") {
        const ended = processes.get(event.data.id);
        if (!ended?.alive) continue;
        if (ended.profiling) ended.weightHistory.push([clock, 0]);
        ended.alive = false; ended.profiling = false;
        const crashed = random() < cfg.crash;
        if (!crashed && !isDown(clock)) members.delete(ended.id);
        if (event.data.renew && cfg.lifetime > 0) {
          // Minute/cold churn replaces the physical host; containers keep their host.
          start({ host: scenario === "containers" ? ended.host : ended.host + "-r" + (ended.generation + 1),
            seat: ended.seat, generation: ended.generation + 1 });
        }
      } else if (event.type === "heartbeat") {
        if (!p?.alive) continue;
        if (!isDown(clock)) writeMember(p);
        scheduleHeartbeat(p);
      } else if (event.type === "poll") {
        if (!p?.alive) continue;
        if (!isDown(clock)) {
          if (p.writtenBucket === null) writeMember(p);
          applyTrim(p); elect(p);
        }
        heap.push(clock + p.pollInterval, "poll", p.id, 3);
      } else if (event.type === "census") {
        if (event.data.generation !== leaderGeneration || passAt !== clock) continue;
        passAt = null;
        if (!leader || !processes.get(leader)?.alive || isDown(clock)) continue;
        rosterEpoch++;
        const phase = clock % bucketWidth;
        if (phase < guard || phase + guard >= bucketWidth) {
          roster.settled = false; roster.thin = null; roster.modified = clock; censusHistory = [];
          planPass(clock + Math.min(poll, phase < guard ? guard - phase : bucketWidth - phase + guard));
          continue;
        }
        const visible = visibleMembers(clock), active = visible.filter(m => m.visibleState === "active");
        const count = visible.length;
        censusHistory.push(count); if (censusHistory.length > 4) censusHistory.shift();
        const settled = censusHistory.length === 4 &&
          censusHistory.slice(1).every((b, i) => b * 100 <= censusHistory[i] * 102);
        if (settled && count > 0) {
          if (!roster.cutoffSettled) emit(clock, "Four plateau counts publish a cutoff for future startups.");
          roster.arm = Math.min(f, cfg.target * f / count); roster.cutoffSettled = true;
        }
        // Seal one independent Bernoulli round; let polls apply it once each.
        // Above the trigger, keep an existing round until its cooldown; a
        // census at/below the trigger clears it immediately.
        if (active.length <= cap) roster.thin = null;
        else if (!roster.thin || clock >= roster.thin.created + 190) {
          roster.thin = { id: rosterEpoch, q: cfg.target / active.length, created: clock };
          emit(clock, "Published thinning round " + roster.thin.id +
            ", q = " + roster.thin.q.toFixed(3) + ". Survivors multiply their expansion weight by 1/q.");
        }
        roster.settled = settled && count > 0;
        roster.counted = count; roster.population = count / f; roster.censusAt = clock; roster.modified = clock;
        archives.push({ time: clock, censusAt: clock, bucket: Math.floor(clock / bucketWidth),
          counted: count, population: count / f, active: active.length, fraction: f, settled: roster.settled,
          noSupport: visible.filter(m => m.visibleState === "no_support").length,
          unarmed: visible.filter(m => m.visibleState === "unarmed" || m.visibleState === "no_support").length,
          round: roster.thin ? { ...roster.thin } : null });
        applyTrim(processes.get(leader));
        planPass(clock + (roster.settled ? poll : 1));
      } else if (event.type === "sample") record();
    }
    notes.sort((a, b) => a.time - b.time);
    const weightExamples = [...processes.values()].filter(p => p.weightHistory.length)
      .sort((a,b) => b.weightHistory.length - a.weightHistory.length || a.id.localeCompare(b.id))
      .slice(0, 3).map(p => ({ processId: p.id, host: p.host, history: p.weightHistory,
        startEpoch: p.startEpoch, appliedEpochs: p.appliedEpochs }));
    return { config: cfg, fraction: f, bits, cap, snapshots, archives, notes, weightExamples,
      summary: snapshots[snapshots.length - 1] };
  }
  return { rng, tracking, binomial, moments, distributions, presets, simulate };
});
