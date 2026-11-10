// Prints, per page and per snapshot phase, which page-state branch rendered and
// whether it claims a row count.
// Run: node tests/modules/10/page-states.mjs
//
// Evidence for evaluator defects D2 (no loading/error surface on start /
// multiplayer / discover) and D3 ("not connected" rendered as "0 items").

import { loadBundle } from "./harness.mjs";

const ui = await loadBundle();

const PAGES = ["start", "instances", "multiplayer", "discover", "downloads", "settings"];
const PHASES = ["loading", "error", "ready"];

function probe(markup) {
  const stateMatch = markup.match(/data-page-state="([a-z_]+)"/);
  return {
    state: stateMatch ? stateMatch[1] : "(none)",
    skeletons: (markup.match(/class="skeleton"/g) ?? []).length,
    alerts: (markup.match(/role="alert"/g) ?? []).length,
    retry: /刷新状态/.test(markup) ? "yes" : "no",
    emptyClaim: /还没有/.test(markup) ? "YES" : "no",
    notConnected: /尚未接入/.test(markup) ? "yes" : "no",
  };
}

console.log("page".padEnd(12), "phase".padEnd(9), "state".padEnd(14), "skeleton".padStart(9), "alert".padStart(6), "retry".padStart(6), "  empty-claim  not-connected");

let failures = 0;
for (const page of PAGES) {
  for (const phase of PHASES) {
    const result = probe(ui.renderShell(page, { phase }));
    console.log(
      page.padEnd(12),
      phase.padEnd(9),
      result.state.padEnd(14),
      String(result.skeletons).padStart(9),
      String(result.alerts).padStart(6),
      result.retry.padStart(6),
      `  ${result.emptyClaim.padEnd(12)}${result.notConnected}`,
    );
    // A page must not conclude anything while loading or after a failure.
    if ((phase === "loading" || phase === "error") && result.emptyClaim === "YES") {
      failures += 1;
    }
    // Settings reads no snapshot data, so it has no loading surface by design.
    if (phase === "loading" && page !== "settings" && result.skeletons === 0) {
      failures += 1;
    }
    if (phase === "error" && result.alerts === 0) {
      failures += 1;
    }
  }
}

console.log("");
console.log(`violations: ${failures}`);
process.exit(failures === 0 ? 0 : 1);
