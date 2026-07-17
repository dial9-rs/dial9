import { describe, expect, it } from "vitest";
import { SegmentForest, spanBucketMerge, type SpanAgg } from "./segment-forest.js";
import { bitFloor, resolutionFor, fetchWindowFor, fetchWindowCovers } from "./windowing.js";

describe("SegmentForest (mipmap LOD)", () => {
  function leaves(durs: number[]): SpanAgg[] {
    return durs.map((d, i) => ({ dur: d, count: 1, idx: i }));
  }
  function bruteforce(durs: number[], lo: number, hi: number): SpanAgg | null {
    if (lo >= hi) return null;
    let best = { dur: -1, count: 0, idx: -1 };
    for (let i = lo; i < hi; i++) {
      best.count += 1;
      if (durs[i] > best.dur) { best.dur = durs[i]; best.idx = i; }
    }
    return best;
  }

  it("range query returns longest dur + summed count, matching brute force", () => {
    const durs = [3, 9, 1, 7, 7, 2, 8, 4, 6, 5];
    const f = new SegmentForest(leaves(durs), spanBucketMerge);
    for (let lo = 0; lo <= durs.length; lo++) {
      for (let hi = lo; hi <= durs.length; hi++) {
        expect(f.query(lo, hi), `[${lo},${hi})`).toEqual(bruteforce(durs, lo, hi));
      }
    }
  });

  it("ties keep the earlier index (a.dur < b.dur is strict)", () => {
    const f = new SegmentForest(leaves([5, 5]), spanBucketMerge);
    expect(f.query(0, 2)).toEqual({ dur: 5, count: 2, idx: 0 });
  });

  it("empty / out-of-bounds ranges are null / clamped", () => {
    const f = new SegmentForest(leaves([1, 2, 3]), spanBucketMerge);
    expect(f.query(1, 1)).toBeNull();
    expect(f.query(2, 1)).toBeNull();
    expect(f.query(-5, 99)).toEqual({ dur: 3, count: 3, idx: 2 });
    expect(new SegmentForest<SpanAgg>([], spanBucketMerge).query(0, 1)).toBeNull();
  });

  it("scales: random arrays match brute force", () => {
    // deterministic pseudo-random (no Math.random)
    let s = 12345;
    const rnd = () => ((s = (s * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff);
    const durs = Array.from({ length: 500 }, () => Math.floor(rnd() * 1000));
    const f = new SegmentForest(leaves(durs), spanBucketMerge);
    for (let t = 0; t < 200; t++) {
      const a = Math.floor(rnd() * 501), b = Math.floor(rnd() * 501);
      const [lo, hi] = a < b ? [a, b] : [b, a];
      expect(f.query(lo, hi)).toEqual(bruteforce(durs, lo, hi));
    }
  });
});

describe("resolution / fetch window", () => {
  it("bitFloor = largest power of two <= x", () => {
    expect([1, 2, 3, 4, 7, 8, 9, 1000].map(bitFloor)).toEqual([1, 2, 2, 4, 4, 8, 8, 512]);
    expect(bitFloor(0.5)).toBe(1);
  });

  it("resolution snaps so small pans keep the same key", () => {
    // 1e9 ns over 1000 px -> 1e6 ns/px -> bitFloor -> 2^19 = 524288
    const r = resolutionFor(1e9, 1000);
    expect(r).toBe(bitFloor(1e6));
    // a 1% smaller window keeps the same resolution
    expect(resolutionFor(0.99e9, 1000)).toBe(r);
  });

  it("fetch window over-fetches 3x and stays covering across small pans", () => {
    const w = fetchWindowFor(1000, 2000, 1000);
    expect(w.start).toBeLessThanOrEqual(0); // padded a full screen left
    expect(w.end).toBeGreaterThanOrEqual(3000); // and right
    expect(fetchWindowCovers(w, 1050, 2050, 1000)).toBe(true); // small pan: still covered
    expect(fetchWindowCovers(w, 100000, 200000, 1000)).toBe(false); // big jump: refetch
  });
});
