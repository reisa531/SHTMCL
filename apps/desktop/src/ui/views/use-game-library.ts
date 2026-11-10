import { useEffect, useRef, useState } from "react";
import { gameInstallApi, type InstalledGame } from "./game-install-api";
import type { SnapshotPhase } from "./page-state";

/** One installed-game index for both launch and management. */
export function useGameLibrary(connected: boolean, revision: string) {
  const [games, setGames] = useState<InstalledGame[]>([]);
  const [phase, setPhase] = useState<SnapshotPhase>("loading");
  const [settledRevision, setSettledRevision] = useState<string | undefined>();
  const generation = useRef(0);
  useEffect(() => {
    const current = ++generation.current;
    if (!connected) { setGames([]); setPhase("ready"); setSettledRevision(undefined); return; }
    setPhase("loading");
    void gameInstallApi().instances().then((items) => {
      if (generation.current === current) { setGames(items); setPhase("ready"); setSettledRevision(revision); }
    }).catch(() => { if (generation.current === current) { setPhase("error"); setSettledRevision(revision); } });
    return () => { generation.current += 1; };
  }, [connected, revision]);
  // A previous disconnected/older snapshot must not clear a persisted selection
  // during the render before the new request effect starts.
  return { games, phase: connected && settledRevision !== revision ? "loading" as const : phase };
}
