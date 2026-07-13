// src/pages/viewer/track-renderers.ts - the track-content renderer registry
// (the seam T21's shell exposes to T22-T30).
//
// The shell (tracks.ts) sizes every track canvas and, for tracks WITHOUT a
// mounted content renderer, paints the empty placeholder. A content ticket
// (T22 lanes first) "claims" its track id here; the shell then skips the
// placeholder for that track, leaving the canvas for the claiming renderer,
// which draws it on its own store subscription (03 F2: a track redraws only
// when the slices it depends on change - the renderer registry maps changed
// slices to affected canvases, one renderer at a time).
//
// This is intentionally tiny: the "registry" is the set of claimed track
// ids. Each renderer owns its own store subscription and canvas sizing; the
// only thing the shell needs to know is "hands off this canvas".

import type { TrackId } from "./track-layout.js";

const claimed = new Set<TrackId>();

/**
 * Claim a track's canvas: the shell stops painting the placeholder over it.
 * Returns a release function (idempotent) for teardown/HMR.
 */
export function claimTrack(id: TrackId): () => void {
  claimed.add(id);
  return () => {
    claimed.delete(id);
  };
}

/** True when a content renderer owns this track's canvas. */
export function isTrackClaimed(id: TrackId): boolean {
  return claimed.has(id);
}
