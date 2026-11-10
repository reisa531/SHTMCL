import { createRoot } from "react-dom/client";
import { DownloadsPage } from "../../../apps/desktop/src/ui/pages/DownloadsPage";
import type { Task } from "../../../apps/desktop/src/contract";
const root=createRoot(document.getElementById('root')!);
const entry={operationId:'download-one',taskId:'task-one',name:'Minecraft',version:'1.20.4',resourceType:'game_environment' as const,destination:{kind:'cache' as const,path:'F:/SHTMCL/SHTMCL-data/cache'}};
const base:Task={id:entry.taskId,operationId:entry.operationId,attemptGeneration:1,state:'running',phase:'download_files',cancelRequested:false};
const results:any[]=[];
const wait=()=>new Promise(r=>setTimeout(r,60));
function check(ok:unknown,label:string){results.push({label,passed:!!ok});if(!ok)throw Error(label);}
function box(selector:string){const e=document.querySelector(selector);if(!e)throw Error('missing '+selector);const r=e.getBoundingClientRect();return {x:r.x,y:r.y,right:r.right};}
async function render(task:Task,phase:'ready'|'error'='ready'){
 root.render(<DownloadsPage tasks={[task]} presentations={[entry]} hasSnapshot phase={phase} refreshing={false} feedback={{}} pendingCancel={new Set()} onCancel={()=>{}} onRefresh={()=>{}}/>);await wait();
}
(async()=>{try{
 let reference:any;
 for(const [current,total] of [[9,100],[10,100],[100,100],[10,null]]){
 await render({...base,progress:{current,total,unit:'bytes'}});
 const now={heading:box('h1'),tabs:box('[role=tablist]'),track:box('.progress__track'),current};
 if(reference){check(Math.abs(now.heading.y-reference.heading.y)<.5,'heading stable '+current+'/'+total);check(Math.abs(now.tabs.y-reference.tabs.y)<.5,'tabs stable '+current+'/'+total);check(Math.abs(now.track.right-reference.track.right)<.5,'track width stable '+current+'/'+total);}
 else reference=now;
 check(!document.querySelector('.banner'),'no duplicate busy banner '+current+'/'+total);
 }
 await render({...base,state:'cancelled',progress:{current:10,total:null,unit:'bytes'}});
 check(Math.abs(box('[role=tablist]').y-reference.tabs.y)<.5,'tabs stable after stop');
 check(!document.querySelector('#task-task-one'),'stopped task leaves active list');
 const unfinished=[...document.querySelectorAll<HTMLButtonElement>('[role=tab]')].find(x=>x.textContent?.includes('未完成'))!;unfinished.click();await wait();
 check(!!document.querySelector('#task-task-one'),'stopped record available in unfinished');
 await render({...base},'error');check(!!document.querySelector('[data-page-state=error]'),'refresh failure stays visible');
 document.body.dataset.testStatus='passed';
 }catch(e){results.push({error:String(e),passed:false});document.body.dataset.testStatus='failed';}
 document.getElementById('report')!.textContent=JSON.stringify(results);
})();
