import { dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";
export interface FabricVersion { version: string; stable: boolean }
type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;
export function fabricApi(read: Dispatch = dispatchCoreRead) {
  return {
    async versions(gameVersion: string): Promise<FabricVersion[]> {
      if (!gameVersion.trim() || gameVersion.length > 256 || /[\\/\u0000-\u001f\u007f]/.test(gameVersion)) throw Error("Invalid game version");
      const response = await read({ requestId: crypto.randomUUID(), payload: { command: "launch.fabric.versions", gameVersion } });
      if (!response || response.type !== "completed" || !response.result || typeof response.result !== "object") throw Error("Invalid Fabric response");
      const result = response.result as Record<string, unknown>;
      if (result.gameVersion !== gameVersion || !Array.isArray(result.entries) || result.entries.length > 4096) throw Error("Invalid Fabric catalogue");
      const seen = new Set<string>();
      return result.entries.map((entry: unknown) => {
        if (!entry || typeof entry !== "object") throw Error("Invalid Fabric version");
        const value = entry as Record<string, unknown>;
        if (typeof value.version !== "string" || !/^[A-Za-z0-9._+-]{1,64}$/.test(value.version) || value.version.includes("..") || typeof value.stable !== "boolean" || seen.has(value.version)) throw Error("Invalid Fabric version");
        seen.add(value.version); return { version: value.version, stable: value.stable };
      });
    },
  };
}
