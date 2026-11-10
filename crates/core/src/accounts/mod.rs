//! 05 · 账户与 MUA（Owner: 05-accounts-and-mua）
//!
//! 本目录由 05 负责：提供方适配、凭据、角色与 MUA 授权。凭据只能留在本层；
//! 对 02 仅返回限目的、限有效期的内存游戏会话句柄。
//!
//! 文件地图：
//! - [`state`]：`Secret`（拒绝打印）、`Identity`、`AuthorizationState`、
//!   `GameSessionHandle` / `SessionView`、`AccountRole`、`AccountStore`
//! - [`ports`]：`AccountsPorts` 与 **fail-closed** 默认实现；令牌经专用端口
//!   进程内交给 02，不经过命令结果
//! - [`service`]：`CoreModule` 命令路由
//! - [`error`]：本模块错误码（命名空间 `accounts.*`）
//!
//! 不变量：
//! - 令牌不出本模块：`Identity` / `SessionView` 都没有令牌字段，`Secret` 的
//!   `Debug` / `Display` 都是掩码，`GameSessionHandle` 的令牌标了 `#[serde(skip)]`。
//! - 句柄限目的、限时、一次性；错误代次或错误用途一律拒绝。
//! - `account.authorization.changed` 是契约 §12 的跨模块保留事件，payload 只有
//!   `state` / `providerId` / `identityId` / `errorCode` / `detailRef`。

pub mod error;
pub mod ports;
pub mod service;
pub mod state;
pub mod yggdrasil;

use shtmcl_domain::ModuleId;

pub const MODULE: ModuleId = ModuleId::Accounts;
pub const OWNER: &str = "05";

pub use error::{codes, AccountsError};
pub use ports::{
    AccountsPorts, FixtureAccounts, LoginChallenge, LoginGrant, PollOutcome, UnavailableAccounts,
    YggdrasilAuthError, YggdrasilGrant,
};
pub use service::{actions, events, AccountsModule, DEFAULT_SESSION_TTL_MS};
pub use state::{
    AccountRole, AccountStore, AuthChallenge, AuthorizationState, GameSessionHandle, Identity,
    ProviderKind, Secret, SessionPurpose, SessionView,
};
pub use yggdrasil::{MuaYggdrasilClient, YggdrasilError, YggdrasilMetadata, MUA_API_ROOT};
