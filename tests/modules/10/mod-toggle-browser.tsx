import {createRoot} from "react-dom/client";
import {InstanceManagementPanel} from "../../../apps/desktop/src/ui/components/InstanceManagementPanel";
const root=createRoot(document.getElementById("root")!);
const checks:{label:string;passed:boolean}[]=[];
const wait=()=>new Promise(resolve=>setTimeout(resolve,60));
function check(value:unknown,label:string){checks.push({label,passed:Boolean(value)});if(!value)throw Error(label);}
const visible=()=>document.getElementById("root")!.textContent??"";
const button=(label:string)=>[...document.querySelectorAll<HTMLButtonElement>("button")].find(b=>b.textContent===label)!;
let fileName="a.jar",revision=1,enabled=true,mode="ok",writes=0,refreshes=0;
let finish:(()=>void)|undefined;
const inventory=()=>({status:"ready" as const,files:[{fileName,enabled,bytes:18}]});
const native=window as any;native.__TAURI_INTERNALS__={};
native.__routeInvoke=async(command:string,args:any)=>{
  const r=args.request,p=r.payload;
  if(command==="dispatch_core_mutation"){
    writes++;check(p.command==="instances.mod.toggle"&&p.fileName===fileName,"exact file mutation");
    check(r.expectedRevision===revision&&p.sha256==="a".repeat(64),"revision and digest bound");
    if(mode==="collision")throw {code:"instances.mod.target_exists"};
    await new Promise<void>(resolve=>{finish=resolve;});
    enabled=p.enabled;fileName=enabled?fileName.slice(0,-9):fileName+".disabled";revision++;
    return {type:"completed",result:{instance:{id:"school",revision}}};
  }
  check(command==="dispatch_core_read","only normal read bridge");
  if(p.command==="instances.mod.snapshot"){
    if(mode==="readback-failure"&&p.fileName!=="a.jar")throw Error("offline");
    return {type:"completed",result:{file:{instanceId:"school",revision,state:mode==="busy"?"busy":"ready",fileName:p.fileName,enabled,bytes:18,sha256:"a".repeat(64)}}};
  }
  return {type:"completed",result:{instance:{id:"school",presentation:{modInventory:inventory(),...(p.modFileName?{modMetadata:{fileName:p.modFileName,metadata:{status:"unsupported"}}}:{})}}}};
};
async function mount(key:string){root.render(<InstanceManagementPanel key={key} initialTab="mods" game={{id:"school",name:"School",version:{gameVersion:"1.20.4",loader:"fabric"},presentation:{modInventory:inventory()}}} targetPath="instances/school" onRefresh={()=>{refreshes++;}}/>);await wait();await wait();}
async function select(){document.querySelector<HTMLButtonElement>(".instance-mod-row")!.click();await wait();await wait();}
(async()=>{try{
  await mount("start");await select();
  check(!button("禁用此模组").disabled,"ready selected mod can be disabled");
  button("禁用此模组").click();await wait();
  check(button("正在处理").disabled&&button("刷新").disabled&&document.querySelector<HTMLButtonElement>(".instance-mod-row")!.disabled,"pending blocks repeated submit and selection changes");
  check(writes===1,"one mutation submitted");finish!();await wait();await wait();
  check(visible().includes("a.jar.disabled 已禁用")&&document.querySelector(".instance-mod-row")?.textContent?.includes("a.jar.disabled"),"confirmed result refreshes actual inventory");
  check(refreshes===1,"parent refresh invoked after confirmation");
  await select();button("启用此模组").click();await wait();finish!();await wait();await wait();
  check(fileName==="a.jar"&&visible().includes("a.jar 已启用"),"enable roundtrip uses latest snapshot");
  mode="collision";await select();button("禁用此模组").click();await wait();
  check(visible().includes("目标文件名已存在")&&fileName==="a.jar","collision preserves existing file state");
  check(button("禁用此模组").disabled,"failure blocks blind retry");
  mode="busy";button("重新读取状态").click();await wait();
  check(button("禁用此模组").disabled&&visible().includes("实例当前不可修改"),"running/busy instance remains read only");
  mode="ok";button("重新读取状态").click();await wait();mode="readback-failure";
  button("禁用此模组").click();await wait();finish!();await wait();await wait();
  check(visible().includes("未能确认操作结果")&&!visible().includes("已禁用。"),"readback failure does not announce success");
  mode="ok";button("刷新").click();await wait();await wait();
  check(document.querySelector(".instance-mod-row")?.textContent?.includes("a.jar.disabled"),"manual refresh recovers committed outcome");
  check(writes===4,"no automatic mutation retries");
  document.documentElement.dataset.testStatus="passed";
}catch(error){checks.push({label:String(error),passed:false});document.documentElement.dataset.testStatus="failed";}
document.getElementById("report")!.textContent=JSON.stringify(checks);
})();
