import type { TaskState } from "../../contract";
import { taskStateLabels } from "../../ux/copy";
import { Icon, type IconName } from "../Icons";

export type StatusTone = "idle" | "active" | "waiting" | "ok" | "error";

const STATE_TONES: Record<TaskState, StatusTone> = {
  queued: "idle",
  running: "active",
  paused: "waiting",
  waiting_user: "waiting",
  succeeded: "ok",
  failed: "error",
  cancelled: "idle",
};

const TONE_ICONS: Record<StatusTone, IconName> = {
  idle: "dot",
  active: "clock",
  waiting: "clock",
  ok: "check",
  error: "alert",
};

/**
 * Task state. Text carries the meaning; the icon and colour only reinforce it,
 * so the pill stays readable with colour removed.
 */
export function StatusPill({ state }: { state: TaskState }) {
  const tone = STATE_TONES[state];
  return (
    <span className={`status-pill status-pill--${tone}`} data-state={state}>
      <span className="status-pill__icon" aria-hidden="true">
        <Icon name={TONE_ICONS[tone]} size={11} />
      </span>
      {taskStateLabels[state]}
    </span>
  );
}
