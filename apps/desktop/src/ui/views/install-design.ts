import type { GameVersion, VersionKind } from "./game-install-api";

/**
 * Exact verified NeoForge pilot. Only this game version + loader version pair is
 * offered; anything else stays unavailable until a real adapter exists.
 */
export const NEOFORGE_PILOT_GAME_VERSION = "1.20.4";
export const NEOFORGE_PILOT_VERSION = "20.4.253";
export const NEOFORGE_PILOT_VERSIONS = [NEOFORGE_PILOT_VERSION] as const;

export function neoAvailableFor(gameVersion: string): boolean {
  return gameVersion === NEOFORGE_PILOT_GAME_VERSION;
}

/**
 * Explicit April Fools / joke release identifiers. These never come from a
 * generic pattern; the list is intentionally curated so normal ids that merely
 * look unusual are not misclassified.
 */
export const APRIL_FOOLS_IDS: readonly string[] = [
  "20w14infinite",
  "20w14\u221E",
  "3d shareware v1.34",
  "1.rv-pre1",
  "15w14a",
  "2.0",
  "22w13oneblockatatime",
  "23w13a_or_b",
  "24w14potato",
  "25w14craftmine",
  "26w14a",
] as const;

const APRIL_FOOLS_SET = new Set<string>(APRIL_FOOLS_IDS);

export function isAprilFools(id: string): boolean {
  return APRIL_FOOLS_SET.has(id);
}

/**
 * Groups used for the browser's version-type filter and for honest per-page
 * grouping. "all" means no filter. The four concrete groups map back onto the
 * existing VersionKind values wherever possible plus a special/other bucket.
 */
export const VERSION_FILTERS = {
  all: "全部版本",
  release: "正式版",
  snapshot: "快照 / 预览版",
  old_alpha: "远古 Alpha",
  old_beta: "远古 Beta",
} as const;
export type VersionFilter = keyof typeof VERSION_FILTERS;

export type VersionGroup = "release" | "preview" | "special" | "ancient" | "other";

export const VERSION_GROUP_LABELS: Record<VersionGroup, string> = {
  release: "正式版",
  preview: "快照 / 预览版",
  special: "愚人节 / 特殊版本",
  ancient: "远古版本",
  other: "其他",
};

const GROUP_ORDER: readonly VersionGroup[] = ["release", "preview", "special", "ancient", "other"];

export function classifyVersion(entry: Pick<GameVersion, "id" | "type">): VersionGroup {
  if (isAprilFools(entry.id)) return "special";
  const type: string = entry.type;
  if (type === "release") return "release";
  if (type === "snapshot") return "preview";
  if (type === "old_alpha" || type === "old_beta") return "ancient";
  return "other";
}

export interface VersionGroupSummary { group: VersionGroup; label: string; entries: GameVersion[] }

/**
 * Groups a single fetched page. Order within each group is preserved and the
 * empty page yields no groups. These are counts for the current page only; the
 * backend metadata has no global latest ids, so this must never be presented as
 * a global "latest" claim.
 */
export function groupVersionPage(entries: readonly GameVersion[]): VersionGroupSummary[] {
  const buckets = new Map<VersionGroup, GameVersion[]>();
  for (const entry of entries) {
    const group = classifyVersion(entry);
    const list = buckets.get(group);
    if (list) list.push(entry);
    else buckets.set(group, [entry]);
  }
  return GROUP_ORDER.filter((group) => buckets.has(group)).map((group) => ({
    group,
    label: VERSION_GROUP_LABELS[group],
    entries: buckets.get(group) as GameVersion[],
  }));
}

/**
 * The filters that map a VersionFilter onto an outgoing API `kind`. "all" maps
 * to the literal "all" (payload omits kind); the rest are real VersionKinds.
 */
export function kindForFilter(filter: VersionFilter): VersionKind | "all" {
  return filter === "all" ? "all" : filter;
}
