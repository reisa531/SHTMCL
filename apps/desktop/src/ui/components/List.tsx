import type { ReactNode } from "react";

export interface ListProps {
  /** Screen-reader name for the group. */
  label: string;
  children: ReactNode;
  className?: string;
}

export function List({ label, children, className }: ListProps) {
  return (
    <ul className={`list${className ? ` ${className}` : ""}`} aria-label={label}>
      {children}
    </ul>
  );
}

export interface ListRowProps {
  children: ReactNode;
  /** Id used by the row's own controls when they need an accessible name. */
  id?: string;
}

export function ListRow({ children, id }: ListRowProps) {
  return (
    <li className="list__row" id={id}>
      {children}
    </li>
  );
}
