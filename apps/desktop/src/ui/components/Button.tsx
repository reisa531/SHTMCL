import {
  forwardRef,
  type ButtonHTMLAttributes,
  type ReactNode,
} from "react";

import { Icon, type IconName } from "../Icons";

export type ButtonVariant = "primary" | "secondary" | "quiet" | "danger";
export type ButtonSize = "md" | "sm";

export interface ButtonProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "className" | "children"> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  icon?: IconName;
  /** In-flight state: keeps the label, disables repeat submission. */
  pending?: boolean;
  pendingLabel?: string;
  /** Id of the element explaining why a disabled button cannot run. */
  reasonId?: string;
  children: ReactNode;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  function Button(props, ref) {
    const {
      variant = "secondary",
      size = "md",
      icon,
      pending = false,
      pendingLabel,
      reasonId,
      children,
      type = "button",
      disabled,
      ...rest
    } = props;

    const describedBy = reasonId ?? rest["aria-describedby"];

    return (
      <button
        {...rest}
        ref={ref}
        type={type}
        className={`btn btn--${variant} btn--${size}`}
        disabled={disabled === true || pending}
        aria-busy={pending ? true : undefined}
        aria-describedby={describedBy}
        data-pending={pending ? "true" : undefined}
      >
        {icon ? (
          <span className="btn__icon" aria-hidden="true">
            <Icon name={icon} size={size === "sm" ? 15 : 16} />
          </span>
        ) : null}
        <span className="btn__label">
          {pending ? pendingLabel ?? children : children}
        </span>
      </button>
    );
  },
);
