//! Read-only, budgeted Fabric declaration inspection. No class loading or extraction.
use serde_json::{json, Value};

use super::{
    error::ContentError,
    zipindex::{read_entry, read_index, ZipBudget},
};

pub const MAX_JAR_BYTES: u64 = 32 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 64 * 1024;

fn status(value: &str) -> Value {
    json!({"status": value})
}
fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        !s.trim().is_empty() && s.chars().count() <= 256 && !s.chars().any(char::is_control)
    })
}

/// Return declarations, never an inferred compatibility verdict. Other loaders
/// and libraries without a Fabric manifest are explicitly unsupported.
pub fn inspect(bytes: &[u8]) -> Value {
    if bytes.len() as u64 > MAX_JAR_BYTES {
        return status("limited");
    }
    let budget = ZipBudget {
        max_entries: 8192,
        max_total_uncompressed: 512 * 1024 * 1024,
        max_entry_uncompressed: 128 * 1024 * 1024,
    };
    let index = match read_index(bytes, &budget) {
        Ok(value) => value,
        Err(ContentError::BudgetExceeded { .. }) => return status("limited"),
        Err(_) => return status("invalid"),
    };
    let entries: Vec<_> = index
        .entries
        .iter()
        .filter(|entry| entry.name == "fabric.mod.json")
        .collect();
    if entries.is_empty() {
        return status("unsupported");
    }
    if entries.len() != 1 {
        return status("invalid");
    }
    if entries[0].uncompressed_size > MAX_METADATA_BYTES {
        return status("limited");
    }
    let bytes = match read_entry(
        bytes,
        entries[0],
        &ZipBudget {
            max_entry_uncompressed: MAX_METADATA_BYTES,
            ..budget
        },
    ) {
        Ok(value) => value,
        Err(ContentError::BudgetExceeded { .. }) => return status("limited"),
        Err(_) => return status("invalid"),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => return status("invalid"),
    };
    if value["schemaVersion"] != 1 {
        return status("unsupported");
    }
    let Some(id) = text(&value["id"]) else {
        return status("invalid");
    };
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return status("invalid");
    }
    let Some(version) = text(&value["version"]) else {
        return status("invalid");
    };
    let mut result =
        json!({"status":"ready", "loader":"fabric", "id":id, "version":version, "dependencies":[]});
    if let Some(name) = value.get("name") {
        let Some(name) = text(name) else {
            return status("invalid");
        };
        result["name"] = json!(name);
    }
    if let Some(environment) = value.get("environment") {
        if !matches!(environment.as_str(), Some("*" | "client" | "server")) {
            return status("invalid");
        }
        result["environment"] = environment.clone();
    }
    if let Some(depends) = value.get("depends") {
        let Some(depends) = depends.as_object() else {
            return status("invalid");
        };
        if depends.len() > 64 {
            return status("limited");
        }
        let mut dependencies = Vec::new();
        for (id, value) in depends {
            if id.is_empty()
                || id.len() > 256
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
            {
                return status("invalid");
            }
            let versions = if let Some(version) = text(value) {
                vec![version]
            } else if let Some(values) = value.as_array() {
                if values.is_empty() || values.len() > 8 {
                    return status("limited");
                }
                let Some(versions) = values.iter().map(text).collect::<Option<Vec<_>>>() else {
                    return status("invalid");
                };
                versions
            } else {
                return status("invalid");
            };
            dependencies.push(json!({"id":id,"versions":versions}));
        }
        result["dependencies"] = json!(dependencies);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    // Minimal ZIP fixture, no third-party archive writer or actual mod code.
    pub(crate) fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = bytes.len() as u32;
            let crc = crate::content::zipindex::crc32(data);
            bytes.extend_from_slice(&0x04034b50u32.to_le_bytes());
            bytes.extend_from_slice(&20u16.to_le_bytes());
            bytes.extend_from_slice(&[0; 8]);
            bytes.extend_from_slice(&crc.to_le_bytes());
            for _ in 0..2 {
                bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
            }
            bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(data);
            central.extend_from_slice(&0x02014b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&crc.to_le_bytes());
            for _ in 0..2 {
                central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            }
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let offset = bytes.len() as u32;
        let size = central.len() as u32;
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(&0x06054b50u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        for _ in 0..2 {
            bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        }
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&[0; 2]);
        bytes
    }
    #[test]
    fn reads_deflated_fixture_without_extracting_or_executing_it() {
        let bytes = include_bytes!(
            "../../../../tests/modules/04/fixtures/mod-metadata/fabric-declaration.zip"
        );
        let value = inspect(bytes);
        assert_eq!(value["status"], "ready");
        assert_eq!(value["id"], "synthetic_example");
        assert_eq!(value["version"], "1.2.3");
        assert_eq!(value["dependencies"].as_array().unwrap().len(), 2);
        assert_eq!(inspect(&bytes[..bytes.len() - 10])["status"], "invalid");
    }
    #[test]
    fn reads_declared_identity_environment_and_version_alternatives() {
        let value = inspect(&archive(&[("fabric.mod.json", br#"{"schemaVersion":1,"id":"example","name":"Example","version":"1.2","environment":"client","depends":{"minecraft":["~1.20",">=1.21"],"fabricloader":">=0.15"}}"#)]));
        assert_eq!(value["status"], "ready");
        assert_eq!(value["name"], "Example");
        assert_eq!(value["environment"], "client");
        assert_eq!(
            value["dependencies"][1]["versions"],
            json!(["~1.20", ">=1.21"])
        );
        assert!(value.get("compatible").is_none());
    }
    #[test]
    fn separates_unknown_corrupt_and_duplicate_manifests() {
        assert_eq!(
            inspect(&archive(&[("notes.txt", b"hi")]))["status"],
            "unsupported"
        );
        assert_eq!(inspect(b"not a zip")["status"], "invalid");
        assert_eq!(
            inspect(&archive(&[
                ("fabric.mod.json", b"{}"),
                ("fabric.mod.json", b"{}")
            ]))["status"],
            "invalid"
        );
        assert_eq!(
            inspect(&archive(&[("fabric.mod.json", b"[]")]))["status"],
            "invalid"
        );
        assert_eq!(
            inspect(&archive(&[("fabric.mod.json", br#"{"schemaVersion":2}"#)]))["status"],
            "unsupported"
        );
    }
    #[test]
    fn rejects_crc_failure_malformed_fields_and_large_metadata() {
        let mut corrupt = archive(&[("fabric.mod.json", br#"{"schemaVersion":1}"#)]);
        corrupt[45] ^= 1;
        assert_eq!(inspect(&corrupt)["status"], "invalid");
        for value in [
            json!({"schemaVersion":1,"id":"../bad","version":"1"}),
            json!({"schemaVersion":1,"id":"ok","version":"1","depends":{"minecraft":17}}),
            json!({"schemaVersion":1,"id":"ok","version":"1","name":"bad\nname"}),
        ] {
            assert_eq!(
                inspect(&archive(&[(
                    "fabric.mod.json",
                    &serde_json::to_vec(&value).unwrap()
                )]))["status"],
                "invalid"
            );
        }
        assert_eq!(
            inspect(&archive(&[(
                "fabric.mod.json",
                &vec![b' '; MAX_METADATA_BYTES as usize + 1]
            )]))["status"],
            "limited"
        );
    }
}
