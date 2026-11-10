// Reconciliation model for the task list.
//
// There is no DOM test renderer in this workspace (no jsdom / vitest /
// react-test-renderer), so re-render counts are derived from the same inputs
// React uses: `React.memo(Component, compare)` bails out when
// `compare(prevProps, nextProps)` is true, otherwise it re-renders the row.
// `memo.test.mjs` asserts that `TaskRow.compare` *is* the comparator used here,
// so this model cannot drift from the component.
//
// Props are rebuilt from scratch every pass and callbacks are fresh closures,
// which is the worst case a re-rendering parent produces.

export function buildTasks(count) {
  return Array.from({ length: count }, (_, index) => ({
    id: `task-${index + 1}`,
    operationId: `op-${index + 1}`,
    attemptGeneration: 1,
    state: "running",
    phase: "download",
    progress: { current: index, total: 100, unit: "items" },
    cancelRequested: false,
  }));
}

export function buildProps(ui, tasks) {
  return tasks.map((task) =>
    ui.taskRowPropsFor(
      task,
      {},
      new Set(),
      // eslint-disable-next-line no-empty-function
      () => {},
      () => {},
    ),
  );
}

/** React's default shallow compare, i.e. the behaviour before an explicit one. */
export function shallowEqualProps(a, b) {
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) {
    return false;
  }
  return keys.every((key) => Object.is(a[key], b[key]));
}

export function countRenders(prevProps, nextProps, compare) {
  let rendered = 0;
  const indexes = [];
  for (let index = 0; index < nextProps.length; index += 1) {
    if (!compare(prevProps[index], nextProps[index])) {
      rendered += 1;
      indexes.push(index);
    }
  }
  return { rendered, indexes };
}

/**
 * One live event applied to `rows` tasks: returns how many rows would re-render
 * with the explicit comparator, and how many with the default shallow compare.
 */
export function simulateOneEvent(ui, rows, targetIndex = 0) {
  const tasks = buildTasks(rows);
  const prevProps = buildProps(ui, tasks);

  const target = tasks[targetIndex];
  const nextTasks = tasks.map((task) =>
    task.id === target.id
      ? {
          ...task,
          progress: {
            ...task.progress,
            current: (task.progress.current ?? 0) + 1,
          },
        }
      : task,
  );
  const nextProps = buildProps(ui, nextTasks);

  const before = countRenders(prevProps, nextProps, shallowEqualProps);
  const after = countRenders(prevProps, nextProps, ui.taskRowPropsAreEqual);

  return {
    rows,
    targetId: target.id,
    before: before.rendered,
    after: after.rendered,
    afterIndexes: after.indexes,
    afterIds: after.indexes.map((index) => nextTasks[index].id),
  };
}
