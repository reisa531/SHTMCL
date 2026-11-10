import { useEffect, useId, useRef, type ReactNode } from "react";

import { uiLabels } from "../labels";
import { IconButton } from "./IconButton";
import { reduceMotion } from "./motion";

export interface DialogProps {
  open: boolean;
  title: string;
  children: ReactNode;
  actions?: ReactNode;
  /** Called for Escape, the close button, backdrop click and native cancel. */
  onClose: () => void;
}

/**
 * Modal dialog built on the native `<dialog>` element, so the platform keeps
 * the top layer and the focus trap. Escape and `cancel` are routed through
 * `onClose` so the shell reducer stays the single owner of open state, and
 * focus returns to the element that opened the dialog.
 */
export function Dialog({ open, title, children, actions, onClose }: DialogProps) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const returnFocusRef = useRef<HTMLElement | null>(null);
  const titleId = useId();

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) {
      return;
    }
    let animation: Animation | undefined;
    let disposed = false;
    const close = () => {
      if (disposed) return;
      dialog.close();
      const target = returnFocusRef.current;
      returnFocusRef.current = null;
      if (target?.isConnected) target.focus();
    };
    if (open && !dialog.open) {
      const active = document.activeElement;
      returnFocusRef.current = active instanceof HTMLElement ? active : null;
      dialog.showModal();
      if (!reduceMotion() && typeof dialog.animate === "function") animation = dialog.animate([
        { opacity: 0, transform: "translateY(12px) scale(.975)" },
        { opacity: 1, transform: "translateY(0) scale(1)" },
      ], { duration: 260, easing: "cubic-bezier(.16,1,.3,1)" });
    } else if (!open && dialog.open) {
      if (reduceMotion() || typeof dialog.animate !== "function") close();
      else {
        animation = dialog.animate([
          { opacity: 1, transform: "translateY(0) scale(1)" },
          { opacity: 0, transform: "translateY(4px) scale(.99)" },
        ], { duration: 160, easing: "cubic-bezier(.4,0,1,1)" });
        void animation.finished.then(close, () => {});
      }
    }
    const settle = () => {
      if (!reduceMotion()) return;
      animation?.cancel();
      if (!open && dialog.open) close();
    };
    const media = window.matchMedia("(prefers-reduced-motion: reduce)");
    const observer = new MutationObserver(settle);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-motion"] });
    media.addEventListener("change", settle);
    return () => {
      disposed = true;
      animation?.cancel();
      observer.disconnect();
      media.removeEventListener("change", settle);
    };
  }, [open]);

  return (
    <dialog
      ref={dialogRef}
      className="dialog"
      aria-labelledby={titleId}
      onCancel={(event) => {
        // Keep the shell state authoritative over the native close.
        event.preventDefault();
        onClose();
      }}
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          onClose();
        }
      }}
      onClick={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div className="dialog__head">
        <h2 className="dialog__title" id={titleId}>
          {title}
        </h2>
        <IconButton
          label={uiLabels.dialogClose}
          icon="close"
          tone="quiet"
          size={16}
          onClick={onClose}
        />
      </div>
      <div className="dialog__body">{children}</div>
      {actions ? <div className="dialog__actions">{actions}</div> : null}
    </dialog>
  );
}
