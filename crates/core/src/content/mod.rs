//! 04 · 内容与整合包（Owner: 04-content-and-modpacks）
//!
//! 本目录由 04 负责：加载器安装描述、依赖求解、整合包（`.mrpack`）解析与
//! 安装/更新/回退计划。内容安装只提交变更计划：实例写入归 01（
//! [`ports::InstanceContentPort`]），下载归 03（[`ports::ArtifactPort`]），本模块
//! 不联网、不直接写游戏目录。
//!
//! 入口：[`module`]（默认 [`ports::ContentPorts::unavailable`]，全部端口 fail-closed，
//! 缺 01/03 时返回 `content.owner.unavailable`）或 [`module_with_ports`]（由应用层注入真实端口）。
//! [`ports::ContentPorts::fixtures`] 只给测试用，绝不能作为默认装配。
//!
//! 文件地图：
//! - [`loader`]：Fabric / Quilt / Forge / NeoForge / Vanilla 安装描述与安装计划
//! - [`resolve`]：有预算的依赖求解（完整解 / 已证明无解 / 预算耗尽 / 取消）
//! - [`modpack`]：`modrinth.index.json` 解析、overrides 语义、导出方向格式
//! - [`plan`]：`ContentLock` 与 `ChangePlan`（新增/替换/删除 + 去重 + 稳定顺序）
//! - [`install`]：安装/更新/回退执行器，安全点取消与回滚
//! - [`recovery`]：中断恢复记录（本模块自有，不写实例目录）
//! - [`zipindex`]：最小 zip 中央目录读取（std only）
//! - [`paths`]：路径规范化与解析预算
//! - [`service`]：`CoreModule` 命令路由

pub mod error;
pub mod fabric;
pub mod install;
pub mod loader;
pub mod modpack;
pub mod neoforge;
pub mod neoforge_download;
pub mod neoforge_mappings;
mod neoforge_processor_allowlist;
pub mod neoforge_profile;
pub mod neoforge_plan;
pub mod neoforge_stage;
pub mod neoforge_runner;
pub mod neoforge_runtime;
pub mod neoforge_pipeline;
pub mod paths;
pub mod plan;
pub mod ports;
pub mod recovery;
pub mod resolve;
pub mod service;
pub mod version;
pub mod zipindex;
pub mod mod_metadata;

use shtmcl_domain::ModuleId;

pub const MODULE: ModuleId = ModuleId::Content;
pub const OWNER: &str = "04";

pub use service::{module, module_with_ports, ContentModule};
