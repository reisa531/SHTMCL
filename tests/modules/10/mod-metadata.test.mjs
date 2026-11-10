import test from "node:test";
import assert from "node:assert/strict";
import { loadBundle } from "./harness.mjs";
const {modMetadataApi, parseModMetadata} = await loadBundle();
const ready = {status:"ready",loader:"fabric",id:"example",version:"1.2",name:"Example",environment:"client",dependencies:[{id:"minecraft",versions:["~1.20",">=1.21"]}]};
const result = (metadata = ready, id = "school", fileName = "example.jar") => ({type:"completed",result:{instance:{id,presentation:{modMetadata:{fileName,metadata}}}}});

test("metadata inspection requests one exact instance and file, preserving declarations", async () => {
  let request;
  const api = modMetadataApi(async value => {request=value; return result();});
  assert.deepEqual(await api.inspect("school","example.jar"),ready);
  assert.deepEqual(request.payload,{command:"instances.snapshot",instanceId:"school",modFileName:"example.jar"});
  assert.equal(request.operationId,undefined);
  assert.equal(Object.hasOwn(ready,"compatible"),false);
});

test("metadata unavailable, unsupported, invalid, limited and busy remain distinct", () => {
  for (const status of ["unavailable","unsupported","invalid","limited","busy"]) {
    assert.deepEqual(parseModMetadata({status,dependencies:[]}),{status});
  }
});

test("metadata rejects mismatched instance and selected file identities", async () => {
  await assert.rejects(modMetadataApi(async () => result(ready,"other")).inspect("school","example.jar"));
  await assert.rejects(modMetadataApi(async () => result(ready,"school","other.jar")).inspect("school","example.jar"));
});

test("old snapshots without metadata remain unavailable instead of empty declarations", async () => {
  const api = modMetadataApi(async () => ({type:"completed",result:{instance:{id:"school"}}}));
  assert.deepEqual(await api.inspect("school","example.jar"),{status:"unavailable"});
});

test("malformed metadata and oversized dependency declarations are rejected", () => {
  for (const value of [null,[],{status:"ready"},{...ready,version:17},{...ready,environment:"guessed"},{...ready,name:"bad\nname"},
    {...ready,dependencies:Array(65).fill(ready.dependencies[0])},{...ready,dependencies:[{id:"minecraft",versions:[]}]},
    {...ready,dependencies:[{id:"minecraft",versions:[17]}]}]) assert.throws(() => parseModMetadata(value));
});
