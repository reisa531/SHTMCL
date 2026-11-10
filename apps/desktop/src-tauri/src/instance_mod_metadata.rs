//! On-demand metadata for a selected file already present in a controlled inventory.
use serde_json::{json, Value};
use shtmcl_core::content::mod_metadata::{inspect, MAX_JAR_BYTES};
use std::{fs, io::Read, path::Path, sync::Mutex};

static INSPECTION: Mutex<()> = Mutex::new(());

pub(crate) fn enrich(result: &mut Value, instances_root: &Path, file_name: &str) {
    // A fresh validated inventory is the only authority for selectable files.
    let instance = &mut result["instance"];
    if !instance["presentation"].is_object() {
        return;
    }
    let inventory = &instance["presentation"]["modInventory"];
    let allowed = inventory["status"] == "ready"
        && inventory["files"]
            .as_array()
            .is_some_and(|files| files.iter().any(|file| file["fileName"] == file_name));
    let metadata = if !allowed {
        json!({"status":"unavailable"})
    } else if let Some(id) = instance["id"].as_str() {
        read(&instances_root.join(id), file_name)
    } else {
        json!({"status":"unavailable"})
    };
    instance["presentation"]["modMetadata"] = json!({"fileName":file_name,"metadata":metadata});
}

fn read(root: &Path, file_name: &str) -> Value {
    let Ok(_guard) = INSPECTION.try_lock() else {
        return json!({"status":"busy"});
    };
    if file_name.is_empty()
        || file_name.len() > 1024
        || file_name.chars().any(char::is_control)
        || file_name.contains(['/', '\\', ':'])
        || !(file_name.to_ascii_lowercase().ends_with(".jar")
            || file_name.to_ascii_lowercase().ends_with(".jar.disabled"))
    {
        return json!({"status":"unavailable"});
    }
    let path = root.join("mods").join(file_name);
    if path
        .ancestors()
        .any(|part| crate::game_install::reject_link(part).is_err())
    {
        return json!({"status":"unavailable"});
    }
    let Ok(file) = fs::File::open(&path) else {
        return json!({"status":"unavailable"});
    };
    let Ok(metadata) = file.metadata() else {
        return json!({"status":"unavailable"});
    };
    if !metadata.is_file() {
        return json!({"status":"unavailable"});
    }
    if metadata.len() > MAX_JAR_BYTES {
        return json!({"status":"limited"});
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_JAR_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return json!({"status":"unavailable"});
    }
    inspect(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_file_is_read_only_and_outside_inventory_is_never_opened() {
        let root = std::env::temp_dir().join(format!(
            "shtmcl-mod-detail-{}",
            shtmcl_platform::entropy::random_hex(8).unwrap()
        ));
        let game = root.join("school");
        fs::create_dir_all(game.join("mods")).unwrap();
        fs::write(
            game.join("mods/test.jar.disabled"),
            b"synthetic invalid jar",
        )
        .unwrap();
        let mut result = json!({"instance":{"id":"school","location":{"root":game}}});
        crate::instance_presentation::enrich_snapshot(&mut result, &root);
        enrich(&mut result, &root, "test.jar.disabled");
        assert_eq!(
            result["instance"]["presentation"]["modMetadata"]["metadata"]["status"],
            "invalid"
        );
        assert_eq!(
            fs::read(game.join("mods/test.jar.disabled")).unwrap(),
            b"synthetic invalid jar"
        );
        enrich(&mut result, &root, "../outside.jar");
        assert_eq!(
            result["instance"]["presentation"]["modMetadata"]["metadata"]["status"],
            "unavailable"
        );
        let archive = include_bytes!(
            "../../../../tests/modules/04/fixtures/mod-metadata/fabric-declaration.zip"
        );
        fs::write(game.join("mods/example.jar.disabled"), archive).unwrap();
        crate::instance_presentation::enrich_snapshot(&mut result, &root);
        enrich(&mut result, &root, "example.jar.disabled");
        assert_eq!(
            result["instance"]["presentation"]["modMetadata"]["metadata"]["name"],
            "Synthetic Example"
        );
        assert_eq!(
            fs::read(game.join("mods/example.jar.disabled")).unwrap(),
            archive
        );
        assert!(!game.join("mods/fabric.mod.json").exists());
        {
            let _guard = INSPECTION.lock().unwrap();
            assert_eq!(read(&game, "example.jar.disabled")["status"], "busy");
        }
        #[cfg(unix)]
        {
            let outside = root.join("outside.jar");
            fs::write(&outside, archive).unwrap();
            std::os::unix::fs::symlink(&outside, game.join("mods/linked.jar")).unwrap();
            assert_eq!(read(&game, "linked.jar")["status"], "unavailable");
            assert_eq!(fs::read(&outside).unwrap(), archive);
        }
        let mut wrong =
            json!({"instance":{"id":"school","location":{"root":root.join("elsewhere")}}});
        crate::instance_presentation::enrich_snapshot(&mut wrong, &root);
        enrich(&mut wrong, &root, "example.jar.disabled");
        assert!(wrong["instance"]["presentation"].is_null());
        fs::remove_dir_all(root).unwrap();
    }
}
