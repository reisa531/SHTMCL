import type { ButtonHTMLAttributes } from "react";

import { Icon, type IconName } from "../Icons";

export type IconButtonTone = "neutral" | "quiet";

export interface IconButtonProps
  extends Omit<
    ButtonHTMLAttributes<HTMLButtonElement>,
    "className" | "children" | "aria-label"
  > {
  /** Accessible name; required because the control has no visible label. */
  label: string;
  icon: IconName;
  tone?: IconButtonTone;
  size?: number;
}

export function IconButton({
  label,
  icon,
  tone = "neutral",
  size = 18,
  type = "button",
  ...rest
}: IconButtonProps) {
  return (
    <button
      {...rest}
      type={type}
      className={`icon-btn icon-btn--${tone}`}
      aria-label={label}
    >
      <Icon name={icon} size={size} />
    </button>
  );
}
