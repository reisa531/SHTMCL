# SHTMCL public contract v1

状态：M0 最小冻结草案。实现所有者：00。变更入口见本文件末尾。

本契约只定义跨模块共享的传输形状与不变量；实例、启动、下载等业务 DTO
由各模块在自己的目录内定义，但不得复制这里的状态类型。

## 1. 编码

- JSON 对象字段一律 camelCase。
- Rust 枚举值一律 snake_case，`ModuleId` 为 kebab-case（当前均为单个单词）。
- 可选字段缺省时通常省略，而不是发送虚构的默认值；`TaskProgress.total` 例外，
  它以 `null` 表示“尚不可估计”。
- ID 是不透明 UTF-8 字符串，1—128 字节，不含空白或控制字符。
- 时间戳尚未进入 v1 信封；模块不得用本地时钟代替 `revision` / `sequence`。

## 2. 三个计数器不能互换

| 字段 | 含义 | 作用域 |
| --- | --- | --- |
| `revision` | 实体状态版本 | 单个实例/房间/服务器等实体 |
| `attempt_generation` | 同一任务第几次尝试 | 单个任务，重试时递增 |
| `sequence` | 事件流内递增序号 | 单个 operation/subscription 流 |

`sequence` 不是全局总序。消费者按 `event_id` 去重；发现缺口或重连时，先读取快照和
最后应用的 `StreamCursor`，再续接增量。旧代次的响应和事件不能写入新代次结果。

## 3. 命令

请求：

```json
{
  "requestId": "req-1",
  "operationId": "op-1",
  "entityId": "instance-1",
  "expectedRevision": 7,
  "payload": {}
}
```

- 读取可以省略 `operationId`；变更操作必须携带，否则返回
  `command.operation_required`，分类 `invalid_request`。
- **命令名固定放在 `payload.command`**，格式与错误码相同 `^[a-z][a-z0-9._-]{2,63}$`
  （如 `launch.start`、`download.resume`）。模块必须用 `CommandRequest::command()`
  读取，不要自己索引 `payload`：缺失返回 `command.name_required`，格式非法返回
  `command.name_invalid`，两者分类都是 `invalid_request`。不接受 `action` 等别名，
  否则会出现两套真相。
- **命令名第一段必须是模块 id**（`launch.`、`content.`、`rooms.`、`servers.`、
  `diagnostics.`、`instances.`、`downloads.`、`accounts.`）。应用层在唯一分发点校验，
  不匹配返回 `command.name_mismatch`。这样日志与权限判断不必猜归属；同模块可以有多个族
  （`launch.prepare`、`launch.runtimes.list`、`launch.quickplay.capabilities` 都合法）。
- `expectedRevision` 只用于需要版本核对的变更；提交前冲突必须返回当前版本并让调用方
  重算，不得静默覆盖。
- 响应二选一：
  - `{ "type": "completed", "result": ... }`
  - `{ "type": "accepted", "operationId": "op-1", "taskId": "task-1" }`
- 长任务立即返回 `accepted`，不阻塞 UI 到安装/下载完成。

UI 的意图首先映射为命令；`request_id` 对应意图 ID，变更业务对应 `operation_id`。
不存在第二套 UI 任务调度。

## 4. Task

```json
{
  "id": "task-1",
  "operationId": "op-1",
  "instanceId": "instance-1",
  "inputRevision": 7,
  "attemptGeneration": 1,
  "state": "running",
  "phase": "download",
  "progress": { "current": 512, "total": 1024, "unit": "bytes" },
  "cancelRequested": false,
  "evidence": ["resumable_ranges"]
}
```

- `id`、`operationId`、`attemptGeneration`、`state`、`phase`、`cancelRequested` 必有。
- `instanceId`、`inputRevision`、`progress`、`result`、`error`、`evidence` 可选；`evidence` 为空时省略。
- `evidence` 是模块**实际产生**的证据标记（如 `resumable_ranges`、`original_file_preserved`、
  `binding_committed`、`safe_stop_reached`）。断言“已保留/已完成/已连接”的文案只有在标记存在时
  才能显示；缺标记时只能使用中性文案。每次重试清空，因为证据属于单次尝试。
- `phase` 是模块定义的稳定阶段 ID，不是 `state` 的同义词；下载 100% 后应切换为校验/安装阶段。
- 进度单位：`bytes`、`items`、`steps`。`total: null` 表示未知总量。
- 任务状态：`queued`、`running`、`paused`、`waiting_user`、`succeeded`、`failed`、`cancelled`。
- 允许的迁移：
  - `queued -> running | cancelled | failed`
  - `running -> paused | waiting_user | succeeded | failed | cancelled`
  - `paused -> running | cancelled | failed`
  - `waiting_user -> running | cancelled | failed`
  - 终态不可再迁移；重试通过 `begin_retry` 提升代次并回到 `queued`。

## 5. 取消

```json
{ "taskId": "task-1", "attemptGeneration": 2 }
```

决定值：`requested`、`already_requested`、`not_cancellable`、`stale_generation`、`not_found`。

取消只置 `cancelRequested = true`；只有任务到达安全终止/回退点，模块才发
`cancelled`。代次不匹配时返回 `stale_generation`，不能影响当前代次。进程被杀不等于
取消完成；需要等待模块确认或恢复记录落盘。

## 6. 事件

```json
{
  "eventId": "event-1",
  "schemaVersion": 1,
  "operationId": "op-1",
  "entityId": "instance-1",
  "revision": 7,
  "sequence": 12,
  "attemptGeneration": 1,
  "kind": "task.progress",
  "payload": { "taskId": "task-1", "current": 512, "total": 1024 }
}
```

- `eventId`、`schemaVersion`、`operationId`、`revision`、`sequence`、
  `attemptGeneration`、`kind`、`payload` 必有；`entityId` 可选。
- 已知 kind：`task.state`、`task.progress`、`task.result`，其 payload 由
  `TaskStatePayload`、`TaskProgressPayload`、`TaskResultPayload` 定义（均为 camelCase）。
  模块可增加 kind，但必须在自己的交接中登记。
- 任务事件描述操作进度而不是实体变更，因此 `revision` 取该任务计划所依据的
  `inputRevision`；没有输入版本时为 `0`。真正改动实体的模块事件必须携带真实 revision。
- `payload` 由 kind 定义；公共信封不解析业务内容。
- 消费端按 `event_id` 去重，并在本地保存 `StreamCursor { operationId, sequence }`。
- 事件不得携带凭据、访问令牌或游戏会话句柄。

## 7. 错误

```json
{
  "code": "task.not_found",
  "category": "not_found",
  "retryable": false,
  "userAction": "reload",
  "detailRef": "task:task-m0-sample",
  "operationId": "op-m0-sample"
}
```

- `code` 稳定、可检索，格式 `^[a-z][a-z0-9._-]{2,63}$`；UI 不直接展示原始异常。
- 分类：`invalid_request`、`conflict`、`not_found`、`permission_denied`、
  `unsupported`、`network`、`storage`、`cancelled`、`internal`。
- **错误码命名空间归模块所有**：`crates/domain` 只定义信封与公共码（`task.*`、`command.*`、
  `module.*`、`internal.*`）。模块在自己的 `error.rs` 里声明本模块的码，第一段必须是模块 id
  （`content.pack.path_escape`、`launch.java.not_found` …）。00 **不**集中登记所有模块码：
  那会让公共 crate 依赖模块内部知识，并让每个新错误都变成跨组阻塞。
  格式一致性由 `scripts/check-codes.mjs` 机械检查（见 §8）。
- 同一规则适用于命令名、事件 kind 与任务 phase：第一段是所有者命名空间，且不得出现
  `packs.*`、`runtimes.*` 这类没有归属的前缀（命令名还会在分发点被硬性拒绝）。
- `userAction` 是稳定动作 ID（如 `retry`、`reload`、`choose_instance`、
  `reauthenticate`、`open_details`），由 UX 映射为本地化句子。
- `detailRef` 是不透明引用，技术详情按需读取；信封不带堆栈或凭据。
- 公共错误码当前包括：`task.not_found`、`task.stale_generation`、
  `task.not_cancellable`、`task.invalid_transition`、`command.operation_required`、
  `command.name_required`、`command.name_invalid`、`command.name_mismatch`、
  `module.unavailable`、`internal.unavailable`。

## 8. 版本与变更

- `CONTRACT_VERSION = 1`，`SchemaVersion` 当前只接受 1。
- 新增可选字段保持兼容；改变含义、删除字段或改变枚举值需提升契约版本并同步
  Rust/TypeScript 两端。
- TypeScript 镜像位于 `apps/desktop/src/contract.ts`；样例校验脚本为
  `scripts/check-contract.mjs`。该脚本检查版本、样例 JSON 与基本不变量，不等价于
  编译级类型一致性检查。
- 命名空间检查脚本为 `scripts/check-codes.mjs`：扫描所有 `const NAME: &str = "a.b.c"`
  声明，断言第一段属于所有者命名空间，并提示重复声明（如 phase 与 command 同名）。
  两者都由 `node scripts/verify.mjs` 串联执行。
- 公共类型变更必须由 00 集成负责人更新 `crates/domain`、样例、TS 镜像和本文件；
  模块不得各自复制状态定义。

## 9. 视图模型（`crates/domain/src/view.rs`）

视图模型是**展示契约**：由 UX 目录按稳定 code 构造，UI 只读，核心模块不得把原始
异常文本塞进其中。两者都带版本字符串，便于文案目录升级时与已装壳对齐。

`UserMessage`（样例 `contracts/samples/user-message.json`）：

| 字段 | 说明 |
| --- | --- |
| `code` | 稳定 code，格式同错误码 `^[a-z][a-z0-9._-]{2,63}$` |
| `codeKnown` | 该 code 是否存在于随应用发布的文案目录 |
| `category` | `platform/account/game/network/local_file/content/room/server/launch/runtime` |
| `title` | 短句，必须具体，不使用泛化免责语 |
| `body` | 可选补充，可缺省 |
| `actions[]` | `{label, capability, role, kind?}`；`kind: "detail"` 只展开 `detailRef`，不提交命令 |
| `detailRef` | 技术详情引用，按需读取 |
| `evidence[]` | 显示该文案所需的证据标记，必须与 `Task.evidence` 对齐 |
| `contentVersion` | 文案目录版本 |

`capability` 是能力 ID（`^[a-z][a-z0-9._-]{0,63}$`，如 `launch.start`、`download.resume`），
反序列化即校验。

`AvailabilityProjection`（样例 `contracts/samples/availability.json`）是带 revision 的
可操作项投影：

- `schemaVersion` 固定 `availability.v1`，`requiresRecheckAtExecution` 恒为 `true`；
- `actions[].available` 表示“可以尝试”，**不是**授权结果；执行前由模块重查 revision、
  租约、运行会话与服务端权限；
- `state` 为 `available/unavailable/unknown/in_progress`，`available` 必须与
  `state == "available"` 一致；
- `reason` 是用户可读原因，`nextStep` 是能解除该原因的**能力 ID**，`condition` 是机器
  条件键（如 `game_running`）。

`JourneyContext`（目标、待决选择、返回点、关联的 operation/task/entity/attempt）属于
应用层展示状态，由 `apps/desktop` 持有并可序列化用于窗口重建；它不是核心模块 DTO，
模块不得据此判断业务成功或权限。

## 10. 任务与事件端口（`crates/core/src/tasks.rs`）

模块不自建调度器，也不再自己编号事件。应用层构造 `CoreApp`，把
`ModuleContext { tasks, events, dirs }` 交给每个模块：

- `TaskService::accept(TaskSpec)` 建任务并返回 `CommandResponse::Accepted` 所需的
  `Task`；`transition/set_phase/set_progress/add_evidence/succeed/fail/cancel_now`
  按 `attempt_generation` 围栏写入；
- 所有写入自动发布 `task.state` / `task.progress` / `task.result`，`event_id` 与
  `sequence` 由 `EventSink` 分配；模块只提供 kind 与 payload；
- `request_cancel` 只置 `cancelRequested`；模块在安全点调用 `cancel_now` 才产生
  `cancelled`；`is_cancel_requested` 供模块在步骤之间协作检查；
- `begin_retry` 提升代次、清空证据与错误，回到 `queued`；
- 事件流按 operation 保留最近 512 条，任务快照保留最近 256 条，均为有界。

## 11. 事件投递与重放

实时推送与历史重放共用同一条保留流，二者都不是“唯一真相”：

- 推送：桌面桥把每个已入账事件转发到 Tauri 事件 `shtmcl:event`；
- 重放：`get_events { operationId, afterSequence? }` 返回
  `{ events, cursor, truncated }`，窗口上限 256 条；
- 每个 operation 流保留最近 512 条。消费端保存 `StreamCursor { operationId, sequence }`，
  重连后先重放再续接；
- `truncated = true` 表示请求窗口被裁剪，消费端必须重新读快照，不能把部分历史当完整增量；
- 无论走哪条路径，消费端都要按 `eventId` 去重、按 `attemptGeneration` 过滤旧代次，
  并按 `sequence` 单调性丢弃乱序；
- 推送可能重复（重载与推送竞争），也可能在窗口冻结期间丢失；两者都由重放兜底。

模块不直接向 UI 推送：所有事件必须经过 `EventSink`，否则无法被重放。

## 12. 保留事件 kind 与必需 payload 字段

跨模块消费的事件不能各写一套字段名。下表已冻结（payload 一律 camelCase）；产出方必须按此发，
消费方可以依赖这些键存在；未列出的键由产出方自行定义并在其 handoff 登记。

| kind | 产出方 | 必需 payload 字段 | 可选字段 |
| --- | --- | --- | --- |
| `membership.changed` | 06 | `approvalState`: `requested` / `approved` / `denied` / `left` / `removed` | `detailRef`、`membershipId`、`roomId` |
| `connection.changed` | 06 | `state`: `gathering` / `connecting` / `connected` / `reconnecting` / `disconnected` / `failed` | `transport`（`direct` / `relay`）、`detailRef`、`connectionId` |
| `account.authorization.changed` | 05 | `state`: `completed` / `cancelled` / `expired` / `failed` | `providerId`、`identityId`、`errorCode`、`detailRef` |

约束：

- 这些事件不得携带凭据、访问令牌、回调 code、完整候选地址或游戏会话句柄；
- `connection.changed` 的 `transport` 只有在真正建立连接后才允许出现，`gathering`/`connecting`
  阶段不得预告直连或中继；
- 「已连接成功」只能由 `state = connected` 表达，不能用 UI 推断或日志文本代替；
- 成员终态（`left` / `removed`）由 06 发布，UI 不得用「离开页面」或本地超时推断已退出房间；
- 非任务终态（例如成员 `left`）也可以用 payload 内的 `evidence: string[]` 承载等价标记；
  它与 `Task.evidence` 含义相同：标记必须来自真实结果，未发生就不能写；
- 三个 kind 的名称是**冻结的跨模块名**（不带模块前缀），因为消费方（11 的旅程）已按此匹配；
  `scripts/check-codes.mjs` 对它们有显式白名单，新增同类 kind 必须先改本节。

## 变更记录

- v1（M0 冻结）：命令/事件/任务/取消/错误信封；任务端口与事件端口；`Task.evidence`；
  `UserMessage`、`ActionAvailability`、`AvailabilityProjection`。
