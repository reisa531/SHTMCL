import type { ReactNode } from "react";

export type IconName =
  | "home"
  | "layers"
  | "users"
  | "compass"
  | "download"
  | "settings"
  | "user"
  | "cloud"
  | "chevron"
  | "plus"
  | "play"
  | "search"
  | "close"
  | "refresh"
  | "stop"
  | "check"
  | "alert"
  | "clock"
  | "dot";

const ICON_PATHS: Record<IconName, ReactNode> = {
  home: (
    <>
      <path d="M3.5 10.5 12 3l8.5 7.5" />
      <path d="M5.5 9.5V21h13V9.5" />
      <path d="M9.5 21v-6h5v6" />
    </>
  ),
  layers: (
    <>
      <path d="m12 3 8.5 4.5L12 12 3.5 7.5 12 3Z" />
      <path d="m3.5 12 8.5 4.5 8.5-4.5" />
      <path d="m3.5 16.5 8.5 4.5 8.5-4.5" />
    </>
  ),
  users: (
    <>
      <circle cx="9" cy="8" r="3.2" />
      <path d="M3.5 20c.6-3.2 2.7-5 5.5-5s4.9 1.8 5.5 5" />
      <path d="M16 5.5a3 3 0 0 1 0 5.8" />
      <path d="M17.5 15.2c1.8.5 3 2.1 3.4 4.3" />
    </>
  ),
  compass: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="m14.5 9.5-1.6 4.9-4.9 1.6 1.6-4.9 4.9-1.6Z" />
    </>
  ),
  download: (
    <>
      <path d="M12 3v12" />
      <path d="m7.5 10.5 4.5 4.5 4.5-4.5" />
      <path d="M4 19.5h16" />
    </>
  ),
  settings: (
    <>
      <circle cx="12" cy="12" r="3" />
      <path d="M12 2.8v2.4M12 18.8v2.4M4.7 4.7l1.7 1.7M17.6 17.6l1.7 1.7M2.8 12h2.4M18.8 12h2.4M4.7 19.3l1.7-1.7M17.6 6.4l1.7-1.7" />
    </>
  ),
  user: (
    <>
      <circle cx="12" cy="8" r="3.2" />
      <path d="M5.5 20c.7-3.4 3.1-5.2 6.5-5.2s5.8 1.8 6.5 5.2" />
    </>
  ),
  cloud: (
    <path d="M7 18h9.5a3.5 3.5 0 0 0 .4-6.98A5.2 5.2 0 0 0 6.6 9.4 4.6 4.6 0 0 0 7 18Z" />
  ),
  chevron: <path d="m9 5 7 7-7 7" />,
  plus: <path d="M12 5v14M5 12h14" />,
  play: <path d="m8 5 11 7-11 7V5Z" />,
  search: (
    <>
      <circle cx="11" cy="11" r="6.5" />
      <path d="m16 16 4 4" />
    </>
  ),
  close: <path d="m6 6 12 12M18 6 6 18" />,
  refresh: (
    <>
      <path d="M20 11.5a8 8 0 1 0-2.6 6.3" />
      <path d="M20 4.5v7h-7" />
    </>
  ),
  stop: <rect x="7" y="7" width="10" height="10" rx="2" />,
  check: <path d="m5 12.5 4.5 4.5L19 7" />,
  alert: (
    <>
      <path d="M12 4.5 21 20H3l9-15.5Z" />
      <path d="M12 10v4.2" />
      <path d="M12 17.2v.1" />
    </>
  ),
  clock: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 7.5V12l3 2" />
    </>
  ),
  dot: <circle cx="12" cy="12" r="3.5" fill="currentColor" stroke="none" />,
};

export function Icon({
  name,
  size = 20,
}: {
  name: IconName;
  size?: number;
}) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {ICON_PATHS[name]}
    </svg>
  );
}
