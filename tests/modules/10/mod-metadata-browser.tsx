import { createRoot } from "react-dom/client";
import { ModMetadataDetails } from "../../../apps/desktop/src/ui/components/ModMetadataDetails";

const root = createRoot(document.getElementById("root")!);
const checks: {label:string;passed:boolean}[] = [];
const wait = () => new Promise(resolve => setTimeout(resolve,60));
function check(value:unknown,label:string) { checks.push({label,passed:Boolean(value)}); if (!value) throw Error(label); }
const ready = {status:"ready",loader:"fabric",id:"example",version:"1.2",name:"Example",environment:"client",dependencies:[{id:"minecraft",versions:["~1.20",">=1.21"]}]};
const requests: any[] = [];
const native = window as any;
native.__TAURI_INTERNALS__ = {};
let response: any = ready;
let deferred: ((value: unknown) => void) | undefined;
native.__routeInvoke = async (command:string,args:any) => {
  check(command === "dispatch_core_read", "metadata uses read bridge");
  requests.push(args.request);
  const fileName = args.request.payload.modFileName;
  const value = response === "deferred" ? await new Promise(resolve => {deferred=resolve;}) : response;
  if (value === "failure") throw Error("raw private error");
  return {type:"completed",result:{instance:{id:"school",presentation:{modMetadata:{fileName,metadata:value}}}}};
};
async function mount(fileName:string) { root.render(<ModMetadataDetails key={fileName} instanceId="school" fileName={fileName} />); await wait(); await wait(); }
(async () => { try {
  response="deferred"; await mount("old.jar");
  check(document.getElementById("root")!.textContent?.includes("正在读取模组声明"),"loading is explicit");
  response=ready; await mount("selected.jar.disabled");
  check(document.getElementById("root")!.textContent?.includes("Example") && document.getElementById("root")!.textContent?.includes("1.2"),"declared name and version rendered");
  check(document.getElementById("root")!.textContent?.includes("~1.20 或 >=1.21"),"dependency alternatives preserved");
  check(document.getElementById("root")!.textContent?.includes("尚未核对当前实例"),"declarations are not a compatibility verdict");
  deferred!({...ready,name:"Stale response"}); await wait();
  check(!document.getElementById("root")!.textContent?.includes("Stale response"),"late response from previous selection ignored");
  check(requests[1].payload.modFileName === "selected.jar.disabled" && !requests[1].operationId,"disabled file has exact read-only target");
  for (const status of ["unsupported","invalid","limited","unavailable","busy"]) {
    response={status}; await mount(status+".jar");
    check(!document.querySelector('[aria-label="模组文件声明"]'),"no invented declaration for "+status);
    check(Boolean(document.querySelector('[role="status"]')),"status visible for "+status);
  }
  response="failure"; await mount("failure.jar");
  check(document.getElementById("root")!.textContent?.includes("无法读取所选文件") && !document.getElementById("root")!.textContent?.includes("raw private"),"read failure stays unavailable without raw error");
  response=ready;
  [...document.querySelectorAll<HTMLButtonElement>("button")].find(b=>b.textContent==="重试读取声明")!.click(); await wait();
  check(document.getElementById("root")!.textContent?.includes("Example"),"retry reads current selection successfully");
  document.documentElement.dataset.testStatus="passed";
} catch(error) { checks.push({label:String(error),passed:false}); document.documentElement.dataset.testStatus="failed"; }
document.getElementById("report")!.textContent=JSON.stringify(checks);
})();
