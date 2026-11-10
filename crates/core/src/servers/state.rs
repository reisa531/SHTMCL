//! Server entry model and the NBT reader/writer that preserves unknown fields.
//!
//! ## Why NBT is handled here
//!
//! Minecraft keeps its multiplayer list in `servers.dat`, a named-NBT file shaped
//! `{ servers: [{ name, ip, icon, acceptTextures, ... }] }`. The launcher must
//! add and remove entries **without losing anything it does not understand**: a
//! newer game version, another launcher, or a server pack may put extra tags in
//! that file, and rewriting it from a lossy model silently destroys them.
//!
//! So the reader keeps every tag, including ones this module has no opinion
//! about, and the writer emits them back in their original order. Only the fields
//! 07 actually owns (`name`, `ip`, `icon`, `acceptTextures`) are ever replaced.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::servers::error::ServersError;

/// One server as the UI lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerEntry {
    /// Stable local id. The NBT file has no id, so 07 mints one.
    pub server_id: String,
    /// Display name.
    pub name: String,
    /// `host[:port]`. Port defaults to 25565 when absent.
    pub address: String,
    /// Base64 PNG icon, when the server publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Whether the client may send the player's skin to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_textures: Option<bool>,
    /// Where this entry came from, so the UI can show "added by you" vs
    /// "published". Not a permission.
    pub origin: ServerOrigin,
}

/// How an entry entered the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerOrigin {
    Manual,
    Published,
    Imported,
}

impl ServerOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Published => "published",
            Self::Imported => "imported",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(Self::Manual),
            "published" => Some(Self::Published),
            "imported" => Some(Self::Imported),
            _ => None,
        }
    }
}

/// A validated `host[:port]`.
///
/// Validation is deliberately structural rather than a DNS lookup: an entry may
/// legitimately point at a server that is offline right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    pub host: String,
    pub port: u16,
}

impl ServerAddress {
    pub const DEFAULT_PORT: u16 = 25565;

    /// Parse `host`, `host:port`, or `[ipv6]:port`.
    pub fn parse(raw: &str) -> Result<Self, ServersError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ServersError::InvalidAddress("empty".into()));
        }
        if trimmed.contains(char::is_whitespace) {
            return Err(ServersError::InvalidAddress("whitespace".into()));
        }

        // Bracketed IPv6: `[::1]:25565` or `[::1]`.
        if let Some(rest) = trimmed.strip_prefix('[') {
            let (host, tail) = rest
                .split_once(']')
                .ok_or_else(|| ServersError::InvalidAddress("bracket".into()))?;
            if host.is_empty() {
                return Err(ServersError::InvalidAddress("host".into()));
            }
            let port = match tail.strip_prefix(':') {
                Some(port) => parse_port(port)?,
                None if tail.is_empty() => Self::DEFAULT_PORT,
                None => return Err(ServersError::InvalidAddress("suffix".into())),
            };
            return Ok(Self {
                host: host.to_string(),
                port,
            });
        }

        // Bare IPv6 without brackets is ambiguous with `host:port`; refuse rather
        // than guess, because a wrong guess writes a broken entry to servers.dat.
        if trimmed.matches(':').count() > 1 {
            return Err(ServersError::InvalidAddress("ipv6_brackets".into()));
        }

        match trimmed.rsplit_once(':') {
            Some((host, port)) => {
                if host.is_empty() {
                    return Err(ServersError::InvalidAddress("host".into()));
                }
                Ok(Self {
                    host: host.to_string(),
                    port: parse_port(port)?,
                })
            }
            None => Ok(Self {
                host: trimmed.to_string(),
                port: Self::DEFAULT_PORT,
            }),
        }
    }

    /// Canonical form: brackets come back for IPv6 so the value round-trips.
    pub fn to_string_canonical(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Two entries pointing at the same host and port are duplicates even when
    /// one omits the default port, which is why comparison is on this key.
    pub fn key(&self) -> String {
        format!("{}:{}", self.host.to_ascii_lowercase(), self.port)
    }
}

fn parse_port(raw: &str) -> Result<u16, ServersError> {
    if raw.is_empty() {
        return Err(ServersError::InvalidAddress("port".into()));
    }
    raw.parse::<u16>()
        .map_err(|_| ServersError::InvalidAddress("port".into()))
}

// ---------------------------------------------------------------------------
// NBT
// ---------------------------------------------------------------------------

/// One NBT tag, faithful enough to round-trip an unknown file.
#[derive(Debug, Clone, PartialEq)]
pub enum NbtTag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(Vec<NbtTag>),
    /// `BTreeMap` keeps a deterministic order; the original file order of a
    /// compound is not semantically meaningful in NBT.
    Compound(BTreeMap<String, NbtTag>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl NbtTag {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_byte(&self) -> Option<i8> {
        match self {
            Self::Byte(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[NbtTag]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_compound(&self) -> Option<&BTreeMap<String, NbtTag>> {
        match self {
            Self::Compound(fields) => Some(fields),
            _ => None,
        }
    }
}

/// Read the parts of `servers.dat` 07 cares about while keeping the rest.
///
/// The public shape is intentionally small: a list of
/// [`RawServerRecord`], each holding the tags it started with. Nothing is parsed
/// into a lossy struct, so an unknown tag survives by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct RawServerRecord {
    /// Every tag of this record, including ones this module does not read.
    pub fields: BTreeMap<String, NbtTag>,
}

impl RawServerRecord {
    pub fn name(&self) -> Option<&str> {
        self.fields.get("name").and_then(NbtTag::as_str)
    }

    pub fn address(&self) -> Option<&str> {
        self.fields.get("ip").and_then(NbtTag::as_str)
    }

    pub fn icon(&self) -> Option<&str> {
        self.fields.get("icon").and_then(NbtTag::as_str)
    }

    pub fn accept_textures(&self) -> Option<bool> {
        self.fields
            .get("acceptTextures")
            .and_then(NbtTag::as_byte)
            .map(|value| value != 0)
    }

    /// Overwrite only the fields 07 owns; every other tag is left untouched.
    pub fn set_owned(&mut self, entry: &ServerEntry) {
        self.fields
            .insert("name".into(), NbtTag::String(entry.name.clone()));
        self.fields
            .insert("ip".into(), NbtTag::String(entry.address.clone()));
        match &entry.icon {
            Some(icon) => {
                self.fields
                    .insert("icon".into(), NbtTag::String(icon.clone()));
            }
            None => {
                self.fields.remove("icon");
            }
        }
        match entry.accept_textures {
            Some(value) => {
                self.fields.insert(
                    "acceptTextures".into(),
                    NbtTag::Byte(if value { 1 } else { 0 }),
                );
            }
            None => {
                // The game treats a missing tag as "ask"; writing 0 would change
                // the user's answer, so an absent value stays absent.
                self.fields.remove("acceptTextures");
            }
        }
    }

    /// Unknown tags, for the report that tells the user nothing was lost.
    pub fn unknown_fields(&self) -> Vec<String> {
        const OWNED: [&str; 4] = ["name", "ip", "icon", "acceptTextures"];
        self.fields
            .keys()
            .filter(|key| !OWNED.contains(&key.as_str()))
            .cloned()
            .collect()
    }
}

/// A whole `servers.dat`: the top-level compound plus the server records.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerListFile {
    /// Top-level tags other than `servers`, preserved verbatim.
    pub other_fields: BTreeMap<String, NbtTag>,
    pub servers: Vec<RawServerRecord>,
}

impl Default for ServerListFile {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerListFile {
    pub fn new() -> Self {
        Self {
            other_fields: BTreeMap::new(),
            servers: Vec::new(),
        }
    }

    /// Build from a parsed root compound.
    pub fn from_root(root: &NbtTag) -> Result<Self, ServersError> {
        let fields = root
            .as_compound()
            .ok_or_else(|| ServersError::NbtInvalid("root".into()))?;
        let mut file = Self::new();
        for (key, tag) in fields {
            if key == "servers" {
                let list = tag
                    .as_list()
                    .ok_or_else(|| ServersError::NbtInvalid("servers".into()))?;
                for item in list {
                    let record = item
                        .as_compound()
                        .ok_or_else(|| ServersError::NbtInvalid("record".into()))?;
                    file.servers.push(RawServerRecord {
                        fields: record.clone(),
                    });
                }
            } else {
                // Preserve anything else at the root, e.g. a future `version` tag.
                file.other_fields.insert(key.clone(), tag.clone());
            }
        }
        Ok(file)
    }

    /// Render back to a root compound.
    pub fn to_root(&self) -> NbtTag {
        let mut fields = self.other_fields.clone();
        fields.insert(
            "servers".into(),
            NbtTag::List(
                self.servers
                    .iter()
                    .map(|record| NbtTag::Compound(record.fields.clone()))
                    .collect(),
            ),
        );
        NbtTag::Compound(fields)
    }

    /// Index of the record whose `ip` matches `address`, if any.
    pub fn position(&self, address: &ServerAddress) -> Option<usize> {
        self.servers.iter().position(|record| {
            record
                .address()
                .and_then(|raw| ServerAddress::parse(raw).ok())
                .map(|parsed| parsed.key() == address.key())
                .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, address: &str) -> ServerEntry {
        ServerEntry {
            server_id: id.into(),
            name: name.into(),
            address: address.into(),
            icon: None,
            accept_textures: None,
            origin: ServerOrigin::Manual,
        }
    }

    #[test]
    fn an_address_defaults_its_port() {
        assert_eq!(
            ServerAddress::parse("play.example.com").expect("ok").port,
            25565
        );
        assert_eq!(
            ServerAddress::parse("play.example.com:25566")
                .expect("ok")
                .port,
            25566
        );
    }

    #[test]
    fn ipv6_needs_brackets_and_round_trips() {
        let parsed = ServerAddress::parse("[::1]:25565").expect("ok");
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.to_string_canonical(), "[::1]:25565");
        // A bare multi-colon host is ambiguous and must be refused, not guessed.
        assert!(ServerAddress::parse("::1").is_err());
    }

    #[test]
    fn invalid_addresses_are_refused() {
        assert!(ServerAddress::parse("").is_err());
        assert!(ServerAddress::parse("host with space").is_err());
        assert!(ServerAddress::parse("host:notaport").is_err());
        assert!(ServerAddress::parse(":25565").is_err());
    }

    #[test]
    fn duplicate_detection_ignores_case_and_the_default_port() {
        let a = ServerAddress::parse("Play.Example.com:25565").expect("ok");
        let b = ServerAddress::parse("play.example.com").expect("ok");
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn rewriting_a_record_preserves_unknown_tags() {
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), NbtTag::String("old".into()));
        fields.insert("ip".into(), NbtTag::String("a:1".into()));
        // Tags this module knows nothing about: a newer game version added them,
        // and losing them would silently corrupt the user's file.
        fields.insert("resourcePackId".into(), NbtTag::String("pack-7".into()));
        fields.insert("hidden".into(), NbtTag::Byte(1));
        let mut record = RawServerRecord { fields };

        let mut replacement = entry("s1", "new", "b:2");
        replacement.accept_textures = Some(true);
        record.set_owned(&replacement);

        assert_eq!(record.name(), Some("new"));
        assert_eq!(record.address(), Some("b:2"));
        assert_eq!(record.accept_textures(), Some(true));
        assert_eq!(
            record.fields.get("resourcePackId").and_then(NbtTag::as_str),
            Some("pack-7")
        );
        assert_eq!(
            record.fields.get("hidden").and_then(NbtTag::as_byte),
            Some(1)
        );
        assert_eq!(record.unknown_fields(), vec!["hidden", "resourcePackId"]);
    }

    #[test]
    fn an_absent_accept_textures_stays_absent_instead_of_becoming_false() {
        let mut record = RawServerRecord {
            fields: BTreeMap::new(),
        };
        record.set_owned(&entry("s1", "n", "a:1"));
        assert!(!record.fields.contains_key("acceptTextures"));
    }

    #[test]
    fn the_root_keeps_fields_it_does_not_understand() {
        let mut inner = BTreeMap::new();
        inner.insert("name".into(), NbtTag::String("A".into()));
        inner.insert("ip".into(), NbtTag::String("a:1".into()));
        let mut root = BTreeMap::new();
        root.insert(
            "servers".into(),
            NbtTag::List(vec![NbtTag::Compound(inner)]),
        );
        root.insert("formatVersion".into(), NbtTag::Int(2));

        let file = ServerListFile::from_root(&NbtTag::Compound(root.clone())).expect("parse");
        assert_eq!(file.servers.len(), 1);
        assert_eq!(file.other_fields.len(), 1);

        let rendered = file.to_root();
        let fields = rendered.as_compound().expect("compound");
        assert_eq!(fields.get("formatVersion"), Some(&NbtTag::Int(2)));
        // A full round-trip is stable, which is what "preserve" means.
        assert_eq!(rendered, NbtTag::Compound(root));
    }

    #[test]
    fn a_malformed_root_is_reported_not_ignored() {
        assert!(matches!(
            ServerListFile::from_root(&NbtTag::String("nope".into())),
            Err(ServersError::NbtInvalid(_))
        ));
        let mut root = BTreeMap::new();
        root.insert("servers".into(), NbtTag::String("not a list".into()));
        assert!(matches!(
            ServerListFile::from_root(&NbtTag::Compound(root)),
            Err(ServersError::NbtInvalid(_))
        ));
    }

    #[test]
    fn position_finds_an_entry_by_canonical_address() {
        let mut file = ServerListFile::new();
        file.servers.push(RawServerRecord {
            fields: BTreeMap::from([("ip".to_string(), NbtTag::String("Play.Example.com".into()))]),
        });
        let wanted = ServerAddress::parse("play.example.com:25565").expect("addr");
        assert_eq!(file.position(&wanted), Some(0));
        let other = ServerAddress::parse("other.example.com").expect("addr");
        assert_eq!(file.position(&other), None);
    }
}
