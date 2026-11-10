import { findEsbuild, repoRoot } from "./harness.mjs";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { pathToFileURL } from "node:url";
import path from "node:path";
import { spawnSync } from "node:child_process";
const { build } = await import(pathToFileURL(findEsbuild()).href);
const output = path.join(repoRoot, "work/validation/10/startup-flow-20261022-01/browser");
mkdirSync(output, { recursive: true });
const bridge = `
const context = () => globalThis.__STARTUP_TEST__;
export const isTauriRuntime = () => true;
export const readAppInfo = async () => ({name:'SHTMCL',version:'0.1.0',native:true});
export async function readShellSnapshot() {
 const x=context(); x.snapshots++; if(x.snapshotFailure) throw Error('bridge unavailable');
 return {mode:'native',modules:['instances','launch'],tasks:x.omitTask?[]:structuredClone(x.tasks)};
}
export async function listenEvents(listener) { const x=context(); x.listener=listener; return ()=>{if(x.listener===listener)x.listener=null;}; }
export const readEvents = async () => ({events:[],truncated:false});
export const isNewerThanCursor = (event,cursor) => !cursor || event.sequence>cursor.sequence;
export const openDownloadDestination = async () => {};
export async function requestCancel(taskId,attemptGeneration) {context().cancellations.push({taskId,attemptGeneration}); context().tasks[0].cancelRequested=true;return{decision:'requested'};}
export async function dispatchCoreRead(request) {
 const x=context(); let result;
 switch(request.payload.command) {
 case 'launch.identity.current': result={identity:structuredClone(x.identity)};break;
 case 'instances.list': result={instances:structuredClone(x.games)};break;
 case 'launch.installations.list': result={entries:[]};break;
 default: throw Error('Unexpected read: '+request.payload.command);
 }
 return {type:'completed',result};
}
export async function dispatchCoreMutation(request) {
 const x=context();
 if(request.payload.command==='launch.identity.select') {
   if(x.selectGate) await x.selectGate;
   x.identity={kind:'offline',username:request.payload.username,uuid:'a'.repeat(32)};
   return {type:'completed',result:{identity:x.identity}};
 }
 if(request.payload.command!=='launch.game.start') throw Error('Unexpected mutation');
 x.mutations.push(request);
 if(x.launchFailure) throw {code:'launch.java.not_found'};
 const task={id:'launch-'+x.mutations.length,operationId:request.operationId,instanceId:request.payload.instanceId,
 attemptGeneration:1,state:'running',phase:'resource_prepare',cancelRequested:false,evidence:[]};
 x.tasks=[task];return{type:'accepted',taskId:task.id,operationId:task.operationId};
}
`;
await build({ entryPoints: [path.join(repoRoot, "tests/modules/10/startup-flow-browser.tsx")], bundle: true,
 jsx: "automatic", platform: "browser", format: "iife", nodePaths: [path.join(repoRoot, "apps/desktop/node_modules")],
 outfile: path.join(output, "test.js"), plugins: [{ name: "bridge", setup(b) {
 b.onLoad({filter:/[\\/]src[\\/]bridge\.ts$/}, () => ({contents:bridge,loader:"js"}));
 }}] });
writeFileSync(path.join(output, "index.html"), `<!doctype html><meta charset="utf-8"><style>${readFileSync(path.join(repoRoot,"apps/desktop/src/styles.css"),"utf8")}</style><div id="root"></div><pre id="report"></pre><script>${readFileSync(path.join(output,"test.js"),"utf8")}</script>`);
const result = spawnSync("C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe", ["--headless=new", "--no-first-run", "--force-prefers-reduced-motion",
 `--user-data-dir=${path.join(output,`profile-${Date.now()}`)}`, "--dump-dom", "--virtual-time-budget=20000",
 pathToFileURL(path.join(output,"index.html")).href], {encoding:"utf8",windowsHide:true,timeout:35000,maxBuffer:5*1024*1024});
writeFileSync(path.join(output,"dom.html"),result.stdout??"");
writeFileSync(path.join(output,"stderr.log"),result.stderr??"");
const passed = result.status===0 && /data-test-status="passed"/.test(result.stdout??"");
const report = {exitCode:result.status,passed,results:result.stdout?.match(/<pre id="report">([\s\S]*?)<\/pre>/)?.[1],error:result.error?.message};
writeFileSync(path.join(output,"result.json"),JSON.stringify(report,null,2));
console.log(JSON.stringify(report,null,2));process.exit(passed?0:1);
