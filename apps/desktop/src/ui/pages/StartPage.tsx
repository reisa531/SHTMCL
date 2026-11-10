import type { PageId } from "../../ux/copy";
import { Button } from "../components/Button";
import { InstanceArtwork } from "../components/InstanceArtwork";
import { InstanceDetails } from "../components/InstanceDetails";
import { PageHeader } from "../views/PageHeader";
import type { SnapshotPhase } from "../views/page-state";
import type { InstalledGame } from "../views/game-install-api";

export interface StartPageProps {
  phase: SnapshotPhase;
  onRetry: () => void;
  retrying: boolean;
  onNavigate: (page: PageId) => void;
  connected?: boolean;
  onCreate?: () => void;
  onLaunch?: (game: InstalledGame) => void;
  launchingId?: string;
  game?: InstalledGame;
  libraryPhase?: SnapshotPhase;
  playerName?: string;
  onOpenAccount?: () => void;
}

export function StartPage({ phase, onRetry, retrying, onNavigate, connected = false, onCreate, onLaunch, launchingId, game, libraryPhase = "ready", playerName, onOpenAccount }: StartPageProps) {
  const loading = connected && libraryPhase === "loading";
  const failed = phase === "error" || libraryPhase === "error";
  const create = onCreate ?? (() => onNavigate("downloads"));
  return <>
    <PageHeader title="启动" />
    <div className="launch-player">
      <div><span className="field-label">当前玩家</span><strong>{playerName ?? "未选择玩家"}</strong></div>
      <Button id="start-account-trigger" variant="quiet" onClick={onOpenAccount}>选择玩家</Button>
    </div>
    <section className="world-hero" aria-labelledby="start-hero-title" aria-busy={loading}>
      <div className="world-hero__content">
        <p className="world-hero__eyebrow">当前游戏</p>
        <h2 className="world-hero__title" id="start-hero-title">{loading ? "正在读取游戏…" : failed ? "暂时无法读取游戏" : game ? game.name : connected ? "还没有安装游戏" : "选择一个游戏"}</h2>
        {game && !failed ? <div className="world-hero__meta"><InstanceDetails game={game} /></div> : <p className="world-hero__body">在下载页选择 Minecraft 版本和组件，安装后即可启动。</p>}
        <div className="world-hero__actions">
          <Button variant="primary" icon={game ? "play" : "download"} pending={Boolean(game && launchingId === game.id)} pendingLabel="正在准备" disabled={loading || failed || Boolean(launchingId) || (Boolean(game) && !connected)} onClick={() => game ? onLaunch?.(game) : create()}>{game ? "启动游戏" : "安装游戏"}</Button>
          <Button onClick={() => onNavigate("instances")}>选择游戏</Button>
        </div>
        {!connected ? <p className="world-hero__body">在桌面启动器中安装和启动游戏。</p> : null}
      </div>
      <div className="world-hero__art" aria-hidden="true"><InstanceArtwork loader={game?.version.loader ?? "vanilla"} /></div>
    </section>
    {failed ? <div className="install-message" role="alert"><p>未能读取游戏信息，请重试。</p><Button size="sm" pending={retrying || loading} onClick={onRetry}>重试</Button></div> : null}
  </>;
}
