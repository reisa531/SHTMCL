// Prints the rendered element counts per page and the re-render counts for one
// live task event.
// Run: node tests/modules/10/measure.mjs
//
// "elements" counts opening tags in the static render of the real shell: it is
// the size of the component tree that first paint actually produces for one
// page, not a score or an estimate.
//
// "rows re-rendered" comes from `memo-sim.mjs`: React bails out of a memoised
// component when its comparator returns true, so the model counts the rows whose
// comparator rejects the new props. `memo.test.mjs` asserts the component really
// uses that comparator.

import { loadBundle } from "./harness.mjs";
import { simulateOneEvent } from "./memo-sim.mjs";

const ui = await loadBundle();
const measured = ui.measure();

const rows = Object.entries(measured).map(([key, value]) => ({
  page: key,
  elements: value.elements,
  markupBytes: value.bytes,
}));

console.log("renderer: react-dom/server renderToStaticMarkup");
console.log("");
console.log("page".padEnd(28), "elements".padStart(9), "markup bytes".padStart(13));
for (const row of rows) {
  console.log(
    row.page.padEnd(28),
    String(row.elements).padStart(9),
    String(row.markupBytes).padStart(13),
  );
}
console.log("");
console.log(`pages measured: ${rows.length}`);

console.log("");
console.log("re-render count for one task.progress event (evaluator D1)");
console.log(
  "rows".padStart(5),
  "before: shallow compare".padStart(25),
  "after: explicit comparator".padStart(26),
  "  touched rows",
);
for (const count of [4, 40, 200]) {
  const result = simulateOneEvent(ui, count, Math.floor(count / 2));
  console.log(
    String(result.rows).padStart(5),
    String(result.before).padStart(25),
    String(result.after).padStart(26),
    `  ${result.afterIds.join(", ")}`,
  );
}
