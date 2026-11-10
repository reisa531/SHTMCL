import type {
  AppInfo,
  CancelTaskOutcome,
  CommandRequest,
  CommandResponse,
  EventEnvelope,
  EventReplay,
  ShellSnapshot,
  StreamCursor,
} from "./contract";
import { CONTRACT_VERSION, EVENT_CHANNEL } from "./contract";

type TauriWindow = Window & { __TAURI_INTERNALS__?: unknown };

export async function openDownloadDestination(operationId: string, taskId: string): Promise<void> {
  if (!isTauriRuntime()) throw new Error("Native runtime required");
  const { invoke } = await import("@tauri-apps/api/core");
  await invoke("open_download_destination", { operationId, taskId });
}

export function isTauriRuntime(): boolean {
  if (typeof window === "undefined") {
    return false;
  }
  return Boolean((window as TauriWindow).__TAURI_INTERNALS__);
}

const browserAppInfo: AppInfo = {
  name: "SHTMCL",
  // Keep in sync with the desktop package/Cargo version while the browser
  // preview has no native process to ask.
  version: "0.1.0",
  contractVersion: CONTRACT_VERSION,
  native: false,
  // A browser preview has no host process; say so instead of guessing the OS.
  platform: "unknown",
  arch: "unknown",
};

export async function readAppInfo(): Promise<AppInfo> {
  if (!isTauriRuntime()) {
    return browserAppInfo;
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<AppInfo>("app_info");
}

export async function readShellSnapshot(): Promise<ShellSnapshot> {
  if (!isTauriRuntime()) {
    // Browser preview is deliberately empty: it must not invent an account,
    // installed instances or a running game.
    return {
      contractVersion: CONTRACT_VERSION,
      mode: "browser",
      tasks: [],
      modules: [],
    };
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<ShellSnapshot>("get_shell_snapshot");
}

export async function requestCancel(
  taskId: string,
  attemptGeneration: number,
): Promise<CancelTaskOutcome> {
  if (!isTauriRuntime()) {
    throw new Error("cancellation requires the desktop core");
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<CancelTaskOutcome>("request_cancel", {
    request: { taskId, attemptGeneration },
  });
}

/** Dispatch a read through the single Rust module registry. */
export async function dispatchCoreRead(
  request: CommandRequest,
): Promise<CommandResponse> {
  if (!isTauriRuntime()) {
    throw new Error("core dispatch requires the desktop runtime");
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<CommandResponse>("dispatch_core_read", { request });
}

/** Dispatch a mutation through the shared task and event infrastructure. */
export async function dispatchCoreMutation(
  request: CommandRequest,
): Promise<CommandResponse> {
  if (!isTauriRuntime()) {
    throw new Error("core dispatch requires the desktop runtime");
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<CommandResponse>("dispatch_core_mutation", { request });
}

/**
 * Replay one operation stream after a saved cursor.
 *
 * A live push may be missed while the webview is frozen or reloaded; the
 * retained stream plus a `StreamCursor` is the recovery path. When `truncated`
 * is true the caller must read a fresh snapshot instead of applying history.
 */
export async function readEvents(
  operationId: string,
  afterSequence?: number,
): Promise<EventReplay> {
  if (!isTauriRuntime()) {
    return {
      events: [],
      cursor: { operationId, sequence: afterSequence ?? 0 },
      truncated: false,
    };
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<EventReplay>("get_events", {
    request: { operationId, afterSequence },
  });
}

/**
 * Subscribe to live events. Returns an unsubscribe function.
 *
 * Callers must still deduplicate by `eventId` and compare `attemptGeneration`:
 * a push that races a reload can be delivered twice.
 */
export async function listenEvents(
  handler: (event: EventEnvelope) => void,
): Promise<() => void> {
  if (!isTauriRuntime()) {
    return () => {};
  }
  const { listen } = await import("@tauri-apps/api/event");
  const unlisten = await listen<EventEnvelope>(EVENT_CHANNEL, (message) => {
    handler(message.payload);
  });
  return unlisten;
}

/** Cursor helper so callers do not invent their own ordering rules. */
export function cursorFrom(event: EventEnvelope): StreamCursor {
  return { operationId: event.operationId, sequence: event.sequence };
}

export function isNewerThanCursor(
  event: EventEnvelope,
  cursor: StreamCursor | null,
): boolean {
  if (!cursor) {
    return true;
  }
  if (event.operationId !== cursor.operationId) {
    return true;
  }
  return event.sequence > cursor.sequence;
}
