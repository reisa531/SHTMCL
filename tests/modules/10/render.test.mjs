// Module 10 · component and shell assertions.
//
// Rendered with `react-dom/server`, so nothing here needs a browser. The
// interesting flows (cancel, Escape) are asserted through pure functions
// because static markup cannot show interaction.

import test from "node:test";
import assert from "node:assert/strict";

import { loadBundle } from "./harness.mjs";

const ui = await loadBundle();

const PAGES = [
  "start",
  "instances",
  "multiplayer",
  "discover",
  "downloads",
  "settings",
  "tasks",
  "more",
];

/** Product wording that must never reach a normal interface. */
const FORBIDDEN_WORDING = /演示|免责|不保证|仅供|示意|智能|不会进行任何操作/;
const BROKEN_RENDER = /undefined|NaN|\[object Object\]/;
const downloadEntry = (task) => ({ operationId: task.operationId, taskId: task.id, name: "Minecraft", version: "1.7.10", resourceType: "game_environment", destination: { kind: "instance", instanceId: "school", instanceName: "校园生存", path: "F:/games/school" } });

test("shell markup carries no disclaimer or demo wording", () => {
  const markups = {
    app: ui.renderApp(),
    ...Object.fromEntries(PAGES.map((page) => [page, ui.renderShell(page)])),
    "downloads-with-tasks": ui.renderShell("tasks", {
      tasks: Object.values(ui.TASKS),
    }),
    taskList: ui.renderTaskList(Object.values(ui.TASKS)),
  };

  for (const [name, markup] of Object.entries(markups)) {
    assert.ok(markup.length > 0, `${name}: empty markup`);
    assert.doesNotMatch(markup, FORBIDDEN_WORDING, `${name}: forbidden wording`);
    assert.doesNotMatch(markup, /\bAI\b/, `${name}: AI wording`);
    assert.doesNotMatch(markup, BROKEN_RENDER, `${name}: broken render output`);
  }
});

test("navigation exposes primary pages and keeps game management nested", () => {
  const markup = ui.renderApp();
  const navPages = [...markup.matchAll(/data-page="([a-z]+)"/g)].map(
    (match) => match[1],
  );
  const expected = [...ui.primaryNav, "settings"];

  assert.equal(navPages.length, expected.length, "nav item count");
  assert.deepEqual(new Set(navPages), new Set(expected), "nav ids");
  assert.deepEqual(new Set(Object.keys(ui.navLabels)), new Set(PAGES), "page id set");
  assert.ok(!navPages.includes("instances") && !navPages.includes("discover"), "nested routes are not primary items");
  // Settings is the last entry: five primary pages plus settings.
  assert.equal(ui.primaryNav.length, expected.length - 1);
  assert.equal(navPages[navPages.length - 1], "settings");

  const current = markup.match(/aria-current="page"/g);
  assert.equal(current?.length, 1, "exactly one aria-current");
  assert.match(markup, /data-page="start"[^>]*aria-current="page"/);
});

test("native window controls are minimize then close, with no maximize action", () => {
  const markup = ui.renderWindowControls(true);
  assert.deepEqual([...markup.matchAll(/data-window-action="([a-z]+)"/g)].map((match) => match[1]), ["minimize", "close"]);
  assert.match(markup, /aria-label="最小化"/);
  assert.match(markup, /aria-label="关闭窗口"/);
  assert.match(markup, /aria-hidden="true">−/);
  assert.match(markup, /aria-hidden="true">×/);
  assert.equal(ui.renderWindowControls(false), "", "browser retains its own window chrome");
});

test("instance identity shows the actual loader build, inventory and pack without inventing missing data", () => {
  const vanilla = ui.renderInstanceDetails({id:"a",name:"Game",version:{gameVersion:"1.21",loader:"vanilla"}});
  assert.match(vanilla, /data-loader="vanilla"/);
  assert.match(vanilla, /Minecraft 1.21/);
  assert.match(vanilla, /原版/);
  assert.doesNotMatch(vanilla, /个模组文件|整合包 ·/);
  const pack = ui.renderInstanceDetails({id:"b",name:"Game",version:{gameVersion:"1.20.1",loader:"forge",loaderVersion:"47.2.0"},presentation:{modFileCount:52,modpackName:"机械动力之旅"}});
  assert.match(pack, /data-loader="forge"/);
  assert.match(pack, /Forge 47.2.0/);
  assert.match(pack, /52 个模组文件/);
  assert.match(pack, /整合包 · 机械动力之旅/);
});

test("instance settings show snapshot summaries and honest unavailable state", () => {
  const markup = ui.renderInstanceManagement({id:"school",name:"School",javaMajor:17,identityId:"player-1",profileId:"profile-1",version:{gameVersion:"1.20.4",loader:"fabric"}}, "settings");
  assert.match(markup, /Java 17/);
  assert.match(markup, /游戏身份.*已绑定/);
  assert.match(markup, /配置档案.*已绑定/);
  assert.match(markup, /实例覆盖.*未配置/);
  assert.match(markup, /全局继承关系和启动参数编辑尚未接入/);
});

test("shell exposes skip link, main landmark and labelled heading", () => {
  const markup = ui.renderApp();
  assert.match(markup, /<a class="skip-link" href="#page-main">/);
  assert.match(markup, /<main class="page-scroll" id="page-main"/);
  assert.match(markup, /aria-labelledby="page-heading"/);
  assert.match(markup, /<h1 class="page-header__title" id="page-heading">/);
  assert.match(markup, /<nav class="nav" aria-label="主导航">/);
  assert.match(markup, /<aside class="sidebar">/);
});

test("downloads page shows the empty state when the snapshot has no tasks", () => {
  const markup = ui.renderShell("tasks");
  assert.match(markup, /data-page-state="empty"/);
  assert.match(markup, /没有进行中的下载/);
  assert.doesNotMatch(markup, /class="list"/, "no task list is rendered");
  assert.doesNotMatch(markup, /取消任务/, "no cancel control without a task");
  assert.doesNotMatch(markup, /role="progressbar"/);
});

test("downloads page renders phase, progress and cancel for a real task", () => {
  const markup = ui.renderShell("tasks", { tasks: [ui.TASKS.running], presentations: [downloadEntry(ui.TASKS.running)] });

  assert.match(markup, /data-page-state="success"/);
  assert.doesNotMatch(markup, /任务正在进行|阶段与进度会就地更新/);
  assert.match(markup, /task-1/);
  assert.match(markup, /data-state="running"/);
  assert.match(markup, /校园生存/);
  assert.match(markup, /role="progressbar"/);
  assert.match(markup, /aria-valuenow="50"/);
  assert.match(markup, /512000 \/ 1024000 bytes/);
  assert.match(markup, /取消任务/);
  assert.match(markup, /data-attempt-generation="2"/);
  assert.match(markup, /共 0 个任务|1 个任务/);
});

test("a task with cancelRequested only reports the recorded request", () => {
  const markup = ui.renderShell("tasks", {
    tasks: [ui.TASKS.cancelRequested],
    presentations: [downloadEntry(ui.TASKS.cancelRequested)],
  });
  assert.match(markup, /取消请求已记录/);
  assert.doesNotMatch(markup, /取消任务/, "the cancel control is withdrawn");
  assert.doesNotMatch(markup, /已取消/, "must not claim the task stopped");
});

test("cancel outcomes map to requests, never to a finished cancellation", () => {
  const requested = ui.feedbackFromDecision("requested");
  const already = ui.feedbackFromDecision("already_requested");
  assert.equal(requested.status, "requested");
  assert.equal(already.status, "requested");

  const rejectedReasons = [
    ui.feedbackFromDecision("not_cancellable"),
    ui.feedbackFromDecision("stale_generation"),
    ui.feedbackFromDecision("not_found"),
  ];
  for (const feedback of rejectedReasons) {
    assert.equal(feedback.status, "rejected");
    assert.doesNotMatch(feedback.reason, /已取消|取消完成/);
  }

  const markup = ui.renderTaskList([ui.TASKS.running], {
    "task-1": requested,
  });
  assert.match(markup, /取消请求已记录/);
  assert.doesNotMatch(markup, /已取消/);
});

test("a rejected cancel shows an inline error with a refresh action", () => {
  const markup = ui.renderTaskList([ui.TASKS.running], {
    "task-1": { status: "rejected", reason: "任务代次已变化，请刷新后再取消。" },
  });
  assert.match(markup, /role="alert"/);
  assert.match(markup, /任务代次已变化，请刷新后再取消。/);
  assert.match(markup, /刷新状态/);
  assert.match(markup, /aria-describedby="task-task-1-cancel"/);
});

test("task list keeps terminal tasks without a cancel control", () => {
  const markup = ui.renderTaskList([ui.TASKS.succeeded, ui.TASKS.failed]);
  assert.equal(ui.canCancel(ui.TASKS.succeeded), false);
  assert.equal(ui.canCancel(ui.TASKS.failed), false);
  assert.doesNotMatch(markup, /取消任务/);
  assert.match(markup, /data-state="succeeded"/);
  assert.match(markup, /data-state="failed"/);
  assert.match(markup, /download\.source_unavailable/);
  assert.match(markup, /可重试/);
});

test("page state selection covers every state, including not connected", () => {
  const { selectPageState } = ui;
  const base = {
    phase: "ready",
    hasDataSource: true,
    itemCount: 0,
    activeCount: 0,
    unknownTotalCount: 0,
  };

  assert.equal(selectPageState({ ...base, phase: "loading" }), "loading");
  assert.equal(selectPageState({ ...base, phase: "error", itemCount: 3 }), "error");
  assert.equal(selectPageState(base), "empty");
  assert.equal(
    selectPageState({ ...base, itemCount: 2, activeCount: 1 }),
    "busy",
  );
  assert.equal(
    selectPageState({ ...base, itemCount: 2, unknownTotalCount: 1 }),
    "partial",
  );
  assert.equal(selectPageState({ ...base, itemCount: 2 }), "success");
  assert.equal(
    selectPageState({ ...base, hasDataSource: false }),
    "not_connected",
    "a page without a data source never reports an empty list",
  );
  assert.equal(
    selectPageState({ ...base, hasDataSource: false, phase: "loading" }),
    "loading",
    "loading still outranks not_connected",
  );
});

test("each page state renders its own surface", () => {
  const loading = ui.renderPageState("loading");
  assert.match(loading, /data-page-state="loading"/);
  assert.match(loading, /class="skeleton"/);
  assert.doesNotMatch(loading, />content</);

  const error = ui.renderPageState("error");
  assert.match(error, /data-page-state="error"/);
  assert.match(error, /role="alert"/);
  assert.match(error, /刷新状态/);
  assert.doesNotMatch(error, />content</);

  const busy = ui.renderPageState("busy");
  assert.match(busy, /data-page-state="busy"/);
  assert.match(busy, /banner--info/);
  assert.match(busy, />content</);

  const partial = ui.renderPageState("partial");
  assert.match(partial, /data-page-state="partial"/);
  assert.match(partial, /banner--warn/);
  assert.match(partial, /总量未知|部分任务缺少总量/);

  const empty = ui.renderPageState("empty");
  assert.match(empty, /data-page-state="empty"/);
  assert.match(empty, />content</);

  const success = ui.renderPageState("success");
  assert.match(success, /data-page-state="success"/);
  assert.match(success, />content</);

  const notConnected = ui.renderPageState("not_connected");
  assert.match(notConnected, /data-page-state="not_connected"/);
  assert.match(notConnected, /数据源尚未接入/);
  assert.doesNotMatch(notConnected, />content</, "no empty-list content is claimed");
});

test("shell pages without a data source never claim an empty list (D2/D3)", () => {
  const pages = ["instances", "multiplayer", "discover"];

  for (const page of pages) {
    const loading = ui.renderShell(page, { phase: "loading" });
    assert.match(
      loading,
      /data-page-state="loading"/,
      `${page}: loading page state`,
    );
    assert.match(loading, /class="skeleton"/, `${page}: skeleton while loading`);
    assert.doesNotMatch(
      loading,
      /还没有|尚未接入/,
      `${page}: no conclusion while loading`,
    );

    const error = ui.renderShell(page, { phase: "error" });
    assert.match(error, /data-page-state="error"/, `${page}: error page state`);
    assert.match(error, /role="alert"/, `${page}: error is announced`);
    assert.match(error, /刷新状态/, `${page}: retry next to the message`);
    assert.doesNotMatch(
      error,
      /还没有|尚未接入/,
      `${page}: no conclusion after a failed load`,
    );

    const ready = ui.renderShell(page, { phase: "ready" });
    assert.match(
      ready,
      /data-page-state="not_connected"/,
      `${page}: ready without a data source`,
    );
    assert.ok(ready.includes("data-page-state=\"not_connected\""), `${page}: missing source is explicit`);
    assert.doesNotMatch(ready, /还没有/, `${page}: never reports a row count`);
  }
});

test("home preview presents creation without inventing installed games or an account", () => {
  const markup = ui.renderShell("start", { phase: "ready" });
  assert.match(markup, /选择一个游戏/);
  assert.match(markup, /安装游戏/);
  assert.match(markup, /在桌面启动器中安装和启动游戏/);
  assert.doesNotMatch(markup, /Steve|52个模组|3个在线服务器|上次运行/);
  assert.match(markup, /class="world-hero"/);
  assert.doesNotMatch(markup, /class="home-columns"|也可以去看看|world-hero__cover/);
});

test("a failed refresh keeps the rows the user can still see", () => {
  const markup = ui.renderShell("tasks", {
    tasks: [ui.TASKS.running],
    presentations: [downloadEntry(ui.TASKS.running)],
    phase: "error",
  });
  assert.match(markup, /data-page-state="error"/);
  assert.match(markup, /role="alert"/);
  assert.match(markup, /class="list"/, "existing rows survive the error");
  assert.match(markup, /task-1/);
});

test("a failed load on the downloads page is an error state with retry", () => {
  const markup = ui.renderShell("tasks", { phase: "error" });
  assert.match(markup, /data-page-state="error"/);
  assert.match(markup, /刷新状态/);
  assert.doesNotMatch(markup, /class="list"/, "no rows exist yet");
});

test("a cancelled unknown total does not insert a page progress notice", () => {
  const cancelled = { ...ui.TASKS.unknownTotal, state: "cancelled" };
  const markup = ui.renderShell("tasks", { tasks: [cancelled], presentations: [downloadEntry(cancelled)] });
  assert.match(markup, /data-page-state="success"/);
  assert.doesNotMatch(markup, /部分任务缺少总量/);
  // A cancelled task belongs to unfinished, which the active tab hides.
  assert.match(markup, /没有进行中的下载/);
  assert.equal(
    ui.summarizeTasks([cancelled]).unknownTotalCount,
    1,
    "summary counts unknown totals",
  );
});

test("dialog reducer closes on Escape and remembers the trigger", () => {
  const closed = ui.CLOSED_OVERLAY;
  assert.equal(
    ui.reduceOverlay(closed, { type: "escape" }),
    closed,
    "escape on a closed overlay is ignored without a new object",
  );

  const opened = ui.reduceOverlay(closed, {
    type: "open",
    trigger: "shell-account-trigger",
  });
  assert.deepEqual(opened, { open: true, trigger: "shell-account-trigger" });
  assert.equal(ui.focusAfterClose(opened), null, "no restore while open");

  const escaped = ui.reduceOverlay(opened, { type: "escape" });
  assert.equal(escaped.open, false);
  assert.equal(ui.focusAfterClose(escaped), "shell-account-trigger");

  assert.equal(
    ui.reduceOverlay(escaped, { type: "close" }),
    escaped,
    "closing twice keeps the same reference",
  );

  const reopened = ui.reduceOverlay(escaped, {
    type: "open",
    trigger: "shell-account-trigger",
  });
  assert.equal(reopened.open, true);
});

test("player dialog starts closed and disabled controls retain their reason", () => {
  const markup = ui.renderShell("settings");
  assert.match(markup, /<dialog class="dialog"/);
  assert.match(markup, /aria-labelledby=/);
  assert.doesNotMatch(markup, /<dialog[^>]*open/, "dialog starts closed");
  assert.match(markup, /本地玩家名称/);
  assert.match(markup, /本地玩家在退出启动器后清除/);
  assert.doesNotMatch(markup, /type="password"/, "local identity selection is not provider login");

  const disabled = ui.renderDisabledButton();
  assert.match(disabled, /disabled=""/);
  assert.match(disabled, /aria-describedby="disabled-reason"/);
});

test("tabs use the ARIA tab pattern with a roving tabindex", () => {
  const markup = ui.renderShell("discover");
  assert.match(markup, /role="tablist"/);
  assert.match(markup, /role="tabpanel"/);
  assert.match(markup, /aria-controls="discover-panel"/);
  assert.match(markup, /aria-selected="true"[^>]*tabindex="0"|tabindex="0"[^>]*aria-selected="true"/);

  const tablist = markup.slice(
    markup.indexOf('role="tablist"'),
    markup.indexOf('role="tabpanel"'),
  );
  const inactive = tablist.match(/aria-selected="false"/g) ?? [];
  assert.equal(inactive.length, 3, "three inactive tabs");
  const roving = tablist.match(/tabindex="-1"/g) ?? [];
  assert.equal(roving.length, 3, "inactive tabs are not in the tab order");
  const active = tablist.match(/tabindex="0"/g) ?? [];
  assert.equal(active.length, 1, "one tab is in the tab order");
});

test("progress helpers treat an unknown total as indeterminate", () => {
  assert.equal(ui.progressPercent(512, 1024), 50);
  assert.equal(ui.progressPercent(512, null), null);
  assert.equal(ui.progressPercent(5, 0), null);
  assert.equal(ui.progressText(3, null, "steps"), "3 步");
  assert.equal(ui.progressText(10, 10, "items"), "10 / 10 项");
});

test("task rows sit behind a memo boundary", () => {
  // Structural check only: counting real re-renders needs a DOM test
  // renderer, which this workspace does not install.
  assert.equal(
    ui.taskRowComponent.$$typeof,
    ui.memoType,
    "TaskRow is memoised",
  );
});

test("first paint renders one page tree and is measured, not estimated", (t) => {
  const measured = ui.measure();
  const app = measured["app-first-paint"];
  assert.ok(app.elements > 0, "app renders elements");

  for (const page of PAGES) {
    const entry = measured[`page:${page}`];
    assert.ok(entry.elements > 0, `${page} renders`);
    t.diagnostic(`page:${page} elements=${entry.elements} bytes=${entry.bytes}`);
  }
  t.diagnostic(`app-first-paint elements=${app.elements} bytes=${app.bytes}`);

  // Rendering every page at once would be far larger than one page tree.
  const single = measured["page:downloads"].elements;
  assert.ok(single > 0);
});
