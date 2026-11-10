//! 09 · 分布式分发 PoC（Owner: 09-distribution-research）
//!
//! 本目录是**独立研究分支**，不进入正式安装链路，不被 01—08 依赖。它的唯一
//! 目的是把 `docs/development/09-distribution-research.md` §5 的调度与校验假设
//! 变成可运行、可测量、可复现的代码，从而回答一个具体问题：
//!
//! > 在源限速相同的前提下，受控分块调度能否比单源 HTTP 更快完成一次安装，
//! > 代价是多少内存、多少重复字节、多少额外代码。
//!
//! ## 文件地图
//!
//! - [`manifest`]：清单与分块摘要（签名覆盖字段、根摘要、不可变身份）
//! - [`plan`]：有界稀有块优先调度 + in-flight 集合 + 尾块重复与取消
//! - [`verify`]：块级与整文件两级校验，流式读取，有限工作池
//! - [`peer`]：来源评分（短窗平滑）、坏块隔离、HTTP 回退阈值
//! - [`error`]：本分支错误码（命名空间 `distribution.*`）
//!
//! ## 明确不做的部分
//!
//! - 不实现 DHT / 公共资源发现；只接受可信清单指定的受控来源。
//! - 不写实例目录、不改安装提交规则；校验后的结果以字节交付给 03。
//! - 不做上传侧实现。上传策略模型见 §3，但 PoC 首轮只测下载收益；缺上传
//!   层面的实测意味着"中心压力"门槛本轮**未验证**，这一点必须在报告里写清。
//! - 不宣称节省比例。所有数字来自 `poc` 二进制在本机的实测输出。

pub mod error;
pub mod manifest;
pub mod peer;
pub mod plan;
pub mod verify;

/// Contract version of this research branch's own interface. Independent of the
/// product contract: nothing here may be consumed by a shipped module.
pub const POC_SCHEMA_VERSION: &str = "distribution.poc.v1";

/// Namespace every stable identifier in this branch uses.
pub const NAMESPACE: &str = "distribution";

/// Whether this branch is wired into the product. Kept as a constant so a reader
/// (and a test) can confirm the answer without reading the whole module.
pub const WIRED_INTO_PRODUCT: bool = false;
