//! 06 · 房间与 P2P（Owner: 06-rooms-and-p2p）
//!
//! 本目录由 06 负责：邀请、审批、成员租约与隧道控制协议。隧道进程
//! （`apps/tunnel/`）与跨网 P2P 不在本轮范围；本轮交付的是房间/成员/连接状态机、
//! 服务端式串行化事务、租约授权、控制协议帧与有界缓冲下的字节转发模拟。
//!
//! 入口：[`module`]（默认端口 **fail-closed**）或 [`module_with_ports`]（应用层
//! 注入真实端口）。替身全部以 `Fixture*` 命名，且只能由测试或显式注入使用。
//!
//! 文件地图：
//! - [`state`]：`Room` / `Membership` / `ConnectionSession` / `TunnelBinding` 与迁移规则
//! - [`invite`]：一次性 secret、`sha256` 摘要、`remaining_uses` / `expires_at` / `revoked_at`
//! - [`lease`]：服务端截止时间授权、连接预算（直连 3s / 中继 +5s）
//! - [`tunnel`]：控制协议帧（握手/候选/开通/背压/关闭）+ 有序去重 + 有界缓冲背压
//! - [`store`]：单锁事务（邀请消耗与成员创建同事务、审批/取消串行化、幂等）
//! - [`ports`]：05/01/02/信令/中继/回环/时钟端口与受控替身
//! - [`service`]：`CoreModule` 命令路由与任务分发
//! - [`error`]：本模块错误码（命名空间 `rooms.*`）

pub mod error;
pub mod invite;
pub mod lease;
pub mod ports;
pub mod service;
pub mod state;
pub mod store;
pub mod tunnel;

use shtmcl_domain::ModuleId;

pub const MODULE: ModuleId = ModuleId::Rooms;
pub const OWNER: &str = "06";

pub use service::{module, module_with_ports, RoomsModule};
