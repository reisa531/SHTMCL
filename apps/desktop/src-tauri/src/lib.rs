//! Tauri command bridge.
//!
//! The application owns one [`shtmcl_core::CoreApp`]: the module registry, the
//! task table and the event stream. The webview only reads snapshots/events and
//! requests actions; it never schedules work itself.
//!
//! Event delivery has two paths that share one stream:
//! - `shtmcl:event` is pushed for live updates,
//! - `get_events` replays from a saved `StreamCursor`, so a reloaded or
//!   reconnected webview can resync without trusting a push it may have missed.
//!
//! The bridge composes the core modules and owns their shared task/event stream.
//! Installed vanilla instances and explicitly selected local players feed the
//! real launch pipeline. MUA is registered separately; its authenticated launch
//! and authlib-injector bridge remain pending.

use std::io::Read;
mod directory_actions;
mod modrinth;
mod modrinth_catalog;
mod content_catalog;
mod environment_cache;
mod install_operations;
mod install_recovery;
mod instance_presentation;
mod instance_mod_inventory;
mod instance_mod_metadata;
#[cfg(test)]
mod pcl_p3_acceptance;
mod instance_copy;
mod operation_history;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use shtmcl_core::{CoreApp, CoreModule};
use shtmcl_domain::{
    CancelTaskOutcome, CancelTaskRequest, CommandRequest, CommandResponse, ErrorEnvelope,
    EventEnvelope, ModuleId, OperationId, Sequence, StreamCursor, Task, CONTRACT_VERSION,
};
use shtmcl_platform::process_std::StdProcesses;
use shtmcl_platform::{PlatformDirs, ProcessLauncher, ProcessSupervisor};
use tauri::{Emitter, Manager, State};

mod game_install;
mod fabric_metadata;
mod neoforge_metadata;
mod neoforge_sources;
mod neoforge_install;
mod neoforge_integrity;
mod neoforge_snapshot;
#[cfg(test)]
mod neoforge_launch_acceptance;
#[cfg(test)]
mod neoforge_product_acceptance;
mod neoforge_commands;
#[cfg(test)]
mod neoforge_app_acceptance;
#[cfg(test)]
mod neoforge_acquisition_acceptance;
#[cfg(test)]
mod fabric_acceptance;
#[cfg(test)]
mod pcl_p2_acceptance;
mod game_launch;
#[cfg(test)]
mod game_launch_tests;
mod game_repair;
mod game_language;
mod neoforge_publication;
mod launch_accounts;
mod launch_instances;
mod launch_lease_journal;
mod launch_leases;
mod metadata_fallback;
#[cfg(windows)]
mod portable;
#[cfg(test)]
mod repair_acceptance_tests;
#[cfg(test)]
mod restart_acceptance_tests;
pub mod runtime_export;
mod runtime_install;
mod version_metadata_source;
mod window_layout;

const EVENT_CHANNEL: &str = "shtmcl:event";
const MAX_REPLAY: usize = 256;

struct AppState {
    core: CoreApp,
    _version_metadata_source: Arc<version_metadata_source::CachedVersionMetadataSource>,
    lease_recovery: Option<Arc<launch_leases::InstanceLeases>>,
    last_lease_recovery: std::sync::Mutex<Option<std::time::Instant>>,
}

impl AppState {
    fn refresh_running_games(&self) {
        let Some(leases) = &self.lease_recovery else {
            return;
        };
        let Ok(mut last) = self.last_lease_recovery.try_lock() else {
            return;
        };
        if last.is_some_and(|time| time.elapsed() < Duration::from_secs(2)) {
            return;
        }
        *last = Some(std::time::Instant::now());
        let _ = leases.recover();
    }
    fn new(dirs: PlatformDirs) -> Self {
        let manifest_fetcher: Arc<dyn shtmcl_core::launch::catalog::ManifestFetcher> =
            match OfficialMetadataHttp::new() {
                Ok(fetcher) => Arc::new(fetcher),
                Err(_) => Arc::new(UnavailableMetadataHttp),
            };
        Self::new_with_manifest_fetcher(dirs, manifest_fetcher)
    }

    fn new_with_manifest_fetcher(
        dirs: PlatformDirs,
        manifest_fetcher: Arc<dyn shtmcl_core::launch::catalog::ManifestFetcher>,
    ) -> Self {
        let mut core = CoreApp::new(dirs);

        // Register modules with their fail-closed port defaults. A missing
        // dependency must answer with a stable error code, never with a
        // fabricated success, so the shell can be honest about what is wired.
        let context = core.context();

        // The process host owns children, pipe handoff, waiting and stopping.
        // Instance and account adapters are composed below before registration.
        let processes = Arc::new(StdProcesses::capturing());
        let launcher: Arc<dyn ProcessLauncher> = processes.clone();
        let supervisor: Arc<dyn ProcessSupervisor> = processes.clone();

        // Diagnostics owns the single log store; the launch module is the single
        // reader of game output. Wiring the store in as 02's `LogSink` keeps one
        // full ring per session instead of two, and it forwards the paging
        // capability so `launch.logs` reads the shared history.
        let diagnostics_module = shtmcl_core::diagnostics::module_with_ports(
            context.clone(),
            shtmcl_core::diagnostics::DiagnosticsPorts::unavailable(),
        );
        let log_sink = diagnostics_module.log_sink();

        let catalog_cache = context.dirs().data.join("catalog");
        let version_metadata_source =
            Arc::new(version_metadata_source::CachedVersionMetadataSource::new(
                shtmcl_core::launch::catalog::CatalogCache::new(&catalog_cache),
                manifest_fetcher.clone(),
            ));
        let catalog = Arc::new(shtmcl_core::launch::catalog::VersionCatalog::new(
            catalog_cache,
            manifest_fetcher.clone(),
        ));
        let diagnostics: Arc<dyn CoreModule> = diagnostics_module;

        let downloads_root = context.dirs().cache.join("downloads");
        let downloads: Arc<dyn CoreModule> = match shtmcl_core::downloads::HttpDownloads::new(
            &downloads_root,
            neoforge_sources::sources(),
        ) {
            Ok(transport) => {
                let transport: Arc<dyn shtmcl_core::downloads::DownloadPort> = Arc::new(transport);
                let disk: Arc<dyn shtmcl_core::downloads::DiskPort> =
                    Arc::new(shtmcl_core::downloads::FileDisk::new(&downloads_root));
                Arc::new(shtmcl_core::downloads::DownloadsModule::new(
                    context.clone(),
                    transport,
                    disk,
                    Arc::new(shtmcl_core::downloads::SystemClock),
                ))
            }
            Err(error) => {
                eprintln!("SHTMCL: downloads transport initialization failed: {error}");
                Arc::new(shtmcl_core::downloads::DownloadsModule::unavailable(
                    context.clone(),
                ))
            }
        };

        let instances: Option<Arc<dyn CoreModule>> = match shtmcl_core::instances::FileStorage::new(
            context.dirs().data.join("instance-store"),
        ) {
            Ok(storage) => match shtmcl_core::instances::InstancesModule::new(
                context.clone(),
                Box::new(storage),
            ) {
                Ok(module) => Some(Arc::new(module)),
                Err(error) => {
                    eprintln!("SHTMCL: instance store initialization failed: {error:?}");
                    None
                }
            },
            Err(error) => {
                eprintln!("SHTMCL: instance storage initialization failed: {error}");
                None
            }
        };

        let accounts: Arc<dyn CoreModule> = match shtmcl_core::accounts::MuaYggdrasilClient::new() {
            Ok(provider) => Arc::new(shtmcl_core::accounts::AccountsModule::new(
                context.clone(),
                Arc::new(provider),
            )),
            Err(_) => Arc::new(shtmcl_core::accounts::AccountsModule::unavailable(
                context.clone(),
            )),
        };

        let identities = Arc::new(launch_accounts::LaunchIdentities::new());
        let leases =
            instances.as_ref().and_then(
                |instances| match launch_leases::InstanceLeases::persistent(
                    instances.clone(),
                    context.dirs().data.join("running-games"),
                ) {
                    Ok(leases) => {
                        if let Err(error) = leases.recover() {
                            eprintln!("SHTMCL: running-game recovery: {}", error.code);
                        }
                        Some(Arc::new(leases))
                    }
                    Err(error) => {
                        eprintln!("SHTMCL: running-game journal unavailable: {}", error.code);
                        None
                    }
                },
            );
        let neoforge_checks = Arc::new(neoforge_snapshot::LaunchChecks::default());
        let instance_source: Option<Arc<dyn shtmcl_core::launch::ports::InstanceSnapshotSource>> =
            instances.as_ref().map(|instances| {
                let source = launch_instances::InstalledInstances::new(
                    instances.clone(),
                    context.dirs().clone(),
                    launch_instances::LaunchOptions {
                        auth: shtmcl_core::launch::ports::AuthRequirement::Offline,
                        offline_username: identities.selection(),
                        memory_mb: Some(2048),
                        java_path: None,
                    },
                ).with_neoforge_checks(neoforge_checks.clone());
                let source = match &leases {
                    Some(leases) => source.with_leases(leases.clone()),
                    None => source,
                };
                Arc::new(source) as Arc<dyn shtmcl_core::launch::ports::InstanceSnapshotSource>
            });
        let mut launch_ports = shtmcl_core::launch::LaunchPorts::unavailable()
            .with_process(launcher, supervisor)
            .with_logs(log_sink)
            .with_accounts(identities.clone());
        if let Some(source) = instance_source.as_ref() {
            launch_ports = launch_ports.with_instances(source.clone());
        }
        if let Some(leases) = leases.as_ref() {
            launch_ports = launch_ports.with_leases(leases.clone());
            if let Some(instances) = instances.as_ref() {
                launch_ports = launch_ports.with_resource_preparer(Arc::new(
                    game_repair::VanillaResourcePreparer::new(
                        context.clone(),
                        instances.clone(),
                        leases.clone(),
                        catalog.clone(),
                        version_metadata_source.clone(),
                        downloads.clone(),
                    ).with_neoforge_checks(neoforge_checks.clone()),
                ));
            }
        }
        let launch: Arc<dyn CoreModule> =
            Arc::new(shtmcl_core::launch::LaunchModule::with_catalog(
                context.clone(),
                launch_ports,
                catalog.clone(),
            ));
        let runtime_provider = Arc::new(runtime_install::RuntimeInstaller::new(
            context.clone(), launch.clone(), downloads.clone(), manifest_fetcher.clone(),
        ));
        let launch: Arc<dyn CoreModule> = Arc::new(game_install::GameInstaller::new(
            context.clone(),
            launch,
            catalog.clone(),
            version_metadata_source.clone(),
            downloads.clone(),
            instances.clone(),
        ));
        let launch: Arc<dyn CoreModule> = Arc::new(launch_accounts::LaunchIdentityCommands::new(
            launch, identities,
        ));
        let launch: Arc<dyn CoreModule> = Arc::new(neoforge_commands::NeoForgeCommands::new(
            context.clone(), launch, runtime_provider.clone(), version_metadata_source.clone(),
            catalog, downloads.clone(), processes, instances.clone(),
        ));
        let launch: Arc<dyn CoreModule> = Arc::new(install_recovery::InstallRecovery::new(
            context.clone(), launch, instances.clone(),
        ));
        let runtime = Arc::new(runtime_provider.with_delegate(launch));
        let launch: Arc<dyn CoreModule> = Arc::new(game_launch::GameLauncher::new(
            context.clone(),
            runtime,
            instance_source,
        ));
        let mut modules: Vec<(&str, Arc<dyn CoreModule>)> = vec![
            ("launch", launch),
            ("content", Arc::new(content_catalog::ContentCatalog::new(context.clone(), instances.clone()))),
            ("downloads", downloads),
            ("accounts", accounts),
            ("rooms", shtmcl_core::rooms::module(context.clone())),
            ("diagnostics", diagnostics),
        ];
        if let Some(instances) = instances {
            modules.push(("instances", Arc::new(instance_copy::InstanceCopies::new(instances, context.clone()))));
        }
        for (name, module) in modules {
            if let Err(error) = core.register(module) {
                // Registration is static, so a duplicate here is a programming
                // error rather than a runtime condition.
                eprintln!("SHTMCL: {name} module registration failed: {error}");
            }
        }

        Self {
            core,
            _version_metadata_source: version_metadata_source,
            lease_recovery: leases,
            last_lease_recovery: std::sync::Mutex::new(None),
        }
    }
}

struct OfficialMetadataHttp {
    client: reqwest::blocking::Client,
}

impl OfficialMetadataHttp {
    fn new() -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let target = attempt.url();
                let safe = target.scheme() == "https"
                    && target.port_or_known_default() == Some(443)
                    && target.username().is_empty()
                    && target.password().is_none()
                    && metadata_host_allowed(target.host_str().unwrap_or_default());
                if safe && attempt.previous().len() < 5 {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|_| "metadata.http.client_create_failed".to_string())?;
        Ok(Self { client })
    }
}

impl shtmcl_core::launch::catalog::ManifestFetcher for OfficialMetadataHttp {
    fn fetch(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
        metadata_fallback::fetch(url, max_bytes, |target, limit| {
            self.fetch_once(target, limit)
        })
    }
}

impl OfficialMetadataHttp {
    fn fetch_once(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
        let parsed = reqwest::Url::parse(url).map_err(|_| "metadata.url_invalid")?;
        if parsed.scheme() != "https"
            || !metadata_host_allowed(parsed.host_str().unwrap_or_default())
            || parsed.port_or_known_default() != Some(443)
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err("metadata.url_not_allowed".into());
        }

        let mut response = self
            .client
            .get(parsed)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|_| "metadata.http.request_failed")?;
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err("metadata.http.response_too_large".into());
        }
        let mut body = Vec::with_capacity(
            response.content_length().unwrap_or(0).min(max_bytes as u64) as usize,
        );
        response
            .by_ref()
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut body)
            .map_err(|_| "metadata.http.read_failed")?;
        if body.len() > max_bytes {
            return Err("metadata.http.response_too_large".into());
        }
        Ok(body)
    }
}

struct UnavailableMetadataHttp;

impl shtmcl_core::launch::catalog::ManifestFetcher for UnavailableMetadataHttp {
    fn fetch(&self, _url: &str, _max_bytes: usize) -> Result<Vec<u8>, String> {
        Err("metadata.http.unavailable".into())
    }
}

fn metadata_host_allowed(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "piston-meta.mojang.com"
            | "launchermeta.mojang.com"
            | "launcher.mojang.com"
            | "piston-data.mojang.com"
            | "bmclapi2.bangbang93.com"
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShellMode {
    Native,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShellSnapshot {
    contract_version: u16,
    mode: ShellMode,
    tasks: Vec<Task>,
    modules: Vec<ModuleId>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    name: &'static str,
    version: &'static str,
    contract_version: u16,
    native: bool,
    /// Host facts the UI must not invent: update checks and platform-specific
    /// copy read them from here instead of building their own strings.
    platform: &'static str,
    arch: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventReplayRequest {
    operation_id: OperationId,
    #[serde(default)]
    after_sequence: Option<Sequence>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventReplay {
    events: Vec<EventEnvelope>,
    cursor: StreamCursor,
    /// True when the requested window exceeded `MAX_REPLAY`; the caller must
    /// read a fresh snapshot instead of applying a partial history.
    truncated: bool,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: "SHTMCL",
        version: env!("CARGO_PKG_VERSION"),
        contract_version: CONTRACT_VERSION,
        native: true,
        platform: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

#[tauri::command]
fn get_shell_snapshot(state: State<'_, AppState>) -> ShellSnapshot {
    state.refresh_running_games();
    ShellSnapshot {
        contract_version: CONTRACT_VERSION,
        mode: ShellMode::Native,
        tasks: state.core.snapshot(),
        modules: state.core.module_ids(),
    }
}

#[tauri::command]
fn get_events(state: State<'_, AppState>, request: EventReplayRequest) -> EventReplay {
    let sink = state.core.event_sink();
    let mut events = sink.since(&request.operation_id, request.after_sequence);
    let truncated = events.len() > MAX_REPLAY;
    if truncated {
        events = events.split_off(events.len() - MAX_REPLAY);
    }
    let cursor = sink
        .cursor_after(&request.operation_id)
        .unwrap_or(StreamCursor {
            operation_id: request.operation_id,
            sequence: Sequence::ZERO,
        });

    EventReplay {
        events,
        cursor,
        truncated,
    }
}

#[tauri::command]
fn request_cancel(
    state: State<'_, AppState>,
    request: CancelTaskRequest,
) -> Result<CancelTaskOutcome, ErrorEnvelope> {
    // Cancellation is cooperative: this only records the request. A task emits
    // `cancelled` once its module reaches a safe stop or rollback point.
    Ok(state.core.request_cancel(&request))
}

#[tauri::command]
async fn dispatch_core_read(
    app: tauri::AppHandle,
    request: CommandRequest,
) -> Result<CommandResponse, ErrorEnvelope> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let module = command_module(&request)?;
        state.refresh_running_games();
        let mut response = state.core.dispatch_read(module, &request)?;
        if request.payload["command"] == "instances.list" {
            if let CommandResponse::Completed { result } = &mut response {
                instance_presentation::enrich(result, &state.core.context().dirs().instances);
            }
        }
        if request.payload["command"] == "instances.snapshot" {
            if let CommandResponse::Completed { result } = &mut response {
                instance_presentation::enrich_snapshot(result, &state.core.context().dirs().instances);
                if let Some(file_name) = request.payload["modFileName"].as_str() {
                    instance_mod_metadata::enrich(result, &state.core.context().dirs().instances, file_name);
                }
            }
        }
        Ok(response)
    }).await.map_err(|_|ErrorEnvelope::internal_unavailable())?
}

#[tauri::command]
async fn dispatch_core_mutation(
    app: tauri::AppHandle,
    request: CommandRequest,
) -> Result<CommandResponse, ErrorEnvelope> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let module = command_module(&request)?;
        state.core.dispatch_mutation(module, &request)
    }).await.map_err(|_| ErrorEnvelope::internal_unavailable())?
}

#[tauri::command]
fn open_download_destination(
    state: State<'_, AppState>,
    operation_id: String,
    task_id: String,
) -> Result<(), String> {
    // Accept identities only; the webview cannot supply an arbitrary filesystem path.
    let path = install_operations::InstallOperations::new(state.core.context().dirs())
        .destination(&operation_id, &task_id)?;
    let mut command = std::process::Command::new(directory_actions::opener()?);
    command
        .arg(path)
        .spawn()
        .map_err(|_| "installation_destination_open".to_string())?;
    Ok(())
}

fn command_module(request: &CommandRequest) -> Result<ModuleId, ErrorEnvelope> {
    let command = request.command()?;
    match command.owner() {
        "instances" => Ok(ModuleId::Instances),
        "launch" => Ok(ModuleId::Launch),
        "downloads" => Ok(ModuleId::Downloads),
        "content" => Ok(ModuleId::Content),
        "accounts" => Ok(ModuleId::Accounts),
        "rooms" => Ok(ModuleId::Rooms),
        "servers" => Ok(ModuleId::Servers),
        "diagnostics" => Ok(ModuleId::Diagnostics),
        _ => Err(ErrorEnvelope::module_unavailable()
            .with_detail_ref(format!("module:{}", command.owner()))
            .with_operation_opt(request.operation_id.clone())),
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use serde_json::json;
    use shtmcl_domain::{ErrorCode, RequestId};

    fn request(command: &str) -> CommandRequest {
        CommandRequest {
            request_id: RequestId::new("bridge-test").expect("valid request id"),
            operation_id: None,
            entity_id: None,
            expected_revision: None,
            payload: json!({ "command": command }),
        }
    }

    fn same_windows_path(left: &str, right: &str) -> bool {
        left.replace('/', "\\")
            .eq_ignore_ascii_case(&right.replace('/', "\\"))
    }

    #[test]
    fn command_namespace_maps_to_the_registered_module_id() {
        assert_eq!(
            command_module(&request("launch.runtimes.list")),
            Ok(ModuleId::Launch)
        );
        assert_eq!(
            command_module(&request("downloads.start")),
            Ok(ModuleId::Downloads)
        );
    }

    #[test]
    fn unknown_command_namespace_fails_closed() {
        let error = command_module(&request("unknown.action")).expect_err("unknown owner");
        assert_eq!(error.code.as_str(), ErrorCode::MODULE_UNAVAILABLE);
    }

    struct FixtureMetadataHttp;

    impl shtmcl_core::launch::catalog::ManifestFetcher for FixtureMetadataHttp {
        fn fetch(&self, _url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
            let bytes = br#"{"latest":{"release":"1.20","snapshot":"24w14a"},"versions":[{"id":"1.20","type":"release","url":"https://example.invalid/v.json"},{"id":"24w14a","type":"snapshot","url":"https://example.invalid/s.json"}]}"#;
            if bytes.len() > max_bytes {
                return Err("fixture exceeds limit".into());
            }
            Ok(bytes.to_vec())
        }
    }

    #[test]
    fn core_dispatch_refreshes_then_reads_the_portable_catalog() {
        use shtmcl_domain::{CommandResponse, OperationId, TaskState};
        use shtmcl_platform::PlatformDirs;

        let root = std::env::temp_dir().join(format!(
            "shtmcl-catalog-dispatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = AppState::new_with_manifest_fetcher(
            PlatformDirs::under(&root),
            Arc::new(FixtureMetadataHttp),
        );
        let mut refresh = request("launch.versions.refresh");
        refresh.operation_id = Some(OperationId::new("catalog-refresh-test").expect("operation"));
        let accepted = state
            .core
            .dispatch_mutation(ModuleId::Launch, &refresh)
            .expect("refresh accepted");
        let (task_id, operation_id) = match accepted {
            CommandResponse::Accepted {
                task_id,
                operation_id,
            } => (task_id, operation_id),
            other => panic!("expected accepted refresh, got {other:?}"),
        };

        let task = (0..250)
            .find_map(|_| {
                let task = state
                    .core
                    .snapshot()
                    .into_iter()
                    .find(|task| task.id == task_id)?;
                if matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    Some(task)
                } else {
                    std::thread::sleep(Duration::from_millis(20));
                    None
                }
            })
            .expect("refresh task reaches a terminal state");
        assert_eq!(task.state, TaskState::Succeeded);
        assert_eq!(task.operation_id, operation_id);

        let read = request("launch.versions.list");
        let response = state
            .core
            .dispatch_read(ModuleId::Launch, &read)
            .expect("cached versions returned");
        match response {
            CommandResponse::Completed { result } => {
                assert_eq!(result["total"], 2);
                assert_eq!(result["entries"][0]["id"], "1.20");
            }
            other => panic!("expected completed read, got {other:?}"),
        }
        drop(state);
        std::fs::remove_dir_all(root).expect("portable catalog cleanup");
    }

    #[test]
    #[ignore = "requires access to the official Mojang metadata endpoint"]
    fn official_manifest_refresh_uses_the_real_bounded_http_adapter() {
        use shtmcl_core::launch::catalog::VersionCatalog;

        let root = std::env::temp_dir().join(format!(
            "shtmcl-live-catalog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let fetcher = Arc::new(OfficialMetadataHttp::new().expect("HTTP client"));
        let catalog = VersionCatalog::new(&root, fetcher);
        let index = catalog.refresh().expect("validated manifest cache");
        assert!(index["counts"]["release"].as_u64().unwrap_or_default() > 0);
        let page = catalog.list(None, None, 0, 5).expect("offline cached page");
        assert_eq!(
            page["total"].as_u64(),
            index["counts"]
                .as_object()
                .map(|counts| { counts.values().filter_map(serde_json::Value::as_u64).sum() })
        );
        eprintln!("official manifest counts={}", index["counts"]);
        std::fs::remove_dir_all(root).expect("temporary cache cleanup");
    }

    #[test]
    #[ignore = "requires access to the official Mojang metadata endpoint"]
    fn official_version_details_are_pinned_cached_and_resolve_offline() {
        use shtmcl_core::launch::catalog::{
            CatalogCache, ManifestFetcher, ManifestIndex, VersionMetadata, OFFICIAL_MANIFEST_URL,
        };

        struct OfflineFetcher;
        impl shtmcl_core::launch::catalog::ManifestFetcher for OfflineFetcher {
            fn fetch(&self, _url: &str, _max_bytes: usize) -> Result<Vec<u8>, String> {
                Err("offline fixture must use the verified cache".into())
            }
        }

        struct TemporaryRoot(std::path::PathBuf);
        impl Drop for TemporaryRoot {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let root = std::env::temp_dir().join(format!(
            "shtmcl-live-version-details-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _temporary_root = TemporaryRoot(root.clone());
        let state = AppState::new(shtmcl_platform::PlatformDirs::under(&root));
        let fetcher = Arc::new(OfficialMetadataHttp::new().expect("Mojang metadata client"));
        let catalog_dir = root.join("data/catalog");
        let manifest_bytes = fetcher
            .fetch(OFFICIAL_MANIFEST_URL, 16 * 1024 * 1024)
            .expect("official version manifest");
        let cache = CatalogCache::new(&catalog_dir);
        cache
            .store(&manifest_bytes)
            .expect("validate and cache manifest");
        let index = ManifestIndex::parse(&manifest_bytes).expect("parse official manifest");
        let release = index
            .latest_release
            .clone()
            .expect("official latest release id");
        let resolved = VersionMetadata::resolve(state._version_metadata_source.as_ref(), &release)
            .expect("download SHA-1-pinned version details and resolve inheritance");
        assert_eq!(resolved.metadata.id, release);
        assert!(resolved.metadata.main_class.is_some());
        assert!(!resolved.metadata.libraries.is_empty());
        assert!(!resolved.inheritance_chain.is_empty());

        let offline = version_metadata_source::CachedVersionMetadataSource::new(
            cache,
            Arc::new(OfflineFetcher),
        );
        let offline_resolved = VersionMetadata::resolve(&offline, &release)
            .expect("resolve from SHA-1-verified portable detail cache");
        assert_eq!(offline_resolved.metadata, resolved.metadata);
        assert_eq!(
            offline_resolved.inheritance_chain,
            resolved.inheritance_chain
        );
        println!(
            "version={} inheritance={} java_major={:?} libraries={} main_class={}",
            release,
            resolved.inheritance_chain.join(" -> "),
            resolved.metadata.java_major,
            resolved.metadata.libraries.len(),
            resolved.metadata.main_class.as_deref().unwrap_or_default()
        );
        drop(state);
    }

    #[test]
    #[ignore = "requires the host JDK at C:/Program Files/Java/latest/jdk-27"]
    fn coreapp_runtime_discovery_probes_a_real_host_jdk() {
        use shtmcl_domain::{CommandResponse, OperationId, TaskState};
        use shtmcl_platform::PlatformDirs;

        let java_home = std::path::PathBuf::from(r"C:\Program Files\Java\latest\jdk-27");
        let java_exe = java_home.join("bin/java.exe");
        assert!(java_exe.is_file(), "host JDK executable is unavailable");
        let root = std::env::temp_dir().join(format!(
            "shtmcl-host-java-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = AppState::new(PlatformDirs::under(&root));
        let mut discover = request("launch.runtimes.discover");
        discover.operation_id = Some(OperationId::new("host-jdk-discovery").expect("operation"));
        discover.payload = json!({
            "command": "launch.runtimes.discover",
            "candidateRoots": [java_home.to_string_lossy()]
        });
        let accepted = state
            .core
            .dispatch_mutation(ModuleId::Launch, &discover)
            .expect("discover host Java");
        let task_id = match accepted {
            CommandResponse::Accepted { task_id, .. } => task_id,
            other => panic!("expected accepted discovery task, got {other:?}"),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let task = loop {
            if let Some(task) = state
                .core
                .snapshot()
                .into_iter()
                .find(|task| task.id == task_id)
            {
                if matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    break task;
                }
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(task.state, TaskState::Succeeded, "{task:?}");
        let report = task.result.expect("report retained");
        println!("host JDK discovery={report}");
        assert_eq!(report["candidates"].as_u64(), Some(1));
        assert!(report["runtimes"][0]["path"]
            .as_str()
            .is_some_and(|path| same_windows_path(path, &java_exe.to_string_lossy())));
        assert_eq!(report["runtimes"][0]["version"]["major"].as_u64(), Some(27));
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "downloads a pinned Java runtime file from Mojang over HTTPS"]
    fn desktop_dispatch_downloads_verifies_and_materializes_a_real_runtime_file() {
        use shtmcl_core::downloads::{ArtifactHash, FileStore};
        use shtmcl_domain::{CommandResponse, OperationId, TaskState};
        use shtmcl_platform::PlatformDirs;

        const JAVA_EXE_URL: &str = "https://piston-data.mojang.com/v1/objects/38dc560c6ff23b550a9df5b79a7de0d852e6f840/java.exe";
        const JAVA_EXE_SHA1: &str = "38dc560c6ff23b550a9df5b79a7de0d852e6f840";
        const JAVA_EXE_SIZE: u64 = 39_424;

        let root = std::env::temp_dir().join(format!(
            "shtmcl-live-download-dispatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = AppState::new(PlatformDirs::under(&root));
        let mut fetch = request("downloads.fetch");
        fetch.operation_id =
            Some(OperationId::new("runtime-file-download-test").expect("operation"));
        fetch.payload = json!({
            "command": "downloads.fetch",
            "artifact": {
                "artifactId": "java-runtime-21/bin/java.exe",
                "sourceId": "mojang-data",
                "url": JAVA_EXE_URL,
                "size": JAVA_EXE_SIZE,
                "hash": { "algorithm": "sha1", "digest": JAVA_EXE_SHA1 }
            }
        });

        let module = command_module(&fetch).expect("downloads namespace");
        let accepted = state
            .core
            .dispatch_mutation(module, &fetch)
            .expect("downloads task accepted");
        let task_id = match accepted {
            CommandResponse::Accepted { task_id, .. } => task_id,
            other => panic!("expected accepted download task, got {other:?}"),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        let task = loop {
            if let Some(task) = state
                .core
                .snapshot()
                .into_iter()
                .find(|task| task.id == task_id)
            {
                if matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    break task;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime file download did not settle within 45 seconds"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(task.state, TaskState::Succeeded, "{task:?}");

        // Read through FileStore's public content identity API instead of
        // reaching into its private object path.
        let store = FileStore::new(root.join("cache/downloads")).expect("download store");
        let hash = ArtifactHash::new("sha1", JAVA_EXE_SHA1).expect("valid SHA-1");
        let artifact_id = "runtime-java-exe-verification";
        assert_eq!(
            store
                .materialize(&hash, artifact_id)
                .expect("materialize verified cache object"),
            Some(JAVA_EXE_SIZE)
        );
        assert_eq!(
            store
                .hash_partial(artifact_id, "sha1")
                .expect("SHA-1 readback"),
            JAVA_EXE_SHA1
        );
        store
            .discard(artifact_id)
            .expect("remove verification copy");
        drop(state);
        std::fs::remove_dir_all(root).expect("temporary download cleanup");
    }

    #[test]
    #[cfg(target_os = "windows")]
    #[ignore = "downloads all official Windows x64 Java 21 files into the verified cache"]
    fn desktop_download_tasks_cache_complete_official_runtime() {
        use shtmcl_core::downloads::{ArtifactHash, FileStore};
        use shtmcl_core::launch::catalog::ManifestFetcher;
        use shtmcl_core::launch::runtime::manifest::{
            parse_install_plan, select_release, RuntimeFile, MAX_RUNTIME_CATALOG_BYTES,
            MAX_RUNTIME_MANIFEST_BYTES, RUNTIME_CATALOG_URL,
        };
        use shtmcl_domain::{CommandResponse, OperationId, TaskState};
        use shtmcl_platform::PlatformDirs;
        use std::sync::atomic::AtomicBool;

        const BATCH_SIZE: usize = 256;
        const TASK_TIMEOUT: Duration = Duration::from_secs(20 * 60);

        struct TemporaryRoot(std::path::PathBuf);
        impl Drop for TemporaryRoot {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let root = std::env::temp_dir().join(format!(
            "shtmcl-java-runtime-install-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _temporary_root = TemporaryRoot(root.clone());

        let metadata = OfficialMetadataHttp::new().expect("Mojang metadata client");
        let catalog_bytes = metadata
            .fetch(RUNTIME_CATALOG_URL, MAX_RUNTIME_CATALOG_BYTES)
            .expect("Mojang runtime catalog");
        let release =
            select_release(&catalog_bytes, "windows-x64", 21).expect("Windows x64 Java 21 runtime");
        let runtime_manifest = metadata
            .fetch(&release.manifest_url, MAX_RUNTIME_MANIFEST_BYTES)
            .expect("Java runtime file manifest");
        let plan = parse_install_plan(&runtime_manifest, &release.manifest_sha1, "windows-x64")
            .expect("validated runtime plan");
        assert!(plan
            .files
            .iter()
            .all(|file| !matches!(file, RuntimeFile::Link { .. })));

        let state = AppState::new(PlatformDirs::under(&root));
        let files = plan
            .files
            .iter()
            .filter_map(|file| match file {
                RuntimeFile::File { .. } => Some(file),
                RuntimeFile::Directory { .. } => None,
                RuntimeFile::Link { .. } => unreachable!("Windows x64 runtime has no links"),
            })
            .collect::<Vec<_>>();
        let batch_count = files.len().div_ceil(BATCH_SIZE);
        for (batch_number, batch) in files.chunks(BATCH_SIZE).enumerate() {
            let artifacts = batch
                .iter()
                .map(|file| match file {
                    RuntimeFile::File {
                        path,
                        url,
                        sha1,
                        size,
                        ..
                    } => json!({
                        "artifactId": format!("runtime/{}/{path}", release.version),
                        "sourceId": "mojang-data",
                        "url": url,
                        "size": size,
                        "hash": { "algorithm": "sha1", "digest": sha1 }
                    }),
                    _ => unreachable!("file list was filtered"),
                })
                .collect::<Vec<_>>();
            let mut fetch = request("downloads.fetch");
            fetch.operation_id = Some(
                OperationId::new(format!("runtime-install-batch-{batch_number}"))
                    .expect("operation id"),
            );
            fetch.payload = json!({ "command": "downloads.fetch", "artifacts": artifacts });
            let module = command_module(&fetch).expect("downloads command owner");
            let accepted = state
                .core
                .dispatch_mutation(module, &fetch)
                .expect("runtime file batch accepted");
            let task_id = match accepted {
                CommandResponse::Accepted { task_id, .. } => task_id,
                other => panic!("expected accepted runtime batch, got {other:?}"),
            };

            let deadline = std::time::Instant::now() + TASK_TIMEOUT;
            let task = loop {
                if let Some(task) = state
                    .core
                    .snapshot()
                    .into_iter()
                    .find(|task| task.id == task_id)
                {
                    if matches!(
                        task.state,
                        TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                    ) {
                        break task;
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "runtime batch {batch_number} did not settle within 20 minutes"
                );
                std::thread::sleep(Duration::from_millis(50));
            };
            assert_eq!(task.state, TaskState::Succeeded, "{task:?}");
            println!(
                "download batch={}/{} artifacts={} state={:?}",
                batch_number + 1,
                batch_count,
                batch.len(),
                task.state
            );
        }

        let cache = state
            .core
            .dispatch_read(ModuleId::Downloads, &request("downloads.cache.list"))
            .expect("read verified download cache");
        match cache {
            CommandResponse::Completed { result } => {
                assert!(result["count"].as_u64().unwrap_or_default() > 0);
                assert!(result["totalBytes"].as_u64().unwrap_or_default() > 50 * 1024 * 1024);
                println!(
                    "runtime={} files={} planned_bytes={} cached_count={} cached_bytes={} java={}",
                    release.version,
                    files.len(),
                    plan.total_bytes,
                    result["count"],
                    result["totalBytes"],
                    plan.java_executable
                );
            }
            other => panic!("expected completed cache snapshot, got {other:?}"),
        }

        let store = FileStore::new(root.join("cache/downloads")).expect("verified file store");
        let runtime_root = root.join("runtime");
        let staging_root = runtime_root.join(format!(".staging-{}", release.version));
        let published_root = runtime_root.join(&release.version);
        std::fs::create_dir_all(&staging_root).expect("runtime staging root");
        let not_cancelled = AtomicBool::new(false);
        let export_started = std::time::Instant::now();
        let mut exported_files = 0usize;

        for file in &files {
            let RuntimeFile::File {
                path, sha1, size, ..
            } = file
            else {
                unreachable!("file list was filtered")
            };
            let artifact_id = format!("runtime/{}/{path}", release.version);
            let hash = ArtifactHash::new("sha1", sha1).expect("validated runtime SHA-1");
            runtime_export::export_cached_artifact(
                &store,
                &hash,
                &artifact_id,
                &staging_root,
                std::path::Path::new(path),
                *size,
                &not_cancelled,
            )
            .unwrap_or_else(|error| panic!("verified runtime export failed for {path}: {error}"));
            exported_files += 1;
            if exported_files % 64 == 0 || exported_files == files.len() {
                println!("runtime export files={exported_files}/{}", files.len());
            }
        }

        let staged_java = staging_root.join(&plan.java_executable);
        let staged_check = std::process::Command::new(&staged_java)
            .arg("-version")
            .output()
            .expect("execute staged Java runtime");
        let staged_version = String::from_utf8_lossy(&staged_check.stderr);
        assert!(staged_check.status.success(), "{staged_version}");
        assert!(
            staged_version.contains(&release.version),
            "{staged_version}"
        );

        std::fs::rename(&staging_root, &published_root).expect("atomically publish Java runtime");
        let published_java = published_root.join(&plan.java_executable);
        let published_check = std::process::Command::new(&published_java)
            .arg("-version")
            .output()
            .expect("execute published Java runtime");
        let published_version = String::from_utf8_lossy(&published_check.stderr);
        assert!(published_check.status.success(), "{published_version}");
        assert!(
            published_version.contains(&release.version),
            "{published_version}"
        );

        let mut discover = request("launch.runtimes.discover");
        discover.operation_id =
            Some(OperationId::new("installed-java-runtime-discovery").expect("operation id"));
        discover.payload = json!({
            "command": "launch.runtimes.discover",
            "candidateRoots": [published_root.to_string_lossy()]
        });
        let accepted = state
            .core
            .dispatch_mutation(ModuleId::Launch, &discover)
            .expect("discover installed Java runtime");
        let task_id = match accepted {
            CommandResponse::Accepted { task_id, .. } => task_id,
            other => panic!("expected runtime discovery task, got {other:?}"),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let discovery = loop {
            if let Some(task) = state
                .core
                .snapshot()
                .into_iter()
                .find(|task| task.id == task_id)
            {
                if matches!(
                    task.state,
                    TaskState::Succeeded | TaskState::Failed | TaskState::Cancelled
                ) {
                    assert_eq!(task.state, TaskState::Succeeded, "{task:?}");
                    break task.result.expect("discovery report");
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime discovery did not settle within 60 seconds"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        println!("runtime discovery report={discovery}");
        let published_java_text = published_java.to_string_lossy().to_string();
        let discovered = discovery["runtimes"]
            .as_array()
            .and_then(|runtimes| {
                runtimes.iter().find(|runtime| {
                    runtime["path"]
                        .as_str()
                        .is_some_and(|path| same_windows_path(path, &published_java_text))
                })
            })
            .expect("published runtime is discoverable by module 02");
        assert_eq!(discovered["version"]["major"].as_u64(), Some(21));
        assert_eq!(discovered["platform"].as_str(), Some("windows"));
        assert_eq!(discovered["arch"].as_str(), Some("x64"));

        let mut list = request("launch.runtimes.list");
        list.payload = json!({
            "command": "launch.runtimes.list",
            "requirement": "21",
            "platform": "windows",
            "arch": "x64"
        });
        let selection = state
            .core
            .dispatch_read(ModuleId::Launch, &list)
            .expect("select installed Java 21 runtime");
        match selection {
            CommandResponse::Completed { result } => {
                assert!(result["runtime"]["path"]
                    .as_str()
                    .is_some_and(|path| same_windows_path(path, &published_java_text)));
                assert_eq!(result["runtime"]["version"]["major"].as_u64(), Some(21));
            }
            other => panic!("expected Java runtime selection, got {other:?}"),
        }
        println!(
            "runtime published={} files={} export_seconds={:.2} java_version={} discovery_candidates={} selected={}",
            published_root.display(),
            exported_files,
            export_started.elapsed().as_secs_f64(),
            published_version.trim(),
            discovery["candidates"],
            published_java.display()
        );
    }

    #[test]
    fn portable_core_registers_and_restores_the_real_instance_store() {
        use shtmcl_domain::{CommandResponse, OperationId};
        use shtmcl_platform::PlatformDirs;

        let root = std::env::temp_dir().join(format!(
            "shtmcl-instance-dispatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = AppState::new(PlatformDirs::under(&root));
        assert!(state.core.module_ids().contains(&ModuleId::Instances));

        let mut create = request("instances.create");
        create.operation_id = Some(OperationId::new("portable-instance-create").expect("op"));
        create.payload = json!({
            "command": "instances.create",
            "instance": {
                "id": "vanilla-test",
                "name": "Vanilla test",
                "version": { "gameVersion": "1.21.4", "loader": "vanilla" },
                "location": { "root": root.join("instances/vanilla-test").to_string_lossy() },
                "javaMajor": 21
            }
        });
        let module = command_module(&create).expect("instance namespace");
        let created = state
            .core
            .dispatch_mutation(module, &create)
            .expect("instance create");
        match created {
            CommandResponse::Completed { result } => {
                assert_eq!(result["revision"], 1);
                assert_eq!(result["instance"]["state"], "ready");
            }
            other => panic!("expected completed instance create, got {other:?}"),
        }
        drop(state);

        let restored = AppState::new(PlatformDirs::under(&root));
        let mut read = request("instances.snapshot");
        read.payload = json!({ "command": "instances.snapshot", "instanceId": "vanilla-test" });
        let snapshot = restored
            .core
            .dispatch_read(ModuleId::Instances, &read)
            .expect("restored instance snapshot");
        match snapshot {
            CommandResponse::Completed { result } => {
                assert_eq!(result["instance"]["id"], "vanilla-test");
                assert_eq!(result["instance"]["revision"], 1);
            }
            other => panic!("expected completed instance snapshot, got {other:?}"),
        }
        drop(restored);
        std::fs::remove_dir_all(root).expect("temporary instance store cleanup");
    }

    #[test]
    fn portable_core_registers_the_mua_accounts_module() {
        use shtmcl_platform::PlatformDirs;

        let root = std::env::temp_dir().join(format!(
            "shtmcl-accounts-dispatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let state = AppState::new(PlatformDirs::under(&root));
        assert!(state.core.module_ids().contains(&ModuleId::Accounts));
        drop(state);
        std::fs::remove_dir_all(root).expect("temporary accounts root cleanup");
    }

    #[test]
    #[ignore = "starts a real Minecraft window with an explicitly selected local identity"]
    fn real_installed_vanilla_launches_with_managed_java() {
        let root = std::path::PathBuf::from(
            std::env::var_os("SHTMCL_INSTALL_TEST_ROOT").expect("explicit retained root"),
        );
        let instance =
            std::env::var("SHTMCL_INSTALL_INSTANCE").expect("explicit installed instance");
        let state = AppState::new(PlatformDirs::under(&root));
        let mut identity = request("launch.identity.select");
        identity.operation_id = Some(OperationId::new("real-local-identity").unwrap());
        identity.payload =
            json!({"command":"launch.identity.select","kind":"offline","username":"SHTMCL_Test"});
        state
            .core
            .dispatch_mutation(ModuleId::Launch, &identity)
            .expect("explicit local selection");
        let mut start = request("launch.game.start");
        start.operation_id = Some(OperationId::new("real-game-start").unwrap());
        start.payload = json!({"command":"launch.game.start","instanceId":instance});
        let task_id = match state
            .core
            .dispatch_mutation(ModuleId::Launch, &start)
            .expect("start accepted")
        {
            CommandResponse::Accepted { task_id, .. } => task_id,
            _ => panic!("start task required"),
        };
        let began = std::time::Instant::now();
        let result = loop {
            let task = state.core.context().tasks().snapshot(&task_id).unwrap();
            if task.state == shtmcl_domain::TaskState::Succeeded {
                break task.result.unwrap();
            }
            assert!(
                !matches!(
                    task.state,
                    shtmcl_domain::TaskState::Failed | shtmcl_domain::TaskState::Cancelled
                ),
                "real launch failed: {task:?}"
            );
            assert!(
                began.elapsed() < Duration::from_secs(1200),
                "launch deadline"
            );
            std::thread::sleep(Duration::from_millis(250));
        };
        println!("real_launch_result={result}");
        let mut indexed = request("instances.snapshot");
        indexed.payload = json!({"command":"instances.snapshot","instanceId":instance});
        let indexed_result = state
            .core
            .dispatch_read(ModuleId::Instances, &indexed)
            .unwrap();
        if let CommandResponse::Completed { result } = &indexed_result {
            assert_eq!(
                result["instance"]["state"], "busy",
                "running game must protect the instance"
            );
            let mut update = request("instances.update");
            update.operation_id = Some(OperationId::new("attempt-edit-running-game").unwrap());
            update.expected_revision = Some(shtmcl_domain::Revision::new(
                result["instance"]["revision"].as_u64().unwrap(),
            ));
            update.payload = json!({"command":"instances.update","instanceId":instance,"patch":{"name":"Blocked edit"}});
            let refusal = state
                .core
                .dispatch_mutation(ModuleId::Instances, &update)
                .unwrap_err();
            assert_eq!(refusal.code.as_str(), "instances.instance.running");
            println!("real_running_mutation_refused={}", refusal.code);
        }
        let observation = std::env::var("SHTMCL_LAUNCH_OBSERVE_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(40)
            .min(180);
        for _ in 0..observation {
            std::thread::sleep(Duration::from_secs(1));
        }
        let mut sessions = request("launch.sessions");
        sessions.payload = json!({"command":"launch.sessions","instanceId":instance});
        let response = state
            .core
            .dispatch_read(ModuleId::Launch, &sessions)
            .expect("sessions");
        println!("real_launch_sessions={response:?}");
        // Session/log evidence is retained independently of test stdout.
        std::fs::write(
            root.join("real-launch-sessions.json"),
            serde_json::to_vec_pretty(&response).unwrap(),
        )
        .unwrap();
        if let CommandResponse::Completed { result } = response {
            let entries = result["sessions"].as_array().expect("session list");
            let session = entries.first().expect("real session");
            let mut logs = request("launch.logs");
            logs.payload =
                json!({"command":"launch.logs","sessionId":session["sessionId"],"limit":200});
            let logs = state
                .core
                .dispatch_read(ModuleId::Launch, &logs)
                .expect("logs");
            std::fs::write(
                root.join("real-launch-logs.json"),
                serde_json::to_vec_pretty(&logs).unwrap(),
            )
            .unwrap();
            assert_eq!(
                session["state"], "process_running",
                "game exited before observation: {session}"
            );
            let mut stop = request("launch.stop");
            stop.operation_id = Some(OperationId::new("real-game-stop").unwrap());
            stop.payload = json!({"command":"launch.stop","sessionId":session["sessionId"]});
            let response = state
                .core
                .dispatch_mutation(ModuleId::Launch, &stop)
                .expect("stop accepted");
            println!("real_stop={response:?}");
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let response = state
                    .core
                    .dispatch_read(ModuleId::Launch, &sessions)
                    .unwrap();
                let exited = matches!(&response, CommandResponse::Completed { result } if result["sessions"][0]["state"] == "exited");
                if exited {
                    std::fs::write(
                        root.join("real-launch-exit.json"),
                        serde_json::to_vec_pretty(&response).unwrap(),
                    )
                    .unwrap();
                    println!("real_exit={response:?}");
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "exit watcher deadline"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let response = state
                    .core
                    .dispatch_read(ModuleId::Instances, &indexed)
                    .unwrap();
                if matches!(&response, CommandResponse::Completed { result } if result["instance"]["state"] == "ready")
                {
                    println!("real_instance_ready_after_exit=true");
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "running lease was not released after real exit"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    #[test]
    #[ignore = "downloads real game resources to a fresh explicit test root; creates no instance"]
    fn real_environment_download_and_history_reopen() {
        let root = std::path::PathBuf::from(
            std::env::var_os("SHTMCL_ENVIRONMENT_TEST_ROOT").expect("fresh retained evidence root"),
        );
        assert!(
            !root.exists(),
            "test must not reuse or overwrite any existing root"
        );
        let state = AppState::new(PlatformDirs::under(&root));
        let mut download = request("launch.environments.download");
        download.operation_id = Some(OperationId::new("real-environment-download").unwrap());
        download.payload = json!({"command":"launch.environments.download","versionId":"c0.0.11a"});
        let response = state
            .core
            .dispatch_mutation(ModuleId::Launch, &download)
            .unwrap();
        let CommandResponse::Accepted { task_id, .. } = response else {
            panic!("not accepted")
        };
        let start = std::time::Instant::now();
        let mut phase = String::new();
        loop {
            let task = state.core.context().tasks().snapshot(&task_id).unwrap();
            if task.phase.as_str() != phase {
                phase = task.phase.as_str().into();
                println!("cache phase={phase} seconds={}", start.elapsed().as_secs());
            }
            if task.state.is_terminal() {
                println!(
                    "cache terminal={:?} error={:?} result={:?}",
                    task.state, task.error, task.result
                );
                assert_eq!(task.state, shtmcl_domain::TaskState::Succeeded);
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(900),
                "download deadline"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        let rows = loop {
            let rows = install_operations::InstallOperations::new(state.core.context().dirs())
                .presentations(&[])
                .unwrap();
            if rows[0]["history"]["state"] == "succeeded" {
                break rows;
            }
            assert!(start.elapsed() < Duration::from_secs(905));
            std::thread::sleep(Duration::from_millis(25));
        };
        assert_eq!(rows[0]["destination"]["kind"], "cache");
        let CommandResponse::Completed { result } = state
            .core
            .dispatch_read(ModuleId::Instances, &request("instances.list"))
            .unwrap()
        else {
            panic!("instance read")
        };
        assert_eq!(
            result["instances"].as_array().unwrap().len(),
            0,
            "download must not register an instance"
        );
        drop(state);
        std::thread::sleep(Duration::from_millis(100));
        let reopened = AppState::new(PlatformDirs::under(&root));
        let CommandResponse::Completed { result } = reopened
            .core
            .dispatch_read(ModuleId::Launch, &request("launch.installations.list"))
            .unwrap()
        else {
            panic!("history read")
        };
        assert_eq!(result["entries"][0]["history"]["state"], "succeeded");
        assert!(
            reopened.core.snapshot().is_empty(),
            "archive must not create synthetic live tasks"
        );
        let CommandResponse::Completed { result } = reopened
            .core
            .dispatch_read(ModuleId::Launch, &request("launch.environments.list"))
            .unwrap()
        else {
            panic!("cache read")
        };
        assert_eq!(result["entries"].as_array().unwrap().len(), 1);
        println!(
            "environment_root={} catalogue={result} seconds={}",
            root.display(),
            start.elapsed().as_secs_f64()
        );
    }

    #[test]
    #[ignore = "repairs a deliberately damaged object in an isolated copy of retained official cache"]
    fn real_environment_management_repairs_fixed_recipe() {
        use shtmcl_core::downloads::{ArtifactHash, FileStore};
        let root = std::path::PathBuf::from(
            std::env::var_os("SHTMCL_CACHE_MANAGEMENT_ROOT").expect("isolated fixture copy"),
        );
        assert!(
            root.starts_with(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .ancestors()
                    .nth(3)
                    .unwrap()
                    .join("work/validation/cache-management")
            ),
            "named test area only"
        );
        assert!(
            !root.join("data/catalog").exists(),
            "repair should not require a version catalog"
        );
        let recipe_path = std::fs::read_dir(root.join("cache/environments/v1"))
            .unwrap()
            .map(|item| item.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .unwrap();
        let recipe: serde_json::Value =
            serde_json::from_slice(&std::fs::read(recipe_path).unwrap()).unwrap();
        let id = recipe["id"].as_str().unwrap();
        let artifact = recipe["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|resource| resource["size"].as_u64().unwrap() > 0)
            .min_by_key(|resource| resource["size"].as_u64().unwrap())
            .unwrap();
        let digest = artifact["sha1"].as_str().unwrap();
        let path = root.join(format!(
            "cache/downloads/objects/sha1/{}/{digest}",
            &digest[..2]
        ));
        let original = std::fs::read(&path).unwrap();
        let mut damaged = original.clone();
        damaged[0] ^= 0xff;
        std::fs::write(&path, &damaged).unwrap();
        let state = AppState::new(PlatformDirs::under(&root));
        let mut command = request("launch.environments.pin");
        command.operation_id = Some(OperationId::new("pin-real-cache").unwrap());
        command.payload =
            json!({"command":"launch.environments.pin","environmentId":id,"pinned":true});
        state
            .core
            .dispatch_mutation(ModuleId::Launch, &command)
            .unwrap();
        command.payload = json!({"command":"launch.environments.remove","environmentId":id});
        assert_eq!(
            state
                .core
                .dispatch_mutation(ModuleId::Launch, &command)
                .unwrap_err()
                .code
                .as_str(),
            "launch.environment.pinned"
        );
        command.operation_id = Some(OperationId::new("repair-real-cache").unwrap());
        command.payload = json!({"command":"launch.environments.repair","environmentId":id});
        let CommandResponse::Accepted { task_id, .. } = state
            .core
            .dispatch_mutation(ModuleId::Launch, &command)
            .unwrap()
        else {
            panic!("repair not accepted")
        };
        let start = std::time::Instant::now();
        loop {
            let task = state.core.context().tasks().snapshot(&task_id).unwrap();
            if task.state.is_terminal() {
                println!("repair task={task:?}");
                assert_eq!(task.state, shtmcl_domain::TaskState::Succeeded);
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(300));
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(
            !root.join("data/catalog").exists(),
            "repair uses pinned metadata without refreshing catalogue"
        );
        let store = FileStore::new(root.join("cache/downloads")).unwrap();
        for resource in recipe["resources"].as_array().unwrap() {
            assert_eq!(
                store
                    .verify_cached(
                        &ArtifactHash::new("sha1", resource["sha1"].as_str().unwrap()).unwrap()
                    )
                    .unwrap(),
                resource["size"].as_u64()
            );
        }
        let start = std::time::Instant::now();
        loop {
            command.payload = json!({"command":"launch.environments.list"});
            command.operation_id = None;
            let CommandResponse::Completed { result } = state
                .core
                .dispatch_read(ModuleId::Launch, &command)
                .unwrap()
            else {
                panic!("list")
            };
            assert_eq!(result["entries"][0]["pinned"], true);
            command.operation_id = Some(OperationId::new("unpin-real-cache").unwrap());
            command.payload =
                json!({"command":"launch.environments.pin","environmentId":id,"pinned":false});
            state
                .core
                .dispatch_mutation(ModuleId::Launch, &command)
                .unwrap();
            command.payload = json!({"command":"launch.environments.remove","environmentId":id});
            if state
                .core
                .dispatch_mutation(ModuleId::Launch, &command)
                .is_ok()
            {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(path.exists(), "shared objects remain after recipe removal");
        println!(
            "fixed_recipe_repair root={} objects={} instance_files_untouched=true",
            root.display(),
            recipe["resources"].as_array().unwrap().len()
        );
    }

    #[test]
    #[ignore = "downloads and installs a real official vanilla game; retains evidence"]
    fn real_vanilla_install_registers_verified_game_files() {
        let version = std::env::var("SHTMCL_INSTALL_VERSION").unwrap_or_else(|_| "c0.0.11a".into());
        let instance =
            std::env::var("SHTMCL_INSTALL_INSTANCE").unwrap_or_else(|_| "real-vanilla".into());
        let root = std::env::var_os("SHTMCL_INSTALL_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("shtmcl-game-install-{}", std::process::id()))
            });
        let state = AppState::new(PlatformDirs::under(&root));
        let mut install = request("launch.install");
        let install_operation = format!("real-game-install-{instance}");
        install.operation_id = Some(OperationId::new(install_operation.clone()).unwrap());
        install.payload =
            json!({"command":"launch.install","versionId":version,"instanceId":instance});
        let accepted = state
            .core
            .dispatch_mutation(ModuleId::Launch, &install)
            .expect("installation accepted");
        let task_id = match accepted {
            CommandResponse::Accepted { task_id, .. } => task_id,
            other => panic!("expected accepted: {other:?}"),
        };
        let start = std::time::Instant::now();
        loop {
            let task = state
                .core
                .snapshot()
                .into_iter()
                .find(|t| t.id == task_id)
                .expect("installation task");
            if matches!(
                task.state,
                shtmcl_domain::TaskState::Succeeded
                    | shtmcl_domain::TaskState::Failed
                    | shtmcl_domain::TaskState::Cancelled
            ) {
                println!(
                    "install_root={} version={} seconds={} task={:?}",
                    root.display(),
                    version,
                    start.elapsed().as_secs(),
                    task
                );
                if task.state != shtmcl_domain::TaskState::Succeeded {
                    for child in state.core.snapshot() {
                        if child.error.is_some() {
                            println!(
                                "failed_task={} phase={} error={:?}",
                                child.id, child.phase, child.error
                            );
                        }
                    }
                }
                assert_eq!(task.state, shtmcl_domain::TaskState::Succeeded);
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(1200),
                "installation deadline"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        let target = root.join("instances").join(&instance);
        let records_request = request("launch.installations.list");
        let records = state
            .core
            .dispatch_read(ModuleId::Launch, &records_request)
            .expect("installation presentation query");
        let entries = match records {
            CommandResponse::Completed { result } => result["entries"].as_array().unwrap().clone(),
            _ => panic!("installation presentation must be a read"),
        };
        let record = entries
            .iter()
            .find(|entry| {
                entry["taskId"] == task_id.as_str() && entry["operationId"] == install_operation
            })
            .expect("matching operation identity");
        assert_eq!(record["destination"]["instanceId"], instance);
        assert_eq!(
            record["destination"]["path"],
            target.to_string_lossy().as_ref()
        );
        assert_eq!(record["resourceType"], "game_environment");
        assert!(
            record.get("state").is_none(),
            "presentation does not assert execution status"
        );
        assert!(target
            .join(format!("versions/{version}/{version}.jar"))
            .is_file());
        assert!(target.join("shtmcl-install.json").is_file());
        let mut snapshot = request("instances.snapshot");
        snapshot.payload = json!({"command":"instances.snapshot","instanceId":instance});
        assert!(state
            .core
            .dispatch_read(ModuleId::Instances, &snapshot)
            .is_ok());
        // Independent on-disk digest verification, not just the service result.
        use sha1::{Digest, Sha1};
        let bytes =
            std::fs::read(target.join(format!("versions/{version}/{version}.json"))).unwrap();
        let metadata =
            shtmcl_core::launch::catalog::VersionMetadata::parse(&bytes, Some(&version)).unwrap();
        let plan =
            shtmcl_core::launch::install::VanillaInstallPlan::parse(&metadata, "windows", "amd64")
                .unwrap();
        let mut artifacts = plan.artifacts.clone();
        if let Some(index) = plan.asset_index_path {
            artifacts.extend(
                shtmcl_core::launch::install::asset_artifacts(
                    &std::fs::read(target.join(index)).unwrap(),
                )
                .unwrap(),
            );
        }
        let mut total = 0u64;
        for artifact in &artifacts {
            let bytes = std::fs::read(target.join(&artifact.path)).unwrap();
            assert_eq!(bytes.len() as u64, artifact.size);
            assert_eq!(format!("{:x}", Sha1::digest(&bytes)), artifact.sha1);
            total += artifact.size;
        }
        println!(
            "verified_files={} verified_bytes={} native_archives={}",
            artifacts.len(),
            total,
            plan.native_archives.len()
        );
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    let portable_root = match portable::prepare() {
        Ok(path) => path,
        Err(()) => return,
    };

    let mut context = tauri::generate_context!();
    #[cfg(windows)]
    {
        use tauri::utils::config::{AppDirectoriesOverride, AppDirectoryOverrides};
        let directories = AppDirectoryOverrides {
            config: Some(portable_root.clone()),
            data: Some(portable_root.clone()),
            local_data: Some(portable_root.clone()),
            cache: Some(portable_root.join("cache")),
            log: Some(portable_root.join("logs")),
        };
        context.config_mut().app.app_directories_override =
            Some(AppDirectoriesOverride::Directories(directories));
    }

    tauri::Builder::default()
        .setup(move |app| {
            if let Some(window) = app.get_webview_window("main") {
                if let Err(error) = window_layout::place(&window) {
                    eprintln!("SHTMCL: initial window placement failed: {error}");
                }
                // Show even if monitor information is unavailable.
                window.show()?;
            }
            #[cfg(windows)]
            let root = portable_root.clone();
            #[cfg(not(windows))]
            let root = app.path().app_data_dir()?;
            let dirs = PlatformDirs::under(root);
            dirs.create_all()?;

            let state = AppState::new(dirs);

            // Forward every published event to the webview. The observer runs
            // after the event is retained, so a lost push is recoverable through
            // `get_events` with the last applied cursor.
            let handle = app.handle().clone();
            state
                .core
                .event_sink()
                .observe(std::sync::Arc::new(move |event: &EventEnvelope| {
                    if let Err(error) = handle.emit(EVENT_CHANNEL, event) {
                        // A closed window is normal (app exiting); the retained
                        // stream still holds the event for a later resync.
                        eprintln!("SHTMCL: event forward failed: {error}");
                    }
                }));

            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            get_shell_snapshot,
            get_events,
            request_cancel,
            dispatch_core_read,
            dispatch_core_mutation,
            open_download_destination
        ])
        .run(context)
        .expect("error while running SHTMCL desktop");
}
