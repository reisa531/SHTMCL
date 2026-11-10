import { dispatchCoreMutation, dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";

export interface CachedEnvironment {
  id: string;
  versionId: string;
  platform: "windows-x64";
  resourceCount: number;
  bytes: number;
  path: string;
  pinned: boolean;
}
export interface EnvironmentCatalogue {
  entries: CachedEnvironment[];
  skippedEntries: number;
}
type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;
export function environmentActionError(action: "pin" | "repair" | "remove", failure: unknown): string {
  const code = failure && typeof failure === "object" ? (failure as {code?: string}).code : undefined;
  if (code === "launch.install.busy") return "这个环境正在使用，请在任务结束后重试。";
  if (code === "launch.environment.pinned") return "这个环境已固定保留，请先取消保留。";
  return action === "pin" ? "未能保存保留设置，请重试。" : action === "remove" ? "未能移除缓存清单，请重试。" : "未能开始校验，请重试。";
}
const object = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
const text = (value: unknown, limit: number): value is string => typeof value === "string" && value.trim().length > 0 && value.length <= limit;
const count = (value: unknown): value is number => Number.isSafeInteger(value) && (value as number) >= 0;

/** Reject malformed catalogues instead of presenting them as a genuine empty library. */
export function parseCachedEnvironments(value: unknown): CachedEnvironment[] {
  if (!Array.isArray(value) || value.length > 10000) throw new Error("Invalid environment catalogue");
  const ids = new Set<string>();
  return value.map((entry) => {
    if (!object(entry) || typeof entry.id !== "string" || !/^[a-f0-9]{64}$/i.test(entry.id)
      || !text(entry.versionId, 256) || entry.platform !== "windows-x64"
      || !count(entry.resourceCount) || !count(entry.bytes) || !text(entry.path, 4096)
      || (entry.pinned !== undefined && typeof entry.pinned !== "boolean")
      || ids.has(entry.id)) throw new Error("Invalid cached environment");
    ids.add(entry.id);
    return { id: entry.id, versionId: entry.versionId, platform: entry.platform,
      resourceCount: entry.resourceCount, bytes: entry.bytes, path: entry.path, pinned: entry.pinned ?? false };
  });
}

/** Injectable dispatchers keep tests on the same command path as the native UI. */
export function environmentApi(read: Dispatch = dispatchCoreRead, mutate: Dispatch = dispatchCoreMutation) {
  async function catalogue(): Promise<EnvironmentCatalogue> {
    const response = await read({ requestId: crypto.randomUUID(), payload: { command: "launch.environments.list" } });
    if (!object(response) || response.type !== "completed" || !object(response.result)) throw new Error("Expected environment catalogue");
    const skippedEntries = response.result.skippedEntries === undefined ? 0 : response.result.skippedEntries;
    if (!count(skippedEntries)) throw new Error("Invalid skipped environment count");
    return { entries: parseCachedEnvironments(response.result.entries), skippedEntries };
  }
  function validateId(environmentId: string) {
    if (!/^[a-f0-9]{64}$/i.test(environmentId)) throw new Error("Invalid environment identity");
  }
  async function completedMutation(command: string, environmentId: string, extra: Record<string, unknown> = {}) {
    validateId(environmentId);
    const response = await mutate({ requestId: crypto.randomUUID(), operationId: crypto.randomUUID(),
      payload: { command, environmentId, ...extra } });
    if (!object(response) || response.type !== "completed" || !object(response.result)
      || response.result.environmentId !== environmentId) throw new Error("Expected environment mutation result");
    return response.result;
  }
  return {
    catalogue,
    async pin(environmentId: string, pinned: boolean): Promise<void> {
      if (typeof pinned !== "boolean") throw new Error("Invalid pin state");
      const result = await completedMutation("launch.environments.pin", environmentId, { pinned });
      if (result.pinned !== pinned) throw new Error("Expected saved pin state");
    },
    async remove(environmentId: string): Promise<void> {
      const result = await completedMutation("launch.environments.remove", environmentId);
      if (result.removed !== true) throw new Error("Expected removed environment");
    },
    async repair(environmentId: string): Promise<string> {
      validateId(environmentId);
      const operationId = crypto.randomUUID();
      const response = await mutate({ requestId: crypto.randomUUID(), operationId,
        payload: { command: "launch.environments.repair", environmentId } });
      if (!object(response) || response.type !== "accepted" || !text(response.taskId, 4096)
        || response.operationId !== operationId) throw new Error("Expected repair task");
      return response.taskId;
    },
    async list(): Promise<CachedEnvironment[]> {
      return (await catalogue()).entries;
    },
    async download(versionId: string): Promise<string> {
      if (!text(versionId, 256)) throw new Error("Invalid game version");
      const operationId = crypto.randomUUID();
      const response = await mutate({ requestId: crypto.randomUUID(), operationId,
        payload: { command: "launch.environments.download", versionId } });
      if (!object(response) || response.type !== "accepted" || !text(response.taskId, 4096)
        || response.operationId !== operationId) throw new Error("Expected environment task");
      return response.taskId;
    },
  };
}
