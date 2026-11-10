import {findEsbuild,repoRoot} from "./harness.mjs";
import {mkdirSync,writeFileSync,readFileSync} from "node:fs";
import path from "node:path";
import {pathToFileURL} from "node:url";
import {spawnSync} from "node:child_process";

const root=repoRoot;
const esbuildPath=findEsbuild();
if(!esbuildPath) throw Error("Workspace esbuild runtime is unavailable");
const {build}=await import(pathToFileURL(esbuildPath).href);
const output=path.join(root,"work/validation/instance-copy/20261022-01/browser");
mkdirSync(output,{recursive:true});
const bridge=`
const c=()=>globalThis.__COPY_TEST__;
export async function dispatchCoreMutation(request){let x=c();x.calls.push(request);x.operationId=request.operationId;x.target=request.payload.newInstanceId;return {type:'accepted',operationId:x.operationId,taskId:'copy-task'};}
export async function dispatchCoreRead(request){let x=c();return {type:'completed',result:{instances:[{id:x.target,name:'测试实例 副本',version:{gameVersion:'1.7.10',loader:'vanilla'}}]}};}
export async function readShellSnapshot(){let x=c();x.reads++;if(x.mode==='transient'&&x.reads===1)throw Error('controlled disconnection');return {tasks:x.mode==='missing'?[]:[{id:'copy-task',operationId:x.operationId,attemptGeneration:1,state:x.state}]};}
export async function requestCancel(id,generation){if(id!=='copy-task'||generation!==1)throw Error('wrong task');let x=c();x.cancellations++;x.state='cancelled';return {type:'requested'};}
`;
await build({entryPoints:[path.join(root,"tests/modules/10/instance-copy-browser.tsx")],bundle:true,format:"iife",jsx:"automatic",platform:"browser",nodePaths:[path.join(root,"apps/desktop/node_modules")],outfile:path.join(output,"test.js"),
  plugins:[{name:"controlled-bridge",setup(b){b.onLoad({filter:/[\\/]src[\\/]bridge\.ts$/},()=>({contents:bridge,loader:"js"}));}}]});
const css=readFileSync(path.join(root,"apps/desktop/src/styles.css"),"utf8");
writeFileSync(path.join(output,"index.html"),`<!doctype html><html lang="zh-CN"><meta charset="utf-8"><title>Instance copy controlled UI test</title><style>${css}</style><body><div id="root"></div><pre id="report"></pre><script>${readFileSync(path.join(output,"test.js"),"utf8")}</script></body></html>`);
const profile=path.join(output,`profile-${Date.now()}`);
const result=spawnSync("C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",["--headless=new","--no-first-run",`--user-data-dir=${profile}`,"--dump-dom","--virtual-time-budget=20000",pathToFileURL(path.join(output,"index.html")).href],{encoding:"utf8",windowsHide:true,timeout:45000,maxBuffer:4*1024*1024});
writeFileSync(path.join(output,"dom.html"),result.stdout??"");writeFileSync(path.join(output,"stderr.log"),result.stderr??"");
const match=result.stdout?.match(/<pre id="report">([\s\S]*?)<\/pre>/);
const passed=result.status===0&&/data-test-status="passed"/.test(result.stdout??"");
console.log(JSON.stringify({exitCode:result.status,passed,results:match?.[1],error:result.error?.message},null,2));
process.exit(passed?0:1);
