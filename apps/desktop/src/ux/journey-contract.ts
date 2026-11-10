// Typed surface for the pure journey runtime in `src/ux/journeys/`.
//
// The runtime is framework-free ES2021 (`.mjs`) so Node tests, Vite and the
// Tauri WebView share one implementation. This file is the only entry point the
// UI may use; it does not re-implement any rule.
//
// Naming: the wire contract (`contracts/domain-v1.md`, `src/contract.ts`) is
// camelCase and is the single source of truth. The runtime internally uses
// snake_case as a *local* choice; `taskFromContract` below is the only
// adaptation point, so a contract change has exactly one place to update.
// Do not read the runtime's internal naming as the contract.

import {
  reconcileWithTask as reconcileWithTaskRaw,
} from "./journeys/model.mjs";
import type { Task as DomainTask } from "../contract";

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

// Wire shapes come from the runtime's own declaration file (single source).
// Import for local use and re-export so callers never maintain a second copy.
import type {
  WireTask,
  CoreEventEnvelope,
} from "./journeys/model.mjs";

export type { WireTask, CoreEventEnvelope };

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

export type RunEvent = CoreEventInput;

/**
 * Named re-exports of the runtime. The runtime's own declarations
 * (`src/journeys/model.d.mts`) are the single source of these types; this module
 * adds only the domain-contract adapter below.
 */
export {
  createJourney,
  reduceJourney,
  restoreJourney,
  journeyView,
  journeyLog,
} from "./journeys/model.mjs";

/**
 * Adapt the typed domain `Task` into the runtime's input shape.
 *
 * The wire contract and the runtime both use camelCase (frozen in
 * `contracts/domain-v1.md` §12), so this is a normalization pass, not a rename
 * layer: it only materializes absent optionals and forwards `evidence` verbatim.
 * `evidence` is the proof that a "retained/completed" wording is allowed; dropping
 * it would silently make every evidence-gated message unreachable.
 */
export function taskFromContract(task: DomainTask): WireTask {
  return {
    id: task.id,
    operationId: task.operationId,
    instanceId: task.instanceId ?? null,
    inputRevision: task.inputRevision ?? null,
    attemptGeneration: task.attemptGeneration,
    state: task.state,
    phase: task.phase,
    progress: task.progress ?? null,
    cancelRequested: task.cancelRequested,
    result: task.result ?? null,
    error: task.error
      ? {
          code: task.error.code,
          detailRef: task.error.detailRef ?? null,
        }
      : null,
    evidence: task.evidence ?? [],
  };
}

/** Reconcile a journey with a core task snapshot from the typed contract. */
export function reconcileWithContractTask(
  state: JourneyState,
  task: DomainTask,
): JourneyState {
  return reconcileWithTaskRaw(state, taskFromContract(task));
}
