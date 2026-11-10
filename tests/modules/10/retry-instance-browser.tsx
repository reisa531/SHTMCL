import { createRoot } from "react-dom/client";
import { RetryInstanceInstall } from "../../../apps/desktop/src/ui/views/RetryCachedDownload";
import { DownloadsPage } from "../../../apps/desktop/src/ui/pages/DownloadsPage";
import type { DownloadPresentation } from "../../../apps/desktop/src/ui/views/download-presentation";
import type { Task } from "../../../apps/desktop/src/contract";
const root = createRoot(document.getElementById("root")!);
const wait = () => new Promise((resolve) => setTimeout(resolve, 60));
const checks: {label: string; passed: boolean}[] = [];
function check(value: unknown, label: string) { checks.push({label, passed: Boolean(value)}); if (!value) throw Error(label); }
const entry: DownloadPresentation = {operationId: "old-op", taskId: "old-task", name: "Minecraft", version: "1.20.4",
  resourceType: "game_environment", destination: {kind: "instance", instanceId: "school"}, canRetryInstall: true,
  history: {state: "cancelled", phase: "download_files", attemptGeneration: 1}};
const task: Task = {id: entry.taskId, operationId: entry.operationId, state: "cancelled", phase: "download_files", attemptGeneration: 1, cancelRequested: true};
const calls: any[] = [];
let resolve: (value: unknown) => void;
let reject: (value: unknown) => void;
let accepted = 0;
(window as any).__TAURI_INTERNALS__ = {};
(window as any).__routeInvoke = (command: string, args: any) => {
  check(command === "dispatch_core_mutation", "uses native mutation bridge"); calls.push(args.request);
  return new Promise((yes, no) => { resolve = yes; reject = no; });
};
const button = () => [...document.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "重新安装");
const finish = () => resolve!({type: "accepted", operationId: calls.at(-1).operationId, taskId: "replacement"});
async function render(value = entry) { root.render(<RetryInstanceInstall entry={value} onAccepted={() => accepted++} />); await wait(); }
async function reset() {root.render(<></>); await wait();}
(async () => {try {
  await render({...entry, canRetryInstall: undefined}); check(!button(), "legacy record has no inferred installation");
  await render(); button()!.click(); button()?.click(); await wait();
  check(calls.length === 1 && accepted === 0, "rapid clicks submit once without optimistic acceptance");
  check(JSON.stringify(calls[0].payload) === JSON.stringify({command: "launch.installations.retry", sourceOperationId: "old-op", sourceTaskId: "old-task"}), "payload contains identity only");
  check(calls[0].operationId !== entry.operationId, "fresh operation identity");
  reject!({code: "launch.install.target_exists"}); await wait();
  check(document.body.textContent?.includes("实例已存在"), "existing instance feedback");
  button()!.click(); await wait(); reject!({code: "launch.install.busy"}); await wait();
  check(document.body.textContent?.includes("正在处理另一个任务"), "busy feedback");
  button()!.click(); await wait(); reject!({code: "launch.install.recovery_unavailable"}); await wait();
  check(document.body.textContent?.includes("请重新选择版本和组件"), "missing recovery data has next action");
  button()!.click(); await wait(); finish(); await wait();
  check(accepted === 1 && document.querySelector('button[disabled]'), "accepted once and row disabled");
  await reset(); await render(); button()!.click(); await wait(); const late = resolve!;
  await render({...entry, operationId: "other-op"}); late({type: "accepted", operationId: calls.at(-1).operationId, taskId: "replacement"}); await wait();
  check(accepted === 1 && button() && !button()!.disabled, "replaced row ignores late result");
  await reset();
  for (const archived of [false, true]) {
    root.render(<DownloadsPage tasks={archived ? [] : [task]} presentations={[entry]} hasSnapshot phase="ready" refreshing={false}
      feedback={{}} pendingCancel={new Set()} onCancel={() => {}} onRefresh={() => {}} onAccepted={() => accepted++} />);
    await wait(); [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((b) => b.textContent?.startsWith("未完成"))!.click(); await wait();
    check(button(), archived ? "history recovery action" : "live recovery action");
    button()!.click(); await wait(); finish(); await wait();
    check(document.querySelector('[role="tab"][aria-selected="true"]')?.textContent?.startsWith("进行中"), "accepted installation selects active tab");
    await reset();
  }
  check(accepted === 3, "page forwards both accepted requests");
  document.body.dataset.testStatus = "passed";
} catch (error) { checks.push({label: String(error), passed: false}); document.body.dataset.testStatus = "failed"; }
finally {document.getElementById("report")!.textContent = JSON.stringify(checks);} })();
