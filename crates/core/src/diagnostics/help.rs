//! Local help entries.
//!
//! `diagnostics.help` resolves an error category, an error code or a specific
//! entry id to short local text. It never performs a network request: an
//! unnotified machine still gets the next step.

use serde_json::{json, Value};

/// Schema marker of the help payload.
pub const HELP_SCHEMA: &str = "diagnostics-help.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpEntry {
    pub id: &'static str,
    pub category: &'static str,
    pub title: &'static str,
    pub body: &'static str,
    /// The concrete next step. Never a disclaimer.
    pub next_step: &'static str,
    /// Command that performs the next step, when one exists.
    pub command: Option<&'static str>,
}

impl HelpEntry {
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "category": self.category,
            "title": self.title,
            "body": self.body,
            "nextStep": self.next_step,
            "command": self.command,
        })
    }
}

pub const HELP_ENTRIES: &[HelpEntry] = &[
    HelpEntry {
        id: "java_mismatch",
        category: "launch",
        title: "The game needs a different Java version",
        body: "The log says the classes were built for a newer or older Java than the one that started the game.",
        next_step: "Pick the Java runtime this game version lists, then start the instance again.",
        command: Some("launch.prepare"),
    },
    HelpEntry {
        id: "missing_dependency",
        category: "content",
        title: "A mod dependency is missing",
        body: "The loader stopped because another mod is required and is not installed.",
        next_step: "Install the missing dependency into this instance, then start it again.",
        command: Some("content.plan"),
    },
    HelpEntry {
        id: "duplicate_mod",
        category: "content",
        title: "The same mod is installed twice",
        body: "Two files provide the same mod id, so the loader cannot decide which one to use.",
        next_step: "Keep one of the two files and remove the other, then start the instance again.",
        command: Some("diagnostics.repair.plan"),
    },
    HelpEntry {
        id: "file_lock",
        category: "platform",
        title: "A file is still in use",
        body: "Another program holds the file open, so the launcher or the game could not write it.",
        next_step: "Close the program that owns the file (often a second game window or an editor), then retry.",
        command: None,
    },
    HelpEntry {
        id: "out_of_memory",
        category: "launch",
        title: "The game ran out of memory",
        body: "Java reached the memory limit of this instance.",
        next_step: "Raise the instance memory limit or close other applications, then start it again.",
        command: None,
    },
    HelpEntry {
        id: "logs_incomplete",
        category: "logs",
        title: "This log was not saved completely",
        body: "The log store dropped lines because the disk or the write queue could not keep up.",
        next_step: "Free disk space and export the log again for a complete record.",
        command: Some("diagnostics.export"),
    },
    HelpEntry {
        id: "export_failed",
        category: "logs",
        title: "The diagnostic bundle was not written",
        body: "The destination folder could not be written, or the bundle exceeded its size budget.",
        next_step: "Choose a writable folder, or select fewer sessions, then export again.",
        command: Some("diagnostics.export"),
    },
    HelpEntry {
        id: "update_unavailable",
        category: "delivery",
        title: "The release source is not connected",
        body: "This build has no release feed, so it cannot tell whether a newer version exists.",
        next_step: "Update this build from the place you downloaded it; automatic updates stay off until a feed is connected.",
        command: None,
    },
    HelpEntry {
        id: "update_signature",
        category: "delivery",
        title: "The update could not be verified",
        body: "The downloaded artifact did not pass the digest or the signature check.",
        next_step: "Retry the download; if it keeps failing, install the release manually.",
        command: Some("diagnostics.update.prepare"),
    },
    HelpEntry {
        id: "update_restart_pending",
        category: "delivery",
        title: "The update waits for the game to close",
        body: "A game session, a connection or an instance transaction is still active.",
        next_step: "Close the game and any active instance task; the switch runs on the next attempt.",
        command: Some("diagnostics.update.apply"),
    },
    HelpEntry {
        id: "update_recovery",
        category: "delivery",
        title: "The last update did not finish",
        body: "An update was interrupted while switching versions. The previous version is still installed.",
        next_step: "Run the recovery step; it keeps the new version only if its artifact still matches.",
        command: Some("diagnostics.update.recover"),
    },
    HelpEntry {
        id: "repair_conflict",
        category: "content",
        title: "The instance changed since the diagnosis",
        body: "The instance revision moved on, so a plan built earlier must not be applied.",
        next_step: "Run the analysis again on the current instance revision.",
        command: Some("diagnostics.analyze"),
    },
    HelpEntry {
        id: "analysis_unavailable",
        category: "logs",
        title: "There is no diagnosis for this session yet",
        body: "The finding or report has expired from the analysis cache.",
        next_step: "Run the analysis again, then open the finding from the fresh report.",
        command: Some("diagnostics.analyze"),
    },
    HelpEntry {
        id: "unknown_command",
        category: "platform",
        title: "The launcher did not recognise the action",
        body: "The command name is not part of this build's diagnostics surface.",
        next_step: "Reload the window so it uses the commands this build implements.",
        command: None,
    },
];

pub fn categories() -> Vec<&'static str> {
    let mut list: Vec<&'static str> = HELP_ENTRIES.iter().map(|entry| entry.category).collect();
    list.push("update");
    list.sort_unstable();
    list.dedup();
    list
}

/// Map a stable error code to the category a help entry is filed under.
pub fn category_of_code(code: &str) -> &'static str {
    let prefix = code.split('.').next().unwrap_or(code);
    match prefix {
        "launch" => "launch",
        "content" | "instances" => "content",
        "downloads" => "network",
        "accounts" => "account",
        "rooms" => "room",
        "servers" => "server",
        "diagnostics" => {
            if code.starts_with("diagnostics.update.") {
                "delivery"
            } else if code.starts_with("diagnostics.repair.") {
                "content"
            } else if code.starts_with("diagnostics.logs.")
                || code.starts_with("diagnostics.export.")
            {
                "logs"
            } else {
                "platform"
            }
        }
        _ => "platform",
    }
}

/// Entry ids that belong to a category.
pub fn entries_in(category: &str) -> Vec<&'static HelpEntry> {
    let wanted = if category == "update" {
        "delivery"
    } else {
        category
    };
    HELP_ENTRIES
        .iter()
        .filter(|entry| entry.category == wanted)
        .collect()
}

pub fn entry_by_id(id: &str) -> Option<&'static HelpEntry> {
    HELP_ENTRIES.iter().find(|entry| entry.id == id)
}

/// Resolve a help query. Unknown input still returns the category list so the UI
/// can offer something concrete instead of a dead end.
pub fn resolve(category: Option<&str>, entry: Option<&str>, code: Option<&str>) -> Value {
    let mut matched: Vec<&'static HelpEntry> = Vec::new();
    let mut resolved_category = category.map(str::to_string);

    if let Some(id) = entry {
        if let Some(found) = entry_by_id(id) {
            resolved_category = Some(found.category.to_string());
            matched.push(found);
        }
    }
    if let Some(code) = code {
        let from_code = category_of_code(code);
        resolved_category.get_or_insert_with(|| from_code.to_string());
        if matched.is_empty() {
            matched = entries_in(from_code);
        }
    }
    if matched.is_empty() {
        if let Some(category) = resolved_category.as_deref() {
            matched = entries_in(category);
        }
    }

    json!({
        "schema": HELP_SCHEMA,
        "category": resolved_category,
        "matched": !matched.is_empty(),
        "entries": matched.iter().map(|entry| entry.to_value()).collect::<Vec<_>>(),
        "categories": categories(),
        "offline": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_is_short_concrete_and_has_a_next_step() {
        for entry in HELP_ENTRIES {
            assert!(!entry.title.is_empty());
            assert!(
                !entry.next_step.is_empty(),
                "{} needs a concrete next step",
                entry.id
            );
            assert!(
                entry.next_step.len() <= 140,
                "{} next step is too long to read",
                entry.id
            );
            assert!(
                entry.body.len() <= 200,
                "{} body is a paragraph, not a hint",
                entry.id
            );
        }
    }

    #[test]
    fn a_log_rule_entry_is_reachable_by_id() {
        let value = resolve(Some("launch"), Some("java_mismatch"), None);
        assert_eq!(value["matched"], json!(true));
        let entries = value["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], json!("java_mismatch"));
        assert_eq!(value["offline"], json!(true));
    }

    #[test]
    fn an_error_code_maps_to_its_category() {
        assert_eq!(
            category_of_code("diagnostics.update.artifact_mismatch"),
            "delivery"
        );
        assert_eq!(category_of_code("diagnostics.logs.not_found"), "logs");
        assert_eq!(category_of_code("diagnostics.repair.conflict"), "content");
        assert_eq!(category_of_code("launch.java.not_found"), "launch");
        let value = resolve(None, None, Some("diagnostics.update.signature_unverified"));
        assert_eq!(value["category"], json!("delivery"));
        assert_eq!(value["matched"], json!(true));
    }

    #[test]
    fn an_unknown_query_still_returns_the_category_list() {
        let value = resolve(Some("nonsense"), None, None);
        assert_eq!(value["matched"], json!(false));
        assert!(value["categories"]
            .as_array()
            .map(|list| !list.is_empty())
            .unwrap_or(false));
    }
}
