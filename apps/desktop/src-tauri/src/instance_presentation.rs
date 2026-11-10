//! Bounded, read-only display metadata. Unknown values remain absent.
use serde_json::{json, Value};
use std::{fs, io::Read, path::Path};

fn checked(path: &Path) -> bool {
    path.ancestors()
        .all(|part| crate::game_install::reject_link(part).is_ok())
}

fn mod_files(root: &Path) -> Option<u32> {
    let mods = root.join("mods");
    if !checked(&mods) {
        return None;
    }
    let entries = match fs::read_dir(&mods) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(0),
        Err(_) => return None,
    };
    let mut count = 0;
    for (index, entry) in entries.enumerate() {
        if index >= 4096 {
            return None;
        }
        let entry = entry.ok()?;
        let metadata = entry.file_type().ok()?;
        if !metadata.is_file() || !checked(&entry.path()) {
            continue;
        }
        if entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
        {
            count += 1;
        }
    }
    Some(count)
}

fn pack_name(root: &Path) -> Option<String> {
    for name in ["modrinth.index.json", "manifest.json"] {
        let path = root.join(name);
        if !checked(&path) {
            continue;
        }
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let mut bytes = Vec::new();
        if !file.metadata().ok()?.is_file()
            || file.take(1024 * 1024 + 1).read_to_end(&mut bytes).is_err()
            || bytes.len() > 1024 * 1024
        {
            continue;
        }
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let valid = if name == "modrinth.index.json" {
            shtmcl_core::content::modpack::parse_modrinth_index_value(&value).is_ok()
        } else {
            value["manifestType"] == "minecraftModpack"
                && value["manifestVersion"] == 1
                && value["minecraft"]["version"].is_string()
                && value["files"].is_array()
        };
        if valid {
            if let Some(name) = value["name"].as_str().map(str::trim).filter(|name| {
                !name.is_empty()
                    && name.chars().count() <= 200
                    && !name.chars().any(char::is_control)
            }) {
                return Some(name.into());
            }
        }
    }
    None
}

pub(crate) fn enrich(result: &mut Value, instances_root: &Path) {
    let Some(instances) = result["instances"].as_array_mut() else {
        return;
    };
    for instance in instances.iter_mut().take(256) {
        let Some(id) = instance["id"].as_str() else {
            continue;
        };
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            continue;
        }
        let root = instances_root.join(id);
        if instance["location"]["root"].as_str().map(Path::new) != Some(root.as_path())
            || !root.is_dir()
            || !checked(&root)
        {
            continue;
        }
        let mut presentation = json!({});
        if let Some(count) = mod_files(&root) {
            presentation["modFileCount"] = json!(count);
        }
        if let Some(name) = pack_name(&root) {
            presentation["modpackName"] = json!(name);
        }
        instance["presentation"] = presentation;
    }
}

/// Read inventory only for an explicitly opened instance. Ordinary list refreshes
/// keep their small count projection and never serialize every Mod filename.
pub(crate) fn enrich_snapshot(result: &mut Value, instances_root: &Path) {
    let mut projection = json!({"instances": [result["instance"].clone()]});
    enrich(&mut projection, instances_root);
    let instance = &mut projection["instances"][0];
    if instance["presentation"].is_object() {
        if let Some(id) = instance["id"].as_str() {
            let inventory = crate::instance_mod_inventory::scan(&instances_root.join(id));
            instance["presentation"]["modInventory"] = inventory;
        }
    }
    result["instance"] = instance.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_inventory_counts_enabled_jars_and_never_guesses_pack_names() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-display-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let game = root.join("school");
        fs::create_dir_all(game.join("mods/nested")).unwrap();
        for file in [
            "a.jar",
            "b.JAR",
            "c.jar.disabled",
            "notes.txt",
            "nested/ignored.jar",
        ] {
            fs::write(game.join("mods").join(file), b"fixture").unwrap();
        }
        fs::write(game.join("manifest.json"), br#"{"name":"Not a pack"}"#).unwrap();
        let mut result = json!({"instances":[{"id":"school","location":{"root":game}}]});
        enrich(&mut result, &root);
        assert_eq!(result["instances"][0]["presentation"]["modFileCount"], 2);
        assert!(result["instances"][0]["presentation"]["modpackName"].is_null());
        fs::write(game.join("manifest.json"), br#"{"name":"My Pack","manifestType":"minecraftModpack","manifestVersion":1,"minecraft":{"version":"1.20.1"},"files":[]}"#).unwrap();
        enrich(&mut result, &root);
        assert_eq!(
            result["instances"][0]["presentation"]["modpackName"],
            "My Pack"
        );
        let mut outside = json!({"instances":[{"id":"school","location":{"root":"elsewhere"}}]});
        enrich(&mut outside, &root);
        assert!(outside["instances"][0]["presentation"].is_null());
        fs::remove_dir_all(root).unwrap();
    }
}
