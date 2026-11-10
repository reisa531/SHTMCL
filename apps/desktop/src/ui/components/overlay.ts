// Overlay (dialog) state machine.
//
// Pure on purpose: the shell keeps dialog state in this reducer so closing via
// Escape, via the close button, or via a backdrop click always take the same
// path, and so the transition can be asserted without a DOM. Igored events
// return the *same* object reference, which lets React skip a re-render.

export interface OverlayState {
  open: boolean;
  /** Element id that opened the overlay, used to restore focus on close. */
  trigger: string | null;
}

export type OverlayEvent =
  | { type: "open"; trigger?: string }
  | { type: "escape" }
  | { type: "dismiss" }
  | { type: "close" };

export const CLOSED_OVERLAY: OverlayState = { open: false, trigger: null };

export function reduceOverlay(
  state: OverlayState,
  event: OverlayEvent,
): OverlayState {
  switch (event.type) {
    case "open":
      if (state.open && state.trigger === (event.trigger ?? null)) {
        return state;
      }
      return { open: true, trigger: event.trigger ?? null };
    case "escape":
    case "dismiss":
    case "close":
      if (!state.open) {
        return state;
      }
      return { open: false, trigger: state.trigger };
  }
}

/** Where focus should land once the overlay is closed. */
export function focusAfterClose(state: OverlayState): string | null {
  return state.open ? null : state.trigger;
}
