export const infoMenuPosition = (
  anchor: { right: number; bottom: number },
  menu: { width: number; height: number },
  viewport: { width: number; height: number },
): { left: number; top: number } => ({
  left: Math.max(12, Math.min(anchor.right - menu.width, viewport.width - menu.width - 12)),
  top: Math.max(12, Math.min(anchor.bottom + 4, viewport.height - menu.height - 12)),
});

/** Keep an open details panel on screen as the toolbar wraps or resizes. */
export const createInfoMenu = () => {
  let observer: ResizeObserver | null = null;
  let cleanup: (() => void) | null = null;

  const dispose = (): void => {
    observer?.disconnect();
    observer = null;
    cleanup?.();
    cleanup = null;
  };

  const onToggle = (event: Event): void => {
    dispose();
    const details = event.currentTarget as HTMLDetailsElement;
    if (!details.open) return;
    const panel = details.querySelector<HTMLElement>(".d9-info-menu-body");
    const view = details.ownerDocument.defaultView;
    if (panel === null || view === null) return;
    const position = (): void => {
      const { left, top } = infoMenuPosition(
        details.getBoundingClientRect(),
        panel.getBoundingClientRect(),
        { width: view.innerWidth, height: view.innerHeight },
      );
      panel.style.left = `${left}px`;
      panel.style.top = `${top}px`;
    };
    position();
    observer = new ResizeObserver(position);
    observer.observe(panel);
    const toolbar = details.closest(".d9-toolbar");
    if (toolbar !== null) observer.observe(toolbar);
    view.addEventListener("resize", position);
    cleanup = () => view.removeEventListener("resize", position);
  };

  return { onToggle, dispose };
};
