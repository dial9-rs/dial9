// Columnar backing for cpuSamples' callchains. A perf-capture trace holds
// millions of CPU samples, each with a deep (dozens of frames) callchain array
// of "0x…" hex strings. As fat arrays that is the single largest parse
// structure (measured ~2.3 GB for 1.9M samples x ~43 frames on the 13.7M
// trace) - and at LOAD TIME nothing reads the callchains (attachCpuSamples
// reads timestamp/workerId only; the flamegraph, the sole callchain reader, is
// lazy). So the frames live in ONE flat Int32Array (interned-string indices)
// with each sample carrying just a [start,len) into it, and `sample.callchain`
// is a lazy getter that materializes the string[] only when the flamegraph
// actually asks. Retained drops from ~2.3 GB to ~0.5 GB.
//
// The parser routes CpuSampleEvent here via the `cpuSampleSink` parse option
// (mirroring its hex formatting, so old traces round-trip identically). Sample
// scalar fields stay plain (mutable) properties because attachCpuSamples and
// finalizeParse write spawnLoc/inPoll/workerId onto them.

const INITIAL_POOL = 1 << 20;

export class ColumnarCpuSamples {
  /** Flat pool of interned hex-string indices; sample i owns [cs, cs+cl). */
  pool: Int32Array;
  private poolLen = 0;
  /** Interned "0x…" frame strings; pool entries index into this. */
  hexStrings: string[] = [];
  private addrIntern = new Map<unknown, number>();

  /** The materialized sample objects - this IS trace.cpuSamples. */
  samples: CpuSample[] = [];

  constructor(cap = INITIAL_POOL) {
    this.pool = new Int32Array(cap);
  }

  private internAddr(addr: unknown): number {
    let i = this.addrIntern.get(addr);
    if (i === undefined) {
      i = this.hexStrings.length;
      this.hexStrings.push("0x" + BigInt(addr as never).toString(16));
      this.addrIntern.set(addr, i);
    }
    return i;
  }

  /**
   * SINK: the parser calls this per CpuSampleEvent with the raw decoded fields
   * and the raw callchain address list (NOT a pre-built hex array). Frames are
   * interned into the flat pool; the sample stores only [cs, cl).
   */
  pushSample(
    timestamp: number,
    workerId: number,
    tid: number,
    source: number,
    rawCallchain: readonly unknown[],
    cpu: number | null
  ): void {
    const cl = rawCallchain.length;
    if (this.poolLen + cl > this.pool.length) {
      let n = this.pool.length * 2;
      while (this.poolLen + cl > n) n *= 2;
      const next = new Int32Array(n);
      next.set(this.pool);
      this.pool = next;
    }
    const cs = this.poolLen;
    for (let k = 0; k < cl; k++) this.pool[this.poolLen++] = this.internAddr(rawCallchain[k]);
    this.samples.push(new CpuSample(this, cs, cl, timestamp, workerId, tid, source, cpu));
  }
}

/**
 * One CPU sample. Scalar fields are plain mutable properties (finalizeParse
 * resolves workerId via tid->worker; attachCpuSamples sets spawnLoc/inPoll).
 * `callchain` is a prototype getter that rebuilds the hex string[] from the
 * store's flat pool on demand - identical value to the old eager array.
 */
export class CpuSample {
  spawnLoc: string | null = null;
  inPoll = false;
  private readonly _store: ColumnarCpuSamples;
  private readonly _cs: number;
  private readonly _cl: number;
  timestamp: number;
  workerId: number;
  tid: number;
  source: number;
  cpu: number | null;
  constructor(
    _store: ColumnarCpuSamples,
    _cs: number,
    _cl: number,
    timestamp: number,
    workerId: number,
    tid: number,
    source: number,
    cpu: number | null
  ) {
    this._store = _store;
    this._cs = _cs;
    this._cl = _cl;
    this.timestamp = timestamp;
    this.workerId = workerId;
    this.tid = tid;
    this.source = source;
    this.cpu = cpu;
  }

  get callchain(): string[] {
    const { _store, _cs, _cl } = this;
    const out = new Array<string>(_cl);
    for (let i = 0; i < _cl; i++) out[i] = _store.hexStrings[_store.pool[_cs + i]!]!;
    return out;
  }
}
