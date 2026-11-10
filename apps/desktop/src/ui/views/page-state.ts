// Page-level state selection.
//
// Pure so the mapping from real snapshot facts to a presented state can be
// asserted without a DOM. Order matters: a load failure outranks stale data,
// "no data source yet" outranks "nothing yet", missing data outranks activity,
// and activity outranks partial progress.
//
// `not_connected` exists because `ShellSnapshot` carries tasks and module ids
// only: an instances/rooms page must be able to say "the data source is not
// connected" instead of claiming the list holds zero items (evaluator D3).

export type PageState =
  | "loading"
  | "empty"
  | "error"
  | "partial"
  | "busy"
  | "success"
  | "not_connected";

/** What the shell knows about the snapshot itself. */
export type SnapshotPhase = "loading" | "ready" | "error";

export interface PageStateInput {
  /** `loading` only while no snapshot exists; refreshes keep the last data. */
  phase: SnapshotPhase;
  /** False when the page's data source is not part of the snapshot at all. */
  hasDataSource: boolean;
  /** Items the page can actually show from the snapshot. */
  itemCount: number;
  /** Items in a non-terminal task state. */
  activeCount: number;
  /** Items whose progress total is unknown (`total: null`). */
  unknownTotalCount: number;
}

export function selectPageState(input: PageStateInput): PageState {
  if (input.phase === "error") {
    return "error";
  }
  if (input.phase === "loading") {
    return "loading";
  }
  if (!input.hasDataSource) {
    return "not_connected";
  }
  if (input.itemCount === 0) {
    return "empty";
  }
  if (input.activeCount > 0) {
    return "busy";
  }
  if (input.unknownTotalCount > 0) {
    return "partial";
  }
  return "success";
}
