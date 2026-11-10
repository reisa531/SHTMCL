//! Java runtime discovery, probing, caching and constraint matching.
//!
//! Design rules taken from `docs/development/02-launch-and-runtime.md` §5:
//!
//! - scanning is restricted to recorded paths, a small set of conventional system
//!   locations and user-supplied roots; never the whole disk,
//! - a candidate is verified by running `java -version` in a short-lived process
//!   with a timeout, through `shtmcl_platform::ProcessLauncher`,
//! - results are cached by (path, file identity, verifier version, platform) and
//!   invalidated when the file changes,
//! - version comparison is structural, never a string comparison,
//! - a candidate the user explicitly pinned is reported with its mismatch reason
//!   instead of being silently replaced by another runtime,
//! - the UI never receives raw stderr: a probe failure becomes a stable code.
//!
//! The probe answers only *existence, version, vendor and architecture*. Whether
//! an architecture is usable is decided by matching against the host.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shtmcl_platform::dirs::PlatformDirs;
use shtmcl_platform::fs::{self, FileIdentity};
use shtmcl_platform::process::ProcessLauncher;

use crate::launch::errors;
use crate::launch::ports::{ProcessSupervisor, WaitOutcome};

/// Initial probe timeout per candidate (3 s, per the module design).
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Initial global probe concurrency.
pub const DEFAULT_PROBE_CONCURRENCY: usize = 2;
/// Version of the parsing rules. Bumping it invalidates every cache entry.
pub const VERIFIER_VERSION: u32 = 1;
/// Bounded scan: how deep below each root we look for `*/bin/java`.
pub const DEFAULT_MAX_DEPTH: usize = 3;
/// Bounded scan: how many entries per directory we consider.
pub const DEFAULT_MAX_ENTRIES_PER_DIR: usize = 64;
/// Cap on captured probe output per stream.
const PROBE_OUTPUT_CAP: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostPlatform {
    Windows,
    MacOs,
    Linux,
    Unknown,
}

impl HostPlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unknown
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::MacOs => "macos",
            Self::Linux => "linux",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "windows" => Some(Self::Windows),
            "macos" => Some(Self::MacOs),
            "linux" => Some(Self::Linux),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Arch {
    X64,
    Arm64,
    X86,
    Unknown,
}

impl Arch {
    pub fn current() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => Self::X64,
            "aarch64" => Self::Arm64,
            "x86" => Self::X86,
            _ => Self::Unknown,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
            Self::X86 => "x86",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "x64" | "x86_64" => Some(Self::X64),
            "arm64" | "aarch64" => Some(Self::Arm64),
            "x86" => Some(Self::X86),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaVendor {
    OpenJdk,
    Oracle,
    Temurin,
    Corretto,
    Zulu,
    GraalVm,
    Liberica,
    SapMachine,
    Microsoft,
    Unknown,
}

impl JavaVendor {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenJdk => "openjdk",
            Self::Oracle => "oracle",
            Self::Temurin => "temurin",
            Self::Corretto => "corretto",
            Self::Zulu => "zulu",
            Self::GraalVm => "graalvm",
            Self::Liberica => "liberica",
            Self::SapMachine => "sapmachine",
            Self::Microsoft => "microsoft",
            Self::Unknown => "unknown",
        }
    }
}

/// Structural Java version. `major` is the value the game requires
/// (`1.8.0_402` becomes `8`, `21.0.2` stays `21`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaVersion {
    pub major: u32,
    pub minor: u32,
    pub security: u32,
    pub update: Option<u32>,
    pub raw: String,
}

impl JavaVersion {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "major": self.major,
            "minor": self.minor,
            "security": self.security,
            "update": self.update,
            "raw": self.raw,
        })
    }
}

/// Parse a bare version token such as `21.0.2`, `1.8.0_402`, `21-ea`, `21.0.2+13-LTS`.
pub fn parse_version_token(token: &str) -> Option<JavaVersion> {
    let token = token.trim().trim_matches('"');
    if token.is_empty() {
        return None;
    }
    let mut split = token.splitn(2, '_');
    let dotted = split.next().unwrap_or_default();
    let update = split
        .next()
        .and_then(|rest| {
            rest.split(|character: char| !character.is_ascii_digit())
                .next()
        })
        .filter(|digits| !digits.is_empty())
        .and_then(|digits| digits.parse::<u32>().ok());

    let mut numbers: Vec<u32> = Vec::new();
    for part in dotted.split('.') {
        let digits: String = part
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            break;
        }
        numbers.push(digits.parse::<u32>().ok()?);
    }
    if numbers.is_empty() {
        return None;
    }

    let (major, rest) = if numbers[0] == 1 && numbers.len() >= 2 {
        (numbers[1], &numbers[2..])
    } else {
        (numbers[0], &numbers[1..])
    };
    if major == 0 {
        return None;
    }
    Some(JavaVersion {
        major,
        minor: rest.first().copied().unwrap_or(0),
        security: rest.get(1).copied().unwrap_or(0),
        update,
        raw: token.to_string(),
    })
}

/// Parse the whole `java -version` output (banner lines are vendor specific).
pub fn parse_version_output(text: &str) -> Option<JavaVersion> {
    // Preferred shape: `java version "21.0.2" 2024-01-16 LTS`.
    for (index, character) in text.char_indices() {
        if character != '"' {
            continue;
        }
        let rest = &text[index + 1..];
        if let Some(end) = rest.find('"') {
            if let Some(version) = parse_version_token(&rest[..end]) {
                return Some(version);
            }
        }
    }
    // Fallback: `openjdk 21.0.1 2023-10-17` (java --version style).
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if !(lower.contains("version") || lower.starts_with("openjdk")) {
            continue;
        }
        for token in line.split_whitespace() {
            let candidate = token.trim_matches(|character: char| {
                character == '"' || character == ',' || character == '-'
            });
            if candidate
                .chars()
                .next()
                .map(|character| character.is_ascii_digit())
                .unwrap_or(false)
            {
                if let Some(version) = parse_version_token(candidate) {
                    return Some(version);
                }
            }
        }
    }
    None
}

pub fn parse_vendor(text: &str) -> JavaVendor {
    let lower = text.to_ascii_lowercase();
    let table: [(JavaVendor, &[&str]); 9] = [
        (JavaVendor::Temurin, &["temurin", "adoptium"]),
        (JavaVendor::Corretto, &["corretto"]),
        (JavaVendor::Zulu, &["zulu", "azul"]),
        (JavaVendor::GraalVm, &["graalvm"]),
        (JavaVendor::Liberica, &["liberica", "bellsoft"]),
        (JavaVendor::SapMachine, &["sapmachine", "sap se"]),
        (JavaVendor::Microsoft, &["microsoft"]),
        (JavaVendor::Oracle, &["oracle"]),
        (JavaVendor::OpenJdk, &["openjdk"]),
    ];
    for (vendor, needles) in table {
        if needles.iter().any(|needle| lower.contains(needle)) {
            return vendor;
        }
    }
    if lower.contains("java(tm)") {
        return JavaVendor::Oracle;
    }
    JavaVendor::Unknown
}

/// What a banner tells us about the JVM's CPU architecture.
///
/// `Bits64` is deliberately *not* an architecture: the same
/// `OpenJDK 64-Bit Server VM ...` line is printed by x86_64 **and** aarch64
/// builds, so inferring x64 from it rejects a correct arm64 runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchHint {
    /// The banner names the architecture explicitly.
    Arch(Arch),
    /// The banner only says "64-bit": the architecture is not determined.
    Bits64,
    /// The banner only says "32-bit".
    Bits32,
    /// No architecture evidence at all.
    None,
}

/// Read the architecture hint from a `java -version` banner.
pub fn parse_arch_hint(text: &str) -> ArchHint {
    let lower = text.to_ascii_lowercase();
    // Explicit architecture names only.
    if lower.contains("aarch64") || lower.contains("arm64") {
        return ArchHint::Arch(Arch::Arm64);
    }
    for needle in ["x86_64", "x86-64", "amd64", "win32-x86-64"] {
        if lower.contains(needle) {
            return ArchHint::Arch(Arch::X64);
        }
    }
    if lower.contains("i386") || lower.contains("i686") || lower.contains("x86-32") {
        return ArchHint::Arch(Arch::X86);
    }
    if lower.contains("64-bit") || lower.contains("64 bit") {
        return ArchHint::Bits64;
    }
    if lower.contains("32-bit") || lower.contains("32 bit") || lower.contains("client vm") {
        return ArchHint::Bits32;
    }
    ArchHint::None
}

/// Resolve a hint into an architecture using the host as evidence.
///
/// The reasoning is sound only because the probe *ran on this host*: a 64-bit JVM
/// that answered here has this host's 64-bit architecture. When the host is
/// unknown the answer stays `Arch::Unknown`, which makes the runtime
/// "unverified" instead of rejected.
pub fn resolve_arch(hint: ArchHint, host: Arch) -> Arch {
    match hint {
        ArchHint::Arch(arch) => arch,
        ArchHint::Bits32 => Arch::X86,
        ArchHint::Bits64 => match host {
            Arch::X64 => Arch::X64,
            Arch::Arm64 => Arch::Arm64,
            _ => Arch::Unknown,
        },
        ArchHint::None => Arch::Unknown,
    }
}

/// Architecture from a banner alone, without host context.
///
/// Ambiguous "64-Bit" lines stay `Arch::Unknown` on purpose; use
/// [`resolve_arch`] (or [`parse_arch_with_host`]) when the host is known.
pub fn parse_arch(text: &str) -> Arch {
    match parse_arch_hint(text) {
        ArchHint::Arch(arch) => arch,
        _ => Arch::Unknown,
    }
}

/// Architecture from a banner plus the host the probe ran on.
pub fn parse_arch_with_host(text: &str, host: Arch) -> Arch {
    resolve_arch(parse_arch_hint(text), host)
}

/// A structured Java version requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaRequirement {
    Any,
    Exact(u32),
    AtLeast(u32),
    Range { min: u32, max: u32 },
}

impl JavaRequirement {
    pub const fn matches(self, major: u32) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(required) => major == required,
            Self::AtLeast(min) => major >= min,
            Self::Range { min, max } => major >= min && major <= max,
        }
    }

    /// Parse a constraint spec: `21`, `>=17`, `17-21`, `17..21`, `any`.
    pub fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        if spec.is_empty() || spec.eq_ignore_ascii_case("any") {
            return Some(Self::Any);
        }
        if let Some(rest) = spec.strip_prefix(">=") {
            return rest.trim().parse::<u32>().ok().map(Self::AtLeast);
        }
        for separator in ["..", "-"] {
            if let Some((min, max)) = spec.split_once(separator) {
                let min = min.trim().parse::<u32>().ok()?;
                let max = max.trim().parse::<u32>().ok()?;
                if min > max {
                    return None;
                }
                return Some(Self::Range { min, max });
            }
        }
        spec.parse::<u32>().ok().map(Self::Exact)
    }

    /// Ranking used when several runtimes satisfy the requirement.
    pub const fn rank(self, major: u32) -> u8 {
        match self {
            Self::Exact(required) if major == required => 0,
            Self::Range { min, .. } if major == min => 1,
            Self::AtLeast(_) => 2,
            Self::Any => 3,
            _ => 4,
        }
    }

    pub fn describe(self) -> String {
        match self {
            Self::Any => "any".to_string(),
            Self::Exact(major) => major.to_string(),
            Self::AtLeast(min) => format!(">={min}"),
            Self::Range { min, max } => format!("{min}-{max}"),
        }
    }

    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "kind": match self {
                Self::Any => "any",
                Self::Exact(_) => "exact",
                Self::AtLeast(_) => "at_least",
                Self::Range { .. } => "range",
            },
            "spec": self.describe(),
            "min": match self {
                Self::Any => None,
                Self::Exact(value) | Self::AtLeast(value) | Self::Range { min: value, .. } => Some(value),
            },
            "max": match self {
                Self::Range { max, .. } => Some(max),
                _ => None,
            },
        })
    }
}

/// Parse a Minecraft version into `(major, minor, patch)`.
///
/// Returns `None` for snapshots/pre-releases (`24w14a`, `1.20.5-pre1`) and for
/// anything unexpected: an unknown version must not silently produce a guess.
pub fn parse_game_version(version: &str) -> Option<(u32, u32, u32)> {
    let trimmed = version.trim();
    let mut numbers = Vec::new();
    for part in trimmed.split('.') {
        if part.is_empty() || !part.chars().all(|character| character.is_ascii_digit()) {
            break;
        }
        numbers.push(part.parse::<u32>().ok()?);
    }
    match numbers.len() {
        0 | 1 => None,
        2 => Some((numbers[0], numbers[1], 0)),
        _ => Some((numbers[0], numbers[1], numbers[2])),
    }
}

/// The Java requirement implied by a game version.
///
/// This is a documented fallback used until real version metadata is wired in
/// (external dependency, see the module design §11). The second tuple element is
/// `true` only when the requirement came from a version this function
/// understands; otherwise the selection is flagged as unverified.
pub fn requirement_for_game_version(version: &str) -> (JavaRequirement, bool) {
    let Some((major, minor, patch)) = parse_game_version(version) else {
        return (JavaRequirement::Any, false);
    };
    if major != 1 {
        return (JavaRequirement::Any, false);
    }
    if (minor, patch) >= (20, 5) {
        return (JavaRequirement::AtLeast(21), true);
    }
    if minor >= 18 {
        return (JavaRequirement::AtLeast(17), true);
    }
    if minor == 17 {
        return (JavaRequirement::AtLeast(16), true);
    }
    (JavaRequirement::Exact(8), true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintSource {
    /// The instance snapshot pinned a runtime path or version.
    Instance,
    /// Derived from game version metadata (currently the fallback heuristic).
    GameMetadata,
    /// Derived from the loader (for example a loader that requires Java 17+).
    Loader,
    /// Nothing was declared; any verified runtime may be used.
    Fallback,
}

impl ConstraintSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::GameMetadata => "game_metadata",
            Self::Loader => "loader",
            Self::Fallback => "fallback",
        }
    }
}

/// What the caller requires from a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConstraint {
    pub requirement: JavaRequirement,
    pub platform: Option<HostPlatform>,
    pub arch: Option<Arch>,
    pub source: ConstraintSource,
    /// A path the user (or the instance) pinned explicitly.
    pub explicit_path: Option<PathBuf>,
}

impl RuntimeConstraint {
    pub fn new(requirement: JavaRequirement, source: ConstraintSource) -> Self {
        Self {
            requirement,
            platform: Some(HostPlatform::current()),
            arch: Some(Arch::current()),
            source,
            explicit_path: None,
        }
    }

    pub fn host(requirement: JavaRequirement) -> Self {
        Self::new(requirement, ConstraintSource::GameMetadata)
    }

    pub fn with_explicit_path(mut self, path: PathBuf) -> Self {
        self.explicit_path = Some(path);
        self.source = ConstraintSource::Instance;
        self
    }

    pub fn without_arch(mut self) -> Self {
        self.arch = None;
        self
    }
}

/// Structured mismatch reason. `code()` is what reaches the UI; the raw stderr
/// never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchReason {
    Platform {
        required: HostPlatform,
        actual: HostPlatform,
    },
    Arch {
        required: Arch,
        actual: Arch,
    },
    Version {
        required: JavaRequirement,
        actual: u32,
    },
    /// An explicitly pinned runtime path does not exist any more.
    Missing,
}

impl MismatchReason {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Platform { .. } => errors::codes::JAVA_PLATFORM_MISMATCH,
            Self::Arch { .. } => errors::codes::JAVA_ARCH_MISMATCH,
            Self::Version { .. } => errors::codes::JAVA_VERSION_MISMATCH,
            Self::Missing => errors::codes::JAVA_NOT_FOUND,
        }
    }

    pub fn describe(self) -> String {
        match self {
            Self::Platform { required, actual } => {
                format!("platform:{}!={}", required.as_str(), actual.as_str())
            }
            Self::Arch { required, actual } => {
                format!("arch:{}!={}", required.as_str(), actual.as_str())
            }
            Self::Version { required, actual } => {
                format!("version:{}!=java{actual}", required.describe())
            }
            Self::Missing => "path:missing".to_string(),
        }
    }

    /// Stable user action that can clear this reason.
    pub const fn user_action(self) -> &'static str {
        match self {
            Self::Platform { .. } | Self::Arch { .. } => errors::ACTION_CHOOSE_JAVA,
            Self::Version { .. } | Self::Missing => errors::ACTION_INSTALL_JAVA,
        }
    }
}

/// One verified runtime.
#[derive(Debug, Clone)]
pub struct JavaRuntime {
    pub id: String,
    pub path: PathBuf,
    pub platform: HostPlatform,
    pub arch: Arch,
    pub vendor: JavaVendor,
    pub version: JavaVersion,
    pub verified_at_unix_ms: u64,
    pub file_identity: FileIdentity,
}

impl JavaRuntime {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "path": self.path.display().to_string(),
            "platform": self.platform.as_str(),
            "arch": self.arch.as_str(),
            "vendor": self.vendor.as_str(),
            "version": self.version.to_json(),
            "verifiedAt": self.verified_at_unix_ms,
            "fileIdentity": {
                "len": self.file_identity.len,
                "modified": self.file_identity.modified_unix_ms,
            },
        })
    }
}

#[derive(Debug, Clone)]
pub struct RejectedRuntime {
    pub runtime_id: String,
    pub path: PathBuf,
    pub reason: MismatchReason,
}

impl RejectedRuntime {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "runtimeId": self.runtime_id,
            "path": self.path.display().to_string(),
            "code": self.reason.code(),
            "detail": self.reason.describe(),
            "userAction": self.reason.user_action(),
        })
    }
}

/// Result of filtering candidates against a constraint.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub selected: Option<JavaRuntime>,
    pub rejected: Vec<RejectedRuntime>,
    /// False when the requirement itself was unknown (unparsed game version).
    pub requirement_verified: bool,
    /// True when the chosen runtime's architecture could not be read.
    pub arch_unverified: bool,
}

impl Selection {
    /// Stable failure envelope when nothing compatible was found.
    pub fn failure_envelope(&self, constraint: &RuntimeConstraint) -> shtmcl_domain::ErrorEnvelope {
        let mut details: Vec<String> = self
            .rejected
            .iter()
            .map(|rejected| rejected.reason.describe())
            .collect();
        details.truncate(8);
        let detail = if details.is_empty() {
            format!(
                "java-requirement:{}:none",
                constraint.requirement.describe()
            )
        } else {
            format!(
                "java-requirement:{}:{}",
                constraint.requirement.describe(),
                details.join(",")
            )
        };

        if self
            .rejected
            .iter()
            .any(|rejected| matches!(rejected.reason, MismatchReason::Version { .. }))
        {
            return errors::with_detail(
                errors::codes::JAVA_VERSION_MISMATCH,
                shtmcl_domain::ErrorCategory::Unsupported,
                false,
                detail,
            )
            .with_user_action(errors::ACTION_INSTALL_JAVA);
        }
        if self
            .rejected
            .iter()
            .any(|rejected| matches!(rejected.reason, MismatchReason::Arch { .. }))
        {
            return errors::with_detail(
                errors::codes::JAVA_ARCH_MISMATCH,
                shtmcl_domain::ErrorCategory::Unsupported,
                false,
                detail,
            )
            .with_user_action(errors::ACTION_CHOOSE_JAVA);
        }
        if self
            .rejected
            .iter()
            .any(|rejected| matches!(rejected.reason, MismatchReason::Platform { .. }))
        {
            return errors::with_detail(
                errors::codes::JAVA_PLATFORM_MISMATCH,
                shtmcl_domain::ErrorCategory::Unsupported,
                false,
                detail,
            )
            .with_user_action(errors::ACTION_CHOOSE_JAVA);
        }
        errors::java_not_found(&constraint.requirement.describe())
    }
}

/// Filter and order runtimes for a constraint.
///
/// Ordering: requirement rank (exact pin first), then the lowest major that
/// still satisfies the requirement (`Any` instead prefers the newest runtime),
/// then path. An explicitly pinned path that turns out to be incompatible is
/// *never* replaced silently: the caller gets no selection plus the reason.
pub fn select(
    runtimes: &[JavaRuntime],
    constraint: &RuntimeConstraint,
    requirement_verified: bool,
) -> Selection {
    let mut selection = Selection {
        requirement_verified,
        ..Selection::default()
    };

    if let Some(explicit) = constraint.explicit_path.as_ref() {
        let pinned: Vec<&JavaRuntime> = runtimes
            .iter()
            .filter(|runtime| same_path(&runtime.path, explicit))
            .collect();
        if pinned.is_empty() {
            selection.rejected.push(RejectedRuntime {
                runtime_id: String::new(),
                path: explicit.clone(),
                reason: MismatchReason::Missing,
            });
            return selection;
        }
        for runtime in pinned {
            if let Some(reason) = mismatch(runtime, constraint) {
                selection.rejected.push(RejectedRuntime {
                    runtime_id: runtime.id.clone(),
                    path: runtime.path.clone(),
                    reason,
                });
                continue;
            }
            selection.arch_unverified = runtime.arch == Arch::Unknown
                && constraint
                    .arch
                    .is_some_and(|required| required != Arch::Unknown);
            selection.selected = Some(runtime.clone());
            return selection;
        }
        return selection;
    }

    let mut compatible: Vec<&JavaRuntime> = Vec::new();
    for runtime in runtimes {
        match mismatch(runtime, constraint) {
            Some(reason) => selection.rejected.push(RejectedRuntime {
                runtime_id: runtime.id.clone(),
                path: runtime.path.clone(),
                reason,
            }),
            None => compatible.push(runtime),
        }
    }

    compatible.sort_by(|left, right| {
        constraint
            .requirement
            .rank(left.version.major)
            .cmp(&constraint.requirement.rank(right.version.major))
            .then_with(|| match constraint.requirement {
                // No requirement: prefer the newest runtime.
                JavaRequirement::Any => right.version.major.cmp(&left.version.major),
                // A requirement: prefer the *lowest satisfying* major. A newer
                // major that also satisfies `>=` is not automatically better and
                // breaks older game versions, so the closest match wins.
                _ => left.version.major.cmp(&right.version.major),
            })
            .then_with(|| left.path.cmp(&right.path))
    });

    if let Some(best) = compatible.first() {
        selection.arch_unverified = best.arch == Arch::Unknown
            && constraint
                .arch
                .is_some_and(|required| required != Arch::Unknown);
        selection.selected = Some((*best).clone());
    }
    selection
}

fn mismatch(runtime: &JavaRuntime, constraint: &RuntimeConstraint) -> Option<MismatchReason> {
    if let Some(required) = constraint.platform {
        if runtime.platform != required {
            return Some(MismatchReason::Platform {
                required,
                actual: runtime.platform,
            });
        }
    }
    if let Some(required) = constraint.arch {
        if required != Arch::Unknown && runtime.arch != Arch::Unknown && runtime.arch != required {
            return Some(MismatchReason::Arch {
                required,
                actual: runtime.arch,
            });
        }
    }
    if !constraint.requirement.matches(runtime.version.major) {
        return Some(MismatchReason::Version {
            required: constraint.requirement,
            actual: runtime.version.major,
        });
    }
    None
}

fn same_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

/// Path equality that respects the platform's case rules (Windows is
/// case-insensitive). Public so a module can merge discovery results without
/// duplicating a runtime it already knows.
pub fn same_file_path(left: &Path, right: &Path) -> bool {
    same_path(left, right)
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Process ran and its output was understood.
    Ok,
    /// The probe did not finish inside the timeout.
    TimedOut,
    /// The process ran but exited with a non-zero status.
    NonZeroExit(i32),
    /// The process ran but the version could not be parsed.
    Unparsable,
    /// The process could not be created.
    SpawnFailed,
    /// The supervisor could not report on the process.
    UnknownExit,
    /// The path does not exist (candidate disappeared during the scan).
    Missing,
}

impl ProbeOutcome {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::TimedOut => errors::codes::JAVA_PROBE_TIMEOUT,
            Self::NonZeroExit(_) | Self::Unparsable => errors::codes::JAVA_PROBE_FAILED,
            Self::SpawnFailed | Self::UnknownExit => errors::codes::JAVA_PROBE_FAILED,
            Self::Missing => errors::codes::JAVA_NOT_FOUND,
        }
    }

    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Ok)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaProbe {
    pub outcome: ProbeOutcome,
    pub version: Option<JavaVersion>,
    pub vendor: JavaVendor,
    pub arch: Arch,
    /// Kept for `detail_ref` only; never rendered as UI text.
    pub detail_ref: String,
}

impl JavaProbe {
    pub fn failed(outcome: ProbeOutcome, detail_ref: impl Into<String>) -> Self {
        Self {
            outcome,
            version: None,
            vendor: JavaVendor::Unknown,
            arch: Arch::Unknown,
            detail_ref: detail_ref.into(),
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn read_bounded(mut reader: impl Read + Send + 'static, cap: usize) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = reader.by_ref().take(cap as u64).read_to_end(&mut buffer);
    buffer
}

/// Runs `java -version` through the platform launcher with a hard timeout.
pub struct JavaProber {
    launcher: Arc<dyn ProcessLauncher>,
    supervisor: Arc<dyn ProcessSupervisor>,
    timeout: Duration,
}

impl JavaProber {
    pub fn new(launcher: Arc<dyn ProcessLauncher>, supervisor: Arc<dyn ProcessSupervisor>) -> Self {
        Self {
            launcher,
            supervisor,
            timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn probe(&self, program: &Path) -> JavaProbe {
        if fs::probe_file(program).is_err() {
            return JavaProbe::failed(
                ProbeOutcome::Missing,
                format!("java-probe:missing:{}", program.display()),
            );
        }
        let working_dir = program
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let request = crate::launch::ports::process_request(
            program.to_path_buf(),
            working_dir,
            vec!["-version".to_string()],
        );

        let handle = match self.launcher.spawn(&request) {
            Ok(handle) => handle,
            Err(_) => {
                return JavaProbe::failed(
                    ProbeOutcome::SpawnFailed,
                    format!("java-probe:spawn:{}", program.display()),
                )
            }
        };
        let pid = handle.pid;

        // Reader threads keep the pipes drained; a hostile runtime that floods
        // stderr cannot stall the probe because the read is capped.
        let (sender, receiver) = mpsc::channel::<(Vec<u8>, Vec<u8>)>();
        match self.supervisor.take_streams(pid) {
            Some(streams) => {
                let stdout = std::thread::Builder::new()
                    .name(format!("java-probe-out-{pid}"))
                    .spawn(move || read_bounded(streams.stdout, PROBE_OUTPUT_CAP));
                let stderr = std::thread::Builder::new()
                    .name(format!("java-probe-err-{pid}"))
                    .spawn(move || read_bounded(streams.stderr, PROBE_OUTPUT_CAP));
                std::thread::Builder::new()
                    .name(format!("java-probe-join-{pid}"))
                    .spawn(move || {
                        let out = stdout
                            .ok()
                            .and_then(|handle| handle.join().ok())
                            .unwrap_or_default();
                        let err = stderr
                            .ok()
                            .and_then(|handle| handle.join().ok())
                            .unwrap_or_default();
                        let _ = sender.send((out, err));
                    })
                    .ok();
            }
            None => drop(sender),
        }

        match self.supervisor.wait_for_exit(pid, Some(self.timeout)) {
            WaitOutcome::Exited(code) => {
                let output = receiver.recv_timeout(Duration::from_millis(500)).ok();
                let (stdout, stderr) = output.unwrap_or_default();
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&stderr),
                    String::from_utf8_lossy(&stdout)
                );
                let version = parse_version_output(&text);
                let probe = JavaProbe {
                    outcome: match (&version, code) {
                        (Some(_), 0) => ProbeOutcome::Ok,
                        (Some(_), other) => ProbeOutcome::NonZeroExit(other),
                        (None, 0) => ProbeOutcome::Unparsable,
                        (None, other) => ProbeOutcome::NonZeroExit(other),
                    },
                    version,
                    vendor: parse_vendor(&text),
                    arch: parse_arch_with_host(&text, Arch::current()),
                    detail_ref: format!("java-probe:{}:exit:{code}", program.display()),
                };
                probe
            }
            WaitOutcome::TimedOut => {
                let _ = self.supervisor.request_stop(pid);
                // Best effort: partial output is only used for the detail ref.
                let _ = receiver.recv_timeout(Duration::from_millis(200));
                JavaProbe::failed(
                    ProbeOutcome::TimedOut,
                    format!(
                        "java-probe:timeout:{}:{}ms",
                        program.display(),
                        self.timeout.as_millis()
                    ),
                )
            }
            WaitOutcome::Unknown => JavaProbe::failed(
                ProbeOutcome::UnknownExit,
                format!("java-probe:unknown-exit:{}", program.display()),
            ),
        }
    }

    /// Probe candidates with bounded concurrency, preserving input order.
    pub fn probe_all(&self, programs: &[PathBuf], concurrency: usize) -> Vec<JavaProbe> {
        if programs.is_empty() {
            return Vec::new();
        }
        let concurrency = concurrency.clamp(1, programs.len());
        let results: Arc<Mutex<Vec<Option<JavaProbe>>>> =
            Arc::new(Mutex::new(vec![None; programs.len()]));
        let next = Arc::new(AtomicUsize::new(0));
        let programs = Arc::new(programs.to_vec());

        std::thread::scope(|scope| {
            for _ in 0..concurrency {
                let results = results.clone();
                let next = next.clone();
                let programs = programs.clone();
                let this = &*self;
                scope.spawn(move || loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    if index >= programs.len() {
                        break;
                    }
                    let probe = this.probe(&programs[index]);
                    if let Ok(mut slots) = results.lock() {
                        slots[index] = Some(probe);
                    }
                });
            }
        });

        let slots = results
            .lock()
            .map(|slots| slots.clone())
            .unwrap_or_default();
        slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    JavaProbe::failed(ProbeOutcome::SpawnFailed, "java-probe:worker-failed")
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheEntry {
    len: u64,
    modified: Option<u128>,
    platform: &'static str,
    verifier: u32,
    probe: JavaProbe,
}

/// Probe cache keyed by normalized path plus file identity.
///
/// A changed file identity, a different platform or a different verifier version
/// invalidates the entry, so a replaced runtime is re-probed instead of trusted.
#[derive(Debug, Default)]
pub struct JavaCache {
    entries: Mutex<BTreeMap<String, CacheEntry>>,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub hits: usize,
    pub misses: usize,
    pub entries: usize,
}

fn cache_key(path: &Path) -> String {
    if cfg!(windows) {
        path.to_string_lossy().to_ascii_lowercase()
    } else {
        path.to_string_lossy().to_string()
    }
}

impl JavaCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path, identity: Option<&FileIdentity>) -> Option<JavaProbe> {
        let Some(identity) = identity else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let key = cache_key(path);
        let hit = self.entries.lock().ok().and_then(|entries| {
            entries.get(&key).and_then(|entry| {
                (entry.len == identity.len
                    && entry.modified == identity.modified_unix_ms
                    && entry.platform == HostPlatform::current().as_str()
                    && entry.verifier == VERIFIER_VERSION)
                    .then(|| entry.probe.clone())
            })
        });
        if hit.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    pub fn put(&self, path: &Path, identity: Option<&FileIdentity>, probe: &JavaProbe) {
        let Some(identity) = identity else {
            return;
        };
        let entry = CacheEntry {
            len: identity.len,
            modified: identity.modified_unix_ms,
            platform: HostPlatform::current().as_str(),
            verifier: VERIFIER_VERSION,
            probe: probe.clone(),
        };
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(cache_key(path), entry);
        }
    }

    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(&cache_key(path));
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: self
                .entries
                .lock()
                .map(|entries| entries.len())
                .unwrap_or(0),
        }
    }
}

/// True when a cached runtime's file identity no longer matches the disk.
pub fn is_stale(runtime: &JavaRuntime) -> bool {
    match fs::probe_file(&runtime.path) {
        Ok(identity) => {
            identity.len != runtime.file_identity.len
                || identity.modified_unix_ms != runtime.file_identity.modified_unix_ms
        }
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Candidate discovery
// ---------------------------------------------------------------------------

/// Bounds for one discovery pass.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryLimits {
    pub max_depth: usize,
    pub max_entries_per_dir: usize,
    pub concurrency: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_entries_per_dir: DEFAULT_MAX_ENTRIES_PER_DIR,
            concurrency: DEFAULT_PROBE_CONCURRENCY,
        }
    }
}

pub fn java_executable_name() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// Conventional locations only; a full-disk scan is never performed.
pub fn candidate_roots(dirs: &PlatformDirs, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !roots.iter().any(|existing| same_path(existing, &path)) {
            roots.push(path);
        }
    };

    for variable in ["JAVA_HOME", "JDK_HOME"] {
        if let Some(value) = std::env::var_os(variable) {
            if !value.is_empty() {
                push(PathBuf::from(value));
            }
        }
    }
    push(dirs.data.join("java"));
    push(dirs.cache.join("java"));
    push(dirs.root.join("java"));

    if cfg!(windows) {
        for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(variable) {
                let base = PathBuf::from(base);
                for vendor in [
                    "Java",
                    "Eclipse Adoptium",
                    "Microsoft",
                    "BellSoft",
                    "Zulu",
                    "Amazon Corretto",
                ] {
                    push(base.join(vendor));
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        push(PathBuf::from("/Library/Java/JavaVirtualMachines"));
        push(PathBuf::from("/System/Library/Java/JavaVirtualMachines"));
    } else {
        push(PathBuf::from("/usr/lib/jvm"));
        push(PathBuf::from("/usr/java"));
    }

    for path in extra {
        push(path.clone());
    }
    roots
}

/// Enumerate `<root>/**/bin/java[.exe]` with hard depth and entry caps.
pub fn candidate_paths(roots: &[PathBuf], limits: DiscoveryLimits) -> Vec<PathBuf> {
    let executable = java_executable_name();
    let mut found: Vec<PathBuf> = Vec::new();
    let mut queue: Vec<(PathBuf, usize)> =
        roots.iter().map(|root| (root.clone(), 0usize)).collect();
    let mut visited: Vec<PathBuf> = Vec::new();
    let mut budget = limits
        .max_entries_per_dir
        .saturating_mul(limits.max_depth.max(1))
        .saturating_mul(roots.len().max(1))
        .max(limits.max_entries_per_dir);

    while let Some((directory, depth)) = queue.pop() {
        if visited.iter().any(|seen| same_path(seen, &directory)) {
            continue;
        }
        visited.push(directory.clone());

        let candidate = directory.join("bin").join(executable);
        if fs::probe_file(&candidate).is_ok()
            && !found.iter().any(|existing| same_path(existing, &candidate))
        {
            found.push(candidate);
        }

        if depth >= limits.max_depth || budget == 0 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .take(limits.max_entries_per_dir)
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
            .map(|entry| entry.path())
            .collect();
        children.sort();
        budget = budget.saturating_sub(children.len());
        for child in children {
            queue.push((child, depth + 1));
        }
    }

    found.sort();
    found
}

#[derive(Debug, Clone)]
pub struct ProbeRecord {
    pub path: PathBuf,
    pub code: String,
    pub detail_ref: String,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    pub runtimes: Vec<JavaRuntime>,
    pub failed: Vec<ProbeRecord>,
    pub cache: CacheStats,
    pub candidates: usize,
}

impl DiscoveryReport {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "runtimes": self.runtimes.iter().map(JavaRuntime::to_json).collect::<Vec<_>>(),
            "failed": self.failed.iter().map(|record| serde_json::json!({
                "path": record.path.display().to_string(),
                "code": record.code,
                "detailRef": record.detail_ref,
            })).collect::<Vec<_>>(),
            "cache": {
                "hits": self.cache.hits,
                "misses": self.cache.misses,
                "entries": self.cache.entries,
            },
            "candidates": self.candidates,
        })
    }
}

/// Discovery = restricted path enumeration + cached probes.
pub struct JavaDiscovery {
    prober: JavaProber,
    cache: Arc<JavaCache>,
    limits: DiscoveryLimits,
}

impl JavaDiscovery {
    pub fn new(prober: JavaProber, limits: DiscoveryLimits) -> Self {
        Self {
            prober,
            cache: Arc::new(JavaCache::new()),
            limits,
        }
    }

    pub fn with_cache(mut self, cache: Arc<JavaCache>) -> Self {
        self.cache = cache;
        self
    }

    pub fn cache(&self) -> Arc<JavaCache> {
        self.cache.clone()
    }

    pub fn limits(&self) -> DiscoveryLimits {
        self.limits
    }

    pub fn discover(&self, roots: &[PathBuf]) -> DiscoveryReport {
        let candidates = candidate_paths(roots, self.limits);
        let mut report = DiscoveryReport {
            candidates: candidates.len(),
            ..DiscoveryReport::default()
        };

        let mut pending: Vec<PathBuf> = Vec::new();
        let mut identities: BTreeMap<PathBuf, Option<FileIdentity>> = BTreeMap::new();
        let mut probes: BTreeMap<PathBuf, JavaProbe> = BTreeMap::new();

        for candidate in &candidates {
            let identity = fs::probe_file(candidate).ok();
            if let Some(probe) = self.cache.get(candidate, identity.as_ref()) {
                probes.insert(candidate.clone(), probe);
            } else {
                identities.insert(candidate.clone(), identity);
                pending.push(candidate.clone());
            }
        }

        if !pending.is_empty() {
            let results = self.prober.probe_all(&pending, self.limits.concurrency);
            for (path, probe) in pending.iter().zip(results.into_iter()) {
                self.cache
                    .put(path, identities.get(path).and_then(Option::as_ref), &probe);
                probes.insert(path.clone(), probe);
            }
        }

        let mut index = 0usize;
        for candidate in &candidates {
            let Some(probe) = probes.get(candidate) else {
                continue;
            };
            match (&probe.outcome, &probe.version) {
                (ProbeOutcome::Ok, Some(version)) => {
                    let identity = fs::probe_file(candidate)
                        .ok()
                        .or_else(|| identities.get(candidate).and_then(|value| *value))
                        .unwrap_or(FileIdentity {
                            len: 0,
                            modified_unix_ms: None,
                        });
                    index += 1;
                    report.runtimes.push(JavaRuntime {
                        id: format!("java-{index}"),
                        path: candidate.clone(),
                        platform: HostPlatform::current(),
                        arch: probe.arch,
                        vendor: probe.vendor,
                        version: version.clone(),
                        verified_at_unix_ms: now_unix_ms(),
                        file_identity: identity,
                    });
                }
                _ => report.failed.push(ProbeRecord {
                    path: candidate.clone(),
                    code: probe.outcome.code().to_string(),
                    detail_ref: probe.detail_ref.clone(),
                }),
            }
        }

        report.cache = self.cache.stats();
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(index: usize, path: &str, major: u32, arch: Arch) -> JavaRuntime {
        JavaRuntime {
            id: format!("java-{index}"),
            path: PathBuf::from(path),
            platform: HostPlatform::current(),
            arch,
            vendor: JavaVendor::Temurin,
            version: JavaVersion {
                major,
                minor: 0,
                security: 1,
                update: None,
                raw: format!("{major}.0.1"),
            },
            verified_at_unix_ms: 0,
            file_identity: FileIdentity {
                len: 0,
                modified_unix_ms: None,
            },
        }
    }

    #[test]
    fn parses_modern_and_legacy_banners() {
        let modern = parse_version_output("openjdk version \"21.0.2\" 2024-01-16 LTS").expect("21");
        assert_eq!(modern.major, 21);
        assert_eq!(modern.security, 2);

        let legacy =
            parse_version_output("java version \"1.8.0_402\"\nJava(TM) SE Runtime Environment")
                .expect("8");
        assert_eq!(legacy.major, 8);
        assert_eq!(legacy.update, Some(402));

        let goofy = parse_version_output("openjdk 17.0.9 2023-10-17").expect("17");
        assert_eq!(goofy.major, 17);

        assert!(parse_version_output("no version here").is_none());
    }

    #[test]
    fn parses_vendor_and_arch() {
        assert_eq!(
            parse_vendor("OpenJDK 64-Bit Server VM Temurin-21.0.2+13"),
            JavaVendor::Temurin
        );
        assert_eq!(
            parse_vendor("Java(TM) SE Runtime Environment"),
            JavaVendor::Oracle
        );
        assert_eq!(
            parse_vendor("OpenJDK Runtime Environment"),
            JavaVendor::OpenJdk
        );

        // "64-Bit" alone is not an architecture: the same line is printed by
        // x86_64 and aarch64 builds (D02-1).
        assert_eq!(
            parse_arch_hint("OpenJDK 64-Bit Server VM (build 21, mixed mode)"),
            ArchHint::Bits64
        );
        assert_eq!(
            parse_arch("OpenJDK 64-Bit Server VM (build 21, mixed mode)"),
            Arch::Unknown,
            "an ambiguous banner must not be guessed as x64"
        );
        // With the host as evidence the same banner resolves correctly on both
        // architectures, because the JVM answered on that host.
        assert_eq!(
            parse_arch_with_host("OpenJDK 64-Bit Server VM (build 21, mixed mode)", Arch::X64),
            Arch::X64
        );
        assert_eq!(
            parse_arch_with_host(
                "OpenJDK 64-Bit Server VM (build 21, mixed mode)",
                Arch::Arm64
            ),
            Arch::Arm64,
            "an arm64 host must not read a 64-bit banner as x64"
        );
        assert_eq!(
            parse_arch_with_host(
                "OpenJDK 64-Bit Server VM (build 21, mixed mode)",
                Arch::Unknown
            ),
            Arch::Unknown
        );

        // Explicit evidence wins over the host.
        assert_eq!(
            parse_arch("OpenJDK 64-Bit Server VM warning: aarch64"),
            Arch::Arm64
        );
        assert_eq!(
            parse_arch_with_host("aarch64 OpenJDK 64-Bit Server VM", Arch::X64),
            Arch::Arm64
        );
        assert_eq!(
            parse_arch_with_host("OpenJDK 64-Bit Server VM, amd64 build", Arch::Arm64),
            Arch::X64
        );
        assert_eq!(
            parse_arch_with_host("Client VM (build 1.8, mixed mode)", Arch::X64),
            Arch::X86
        );
        assert_eq!(parse_arch_hint("no arch evidence here"), ArchHint::None);
        assert_eq!(parse_arch("no arch evidence here"), Arch::Unknown);
    }

    #[test]
    fn requirement_matching_is_structural() {
        assert!(JavaRequirement::Exact(8).matches(8));
        assert!(!JavaRequirement::Exact(8).matches(18));
        assert!(JavaRequirement::AtLeast(17).matches(21));
        assert!(!JavaRequirement::AtLeast(21).matches(17));
        assert!(JavaRequirement::Range { min: 17, max: 21 }.matches(20));
        assert!(JavaRequirement::Any.matches(1));
        assert_eq!(
            JavaRequirement::parse(">=17"),
            Some(JavaRequirement::AtLeast(17))
        );
        assert_eq!(
            JavaRequirement::parse("21"),
            Some(JavaRequirement::Exact(21))
        );
        assert_eq!(
            JavaRequirement::parse("17-21"),
            Some(JavaRequirement::Range { min: 17, max: 21 })
        );
        assert_eq!(JavaRequirement::parse("any"), Some(JavaRequirement::Any));
        assert_eq!(JavaRequirement::parse("nope"), None);
    }

    #[test]
    fn game_version_requirements_are_explicit_about_unknown_versions() {
        assert_eq!(
            requirement_for_game_version("1.20.6"),
            (JavaRequirement::AtLeast(21), true)
        );
        assert_eq!(
            requirement_for_game_version("1.20.4"),
            (JavaRequirement::AtLeast(17), true)
        );
        assert_eq!(
            requirement_for_game_version("1.17.1"),
            (JavaRequirement::AtLeast(16), true)
        );
        assert_eq!(
            requirement_for_game_version("1.12.2"),
            (JavaRequirement::Exact(8), true)
        );
        // Snapshots are honestly unknown instead of guessed.
        assert_eq!(
            requirement_for_game_version("24w14a"),
            (JavaRequirement::Any, false)
        );
        assert!(parse_game_version("1.20").is_some());
        assert!(parse_game_version("24w14a").is_none());
    }

    #[test]
    fn selection_reports_structured_mismatch_reasons() {
        let runtimes = vec![runtime(1, "C:/jdk8/bin/java.exe", 8, Arch::X64)];
        let constraint = RuntimeConstraint::host(JavaRequirement::AtLeast(21));
        let selection = select(&runtimes, &constraint, true);
        assert!(selection.selected.is_none());
        assert_eq!(
            selection.rejected[0].reason.code(),
            errors::codes::JAVA_VERSION_MISMATCH
        );
        let envelope = selection.failure_envelope(&constraint);
        assert_eq!(envelope.code.as_str(), errors::codes::JAVA_VERSION_MISMATCH);
        assert!(!envelope.retryable);

        let wrong_arch = vec![runtime(1, "C:/jdk21-arm64/bin/java.exe", 21, Arch::Arm64)];
        let constraint = RuntimeConstraint::host(JavaRequirement::AtLeast(17));
        let selection = select(&wrong_arch, &constraint, true);
        assert_eq!(
            selection.rejected[0].reason.code(),
            errors::codes::JAVA_ARCH_MISMATCH
        );
    }

    #[test]
    fn explicit_pin_is_reported_instead_of_replaced() {
        let runtimes = vec![
            runtime(1, "C:/pinned/jdk8/bin/java.exe", 8, Arch::X64),
            runtime(2, "C:/other/jdk21/bin/java.exe", 21, Arch::X64),
        ];
        let constraint = RuntimeConstraint::host(JavaRequirement::AtLeast(17))
            .with_explicit_path(PathBuf::from("C:/pinned/jdk8/bin/java.exe"));
        let selection = select(&runtimes, &constraint, true);
        assert!(selection.selected.is_none(), "no silent replacement");
        assert_eq!(
            selection.rejected[0].reason.code(),
            errors::codes::JAVA_VERSION_MISMATCH
        );
    }

    #[test]
    fn selection_prefers_the_exact_requirement() {
        let runtimes = vec![
            runtime(1, "C:/jdk17/bin/java.exe", 17, Arch::X64),
            runtime(2, "C:/jdk21/bin/java.exe", 21, Arch::X64),
            runtime(3, "C:/jdk8/bin/java.exe", 8, Arch::X64),
        ];
        let constraint = RuntimeConstraint::host(JavaRequirement::AtLeast(17));
        let selection = select(&runtimes, &constraint, true);
        assert_eq!(
            selection.selected.map(|runtime| runtime.version.major),
            Some(17),
            "the lowest satisfying major is the closest match"
        );
        assert_eq!(selection.rejected.len(), 1);
    }
}
