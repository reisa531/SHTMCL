//! `.mrpack` (Modrinth) manifest parsing, override semantics and the export side.

use serde_json::{Map, Value, json};

use super::error::ContentError;
use super::loader::{GameTarget, LoaderKind};
use super::paths::{
    MAX_MANIFEST_BYTES, collision_key, is_excluded_from_export, normalise_content_path,
};
use super::zipindex::{ZipBudget, ZipIndex, read_entry, read_index, verify_entry};

pub const MODRINTH_INDEX: &str = "modrinth.index.json";
pub const MRPACK_EXTENSION: &str = "mrpack";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackSide {
    Client,
    Server,
}

impl PackSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

/// Modrinth `env` value for one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideSupport {
    Required,
    Optional,
    Unsupported,
}

impl SideSupport {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "required" => Some(Self::Required),
            "optional" => Some(Self::Optional),
            "unsupported" => Some(Self::Unsupported),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
            Self::Unsupported => "unsupported",
        }
    }

    fn wanted(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackFormat {
    Modrinth,
    /// Export-only format defined here; import support is out of scope this round.
    ShtmclV1,
}

impl PackFormat {
    pub fn parse(value: &str) -> Result<Self, ContentError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mrpack" | "modrinth" | "modrinth-modpack" => Ok(Self::Modrinth),
            "shtmcl.v1" | "shtmcl-pack-v1" => Ok(Self::ShtmclV1),
            other => Err(ContentError::UnsupportedFormat {
                format: other.to_string(),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Modrinth => "mrpack",
            Self::ShtmclV1 => "shtmcl.v1",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hashes {
    pub sha1: Option<String>,
    pub sha512: Option<String>,
    /// Not part of the Modrinth schema: 04 computes it for files that come from the
    /// archive itself (overrides), which carry no manifest digest.
    pub sha256: Option<String>,
}

impl Hashes {
    fn is_empty(&self) -> bool {
        self.sha1.is_none() && self.sha512.is_none() && self.sha256.is_none()
    }

    /// Identity key used for artifact dedupe: prefer sha512, then sha256, then sha1.
    pub fn identity_key(&self) -> Option<String> {
        if let Some(value) = &self.sha512 {
            return Some(format!("sha512:{value}"));
        }
        if let Some(value) = &self.sha256 {
            return Some(format!("sha256:{value}"));
        }
        self.sha1.as_ref().map(|value| format!("sha1:{value}"))
    }

    fn to_value(&self) -> Value {
        let mut object = Map::new();
        if let Some(value) = &self.sha1 {
            object.insert("sha1".to_string(), json!(value));
        }
        if let Some(value) = &self.sha512 {
            object.insert("sha512".to_string(), json!(value));
        }
        if let Some(value) = &self.sha256 {
            object.insert("sha256".to_string(), json!(value));
        }
        Value::Object(object)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// Normalised `/`-separated path relative to the instance root.
    pub path: String,
    pub hashes: Hashes,
    pub downloads: Vec<String>,
    pub file_size: Option<u64>,
    pub client: SideSupport,
    pub server: SideSupport,
}

impl ManifestFile {
    /// A file with no download URL needs a user-supplied copy (restricted file).
    pub fn is_restricted(&self) -> bool {
        self.downloads.is_empty()
    }
}

/// Where an override entry goes: `overrides/` applies to both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideScope {
    Shared,
    Client,
    Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideEntry {
    pub scope: OverrideScope,
    /// Archive path (`overrides/config/a.json`).
    pub archive_path: String,
    /// Destination path inside the instance root (`config/a.json`).
    pub target_path: String,
    /// Verified SHA-256 and size of the archive payload. `None` only when the entry
    /// was classified without reading the archive (unit-test helper).
    pub sha256: Option<String>,
    pub size: Option<u64>,
}

impl OverrideEntry {
    /// Identity of the payload this override will install.
    pub fn hashes(&self) -> Hashes {
        Hashes {
            sha1: None,
            sha512: None,
            sha256: self.sha256.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackManifest {
    pub format: PackFormat,
    pub name: String,
    pub version_id: String,
    pub summary: Option<String>,
    pub game_version: Option<String>,
    pub loader: Option<LoaderKind>,
    pub loader_version: Option<String>,
    pub files: Vec<ManifestFile>,
    pub overrides: Vec<OverrideEntry>,
    /// Files referenced by the manifest that carry no `downloads` entry.
    pub restricted_count: u32,
}

impl PackManifest {
    pub fn target(&self) -> Option<GameTarget> {
        let game_version = self.game_version.clone()?;
        let loader = self.loader.unwrap_or(LoaderKind::Vanilla);
        let mut target = GameTarget::new(game_version, loader);
        target.loader_version = self.loader_version.clone();
        Some(target)
    }

    /// Map the manifest to installation targets for one side. Overrides come from
    /// the archive and are never downloaded.
    pub fn targets(&self, side: PackSide) -> Vec<PackTarget> {
        let mut targets: Vec<PackTarget> = self
            .files
            .iter()
            .filter(|file| match side {
                PackSide::Client => file.client.wanted(),
                PackSide::Server => file.server.wanted(),
            })
            .map(|file| PackTarget {
                path: file.path.clone(),
                origin: PackOrigin::Download {
                    urls: file.downloads.clone(),
                    hashes: file.hashes.clone(),
                    size: file.file_size,
                },
            })
            .collect();

        targets.extend(
            self.overrides
                .iter()
                .filter(|entry| match side {
                    PackSide::Client => entry.scope != OverrideScope::Server,
                    PackSide::Server => entry.scope != OverrideScope::Client,
                })
                .map(|entry| PackTarget {
                    path: entry.target_path.clone(),
                    origin: PackOrigin::Override {
                        archive_path: entry.archive_path.clone(),
                    },
                }),
        );

        targets.sort_by(|left, right| left.path.cmp(&right.path));
        targets
    }

    pub fn download_bytes(&self) -> u64 {
        self.files.iter().filter_map(|file| file.file_size).sum()
    }

    pub fn summary_value(&self) -> Value {
        json!({
            "format": self.format.as_str(),
            "name": self.name,
            "versionId": self.version_id,
            "summary": self.summary,
            "target": self.target().map(|target| target.to_value()),
            "fileCount": self.files.len(),
            "overrideCount": self.overrides.len(),
            "restrictedCount": self.restricted_count,
            "downloadBytes": self.download_bytes(),
            "blockers": self.blockers(),
        })
    }

    /// Items the importer cannot resolve without a user decision.
    pub fn blockers(&self) -> Vec<Value> {
        let mut blockers = Vec::new();
        if self.game_version.is_none() {
            blockers.push(
                json!({ "kind": "missing_game_version", "detail": "dependencies.minecraft" }),
            );
        }
        for file in &self.files {
            if file.is_restricted() {
                blockers.push(json!({
                    "kind": "restricted_file",
                    "path": file.path,
                    "detail": "no download url in the manifest",
                }));
            }
        }
        blockers
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackTarget {
    pub path: String,
    pub origin: PackOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackOrigin {
    Download {
        urls: Vec<String>,
        hashes: Hashes,
        size: Option<u64>,
    },
    Override {
        archive_path: String,
    },
}

fn manifest_error(detail: impl Into<String>) -> ContentError {
    ContentError::InvalidManifest {
        detail: detail.into(),
    }
}

/// Parse `modrinth.index.json`. Missing required fields are rejected, never
/// defaulted, and every path is normalised through the shared guard.
pub fn parse_modrinth_index(text: &str) -> Result<PackManifest, ContentError> {
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ContentError::BudgetExceeded {
            budget: "manifest_bytes",
            limit: MAX_MANIFEST_BYTES,
            observed: text.len() as u64,
        });
    }
    let value: Value =
        serde_json::from_str(text).map_err(|error| manifest_error(error.to_string()))?;
    parse_modrinth_index_value(&value)
}

pub fn parse_modrinth_index_value(value: &Value) -> Result<PackManifest, ContentError> {
    let object = value
        .as_object()
        .ok_or_else(|| manifest_error("manifest must be a JSON object"))?;

    let format_version = object
        .get("formatVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| manifest_error("formatVersion is required"))?;
    if format_version != 1 {
        return Err(ContentError::UnsupportedFormat {
            format: format!("mrpack.formatVersion={format_version}"),
        });
    }
    let game = object
        .get("game")
        .and_then(Value::as_str)
        .ok_or_else(|| manifest_error("game is required"))?;
    if game != "minecraft" {
        return Err(manifest_error(format!("unsupported game {game}")));
    }
    let name = required_string(object, "name")?;
    let version_id = required_string(object, "versionId")?;
    let summary = object
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut game_version = None;
    let mut loader = None;
    let mut loader_version = None;
    if let Some(dependencies) = object.get("dependencies") {
        let dependencies = dependencies
            .as_object()
            .ok_or_else(|| manifest_error("dependencies must be an object"))?;
        if let Some(value) = dependencies.get("minecraft").and_then(Value::as_str) {
            game_version = Some(value.to_string());
        }
        for (key, kind) in [
            ("fabric-loader", LoaderKind::Fabric),
            ("quilt-loader", LoaderKind::Quilt),
            ("forge", LoaderKind::Forge),
            ("neoforge", LoaderKind::NeoForge),
        ] {
            if let Some(value) = dependencies.get(key).and_then(Value::as_str) {
                loader = Some(kind);
                loader_version = Some(value.to_string());
                break;
            }
        }
    }

    let files_value = object
        .get("files")
        .ok_or_else(|| manifest_error("files is required"))?;
    let files_array = files_value
        .as_array()
        .ok_or_else(|| manifest_error("files must be an array"))?;

    let mut files: Vec<ManifestFile> = Vec::new();
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut restricted_count = 0u32;

    for entry in files_array {
        let entry = entry
            .as_object()
            .ok_or_else(|| manifest_error("each file must be an object"))?;
        let raw_path = required_string(entry, "path")?;
        let path = normalise_content_path(&raw_path)?;

        let hashes_object = entry
            .get("hashes")
            .and_then(Value::as_object)
            .ok_or_else(|| manifest_error(format!("file {path} has no hashes")))?;
        let sha1 = hashes_object
            .get("sha1")
            .and_then(Value::as_str)
            .map(str::to_string);
        let sha512 = hashes_object
            .get("sha512")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(value) = &sha1 {
            if value.len() != 40 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err(manifest_error(format!("file {path} has a malformed sha1")));
            }
        }
        if let Some(value) = &sha512 {
            if value.len() != 128 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err(manifest_error(format!(
                    "file {path} has a malformed sha512"
                )));
            }
        }
        let hashes = Hashes {
            sha1,
            sha512,
            sha256: None,
        };
        if hashes.is_empty() {
            return Err(manifest_error(format!("file {path} has no usable hash")));
        }

        let downloads: Vec<String> = match entry.get("downloads") {
            Some(value) => value
                .as_array()
                .ok_or_else(|| manifest_error(format!("file {path} downloads must be an array")))?
                .iter()
                .map(|url| {
                    url.as_str()
                        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                        .map(str::to_string)
                        .ok_or_else(|| manifest_error(format!("file {path} has a non-http url")))
                })
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };

        let file_size =
            match entry.get("fileSize") {
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    manifest_error(format!("file {path} fileSize must be a number"))
                })?),
                None => None,
            };

        let mut client = SideSupport::Required;
        let mut server = SideSupport::Required;
        if let Some(env) = entry.get("env") {
            let env = env
                .as_object()
                .ok_or_else(|| manifest_error(format!("file {path} env must be an object")))?;
            if let Some(value) = env.get("client") {
                client = value.as_str().and_then(SideSupport::parse).ok_or_else(|| {
                    manifest_error(format!("file {path} has an invalid env.client"))
                })?;
            }
            if let Some(value) = env.get("server") {
                server = value.as_str().and_then(SideSupport::parse).ok_or_else(|| {
                    manifest_error(format!("file {path} has an invalid env.server"))
                })?;
            }
        }

        let key = collision_key(&path);
        let identity = hashes.identity_key().unwrap_or_default();
        match seen.get(&key) {
            Some(existing) if existing == &identity => continue, // same file, deduped
            Some(_) => return Err(ContentError::DuplicatePath { path }),
            None => {
                seen.insert(key, identity);
            }
        }

        if downloads.is_empty() {
            restricted_count += 1;
        }

        files.push(ManifestFile {
            path,
            hashes,
            downloads,
            file_size,
            client,
            server,
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(PackManifest {
        format: PackFormat::Modrinth,
        name,
        version_id,
        summary,
        game_version,
        loader,
        loader_version,
        files,
        overrides: Vec::new(),
        restricted_count,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, ContentError> {
    match object.get(key).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_string()),
        _ => Err(manifest_error(format!(
            "{key} is required and must be non-empty"
        ))),
    }
}

/// Classify one archive entry as an override, returning its scope and the path it
/// must land on. `None` means the entry is not an override.
pub fn classify_override(name: &str) -> Result<Option<OverrideEntry>, ContentError> {
    let (scope, rest) = if let Some(rest) = name.strip_prefix("overrides/") {
        (OverrideScope::Shared, rest)
    } else if let Some(rest) = name.strip_prefix("client-overrides/") {
        (OverrideScope::Client, rest)
    } else if let Some(rest) = name.strip_prefix("server-overrides/") {
        (OverrideScope::Server, rest)
    } else {
        return Ok(None);
    };
    if rest.is_empty() || rest.ends_with('/') {
        // Directory record for the prefix itself.
        return Ok(None);
    }
    let target_path = normalise_content_path(rest)?;
    Ok(Some(OverrideEntry {
        scope,
        archive_path: name.to_string(),
        target_path,
        sha256: None,
        size: None,
    }))
}

/// Read a `.mrpack` archive: the manifest, plus every override record with its
/// **verified** payload identity (length, CRC and SHA-256).
///
/// A duplicate `modrinth.index.json` is rejected instead of silently taking the
/// first one, and a truncated or corrupted override payload fails here rather than
/// during installation.
pub fn read_mrpack(bytes: &[u8]) -> Result<PackManifest, ContentError> {
    let budget = ZipBudget::default();
    let index = read_index(bytes, &budget)?;

    let manifests: Vec<&super::zipindex::ZipEntry> = index
        .entries
        .iter()
        .filter(|entry| entry.name == MODRINTH_INDEX)
        .collect();
    let manifest_entry = match manifests.as_slice() {
        [] => return Err(manifest_error(format!("archive has no {MODRINTH_INDEX}"))),
        [only] => *only,
        _ => {
            return Err(ContentError::DuplicatePath {
                path: MODRINTH_INDEX.to_string(),
            });
        }
    };

    let raw = read_entry(bytes, manifest_entry, &budget)?;
    let text =
        String::from_utf8(raw).map_err(|_| manifest_error("manifest is not utf-8".to_string()))?;
    let mut manifest = parse_modrinth_index(&text)?;
    manifest.overrides = read_overrides(bytes, &index, &budget)?;
    Ok(manifest)
}

fn read_overrides(
    bytes: &[u8],
    index: &ZipIndex,
    budget: &ZipBudget,
) -> Result<Vec<OverrideEntry>, ContentError> {
    let mut overrides = Vec::new();
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for entry in &index.entries {
        if entry.is_directory() {
            continue;
        }
        if let Some(mut record) = classify_override(&entry.name)? {
            // Override payloads must be verified: an override has no manifest digest,
            // so the archive itself has to prove the bytes before they can be planned.
            let digest = verify_entry(bytes, entry, budget)?;
            record.sha256 = Some(digest.sha256);
            record.size = Some(digest.uncompressed);

            let key = collision_key(&record.target_path);
            match seen.get(&key) {
                Some(existing) if existing == &record.archive_path => continue,
                Some(_) => {
                    return Err(ContentError::DuplicatePath {
                        path: record.target_path,
                    });
                }
                None => {
                    seen.insert(key, record.archive_path.clone());
                }
            }
            overrides.push(record);
        }
    }
    overrides.sort_by(|left, right| left.target_path.cmp(&right.target_path));
    Ok(overrides)
}

// ---------------------------------------------------------------------------
// Export direction
// ---------------------------------------------------------------------------

/// One candidate file for an export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportCandidate {
    pub path: String,
    pub hashes: Hashes,
    pub downloads: Vec<String>,
    pub file_size: Option<u64>,
    pub client: SideSupport,
    pub server: SideSupport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSpec {
    pub format: PackFormat,
    pub name: String,
    pub version_id: String,
    pub summary: Option<String>,
    pub target: GameTarget,
    pub files: Vec<ExportCandidate>,
    /// Paths in the request that were dropped, with the reason.
    pub excluded: Vec<(String, &'static str)>,
}

impl ExportSpec {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().filter_map(|file| file.file_size).sum()
    }

    pub fn to_value(&self) -> Value {
        json!({
            "format": self.format.as_str(),
            "name": self.name,
            "versionId": self.version_id,
            "target": self.target.to_value(),
            "fileCount": self.files.len(),
            "bytes": self.total_bytes(),
            "excluded": self
                .excluded
                .iter()
                .map(|(path, reason)| json!({ "path": path, "reason": reason }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Build the export candidate set: normalise, drop credentials/logs, optionally
/// drop the world, dedupe by case-insensitive path.
pub fn plan_export(
    format: PackFormat,
    name: &str,
    version_id: &str,
    target: GameTarget,
    files: Vec<ExportCandidate>,
    include_saves: bool,
) -> Result<ExportSpec, ContentError> {
    if name.trim().is_empty() || version_id.trim().is_empty() {
        return Err(ContentError::InvalidPlan {
            detail: "export name and versionId are required".to_string(),
        });
    }
    let mut accepted: Vec<ExportCandidate> = Vec::new();
    let mut excluded: Vec<(String, &'static str)> = Vec::new();
    let mut seen: std::collections::BTreeMap<String, ()> = std::collections::BTreeMap::new();

    for file in files {
        let path = normalise_content_path(&file.path)?;
        if is_excluded_from_export(&path) {
            excluded.push((path, "credentials_or_logs"));
            continue;
        }
        if !include_saves && path.starts_with("saves/") {
            excluded.push((path, "saves_not_selected"));
            continue;
        }
        let key = collision_key(&path);
        if seen.insert(key, ()).is_some() {
            excluded.push((path, "duplicate_path"));
            continue;
        }
        if file.hashes.is_empty() {
            excluded.push((path, "no_hash"));
            continue;
        }
        accepted.push(ExportCandidate { path, ..file });
    }

    accepted.sort_by(|left, right| left.path.cmp(&right.path));
    excluded.sort_by(|left, right| left.0.cmp(&right.0));

    Ok(ExportSpec {
        format,
        name: name.to_string(),
        version_id: version_id.to_string(),
        summary: None,
        target,
        files: accepted,
        excluded,
    })
}

/// Serialise an `ExportSpec` to a `modrinth.index.json` document. Deterministic:
/// files sorted by path, dependencies ordered by the same rule as the parser.
pub fn build_modrinth_index(spec: &ExportSpec) -> Result<String, ContentError> {
    if spec.format != PackFormat::Modrinth {
        return Err(ContentError::UnsupportedFormat {
            format: spec.format.as_str().to_string(),
        });
    }
    let mut dependencies = Map::new();
    dependencies.insert("minecraft".to_string(), json!(spec.target.game_version));
    match spec.target.loader {
        LoaderKind::Fabric => {
            if let Some(version) = &spec.target.loader_version {
                dependencies.insert("fabric-loader".to_string(), json!(version));
            }
        }
        LoaderKind::Quilt => {
            if let Some(version) = &spec.target.loader_version {
                dependencies.insert("quilt-loader".to_string(), json!(version));
            }
        }
        LoaderKind::Forge => {
            if let Some(version) = &spec.target.loader_version {
                dependencies.insert("forge".to_string(), json!(version));
            }
        }
        LoaderKind::NeoForge => {
            if let Some(version) = &spec.target.loader_version {
                dependencies.insert("neoforge".to_string(), json!(version));
            }
        }
        LoaderKind::Vanilla => {}
    }

    let files: Vec<Value> = spec
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path,
                "hashes": file.hashes.to_value(),
                "env": {
                    "client": file.client.as_str(),
                    "server": file.server.as_str(),
                },
                "downloads": file.downloads,
                "fileSize": file.file_size,
            })
        })
        .collect();

    let document = json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": spec.version_id,
        "name": spec.name,
        "summary": spec.summary,
        "files": files,
        "dependencies": Value::Object(dependencies),
    });
    serde_json::to_string_pretty(&document).map_err(|error| ContentError::Internal {
        detail: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";

    fn minimal() -> String {
        format!(
            r#"{{
              "formatVersion": 1,
              "game": "minecraft",
              "name": "Test",
              "versionId": "1.0.0",
              "files": [
                {{"path": "mods/a.jar", "hashes": {{"sha1": "{SHA1}"}}, "downloads": ["https://example.invalid/a.jar"], "fileSize": 10}}
              ],
              "dependencies": {{"minecraft": "1.20.1", "fabric-loader": "0.15.0"}}
            }}"#
        )
    }

    #[test]
    fn parses_a_minimal_manifest_and_derives_the_target() {
        let manifest = parse_modrinth_index(&minimal()).expect("parse");
        assert_eq!(manifest.name, "Test");
        assert_eq!(manifest.files.len(), 1);
        let target = manifest.target().expect("target");
        assert_eq!(target.game_version, "1.20.1");
        assert_eq!(target.loader, LoaderKind::Fabric);
        assert_eq!(target.loader_version.as_deref(), Some("0.15.0"));
    }

    #[test]
    fn rejects_missing_required_fields() {
        let mut value: Value = serde_json::from_str(&minimal()).expect("json");
        value.as_object_mut().expect("object").remove("files");
        assert!(matches!(
            parse_modrinth_index_value(&value),
            Err(ContentError::InvalidManifest { .. })
        ));

        let mut value: Value = serde_json::from_str(&minimal()).expect("json");
        value["formatVersion"] = json!(2);
        assert!(matches!(
            parse_modrinth_index_value(&value),
            Err(ContentError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn rejects_path_traversal_and_duplicate_paths() {
        let mut value: Value = serde_json::from_str(&minimal()).expect("json");
        value["files"][0]["path"] = json!("../escape.jar");
        assert!(matches!(
            parse_modrinth_index_value(&value),
            Err(ContentError::PathEscape { .. })
        ));

        let mut value: Value = serde_json::from_str(&minimal()).expect("json");
        let second = json!({
            "path": "mods/A.jar",
            "hashes": { "sha1": "ffffffffffffffffffffffffffffffffffffffff" },
            "downloads": ["https://example.invalid/b.jar"]
        });
        value["files"].as_array_mut().expect("array").push(second);
        assert!(matches!(
            parse_modrinth_index_value(&value),
            Err(ContentError::DuplicatePath { .. })
        ));
    }

    #[test]
    fn classifies_override_scopes() {
        let shared = classify_override("overrides/config/a.json")
            .expect("ok")
            .expect("some");
        assert_eq!(shared.scope, OverrideScope::Shared);
        assert_eq!(shared.target_path, "config/a.json");

        let client = classify_override("client-overrides/mods/x.jar")
            .expect("ok")
            .expect("some");
        assert_eq!(client.scope, OverrideScope::Client);

        assert!(
            classify_override("modrinth.index.json")
                .expect("ok")
                .is_none()
        );
        assert!(matches!(
            classify_override("overrides/../evil.json"),
            Err(ContentError::PathEscape { .. })
        ));
    }
}
