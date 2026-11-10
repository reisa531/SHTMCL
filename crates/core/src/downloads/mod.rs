//! 03 · 下载与缓存（Owner: 03-download-and-cache）
//!
//! 本目录由 03 负责：资源身份、去重、调度、续传、校验与共享缓存。公共资源
//! 与缓存的写入权在本模块；其他模块只提交下载计划或读取结果。
//!
//! 文件地图：
//! - [`state`]：内容身份 `ArtifactHash`、区间合并与续传 `Transfer`、
//!   有界调度 `Scheduler`、引用计数缓存 `CacheIndex`
//! - [`ports`]：`DownloadPort` / `DiskPort` / `ClockPort` 与 **fail-closed** 默认实现
//! - [`service`]：`CoreModule` 命令路由与下载任务
//! - [`error`]：本模块错误码（命名空间 `downloads.*`）与证据标记
//!
//! 长任务一律 `accepted`，进度通过 `task` 事件增量发布；证据标记只在事实
//! 成立后写入（`resumable_ranges` / `completed_artifacts`）。

pub mod error;
pub mod http;
pub mod ports;
pub mod service;
pub mod state;
pub mod storage;

use shtmcl_domain::ModuleId;

pub const MODULE: ModuleId = ModuleId::Downloads;
pub const OWNER: &str = "03";

pub use error::{codes, evidence, DownloadsError};
pub use http::{standard_sources, HttpDownloads, HttpSource};
pub use ports::{
    ClockPort, DiskPort, DownloadPort, FetchOutcome, FixtureClock, FixtureDisk, FixtureDownloads,
    SystemClock, UnavailableDownloads, UnknownDisk,
};
pub use service::{actions, events, DownloadsModule};
pub use state::{
    ArtifactHash, ArtifactRequest, ArtifactState, ByteRange, CacheEntry, CacheIndex, Scheduler,
    Slot, Transfer,
};
pub use storage::{FileDisk, FileStore};
