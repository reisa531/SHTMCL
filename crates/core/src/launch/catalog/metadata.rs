use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

pub const MAX_VERSION_METADATA_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INHERITANCE_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    TooLarge,
    InvalidJson,
    InvalidRoot,
    MissingVersionId,
    RequestedIdMismatch { requested: String, found: String },
    MissingParent(String),
    SourceUnavailable { id: String, code: String },
    InheritanceCycle(String),
    InheritanceTooDeep,
    InvalidLibraries,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VersionMetadata {
    pub id: String,
    pub inherits_from: Option<String>,
    pub asset_index: Option<Value>,
    pub assets: Option<String>,
    pub main_class: Option<String>,
    pub minecraft_arguments: Option<String>,
    pub arguments: Option<Value>,
    pub java_major: Option<u32>,
    pub libraries: Vec<Value>,
    /// Retains downloads, logging, compliance and fields not understood yet.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedVersion {
    pub metadata: VersionMetadata,
    pub inheritance_chain: Vec<String>,
}

pub trait VersionMetadataSource {
    fn load(&self, id: &str) -> Result<Option<Value>, String>;
}

impl VersionMetadata {
    pub fn parse(bytes: &[u8], expected_id: Option<&str>) -> Result<Self, MetadataError> {
        if bytes.len() > MAX_VERSION_METADATA_BYTES {
            return Err(MetadataError::TooLarge);
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| MetadataError::InvalidJson)?;
        Self::from_value(value, expected_id)
    }

    pub fn from_value(raw: Value, expected_id: Option<&str>) -> Result<Self, MetadataError> {
        if !raw.is_object() {
            return Err(MetadataError::InvalidRoot);
        }
        let id = raw
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or(MetadataError::MissingVersionId)?
            .to_owned();
        if let Some(requested) = expected_id {
            if requested != id {
                return Err(MetadataError::RequestedIdMismatch {
                    requested: requested.to_owned(),
                    found: id,
                });
            }
        }
        let libraries = raw
            .get("libraries")
            .map(|value| {
                value
                    .as_array()
                    .cloned()
                    .ok_or(MetadataError::InvalidLibraries)
            })
            .transpose()?
            .unwrap_or_default();
        let java_major = raw
            .pointer("/javaVersion/majorVersion")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        Ok(Self {
            id,
            inherits_from: raw
                .get("inheritsFrom")
                .and_then(Value::as_str)
                .map(str::to_owned),
            asset_index: raw.get("assetIndex").cloned(),
            assets: raw.get("assets").and_then(Value::as_str).map(str::to_owned),
            main_class: raw
                .get("mainClass")
                .and_then(Value::as_str)
                .map(str::to_owned),
            minecraft_arguments: raw
                .get("minecraftArguments")
                .and_then(Value::as_str)
                .map(str::to_owned),
            arguments: raw.get("arguments").cloned(),
            java_major,
            libraries,
            raw,
        })
    }

    pub fn resolve<S: VersionMetadataSource>(
        source: &S,
        requested_id: &str,
    ) -> Result<ResolvedVersion, MetadataError> {
        let mut visiting = BTreeSet::new();
        let mut chain = Vec::new();
        let raw = resolve_value(source, requested_id, &mut visiting, &mut chain, 0)?;
        let metadata = Self::from_value(raw, Some(requested_id))?;
        Ok(ResolvedVersion {
            metadata,
            inheritance_chain: chain,
        })
    }
}

fn resolve_value<S: VersionMetadataSource>(
    source: &S,
    id: &str,
    visiting: &mut BTreeSet<String>,
    chain: &mut Vec<String>,
    depth: usize,
) -> Result<Value, MetadataError> {
    if depth >= MAX_INHERITANCE_DEPTH {
        return Err(MetadataError::InheritanceTooDeep);
    }
    if !visiting.insert(id.to_owned()) {
        return Err(MetadataError::InheritanceCycle(id.to_owned()));
    }
    let raw = source
        .load(id)
        .map_err(|error| MetadataError::SourceUnavailable {
            id: id.to_owned(),
            code: safe_source_error_code(error),
        })?
        .ok_or_else(|| MetadataError::MissingParent(id.to_owned()))?;
    let child_id = raw
        .get("id")
        .and_then(Value::as_str)
        .ok_or(MetadataError::MissingVersionId)?;
    if child_id != id {
        return Err(MetadataError::RequestedIdMismatch {
            requested: id.to_owned(),
            found: child_id.to_owned(),
        });
    }
    chain.push(id.to_owned());
    let parent_id = raw.get("inheritsFrom").and_then(Value::as_str);
    let merged = if let Some(parent_id) = parent_id {
        let parent = resolve_value(source, parent_id, visiting, chain, depth + 1)?;
        merge_metadata(parent, raw)?
    } else {
        raw
    };
    visiting.remove(id);
    Ok(merged)
}

// Sources may return transport diagnostics containing URLs or credentials.
// Only bounded machine codes cross the metadata boundary.
fn safe_source_error_code(error: String) -> String {
    if !error.is_empty()
        && error.len() <= 96
        && error.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
    {
        error
    } else {
        "version.source_unavailable".to_owned()
    }
}

fn merge_metadata(mut parent: Value, child: Value) -> Result<Value, MetadataError> {
    let parent_object = parent.as_object_mut().ok_or(MetadataError::InvalidRoot)?;
    let child_object = child.as_object().ok_or(MetadataError::InvalidRoot)?;
    for (key, child_value) in child_object {
        if key == "libraries" {
            let parent_libraries = parent_object
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let child_libraries = child_value
                .as_array()
                .ok_or(MetadataError::InvalidLibraries)?;
            parent_object.insert(
                key.clone(),
                Value::Array(merge_libraries(parent_libraries, child_libraries.clone())?),
            );
        } else if key != "inheritsFrom" {
            match (parent_object.get_mut(key), child_value) {
                (Some(Value::Object(parent_map)), Value::Object(child_map)) => {
                    merge_objects(parent_map, child_map);
                }
                _ => {
                    parent_object.insert(key.clone(), child_value.clone());
                }
            }
        }
    }
    // Report the installed child as the resolved version while retaining the
    // parent's unrelated metadata and all library rules/classifiers.
    if let Some(id) = child_object.get("id") {
        parent_object.insert("id".to_owned(), id.clone());
    }
    parent_object.remove("inheritsFrom");
    Ok(parent)
}

fn merge_objects(parent: &mut Map<String, Value>, child: &Map<String, Value>) {
    for (key, value) in child {
        match (parent.get_mut(key), value) {
            (Some(Value::Object(parent_nested)), Value::Object(child_nested)) => {
                merge_objects(parent_nested, child_nested);
            }
            _ => {
                parent.insert(key.clone(), value.clone());
            }
        }
    }
}

fn merge_libraries(parent: Vec<Value>, child: Vec<Value>) -> Result<Vec<Value>, MetadataError> {
    let mut result = Vec::with_capacity(parent.len() + child.len());
    let mut positions = BTreeMap::<String, usize>::new();
    for library in parent.into_iter().chain(child) {
        let name = library
            .get("name")
            .and_then(Value::as_str)
            .ok_or(MetadataError::InvalidLibraries)?
            .to_owned();
        if let Some(index) = positions.get(&name).copied() {
            result[index] = library;
        } else {
            positions.insert(name, result.len());
            result.push(library);
        }
    }
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryRuleDecision {
    Included,
    Excluded,
    Undetermined(String),
}

/// Evaluate the portable subset of Mojang library rules. Platform/version
/// regexes and unknown rule fields stay indeterminate; callers must not install
/// a library based on a guessed match.
pub fn evaluate_rules(
    rules: Option<&Value>,
    os_name: &str,
    arch_name: &str,
    features: &BTreeMap<String, bool>,
) -> LibraryRuleDecision {
    let Some(rules) = rules else {
        return LibraryRuleDecision::Included;
    };
    let Some(rules) = rules.as_array() else {
        return LibraryRuleDecision::Undetermined("rules_not_array".into());
    };
    let mut decision = LibraryRuleDecision::Excluded;
    for rule in rules {
        let Some(object) = rule.as_object() else {
            return LibraryRuleDecision::Undetermined("rule_not_object".into());
        };
        let Some(action) = object.get("action").and_then(Value::as_str) else {
            return LibraryRuleDecision::Undetermined("action_missing".into());
        };
        if action != "allow" && action != "disallow" {
            return LibraryRuleDecision::Undetermined("action_unknown".into());
        }
        let mut matches = true;
        if let Some(os) = object.get("os") {
            let Some(os) = os.as_object() else {
                return LibraryRuleDecision::Undetermined("os_rule_invalid".into());
            };
            if os
                .keys()
                .any(|key| !matches!(key.as_str(), "name" | "arch" | "version"))
            {
                return LibraryRuleDecision::Undetermined("os_field_unknown".into());
            }
            if let Some(required) = os.get("name").and_then(Value::as_str) {
                matches &= required == os_name;
            }
            if let Some(required) = os.get("arch").and_then(Value::as_str) {
                matches &= required == arch_name;
            }
            if matches && os.get("version").is_some() {
                return LibraryRuleDecision::Undetermined("os_version_pattern_unsupported".into());
            }
        }
        if let Some(required_features) = object.get("features") {
            let Some(required_features) = required_features.as_object() else {
                return LibraryRuleDecision::Undetermined("features_invalid".into());
            };
            for (name, expected) in required_features {
                let Some(expected) = expected.as_bool() else {
                    return LibraryRuleDecision::Undetermined("feature_value_invalid".into());
                };
                matches &= features.get(name).copied().unwrap_or(false) == expected;
            }
        }
        if matches {
            decision = if action == "allow" {
                LibraryRuleDecision::Included
            } else {
                LibraryRuleDecision::Excluded
            };
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Source(HashMap<String, Value>);
    impl VersionMetadataSource for Source {
        fn load(&self, id: &str) -> Result<Option<Value>, String> {
            Ok(self.0.get(id).cloned())
        }
    }

    fn source(values: &[(&str, &str)]) -> Source {
        Source(
            values
                .iter()
                .map(|(id, json)| ((*id).to_owned(), serde_json::from_str(json).unwrap()))
                .collect(),
        )
    }

    #[test]
    fn resolves_child_parent_libraries_assets_and_historical_arguments() {
        let source = source(&[
            (
                "base",
                r#"{"id":"base","mainClass":"net.minecraft.Main","assets":"legacy","assetIndex":{"id":"legacy"},"minecraftArguments":"--username ${auth_player_name}","libraries":[{"name":"g:a:1","downloads":{"artifact":{"path":"g/a/1/a.jar"}}},{"name":"g:b:1"}]}"#,
            ),
            (
                "child",
                r#"{"id":"child","inheritsFrom":"base","javaVersion":{"majorVersion":21},"libraries":[{"name":"g:a:1","downloads":{"artifact":{"path":"g/a/2/a.jar"}}},{"name":"g:c:1","rules":[{"action":"allow","os":{"name":"windows"}}]}]}"#,
            ),
        ]);
        let resolved = VersionMetadata::resolve(&source, "child").unwrap();
        assert_eq!(resolved.inheritance_chain, ["child", "base"]);
        assert_eq!(
            resolved.metadata.main_class.as_deref(),
            Some("net.minecraft.Main")
        );
        assert_eq!(
            resolved.metadata.minecraft_arguments.as_deref(),
            Some("--username ${auth_player_name}")
        );
        assert_eq!(resolved.metadata.java_major, Some(21));
        assert_eq!(resolved.metadata.libraries.len(), 3);
        assert_eq!(
            resolved.metadata.libraries[0]["downloads"]["artifact"]["path"],
            "g/a/2/a.jar"
        );
        assert_eq!(
            resolved.metadata.asset_index.as_ref().unwrap()["id"],
            "legacy"
        );
    }

    #[test]
    fn inheritance_cycle_missing_parent_and_depth_are_controlled() {
        let cycle = source(&[
            ("a", r#"{"id":"a","inheritsFrom":"b"}"#),
            ("b", r#"{"id":"b","inheritsFrom":"a"}"#),
        ]);
        assert!(matches!(
            VersionMetadata::resolve(&cycle, "a"),
            Err(MetadataError::InheritanceCycle(_))
        ));
        let missing = source(&[("a", r#"{"id":"a","inheritsFrom":"missing"}"#)]);
        assert_eq!(
            VersionMetadata::resolve(&missing, "a"),
            Err(MetadataError::MissingParent("missing".into()))
        );
        let mut chain = HashMap::new();
        for i in 0..=MAX_INHERITANCE_DEPTH {
            let parent = if i == 0 {
                "".into()
            } else {
                format!(",\"inheritsFrom\":\"v{}\"", i - 1)
            };
            chain.insert(
                format!("v{i}"),
                serde_json::from_str(&format!("{{\"id\":\"v{i}\"{parent}}}")).unwrap(),
            );
        }
        assert_eq!(
            VersionMetadata::resolve(&Source(chain), &format!("v{MAX_INHERITANCE_DEPTH}")),
            Err(MetadataError::InheritanceTooDeep)
        );
    }

    #[test]
    fn source_failure_is_distinct_from_missing_parent() {
        struct UnavailableParent;
        impl VersionMetadataSource for UnavailableParent {
            fn load(&self, id: &str) -> Result<Option<Value>, String> {
                if id == "child" {
                    Ok(Some(
                        serde_json::json!({"id":"child", "inheritsFrom":"parent"}),
                    ))
                } else {
                    Err("version.metadata.network_unavailable".into())
                }
            }
        }
        assert_eq!(
            VersionMetadata::resolve(&UnavailableParent, "child"),
            Err(MetadataError::SourceUnavailable {
                id: "parent".into(),
                code: "version.metadata.network_unavailable".into(),
            })
        );
        let missing = source(&[("child", r#"{"id":"child","inheritsFrom":"parent"}"#)]);
        assert_eq!(
            VersionMetadata::resolve(&missing, "child"),
            Err(MetadataError::MissingParent("parent".into()))
        );
    }

    #[test]
    fn source_failure_redacts_unstructured_diagnostics_and_bounds_codes() {
        struct FailingSource(String);
        impl VersionMetadataSource for FailingSource {
            fn load(&self, _: &str) -> Result<Option<Value>, String> {
                Err(self.0.clone())
            }
        }
        for unsafe_error in [
            String::new(),
            "GET https://example.invalid/version?token=secret failed".into(),
            "version.error\nAuthorization: secret".into(),
            "版本获取失败".into(),
            "VERSION.ERROR".into(),
            "x".repeat(97),
        ] {
            let error = VersionMetadata::resolve(&FailingSource(unsafe_error), "release");
            assert_eq!(
                error,
                Err(MetadataError::SourceUnavailable {
                    id: "release".into(),
                    code: "version.source_unavailable".into(),
                })
            );
        }
        let longest_code = "x".repeat(96);
        assert_eq!(
            VersionMetadata::resolve(&FailingSource(longest_code.clone()), "release"),
            Err(MetadataError::SourceUnavailable {
                id: "release".into(),
                code: longest_code,
            })
        );
    }

    #[test]
    fn rules_are_ordered_and_unsupported_patterns_are_not_guessed() {
        let rules: Value = serde_json::from_str(
            r#"[{"action":"allow"},{"action":"disallow","os":{"name":"linux"}}]"#,
        )
        .unwrap();
        assert_eq!(
            evaluate_rules(Some(&rules), "linux", "x64", &BTreeMap::new()),
            LibraryRuleDecision::Excluded
        );
        let pattern: Value =
            serde_json::from_str(r#"[{"action":"allow","os":{"version":"^10\\."}}]"#).unwrap();
        assert_eq!(
            evaluate_rules(Some(&pattern), "windows", "x64", &BTreeMap::new()),
            LibraryRuleDecision::Undetermined("os_version_pattern_unsupported".into())
        );
    }
}
