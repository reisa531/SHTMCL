import type { InstalledGame } from "./game-install-api";

export const SELECTED_GAME_KEY = "shtmcl.selected-game.v1";

type ReadStorage = Pick<Storage, "getItem">;
type WriteStorage = Pick<Storage, "setItem" | "removeItem">;

function defaultReadStorage(): ReadStorage | undefined {
  try {
    if (typeof window === "undefined" || !window.localStorage) return undefined;
    return window.localStorage;
  } catch {
    return undefined;
  }
}

function defaultWriteStorage(): WriteStorage | undefined {
  try {
    if (typeof window === "undefined" || !window.localStorage) return undefined;
    return window.localStorage;
  } catch {
    return undefined;
  }
}

export function readSelectedGame(storage?: ReadStorage): string | undefined {
  const target = storage ?? defaultReadStorage();
  if (!target) return undefined;
  try {
    const value = target.getItem(SELECTED_GAME_KEY);
    if (typeof value !== "string") return undefined;
    const trimmed = value.trim();
    return trimmed ? trimmed : undefined;
  } catch {
    return undefined;
  }
}

export function saveSelectedGame(id: string | undefined, storage?: WriteStorage): void {
  const target = storage ?? defaultWriteStorage();
  if (!target) return;
  try {
    const trimmed = typeof id === "string" ? id.trim() : "";
    if (trimmed) target.setItem(SELECTED_GAME_KEY, trimmed);
    else target.removeItem(SELECTED_GAME_KEY);
  } catch {
    // Storage unavailable or full: best effort only.
  }
}

export function resolveSelectedGame(
  games: readonly InstalledGame[],
  selectedId?: string,
): InstalledGame | undefined {
  if (!Array.isArray(games) || games.length === 0) return undefined;
  const trimmed = typeof selectedId === "string" ? selectedId.trim() : "";
  if (trimmed) {
    const match = games.find((game) => game && game.id === trimmed);
    if (match) return match;
  }
  return games[0];
}
