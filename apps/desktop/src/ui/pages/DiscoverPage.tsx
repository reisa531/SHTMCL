import { useState } from "react";

import { copy, pageCopy } from "../../ux/copy";
import { EmptyState } from "../components/EmptyState";
import { TabPanel, Tabs } from "../components/Tabs";
import type { IconName } from "../Icons";
import { uiLabels } from "../labels";
import { PageHeader } from "../views/PageHeader";
import { PageStateView } from "../views/PageState";
import { selectPageState, type SnapshotPhase } from "../views/page-state";

type DiscoverTab = "servers" | "modpacks" | "mods" | "packs";

const TAB_ITEMS = [
  { id: "servers", label: copy.discover.serversTab },
  { id: "modpacks", label: copy.discover.modpacksTab },
  { id: "mods", label: copy.discover.modsTab },
  { id: "packs", label: copy.discover.packsTab },
] as const satisfies ReadonlyArray<{ id: DiscoverTab; label: string }>;

const VIEWS: Record<
  DiscoverTab,
  { title: string; body: string; icon: IconName; href?: string; linkLabel?: string }
> = {
  servers: {
    title: "寻找下一个相聚的世界",
    body: "服务器目录准备中。已知服务器地址可在游戏中直接添加。",
    icon: "compass",
  },
  modpacks: {
    title: "下一个冒险，从这里开始",
    body: "前往社区挑选整合包，探索新的玩法。",
    icon: "layers",
    href: "https://modrinth.com/modpacks",
    linkLabel: "浏览 Modrinth 整合包",
  },
  mods: {
    title: "为你的世界加一点新鲜感",
    body: "在社区寻找模组，了解玩法与兼容版本。",
    icon: "plus",
    href: "https://www.mcmod.cn/",
    linkLabel: "浏览 MC 百科",
  },
  packs: {
    title: "换一种方式看世界",
    body: "发现新的材质与视觉风格。",
    icon: "cloud",
    href: "https://modrinth.com/resourcepacks",
    linkLabel: "浏览 Modrinth 材质包",
  },
};

const PANEL_ID = "discover-panel";

export interface DiscoverPageProps {
  phase: SnapshotPhase;
  onRetry: () => void;
  retrying: boolean;
}

/**
 * Online directories are not connected. Each tab keeps its own wording, and
 * the page now also has loading/error surfaces instead of rendering the
 * "not connected" text during every phase.
 */
export function DiscoverPage({ phase, onRetry, retrying }: DiscoverPageProps) {
  const [tab, setTab] = useState<DiscoverTab>("servers");
  const view = VIEWS[tab];
  const state = selectPageState({
    phase,
    hasDataSource: false,
    itemCount: 0,
    activeCount: 0,
    unknownTotalCount: 0,
  });

  return (
    <>
      <PageHeader
        icon="compass"
        title={pageCopy.discover.title}
        description="发现值得加入你世界的内容。"
      />

      <Tabs
        items={TAB_ITEMS}
        active={tab}
        onChange={setTab}
        label="发现视图"
        panelId={PANEL_ID}
      />

      <div className="catalog-surface"><TabPanel id={PANEL_ID} activeTabId={tab}>
        <PageStateView
          state={state}
          loadingLabel={uiLabels.loadingTitle}
          onRetry={onRetry}
          retryPending={retrying}
          notConnectedTitle={view.title}
          notConnectedBody={view.body}
        >
          <EmptyState icon={view.icon} title={view.title} body={view.body} />
        </PageStateView>
      </TabPanel>
        {state === "not_connected" && view.href ? <div className="resource-links"><a className="btn btn--secondary btn--md" href={view.href} target="_blank" rel="noopener noreferrer">{view.linkLabel}<span aria-hidden="true"> ↗</span></a></div> : null}
      </div>
    </>
  );
}
