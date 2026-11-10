import type { Task, TaskState } from "../../contract";

export type DownloadResourceType = "game_environment" | "loader" | "mod" | "shader" | "resourcepack" | "datapack" | "modpack";

/** Business identity and destination supplied by the application use case. */
export interface DownloadPresentation {
  operationId: string;
  taskId: string;
  name: string;
  version?: string;
  resourceType: DownloadResourceType;
  destination: {
    kind: "cache" | "instance" | "file";
    path?: string;
    instanceId?: string;
    instanceName?: string;
  };
  state?: TaskState;
  phase?: string;
  errorCode?: string;
  canRetryInstall?: boolean;
  history?: {
    state: TaskState;
    phase: string;
    attemptGeneration: number;
    errorCode?: string;
    interrupted?: boolean;
    activeElsewhere?: boolean;
  };
}

const RESOURCE_LABELS: Record<DownloadResourceType, string> = {
  game_environment: "启动包", loader: "加载器", mod: "Mod", shader: "光影",
  resourcepack: "材质包", datapack: "数据包", modpack: "整合包",
};
const STATES = new Set<TaskState>(["queued", "running", "paused", "waiting_user", "succeeded", "failed", "cancelled"]);
const object = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
const text = (value: unknown): value is string => typeof value === "string" && value.trim().length > 0 && value.length <= 4096;

/** An unsupported entry is skipped without discarding other valid records. */
export function parseDownloadPresentations(value: unknown): DownloadPresentation[] {
  if (!Array.isArray(value) || value.length > 10000) return [];
  const result: DownloadPresentation[] = [];
  const seen = new Map<string, Set<string>>();
  for (const entry of value) {
    if (!object(entry) || !text(entry.operationId) || !text(entry.taskId) || !text(entry.name)) continue;
    if (!text(entry.resourceType) || !Object.prototype.hasOwnProperty.call(RESOURCE_LABELS, entry.resourceType)) continue;
    const destination = entry.destination;
    if (!object(destination) || (destination.kind !== "cache" && destination.kind !== "instance" && destination.kind !== "file")) continue;
    if ([entry.version, entry.phase, entry.errorCode, destination.path, destination.instanceId, destination.instanceName].some((item) => item !== undefined && !text(item))) continue;
    if (entry.state !== undefined && !STATES.has(entry.state as TaskState)) continue;
    if (entry.canRetryInstall !== undefined && typeof entry.canRetryInstall !== "boolean") continue;
    let history: DownloadPresentation["history"];
    if (entry.history !== undefined) {
      const value = entry.history;
      if (!object(value) || !STATES.has(value.state as TaskState) || !text(value.phase)
        || !Number.isSafeInteger(value.attemptGeneration) || (value.attemptGeneration as number) < 1
        || (value.errorCode !== undefined && !text(value.errorCode))
        || [value.interrupted, value.activeElsewhere].some((flag) => flag !== undefined && typeof flag !== "boolean")) continue;
      history = { state: value.state as TaskState, phase: value.phase as string, attemptGeneration: value.attemptGeneration as number,
        ...(value.errorCode === undefined ? {} : { errorCode: value.errorCode as string }),
        ...(value.interrupted === undefined ? {} : { interrupted: value.interrupted as boolean }),
        ...(value.activeElsewhere === undefined ? {} : { activeElsewhere: value.activeElsewhere as boolean }) };
    }
    const taskIds = seen.get(entry.operationId) ?? new Set<string>();
    if (taskIds.has(entry.taskId)) continue;
    taskIds.add(entry.taskId);
    seen.set(entry.operationId, taskIds);
    result.push({
      operationId: entry.operationId, taskId: entry.taskId, name: entry.name,
      resourceType: entry.resourceType as DownloadResourceType,
      ...(entry.canRetryInstall === undefined ? {} : { canRetryInstall: entry.canRetryInstall as boolean }),
      ...(history === undefined ? {} : { history }),
      ...(entry.version === undefined ? {} : { version: entry.version as string }),
      ...(entry.state === undefined ? {} : { state: entry.state as TaskState }),
      ...(entry.phase === undefined ? {} : { phase: entry.phase as string }),
      ...(entry.errorCode === undefined ? {} : { errorCode: entry.errorCode as string }),
      destination: {
        kind: destination.kind as DownloadPresentation["destination"]["kind"],
        ...(destination.path === undefined ? {} : { path: destination.path as string }),
        ...(destination.instanceId === undefined ? {} : { instanceId: destination.instanceId as string }),
        ...(destination.instanceName === undefined ? {} : { instanceName: destination.instanceName as string }),
      },
    });
  }
  return result;
}

export function presentationForTask(task: Task, presentations: readonly DownloadPresentation[]): DownloadPresentation | undefined {
  return presentations.find((entry) => entry.taskId === task.id && entry.operationId === task.operationId);
}

export function downloadDestinationLabel(entry: DownloadPresentation): string {
  const { destination } = entry;
  const target = destination.kind === "instance"
    ? `实例 · ${destination.instanceName ?? destination.instanceId ?? "名称待读取"}`
    : destination.kind === "cache" ? "本地缓存" : "本地文件";
  return `${RESOURCE_LABELS[entry.resourceType]} → ${target}`;
}

export function downloadPhaseLabel(phase: string): string {
  const labels: Record<string, string> = {
    install: "准备安装", metadata: "读取游戏信息", download_files: "获取游戏文件",
    assets: "补齐游戏资源", natives: "准备运行库", commit: "完成安装",
  };
  return labels[phase] ?? "处理中";
}
