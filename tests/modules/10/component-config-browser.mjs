import {findEsbuild,repoRoot} from "./harness.mjs";
import {readFileSync,writeFileSync,mkdirSync} from "node:fs";
import {pathToFileURL} from "node:url";
import path from "node:path";
import {spawnSync} from "node:child_process";
const {build}=await import(pathToFileURL(findEsbuild()).href);
const narrow=process.argv.includes("--narrow");
const output=path.join(repoRoot,"work/validation/10/component-refresh-20261023-01",narrow?"browser-narrow":"browser");mkdirSync(output,{recursive:true});
const bridge=`
export async function dispatchCoreRead(request){const x=globalThis.__FABRIC_TEST__;x.reads.push(request);let result;
if(request.payload.command==='launch.versions.list')result={entries:[{id:'1.20.4',type:'release'},{id:'1.7.10',type:'release'}],total:2};
else if(request.payload.command==='launch.fabric.versions'){await new Promise(r=>setTimeout(r,50));if(x.fabricMode==='fail')throw Error('fixture offline');result={gameVersion:request.payload.gameVersion,entries:request.payload.gameVersion==='1.20.4'&&x.fabricMode!=='remove'?[{version:'0.16.10',stable:true}]:[]};}
else throw Error('unexpected read');return {type:'completed',result};}
export async function dispatchCoreMutation(request){globalThis.__FABRIC_TEST__.mutations.push(request);return{type:'accepted',taskId:'install',operationId:request.operationId};}
export async function readShellSnapshot(){return{tasks:[]};}
`;
await build({entryPoints:[path.join(repoRoot,"tests/modules/10/fabric-browser.tsx")],bundle:true,jsx:"automatic",platform:"browser",format:"iife",nodePaths:[path.join(repoRoot,"apps/desktop/node_modules")],outfile:path.join(output,"test.js"),plugins:[{name:"bridge",setup(b){b.onLoad({filter:/[\\/]src[\\/]bridge\.ts$/},()=>({contents:bridge,loader:"js"}));}}]});
writeFileSync(path.join(output,"index.html"),`<!doctype html><meta charset="utf-8"><style>${readFileSync(path.join(repoRoot,"apps/desktop/src/styles.css"),"utf8")}</style><div id="root" style="max-width:900px;margin:24px auto"></div><pre id="report" style="display:none"></pre><script>${readFileSync(path.join(output,"test.js"),"utf8")}</script>`);
const result=spawnSync("C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",["--headless=new","--no-first-run","--force-prefers-reduced-motion",`--screenshot=${path.join(output,"configuration.png")}`,narrow?"--window-size=700,740":"--window-size=1184,960",`--user-data-dir=${path.join(output,`profile-${Date.now()}`)}`,"--dump-dom","--virtual-time-budget=12000",pathToFileURL(path.join(output,"index.html")).href],{encoding:"utf8",windowsHide:true,timeout:30000,maxBuffer:4*1024*1024});
writeFileSync(path.join(output,"dom.html"),result.stdout??"");writeFileSync(path.join(output,"stderr.log"),result.stderr??"");
const passed=result.status===0&&/data-test-status="passed"/.test(result.stdout??"");console.log(JSON.stringify({exitCode:result.status,passed,results:result.stdout?.match(/<pre id="report"[^>]*>([\s\S]*?)<\/pre>/)?.[1],error:result.error?.message},null,2));
writeFileSync(path.join(output,"result.json"),JSON.stringify({passed,exitCode:result.status,results:result.stdout?.match(/<pre id="report"[^>]*>([\s\S]*?)<\/pre>/)?.[1]},null,2));
if(passed){
 const preview=spawnSync("C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",["--headless=new","--no-first-run","--force-prefers-reduced-motion",`--user-data-dir=${path.join(output,`preview-${Date.now()}`)}`,`--screenshot=${path.join(output,"versions.png")}`,"--window-size=1184,740","--virtual-time-budget=1000",pathToFileURL(path.join(output,"index.html")).href+"?preview=versions"],{encoding:"utf8",windowsHide:true,timeout:30000});
 if(preview.status!==0)throw Error("Version preview failed");
}
process.exit(passed?0:1);
