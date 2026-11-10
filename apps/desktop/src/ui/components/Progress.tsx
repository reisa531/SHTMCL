import type { CSSProperties } from "react";

import type { ProgressUnit } from "../../contract";

const UNIT_LABELS: Record<ProgressUnit, string> = {
  bytes: "B",
  items: "项",
  steps: "步",
};

export interface ProgressProps {
  /** Screen-reader name, e.g. "任务进度". */
  label: string;
  current: number;
  total: number | null;
  unit: ProgressUnit;
  /** Optional stable text for assistive tech; defaults to the visible text. */
  valueText?: string;
}

export function progressText(
  current: number,
  total: number | null,
  unit: ProgressUnit,
): string {
  const suffix = UNIT_LABELS[unit];
  return total === null
    ? `${current} ${suffix}`
    : `${current} / ${total} ${suffix}`;
}

export function progressPercent(
  current: number,
  total: number | null,
): number | null {
  if (total === null || total <= 0) {
    return null;
  }
  return Math.min(100, Math.max(0, Math.round((current / total) * 100)));
}

/**
 * Determinate only when the core reports a positive total. The bar width comes
 * from a CSS custom property set inline: it is a measured value, not a style
 * literal, and every colour/size still comes from tokens.
 */
export function Progress({
  label,
  current,
  total,
  unit,
  valueText,
}: ProgressProps) {
  const percent = progressPercent(current, total);
  const text = progressText(current, total, unit);

  return (
    <div className="progress">
      <div
        className="progress__track"
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={percent === null ? undefined : 100}
        aria-valuenow={percent === null ? undefined : percent}
        aria-valuetext={valueText ?? text}
        data-indeterminate={percent === null ? "true" : undefined}
        style={
          percent === null
            ? undefined
            : ({ "--progress-value": `${percent}%` } as CSSProperties)
        }
      >
        <span className="progress__fill" />
      </div>
      <span className="progress__text">{text}</span>
    </div>
  );
}
