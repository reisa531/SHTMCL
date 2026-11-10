import { useEffect, useState } from "react";
import { Button } from "../components/Button";
import { Tabs, TabPanel } from "../components/Tabs";
import type { MotionPoint } from "../components/motion";
import { PageHeader } from "../views/PageHeader";
import { GameInstallPanel } from "../views/GameInstallPanel";
import { EnvironmentLibrary } from "../views/EnvironmentLibrary";
import type { CachedEnvironment } from "../views/environment-api";
import { RESOURCE_CATEGORIES, ResourceCatalog, type ResourceCategory } from "../views/ResourceCatalog";
import type { InstalledGame } from "../views/game-install-api";

type DownloadArea = "game" | ResourceCategory | "library" | "cache";
const ITEMS = [{ id: "game", label: "原版游戏" }, ...RESOURCE_CATEGORIES, { id: "library", label: "本地资源" }, { id: "cache", label: "仅下载游戏" }] as const;

export interface DownloadHubProps {
  revision: string;
  games?: InstalledGame[];
  gameRequest?: number;
  onAccepted: (origin?: MotionPoint) => void;
  onCreateEnvironment: (environment: CachedEnvironment) => void;
  onViewTasks: () => void;
}

export function DownloadHubPage({ revision, games, gameRequest = 0, onAccepted, onCreateEnvironment, onViewTasks }: DownloadHubProps) {
  const [area, setArea] = useState<DownloadArea>("game");
  const [cacheVisited, setCacheVisited] = useState(false);
  const selectArea = (next: DownloadArea) => { setArea(next); if (next === "cache") setCacheVisited(true); };
  useEffect(() => { setArea("game"); }, [gameRequest]);
  return <>
    <PageHeader title="下载" description="选择游戏版本或社区资源。" actions={<Button icon="clock" onClick={onViewTasks}>查看任务</Button>} />
    <Tabs items={ITEMS} active={area} onChange={selectArea} label="下载分类" panelId="download-hub-panel" />
    <TabPanel id="download-hub-panel" activeTabId={area}>
      <div hidden={area !== "game"}><GameInstallPanel onAccepted={onAccepted} /></div>
      {area === "library" ? <EnvironmentLibrary revision={revision} onAccepted={onAccepted} onGetGame={() => setArea("game")} onCreate={onCreateEnvironment} /> : null}
      {cacheVisited ? <div hidden={area !== "cache"}><GameInstallPanel mode="cache" onAccepted={onAccepted} /></div> : null}
      {RESOURCE_CATEGORIES.some((item) => item.id === area) ? <ResourceCatalog category={area as ResourceCategory} games={games} /> : null}
    </TabPanel>
  </>;
}
