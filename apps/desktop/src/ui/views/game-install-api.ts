import { dispatchCoreMutation, dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";

export const VERSION_KINDS = {
  release: "正式版", snapshot: "快照 / 预览版", old_alpha: "远古 Alpha", old_beta: "远古 Beta",
} as const;
export type VersionKind = keyof typeof VERSION_KINDS;

/**
 * A version page may contain entries whose `type` is a manifest kind we do not
 * yet model locally (e.g. a future classification). Those are preserved as-is
 * strings rather than failing the whole page. Known kinds stay narrowed.
 */
export type GameVersionType = VersionKind | (string & {});
export interface GameVersion { id: string; type: GameVersionType; releaseTime?: string }
/**
 * Optional authoritative latest IDs supplied by the backend index. They are
 * omitted entirely when the backend does not return a nonempty string, so a
 * page without this metadata is structurally identical to the legacy shape.
 */
export interface VersionPage {
  entries: GameVersion[];
  total: number;
  latestRelease?: string;
  latestSnapshot?: string;
}

export type ModInventoryStatus = "ready" | "limited" | "unavailable";
export interface ModInventoryFile {
  fileName: string;
  enabled: boolean;
  bytes: number;
}
/**
 * Optional, read-only local Mod inventory. Only a validated `ready`/`limited`
 * status with defensively parsed files is accepted; any malformed entry drops
 * the whole inventory so the UI falls back to an honest unknown state instead
 * of guessing a count or fabrication.
 */
export interface ModInventory {
  status: ModInventoryStatus;
  files: ModInventoryFile[];
}
export interface InstalledGame {
  id: string;
  name: string;
  version: { gameVersion: string; loader: string; loaderVersion?: string };
  /** Optional fields are copied from the persisted instance snapshot only. */
  javaMajor?: number;
  identityId?: string;
  profileId?: string;
  presentation?: { modFileCount?: number; modpackName?: string; modInventory?: ModInventory };
}
type InstallLoader = { kind: "fabric"; version: string } | { kind: "neoforge"; version: string };
type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid core response");
  return value as Record<string, unknown>;
}
function completed(response: CommandResponse): Record<string, unknown> {
  if (response.type !== "completed") throw new Error("Expected completed response");
  return object(response.result);
}
function accepted(response: CommandResponse): string {
  if (response.type !== "accepted" || !response.taskId) throw new Error("Expected task response");
  return response.taskId;
}
export function isKnownVersionKind(type: GameVersionType): type is VersionKind {
  return Object.prototype.hasOwnProperty.call(VERSION_KINDS, type);
}

function validFileName(name: unknown): name is string {
  if (typeof name !== "string" || !name.length || [...name].length > 255) return false;
  // eslint-disable-next-line no-control-regex
  return !/[\u0000-\u001f\u007f-\u009f]/.test(name) && !name.includes("/") && !name.includes("\\");
}

/**
 * Parse an optional inventory payload defensively. Any invalid status, entry
 * shape, filename, `enabled`, or `bytes` (nonnegative safe integer) drops the
 * entire inventory so callers never display a partially fabricated state.
 */
export function parseModInventory(value: unknown): ModInventory | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  const raw = value as Record<string, unknown>;
  const status = raw.status;
  if (status !== "ready" && status !== "limited" && status !== "unavailable") return undefined;
  if (status === "unavailable") return { status, files: [] };
  if (!Array.isArray(raw.files)) return undefined;
  if (raw.files.length > 512) return undefined;
  const files: ModInventoryFile[] = [];
  const names = new Set<string>();
  for (const item of raw.files) {
    if (!item || typeof item !== "object" || Array.isArray(item)) return undefined;
    const entry = item as Record<string, unknown>;
    if (!validFileName(entry.fileName)) return undefined;
    if (names.has(entry.fileName)) return undefined;
    names.add(entry.fileName);
    if (typeof entry.enabled !== "boolean") return undefined;
    const lower = entry.fileName.toLowerCase();
    if (entry.enabled ? !lower.endsWith(".jar") : !lower.endsWith(".jar.disabled")) return undefined;
    if (!Number.isSafeInteger(entry.bytes) || (entry.bytes as number) < 0) return undefined;
    files.push({ fileName: entry.fileName, enabled: entry.enabled, bytes: entry.bytes as number });
  }
  return { status, files };
}

/** The dispatcher can be injected in contract tests; production always uses the native bridge. */
export function gameInstallApi(read: Dispatch = dispatchCoreRead, mutate: Dispatch = dispatchCoreMutation) {
  function request(command: string, payload: Record<string, unknown> = {}, mutation = false): CommandRequest {
    return { requestId: crypto.randomUUID(), ...(mutation ? { operationId: crypto.randomUUID() } : {}), payload: { command, ...payload } };
  }
  return {
    async modInventory(instanceId: string): Promise<ModInventory | undefined> {
      const result = completed(await read(request("instances.snapshot", { instanceId })));
      const instance = object(result.instance);
      if (instance.id !== instanceId) throw new Error("Instance snapshot identity mismatch");
      const presentation = instance.presentation;
      return presentation && typeof presentation === "object"
        ? parseModInventory((presentation as Record<string, unknown>).modInventory) : undefined;
    },
    /**
     * `kind` may be a concrete VersionKind or the literal "all". For "all" the
     * kind field is omitted from the payload so the backend default applies.
     * The existing four-kind payload shape is preserved exactly (kind + query +
     * offset + limit:40); "all" deviates only by omitting `kind`.
     */
    async versions(kind: VersionKind | "all", query: string, offset: number): Promise<VersionPage> {
      const shared = { query, offset, limit: 40 };
      const payload = kind === "all" ? shared : { kind, ...shared };
      const result = completed(await read(request("launch.versions.list", payload)));
      if (!Array.isArray(result.entries) || !Number.isSafeInteger(result.total) || (result.total as number) < 0) throw new Error("Invalid version page");
      const entries = result.entries.map((item) => {
        const entry = object(item);
        // Unknown string types are preserved instead of failing the whole page;
        // only a non-string / empty id or type is genuinely invalid.
        if (typeof entry.id !== "string" || !entry.id || typeof entry.type !== "string" || !entry.type) throw new Error("Invalid version");
        return { id: entry.id, type: entry.type, releaseTime: typeof entry.releaseTime === "string" ? entry.releaseTime : undefined } as GameVersion;
      });
      const page: VersionPage = { entries, total: result.total as number };
      // Authoritative latest IDs are only adopted when the backend supplies a
      // nonempty string; anything else leaves the field entirely absent so no
      // global latest is ever invented client-side.
      if (typeof result.latestRelease === "string" && result.latestRelease) page.latestRelease = result.latestRelease;
      if (typeof result.latestSnapshot === "string" && result.latestSnapshot) page.latestSnapshot = result.latestSnapshot;
      return page;
    },
    async instances(): Promise<InstalledGame[]> {
      const result = completed(await read(request("instances.list")));
      if (!Array.isArray(result.instances)) throw new Error("Invalid instance list");
      return result.instances.map((item) => {
        const entry = object(item), version = object(entry.version);
        if (typeof entry.id !== "string" || typeof entry.name !== "string" || typeof version.gameVersion !== "string" || typeof version.loader !== "string") throw new Error("Invalid instance");
        const game: InstalledGame = { id: entry.id, name: entry.name, version: { gameVersion: version.gameVersion, loader: version.loader } };
        if (typeof version.loaderVersion === "string" && version.loaderVersion.trim()) game.version.loaderVersion = version.loaderVersion;
        if (Number.isSafeInteger(entry.javaMajor) && (entry.javaMajor as number) > 0) game.javaMajor = entry.javaMajor as number;
        if (typeof entry.identityId === "string" && entry.identityId.trim()) game.identityId = entry.identityId;
        if (typeof entry.profileId === "string" && entry.profileId.trim()) game.profileId = entry.profileId;
        if (entry.presentation && typeof entry.presentation === "object") {
          const value = entry.presentation as Record<string, unknown>;
          game.presentation = {};
          if (Number.isSafeInteger(value.modFileCount) && (value.modFileCount as number) >= 0) game.presentation.modFileCount = value.modFileCount as number;
          if (typeof value.modpackName === "string" && value.modpackName.trim()) game.presentation.modpackName = value.modpackName;
          const inventory = parseModInventory(value.modInventory);
          if (inventory) game.presentation.modInventory = inventory;
        }
        return game;
      });
    },
    async refresh(): Promise<string> {
      return accepted(await mutate(request("launch.versions.refresh", {}, true)));
    },
    async install(versionId: string, instanceId: string, name: string, environmentId?: string, loader?: InstallLoader): Promise<string> {
      return accepted(await mutate(request("launch.install", { versionId, instanceId, name: name.trim(), ...(environmentId ? { environmentId } : {}), ...(loader ? { loader: loader.kind, loaderVersion: loader.version } : {}) }, true)));
    },
  };
}

export function installError(error: unknown): string {
  const code = error && typeof error === "object" ? (error as { code?: string }).code : undefined;
  if (code === "launch.install.busy") return "正在创建另一个实例，请稍后再试。";
  return "未能开始创建，请重试。";
}
