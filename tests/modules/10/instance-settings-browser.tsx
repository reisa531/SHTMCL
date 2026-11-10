import { createRoot } from "react-dom/client";
import { InstanceNameSettings } from "../../../apps/desktop/src/ui/components/InstanceNameSettings";

const root = createRoot(document.getElementById("root")!);
const checks: {label: string; passed: boolean}[] = [];
const wait = () => new Promise(resolve => setTimeout(resolve, 60));
function check(value: unknown, label: string) {
  checks.push({label, passed: Boolean(value)});
  if (!value) throw Error(label);
}
let record = {id:"school",name:"原名称",revision:3,state:"ready"};
let mode = "ok", writes = 0, reads = 0, refreshed = 0;
let finish: (() => void) | undefined;
const native = window as any;
native.__TAURI_INTERNALS__ = {};
native.__routeInvoke = async (command: string, args: any) => {
  const request = args.request;
  if (command === "dispatch_core_read") {
    reads++;
    if (mode === "read-failure") throw Error("offline");
    return {type:"completed",result:{instance:{...record}}};
  }
  check(command === "dispatch_core_mutation", "write uses native mutation bridge");
  writes++;
  check(request.expectedRevision === record.revision, "write carries displayed revision");
  check(Object.keys(request.payload.patch).join() === "name", "write only changes name");
  if (mode === "conflict") throw {code:"instances.revision.stale"};
  await new Promise<void>(resolve => {finish = resolve;});
  record = {...record,name:request.payload.patch.name,revision:record.revision+1};
  if (mode === "readback-failure") mode = "read-failure";
  return {type:"completed",result:{instance:{...record}}};
};
const button = (label: string) => [...document.querySelectorAll<HTMLButtonElement>("button")].find(b => b.textContent === label)!;
function input(value: string) {
  const field = document.querySelector("input")!;
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")!.set!.call(field,value);
  field.dispatchEvent(new Event("input",{bubbles:true}));
}
async function mount(key: string) {
  root.render(<InstanceNameSettings key={key} instanceId="school" onRefresh={() => {refreshed++;}} />);
  await wait(); await wait();
}
(async () => {
  try {
    await mount("initial");
    check(document.querySelector("input")!.value === "原名称", "initial name comes from snapshot");
    check(button("保存名称").disabled, "unchanged name cannot be submitted");
    input("  "); await wait();
    check(button("保存名称").disabled, "blank name cannot be submitted");
    input(" 校园世界 "); await wait(); button("保存名称").click(); await wait();
    check(writes === 1, "single request sent");
    check(button("正在保存").disabled && button("重新读取").disabled, "pending controls prevent repeated writes and stale reloads");
    finish!(); await wait(); await wait();
    check(record.name === "校园世界" && refreshed === 1 && reads >= 2, "save refreshes parent and reads result back");
    check(document.body.textContent?.includes("名称已保存"), "success shown only after readback");
    await mount("reopen");
    check(document.querySelector("input")!.value === "校园世界", "remounted component reads saved name");
    mode = "conflict"; input("冲突草稿"); await wait(); button("保存名称").click(); await wait();
    check(document.body.textContent?.includes("其他操作修改"), "conflict asks for fresh read");
    check(document.querySelector("input")!.value === "冲突草稿" && button("保存名称").disabled, "failed save preserves draft and blocks blind retry");
    mode = "ok"; button("重新读取").click(); await wait();
    check(document.querySelector("input")!.value === "校园世界", "explicit reload restores stored value");
    record.state = "busy"; button("重新读取").click(); await wait();
    check(document.querySelector("input")!.disabled && button("保存名称").disabled, "busy snapshot is read only");
    record.state = "ready"; button("重新读取").click(); await wait();
    mode = "readback-failure"; input("已提交待确认"); await wait(); button("保存名称").click(); await wait(); finish!(); await wait(); await wait();
    check(!document.body.textContent?.includes("名称已保存") && document.body.textContent?.includes("未能确认保存结果"), "readback failure does not announce success");
    mode = "ok"; button("重新读取").click(); await wait();
    check(document.querySelector("input")!.value === "已提交待确认", "reload recovers committed result without another mutation");
    check(writes === 3, "no automatic retries");
    document.documentElement.dataset.testStatus = "passed";
  } catch (error) {
    checks.push({label:String(error),passed:false});
    document.documentElement.dataset.testStatus = "failed";
  }
  document.getElementById("report")!.textContent = JSON.stringify(checks);
})();
