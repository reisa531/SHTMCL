import { createRoot } from "react-dom/client";
import { GameInstallPanel } from "../../../apps/desktop/src/ui/views/GameInstallPanel";
const context = (globalThis as any).__FABRIC_TEST__ = { mutations: [], reads: [], accepted: 0 };
const root = createRoot(document.getElementById("root")!);
const results: { name: string; passed: boolean; error?: string }[] = [];
const delay = (ms = 25) => new Promise(resolve => setTimeout(resolve, ms));
async function until(test: () => unknown) { for (let i = 0; i < 150; i++) { if (test()) return; await delay(); } throw Error("UI timeout"); }
function check(value: unknown, message: string) { if (!value) throw Error(message); }
function button(text: string) { const item = [...document.querySelectorAll<HTMLButtonElement>("button")].find(b => b.textContent?.trim() === text); if (!item) throw Error("Missing button: " + text); return item; }
const submit = () => document.querySelector<HTMLButtonElement>('button[type="submit"]')!;
function choose(version: string) { document.querySelector<HTMLButtonElement>(`[data-version="${version}"]`)!.click(); }
function setCatalogue(mode: string) {
  const store = (globalThis as any).__FABRIC_TEST__;
  if (store) store.fabricMode = mode;
  if (typeof (globalThis as any).__fabricSetMode === "function") (globalThis as any).__fabricSetMode(mode);
}
async function fabric() {
  await until(() => !!document.querySelector('[data-fabric="0.16.10"]'));
  document.querySelector<HTMLElement>('[data-component="fabric"] summary')!.click();
  document.querySelector<HTMLButtonElement>('[data-fabric="0.16.10"]')!.click();
  await until(() => document.getElementById("root")!.textContent?.includes("移除 Fabric"));
}
async function run(name: string, test: () => Promise<void>) { try { await test(); results.push({ name, passed: true }); } catch (error) { results.push({ name, passed: false, error: String(error) }); } document.getElementById("report")!.textContent = JSON.stringify(results); }
async function main() {
  if (new URLSearchParams(location.search).get("preview") === "versions") {
    root.render(<GameInstallPanel onAccepted={() => {}} />);
    await until(() => document.querySelector('[data-version="1.20.4"]'));
    return;
  }
  root.render(<GameInstallPanel onAccepted={() => context.accepted++} />);
  await until(() => !!document.querySelector('[data-version="1.20.4"]'));
  await run("version browsing and component configuration are separate steps", async () => {
    check(!submit(), "installation form appeared before choosing a game");
    choose("1.20.4"); await until(() => !!submit());
    check(!document.querySelector('[data-version]'), "version list still mixed with configuration");
    check(document.activeElement?.textContent?.includes("配置 Minecraft"), "step heading did not receive keyboard focus");
    check(document.querySelectorAll('.component-card--unavailable').length === 3, "unsupported adapters falsely selectable");
  });
  await run("Fabric card submits an explicitly chosen compatible version", async () => {
    await fabric(); check(!submit().disabled, "valid configuration disabled");
    submit().click(); submit().click(); await until(() => context.accepted === 1);
    const request = context.mutations[0] as any;
    check(request.payload.loader === "fabric" && request.payload.loaderVersion === "0.16.10" && request.payload.versionId === "1.20.4", "wrong environment submitted");
    check(context.mutations.length === 1, "double submission");
  });
  await run("return preserves custom name and choosing another game clears old loader", async () => {
    const input = document.querySelector<HTMLInputElement>('.component-setup__identity input')!;
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "校园世界");
    input.dispatchEvent(new Event("input", { bubbles: true })); await delay();
    button("返回版本").click(); await until(() => document.querySelector('[data-version="1.7.10"]')); choose("1.7.10");
    await until(() => document.getElementById("root")!.textContent?.includes("没有适用版本"));
    check(document.querySelector<HTMLInputElement>('.component-setup__identity input')?.value === "校园世界", "custom name lost");
    check(!document.querySelector('[data-fabric]'), "old Fabric choices leaked");
    submit().click(); await until(() => context.accepted === 2);
    const request = context.mutations[1] as any;
    check(!('loader' in request.payload) && !('loaderVersion' in request.payload), "stale Fabric parameters leaked");
  });
  await run("removing Fabric returns to vanilla and clears API dependency card", async () => {
    button("返回版本").click(); await until(() => document.querySelector('[data-version="1.20.4"]')); choose("1.20.4"); await fabric();
    button("移除 Fabric").click(); await until(() => !document.getElementById("root")!.textContent?.includes("Fabric API"));
    check(document.querySelector('.component-setup__footer')?.textContent?.includes("原版"), "summary not reset");
  });
  await run("refresh retains a still-available Fabric selection", async () => {
    root.render(<GameInstallPanel key="refresh" onAccepted={() => context.accepted++} />);
    await until(() => document.querySelector('[data-version="1.20.4"]'));
    // Start with an automatically generated name.
    choose("1.20.4"); await fabric();
    check(!submit().disabled, "valid configuration disabled before refresh");
    setCatalogue("retain");
    button("刷新 Fabric 版本").click();
    await until(() => !!document.querySelector('[role="status"]'));
    await until(() => !!document.querySelector('[data-fabric="0.16.10"]'));
    check(document.getElementById("root")!.textContent?.includes("移除 Fabric"), "selection lost after retained refresh");
    check(document.querySelector('.component-setup__footer')?.textContent?.includes("Fabric 0.16.10"), "loader/version summary not preserved");
    check(!submit().disabled, "retained selection blocks submission");
  });
  await run("refresh drops an absent Fabric selection and resets loader/name", async () => {
    setCatalogue("remove");
    button("刷新 Fabric 版本").click();
    await until(() => !document.getElementById("root")!.textContent?.includes("Fabric 0.16.10"));
    check(document.querySelector('.component-setup__footer')?.textContent?.includes("原版"), "loader not reset to vanilla");
    check(!document.querySelector('[data-fabric="0.16.10"]'), "removed version still present");
    check(document.querySelector<HTMLInputElement>('.component-setup__identity input')?.value === "1.20.4", "generated name not reset");
    check(!submit().disabled, "vanilla fallback blocks submission");
  });
  await run("refresh failure blocks Fabric submission until retry succeeds", async () => {
    // Re-select Fabric then force a failure.
    setCatalogue("retain");
    button("刷新 Fabric 版本").click();
    await until(() => !!document.querySelector('[data-fabric="0.16.10"]')); await fabric();
    setCatalogue("fail");
    button("刷新 Fabric 版本").click();
    await until(() => document.getElementById("root")!.textContent?.includes("获取失败"));
    check(submit().disabled, "submission not blocked during Fabric failure");
    check(document.querySelector('.component-setup__footer')?.textContent?.includes("Fabric 0.16.10"), "selection not preserved on failure");
    // Retry succeeds and unblocks submission.
    setCatalogue("retain");
    button("重试").click();
    await until(() => !!document.querySelector('[data-fabric="0.16.10"]'));
    check(!document.getElementById("root")!.textContent?.includes("获取失败"), "error not cleared after successful retry");
    check(!submit().disabled, "submission still blocked after successful retry");
  });
  await run("invalidating a selection preserves a custom instance name", async () => {
    const input = document.querySelector<HTMLInputElement>('.component-setup__identity input')!;
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "我的世界");
    input.dispatchEvent(new Event("input", { bubbles: true })); await delay();
    setCatalogue("remove"); button("刷新 Fabric 版本").click();
    await until(() => document.querySelector('.component-setup__footer')?.textContent?.includes("原版"));
    check(input.value === "我的世界", "custom name overwritten by catalogue reconciliation");
    setCatalogue("retain");
  });
  await run("cache mode keeps the separate confirmation without advertising loader recipes", async () => {
    root.render(<GameInstallPanel key="cache" mode="cache" onAccepted={() => {}} />);
    await until(() => document.querySelector('[data-version="1.20.4"]')); choose("1.20.4"); await until(() => !!submit());
    check(submit().textContent?.includes("下载到本地"), "wrong cache action");
    check(!document.querySelector('[data-component]'), "unimplemented loader cache recipe advertised");
  });
  root.render(<GameInstallPanel key="preview" onAccepted={() => {}} />);
  await until(() => document.querySelector('[data-version="1.20.4"]')); choose("1.20.4"); await fabric();
  await run("configuration fits viewport without horizontal scrolling", async () => {
    check(document.documentElement.scrollWidth <= window.innerWidth, "configuration overflows viewport");
    const action = submit().getBoundingClientRect();
    check(action.left >= 0 && action.right <= window.innerWidth && action.bottom <= window.innerHeight, "primary action outside viewport");
  });
  document.body.dataset.testStatus = results.every(item => item.passed) ? "passed" : "failed";
}
main().catch(error => { document.body.dataset.testStatus = "failed"; document.getElementById("report")!.textContent = String(error); });
