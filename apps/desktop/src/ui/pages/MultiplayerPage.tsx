import { useState } from "react";

import { copy, pageCopy } from "../../ux/copy";
import { EmptyState } from "../components/EmptyState";
import { TabPanel, Tabs } from "../components/Tabs";
import { uiLabels } from "../labels";
import { PageHeader } from "../views/PageHeader";
import { PageStateView } from "../views/PageState";
import { selectPageState, type SnapshotPhase } from "../views/page-state";

type MultiplayerTab = "join" | "mine";

const TAB_ITEMS = [
  { id: "join", label: copy.multiplayer.joinTab },
  { id: "mine", label: copy.multiplayer.mineTab },
] as const satisfies ReadonlyArray<{ id: MultiplayerTab; label: string }>;

const VIEWS: Record<
  MultiplayerTab,
  { title: string; body: string; notConnectedTitle: string; notConnectedBody: string }
> = {
  join: {
    title: copy.multiplayer.joinTitle,
    body: copy.multiplayer.joinBody,
    notConnectedTitle: "一起进入同一个世界",
    notConnectedBody: "房间与服务器服务准备中。你仍可在游戏内使用服务器地址加入好友。",
  },
  mine: {
    title: copy.multiplayer.mineTitle,
    body: copy.multiplayer.mineBody,
    notConnectedTitle: "为朋友留一个位置",
    notConnectedBody: "房间服务准备中。开启游戏内的局域网世界，也可以和同一网络的朋友一起游玩。",
  },
};

const PANEL_ID = "multiplayer-panel";

export interface MultiplayerPageProps {
  phase: SnapshotPhase;
  onRetry: () => void;
  retrying: boolean;
}

/**
 * No room module is registered, so both tabs report the missing data source
 * (and keep loading/error surfaces) instead of claiming there are no rooms.
 */
export function MultiplayerPage({ phase, onRetry, retrying }: MultiplayerPageProps) {
  const [tab, setTab] = useState<MultiplayerTab>("join");
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
        icon="users"
        title={pageCopy.multiplayer.title}
        description="相聚在熟悉的方块世界。"
      />

      <Tabs
        items={TAB_ITEMS}
        active={tab}
        onChange={setTab}
        label="联机视图"
        panelId={PANEL_ID}
      />

      <div className="catalog-surface"><TabPanel id={PANEL_ID} activeTabId={tab}>
        <PageStateView
          state={state}
          loadingLabel={uiLabels.loadingTitle}
          onRetry={onRetry}
          retryPending={retrying}
          notConnectedTitle={view.notConnectedTitle}
          notConnectedBody={view.notConnectedBody}
        >
          <EmptyState icon="users" title={view.notConnectedTitle} body={view.notConnectedBody} />
        </PageStateView>
      </TabPanel></div>
    </>
  );
}
