//! Frozen view-model DTOs shared by the UX layer, the UI layer and the
//! application bridge.
//!
//! These types are *presentation* contracts: core modules never construct them
//! from raw exceptions. A message is built from a stable code plus evidence the
//! core actually produced, and an availability projection is built from domain
//! facts plus a server authorization result. See `contracts/domain-v1.md` §9.
//!
//! Field naming follows the workspace convention (camelCase on the wire) so the
//! TypeScript mirror in `apps/desktop/src/contract.ts` stays byte-compatible.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::revision::Revision;

/// Stable identifier for a domain capability such as `launch.start`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CapabilityId(String);

impl CapabilityId {
    pub fn new(value: impl Into<String>) -> Result<Self, ViewError> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 {
            return Err(ViewError::InvalidCapabilityId);
        }
        let mut characters = value.chars();
        let first = characters.next().ok_or(ViewError::InvalidCapabilityId)?;
        if !first.is_ascii_lowercase() {
            return Err(ViewError::InvalidCapabilityId);
        }
        if !characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        }) {
            return Err(ViewError::InvalidCapabilityId);
        }
        Ok(Self(value))
    }

    pub fn from_static(value: &'static str) -> Self {
        debug_assert!(
            Self::new(value).is_ok(),
            "invalid static capability id: {value}"
        );
        Self(value.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CapabilityId {
    type Error = ViewError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CapabilityId> for String {
    fn from(value: CapabilityId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageCategory {
    Platform,
    Account,
    Game,
    Network,
    LocalFile,
    Content,
    Room,
    Server,
    Launch,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageActionRole {
    Primary,
    Secondary,
}

/// `Detail` marks an action that only expands `detail_ref`; it must never be
/// mapped to a business command by the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageActionKind {
    Detail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageAction {
    pub label: String,
    pub capability: CapabilityId,
    pub role: MessageActionRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<MessageActionKind>,
}

/// Localized title/body/actions for one stable code.
///
/// `evidence` lists the markers the core must have produced before the wording
/// may be shown (for example `resumable_ranges` for "progress was kept").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub code: ErrorCode,
    pub code_known: bool,
    pub category: MessageCategory,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub actions: Vec<MessageAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_ref: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub content_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    Available,
    Unavailable,
    Unknown,
    InProgress,
}

/// One projected capability.
///
/// `available` means "the bridge may try"; it is not an authorization result.
/// The executing module re-checks revision, lease, running session and server
/// permission before acting on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionAvailability {
    pub label: String,
    pub capability: CapabilityId,
    pub role: MessageActionRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// Revision the projection was computed from; `None` when the entity has no
    /// revision yet (for example a not-yet-created instance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub based_on_revision: Option<Revision>,
    pub state: AvailabilityState,
    pub available: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<CapabilityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

/// Whole-surface projection handed to the UI in one snapshot field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailabilityProjection {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    pub requires_recheck_at_execution: bool,
    #[serde(default)]
    pub actions: Vec<ActionAvailability>,
    #[serde(default)]
    pub enabled: Vec<CapabilityId>,
}

impl AvailabilityProjection {
    pub const SCHEMA_VERSION: &'static str = "availability.v1";

    pub fn new(revision: Option<Revision>) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION.to_string(),
            revision,
            requires_recheck_at_execution: true,
            actions: Vec::new(),
            enabled: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    InvalidCapabilityId,
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapabilityId => f.write_str(
                "capability id must start with a lowercase letter and use only a-z, 0-9, '.', '_' or '-'",
            ),
        }
    }
}

impl std::error::Error for ViewError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_capability_ids() {
        assert!(CapabilityId::new("launch.start").is_ok());
        assert!(CapabilityId::new("").is_err());
        assert!(CapabilityId::new("Launch.start").is_err());
        assert!(CapabilityId::new("launch start").is_err());
        assert!(CapabilityId::new("a".repeat(65)).is_err());
    }

    #[test]
    fn projection_always_requires_a_recheck() {
        let projection = AvailabilityProjection::new(Some(Revision::new(7)));
        assert!(projection.requires_recheck_at_execution);
        assert_eq!(projection.schema_version, "availability.v1");
    }

    #[test]
    fn message_omits_absent_optional_fields() {
        let message = UserMessage {
            code: ErrorCode::from_static("launch.prepare.cancelled"),
            code_known: true,
            category: MessageCategory::Launch,
            title: "已取消".to_string(),
            body: None,
            actions: Vec::new(),
            detail_ref: None,
            evidence: vec!["safe_stop_reached".to_string()],
            content_version: "copy.v1".to_string(),
        };
        let json = serde_json::to_value(&message).expect("serialize");
        assert!(json.get("body").is_none());
        assert!(json.get("detailRef").is_none());
        assert_eq!(
            json.get("codeKnown").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            json.get("evidence")
                .and_then(|value| value.as_array())
                .map(Vec::len),
            Some(1)
        );
    }
}
