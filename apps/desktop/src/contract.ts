// TypeScript mirror of `crates/domain` contract v1.
// Keep field names/values in sync with the Rust serde attributes and
// `contracts/domain-v1.md`. This mirror is checked for version/sample drift by
// `scripts/check-contract.mjs`; it is not generated code.

export const CONTRACT_VERSION = 1;

export type TaskState =
  | "queued"
  | "running"
  | "paused"
  | "waiting_user"
  | "succeeded"
  | "failed"
  | "cancelled";

export type ProgressUnit = "bytes" | "items" | "steps";

export interface TaskProgress {
  current: number;
  total: number | null;
  unit: ProgressUnit;
}

export type ErrorCategory =
  | "invalid_request"
  | "conflict"
  | "not_found"
  | "permission_denied"
  | "unsupported"
  | "network"
  | "storage"
  | "cancelled"
  | "internal";

export interface ErrorEnvelope {
  code: string;
  category: ErrorCategory;
  retryable: boolean;
  userAction?: string;
  detailRef?: string;
  operationId?: string;
}

export interface Task {
  id: string;
  operationId: string;
  instanceId?: string;
  inputRevision?: number;
  /** Attempt index *within one task*, starting at 1. Not a global counter and
   * not comparable across tasks; a retry increments it to fence late writes. */
  attemptGeneration: number;
  state: TaskState;
  phase: string;
  progress?: TaskProgress;
  cancelRequested: boolean;
  result?: unknown;
  error?: ErrorEnvelope;
  /** Evidence markers the module actually produced. Copy that asserts an
   * outcome must check its marker here; absence means only the neutral wording
   * may be shown. */
  evidence?: string[];
}

export type CancelDecision =
  | "requested"
  | "already_requested"
  | "not_cancellable"
  | "stale_generation"
  | "not_found";

export interface CancelTaskRequest {
  taskId: string;
  attemptGeneration: number;
}

export interface CancelTaskOutcome {
  taskId: string;
  decision: CancelDecision;
}

// ---------------------------------------------------------------------------
// Event envelope (contract v1 §6)
// ---------------------------------------------------------------------------

export interface EventEnvelope {
  eventId: string;
  schemaVersion: number;
  operationId: string;
  entityId?: string;
  revision: number;
  sequence: number;
  attemptGeneration: number;
  kind: string;
  payload: unknown;
}

export const EVENT_KIND_TASK_STATE = "task.state";
export const EVENT_KIND_TASK_PROGRESS = "task.progress";
export const EVENT_KIND_TASK_RESULT = "task.result";

export interface TaskStatePayload {
  taskId: string;
  state: TaskState;
  phase: string;
  cancelRequested: boolean;
}

export interface TaskProgressPayload {
  taskId: string;
  current: number;
  total: number | null;
  unit: ProgressUnit;
}

export interface TaskResultPayload {
  taskId: string;
  state: TaskState;
  result?: unknown;
  error?: ErrorEnvelope;
}

/** Cursor persisted by the webview so it can resume one operation stream. */
export interface StreamCursor {
  operationId: string;
  sequence: number;
}

export interface EventReplayRequest {
  operationId: string;
  afterSequence?: number;
}

export interface EventReplay {
  events: EventEnvelope[];
  cursor: StreamCursor;
  /** True when the requested window was clipped: read a fresh snapshot instead
   * of applying a partial history. */
  truncated: boolean;
}

/** Tauri event channel used for live updates. */
export const EVENT_CHANNEL = "shtmcl:event";

// ---------------------------------------------------------------------------
// View models (contract v1 §9)
// ---------------------------------------------------------------------------

export type MessageCategory =
  | "platform"
  | "account"
  | "game"
  | "network"
  | "local_file"
  | "content"
  | "room"
  | "server"
  | "launch"
  | "runtime";

export type MessageActionRole = "primary" | "secondary";

/** `detail` only expands `detailRef`; it never maps to a business command. */
export type MessageActionKind = "detail";

export interface MessageAction {
  label: string;
  capability: string;
  role: MessageActionRole;
  kind?: MessageActionKind;
}

export interface UserMessage {
  code: string;
  codeKnown: boolean;
  category: MessageCategory;
  title: string;
  body?: string;
  actions: MessageAction[];
  detailRef?: string;
  evidence: string[];
  contentVersion: string;
}

export type AvailabilityState =
  | "available"
  | "unavailable"
  | "unknown"
  | "in_progress";

export interface ActionAvailability {
  label: string;
  capability: string;
  role: MessageActionRole;
  permission?: string;
  basedOnRevision?: number;
  state: AvailabilityState;
  available: boolean;
  reason?: string;
  nextStep?: string;
  condition?: string;
}

export interface AvailabilityProjection {
  schemaVersion: string;
  revision?: number;
  requiresRecheckAtExecution: boolean;
  actions: ActionAvailability[];
  enabled: string[];
}

export const AVAILABILITY_SCHEMA_VERSION = "availability.v1";

// ---------------------------------------------------------------------------
// Application bridge snapshot
// ---------------------------------------------------------------------------

export type ModuleId =
  | "instances"
  | "launch"
  | "downloads"
  | "content"
  | "accounts"
  | "rooms"
  | "servers"
  | "diagnostics";

export interface ShellSnapshot {
  contractVersion: number;
  mode: "native" | "browser";
  tasks: Task[];
  modules: ModuleId[];
}

export interface AppInfo {
  name: string;
  version: string;
  contractVersion: number;
  native: boolean;
  /** Host facts from the native process (`windows`/`macos`/`linux`, `x86_64`/`aarch64`).
   * Update checks and platform-specific copy read these instead of inventing them. */
  platform: string;
  arch: string;
}

export type CommandKind = "read" | "mutate";

/** Stable command name carried in `payload.command`, e.g. `launch.start`.
 * Format: `^[a-z][a-z0-9._-]{2,63}$`. `action` is not an accepted alias. */
export type CommandName = string;

export interface CommandRequest {
  requestId: string;
  operationId?: string;
  entityId?: string;
  expectedRevision?: number;
  payload: { command: CommandName } & Record<string, unknown>;
}

export type CommandResponse =
  | { type: "completed"; result: unknown }
  | { type: "accepted"; operationId: string; taskId: string };
