import { findEsbuild, repoRoot } from './harness.mjs';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
const { build } = await import(pathToFileURL(findEsbuild()).href);
const output = path.resolve(process.argv[2]);
mkdirSync(output, { recursive: true });
const fixture = process.argv[3] ?? 'tests/modules/10/download-instance-route-browser.tsx';
await build({ entryPoints: [path.resolve(repoRoot, fixture)],
  bundle: true, jsx: 'automatic', platform: 'browser', format: 'iife',
  nodePaths: [path.join(repoRoot, 'apps/desktop/node_modules')], outfile: path.join(output, 'test.js'),
  plugins: [{name: 'native-fixture', setup(builder) {
    builder.onResolve({filter: /^@tauri-apps\/api\/(core|event|window)$/}, (args) => ({path: args.path, namespace: 'native-fixture'}));
    builder.onLoad({filter: /.*/, namespace: 'native-fixture'}, () => ({contents:
      'export const invoke=(command,args)=>window.__routeInvoke(command,args); export const listen=async()=>()=>{}; export const getCurrentWindow=()=>({isMaximized:async()=>false,onResized:async()=>()=>{},minimize:async()=>{},close:async()=>{}});'}));
  }}] });
writeFileSync(path.join(output, 'index.html'), `<!doctype html><meta charset="utf-8"><style>${readFileSync(path.join(repoRoot, 'apps/desktop/src/styles.css'), 'utf8')}</style><div id="root"></div><pre id="report" hidden></pre><script>${readFileSync(path.join(output, 'test.js'), 'utf8')}</script>`);
const browser = process.env.SHTMCL_TEST_BROWSER ?? 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe';
const run = spawnSync(browser, ['--headless=new', '--no-first-run',
  '--force-prefers-reduced-motion', '--window-size=1184,960', `--user-data-dir=${path.join(output, 'profile-' + Date.now())}`,
  '--dump-dom', '--virtual-time-budget=8000', pathToFileURL(path.join(output, 'index.html')).href],
  {encoding: 'utf8', timeout: 30000, windowsHide: true, maxBuffer: 5 * 1024 * 1024});
writeFileSync(path.join(output, 'dom.html'), run.stdout ?? '');
writeFileSync(path.join(output, 'stderr.log'), run.stderr ?? '');
const result = {exitCode: run.status, passed: run.status === 0 && /data-test-status="passed"/.test(run.stdout ?? ''),
  report: run.stdout?.match(/<pre id="report"[^>]*>([\s\S]*?)<\/pre>/)?.[1]};
writeFileSync(path.join(output, 'result.json'), JSON.stringify(result, null, 2));
console.log(result); process.exit(result.passed ? 0 : 1);
