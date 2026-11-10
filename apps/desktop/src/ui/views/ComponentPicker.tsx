import { useState } from "react";
import { Button } from "../components/Button";
import { InstanceArtwork } from "../components/InstanceArtwork";
import type { FabricVersion } from "./fabric-api";
import { NEOFORGE_PILOT_GAME_VERSION, NEOFORGE_PILOT_VERSIONS, neoAvailableFor } from "./install-design";

/**
 * Component selection surface. Availability is explicit: only installed
 * adapters offer real selection. NeoForge is limited to the exact pilot; other
 * game versions show it as unsupported (not as an installable adapter).
 */
export function ComponentPicker({ selected, versions, loading, error, disabled, onSelect, onRetry, onRefresh, gameVersion, neoSelected, onSelectNeo }: {
  selected: string; versions: FabricVersion[]; loading: boolean; error: string; disabled: boolean;
  onSelect: (version: string) => void; onRetry: () => void; onRefresh?: () => void;
  gameVersion?: string; neoSelected?: boolean; onSelectNeo?: () => void;
}) {
  const [visibleCount, setVisibleCount] = useState(40);
  // `disabled` already reflects submitting state from the parent.
  const busy = disabled;
  const neoActive = typeof gameVersion === "string" && neoAvailableFor(gameVersion);

  function fabricStateLabel(): string {
    if (loading) return "正在获取…";
    if (error) return "获取失败";
    if (selected) return "已选择";
    if (versions.length) return "可以添加";
    return "没有适用版本";
  }

  function option(entry: FabricVersion) {
    return <button type="button" className="component-option" data-fabric={entry.version} key={entry.version}
      aria-pressed={selected === entry.version} disabled={busy || loading} onClick={() => onSelect(entry.version)}>
      <InstanceArtwork loader="fabric" /><span><strong>{entry.version}</strong><small>{entry.stable ? "稳定版" : "预览版"}</small></span>
      <span className="component-option__choice" aria-hidden="true">{selected === entry.version ? "✓" : "+"}</span>
    </button>;
  }

  // An adapter that is not implemented in the core yet. Distinct from a game
  // version that is simply outside the fixed NeoForge pilot.
  const unavailableAdapter = (title: string, kind: string, note: string) => <div className="component-card component-card--unavailable" data-component={kind} key={kind}>
    <InstanceArtwork loader={kind} /><strong>{title}</strong><span>{note}</span>
  </div>;

  function neoforgeCard() {
    if (!neoActive) return unavailableAdapter("NeoForge", "neoforge", `仅支持 Minecraft ${NEOFORGE_PILOT_GAME_VERSION}`);
    return <details className="component-card" data-component="neoforge" key="neoforge" open>
      <summary><InstanceArtwork loader="neoforge" /><strong>NeoForge</strong><span>{neoSelected ? "已选择" : "可以添加"}</span><span className="component-card__chevron" aria-hidden="true">›</span></summary>
      <div className="component-card__content">
        {selected ? <p className="field-note">选择 NeoForge 将替换 Fabric。</p> : null}
        {neoSelected ? <div className="component-card__refresh">
          <Button size="sm" disabled={busy} onClick={() => onSelectNeo?.()}>移除 NeoForge</Button>
        </div> : null}
        {NEOFORGE_PILOT_VERSIONS.map((version) => <button type="button" className="component-option" data-neoforge={version} key={version}
          aria-pressed={!!neoSelected} disabled={busy || !!neoSelected} onClick={() => onSelectNeo?.()}>
          <InstanceArtwork loader="neoforge" /><span><strong>{version}</strong><small>稳定版</small></span>
          <span className="component-option__choice" aria-hidden="true">{neoSelected ? "✓" : "+"}</span>
        </button>)}
      </div>
    </details>;
  }

  function fabricContent() {
    if (loading) return <p role="status">正在获取兼容版本…</p>;
    if (error) return <div role="alert"><p>{error}</p><Button size="sm" disabled={busy} onClick={onRetry}>重试</Button></div>;
    if (versions.length) {
      const recommended = versions.find((entry) => entry.stable);
      const rest = versions.filter((entry) => entry !== recommended);
      return <>
        {recommended ? <section aria-label="推荐 Fabric 版本"><h3>推荐</h3>{option(recommended)}</section> : null}
        <section aria-label="全部其他 Fabric 版本"><h3>{recommended ? "其他版本" : "全部版本"}（{rest.length}）</h3>{rest.slice(0, visibleCount).map(option)}</section>
        {rest.length > visibleCount ? <Button size="sm" disabled={busy} onClick={() => setVisibleCount((count) => count + 40)}>显示更多版本</Button> : null}
      </>;
    }
    return <p>Fabric 尚未提供这个游戏版本的加载器。</p>;
  }

  return <div className="component-picker">
    {unavailableAdapter("Forge", "forge", "安装支持开发中")}
    {neoforgeCard()}
    <details className="component-card" data-component="fabric">
      <summary><InstanceArtwork loader="fabric" /><strong>Fabric</strong><span>{fabricStateLabel()}</span><span className="component-card__chevron" aria-hidden="true">›</span></summary>
      <div className="component-card__content">
        {neoSelected ? <p className="field-note">选择 Fabric 将替换 NeoForge。</p> : null}
        <div className="component-card__refresh">
          <Button size="sm" pending={loading} pendingLabel="正在刷新" disabled={busy || loading} onClick={() => (onRefresh ?? onRetry)()}>刷新 Fabric 版本</Button>
          {selected ? <Button size="sm" disabled={busy || loading} onClick={() => onSelect("")}>移除 Fabric</Button> : null}
        </div>
        {fabricContent()}
      </div>
    </details>
    {selected ? <div className="component-card component-card--unavailable"><InstanceArtwork loader="fabric" /><strong>Fabric API</strong><span>自动安装开发中</span></div> : null}
    {unavailableAdapter("OptiFine", "optifine", "安装支持开发中")}
  </div>;
}
