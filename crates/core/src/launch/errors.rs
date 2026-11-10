//! Stable error codes owned by module 02.
//!
//! The launch module never puts raw stderr, process output or credentials into
//! an error envelope. A failure carries a stable `code`, a category, whether a
//! retry can help, an optional user action ID and an opaque `detail_ref`. The
//! user-facing sentence is produced by the UX catalog (module 11) from the code.

use shtmcl_domain::{ErrorCategory, ErrorCode, ErrorEnvelope};

/// Codes in this module. Format is the shared `^[a-z][a-z0-9._-]{2,63}$`.
pub mod codes {
    /// The command name is missing or the payload does not match the command.
    pub const REQUEST_INVALID: &str = "launch.request.invalid";
    /// The instance snapshot port is not delivered yet (01 owns the real one).
    pub const INSTANCE_UNAVAILABLE: &str = "launch.instance.unavailable";
    /// The instance source refused the snapshot (unknown/being restored).
    pub const INSTANCE_REJECTED: &str = "launch.instance.rejected";
    /// The frozen snapshot revision no longer matches the instance.
    pub const INSTANCE_STALE_REVISION: &str = "launch.instance.stale_revision";
    /// The account module cannot provide a game session for this launch.
    pub const ACCOUNT_REQUIRED: &str = "launch.account.required";
    /// No usable Java runtime was found in the searched locations.
    pub const JAVA_NOT_FOUND: &str = "launch.java.not_found";
    /// A runtime exists but does not satisfy the version requirement.
    pub const JAVA_VERSION_MISMATCH: &str = "launch.java.version_mismatch";
    /// A runtime exists but its CPU architecture is not usable on this host.
    pub const JAVA_ARCH_MISMATCH: &str = "launch.java.arch_mismatch";
    /// A runtime exists but targets another operating system.
    pub const JAVA_PLATFORM_MISMATCH: &str = "launch.java.platform_mismatch";
    /// `java -version` did not answer within the probe timeout.
    pub const JAVA_PROBE_TIMEOUT: &str = "launch.java.probe_timeout";
    /// `java -version` ran but its output could not be understood.
    pub const JAVA_PROBE_FAILED: &str = "launch.java.probe_failed";
    /// Required game files are absent; the launch is refused before spawning.
    pub const FILES_MISSING: &str = "launch.files.missing";
    /// The plan could not be built (undefined template variable, bad arguments).
    pub const PLAN_INVALID: &str = "launch.plan.invalid";
    /// Creating the game process failed.
    pub const SPAWN_FAILED: &str = "launch.spawn.failed";
    /// The process existed but ended before the game could report anything.
    pub const PROCESS_EXITED_EARLY: &str = "launch.process.exited_early";
    /// The recorded process identity no longer matches the live process.
    pub const PROCESS_MISMATCH: &str = "launch.process.mismatch";
    /// No session with that id is known to this module.
    pub const SESSION_NOT_FOUND: &str = "launch.session.not_found";
    /// A session is already preparing/running and multi-open was not requested.
    pub const SESSION_CONFLICT: &str = "launch.session.conflict";
    /// The instance snapshot does not allow a second concurrent session.
    pub const MULTI_OPEN_DISABLED: &str = "launch.multi_open.disabled";
    /// Log retention is not wired yet, so no page can be produced.
    pub const LOGS_UNAVAILABLE: &str = "launch.logs.unavailable";
    /// The official version catalog could not be refreshed or read.
    pub const CATALOG_UNAVAILABLE: &str = "launch.catalog.unavailable";
}

/// Stable user-action IDs for the launch journey (UX layer localizes them).
pub const ACTION_INSTALL_JAVA: &str = "install_java";
pub const ACTION_CHOOSE_JAVA: &str = "choose_java";

/// Build an envelope from a module code.
pub fn envelope(code: &'static str, category: ErrorCategory, retryable: bool) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::from_static(code), category, retryable)
}

/// Build an envelope with an opaque technical detail reference.
pub fn with_detail(
    code: &'static str,
    category: ErrorCategory,
    retryable: bool,
    detail_ref: impl Into<String>,
) -> ErrorEnvelope {
    envelope(code, category, retryable).with_detail_ref(detail_ref)
}

/// Payload/transport problem; the caller sent something this module cannot read.
pub fn request_invalid(detail_ref: impl Into<String>) -> ErrorEnvelope {
    with_detail(
        codes::REQUEST_INVALID,
        ErrorCategory::InvalidRequest,
        false,
        detail_ref,
    )
}

/// The instance snapshot port has no implementation registered yet.
pub fn instance_unavailable() -> ErrorEnvelope {
    envelope(
        codes::INSTANCE_UNAVAILABLE,
        ErrorCategory::Unsupported,
        false,
    )
}

/// The instance source rejected the request (unknown instance, restoring, ...).
pub fn instance_rejected(detail_ref: impl Into<String>) -> ErrorEnvelope {
    with_detail(
        codes::INSTANCE_REJECTED,
        ErrorCategory::NotFound,
        false,
        detail_ref,
    )
}

/// The snapshot was computed for another revision; the caller must recompute.
pub fn instance_stale_revision(expected: u64, actual: u64) -> ErrorEnvelope {
    with_detail(
        codes::INSTANCE_STALE_REVISION,
        ErrorCategory::Conflict,
        true,
        format!("instance-revision:{expected}->{actual}"),
    )
}

/// A game session handle could not be obtained; the user must sign in again.
pub fn account_required(detail_ref: impl Into<String>) -> ErrorEnvelope {
    with_detail(
        codes::ACCOUNT_REQUIRED,
        ErrorCategory::PermissionDenied,
        false,
        detail_ref,
    )
    .with_user_action(shtmcl_domain::USER_ACTION_REAUTHENTICATE)
}

/// No Java runtime at all was found for the requirement.
pub fn java_not_found(requirement: &str) -> ErrorEnvelope {
    with_detail(
        codes::JAVA_NOT_FOUND,
        ErrorCategory::NotFound,
        false,
        format!("java-requirement:{requirement}"),
    )
    .with_user_action(ACTION_INSTALL_JAVA)
}

/// Required game files are missing; the launch is refused.
pub fn files_missing(count: usize) -> ErrorEnvelope {
    with_detail(
        codes::FILES_MISSING,
        ErrorCategory::Storage,
        true,
        format!("missing-files:{count}"),
    )
    .with_user_action(shtmcl_domain::USER_ACTION_RETRY)
}

/// A plan template referenced a variable this module does not define.
pub fn plan_invalid(detail_ref: impl Into<String>) -> ErrorEnvelope {
    with_detail(
        codes::PLAN_INVALID,
        ErrorCategory::InvalidRequest,
        false,
        detail_ref,
    )
}

/// Creating the game process failed.
pub fn spawn_failed(detail_ref: impl Into<String>) -> ErrorEnvelope {
    with_detail(
        codes::SPAWN_FAILED,
        ErrorCategory::Internal,
        true,
        detail_ref,
    )
}

/// The process ended before the game reported anything.
pub fn process_exited_early(exit_code: Option<i32>) -> ErrorEnvelope {
    with_detail(
        codes::PROCESS_EXITED_EARLY,
        ErrorCategory::Internal,
        true,
        match exit_code {
            Some(code) => format!("process-exit:{code}"),
            None => "process-exit:unknown".to_string(),
        },
    )
    .with_user_action(shtmcl_domain::USER_ACTION_OPEN_DETAILS)
}

/// A session id the module does not know.
pub fn session_not_found(session_id: &str) -> ErrorEnvelope {
    with_detail(
        codes::SESSION_NOT_FOUND,
        ErrorCategory::NotFound,
        false,
        format!("session:{session_id}"),
    )
}

/// Multi-open is not permitted by the frozen instance snapshot.
pub fn multi_open_disabled(instance_id: &str) -> ErrorEnvelope {
    with_detail(
        codes::MULTI_OPEN_DISABLED,
        ErrorCategory::Unsupported,
        false,
        format!("instance:{instance_id}"),
    )
}

/// The process identity recorded for a session no longer matches.
pub fn process_mismatch(session_id: &str) -> ErrorEnvelope {
    with_detail(
        codes::PROCESS_MISMATCH,
        ErrorCategory::Conflict,
        false,
        format!("session:{session_id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_matches_the_frozen_format() {
        for code in [
            codes::REQUEST_INVALID,
            codes::INSTANCE_UNAVAILABLE,
            codes::INSTANCE_REJECTED,
            codes::INSTANCE_STALE_REVISION,
            codes::ACCOUNT_REQUIRED,
            codes::JAVA_NOT_FOUND,
            codes::JAVA_VERSION_MISMATCH,
            codes::JAVA_ARCH_MISMATCH,
            codes::JAVA_PLATFORM_MISMATCH,
            codes::JAVA_PROBE_TIMEOUT,
            codes::JAVA_PROBE_FAILED,
            codes::FILES_MISSING,
            codes::PLAN_INVALID,
            codes::SPAWN_FAILED,
            codes::PROCESS_EXITED_EARLY,
            codes::PROCESS_MISMATCH,
            codes::SESSION_NOT_FOUND,
            codes::SESSION_CONFLICT,
            codes::MULTI_OPEN_DISABLED,
            codes::LOGS_UNAVAILABLE,
        ] {
            assert!(ErrorCode::new(code).is_ok(), "invalid error code: {code}");
        }
    }

    #[test]
    fn envelopes_never_carry_raw_text_or_tokens() {
        let envelope = process_exited_early(Some(1));
        assert_eq!(envelope.code.as_str(), codes::PROCESS_EXITED_EARLY);
        assert_eq!(envelope.detail_ref.as_deref(), Some("process-exit:1"));
        // `retryable` only tells the UI a user-initiated retry can help; the
        // module itself never restarts a crashed game.
        assert!(envelope.retryable);
    }
}
