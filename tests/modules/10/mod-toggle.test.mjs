import test from "node:test";
import assert from "node:assert/strict";
import {loadBundle} from "./harness.mjs";
const {modToggleApi,modToggleError}=await loadBundle();
const file={instanceId:"school",revision:1,state:"ready",fileName:"a.jar",enabled:true,bytes:18,sha256:"a".repeat(64)};
const completed=file=>({type:"completed",result:{file}});
test("mod toggle binds revision, exact file and digest then confirms target via readback",async()=>{
  const reads=[],writes=[];
  const api=modToggleApi(async request=>{reads.push(request);return completed({...file,revision:2,fileName:"a.jar.disabled",enabled:false});},async request=>{writes.push(request);return {type:"completed",result:{instance:{id:"school",revision:2}}};});
  assert.equal((await api.toggle(file)).enabled,false);
  assert.equal(writes[0].expectedRevision,1);
  assert.notEqual(writes[0].requestId,writes[0].operationId);
  assert.deepEqual(writes[0].payload,{command:"instances.mod.toggle",instanceId:"school",fileName:"a.jar",enabled:false,sha256:file.sha256});
  assert.deepEqual(reads[0].payload,{command:"instances.mod.snapshot",instanceId:"school",fileName:"a.jar.disabled"});
  assert.equal(reads[0].operationId,undefined);
});
test("readback failure never becomes an assumed toggle success",async()=>{
  let writes=0;
  const api=modToggleApi(async()=>{throw Error("offline");},async()=>{writes++;return {type:"completed",result:{instance:{id:"school",revision:2}}};});
  await assert.rejects(api.toggle(file));assert.equal(writes,1);
});
test("mod snapshots reject wrong identities and invalid digests or revisions",async()=>{
  for(const value of [{...file,instanceId:"other"},{...file,fileName:"other.jar"},{...file,sha256:"x"},{...file,revision:0},{...file,enabled:false}]){
    await assert.rejects(modToggleApi(async()=>completed(value)).snapshot("school","a.jar"));
  }
});
test("mismatched mutation acknowledgements and changed readback content are rejected",async()=>{
  await assert.rejects(modToggleApi(undefined,async()=>({type:"completed",result:{instance:{id:"other",revision:2}}})).toggle(file));
  const api=modToggleApi(async()=>completed({...file,revision:2,fileName:"a.jar.disabled",enabled:false,sha256:"b".repeat(64)}),async()=>({type:"completed",result:{instance:{id:"school",revision:2}}}));
  await assert.rejects(api.toggle(file));
});
test("core failures preserve their identity with no automatic retries",async()=>{
  for(const code of ["instances.mod.target_exists","instances.mod.changed","instances.revision.stale","instances.instance.running"]){
    let writes=0;const error={code};
    await assert.rejects(modToggleApi(undefined,async()=>{writes++;throw error;}).toggle(file),value=>value===error);
    assert.equal(writes,1);assert.doesNotMatch(modToggleError(error),/未能确认/);
  }
  assert.doesNotMatch(modToggleError(new Error("raw stack")),/raw stack/);
});
