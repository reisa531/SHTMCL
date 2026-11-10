//! Quick Play capability table (game version × loader × authentication).
//!
//! Modules 06 (rooms), 07 (server platform) and 10 (UI) must not each hardcode
//! version checks: they ask this table. Every combination that has not been
//! verified is reported as [`QuickPlayCapability::Unknown`] — never as usable.
//! "Unknown" is an honest answer, not a failure of the table.
//!
//! Verified basis (documented, not guessed): `--quickPlayMultiplayer` exists from
//! Minecraft 1.20 on. Older versions can still add the server to `servers.dat`
//! and start the game, which is `LaunchAndJoin`. Loaders other than vanilla and
//! Fabric are unverified for this contract, so they answer `Unknown`.

use serde_json::Value;

use crate::launch::java::parse_game_version;
use crate::launch::plan::LoaderKind;
use crate::launch::ports::AuthRequirement;

/// Reason codes published alongside a capability result.
pub mod reasons {
    pub const DIRECT_JOIN: &str = "launch.quickplay.direct_join";
    pub const ADD_THEN_LAUNCH: &str = "launch.quickplay.add_then_launch";
    pub const UNKNOWN_VERSION: &str = "launch.quickplay.unknown_version";
    pub const UNKNOWN_LOADER: &str = "launch.quickplay.unknown_loader";
    pub const ONLINE_SESSION_REQUIRED: &str = "launch.quickplay.online_session_required";
    pub const AUTH_UNVERIFIED: &str = "launch.quickplay.auth_unverified";
}

/// What "join a server" means for one combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickPlayCapability {
    /// `--quickPlayMultiplayer <address>` is supported and verified.
    DirectJoin,
    /// The server must be written to `servers.dat` first, then the game starts.
    LaunchAndJoin,
    /// The combination is understood and cannot work (for example an offline
    /// identity against an online-mode server).
    Unsupported,
    /// Not verified: consumers must treat this as "ask the user", never as yes.
    Unknown,
}

impl QuickPlayCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectJoin => "direct_join",
            Self::LaunchAndJoin => "launch_and_join",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// One row key: everything the answer depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityKey {
    pub game_version: String,
    pub loader: LoaderKind,
    pub auth: AuthRequirement,
}

impl CapabilityKey {
    pub fn new(game_version: impl Into<String>, loader: LoaderKind, auth: AuthRequirement) -> Self {
        Self {
            game_version: game_version.into(),
            loader,
            auth,
        }
    }
}

/// The answer, plus why, plus the argument to add when one is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityEntry {
    pub capability: QuickPlayCapability,
    pub verified: bool,
    pub reason_code: &'static str,
    /// Argument name to append when the capability needs one.
    pub argument: Option<&'static str>,
    /// True when the capability may be used without asking the user again.
    pub available: bool,
}

impl CapabilityEntry {
    fn new(
        capability: QuickPlayCapability,
        verified: bool,
        reason_code: &'static str,
        argument: Option<&'static str>,
    ) -> Self {
        let available = verified
            && matches!(
                capability,
                QuickPlayCapability::DirectJoin | QuickPlayCapability::LaunchAndJoin
            );
        Self {
            capability,
            verified,
            reason_code,
            argument,
            available,
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "capability": self.capability.as_str(),
            "verified": self.verified,
            "available": self.available,
            "reasonCode": self.reason_code,
            "argument": self.argument,
        })
    }
}

/// The frozen capability table.
#[derive(Debug, Default, Clone, Copy)]
pub struct QuickPlayTable;

impl QuickPlayTable {
    /// Look one combination up.
    pub fn lookup(key: &CapabilityKey) -> CapabilityEntry {
        let Some((major, minor, _patch)) = parse_game_version(&key.game_version) else {
            // Snapshots/pre-releases: unknown, never assumed.
            return CapabilityEntry::new(
                QuickPlayCapability::Unknown,
                false,
                reasons::UNKNOWN_VERSION,
                None,
            );
        };
        if major != 1 {
            return CapabilityEntry::new(
                QuickPlayCapability::Unknown,
                false,
                reasons::UNKNOWN_VERSION,
                None,
            );
        }

        let loader_supported =
            matches!(key.loader, LoaderKind::Vanilla | LoaderKind::Fabric { .. });
        if !loader_supported {
            return CapabilityEntry::new(
                QuickPlayCapability::Unknown,
                false,
                reasons::UNKNOWN_LOADER,
                None,
            );
        }

        // One auth gate for every version row: an offline identity cannot join a
        // server, and an unknown account state is never a "yes". Applying this
        // before the version branch keeps 1.19.4 and 1.20.4 consistent (D02-4).
        let direct_join_supported = minor >= 20;
        let version_capability = if direct_join_supported {
            QuickPlayCapability::DirectJoin
        } else {
            QuickPlayCapability::LaunchAndJoin
        };
        let version_argument = if direct_join_supported {
            Some("--quickPlayMultiplayer")
        } else {
            None
        };
        let version_reason = if direct_join_supported {
            reasons::DIRECT_JOIN
        } else {
            reasons::ADD_THEN_LAUNCH
        };

        match key.auth {
            AuthRequirement::Online => {
                CapabilityEntry::new(version_capability, true, version_reason, version_argument)
            }
            AuthRequirement::Any => CapabilityEntry::new(
                version_capability,
                false,
                reasons::AUTH_UNVERIFIED,
                version_argument,
            ),
            AuthRequirement::Offline => CapabilityEntry::new(
                QuickPlayCapability::Unsupported,
                true,
                reasons::ONLINE_SESSION_REQUIRED,
                None,
            ),
        }
    }

    /// The argument to pass when `entry` is a verified direct join.
    pub fn direct_join_argument(entry: &CapabilityEntry) -> Option<&'static str> {
        if entry.available && entry.capability == QuickPlayCapability::DirectJoin {
            entry.argument
        } else {
            None
        }
    }

    /// The documented matrix, for consumers that render a capability summary.
    pub fn matrix(game_version: &str, loader: LoaderKind) -> Value {
        let rows: Vec<Value> = [
            AuthRequirement::Online,
            AuthRequirement::Offline,
            AuthRequirement::Any,
        ]
        .into_iter()
        .map(|auth| {
            let key = CapabilityKey::new(game_version, loader.clone(), auth);
            let entry = Self::lookup(&key);
            let mut row = entry.to_json();
            if let Some(object) = row.as_object_mut() {
                object.insert("auth".to_string(), Value::String(auth.as_str().to_string()));
            }
            row
        })
        .collect();
        serde_json::json!({
            "gameVersion": game_version,
            "loader": loader.to_json(),
            "combinations": rows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_vanilla_online_is_a_verified_direct_join() {
        let key = CapabilityKey::new("1.20.4", LoaderKind::Vanilla, AuthRequirement::Online);
        let entry = QuickPlayTable::lookup(&key);
        assert_eq!(entry.capability, QuickPlayCapability::DirectJoin);
        assert!(entry.available);
        assert_eq!(
            QuickPlayTable::direct_join_argument(&entry),
            Some("--quickPlayMultiplayer")
        );
    }

    #[test]
    fn old_versions_add_then_launch() {
        let key = CapabilityKey::new("1.19.4", LoaderKind::Vanilla, AuthRequirement::Online);
        let entry = QuickPlayTable::lookup(&key);
        assert_eq!(entry.capability, QuickPlayCapability::LaunchAndJoin);
        assert!(entry.available);
        assert_eq!(QuickPlayTable::direct_join_argument(&entry), None);
    }

    #[test]
    fn unverified_combinations_answer_unknown_and_are_never_available() {
        let snapshot = CapabilityKey::new("24w14a", LoaderKind::Vanilla, AuthRequirement::Online);
        let entry = QuickPlayTable::lookup(&snapshot);
        assert_eq!(entry.capability, QuickPlayCapability::Unknown);
        assert!(!entry.available);
        assert_eq!(entry.reason_code, reasons::UNKNOWN_VERSION);

        let forge = CapabilityKey::new(
            "1.20.4",
            LoaderKind::Forge {
                version: Some("49.0.30".to_string()),
            },
            AuthRequirement::Online,
        );
        let entry = QuickPlayTable::lookup(&forge);
        assert_eq!(entry.capability, QuickPlayCapability::Unknown);
        assert!(!entry.available);
        assert_eq!(entry.reason_code, reasons::UNKNOWN_LOADER);

        let unverified_auth =
            CapabilityKey::new("1.20.4", LoaderKind::Vanilla, AuthRequirement::Any);
        let entry = QuickPlayTable::lookup(&unverified_auth);
        assert_eq!(entry.capability, QuickPlayCapability::DirectJoin);
        assert!(!entry.available, "an unknown account state is not a yes");
        assert_eq!(entry.reason_code, reasons::AUTH_UNVERIFIED);
    }

    #[test]
    fn offline_identity_cannot_direct_join_an_online_server() {
        let key = CapabilityKey::new("1.20.4", LoaderKind::Vanilla, AuthRequirement::Offline);
        let entry = QuickPlayTable::lookup(&key);
        assert_eq!(entry.capability, QuickPlayCapability::Unsupported);
        assert!(!entry.available);
    }
}
