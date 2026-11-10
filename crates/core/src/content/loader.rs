//! Loader install descriptors and install-plan generation.
//!
//! This module never reaches the network: the version matrix
//! (Minecraft version × loader version → libraries, main class, JVM args) arrives
//! through the [`LoaderCatalog`] port. The concrete catalog is supplied by the
//! application layer (and by 02 for runtime constraints); this build ships
//! `FixtureLoaderCatalog` for tests and offline behaviour.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::error::ContentError;
use super::version::{Version, VersionRange};

/// Everything a launcher needs to start one loader for one Minecraft version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LoaderKind {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    NeoForge,
}

impl LoaderKind {
    pub const ALL: [Self; 5] = [
        Self::Vanilla,
        Self::Fabric,
        Self::Quilt,
        Self::Forge,
        Self::NeoForge,
    ];

    pub fn parse(value: &str) -> Result<Self, ContentError> {
        let normalised = value.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        match normalised.as_str() {
            "vanilla" | "official" => Ok(Self::Vanilla),
            "fabric" => Ok(Self::Fabric),
            "quilt" => Ok(Self::Quilt),
            "forge" => Ok(Self::Forge),
            "neoforge" | "neo-forge" => Ok(Self::NeoForge),
            _ => Err(ContentError::UnsupportedLoader {
                loader: value.to_string(),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Fabric => "fabric",
            Self::Quilt => "quilt",
            Self::Forge => "forge",
            Self::NeoForge => "neoforge",
        }
    }

    /// Vanilla has no loader library set; the plan still describes the main class
    /// so 02 can build one launch plan shape for every case.
    pub fn needs_loader_version(self) -> bool {
        !matches!(self, Self::Vanilla)
    }

    pub fn vanilla_main_class() -> &'static str {
        "net.minecraft.client.main.Main"
    }
}

/// The install target a content lock or pack is resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameTarget {
    pub game_version: String,
    pub loader: LoaderKind,
    pub loader_version: Option<String>,
}

impl GameTarget {
    pub fn new(game_version: impl Into<String>, loader: LoaderKind) -> Self {
        Self {
            game_version: game_version.into(),
            loader,
            loader_version: None,
        }
    }

    pub fn with_loader_version(mut self, version: impl Into<String>) -> Self {
        self.loader_version = Some(version.into());
        self
    }

    pub fn to_value(&self) -> Value {
        json!({
            "gameVersion": self.game_version,
            "loader": self.loader.as_str(),
            "loaderVersion": self.loader_version,
        })
    }

    pub fn from_value(value: &Value) -> Result<Self, ContentError> {
        let object = value
            .as_object()
            .ok_or_else(|| ContentError::InvalidManifest {
                detail: "target must be an object".to_string(),
            })?;
        let game_version = object
            .get("gameVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| ContentError::InvalidManifest {
                detail: "target.gameVersion is required".to_string(),
            })?;
        let loader = match object.get("loader").and_then(Value::as_str) {
            Some(value) => LoaderKind::parse(value)?,
            None => LoaderKind::Vanilla,
        };
        let loader_version = object
            .get("loaderVersion")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Self {
            game_version: game_version.to_string(),
            loader,
            loader_version,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderLibrary {
    pub group: String,
    pub artifact: String,
    pub version: String,
    pub url: Option<String>,
}

impl LoaderLibrary {
    pub fn new(
        group: impl Into<String>,
        artifact: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            group: group.into(),
            artifact: artifact.into(),
            version: version.into(),
            url: None,
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Dedupe key for a Maven coordinate; the version is what may differ.
    pub fn coordinate(&self) -> String {
        format!("{}:{}", self.group, self.artifact)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "coordinate": self.coordinate(),
            "version": self.version,
            "url": self.url,
        })
    }
}

/// One published loader version as reported by the catalog port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderVersionInfo {
    pub version: Version,
    pub stable: bool,
    /// Minecraft versions this loader version supports; `*` means "all".
    pub game_versions: Vec<String>,
    pub main_class: String,
    pub libraries: Vec<LoaderLibrary>,
    pub jvm_args: Vec<String>,
    pub required_java_major: Option<u32>,
}

impl LoaderVersionInfo {
    pub fn supports_game(&self, game_version: &str) -> bool {
        self.game_versions
            .iter()
            .any(|supported| supported == game_version || supported == "*")
    }
}

/// Metadata port. Implementations fetch or cache the matrix; 04 only selects.
pub trait LoaderCatalog: Send + Sync {
    fn loader_versions(
        &self,
        kind: LoaderKind,
        game_version: &str,
    ) -> Result<Vec<LoaderVersionInfo>, ContentError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderRequest {
    pub kind: LoaderKind,
    pub game_version: String,
    /// `None` means "pick the best published version for this game version".
    pub selector: Option<VersionRange>,
}

impl LoaderRequest {
    pub fn new(kind: LoaderKind, game_version: impl Into<String>) -> Self {
        Self {
            kind,
            game_version: game_version.into(),
            selector: None,
        }
    }

    pub fn with_selector(mut self, selector: VersionRange) -> Self {
        self.selector = Some(selector);
        self
    }
}

/// The generated install plan: which loader build, which main class, which
/// libraries (deduplicated and deterministically ordered) and which JVM args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderInstallPlan {
    pub loader: LoaderKind,
    pub game_version: String,
    pub loader_version: Option<String>,
    pub main_class: String,
    pub libraries: Vec<LoaderLibrary>,
    pub jvm_args: Vec<String>,
    pub required_java_major: Option<u32>,
    pub merged_libraries: u32,
    pub metadata_source: String,
}

impl LoaderInstallPlan {
    pub fn target(&self) -> GameTarget {
        GameTarget {
            game_version: self.game_version.clone(),
            loader: self.loader,
            loader_version: self.loader_version.clone(),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "loader": self.loader.as_str(),
            "gameVersion": self.game_version,
            "loaderVersion": self.loader_version,
            "mainClass": self.main_class,
            "libraries": self.libraries.iter().map(LoaderLibrary::to_value).collect::<Vec<_>>(),
            "jvmArgs": self.jvm_args,
            "requiredJavaMajor": self.required_java_major,
            "mergedLibraries": self.merged_libraries,
            "metadataSource": self.metadata_source,
        })
    }
}

/// Build a loader install plan from catalog metadata.
///
/// Determinism: candidate selection prefers `stable` builds and then the highest
/// version; libraries are merged by coordinate (highest version wins) and sorted.
pub fn plan_loader_install(
    request: &LoaderRequest,
    catalog: &dyn LoaderCatalog,
) -> Result<LoaderInstallPlan, ContentError> {
    if !request.kind.needs_loader_version() {
        return Ok(LoaderInstallPlan {
            loader: request.kind,
            game_version: request.game_version.clone(),
            loader_version: None,
            main_class: LoaderKind::vanilla_main_class().to_string(),
            libraries: Vec::new(),
            jvm_args: Vec::new(),
            required_java_major: None,
            merged_libraries: 0,
            metadata_source: "builtin".to_string(),
        });
    }

    let published = catalog.loader_versions(request.kind, &request.game_version)?;
    let mut candidates: Vec<LoaderVersionInfo> = published
        .into_iter()
        .filter(|info| info.supports_game(&request.game_version))
        .filter(|info| match &request.selector {
            None => true,
            Some(selector) => selector.satisfies(&info.version),
        })
        .collect();

    if candidates.is_empty() {
        return Err(ContentError::IncompatibleLoader {
            detail: format!(
                "{} has no version for Minecraft {}{}",
                request.kind.as_str(),
                request.game_version,
                match &request.selector {
                    Some(selector) => format!(" matching {selector}"),
                    None => String::new(),
                }
            ),
        });
    }

    candidates.sort_by(|left, right| {
        right
            .stable
            .cmp(&left.stable)
            .then_with(|| right.version.cmp(&left.version))
    });
    let chosen = candidates.remove(0);

    if chosen.main_class.trim().is_empty() {
        return Err(ContentError::InvalidLoaderMetadata {
            detail: format!(
                "{} {} has no main class",
                request.kind.as_str(),
                chosen.version
            ),
        });
    }

    let mut merged: BTreeMap<String, LoaderLibrary> = BTreeMap::new();
    let mut duplicates = 0u32;
    for library in chosen.libraries {
        match merged.get(&library.coordinate()) {
            Some(existing) => {
                duplicates += 1;
                let replace = match (
                    Version::parse(&library.version),
                    Version::parse(&existing.version),
                ) {
                    (Ok(candidate), Ok(current)) => candidate > current,
                    _ => library.version > existing.version,
                };
                if replace {
                    merged.insert(library.coordinate(), library);
                }
            }
            None => {
                merged.insert(library.coordinate(), library);
            }
        }
    }

    let mut jvm_args: Vec<String> = Vec::new();
    for argument in chosen.jvm_args {
        if !jvm_args.contains(&argument) {
            jvm_args.push(argument);
        }
    }

    Ok(LoaderInstallPlan {
        loader: request.kind,
        game_version: request.game_version.clone(),
        loader_version: Some(chosen.version.to_string()),
        main_class: chosen.main_class,
        libraries: merged.into_values().collect(),
        jvm_args,
        required_java_major: chosen.required_java_major,
        merged_libraries: duplicates,
        metadata_source: "loader-catalog-port".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OnlyForge;

    impl LoaderCatalog for OnlyForge {
        fn loader_versions(
            &self,
            _kind: LoaderKind,
            _game_version: &str,
        ) -> Result<Vec<LoaderVersionInfo>, ContentError> {
            Ok(vec![LoaderVersionInfo {
                version: Version::parse("47.2.0").expect("version"),
                stable: true,
                game_versions: vec!["1.20.1".to_string()],
                main_class: "cpw.mods.bootstraplauncher.BootstrapLauncher".to_string(),
                libraries: vec![
                    LoaderLibrary::new("cpw.mods", "securejarhandler", "2.1.10"),
                    LoaderLibrary::new("cpw.mods", "securejarhandler", "2.1.6"),
                ],
                jvm_args: vec![
                    "-Djava.net.preferIPv4Stack=true".to_string(),
                    "-Djava.net.preferIPv4Stack=true".to_string(),
                ],
                required_java_major: Some(17),
            }])
        }
    }

    #[test]
    fn vanilla_plan_needs_no_catalog_and_no_libraries() {
        let plan = plan_loader_install(
            &LoaderRequest::new(LoaderKind::Vanilla, "1.20.1"),
            &OnlyForge,
        )
        .expect("vanilla plan");
        assert_eq!(plan.main_class, LoaderKind::vanilla_main_class());
        assert!(plan.libraries.is_empty());
        assert_eq!(plan.loader_version, None);
        assert_eq!(plan.metadata_source, "builtin");
    }

    #[test]
    fn libraries_merge_by_coordinate_and_args_dedupe() {
        let plan =
            plan_loader_install(&LoaderRequest::new(LoaderKind::Forge, "1.20.1"), &OnlyForge)
                .expect("forge plan");
        assert_eq!(plan.loader_version.as_deref(), Some("47.2.0"));
        assert_eq!(plan.libraries.len(), 1);
        assert_eq!(plan.libraries[0].version, "2.1.10");
        assert_eq!(plan.merged_libraries, 1);
        assert_eq!(plan.jvm_args.len(), 1);
        assert_eq!(plan.required_java_major, Some(17));
    }

    #[test]
    fn unsupported_game_version_is_reported_as_incompatible() {
        let error =
            plan_loader_install(&LoaderRequest::new(LoaderKind::Forge, "1.19.2"), &OnlyForge)
                .expect_err("no 1.19.2 forge build");
        assert_eq!(
            error.code(),
            super::super::error::codes::LOADER_INCOMPATIBLE
        );
    }

    #[test]
    fn unknown_loader_name_is_unsupported() {
        let error = LoaderKind::parse("liteloader").expect_err("unsupported loader");
        assert_eq!(error.code(), super::super::error::codes::LOADER_UNSUPPORTED);
    }
}
