import { dispatchCoreMutation } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";

export interface AcceptedCopy { taskId: string; operationId: string; newInstanceId: string }
type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;
const validId = (value: unknown): value is string => typeof value === "string" && /^[a-z0-9_-]{1,64}$/i.test(value)
  && !/^(con|prn|aux|nul|com[1-9]|lpt[1-9])$/i.test(value);

/** A local reference, never an archive or an arbitrary filesystem path. */
export function encodeInstanceReference(sourceInstanceId: string): string {
  if (!validId(sourceInstanceId)) throw new Error("Invalid instance reference");
  return JSON.stringify({ format: "shtmcl-instance-reference", schemaVersion: 1, sourceInstanceId });
}
export function parseInstanceReference(text: string): string {
  if (text.length > 1024) throw new Error("Invalid instance reference");
  const value: unknown = JSON.parse(text);
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid instance reference");
  const item = value as Record<string, unknown>;
  if (item.format !== "shtmcl-instance-reference" || item.schemaVersion !== 1 || !validId(item.sourceInstanceId)
    || Object.keys(item).some((key) => !["format", "schemaVersion", "sourceInstanceId"].includes(key))) throw new Error("Invalid instance reference");
  return item.sourceInstanceId;
}

export function instanceCopyError(failure: unknown): string {
  const code = failure && typeof failure === "object" ? (failure as { code?: string }).code : undefined;
  if (code === "instances.instance.locked" || code === "instances.instance.running") return "这个实例正在使用，请结束游戏或任务后再复制。";
  if (code === "instances.instance.duplicate") return "副本已存在，请重新创建副本。";
  if (code === "instances.instance.not_found") return "找不到原实例，请刷新实例列表。";
  return "未能完成复制，请重试。";
}

export function instanceCopyApi(mutate: Dispatch = dispatchCoreMutation) {
  return {
    async copy(sourceInstanceId: string, newInstanceId: string, name: string, includeSaves: boolean): Promise<AcceptedCopy> {
      if (!validId(sourceInstanceId) || !validId(newInstanceId) || sourceInstanceId.toLowerCase() === newInstanceId.toLowerCase()
        || !name.trim() || [...name.trim()].length > 128 || /[\u0000-\u001f\u007f]/.test(name) || typeof includeSaves !== "boolean") throw new Error("Invalid copy request");
      const operationId = crypto.randomUUID();
      const response = await mutate({ requestId: crypto.randomUUID(), operationId,
        payload: { command: "instances.copy", sourceInstanceId, newInstanceId, name: name.trim(), includeSaves } });
      if (!response || response.type !== "accepted" || response.operationId !== operationId || typeof response.taskId !== "string" || !response.taskId.trim()) throw new Error("Invalid copy response");
      return { taskId: response.taskId, operationId, newInstanceId };
    },
  };
}
