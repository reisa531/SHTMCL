/**
 * Module 11 · 产品文案目录（简体中文）
 * ---------------------------------------------------------------------------
 * 纯数据 + 纯函数，无副作用、无外部依赖，Node 与 Tauri WebView 都可直接使用。
 *
 * 职责边界：
 * - 把稳定的错误/状态 `code` 映射为 `UserMessage`（短标题、必要说明、可执行的下一步动作）；
 * - 不发起网络请求、不写本地文件、不判断业务成功与否——只由核心提供的状态与证据决定；
 * - 不使用泛化免责语或“AI 智能”类措辞；只有底层真缺证据时才出现「已保留/已完成」。
 *
 * `UserMessage` 是 00 冻结的 DTO（contracts/domain-v1.md §9）。本模块用 JS 数据结构承载文案，
 * 形状由 `tests/modules/11/` 校验；类型来源是 `apps/desktop/src/contract.ts`。
 */

/** 文案目录版本。文案修改时必须提升，UI 据此判断已装壳与文案是否匹配。 */
export const CONTENT_VERSION = '11.1.0';

/**
 * 用户动作的固定用词，与产品稿 v0.3 §8.1「按钮标签」一致；用于同一动作在不同文案里保持同一动词。
 * @type {ReadonlyArray<string>}
 */
export const PRIMARY_VERBS = Object.freeze([
  '启动游戏',
  '安装游戏',
  '导入游戏',
  '创建实例',
  '添加内容',
  '加入房间',
  '添加服务器',
  '新建服务器',
  '登录',
  '重新登录',
  '重试',
  '恢复上一版本',
]);

/** 消息分类：平台、账户、游戏、网络、本地文件、内容包、房间、服务器、启动、运行时。 */
export const MESSAGE_CATEGORY = Object.freeze({
  PLATFORM: 'platform',
  ACCOUNT: 'account',
  GAME: 'game',
  NETWORK: 'network',
  LOCAL_FILE: 'local_file',
  CONTENT: 'content',
  ROOM: 'room',
  SERVER: 'server',
  LAUNCH: 'launch',
  JAVA: 'runtime',
});

/** 技术详情类别：只在用户展开时按 `detail_ref` 读取。 */
export const DETAIL_KIND = Object.freeze({
  LOG: 'log',
  DIAGNOSTIC: 'diagnostic',
  TASK: 'task',
  CONNECTION: 'connection',
  FILE: 'file',
  CHANGE_PLAN: 'change_plan',
  ACCOUNT: 'account',
});

/**
 * 文案模板。`{name}` 是占位符，由 `formatMessage` 用调用方提供的数据填充；
 * 缺失时退回 `fallbackName`（通用说法），绝不露出字段名。
 * `evidence` 是显示该文案前必须由核心真实产出的证据标记（contracts/domain-v1.md §9）。
 * @type {Readonly<Record<string, Object>>}
 */

/** 不提供技术详情。 */
const noDetail = () => null;
/** 详情引用由调用方给出时直接使用，否则留空。 */
const logDetail = (ref) => ref || null;

const TEMPLATES = {
  // ---------------------------------------------------------------- 启动 ----
  'launch.process.started': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '正在启动游戏',
    body: '游戏进程已创建，窗口出现后即可进入。',
    detail: (ctx) => ctx?.detail_ref ?? null,
    // 进程已创建 ≠ 游戏画面：只允许核心的真实证据。
    evidence: ['process_created'],
  },
  'launch.first_use.empty': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '安装一个版本再开始游戏',
    body: null,
    actions: [
      { label: '安装游戏', capability: 'launch.install_game', role: 'primary' },
      { label: '导入游戏', capability: 'instance.import_game', role: 'secondary' },
    ],
  },
  'launch.continue.resume_hint': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '继续上次的 {instance_name}',
    body: '将使用 {profile_name} 与上次的 Java 设置。',
    fallbackName: { instance_name: '实例', profile_name: '上次的游戏角色' },
    actions: [
      { label: '启动游戏', capability: 'launch.start', role: 'primary' },
    ],
  },
  'launch.dependency.missing': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '{instance_name} 缺少 {size_text} 的文件',
    body: '下载后即可直接启动。',
    fallbackName: { instance_name: '该实例', size_text: '部分' },
    actions: [
      { label: '下载并启动', capability: 'launch.prepare_and_start', role: 'primary' },
    ],
  },
  'launch.preparing.cancellable': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '正在准备 {instance_name}',
    body: '可以继续使用其他实例。',
    fallbackName: { instance_name: '游戏' },
    actions: [
      { label: '取消准备', capability: 'task.cancel', role: 'secondary' },
    ],
  },
  'launch.failed.unknown': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '启动失败，查看日志',
    body: '游戏进程没有继续运行。',
    detail: logDetail,
    actions: [
      { label: '查看日志', kind: 'detail', capability: 'diagnostics.open_log', role: 'primary' },
      { label: '导出诊断', capability: 'diagnostics.export', role: 'secondary' },
    ],
  },
  'launch.session.cannot_cancel': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '游戏正在使用 Java 进程',
    body: '请从游戏内退出；强制停止会在下次启动时提示修复。',
  },

  // ---------------------------------------------------------------- 账户 ----
  'account.login.start': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '登录',
    body: '{provider_name} 将在系统浏览器中打开授权页面。',
    fallbackName: { provider_name: '账户提供方' },
    actions: [
      { label: '登录', capability: 'account.begin_authorization', role: 'primary' },
    ],
  },
  'account.browser.waiting': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '等待浏览器中的 {provider_name} 授权',
    body: '回到本应用后会继续原来的步骤。',
    fallbackName: { provider_name: '账户' },
    actions: [
      { label: '取消登录', capability: 'account.cancel_authorization', role: 'secondary' },
    ],
  },
  'account.expired': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '登录已过期',
    body: '{provider_name} 的登录需要更新。',
    fallbackName: { provider_name: '账户' },
    actions: [
      { label: '重新登录', capability: 'account.reauthorize', role: 'primary' },
    ],
  },
  'account.auth.cancelled': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '已取消登录',
    body: '原有账户与选择保持不变。',
    actions: [
      { label: '重新登录', capability: 'account.begin_authorization', role: 'secondary' },
    ],
  },
  'account.auth.attempt_expired': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '本次登录已结束',
    body: '浏览器授权页面超过有效期，需要重新打开。',
    actions: [
      { label: '重新登录', capability: 'account.begin_authorization', role: 'primary' },
    ],
  },
  'account.platform.ok_profile.missing': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '选择一个游戏角色',
    body: '{provider_name} 账户已登录，但没有可用的 Minecraft 角色。',
    fallbackName: { provider_name: '账户' },
    detail: (ctx) => ctx?.detail_ref ?? null,
    actions: [
      { label: '选择角色', capability: 'account.list_profiles', role: 'primary' },
      { label: '打开账户管理', kind: 'detail', capability: 'account.open_manager', role: 'secondary' },
    ],
  },
  'account.profile.multi_choice': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '选择角色继续',
    body: '这些角色属于 {provider_name}。',
    fallbackName: { provider_name: '当前账户' },
  },
  'account.session.available': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '已登录 {profile_name}',
    body: null,
    fallbackName: { profile_name: '游戏角色' },
  },
  'account.service.unavailable': {
    category: MESSAGE_CATEGORY.ACCOUNT,
    title: '账户服务暂时连不上',
    body: '本地已保存的账户仍可使用离线模式。',
    actions: [
      { label: '重试', capability: 'account.refresh_session', role: 'primary' },
    ],
  },

  // ---------------------------------------------------------------- Java ----
  'launch.java.mismatch': {
    category: MESSAGE_CATEGORY.JAVA,
    title: 'Java 版本不匹配',
    body: '当前 Java {java_current}，这个实例需要 Java {java_major}。',
    fallbackName: { java_current: '未知版本', java_major: '对应版本' },
    actions: [
      { label: '安装 Java {java_major}', capability: 'runtime.install_java', role: 'primary' },
      { label: '选择已有 Java', capability: 'runtime.choose_java', role: 'secondary' },
    ],
  },
  'launch.java.not_found': {
    category: MESSAGE_CATEGORY.JAVA,
    title: '没有找到可用的 Java {java_major}',
    body: '可以安装所需版本，或指定本机已有的 Java。',
    fallbackName: { java_major: '' },
    actions: [
      { label: '安装所需 Java', capability: 'runtime.install_java', role: 'primary' },
      { label: '选择已有 Java', capability: 'runtime.choose_java', role: 'secondary' },
    ],
  },

  // ---------------------------------------------------------------- 下载 ----
  'download.interrupted': {
    category: MESSAGE_CATEGORY.NETWORK,
    title: '下载中断',
    // 只有核心确认存在可续传区间才允许出现「已保留」，见 evidence。
    body: '已保留进度，可以直接继续。',
    evidence: ['resumable_ranges'],
    actions: [
      { label: '继续下载', capability: 'download.resume', role: 'primary' },
    ],
  },
  'download.interrupted.no_progress': {
    category: MESSAGE_CATEGORY.NETWORK,
    title: '下载中断',
    body: '需要从头重新下载这项文件。',
    actions: [
      { label: '重新下载', capability: 'download.resume', role: 'primary' },
    ],
  },
  'download.paused': {
    category: MESSAGE_CATEGORY.NETWORK,
    title: '下载已暂停',
    body: '已完成的文件会保留，未完成的部分继续下载。',
    evidence: ['completed_artifacts'],
    actions: [
      { label: '继续下载', capability: 'download.resume', role: 'primary' },
    ],
  },
  'download.paused.no_progress': {
    category: MESSAGE_CATEGORY.NETWORK,
    title: '下载已暂停',
    body: '继续时会重新校验已下载的部分。',
    actions: [
      { label: '继续下载', capability: 'download.resume', role: 'primary' },
    ],
  },
  'download.verifying': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '正在校验 {file_name}',
    body: null,
    fallbackName: { file_name: '文件' },
  },
  'download.installing': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '正在写入文件',
    body: '完成后会自动继续。',
    actions: [
      { label: '取消安装', capability: 'task.cancel', role: 'secondary' },
    ],
  },
  'download.rate_limited': {
    category: MESSAGE_CATEGORY.NETWORK,
    title: '下载来源繁忙',
    body: '{source_name} 正在限速。',
    fallbackName: { source_name: '下载源' },
    actions: [
      { label: '切换来源', capability: 'download.pick_source', role: 'primary' },
      { label: '暂停', capability: 'download.pause', role: 'secondary' },
    ],
  },
  'download.no_space': {
    category: MESSAGE_CATEGORY.LOCAL_FILE,
    title: '{location_name} 空间不足',
    body: '这项内容需要 {size_text}。',
    fallbackName: { location_name: '目标目录', size_text: '更多空间' },
    actions: [
      { label: '更改位置', capability: 'instance.change_location', role: 'primary' },
      { label: '清理缓存', capability: 'cache.clean', role: 'secondary' },
    ],
  },
  'download.untrusted_file': {
    category: MESSAGE_CATEGORY.LOCAL_FILE,
    title: '{file_name} 与记录的来源不一致',
    body: '文件来自 {source_name}，与预期来源不同。',
    fallbackName: { file_name: '这个文件', source_name: '未知来源' },
    actions: [
      { label: '选择本地文件', capability: 'download.pick_local_file', role: 'primary' },
    ],
  },
  'download.hash_mismatch': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '{file_name} 校验不通过',
    body: '文件可能在下载过程中损坏。',
    fallbackName: { file_name: '文件' },
    actions: [
      { label: '重新下载', capability: 'download.retry_artifact', role: 'primary' },
      { label: '换一个来源', capability: 'download.pick_source', role: 'secondary' },
    ],
  },
  'download.source.unavailable': {
    category: MESSAGE_CATEGORY.NETWORK,
    title: '这个下载源暂时不可用',
    body: '可以换一个来源继续。',
    actions: [
      { label: '继续下载', capability: 'download.resume', role: 'primary' },
      { label: '换一个来源', capability: 'download.pick_source', role: 'secondary' },
    ],
  },

  // ---------------------------------------------------------------- 内容 ----
  'content.mod_conflict': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '{mod_a} 与 {mod_b} 不能同时启用',
    body: '两者要求不同的加载器。',
    fallbackName: { mod_a: '模组 A', mod_b: '模组 B' },
    actions: [
      { label: '查看冲突', kind: 'detail', capability: 'content.open_conflict', role: 'primary' },
    ],
  },
  'content.solve_budget_exhausted': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '组合尝试已达上限',
    body: '可以缩小范围，或改用一个已知可用的版本组合。',
    actions: [
      { label: '缩小范围', capability: 'content.narrow_scope', role: 'primary' },
      { label: '选择其他版本', capability: 'content.choose_version', role: 'secondary' },
    ],
  },
  'content.no_solution_proven': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '没有找到可用的组合',
    body: '已知冲突：{conflict_summary}。',
    fallbackName: { conflict_summary: '存在互相排斥的要求' },
    actions: [
      { label: '查看冲突', kind: 'detail', capability: 'content.open_conflict', role: 'primary' },
    ],
  },
  'content.preview.ready': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '准备写入 {instance_name}',
    body: '新增 {add_count} 项，更新 {changed_count} 项，移除 {remove_count} 项。',
    fallbackName: {
      instance_name: '实例',
      add_count: '若干',
      changed_count: '若干',
      remove_count: '若干',
    },
    actions: [
      { label: '确认写入', capability: 'content.commit_plan', role: 'primary' },
    ],
  },
  'content.update.deferred_running': {
    category: MESSAGE_CATEGORY.CONTENT,
    title: '{instance_name} 正在运行',
    body: '更新会在游戏退出后应用。',
    fallbackName: { instance_name: '实例' },
    actions: [
      { label: '退出游戏后添加', capability: 'content.keep_deferred_update', role: 'secondary' },
      { label: '退出游戏', capability: 'launch.stop_game', role: 'secondary' },
    ],
  },

  // ---------------------------------------------------------------- 房间 ----
  'room.approval.waiting': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '等待房主同意',
    body: '{owner_name} 确认后会自动开始连接。',
    fallbackName: { owner_name: '房主' },
    actions: [
      { label: '取消加入', capability: 'room.leave_membership', role: 'secondary' },
    ],
  },
  'room.leave.cancelling': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '正在离开房间',
    body: '等待服务器确认。',
    evidence: ['leave_pending'],
  },
  'room.leave.confirmed': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '已离开房间',
    body: null,
    evidence: ['membership_left'],
  },
  'room.connecting': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '正在连接',
    body: '正在寻找可用的连接方式。',
  },
  'room.connected.direct': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '正在直连',
    body: null,
    evidence: ['connection_connected', 'transport_direct'],
    actions: [
      { label: '连接详情', kind: 'detail', capability: 'room.open_connection_detail', role: 'secondary' },
    ],
  },
  'room.connected.relay': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '已通过中继连接',
    body: null,
    evidence: ['connection_connected', 'transport_relay'],
    actions: [
      { label: '连接详情', kind: 'detail', capability: 'room.open_connection_detail', role: 'secondary' },
    ],
  },
  'room.disconnected': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '连接已断开',
    body: '可以重新加入这个房间。',
    evidence: ['connection_disconnected'],
    actions: [
      { label: '重新加入', capability: 'room.join', role: 'primary' },
    ],
  },
  'room.denied': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '加入申请未通过',
    body: '房主没有通过这次申请。',
    actions: [
      { label: '查看房间列表', capability: 'room.open_list', role: 'secondary' },
    ],
  },
  'room.instance.missing': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '没有符合要求的实例',
    body: '这个房间需要 {game_version} + {loader}。',
    fallbackName: { game_version: '指定版本', loader: '指定加载器' },
    actions: [
      { label: '创建实例', capability: 'instance.create_for_room', role: 'primary' },
    ],
  },
  'room.profile.missing': {
    category: MESSAGE_CATEGORY.ROOM,
    title: '还没有选择游戏角色',
    body: '加入房间前需要选择一个角色。',
    actions: [
      { label: '选择角色', capability: 'account.list_profiles', role: 'primary' },
    ],
  },

  // ---------------------------------------------------------------- 服务器 ----
  'server.added': {
    category: MESSAGE_CATEGORY.SERVER,
    title: '已添加 {server_name}',
    body: null,
    fallbackName: { server_name: '服务器' },
    evidence: ['binding_committed'],
    actions: [
      { label: '启动游戏', capability: 'launch.start', role: 'primary' },
    ],
  },
  'server.added.start': {
    category: MESSAGE_CATEGORY.SERVER,
    title: '已添加 {server_name}，可以开始游戏',
    body: null,
    fallbackName: { server_name: '服务器' },
    evidence: ['binding_committed'],
    actions: [
      { label: '启动游戏', capability: 'launch.start', role: 'primary' },
    ],
  },
  'server.waiting_game_exit': {
    category: MESSAGE_CATEGORY.SERVER,
    title: '退出游戏后添加',
    body: '{instance_name} 正在运行，退出后写入服务器列表。',
    fallbackName: { instance_name: '实例' },
    actions: [
      { label: '取消添加', capability: 'server.cancel_add', role: 'secondary' },
    ],
  },
  'server.committing': {
    category: MESSAGE_CATEGORY.SERVER,
    title: '正在写入服务器列表',
    body: '原文件已备份。',
  },
  'server.list_corrupt': {
    category: MESSAGE_CATEGORY.SERVER,
    title: '服务器列表无法解析',
    body: '原文件已备份，可以重新读取或手动打开。',
    evidence: ['original_file_preserved'],
    actions: [
      { label: '打开文件', kind: 'detail', capability: 'server.open_list_file', role: 'primary' },
      { label: '重新读取', capability: 'server.retry_read_list', role: 'secondary' },
    ],
  },
  'server.incompatible.version': {
    category: MESSAGE_CATEGORY.SERVER,
    title: '版本不一致',
    body: '服务器使用 {game_version}，当前实例是 {instance_version}。',
    fallbackName: { game_version: '其他版本', instance_version: '当前版本' },
    actions: [
      { label: '选择其他实例', capability: 'instance.open_picker', role: 'primary' },
    ],
  },

  // ---------------------------------------------------------------- 平台 ----
  'platform.permission.changed': {
    category: MESSAGE_CATEGORY.PLATFORM,
    title: '{location_name} 的访问权限已变化',
    body: '需要重新确认这个位置的写入权限。',
    fallbackName: { location_name: '目标目录' },
    actions: [
      { label: '更换位置', capability: 'instance.change_location', role: 'primary' },
    ],
  },
  'platform.disk_full': {
    category: MESSAGE_CATEGORY.PLATFORM,
    title: '磁盘空间不足',
    body: '{location_name} 剩余空间不够。',
    fallbackName: { location_name: '目标磁盘' },
    actions: [
      { label: '清理缓存', capability: 'cache.clean', role: 'primary' },
      { label: '更换位置', capability: 'instance.change_location', role: 'secondary' },
    ],
  },
  'platform.file_busy': {
    category: MESSAGE_CATEGORY.PLATFORM,
    title: '文件正被其他程序占用',
    body: '关闭占用它的程序后重试。',
    actions: [
      { label: '退出游戏', capability: 'launch.stop_game', role: 'secondary' },
    ],
  },

  // ---------------------------------------------------------------- 操作 ----
  'operation.cancel.requested': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '正在停止',
    body: '等待当前步骤安全退出。',
    evidence: ['cancel_requested'],
  },
  'operation.cancel.unsupported': {
    category: MESSAGE_CATEGORY.LAUNCH,
    title: '这一步不能中途停止',
    body: '写入文件的过程不能被中断。',
  },
  'operation.conflict.revision': {
    category: MESSAGE_CATEGORY.PLATFORM,
    title: '内容已被改动',
    body: '在你操作期间，这项内容被其他步骤修改过。',
    actions: [
      { label: '重新检查', capability: 'operation.replan', role: 'primary' },
    ],
  },
  'network.offline': {
    category: MESSAGE_CATEGORY.NETWORK,
    title: '当前已离线',
    body: '本地实例可以启动，联网内容需要恢复连接后再用。',
    actions: [
      { label: '重试', capability: 'network.recheck', role: 'primary' },
    ],
  },
};

/**
 * 已知的文案代码集合。
 * @type {ReadonlyArray<string>}
 */
export const MESSAGE_CODES = Object.freeze(Object.keys(TEMPLATES).sort());

function pickActions(actions, options) {
  const maxActions = options?.maxActions ?? 2;
  const allowed = options?.allowedCapabilities ? new Set(options.allowedCapabilities) : null;
  return (actions ?? [])
    .filter((a) => !allowed || allowed.has(a.capability))
    .slice(0, maxActions)
    .map((a) => ({
      label: a.label,
      capability: a.capability,
      role: a.role ?? 'secondary',
      ...(a.kind ? { kind: a.kind } : {}),
    }));
}

function fill(template, ctx, fallback) {
  if (template == null) return null;
  return template.replace(/\{(\w+)\}/g, (_, key) => {
    const value = ctx?.[key];
    if (value === undefined || value === null || value === '') {
      const fb = fallback?.[key];
      return fb === undefined ? `{${key}}` : fb;
    }
    return String(value);
  });
}

/**
 * 把稳定 code 渲染为 UserMessage。
 * @param {string} code
 * @param {Object<string, unknown>} [context] 用于展示的具体数据（实例名、提供方、大小等），不含凭据。
 * @param {{ maxActions?: number, detail_ref?: string }} [options]
 * @returns {Object} 形状见 contracts/domain-v1.md §9 与 apps/desktop/src/contract.ts
 */
export function formatMessage(code, context = {}, options = {}) {
  const t = Object.prototype.hasOwnProperty.call(TEMPLATES, code) ? TEMPLATES[code] : null;
  const ctx = { ...context, detail_ref: options?.detail_ref ?? context?.detail_ref };
  if (!t) {
    // 未知错误码：不展示内部异常文本，只给出可观察的真实入口。
    return {
      code,
      codeKnown: false,
      contentVersion: CONTENT_VERSION,
      category: MESSAGE_CATEGORY.PLATFORM,
      title: '操作没有完成',
      body: '可以在诊断日志中查看详情。',
      actions: [
        { label: '查看日志', capability: 'diagnostics.open_log', role: 'primary', kind: 'detail' },
      ],
      detailRef: ctx?.detail_ref ?? null,
      evidence: [],
    };
  }
  const fallback = t.fallbackName ?? {};
  let detailRef = ctx?.detail_ref ?? null;
  if (typeof t.detail === 'function') {
    detailRef = t.detail(ctx) ?? null;
  }
  return {
    code,
    codeKnown: true,
    contentVersion: CONTENT_VERSION,
    category: t.category,
    title: fill(t.title, ctx, fallback),
    body: fill(t.body, ctx, fallback),
    actions: pickActions(t.actions, options).map((a) => ({
      ...a,
      label: fill(a.label, ctx, fallback),
    })),
    detailRef,
    evidence: t.evidence ? [...t.evidence] : [],
  };
}

/**
 * 是否允许展示该文案：`evidence` 不为空的文案要求核心提供全部证据标记。
 * 例如 core 的 `download.interrupted` 事件携带 `evidence: ['resumable_ranges']` 才可提示「已保留」。
 * @param {string} code
 * @param {ReadonlyArray<string>|undefined} evidence 核心事件/任务提供的证据标记
 */
export function messageAllowed(code, evidence) {
  const t = Object.prototype.hasOwnProperty.call(TEMPLATES, code) ? TEMPLATES[code] : null;
  if (!t) return false;
  if (!t.evidence || t.evidence.length === 0) return true;
  const have = new Set(evidence ?? []);
  return t.evidence.every((e) => have.has(e));
}

/** 供测试与文档使用：返回不可变目录摘要，便于对照。 */
export function messageIndex() {
  return MESSAGE_CODES.map((code) => {
    const t = TEMPLATES[code];
    return {
      code,
      category: t.category,
      title: t.title,
      actions: (t.actions ?? []).map((a) => ({ label: a.label, capability: a.capability })),
      evidence: t.evidence ? [...t.evidence] : [],
    };
  });
}
