import { useState, type PointerEvent } from "react";
import { isTauriRuntime } from "../../bridge";

type WindowAction = "close" | "minimize";

async function windowAction(action: WindowAction): Promise<void> {
  if (!isTauriRuntime()) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow()[action]();
}

function isTitleSurface(target: EventTarget): boolean {
  return target instanceof Element && !target.closest("button, a, input, select, textarea, [role='button']");
}

export function onWindowTitlePointerDown(event: PointerEvent<HTMLElement>): void {
  if (!isTauriRuntime() || event.button !== 0 || !isTitleSurface(event.target)) return;
  void import("@tauri-apps/api/window")
    .then(({ getCurrentWindow }) => getCurrentWindow().startDragging())
    .catch((error: unknown) => console.warn("Window drag unavailable", error));
}

/** Native actions only; the browser preview keeps its own window controls. */
export function WindowControls({ native }: { native: boolean }) {
  const [error, setError] = useState("");
  if (!native) return null;
  const act = (action: WindowAction) => {
    setError("");
    void windowAction(action).catch((failure: unknown) => {
      console.warn("Window action unavailable", failure);
      setError("窗口操作未完成，请重试。");
    });
  };
  return (
    <div className="window-controls" role="group" aria-label="窗口">
      <button type="button" className="window-control" data-window-action="minimize" title="最小化" aria-label="最小化" onClick={() => act("minimize")}><span aria-hidden="true">−</span></button>
      <button type="button" className="window-control" data-window-action="close" title="关闭窗口" aria-label="关闭窗口" onClick={() => act("close")}><span aria-hidden="true">×</span></button>
      {error ? <span className="sr-only" role="alert">{error}</span> : null}
    </div>
  );
}
