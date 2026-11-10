// Module 11 · 测试入口：不依赖 node --test 的目录 glob，确保在所有 Node ≥ 18 版本上可直接运行。
// 运行：node tests/modules/11/run.mjs   成功退出码 0，失败退出码 1。
import { spawnSync } from 'node:child_process';

const files = [
  'content.test.mjs',
  'journey-model.test.mjs',
  'availability-intents.test.mjs',
];

// stdio: 'inherit' — the Windows sandbox forbids piped grandchild stdio.
const result = spawnSync(process.execPath, ['--test', ...files], { stdio: 'inherit' });
if (result.error) {
  console.error(result.error);
  process.exit(1);
}
process.exit(result.status ?? 1);
