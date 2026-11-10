import { dispatchCoreMutation, dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";

type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;
export interface InstanceSettingsSnapshot {
  id: string;
  name: string;
  revision: number;
  state: string;
}

function snapshot(response: CommandResponse, instanceId: string): InstanceSettingsSnapshot {
  if (response.type !== "completed") throw new Error("Expected completed response");
  const result = response.result;
  if (!result || typeof result !== "object" || Array.isArray(result)) throw new Error("Invalid instance settings response");
  const value = (result as { instance?: Partial<InstanceSettingsSnapshot> }).instance;
  if (!value || value.id !== instanceId || typeof value.name !== "string" || !value.name.trim()
    || !Number.isSafeInteger(value.revision) || value.revision! < 1 || typeof value.state !== "string") {
    throw new Error("Invalid instance settings snapshot");
  }
  return { id: value.id, name: value.name, revision: value.revision!, state: value.state };
}

export function instanceSettingsApi(read: Dispatch = dispatchCoreRead, mutate: Dispatch = dispatchCoreMutation) {
  return {
    async read(instanceId: string): Promise<InstanceSettingsSnapshot> {
      return snapshot(await read({ requestId: crypto.randomUUID(), payload: { command: "instances.snapshot", instanceId } }), instanceId);
    },
    async rename(instanceId: string, revision: number, name: string): Promise<void> {
      name = name.trim();
      if (!name || name.length > 200 || /[\u0000-\u001f\u007f]/.test(name)
        || !Number.isSafeInteger(revision) || revision < 1) throw new Error("Invalid instance name or revision");
      const result = snapshot(await mutate({
        requestId: crypto.randomUUID(), operationId: crypto.randomUUID(), expectedRevision: revision,
        payload: { command: "instances.update", instanceId, patch: { name } },
      }), instanceId);
      if (result.name !== name || result.revision <= revision) throw new Error("Instance rename response mismatch");
    },
  };
}

export function instanceSettingsError(error: unknown): string {
  const code = error && typeof error === "object" ? (error as { code?: string }).code : undefined;
  if (code === "instances.revision.stale" || code === "instances.revision.conflict") return "实例已被其他操作修改，请重新读取后再保存。";
  if (code === "instances.instance.running") return "游戏正在运行，请退出游戏后再修改。";
  if (code === "instances.instance.locked") return "实例正在处理其他任务，请稍后重新读取。";
  if (code === "instances.instance.not_found") return "实例已移除，请返回实例列表。";
  return "未能确认保存结果，请重新读取实例后再操作。";
}
