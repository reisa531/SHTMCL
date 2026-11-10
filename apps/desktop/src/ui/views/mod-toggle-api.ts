import { dispatchCoreMutation, dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";
type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;
export interface ModFileState {
  instanceId: string; revision: number; state: string; fileName: string;
  enabled: boolean; bytes: number; sha256: string;
}
function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw Error("Invalid Mod state");
  return value as Record<string, unknown>;
}
function completed(response: CommandResponse) {
  if (response.type !== "completed") throw Error("Expected completed response");
  return object(response.result);
}
function parse(value: unknown, instanceId: string, fileName: string): ModFileState {
  const raw = object(value);
  if (raw.instanceId !== instanceId || raw.fileName !== fileName || !Number.isSafeInteger(raw.revision) || (raw.revision as number) < 1
    || typeof raw.state !== "string" || typeof raw.enabled !== "boolean" || !Number.isSafeInteger(raw.bytes) || (raw.bytes as number) < 0
    || typeof raw.sha256 !== "string" || !/^[a-f0-9]{64}$/.test(raw.sha256)
    || (raw.enabled ? !fileName.toLowerCase().endsWith(".jar") : !fileName.toLowerCase().endsWith(".jar.disabled"))) throw Error("Invalid Mod state");
  return { instanceId, fileName, revision: raw.revision as number, state: raw.state, enabled: raw.enabled, bytes: raw.bytes as number, sha256: raw.sha256 };
}
export function modToggleApi(read: Dispatch = dispatchCoreRead, mutate: Dispatch = dispatchCoreMutation) {
  async function snapshot(instanceId: string, fileName: string): Promise<ModFileState> {
    return parse(completed(await read({ requestId: crypto.randomUUID(), payload: { command: "instances.mod.snapshot", instanceId, fileName } })).file, instanceId, fileName);
  }
  return {
    snapshot,
    async toggle(file: ModFileState): Promise<ModFileState> {
      parse(file, file.instanceId, file.fileName);
      if (file.state !== "ready") throw Error("Instance is unavailable");
      const result = completed(await mutate({ requestId: crypto.randomUUID(), operationId: crypto.randomUUID(), expectedRevision: file.revision,
        payload: { command: "instances.mod.toggle", instanceId: file.instanceId, fileName: file.fileName, enabled: !file.enabled, sha256: file.sha256 } }));
      const instance = object(result.instance);
      if (instance.id !== file.instanceId || !Number.isSafeInteger(instance.revision) || (instance.revision as number) <= file.revision) throw Error("Mod toggle acknowledgement mismatch");
      const target = file.enabled ? `${file.fileName}.disabled` : file.fileName.slice(0, -9);
      const saved = await snapshot(file.instanceId, target);
      if (saved.sha256 !== file.sha256 || saved.enabled === file.enabled || saved.revision < (instance.revision as number)) throw Error("Mod toggle readback mismatch");
      return saved;
    },
  };
}
export function modToggleError(error: unknown): string {
  const code = error && typeof error === "object" ? (error as { code?: string }).code : undefined;
  if (code === "instances.mod.target_exists") return "目标文件名已存在，未覆盖文件。请刷新清单。";
  if (code === "instances.mod.changed") return "文件内容已变化，请刷新清单后重新选择。";
  if (code === "instances.revision.stale" || code === "instances.revision.conflict") return "实例已被其他操作修改，请重新读取状态。";
  if (code === "instances.instance.locked" || code === "instances.instance.running") return "实例正在运行或处理其他任务，请结束后重新读取状态。";
  return "未能确认操作结果，请刷新清单并检查文件状态。";
}
