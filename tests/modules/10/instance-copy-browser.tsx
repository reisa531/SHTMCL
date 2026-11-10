// Real React/DOM interactions with a controlled bridge, never a native acceptance claim.
import { createRoot } from "react-dom/client";
import { InstanceCopyDialog } from "../../../apps/desktop/src/ui/components/InstanceCopyDialog";
import { encodeInstanceReference } from "../../../apps/desktop/src/ui/views/instance-copy-api";

const context = (globalThis as any).__COPY_TEST__ = {
  calls: [] as any[], operationId: "", target: "", state: "running", reads: 0,
  clipboard: "", pasted: "", cancellations: 0, completions: 0, mode: "normal",
};
const source = {id:"source",name:"测试实例",version:{gameVersion:"1.7.10",loader:"vanilla"}};
const root = createRoot(document.getElementById("root")!);
const report: {name:string;passed:boolean;error?:string}[] = [];
let key=0;
const delay = (ms=25) => new Promise(resolve=>setTimeout(resolve,ms));
function check(value:unknown,message:string):asserts value {if(!value) throw Error(message);}
async function waitFor(test:()=>boolean) {
  for(let i=0;i<200;i++){if(test())return;await delay();}throw Error("UI condition timed out");
}
function button(text:string):HTMLButtonElement {
  const result=[...document.querySelectorAll("button")].find(b=>b.textContent===text);
  check(result,`Missing button ${text}`);return result;
}
async function render(paste=false,mode="normal") {
  Object.assign(context,{calls:[],operationId:"",target:"",state:"running",reads:0,cancellations:0,completions:0,mode});
  root.render(<InstanceCopyDialog key={++key} open source={paste?null:source} games={[source]} onClose={()=>{}} onComplete={()=>context.completions++} />);
  await waitFor(()=>!!document.querySelector("dialog[open]"));await delay();
}
Object.defineProperty(navigator,"clipboard",{configurable:true,value:{
  readText:async()=>context.pasted,writeText:async(text:string)=>{context.clipboard=text;},
}});
document.documentElement.dataset.motion="reduced";

async function run(name:string,work:()=>Promise<void>) {
  try{await work();report.push({name,passed:true});}
  catch(error){report.push({name,passed:false,error:String(error)});}
  document.getElementById("report")!.textContent=JSON.stringify(report);
}
async function main(){
  await run("copy reference uses selected instance",async()=>{
    await render();button("复制实例引用").click();await waitFor(()=>!!context.clipboard);
    check(context.clipboard===encodeInstanceReference("source"),"incorrect reference");
    await waitFor(()=>document.body.textContent?.includes("实例引用已复制")===true);
  });
  await run("paste, include saves and create a confirmed copy",async()=>{
    await render(true);context.pasted=encodeInstanceReference("source");button("读取剪贴板").click();
    await waitFor(()=>!!document.querySelector('input[type="checkbox"]'));
    (document.querySelector('input[type="checkbox"]') as HTMLInputElement).click();await delay();
    button("创建副本").click();await waitFor(()=>context.calls.length===1);
    check(context.calls[0].payload.includeSaves===true,"save checkbox was ignored");
    check(context.calls[0].payload.name==="测试实例 副本","name not carried");
    context.state="succeeded";
    await waitFor(()=>context.completions===1);
    check(document.body.textContent?.includes("副本已创建。"),"terminal acknowledgement missing");
  });
  await run("cancel goes through the shared task and never reports success",async()=>{
    await render();button("创建副本").click();await waitFor(()=>context.calls.length===1);
    check(context.calls[0].payload.includeSaves===false,"saves should default off");
    await waitFor(()=>[...document.querySelectorAll("button")].some(b=>b.textContent==="取消复制"&&!b.disabled));
    button("取消复制").click();await waitFor(()=>context.cancellations===1);
    await waitFor(()=>document.body.textContent?.includes("已取消复制。")===true);
    check(context.completions===0,"cancelled copy reported success");
  });
  await run("unknown pasted source cannot submit a copy",async()=>{
    await render(true);context.pasted=encodeInstanceReference("elsewhere");button("读取剪贴板").click();
    await waitFor(()=>document.body.textContent?.includes("不在当前实例库")===true);
    check(button("创建副本").disabled,"unknown source can be copied");check(context.calls.length===0,"unexpected mutation");
  });
  await run("transient observation failure clears after confirmed success",async()=>{
    await render(false,"transient");button("创建副本").click();await waitFor(()=>context.calls.length===1);
    await waitFor(()=>document.body.textContent?.includes("连接暂时中断")===true);
    context.state="succeeded";await waitFor(()=>document.body.textContent?.includes("副本已创建。")===true);
    check(context.completions===1,"missing completion callback");
    check(!document.querySelector('[role="alert"]'),"stale connection error remained after success");
  });
  await run("missing task recovers a committed copy from the native index",async()=>{
    await render(false,"missing");button("创建副本").click();await waitFor(()=>context.calls.length===1);
    await waitFor(()=>document.body.textContent?.includes("副本已创建。")===true);
    check(context.completions===1,"missing completion callback");
    check(context.reads>=3,"index recovery happened without retrying observation");
    check(!document.querySelector('[role="alert"]'),"stale missing task error remained");
  });
  document.body.dataset.testStatus=report.every(x=>x.passed)?"passed":"failed";
}
main().catch(error=>{document.body.dataset.testStatus="failed";document.getElementById("report")!.textContent=String(error);});
