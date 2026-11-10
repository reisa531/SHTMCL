import type { ReactNode } from "react";

import { Icon, type IconName } from "../Icons";

export interface EmptyStateProps {
  icon: IconName;
  title: string;
  body?: string;
  actions?: ReactNode;
}

/** A page region with no items yet, plus the action that would create them. */
export function EmptyState({ icon, title, body, actions }: EmptyStateProps) {
  return (
    <section className="empty-state">
      <span className="empty-state__icon" aria-hidden="true">
        <Icon name={icon} size={24} />
      </span>
      <h2 className="empty-state__title">{title}</h2>
      {body ? <p className="empty-state__body">{body}</p> : null}
      {actions ? <div className="empty-state__actions">{actions}</div> : null}
    </section>
  );
}
