import type { ReactNode } from "react";
import { Icon, type IconName } from "../Icons";

export interface PageHeaderProps {
  headingId?: string;
  title: string;
  description?: string;
  actions?: ReactNode;
  icon?: IconName;
}

export function PageHeader({ title, description, actions, icon, headingId = "page-heading" }: PageHeaderProps) {
  return (
    <header className="page-header">
      <div className="page-header__heading">
        <h1 className="page-header__title" id={headingId}>
          {icon ? <span className="page-header__icon"><Icon name={icon} size={25} /></span> : null}
          {title}
        </h1>
        {description ? (
          <p className="page-header__description">{description}</p>
        ) : null}
      </div>
      {actions ? <div className="page-header__actions">{actions}</div> : null}
    </header>
  );
}
