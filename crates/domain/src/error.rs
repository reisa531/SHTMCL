//! Stable error envelope shared by commands, tasks and events.
//!
//! The wire format carries no raw exception text or stack trace. UI layers map
//! `code` / `user_action` to localized copy; technical details are fetched by
//! `detail_ref` on demand.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{OperationId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCodeError {
    Empty,
    TooShort,
    TooLong,
    InvalidStart,
    InvalidCharacter,
}

impl fmt::Display for ErrorCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("error code must not be empty"),
            Self::TooShort => f.write_str("error code must be at least 3 characters"),
            Self::TooLong => f.write_str("error code must be at most 64 characters"),
            Self::InvalidStart => f.write_str("error code must start with a lowercase letter"),
            Self::InvalidCharacter => f.write_str(
                "error code may only contain lowercase letters, digits, '.', '_' or '-'",
            ),
        }
    }
}

impl std::error::Error for ErrorCodeError {}

/// Stable, machine-readable error identifier such as `task.not_found`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ErrorCode(String);

impl ErrorCode {
    pub const TASK_NOT_FOUND: &'static str = "task.not_found";
    pub const TASK_STALE_GENERATION: &'static str = "task.stale_generation";
    pub const TASK_NOT_CANCELLABLE: &'static str = "task.not_cancellable";
    pub const TASK_INVALID_TRANSITION: &'static str = "task.invalid_transition";
    pub const COMMAND_OPERATION_REQUIRED: &'static str = "command.operation_required";
    pub const COMMAND_NAME_REQUIRED: &'static str = "command.name_required";
    pub const COMMAND_NAME_INVALID: &'static str = "command.name_invalid";
    pub const COMMAND_NAME_MISMATCH: &'static str = "command.name_mismatch";
    pub const MODULE_UNAVAILABLE: &'static str = "module.unavailable";
    pub const INTERNAL_UNAVAILABLE: &'static str = "internal.unavailable";

    pub fn new(value: impl Into<String>) -> Result<Self, ErrorCodeError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ErrorCodeError::Empty);
        }
        if value.len() < 3 {
            return Err(ErrorCodeError::TooShort);
        }
        if value.len() > 64 {
            return Err(ErrorCodeError::TooLong);
        }
        let mut characters = value.chars();
        let first = characters.next().ok_or(ErrorCodeError::Empty)?;
        if !first.is_ascii_lowercase() {
            return Err(ErrorCodeError::InvalidStart);
        }
        if !characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        }) {
            return Err(ErrorCodeError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    /// Construct a code that is a compile-time constant. Invalid constants are
    /// caught by debug assertions and by the error-code unit tests.
    pub fn from_static(value: &'static str) -> Self {
        debug_assert!(
            Self::new(value).is_ok(),
            "invalid static error code: {value}"
        );
        Self(value.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ErrorCode {
    type Error = ErrorCodeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ErrorCode> for String {
    fn from(value: ErrorCode) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    InvalidRequest,
    Conflict,
    NotFound,
    PermissionDenied,
    Unsupported,
    Network,
    Storage,
    Cancelled,
    Internal,
}

/// Stable action identifier that the UI/UX layer can localize.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserAction(String);

pub const USER_ACTION_RETRY: &str = "retry";
pub const USER_ACTION_RELOAD: &str = "reload";
pub const USER_ACTION_CHOOSE_INSTANCE: &str = "choose_instance";
pub const USER_ACTION_REAUTHENTICATE: &str = "reauthenticate";
pub const USER_ACTION_OPEN_DETAILS: &str = "open_details";

impl UserAction {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for UserAction {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEnvelope {
    pub code: ErrorCode,
    pub category: ErrorCategory,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_action: Option<UserAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

impl ErrorEnvelope {
    pub fn new(code: ErrorCode, category: ErrorCategory, retryable: bool) -> Self {
        Self {
            code,
            category,
            retryable,
            user_action: None,
            detail_ref: None,
            operation_id: None,
        }
    }

    pub fn with_user_action(mut self, action: impl Into<UserAction>) -> Self {
        self.user_action = Some(action.into());
        self
    }

    pub fn with_detail_ref(mut self, detail_ref: impl Into<String>) -> Self {
        self.detail_ref = Some(detail_ref.into());
        self
    }

    pub fn with_operation(mut self, operation_id: OperationId) -> Self {
        self.operation_id = Some(operation_id);
        self
    }

    pub fn with_operation_opt(mut self, operation_id: Option<OperationId>) -> Self {
        self.operation_id = operation_id;
        self
    }

    pub fn task_not_found(task_id: &TaskId, operation_id: Option<OperationId>) -> Self {
        let mut envelope = Self::new(
            ErrorCode::from_static(ErrorCode::TASK_NOT_FOUND),
            ErrorCategory::NotFound,
            false,
        )
        .with_detail_ref(format!("task:{}", task_id.as_str()));
        if let Some(operation_id) = operation_id {
            envelope = envelope.with_operation(operation_id);
        }
        envelope
    }

    pub fn stale_generation(
        expected: crate::revision::AttemptGeneration,
        actual: crate::revision::AttemptGeneration,
        operation_id: Option<OperationId>,
    ) -> Self {
        let mut envelope = Self::new(
            ErrorCode::from_static(ErrorCode::TASK_STALE_GENERATION),
            ErrorCategory::Conflict,
            false,
        )
        .with_detail_ref(format!("attempt:{expected}->{actual}"))
        .with_user_action(USER_ACTION_RELOAD);
        if let Some(operation_id) = operation_id {
            envelope = envelope.with_operation(operation_id);
        }
        envelope
    }

    pub fn command_operation_required() -> Self {
        Self::new(
            ErrorCode::from_static(ErrorCode::COMMAND_OPERATION_REQUIRED),
            ErrorCategory::InvalidRequest,
            false,
        )
    }

    /// `payload.command` is absent; the module cannot dispatch.
    pub fn command_name_required() -> Self {
        Self::new(
            ErrorCode::from_static(ErrorCode::COMMAND_NAME_REQUIRED),
            ErrorCategory::InvalidRequest,
            false,
        )
    }

    /// `payload.command` is present but not a valid stable command name.
    pub fn command_name_invalid() -> Self {
        Self::new(
            ErrorCode::from_static(ErrorCode::COMMAND_NAME_INVALID),
            ErrorCategory::InvalidRequest,
            false,
        )
    }

    /// The command name belongs to another module's namespace.
    pub fn command_name_mismatch(module: &str, command: &str) -> Self {
        Self::new(
            ErrorCode::from_static(ErrorCode::COMMAND_NAME_MISMATCH),
            ErrorCategory::InvalidRequest,
            false,
        )
        .with_detail_ref(format!("command:{command}:expected-prefix={module}"))
    }

    pub fn module_unavailable() -> Self {
        Self::new(
            ErrorCode::from_static(ErrorCode::MODULE_UNAVAILABLE),
            ErrorCategory::Unsupported,
            false,
        )
    }

    pub fn internal_unavailable() -> Self {
        Self::new(
            ErrorCode::from_static(ErrorCode::INTERNAL_UNAVAILABLE),
            ErrorCategory::Internal,
            false,
        )
    }
}

impl fmt::Display for ErrorEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?})", self.code, self.category)
    }
}

impl std::error::Error for ErrorEnvelope {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_error_codes() {
        assert!(ErrorCode::new("task.not_found").is_ok());
        assert!(ErrorCode::new("").is_err());
        assert!(ErrorCode::new("Bad").is_err());
        assert!(ErrorCode::new("bad code").is_err());
        assert!(ErrorCode::new("a").is_err());
    }

    #[test]
    fn skips_absent_optional_fields() {
        let envelope = ErrorEnvelope::new(
            ErrorCode::from_static(ErrorCode::TASK_NOT_FOUND),
            ErrorCategory::NotFound,
            false,
        );
        let json = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(
            json.get("code").and_then(|value| value.as_str()),
            Some("task.not_found")
        );
        assert!(json.get("userAction").is_none());
    }
}
