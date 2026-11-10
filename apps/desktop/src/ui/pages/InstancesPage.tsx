import { useEffect, useRef, useState } from "react";
import { copy, pageCopy } from "../../ux/copy";
import { Button } from "../components/Button";
import { InstanceArtwork } from "../components/InstanceArtwork";
import { InstanceDetails } from "../components/InstanceDetails";
import { InstanceCopyDialog } from "../components/InstanceCopyDialog";
import { InstanceManagementPanel } from "../components/InstanceManagementPanel";
import type { InstalledGame } from "../views/game-install-api";
import { EmptyState } from "../components/EmptyState";
import { uiLabels } from "../labels";
import { PageHeader } from "../views/PageHeader";
import { PageStateView } from "../views/PageState";
import { selectPageState, type SnapshotPhase } from "../views/page-state";

export interface InstancesPageProps {
  phase: SnapshotPhase;
  onRetry: () => void;
  retrying: boolean;
  connected?: boolean;
  onCreate?: () => void;
  onLaunch?: (game: InstalledGame) => void;
  launchingId?: string;
  /** Kept for callers transitioning to the dedicated creation page. */
  onInstallAccepted?: () => void;
  targetInstanceId?: string;
  games?: readonly InstalledGame[];
  libraryPhase?: SnapshotPhase;
  selectedGameId?: string;
  onSelectGame?: (game: InstalledGame) => void;
  onBack?: () => void;
}

function managedTarget(id: string): string {
  return `instances/${id}`;
}

/** Instance rows come from module 01; the browser shell has no native index. */
export function InstancesPage({ phase, onRetry, retrying, connected = false, onCreate, onLaunch, launchingId, targetInstanceId, games = [], libraryPhase = "ready", selectedGameId, onSelectGame, onBack }: InstancesPageProps) {
  const indexPhase = libraryPhase;
  const [query, setQuery] = useState("");
  const [copyOpen, setCopyOpen] = useState(false);
  const [copyPending, setCopyPending] = useState(false);
  const [copySource, setCopySource] = useState<InstalledGame | null>(null);
  const [detailId, setDetailId] = useState<string | null>(null);
  const targetRef = useRef<HTMLElement | null>(null);
  const selectRefs = useRef(new Map<string, HTMLButtonElement | null>());
  const restoreFocusId = useRef<string | null>(null);

  useEffect(() => { setQuery(""); }, [targetInstanceId]);

  const findGame = (id: string | null | undefined): InstalledGame | undefined =>
    id ? games.find((game) => game.id === id) : undefined;
  // Detail wins; otherwise the list scrolls to the deep-linked target.
  const detailGame = findGame(detailId);
  const resolvedTarget = connected && indexPhase === "ready" && targetInstanceId
    ? games.some((game) => game.id === targetInstanceId) ? targetInstanceId : null
    : undefined;

  useEffect(() => {
    if (!resolvedTarget || detailGame) return;
    targetRef.current?.scrollIntoView({ block: "nearest", behavior: "instant" });
    targetRef.current?.focus({ preventScroll: true });
  }, [resolvedTarget, detailGame]);

  // Restore focus to the originating select button when leaving the detail.
  useEffect(() => {
    if (detailGame || !restoreFocusId.current) return;
    const id = restoreFocusId.current;
    restoreFocusId.current = null;
    selectRefs.current.get(id)?.focus({ preventScroll: false });
  }, [detailGame]);

  // A removed/renamed instance must not keep a dangling detail surface.
  useEffect(() => {
    if (detailId && !findGame(detailId)) setDetailId(null);
  }, [detailId, games]);

  const openDetail = (game: InstalledGame) => {
    onSelectGame?.(game);
    restoreFocusId.current = game.id;
    setDetailId(game.id);
  };
  const closeDetail = () => setDetailId(null);

  const state = selectPageState({
    phase: connected ? indexPhase : phase,
    hasDataSource: connected,
    itemCount: games.length,
    activeCount: 0,
    unknownTotalCount: 0,
  });

  if (detailGame) {
    return (
      <>
        <PageHeader
          title={detailGame.name}
          description="实例详情"
          actions={<>{onBack ? <Button onClick={onBack}>返回启动</Button> : null}<Button onClick={closeDetail}>返回实例</Button></>}
        />
        <div className="instance-detail-surface">
          <span className="instance-detail-surface__art"><InstanceArtwork loader={detailGame.version.loader} /></span>
          <InstanceManagementPanel game={detailGame} targetPath={managedTarget(detailGame.id)} onRefresh={onRetry} retrying={retrying} />
        </div>
        <InstanceCopyDialog open={copyOpen} source={copySource} games={[...games]} onClose={() => setCopyOpen(false)} onComplete={onRetry} onBusyChange={setCopyPending} />
      </>
    );
  }

  return (
    <>
      <PageHeader
        title={pageCopy.instances.title}
        description="管理独立的游戏环境。"
        actions={<>{onBack ? <Button onClick={onBack}>返回启动</Button> : null}{connected ? <Button onClick={() => { if (!copyPending) setCopySource(null); setCopyOpen(true); }}>{copyPending ? "查看复制进度" : "粘贴实例"}</Button> : null}{onCreate ? <Button variant="primary" onClick={onCreate}>安装游戏</Button> : null}</>}
      />
      <PageStateView
        state={state}
        loadingLabel={uiLabels.loadingTitle}
        onRetry={onRetry}
        retryPending={retrying}
        notConnectedTitle={uiLabels.instancesNotConnectedTitle}
        notConnectedBody={uiLabels.instancesNotConnectedBody}
      >
        {games.length ? <><label className="install-field game-search">搜索游戏<input type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="名称、Minecraft 版本或加载器" /></label><div className="instance-library">{games.filter((game) => `${game.name} ${game.version.gameVersion} ${game.version.loader}`.toLowerCase().includes(query.trim().toLowerCase())).map((game) => (
          <section key={game.id} className="instance-card" aria-label={game.name}
            aria-current={selectedGameId === game.id ? "true" : undefined}
            data-target={resolvedTarget === game.id ? "true" : undefined}
            tabIndex={resolvedTarget === game.id ? -1 : undefined}
            ref={resolvedTarget === game.id ? targetRef : undefined}>
            <span className="instance-card__art"><InstanceArtwork loader={game.version.loader} /></span>
            <div className="instance-card__content"><h2>{game.name}</h2><p className="instance-card__meta"><InstanceDetails game={game} /></p></div>
            <div className="instance-card__actions"><Button size="sm" ref={(node) => { selectRefs.current.set(game.id, node); }} aria-pressed={selectedGameId === game.id} onClick={() => openDetail(game)}>{selectedGameId === game.id ? "查看详情" : "选用此游戏"}</Button><Button size="sm" disabled={copyPending || launchingId === game.id} onClick={() => { setCopySource(game); setCopyOpen(true); }}>复制</Button><Button variant="primary" icon="play" onClick={() => onLaunch?.(game)} disabled={Boolean(launchingId) && launchingId !== game.id} pending={launchingId === game.id} pendingLabel="正在准备">启动</Button></div>
          </section>
        ))}</div>{!games.some((game) => `${game.name} ${game.version.gameVersion} ${game.version.loader}`.toLowerCase().includes(query.trim().toLowerCase())) ? <p role="status">没有匹配的游戏</p> : null}</> : connected ? <EmptyState
          icon="layers"
          title="创建你的第一个实例"
          body="选择游戏版本，为它起个名字。每个实例使用独立目录。"
        /> : <EmptyState
          icon="layers"
          title={copy.instances.emptyTitle}
          body={copy.instances.emptyBody}
        />}
        {resolvedTarget === null ? <p role="status">实例已移除或暂不可用</p> : null}
      </PageStateView>
      {connected ? <div className="game-install-section">
        <Button size="sm" pending={retrying || indexPhase === "loading"} onClick={onRetry}>刷新游戏</Button>
      </div> : null}
      <InstanceCopyDialog open={copyOpen} source={copySource} games={[...games]} onClose={() => setCopyOpen(false)} onComplete={onRetry} onBusyChange={setCopyPending} />
    </>
  );
}
