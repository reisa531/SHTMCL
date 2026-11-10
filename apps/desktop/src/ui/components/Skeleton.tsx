export interface SkeletonProps {
  /** Announced while the region is loading. */
  label: string;
  lines?: number;
}

/**
 * Placeholder shape for a region that is still loading. The shapes are hidden
 * from assistive tech; the label is announced once as a status.
 */
export function Skeleton({ label, lines = 3 }: SkeletonProps) {
  return (
    <div className="skeleton" role="status">
      <span className="sr-only">{label}</span>
      <div className="skeleton__lines" aria-hidden="true">
        {Array.from({ length: lines }, (_, index) => (
          <span
            key={index}
            className="skeleton__line"
            data-width={index === 0 ? "wide" : "normal"}
          />
        ))}
      </div>
    </div>
  );
}
