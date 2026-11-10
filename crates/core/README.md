# shtmcl-core

公共模块边界。当前只有 `CoreModule` trait 与 `ModuleRegistry`，没有业务实现。

## 模块目录

| 目录 | Owner | 责任 |
| --- | --- | --- |
| `src/instances/` | 01 | 实例索引、revision 提交、恢复、租约 |
| `src/launch/` | 02 | Java、启动计划、游戏会话、能力表 |
| `src/downloads/` | 03 | 资源身份、下载、缓存、调度 |
| `src/content/` | 04 | 加载器、整合包、依赖求解、更新 |
| `src/accounts/` | 05 | 提供方、角色、凭据、MUA |
| `src/rooms/` | 06 | 房间、邀请、P2P 隧道 |
| `src/servers/` | 07 | 发布、目录、服务器列表、加入 |
| `src/diagnostics/` | 08 | 日志、诊断、更新与交付 |

## 接入方式

1. 在模块目录中定义服务类型并实现：

   ```rust
   impl shtmcl_core::CoreModule for MyService {
       fn id(&self) -> shtmcl_domain::ModuleId { ... }
       fn handle(&self, request: &shtmcl_domain::CommandRequest)
           -> Result<shtmcl_domain::CommandResponse, shtmcl_domain::ErrorEnvelope> { ... }
   }
   ```

2. 在应用层（`apps/desktop/src-tauri`）把 `Arc<MyService>` 注册到
   `ModuleRegistry`。不要修改注册表的所有权，也不要创建第二个注册表。
3. 长任务返回 `CommandResponse::Accepted`；进度通过 task 事件发布。
4. 需要新的公共 DTO/错误码时，先向 00 提契约变更；不要复制
   `crates/domain` 的状态定义。
5. 模块测试放本目录或 `work/validation/<module>/`，并记录实际命令与退出码。

## 当前限制

- 注册表为空；桌面命令 `get_shell_snapshot` 返回空模块列表。
- 没有任务调度器；任务由应用层持有，模块只处理命令与事件。
