import { dispatchCoreMutation, dispatchCoreRead } from "../../bridge";
import type { CommandRequest, CommandResponse } from "../../contract";

export interface LocalIdentity { kind: "offline"; username: string; uuid: string }
export interface AcceptedLaunch { operationId: string; taskId: string }
type Dispatch = (request: CommandRequest) => Promise<CommandResponse>;

function identityFrom(response: CommandResponse): LocalIdentity | null {
  if (response.type !== "completed" || !response.result || typeof response.result !== "object") {
    throw new Error("Invalid identity response");
  }
  const value = (response.result as Record<string, unknown>).identity;
  if (value === null) return null;
  if (!value || typeof value !== "object") throw new Error("Invalid identity");
  const identity = value as Record<string, unknown>;
  if (identity.kind !== "offline" || typeof identity.username !== "string"
    || !/^[A-Za-z0-9_]{1,16}$/.test(identity.username)
    || typeof identity.uuid !== "string" || !/^[0-9a-f]{32}$/.test(identity.uuid)) {
    throw new Error("Invalid identity");
  }
  return { kind: "offline", username: identity.username, uuid: identity.uuid };
}

/** The launch command prepares the environment and then starts the game process. */
export function gameLaunchApi(read: Dispatch = dispatchCoreRead, mutate: Dispatch = dispatchCoreMutation) {
  function request(command: string, payload: Record<string, unknown>, mutation: boolean): CommandRequest {
    return {
      requestId: crypto.randomUUID(),
      ...(mutation ? { operationId: crypto.randomUUID() } : {}),
      payload: { command, ...payload },
    };
  }
  return {
    async currentIdentity(): Promise<LocalIdentity | null> {
      return identityFrom(await read(request("launch.identity.current", {}, false)));
    },
    async selectOffline(username: string): Promise<LocalIdentity> {
      const identity = identityFrom(await mutate(request("launch.identity.select", { kind: "offline", username }, true)));
      if (!identity) throw new Error("Expected selected identity");
      return identity;
    },
    async launch(instanceId: string): Promise<AcceptedLaunch> {
      const response = await mutate(request("launch.game.start", { instanceId }, true));
      if (response.type !== "accepted" || typeof response.operationId !== "string" || !response.operationId || typeof response.taskId !== "string" || !response.taskId) throw new Error("Expected launch task");
      return { operationId: response.operationId, taskId: response.taskId };
    },
  };
}

export function launchError(error: unknown): string {
  const code = error && typeof error === "object" ? (error as { code?: string }).code : undefined;
  if (code === "launch.identity.required") return "先选择本地玩家名称。";
  if (code === "launch.identity.changed") return "玩家已切换，请重新启动。";
  if (code === "launch.identity.online_required") return "此游戏需要在线账户。";
  if (code === "launch.java.not_found") return "未找到 Java，请先配置游戏运行环境。";
  if (code === "launch.java.version_mismatch") return "Java 版本不适合此游戏。";
  if (code === "launch.files.missing") return "游戏文件不完整，请修复后重试。";
  if (code === "launch.request.invalid") return "请检查输入内容后重试。";
  return "未能完成操作，请重试。";
}
