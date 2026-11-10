import { useEffect, useMemo, useState } from "react";
import { gameInstallApi } from "../views/game-install-api";
import type { InstalledGame, ModInventory } from "../views/game-install-api";
import { TabPanel, Tabs, type TabItem } from "./Tabs";
import { Button } from "./Button";
import { InstanceNameSettings } from "./InstanceNameSettings";
import { ModMetadataDetails } from "./ModMetadataDetails";
import { ModFileActions } from "./ModFileActions";

export type InstanceDetailTab = "overview" | "settings" | "mods";

export const INSTANCE_DETAIL_TABS: ReadonlyArray<TabItem<InstanceDetailTab>> = [
  { id: "overview", label: "概览" },
  { id: "settings", label: "设置" },
  { id: "mods", label: "模组" },
];

export interface InstanceManagementPanelProps {
  game: InstalledGame;
  /** Managed relative target, e.g. `instances/<id>`. */
  targetPath: string;
  /** Triggers a refreshed snapshot; inventory is re-derived from it. */
  onRefresh?: () => void;
  /** True while a refresh is in flight. */
  retrying?: boolean;
  /** Used by deterministic previews/tests; normal navigation starts at overview. */
  initialTab?: InstanceDetailTab;
}

function loaderLabel(loader: string): string {
  const known: Record<string, string> = { vanilla: "原版", fabric: "Fabric", neoforge: "NeoForge", forge: "Forge", quilt: "Quilt" };
  return known[loader] ?? loader;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function ModRow({ file, selected, onSelect, disabled }: { file: { fileName: string; enabled: boolean; bytes: number }; selected: boolean; onSelect: () => void; disabled?: boolean }) {
  return (
    <button type="button" className="instance-mod-row" aria-pressed={selected} onClick={onSelect} disabled={disabled}>
      <span className="instance-mod-row__name">{file.fileName}</span>
      <span className="instance-mod-row__state">{file.enabled ? "已启用" : "已禁用"}</span>
    </button>
  );
}

function ModsTab({ game, targetPath, inventory, onRefresh, retrying }: {
  game: InstalledGame;
  targetPath: string;
  inventory: ModInventory | undefined;
  onRefresh?: () => void;
  retrying?: boolean;
}) {
  const [query, setQuery] = useState("");
  const [selectedName, setSelectedName] = useState<string | null>(null);
  const [localInventory, setLocalInventory] = useState<ModInventory | undefined>(inventory);
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  const [mutating, setMutating] = useState(false);
  const [message, setMessage] = useState("");
  useEffect(() => {
    let current = true;
    setSelectedName(null);
    setLocalInventory(inventory);
    if (inventory && revision === 0) { setLoading(false); return; }
    setLoading(true);
    void gameInstallApi().modInventory(game.id).then((value) => {
      if (current) setLocalInventory(value);
    }).catch(() => {
      if (current) setLocalInventory({ status: "unavailable", files: [] });
    }).finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [game.id, inventory, revision]);
  inventory = localInventory;
  const loaderName = loaderLabel(game.version.loader);
  const observedCount = game.presentation?.modFileCount;

  const filtered = useMemo(() => {
    if (!inventory || inventory.status !== "ready") return [];
    const q = query.trim().toLowerCase();
    return inventory.files.filter((file) => !q || file.fileName.toLowerCase().includes(q));
  }, [inventory, query]);

  const selected = inventory && inventory.status === "ready"
    ? inventory.files.find((file) => file.fileName === selectedName) ?? null
    : null;

  const refresh = <Button size="sm" disabled={mutating} pending={loading || Boolean(retrying)} onClick={() => { onRefresh?.(); setRevision((value) => value + 1); }}>刷新</Button>;

  if (loading) return <div className="instance-detail__note" role="status">正在读取 Mod 文件…</div>;

  if (!inventory || inventory.status === "unavailable") {
    return (
      <div className="instance-detail__note">
        <p>{typeof observedCount === "number" ? `无法读取 Mod 清单；此前观察到 ${observedCount} 个 Mod 文件。` : "无法确定 Mod 数量，Mod 清单暂不可用。"}</p>
        <p>清单恢复后可查看文件并切换启用状态；安装尚未接入。</p>
        {refresh}
      </div>
    );
  }

  const target = `模组目标：${game.version.gameVersion} · ${loaderName}（仅文件扫描，兼容性未验证）`;

  if (inventory.status === "limited") {
    return (
      <div className="instance-detail__note">
        <p>Mod 目录条目过多，清单被截断，未返回具体文件。</p>
        <p>{target}</p>
        {refresh}
      </div>
    );
  }

  return (
    <div className="instance-mods">
      <div className="instance-mods__toolbar">
        <label className="install-field game-search">筛选 Mod<input type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="按文件名筛选" /></label>
        {refresh}
      </div>
      <p className="instance-detail__note">{target}</p>
      {message ? <p role="status">{message}</p> : null}
      {inventory.files.length === 0
        ? <p role="status">尚未发现 Mod 文件。</p>
        : filtered.length === 0
          ? <p role="status">没有匹配的 Mod</p>
          : <ul className="instance-mod-list">
            {filtered.map((file) => (
              <li key={file.fileName}>
                <ModRow file={file} selected={selectedName === file.fileName} disabled={mutating} onSelect={() => { setMessage(""); setSelectedName(file.fileName); }} />
              </li>
            ))}
          </ul>}
      {selected ? <>
        <dl className="instance-summary">
          <div><dt>文件名</dt><dd>{selected.fileName}</dd></div>
          <div><dt>相对路径</dt><dd><code>mods/{selected.fileName}</code></dd></div>
          <div><dt>状态</dt><dd>{selected.enabled ? "已启用" : "已禁用"}</dd></div>
          <div><dt>大小</dt><dd>{formatBytes(selected.bytes)}</dd></div>
          <div><dt>兼容目标</dt><dd>{game.version.gameVersion} · {loaderName}（兼容性未验证）</dd></div>
        </dl>
        <ModFileActions key={`actions:${game.id}:${selected.fileName}`} instanceId={game.id} fileName={selected.fileName} bytes={selected.bytes} enabled={selected.enabled} onBusyChange={setMutating} onCommitted={(file) => {
          setMessage(`${file.fileName} ${file.enabled ? "已启用" : "已禁用"}。`);
          onRefresh?.(); setRevision(value => value + 1);
        }} />
        <ModMetadataDetails key={`${game.id}:${selected.fileName}`} instanceId={game.id} fileName={selected.fileName} />
      </> : null}
      <p className="settings-note">目录：{targetPath}/mods · 安装与删除尚未接入。</p>
    </div>
  );
}

/**
 * Inline instance detail with the three PCL2-style tabs. All facts come from the
 * current InstalledGame; unavailable settings or mods are stated honestly rather
 * than fabricated.
 */
export function InstanceManagementPanel({ game, targetPath, onRefresh, retrying, initialTab = "overview" }: InstanceManagementPanelProps) {
  const [active, setActive] = useState<InstanceDetailTab>(initialTab);
  const panelId = `instance-detail-${game.id}`;
  const loaderName = loaderLabel(game.version.loader);
  const inventory = game.presentation?.modInventory;

  return (
    <div className="instance-detail">
      <Tabs items={INSTANCE_DETAIL_TABS} active={active} onChange={setActive} label="实例详情" panelId={panelId} />
      <TabPanel id={panelId} activeTabId={active}>
        {active === "overview" ? <dl className="instance-summary">
          <div><dt>游戏版本</dt><dd>{game.version.gameVersion}</dd></div>
          <div><dt>加载器</dt><dd>{loaderName}{game.version.loaderVersion ? ` ${game.version.loaderVersion}` : ""}</dd></div>
          <div><dt>实例目录</dt><dd><code>{targetPath}</code></dd></div>
          <div><dt>实例状态</dt><dd>已安装</dd></div>
          <div><dt>兼容目标</dt><dd>{game.version.gameVersion} · {loaderName}</dd></div>
        </dl> : null}

        {active === "settings" ? <div>
          <InstanceNameSettings key={game.id} instanceId={game.id} onRefresh={onRefresh} />
          <dl className="instance-summary">
          <div><dt>Java</dt><dd>{game.javaMajor ? `Java ${game.javaMajor}` : "未指定"}</dd></div>
          <div><dt>游戏身份</dt><dd>{game.identityId ? "已绑定" : "未绑定"}</dd></div>
          <div><dt>配置档案</dt><dd>{game.profileId ? "已绑定" : "未绑定"}</dd></div>
          <div><dt>实例覆盖</dt><dd>未配置</dd></div>
          </dl>
          <p className="settings-note">以上为实例快照中的摘要；全局继承关系和启动参数编辑尚未接入。</p>
        </div> : null}

        {active === "mods" ? <ModsTab game={game} targetPath={targetPath} inventory={inventory} onRefresh={onRefresh} retrying={retrying} /> : null}
      </TabPanel>
    </div>
  );
}
