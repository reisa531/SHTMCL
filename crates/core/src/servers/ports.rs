//! Ports 07 needs, and the **fail-closed** defaults.
//!
//! 07 is a coordinator: it must not open a socket itself (that is 03's and 06's
//! job) and it must not write `servers.dat` directly (01 owns instance writes).
//! Everything external goes through these traits, which is also what lets the
//! tests drive "server unreachable", "version mismatch" and a locked instance
//! without a network or a filesystem.

use std::sync::Mutex;

use crate::servers::state::{ServerAddress, ServerEntry, ServerListFile};

/// Result of a reachability/version check before joining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    /// Server answered and speaks a compatible version.
    Ready,
    /// Server answered but runs a different game version.
    VersionMismatch { expected: String, actual: String },
    /// No answer (offline, firewalled, wrong port).
    Unreachable(String),
    /// Server requires an authenticated session that is not available.
    AuthRequired,
}

/// Where published servers come from and what the directory says.
pub trait DirectoryPort: Send + Sync {
    /// Probe one address. Must not block for long: this runs while the user waits.
    fn probe(&self, address: &ServerAddress) -> Reachability;

    /// Search the public directory. Empty query lists the featured entries.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<ServerEntry>, String>;

    /// Publish a local server to the directory, returning the published id.
    fn publish(&self, entry: &ServerEntry, publisher_identity: &str) -> Result<String, String>;

    /// Remove a published entry.
    fn unpublish(&self, published_id: &str, publisher_identity: &str) -> Result<(), String>;
}

/// Instance-side access 07 needs to write the server list.
///
/// The write goes through 01's lock and revision, never around it: appending a
/// server to a directory that an install is currently writing would race.
pub trait InstancePort: Send + Sync {
    /// Whether the instance exists and is currently editable.
    fn is_editable(&self, instance_id: &str) -> Result<bool, String>;

    /// Read `servers.dat` for an instance; an absent file is an empty list.
    fn read_server_list(&self, instance_id: &str) -> Result<ServerListFile, String>;

    /// Persist `servers.dat`. Must be atomic (temp + rename).
    fn write_server_list(&self, instance_id: &str, file: &ServerListFile) -> Result<(), String>;
}

/// Default directory: every call fails.
///
/// Fail-closed: an unreachable directory must show "directory unavailable", not a
/// fabricated empty result that looks like "no servers exist".
pub struct UnavailableDirectory;

impl DirectoryPort for UnavailableDirectory {
    fn probe(&self, _address: &ServerAddress) -> Reachability {
        Reachability::Unreachable("servers.directory.unavailable".into())
    }

    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<ServerEntry>, String> {
        Err("servers.directory.unavailable".into())
    }

    fn publish(&self, _entry: &ServerEntry, _publisher_identity: &str) -> Result<String, String> {
        Err("servers.publish.unsupported".into())
    }

    fn unpublish(&self, _published_id: &str, _publisher_identity: &str) -> Result<(), String> {
        Err("servers.publish.unsupported".into())
    }
}

/// Default instance port: every write fails, so nothing is claimed that is not
/// actually on disk.
pub struct UnavailableInstances;

impl InstancePort for UnavailableInstances {
    fn is_editable(&self, _instance_id: &str) -> Result<bool, String> {
        Err("instances.unavailable".into())
    }

    fn read_server_list(&self, _instance_id: &str) -> Result<ServerListFile, String> {
        Err("instances.unavailable".into())
    }

    fn write_server_list(&self, _instance_id: &str, _file: &ServerListFile) -> Result<(), String> {
        Err("instances.unavailable".into())
    }
}

/// In-memory fixtures for tests.
pub struct FixtureDirectory {
    inner: Mutex<FixtureDirectoryState>,
}

struct FixtureDirectoryState {
    reachability: Reachability,
    directory: Vec<ServerEntry>,
    published: Vec<(String, String)>,
    search_fails: bool,
}

impl FixtureDirectory {
    pub fn new(reachability: Reachability) -> Self {
        Self {
            inner: Mutex::new(FixtureDirectoryState {
                reachability,
                directory: Vec::new(),
                published: Vec::new(),
                search_fails: false,
            }),
        }
    }

    pub fn with_directory(self, entries: Vec<ServerEntry>) -> Self {
        self.inner.lock().expect("lock").directory = entries;
        self
    }

    pub fn failing_search(self) -> Self {
        self.inner.lock().expect("lock").search_fails = true;
        self
    }

    pub fn published(&self) -> Vec<(String, String)> {
        self.inner.lock().expect("lock").published.clone()
    }
}

impl DirectoryPort for FixtureDirectory {
    fn probe(&self, _address: &ServerAddress) -> Reachability {
        self.inner.lock().expect("lock").reachability.clone()
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<ServerEntry>, String> {
        let inner = self.inner.lock().expect("lock");
        if inner.search_fails {
            return Err("servers.directory.unavailable".into());
        }
        Ok(inner
            .directory
            .iter()
            .filter(|entry| {
                query.is_empty()
                    || entry
                        .name
                        .to_ascii_lowercase()
                        .contains(&query.to_ascii_lowercase())
            })
            .take(limit)
            .cloned()
            .collect())
    }

    /// Fixture publisher: returns deterministic ids without any network call.
    ///
    /// `entry` is deliberately unused — the point of the fixture is that
    /// publishing succeeds unconditionally, so a test can assert the caller's
    /// behaviour without also modelling a directory server's validation.
    fn publish(&self, _entry: &ServerEntry, publisher: &str) -> Result<String, String> {
        let mut inner = self.inner.lock().expect("lock");
        let published_id = format!("pub-{}", inner.published.len() + 1);
        inner
            .published
            .push((published_id.clone(), publisher.to_string()));
        Ok(published_id)
    }

    fn unpublish(&self, published_id: &str, publisher: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("lock");
        let before = inner.published.len();
        inner
            .published
            .retain(|(id, who)| !(id == published_id && who == publisher));
        if inner.published.len() == before {
            return Err(format!("server:{published_id}"));
        }
        Ok(())
    }
}

/// In-memory instance store for tests.
pub struct FixtureInstances {
    inner: Mutex<FixtureInstancesState>,
}

struct FixtureInstancesState {
    files: std::collections::BTreeMap<String, ServerListFile>,
    /// Instance id -> editable. Missing means "does not exist".
    editable: std::collections::BTreeMap<String, bool>,
    writes: usize,
}

impl FixtureInstances {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FixtureInstancesState {
                files: std::collections::BTreeMap::new(),
                editable: std::collections::BTreeMap::new(),
                writes: 0,
            }),
        }
    }

    /// Register an instance; `editable = false` models a busy/locked instance.
    pub fn with_instance(self, instance_id: &str, editable: bool) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .editable
            .insert(instance_id.to_string(), editable);
        self
    }

    pub fn writes(&self) -> usize {
        self.inner.lock().expect("lock").writes
    }

    pub fn file_for(&self, instance_id: &str) -> Option<ServerListFile> {
        self.inner
            .lock()
            .expect("lock")
            .files
            .get(instance_id)
            .cloned()
    }
}

impl Default for FixtureInstances {
    fn default() -> Self {
        Self::new()
    }
}

impl InstancePort for FixtureInstances {
    fn is_editable(&self, instance_id: &str) -> Result<bool, String> {
        self.inner
            .lock()
            .expect("lock")
            .editable
            .get(instance_id)
            .copied()
            .ok_or_else(|| format!("instance:{instance_id}"))
    }

    fn read_server_list(&self, instance_id: &str) -> Result<ServerListFile, String> {
        let inner = self.inner.lock().expect("lock");
        Ok(inner
            .files
            .get(instance_id)
            .cloned()
            .unwrap_or_else(ServerListFile::new))
    }

    fn write_server_list(&self, instance_id: &str, file: &ServerListFile) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("lock");
        inner.files.insert(instance_id.to_string(), file.clone());
        inner.writes += 1;
        Ok(())
    }
}
