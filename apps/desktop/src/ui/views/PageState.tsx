import { useId, type ReactNode } from "react";

import { Banner } from "../components/Banner";
import { Button } from "../components/Button";
import { InlineError } from "../components/InlineError";
import { Skeleton } from "../components/Skeleton";
import { uiLabels } from "../labels";
import type { PageState } from "./page-state";

export interface PageStateViewProps {
  state: PageState;
  /** Announced while the region loads. */
  loadingLabel: string;
  /** Rendered for `empty`, `busy`, `partial` and `success`. */
  children: ReactNode;
  onRetry?: () => void;
  retryPending?: boolean;
  retryLabel?: string;
  errorTitle?: string;
  errorBody?: string;
  busyTitle?: string;
  busyBody?: string;
  partialTitle?: string;
  partialBody?: string;
  /** Page wording for the "data source not connected" state. */
  notConnectedTitle?: string;
  notConnectedBody?: string;
  /**
   * Keep already-loaded content under an error message. Used where a refresh
   * failure must not erase the rows the user can still see.
   */
  preserveContent?: boolean;
}

/**
 * Presents the page states. `empty` content stays with the page (each page owns
 * its own empty copy), while loading, error, busy, partial and not_connected get
 * a consistent in-place surface.
 *
 * The error branch uses `InlineError` with the retry control attached, so the
 * recovery action sits next to the message instead of in a global banner.
 */
export function PageStateView({
  state,
  loadingLabel,
  children,
  onRetry,
  retryPending = false,
  retryLabel,
  errorTitle = uiLabels.errorTitle,
  errorBody = uiLabels.errorBody,
  busyTitle = uiLabels.busyTitle,
  busyBody = uiLabels.busyBody,
  partialTitle = uiLabels.partialTitle,
  partialBody = uiLabels.partialBody,
  notConnectedTitle = uiLabels.notConnectedTitle,
  notConnectedBody = uiLabels.notConnectedBody,
  preserveContent = false,
}: PageStateViewProps) {
  const errorId = useId();
  const showContent =
    state === "empty" ||
    state === "busy" ||
    state === "partial" ||
    state === "success" ||
    (state === "error" && preserveContent);

  return (
    <div className="page-state" data-page-state={state}>
      {state === "loading" ? <Skeleton label={loadingLabel} lines={4} /> : null}

      {state === "error" ? (
        <InlineError id={errorId} message={errorTitle} hint={errorBody}>
          {onRetry ? (
            <Button
              variant="secondary"
              size="sm"
              icon="refresh"
              pending={retryPending}
              onClick={onRetry}
            >
              {retryLabel ?? uiLabels.taskRefresh}
            </Button>
          ) : null}
        </InlineError>
      ) : null}

      {state === "busy" ? (
        <Banner tone="info" title={busyTitle} body={busyBody} />
      ) : null}

      {state === "partial" ? (
        <Banner tone="warn" title={partialTitle} body={partialBody} />
      ) : null}

      {state === "not_connected" ? (
        <Banner tone="info" title={notConnectedTitle} body={notConnectedBody} />
      ) : null}

      {showContent ? children : null}
    </div>
  );
}
