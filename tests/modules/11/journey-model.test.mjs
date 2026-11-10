// Module 11 · JourneyContext 状态转换测试（node:test，无第三方依赖）
import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  createJourney,
  reduceJourney,
  restoreJourney,
  reconcileWithTask,
  journeyView,
  journeyLog,
  JOURNEY_SCHEMA_VERSION,
} from '../../../apps/desktop/src/ux/journeys/model.mjs';
import { task, envelope, resetSeq } from './fixtures.mjs';

function joinJourney() {
  return createJourney({
    kind: 'join_room',
    journeyId: 'j-room-1',
    entityId: 'room-1',
    revision: 5,
    returnPoint: { route: 'room:room-1', entityId: 'room-1', revision: 5 },
  });
}

function started() {
  return reduceJourney(joinJourney(), { type: 'intent.start', requestId: 'req-join', operationId: 'op-1' });
}

/** 一次真实提交的完整前半段：意图提交 + 核心 accepted（绑�task-1/op-1）�*/
function startedAndAccepted() {
  return reduceJourney(started(), { type: 'operation.accepted', operationId: 'op-1', taskId: 'task-1', revision: 5 });
}

function accepted(s, gen = 1) {
  return reduceJourney(s, { type: 'operation.accepted', operationId: 'op-1', taskId: 'task-1', revision: 5 });
}
const withTask = (gen) => accepted(started());

function membership(s, payload, overrides = {}) {
  return reduceJourney(s, { type: 'business.event', envelope: envelope({ payload, ...overrides }) });
}

function lastLog(s) {
  return journeyLog(s).at(-1);
}

// ---------------------------------------------------------------------------
test('createJourney validates kind and freezes state', () => {
  assert.throws(() => createJourney({ kind: 'nope', journeyId: 'x' }), /unknown journey kind/);
  assert.throws(() => createJourney({ kind: 'login' }), /journeyId/);
  const j = createJourney({ kind: 'login', journeyId: 'j-login' });
  assert.equal(j.schemaVersion, JOURNEY_SCHEMA_VERSION);
  assert.equal(j.phase, 'idle');
  assert.ok(Object.isFrozen(j));
});

test('start creates operation, bumps attempt generation, second click returns original session', () => {
  resetSeq();
  const s1 = started();
  assert.equal(s1.operationId, 'op-1');
  assert.equal(s1.attemptGeneration, 1);
  assert.equal(s1.phase, 'in_progress');

  const replay = reduceJourney(s1, { type: 'intent.start', requestId: 'req-join', operationId: 'op-1' });
  assert.equal(replay, s1, 'same request id must not change state');

  //  提交与核�accepted 之间（高延迟下最常见）：重放同一 operation 必须是空操作�  // 不能因为还没�task 就再提交一次�
  const beforeAccept = reduceJourney(s1, { type: 'intent.start', requestId: 'req-join-3', operationId: 'op-1' });
  assert.equal(beforeAccept, s1, 're-clicking the submitted operation is a no-op');

  //  核心接受后，会话身份�op-1/task-1 承担；此后再次点击必须回到原会话�
  const live = startedAndAccepted();
  assert.equal(live.taskId, 'task-1');

  const otherClick = reduceJourney(live, { type: 'intent.start', requestId: 'req-join-2', operationId: 'op-2' });
  assert.equal(otherClick.operationId, 'op-1', 'active operation is never replaced by a new click');
  assert.equal(otherClick.taskId, 'task-1', 'the bound core task is never replaced by a new click');
  assert.equal(otherClick.attemptGeneration, 1, 'a stray click must not bump the attempt generation');
  assert.equal(otherClick.phase, 'in_progress');
  assert.equal(lastLog(otherClick).ignored, 'operation_active');

  // 失败之后是一次新操作：`failed` 是终态，不再接受任何事件（含"重试"事件本身），
  //  因此应用层为一次新旅程传入 continuation 标记，由 `intent.start` 承接原任务�
  const failed = reduceJourney(live, { type: 'task.changed', task: task({ state: 'failed', error: { code: 'room.denied' } }) });
  assert.equal(failed.phase, 'failed');
  const retried = reduceJourney(failed, { type: 'intent.start', requestId: 'req-retry', operationId: 'op-3', continuation: true });
  assert.equal(retried.operationId, 'op-3');
  assert.equal(retried.taskId, 'task-1', 'a continuation keeps the core task; only the generation is bumped');
  assert.equal(retried.attemptGeneration, 2, 'the retry must fence the previous attempt');
  assert.equal(retried.phase, 'in_progress');
  assert.equal(retried.message, null);

  //  旧代次的失败快照不得把新尝试打回失败�
  const lateOld = reduceJourney(retried, { type: 'task.changed', task: task({ attemptGeneration: 1, state: 'failed', error: { code: 'room.denied' } }) });
  assert.equal(lateOld, retried, 'a late snapshot from the failed attempt must not touch the retry');
});

// ---------------------------------------------------------------------------
test('approval waiting, approval by owner, then connection events advance without inventing success', () => {
  resetSeq();
  let s = withTask();
  s = membership(s, { approvalState: 'requested' });
  assert.equal(s.phase, 'waiting');
  assert.equal(s.step, 'approval');
  assert.deepEqual(s.pendingChoice, { kind: 'approval', roomId: 'room-1' });

  s = membership(s, { approvalState: 'approved' });
  assert.equal(s.phase, 'in_progress');
  assert.equal(s.step, 'connect');
  assert.notEqual(s.phase, 'completed', 'approved membership is not a connected or completed journey');

  s = membership(s, { state: 'connected' }, { kind: 'connection.changed' });
  assert.equal(s.phase, 'in_progress');
  assert.equal(s.step, 'connect');
});

test('denied approval fails the journey with room.denied and no success evidence', () => {
  resetSeq();
  let s = membership(withTask(), { approvalState: 'requested' });
  s = membership(s, { approvalState: 'denied' });
  assert.equal(s.phase, 'failed');
  assert.equal(s.message.code, 'room.denied');
  assert.equal(s.evidence.includes('approved'), false);
});

test('duplicate business events are ignored even when they would change the phase', () => {
  resetSeq();
  let s = membership(withTask(), { approvalState: 'requested' });
  const ev = envelope({ payload: { approvalState: 'approved' } });
  s = reduceJourney(s, { type: 'business.event', envelope: ev });
  const after = s;
  //  重复投递完全幂等：返回同一引用，UI 直接跳过渲染�
  const dup = reduceJourney(s, { type: 'business.event', envelope: ev });
  assert.equal(dup, after, 'a duplicate event must not produce a new state object');
  assert.equal(dup.phase, after.phase);

  // A late "requested" event with new id but older sequence must not regress the phase.
  const s2 = reduceJourney(s, {
    type: 'business.event',
    envelope: envelope({ payload: { approvalState: 'requested' }, sequence: 0 }),
  });
  assert.equal(s2, s, 'an out-of-order event must not mutate the journey');
  assert.equal(s2.step, 'connect');
  assert.equal(s2.phase, 'in_progress');
});

// ---------------------------------------------------------------------------
test('cancel is pending until core confirms; non-terminal task updates do not end the journey', () => {
  resetSeq();
  let s = withTask();
  s = reduceJourney(s, { type: 'task.changed', task: task({ state: 'running' }) });
  assert.equal(s.phase, 'in_progress');

  s = reduceJourney(s, { type: 'cancel.request', requestId: 'req-cancel' });
  assert.equal(s.phase, 'cancelling');
  assert.equal(s.cancel.confirmedAt, null);
  assert.equal(journeyView(s).canCancel, true);
  assert.equal(journeyView(s).cancelPending, true);

  const replay = reduceJourney(s, { type: 'cancel.request', requestId: 'req-cancel' });
  assert.equal(replay, s);

  // Core still reports running / waiting_user: journey must stay cancelling.
  for (const st of ['running', 'waiting_user', 'paused']) {
    s = reduceJourney(s, { type: 'task.changed', task: task({ state: st }) });
    assert.equal(s.phase, 'cancelling', `state ${st} must not end cancel`);
  }
  // Cancel request itself does not create success or cancellation fact.
  assert.equal(s.cancel.confirmedAt, null);

  s = reduceJourney(s, { type: 'task.changed', task: task({ state: 'cancelled', cancel_requested: true, attemptGeneration: 1 }) });
  assert.equal(s.phase, 'cancelled');
  assert.notEqual(s.cancel.confirmedAt, null);
  assert.equal(journeyView(s).cancelPending, false);
});

test('cancel without a core task explains it was not submitted instead of pretending to stop', () => {
  const s = reduceJourney(joinJourney(), { type: 'cancel.request', requestId: 'req-x' });
  assert.equal(s.phase, 'idle');
  assert.equal(s.message.code, 'operation.cancel.not_started');
  assert.equal(lastLog(s).ignored, 'no_task');
});

// ---------------------------------------------------------------------------
test('approval race after cancel: approved while cancelling does not un-cancel; server left confirms', () => {
  resetSeq();
  let s = withTask();
  s = membership(s, { approvalState: 'requested' });
  assert.equal(s.phase, 'waiting');

  s = reduceJourney(s, { type: 'cancel.request', requestId: 'req-leave' });
  assert.equal(s.phase, 'cancelling');

  // Late approval arrives while cancel is pending.
  s = membership(s, { approvalState: 'approved' });
  assert.equal(s.phase, 'cancelling', 'approval must not resurrect a cancelled join');
  assert.equal(s.pendingChoice.kind, 'approval', 'pending approval stays visible until server confirms');

  // Even task success-like running progress cannot complete the journey.
  s = reduceJourney(s, { type: 'task.changed', task: task({ state: 'succeeded' }) });
  assert.equal(s.phase, 'cancelling');

  // Server confirms leave (same membership transaction serialized leave after approve).
  s = membership(s, { approvalState: 'left' });
  assert.equal(s.phase, 'cancelled');
  assert.notEqual(s.cancel.confirmedAt, null);
  assert.equal(s.pendingChoice, null);

  // Terminal state never accepts more events.
  const afterTerminal = membership(s, { approvalState: 'approved' });
  assert.equal(afterTerminal, s);
});

test('approve committed first then cancel: service closes permit and reports left', () => {
  resetSeq();
  let s = membership(withTask(), { approvalState: 'requested' });
  s = membership(s, { approvalState: 'approved' });
  s = reduceJourney(s, { type: 'cancel.request', requestId: 'req-leave' });
  assert.equal(s.phase, 'cancelling');
  s = membership(s, { approvalState: 'left' });
  assert.equal(s.phase, 'cancelled');
});

// ---------------------------------------------------------------------------
test('old attempt generation events and task snapshots are ignored', () => {
  resetSeq();
  //  核心任务 task-1 的第一次尝试失败（代次 1）�
  let s = withTask(1);
  s = reduceJourney(s, { type: 'task.changed', task: task({ attemptGeneration: 1, state: 'failed', error: { code: 'download.hash_mismatch' } }) });
  assert.equal(s.phase, 'failed');
  assert.equal(s.taskId, 'task-1');

  // 用户确认重试：`failed` 是终态，重试由新�`intent.start` + continuation 承接原任务，
  //  并把代次提升�2；之后旧代次的快照与事件必须失效�
  s = reduceJourney(s, { type: 'intent.start', requestId: 'req-retry', operationId: 'op-1', continuation: true });
  assert.equal(s.attemptGeneration, 2);
  assert.equal(s.phase, 'in_progress');
  assert.equal(s.message, null);
  assert.equal(s.taskId, 'task-1');

  s = reduceJourney(s, { type: 'task.changed', task: task({ attemptGeneration: 2, state: 'running' }) });
  assert.equal(s.attemptGeneration, 2);
  assert.equal(s.phase, 'in_progress');

  const staleTask = reduceJourney(s, { type: 'task.changed', task: task({ attemptGeneration: 1, state: 'failed', error: { code: 'download.hash_mismatch', detailRef: 'ref-old' } }) });
  assert.equal(staleTask, s, 'a snapshot from the previous attempt must not change the journey');
  assert.equal(staleTask.phase, 'in_progress');
  assert.equal(staleTask.message, null);

  const staleEvent = reduceJourney(s, {
    type: 'business.event',
    envelope: envelope({ attemptGeneration: 1, payload: { approvalState: 'denied' }, sequence: 999, eventId: 'ev-oldgen' }),
  });
  assert.equal(staleEvent.phase, 'in_progress');
  assert.equal(staleEvent.message, null, 'a stale-generation event must not fail the new attempt');
  assert.equal(lastLog(staleEvent).ignored, 'stale_generation');
});

test('stale revision and foreign operation/entity events are ignored', () => {
  resetSeq();
  let s = membership(withTask(), { approvalState: 'requested' }, { revision: 7 });
  assert.equal(s.revision, 7);

  //  幂等拒绝（旧 revision / 外来 operation / 外来实体 / 其它 task）不改变引用�
  const staleRev = membership(s, { approvalState: 'denied' }, { revision: 3, eventId: 'ev-rev3' });
  assert.equal(staleRev, s, 'stale revision must not change the journey');
  assert.equal(staleRev.phase, 'waiting');

  const foreignOp = membership(s, { approvalState: 'denied' }, { operationId: 'op-99', eventId: 'ev-op99' });
  assert.equal(foreignOp, s, 'an event from another operation must not change the journey');
  assert.equal(foreignOp.phase, 'waiting');

  const foreignEntity = membership(s, { approvalState: 'denied' }, { entityId: 'room-2', eventId: 'ev-room2', sequence: 100 });
  assert.equal(foreignEntity, s, 'an event for another entity must not change the journey');
  assert.equal(foreignEntity.phase, 'waiting');

  const foreignTask = reduceJourney(s, { type: 'task.changed', task: task({ id: 'task-9', attemptGeneration: 1, state: 'succeeded' }) });
  assert.equal(foreignTask, s, 'a task snapshot for another task must not change the journey');
  assert.equal(foreignTask.phase, 'waiting');
});

// ---------------------------------------------------------------------------
test('login: expiry returned to original task keeps return point; old callback rejected later', () => {
  resetSeq();
  const base = createJourney({
    kind: 'login',
    journeyId: 'j-launch-login',
    entityId: 'inst-1',
    revision: 3,
    returnPoint: { route: 'start', entityId: 'inst-1', revision: 3 },
  });
  let s = reduceJourney(base, { type: 'intent.start', requestId: 'req-auth', operationId: 'op-auth' });
  s = reduceJourney(s, { type: 'task.changed', task: task({ id: 'task-auth', operationId: 'op-auth', state: 'succeeded' }) });
  assert.equal(s.phase, 'completed');

  // New login journey for the same return point, because the previous one completed.
  const expired = createJourney({ kind: 'login', journeyId: 'j-launch-login-2', entityId: 'inst-1', revision: 4, returnPoint: { route: 'start', entityId: 'inst-1', revision: 3 } });
  let t = reduceJourney(expired, { type: 'intent.start', requestId: 'req-auth-2', operationId: 'op-auth-2' });
  t = reduceJourney(t, { type: 'auth.callback', attemptId: 'att-1', result: 'completed', providerId: 'microsoft', profileIds: ['p-1'] });
  assert.equal(t.phase, 'need_choice');
  assert.equal(t.step, 'profile');
  assert.deepEqual(t.returnPoint, { route: 'start', entityId: 'inst-1', revision: 3 });

  t = reduceJourney(t, { type: 'choice.select', requestId: 'req-pick', kind: 'profile', valueId: 'p-1' });
  assert.equal(t.pendingChoice.valueId, 'p-1');
  const back = reduceJourney(t, { type: 'returnToTask', at: 10 });
  assert.equal(back.returnPoint, null);
  assert.equal(back.phase, 'in_progress');

  // Duplicate browser callback must not create a second choice/operation.
  const dupCb = reduceJourney(back, { type: 'auth.callback', attemptId: 'att-1', result: 'completed', providerId: 'microsoft', profileIds: ['p-1'] });
  assert.equal(dupCb, back);
});

test('authorization cancelled keeps existing account, target and return point', () => {
  resetSeq();
  const j = createJourney({ kind: 'login', journeyId: 'j-cancel', returnPoint: { route: 'downloads', entityId: 'inst-1' } });
  let s = reduceJourney(j, { type: 'intent.start', requestId: 'r1', operationId: 'op-a' });
  s = reduceJourney(s, { type: 'auth.callback', attemptId: 'att-x', result: 'cancelled' });
  assert.deepEqual(s.returnPoint, { route: 'downloads', entityId: 'inst-1' });
  assert.equal(s.message.code, 'account.auth.cancelled');
  const replay = reduceJourney(s, { type: 'auth.callback', attemptId: 'att-x', result: 'completed', profileIds: [] });
  assert.equal(replay, s, 'consumed attempt cannot be replayed as completion');
});

test('return to task is blocked until a profile is chosen', () => {
  resetSeq();
  let s = createJourney({ kind: 'login', journeyId: 'j-r', returnPoint: { route: 'join_room' } });
  s = reduceJourney(s, { type: 'intent.start', requestId: 'r1', operationId: 'op-a' });
  s = reduceJourney(s, { type: 'auth.callback', attemptId: 'att', result: 'completed', profileIds: ['p1', 'p2'] });
  const blocked = reduceJourney(s, { type: 'returnToTask' });
  assert.equal(blocked.returnPoint.route, 'join_room');
  assert.equal(lastLog(blocked).ignored, 'choice_pending');
});

test('wrong-kind or missing choice is ignored; duplicate choose request is ignored', () => {
  resetSeq();
  let s = createJourney({ kind: 'login', journeyId: 'j-c', returnPoint: { route: 'x' } });
  s = reduceJourney(s, { type: 'intent.start', requestId: 'r1', operationId: 'op-a' });
  s = reduceJourney(s, { type: 'auth.callback', attemptId: 'att', result: 'completed', profileIds: ['p1'] });
  const wrong = reduceJourney(s, { type: 'choice.select', requestId: 'c1', kind: 'instance', valueId: 'i1' });
  assert.equal(lastLog(wrong).ignored, 'wrong_choice_kind');
  const ok = reduceJourney(s, { type: 'choice.select', requestId: 'c2', kind: 'profile', valueId: 'p1' });
  const dup = reduceJourney(ok, { type: 'choice.select', requestId: 'c2', kind: 'profile', valueId: 'p2' });
  assert.equal(dup, ok);
  assert.equal(dup.pendingChoice.valueId, 'p1');
});

// ---------------------------------------------------------------------------
test('restoreJourney round-trips a snapshot, rejects bad shapes, never resubmits', () => {
  resetSeq();
  const s = reduceJourney(withTask(), { type: 'task.changed', task: task({ state: 'running' }) });
  const snapshot = JSON.parse(JSON.stringify(s));
  const restored = restoreJourney(snapshot);
  assert.ok(restored);
  assert.equal(restored.operationId, 'op-1');
  assert.equal(restored.taskId, 'task-1');
  assert.ok(Object.isFrozen(restored));
  assert.equal(restoreJourney({ schemaVersion: 'x' }), null);
  assert.equal(restoreJourney({ schemaVersion: JOURNEY_SCHEMA_VERSION, journeyId: 'j', kind: 'bad' }), null);

  // Reconcile with a newer core task snapshot wins over local progress.
  const reconciled = reconcileWithTask(restored, task({ state: 'succeeded', attemptGeneration: 1 }));
  assert.equal(reconciled.phase, 'completed');
});

test('waiting_user maps to need_choice and failed carries error code and detail ref only', () => {
  resetSeq();
  let s = withTask();
  s = reduceJourney(s, { type: 'task.changed', task: task({ state: 'waiting_user' }) });
  assert.equal(s.phase, 'need_choice');
  s = reduceJourney(s, { type: 'task.changed', task: task({ state: 'failed', error: { code: 'download.no_space', detailRef: 'diag://t/1', message: 'raw io error' } }) });
  assert.equal(s.phase, 'failed');
  assert.deepEqual(s.message, { code: 'download.no_space', detailRef: 'diag://t/1' });
});

test('journey log is bounded', () => {
  resetSeq();
  let s = withTask();
  for (let i = 0; i < 100; i += 1) {
    s = membership(s, { approvalState: 'requested' }, { sequence: 1000 + i, eventId: `ev-b-${i}`, revision: 5 + i });
  }
  assert.ok(journeyLog(s).length <= 40);
});
