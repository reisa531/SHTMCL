// Module 11 · ActionAvailability 投影与意图映射测试
import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  availabilityFor,
  projectActions,
  staleAvailability,
  CAPABILITY_IDS,
  CONDITIONS,
} from '../../../apps/desktop/src/ux/journeys/availability.mjs';
import {
  startIntent,
  resumeIntent,
  cancelIntent,
  chooseIntent,
  returnToTaskIntent,
  ALLOWED_PAYLOAD_KEYS,
} from '../../../apps/desktop/src/ux/journeys/intents.mjs';
import { formatMessage } from '../../../apps/desktop/src/ux/messages/catalog.mjs';
import { createJourney, reduceJourney } from '../../../apps/desktop/src/ux/journeys/model.mjs';

const start = formatMessage('launch.first_use.empty').actions;

test('availability exposes capability + reason + next step, never claims permission by itself', () => {
  const projected = projectActions(start, { revision: 4, conditions: { 'launch.start': 'game_running' } });
  const install = projected.find((a) => a.capability === 'launch.install_game');
  assert.equal(install.available, true);
  assert.equal(install.state, 'available');
  assert.equal(install.permission, 'instance.create');
  assert.equal(install.basedOnRevision, 4);

  const blocked = projectActions([{ label: '启动游戏', capability: 'launch.start' }], { conditions: { 'launch.start': 'game_running' } })[0];
  assert.equal(blocked.available, false);
  assert.equal(blocked.reason, CONDITIONS.game_running.reason);
  assert.equal(blocked.nextStep, 'launch.stop_game');
  assert.equal(blocked.condition, 'game_running');

  const summary = availabilityFor(start, { revision: 4 });
  assert.equal(summary.requiresRecheckAtExecution, true);
  assert.deepEqual(summary.enabled.sort(), ['instance.import_game', 'launch.install_game']);
});

test('missing server-side permission disables with a reason and next step', () => {
  const [a] = projectActions([{ label: '启动游戏', capability: 'launch.start' }], { grantedPermissions: ['instance.manage'] });
  assert.equal(a.available, false);
  assert.equal(a.condition, 'no_permission');
  assert.equal(a.nextStep, 'instance.open_settings');
  assert.ok(CAPABILITY_IDS.includes(a.nextStep));

  const [ok] = projectActions([{ label: '启动游戏', capability: 'launch.start' }], { grantedPermissions: ['instance.launch'] });
  assert.equal(ok.available, true);
});

test('unknown capability is disabled; in-flight request prevents duplicate dispatch', () => {
  const [u] = projectActions([{ label: '某动作', capability: 'nope.do' }]);
  assert.equal(u.available, false);
  assert.match(u.reason, /尚未接入/);

  const [p] = projectActions([{ label: '开始', capability: 'account.begin_authorization' }], {
    inProgressRequestId: 'req-1',
    currentRequest: { 'account.begin_authorization': 'req-1' },
  });
  assert.equal(p.state, 'in_progress');
  assert.equal(p.available, false);
});

test('stale revision downgrades only write capabilities', () => {
  const actions = [
    { label: '添加并启�', capability: 'server.cancel_add' },
    { label: '连接详情', capability: 'room.open_connection_detail' },
  ];
  const p = projectActions(actions, { revision: 2 });
  const s = staleAvailability(p, 3);
  assert.equal(s[0].available, false);
  assert.equal(s[0].nextStep, 'operation.replan');
  assert.equal(s[1].available, true, 'read-only action stays available');
});

test('start intent maps request_id/operation_id/entity/revision and strips secrets', () => {
  const intent = startIntent({
    requestId: 'req-1',
    command: 'instance.create',
    entityId: 'inst-1',
    expectedRevision: 3,
    payload: { version_id: '1.21.4', loader: 'fabric', token: 'secret', callback: 'code=abc', java_path: 'C:\\Java\\bin\\java.exe' },
  });
  assert.equal(intent.kind, 'start');
  assert.deepEqual(intent.bridge, {
    command: 'instance.create',
    request_id: 'req-1',
    operation_id: 'req-1',
    entity_id: 'inst-1',
    expected_revision: 3,
    payload: { version_id: '1.21.4', loader: 'fabric', java_path: 'C:\\Java\\bin\\java.exe' },
  });
  const body = JSON.stringify(intent);
  assert.doesNotMatch(body, /secret|code=abc/);
  for (const k of Object.keys(intent.bridge.payload)) assert.ok(ALLOWED_PAYLOAD_KEYS.includes(k));
});

test('cancel intent is the shared RequestCancel with task + attempt generation only', () => {
  const c = cancelIntent({ requestId: 'req-c', operationId: 'op-1', taskId: 'task-1', attemptGeneration: 4 });
  assert.equal(c.bridge.command, 'RequestCancel');
  assert.deepEqual(c.bridge.payload, { task_id: 'task-1', attempt_generation: 4 });
  assert.throws(() => cancelIntent({ requestId: 'x', operationId: 'op', taskId: 't', attemptGeneration: '4' }), /integer/);
  assert.throws(() => resumeIntent({ requestId: 'x', operationId: 'op', taskId: 't', attemptGeneration: 1, payload: { unrelated: 1 } }));
});

test('resume and choose intents carry generation and choice kind', () => {
  const r = resumeIntent({ requestId: 'r', operationId: 'op', taskId: 't', attemptGeneration: 2, payload: { source_id: 's1' } });
  assert.deepEqual(r.bridge.payload, { task_id: 't', attempt_generation: 2, source_id: 's1' });
  //  凭据类字段不被静默剥离，而是直接拒绝：调用方必须知道它没有被带上�
  assert.throws(
    () => resumeIntent({ requestId: 'r2', operationId: 'op', taskId: 't', attemptGeneration: 2, payload: { password: 'x' } }),
    /not allowed: password/,
  );
  const ch = chooseIntent({ requestId: 'c', operationId: 'op', choiceKind: 'profile', valueId: 'p-1', expectedRevision: 5 });
  assert.equal(ch.bridge.command, 'profile.choose');
  assert.equal(ch.bridge.payload.value_id, 'p-1');
  assert.throws(() => chooseIntent({ requestId: 'c', operationId: 'op' }), /choiceKind/);
});

test('returnToTask refetches an active task, never resubmits, and stays when task was cancelled', () => {
  let j = createJourney({ kind: 'login', journeyId: 'j', returnPoint: { route: 'join_room', entityId: 'room-1' } });
  j = reduceJourney(j, { type: 'intent.start', requestId: 's', operationId: 'op-1' });
  j = reduceJourney(j, { type: 'operation.accepted', operationId: 'op-1', taskId: 'task-1' });
  const view = { returnPoint: j.returnPoint, taskId: j.taskId, phase: j.phase };
  const r = returnToTaskIntent({ requestId: 'b', operationId: 'op-1', journey: view });
  assert.equal(r.action, 'refetch');
  assert.equal(r.bridge.command, 'task.snapshot');
  assert.equal(r.resubmits, false);

  const stay = returnToTaskIntent({ requestId: 'b2', operationId: 'op-1', journey: { ...view, phase: 'cancelled' } });
  assert.equal(stay.action, 'navigate');
  assert.equal(stay.bridge.command, 'ui.navigate');
  assert.equal(stay.resubmits, false);

  const none = returnToTaskIntent({ requestId: 'b3', operationId: 'op-1', journey: { returnPoint: null } });
  assert.equal(none.action, 'stay');
});
