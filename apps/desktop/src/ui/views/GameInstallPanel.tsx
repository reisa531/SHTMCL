import { useEffect, useRef, useState } from "react";
import { readShellSnapshot } from "../../bridge";
import { Button } from "../components/Button";
import { Panel } from "../components/Panel";
import { InstanceArtwork } from "../components/InstanceArtwork";
import { ComponentPicker } from "./ComponentPicker";
import type { MotionPoint } from "../components/motion";
import { environmentApi } from "./environment-api";
import { fabricApi, type FabricVersion } from "./fabric-api";
import { gameInstallApi, installError, isKnownVersionKind, VERSION_KINDS, type VersionPage } from "./game-install-api";
import {
  NEOFORGE_PILOT_VERSION, VERSION_FILTERS, groupVersionPage, kindForFilter, neoAvailableFor, type VersionFilter, type VersionGroupSummary,
} from "./install-design";

const api = gameInstallApi();

type LoaderKind = "vanilla" | "fabric" | "neoforge";

// Capture scroll offsets for every scrollable ancestor from the focus target up
// to document.scrollingElement, so the shared parent main scroll is restored too.
function scrollAncestors(node: HTMLElement | null): Array<{ el: Element; top: number; left: number }> {
  const saved: Array<{ el: Element; top: number; left: number }> = [];
  let current: Element | null = node;
  while (current) {
    if (current.scrollTop !== 0 || current.scrollLeft !== 0 || current === document.scrollingElement) {
      saved.push({ el: current, top: current.scrollTop, left: current.scrollLeft });
    }
    current = current.parentElement;
  }
  if (document.scrollingElement && !saved.some((item) => item.el === document.scrollingElement)) {
    const el = document.scrollingElement;
    saved.push({ el, top: el.scrollTop, left: el.scrollLeft });
  }
  return saved;
}

function restoreScroll(saved: Array<{ el: Element; top: number; left: number }>) {
  for (const item of saved) { item.el.scrollTop = item.top; item.el.scrollLeft = item.left; }
}

export function GameInstallPanel({ onAccepted, mode = "instance" }: { onAccepted: (origin?: MotionPoint) => void; mode?: "instance" | "cache" }) {
  const [kind, setKind] = useState<VersionFilter>("all");
  const [query, setQuery] = useState("");
  const [offset, setOffset] = useState(0);
  const [revision, setRevision] = useState(0);
  const [page, setPage] = useState<VersionPage | null>(null);
  const [selected, setSelected] = useState("");
  const [name, setName] = useState("");
  const [customName, setCustomName] = useState(false);
  const [configuring, setConfiguring] = useState(false);
  const [fabricRevision, setFabricRevision] = useState(0);
  const heading = useRef<HTMLHeadingElement>(null);
  const lastClicked = useRef<HTMLButtonElement | null>(null);
  const savedScroll = useRef<Array<{ el: Element; top: number; left: number }>>([]);
  const [loader, setLoader] = useState<LoaderKind>("vanilla");
  const [fabricVersions, setFabricVersions] = useState<FabricVersion[]>([]);
  const [fabricVersion, setFabricVersion] = useState("");
  const [fabricLoading, setFabricLoading] = useState(false);
  const [fabricError, setFabricError] = useState("");
  const [loading, setLoading] = useState(true);
  const [refreshTask, setRefreshTask] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [catalogError, setCatalogError] = useState("");
  const [draftId, setDraftId] = useState("");
  const submitLock = useRef(false);
  const refreshLock = useRef(false);

  // Track the latest Fabric request so older responses cannot overwrite newer state.
  const fabricRequestId = useRef(0);
  // Synchronous refs mirror state so late Fabric responses are fenced before any
  // state write, even within the same tick as a game/loader change.
  const selectedRef = useRef(selected);
  const fabricVersionRef = useRef(fabricVersion);
  const loaderRef = useRef<LoaderKind>(loader);
  const customNameRef = useRef(customName);
  const draftIdRef = useRef(draftId);

  useEffect(() => { selectedRef.current = selected; }, [selected]);
  useEffect(() => { fabricVersionRef.current = fabricVersion; }, [fabricVersion]);
  useEffect(() => { loaderRef.current = loader; }, [loader]);
  useEffect(() => { customNameRef.current = customName; }, [customName]);
  useEffect(() => { draftIdRef.current = draftId; }, [draftId]);

  // Focus the configuration heading only after it is actually mounted; the
  // configuration subtree is conditionally mounted so hidden markup never
  // announces false empty components / SSR controls.
  useEffect(() => {
    if (configuring) {
      heading.current?.focus({ preventScroll: true });
      for (const item of savedScroll.current) item.el.scrollTop = 0;
    } else if (lastClicked.current?.isConnected) {
      lastClicked.current.focus({ preventScroll: true });
      restoreScroll(savedScroll.current);
    }
  }, [configuring]);

  // Generate a stable draft instance id when a new game is chosen and after an
  // accepted submission. Same draft id is reused across loader changes/retries.
  function newDraft() {
    const id = `game-${crypto.randomUUID()}`;
    draftIdRef.current = id;
    setDraftId(id);
  }

  useEffect(() => {
    const requestId = ++fabricRequestId.current;
    setFabricError("");
    if (!selected || mode !== "instance") { setFabricLoading(false); return; }
    setFabricLoading(true);
    void fabricApi().versions(selected).then((entries) => {
      if (fabricRequestId.current !== requestId) return;
      if (selectedRef.current !== selected) return;
      setFabricVersions(entries);
      applyFabricCatalogue(entries);
    }).catch(() => {
      if (fabricRequestId.current !== requestId) return;
      if (selectedRef.current !== selected) return;
      setFabricError("未能获取 Fabric 版本。");
    }).finally(() => {
      if (fabricRequestId.current !== requestId) return;
      setFabricLoading(false);
    });
    return () => { fabricRequestId.current++; };
  }, [selected, mode, fabricRevision]);

  // Debounce typing and fence late responses when filters or pages change.
  useEffect(() => {
    let disposed = false;
    setLoading(true);
    setPage(null);
    const timer = window.setTimeout(() => {
      void api.versions(kindForFilter(kind), query.trim(), offset).then((result) => {
        if (!disposed) { setPage(result); setCatalogError(""); }
      }).catch(() => {
        if (!disposed) setCatalogError("未能读取版本列表，请获取或刷新列表。");
      }).finally(() => { if (!disposed) setLoading(false); });
    }, 200);
    return () => { disposed = true; window.clearTimeout(timer); };
  }, [kind, query, offset, revision]);

  // Observe the shared core task; no frontend download or second task scheduler.
  useEffect(() => {
    if (!refreshTask) return;
    let disposed = false;
    let timer: number | undefined;
    async function observe() {
      try {
        const snapshot = await readShellSnapshot();
        if (disposed) return;
        const task = snapshot.tasks.find((item) => item.id === refreshTask);
        if (task && ["succeeded", "failed", "cancelled"].includes(task.state)) {
          setRefreshTask(null); setRefreshing(false); refreshLock.current = false;
          if (task.state === "succeeded") { setOffset(0); setRevision((value) => value + 1); }
          else setCatalogError("版本列表未更新，请重试。");
          return;
        }
      } catch {
        if (disposed) return;
        setCatalogError("连接已中断，请重试。");
        setRefreshTask(null); setRefreshing(false); refreshLock.current = false;
        return;
      }
      timer = window.setTimeout(() => { void observe(); }, 800);
    }
    void observe();
    return () => { disposed = true; window.clearTimeout(timer); };
  }, [refreshTask]);

  async function refresh() {
    if (refreshLock.current) return;
    refreshLock.current = true; setRefreshing(true); setCatalogError("");
    try { setRefreshTask(await api.refresh()); }
    catch { setCatalogError("未能更新版本列表，请检查网络后重试。"); setRefreshing(false); refreshLock.current = false; }
  }

  // Refresh the Fabric catalogue in place during configuration, preserving a
  // still-available selection and resetting loader/name when it disappears.
  function refreshFabric() {
    // Fence any in-flight response synchronously before starting the refresh.
    fabricRequestId.current++;
    setFabricLoading(true); setFabricError("");
    setFabricRevision((value) => value + 1);
  }

  function applyFabricCatalogue(entries: FabricVersion[]) {
    // A NeoForge selection must not let Fabric catalogue reconciliation reset
    // the loader; the catalogue simply becomes available for a later switch.
    if (loaderRef.current === "neoforge") return;
    const selection = fabricVersionRef.current;
    if (selection && !entries.some((entry) => entry.version === selection)) {
      fabricVersionRef.current = "";
      setFabricVersion("");
      if (loaderRef.current === "fabric") { loaderRef.current = "vanilla"; setLoader("vanilla"); }
      if (!customNameRef.current) setName(selectedRef.current);
      setNotice(`刷新后 Fabric ${selection} 已不可用，已移除该选择。`);
    }
  }

  function neoAvailable() {
    return mode === "instance" && neoAvailableFor(selected);
  }

  function chooseNeo() {
    const active = neoAvailable() && loader === "neoforge";
    if (active) {
      // Remove NeoForge: drop to vanilla and clear Fabric selection.
      loaderRef.current = "vanilla"; setLoader("vanilla");
      fabricVersionRef.current = ""; setFabricVersion("");
      if (!customName) setName(selected);
      return;
    }
    if (!neoAvailable()) return;
    // Choosing NeoForge clears the Fabric choice, but a pending Fabric fetch is
    // intentionally left to finish so the catalogue can populate for a later
    // switch back. applyFabricCatalogue skips selection reset while Neo active.
    loaderRef.current = "neoforge"; setLoader("neoforge");
    fabricVersionRef.current = ""; setFabricVersion("");
    if (!customName) setName(`${selected}-NeoForge ${NEOFORGE_PILOT_VERSION}`);
  }

  async function createInstance(event: React.FormEvent) {
    event.preventDefault();
    const neoBlocked = loader === "neoforge" && !neoAvailable();
    if (!selected || neoBlocked || (mode === "instance" && (!name.trim() || (loader === "fabric" && (!fabricVersion || fabricLoading || !!fabricError)))) || submitLock.current) return;
    const button = event.currentTarget.querySelector('button[type="submit"]');
    const rect = button?.getBoundingClientRect();
    const origin = rect ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 } : undefined;
    const instanceId = draftIdRef.current || `game-${crypto.randomUUID()}`;
    submitLock.current = true; setSubmitting(true); setError(""); setNotice("");
    try {
      if (mode === "cache") await environmentApi().download(selected);
      else await api.install(selected, instanceId, name, undefined,
        loader === "fabric" ? { kind: "fabric", version: fabricVersion }
          : loader === "neoforge" ? { kind: "neoforge", version: NEOFORGE_PILOT_VERSION }
          : undefined);
      // Accepted: future submission from this panel must use a fresh id.
      newDraft();
      onAccepted(origin);
    } catch (failure) { setError(mode === "cache" ? "未能开始下载，请稍后重试。" : installError(failure)); }
    finally { submitLock.current = false; setSubmitting(false); }
  }

  function chooseGame(version: string) {
    const changed = version !== selectedRef.current;
    if (changed) {
      // Only a real game change fences the in-flight Fabric request.
      fabricRequestId.current++;
      loaderRef.current = "vanilla"; setLoader("vanilla");
      fabricVersionRef.current = ""; setFabricVersion(""); setFabricVersions([]);
      setFabricLoading(false); setFabricError("");
      if (!customName) setName(version);
      newDraft();
    }
    selectedRef.current = version;
    setSelected(version); setConfiguring(true); setError(""); setNotice(""); setCatalogError("");
  }

  function chooseFabric(version: string) {
    loaderRef.current = version ? "fabric" : "vanilla";
    fabricVersionRef.current = version; setFabricVersion(version);
    setLoader(version ? "fabric" : "vanilla");
    setNotice("");
    if (!customName) setName(version ? `${selected}-Fabric ${version}` : selected);
  }

  function backToBrowser() {
    setConfiguring(false);
  }

  function typeLabel(entry: { type: string }): string {
    if (isKnownVersionKind(entry.type)) return VERSION_KINDS[entry.type];
    return entry.type || "未知类型";
  }

  // Badge labels are purely a comparison against authoritative backend IDs;
  // date/page position is never used to infer a latest version.
  function latestLabel(id: string): string | null {
    if (!page) return null;
    if (page.latestRelease === id) return "最新正式版";
    if (page.latestSnapshot === id) return "最新预览版";
    return null;
  }

  function row(entry: { id: string; type: string; releaseTime?: string }) {
    const badge = latestLabel(entry.id);
    return <button className="version-browser__row" type="button" key={entry.id}
      data-version={entry.id} onClick={(event) => { lastClicked.current = event.currentTarget; savedScroll.current = scrollAncestors(event.currentTarget); chooseGame(entry.id); }}>
      <InstanceArtwork loader="vanilla" />
      <span><strong>{entry.id}</strong>{badge ? <span className="version-browser__latest-badge">{badge}</span> : null}<small>{typeLabel(entry)}{entry.releaseTime ? ` · ${entry.releaseTime.slice(0, 10)}` : ""}</small></span>
      <span className="version-browser__arrow" aria-hidden="true">›</span>
    </button>;
  }

  const groups: VersionGroupSummary[] = page ? groupVersionPage(page.entries) : [];

  const loaderLabel = loader === "fabric" ? `Fabric ${fabricVersion}`
    : loader === "neoforge" ? `NeoForge ${NEOFORGE_PILOT_VERSION}`
    : "原版";

  const neoBlocked = loader === "neoforge" && !neoAvailable();
  const submitDisabled = !selected || neoBlocked || (mode === "instance" && (!name.trim() || (loader === "fabric" && (!fabricVersion || fabricLoading || !!fabricError))));

  return <div className="game-install">
    {/* Browser subtree stays mounted (hidden while configuring) so native scroll,
        details state, and last-clicked focus survive the round trip. */}
    <div hidden={configuring} aria-hidden={configuring ? true : undefined}>
      <Panel title="选择 Minecraft" description="先选游戏版本，再选择需要的组件。"
        actions={<Button size="sm" pending={refreshing} pendingLabel="正在更新" onClick={() => void refresh()}>更新版本列表</Button>}>
        <div className="version-browser__filters">
          <div className="version-browser__categories" role="group" aria-label="版本类型">
            {(Object.keys(VERSION_FILTERS) as VersionFilter[]).map((value) => <button type="button" key={value} value={value}
              aria-pressed={kind === value} onClick={() => { setKind(value); setOffset(0); }}>{VERSION_FILTERS[value]}</button>)}
          </div>
          <label className="install-field">搜索版本
            <input type="search" placeholder="搜索 Minecraft 版本" maxLength={64} value={query} onChange={(event) => { setQuery(event.target.value); setOffset(0); }} />
          </label>
        </div>
        {catalogError ? <p role="alert" className="install-message">{catalogError}</p> : null}
        <div className="version-browser__list" aria-busy={loading} aria-label={VERSION_FILTERS[kind]}>
          {loading ? <p role="status">正在读取版本…</p> : null}
          {!loading && page?.entries.length === 0 ? <p>没有匹配的版本</p> : null}
          {!loading && groups.map((group) => <section className="version-browser__group" data-group={group.group} key={group.group} aria-label={group.label}>
            <h3 className="version-browser__group-heading">{group.label}（本页 {group.entries.length}）</h3>
            {group.entries.map(row)}
          </section>)}
        </div>
        {page ? <div className="install-pagination">
          <Button size="sm" disabled={loading || offset === 0} onClick={() => setOffset(Math.max(0, offset - 40))}>上一页</Button>
          <span>{page.total === 0 ? "0" : `${offset + 1}–${Math.min(offset + 40, page.total)}`} / {page.total}</span>
          <Button size="sm" disabled={loading || offset + 40 >= page.total} onClick={() => setOffset(offset + 40)}>下一页</Button>
        </div> : null}
      </Panel>
    </div>
    {configuring ? <div>
      <form className="component-setup" onSubmit={(event) => void createInstance(event)}>
        <h2 className="sr-only" tabIndex={-1} ref={heading}>配置 Minecraft {selected}</h2>
        <div className="component-setup__identity">
          <Button disabled={submitting} onClick={backToBrowser}>返回版本</Button>
          <InstanceArtwork loader={loader} />
          {mode === "instance" ? <label className="install-field">实例名称
            <input required maxLength={80} value={name} disabled={submitting} onChange={(event) => { setName(event.target.value); setCustomName(true); }} />
          </label> : <div><strong>Minecraft {selected}</strong><p className="field-note">下载到本地</p></div>}
        </div>
        {mode === "instance" ? <ComponentPicker key={selected} selected={fabricVersion} versions={fabricVersions} loading={fabricLoading}
          error={fabricError} disabled={submitting} onSelect={chooseFabric} onRetry={refreshFabric} onRefresh={refreshFabric}
          gameVersion={selected} neoSelected={loader === "neoforge"} onSelectNeo={chooseNeo} />
          : <Panel title="原版游戏"><p>保存游戏资源，之后可直接用于创建实例。</p></Panel>}
        {notice ? <p role="status" className="install-message">{notice}</p> : null}
        {error ? <p role="alert" className="install-message">{error}</p> : null}
        {mode === "instance" ? <div className="component-setup__footer">
          <dl className="component-setup__summary" data-draft-id={draftId}>
            <div><dt>Minecraft</dt><dd>{selected}</dd></div>
            <div><dt>加载器</dt><dd>{loaderLabel}</dd></div>
            <div><dt>名称</dt><dd>{name.trim() || "（未填写）"}</dd></div>
            <div><dt>目标目录</dt><dd><code>instances/{draftId || "…"}</code></dd></div>
          </dl>
          <Button type="submit" variant="primary" disabled={submitDisabled}
            pending={submitting} pendingLabel="正在提交">创建并安装</Button>
        </div> : <div className="component-setup__footer">
          <dl className="component-setup__summary">
            <div><dt>Minecraft</dt><dd>{selected}</dd></div>
            <div><dt>缓存目标</dt><dd>本地游戏资源缓存</dd></div>
          </dl>
          <Button type="submit" variant="primary" disabled={submitDisabled}
            pending={submitting} pendingLabel="正在提交">下载到本地</Button>
        </div>}
      </form>
    </div> : null}
  </div>;
}
