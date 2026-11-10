import { createRoot } from "react-dom/client";
import { TaskList } from "../../../apps/desktop/src/ui/views/TaskList";
import { DownloadHistoryList } from "../../../apps/desktop/src/ui/views/DownloadHistoryList";
import { InstancesPage } from "../../../apps/desktop/src/ui/pages/InstancesPage";
import { DownloadsPage } from "../../../apps/desktop/src/ui/pages/DownloadsPage";
import { App } from "../../../apps/desktop/src/App";
import type { Task } from "../../../apps/desktop/src/contract";
import type { DownloadPresentation } from "../../../apps/desktop/src/ui/views/download-presentation";

const root = createRoot(document.getElementById("root")!);
const results: {label: string; passed: boolean}[] = [];
const wait = () => new Promise((resolve) => setTimeout(resolve, 60));
function check(value: unknown, label: string) {
  results.push({label, passed: Boolean(value)}); if (!value) throw Error(label);
}
const buttons = () => [...document.querySelectorAll<HTMLButtonElement>("button")];
const view = () => buttons().find((button) => button.textContent === "查看实例");
const entry: DownloadPresentation = {operationId: "op", taskId: "task", name: "Minecraft", resourceType: "game_environment",
  destination: {kind: "instance", instanceId: "target", instanceName: "同名"}};
const task: Task = {id: "task", operationId: "op", attemptGeneration: 1, state: "succeeded", phase: "commit", cancelRequested: false};
let called = "";
const callback = (id: string) => {called = id;};
async function row(state: Task["state"], destination = entry.destination, onViewInstance = callback) {
  root.render(<TaskList tasks={[{...task, state}]} presentations={[{...entry, destination}]} feedback={{}} pendingCancel={new Set()}
    onCancel={() => {}} onRefresh={() => {}} onViewInstance={onViewInstance} />); await wait();
}
const games = [{id: "other", name: "同名", version: {gameVersion: "1.20.4", loader: "vanilla"}},
  {id: "target", name: "同名", version: {gameVersion: "1.20.4", loader: "fabric"}}];
let resolveList: (response: unknown) => void;
let rejectList: (reason: unknown) => void;
let pendingList: Promise<unknown>;
function deferList() { pendingList = new Promise((resolve, reject) => {resolveList = resolve; rejectList = reject;}); }
const native = window as any;
native.__TAURI_INTERNALS__ = {};
native.__routeInvoke = async (command: string, args: any) => {
  if (command === "dispatch_core_read") {
    const name = args.request.payload.command;
    if (name === "instances.list") return pendingList;
    if (name === "launch.installations.list") return {type: "completed", result: {entries: [entry]}};
    if (name === "accounts.local.current") return {type: "completed", result: {identity: null}};
    return {type: "completed", result: {}};
  }
  if (command === "get_shell_snapshot") return {contractVersion: 1, mode: "native", modules: ["instances", "launch"], tasks: [task]};
  if (command === "app_info") return {name: "SHTMCL", version: "test", native: true, platform: "windows", arch: "x86_64", contractVersion: 1};
  if (command === "read_events") return {events: [], cursor: null};
  return null;
};
async function instances(targetInstanceId: string, key: string) {
  root.render(<InstancesPage key={key} connected phase="ready" retrying={false} onRetry={() => {}} targetInstanceId={targetInstanceId} />);
  await wait();
}
(async () => { try {
  await row("succeeded"); view()!.click(); check(called === "target", "live success passes exact ID");
  await row("succeeded", entry.destination, (id) => {called = "new:" + id;});
  view()!.click(); check(called === "new:target", "memo uses current callback");
  for (const state of ["running", "failed", "cancelled"] as const) {
    await row(state); check(!view(), "no route before successful commit: " + state);
  }
  for (const destination of [{kind: "cache" as const, instanceId: "target"}, {kind: "instance" as const}, {kind: "instance" as const, instanceId: ""}]) {
    await row("succeeded", destination); check(!view(), "no route for cache or absent ID: " + JSON.stringify(destination));
  }
  root.render(<DownloadHistoryList entries={[{...entry, history: {state: "succeeded", phase: "commit", attemptGeneration: 1}}]} tasks={[]} group="completed" onViewInstance={callback} />);
  await wait(); view()!.click(); check(called === "target", "archived success passes exact ID");
  root.render(<DownloadHistoryList entries={[{...entry, history: {state: "cancelled", phase: "download", attemptGeneration: 1}}]} tasks={[]} group="unfinished" onViewInstance={callback} />);
  await wait(); check(!view(), "archived cancellation has no route");
  root.render(<DownloadsPage tasks={[task]} presentations={[entry]} hasSnapshot phase="ready" refreshing={false} feedback={{}} pendingCancel={new Set()} onCancel={() => {}} onRefresh={() => {}} onViewInstance={callback} />);
  await wait(); buttons().find((b) => b.textContent?.startsWith("已完成"))!.click(); await wait();
  called = ""; view()!.click(); check(called === "target", "page forwards callback");
  deferList(); await instances("target", "loaded");
  check(!document.body.textContent?.includes("实例已移除"), "loading does not announce missing instance");
  resolveList!({type: "completed", result: {instances: games}}); await wait();
  const selected = document.querySelector<HTMLElement>('[aria-current="true"]');
  check(selected?.textContent?.includes("Fabric"), "exact ID selects correct same-name instance");
  check(document.activeElement === selected, "destination card receives focus");
  check(document.querySelectorAll('[aria-current="true"]').length === 1, "only one target");
  deferList(); await instances("removed", "missing"); resolveList!({type: "completed", result: {instances: games}}); await wait();
  check(!document.querySelector('[aria-current="true"]'), "missing ID never selects same-name game");
  check(document.body.textContent?.includes("实例已移除或暂不可用"), "missing instance status");
  deferList(); await instances("target", "failure"); rejectList!(Error("fixture offline")); await wait();
  check(!document.body.textContent?.includes("实例已移除"), "read failure is not reported as deleted");
  check(!!document.querySelector('[data-page-state="error"]'), "read failure can be retried");
  // Exercise real App composition, including a creation draft left open.
  root.render(<></>); await wait(); deferList();
  localStorage.setItem("shtmcl.shell.page.v1", "instances");
  root.render(<App />); await wait(); await wait();
  resolveList!({type: "completed", result: {instances: games}}); await wait();
  buttons().find((b) => b.textContent === "创建实例")!.click(); await wait();
  [...document.querySelectorAll<HTMLElement>(".sidebar button")].find((b) => b.textContent?.includes("下载"))!.click();
  await wait(); buttons().find((b) => b.textContent?.startsWith("已完成"))!.click(); await wait();
  view()!.click(); await wait(); await wait();
  check(!document.querySelector('[aria-current="true"]')?.closest("[hidden]"), "App route exits creation and shows target");
  check(document.activeElement?.getAttribute("aria-current") === "true", "App route focuses loaded target");
  check(document.activeElement?.textContent?.includes("Fabric"), "App chooses exact ID among equal names");
  document.querySelector<HTMLButtonElement>('.sidebar [data-page="instances"]')!.click(); await wait();
  check(!document.querySelector('.instance-card[aria-current="true"]'), "ordinary navigation clears route target");
  root.render(<></>); await wait(); document.body.dataset.testStatus = "passed";
} catch (error) {
  results.push({label: String(error), passed: false}); document.body.dataset.testStatus = "failed";
} finally {document.getElementById("report")!.textContent = JSON.stringify(results);} })();
