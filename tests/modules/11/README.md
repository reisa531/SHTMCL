# tests/modules/11 — UX 文案与旅程

无第三方依赖，使用 Node 内置测试（Node ≥ 18）。

| 文件 | 内容 |
| --- | --- |
| `content.test.mjs` | 文案目录：必选场景、禁用修辞/免责、证据门控、未知 code、动作能力映射、无副作用 |
| `journey-model.test.mjs` | JourneyContext：重复事件与重复点击、取消确认、取消后审批竞态、旧代次/旧 revision、授权返回、快照恢复 |
| `availability-intents.test.mjs` | ActionAvailability 投影与 intent→桥命令映射 |
| `fixtures.mjs` | 00 任务/事件信封样例 |
| `run.mjs` | 测试入口（固定文件列表，兼容旧 Node），退出码 0/1 |
| `demo.mjs` | 可读走查剧本，打印各阶段与断言，失败退出码 1 |

```powershell
node tests/modules/11/run.mjs
node tests/modules/11/demo.mjs
```
