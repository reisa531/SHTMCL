import { createRoot } from "react-dom/client";
import { RetryCachedDownload, canRetryCachedDownload } from "../../../apps/desktop/src/ui/views/RetryCachedDownload";
import { DownloadsPage } from "../../../apps/desktop/src/ui/pages/DownloadsPage";
import type { Task } from "../../../apps/desktop/src/contract";
import type { DownloadPresentation } from "../../../apps/desktop/src/ui/views/download-presentation";
const root = createRoot(document.getElementById("root")!);
const wait = () => new Promise((resolve) => setTimeout(resolve, 60));
const checks: {label: string; passed: boolean}[] = [];
function check(ok: unknown, label: string) { checks.push({label, passed: Boolean(ok)}); if (!ok) throw Error(label); }
const entry: DownloadPresentation = {operationId: "old-op", taskId: "old-task", name: "Minecraft", version: "1.20.4",
  resourceType: "game_environment", destination: {kind: "cache"}, history: {state: "cancelled", phase: "download_files", attemptGeneration: 1}};
const task: Task = {id: entry.taskId, operationId: entry.operationId, state: "cancelled", phase: "download_files", attemptGeneration: 1, cancelRequested: true};
const calls: any[] = [];
let accept: () => void;
let reject: (failure: unknown) => void;
let acceptedCount = 0;
const onAccepted = () => { acceptedCount++; };
(window as any).__TAURI_INTERNALS__ = {};
(window as any).__routeInvoke = (_command: string, {request}: any) => {
  calls.push(request);
  return new Promise((resolve, fail) => {
    accept = () => resolve({type: "accepted", operationId: request.operationId, taskId: "new-task"}); reject = fail;
  });
};
const retry = () => [...document.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "重新下载")!;
async function render(value = entry) { root.render(<RetryCachedDownload entry={value} onAccepted={onAccepted} />); await wait(); }
async function reset() { root.render(<></>); await wait(); }
(async () => { try {
  check(canRetryCachedDownload(entry, task), "live cancelled cache download can retry");
  check(canRetryCachedDownload(entry, {...task, state: "failed"}), "live failed cache download can retry");
  for (const state of ["running", "queued", "paused", "waiting_user", "succeeded"] as const) {
    check(!canRetryCachedDownload(entry, {...task, state}), "no live retry: " + state);
  }
  check(!canRetryCachedDownload(entry, {...task, operationId: "other"}), "reject mismatched live identity");
  for (const state of ["cancelled", "failed"] as const) check(canRetryCachedDownload({...entry, history: {...entry.history!, state}}), "archived retry: " + state);
  check(canRetryCachedDownload({...entry, history: {...entry.history!, state: "running", interrupted: true}}), "interrupted prior session can retry");
  for (const history of [undefined, {...entry.history!, state: "running" as const}, {...entry.history!, state: "succeeded" as const},
    {...entry.history!, state: "paused" as const, interrupted: true}, {...entry.history!, activeElsewhere: true}]) {
    check(!canRetryCachedDownload({...entry, history}), "no unsafe archive retry: " + JSON.stringify(history));
  }
  check(!canRetryCachedDownload({...entry, version: undefined}), "version required");
  check(!canRetryCachedDownload({...entry, destination: {kind: "instance", instanceId: "game"}}), "never recreate instance as vanilla");
  check(!canRetryCachedDownload({...entry, resourceType: "mod"}), "no unsupported resource retry");
  await render(); retry().click(); retry()?.click(); await wait();
  check(calls.length === 1, "two rapid clicks dispatch once");
  check(!!document.querySelector('button[disabled][aria-busy="true"]'), "pending state disables submission");
  check(acceptedCount === 0, "pending request does not announce accepted");
  check(calls[0].payload.command === "launch.environments.download" && calls[0].payload.versionId === entry.version, "real API uses cache command and version");
  check(calls[0].operationId !== entry.operationId, "new attempt has fresh operation identity");
  reject!({code: "launch.install.busy"}); await wait();
  check(document.body.textContent?.includes("正在处理另一个任务"), "busy feedback shown");
  check(!retry().disabled && acceptedCount === 0, "rejected submission can retry without success callback");
  retry().click(); await wait(); check(calls[1].operationId !== calls[0].operationId, "manual retry gets a new operation");
  accept!(); await wait(); check(acceptedCount === 1, "accepted invokes callback once");
  check(!!document.querySelector('button[disabled]') && document.body.textContent?.includes("已加入下载"), "accepted old row cannot submit again");
  await reset(); await render(); retry().click(); await wait(); const staleAccept = accept!;
  await render({...entry, operationId: "replacement"}); staleAccept(); await wait();
  check(acceptedCount === 1 && !retry().disabled, "record replacement ignores stale response");
  retry().click(); await wait(); const unmountedAccept = accept!; await reset(); unmountedAccept(); await wait();
  check(acceptedCount === 1, "unmounted row ignores late acceptance");
  await render(); retry().click(); await wait(); reject!(Error("offline")); await wait();
  check(document.body.textContent?.includes("未能开始下载，请重试"), "network error offers retry");
  await reset();
  for (const archived of [false, true]) {
    root.render(<DownloadsPage tasks={archived ? [] : [task]} presentations={[entry]} hasSnapshot phase="ready" refreshing={false}
      feedback={{}} pendingCancel={new Set()} onCancel={() => {}} onRefresh={() => {}} onAccepted={onAccepted} />);
    await wait(); [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((b) => b.textContent?.startsWith("未完成"))!.click(); await wait();
    check(!!retry(), (archived ? "history" : "live") + " unfinished page has retry action");
    retry().click(); await wait(); accept!(); await wait();
    check(document.querySelector('[role="tab"][aria-selected="true"]')?.textContent?.startsWith("进行中"), "accepted retry returns to active tab");
    await reset();
  }
  check(acceptedCount === 3, "page acceptance forwarded for live and archived rows");
  document.body.dataset.testStatus = "passed";
} catch (error) {checks.push({label: String(error), passed: false}); document.body.dataset.testStatus = "failed";}
  finally {document.getElementById("report")!.textContent = JSON.stringify(checks);} })();
