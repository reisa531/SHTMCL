/**
 * Module 11 · ActionAvailability 投影
 * ---------------------------------------------------------------------------
 * 只把动作/能力与已经知道的授权结果映射为「是否可见、原因、下一步、所需权限」。
 * 本文件不执行任何动作；`available: true` 只表示“可以尝试”——真正执行前由核心重查
 * revision、租约、运行会话与服务端授权（contracts/domain-v1.md §9：ActionAvailability
 * 不是权限依据）。
 *
 * `basedOnRevision` 是本投影依据的 entity revision；若 UI 持有的 revision 已被更新改动，
 * 由 `staleAvailability` 把可写能力降级为 unavailable，并提示“重新检查”。
 */

export const AVAILABILITY_SCHEMA_VERSION = 'availability.v1';

/** 本地能力目录：`write` 表示执行前必须由核心重查 revision；`read` 表示只读入口。 */
const CAPABILITIES = {
  'launch.start': { write: true, permission: 'instance.launch', owner: '02' },
  'launch.prepare_and_start': { write: true, permission: 'instance.launch', owner: '02' },
  'launch.install_game': { write: true, permission: 'instance.create', owner: '02' },
  'instance.import_game': { write: true, permission: 'instance.import', owner: '01' },
  'instance.change_location': { write: true, permission: 'instance.manage', owner: '01' },
  'instance.create_for_room': { write: true, permission: 'instance.create', owner: '01' },
  'instance.open_picker': { write: false, permission: null, owner: '10' },
  'task.cancel': { write: true, permission: 'task.cancel', owner: '00' },
  'operation.replan': { write: false, permission: null, owner: '00' },
  'download.resume': { write: true, permission: 'download.resume', owner: '03' },
  'download.pause': { write: true, permission: 'download.pause', owner: '03' },
  'download.pick_source': { write: true, permission: 'download.set_source', owner: '03' },
  'download.pick_local_file': { write: true, permission: 'download.supply_file', owner: '03' },
  'download.retry_artifact': { write: true, permission: 'download.resume', owner: '03' },
  'cache.clean': { write: true, permission: 'cache.clean', owner: '03' },
  'content.commit_plan': { write: true, permission: 'instance.content_write', owner: '01' },
  'content.open_conflict': { write: false, permission: null, owner: '04' },
  'content.narrow_scope': { write: false, permission: null, owner: '04' },
  'content.choose_version': { write: false, permission: null, owner: '04' },
  'content.keep_deferred_update': { write: false, permission: null, owner: '10' },
  'account.begin_authorization': { write: true, permission: 'account.local', owner: '05' },
  'account.reauthorize': { write: true, permission: 'account.local', owner: '05' },
  'account.cancel_authorization': { write: true, permission: 'account.local', owner: '05' },
  'account.list_profiles': { write: false, permission: 'account.local', owner: '05' },
  'account.refresh_session': { write: true, permission: 'account.local', owner: '05' },
  'account.open_manager': { write: false, permission: null, owner: '10' },
  'runtime.install_java': { write: true, permission: 'runtime.install', owner: '02' },
  'runtime.choose_java': { write: true, permission: 'runtime.choose', owner: '02' },
  'room.join': { write: true, permission: 'room.join', owner: '06' },
  'room.leave_membership': { write: true, permission: 'membership.leave', owner: '06' },
  'room.open_connection_detail': { write: false, permission: null, owner: '10' },
  'room.open_list': { write: false, permission: null, owner: '10' },
  'server.cancel_add': { write: true, permission: 'server.add', owner: '07' },
  'server.retry_read_list': { write: false, permission: null, owner: '07' },
  'server.open_list_file': { write: false, permission: null, owner: '10' },
  'launch.stop_game': { write: false, permission: null, owner: '10' },
  'diagnostics.open_log': { write: false, permission: null, owner: '08' },
  'diagnostics.export': { write: true, permission: 'diagnostics.export', owner: '08' },
  'network.recheck': { write: false, permission: null, owner: '00' },
  'instance.open_settings': { write: false, permission: null, owner: '10' },
};

export const CAPABILITY_IDS = Object.freeze(Object.keys(CAPABILITIES).sort());

/** 已知缺失条件 → 用户可读原因、可解除它的下一步能力。与 availability 输出同一张表。 */
export const CONDITIONS = Object.freeze({
  game_running: { reason: '游戏正在运行，退出后才能继续', nextStep: 'launch.stop_game' },
  revision_changed: { reason: '内容已在别处更改，需要重新检查', nextStep: 'operation.replan' },
  offline: { reason: '当前处于离线，需要重试', nextStep: 'network.recheck' },
  profile_missing: { reason: '还没有选择游戏角色', nextStep: 'account.list_profiles' },
  membership_pending: { reason: '加入申请仍在等待房主', nextStep: 'wait' },
  membership_unknown: { reason: '暂时无法查询成员状态', nextStep: 'refresh' },
  no_permission: { reason: '当前账户没有这项权限', nextStep: 'instance.open_settings' },
});

/**
 * @param {Array<{label: string, capability: string}>} actions 来自 UserMessage
 * @param {Object} input
 * @param {Object<string, string>} [input.conditions] capability → 缺失条件键（如 'game_running'）
 * @param {string[]} [input.grantedPermissions] 核心/服务端已确认的权限 ID
 * @param {number|null} [input.revision] 投影所依据的 entity revision
 * @param {string|null} [input.inProgressRequestId] 正在提交的请求（防止重复派发）
 * @param {Object<string,string>|null} [input.currentRequest] capability → 已提交的 requestId
 * @param {Object<string,{reason:string,nextStep:string|null}>} [input.blockers] 核心给出的不可用原因
 * @returns {ReadonlyArray<Object>}
 */
export function projectActions(actions, input = {}) {
  const granted = new Set(input.grantedPermissions ?? []);
  const conditions = input.conditions ?? {};
  const blockers = input.blockers ?? {};
  const current = input.currentRequest ?? {};
  const revision = Number.isInteger(input.revision) ? input.revision : null;

  return Object.freeze((actions ?? []).map((action) => {
    const spec = CAPABILITIES[action.capability];
    const base = {
      label: action.label,
      capability: action.capability,
      role: action.role ?? 'secondary',
      permission: spec?.permission ?? null,
      basedOnRevision: revision,
    };
    if (!spec) {
      return Object.freeze({
        ...base,
        state: 'unavailable',
        available: false,
        reason: '这项操作尚未接入',
        nextStep: null,
      });
    }
    if (input.inProgressRequestId && current[action.capability] === input.inProgressRequestId) {
      return Object.freeze({
        ...base,
        state: 'in_progress',
        available: false,
        reason: '已提交，等待结果',
        nextStep: null,
      });
    }
    if (blockers[action.capability]) {
      return Object.freeze({ ...base, state: 'unavailable', available: false, ...blockers[action.capability] });
    }
    if (conditions[action.capability]) {
      const c = CONDITIONS[conditions[action.capability]];
      return Object.freeze({
        ...base,
        state: 'unavailable',
        available: false,
        reason: c?.reason ?? '缺少条件',
        nextStep: c?.nextStep ?? null,
        condition: conditions[action.capability],
      });
    }
    if (spec.permission && granted.size > 0 && !granted.has(spec.permission)) {
      return Object.freeze({
        ...base,
        state: 'unavailable',
        available: false,
        reason: CONDITIONS.no_permission.reason,
        nextStep: CONDITIONS.no_permission.nextStep,
        condition: 'no_permission',
      });
    }
    return Object.freeze({ ...base, state: 'available', available: true, reason: '', nextStep: null, condition: null });
  }));
}

/**
 * 核心已给出更高 revision：可写能力必须降级，提示重做检查，防止用旧快照执行。
 * @param {ReadonlyArray<Object>} projected
 * @param {number|null} coreRevision
 */
export function staleAvailability(projected, coreRevision) {
  return Object.freeze(projected.map((a) => {
    const spec = CAPABILITIES[a.capability];
    if (!spec?.write) return a;
    if (Number.isInteger(coreRevision) && Number.isInteger(a.basedOnRevision) && coreRevision > a.basedOnRevision) {
      return Object.freeze({
        ...a,
        state: 'unavailable',
        available: false,
        reason: '内容已在别处更改，需要重新检查',
        nextStep: 'operation.replan',
      });
    }
    return a;
  }));
}

/** 汇总：只返回可执行动作；每条都标记需要核心重查。可作为 UI `capabilities` 字段。 */
export function availabilityFor(actions, input = {}) {
  const projected = staleAvailability(projectActions(actions, input), input.coreRevision ?? null);
  return Object.freeze({
    schemaVersion: AVAILABILITY_SCHEMA_VERSION,
    revision: Number.isInteger(input.revision) ? input.revision : null,
    requiresRecheckAtExecution: true,
    actions: projected,
    enabled: projected.filter((a) => a.available).map((a) => a.capability),
  });
}

export function capabilitySpec(id) {
  return CAPABILITIES[id] ?? null;
}
