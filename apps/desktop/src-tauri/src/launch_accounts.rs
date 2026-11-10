//! Explicit local identity selection. Provider authentication remains in module 05.
use md5::{Digest, Md5};
use serde::Serialize;
use serde_json::json;
use shtmcl_core::{
    launch::ports::{AccountSessionSource, AuthRequirement, GameSession},
    CoreModule,
};
use shtmcl_domain::{
    CommandKind, CommandRequest, CommandResponse, ErrorCategory, ErrorCode, ErrorEnvelope, ModuleId,
};
use std::sync::{Arc, RwLock};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfflineIdentity {
    kind: &'static str,
    username: String,
    uuid: String,
}

/// One in-memory selection shared by the launch account and instance adapters.
#[derive(Default)]
pub(crate) struct LaunchIdentities {
    username: Arc<RwLock<Option<String>>>,
}

impl LaunchIdentities {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn selection(&self) -> Arc<RwLock<Option<String>>> {
        self.username.clone()
    }

    pub(crate) fn current(&self) -> Result<Option<OfflineIdentity>, ErrorEnvelope> {
        let selected = self
            .username
            .read()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        selected.as_deref().map(offline_identity).transpose()
    }

    pub(crate) fn select_offline(&self, username: &str) -> Result<OfflineIdentity, ErrorEnvelope> {
        let identity = offline_identity(username)?;
        let mut selected = self
            .username
            .write()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        *selected = Some(identity.username.clone());
        Ok(identity)
    }
}

impl AccountSessionSource for LaunchIdentities {
    fn ensure_game_session(
        &self,
        requirement: AuthRequirement,
        expected_username: &str,
    ) -> Result<GameSession, ErrorEnvelope> {
        if requirement == AuthRequirement::Online {
            return Err(identity_error("launch.identity.online_required"));
        }
        let identity = self
            .current()?
            .ok_or_else(|| identity_error("launch.identity.required"))?;
        // This name is the instance snapshot's expectation, never a fallback
        // from which an unselected identity may be manufactured.
        if identity.username != expected_username {
            return Err(identity_error("launch.identity.changed"));
        }
        Ok(GameSession::new(
            Some(format!("offline-{}", identity.uuid)),
            identity.username,
            identity.uuid,
            "legacy",
            "0",
        ))
    }
}

pub(crate) struct LaunchIdentityCommands {
    inner: Arc<dyn CoreModule>,
    identities: Arc<LaunchIdentities>,
}

impl LaunchIdentityCommands {
    pub(crate) fn new(inner: Arc<dyn CoreModule>, identities: Arc<LaunchIdentities>) -> Self {
        Self { inner, identities }
    }
}

impl CoreModule for LaunchIdentityCommands {
    fn id(&self) -> ModuleId {
        ModuleId::Launch
    }

    fn handle(&self, request: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
        match request.command()?.as_str() {
            "launch.identity.current" => {
                request.validate(CommandKind::Read)?;
                Ok(CommandResponse::Completed {
                    result: json!({"identity": self.identities.current()?}),
                })
            }
            "launch.identity.select" => {
                request.validate(CommandKind::Mutate)?;
                if request.payload["kind"].as_str() != Some("offline") {
                    return Err(shtmcl_core::launch::errors::request_invalid(
                        "identity_kind",
                    ));
                }
                let username = request.payload["username"].as_str().ok_or_else(|| {
                    shtmcl_core::launch::errors::request_invalid("identity_username")
                })?;
                Ok(CommandResponse::Completed {
                    result: json!({"identity": self.identities.select_offline(username)?}),
                })
            }
            _ => self.inner.handle(request),
        }
    }
}

fn identity_error(code: &'static str) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::from_static(code),
        ErrorCategory::PermissionDenied,
        false,
    )
}

fn offline_identity(username: &str) -> Result<OfflineIdentity, ErrorEnvelope> {
    if username.is_empty()
        || username.len() > 16
        || !username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(shtmcl_core::launch::errors::request_invalid(
            "identity_username",
        ));
    }
    // Java UUID.nameUUIDFromBytes("OfflinePlayer:" + username): UUID version 3.
    let mut hash = Md5::new();
    hash.update(b"OfflinePlayer:");
    hash.update(username.as_bytes());
    let mut bytes: [u8; 16] = hash.finalize().into();
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(OfflineIdentity {
        kind: "offline",
        username: username.into(),
        uuid: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shtmcl_domain::{OperationId, RequestId};

    #[test]
    fn explicit_selection_uses_standard_offline_uuid_and_shared_name() {
        let identities = LaunchIdentities::default();
        assert_eq!(identities.current().unwrap(), None);
        let selected = identities.select_offline("Steve").unwrap();
        assert_eq!(selected.uuid, "5627dd98e6be3c21b8a8e92344183641");
        assert_eq!(
            identities.selection().read().unwrap().as_deref(),
            Some("Steve")
        );
        let session = identities
            .ensure_game_session(AuthRequirement::Offline, "Steve")
            .unwrap();
        assert_eq!(session.username, "Steve");
        assert_eq!(session.uuid, selected.uuid);
        assert_eq!(session.access_token(), "0");
        assert_eq!(session.user_type, "legacy");
        assert_eq!(
            identities
                .ensure_game_session(AuthRequirement::Any, "Steve")
                .unwrap()
                .username,
            "Steve"
        );
    }

    #[test]
    fn missing_selection_never_uses_fallback_and_online_is_always_rejected() {
        let identities = LaunchIdentities::default();
        assert_eq!(
            identities
                .ensure_game_session(AuthRequirement::Any, "Player")
                .unwrap_err()
                .code
                .as_str(),
            "launch.identity.required"
        );
        identities.select_offline("Steve").unwrap();
        assert_eq!(
            identities
                .ensure_game_session(AuthRequirement::Online, "Steve")
                .unwrap_err()
                .code
                .as_str(),
            "launch.identity.online_required"
        );
    }

    #[test]
    fn invalid_names_leave_the_last_explicit_choice_unchanged() {
        let identities = LaunchIdentities::default();
        identities.select_offline("Alex").unwrap();
        for invalid in [
            "",
            " ",
            "Alex Player",
            "玩家",
            "../Alex",
            "abcdefghijklmnopq",
            "Alex\n",
        ] {
            assert!(identities.select_offline(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(identities.current().unwrap().unwrap().username, "Alex");
        assert_ne!(
            offline_identity("Alex").unwrap().uuid,
            offline_identity("alex").unwrap().uuid
        );
    }

    #[test]
    fn selection_changed_after_snapshot_requires_a_new_launch() {
        let identities = LaunchIdentities::default();
        identities.select_offline("Alice").unwrap();
        identities.select_offline("Bob").unwrap();
        assert_eq!(
            identities
                .ensure_game_session(AuthRequirement::Offline, "Alice")
                .unwrap_err()
                .code
                .as_str(),
            "launch.identity.changed"
        );
        assert_eq!(
            identities
                .ensure_game_session(AuthRequirement::Offline, "Bob")
                .unwrap()
                .username,
            "Bob"
        );
    }

    struct Inner;
    impl CoreModule for Inner {
        fn id(&self) -> ModuleId {
            ModuleId::Launch
        }
        fn handle(&self, _: &CommandRequest) -> Result<CommandResponse, ErrorEnvelope> {
            Ok(CommandResponse::Completed {
                result: json!({"delegated": true}),
            })
        }
    }
    fn request(command: &str, payload: serde_json::Value, mutation: bool) -> CommandRequest {
        let mut payload = payload;
        payload["command"] = command.into();
        CommandRequest {
            request_id: RequestId::new("identity-test").unwrap(),
            operation_id: mutation.then(|| OperationId::new("identity-test-operation").unwrap()),
            entity_id: None,
            expected_revision: None,
            payload,
        }
    }

    #[test]
    fn commands_validate_mutations_and_delegate_other_launch_commands() {
        let identities = Arc::new(LaunchIdentities::default());
        let module = LaunchIdentityCommands::new(Arc::new(Inner), identities.clone());
        assert!(module
            .handle(&request(
                "launch.identity.select",
                json!({"kind":"offline","username":"Steve"}),
                false
            ))
            .is_err());
        assert!(module
            .handle(&request(
                "launch.identity.select",
                json!({"kind":"mua","username":"Steve"}),
                true
            ))
            .is_err());
        module
            .handle(&request(
                "launch.identity.select",
                json!({"kind":"offline","username":"Steve"}),
                true,
            ))
            .unwrap();
        let result = serde_json::to_value(
            module
                .handle(&request("launch.identity.current", json!({}), false))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["result"]["identity"]["username"], "Steve");
        assert_eq!(
            module
                .handle(&request("launch.versions.list", json!({}), false))
                .unwrap(),
            CommandResponse::Completed {
                result: json!({"delegated":true})
            }
        );
    }
}
