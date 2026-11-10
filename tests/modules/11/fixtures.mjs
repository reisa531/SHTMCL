// Shared fixtures for module 11 tests.
//
// Shapes follow the FROZEN contract (`contracts/domain-v1.md` §4, §6, §12):
// every field is camelCase, including event payload keys. If these diverge from
// the frozen contract the tests validate a contract nobody implements.

let seq = 0;

export function resetSeq() {
  seq = 0;
}

export function nextSeq() {
  seq += 1;
  return seq;
}

/** Minimal core Task projection (frozen contract §4). */
export function task(overrides = {}) {
  return {
    id: 'task-1',
    operationId: 'op-1',
    instanceId: 'inst-1',
    inputRevision: 3,
    attemptGeneration: 1,
    state: 'running',
    phase: 'download',
    progress: { current: 1, total: 10, unit: 'items' },
    cancelRequested: false,
    result: null,
    error: null,
    evidence: [],
    ...overrides,
  };
}

/** Event envelope (frozen contract §6) plus the §12 payload key for its kind. */
export function envelope(overrides = {}) {
  return {
    eventId: `ev-${nextSeq()}`,
    schemaVersion: 1,
    operationId: 'op-1',
    entityId: 'room-1',
    revision: 5,
    sequence: nextSeq(),
    attemptGeneration: 1,
    kind: 'membership.changed',
    payload: { approvalState: 'requested' },
    ...overrides,
  };
}
