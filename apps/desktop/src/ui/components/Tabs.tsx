import { useEffect, useRef, useState, type ReactNode } from "react";
import { enterSurface } from "./surface-motion";

export interface TabItem<T extends string> {
  id: T;
  label: string;
}

export interface TabsProps<T extends string> {
  items: ReadonlyArray<TabItem<T>>;
  active: T;
  onChange: (id: T) => void;
  /** Screen-reader name for the tab group. */
  label: string;
  /** Id of the element the active tab controls. */
  panelId: string;
}

/**
 * WAI-ARIA tab pattern: roving tabindex, Left/Right/Home/End move the active
 * tab. Panels are owned by the caller so the tab list stays a pure control.
 */
export function Tabs<T extends string>({
  items,
  active,
  onChange,
  label,
  panelId,
}: TabsProps<T>) {
  const refs = useRef(new Map<T, HTMLButtonElement | null>());
  const [moveFocus, setMoveFocus] = useState(false);

  useEffect(() => {
    if (!moveFocus) {
      return;
    }
    refs.current.get(active)?.focus();
    setMoveFocus(false);
  }, [moveFocus, active]);

  const index = items.findIndex((item) => item.id === active);

  const select = (next: T, focus: boolean) => {
    if (focus) {
      setMoveFocus(true);
    }
    if (next !== active) {
      onChange(next);
    }
  };

  const step = (delta: number) => {
    if (items.length === 0) {
      return;
    }
    const from = index < 0 ? 0 : index;
    const next = items[(from + delta + items.length) % items.length];
    if (next) {
      select(next.id, true);
    }
  };

  return (
    <div className="tabs" role="tablist" aria-label={label}>
      {items.map((item) => {
        const selected = item.id === active;
        return (
          <button
            key={item.id}
            ref={(node) => {
              refs.current.set(item.id, node);
            }}
            type="button"
            role="tab"
            id={`tab-${panelId}-${item.id}`}
            aria-selected={selected}
            aria-controls={panelId}
            tabIndex={selected ? 0 : -1}
            className={`tab${selected ? " tab--active" : ""}`}
            onClick={() => select(item.id, false)}
            onKeyDown={(event) => {
              switch (event.key) {
                case "ArrowRight":
                case "ArrowDown":
                  event.preventDefault();
                  step(1);
                  break;
                case "ArrowLeft":
                case "ArrowUp":
                  event.preventDefault();
                  step(-1);
                  break;
                case "Home":
                  event.preventDefault();
                  select(items[0].id, true);
                  break;
                case "End": {
                  event.preventDefault();
                  const last = items[items.length - 1];
                  select(last.id, true);
                  break;
                }
                default:
                  break;
              }
            }}
          >
            {item.label}
          </button>
        );
      })}
    </div>
  );
}

export interface TabPanelProps {
  id: string;
  activeTabId: string;
  label?: string;
  children: ReactNode;
}

export function TabPanel({ id, activeTabId, label, children }: TabPanelProps) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => enterSurface(ref.current), [activeTabId]);
  return (
    <div
      ref={ref}
      className="tab-panel"
      id={id}
      role="tabpanel"
      aria-labelledby={`tab-${id}-${activeTabId}`}
      aria-label={label}
      tabIndex={0}
    >
      {children}
    </div>
  );
}
