/**
 * Module 11 · JourneyContext 纯状态转换
 * ---------------------------------------------------------------------------
 * 设计约束（contracts/domain-v1.md §2、§11、§12；docs/development/11-ux-and-service.md §3）：
 * - 旅程只关联核心事实：operationId / taskId / entityId / revision / attemptGeneration / sequence。
 * - 不产生业务成功事实：只有核心任务状态或业务事件能把阶段推进到 completed。
 * - 旧代次（attemptGeneration 更小）、旧 revision、逆序 sequence、重复 eventId 与重复 requestId 一律忽略。
 * - 取消需要确认：请求取消后进入 cancelling，直到核心 `cancelled`（或成员离开被服务端确认）才进入 cancelled。
 * - 授权返回时恢复返回点（returnPoint）；原任务已被核心取消则清除返回点、不复活操作。
 * - 不保存凭据；账户只记录稳定引用（identityId / profileId）。
 *
 * 命名：线上契约一律 camelCase（contracts/domain-v1.md §12），本模块内部同用 camelCase。
 * 纯函数：`createJourney(init)` 与 `reduceJourney(state, event)`。状态对象深度冻结；
 * 被忽略的事件原样返回同一引用，调用方可据此跳过渲染。
 * 每次接受事件追加一条有界日志（≤40 条），记录忽略原因，供测试与诊断断言。
 */

export const JOURNEY_SCHEMA_VERSION = 'journey.v1';

/** 旅程类型 → 默认步骤顺序。步骤是展示阶段标签，不承担业务语义。 */
export const JOURNEY_KINDS = Object.freeze({
  first_use: ['choose_version', 'prepare', 'launch'],
  continue_game: ['launch'],
  install_content: ['choose_instance', 'search', 'preview', 'install'],
  login: ['choose_provider', 'browser_authorize', 'profile'],
  join_room: ['match_requirements', 'prepare', 'approval', 'connect'],
  add_server: ['choose_instance', 'compatibility', 'add', 'launch'],
  recover: ['diagnose', 'fix'],
});

/** 00 公共任务状态（contracts/domain-v1.md §4）。 */
export const TASK_STATES = Object.freeze([
  'queued',
  'running',
  'paused',
  'waiting_user',
  'succeeded',
  'failed',
  'cancelled',
]);

/**
 * 旅程阶段。`waiting`（审批/授权/网络退避）与 `blocked`（连接断开等需用户操作）单列，
 * UI 不得统一写成“处理中”。
 */
export const PHASES = Object.freeze([
  'idle',
  'in_progress',
  'waiting',
  'need_choice',
  'blocked',
  'cancelling',
  'completed',
  'failed',
  'cancelled',
]);

/**
 * 终态：不再接受任何事件。`failed` 也在其中 —— 失败的重试是一条新操作，
 * 不能靠往失败旅程里灌事件把它“救活”，否则旧代次的迟到事件会把失败覆盖成进行中。
 */
const TERMINAL_PHASES = new Set(['completed', 'cancelled', 'failed']);

const LOG_CAP = 40;
const EVENT_ID_CAP = 256;
const REQUEST_ID_CAP = 64;

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

function freeze(obj) {
  if (obj && typeof obj === 'object' && !Object.isFrozen(obj)) {
    Object.freeze(obj);
    for (const k of Object.keys(obj)) freeze(obj[k]);
  }
  return obj;
}

function isInt(n) {
  return typeof n === 'number' && Number.isInteger(n);
}

function nonEmpty(s) {
  return typeof s === 'string' && s.length > 0;
}

function log(state, entry) {
  const list =
    state.log.length >= LOG_CAP
      ? state.log.slice(state.log.length - LOG_CAP + 1)
      : state.log.slice();
  list.push(entry);
  return list;
}

function hasRequest(state, requestId) {
  return state.seenRequestIds.includes(requestId);
}

function withRequest(state, requestId) {
  if (!nonEmpty(requestId) || hasRequest(state, requestId)) {
    return { state, duplicate: nonEmpty(requestId) };
  }
  return {
    state: {
      ...state,
      seenRequestIds: [...state.seenRequestIds, requestId].slice(-REQUEST_ID_CAP),
    },
    duplicate: false,
  };
}

/**
 * 记录“被忽略原因”并返回新状态引用。
 *
 * 只用于调用方需要看到原因的诊断路径（重复点击、重复回调、取消待确认时的抑制）。
 * 幂等路径（重复 eventId、顺序号回退、revision 回退）必须原样返回输入引用，
 * 让 `reduceJourney` 的“未改变则返回同一引用”契约保持成立，UI 才能跳过渲染。
 */
function ignored(state, type, reason, extra = {}) {
  return { ...state, log: log(state, { type, ignored: reason, ...extra }) };
}

/** 把“被忽略原因”写进状态，但不替换可见状态：用于同会话重复点击。 */
function withIgnoreLog(state, entry) {
  return { ...state, log: log(state, entry) };
}

function hasCoreTask(state) {
  return nonEmpty(state.taskId);
}

// ---------------------------------------------------------------------------
// 创建与恢复
// ---------------------------------------------------------------------------

/**
 * @param {Object} init
 * @param {string} init.kind JOURNEY_KINDS 之一
 * @param {string} init.journeyId 应用层生成的稳定 ID（窗口重建复用）
 * @param {string[]} [init.steps]
 * @param {string} [init.entityId] 目标对象（实例/房间/服务器/账户），非凭据
 * @param {number} [init.revision]
 * @param {Object} [init.returnPoint] { route, entityId?, revision?, label? }
 * @param {Object} [init.pendingChoice] 待决选择（如登录后的角色选择）
 * @returns {Readonly<Object>}
 */
export function createJourney(init) {
  if (!init || !nonEmpty(init.journeyId)) throw new Error('journeyId is required');
  if (!Object.prototype.hasOwnProperty.call(JOURNEY_KINDS, init.kind)) {
    throw new Error(`unknown journey kind: ${init.kind}`);
  }
  const steps = init.steps ?? JOURNEY_KINDS[init.kind];
  if (!Array.isArray(steps) || steps.length === 0) {
    throw new Error('steps must be a non-empty array');
  }
  return freeze({
    schemaVersion: JOURNEY_SCHEMA_VERSION,
    journeyId: init.journeyId,
    kind: init.kind,
    steps: [...steps],
    step: steps[0],
    phase: 'idle',
    operationId: null,
    taskId: null,
    entityId: init.entityId ?? null,
    revision: isInt(init.revision) ? init.revision : null,
    // 尚未提交任何尝试：代次为 0，首次 `intent.start` 才提升到 1，
    // 与核心 `Task::attemptGeneration` 从 1 开始的约定对齐。
    attemptGeneration: 0,
    returnPoint: init.returnPoint ?? null,
    pendingChoice: init.pendingChoice ?? null,
    cancel: { requestedAt: null, confirmedAt: null },
    lastSequence: null,
    seenEventIds: [],
    seenRequestIds: [],
    message: null,
    evidence: [],
    log: [],
  });
}

/**
 * 窗口重建：只恢复展示，不重发任何已提交动作。
 * 形状不合法时返回 null，调用方应新建旅程而不是猜测字段。
 */
export function restoreJourney(snapshot) {
  if (!snapshot || typeof snapshot !== 'object') return null;
  if (snapshot.schemaVersion !== JOURNEY_SCHEMA_VERSION) return null;
  if (
    !nonEmpty(snapshot.journeyId) ||
    !Object.prototype.hasOwnProperty.call(JOURNEY_KINDS, snapshot.kind)
  ) {
    return null;
  }
  if (!PHASES.includes(snapshot.phase)) return null;
  return freeze({
    schemaVersion: JOURNEY_SCHEMA_VERSION,
    journeyId: snapshot.journeyId,
    kind: snapshot.kind,
    steps: Array.isArray(snapshot.steps) ? [...snapshot.steps] : [...JOURNEY_KINDS[snapshot.kind]],
    step: nonEmpty(snapshot.step) ? snapshot.step : JOURNEY_KINDS[snapshot.kind][0],
    phase: snapshot.phase,
    operationId: nonEmpty(snapshot.operationId) ? snapshot.operationId : null,
    taskId: nonEmpty(snapshot.taskId) ? snapshot.taskId : null,
    entityId: nonEmpty(snapshot.entityId) ? snapshot.entityId : null,
    revision: isInt(snapshot.revision) ? snapshot.revision : null,
    attemptGeneration:
      isInt(snapshot.attemptGeneration) && snapshot.attemptGeneration > 0
        ? snapshot.attemptGeneration
        : 1,
    returnPoint: snapshot.returnPoint ?? null,
    pendingChoice: snapshot.pendingChoice ?? null,
    cancel: {
      requestedAt: snapshot.cancel?.requestedAt ?? null,
      confirmedAt: snapshot.cancel?.confirmedAt ?? null,
    },
    lastSequence: isInt(snapshot.lastSequence) ? snapshot.lastSequence : null,
    seenEventIds: Array.isArray(snapshot.seenEventIds)
      ? snapshot.seenEventIds.slice(-EVENT_ID_CAP)
      : [],
    seenRequestIds: Array.isArray(snapshot.seenRequestIds)
      ? snapshot.seenRequestIds.slice(-REQUEST_ID_CAP)
      : [],
    message: snapshot.message ?? null,
    evidence: Array.isArray(snapshot.evidence) ? [...snapshot.evidence] : [],
    log: Array.isArray(snapshot.log) ? snapshot.log.slice(-LOG_CAP) : [],
  });
}

// ---------------------------------------------------------------------------
// 事件处理
// ---------------------------------------------------------------------------

function onIntentStart(state, event) {
  // 重复点击：同一 requestId 只允许一次副作用，且不改变状态引用（UI 跳过渲染）。
  if (nonEmpty(event.requestId) && hasRequest(state, event.requestId)) return state;
  // 已绑定核心任务且未到终态：返回**原会话**，不替换 operation/task/代次，也不重复提交。
  if (hasCoreTask(state) && !TERMINAL_PHASES.has(state.phase)) {
    return withIgnoreLog(state, {
      type: 'intent.start',
      ignored: 'operation_active',
      operationId: state.operationId,
      taskId: state.taskId,
    });
  }
  // 尚无核心任务（含“已提交但尚未 accepted”）：重复点击由 operationId 兜底，
  // 同一操作不会生成第二个任务。
  if (!event.continuation && nonEmpty(event.operationId) && state.operationId === event.operationId) {
    return state;
  }
  if (!nonEmpty(event.operationId) && !nonEmpty(event.taskId)) {
    return withIgnoreLog(state, {
      type: 'intent.start',
      ignored: 'missing_operation',
      requestId: event.requestId ?? null,
    });
  }
  const dedup = withRequest(state, event.requestId);
  const s = dedup.state;
  // 失败后重试：内核用 `begin_retry` 提升同一任务的代次，
  // 因此旅程也保留 taskId 并只提升代次，让同一 task 的新快照能继续被接受。
  const continuation = Boolean(event.continuation) && hasCoreTask(s);
  // 新操作 = 新代次。核心任务从 1 开始计数，因此首次提交得到 1，而不是 2。
  const generation = s.attemptGeneration + 1;
  return {
    ...s,
    operationId: event.operationId ?? s.operationId,
    taskId: continuation ? s.taskId : null,
    attemptGeneration: generation,
    lastSequence: null,
    phase: 'in_progress',
    step: event.step ?? s.step,
    message: null,
    evidence: [],
    cancel: { requestedAt: null, confirmedAt: null },
    log: log(s, {
      type: 'intent.start',
      operationId: event.operationId ?? null,
      attemptGeneration: generation,
      continuation,
    }),
  };
}

/**
 * 重试/继续：显式提升尝试代次，旧任务快照与事件随之失效。
 * 只接受带核心任务的旅程：没有任务就没有可继续的尝试。
 */
function onIntentResume(state, event) {
  if (nonEmpty(event.requestId) && hasRequest(state, event.requestId)) return state;
  if (!hasCoreTask(state)) {
    return ignored(state, 'intent.resume', 'missing_task', { requestId: event.requestId ?? null });
  }
  const dedup = withRequest(state, event.requestId);
  const s = {
    ...dedup.state,
    attemptGeneration: dedup.state.attemptGeneration + 1,
    lastSequence: null,
    phase: 'in_progress',
    message: null,
    evidence: [],
    cancel: { requestedAt: null, confirmedAt: null },
    returnPoint: event.returnPoint ?? dedup.state.returnPoint,
  };
  return {
    ...s,
    log: log(s, {
      type: 'intent.resume',
      taskId: s.taskId,
      attemptGeneration: s.attemptGeneration,
    }),
  };
}

function onOperationAccepted(state, event) {
  if (event.operationId && state.operationId && event.operationId !== state.operationId) {
    return ignored(state, 'operation.accepted', 'foreign_operation');
  }
  if (isInt(event.revision) && isInt(state.revision) && event.revision < state.revision) {
    return ignored(state, 'operation.accepted', 'stale_revision');
  }
  const s = {
    ...state,
    operationId: event.operationId ?? state.operationId,
    taskId: event.taskId ?? state.taskId,
    entityId: event.entityId ?? state.entityId,
    revision: isInt(event.revision) ? event.revision : state.revision,
  };
  return { ...s, log: log(s, { type: 'operation.accepted', taskId: event.taskId ?? null }) };
}

/**
 * 核心任务快照。只接受本操作/本任务/本代次的快照；其余原样返回输入引用。
 *
 * `attemptGeneration` 只在**同一任务**内可比：核心每个任务从 1 计数，
 * 所以不同 task 之间的代次高低没有可比性（误用为全局计数器会让新任务被旧任务永久压住）。
 */
function onTaskChanged(state, event) {
  const task = event.task;
  if (
    !task ||
    !nonEmpty(task.id) ||
    !nonEmpty(task.operationId) ||
    !TASK_STATES.includes(task.state)
  ) {
    return state;
  }
  if (!isInt(task.attemptGeneration) || task.attemptGeneration < 1) return state;
  if (state.operationId && task.operationId !== state.operationId) return state;
  if (hasCoreTask(state)) {
    if (state.taskId !== task.id) return state;
    if (task.attemptGeneration < state.attemptGeneration) return state;
    if (task.attemptGeneration > state.attemptGeneration) {
      return ignored(state, 'task.changed', 'unexpected_generation', {
        taskId: task.id,
        got: task.attemptGeneration,
        current: state.attemptGeneration,
      });
    }
  }
  const evidence = Array.isArray(task.evidence) ? task.evidence : [];
  let s = {
    ...state,
    taskId: task.id,
    attemptGeneration: Math.max(state.attemptGeneration, task.attemptGeneration),
    evidence: [...new Set([...state.evidence, ...evidence])],
  };

  if (task.state === 'cancelled') {
    s = {
      ...s,
      phase: 'cancelled',
      cancel: { requestedAt: s.cancel.requestedAt, confirmedAt: event.at ?? true },
      message: null,
    };
    return { ...s, log: log(s, { type: 'task.changed', state: 'cancelled' }) };
  }
  if (s.phase === 'cancelling') {
    // 取消待确认：核心的任何非终态进展（running/waiting_user/paused/succeeded）都不能
    // 把旅程拉回其他阶段，也不能宣告完成。只有核心 `cancelled` 才是结束。
    return {
      ...s,
      log: log(s, { type: 'task.changed', state: task.state, suppressed: 'cancel_pending' }),
    };
  }

  let phase;
  let message = s.message;
  switch (task.state) {
    case 'queued':
    case 'running':
      phase = 'in_progress';
      message = null;
      break;
    case 'paused':
      phase = 'waiting';
      break;
    case 'waiting_user':
      phase = 'need_choice';
      break;
    case 'succeeded':
      phase = 'completed';
      message = null;
      break;
    case 'failed':
      phase = 'failed';
      message =
        task.error && nonEmpty(task.error.code)
          ? { code: task.error.code, detailRef: task.error.detailRef ?? null }
          : message;
      break;
  }
  s = { ...s, phase, message };
  return { ...s, log: log(s, { type: 'task.changed', state: task.state }) };
}

/** 业务事件：房间成员、连接、账户授权。只读取核心事件信封，不推断成功。 */
function onBusinessEvent(state, event) {
  const env = event.envelope;
  if (!env || !nonEmpty(env.eventId) || !nonEmpty(env.kind)) return state;
  // 重复投递：完全幂等，原样返回输入引用（UI 跳过渲染）。
  if (state.seenEventIds.includes(env.eventId)) return state;
  // 只把幂等拒绝（重复、越序、旧版本）原样返回；有诊断价值的原因才记录日志。
  const s = {
    ...state,
    seenEventIds: [...state.seenEventIds, env.eventId].slice(-EVENT_ID_CAP),
  };

  if (env.operationId && state.operationId && env.operationId !== state.operationId) return state;
  if (isInt(env.attemptGeneration) && env.attemptGeneration < state.attemptGeneration) {
    return ignored(s, 'business.event', 'stale_generation', { eventId: env.eventId });
  }
  if (env.entityId && state.entityId && env.entityId !== state.entityId) return state;
  if (isInt(env.revision) && isInt(state.revision) && env.revision < state.revision) return state;
  if (isInt(env.sequence) && isInt(state.lastSequence) && env.sequence <= state.lastSequence) {
    return state;
  }

  const payload = env.payload ?? {};
  let next = {
    ...s,
    lastSequence: isInt(env.sequence) ? env.sequence : s.lastSequence,
    revision: isInt(env.revision) ? env.revision : s.revision,
    attemptGeneration: isInt(env.attemptGeneration)
      ? Math.max(s.attemptGeneration, env.attemptGeneration)
      : s.attemptGeneration,
    entityId: s.entityId ?? env.entityId ?? null,
  };

  switch (env.kind) {
    case 'membership.changed': {
      const approval = payload.approvalState;
      if (next.phase === 'cancelling') {
        // 取消待确认期间审批返回：不回到“已批准”，也不宣告连接；只有服务端确认离开才算取消完成。
        if (approval === 'left' || approval === 'removed') {
          next = {
            ...next,
            phase: 'cancelled',
            cancel: { requestedAt: next.cancel.requestedAt, confirmedAt: event.at ?? true },
            pendingChoice: null,
          };
        } else {
          // 记录“被抑制”的诊断事实：不改变阶段，也不消费待决审批的展示。
          return {
            ...next,
            log: log(next, {
              type: 'business.event',
              kind: env.kind,
              eventId: env.eventId,
              suppressed: 'cancel_pending',
              approval,
            }),
          };
        }
        break;
      }
      if (approval === 'requested') {
        next = {
          ...next,
          phase: 'waiting',
          step: 'approval',
          pendingChoice: { kind: 'approval', roomId: env.entityId ?? next.entityId },
        };
      } else if (approval === 'approved') {
        next = {
          ...next,
          phase: 'in_progress',
          step: 'connect',
          pendingChoice: null,
          message: null,
        };
      } else if (approval === 'denied') {
        next = {
          ...next,
          phase: 'failed',
          message: { code: 'room.denied', detailRef: payload.detailRef ?? null },
        };
      } else if (approval === 'left' || approval === 'removed') {
        next = {
          ...next,
          phase: 'cancelled',
          cancel: {
            requestedAt: next.cancel.requestedAt ?? (event.at ?? true),
            confirmedAt: event.at ?? true,
          },
        };
      }
      break;
    }
    case 'connection.changed': {
      const st = payload.state;
      if (st === 'connected') {
        next = { ...next, phase: 'in_progress', step: 'connect', message: null };
      } else if (st === 'failed') {
        next = {
          ...next,
          phase: 'failed',
          message: { code: 'room.connecting_failed', detailRef: payload.detailRef ?? null },
        };
      } else if (st === 'disconnected' || st === 'reconnecting') {
        next = {
          ...next,
          phase: 'blocked',
          message: { code: 'room.disconnected', detailRef: payload.detailRef ?? null },
        };
      } else {
        next = { ...next, phase: 'in_progress', step: 'connect' };
      }
      break;
    }
    case 'account.authorization.changed': {
      switch (payload.state) {
        case 'completed':
          // 身份能力变化 ≠ 全部登录完成：还需要角色。
          next = {
            ...next,
            phase: 'need_choice',
            step: 'profile',
            pendingChoice: {
              kind: 'profile',
              providerId: payload.providerId ?? null,
              identityId: payload.identityId ?? null,
            },
          };
          break;
        case 'cancelled':
          next = {
            ...next,
            phase: 'failed',
            message: { code: 'account.auth.cancelled', detailRef: null },
          };
          break;
        case 'expired':
          next = {
            ...next,
            phase: 'failed',
            message: { code: 'account.auth.attempt_expired', detailRef: null },
          };
          break;
        case 'failed':
          next = {
            ...next,
            phase: 'failed',
            message: {
              code: payload.errorCode ?? 'account.service.unavailable',
              detailRef: payload.detailRef ?? null,
            },
          };
          break;
        default:
          next = { ...next, phase: 'waiting', step: 'browser_authorize' };
      }
      break;
    }
    default:
      break; // 未知业务事件只推进序号/代次，不改变阶段
  }
  return { ...next, log: log(next, { type: 'business.event', kind: env.kind, eventId: env.eventId }) };
}

function onCancelRequest(state, event) {
  // 同一取消请求重复点击：已提交，状态引用不变。
  if (nonEmpty(event.requestId) && hasRequest(state, event.requestId)) return state;
  const dedup = withRequest(state, event.requestId);
  const s = dedup.state;
  if (!hasCoreTask(s)) {
    // 没有可取消的核心任务：说明未提交，而不是假装在停止。
    return {
      ...s,
      message: { code: 'operation.cancel.not_started', detailRef: null },
      log: log(s, { type: 'cancel.request', ignored: 'no_task' }),
    };
  }
  if (s.phase === 'cancelling') return ignored(s, 'cancel.request', 'already_cancelling');
  return {
    ...s,
    phase: 'cancelling',
    cancel: { requestedAt: event.at ?? true, confirmedAt: null },
    log: log(s, {
      type: 'cancel.request',
      taskId: s.taskId,
      attemptGeneration: s.attemptGeneration,
    }),
  };
}

function onAuthCallback(state, event) {
  // 浏览器回调可能重复投递：按 attemptId（或调用方 requestId）去重。
  // 已消费的 attempt 不能被重放成另一种结果（例如先 cancelled 后 completed）。
  const key = nonEmpty(event.attemptId) ? `auth:${event.attemptId}` : event.requestId;
  if (!nonEmpty(key)) return ignored(state, 'auth.callback', 'missing_attempt', {});
  if (hasRequest(state, key)) return state;
  const dedup = withRequest(state, key);
  const s = dedup.state;

  if (event.result === 'cancelled') {
    // 取消授权：保留已有账户、目标、待决选择与返回点；旅程不结束（用户可能重新登录）。
    const next = { ...s, message: { code: 'account.auth.cancelled', detailRef: null } };
    return { ...next, log: log(next, { type: 'auth.callback', result: 'cancelled' }) };
  }
  if (event.result !== 'completed') {
    const next = {
      ...s,
      phase: s.phase === 'cancelling' ? 'cancelling' : 'failed',
      message: {
        code: event.errorCode ?? 'account.auth.attempt_expired',
        detailRef: event.detailRef ?? null,
      },
    };
    return { ...next, log: log(next, { type: 'auth.callback', result: 'failed' }) };
  }
  // 完成后的角色选择在此等待；返回原任务只在这些条件满足时由 returnToTask 触发。
  const pendingChoice = {
    kind: 'profile',
    providerId: event.providerId ?? null,
    identityId: event.identityId ?? null,
    profileIds: Array.isArray(event.profileIds) ? [...event.profileIds] : [],
  };
  const next = {
    ...s,
    step: 'profile',
    phase: 'need_choice',
    pendingChoice,
    message:
      pendingChoice.profileIds.length === 0
        ? { code: 'account.platform.ok_profile.missing', detailRef: event.detailRef ?? null }
        : null,
  };
  return { ...next, log: log(next, { type: 'auth.callback', result: 'completed' }) };
}

function onChoiceSelect(state, event) {
  // 同一选择请求重复点击：已提交，引用不变（不重复派发，也不覆盖已选值）。
  if (nonEmpty(event.requestId) && hasRequest(state, event.requestId)) return state;
  if (!state.pendingChoice) return ignored(state, 'choice.select', 'no_pending_choice');
  if (event.kind && state.pendingChoice.kind !== event.kind) {
    return ignored(state, 'choice.select', 'wrong_choice_kind');
  }
  if (!nonEmpty(event.valueId)) return ignored(state, 'choice.select', 'missing_value');
  const dedup = withRequest(state, event.requestId);
  const s = dedup.state;
  const next = {
    ...s,
    pendingChoice: {
      ...s.pendingChoice,
      kind: event.kind ?? s.pendingChoice.kind,
      valueId: event.valueId,
    },
    message: null,
    // 选择落地后回到进行中；原任务是否仍需授权返回，由 returnToTask 显式消费返回点。
    phase: s.phase === 'need_choice' ? 'in_progress' : s.phase,
  };
  return { ...next, log: log(next, { type: 'choice.select', valueId: event.valueId }) };
}

function onReturnToTask(state, event) {
  if (!state.returnPoint) return ignored(state, 'returnToTask', 'no_return_point');
  if (state.pendingChoice && state.pendingChoice.kind === 'profile' && !state.pendingChoice.valueId) {
    // 角色尚未确定：不回到原任务。
    return ignored(state, 'returnToTask', 'choice_pending');
  }
  return {
    ...state,
    returnPoint: null,
    pendingChoice: null,
    phase: 'in_progress',
    log: log(state, { type: 'returnToTask', to: event.to ?? event.returnPoint ?? null }),
  };
}

/**
 * 状态转换入口。
 *
 * @param {Readonly<Object>} state
 * @param {Object} event intent.start / intent.resume / operation.accepted / task.changed /
 *   business.event / cancel.request / auth.callback / choice.select / returnToTask
 * @returns {Readonly<Object>} 新状态；事件被忽略或未识别时返回 state 本身
 */
export function reduceJourney(state, event) {
  if (!state || !event || !nonEmpty(event.type)) return state;
  if (TERMINAL_PHASES.has(state.phase)) {
    // 终态是单向闸门：迟到的授权回调、重复取消确认、旧任务快照一律丢弃，新旅程需要新的 journeyId。
    // 唯一例外是应用层显式声明的 continuation（失败后重试）：它承接原核心任务并提升代次，
    // 不能靠往终态里灌事件“复活”，但也不能让用户失去对失败任务的重试路径。
    const isRetry =
      (event.type === 'intent.start' && event.continuation) ||
      (event.type === 'intent.resume' && state.phase === 'failed');
    if (!isRetry) return state;
  }
  switch (event.type) {
    case 'intent.start':
      return onIntentStart(state, event);
    case 'intent.resume':
      return onIntentResume(state, event);
    case 'operation.accepted':
      return onOperationAccepted(state, event);
    case 'task.changed':
      return onTaskChanged(state, event);
    case 'business.event':
      return onBusinessEvent(state, event);
    case 'cancel.request':
      return onCancelRequest(state, event);
    case 'auth.callback':
      return onAuthCallback(state, event);
    case 'choice.select':
      return onChoiceSelect(state, event);
    case 'returnToTask':
      return onReturnToTask(state, event);
    default:
      return state;
  }
}

// ---------------------------------------------------------------------------
// 投影
// ---------------------------------------------------------------------------

/** 用核心任务快照替代本地进度（重连后以核心为准）。 */
export function reconcileWithTask(state, task, at = null) {
  return reduceJourney(state, { type: 'task.changed', task, at });
}

/**
 * 只读展示投影。不是权限依据；动作执行前核心仍需重查 revision 与权限。
 *
 * `canCancel` 在 `cancelling` 期间保持 true：取消请求只是提交了 `RequestCancel`，
 * 核心确认之前用户仍应看到“正在停止”并可以再次确认进度；是否已经结束由 `cancelPending` 表达。
 */
export function journeyView(state) {
  const active =
    state.phase === 'in_progress' ||
    state.phase === 'waiting' ||
    state.phase === 'need_choice' ||
    state.phase === 'blocked';
  return freeze({
    journeyId: state.journeyId,
    kind: state.kind,
    phase: state.phase,
    step: state.step,
    operationId: state.operationId,
    taskId: state.taskId,
    entityId: state.entityId,
    revision: state.revision,
    attemptGeneration: state.attemptGeneration,
    returnPoint: state.returnPoint,
    pendingChoice: state.pendingChoice,
    canCancel: (active || state.phase === 'cancelling') && state.taskId !== null,
    cancelPending: state.phase === 'cancelling',
    message: state.message,
    evidence: [...state.evidence],
    lastSequence: state.lastSequence,
  });
}

/** 诊断用：返回被忽略/接收的日志（有界）。 */
export function journeyLog(state) {
  return freeze([...state.log]);
}
