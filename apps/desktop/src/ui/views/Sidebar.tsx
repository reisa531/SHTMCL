import type { AppInfo } from "../../contract";
import {
  navLabels,
  primaryNav,
  shellCopy,
  type PageId,
  type PrimaryPageId,
} from "../../ux/copy";
import { Icon, type IconName } from "../Icons";
import { uiLabels } from "../labels";

const NAV_ICONS: Record<PrimaryPageId, IconName> = {
  start: "home",
  multiplayer: "users",
  more: "compass",
  downloads: "download",
  tasks: "clock",
};

export interface SidebarProps {
  page: PageId;
  onNavigate: (page: PageId) => void;
  appInfo: AppInfo | null;
  environment: "native" | "browser";
  activeDownloads?: number;
}

export function Sidebar({ page, onNavigate, appInfo, environment, activeDownloads = 0 }: SidebarProps) {
  const version = appInfo ? `v${appInfo.version}` : shellCopy.unknown;

  return (
    <aside className="sidebar">
      <div className="brand">
        <span className="brand__mark" aria-hidden="true">
          <Icon name="cloud" size={20} />
        </span>
        <span className="brand__copy">
          <strong className="brand__name">SHTMCL</strong>
          <span className="brand__subtitle">{shellCopy.brandSubtitle}</span>
        </span>
      </div>

      <nav className="nav" aria-label={uiLabels.primaryNav}>
        {primaryNav.map((item) => (
          <button
            key={item}
            type="button"
            className="nav__item"
            data-page={item}
            title={navLabels[item]}
            aria-label={item === "tasks" && activeDownloads > 0 ? `任务，${activeDownloads}个进行中的下载` : navLabels[item]}
            aria-current={(page === item || (item === "start" && page === "instances") || (item === "downloads" && page === "discover")) ? "page" : undefined}
            onClick={() => onNavigate(item)}
          >
            <span className="nav__icon" aria-hidden="true">
              <Icon name={NAV_ICONS[item]} size={19} />
            </span>
            <span className="nav__label">{navLabels[item]}</span>
            {item === "tasks" && activeDownloads > 0 ? <span className="nav__badge" aria-hidden="true">{activeDownloads > 99 ? "99+" : activeDownloads}</span> : null}
          </button>
        ))}
      </nav>
      <span className="sr-only" role="status">{activeDownloads > 0 ? `${activeDownloads}个下载正在进行` : ""}</span>

      <div className="sidebar__footer">
        <button
          type="button"
          className="nav__item"
          data-page="settings"
          title={navLabels.settings}
          aria-label={navLabels.settings}
          aria-current={page === "settings" ? "page" : undefined}
          onClick={() => onNavigate("settings")}
        >
          <span className="nav__icon" aria-hidden="true">
            <Icon name="settings" size={19} />
          </span>
          <span className="nav__label">{navLabels.settings}</span>
        </button>
        <div className="build-chip">
          <span className="build-chip__dot" aria-hidden="true" />
          <span className="build-chip__env">
            {environment === "native"
              ? shellCopy.environmentNative
              : shellCopy.environmentBrowser}
          </span>
          <span className="build-chip__version">{version}</span>
        </div>
      </div>
    </aside>
  );
}
