import { useId, type ReactNode } from "react";

export type PanelTone = "default" | "raised";

export interface PanelProps {
  title?: string;
  description?: string;
  actions?: ReactNode;
  tone?: PanelTone;
  /** Extra class for layout only (grid spans, spacing). */
  layout?: string;
  children: ReactNode;
}

/**
 * Content surface. Title renders as `h2` so each page keeps a single `h1`.
 */
export function Panel({
  title,
  description,
  actions,
  tone = "default",
  layout,
  children,
}: PanelProps) {
  const generatedId = useId();
  const titleId = title ? `${generatedId}-title` : undefined;

  return (
    <section
      className={`panel panel--${tone}${layout ? ` ${layout}` : ""}`}
      aria-labelledby={titleId}
    >
      {title ? (
        <div className="panel__head">
          <div className="panel__heading">
            <h2 className="panel__title" id={titleId}>
              {title}
            </h2>
            {description ? (
              <p className="panel__description">{description}</p>
            ) : null}
          </div>
          {actions ? <div className="panel__actions">{actions}</div> : null}
        </div>
      ) : null}
      {children}
    </section>
  );
}
