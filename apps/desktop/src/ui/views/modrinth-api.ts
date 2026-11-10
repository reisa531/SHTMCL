import { dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";
import type { ResourceTarget } from "./resource-catalog-api";

export interface ModrinthProject {
  projectId: string; slug: string; title: string; description: string;
  clientSide: "required" | "optional" | "unsupported" | "unknown";
}
export interface ModDependency {
  kind: "required" | "optional" | "incompatible" | "embedded";
  projectId?: string; versionId?: string; fileName?: string;
}
export interface ModVersion {
  versionId: string; name: string; versionNumber: string;
  kind: "release" | "beta" | "alpha"; published: string;
  files: { name: string; bytes: number; primary: boolean }[];
  dependencies: ModDependency[];
}
export interface ModVersions {
  projectId: string; instanceId: string; target: ResourceTarget;
  versions: ModVersion[]; total: number; offset: number;
}
type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;
function object(v: unknown): Record<string, unknown> {
  if (!v || typeof v !== "object" || Array.isArray(v)) throw new Error("Invalid catalog response");
  return v as Record<string, unknown>;
}
function text(v: unknown, max = 512): string {
  if (typeof v !== "string" || !v || v.length > max) throw new Error("Invalid catalog text");
  return v;
}
function id(v: unknown): string {
  const s = text(v, 128);
  if (!/^[a-zA-Z0-9_-]+$/.test(s)) throw new Error("Invalid catalog id");
  return s;
}
function slug(v: unknown): string {
  const s = text(v, 128);
  if (!/^[a-zA-Z0-9_.-]+$/.test(s) || s === "." || s === "..") throw new Error("Invalid catalog slug");
  return s;
}
function list(v: unknown, max: number): unknown[] {
  if (!Array.isArray(v) || v.length > max) throw new Error("Invalid catalog list");
  return v;
}
function choice<T extends string>(v: unknown, choices: readonly T[]): T {
  if (typeof v !== "string" || !choices.includes(v as T)) throw new Error("Invalid catalog enum");
  return v as T;
}
function count(v: unknown, max: number): number {
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < 0 || v > max) throw new Error("Invalid catalog count");
  return v;
}
export function modrinthApi(read: Dispatch = dispatchCoreRead) {
  async function request(payload: { command: string } & Record<string, unknown>) {
    const response = await read({ requestId: crypto.randomUUID(), payload });
    if (response.type !== "completed") throw new Error("Expected catalog response");
    return object(response.result);
  }
  return {
    async project(projectId: string): Promise<ModrinthProject> {
      const raw = await request({ command: "content.modrinth.project", projectId: id(projectId) });
      if (raw.provider !== "modrinth" || raw.projectId !== projectId) throw new Error("Catalog identity mismatch");
      if (typeof raw.description !== "string" || raw.description.length > 4096) throw new Error("Invalid description");
      return { projectId, slug: slug(raw.slug), title: text(raw.title), description: raw.description,
        clientSide: choice(raw.clientSide, ["required", "optional", "unsupported", "unknown"] as const) };
    },
    async versions(projectId: string, instanceId: string, offset = 0): Promise<ModVersions> {
      const raw = await request({ command: "content.modrinth.versions", projectId: id(projectId), instanceId: text(instanceId, 128), offset: count(offset, 4096) });
      if (raw.projectId !== projectId || raw.instanceId !== instanceId || raw.offset !== offset) throw new Error("Catalog identity mismatch");
      const target = object(raw.target);
      const versions = list(raw.versions, 20).map((item): ModVersion => {
        const v = object(item);
        return { versionId: id(v.versionId), name: text(v.name), versionNumber: text(v.versionNumber),
          kind: choice(v.kind, ["release", "beta", "alpha"] as const), published: text(v.published, 64),
          files: list(v.files, 64).map((item) => {
            const f = object(item);
            if (typeof f.primary !== "boolean") throw new Error("Invalid primary file flag");
            return { name: text(f.name, 255), bytes: count(f.bytes, 2 * 1024 ** 3), primary: f.primary };
          }),
          dependencies: list(v.dependencies, 256).map((item): ModDependency => {
            const d = object(item);
            return { kind: choice(d.kind, ["required", "optional", "incompatible", "embedded"] as const),
              ...(d.projectId !== undefined ? { projectId: id(d.projectId) } : {}),
              ...(d.versionId !== undefined ? { versionId: id(d.versionId) } : {}),
              ...(d.fileName !== undefined ? { fileName: text(d.fileName, 255) } : {}) };
          }),
        };
      });
      const total = count(raw.total, 4096);
      if (versions.length > Math.max(0, total - offset) || new Set(versions.map(v => v.versionId)).size !== versions.length) throw new Error("Invalid catalog page");
      return { projectId, instanceId, target: { gameVersion: text(target.gameVersion, 256),
        loader: choice(target.loader, ["vanilla", "fabric", "quilt", "forge", "neoforge"] as const),
        ...(target.loaderVersion ? { loaderVersion: text(target.loaderVersion) } : {}) }, versions, total, offset };
    },
  };
}
