//! 07 · 服务器平台（Owner: 07-server-platform）
//!
//! 本目录由 07 负责：服务器发布、目录、服务器列表与加入。服务器列表的 NBT
//! 解析/构造保留未知字段；写入实例必须走 01 的锁、revision 与提交。
//!
//! 文件地图：
//! - [`state`]：`ServerEntry`、`ServerAddress`（含 IPv6 括号与默认端口）、
//!   `NbtTag` / `RawServerRecord` / `ServerListFile` —— 未知标签原样保留
//! - [`ports`]：`DirectoryPort` / `InstancePort` 与 **fail-closed** 默认实现
//! - [`service`]：`CoreModule` 命令路由
//! - [`error`]：本模块错误码（命名空间 `servers.*`）
//!
//! 不变量：
//! - 写 `servers.dat` 只改 07 拥有的四个字段（`name` / `ip` / `icon` /
//!   `acceptTextures`），其余标签（含未来版本新增的）逐字节保留；缺失的
//!   `acceptTextures` 不会被写成 `0`，因为那会改变用户的选择。
//! - 加入前只做检查（可达性、版本、实例是否可写），启动由 02 负责，07 不启动。

pub mod error;
pub mod ports;
pub mod service;
pub mod state;

use shtmcl_domain::ModuleId;

pub const MODULE: ModuleId = ModuleId::Servers;
pub const OWNER: &str = "07";

pub use error::{codes, ServersError};
pub use ports::{
    DirectoryPort, FixtureDirectory, FixtureInstances, InstancePort, Reachability,
    UnavailableDirectory, UnavailableInstances,
};
pub use service::{actions, events, ServersModule};
pub use state::{
    NbtTag, RawServerRecord, ServerAddress, ServerEntry, ServerListFile, ServerOrigin,
};
