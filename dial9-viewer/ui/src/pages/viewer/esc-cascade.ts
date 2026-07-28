// The Escape cascade MECHANISM: an ordered list of "escapable" surfaces, each
// a predicate + close callback, tried highest-priority first; the first one
// that was open consumes the key and stops. Surfaces register with an explicit
// priority so registrations from different modules compose without a central
// edit: higher number = tried first (help highest, task-selection lowest).

export interface EscapableSurface {
  /** Higher = closed first. */
  priority: number;
  /** True when the surface is currently open (and thus can consume Esc). */
  isOpen(): boolean;
  /** Close the surface. Only called when isOpen() returned true. */
  close(): void;
  /** Debug/name tag (surfaced by describe()). */
  name: string;
}

export interface EscCascade {
  /** Register a surface; returns an unregister function. */
  register(surface: EscapableSurface): () => void;
  /**
   * Run the cascade once (call from a keydown handler on Escape). Closes the
   * highest-priority OPEN surface and returns true; returns false when nothing
   * was open (the caller then applies its own fallback - e.g. clear task
   * selection + refocus the main area).
   */
  handle(): boolean;
  /** Ordered surface names, highest priority first (tests/debug). */
  describe(): string[];
}

/** Create an empty cascade. Pages register their surfaces into it. */
export function createEscCascade(): EscCascade {
  const surfaces = new Set<EscapableSurface>();

  function ordered(): EscapableSurface[] {
    return [...surfaces].sort((a, b) => b.priority - a.priority);
  }

  return {
    register(surface: EscapableSurface): () => void {
      surfaces.add(surface);
      return () => {
        surfaces.delete(surface);
      };
    },
    handle(): boolean {
      for (const surface of ordered()) {
        if (surface.isOpen()) {
          surface.close();
          return true;
        }
      }
      return false;
    },
    describe(): string[] {
      return ordered().map((s) => s.name);
    },
  };
}

/**
 * Canonical priority bands so registrations stay ordered without coordination
 * (higher = closed first): help > popups > sidebar > selection.
 */
export const ESC_PRIORITY = {
  help: 100,
  // A modal over everything but help: Esc closes an open search first.
  search: 95,
  // The load section: a modal drop-zone/loading surface. Sits below help (a
  // `?` overlay opened over a load still closes first) and above the
  // popups/sidebar it covers, so Esc cancels a load / dismisses the New-File
  // chooser before touching anything behind it.
  load: 90,
  popup: 80,
  sidebar: 60,
  selection: 20,
} as const;
