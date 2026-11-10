//! Opaque identifiers shared by commands, events and tasks.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Maximum encoded length of an identifier. Identifiers are opaque strings.
pub const MAX_ID_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    Empty,
    TooLong,
    InvalidCharacter,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("id must not be empty"),
            Self::TooLong => write!(f, "id must be at most {MAX_ID_LEN} bytes"),
            Self::InvalidCharacter => {
                f.write_str("id must not contain whitespace or control characters")
            }
        }
    }
}

impl std::error::Error for IdError {}

fn validate(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(IdError::InvalidCharacter);
    }
    Ok(())
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                Self::try_from(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                validate(&value)?;
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

id_type!(TaskId);
id_type!(OperationId);
id_type!(RequestId);
id_type!(EventId);
// Generic entity identifier for an instance, room, server, artifact or account.
id_type!(EntityId);
id_type!(InstanceId);
id_type!(AccountId);
id_type!(RoomId);
id_type!(ServerId);
id_type!(ArtifactId);
id_type!(LaunchSessionId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_ids() {
        assert_eq!(TaskId::new(""), Err(IdError::Empty));
        assert_eq!(TaskId::new("two words"), Err(IdError::InvalidCharacter));
        assert_eq!(TaskId::new("line\nbreak"), Err(IdError::InvalidCharacter));
    }

    #[test]
    fn rejects_overlong_ids() {
        let value = "a".repeat(MAX_ID_LEN + 1);
        assert_eq!(TaskId::new(value), Err(IdError::TooLong));
    }

    #[test]
    fn serializes_as_a_plain_string_and_validates_on_deserialize() {
        let id = TaskId::new("task-1").expect("valid id");
        assert_eq!(serde_json::to_string(&id).expect("serialize"), "\"task-1\"");
        let decoded: TaskId = serde_json::from_str("\"task-1\"").expect("deserialize");
        assert_eq!(decoded, id);
        assert!(serde_json::from_str::<TaskId>("\"\"").is_err());
    }
}
