import { createRoot } from "react-dom/client";
import { App } from "../../../apps/desktop/src/App";

const ctx = (globalThis as any).__STARTUP_TEST__ = {
  identity: null, games: [], tasks: [], mutations: [], snapshots: 0, cancellations: [],
  listener: null, snapshotFailure: false, launchFailure: false, omitTask: false,
  selectGate: null,
};
const root = createRoot(document.getElementById("root")!);
const results: { name: string; passed: boolean; error?: string }[] = [];
const delay = (ms = 25) => new Promise((resolve) => setTimeout(resolve, ms));
async function until(test: () => unknown) {
  for (let i = 0; i < 120; i++) { if (test()) return; await delay(); }
  throw Error("UI timeout: " + document.querySelector('[data-launch-status]')?.textContent);
}
function check(value: unknown, message: string) { if (!value) throw Error(message); }
function button(label: string, scope: ParentNode = document) {
  const found = [...scope.querySelectorAll<HTMLButtonElement>("button")].find((item) => item.textContent?.trim() === label);
  if (!found) throw Error("Missing button: " + label);
  return found;
}
function homeLaunch() { return document.querySelector<HTMLButtonElement>(".world-hero__actions button")!; }
function status() { return document.querySelector<HTMLElement>("[data-launch-status]"); }
function nav(page: string) { document.querySelector<HTMLButtonElement>(`[data-page="${page}"]`)!.click(); }
function dialog() { return document.querySelector<HTMLDialogElement>("dialog[open]"); }
async function selectPlayer() {
  const input = dialog()!.querySelector<HTMLInputElement>("input")!;
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "Steve");
  input.dispatchEvent(new Event("input", { bubbles: true }));
  await until(() => !button("使用此玩家", dialog()!).disabled);
  button("使用此玩家", dialog()!).click();
}
let key = 0;
async function reset(identity: boolean = false) {
  root.render(<div />); await delay();
  Object.assign(ctx, { identity: identity ? { kind: "offline", username: "Steve", uuid: "a".repeat(32) } : null,
    games: [{ id: "campus", name: "校园世界", version: { gameVersion: "1.20.4", loader: "vanilla" } }],
    tasks: [], mutations: [], snapshots: 0, cancellations: [], listener: null,
    snapshotFailure: false, launchFailure: false, omitTask: false, selectGate: null });
  localStorage.setItem("shtmcl.shell.page.v1", "start");
  root.render(<App key={++key} />);
  await until(() => document.querySelector(".world-hero__title")?.textContent === "校园世界" && ctx.listener);
}
function emit(state: string, phase = "runtime", error?: object) {
  const task = ctx.tasks[0];
  Object.assign(task, { state, phase, ...(error ? { error } : {}) });
  const sequence = (ctx.sequence = (ctx.sequence ?? 0) + 1);
  ctx.listener({ eventId: "event-" + sequence, schemaVersion: 1, operationId: task.operationId,
    entityId: "campus", revision: 1, sequence, attemptGeneration: 1,
    kind: state === "failed" ? "task.result" : "task.state",
    payload: { taskId: task.id, state, phase, cancelRequested: task.cancelRequested, ...(error ? { error } : {}) } });
}
async function run(name: string, test: () => Promise<void>) {
  try { ctx.sequence = 0; await test(); results.push({ name, passed: true }); }
  catch (error) { results.push({ name, passed: false, error: String(error) }); }
  document.getElementById("report")!.textContent = JSON.stringify(results);
}
async function main() {
  await run("home resumes original target after player selection and deduplicates clicks", async () => {
    await reset(); homeLaunch().click(); homeLaunch().click();
    await until(dialog); check(ctx.mutations.length === 0, "launched before player selection");
    await selectPlayer(); await until(() => ctx.mutations.length === 1 && status()?.dataset.launchStatus === "running");
    check(ctx.mutations[0].payload.instanceId === "campus", "lost original target");
    nav("instances"); await until(() => document.querySelector(".instance-card"));
    check(button("正在准备").disabled, "navigation lost busy state");
    const count = ctx.snapshots; emit("running", "files");
    await until(() => status()?.textContent?.includes("检查游戏文件")); await delay(900);
    check(ctx.snapshots === count, "page still polls snapshots");
    emit("failed", "files", { code: "launch.files.missing", category: "validation", retryable: true });
    await until(() => status()?.textContent?.includes("游戏文件不完整"));
    check(!button("启动").disabled, "failure did not unlock retry");
  });
  await run("instance page resumes and cancellation waits for terminal event", async () => {
    await reset(); nav("instances"); await until(() => document.querySelector(".instance-card"));
    button("启动").click(); await until(dialog); await selectPlayer();
    await until(() => status()?.dataset.launchStatus === "running");
    button("取消启动").click(); await until(() => ctx.cancellations.length === 1);
    check(status()?.dataset.launchStatus === "running", "request was mistaken for cancellation");
    await until(() => status()?.textContent?.includes("正在取消启动"));
    emit("cancelled"); await until(() => status()?.textContent?.includes("已取消启动"));
    check(!status()?.textContent?.includes("正在取消启动"), "pending cancellation hid terminal state");
  });
  await run("dismissing player dialog discards launch intent", async () => {
    await reset(); homeLaunch().click(); await until(dialog);
    dialog()!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })); await until(() => !dialog());
    document.getElementById("shell-account-trigger")!.click(); await until(dialog); await selectPlayer(); await until(() => !dialog());
    check(ctx.mutations.length === 0, "cancelled launch resumed on later account change");
    check(!status(), "cancelled intent remained visible");
  });
  for (const mode of ["changed", "deleted"]) await run(`${mode} instance is rechecked before resuming`, async () => {
    await reset(); homeLaunch().click(); await until(dialog);
    if (mode === "deleted") ctx.games = []; else ctx.games[0].version.gameVersion = "1.21";
    await selectPlayer(); await until(() => status()?.dataset.launchStatus === "error");
    check(ctx.mutations.length === 0, "stale instance launched");
    check(status()?.textContent?.includes("实例已变更"), "no recovery explanation");
  });
  await run("snapshot failure preserves accepted task and explicit refresh recovers", async () => {
    await reset(true); ctx.snapshotFailure = true; homeLaunch().click();
    await until(() => status()?.dataset.launchStatus === "accepted");
    check(homeLaunch().disabled, "lost launch ownership on bridge failure");
    ctx.snapshotFailure = false; button("刷新进度").click(); await until(() => status()?.dataset.launchStatus === "running");
    emit("succeeded", "spawn"); await until(() => status()?.textContent?.includes("游戏进程已启动"));
    check(!status()?.textContent?.includes("主菜单"), "process start overclaimed playable game");
  });
  await run("missing task stays unresolved until refreshed, without duplicate launch", async () => {
    await reset(true); ctx.omitTask = true; homeLaunch().click(); await until(() => status()?.dataset.launchStatus === "accepted");
    check(homeLaunch().disabled, "missing task unlocked launch");
    ctx.omitTask = false; button("刷新进度").click(); await until(() => status()?.dataset.launchStatus === "running");
    check(ctx.mutations.length === 1, "refresh submitted a second launch");
  });
  await run("failed launch command can be retried", async () => {
    await reset(true); ctx.launchFailure = true; homeLaunch().click(); await until(() => status()?.dataset.launchStatus === "error");
    ctx.launchFailure = false; homeLaunch().click(); await until(() => status()?.dataset.launchStatus === "running");
    check(ctx.mutations.length === 2, "retry was lost or duplicated");
  });
  await run("closing during player selection prevents late automatic launch", async () => {
    await reset(); homeLaunch().click(); await until(dialog);
    let release: () => void = () => {}; ctx.selectGate = new Promise<void>((resolve) => { release = resolve; });
    await selectPlayer(); await until(() => !!dialog()?.querySelector("input:disabled"));
    dialog()!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })); await until(() => !dialog());
    release(); await delay(100); check(ctx.mutations.length === 0, "late player response launched a cancelled intent");
  });
  root.unmount();
  document.body.dataset.testStatus = results.every((item) => item.passed) ? "passed" : "failed";
}
main().catch((error) => { document.body.dataset.testStatus = "failed"; document.getElementById("report")!.textContent = String(error); });
