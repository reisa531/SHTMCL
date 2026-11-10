import { dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";

type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;
export type ModMetadata = { status: "unsupported" | "invalid" | "limited" | "unavailable" | "busy" }
  | { status: "ready"; loader: "fabric"; id: string; version: string; name?: string;
    environment?: "*" | "client" | "server"; dependencies: { id: string; versions: string[] }[] };

function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw Error("Invalid mod metadata response");
  return value as Record<string, unknown>;
}
function text(value: unknown): value is string {
  return typeof value === "string" && Boolean(value.trim()) && [...value].length <= 256 && !/[\u0000-\u001f\u007f-\u009f]/.test(value);
}
export function parseModMetadata(value: unknown): ModMetadata {
  const raw = object(value);
  if (raw.status === "unsupported" || raw.status === "invalid" || raw.status === "limited" || raw.status === "unavailable" || raw.status === "busy") return { status: raw.status };
  if (raw.status !== "ready" || raw.loader !== "fabric" || !text(raw.id) || !text(raw.version)
    || !Array.isArray(raw.dependencies) || raw.dependencies.length > 64) throw Error("Invalid mod metadata");
  const result: ModMetadata = { status: "ready", loader: "fabric", id: raw.id, version: raw.version,
    dependencies: raw.dependencies.map(value => {
      const dependency = object(value);
      if (!text(dependency.id) || !Array.isArray(dependency.versions) || !dependency.versions.length
        || dependency.versions.length > 8 || !dependency.versions.every(text)) throw Error("Invalid mod dependency");
      return { id: dependency.id, versions: dependency.versions as string[] };
    }) };
  if (raw.name !== undefined) {
    if (!text(raw.name)) throw Error("Invalid mod name");
    result.name = raw.name;
  }
  if (raw.environment !== undefined) {
    if (raw.environment !== "*" && raw.environment !== "client" && raw.environment !== "server") throw Error("Invalid mod environment");
    result.environment = raw.environment;
  }
  return result;
}

export function modMetadataApi(read: Dispatch = dispatchCoreRead) {
  return {
    async inspect(instanceId: string, fileName: string): Promise<ModMetadata> {
      const response = await read({ requestId: crypto.randomUUID(), payload: { command: "instances.snapshot", instanceId, modFileName: fileName } });
      if (response.type !== "completed") throw Error("Expected completed snapshot");
      const instance = object(object(response.result).instance);
      if (instance.id !== instanceId) throw Error("Instance identity mismatch");
      if (!instance.presentation || !object(instance.presentation).modMetadata) return { status: "unavailable" };
      const detail = object(object(instance.presentation).modMetadata);
      if (detail.fileName !== fileName) throw Error("Mod identity mismatch");
      return parseModMetadata(detail.metadata);
    },
  };
}
