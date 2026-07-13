// The track-content renderer registry: the seam the shell exposes to content
// tracks.
//
// The shell (tracks.ts) sizes every track canvas and, for tracks without a
// mounted content renderer, paints the empty placeholder. A content renderer
// "claims" its track id here; the shell then skips the placeholder for that
// track, leaving the canvas for the claiming renderer, which draws it on its own
// store subscription.
//
// Intentionally tiny: the "registry" is the set of claimed track ids. Each
// renderer owns its own store subscription and canvas sizing; the only thing the
// shell needs to know is "hands off this canvas".

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
