import type { TaskState } from "../contract";

export const navLabels = {
  start: "启动",
  instances: "游戏管理",
  multiplayer: "联机",
  discover: "发现",
  downloads: "下载",
  tasks: "任务",
  more: "更多",
  settings: "设置",
} as const;

export type PageId = keyof typeof navLabels;
export type PrimaryPageId = Exclude<PageId, "settings" | "instances" | "discover">;

export const primaryNav: ReadonlyArray<PrimaryPageId> = [
  "start",
  "downloads",
  "multiplayer",
  "more",
  "tasks",
];

export const pageCopy: Record<PageId, { title: string; description: string }> = {
  start: {
    title: "启动",
    description: "选择玩家与游戏，准备启动。",
  },
  instances: {
    title: "游戏管理",
    description: "为不同版本、加载器和模组保留独立环境。",
  },
  multiplayer: {
    title: "联机",
    description: "和熟悉的人进入同一个世界。",
  },
  discover: {
    title: "发现",
    description: "服务器、整合包与内容会在这里出现。",
  },
  downloads: {
    title: "下载",
    description: "安装游戏，获取社区资源。",
  },
  tasks: { title: "任务", description: "查看下载和安装进度，以及最终保存位置。" },
  more: { title: "更多", description: "服务器与扩展功能。" },
  settings: {
    title: "设置",
    description: "外观与本地壳偏好。",
  },
};

export const shellCopy = {
  brandSubtitle: "Minecraft 启动器",
  environmentNative: "桌面壳",
  environmentBrowser: "浏览器预览",
  accountSignedOut: "未登录",
  openAccount: "打开账户",
  loading: "正在读取本地状态…",
  loadError: "本地状态读取失败。",
  retry: "重试",
  m0Note: "当前为 M0 工程骨架，业务核心尚未接入。",
  accountTitle: "账户",
  accountBody: "尚未登录。账户与 MUA 接入后会在这里管理。",
  accountAdd: "添加账户",
  close: "关闭",
  version: "版本",
  contract: "契约",
  environment: "环境",
  settingsAppearance: "外观",
  settingsTheme: "主题",
  settingsDensity: "密度",
  settingsMotion: "动效",
  settingsAbout: "关于",
  themeAuto: "跟随系统",
  themeLight: "浅色",
  themeDark: "深色",
  densityComfortable: "舒展",
  densityCompact: "紧凑",
  motionSystem: "跟随系统",
  motionReduced: "减少动效",
  saveNote: "偏好保存在本机，不上传。",
  unknown: "—",
  cancelPending: "取消请求已记录",
} as const;

export const copy = {
  start: {
    eyebrow: "第一次使用",
    heroTitle: "从第一个实例开始",
    heroBody: "安装原版游戏，或在“实例”中导入现有目录。",
    install: "安装游戏",
    import: "导入游戏",
    aside: "独立实例让你为不同版本、加载器与整合包保留各自的环境。",
    recentTitle: "接着玩",
    recentEmpty: "还没有最近启动的实例。",
    quickTitle: "也可以去看看",
    quickMultiplayerTitle: "联机",
    quickMultiplayerBody: "与朋友进入同一个世界。",
    quickDiscoverTitle: "发现",
    quickDiscoverBody: "浏览服务器与内容。",
  },
  instances: {
    emptyTitle: "还没有实例",
    emptyBody: "安装原版或导入现有游戏后，实例会显示在这里。",
    create: "创建实例",
    install: "安装游戏",
  },
  multiplayer: {
    joinTab: "加入房间",
    mineTab: "我的房间",
    joinTitle: "还没有可加入的房间",
    joinBody: "收到邀请或房间号后，可以在这里加入。",
    mineTitle: "还没有创建房间",
    mineBody: "创建与审批流程接入后，房间会显示在这里。",
    action: "创建房间",
  },
  discover: {
    serversTab: "服务器",
    modpacksTab: "整合包",
    modsTab: "模组",
    packsTab: "资源包与光影",
    serversTitle: "服务器目录尚未接入",
    serversBody: "在线目录与发布接入后，服务器会在这里出现。",
    modpacksTitle: "整合包目录尚未接入",
    modpacksBody: "内容平台接入后，可以在这里检索整合包。",
    modsTitle: "模组目录尚未接入",
    modsBody: "内容平台接入后，可以在这里检索模组。",
    packsTitle: "资源包与光影尚未接入",
    packsBody: "内容平台接入后，可以在这里检索资源包与光影。",
    action: "浏览目录",
  },
  downloads: {
    activeTab: "进行中",
    completedTab: "已完成",
    activeTitle: "没有进行中的下载",
    activeBody: "安装游戏或内容后，下载与校验任务会显示在这里。",
    completedTitle: "还没有已完成的任务",
    completedBody: "完成、失败或取消的任务会保留在这里。",
  },
} as const;

export const taskStateLabels: Record<TaskState, string> = {
  queued: "排队中",
  running: "进行中",
  paused: "已暂停",
  waiting_user: "需要处理",
  succeeded: "已完成",
  failed: "失败",
  cancelled: "已取消",
};

export function isTerminalState(state: TaskState): boolean {
  return state === "succeeded" || state === "failed" || state === "cancelled";
}
