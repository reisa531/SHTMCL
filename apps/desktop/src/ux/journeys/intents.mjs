/**
 * Module 11 · 用途意图 → 桥接命令（纯映射，不执行）
 * ---------------------------------------------------------------------------
 * 00 契约 §3：UI `onIntent` 把 UX 的 start/resume/cancel/choose/returnToTask 展示动作
 * 映射为桥接命令；请求 ID 对应 `request_id`，所属操作对应 `operation_id`。
 * 桥接层用 request_id 作为 Idempotency-Key 去重，因此这里不生成第二个请求。
 *
 * 本模块只构造请求，不调用桥（那是 agent 与 Tauri 层）；结果是否被接受由 accepted 事件决定。
 * 所有 payload 都经过白名单过滤：凭据、token、完整文件路径与原始回调内容不进入 UI。
 */

export const INTENT_KINDS = Object.freeze(['start', 'resume', 'cancel', 'choose', 'returnToTask']);

const CANCEL_COMMAND = 'RequestCancel';
function isStr(s) {
  return typeof s === 'string' && s.length > 0;
}

function assertIntent(base) {
  if (!isStr(base.requestId)) throw new Error('requestId is required');
  if (!isStr(base.operationId)) throw new Error('operationId is required');
  if (!INTENT_KINDS.includes(base.kind)) throw new Error(`unknown intent kind: ${base.kind}`);
}

/**
 * start：新操作，由核心分配 operationId；缺省时以 requestId 兜底。
 * @param {{requestId:string, command:string, operationId?:(string|null), entityId?:(string|null), expectedRevision?:(number|null), payload:Object}} input
 */
export function startIntent(input) {
  const base = {
    kind: 'start',
    requestId: input.requestId,
    operationId: input.operationId ?? input.requestId,
  };
  assertIntent(base);
  if (!isStr(input.command)) throw new Error('command is required');
  return freeze({
    ...base,
    bridge: {
      command: input.command,
      request_id: base.requestId,
      operation_id: base.operationId,
      entity_id: input.entityId ?? null,
      expected_revision: Number.isInteger(input.expectedRevision) ? input.expectedRevision : null,
      payload: sanitizePayload(input.payload ?? {}),
    },
  });
}

/**
 * resume：用户确认重试/继续，用 `task_id + attempt_generation` 定位要接续的尝试。
 *
 * 未列入白名单的字段直接报错，而不是静默丢弃：静默丢弃会让调用方以为附加参数生效，
 * 例如 `payload: { unrelated: 1 }` —— 用户什么都没收到，凭证类字段同样必须被拒。
 */
export function resumeIntent(input) {
  const base = { kind: 'resume', requestId: input.requestId, operationId: input.operationId };
  assertIntent(base);
  if (!isStr(input.taskId)) throw new Error('taskId is required');
  if (!Number.isInteger(input.attemptGeneration)) throw new Error('attemptGeneration must be an integer');
  const extra = input.payload ?? {};
  for (const key of Object.keys(extra)) {
    if (!ALLOWED_PAYLOAD_KEYS.includes(key) && !EXTRA_ALLOWED.has(key)) {
      throw new Error(`payload key not allowed: ${key}`);
    }
  }
  return freeze({
    ...base,
    bridge: {
      command: input.command ?? 'task.resume',
      request_id: base.requestId,
      operation_id: base.operationId,
      payload: sanitizePayload({
        task_id: input.taskId,
        attempt_generation: input.attemptGeneration,
        ...extra,
      }),
    },
  });
}

/** cancel：映射为核心 RequestCancel(task_id, attempt_generation)。 */
export function cancelIntent(input) {
  const base = { kind: 'cancel', requestId: input.requestId, operationId: input.operationId };
  assertIntent(base);
  if (!isStr(input.taskId)) throw new Error('taskId is required');
  if (!Number.isInteger(input.attemptGeneration)) throw new Error('attemptGeneration must be an integer');
  return freeze({
    ...base,
    bridge: {
      command: CANCEL_COMMAND,
      request_id: base.requestId,
      operation_id: base.operationId,
      payload: { task_id: input.taskId, attempt_generation: input.attemptGeneration },
    },
  });
}

/** choose：提交用户选择（角色/实例/版本/冲突处理等）。 */
export function chooseIntent(input) {
  const base = { kind: 'choose', requestId: input.requestId, operationId: input.operationId };
  assertIntent(base);
  if (!isStr(input.choiceKind)) throw new Error('choiceKind is required');
  if (input.valueId !== undefined && !isStr(input.valueId)) throw new Error('valueId must be a non-empty string');
  const extra = input.payload ?? {};
  for (const key of Object.keys(extra)) {
    if (!ALLOWED_PAYLOAD_KEYS.includes(key) && !EXTRA_ALLOWED.has(key)) {
      throw new Error(`payload key not allowed: ${key}`);
    }
  }
  return freeze({
    ...base,
    bridge: {
      command: input.command ?? `${input.choiceKind}.choose`,
      request_id: base.requestId,
      operation_id: base.operationId,
      payload: sanitizePayload({
        choice_kind: input.choiceKind,
        value_id: input.valueId ?? null,
        expected_revision: Number.isInteger(input.expectedRevision) ? input.expectedRevision : null,
        ...(input.payload ?? {}),
      }),
    },
  });
}

/**
 * returnToTask：授权完成后回到原任务。原任务仍是活跃的，就发 `refetch` 重新读取快照；
 * 否则只做 `navigate`，不重新提交写命令。原任务已取消则 `stay`，UI 停在当前页面。
 */
export function returnToTaskIntent(input) {
  const base = { kind: 'returnToTask', requestId: input.requestId, operationId: input.operationId };
  assertIntent(base);
  const journey = input.journey;
  const returnPoint = input.returnPoint ?? journey?.returnPoint ?? null;
  if (!returnPoint) {
    return freeze({ ...base, action: 'stay', bridge: null, reason: 'no_return_point' });
  }
  const taskActive = journey && journey.taskId && !['completed', 'cancelled', 'failed'].includes(journey.phase);
  return freeze({
    ...base,
    action: taskActive ? 'refetch' : 'navigate',
    route: returnPoint.route ?? null,
    bridge: taskActive
      ? { command: 'task.snapshot', request_id: base.requestId, payload: { task_id: journey.taskId } }
      : {
          command: 'ui.navigate',
          request_id: base.requestId,
          payload: { route: returnPoint.route ?? null, entity_id: returnPoint.entityId ?? null },
        },
    // 返回点只恢复展示，不重新提交动作。
    resubmits: false,
  });
}

/** 只接受信誉已知的字段，其余一律丢弃，防止凭据、路径、回调原文进入请求。 */
export const ALLOWED_PAYLOAD_KEYS = Object.freeze([
  'instance_id',
  'profile_id',
  'identity_id',
  'game_version',
  'loader',
  'java_path',
  'java_major',
  'version_id',
  'room_id',
  'membership_id',
  'invite_id',
  'listing_id',
  'server_name',
  'install_location',
  'size_bytes',
  'source_id',
  'artifact_key',
  'local_file',
  'choice_id',
  'conflict_resolution',
  'plan_id',
  'task_id',
  'attempt_generation',
  'visibility',
  'join_policy',
  'port',
  'content_version',
]);

const EXTRA_ALLOWED = new Set(['task_id', 'attempt_generation', 'choice_kind', 'value_id', 'expected_revision']);

function sanitizePayload(payload) {
  const out = {};
  for (const k of Object.keys(payload)) {
    if (ALLOWED_PAYLOAD_KEYS.includes(k) || EXTRA_ALLOWED.has(k)) out[k] = payload[k];
  }
  return out;
}

function freeze(o) {
  return Object.freeze(o);
}
