//! Pure, side-effect-free planning of NeoForge client processor staging.
//!
//! This module only *describes* what a later, separately reviewed install flow
//! would do. Nothing here reads files, executes tools, writes output or touches
//! the network. Returned paths are validated normalised relative
//! content paths and are **not** authorization to execute anything.

use std::collections::BTreeMap;

use super::error::ContentError;
use super::neoforge_profile::maven_path;
use super::neoforge_profile::{
    assess_processors, InstallerData, InstallerProcessor, InstallerSide, NeoForgeInstallerProfile,
    ProcessorOperation,
};
use super::paths::normalise_content_path;

/// Fixed stage-relative installer input (no absolute root is ever supplied).
pub const INSTALLER_INPUT: &str = "inputs/installer.jar";
/// Fixed stage-relative Minecraft input jar.
pub const MINECRAFT_INPUT: &str = "inputs/minecraft.jar";

const SIDE_VALUE: &str = "client";

/// One fully resolved processor argument. Path-like values are simulated
/// staging paths (relative to the staging root); literals are passed through
/// verbatim, e.g. task names and flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingArgument {
    Literal(String),
    StagedPath(String),
    StagingRoot,
}

/// A resolved client processor step. `index` is the original processor index
/// in the installer profile, preserved for deterministic ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingStep {
    pub index: usize,
    pub operation: ProcessorOperation,
    pub tool_path: String,
    pub classpath: Vec<String>,
    pub args: Vec<StagingArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingPlanError {
    Assessment,
    Path(ContentError),
    UnknownData(String),
    MissingClientData(String),
    MalformedToken(String),
    UnsupportedDataValue(String),
    Maven,
}

impl std::fmt::Display for StagingPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Assessment => write!(f, "processor assessment rejected the profile"),
            Self::Path(e) => write!(f, "unsafe staging path: {e}"),
            Self::UnknownData(k) => write!(f, "unknown data placeholder {{{k}}}"),
            Self::MissingClientData(k) => write!(f, "data {k} has no client value"),
            Self::MalformedToken(t) => write!(f, "malformed placeholder token {t}"),
            Self::UnsupportedDataValue(v) => write!(f, "unsupported data value {v}"),
            Self::Maven => write!(f, "invalid maven coordinate"),
        }
    }
}

impl std::error::Error for StagingPlanError {}

/// Build the deterministic client staging plan for one installer profile.
/// Assessment runs before any resolution so unsupported syntax is refused.
pub fn plan_client_staging(
    profile: &NeoForgeInstallerProfile,
) -> Result<Vec<StagingStep>, StagingPlanError> {
    let assessments = assess_processors(profile).map_err(|_| StagingPlanError::Assessment)?;
    let by_index: BTreeMap<usize, ProcessorOperation> = assessments
        .into_iter()
        .map(|a| (a.index, a.operation))
        .collect();

    let mut steps = Vec::new();
    for (index, processor) in profile.processors.iter().enumerate() {
        if !is_client_processor(processor) {
            continue;
        }
        let operation = *by_index.get(&index).ok_or(StagingPlanError::Assessment)?;
        steps.push(resolve_step(profile, index, processor, operation)?);
    }
    Ok(steps)
}

fn is_client_processor(processor: &InstallerProcessor) -> bool {
    match &processor.sides {
        None => true,
        Some(sides) => sides.iter().any(|s| *s == InstallerSide::Client),
    }
}

fn resolve_step(
    profile: &NeoForgeInstallerProfile,
    index: usize,
    processor: &InstallerProcessor,
    operation: ProcessorOperation,
) -> Result<StagingStep, StagingPlanError> {
    let tool_path = coordinate_to_path(&processor.jar)?;

    let mut classpath = vec![tool_path.clone()];
    for entry in &processor.classpath {
        let path = coordinate_to_path(entry)?;
        if !classpath.contains(&path) {
            classpath.push(path);
        }
    }

    let mut args = Vec::with_capacity(processor.args.len());
    for raw in &processor.args {
        args.push(resolve_argument(profile, raw)?);
    }

    Ok(StagingStep {
        index,
        operation,
        tool_path,
        classpath,
        args,
    })
}

fn coordinate_to_path(coordinate: &str) -> Result<String, StagingPlanError> {
    let raw = format!(
        "libraries/{}",
        maven_path(coordinate).map_err(|_| StagingPlanError::Maven)?
    );
    safe_path(&raw)
}

fn safe_path(raw: &str) -> Result<String, StagingPlanError> {
    let normalised = normalise_content_path(raw).map_err(StagingPlanError::Path)?;
    if normalised != raw {
        return Err(StagingPlanError::Path(ContentError::PathEscape {
            path: raw.to_string(),
        }));
    }
    Ok(normalised)
}

fn resolve_argument(
    profile: &NeoForgeInstallerProfile,
    raw: &str,
) -> Result<StagingArgument, StagingPlanError> {
    if let Some(rest) = raw.strip_prefix('{') {
        return resolve_placeholder(profile, raw, rest);
    }
    if let Some(inner) = bracket_inner(raw) {
        return coordinate_to_path(inner).map(StagingArgument::StagedPath);
    }
    Ok(StagingArgument::Literal(raw.to_string()))
}

fn resolve_placeholder(
    profile: &NeoForgeInstallerProfile,
    raw: &str,
    after_open: &str,
) -> Result<StagingArgument, StagingPlanError> {
    let close = after_open
        .find('}')
        .ok_or_else(|| StagingPlanError::MalformedToken(raw.to_string()))?;
    let key = &after_open[..close];
    let suffix = &after_open[close + 1..];
    if key.is_empty() || key.contains('{') || key.contains('}') {
        return Err(StagingPlanError::MalformedToken(raw.to_string()));
    }

    match key {
        "SIDE" => {
            if suffix.is_empty() {
                Ok(StagingArgument::Literal(SIDE_VALUE.to_string()))
            } else {
                Err(StagingPlanError::MalformedToken(raw.to_string()))
            }
        }
        "ROOT" => {
            let rel = suffix.strip_prefix('/').unwrap_or(suffix);
            if suffix.is_empty() {
                // {ROOT} alone denotes the staging root; no relative path.
                return Ok(StagingArgument::StagingRoot);
            }
            if !suffix.starts_with('/') {
                return Err(StagingPlanError::MalformedToken(raw.to_string()));
            }
            safe_path(rel).map(StagingArgument::StagedPath)
        }
        "INSTALLER" => single_path(raw, suffix, INSTALLER_INPUT),
        "MINECRAFT_JAR" => single_path(raw, suffix, MINECRAFT_INPUT),
        name => {
            if !suffix.is_empty() {
                return Err(StagingPlanError::MalformedToken(raw.to_string()));
            }
            let value = client_data(profile, name)?;
            resolve_data_value(raw, &value)
        }
    }
}

fn single_path(raw: &str, suffix: &str, fixed: &str) -> Result<StagingArgument, StagingPlanError> {
    if !suffix.is_empty() {
        return Err(StagingPlanError::MalformedToken(raw.to_string()));
    }
    safe_path(fixed).map(StagingArgument::StagedPath)
}

fn client_data(profile: &NeoForgeInstallerProfile, key: &str) -> Result<String, StagingPlanError> {
    let Some(data) = profile.data.get(key) else {
        return Err(StagingPlanError::UnknownData(key.to_string()));
    };
    let value = match data {
        InstallerData::Shared(v) => Some(v.clone()),
        InstallerData::Sided { client, .. } => client.clone(),
    };
    value.ok_or_else(|| StagingPlanError::MissingClientData(key.to_string()))
}

fn resolve_data_value(raw: &str, value: &str) -> Result<StagingArgument, StagingPlanError> {
    if let Some(inner) = bracket_inner(value) {
        return coordinate_to_path(inner).map(StagingArgument::StagedPath);
    }
    if let Some(rest) = value.strip_prefix("/data/") {
        let rel = format!("installer-resources/data/{rest}");
        return safe_path(&rel).map(StagingArgument::StagedPath);
    }
    Err(StagingPlanError::UnsupportedDataValue(raw.to_string()))
}

fn bracket_inner(value: &str) -> Option<&str> {
    let v = value;
    if v.len() >= 2 && v.starts_with('[') && v.ends_with(']') {
        Some(&v[1..v.len() - 1])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::neoforge_profile::parse_install_profile;
    use super::*;
    fn profile() -> NeoForgeInstallerProfile {
        parse_install_profile(
            "1.20.4",
            "20.4.253",
            include_bytes!("fixtures/neoforge-plan-20.4.253.json"),
        )
        .unwrap()
    }
    #[test]
    fn captured_client_order_arguments_and_classpath() {
        let p = profile();
        let plan = plan_client_staging(&p).unwrap();
        assert_eq!(
            plan.iter().map(|s| s.index).collect::<Vec<_>>(),
            [3, 4, 5, 6, 8, 9]
        );
        assert_eq!(plan, plan_client_staging(&p).unwrap());
        for step in &plan {
            assert_eq!(step.classpath.first(), Some(&step.tool_path));
            let set: std::collections::BTreeSet<_> = step.classpath.iter().collect();
            assert_eq!(set.len(), step.classpath.len());
            for path in &step.classpath {
                assert!(path.starts_with("libraries/"));
                assert_eq!(safe_path(path).unwrap(), *path);
            }
            for arg in &step.args {
                match arg {
                    StagingArgument::Literal(v) => assert!(!v.contains('{') && !v.contains('}')),
                    StagingArgument::StagedPath(v) => assert_eq!(safe_path(v).unwrap(), *v),
                    StagingArgument::StagingRoot => (),
                }
            }
        }
        assert!(plan[1]
            .args
            .contains(&StagingArgument::Literal("client".into())));
        assert!(plan[3]
            .args
            .contains(&StagingArgument::StagedPath(MINECRAFT_INPUT.into())));
        assert!(plan[5].args.contains(&StagingArgument::StagedPath(
            "installer-resources/data/client.lzma".into()
        )));
        assert_eq!(plan[0].args[5], StagingArgument::StagedPath("libraries/net/neoforged/neoform/1.20.4-20240627.114801/neoform-1.20.4-20240627.114801-mappings.txt".into()));
    }
    #[test]
    fn unsafe_paths_and_tokens_rejected() {
        let p = profile();
        for raw in ["../x", "/x", "C:/x", "a//b", "a/./b", "a/CON.txt", "a\\b"] {
            assert!(safe_path(raw).is_err(), "{raw}");
        }
        for raw in [
            "{ROOT}/../x",
            "{ROOT}//x",
            "{ROOT}x",
            "{ROOT}/a/./b",
            "{SIDE}/x",
            "{NOPE}",
            "{SIDE",
        ] {
            assert!(resolve_argument(&p, raw).is_err(), "{raw}");
        }
        assert_eq!(
            resolve_argument(&p, "{ROOT}").unwrap(),
            StagingArgument::StagingRoot
        );
        assert_eq!(
            resolve_argument(&p, "{ROOT}/libraries/a.jar").unwrap(),
            StagingArgument::StagedPath("libraries/a.jar".into())
        );
    }
    #[test]
    fn rejects_missing_client_data_and_mutated_tool() {
        let mut p = profile();
        p.data.insert(
            "BINPATCH".into(),
            InstallerData::Sided {
                client: None,
                server: Some("/data/server.lzma".into()),
            },
        );
        assert!(matches!(
            plan_client_staging(&p),
            Err(StagingPlanError::MissingClientData(_))
        ));
        let mut p = profile();
        p.processors[3].jar = "unknown:tool:1".into();
        assert!(matches!(
            plan_client_staging(&p),
            Err(StagingPlanError::Assessment)
        ));
    }
    #[test]
    fn rejects_invalid_data_bindings_and_unsupported_profiles() {
        for value in [
            "/data/../outside",
            "[bad:coordinate]",
            "C:/outside",
            "\\outside",
            "'../../outside'",
        ] {
            let mut p = profile();
            p.data
                .insert("BINPATCH".into(), InstallerData::Shared(value.into()));
            assert!(plan_client_staging(&p).is_err(), "{value}");
        }
        let mut p = profile();
        p.game_version = "1.21".into();
        assert!(matches!(
            plan_client_staging(&p),
            Err(StagingPlanError::Assessment)
        ));
        let mut p = profile();
        p.processors[3].args.push("{UNKNOWN}".into());
        assert!(matches!(
            plan_client_staging(&p),
            Err(StagingPlanError::Assessment)
        ));
    }
    #[test]
    #[ignore = "requires SHTMCL_NEOFORGE_PROFILE"]
    fn external_captured_profile() {
        let bytes = std::fs::read(std::env::var("SHTMCL_NEOFORGE_PROFILE").unwrap()).unwrap();
        let p = parse_install_profile("1.20.4", "20.4.253", &bytes).unwrap();
        assert_eq!(
            plan_client_staging(&p).unwrap(),
            plan_client_staging(&profile()).unwrap()
        );
    }
}
