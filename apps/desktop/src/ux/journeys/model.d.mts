// Types for the framework-free journey runtime (`src/journeys/*.mjs`).
//
// The runtime is plain ES2021 so Node tests, Vite and the Tauri WebView share one
// implementation without a build step. `src/ux/journey-contract.ts` is the only UI
// entry point; it adapts the typed domain contract onto this runtime.
//
// When 00 freezes the public DTOs in `crates/domain`, overlapping definitions here
// must be replaced by generated types and this file reduced to runtime helpers.


export interface ReturnPoint {
  route: string;
  entityId?: string | null;
  revision?: number | null;
  label?: string | null;
}

export interface PendingChoice {
  kind: string;
  valueId?: string;
  roomId?: string | null;
  providerId?: string | null;
  identityId?: string | null;
  profileIds?: string[];
}

export interface MessageRef {
  code: string;
  detailRef: string | null;
}

export interface CancelState {
  requestedAt: number | true | null;
  confirmedAt: number | true | null;
}

export interface WireTask {
  id: string;
  operationId: string;
  instanceId?: string | null;
  inputRevision?: number | null;
  attemptGeneration: number;
  state: string;
  phase?: string;
  progress?: unknown;
  cancelRequested?: boolean;
  result?: unknown;
  error?: { code?: string; detailRef?: string | null } | null;
  evidence?: string[];
}

export interface CoreEventEnvelope {
  eventId: string;
  schemaVersion?: string | number;
  operationId?: string | null;
  entityId?: string | null;
  revision?: number;
  sequence?: number;
  attemptGeneration?: number;
  kind: string;
  payload?: Record<string, unknown>;
}

export type JourneyPhase =
  | "idle"
  | "in_progress"
  | "waiting"
  | "need_choice"
  | "blocked"
  | "cancelling"
  | "completed"
  | "failed"
  | "cancelled";

export interface JourneyState {
  readonly schemaVersion: string;
  readonly journeyId: string;
  readonly kind: string;
  readonly steps: readonly string[];
  readonly step: string;
  readonly phase: JourneyPhase;
  readonly operationId: string | null;
  readonly taskId: string | null;
  readonly entityId: string | null;
  readonly revision: number | null;
  readonly attemptGeneration: number;
  readonly returnPoint: ReturnPoint | null;
  readonly pendingChoice: PendingChoice | null;
  readonly cancel: CancelState;
  readonly lastSequence: number | null;
  readonly seenEventIds: readonly string[];
  readonly seenRequestIds: readonly string[];
  readonly message: MessageRef | null;
  readonly evidence: readonly string[];
  readonly log: readonly Readonly<Record<string, unknown>>[];
}

export interface JourneyView {
  readonly journeyId: string;
  readonly kind: string;
  readonly phase: JourneyPhase;
  readonly step: string;
  readonly operationId: string | null;
  readonly taskId: string | null;
  readonly entityId: string | null;
  readonly revision: number | null;
  readonly attemptGeneration: number;
  readonly returnPoint: ReturnPoint | null;
  readonly pendingChoice: PendingChoice | null;
  readonly canCancel: boolean;
  readonly cancelPending: boolean;
  readonly message: MessageRef | null;
  readonly evidence: readonly string[];
  readonly lastSequence: number | null;
}

export interface JourneyInit {
  kind: string;
  journeyId: string;
  steps?: string[];
  entityId?: string | null;
  revision?: number | null;
  returnPoint?: ReturnPoint | null;
  pendingChoice?: PendingChoice | null;
}

export type CoreEventInput =
  | { type: "intent.start"; requestId: string; operationId?: string | null; taskId?: string | null; step?: string; continuation?: boolean }
  | { type: "intent.resume"; requestId: string; taskId?: string | null; returnPoint?: ReturnPoint | null }
  | { type: "operation.accepted"; operationId?: string | null; taskId?: string | null; entityId?: string | null; revision?: number }
  | { type: "task.changed"; task: WireTask; at?: number | null }
  | { type: "business.event"; envelope: CoreEventEnvelope; at?: number | null }
  | { type: "cancel.request"; requestId: string; at?: number | null }
  | {
      type: "auth.callback";
      requestId?: string;
      attemptId?: string | null;
      result: string;
      errorCode?: string;
      detailRef?: string | null;
      providerId?: string | null;
      identityId?: string | null;
      profileIds?: string[];
    }
  | { type: "choice.select"; requestId: string; kind?: string; valueId: string }
  | { type: "returnToTask"; to?: string | null; returnPoint?: ReturnPoint | null };

export const JOURNEY_SCHEMA_VERSION: string;
export const JOURNEY_KINDS: Readonly<Record<string, readonly string[]>>;
export const TASK_STATES: readonly string[];
export const PHASES: readonly JourneyPhase[];

export function createJourney(init: JourneyInit): JourneyState;
export function restoreJourney(snapshot: unknown): JourneyState | null;
export function reduceJourney(state: JourneyState, event: CoreEventInput): JourneyState;
export function reconcileWithTask(state: JourneyState, task: WireTask, at?: number | null): JourneyState;
export function journeyView(state: JourneyState): JourneyView;
export function journeyLog(state: JourneyState): readonly Readonly<Record<string, unknown>>[];
