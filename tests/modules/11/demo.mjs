// Module 11 · 可复现的本地走查脚本（无网络、无 UI）。打印剧本与脚本内断言结果，并统计覆盖。
// 运行：node tests/modules/11/demo.mjs   （失败时退出码 1）
import assert from 'node:assert/strict';

import {
  createJourney,
  reduceJourney,
  journeyView,
  journeyLog,
} from '../../../apps/desktop/src/ux/journeys/model.mjs';
import { availabilityFor } from '../../../apps/desktop/src/ux/journeys/availability.mjs';
import { formatMessage, messageAllowed } from '../../../apps/desktop/src/ux/messages/catalog.mjs';

let checks = 0;
function check(label, fn) {
  fn();
  checks += 1;
  console.log(`ok ${checks} - ${label}`);
}

function view(label, s) {
  const v = journeyView(s);
  const msg = v.message ? `${v.message.code}` : '(none)';
  console.log(`  [${label}] phase=${v.phase} step=${v.step} gen=${v.attemptGeneration} msg=${msg}`);
  return v;
}

function task(state, extra = {}) {
  return { id: 'task-1', operationId: 'op-1', attemptGeneration: 1, state, phase: 'download', cancel_requested: false, error: null, ...extra };
}

function env(id, seq, payload, extra = {}) {
  return { eventId: id, schema_version: 'event.v1', operationId: 'op-1', entityId: 'room-1', revision: 5, sequence: seq, attemptGeneration: 1, kind: 'membership.changed', payload, ...extra };
}

console.log('## 剧本 A：加入房��等待审批 �取消 �审批竞��服务端确认离开');
let a = createJourney({ kind: 'join_room', journeyId: 'demo-room', entityId: 'room-1', revision: 5, returnPoint: { route: 'room:room-1' } });
a = reduceJourney(a, { type: 'intent.start', requestId: 'join-1', operationId: 'op-1' });
a = reduceJourney(a, { type: 'operation.accepted', operationId: 'op-1', taskId: 'task-1' });
a = reduceJourney(a, { type: 'business.event', envelope: env('ev-1', 1, { approvalState: 'requested' }) });
check('等待房主同意显示�waiting + approval', () => {
  const v = view('waiting approval', a);
  assert.equal(v.phase, 'waiting');
  assert.equal(v.pendingChoice.kind, 'approval');
  assert.equal(formatMessage('room.approval.waiting').title, '等待房主同意');
});
a = reduceJourney(a, { type: 'cancel.request', requestId: 'leave-1' });
check('取消加入后为 cancelling，不能宣称已取消', () => {
  const v = view('cancel requested', a);
  assert.equal(v.phase, 'cancelling');
  assert.equal(v.cancelPending, true);
});
a = reduceJourney(a, { type: 'business.event', envelope: env('ev-2', 2, { approvalState: 'approved' }) });
check('迟到审批不复活已取消的申�', () => {
  assert.equal(view('late approve (ignored for phase)', a).phase, 'cancelling');
});
a = reduceJourney(a, { type: 'business.event', envelope: env('ev-3', 3, { approvalState: 'left' }) });
check('服务端确认离开后才进入 cancelled', () => {
  const v = view('left confirmed', a);
  assert.equal(v.phase, 'cancelled');
  assert.equal(v.cancelPending, false);
  //  晚到�approval 被抑制（cancelling 期间），必须在诊断日志中留下 cancel_pending 记录�
  assert.equal(JSON.stringify(journeyLog(a).slice(-3)).includes('cancel_pending'), true);
});

console.log('## 剧本 B：下载失��重试（新代次）→ 旧代次失败事件被忽略');
let b = createJourney({ kind: 'install_content', journeyId: 'demo-dl', entityId: 'inst-1', revision: 2 });
b = reduceJourney(b, { type: 'intent.start', requestId: 'dl-1', operationId: 'op-1' });
b = reduceJourney(b, { type: 'task.changed', task: task('failed', { error: { code: 'download.no_space', detailRef: 'diag://1' } }) });
check('空间不足失败携带稳定 code 与详情引�', () => {
  const v = view('download failed', b);
  assert.equal(v.phase, 'failed');
  const m = formatMessage(v.message.code, { size_text: '2.4 GB', location_name: 'D:\\Games' });
  assert.equal(m.title, 'D:\\Games 空间不足');
  assert.equal(m.actions[0].label, '更改位置');
});
// failed 是终态：用户确认重试时由应用层发�continuation，承接原任务并提升代次�b = reduceJourney(b, { type: 'intent.start', requestId: 'retry-1', operationId: 'op-1', continuation: true });
b = reduceJourney(b, { type: 'task.changed', task: task('running', { attemptGeneration: 2 }) });
const afterRetry = b;
b = reduceJourney(b, { type: 'task.changed', task: task('failed', { attemptGeneration: 1 }) });
check('旧代�gen=1)失败快照不覆�gen=2 运行中状�', () => {
  assert.strictEqual(b, afterRetry);
  view('after stale event', b);
});

console.log('## 剧本 C：下载中断证据门�');
check('“已保留进度”只在核心给�resumable_ranges 时允�', () => {
  assert.equal(messageAllowed('download.interrupted', []), false);
  assert.equal(messageAllowed('download.interrupted', ['resumable_ranges']), true);
  const safe = messageAllowed('download.interrupted', ['resumable_ranges'])
    ? formatMessage('download.interrupted')
    : formatMessage('download.interrupted.no_progress');
  assert.equal(safe.title, '下载中断');
});

console.log('## 剧本 D：首个实例路径动作可用�');
check('首次使用时“安装游�导入游戏”可用，并在执行前标记需重查', () => {
  const s = availabilityFor(formatMessage('launch.first_use.empty').actions, { revision: 0 });
  assert.deepEqual(s.enabled.sort(), ['instance.import_game', 'launch.install_game']);
  assert.equal(s.requiresRecheckAtExecution, true);
});

console.log(`\n${checks} checks passed`);
