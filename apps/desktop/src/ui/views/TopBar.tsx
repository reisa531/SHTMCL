import { shellCopy } from "../../ux/copy";
import { Icon } from "../Icons";
import { WindowControls, onWindowTitlePointerDown } from "../components/WindowControls";

export interface TopBarProps {
  title: string;
  environment: "native" | "browser";
  loading: boolean;
  accountOpen: boolean;
  onOpenAccount: () => void;
  identityLabel?: string;
  accountSelected?: boolean;
}

export function TopBar({
  title,
  environment,
  loading,
  accountOpen,
  onOpenAccount,
  identityLabel,
  accountSelected = false,
}: TopBarProps) {
  return (
    <header className="topbar" onPointerDown={onWindowTitlePointerDown}>
      <div className="topbar__title">
        <span className="sr-only">{title}</span>
        {loading ? (
          <span className="topbar__loading" aria-live="polite">
            {shellCopy.loading}
          </span>
        ) : null}
      </div>

      <div className="topbar__actions">
        {environment === "browser" ? <span className="env-chip"><Icon name="layers" size={13} />网页预览</span> : null}
        <button
          type="button"
          className="account-button"
          id="shell-account-trigger"
          onClick={onOpenAccount}
          aria-haspopup="dialog"
          aria-expanded={accountOpen}
          aria-label={shellCopy.openAccount}
        >
          <span className="account-button__avatar" data-selected={accountSelected} aria-hidden="true">
            <Icon name="user" size={16} />
          </span>
          <span className="account-button__label">
            {accountSelected && identityLabel ? identityLabel : "未选择玩家"}
          </span>
          <span className="account-button__chevron" aria-hidden="true">
            <Icon name="chevron" size={14} />
          </span>
        </button>
        <WindowControls native={environment === "native"} />
      </div>
    </header>
  );
}
