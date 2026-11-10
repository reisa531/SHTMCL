// Module 11 · 文案内容与动作映射测试（node:test）
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import {
  MESSAGE_CODES,
  formatMessage,
  messageAllowed,
  messageIndex,
  PRIMARY_VERBS,
} from '../../../apps/desktop/src/ux/messages/catalog.mjs';
import { CAPABILITY_IDS } from '../../../apps/desktop/src/ux/journeys/availability.mjs';

// Required scenario coverage (product design §8.2 + task brief).
const REQUIRED_CODES = [
  'launch.process.started',
  'launch.first_use.empty',
  'account.expired',
  'account.auth.cancelled',
  'account.platform.ok_profile.missing',
  'launch.java.mismatch',
  'download.interrupted',
  'download.rate_limited',
  'download.no_space',
  'content.mod_conflict',
  'room.approval.waiting',
  'room.connected.direct',
  'room.connected.relay',
  'room.disconnected',
  'server.added',
  'server.added.start',
  'server.waiting_game_exit',
  'server.list_corrupt',
];

// Rhetoric / disclaimer sentences that must never reach product copy (§8.2).
const FORBIDDEN = /仅供演示|不保证|请知悉|为了你的安全|不会进行任何操作|安全免责|智能修复|极速增强|无忧保障|AI\s|人工智能|演示|免责|祝你好运/;
/** Tasks rely on evidence tokens such as `cancel_requested`; only user-visible text is checked. */

const EVIDENCE_PHRASES = /已保留|已完成|已添加|已取消加入|已通过中继/;

function allRendered() {
  return MESSAGE_CODES.map((code) => formatMessage(code, { instance_name: '山间小屋', server_name: '山间小屋', file_name: 'fabric.jar', source_name: '示例来源', mod_a: 'A', mod_b: 'B', size_text: '2.4 GB', location_name: 'D:\\Games', java_major: '21', java_current: '17', provider_name: '微软', profile_name: 'Steve', owner_name: 'Alex', game_version: '1.21.4', loader: 'Fabric', instance_version: '1.20.1', changed_count: '12', add_count: '10', remove_count: '2', conflict_summary: '两者要求不同的加载器' }));
}

test('every required scenario has copy', () => {
  for (const code of REQUIRED_CODES) assert.ok(MESSAGE_CODES.includes(code), `missing ${code}`);
});

test('no forbidden disclaimer / rhetoric phrases; no placeholder left unrendered', () => {
  for (const m of allRendered()) {
    const text = `${m.title} ${m.body ?? ''} ${m.actions.map((a) => a.label).join(' ')}`;
    assert.doesNotMatch(text, FORBIDDEN, `rhetoric in ${m.code}: ${text}`);
    assert.doesNotMatch(text, /\{[a-z_]+\}/, `unrendered placeholder in ${m.code} (${text})`);
  }
});

test('completed / retained claims require core evidence', () => {
  for (const m of allRendered()) {
    if (EVIDENCE_PHRASES.test(`${m.title} ${m.body ?? ''}`)) {
      assert.ok(m.evidence.length > 0, `${m.code} claims completion without evidence markers`);
      assert.equal(messageAllowed(m.code, []), false, `${m.code} must be blocked without evidence`);
      assert.equal(messageAllowed(m.code, m.evidence), true);
    }
  }
  const interrupted = formatMessage('download.interrupted');
  assert.match(interrupted.body, /已保留进度/);
  assert.equal(messageAllowed('download.interrupted', undefined), false);
  assert.equal(messageAllowed('download.interrupted', ['resumable_ranges']), true);
  assert.equal(messageAllowed('download.interrupted.no_progress', undefined), true);
  assert.doesNotMatch(formatMessage('download.interrupted.no_progress').body, /已保留/);

  assert.equal(messageAllowed('server.list_corrupt', []), false);
  assert.equal(messageAllowed('server.list_corrupt', ['original_file_preserved']), true);
  assert.equal(messageAllowed('server.added', ['binding_committed']), true);
  assert.equal(messageAllowed('room.connected.relay', ['connection_connected', 'transport_relay']), true);
});

test('download interruption and rate limit keep distinct wording', () => {
  const a = formatMessage('download.interrupted');
  const b = formatMessage('download.rate_limited', { source_name: 'Mojang' });
  assert.equal(a.title, '下载中断');
  assert.equal(b.title, '下载来源繁忙');
  assert.deepEqual(b.actions.map((x) => x.label), ['切换来源', '暂停']);
});

test('mod conflict names both mods and offers an adjustment, does not claim root cause certainty', () => {
  const m = formatMessage('content.mod_conflict', { mod_a: 'Create', mod_b: 'Create Addons' });
  assert.equal(m.title, 'Create 与 Create Addons 不能同时启用');
  assert.equal(m.actions[0].label, '查看冲突');
  assert.equal(m.actions[0].kind, 'detail');
  assert.equal(m.codeKnown, true);
});

test('room messages: approval waits, relay is a separate confirmation, disconnect does not promise restore', () => {
  const wait = formatMessage('room.approval.waiting', { owner_name: 'Alex' });
  assert.equal(wait.title, '等待房主同意');
  assert.equal(wait.actions[0].label, '取消加入');
  assert.equal(formatMessage('room.connecting').title, '正在连接');
  assert.notEqual(formatMessage('room.connecting').title, wait.title);

  const relay = formatMessage('room.connected.relay');
  assert.equal(relay.title, '已通过中继连接');
  const direct = formatMessage('room.connected.direct');
  assert.equal(direct.title, '正在直连');
  assert.notEqual(direct.title, relay.title);

  const disc = formatMessage('room.disconnected');
  assert.equal(disc.title, '连接已断开');
  assert.doesNotMatch(`${disc.title}${disc.body}`, /已自动重连|已恢复连接/);
  assert.equal(disc.actions[0].label, '重新加入');
});

test('server add and running-instance queue are spelled out with the real object', () => {
  const added = formatMessage('server.added', { server_name: '山间小屋' });
  assert.equal(added.title, '已添加 山间小屋');
  assert.equal(added.actions[0].label, '启动游戏');
  const queued = formatMessage('server.waiting_game_exit', { instance_name: '小屋' });
  assert.equal(queued.title, '退出游戏后添加');
  assert.equal(formatMessage('server.added.start').actions[0].label, '启动游戏');
});

test('unknown code falls back to observable fact and log, not raw exception text', () => {
  const m = formatMessage('core.internal.panic', { message: 'thread panicked at src/x.rs' });
  assert.equal(m.codeKnown, false);
  assert.equal(m.title, '操作没有完成');
  assert.doesNotMatch(JSON.stringify(m), /panicked/);
  assert.equal(m.actions[0].capability, 'diagnostics.open_log');
});

test('message actions reference known capabilities; primary verbs are concrete', () => {
  const declaredActions = messageIndex().flatMap((m) => m.actions);
  for (const a of declaredActions) {
    assert.ok(CAPABILITY_IDS.includes(a.capability), `unknown capability ${a.capability} (${a.label})`);
  }
  const labels = allRendered().flatMap((m) => m.actions.map((a) => a.label));
  for (const label of labels) assert.doesNotMatch(label, /演示|稍后|^\s*确定\s*$|^OK$/);
  const fixed = labels.filter((l) => PRIMARY_VERBS.includes(l));
  assert.ok(fixed.length >= 6, 'uses the fixed primary action vocabulary');
  assert.ok(labels.includes('查看日志'));
  assert.ok(labels.includes('继续下载'));
  assert.ok(labels.includes('重新登录'));
});

test('maxActions and allowedCapabilities filtering', () => {
  const m = formatMessage('download.no_space', {}, { maxActions: 1 });
  assert.equal(m.actions.length, 1);
  const only = formatMessage('download.no_space', {}, { allowedCapabilities: ['cache.clean'] });
  assert.deepEqual(only.actions.map((a) => a.capability), ['cache.clean']);
});

test('content module has no network/IO side effects', () => {
  const src = readFileSync(fileURLToPath(new URL('../../../apps/desktop/src/ux/messages/catalog.mjs', import.meta.url)), 'utf8');
  assert.doesNotMatch(src, /\bfetch\s*\(|XMLHttpRequest|node:fs|require\(|process\.env|localStorage/);
});

test('java mismatch names the required version', () => {
  const m = formatMessage('launch.java.mismatch', { java_major: '21', java_current: '17' });
  assert.equal(m.title, 'Java 版本不匹配');
  assert.match(m.body, /17/);
  assert.equal(m.actions[0].label, '安装 Java 21');
});
