//! Bounded parsing of NeoForge installer profiles.
//!
//! This module only validates and describes installer metadata. It never runs
//! processors, extracts files, or mutates an instance.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_PROFILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_VERSION_JSON_BYTES: usize = 2 * 1024 * 1024;
const MAX_INSTALLER_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;
const MAX_PROFILE_STRING: usize = 8192;
const MAX_VERSION_LIBRARIES: usize = 4096;
const MAX_VERSION_ARGUMENTS: usize = 2048;
const MAX_LIBRARIES: usize = 2048;
const MAX_PROCESSORS: usize = 256;
const MAX_PROCESSOR_CLASSPATH: usize = 256;
const MAX_PROCESSOR_ARGS: usize = 256;
const MAX_DATA_KEYS: usize = 256;
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_LIBRARY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_PROFILE_RESOURCE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeoForgeInstallerProfile {
    pub game_version: String,
    pub loader_version: String,
    pub version_json_path: String,
    pub libraries: Vec<InstallerLibrary>,
    pub processors: Vec<InstallerProcessor>,
    pub data: BTreeMap<String, InstallerData>,
}

/// Validated metadata read from an installer archive. Resources are hashed in
/// place and are not copied into memory; processor execution and file writes
/// remain the responsibility of a later, separately reviewed install flow.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNeoForgeInstaller {
    pub profile: NeoForgeInstallerProfile,
    pub version_metadata: Value,
    pub resources: Vec<InstallerResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerResource {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// A non-executable classification of one processor descriptor. This is not
/// authorization to launch its tool or write its outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessorOperation {
    ExtractFiles,
    BundlerExtract,
    McpData,
    DownloadMojmaps,
    MergeMapping,
    JarSplitter,
    AutoRenamingTool,
    BinaryPatcher,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessorAssessment {
    pub index: usize,
    pub operation: ProcessorOperation,
    pub tool_coordinate: String,
    pub sides: Option<Vec<InstallerSide>>,
    pub argument_count: usize,
    pub classpath_count: usize,
}

/// Recognizes tool families and NeoForge task names observed in the captured
/// 20.4.253 installer. It deliberately returns a summary only.
pub fn assess_processors(
    profile: &NeoForgeInstallerProfile,
) -> Result<Vec<ProcessorAssessment>, &'static str> {
    validate_processor_artifacts(profile)?;
    assess_processor_syntax(profile)
}

fn assess_processor_syntax(
    profile: &NeoForgeInstallerProfile,
) -> Result<Vec<ProcessorAssessment>, &'static str> {
    profile
        .processors
        .iter()
        .enumerate()
        .map(|(index, processor)| {
            let operation = classify_processor(processor)?;
            validate_processor_placeholders(&processor.args)?;
            validate_processor_arguments(profile, operation, &processor.args)?;
            Ok(ProcessorAssessment {
                index,
                operation,
                tool_coordinate: processor.jar.clone(),
                sides: processor.sides.clone(),
                argument_count: processor.args.len(),
                classpath_count: processor.classpath.len(),
            })
        })
        .collect()
}

fn validate_processor_artifacts(profile: &NeoForgeInstallerProfile) -> Result<(), &'static str> {
    if profile.game_version != "1.20.4" || profile.loader_version != "20.4.253" {
        return Err("neoforge_processor_artifact_version_unsupported");
    }

    let mut pins = BTreeMap::new();
    for pin in super::neoforge_processor_allowlist::ARTIFACTS {
        let identity = coordinate_identity(pin.coordinate)
            .map_err(|_| "neoforge_processor_allowlist_invalid")?;
        if pin.sha1.len() != 40
            || !pin.sha1.bytes().all(|byte| byte.is_ascii_hexdigit())
            || pin.sha256.len() != 64
            || !pin.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || pin.size == 0
            || pins.insert(identity, pin).is_some()
        {
            return Err("neoforge_processor_allowlist_invalid");
        }
    }

    let mut used = BTreeSet::new();
    for coordinate in profile.processors.iter().flat_map(|processor| {
        std::iter::once(processor.jar.as_str())
            .chain(processor.classpath.iter().map(String::as_str))
    }) {
        let identity =
            coordinate_identity(coordinate).map_err(|_| "neoforge_processor_artifact_unpinned")?;
        let pin = pins
            .get(&identity)
            .ok_or("neoforge_processor_artifact_unpinned")?;
        let mut matching_libraries = profile.libraries.iter().filter(|library| {
            coordinate_identity(&library.coordinate)
                .is_ok_and(|library_identity| library_identity == identity)
        });
        let library = matching_libraries
            .next()
            .ok_or("neoforge_processor_artifact_missing")?;
        if matching_libraries.next().is_some()
            || library.path != pin.path
            || library.url != pin.url
            || library.sha1 != pin.sha1
            || library.size != pin.size
        {
            return Err("neoforge_processor_artifact_mismatch");
        }
        used.insert(identity);
    }

    if used.len() != pins.len() || pins.keys().any(|identity| !used.contains(identity)) {
        return Err("neoforge_processor_artifact_closure_mismatch");
    }
    Ok(())
}

fn classify_processor(processor: &InstallerProcessor) -> Result<ProcessorOperation, &'static str> {
    let operation = match processor
        .jar
        .split(':')
        .take(2)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["net.neoforged.installertools", "installertools"] => {
            let mut task = None;
            for (index, argument) in processor.args.iter().enumerate() {
                if argument == "--task" {
                    if task.is_some() {
                        return Err("neoforge_processor_task_duplicate");
                    }
                    task = Some(
                        processor
                            .args
                            .get(index + 1)
                            .map(String::as_str)
                            .ok_or("neoforge_processor_task_missing")?,
                    );
                }
            }
            match task.ok_or("neoforge_processor_task_missing")? {
                "EXTRACT_FILES" => ProcessorOperation::ExtractFiles,
                "BUNDLER_EXTRACT" => ProcessorOperation::BundlerExtract,
                "MCP_DATA" => ProcessorOperation::McpData,
                "DOWNLOAD_MOJMAPS" => ProcessorOperation::DownloadMojmaps,
                "MERGE_MAPPING" => ProcessorOperation::MergeMapping,
                _ => return Err("neoforge_processor_task_unsupported"),
            }
        }
        ["net.neoforged.installertools", "jarsplitter"] => ProcessorOperation::JarSplitter,
        ["net.neoforged", "AutoRenamingTool"] => ProcessorOperation::AutoRenamingTool,
        ["net.neoforged.installertools", "binarypatcher"] => ProcessorOperation::BinaryPatcher,
        _ => return Err("neoforge_processor_tool_unsupported"),
    };
    Ok(operation)
}

fn validate_processor_arguments(
    profile: &NeoForgeInstallerProfile,
    operation: ProcessorOperation,
    all_args: &[String],
) -> Result<(), &'static str> {
    let expected_task = match operation {
        ProcessorOperation::ExtractFiles => Some("EXTRACT_FILES"),
        ProcessorOperation::BundlerExtract => Some("BUNDLER_EXTRACT"),
        ProcessorOperation::McpData => Some("MCP_DATA"),
        ProcessorOperation::DownloadMojmaps => Some("DOWNLOAD_MOJMAPS"),
        ProcessorOperation::MergeMapping => Some("MERGE_MAPPING"),
        _ => None,
    };
    let args = if let Some(task) = expected_task {
        if all_args.get(0).map(String::as_str) != Some("--task")
            || all_args.get(1).map(String::as_str) != Some(task)
        {
            return Err("neoforge_processor_task_shape");
        }
        &all_args[2..]
    } else {
        all_args
    };

    let (value_options, flag_options, required_options, repeatable_options): (
        &[&str],
        &[&str],
        &[&str],
        &[&str],
    ) = match operation {
        ProcessorOperation::ExtractFiles => (
            &["--archive", "--from", "--to", "--exec", "--optional"],
            &[],
            &["--archive", "--from", "--to"],
            &["--from", "--to", "--exec", "--optional"],
        ),
        ProcessorOperation::BundlerExtract => (
            &["--input", "--output"],
            &["--jar-only", "--libraries"],
            &["--input", "--output"],
            &[],
        ),
        ProcessorOperation::McpData => (
            &["--input", "--output", "--key"],
            &[],
            &["--input", "--output", "--key"],
            &[],
        ),
        ProcessorOperation::DownloadMojmaps => (
            &["--version", "--side", "--output"],
            &[],
            &["--version", "--side", "--output"],
            &[],
        ),
        ProcessorOperation::MergeMapping => (
            &["--left", "--right", "--output"],
            &["--classes", "--fields", "--methods", "--reverse-right"],
            &[
                "--left",
                "--right",
                "--output",
                "--classes",
                "--fields",
                "--methods",
            ],
            &[],
        ),
        ProcessorOperation::JarSplitter => (
            &["--input", "--slim", "--extra", "--srg"],
            &[],
            &["--input", "--slim", "--extra", "--srg"],
            &[],
        ),
        ProcessorOperation::AutoRenamingTool => (
            &["--input", "--output", "--names"],
            &["--ann-fix", "--ids-fix", "--src-fix", "--record-fix"],
            &["--input", "--output", "--names"],
            &[],
        ),
        ProcessorOperation::BinaryPatcher => (
            &["--clean", "--output", "--apply"],
            &[],
            &["--clean", "--output", "--apply"],
            &[],
        ),
    };

    let mut seen = BTreeMap::<&str, usize>::new();
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if value_options.contains(&option) {
            let value = args
                .get(index + 1)
                .ok_or("neoforge_processor_argument_value")?;
            if value.starts_with("--") {
                return Err("neoforge_processor_argument_value");
            }
            let count = seen.entry(option).or_default();
            *count += 1;
            if *count > 1 && !repeatable_options.contains(&option) {
                return Err("neoforge_processor_argument_duplicate");
            }
            validate_processor_argument_value(profile, operation, option, value)?;
            index += 2;
        } else if flag_options.contains(&option) {
            let count = seen.entry(option).or_default();
            *count += 1;
            if *count > 1 {
                return Err("neoforge_processor_argument_duplicate");
            }
            index += 1;
        } else {
            return Err("neoforge_processor_argument_unsupported");
        }
    }

    if required_options
        .iter()
        .any(|option| seen.get(option).copied().unwrap_or_default() == 0)
    {
        return Err("neoforge_processor_argument_missing");
    }
    if operation == ProcessorOperation::ExtractFiles && seen.get("--from") != seen.get("--to") {
        return Err("neoforge_processor_extract_pair");
    }
    if operation == ProcessorOperation::BundlerExtract
        && seen.contains_key("--jar-only") == seen.contains_key("--libraries")
    {
        return Err("neoforge_processor_bundler_mode");
    }
    Ok(())
}

fn validate_processor_argument_value(
    profile: &NeoForgeInstallerProfile,
    operation: ProcessorOperation,
    option: &str,
    value: &str,
) -> Result<(), &'static str> {
    let expected = match (operation, option) {
        (ProcessorOperation::ExtractFiles, "--archive") => Some("{INSTALLER}"),
        (ProcessorOperation::BundlerExtract, "--input") => Some("{MINECRAFT_JAR}"),
        (ProcessorOperation::McpData, "--output") => Some("{MAPPINGS}"),
        (ProcessorOperation::DownloadMojmaps, "--side") => Some("{SIDE}"),
        (ProcessorOperation::DownloadMojmaps, "--output") => Some("{MOJMAPS}"),
        (ProcessorOperation::MergeMapping, "--left") => Some("{MAPPINGS}"),
        (ProcessorOperation::MergeMapping, "--right") => Some("{MOJMAPS}"),
        (ProcessorOperation::MergeMapping, "--output") => Some("{MERGED_MAPPINGS}"),
        (ProcessorOperation::JarSplitter, "--slim") => Some("{MC_SLIM}"),
        (ProcessorOperation::JarSplitter, "--extra") => Some("{MC_EXTRA}"),
        (ProcessorOperation::JarSplitter, "--srg") => Some("{MERGED_MAPPINGS}"),
        (ProcessorOperation::AutoRenamingTool, "--input") => Some("{MC_SLIM}"),
        (ProcessorOperation::AutoRenamingTool, "--output") => Some("{MC_SRG}"),
        (ProcessorOperation::AutoRenamingTool, "--names") => Some("{MERGED_MAPPINGS}"),
        (ProcessorOperation::BinaryPatcher, "--clean") => Some("{MC_SRG}"),
        (ProcessorOperation::BinaryPatcher, "--output") => Some("{PATCHED}"),
        (ProcessorOperation::BinaryPatcher, "--apply") => Some("{BINPATCH}"),
        _ => None,
    };
    if let Some(expected) = expected {
        if value != expected {
            return Err("neoforge_processor_argument_binding");
        }
        return Ok(());
    }

    match (operation, option) {
        (ProcessorOperation::ExtractFiles, "--from") => {
            if super::paths::normalise_content_path(value)
                .map(|normalized| normalized != value)
                .unwrap_or(true)
            {
                return Err("neoforge_processor_archive_path");
            }
        }
        (ProcessorOperation::ExtractFiles, "--to" | "--exec" | "--optional")
        | (ProcessorOperation::BundlerExtract, "--output") => {
            if !value.starts_with("{ROOT}")
                && !(operation == ProcessorOperation::BundlerExtract && value == "{MC_UNPACKED}")
            {
                return Err("neoforge_processor_output_path");
            }
        }
        (ProcessorOperation::BundlerExtract, "--input") => {
            if value != "{MINECRAFT_JAR}" {
                return Err("neoforge_processor_argument_binding");
            }
        }
        (ProcessorOperation::McpData, "--input") => {
            let coordinate = value
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .ok_or("neoforge_processor_data_input")?;
            let identity =
                coordinate_identity(coordinate).map_err(|_| "neoforge_processor_data_input")?;
            if !profile.libraries.iter().any(|library| {
                coordinate_identity(&library.coordinate)
                    .is_ok_and(|library_identity| library_identity == identity)
            }) {
                return Err("neoforge_processor_data_library_missing");
            }
        }
        (ProcessorOperation::McpData, "--key") => {
            if value != "mappings" {
                return Err("neoforge_processor_data_key");
            }
        }
        (ProcessorOperation::DownloadMojmaps, "--version") => {
            if !valid_version_token(value) {
                return Err("neoforge_processor_version");
            }
        }
        (ProcessorOperation::JarSplitter, "--input") => {
            if value != "{MC_UNPACKED}" && value != "{MINECRAFT_JAR}" {
                return Err("neoforge_processor_argument_binding");
            }
        }
        _ => return Err("neoforge_processor_argument_binding"),
    }
    Ok(())
}

fn validate_processor_placeholders(args: &[String]) -> Result<(), &'static str> {
    const KNOWN: &[&str] = &[
        "INSTALLER",
        "ROOT",
        "MINECRAFT_JAR",
        "MC_UNPACKED",
        "MAPPINGS",
        "SIDE",
        "MOJMAPS",
        "MERGED_MAPPINGS",
        "MC_SLIM",
        "MC_EXTRA",
        "MC_SRG",
        "PATCHED",
        "BINPATCH",
    ];
    for argument in args {
        let Some(open) = argument.find('{') else {
            if argument.contains('}') {
                return Err("neoforge_processor_placeholder_invalid");
            }
            continue;
        };
        let close = argument[open..]
            .find('}')
            .map(|offset| open + offset)
            .ok_or("neoforge_processor_placeholder_invalid")?;
        if argument[close + 1..]
            .chars()
            .any(|character| matches!(character, '{' | '}'))
            || argument[..open].contains('}')
        {
            return Err("neoforge_processor_placeholder_invalid");
        }
        let name = &argument[open + 1..close];
        if !KNOWN.contains(&name) {
            return Err("neoforge_processor_placeholder_unsupported");
        }
        if name != "ROOT" && (open != 0 || close + 1 != argument.len()) {
            return Err("neoforge_processor_placeholder_invalid");
        }
        if name == "ROOT" {
            if open != 0 {
                return Err("neoforge_processor_root_template_invalid");
            }
            let remainder = &argument[close + 1..];
            let suffix = if remainder.is_empty() {
                remainder
            } else {
                remainder
                    .strip_prefix('/')
                    .filter(|suffix| !suffix.starts_with('/') && !suffix.starts_with('\\'))
                    .ok_or("neoforge_processor_root_template_invalid")?
            };
            if !suffix.is_empty() {
                let expected = suffix.strip_suffix('/').unwrap_or(suffix);
                if super::paths::normalise_content_path(suffix)
                    .map(|normalized| normalized != expected)
                    .unwrap_or(true)
                {
                    return Err("neoforge_processor_root_template_invalid");
                }
            }
        }
    }
    Ok(())
}

/// Reads only the profile, its referenced version JSON, and profile data files
/// from a NeoForge installer. Every selected ZIP entry is CRC/length checked.
pub fn read_installer_archive(
    game_version: &str,
    loader_version: &str,
    bytes: &[u8],
) -> Result<ParsedNeoForgeInstaller, &'static str> {
    if bytes.len() > MAX_INSTALLER_ARCHIVE_BYTES {
        return Err("neoforge_installer_too_large");
    }
    let budget = super::zipindex::ZipBudget::default();
    let index =
        super::zipindex::read_index(bytes, &budget).map_err(|_| "neoforge_installer_archive")?;
    let profile_entry = unique_entry(&index, "install_profile.json")?;
    let profile_bytes = super::zipindex::read_entry(bytes, profile_entry, &json_budget())
        .map_err(|_| "neoforge_installer_profile_entry")?;
    let profile = parse_install_profile(game_version, loader_version, &profile_bytes)?;

    let version_entry = unique_entry(&index, &profile.version_json_path)?;
    let version_bytes = super::zipindex::read_entry(bytes, version_entry, &json_budget())
        .map_err(|_| "neoforge_installer_version_entry")?;
    let version_metadata = parse_version_metadata(&version_bytes, game_version, loader_version)?;

    let mut resource_paths = BTreeSet::new();
    let mut resources = Vec::new();
    let mut total_resource_bytes = 0u64;
    for value in profile.data.values() {
        for value in data_values(value) {
            if let Some(path) = value.strip_prefix('/') {
                let path = path.to_string();
                if !resource_paths.insert(path.clone()) {
                    continue;
                }
                let entry = unique_entry(&index, &path)?;
                let digest = super::zipindex::verify_entry(bytes, entry, &budget)
                    .map_err(|_| "neoforge_installer_resource_invalid")?;
                total_resource_bytes = total_resource_bytes
                    .checked_add(digest.uncompressed)
                    .filter(|total| *total <= MAX_PROFILE_RESOURCE_BYTES)
                    .ok_or("neoforge_installer_resource_budget")?;
                resources.push(InstallerResource {
                    path,
                    size: digest.uncompressed,
                    sha256: digest.sha256,
                });
            }
        }
    }
    resources.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(ParsedNeoForgeInstaller {
        profile,
        version_metadata,
        resources,
    })
}

fn json_budget() -> super::zipindex::ZipBudget {
    super::zipindex::ZipBudget {
        max_entries: 1,
        max_total_uncompressed: MAX_VERSION_JSON_BYTES as u64,
        max_entry_uncompressed: MAX_VERSION_JSON_BYTES as u64,
    }
}

fn unique_entry<'a>(
    index: &'a super::zipindex::ZipIndex,
    path: &str,
) -> Result<&'a super::zipindex::ZipEntry, &'static str> {
    let mut matching = index.entries.iter().filter(|entry| entry.name == path);
    let entry = matching.next().ok_or("neoforge_installer_entry_missing")?;
    if matching.next().is_some() {
        return Err("neoforge_installer_entry_duplicated");
    }
    Ok(entry)
}

fn data_values(data: &InstallerData) -> Vec<&str> {
    match data {
        InstallerData::Shared(value) => vec![value],
        InstallerData::Sided { client, server } => client
            .iter()
            .chain(server.iter())
            .map(String::as_str)
            .collect(),
    }
}

fn parse_version_metadata(
    bytes: &[u8],
    game_version: &str,
    loader_version: &str,
) -> Result<Value, &'static str> {
    if bytes.len() > MAX_VERSION_JSON_BYTES {
        return Err("neoforge_version_too_large");
    }
    let raw: Value = serde_json::from_slice(bytes).map_err(|_| "neoforge_version_invalid")?;
    let object = raw.as_object().ok_or("neoforge_version_invalid")?;
    if object.get("id").and_then(Value::as_str)
        != Some(format!("neoforge-{loader_version}").as_str())
        || object.get("inheritsFrom").and_then(Value::as_str) != Some(game_version)
    {
        return Err("neoforge_version_mismatch");
    }
    if !matches!(
        object.get("mainClass").and_then(Value::as_str),
        Some("cpw.mods.bootstraplauncher.BootstrapLauncher")
    ) {
        return Err("neoforge_version_main_class");
    }

    let libraries = object
        .get("libraries")
        .and_then(Value::as_array)
        .filter(|items| items.len() <= MAX_VERSION_LIBRARIES)
        .ok_or("neoforge_version_libraries")?;
    for library in libraries {
        let name = bounded_string(library.get("name"), MAX_PROFILE_STRING)?;
        coordinate_identity(name).map_err(|_| "neoforge_version_library")?;
    }
    if let Some(arguments) = object.get("arguments") {
        let arguments = arguments.as_object().ok_or("neoforge_version_arguments")?;
        for key in ["game", "jvm"] {
            if let Some(value) = arguments.get(key) {
                let values = value
                    .as_array()
                    .filter(|values| values.len() <= MAX_VERSION_ARGUMENTS)
                    .ok_or("neoforge_version_arguments")?;
                for argument in values {
                    validate_version_argument(argument)?;
                }
            }
        }
    }
    Ok(raw)
}

fn validate_version_argument(value: &Value) -> Result<(), &'static str> {
    if value.as_str().is_some_and(|value| {
        !value.is_empty()
            && value.len() <= MAX_PROFILE_STRING
            && !value.chars().any(char::is_control)
    }) {
        return Ok(());
    }
    let object = value.as_object().ok_or("neoforge_version_argument")?;
    if object.len() > 2
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "value" | "rules"))
    {
        return Err("neoforge_version_argument");
    }
    if let Some(values) = object.get("value") {
        if let Some(value) = values.as_str() {
            if value.is_empty()
                || value.len() > MAX_PROFILE_STRING
                || value.chars().any(char::is_control)
            {
                return Err("neoforge_version_argument");
            }
        } else {
            let values = values
                .as_array()
                .filter(|values| !values.is_empty() && values.len() <= 32)
                .ok_or("neoforge_version_argument")?;
            for value in values {
                validate_version_argument(value)?;
            }
        }
    } else {
        return Err("neoforge_version_argument");
    }
    if let Some(rules) = object.get("rules") {
        let rules = rules
            .as_array()
            .filter(|rules| !rules.is_empty() && rules.len() <= 32)
            .ok_or("neoforge_version_argument")?;
        for rule in rules {
            let rule = rule.as_object().ok_or("neoforge_version_argument")?;
            if !matches!(
                rule.get("action").and_then(Value::as_str),
                Some("allow" | "disallow")
            ) || rule.len() > 2
                || rule
                    .keys()
                    .any(|key| !matches!(key.as_str(), "action" | "os"))
            {
                return Err("neoforge_version_argument");
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerLibrary {
    pub coordinate: String,
    pub path: String,
    pub url: String,
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerProcessor {
    pub sides: Option<Vec<InstallerSide>>,
    pub jar: String,
    pub classpath: Vec<String>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallerSide {
    Client,
    Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallerData {
    Shared(String),
    Sided {
        client: Option<String>,
        server: Option<String>,
    },
}

/// Parse one `install_profile.json` for the exact game and loader selection.
/// Every version identity and download record is checked before the caller can
/// create a task or stage instance files.
pub fn parse_install_profile(
    game_version: &str,
    loader_version: &str,
    bytes: &[u8],
) -> Result<NeoForgeInstallerProfile, &'static str> {
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err("neoforge_profile_too_large");
    }
    if !valid_version_token(game_version) || !valid_version_token(loader_version) {
        return Err("neoforge_profile_version_invalid");
    }
    let root: Value = serde_json::from_slice(bytes).map_err(|_| "neoforge_profile_invalid")?;
    let object = root.as_object().ok_or("neoforge_profile_invalid")?;
    if object.get("spec").and_then(Value::as_u64) != Some(1)
        || object.get("profile").and_then(Value::as_str) != Some("NeoForge")
        || object.get("minecraft").and_then(Value::as_str) != Some(game_version)
        || object.get("version").and_then(Value::as_str)
            != Some(format!("neoforge-{loader_version}").as_str())
    {
        return Err("neoforge_profile_mismatch");
    }

    let version_json_path = parse_version_json_path(object.get("json"))?;
    let libraries = parse_libraries(object.get("libraries"))?;
    let processors = parse_processors(object.get("processors"), &libraries)?;
    let data = parse_data(object.get("data"))?;

    Ok(NeoForgeInstallerProfile {
        game_version: game_version.to_string(),
        loader_version: loader_version.to_string(),
        version_json_path,
        libraries,
        processors,
        data,
    })
}

fn parse_version_json_path(value: Option<&Value>) -> Result<String, &'static str> {
    let raw = value
        .and_then(Value::as_str)
        .filter(|raw| raw.len() <= 512)
        .ok_or("neoforge_profile_version_json")?;
    let relative = raw
        .strip_prefix('/')
        .filter(|relative| !relative.starts_with('/'))
        .ok_or("neoforge_profile_version_json")?;
    let normalized = super::paths::normalise_content_path(relative)
        .map_err(|_| "neoforge_profile_version_json")?;
    if normalized != relative {
        return Err("neoforge_profile_version_json");
    }
    Ok(normalized)
}

fn parse_libraries(value: Option<&Value>) -> Result<Vec<InstallerLibrary>, &'static str> {
    let entries = value
        .and_then(Value::as_array)
        .filter(|entries| !entries.is_empty() && entries.len() <= MAX_LIBRARIES)
        .ok_or("neoforge_profile_libraries")?;
    let mut libraries = BTreeMap::new();
    let mut total_size = 0u64;
    for entry in entries {
        let name = bounded_string(entry.get("name"), MAX_PROFILE_STRING)?;
        let expected_path = maven_path(name)?;
        let artifact = entry
            .get("downloads")
            .and_then(|downloads| downloads.get("artifact"))
            .ok_or("neoforge_profile_library_artifact")?;
        let path = bounded_string(artifact.get("path"), 1024)?;
        let path = super::paths::normalise_content_path(path)
            .map_err(|_| "neoforge_profile_library_path")?;
        if path != expected_path {
            return Err("neoforge_profile_library_path");
        }
        let sha1 = bounded_string(artifact.get("sha1"), 40)?;
        if sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("neoforge_profile_library_checksum");
        }
        let size = artifact
            .get("size")
            .and_then(Value::as_u64)
            .filter(|size| *size > 0 && *size <= MAX_ARTIFACT_BYTES)
            .ok_or("neoforge_profile_library_size")?;
        let url = bounded_string(artifact.get("url"), 2048)?;
        validate_library_url(url, &path)?;
        let library = InstallerLibrary {
            coordinate: name.to_string(),
            path,
            url: url.to_string(),
            sha1: sha1.to_ascii_lowercase(),
            size,
        };
        if let Some(existing) = libraries.get(name) {
            if existing != &library {
                return Err("neoforge_profile_library_conflict");
            }
        } else {
            total_size = total_size
                .checked_add(size)
                .filter(|total| *total <= MAX_TOTAL_LIBRARY_BYTES)
                .ok_or("neoforge_profile_library_budget")?;
            libraries.insert(name.to_string(), library);
        }
    }
    Ok(libraries.into_values().collect())
}

fn parse_processors(
    value: Option<&Value>,
    libraries: &[InstallerLibrary],
) -> Result<Vec<InstallerProcessor>, &'static str> {
    let entries = value
        .and_then(Value::as_array)
        .filter(|entries| entries.len() <= MAX_PROCESSORS)
        .ok_or("neoforge_profile_processors")?;
    let library_names: BTreeSet<String> = libraries
        .iter()
        .filter_map(|library| coordinate_identity(&library.coordinate).ok())
        .collect();
    let mut processors = Vec::with_capacity(entries.len());
    for entry in entries {
        let jar = bounded_string(entry.get("jar"), MAX_PROFILE_STRING)?;
        let jar_identity = coordinate_identity(jar)?;
        let classpath_values = entry
            .get("classpath")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty() && items.len() <= MAX_PROCESSOR_CLASSPATH)
            .ok_or("neoforge_profile_processor_classpath")?;
        let mut classpath = Vec::with_capacity(classpath_values.len());
        let mut classpath_identities = BTreeSet::new();
        for dependency in classpath_values {
            let coordinate = bounded_string(Some(dependency), MAX_PROFILE_STRING)?;
            let identity = coordinate_identity(coordinate)?;
            if !library_names.contains(&identity) {
                return Err("neoforge_profile_processor_library_missing");
            }
            classpath_identities.insert(identity);
            classpath.push(coordinate.to_string());
        }
        if !classpath_identities.contains(&jar_identity) {
            return Err("neoforge_profile_processor_jar_missing");
        }
        let args_values = entry
            .get("args")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty() && items.len() <= MAX_PROCESSOR_ARGS)
            .ok_or("neoforge_profile_processor_args")?;
        let args = args_values
            .iter()
            .map(|argument| bounded_string(Some(argument), MAX_PROFILE_STRING).map(str::to_string))
            .collect::<Result<Vec<_>, _>>()?;
        let sides = match entry.get("sides") {
            None => None,
            Some(value) => {
                let values = value
                    .as_array()
                    .filter(|values| !values.is_empty() && values.len() <= 2)
                    .ok_or("neoforge_profile_processor_sides")?;
                let mut parsed = Vec::with_capacity(values.len());
                for side in values {
                    let side = match side.as_str() {
                        Some("client") => InstallerSide::Client,
                        Some("server") => InstallerSide::Server,
                        _ => return Err("neoforge_profile_processor_sides"),
                    };
                    if parsed.contains(&side) {
                        return Err("neoforge_profile_processor_sides");
                    }
                    parsed.push(side);
                }
                Some(parsed)
            }
        };
        processors.push(InstallerProcessor {
            sides,
            jar: jar.to_string(),
            classpath,
            args,
        });
    }
    Ok(processors)
}

fn parse_data(value: Option<&Value>) -> Result<BTreeMap<String, InstallerData>, &'static str> {
    let object = value
        .and_then(Value::as_object)
        .filter(|object| object.len() <= MAX_DATA_KEYS)
        .ok_or("neoforge_profile_data")?;
    let mut data = BTreeMap::new();
    for (key, value) in object {
        if key.is_empty()
            || key.len() > 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err("neoforge_profile_data_key");
        }
        let entry = if let Some(shared) = value.as_str() {
            InstallerData::Shared(validate_data_value(shared)?.to_string())
        } else if let Some(sided) = value.as_object() {
            if sided.is_empty() || sided.len() > 2 {
                return Err("neoforge_profile_data_value");
            }
            let mut client = None;
            let mut server = None;
            for (side, value) in sided {
                let value =
                    validate_data_value(value.as_str().ok_or("neoforge_profile_data_value")?)?
                        .to_string();
                match side.as_str() {
                    "client" if client.is_none() => client = Some(value),
                    "server" if server.is_none() => server = Some(value),
                    _ => return Err("neoforge_profile_data_value"),
                }
            }
            InstallerData::Sided { client, server }
        } else {
            return Err("neoforge_profile_data_value");
        };
        data.insert(key.clone(), entry);
    }
    Ok(data)
}

fn validate_data_value(value: &str) -> Result<&str, &'static str> {
    if value.is_empty() || value.len() > MAX_PROFILE_STRING || value.chars().any(char::is_control) {
        return Err("neoforge_profile_data_value");
    }
    if value.starts_with('[') || value.ends_with(']') {
        let coordinate = value
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .ok_or("neoforge_profile_data_value")?;
        maven_path(coordinate).map_err(|_| "neoforge_profile_data_value")?;
    } else if value.contains('/') || value.contains('\\') {
        let path = value
            .strip_prefix('/')
            .filter(|path| !path.starts_with('/') && !path.contains('\\'))
            .ok_or("neoforge_profile_data_value")?;
        if super::paths::normalise_content_path(path)
            .map(|normalized| normalized != path)
            .unwrap_or(true)
        {
            return Err("neoforge_profile_data_value");
        }
    }
    Ok(value)
}

fn validate_library_url(url: &str, path: &str) -> Result<(), &'static str> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "neoforge_profile_library_url")?;
    let allowed_prefix = match parsed.host_str() {
        Some("maven.neoforged.net") => "/releases/",
        Some("libraries.minecraft.net") => "/",
        _ => return Err("neoforge_profile_library_url"),
    };
    if parsed.scheme() != "https"
        || parsed.port().is_some_and(|port| port != 443)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != format!("{allowed_prefix}{path}")
    {
        return Err("neoforge_profile_library_url");
    }
    Ok(())
}

pub(super) fn maven_path(coordinate: &str) -> Result<String, &'static str> {
    let (base, extension) = coordinate
        .split_once('@')
        .map_or((coordinate, "jar"), |(base, extension)| (base, extension));
    if !matches!(extension, "jar" | "zip" | "txt") {
        return Err("neoforge_profile_coordinate");
    }
    let parts: Vec<&str> = base.split(':').collect();
    if !matches!(parts.len(), 3 | 4)
        || !valid_maven_group(parts[0])
        || !valid_maven_atom(parts[1])
        || !valid_maven_atom(parts[2])
        || (parts.len() == 4 && !valid_maven_atom(parts[3]))
    {
        return Err("neoforge_profile_coordinate");
    }
    let group_path = parts[0].replace('.', "/");
    let classifier = parts
        .get(3)
        .map_or(String::new(), |value| format!("-{value}"));
    Ok(format!(
        "{group_path}/{artifact}/{version}/{artifact}-{version}{classifier}.{extension}",
        artifact = parts[1],
        version = parts[2]
    ))
}

fn coordinate_identity(coordinate: &str) -> Result<String, &'static str> {
    maven_path(coordinate)?;
    let (base, extension) = coordinate
        .split_once('@')
        .map_or((coordinate, "jar"), |(base, extension)| (base, extension));
    Ok(format!("{base}@{extension}"))
}

fn valid_maven_group(group: &str) -> bool {
    !group.is_empty()
        && group.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
}

fn valid_maven_atom(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        && !value.contains("..")
        && !value.starts_with('.')
        && !value.ends_with('.')
}

fn valid_version_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
}

fn bounded_string<'a>(value: Option<&'a Value>, max: usize) -> Result<&'a str, &'static str> {
    value
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        })
        .ok_or("neoforge_profile_string")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile() -> Value {
        json!({
            "spec": 1,
            "profile": "NeoForge",
            "version": "neoforge-20.4.253",
            "minecraft": "1.20.4",
            "json": "/version.json",
            "data": {
                "MAPPINGS": {"client": "[net.neoforged:neoform:1.20.4-20240627.114801:mappings@txt]", "server": "[net.neoforged:neoform:1.20.4-20240627.114801:mappings@txt]"},
                "BINPATCH": {"client": "/data/client.lzma", "server": "/data/server.lzma"}
            },
            "libraries": [{
                "name": "net.neoforged.installertools:installertools:2.1.2@jar",
                "downloads": {"artifact": {
                    "path": "net/neoforged/installertools/installertools/2.1.2/installertools-2.1.2.jar",
                    "url": "https://maven.neoforged.net/releases/net/neoforged/installertools/installertools/2.1.2/installertools-2.1.2.jar",
                    "sha1": "72524c0362f812d8aa4cdb4c03e9b45e2b71ae3b",
                    "size": 83543
                }}
            }, {
                "name": "net.neoforged:neoform:1.20.4-20240627.114801@zip",
                "downloads": {"artifact": {
                    "path": "net/neoforged/neoform/1.20.4-20240627.114801/neoform-1.20.4-20240627.114801.zip",
                    "url": "https://maven.neoforged.net/releases/net/neoforged/neoform/1.20.4-20240627.114801/neoform-1.20.4-20240627.114801.zip",
                    "sha1": "1a85c9aabc4a0272b78cdca7090b806e50b41178",
                    "size": 1785114
                }}
            }],
            "processors": [{
                "sides": ["client"],
                "jar": "net.neoforged.installertools:installertools:2.1.2",
                "classpath": ["net.neoforged.installertools:installertools:2.1.2@jar"],
                "args": ["--task", "MCP_DATA", "--input", "[net.neoforged:neoform:1.20.4-20240627.114801@zip]", "--output", "{MAPPINGS}", "--key", "mappings"]
            }]
        })
    }

    #[test]
    fn parses_current_profile_shape_without_executing_processors() {
        let bytes = serde_json::to_vec(&profile()).expect("profile json");
        let parsed = parse_install_profile("1.20.4", "20.4.253", &bytes).expect("profile");
        assert_eq!(parsed.version_json_path, "version.json");
        assert_eq!(parsed.libraries.len(), 2);
        assert_eq!(parsed.processors.len(), 1);
        assert_eq!(
            parsed.processors[0].sides,
            Some(vec![InstallerSide::Client])
        );
        assert_eq!(
            parsed.processors[0].jar,
            "net.neoforged.installertools:installertools:2.1.2"
        );
        assert!(matches!(
            parsed.data.get("BINPATCH"),
            Some(InstallerData::Sided { .. })
        ));
        let assessment = assess_processor_syntax(&parsed).expect("known task assessment");
        assert_eq!(assessment.len(), 1);
        assert_eq!(assessment[0].operation, ProcessorOperation::McpData);
        assert_eq!(assessment[0].tool_coordinate, parsed.processors[0].jar);
    }

    #[test]
    fn processor_assessment_rejects_unknown_tools_tasks_and_placeholders() {
        let mut parsed = parse_install_profile(
            "1.20.4",
            "20.4.253",
            &serde_json::to_vec(&profile()).unwrap(),
        )
        .unwrap();
        parsed.processors[0].args[1] = "RUN_ARBITRARY".into();
        assert_eq!(
            assess_processor_syntax(&parsed),
            Err("neoforge_processor_task_unsupported")
        );

        let mut parsed = parse_install_profile(
            "1.20.4",
            "20.4.253",
            &serde_json::to_vec(&profile()).unwrap(),
        )
        .unwrap();
        parsed.processors[0].args.push("--task".into());
        assert_eq!(
            assess_processor_syntax(&parsed),
            Err("neoforge_processor_task_duplicate")
        );

        let mut parsed = parse_install_profile(
            "1.20.4",
            "20.4.253",
            &serde_json::to_vec(&profile()).unwrap(),
        )
        .unwrap();
        parsed.processors[0].args.push("--unknown-option".into());
        assert_eq!(
            assess_processor_syntax(&parsed),
            Err("neoforge_processor_argument_unsupported")
        );

        let mut parsed = parse_install_profile(
            "1.20.4",
            "20.4.253",
            &serde_json::to_vec(&profile()).unwrap(),
        )
        .unwrap();
        parsed.processors[0].args[3] = "[attacker:artifact:1.0@zip]".into();
        assert_eq!(
            assess_processor_syntax(&parsed),
            Err("neoforge_processor_data_library_missing")
        );

        let mut parsed = parse_install_profile(
            "1.20.4",
            "20.4.253",
            &serde_json::to_vec(&profile()).unwrap(),
        )
        .unwrap();
        parsed.processors[0].args[3] = "{UNKNOWN}/output".into();
        assert_eq!(
            assess_processor_syntax(&parsed),
            Err("neoforge_processor_placeholder_unsupported")
        );

        let mut parsed = parse_install_profile(
            "1.20.4",
            "20.4.253",
            &serde_json::to_vec(&profile()).unwrap(),
        )
        .unwrap();
        parsed.processors[0].jar = "com.attacker:tool:1.0".into();
        assert_eq!(
            assess_processor_syntax(&parsed),
            Err("neoforge_processor_tool_unsupported")
        );

        assert_eq!(
            validate_processor_placeholders(&["{ROOT}/../outside".into()]),
            Err("neoforge_processor_root_template_invalid")
        );
        assert_eq!(
            validate_processor_placeholders(&["{MINECRAFT_JAR}/suffix".into()]),
            Err("neoforge_processor_placeholder_invalid")
        );
    }

    fn captured_neoforge_profile() -> NeoForgeInstallerProfile {
        let path = std::env::var_os("SHTMCL_NEOFORGE_PROFILE")
            .expect("set SHTMCL_NEOFORGE_PROFILE to a captured install_profile.json");
        let bytes = std::fs::read(path).expect("read captured profile");
        parse_install_profile("1.20.4", "20.4.253", &bytes).expect("captured profile")
    }

    #[test]
    #[ignore = "requires SHTMCL_NEOFORGE_PROFILE to point at a captured install_profile.json"]
    fn pins_the_captured_processor_and_classpath_artifact_closure() {
        let profile = captured_neoforge_profile();
        let assessment = assess_processors(&profile).expect("pinned processor artifacts");
        assert_eq!(assessment.len(), 10);
        assert_eq!(
            super::super::neoforge_processor_allowlist::ARTIFACTS.len(),
            33
        );
        assert!(validate_processor_artifacts(&profile).is_ok());
    }

    #[test]
    #[ignore = "requires SHTMCL_NEOFORGE_PROFILE to point at a captured install_profile.json"]
    fn rejects_changed_or_unpinned_processor_artifact_identities() {
        let mut profile = captured_neoforge_profile();
        let installertools = profile
            .libraries
            .iter_mut()
            .find(|library| {
                library
                    .coordinate
                    .starts_with("net.neoforged.installertools:installertools:2.1.2")
            })
            .expect("captured installertools library");
        installertools.sha1 = "0".repeat(40);
        assert_eq!(
            assess_processors(&profile),
            Err("neoforge_processor_artifact_mismatch")
        );

        let mut profile = captured_neoforge_profile();
        profile.processors[0]
            .classpath
            .push("net.neoforged:neoform:1.20.4-20240627.114801@zip".into());
        assert_eq!(
            assess_processors(&profile),
            Err("neoforge_processor_artifact_unpinned")
        );

        let mut profile = captured_neoforge_profile();
        profile.loader_version = "20.4.254".into();
        assert_eq!(
            assess_processors(&profile),
            Err("neoforge_processor_artifact_version_unsupported")
        );
    }

    #[test]
    fn validates_child_metadata_and_keeps_parent_inheritance() {
        let metadata = json!({
            "id": "neoforge-20.4.253",
            "inheritsFrom": "1.20.4",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "libraries": [{"name": "net.neoforged:neoforge:20.4.253"}],
            "arguments": {"game": ["--launchTarget", "forgeserver"], "jvm": ["-Dexample=true"]}
        });
        let bytes = serde_json::to_vec(&metadata).unwrap();
        let parsed = parse_version_metadata(&bytes, "1.20.4", "20.4.253").unwrap();
        assert_eq!(parsed["inheritsFrom"], "1.20.4");
        assert_eq!(parsed["id"], "neoforge-20.4.253");

        let mut mismatched = metadata;
        mismatched["inheritsFrom"] = json!("1.20.1");
        assert_eq!(
            parse_version_metadata(
                &serde_json::to_vec(&mismatched).unwrap(),
                "1.20.4",
                "20.4.253"
            ),
            Err("neoforge_version_mismatch")
        );
    }

    #[test]
    fn rejects_version_metadata_with_unbounded_or_unexpected_arguments() {
        let mut metadata = json!({
            "id": "neoforge-20.4.253",
            "inheritsFrom": "1.20.4",
            "mainClass": "cpw.mods.bootstraplauncher.BootstrapLauncher",
            "libraries": [],
            "arguments": {"game": [{"value": "--malformed", "extra": true}]}
        });
        assert_eq!(
            parse_version_metadata(
                &serde_json::to_vec(&metadata).unwrap(),
                "1.20.4",
                "20.4.253"
            ),
            Err("neoforge_version_argument")
        );
        metadata["mainClass"] = json!("attacker.Main");
        assert_eq!(
            parse_version_metadata(
                &serde_json::to_vec(&metadata).unwrap(),
                "1.20.4",
                "20.4.253"
            ),
            Err("neoforge_version_main_class")
        );
    }

    #[test]
    fn rejects_identity_mismatch_and_unsafe_download_paths() {
        let mut value = profile();
        value["minecraft"] = json!("1.20.1");
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &serde_json::to_vec(&value).unwrap()),
            Err("neoforge_profile_mismatch")
        );

        let mut value = profile();
        value["libraries"][0]["downloads"]["artifact"]["path"] = json!("../../outside.jar");
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &serde_json::to_vec(&value).unwrap()),
            Err("neoforge_profile_library_path")
        );
    }

    #[test]
    fn rejects_unpinned_or_unbound_processors() {
        let mut value = profile();
        value["processors"][0]["classpath"] = json!(["com.attacker:tool:1.0@jar"]);
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &serde_json::to_vec(&value).unwrap()),
            Err("neoforge_profile_processor_library_missing")
        );

        let mut value = profile();
        value["processors"][0]["jar"] = json!("com.attacker:tool:1.0");
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &serde_json::to_vec(&value).unwrap()),
            Err("neoforge_profile_processor_jar_missing")
        );
    }

    #[test]
    fn rejects_conflicting_duplicate_libraries_and_traversal_data() {
        let mut value = profile();
        let mut duplicate = value["libraries"][0].clone();
        duplicate["downloads"]["artifact"]["sha1"] =
            json!("0000000000000000000000000000000000000000");
        value["libraries"]
            .as_array_mut()
            .expect("libraries")
            .push(duplicate);
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &serde_json::to_vec(&value).unwrap()),
            Err("neoforge_profile_library_conflict")
        );

        let mut value = profile();
        value["data"]["BINPATCH"]["client"] = json!("/../../outside.lzma");
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &serde_json::to_vec(&value).unwrap()),
            Err("neoforge_profile_data_value")
        );
    }

    #[test]
    fn bounds_declared_installer_library_download_size() {
        let mut value = profile();
        value["libraries"][0]["downloads"]["artifact"]["size"] = json!(MAX_ARTIFACT_BYTES + 1);
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &serde_json::to_vec(&value).unwrap()),
            Err("neoforge_profile_library_size")
        );
    }

    #[test]
    fn rejects_oversized_and_non_maven_coordinates() {
        assert_eq!(
            parse_install_profile("1.20.4", "20.4.253", &vec![b' '; MAX_PROFILE_BYTES + 1]),
            Err("neoforge_profile_too_large")
        );
        assert_eq!(
            maven_path("../evil:tool:1.0@jar"),
            Err("neoforge_profile_coordinate")
        );
        assert_eq!(
            maven_path("com.example:tool:1.0@exe"),
            Err("neoforge_profile_coordinate")
        );
    }

    #[test]
    #[ignore = "requires SHTMCL_NEOFORGE_PROFILE to point at a captured installer profile"]
    fn parses_captured_neoforge_installer_profile() {
        let path = std::env::var_os("SHTMCL_NEOFORGE_PROFILE")
            .expect("set SHTMCL_NEOFORGE_PROFILE to install_profile.json");
        let bytes = std::fs::read(path).expect("read captured installer profile");
        let parsed = parse_install_profile("1.20.4", "20.4.253", &bytes).expect("profile");
        assert_eq!(parsed.version_json_path, "version.json");
        assert!(!parsed.libraries.is_empty());
        assert!(parsed.libraries.len() < 188);
        assert_eq!(parsed.processors.len(), 10);
        assert!(parsed.data.contains_key("BINPATCH"));
    }

    #[test]
    #[ignore = "requires SHTMCL_NEOFORGE_INSTALLER to point at the captured installer jar"]
    fn reads_captured_neoforge_installer_archive() {
        let path = std::env::var_os("SHTMCL_NEOFORGE_INSTALLER")
            .expect("set SHTMCL_NEOFORGE_INSTALLER to an installer jar");
        let bytes = std::fs::read(path).expect("read captured installer");
        let parsed = read_installer_archive("1.20.4", "20.4.253", &bytes)
            .expect("read bounded installer entries");
        assert_eq!(parsed.profile.version_json_path, "version.json");
        assert_eq!(parsed.version_metadata["id"], "neoforge-20.4.253");
        assert_eq!(parsed.version_metadata["inheritsFrom"], "1.20.4");
        assert_eq!(
            parsed.version_metadata["mainClass"],
            "cpw.mods.bootstraplauncher.BootstrapLauncher"
        );
        assert_eq!(parsed.resources.len(), 2);
        assert!(parsed
            .resources
            .iter()
            .all(|resource| resource.sha256.len() == 64));
        let processors = assess_processors(&parsed.profile).expect("classify processors");
        assert_eq!(processors.len(), 10);
        assert!(processors
            .iter()
            .any(|item| item.operation == ProcessorOperation::McpData));
    }

    #[test]
    #[ignore = "requires captured NeoForge installer and vanilla metadata paths"]
    fn merges_captured_child_with_vanilla_metadata() {
        use crate::launch::catalog::{VersionMetadata, VersionMetadataSource};

        struct MetadataSource(BTreeMap<String, Value>);
        impl VersionMetadataSource for MetadataSource {
            fn load(&self, id: &str) -> Result<Option<Value>, String> {
                Ok(self.0.get(id).cloned())
            }
        }

        let installer_path = std::env::var_os("SHTMCL_NEOFORGE_INSTALLER")
            .expect("set SHTMCL_NEOFORGE_INSTALLER to an installer jar");
        let base_path = std::env::var_os("SHTMCL_NEOFORGE_BASE_METADATA")
            .expect("set SHTMCL_NEOFORGE_BASE_METADATA to vanilla version metadata");
        let installer = std::fs::read(installer_path).expect("read captured installer");
        let base: Value =
            serde_json::from_slice(&std::fs::read(base_path).expect("read vanilla metadata"))
                .expect("parse vanilla metadata");
        let parsed = read_installer_archive("1.20.4", "20.4.253", &installer)
            .expect("parse installer metadata");
        let child_id = parsed.version_metadata["id"].as_str().unwrap().to_string();
        let source = MetadataSource(BTreeMap::from([
            ("1.20.4".to_string(), base),
            (child_id.clone(), parsed.version_metadata),
        ]));
        let resolved = VersionMetadata::resolve(&source, &child_id).expect("resolve child");
        assert_eq!(resolved.metadata.id, child_id);
        assert_eq!(resolved.metadata.inherits_from, None);
        assert_eq!(
            resolved.metadata.main_class.as_deref(),
            Some("cpw.mods.bootstraplauncher.BootstrapLauncher")
        );
        assert_eq!(
            resolved.inheritance_chain,
            vec![child_id, "1.20.4".to_string()]
        );
        assert!(resolved.metadata.asset_index.is_some());
        assert!(resolved.metadata.libraries.len() > 49);
    }
}
