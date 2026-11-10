//! Stable logical module identifiers and ownership metadata.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModuleId {
    Instances,
    Launch,
    Downloads,
    Content,
    Accounts,
    Rooms,
    Servers,
    Diagnostics,
}

impl ModuleId {
    pub const ALL: [Self; 8] = [
        Self::Instances,
        Self::Launch,
        Self::Downloads,
        Self::Content,
        Self::Accounts,
        Self::Rooms,
        Self::Servers,
        Self::Diagnostics,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instances => "instances",
            Self::Launch => "launch",
            Self::Downloads => "downloads",
            Self::Content => "content",
            Self::Accounts => "accounts",
            Self::Rooms => "rooms",
            Self::Servers => "servers",
            Self::Diagnostics => "diagnostics",
        }
    }

    /// Development module number from `docs/development`.
    pub const fn owner(self) -> &'static str {
        match self {
            Self::Instances => "01",
            Self::Launch => "02",
            Self::Downloads => "03",
            Self::Content => "04",
            Self::Accounts => "05",
            Self::Rooms => "06",
            Self::Servers => "07",
            Self::Diagnostics => "08",
        }
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_module_has_a_unique_owner() {
        let mut owners: Vec<&str> = ModuleId::ALL.iter().map(|module| module.owner()).collect();
        owners.sort_unstable();
        owners.dedup();
        assert_eq!(owners.len(), ModuleId::ALL.len());
    }
}
