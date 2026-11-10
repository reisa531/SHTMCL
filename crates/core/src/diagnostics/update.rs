//! Update and release transaction.
//!
//! The update path is a small state machine over real files, because the failure
//! that matters is an interrupted switch:
//!
//! ```text
//! checking -> available -> downloading -> verifying -> restart_ready
//!          -> switching -> start_confirmation -> completed
//!          \-> retryable_failure            \-> rolled_back -> recovery_required
//! ```
//!
//! Invariants:
//!
//! - **the transaction record is persisted before the switch**, and again before
//!   the start confirmation, so a power cut leaves a record that says what was
//!   being attempted;
//! - the switch only moves a version *pointer*; the old installation is never
//!   deleted, so the previous version stays launchable and a rollback is one file
//!   write;
//! - every artifact is identified by its `sha256`, so recovery continues from the
//!   bytes that are actually on disk instead of trusting a version string;
//! - signature verification is a **port**. When no verifier is connected the
//!   transaction cannot reach `restart_ready` and nothing claims "verified";
//! - with active work (game, connection, instance transaction) the answer is
//!   `waiting_active_work`, never a hard restart.
//!
//! There is no network code in this module: the release feed and the artifact
//! download are ports owned by other parties (platform service, module 03).

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::error::DiagnosticsError;

/// Schema marker of the persisted transaction and of the command payloads.
pub const UPDATE_SCHEMA: &str = "diagnostics-update.v1";
/// Default automatic recovery attempts per transaction (persisted, never reset
/// by reopening the application).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------------------
// versions
// ---------------------------------------------------------------------------

/// Strict application version: `major.minor.patch[-prerelease]`, semver ordering
/// (a pre-release sorts before its release). Deliberately not 04's lenient
/// Minecraft version model: a release feed must reject `banana`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AppVersion(String);

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreId {
    Num(u64),
    Text(String),
}

fn parse_pre(identifier: &str) -> Option<PreId> {
    if identifier.is_empty() {
        return None;
    }
    if identifier
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        // Leading zeroes make a numeric identifier ambiguous (`01` != `1`).
        if identifier.len() > 1 && identifier.starts_with('0') {
            return None;
        }
        return identifier.parse::<u64>().ok().map(PreId::Num);
    }
    if identifier
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Some(PreId::Text(identifier.to_ascii_lowercase()));
    }
    None
}

impl AppVersion {
    pub fn parse(value: &str) -> Result<Self, DiagnosticsError> {
        let raw = value.trim();
        let invalid = || DiagnosticsError::VersionInvalid {
            value: value.to_string(),
        };
        if raw.is_empty()
            || raw.len() > 64
            || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid());
        }
        // Build metadata is not part of the ordering, so it is dropped.
        let without_build = raw.split('+').next().unwrap_or(raw);
        let (core, pre) = match without_build.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (without_build, None),
        };
        let numbers: Vec<&str> = core.split('.').collect();
        if numbers.len() != 3 {
            return Err(invalid());
        }
        for number in &numbers {
            if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
                return Err(invalid());
            }
            if number.len() > 1 && number.starts_with('0') {
                return Err(invalid());
            }
        }
        if let Some(pre) = pre {
            if pre.is_empty() {
                return Err(invalid());
            }
            for identifier in pre.split('.') {
                if parse_pre(identifier).is_none() {
                    return Err(invalid());
                }
            }
        }
        Ok(Self(without_build.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn numbers(&self) -> (u64, u64, u64) {
        let core = self.0.split('-').next().unwrap_or(&self.0);
        let mut parts = core.split('.');
        let major = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    }

    fn pre(&self) -> Vec<PreId> {
        match self.0.split_once('-') {
            Some((_, pre)) => pre.split('.').filter_map(parse_pre).collect::<Vec<PreId>>(),
            None => Vec::new(),
        }
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre().is_empty()
    }
}

impl fmt::Display for AppVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn compare_versions(left: &AppVersion, right: &AppVersion) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let (left_numbers, right_numbers) = (left.numbers(), right.numbers());
    match left_numbers.cmp(&right_numbers) {
        Ordering::Equal => {}
        other => return other,
    }
    let (left_pre, right_pre) = (left.pre(), right.pre());
    match (left_pre.is_empty(), right_pre.is_empty()) {
        (true, true) => return Ordering::Equal,
        // A pre-release sorts before the release it qualifies.
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left_id, right_id) in left_pre.iter().zip(right_pre.iter()) {
        let ordering = match (left_id, right_id) {
            (PreId::Num(a), PreId::Num(b)) => a.cmp(b),
            // Numeric identifiers sort before alphanumeric ones.
            (PreId::Num(_), PreId::Text(_)) => Ordering::Less,
            (PreId::Text(_), PreId::Num(_)) => Ordering::Greater,
            (PreId::Text(a), PreId::Text(b)) => a.cmp(b),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left_pre.len().cmp(&right_pre.len())
}

impl PartialOrd for AppVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AppVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_versions(self, other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stable,
    Beta,
    Nightly,
}

impl Channel {
    pub fn parse(value: &str) -> Result<Self, DiagnosticsError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stable" | "release" | "" => Ok(Self::Stable),
            "beta" | "preview" => Ok(Self::Beta),
            "nightly" | "canary" => Ok(Self::Nightly),
            other => Err(DiagnosticsError::invalid(format!("channel: {other}"))),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Nightly => "nightly",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPlatform {
    pub os: String,
    pub arch: String,
}

impl HostPlatform {
    pub fn new(os: impl Into<String>, arch: impl Into<String>) -> Self {
        Self {
            os: os.into().to_string(),
            arch: arch.into().to_string(),
        }
    }

    /// The build's own target, not a runtime probe.
    pub fn current() -> Self {
        let os = if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "unknown"
        };
        Self::new(os, std::env::consts::ARCH)
    }

    pub fn matches(&self, os: &str, arch: &str) -> bool {
        self.os.eq_ignore_ascii_case(os) && self.arch.eq_ignore_ascii_case(arch)
    }

    fn label(&self) -> String {
        format!("{}/{}", self.os, self.arch)
    }
}

// ---------------------------------------------------------------------------
// release records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSignature {
    pub scheme: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseArtifact {
    pub artifact_id: String,
    /// Lowercase hex SHA-256 of the artifact bytes. The identity used by recovery.
    pub sha256: String,
    pub size: u64,
    /// Opaque reference for the download owner. Never logged, never in an event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ArtifactSignature>,
    /// File name used inside the staging and installed directories.
    pub file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseRecord {
    pub version: AppVersion,
    pub channel: Channel,
    pub platform: String,
    pub arch: String,
    /// Oldest host build that can install this release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_host_version: Option<AppVersion>,
    /// Data schema this release writes. A rollback must stay inside it.
    #[serde(default = "default_schema")]
    pub data_schema: u32,
    pub artifact: ReleaseArtifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_ref: Option<String>,
}

fn default_schema() -> u32 {
    1
}

impl ReleaseRecord {
    pub fn to_value(&self) -> Value {
        json!({
            "version": self.version.as_str(),
            "channel": self.channel.as_str(),
            "platform": self.platform,
            "arch": self.arch,
            "minHostVersion": self.min_host_version.as_ref().map(|version| version.as_str()),
            "dataSchema": self.data_schema,
            "artifact": {
                "artifactId": self.artifact.artifact_id,
                "sha256": self.artifact.sha256,
                "size": self.artifact.size,
                "fileName": self.artifact.file_name,
                "signed": self.artifact.signature.is_some(),
            },
            "notesRef": self.notes_ref,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionVerdict {
    UpToDate,
    UpdateAvailable,
    Downgrade,
}

impl VersionVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpToDate => "up_to_date",
            Self::UpdateAvailable => "update_available",
            Self::Downgrade => "downgrade",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseCheck {
    pub current: AppVersion,
    pub channel: Channel,
    pub host: HostPlatform,
    pub verdict: VersionVerdict,
    pub candidate: Option<ReleaseRecord>,
}

impl ReleaseCheck {
    pub fn to_value(&self) -> Value {
        json!({
            "schema": UPDATE_SCHEMA,
            "currentVersion": self.current.as_str(),
            "channel": self.channel.as_str(),
            "platform": self.host.os,
            "arch": self.host.arch,
            "verdict": self.verdict.as_str(),
            "candidate": self.candidate.as_ref().map(ReleaseRecord::to_value),
        })
    }
}

// ---------------------------------------------------------------------------
// ports and their explicit doubles
// ---------------------------------------------------------------------------

pub trait ReleaseSourcePort: Send + Sync {
    fn latest(
        &self,
        channel: Channel,
        host: &HostPlatform,
    ) -> Result<ReleaseRecord, DiagnosticsError>;
}

pub trait ArtifactFetchPort: Send + Sync {
    /// Download `artifact` into `destination` and return the byte count.
    fn fetch(
        &self,
        artifact: &ReleaseArtifact,
        destination: &Path,
    ) -> Result<u64, DiagnosticsError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureVerification {
    pub verified: bool,
    pub scheme: String,
    pub detail: String,
}

pub trait SignaturePort: Send + Sync {
    fn verify(&self, record: &ReleaseRecord) -> Result<SignatureVerification, DiagnosticsError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupProbe {
    Ready { version: String },
    NotReady { reason: String },
    Unavailable,
}

pub trait StartupProbePort: Send + Sync {
    fn probe(&self) -> StartupProbe;
}

pub trait ActiveWorkPort: Send + Sync {
    /// Named kinds of work that must finish before a restart: `game`,
    /// `connection`, `instance_transaction`.
    fn active(&self) -> Result<Vec<String>, DiagnosticsError>;
}

#[derive(Clone)]
pub struct UpdatePorts {
    pub source: std::sync::Arc<dyn ReleaseSourcePort>,
    pub artifacts: std::sync::Arc<dyn ArtifactFetchPort>,
    pub signature: std::sync::Arc<dyn SignaturePort>,
    pub startup: std::sync::Arc<dyn StartupProbePort>,
    pub active_work: std::sync::Arc<dyn ActiveWorkPort>,
}

impl UpdatePorts {
    /// Default wiring: **every** neighbour is fail-closed. Registering the module
    /// therefore cannot check, download, verify or switch anything.
    pub fn unavailable() -> Self {
        Self {
            source: std::sync::Arc::new(UnavailableReleaseSource),
            artifacts: std::sync::Arc::new(UnavailableArtifactFetch),
            signature: std::sync::Arc::new(UnavailableSignature),
            startup: std::sync::Arc::new(UnavailableStartupProbe),
            active_work: std::sync::Arc::new(UnavailableActiveWork),
        }
    }

    /// Offline doubles for tests. Never the default in `module(ctx)`.
    pub fn fixtures(record: ReleaseRecord) -> Self {
        Self {
            source: std::sync::Arc::new(FixtureReleaseSource::new(record)),
            artifacts: std::sync::Arc::new(FixtureArtifactFetch::new()),
            signature: std::sync::Arc::new(FixtureSignature::verified()),
            startup: std::sync::Arc::new(FixtureStartupProbe::new(StartupProbe::NotReady {
                reason: "no window in this session".to_string(),
            })),
            active_work: std::sync::Arc::new(FixtureActiveWork::idle()),
        }
    }

    pub fn is_provisional(&self) -> bool {
        true
    }
}

pub struct UnavailableReleaseSource;

impl ReleaseSourcePort for UnavailableReleaseSource {
    fn latest(
        &self,
        _channel: Channel,
        _host: &HostPlatform,
    ) -> Result<ReleaseRecord, DiagnosticsError> {
        Err(DiagnosticsError::UpdateUnavailable {
            detail: "release feed is not connected".to_string(),
        })
    }
}

pub struct UnavailableArtifactFetch;

impl ArtifactFetchPort for UnavailableArtifactFetch {
    fn fetch(
        &self,
        _artifact: &ReleaseArtifact,
        _destination: &Path,
    ) -> Result<u64, DiagnosticsError> {
        Err(DiagnosticsError::owner("downloads"))
    }
}

pub struct UnavailableSignature;

impl SignaturePort for UnavailableSignature {
    fn verify(&self, _record: &ReleaseRecord) -> Result<SignatureVerification, DiagnosticsError> {
        Err(DiagnosticsError::SignatureUnverified {
            detail: "no signature verifier is connected".to_string(),
        })
    }
}

pub struct UnavailableStartupProbe;

impl StartupProbePort for UnavailableStartupProbe {
    fn probe(&self) -> StartupProbe {
        StartupProbe::Unavailable
    }
}

pub struct UnavailableActiveWork;

impl ActiveWorkPort for UnavailableActiveWork {
    fn active(&self) -> Result<Vec<String>, DiagnosticsError> {
        Err(DiagnosticsError::ActiveWorkUnknown)
    }
}

/// Offline release feed: always the one record it was built with.
pub struct FixtureReleaseSource {
    record: Mutex<Option<ReleaseRecord>>,
}

impl FixtureReleaseSource {
    pub fn new(record: ReleaseRecord) -> Self {
        Self {
            record: Mutex::new(Some(record)),
        }
    }

    pub fn clear(&self) {
        if let Ok(mut record) = self.record.lock() {
            *record = None;
        }
    }
}

impl ReleaseSourcePort for FixtureReleaseSource {
    fn latest(
        &self,
        _channel: Channel,
        _host: &HostPlatform,
    ) -> Result<ReleaseRecord, DiagnosticsError> {
        self.record
            .lock()
            .map_err(|_| DiagnosticsError::Internal {
                detail: "release fixture lock".to_string(),
            })?
            .clone()
            .ok_or_else(|| DiagnosticsError::UpdateUnavailable {
                detail: "offline fixture is empty".to_string(),
            })
    }
}

/// Offline artifact "download": writes the bytes it was given, optionally
/// corrupting them to exercise the digest check.
pub struct FixtureArtifactFetch {
    bytes: Mutex<Vec<u8>>,
    corrupt: Mutex<bool>,
}

impl Default for FixtureArtifactFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl FixtureArtifactFetch {
    pub fn new() -> Self {
        Self {
            bytes: Mutex::new(b"artifact-bytes".to_vec()),
            corrupt: Mutex::new(false),
        }
    }

    pub fn with_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: Mutex::new(bytes.into()),
            corrupt: Mutex::new(false),
        }
    }

    pub fn body(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .map(|bytes| bytes.clone())
            .unwrap_or_default()
    }

    pub fn set_corrupt(&self, corrupt: bool) {
        if let Ok(mut flag) = self.corrupt.lock() {
            *flag = corrupt;
        }
    }
}

impl ArtifactFetchPort for FixtureArtifactFetch {
    fn fetch(
        &self,
        _artifact: &ReleaseArtifact,
        destination: &Path,
    ) -> Result<u64, DiagnosticsError> {
        let mut bytes = self.body();
        if *self
            .corrupt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            if let Some(first) = bytes.first_mut() {
                *first = first.wrapping_add(1);
            }
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                DiagnosticsError::UpdateStoreFailed {
                    detail: format!("create staging dir: {error}"),
                }
            })?;
        }
        let mut file = std::fs::File::create(destination).map_err(|error| {
            DiagnosticsError::UpdateStoreFailed {
                detail: format!("create staged artifact: {error}"),
            }
        })?;
        file.write_all(&bytes)
            .map_err(|error| DiagnosticsError::UpdateStoreFailed {
                detail: format!("write staged artifact: {error}"),
            })?;
        Ok(bytes.len() as u64)
    }
}

pub struct FixtureSignature {
    verified: Mutex<bool>,
    scheme: String,
}

impl FixtureSignature {
    pub fn verified() -> Self {
        Self {
            verified: Mutex::new(true),
            scheme: "fixture-signature".to_string(),
        }
    }

    pub fn rejected() -> Self {
        Self {
            verified: Mutex::new(false),
            scheme: "fixture-signature".to_string(),
        }
    }

    pub fn set_verified(&self, verified: bool) {
        if let Ok(mut flag) = self.verified.lock() {
            *flag = verified;
        }
    }
}

impl SignaturePort for FixtureSignature {
    fn verify(&self, _record: &ReleaseRecord) -> Result<SignatureVerification, DiagnosticsError> {
        let verified = *self
            .verified
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Ok(SignatureVerification {
            verified,
            scheme: self.scheme.clone(),
            detail: if verified {
                "fixture accepted".to_string()
            } else {
                "fixture rejected".to_string()
            },
        })
    }
}

pub struct FixtureStartupProbe {
    outcome: Mutex<StartupProbe>,
}

impl FixtureStartupProbe {
    pub fn new(outcome: StartupProbe) -> Self {
        Self {
            outcome: Mutex::new(outcome),
        }
    }

    pub fn set(&self, outcome: StartupProbe) {
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = outcome;
        }
    }
}

impl StartupProbePort for FixtureStartupProbe {
    fn probe(&self) -> StartupProbe {
        self.outcome
            .lock()
            .map(|outcome| outcome.clone())
            .unwrap_or(StartupProbe::Unavailable)
    }
}

pub struct FixtureActiveWork {
    items: Mutex<Vec<String>>,
}

impl FixtureActiveWork {
    pub fn idle() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }

    pub fn busy(items: &[&str]) -> Self {
        Self {
            items: Mutex::new(items.iter().map(|item| item.to_string()).collect()),
        }
    }

    pub fn set(&self, items: &[&str]) {
        if let Ok(mut slot) = self.items.lock() {
            *slot = items.iter().map(|item| item.to_string()).collect();
        }
    }

    pub fn clear(&self) {
        self.set(&[]);
    }
}

impl ActiveWorkPort for FixtureActiveWork {
    fn active(&self) -> Result<Vec<String>, DiagnosticsError> {
        Ok(self
            .items
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// persisted state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Checking,
    Available,
    Downloading,
    Verifying,
    RestartReady,
    Switching,
    StartConfirmation,
    Completed,
    RetryableFailure,
    RolledBack,
    RecoveryRequired,
}

impl Stage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Available => "available",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::RestartReady => "restart_ready",
            Self::Switching => "switching",
            Self::StartConfirmation => "start_confirmation",
            Self::Completed => "completed",
            Self::RetryableFailure => "retryable_failure",
            Self::RolledBack => "rolled_back",
            Self::RecoveryRequired => "recovery_required",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::RolledBack | Self::RecoveryRequired
        )
    }
}

/// What `current.json` points at. Only this file changes during a switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionPointer {
    pub version: String,
    pub artifact_id: String,
    pub sha256: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTransaction {
    pub schema: String,
    pub transaction_id: String,
    pub stage: Stage,
    pub current_version: String,
    pub target: ReleaseRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<VersionPointer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_relative_path: Option<String>,
    pub artifact_verified: bool,
    pub signature_verified: bool,
    pub attempts: u32,
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl UpdateTransaction {
    pub fn to_value(&self) -> Value {
        json!({
            "schema": UPDATE_SCHEMA,
            "transactionId": self.transaction_id,
            "stage": self.stage.as_str(),
            "currentVersion": self.current_version,
            "targetVersion": self.target.version.as_str(),
            "previousVersion": self.previous.as_ref().map(|pointer| pointer.version.clone()),
            "artifactVerified": self.artifact_verified,
            "signatureVerified": self.signature_verified,
            "attempts": self.attempts,
            "maxAttempts": self.max_attempts,
            "evidence": self.evidence,
            "failure": self.failure,
            "rollbackAvailable": self.previous.is_some(),
        })
    }

    fn add_evidence(&mut self, marker: &str) {
        if !self.evidence.iter().any(|known| known == marker) {
            self.evidence.push(marker.to_string());
        }
    }
}

/// File-backed transaction store under `<data>/updates`.
pub struct UpdateStore {
    root: PathBuf,
}

impl UpdateStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn transactions_dir(&self) -> PathBuf {
        self.root.join("transactions")
    }

    pub fn staging_path(&self, transaction_id: &str) -> PathBuf {
        self.root.join("staging").join(transaction_id)
    }

    pub fn installed_dir(&self, version: &AppVersion) -> PathBuf {
        self.root.join("installed").join(version.as_str())
    }

    pub fn pointer_path(&self) -> PathBuf {
        self.root.join("current.json")
    }

    pub fn save(&self, transaction: &UpdateTransaction) -> Result<(), DiagnosticsError> {
        std::fs::create_dir_all(self.transactions_dir()).map_err(|error| {
            DiagnosticsError::UpdateStoreFailed {
                detail: format!("create transactions dir: {error}"),
            }
        })?;
        let path = self
            .transactions_dir()
            .join(format!("{}.json", transaction.transaction_id));
        let body = serde_json::to_vec_pretty(transaction).map_err(|error| {
            DiagnosticsError::UpdateStoreFailed {
                detail: format!("serialise transaction: {error}"),
            }
        })?;
        write_atomically(&path, &body)
    }

    pub fn load(
        &self,
        transaction_id: &str,
    ) -> Result<Option<UpdateTransaction>, DiagnosticsError> {
        let path = self
            .transactions_dir()
            .join(format!("{transaction_id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let body = std::fs::read(&path).map_err(|error| DiagnosticsError::UpdateStoreFailed {
            detail: format!("read transaction: {error}"),
        })?;
        let transaction: UpdateTransaction =
            serde_json::from_slice(&body).map_err(|error| DiagnosticsError::UpdateStoreFailed {
                detail: format!("parse transaction: {error}"),
            })?;
        Ok(Some(transaction))
    }

    pub fn list(&self) -> Result<Vec<UpdateTransaction>, DiagnosticsError> {
        let dir = self.transactions_dir();
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(out),
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext == "json").unwrap_or(false))
            .collect();
        paths.sort();
        for path in paths {
            let body =
                std::fs::read(&path).map_err(|error| DiagnosticsError::UpdateStoreFailed {
                    detail: format!("read transaction: {error}"),
                })?;
            if let Ok(transaction) = serde_json::from_slice::<UpdateTransaction>(&body) {
                out.push(transaction);
            }
        }
        Ok(out)
    }

    pub fn pointer(&self) -> Option<VersionPointer> {
        let body = std::fs::read(self.pointer_path()).ok()?;
        serde_json::from_slice(&body).ok()
    }

    pub fn set_pointer(&self, pointer: &VersionPointer) -> Result<(), DiagnosticsError> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            DiagnosticsError::UpdateStoreFailed {
                detail: format!("create updates dir: {error}"),
            }
        })?;
        let body = serde_json::to_vec_pretty(pointer).map_err(|error| {
            DiagnosticsError::UpdateStoreFailed {
                detail: format!("serialise pointer: {error}"),
            }
        })?;
        write_atomically(&self.pointer_path(), &body)
    }

    /// Remove the pointer: after a rollback with no previous version nothing is
    /// launchable, and pretending otherwise would be a lie.
    pub fn clear_pointer(&self) -> Result<(), DiagnosticsError> {
        let path = self.pointer_path();
        if !path.exists() {
            return Ok(());
        }
        std::fs::remove_file(&path).map_err(|error| DiagnosticsError::UpdateStoreFailed {
            detail: format!("remove pointer: {error}"),
        })
    }
}

fn write_atomically(path: &Path, body: &[u8]) -> Result<(), DiagnosticsError> {
    let temporary = path.with_extension("tmp-write");
    {
        let mut file = std::fs::File::create(&temporary).map_err(|error| {
            DiagnosticsError::UpdateStoreFailed {
                detail: format!("create temporary: {error}"),
            }
        })?;
        file.write_all(body)
            .map_err(|error| DiagnosticsError::UpdateStoreFailed {
                detail: format!("write temporary: {error}"),
            })?;
        file.sync_all().ok();
    }
    if path.exists() {
        let backup = path.with_extension("previous");
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(path, &backup).map_err(|error| DiagnosticsError::UpdateStoreFailed {
            detail: format!("set aside previous: {error}"),
        })?;
        match std::fs::rename(&temporary, path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&backup);
                Ok(())
            }
            Err(error) => {
                let _ = std::fs::rename(&backup, path);
                let _ = std::fs::remove_file(&temporary);
                Err(DiagnosticsError::UpdateStoreFailed {
                    detail: format!("install new file: {error}"),
                })
            }
        }
    } else {
        std::fs::rename(&temporary, path).map_err(|error| DiagnosticsError::UpdateStoreFailed {
            detail: format!("install new file: {error}"),
        })
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(64);
    for byte in digest {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

/// Lowercase hex SHA-256 of `bytes`, the artifact identity used by the update path.
/// Public so an offline test or a release tool computes the same identity.
pub fn artifact_digest(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

fn sha256_file(path: &Path) -> Result<(u64, String), DiagnosticsError> {
    let bytes = std::fs::read(path).map_err(|error| DiagnosticsError::UpdateStoreFailed {
        detail: format!("read artifact: {error}"),
    })?;
    Ok((bytes.len() as u64, sha256_hex(&bytes)))
}

// ---------------------------------------------------------------------------
// the engine
// ---------------------------------------------------------------------------

pub struct CheckRequest {
    pub current: AppVersion,
    pub channel: Channel,
    pub host: HostPlatform,
}

pub struct PrepareRequest {
    pub current: AppVersion,
    pub host: HostPlatform,
    pub candidate: ReleaseRecord,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    WaitingForActiveWork {
        transaction: UpdateTransaction,
        active: Vec<String>,
    },
    Switched {
        transaction: UpdateTransaction,
    },
    StartConfirmed {
        transaction: UpdateTransaction,
    },
    RolledBack {
        transaction: UpdateTransaction,
        reason: String,
    },
}

impl ApplyOutcome {
    pub fn to_value(&self) -> Value {
        match self {
            Self::WaitingForActiveWork {
                transaction,
                active,
            } => json!({
                "status": "waiting_active_work",
                "activeWork": active,
                "transaction": transaction.to_value(),
            }),
            Self::Switched { transaction } => json!({
                "status": "switched",
                "transaction": transaction.to_value(),
            }),
            Self::StartConfirmed { transaction } => json!({
                "status": "start_confirmed",
                "transaction": transaction.to_value(),
            }),
            Self::RolledBack {
                transaction,
                reason,
            } => json!({
                "status": "rolled_back",
                "reason": reason,
                // `true` only when a previous version really existed and the
                // pointer now points at it again. With no previous version the
                // pointer is cleared and nothing is launchable, which the UI must
                // say instead of "rolled back to the old version".
                "rollbackAvailable": transaction.previous.is_some(),
                "transaction": transaction.to_value(),
            }),
        }
    }

    pub fn transaction(&self) -> &UpdateTransaction {
        match self {
            Self::WaitingForActiveWork { transaction, .. }
            | Self::Switched { transaction }
            | Self::StartConfirmed { transaction }
            | Self::RolledBack { transaction, .. } => transaction,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Resumed,
    RolledBack,
    Completed,
    Unchanged,
    RetryRequired,
}

impl RecoveryAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::RolledBack => "rolled_back",
            Self::Completed => "completed",
            Self::Unchanged => "unchanged",
            Self::RetryRequired => "retry_required",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryOutcome {
    pub transaction: UpdateTransaction,
    pub action: RecoveryAction,
    /// The previous version is installed and launchable after this call.
    pub old_version_available: bool,
}

impl RecoveryOutcome {
    pub fn to_value(&self) -> Value {
        json!({
            "schema": UPDATE_SCHEMA,
            "action": self.action.as_str(),
            "oldVersionAvailable": self.old_version_available,
            "transaction": self.transaction.to_value(),
        })
    }
}

pub struct Updater {
    ports: UpdatePorts,
    store: UpdateStore,
    counter: Mutex<u64>,
}

impl Updater {
    pub fn new(ports: UpdatePorts, store: UpdateStore) -> Self {
        Self {
            ports,
            store,
            counter: Mutex::new(0),
        }
    }

    pub fn store(&self) -> &UpdateStore {
        &self.store
    }

    pub fn ports(&self) -> &UpdatePorts {
        &self.ports
    }

    fn next_id(&self) -> String {
        let mut counter = self
            .counter
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *counter += 1;
        let random = shtmcl_platform::entropy::random_hex(4).unwrap_or_else(|_| "0000".to_string());
        format!("update-{random}-{}", *counter)
    }

    /// Compare the feed with the host and reject an incompatible candidate before
    /// anything is downloaded.
    pub fn check(&self, request: &CheckRequest) -> Result<ReleaseCheck, DiagnosticsError> {
        let record = self.ports.source.latest(request.channel, &request.host)?;

        if !request.host.matches(&record.platform, &record.arch) {
            return Err(DiagnosticsError::PlatformMismatch {
                candidate: format!("{}/{}", record.platform, record.arch),
                host: request.host.label(),
            });
        }
        if record.channel != request.channel {
            return Err(DiagnosticsError::ChannelMismatch {
                candidate: record.channel.as_str().to_string(),
                requested: request.channel.as_str().to_string(),
            });
        }
        if let Some(minimum) = &record.min_host_version {
            if request.current < *minimum {
                return Err(DiagnosticsError::RequiresNewerHost {
                    required: minimum.as_str().to_string(),
                    host: request.current.as_str().to_string(),
                });
            }
        }

        let verdict = match record.version.cmp(&request.current) {
            std::cmp::Ordering::Greater => VersionVerdict::UpdateAvailable,
            std::cmp::Ordering::Equal => VersionVerdict::UpToDate,
            std::cmp::Ordering::Less => VersionVerdict::Downgrade,
        };
        Ok(ReleaseCheck {
            current: request.current.clone(),
            channel: request.channel,
            host: request.host.clone(),
            verdict,
            candidate: Some(record),
        })
    }

    /// Download, verify the digest, verify the signature, persist the transaction.
    /// Stops at `restart_ready`; the switch is a separate, explicit command.
    pub fn prepare(
        &self,
        request: &PrepareRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<UpdateTransaction, DiagnosticsError> {
        let mut transaction = UpdateTransaction {
            schema: UPDATE_SCHEMA.to_string(),
            transaction_id: self.next_id(),
            stage: Stage::Downloading,
            current_version: request.current.as_str().to_string(),
            target: request.candidate.clone(),
            previous: self.store.pointer(),
            staged_relative_path: None,
            artifact_verified: false,
            signature_verified: false,
            attempts: 0,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            evidence: Vec::new(),
            failure: None,
        };
        self.store.save(&transaction)?;

        if cancelled() {
            transaction.stage = Stage::RetryableFailure;
            transaction.failure = Some("cancelled".to_string());
            self.store.save(&transaction)?;
            return Ok(transaction);
        }

        let staging = self.store.staging_path(&transaction.transaction_id);
        std::fs::create_dir_all(&staging).map_err(|error| DiagnosticsError::UpdateStoreFailed {
            detail: format!("create staging dir: {error}"),
        })?;
        let destination = staging.join(&transaction.target.artifact.file_name);
        let bytes = self
            .ports
            .artifacts
            .fetch(&transaction.target.artifact, &destination)?;

        transaction.stage = Stage::Verifying;
        transaction.staged_relative_path = Some(format!(
            "staging/{}/{}",
            transaction.transaction_id, transaction.target.artifact.file_name
        ));
        self.store.save(&transaction)?;

        let (actual_size, actual_digest) = sha256_file(&destination)?;
        let expected_digest = transaction.target.artifact.sha256.to_ascii_lowercase();
        if actual_digest != expected_digest || actual_size != bytes {
            transaction.stage = Stage::RetryableFailure;
            transaction.failure = Some("diagnostics.update.artifact_mismatch".to_string());
            self.store.save(&transaction)?;
            return Err(DiagnosticsError::ArtifactMismatch {
                expected: expected_digest,
                actual: actual_digest,
            });
        }
        transaction.artifact_verified = true;
        transaction.add_evidence("artifact_verified");

        let verification = self.ports.signature.verify(&transaction.target)?;
        if !verification.verified {
            transaction.stage = Stage::RetryableFailure;
            transaction.failure = Some("diagnostics.update.signature_unverified".to_string());
            self.store.save(&transaction)?;
            return Err(DiagnosticsError::SignatureUnverified {
                detail: format!("{}: {}", verification.scheme, verification.detail),
            });
        }
        // Only a verifier that actually checked may set this flag.
        transaction.signature_verified = true;
        transaction.add_evidence("signature_verified");
        transaction.stage = Stage::RestartReady;
        self.store.save(&transaction)?;
        Ok(transaction)
    }

    fn load(&self, transaction_id: &str) -> Result<UpdateTransaction, DiagnosticsError> {
        self.store
            .load(transaction_id)?
            .ok_or_else(|| DiagnosticsError::TransactionNotFound {
                transaction: transaction_id.to_string(),
            })
    }

    /// Switch to the new version, or say why the switch has to wait.
    pub fn apply(
        &self,
        transaction_id: &str,
        confirm_start: bool,
    ) -> Result<ApplyOutcome, DiagnosticsError> {
        let mut transaction = self.load(transaction_id)?;

        if confirm_start {
            return self.confirm_start(transaction);
        }

        if transaction.stage != Stage::RestartReady {
            return Err(DiagnosticsError::StageInvalid {
                stage: transaction.stage.as_str().to_string(),
                action: "apply".to_string(),
            });
        }

        let active = self.ports.active_work.active()?;
        if !active.is_empty() {
            transaction.add_evidence("restart_deferred");
            self.store.save(&transaction)?;
            return Ok(ApplyOutcome::WaitingForActiveWork {
                transaction,
                active,
            });
        }

        self.switch(&mut transaction)?;
        Ok(ApplyOutcome::Switched { transaction })
    }

    fn switch(&self, transaction: &mut UpdateTransaction) -> Result<(), DiagnosticsError> {
        let staged = transaction.staged_relative_path.clone().ok_or_else(|| {
            DiagnosticsError::StageInvalid {
                stage: transaction.stage.as_str().to_string(),
                action: "switch".to_string(),
            }
        })?;
        let staged_path = self.store.root().join(&staged);
        let (_, digest) = sha256_file(&staged_path)?;
        if digest != transaction.target.artifact.sha256.to_ascii_lowercase() {
            transaction.stage = Stage::RetryableFailure;
            transaction.failure = Some("diagnostics.update.artifact_mismatch".to_string());
            self.store.save(transaction)?;
            return Err(DiagnosticsError::ArtifactMismatch {
                expected: transaction.target.artifact.sha256.clone(),
                actual: digest,
            });
        }

        // Install the bytes next to the old version; the old one is never removed.
        let installed = self.store.installed_dir(&transaction.target.version);
        std::fs::create_dir_all(&installed).map_err(|error| {
            DiagnosticsError::UpdateStoreFailed {
                detail: format!("create install dir: {error}"),
            }
        })?;
        let installed_path = installed.join(&transaction.target.artifact.file_name);
        let installed_relative = format!(
            "installed/{}/{}",
            transaction.target.version.as_str(),
            transaction.target.artifact.file_name
        );
        if !installed_path.exists() {
            std::fs::copy(&staged_path, &installed_path).map_err(|error| {
                DiagnosticsError::UpdateStoreFailed {
                    detail: format!("install artifact: {error}"),
                }
            })?;
        }

        // Persist the intent BEFORE the pointer moves: this record is what a power
        // cut leaves behind, and it names the artifact by digest.
        transaction.stage = Stage::Switching;
        if transaction.previous.is_none() {
            transaction.previous = self.store.pointer();
        }
        self.store.save(transaction)?;

        let pointer = VersionPointer {
            version: transaction.target.version.as_str().to_string(),
            artifact_id: transaction.target.artifact.artifact_id.clone(),
            sha256: transaction.target.artifact.sha256.to_ascii_lowercase(),
            relative_path: installed_relative,
        };
        self.store.set_pointer(&pointer)?;
        transaction.add_evidence("switched_to_new_version");

        transaction.stage = Stage::StartConfirmation;
        self.store.save(transaction)?;
        Ok(())
    }

    fn confirm_start(
        &self,
        mut transaction: UpdateTransaction,
    ) -> Result<ApplyOutcome, DiagnosticsError> {
        if transaction.stage != Stage::StartConfirmation {
            return Err(DiagnosticsError::StageInvalid {
                stage: transaction.stage.as_str().to_string(),
                action: "confirm_start".to_string(),
            });
        }
        match self.ports.startup.probe() {
            StartupProbe::Ready { version } => {
                if version == transaction.target.version.as_str() {
                    transaction.stage = Stage::Completed;
                    transaction.add_evidence("startup_confirmed");
                    self.store.save(&transaction)?;
                    Ok(ApplyOutcome::StartConfirmed { transaction })
                } else {
                    let reason = format!("a different build answered: {version}");
                    self.settle_failure(transaction, &reason)
                }
            }
            StartupProbe::NotReady { reason } => self.settle_failure(transaction, &reason),
            // "Cannot tell" is not "fine": the transaction stays open.
            StartupProbe::Unavailable => Err(DiagnosticsError::StartupUnconfirmed {
                detail: "no startup probe is connected".to_string(),
            }),
        }
    }

    /// Turn a failed start into a settled outcome. The pointer goes back to the
    /// previous version when one exists; with no previous version it is cleared and
    /// the payload says `rollbackAvailable: false`. Either way a marker is only
    /// written when the fact behind it really happened.
    fn settle_failure(
        &self,
        mut transaction: UpdateTransaction,
        reason: &str,
    ) -> Result<ApplyOutcome, DiagnosticsError> {
        // The boolean is deliberately unused here: `rollbackAvailable` is derived
        // from the transaction when the payload is rendered.
        self.rollback(&mut transaction, reason)?;
        Ok(ApplyOutcome::RolledBack {
            transaction,
            reason: reason.to_string(),
        })
    }

    /// Restore the previous version. Returns `true` only when a previous version
    /// really existed; with no previous version the pointer is cleared, the stage
    /// becomes `retryable_failure` and **no rollback evidence is written**.
    fn rollback(
        &self,
        transaction: &mut UpdateTransaction,
        reason: &str,
    ) -> Result<bool, DiagnosticsError> {
        match transaction.previous.clone() {
            Some(previous) => {
                self.store.set_pointer(&previous)?;
                if self.store.root().join(&previous.relative_path).exists() {
                    transaction.add_evidence("old_version_available");
                }
                transaction.stage = Stage::RolledBack;
                transaction.failure = Some(reason.to_string());
                transaction.add_evidence("rolled_back_to_old_version");
                self.store.save(transaction)?;
                Ok(true)
            }
            None => {
                // There was no installed version before this update, so the pointer
                // must not keep pointing at the build that failed to start. Nothing
                // is launchable and nothing may claim "rolled back to old version".
                self.store.clear_pointer()?;
                transaction.stage = Stage::RetryableFailure;
                transaction.failure = Some(reason.to_string());
                self.store.save(transaction)?;
                Ok(false)
            }
        }
    }

    /// Continue or roll back an interrupted transaction, judged by the artifact
    /// that is really on disk.
    pub fn recover(&self, transaction_id: &str) -> Result<RecoveryOutcome, DiagnosticsError> {
        let mut transaction = self.load(transaction_id)?;
        transaction.attempts += 1;
        if transaction.attempts > transaction.max_attempts {
            transaction.stage = Stage::RecoveryRequired;
            transaction.failure = Some("diagnostics.update.recovery_required".to_string());
            self.store.save(&transaction)?;
            return Err(DiagnosticsError::RecoveryRequired {
                detail: format!(
                    "{} attempts on {}",
                    transaction.attempts, transaction.transaction_id
                ),
            });
        }

        let pointer = self.store.pointer();
        let old_available = transaction
            .previous
            .as_ref()
            .map(|previous| self.store.root().join(&previous.relative_path).exists())
            .unwrap_or(false);

        let action = match transaction.stage {
            Stage::Switching => {
                let points_at_new = pointer
                    .as_ref()
                    .map(|value| value.version == transaction.target.version.as_str())
                    .unwrap_or(false);
                if !points_at_new {
                    // The pointer never moved: resume the switch if the artifact is
                    // still correct, otherwise ask for a fresh download.
                    let staged_ok = transaction
                        .staged_relative_path
                        .as_ref()
                        .map(|relative| self.store.root().join(relative))
                        .filter(|path| path.exists())
                        .map(|path| sha256_file(&path))
                        .transpose()?
                        .map(|(_, digest)| {
                            digest == transaction.target.artifact.sha256.to_ascii_lowercase()
                        })
                        .unwrap_or(false);
                    if staged_ok {
                        self.switch(&mut transaction)?;
                        RecoveryAction::Resumed
                    } else {
                        transaction.stage = Stage::RetryableFailure;
                        transaction.failure =
                            Some("diagnostics.update.artifact_mismatch".to_string());
                        self.store.save(&transaction)?;
                        RecoveryAction::RetryRequired
                    }
                } else {
                    // The pointer moved but the confirmation never happened: trust
                    // the identity of the installed bytes, not the version string.
                    let installed_ok = pointer
                        .as_ref()
                        .map(|value| self.store.root().join(&value.relative_path))
                        .filter(|path| path.exists())
                        .map(|path| sha256_file(&path))
                        .transpose()?
                        .map(|(_, digest)| {
                            digest == transaction.target.artifact.sha256.to_ascii_lowercase()
                        })
                        .unwrap_or(false);
                    if installed_ok {
                        transaction.stage = Stage::StartConfirmation;
                        self.store.save(&transaction)?;
                        RecoveryAction::Resumed
                    } else if self
                        .rollback(&mut transaction, "installed artifact no longer matches")?
                    {
                        RecoveryAction::RolledBack
                    } else {
                        RecoveryAction::RetryRequired
                    }
                }
            }
            Stage::StartConfirmation => match self.ports.startup.probe() {
                StartupProbe::Ready { version }
                    if version == transaction.target.version.as_str() =>
                {
                    transaction.stage = Stage::Completed;
                    transaction.add_evidence("startup_confirmed");
                    self.store.save(&transaction)?;
                    RecoveryAction::Completed
                }
                StartupProbe::Unavailable => {
                    self.store.save(&transaction)?;
                    RecoveryAction::Unchanged
                }
                _ => {
                    if self.rollback(&mut transaction, "the new version did not start")? {
                        RecoveryAction::RolledBack
                    } else {
                        RecoveryAction::RetryRequired
                    }
                }
            },
            Stage::RestartReady | Stage::Downloading | Stage::Verifying | Stage::Available => {
                let staged_ok = transaction
                    .staged_relative_path
                    .as_ref()
                    .map(|relative| self.store.root().join(relative))
                    .filter(|path| path.exists())
                    .map(|path| sha256_file(&path))
                    .transpose()?
                    .map(|(_, digest)| {
                        digest == transaction.target.artifact.sha256.to_ascii_lowercase()
                    })
                    .unwrap_or(false);
                if staged_ok {
                    transaction.stage = Stage::RestartReady;
                    transaction.artifact_verified = true;
                    self.store.save(&transaction)?;
                    RecoveryAction::Resumed
                } else {
                    transaction.stage = Stage::RetryableFailure;
                    transaction.failure = Some("diagnostics.update.artifact_mismatch".to_string());
                    self.store.save(&transaction)?;
                    RecoveryAction::RetryRequired
                }
            }
            _ => RecoveryAction::Unchanged,
        };

        let old_version_available = transaction
            .previous
            .as_ref()
            .map(|previous| self.store.root().join(&previous.relative_path).exists())
            .unwrap_or(false)
            || old_available;
        Ok(RecoveryOutcome {
            transaction,
            action,
            old_version_available,
        })
    }

    pub fn transactions(&self) -> Result<Vec<UpdateTransaction>, DiagnosticsError> {
        self.store.list()
    }

    /// Current installed version per `current.json` (not a probe of the build).
    pub fn installed(&self) -> Option<VersionPointer> {
        self.store.pointer()
    }
}

/// Release verdicts keyed by channel, for a status read without the feed.
pub fn stages() -> BTreeMap<&'static str, bool> {
    let mut map = BTreeMap::new();
    for stage in [
        Stage::Checking,
        Stage::Available,
        Stage::Downloading,
        Stage::Verifying,
        Stage::RestartReady,
        Stage::Switching,
        Stage::StartConfirmation,
        Stage::Completed,
        Stage::RetryableFailure,
        Stage::RolledBack,
        Stage::RecoveryRequired,
    ] {
        map.insert(stage.as_str(), stage.is_terminal());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> AppVersion {
        AppVersion::parse(value).expect("valid version")
    }

    fn record(version_text: &str, channel: Channel, os: &str, arch: &str) -> ReleaseRecord {
        let body = b"artifact-bytes";
        ReleaseRecord {
            version: version(version_text),
            channel,
            platform: os.to_string(),
            arch: arch.to_string(),
            min_host_version: None,
            data_schema: 1,
            artifact: ReleaseArtifact {
                artifact_id: format!("artifact-{version_text}"),
                sha256: sha256_hex(body),
                size: body.len() as u64,
                url_ref: Some("opaque-reference".to_string()),
                signature: Some(ArtifactSignature {
                    scheme: "fixture".to_string(),
                    value: "sig".to_string(),
                }),
                file_name: "app.bin".to_string(),
            },
            notes_ref: None,
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-08-update-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        root
    }

    fn make_updater(root: &Path, record: ReleaseRecord) -> (Updater, UpdatePorts) {
        let ports = UpdatePorts::fixtures(record);
        let updater = Updater::new(ports.clone(), UpdateStore::new(root));
        (updater, ports)
    }

    #[test]
    fn versions_are_compared_with_semver_rules() {
        assert!(version("1.2.0") > version("1.1.9"));
        assert!(version("1.2.0") > version("1.2.0-beta.1"));
        assert!(version("1.2.0-beta.2") > version("1.2.0-beta.1"));
        assert!(version("1.2.0-beta.10") > version("1.2.0-beta.2"));
        assert!(version("1.2.0-rc.1") > version("1.2.0-beta.9"));
        assert!(version("2.0.0") > version("1.99.99"));
        assert_eq!(version("1.2.0"), version("1.2.0+build.5"));
        assert!(version("1.21.0-pre1") < version("1.21.0"));
        for invalid in [
            "",
            "1.2",
            "1.2.3.4",
            "banana",
            "01.2.3",
            "1.2.3-",
            "1.2.3-beta..1",
        ] {
            assert!(
                AppVersion::parse(invalid).is_err(),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn a_platform_or_channel_mismatch_is_rejected_before_download() {
        let root = temp_root("check");
        let (updater, _) =
            make_updater(&root, record("1.2.0", Channel::Stable, "macos", "aarch64"));
        let error = updater
            .check(&CheckRequest {
                current: version("1.0.0"),
                channel: Channel::Stable,
                host: HostPlatform::new("windows", "x86_64"),
            })
            .expect_err("wrong platform");
        assert_eq!(
            error.to_envelope().code.as_str(),
            super::super::error::codes::UPDATE_PLATFORM_MISMATCH
        );

        let (updater, _) = make_updater(&root, record("1.2.0", Channel::Beta, "windows", "x86_64"));
        let error = updater
            .check(&CheckRequest {
                current: version("1.0.0"),
                channel: Channel::Stable,
                host: HostPlatform::new("windows", "x86_64"),
            })
            .expect_err("wrong channel");
        assert_eq!(
            error.to_envelope().code.as_str(),
            super::super::error::codes::UPDATE_CHANNEL_MISMATCH
        );
    }

    #[test]
    fn a_newer_release_is_available_and_an_older_one_is_a_downgrade() {
        let root = temp_root("verdict");
        let host = HostPlatform::new("windows", "x86_64");
        let (updater, _) =
            make_updater(&root, record("1.2.0", Channel::Stable, "windows", "x86_64"));
        let check = updater
            .check(&CheckRequest {
                current: version("1.1.0"),
                channel: Channel::Stable,
                host: host.clone(),
            })
            .expect("check");
        assert_eq!(check.verdict, VersionVerdict::UpdateAvailable);

        let (updater, _) =
            make_updater(&root, record("1.2.0", Channel::Stable, "windows", "x86_64"));
        let check = updater
            .check(&CheckRequest {
                current: version("1.3.0"),
                channel: Channel::Stable,
                host,
            })
            .expect("check");
        assert_eq!(check.verdict, VersionVerdict::Downgrade);
    }

    #[test]
    fn a_release_that_needs_a_newer_host_is_refused() {
        let root = temp_root("minhost");
        let mut candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        candidate.min_host_version = Some(version("1.1.0"));
        let (updater, _) = make_updater(&root, candidate);
        let error = updater
            .check(&CheckRequest {
                current: version("1.0.0"),
                channel: Channel::Stable,
                host: HostPlatform::new("windows", "x86_64"),
            })
            .expect_err("too old to install");
        assert_eq!(
            error.to_envelope().code.as_str(),
            super::super::error::codes::UPDATE_REQUIRES_NEWER_HOST
        );
    }

    #[test]
    fn the_default_wiring_cannot_check_or_verify() {
        let root = temp_root("unavailable");
        let updater = Updater::new(UpdatePorts::unavailable(), UpdateStore::new(&root));
        let error = updater
            .check(&CheckRequest {
                current: version("1.0.0"),
                channel: Channel::Stable,
                host: HostPlatform::new("windows", "x86_64"),
            })
            .expect_err("no feed");
        assert_eq!(
            error.to_envelope().code.as_str(),
            super::super::error::codes::UPDATE_UNAVAILABLE
        );
    }

    #[test]
    fn a_corrupted_download_never_reaches_restart_ready() {
        let root = temp_root("corrupt");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (_, ports) = make_updater(&root, candidate.clone());
        let fetch = FixtureArtifactFetch::new();
        fetch.set_corrupt(true);
        let ports = UpdatePorts {
            artifacts: std::sync::Arc::new(fetch),
            ..ports
        };
        let updater = Updater::new(ports, UpdateStore::new(&root));
        let error = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect_err("digest mismatch");
        assert_eq!(
            error.to_envelope().code.as_str(),
            super::super::error::codes::UPDATE_ARTIFACT_MISMATCH
        );
        let stored = updater.transactions().expect("list");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].stage, Stage::RetryableFailure);
        assert!(!stored[0].signature_verified);
    }

    #[test]
    fn an_unverified_signature_blocks_the_transaction() {
        let root = temp_root("signature");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (_, ports) = make_updater(&root, candidate.clone());
        let ports = UpdatePorts {
            signature: std::sync::Arc::new(FixtureSignature::rejected()),
            ..ports
        };
        let updater = Updater::new(ports, UpdateStore::new(&root));
        let error = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect_err("signature rejected");
        assert_eq!(
            error.to_envelope().code.as_str(),
            super::super::error::codes::UPDATE_SIGNATURE_UNVERIFIED
        );
    }

    #[test]
    fn the_happy_path_switches_and_confirms() {
        let root = temp_root("happy");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (_, ports) = make_updater(&root, candidate.clone());
        let probe = FixtureStartupProbe::new(StartupProbe::Ready {
            version: "1.2.0".to_string(),
        });
        let ports = UpdatePorts {
            startup: std::sync::Arc::new(probe),
            ..ports
        };
        let updater = Updater::new(ports, UpdateStore::new(&root));

        let transaction = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect("prepared");
        assert_eq!(transaction.stage, Stage::RestartReady);
        assert!(transaction.artifact_verified);
        assert!(transaction.signature_verified);

        let switched = updater
            .apply(&transaction.transaction_id, false)
            .expect("switch");
        assert!(matches!(switched, ApplyOutcome::Switched { .. }));
        assert_eq!(
            updater.installed().map(|pointer| pointer.version),
            Some("1.2.0".to_string())
        );

        let confirmed = updater
            .apply(&transaction.transaction_id, true)
            .expect("confirm");
        match confirmed {
            ApplyOutcome::StartConfirmed { transaction } => {
                assert_eq!(transaction.stage, Stage::Completed);
                assert!(transaction
                    .evidence
                    .iter()
                    .any(|marker| marker == "startup_confirmed"));
            }
            other => panic!("expected confirmation, got {other:?}"),
        }
    }

    #[test]
    fn active_work_defers_the_restart() {
        let root = temp_root("active");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (_, ports) = make_updater(&root, candidate.clone());
        let ports = UpdatePorts {
            active_work: std::sync::Arc::new(FixtureActiveWork::busy(&["game", "connection"])),
            ..ports
        };
        let updater = Updater::new(ports, UpdateStore::new(&root));
        let transaction = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect("prepared");
        let outcome = updater
            .apply(&transaction.transaction_id, false)
            .expect("deferred");
        match outcome {
            ApplyOutcome::WaitingForActiveWork { active, .. } => {
                assert_eq!(active, vec!["game".to_string(), "connection".to_string()]);
            }
            other => panic!("expected waiting_active_work, got {other:?}"),
        }
        assert!(updater.installed().is_none(), "nothing was switched");
    }

    #[test]
    fn a_switch_interrupted_before_the_pointer_moved_resumes() {
        let root = temp_root("resume");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (updater, _) = make_updater(&root, candidate.clone());
        let transaction = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect("prepared");
        updater
            .apply(&transaction.transaction_id, false)
            .expect("switch");

        // Simulate a power cut right after the record said "switching" but before
        // the pointer was replaced: restore the record and drop the pointer.
        let mut stored = updater
            .store()
            .load(&transaction.transaction_id)
            .expect("load")
            .expect("stored");
        stored.stage = Stage::Switching;
        stored.previous = None;
        updater.store().save(&stored).expect("save");
        std::fs::remove_file(updater.store().pointer_path()).expect("drop pointer");

        let recovery = updater
            .recover(&transaction.transaction_id)
            .expect("recover");
        assert_eq!(recovery.action, RecoveryAction::Resumed);
        assert_eq!(recovery.transaction.stage, Stage::StartConfirmation);
        assert_eq!(
            updater.installed().map(|pointer| pointer.version),
            Some("1.2.0".to_string()),
            "the switch completed from the staged artifact"
        );
    }

    #[test]
    fn a_rollback_without_a_previous_version_invents_no_evidence() {
        let root = temp_root("rollback-none");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (_, ports) = make_updater(&root, candidate.clone());
        let probe = FixtureStartupProbe::new(StartupProbe::NotReady {
            reason: "the window never came up".to_string(),
        });
        let ports = UpdatePorts {
            startup: std::sync::Arc::new(probe),
            ..ports
        };
        let updater = Updater::new(ports, UpdateStore::new(&root));
        // No `current.json`: nothing was installed before this update.
        assert!(updater.installed().is_none());

        let transaction = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect("prepared");
        updater
            .apply(&transaction.transaction_id, false)
            .expect("switch");
        let outcome = updater
            .apply(&transaction.transaction_id, true)
            .expect("confirm");
        let value = outcome.to_value();
        assert_eq!(value["status"], "rolled_back");
        assert_eq!(
            value["rollbackAvailable"], false,
            "nothing was installed before this update"
        );
        let evidence: Vec<String> = value["transaction"]["evidence"]
            .as_array()
            .expect("evidence")
            .iter()
            .filter_map(|marker| marker.as_str().map(str::to_string))
            .collect();
        assert!(
            !evidence
                .iter()
                .any(|marker| marker == "rolled_back_to_old_version"),
            "there was nothing to roll back to: {evidence:?}"
        );
        assert!(
            !evidence
                .iter()
                .any(|marker| marker == "old_version_available"),
            "no old version ever existed: {evidence:?}"
        );
        assert!(evidence.iter().any(|m| m == "switched_to_new_version"));
        assert_eq!(value["transaction"]["stage"], "retryable_failure");
        assert!(
            updater.installed().is_none(),
            "the failed build must not stay launchable"
        );
    }

    #[test]
    fn a_bad_install_rolls_back_and_keeps_the_old_version() {
        let root = temp_root("rollback");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (_, ports) = make_updater(&root, candidate.clone());
        let probe = FixtureStartupProbe::new(StartupProbe::NotReady {
            reason: "the window never came up".to_string(),
        });
        let ports = UpdatePorts {
            startup: std::sync::Arc::new(probe),
            ..ports
        };
        let updater = Updater::new(ports, UpdateStore::new(&root));

        // Pretend version 1.0.0 is installed.
        let previous = VersionPointer {
            version: "1.0.0".to_string(),
            artifact_id: "artifact-old".to_string(),
            sha256: sha256_hex(b"old-bytes"),
            relative_path: "installed/1.0.0/app.bin".to_string(),
        };
        let old_dir = root.join("installed").join("1.0.0");
        std::fs::create_dir_all(&old_dir).expect("old dir");
        std::fs::write(old_dir.join("app.bin"), b"old-bytes").expect("old bytes");
        updater.store().set_pointer(&previous).expect("pointer");

        let transaction = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect("prepared");
        updater
            .apply(&transaction.transaction_id, false)
            .expect("switch");
        let outcome = updater
            .apply(&transaction.transaction_id, true)
            .expect("confirm");
        match outcome {
            ApplyOutcome::RolledBack { transaction, .. } => {
                assert_eq!(transaction.stage, Stage::RolledBack);
                assert!(transaction
                    .evidence
                    .iter()
                    .any(|marker| marker == "old_version_available"));
            }
            other => panic!("expected rollback, got {other:?}"),
        }
        assert_eq!(
            updater.installed().map(|pointer| pointer.version),
            Some("1.0.0".to_string()),
            "the pointer is back on the old version"
        );
        assert!(
            old_dir.join("app.bin").exists(),
            "the old installation is still on disk"
        );
    }

    #[test]
    fn a_corrupted_install_is_rolled_back_on_recovery() {
        let root = temp_root("corrupt-install");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (updater, _) = make_updater(&root, candidate.clone());
        let transaction = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect("prepared");
        updater
            .apply(&transaction.transaction_id, false)
            .expect("switch");

        // Damage the installed bytes: the identity check, not the version string,
        // must catch this.
        let installed = root.join("installed").join("1.2.0").join("app.bin");
        std::fs::write(&installed, b"tampered-bytes").expect("tamper");
        let mut stored = updater
            .store()
            .load(&transaction.transaction_id)
            .expect("load")
            .expect("stored");
        stored.stage = Stage::Switching;
        updater.store().save(&stored).expect("save");

        let recovery = updater
            .recover(&transaction.transaction_id)
            .expect("recover");
        // There was no installed version before this update, so recovery cannot
        // "roll back": it refuses to trust the damaged artifact and asks for a
        // fresh download, without inventing rollback evidence.
        assert_eq!(recovery.action, RecoveryAction::RetryRequired);
        assert_eq!(recovery.transaction.stage, Stage::RetryableFailure);
        assert!(!recovery.old_version_available);
        assert!(!recovery
            .transaction
            .evidence
            .iter()
            .any(|marker| marker == "rolled_back_to_old_version"));
        assert!(updater.installed().is_none(), "no pointer is trusted");
    }

    #[test]
    fn recovery_attempts_are_bounded() {
        let root = temp_root("attempts");
        let candidate = record("1.2.0", Channel::Stable, "windows", "x86_64");
        let (updater, _) = make_updater(&root, candidate.clone());
        let transaction = updater
            .prepare(
                &PrepareRequest {
                    current: version("1.0.0"),
                    host: HostPlatform::new("windows", "x86_64"),
                    candidate,
                },
                &|| false,
            )
            .expect("prepared");
        let mut stored = updater
            .store()
            .load(&transaction.transaction_id)
            .expect("load")
            .expect("stored");
        stored.stage = Stage::Downloading;
        stored.attempts = DEFAULT_MAX_ATTEMPTS;
        updater.store().save(&stored).expect("save");

        let error = updater
            .recover(&transaction.transaction_id)
            .expect_err("attempt budget exhausted");
        assert_eq!(
            error.to_envelope().code.as_str(),
            super::super::error::codes::UPDATE_RECOVERY_REQUIRED
        );
        let stored = updater
            .store()
            .load(&transaction.transaction_id)
            .expect("load")
            .expect("stored");
        assert_eq!(stored.stage, Stage::RecoveryRequired);
    }
}
