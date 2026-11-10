// Interim UI structure labels.
//
// Static interface copy belongs to module 11 (`apps/desktop/src/ux/copy.ts`).
// These strings are used by the shell until 11 moves them into the shared
// catalog; every key here is listed in `work/dsh-runs/10-copy-requests.md`
// together with the call site. This file is a temporary asset: 11 deletes the
// duplicate after migrating, 10 removes the local copy then.
//
// Two rules apply to everything below:
//   1. No line asserts a product outcome.
//   2. A page without a data source says "not connected"; it never reports a
//      row count it cannot know (evaluator D3).

export const uiLabels = {
  skipToContent: "跳到主内容",
  primaryNav: "主导航",
  tasksHeading: "任务",
  tasksHeadingHint: "来自桌面核心的任务快照",
  taskCancel: "取消任务",
  taskCancelPending: "正在记录取消请求…",
  taskCancelRejectedNotCancellable: "该任务已不能再取消。",
  taskCancelRejectedStale: "任务代次已变化，请刷新后再取消。",
  taskCancelRejectedMissing: "任务已不在列表中。",
  taskCancelUnavailable: "取消需要桌面核心连接。",
  taskRefresh: "刷新状态",
  progressLabel: "任务进度",
  progressUnknownTotal: "总量未知",
  taskRetryable: "可重试",
  busyTitle: "任务正在进行",
  busyBody: "阶段与进度会就地更新。",
  partialTitle: "部分任务缺少总量",
  partialBody: "这些任务的进度按已传输量显示，不显示百分比。",
  loadingTitle: "正在读取本地状态",
  errorTitle: "本地状态读取失败",
  errorBody: "桌面核心没有返回快照。",
  accountDisabledReason: "登录将在账户接入后可用。",
  dialogClose: "关闭对话框",
  openInstances: "去实例",
  platform: "平台",
  arch: "架构",

  // Data sources that the shell snapshot does not carry yet. Wording names the
  // missing source instead of reporting "0 items".
  notConnectedTitle: "数据源尚未接入",
  notConnectedBody: "该页面的数据源还没有加入桌面核心。",
  instancesNotConnectedTitle: "实例列表尚未接入",
  instancesNotConnectedBody: "实例模块接入后，这里会显示实例列表。",
  recentNotConnectedTitle: "最近启动尚未接入",
  recentNotConnectedBody: "实例模块接入后，这里会显示最近启动的实例。",
  roomsJoinNotConnectedTitle: "房间列表尚未接入",
  roomsJoinNotConnectedBody: "房间模块接入后，这里会显示可加入的房间。",
  roomsMineNotConnectedTitle: "我的房间尚未接入",
  roomsMineNotConnectedBody: "房间模块接入后，这里会显示你创建的房间。",
} as const;

/** Task count shown in the downloads header. */
export function formatTaskCount(count: number): string {
  return `${count} 个任务`;
}
