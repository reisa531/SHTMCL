//! 01 · 实例核心（Owner: 01-core-and-instances）
//!
//! 本目录由 01 负责：实例索引、revision 提交、写入协调、恢复与租约。
//!
//! 文件地图：
//! - [`state`]：`Instance` / `InstanceState` / `InstancePatch` / 租约 / 恢复记录与迁移规则
//! - [`store`]：单锁提交、幂等键、乐观并发、写前日志与回滚
//! - [`service`]：`CoreModule` 命令路由与任务分发
//! - [`error`]：本模块错误码（命名空间 `instances.*`）
//!
//! 不复制公共状态类型，不建立第二套任务调度；长任务一律通过共享
//! [`crate::tasks::TaskService`] 接受，取消只走 `request_cancel`。

pub mod error;
pub mod service;
pub mod state;
pub mod store;

use shtmcl_domain::ModuleId;

pub const MODULE: ModuleId = ModuleId::Instances;
pub const OWNER: &str = "01";

pub use service::{actions, events, InstancesModule};
pub use state::{
    GameVersion, Instance, InstanceLease, InstanceLocation, InstancePatch, InstanceState,
    LoaderKind, RecoveryRecord,
};
pub use store::{CommitOutcome, FileStorage, InstanceStore, StoragePort};
