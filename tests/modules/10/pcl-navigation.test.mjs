import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";
const ui = await loadBundle();
const games = [
  { id: "first", name: "原版", version: { gameVersion: "1.21", loader: "vanilla" } },
  { id: "fabric", name: "建筑存档", version: { gameVersion: "1.20.1", loader: "fabric" } },
];

test("selection resolves exact ids and safely falls back after removal", () => {
  assert.equal(ui.resolveSelectedGame(games, "fabric"), games[1]);
  assert.equal(ui.resolveSelectedGame(games, "removed"), games[0]);
  assert.equal(ui.resolveSelectedGame([], "fabric"), undefined);
  assert.equal(ui.resolveSelectedGame(games, "fab"), games[0]);
});

test("selection storage survives blocked profiles and does not persist game payloads", () => {
  const values = new Map();
  const storage = { getItem: (key) => values.get(key), setItem: (key, value) => values.set(key, value), removeItem: (key) => values.delete(key) };
  ui.saveSelectedGame("fabric", storage);
  assert.equal(ui.readSelectedGame(storage), "fabric");
  assert.deepEqual([...values], [[ui.SELECTED_GAME_KEY, "fabric"]]);
  ui.saveSelectedGame(undefined, storage);
  assert.equal(ui.readSelectedGame(storage), undefined);
  const blocked = { getItem() { throw Error("blocked"); }, setItem() { throw Error("blocked"); }, removeItem() { throw Error("blocked"); } };
  assert.equal(ui.readSelectedGame(blocked), undefined);
  assert.doesNotThrow(() => ui.saveSelectedGame("fabric", blocked));
  assert.equal(ui.readSelectedGame({ getItem: () => "  " }), undefined);
});

test("launch and management display the same selected game", () => {
  const selected = ui.resolveSelectedGame(games, "fabric");
  const launch = ui.renderSelectedStart(selected);
  assert.match(launch, /建筑存档/);
  assert.match(launch, /TestPlayer/);
  assert.match(launch, /选择游戏/);
  const management = ui.renderSelectedGames(games, selected.id);
  assert.match(management, /aria-current="true"[^>]*aria-label="建筑存档"|aria-label="建筑存档"[^>]*aria-current="true"/);
  assert.match(management, /返回启动/);
  assert.equal((management.match(/aria-pressed="true"/g) ?? []).length, 1);
});

test("download acquires content while task route alone observes operations", () => {
  const hub = ui.renderShell("downloads");
  for (const label of ["原版游戏", "Mod", "整合包", "数据包", "资源包", "光影包", "本地资源"]) assert.ok(hub.includes(label), label);
  assert.match(hub, /搜索版本/);
  assert.doesNotMatch(hub, /没有进行中的下载|取消任务/);
  const tasks = ui.renderShell("tasks");
  assert.match(tasks, /没有进行中的下载/);
  assert.doesNotMatch(tasks, /搜索版本/);
});
