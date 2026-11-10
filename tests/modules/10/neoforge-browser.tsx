import { createRoot } from "react-dom/client";
import { GameInstallPanel } from "../../../apps/desktop/src/ui/views/GameInstallPanel";
const context = (globalThis as any).__FABRIC_TEST__ = { mutations: [], reads: [], accepted: 0 };
const root = createRoot(document.getElementById("root")!);
const results: { name: string; passed: boolean; error?: string }[] = [];
const delay = (ms = 25) => new Promise(resolve => setTimeout(resolve, ms));
async function until(test: () => unknown) { for (let i = 0; i < 200; i++) { if (test()) return; await delay(); } throw Error("UI timeout: " + footer() + " step=" + (globalThis as any).__neoStep); }
function check(value: unknown, message: string) { if (!value) throw Error(message); }
function button(text: string) { const item = [...document.querySelectorAll<HTMLButtonElement>("button")].find(b => b.textContent?.trim() === text); if (!item) throw Error("Missing button: " + text); return item; }
const submit = () => document.querySelector<HTMLButtonElement>('button[type="submit"]')!;
const footer = () => document.querySelector('.component-setup__footer')?.textContent ?? "";
function choose(version: string) { document.querySelector<HTMLButtonElement>(`[data-version="${version}"]`)!.click(); }
function setCatalogue(mode: string) {
  const store = (globalThis as any).__FABRIC_TEST__;
  if (store) store.fabricMode = mode;
  if (typeof (globalThis as any).__fabricSetMode === "function") (globalThis as any).__fabricSetMode(mode);
}
function openFabric() { const card = document.querySelector<HTMLDetailsElement>('[data-component="fabric"]')!; if (!card.open) card.querySelector<HTMLElement>('summary')!.click(); }
async function pickFabric() {
  await until(() => !!document.querySelector('[data-fabric="0.16.10"]'));
  openFabric();
  document.querySelector<HTMLButtonElement>('[data-fabric="0.16.10"]')!.click();
  await until(() => footer().includes("Fabric 0.16.10"));
}
function pickNeo() {
  const card = document.querySelector<HTMLDetailsElement>('[data-component="neoforge"]');
  if (!card) throw Error("Missing NeoForge card");
  if (!card.open) card.querySelector<HTMLElement>("summary")!.click();
  document.querySelector<HTMLButtonElement>('[data-neoforge="20.4.253"]')!.click();
}
function setCustomName(value: string) {
  const input = document.querySelector<HTMLInputElement>('.component-setup__identity input')!;
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}
function mount(key: string, onAccepted: () => void, mode: "instance" | "cache" = "instance") {
  root.render(<GameInstallPanel key={key} mode={mode} onAccepted={onAccepted} />);
}
async function startAt(version: string) {
  await until(() => !!document.querySelector(`[data-version="${version}"]`));
  choose(version);
  await until(() => !!submit());
}
async function run(name: string, test: () => Promise<void>) { try { await test(); results.push({ name, passed: true }); } catch (error) { results.push({ name, passed: false, error: String(error) }); } document.getElementById("report")!.textContent = JSON.stringify(results); }
async function main() {
  mount("start", () => context.accepted++);
  await run("NeoForge is offered only on the 1.20.4 pilot and selecting it clears Fabric", async () => {
    await startAt("1.20.4");
    await until(() => !!document.querySelector('[data-component="neoforge"]'));
    check(!document.querySelector('[data-neoforge]') || !!document.querySelector('[data-neoforge="20.4.253"]'), "unexpected NeoForge versions");
    await pickFabric();
    check(footer().includes("Fabric 0.16.10"), "Fabric not selected first");
    pickNeo();
    await until(() => footer().includes("NeoForge 20.4.253"));
    check(!document.getElementById("root")!.textContent?.includes("移除 Fabric"), "Fabric selection not cleared by NeoForge");
    check(!document.querySelector('[data-fabric][aria-pressed="true"]'), "Fabric option still pressed after NeoForge");
    check(document.querySelector('[data-neoforge="20.4.253"]')?.getAttribute("aria-pressed") === "true", "NeoForge option not pressed");
    const input = document.querySelector<HTMLInputElement>('.component-setup__identity input')!;
    check(input.value === "1.20.4-NeoForge 20.4.253", "generated name not updated for NeoForge: " + input.value);
    const art = document.querySelector('.component-setup__identity [data-loader], .component-setup__identity svg, .component-setup__identity img');
    check(!!art, "identity artwork missing");
  });
  await run("Choosing Fabric switches back from NeoForge and regenerates the name", async () => {
    await pickFabric();
    await until(() => footer().includes("Fabric 0.16.10"));
    check(!footer().includes("NeoForge"), "NeoForge summary still present");
    check(document.querySelector('[data-neoforge="20.4.253"]')?.getAttribute("aria-pressed") !== "true", "NeoForge still pressed after Fabric");
    const input = document.querySelector<HTMLInputElement>('.component-setup__identity input')!;
    check(input.value === "1.20.4-Fabric 0.16.10", "generated name not restored for Fabric: " + input.value);
  });
  await run("Selecting NeoForge during a Fabric refresh is not overridden by the late response", async () => {
    setCatalogue("retain");
    openFabric();
    const refresh = [...document.querySelectorAll<HTMLButtonElement>("button")].find(b => b.textContent?.trim() === "刷新 Fabric 版本");
    if (refresh) refresh.click();
    pickNeo();
    await until(() => footer().includes("NeoForge 20.4.253"));
    await delay(150);
    check(footer().includes("NeoForge 20.4.253"), "late Fabric response overrode NeoForge summary");
    check(!footer().includes("Fabric 0.16.10"), "late Fabric response reintroduced Fabric");
    check(!document.getElementById("root")!.textContent?.includes("移除 Fabric"), "late Fabric response re-selected Fabric");
    check(document.querySelector('[data-neoforge="20.4.253"]')?.getAttribute("aria-pressed") === "true", "NeoForge selection lost after late response");
    check(!submit().disabled, "NeoForge configuration blocked");
  });
  await run("Custom name is preserved across NeoForge and Fabric switches", async () => {
    setCustomName("我的整合");
    await delay();
    (globalThis as any).__neoStep="remove"; button("移除 NeoForge").click();
    await until(() => footer().includes("原版"));
    const input = document.querySelector<HTMLInputElement>('.component-setup__identity input')!;
    check(input.value === "我的整合", "custom name lost switching to vanilla/NeoForge"); (globalThis as any).__neoStep="reenable";
    pickNeo();
    await until(() => footer().includes("NeoForge 20.4.253"));
    check(input.value === "我的整合", "custom name lost when enabling NeoForge"); (globalThis as any).__neoStep="fabric";
    await pickFabric();
    check(input.value === "我的整合", "custom name lost switching to Fabric"); (globalThis as any).__neoStep="neo-again";
    pickNeo();
    await until(() => footer().includes("NeoForge 20.4.253"));
    check(input.value === "我的整合", "custom name lost switching back to NeoForge");
  });
  await run("Submitting NeoForge produces a neoforge install payload for 1.20.4", async () => {
    await until(() => footer().includes("NeoForge 20.4.253"));
    const before = context.mutations.length;
    submit().click(); submit().click();
    await until(() => context.mutations.length > before);
    await delay();
    check(context.mutations.length === before + 1, "double NeoForge submission");
    const request = context.mutations[before] as any;
    check(request.payload.loader === "neoforge", "loader not neoforge: " + request.payload.loader);
    check(request.payload.loaderVersion === "20.4.253", "wrong NeoForge version: " + request.payload.loaderVersion);
    check(request.payload.versionId === "1.20.4", "wrong game version: " + request.payload.versionId);
    check(!request.payload.fabricVersion, "stale Fabric field leaked into NeoForge payload");
  });
  await run("NeoForge option is absent for 1.7.10", async () => {
    mount("legacy", () => context.accepted++);
    await startAt("1.7.10");
    await until(() => !!document.querySelector('[data-component="fabric"]'));
    check(!document.querySelector('[data-neoforge]'), "NeoForge option shown for 1.7.10");
    const hidden = document.querySelector('[data-component="neoforge"]');
    check(!hidden, "NeoForge card rendered for 1.7.10");
  });
  await run("Cache mode advertises no NeoForge selector", async () => {
    mount("cache", () => {}, "cache");
    await startAt("1.20.4");
    await until(() => !!document.querySelector('[data-version]') === false || !!submit());
    check(!document.querySelector('[data-component="neoforge"]'), "NeoForge card rendered in cache mode");
    check(!document.querySelector('[data-neoforge]'), "NeoForge option rendered in cache mode");
    check(submit().textContent?.includes("下载到本地"), "wrong cache action");
  });
  document.body.dataset.testStatus = results.every(item => item.passed) ? "passed" : "failed";
}
main().catch(error => { document.body.dataset.testStatus = "failed"; document.getElementById("report")!.textContent = String(error); });
