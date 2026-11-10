import { createRoot } from "react-dom/client";
import { ResourceCatalog } from "../../../apps/desktop/src/ui/views/ResourceCatalog";
import type { InstalledGame } from "../../../apps/desktop/src/ui/views/game-install-api";
const root = createRoot(document.getElementById("root")!);
const wait = () => new Promise(resolve => setTimeout(resolve, 40));
const checks: {label:string;passed:boolean}[] = [];
function check(value:unknown,label:string) { checks.push({label,passed:Boolean(value)}); if(!value) throw Error(label); }
const games:InstalledGame[] = [{id:"fabric",name:"校园生存",version:{gameVersion:"1.20.4",loader:"fabric"}}, {id:"forge",name:"Forge 世界",version:{gameVersion:"1.20.1",loader:"forge"}}, {id:"vanilla",name:"原版",version:{gameVersion:"1.20.4",loader:"vanilla"}}];
const pending: {payload:any;resolve:(v:any)=>void;reject:(v:any)=>void}[] = [];
(window as any).__TAURI_INTERNALS__ = {};
(window as any).__routeInvoke = (command:string,args:any) => {
  check(command==="dispatch_core_read","all catalog actions use read bridge");
  return new Promise((resolve,reject)=>pending.push({payload:args.request.payload,resolve,reject}));
};
const find = (label:string) => [...document.querySelectorAll<HTMLButtonElement>("button")].find(b=>b.textContent===label)!;
const take = (command:string) => { const i=pending.findIndex(p=>p.payload.command===command); if(i<0) throw Error(`missing ${command}`); return pending.splice(i,1)[0]; };
const done = (call:ReturnType<typeof take>,result:any) => call.resolve({type:"completed",result});
function select(value:string) { const control=document.querySelector<HTMLSelectElement>(".catalog-instance select")!; control.value=value; control.dispatchEvent(new Event("change",{bubbles:true})); }
function page(instanceId:string,offset=0) { return {projectId:"sodium",instanceId,target:games.find(g=>g.id===instanceId)!.version,offset,total:instanceId==="vanilla"?0:21,
  versions:instanceId==="vanilla"?[]:[{versionId:`v${offset}`,name:offset?"Second page":"Sodium Release",versionNumber:"0.5+1.20.4",kind:"release",published:"2026-10-30",files:[{name:"sodium.jar",bytes:1024,primary:true}],
  dependencies:[{kind:"required",projectId:"fabric-api",versionId:"exact-version"},{kind:"optional",projectId:"iris"}]}]}; }
async function search() {find("搜索").click();await wait();const call=take("content.search");done(call,{connected:true,queryId:call.payload.queryId,projects:[{provider:"modrinth",projectId:"sodium",slug:"sodium",title:"Sodium"}]});await wait();}
(async()=>{try {
  root.render(<ResourceCatalog category="mods" games={games}/>);await wait();
  check(pending.length===0,"catalog opening does not send automatic requests");
  select("fabric");await wait();await search();
  find("详情与版本").click();await wait();
  const project=take("content.modrinth.project");const first=take("content.modrinth.versions");
  check(first.payload.instanceId==="fabric" && !first.payload.target,"version request sends instance identity only");
  done(project,{provider:"modrinth",projectId:"sodium",slug:"sodium",title:"Sodium",description:"<img src=x onerror=alert(1)>",clientSide:"required"});done(first,page("fabric"));await wait();
  check(document.getElementById("root")!.textContent?.includes("<img src=x"),"provider text rendered as text");
  check(!document.querySelector(".mod-project img"),"provider text cannot create markup");
  check(document.activeElement?.id==="mod-project-heading","detail heading receives keyboard focus");
  document.querySelector("details")!.open=true;await wait();
  check(document.getElementById("root")!.textContent?.includes("必需") && document.getElementById("root")!.textContent?.includes("exact-version"),"required and pinned dependencies visible");
  check(document.getElementById("root")!.textContent?.includes("sodium.jar"),"version files visible");
  check(![...document.querySelectorAll("button")].some(b=>/安装/.test(b.textContent??"")),"catalog does not claim unfinished install support");
  find("下一页").click();await wait();const next=take("content.modrinth.versions");check(next.payload.offset===20,"page advances by bounded offset");done(next,page("fabric",20));await wait();
  check(document.getElementById("root")!.textContent?.includes("Second page"),"second page displayed");
  check(find("下一页").disabled,"last page disables next");
  select("forge");await wait();const oldProject=take("content.modrinth.project");const old=take("content.modrinth.versions");
  select("vanilla");await wait();const freshProject=take("content.modrinth.project");const fresh=take("content.modrinth.versions");
  done(freshProject,{provider:"modrinth",projectId:"sodium",slug:"sodium",title:"Sodium",description:"Fast rendering",clientSide:"required"});done(fresh,page("vanilla"));await wait();
  done(oldProject,{provider:"modrinth",projectId:"sodium",slug:"sodium",title:"OLD PROJECT",description:"Old",clientSide:"required"});done(old,{...page("forge"),versions:[{...page("forge").versions[0],name:"STALE VERSION"}]});await wait();
  check(!document.getElementById("root")!.textContent?.includes("STALE VERSION") && !document.getElementById("root")!.textContent?.includes("OLD PROJECT"),"late old-instance results ignored");
  check(document.getElementById("root")!.textContent?.includes("此实例是原版游戏"),"vanilla guidance displayed");
  find("返回搜索结果").click();await wait();check(document.activeElement?.getAttribute("type")==="search","back returns keyboard focus to search");
  select("fabric");await wait();find("搜索").click();await wait();take("content.search").reject({code:"content.source.unavailable"});await wait();
  check(document.querySelector('[role="alert"]')?.textContent?.includes("无法连接"),"network error remains an error");
  check(!document.getElementById("root")!.textContent?.includes("没有匹配项目"),"network failure is not an empty result");
  find("重试").click();await wait();const retry=take("content.search");done(retry,{connected:true,queryId:retry.payload.queryId,projects:[]});await wait();
  check(document.getElementById("root")!.textContent?.includes("没有匹配项目"),"real empty response displayed after retry");
  find("搜索").click();await wait();const unmounted=take("content.search");root.render(<p>Other page</p>);await wait();done(unmounted,{connected:true,queryId:unmounted.payload.queryId,projects:[]});await wait();
  check(document.getElementById("root")?.textContent==="Other page","unmounted catalog ignores late response");
  document.body.dataset.testStatus="passed";
}catch(error){document.body.dataset.testStatus="failed";checks.push({label:String(error),passed:false});}
document.getElementById("report")!.textContent=JSON.stringify(checks,null,2);
})();
