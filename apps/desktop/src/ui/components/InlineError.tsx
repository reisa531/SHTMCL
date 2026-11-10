import type { ReactNode } from "react";

export interface InlineErrorProps {
  /** Referenced by the control that failed, via aria-describedby. */
  id: string;
  message: string;
  hint?: string;
  /** Recovery control shown next to the message. */
  children?: ReactNode;
}

/** Field-adjacent error. Owns an id so controls can point at it. */
export function InlineError({ id, message, hint, children }: InlineErrorProps) {
  return (
    <div className="inline-error" id={id} role="alert">
      <p className="inline-error__message">{message}</p>
      {hint ? <p className="inline-error__hint">{hint}</p> : null}
      {children ? <div className="inline-error__actions">{children}</div> : null}
    </div>
  );
}
