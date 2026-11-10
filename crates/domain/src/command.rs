//! Command request/response envelope.
//!
//! Reads may omit `operation_id`; mutations must carry one. A long-running
//! mutation returns `accepted` with a task id instead of blocking the UI until
//! completion.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ErrorEnvelope;
use crate::ids::{EntityId, OperationId, RequestId, TaskId};
use crate::revision::Revision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Read,
    Mutate,
}

/// Stable command name carried in `payload.command`, e.g. `launch.start`.
///
/// A module dispatches on this name; it is not the module id and not the
/// operation id. Format matches error codes (`^[a-z][a-z0-9._-]{2,63}$`) so a
/// name is searchable in logs without parsing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandName(String);

impl CommandName {
    /// Key inside `payload` that every module reads its command name from.
    pub const PAYLOAD_KEY: &'static str = "command";

    pub fn new(value: impl Into<String>) -> Result<Self, CommandNameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CommandNameError::Empty);
        }
        if value.len() < 3 {
            return Err(CommandNameError::TooShort);
        }
        if value.len() > 64 {
            return Err(CommandNameError::TooLong);
        }
        let mut characters = value.chars();
        let first = characters.next().ok_or(CommandNameError::Empty)?;
        if !first.is_ascii_lowercase() {
            return Err(CommandNameError::InvalidStart);
        }
        if !characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        }) {
            return Err(CommandNameError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    /// Construct a compile-time constant name; invalid constants are caught by
    /// debug assertions and the command unit tests.
    pub fn from_static(value: &'static str) -> Self {
        debug_assert!(
            Self::new(value).is_ok(),
            "invalid static command name: {value}"
        );
        Self(value.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Module that owns a dotted name, i.e. the part before the first `.`.
    pub fn owner(&self) -> &str {
        self.0.split('.').next().unwrap_or(&self.0)
    }
}

impl std::fmt::Display for CommandName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CommandName {
    type Error = CommandNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CommandName> for String {
    fn from(value: CommandName) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandNameError {
    Empty,
    TooShort,
    TooLong,
    InvalidStart,
    InvalidCharacter,
}

impl std::fmt::Display for CommandNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("command name must not be empty"),
            Self::TooShort => f.write_str("command name must be at least 3 characters"),
            Self::TooLong => f.write_str("command name must be at most 64 characters"),
            Self::InvalidStart => f.write_str("command name must start with a lowercase letter"),
            Self::InvalidCharacter => f.write_str(
                "command name may only contain lowercase letters, digits, '.', '_' or '-'",
            ),
        }
    }
}

impl std::error::Error for CommandNameError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRequest {
    pub request_id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<EntityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<Revision>,
    pub payload: Value,
}

impl CommandRequest {
    /// Apply the minimum transport-level validation owned by the shared
    /// contract. Authorization and revision checks remain the module's job.
    pub fn validate(&self, kind: CommandKind) -> Result<(), ErrorEnvelope> {
        if kind == CommandKind::Mutate && self.operation_id.is_none() {
            return Err(ErrorEnvelope::command_operation_required());
        }
        Ok(())
    }

    /// Read and validate the command name from `payload.command`.
    ///
    /// Modules must use this accessor instead of indexing the payload: it keeps
    /// one canonical key and one error code across all modules.
    pub fn command(&self) -> Result<CommandName, ErrorEnvelope> {
        let raw = self
            .payload
            .get(CommandName::PAYLOAD_KEY)
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                ErrorEnvelope::command_name_required().with_operation_opt(self.operation_id.clone())
            })?;
        CommandName::new(raw).map_err(|_| {
            ErrorEnvelope::command_name_invalid()
                .with_detail_ref(format!("command:{raw}"))
                .with_operation_opt(self.operation_id.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use serde_json::json;

    fn request(payload: Value) -> CommandRequest {
        CommandRequest {
            request_id: RequestId::new("req-1").expect("id"),
            operation_id: Some(OperationId::new("op-1").expect("id")),
            entity_id: None,
            expected_revision: None,
            payload,
        }
    }

    #[test]
    fn validates_command_names() {
        assert!(CommandName::new("launch.start").is_ok());
        assert_eq!(CommandName::new(""), Err(CommandNameError::Empty));
        assert_eq!(CommandName::new("ab"), Err(CommandNameError::TooShort));
        assert_eq!(
            CommandName::new("Launch.start"),
            Err(CommandNameError::InvalidStart)
        );
        assert_eq!(
            CommandName::new("launch start"),
            Err(CommandNameError::InvalidCharacter)
        );
        assert_eq!(
            CommandName::new("a".repeat(65)),
            Err(CommandNameError::TooLong)
        );
    }

    #[test]
    fn reads_the_canonical_payload_key() {
        let name = request(json!({ "command": "download.resume", "taskId": "task-1" }))
            .command()
            .expect("command name");
        assert_eq!(name.as_str(), "download.resume");
        assert_eq!(name.owner(), "download");
    }

    #[test]
    fn reports_missing_and_invalid_command_names() {
        let missing = request(json!({ "action": "download.resume" }))
            .command()
            .expect_err("alias key is not accepted");
        assert_eq!(missing.code.as_str(), ErrorCode::COMMAND_NAME_REQUIRED);

        let invalid = request(json!({ "command": "Download Resume" }))
            .command()
            .expect_err("invalid name");
        assert_eq!(invalid.code.as_str(), ErrorCode::COMMAND_NAME_INVALID);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandResponse {
    Completed {
        result: Value,
    },
    Accepted {
        #[serde(rename = "operationId")]
        operation_id: OperationId,
        #[serde(rename = "taskId")]
        task_id: TaskId,
    },
}
