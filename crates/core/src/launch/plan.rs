//! Launch plan: the frozen input snapshot, the argument list and the files that
//! must exist before a process may be created.
//!
//! Two invariants matter more than anything else here:
//!
//! - arguments are built as a `Vec<String>`, never by joining a shell command;
//! - credentials are marked sensitive, excluded from the plan summary and from
//!   the digest, and never written to a log or an event.
//!
//! A plan is only produced when every required file exists. Missing files are a
//! refusal, not a warning: creating a process that cannot start the game would
//! misreport "process running" as progress.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use shtmcl_domain::{ErrorEnvelope, InstanceId, Revision};
use shtmcl_platform::process::ProcessRequest;

use crate::launch::errors;
use crate::launch::java::{JavaRequirement, JavaRuntime};
use crate::launch::ports::{AuthRequirement, FileProbe, GameSession};

/// Launcher module that owns the instance content graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoaderKind {
    Vanilla,
    Fabric { loader_version: Option<String> },
    Quilt { loader_version: Option<String> },
    Forge { version: Option<String> },
    NeoForge { version: Option<String> },
    Unknown(String),
}

impl LoaderKind {
    pub fn parse(family: &str, version: Option<&str>) -> Self {
        let version = version.map(str::to_string);
        match family.trim().to_ascii_lowercase().as_str() {
            "" | "vanilla" | "release" => Self::Vanilla,
            "fabric" => Self::Fabric {
                loader_version: version,
            },
            "quilt" => Self::Quilt {
                loader_version: version,
            },
            "forge" => Self::Forge { version },
            "neoforge" => Self::NeoForge { version },
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn family(&self) -> &str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Fabric { .. } => "fabric",
            Self::Quilt { .. } => "quilt",
            Self::Forge { .. } => "forge",
            Self::NeoForge { .. } => "neoforge",
            Self::Unknown(other) => other.as_str(),
        }
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Fabric { loader_version } | Self::Quilt { loader_version } => {
                loader_version.as_deref()
            }
            Self::Forge { version } | Self::NeoForge { version } => version.as_deref(),
            _ => None,
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({ "family": self.family(), "version": self.version() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactRole {
    Client,
    Library,
    Natives,
    AssetIndex,
    Asset,
    Mods,
}

impl ArtifactRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Library => "library",
            Self::Natives => "natives",
            Self::AssetIndex => "asset_index",
            Self::Asset => "asset",
            Self::Mods => "mods",
        }
    }
}

/// A file the launch cannot proceed without.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredFile {
    pub path: PathBuf,
    pub role: ArtifactRole,
}

impl RequiredFile {
    pub fn new(path: impl Into<PathBuf>, role: ArtifactRole) -> Self {
        Self {
            path: path.into(),
            role,
        }
    }
}

/// The frozen instance snapshot module 02 plans against.
///
/// Owned by 01 in the final architecture; 02 defines the consumed shape until
/// that port is delivered (see the handoff "public interface requests").
#[derive(Debug, Clone)]
pub struct InstanceLaunchSnapshot {
    pub instance_id: InstanceId,
    pub revision: Revision,
    pub content_revision: Option<Revision>,
    pub name: String,
    pub game_version: String,
    pub version_type: Option<String>,
    pub loader: LoaderKind,
    /// Working directory of the game (`--gameDir` / process cwd).
    pub game_dir: PathBuf,
    pub main_class: String,
    /// Ordered classpath entries; the order is preserved verbatim.
    pub classpath: Vec<PathBuf>,
    pub assets_dir: Option<PathBuf>,
    pub asset_index: Option<String>,
    pub natives_dir: Option<PathBuf>,
    /// JVM arguments as declared by the version/loader metadata (templates allowed).
    pub jvm_args: Vec<String>,
    /// Game arguments as declared by the version/loader metadata.
    pub game_args: Vec<String>,
    pub memory_mb: Option<u32>,
    /// Explicit requirement from the instance settings, when pinned.
    pub java_requirement: Option<JavaRequirement>,
    /// Explicit runtime path from the instance settings, when pinned.
    pub java_path: Option<PathBuf>,
    pub required_files: Vec<RequiredFile>,
    pub auth: AuthRequirement,
    /// Offline identity to use when the instance accepts an offline launch.
    pub offline_username: Option<String>,
    pub multi_open: bool,
    pub profile_ref: Option<String>,
}

impl InstanceLaunchSnapshot {
    /// Everything that must exist before spawning.
    pub fn collect_required(&self) -> Vec<RequiredFile> {
        let mut files: Vec<RequiredFile> = self.required_files.clone();
        for entry in &self.classpath {
            files.push(RequiredFile::new(entry.clone(), ArtifactRole::Library));
        }
        if let Some(natives) = self.natives_dir.as_ref() {
            files.push(RequiredFile::new(natives.clone(), ArtifactRole::Natives));
        }
        if let Some(assets) = self.assets_dir.as_ref() {
            files.push(RequiredFile::new(assets.clone(), ArtifactRole::Asset));
        }

        let mut seen = HashSet::new();
        files.retain(|file| {
            let key = if cfg!(windows) {
                file.path.to_string_lossy().to_ascii_lowercase()
            } else {
                file.path.to_string_lossy().to_string()
            };
            seen.insert(key)
        });
        files
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "instanceId": self.instance_id.as_str(),
            "revision": self.revision.get(),
            "contentRevision": self.content_revision.map(Revision::get),
            "gameVersion": self.game_version,
            "loader": self.loader.to_json(),
            "gameDir": self.game_dir.display().to_string(),
            "auth": self.auth.as_str(),
            "multiOpen": self.multi_open,
        })
    }
}

/// One ordered classpath/library artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedArtifact {
    pub path: PathBuf,
    pub role: ArtifactRole,
}

impl PlannedArtifact {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "path": self.path.display().to_string(),
            "role": self.role.as_str(),
        })
    }
}

/// One argument, with an explicit sensitivity marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanArg {
    pub value: String,
    pub sensitive: bool,
}

impl PlanArg {
    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            sensitive: false,
        }
    }

    pub fn sensitive(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            sensitive: true,
        }
    }

    pub fn redacted(&self) -> String {
        if self.sensitive {
            "<redacted>".to_string()
        } else {
            self.value.clone()
        }
    }
}

/// Why a plan could not be built.
#[derive(Debug, Clone)]
pub struct LaunchRefusal {
    pub envelope: ErrorEnvelope,
    pub missing: Vec<PathBuf>,
}

impl LaunchRefusal {
    pub fn new(envelope: ErrorEnvelope) -> Self {
        Self {
            envelope,
            missing: Vec::new(),
        }
    }

    pub fn code(&self) -> &str {
        self.envelope.code.as_str()
    }
}

/// The plan a session is started from. Frozen: changing settings later only
/// affects the next plan.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub id: String,
    pub instance_id: InstanceId,
    pub instance_revision: Revision,
    pub content_revision: Option<Revision>,
    pub runtime_id: String,
    pub java_major: u32,
    pub profile_ref: Option<String>,
    pub program: PathBuf,
    pub jvm_args: Vec<PlanArg>,
    pub game_args: Vec<PlanArg>,
    pub working_dir: PathBuf,
    pub artifacts: Vec<PlannedArtifact>,
    pub required_files: Vec<RequiredFile>,
    /// FNV-1a over program + non-sensitive arguments; sensitive values are
    /// replaced by `<redacted>` so the digest never encodes a credential.
    pub args_digest: String,
    pub sensitive_args: usize,
    pub game_version: String,
    pub loader: LoaderKind,
    pub quick_play: Option<String>,
}

impl LaunchPlan {
    pub fn all_args(&self) -> Vec<String> {
        self.jvm_args
            .iter()
            .chain(self.game_args.iter())
            .map(|argument| argument.value.clone())
            .collect()
    }

    /// Complete argument list for the platform launcher (credentials included —
    /// the game needs them, nothing else may see them).
    pub fn to_process_request(&self) -> ProcessRequest {
        ProcessRequest {
            program: self.program.clone(),
            args: self.all_args(),
            working_dir: self.working_dir.clone(),
            environment: BTreeMap::new(),
        }
    }

    /// Redacted view for logs, events, task results and diagnostics.
    pub fn redacted_args(&self) -> Vec<String> {
        self.jvm_args
            .iter()
            .chain(self.game_args.iter())
            .map(PlanArg::redacted)
            .collect()
    }

    /// Plan summary safe to publish.
    pub fn summary_json(&self) -> Value {
        serde_json::json!({
            "planId": self.id,
            "instanceId": self.instance_id.as_str(),
            "instanceRevision": self.instance_revision.get(),
            "contentRevision": self.content_revision.map(Revision::get),
            "runtimeId": self.runtime_id,
            "javaMajor": self.java_major,
            "profileRef": self.profile_ref,
            "program": self.program.display().to_string(),
            "workingDir": self.working_dir.display().to_string(),
            "argsDigest": self.args_digest,
            "sensitiveArgs": self.sensitive_args,
            "gameVersion": self.game_version,
            "loader": self.loader.to_json(),
            "quickPlay": self.quick_play,
            "artifacts": self.artifacts.iter().map(PlannedArtifact::to_json).collect::<Vec<_>>(),
            "requiredFiles": self.required_files.len(),
            "args": self.redacted_args(),
        })
    }
}

/// FNV-1a 64-bit. Deterministic, dependency-free argument identity.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn args_digest(program: &Path, jvm_args: &[PlanArg], game_args: &[PlanArg]) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    buffer.extend_from_slice(program.to_string_lossy().as_bytes());
    buffer.push(0);
    for argument in jvm_args.iter().chain(game_args.iter()) {
        buffer.extend_from_slice(argument.redacted().as_bytes());
        buffer.push(0);
    }
    format!("fnv1a64:{:016x}", fnv1a64(&buffer))
}

/// Result of checking required files.
#[derive(Debug, Clone, Default)]
pub struct FileValidation {
    pub checked: usize,
    pub missing: Vec<PathBuf>,
}

impl FileValidation {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

pub fn validate_files(files: &[RequiredFile], probe: &dyn FileProbe) -> FileValidation {
    let mut validation = FileValidation::default();
    for file in files {
        validation.checked += 1;
        if !probe.exists(&file.path) {
            validation.missing.push(file.path.clone());
        }
    }
    validation
}

/// Expand `${name}` placeholders; any undefined variable is an error.
pub fn expand_template(
    template: &str,
    variables: &BTreeMap<&str, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'$' && index + 1 < bytes.len() && bytes[index + 1] == b'{' {
            let rest = &template[index + 2..];
            let Some(end) = rest.find('}') else {
                return Err("${".to_string());
            };
            let name = &rest[..end];
            match variables.get(name) {
                Some(value) => out.push_str(value),
                None => return Err(name.to_string()),
            }
            index += 2 + end + 1;
        } else {
            let character = template[index..]
                .chars()
                .next()
                .expect("index is a char boundary");
            out.push(character);
            index += character.len_utf8();
        }
    }
    Ok(out)
}

fn classpath_separator() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

/// True when the metadata already passes the classpath itself, i.e. a
/// `-cp` / `-classpath` flag immediately followed by a value built from
/// `${classpath}`.
///
/// A template that merely mentions the variable (`-Dmodpath=${classpath}`) is not
/// evidence that a classpath argument exists, so 02 still adds one.
pub fn classpath_flag_supplied_by_metadata(templates: &[String]) -> bool {
    templates.windows(2).any(|pair| {
        matches!(pair[0].as_str(), "-cp" | "-classpath") && pair[1].contains("${classpath}")
    })
}

/// Template variables whose substituted value is a credential.
///
/// Every argument family (JVM args, game args, and any future template source)
/// goes through [`template_is_sensitive`], so adding a family cannot silently
/// skip redaction.
pub const SENSITIVE_TEMPLATE_VARIABLES: [&str; 2] = ["auth_access_token", "auth_session"];

/// True when substituting `template` would place a credential into an argument.
///
/// Scans every `${name}` occurrence instead of matching a fixed prefix, because a
/// credential can appear anywhere in a JVM property (`-Dx=${auth_access_token}`).
pub fn template_is_sensitive(template: &str) -> bool {
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return false;
        };
        let name = &after[..end];
        if SENSITIVE_TEMPLATE_VARIABLES.contains(&name) {
            return true;
        }
        rest = &after[end + 1..];
    }
    false
}

/// Expand one argument template of `family` (`jvm` / `game`).
///
/// The sensitivity decision comes from the template, never from the family, so a
/// credential in a JVM property is redacted exactly like one in a game argument.
fn expand_argument(
    template: &str,
    variables: &BTreeMap<&str, String>,
    family: &str,
) -> Result<PlanArg, LaunchRefusal> {
    let expanded = expand_template(template, variables).map_err(|name| {
        LaunchRefusal::new(errors::plan_invalid(format!("plan:{family}-var:{name}")))
    })?;
    Ok(PlanArg {
        value: expanded,
        sensitive: template_is_sensitive(template),
    })
}

/// Everything needed to turn a frozen snapshot + runtime + session into a plan.
pub struct PlanInput<'a> {
    pub snapshot: &'a InstanceLaunchSnapshot,
    pub runtime: &'a JavaRuntime,
    pub session: &'a GameSession,
    /// Server address when the Quick Play capability table allows direct join.
    pub quick_play_address: Option<&'a str>,
}

/// Build the plan or refuse with a stable code.
pub fn build_plan(input: PlanInput<'_>, plan_id: String) -> Result<LaunchPlan, LaunchRefusal> {
    let snapshot = input.snapshot;
    let classpath = snapshot
        .classpath
        .iter()
        .map(|entry| entry.display().to_string())
        .collect::<Vec<_>>()
        .join(&classpath_separator().to_string());

    if snapshot.main_class.trim().is_empty() {
        return Err(LaunchRefusal::new(errors::plan_invalid(
            "plan:main-class:empty",
        )));
    }
    if classpath.is_empty() {
        return Err(LaunchRefusal::new(errors::plan_invalid(
            "plan:classpath:empty",
        )));
    }

    let mut variables: BTreeMap<&str, String> = BTreeMap::new();
    variables.insert("classpath", classpath.clone());
    variables.insert("game_directory", snapshot.game_dir.display().to_string());
    variables.insert("version_name", snapshot.game_version.clone());
    variables.insert(
        "version_type",
        snapshot
            .version_type
            .clone()
            .unwrap_or_else(|| snapshot.loader.family().to_string()),
    );
    let assets_root = snapshot
        .assets_dir
        .as_ref()
        .map_or_else(String::new, |path| path.display().to_string());
    variables.insert("assets_root", assets_root.clone());
    variables.insert("game_assets", assets_root);
    variables.insert(
        "assets_index_name",
        snapshot.asset_index.clone().unwrap_or_default(),
    );
    variables.insert(
        "natives_directory",
        snapshot
            .natives_dir
            .as_ref()
            .map_or_else(String::new, |path| path.display().to_string()),
    );
    variables.insert("auth_player_name", input.session.username.clone());
    variables.insert("auth_uuid", input.session.uuid.clone());
    variables.insert(
        "auth_access_token",
        input.session.access_token().to_string(),
    );
    variables.insert("auth_session", input.session.access_token().to_string());
    variables.insert("user_type", input.session.user_type.clone());
    variables.insert("user_properties", "{}".to_string());
    variables.insert("profile_properties", "{}".to_string());
    variables.insert("classpath_separator", classpath_separator().to_string());
    variables.insert(
        "library_directory",
        snapshot.game_dir.join("libraries").display().to_string(),
    );
    // These values are absent for an explicitly selected local identity.
    // Online providers must supply their own authenticated context first.
    if input.session.user_type == "legacy" && input.session.access_token() == "0" {
        variables.insert("clientid", String::new());
        variables.insert("auth_xuid", String::new());
    }
    variables.insert("launcher_name", "shtmcl".to_string());
    variables.insert("launcher_version", env!("CARGO_PKG_VERSION").to_string());

    let mut jvm_args: Vec<PlanArg> = Vec::new();
    if let Some(memory) = snapshot.memory_mb {
        jvm_args.push(PlanArg::plain(format!("-Xmx{memory}M")));
    }
    if let Some(natives) = snapshot.natives_dir.as_ref() {
        jvm_args.push(PlanArg::plain(format!(
            "-Djava.library.path={}",
            natives.display()
        )));
    }
    jvm_args.push(PlanArg::plain("-Dminecraft.launcher.brand=shtmcl"));
    jvm_args.push(PlanArg::plain(format!(
        "-Dminecraft.launcher.version={}",
        env!("CARGO_PKG_VERSION")
    )));
    // classpath ownership: 02 assembles the classpath, but vanilla metadata often
    // already ships `-cp ${classpath}`. Only an actual `-cp` / `-classpath` flag
    // whose value comes from `${classpath}` claims the classpath — merely *reading*
    // the variable (for example `-Dmodpath=${classpath}`) does not, or the plan
    // would end up with no classpath at all (O02-1b).
    let metadata_supplies_classpath = classpath_flag_supplied_by_metadata(&snapshot.jvm_args);
    for template in &snapshot.jvm_args {
        jvm_args.push(expand_argument(template, &variables, "jvm")?);
    }
    if !metadata_supplies_classpath {
        jvm_args.push(PlanArg::plain("-cp"));
        jvm_args.push(PlanArg::plain(classpath));
    }
    jvm_args.push(PlanArg::plain(snapshot.main_class.clone()));

    let mut game_args: Vec<PlanArg> = Vec::new();
    // The instance snapshot carries the complete game argument template list
    // (as version/loader metadata defines it). 02 does not invent extra
    // arguments: it only substitutes defined variables.
    for template in &snapshot.game_args {
        game_args.push(expand_argument(template, &variables, "game")?);
    }
    if let Some(address) = input.quick_play_address {
        game_args.push(PlanArg::plain("--quickPlayMultiplayer"));
        game_args.push(PlanArg::plain(address));
    }

    let sensitive_args = jvm_args
        .iter()
        .chain(game_args.iter())
        .filter(|argument| argument.sensitive)
        .count();
    let digest = args_digest(&input.runtime.path, &jvm_args, &game_args);

    let artifacts = snapshot
        .classpath
        .iter()
        .map(|path| PlannedArtifact {
            path: path.clone(),
            role: match snapshot.loader {
                LoaderKind::Vanilla => ArtifactRole::Client,
                _ => ArtifactRole::Library,
            },
        })
        .collect();

    Ok(LaunchPlan {
        id: plan_id,
        instance_id: snapshot.instance_id.clone(),
        instance_revision: snapshot.revision,
        content_revision: snapshot.content_revision,
        runtime_id: input.runtime.id.clone(),
        java_major: input.runtime.version.major,
        profile_ref: snapshot.profile_ref.clone(),
        program: input.runtime.path.clone(),
        jvm_args,
        game_args,
        working_dir: snapshot.game_dir.clone(),
        artifacts,
        required_files: snapshot.collect_required(),
        args_digest: digest,
        sensitive_args,
        game_version: snapshot.game_version.clone(),
        loader: snapshot.loader.clone(),
        quick_play: input.quick_play_address.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variables() -> BTreeMap<&'static str, String> {
        BTreeMap::from([
            ("game_directory", "/games/one".to_string()),
            ("auth_player_name", "Steve".to_string()),
        ])
    }

    #[test]
    fn only_a_classpath_flag_claims_the_classpath() {
        // O02-1b: reading `${classpath}` elsewhere must not suppress 02's flag.
        assert!(!classpath_flag_supplied_by_metadata(&[
            "-Dmodpath=${classpath}".to_string()
        ]));
        assert!(!classpath_flag_supplied_by_metadata(&[]));
        assert!(!classpath_flag_supplied_by_metadata(&["-cp".to_string()]));

        assert!(classpath_flag_supplied_by_metadata(&[
            "-cp".to_string(),
            "${classpath}".to_string()
        ]));
        assert!(classpath_flag_supplied_by_metadata(&[
            "-classpath".to_string(),
            "${classpath}".to_string()
        ]));
        assert!(classpath_flag_supplied_by_metadata(&[
            "-Dmodpath=${classpath}".to_string(),
            "-cp".to_string(),
            "${classpath}".to_string(),
            "-Xss1M".to_string(),
        ]));
        // A flag whose value does not come from the variable does not claim it.
        assert!(!classpath_flag_supplied_by_metadata(&[
            "-cp".to_string(),
            "/hardcoded/mods".to_string()
        ]));
    }

    #[test]
    fn templates_reject_undefined_variables() {
        let vars = variables();
        assert_eq!(
            expand_template("--demo --dir ${game_directory}", &vars),
            Ok("--demo --dir /games/one".to_string())
        );
        assert_eq!(
            expand_template("--name ${auth_player_name}", &vars),
            Ok("--name Steve".to_string())
        );
        assert_eq!(
            expand_template("--x ${unknown_var}", &vars),
            Err("unknown_var".to_string())
        );
        assert_eq!(
            expand_template("--x ${unterminated", &vars),
            Err("${".to_string())
        );
    }

    #[test]
    fn digest_never_contains_a_token() {
        let jvm = vec![PlanArg::plain("-cp"), PlanArg::plain("a.jar")];
        let game = vec![
            PlanArg::plain("--accessToken"),
            PlanArg::sensitive("super-secret-token"),
        ];
        let digest = args_digest(Path::new("java"), &jvm, &game);
        assert!(digest.starts_with("fnv1a64:"));
        let redacted: Vec<String> = jvm
            .iter()
            .chain(game.iter())
            .map(PlanArg::redacted)
            .collect();
        assert!(!redacted.iter().any(|value| value.contains("super-secret")));
        // The same arguments always digest the same way.
        assert_eq!(digest, args_digest(Path::new("java"), &jvm, &game));
    }

    #[test]
    fn required_files_merge_classpath_and_natives() {
        let snapshot = InstanceLaunchSnapshot {
            instance_id: InstanceId::new("instance-1").expect("id"),
            revision: Revision::new(3),
            content_revision: None,
            name: "One".to_string(),
            game_version: "1.20.4".to_string(),
            version_type: None,
            loader: LoaderKind::Vanilla,
            game_dir: PathBuf::from("/games/one"),
            main_class: "net.minecraft.client.main.Main".to_string(),
            classpath: vec![
                PathBuf::from("/games/one/a.jar"),
                PathBuf::from("/games/one/a.jar"),
            ],
            assets_dir: None,
            asset_index: None,
            natives_dir: Some(PathBuf::from("/games/one/natives")),
            jvm_args: Vec::new(),
            game_args: Vec::new(),
            memory_mb: None,
            java_requirement: None,
            java_path: None,
            required_files: vec![RequiredFile::new(
                PathBuf::from("/games/one/client.jar"),
                ArtifactRole::Client,
            )],
            auth: AuthRequirement::Offline,
            offline_username: None,
            multi_open: false,
            profile_ref: None,
        };
        let required = snapshot.collect_required();
        assert_eq!(required.len(), 3, "client jar + one jar + natives");
        assert!(required
            .iter()
            .any(|file| file.role == ArtifactRole::Natives));
    }
}
