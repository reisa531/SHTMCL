//! 08 · 诊断、帮助、更新与发行（Owner: 08-diagnostics-and-delivery）
//!
//! 本目录由 08 负责：有界日志（LogStore）、诊断规则与报告、修复计划、诊断导出、
//! 本地帮助、以及更新与发行事务。02 是游戏 stdout/stderr 的唯一读者，把每一行交给
//! 本模块的 [`logstore::LogStore`]（实现 02 的 [`crate::launch::logs::LogSink`]）；
//! 诊断、导出与 UI 都通过游标读同一份有界存储，不再读第二次管道。
//!
//! 入口：[`module`]（默认端口 **fail-closed**）或 [`module_with_ports`]（由应用层注入
//! 真实端口，例如 01 的实例端口与发布源）。
//!
//! 文件地图：
//! - [`logstore`]：内存环 8 MiB / 5,000 行、滚动文件 8 MiB/份、会话 64 MiB、总量
//!   256 MiB、写盘队列 4 MiB（落后即丢弃并计数）、游标分页 `read_log`
//! - [`rules`]：5 条规则（Java 不匹配、缺少依赖、重复模组、文件占用、内存不足）、
//!   置信度分级、2 秒预算、增量分析与缓存键
//! - [`repair`]：`RepairPlanner` 输出 04 的 `ChangePlan`，实例读写只走端口
//! - [`export`]：流式脱敏导出、字段白名单、预览、可取消、同目录临时文件 + 替换
//! - [`help`]：按错误类别返回的本地说明条目（不联网）
//! - [`update`]：语义化版本比较、构件摘要与签名端口、`UpdateTransaction` 状态机、
//!   切换前持久化、断电恢复与旧版本保持可用
//! - [`error`]：`diagnostics.*` 错误码
//! - [`service`]：`CoreModule` 命令路由与长任务
//!
//! 公共接口：实现 [`shtmcl_core::CoreModule`]。诊断导出与用户遥测是不同路径，
//! 导出前必须处理敏感字段。

pub mod error;
pub mod export;
pub mod help;
pub mod logstore;
pub mod repair;
pub mod rules;
pub mod service;
pub mod update;

use shtmcl_domain::ModuleId;

pub const MODULE: ModuleId = ModuleId::Diagnostics;
pub const OWNER: &str = "08";

pub use logstore::{LogLimits, LogStore, SessionStatus};
pub use repair::{RepairPlanner, RepairRequest};
pub use rules::{
    AnalysisRequest, Confidence, DiagnosticEngine, DiagnosticReport, Finding, FindingAction,
    RepairKind, ReportStatus,
};
pub use service::{module, module_with_ports, DiagnosticsModule, DiagnosticsPorts};
pub use update::{AppVersion, Channel, ReleaseRecord, UpdatePorts, UpdateTransaction};
