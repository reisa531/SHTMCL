import type { ReactNode } from "react";

import { Icon, type IconName } from "../Icons";

export type BannerTone = "info" | "warn" | "error" | "success";

const BANNER_ICONS: Record<BannerTone, IconName> = {
  info: "clock",
  warn: "clock",
  error: "alert",
  success: "check",
};

export interface BannerProps {
  tone: BannerTone;
  title: string;
  body?: string;
  actions?: ReactNode;
}

/**
 * In-place message for the current page. Errors announce assertively, other
 * tones politely; the tone is also carried by an icon and a border style so
 * colour is never the only signal.
 */
export function Banner({ tone, title, body, actions }: BannerProps) {
  const assertive = tone === "error";
  return (
    <div
      className={`banner banner--${tone}`}
      role={assertive ? "alert" : "status"}
      aria-live={assertive ? "assertive" : "polite"}
    >
      <span className="banner__icon" aria-hidden="true">
        <Icon name={BANNER_ICONS[tone]} size={17} />
      </span>
      <div className="banner__text">
        <p className="banner__title">{title}</p>
        {body ? <p className="banner__body">{body}</p> : null}
      </div>
      {actions ? <div className="banner__actions">{actions}</div> : null}
    </div>
  );
}
