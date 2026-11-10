//! Fabric client profile validation and deterministic composition. No I/O.
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct FabricLibrary {
    pub name: String,
    pub path: String,
    pub url: String,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}
#[derive(Debug, Clone)]
pub struct FabricProfile {
    pub loader_version: String,
    pub main_class: String,
    pub libraries: Vec<FabricLibrary>,
    pub jvm_args: Vec<String>,
    pub game_args: Vec<String>,
}

pub fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.contains("..")
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
}
fn args(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let items = value
        .as_array()
        .filter(|items| items.len() <= 128)
        .ok_or("fabric_arguments")?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|s| s.len() <= 4096 && !s.contains('\0'))
                .map(str::to_string)
                .ok_or_else(|| "fabric_argument".into())
        })
        .collect()
}
pub fn parse_profile(raw: &Value, game: &str, loader: &str) -> Result<FabricProfile, String> {
    if !valid_version(loader)
        || raw["inheritsFrom"] != game
        || raw["id"] != format!("fabric-loader-{loader}-{game}")
    {
        return Err("fabric_profile_identity".into());
    }
    let main = raw["mainClass"]
        .as_str()
        .filter(|s| {
            matches!(
                *s,
                "net.fabricmc.loader.impl.launch.knot.KnotClient"
                    | "net.fabricmc.loader.launch.knot.KnotClient"
            )
        })
        .ok_or("fabric_main_class")?;
    let libraries = raw["libraries"]
        .as_array()
        .filter(|items| !items.is_empty() && items.len() <= 128)
        .ok_or("fabric_libraries")?;
    let mut paths = BTreeSet::new();
    let mut result = vec![];
    for library in libraries {
        let name = library["name"]
            .as_str()
            .filter(|s| s.len() <= 256)
            .ok_or("fabric_coordinate")?;
        let parts: Vec<_> = name.split(':').collect();
        if parts.len() != 3
            || parts.iter().any(|part| !valid_version(part))
            || parts[0].split('.').any(|part| part.is_empty())
        {
            return Err("fabric_coordinate".into());
        }
        if library["url"] != "https://maven.fabricmc.net/" {
            return Err("fabric_library_origin".into());
        }
        let path = format!(
            "{}/{}/{}/{}-{}.jar",
            parts[0].replace('.', "/"),
            parts[1],
            parts[2],
            parts[1],
            parts[2]
        );
        if !crate::launch::install::safe_path(&path) || !paths.insert(path.clone()) {
            return Err("fabric_library_path".into());
        }
        let sha1 = match library.get("sha1") {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|s| s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit()))
                    .ok_or("fabric_library_hash")?
                    .to_ascii_lowercase(),
            ),
        };
        let size = match library.get("size") {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|s| *s > 0 && *s <= 256 * 1024 * 1024)
                    .ok_or("fabric_library_size")?,
            ),
        };
        result.push(FabricLibrary {
            name: name.into(),
            url: format!("https://maven.fabricmc.net/{path}"),
            path,
            sha1,
            size,
        });
    }
    // The selected loader must actually be in the downloaded library set.
    if !result
        .iter()
        .any(|lib| lib.name == format!("net.fabricmc:fabric-loader:{loader}"))
        || !result
            .iter()
            .any(|lib| lib.name == format!("net.fabricmc:intermediary:{game}"))
    {
        return Err("fabric_required_libraries".into());
    }
    Ok(FabricProfile {
        loader_version: loader.into(),
        main_class: main.into(),
        libraries: result,
        jvm_args: args(raw.pointer("/arguments/jvm"))?,
        game_args: args(raw.pointer("/arguments/game"))?,
    })
}

impl FabricProfile {
    /// All library identities must be resolved before a profile becomes installable.
    pub fn merge(&self, base: &Value) -> Result<Value, String> {
        let mut raw = base.clone();
        let libraries = raw["libraries"]
            .as_array_mut()
            .ok_or("fabric_base_libraries")?;
        for library in &self.libraries {
            let sha1 = library.sha1.as_deref().ok_or("fabric_unresolved_hash")?;
            let size = library.size.ok_or("fabric_unresolved_size")?;
            let key = library.name.rsplit_once(':').ok_or("fabric_coordinate")?.0;
            libraries.retain(|item| {
                item["name"]
                    .as_str()
                    .and_then(|s| s.rsplit_once(':'))
                    .is_none_or(|(name, _)| name != key)
            });
            libraries.push(json!({"name":library.name,"downloads":{"artifact":{"path":library.path,"url":library.url,"sha1":sha1,"size":size}}}));
        }
        raw["mainClass"] = json!(self.main_class);
        // Modern supported profiles inherit the base game's arguments; keep their ordering.
        let arguments = raw["arguments"]
            .as_object_mut()
            .ok_or("fabric_base_arguments")?;
        for (key, extra) in [("jvm", &self.jvm_args), ("game", &self.game_args)] {
            let items = arguments
                .entry(key)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or("fabric_base_arguments")?;
            items.extend(extra.iter().map(|s| json!(s)));
        }
        raw["shtmclLoader"] = json!({"kind":"fabric","version":self.loader_version});
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"id":"fabric-loader-0.16.10-1.20.4","inheritsFrom":"1.20.4","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient","libraries":[{"name":"net.fabricmc:fabric-loader:0.16.10","url":"https://maven.fabricmc.net/"},{"name":"net.fabricmc:intermediary:1.20.4","url":"https://maven.fabricmc.net/"}],"arguments":{"game":[],"jvm":["-Dfabric.test=true"]}})
    }
    #[test]
    fn resolves_coordinates_and_requires_identity_before_merge() {
        let p = parse_profile(&fixture(), "1.20.4", "0.16.10").unwrap();
        assert_eq!(
            p.libraries[0].path,
            "net/fabricmc/fabric-loader/0.16.10/fabric-loader-0.16.10.jar"
        );
        assert!(
            p.merge(&json!({"libraries":[],"arguments":{"game":[]}}))
                .is_err()
        );
    }
    #[test]
    fn rejects_mismatched_origins_paths_hashes_and_missing_loader() {
        for (pointer, value) in [
            ("/inheritsFrom", json!("1.20.1")),
            ("/mainClass", json!("evil.Main")),
            ("/libraries/0/name", json!("net.fabricmc:../evil:0.16.10")),
            ("/libraries/0/url", json!("http://maven.fabricmc.net/")),
            ("/libraries/0/sha1", json!("bad")),
            ("/libraries/0/size", json!(0)),
            ("/arguments/jvm", json!([{"rules":[]}])),
        ] {
            let mut raw = fixture();
            if let Some(target) = raw.pointer_mut(pointer) {
                *target = value;
            } else {
                let (parent, key) = pointer.rsplit_once('/').unwrap();
                raw.pointer_mut(parent).unwrap()[key] = value;
            }
            assert!(parse_profile(&raw, "1.20.4", "0.16.10").is_err());
        }
    }
    #[test]
    fn composes_main_arguments_and_replaces_same_library_coordinate() {
        let mut p = parse_profile(&fixture(), "1.20.4", "0.16.10").unwrap();
        for l in &mut p.libraries {
            l.sha1 = Some("a".repeat(40));
            l.size = Some(12);
        }
        let merged=p.merge(&json!({"id":"1.20.4","libraries":[{"name":"net.fabricmc:fabric-loader:old"}],"arguments":{"game":["--demo"],"jvm":["-Xmx1G"]}})).unwrap();
        assert_eq!(merged["libraries"].as_array().unwrap().len(), 2);
        assert_eq!(merged["arguments"]["game"], json!(["--demo"]));
        assert_eq!(
            merged["arguments"]["jvm"],
            json!(["-Xmx1G", "-Dfabric.test=true"])
        );
        assert_eq!(merged["id"], "1.20.4");
    }
}
