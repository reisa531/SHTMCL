import test from "node:test";
import assert from "node:assert/strict";
import {loadBundle} from "./harness.mjs";
const {fabricApi,gameInstallApi}=await loadBundle();
test("Fabric catalogue carries the selected game version and stable flag",async()=>{
  let sent;
  const entries=[{version:"0.16.10",stable:true},{version:"0.20.1",stable:false}];
  assert.deepEqual(await fabricApi(async request=>{sent=request;return{type:"completed",result:{gameVersion:"1.20.4",entries}};}).versions("1.20.4"),entries);
  assert.deepEqual(sent.payload,{command:"launch.fabric.versions",gameVersion:"1.20.4"});assert.equal(sent.operationId,undefined);
});
test("Fabric rejects mismatched game versions and malformed loader identities",async()=>{
  for(const result of [{gameVersion:"1.20.1",entries:[]},{gameVersion:"1.20.4",entries:[{version:"../bad",stable:true}]},{gameVersion:"1.20.4",entries:[{version:"0.16.10",stable:"true"}]},{gameVersion:"1.20.4",entries:[{version:"0.16.10",stable:true},{version:"0.16.10",stable:true}]}]){
    await assert.rejects(fabricApi(async()=>({type:"completed",result})).versions("1.20.4"));
  }
  await assert.rejects(fabricApi(async()=>{throw Error("must not dispatch");}).versions("../file"));
});
test("unsupported Fabric games are an explicit empty catalogue",async()=>{
  assert.deepEqual(await fabricApi(async()=>({type:"completed",result:{gameVersion:"1.7.10",entries:[]}})).versions("1.7.10"),[]);
});
test("Fabric installation retains the pinned loader and target instance",async()=>{
  let sent;
  const api=gameInstallApi(undefined,async request=>{sent=request;return{type:"accepted",operationId:request.operationId,taskId:"fabric-task"};});
  await api.install("1.20.4","fabric-instance","模组环境",undefined,{kind:"fabric",version:"0.16.10"});
  assert.deepEqual(sent.payload,{command:"launch.install",versionId:"1.20.4",instanceId:"fabric-instance",name:"模组环境",loader:"fabric",loaderVersion:"0.16.10"});
});
