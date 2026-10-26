//! Durable persistence of the settings snapshot and its runtime generations.
//!
//! [`ConfigStore`] owns the [`Config`] snapshot the application reads and the
//! file it is stored in. It has no GPUI dependency; [`crate::manager`] holds it
//! as process-global state.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use app_config::{
    change::ConfigChangeSet,
    network::{
        KytrixServiceCapability, NamedProxyError, NetworkRouteGeneration,
        NetworkRuntimeGenerations, ProxyRoute, ResolvedProxy, ServiceTarget,
    },
    settings::Config,
};
use market_participants_model::identifier::VenueId;
use tempfile::NamedTempFile;
use thiserror::Error;

const CONFIG_DIRECTORY_NAME: &str = "kytrix";
const CONFIG_FILE_NAME: &str = "config.json";
/// How many `-N` suffixes a backup name tries before giving up.
const MAX_BACKUP_ATTEMPTS: u32 = 1_000;

fn config_file_path(config_directory: &Path) -> PathBuf {
    config_directory
        .join(CONFIG_DIRECTORY_NAME)
        .join(CONFIG_FILE_NAME)
}

#[derive(Clone, PartialEq, Eq)]
enum ProxyFingerprint {
    Direct,
    System,
    Named(Result<ResolvedProxy, NamedProxyError>),
}

#[derive(Clone, PartialEq, Eq)]
struct EffectiveRouteFingerprint {
    proxy: ProxyFingerprint,
}

struct NetworkGenerationTracker {
    next_route_generation: u64,
    default_route: Option<(ProxyFingerprint, NetworkRouteGeneration)>,
    venues: BTreeMap<VenueId, (EffectiveRouteFingerprint, NetworkRouteGeneration)>,
    services: BTreeMap<ServiceTarget, (EffectiveRouteFingerprint, NetworkRouteGeneration)>,
}

impl NetworkGenerationTracker {
    fn new() -> Self {
        Self {
            next_route_generation: 1,
            default_route: None,
            venues: BTreeMap::new(),
            services: BTreeMap::new(),
        }
    }

    fn stamp(&mut self, config: &mut Config) {
        let mut runtime = NetworkRuntimeGenerations::default();
        let default_fingerprint = proxy_fingerprint(config, &config.proxy_default);
        let default_generation = self
            .default_route
            .as_ref()
            .filter(|(previous, _)| previous == &default_fingerprint)
            .map_or_else(
                || {
                    let generation = NetworkRouteGeneration::new(self.next_route_generation);
                    self.next_route_generation = self.next_route_generation.saturating_add(1);
                    generation
                },
                |(_, generation)| *generation,
            );
        self.default_route = Some((default_fingerprint, default_generation));
        runtime.default_route = default_generation;

        for venue in VenueId::ALL {
            let fingerprint = effective_route_fingerprint(config, &venue);
            let generation = self
                .venues
                .get(&venue)
                .filter(|(previous, _)| previous == &fingerprint)
                .map_or_else(
                    || {
                        let generation = NetworkRouteGeneration::new(self.next_route_generation);
                        self.next_route_generation = self.next_route_generation.saturating_add(1);
                        generation
                    },
                    |(_, generation)| *generation,
                );
            self.venues.insert(venue, (fingerprint, generation));
            runtime.venues.insert(venue, generation);
        }

        // Kytrix's own services are stamped apart from the venues: they are routed
        // by their own applications and a venue moving must not invalidate them.
        for target in all_network_targets(config, self.services.keys().cloned()) {
            let fingerprint = service_route_fingerprint(config, &target);
            let generation = self
                .services
                .get(&target)
                .filter(|(previous, _)| previous == &fingerprint)
                .map_or_else(
                    || {
                        let generation = NetworkRouteGeneration::new(self.next_route_generation);
                        self.next_route_generation = self.next_route_generation.saturating_add(1);
                        generation
                    },
                    |(_, generation)| *generation,
                );
            self.services
                .insert(target.clone(), (fingerprint, generation));
            runtime.services.insert(target, generation);
        }

        config.set_network_generations(runtime);
    }
}

fn all_network_targets(
    config: &Config,
    known_targets: impl IntoIterator<Item = ServiceTarget>,
) -> BTreeSet<ServiceTarget> {
    let mut targets = [
        KytrixServiceCapability::Account,
        KytrixServiceCapability::Etf,
        KytrixServiceCapability::News,
        KytrixServiceCapability::Indices,
        KytrixServiceCapability::X,
        KytrixServiceCapability::Wallet,
        KytrixServiceCapability::XMedia,
        KytrixServiceCapability::Cmc,
        KytrixServiceCapability::FinancialCalendar,
        KytrixServiceCapability::PentagonPizzaIndex,
        KytrixServiceCapability::UpdaterMetadata,
        KytrixServiceCapability::UpdaterDownload,
        KytrixServiceCapability::Ais,
    ]
    .into_iter()
    .map(|capability| ServiceTarget::KytrixService { capability })
    .collect::<BTreeSet<_>>();
    targets.extend(config.proxy_applications.keys().cloned());
    targets.extend(known_targets);
    targets
}

/// Fingerprints the route one venue's requests take.
///
/// A venue that names a line of its own carries it; a venue that names none
/// follows the default, exactly as the network snapshot answers it — so the
/// default moving rotates every venue that has no line of its own, and no venue
/// that has one.
fn effective_route_fingerprint(config: &Config, venue: &VenueId) -> EffectiveRouteFingerprint {
    let route = config
        .venue(venue)
        .and_then(|settings| settings.proxy())
        .unwrap_or_else(|| config.proxy_default.clone());
    EffectiveRouteFingerprint {
        proxy: proxy_fingerprint(config, &route),
    }
}

/// Fingerprints the route one Kytrix service takes.
fn service_route_fingerprint(config: &Config, target: &ServiceTarget) -> EffectiveRouteFingerprint {
    let route = config
        .proxy_applications
        .get(target)
        .cloned()
        .unwrap_or_else(|| config.proxy_default.clone());
    EffectiveRouteFingerprint {
        proxy: proxy_fingerprint(config, &route),
    }
}

/// Fingerprints the effective route itself.
///
/// The default route has no target, and two targets that select the same route
/// must fingerprint alike, so the target never takes part in the fingerprint.
fn proxy_fingerprint(config: &Config, route: &ProxyRoute) -> ProxyFingerprint {
    match route {
        ProxyRoute::Direct => ProxyFingerprint::Direct,
        ProxyRoute::System => ProxyFingerprint::System,
        ProxyRoute::Named { name } => {
            ProxyFingerprint::Named(config.network_config().resolve_named_proxy_entry(name))
        }
    }
}

/// Owns the persisted settings snapshot and its runtime generations.
pub struct ConfigStore {
    config: Arc<Config>,
    network_generations: NetworkGenerationTracker,
    path: Option<PathBuf>,
}

impl ConfigStore {
    /// Loads settings from `dirs::config_dir()/kytrix/config.json`, replacing an
    /// unusable file with defaults.
    ///
    /// A missing file starts with [`Config::default`]. A file that cannot be read
    /// or decoded is moved aside, replaced with [`Config::default`], and reported
    /// so the user can be told once a window exists to tell them in. Starting up
    /// is never blocked by a damaged file.
    pub fn load_default() -> (Self, Option<ConfigRecovery>) {
        let Some(config_directory) = dirs::config_dir() else {
            let error = ConfigLoadError::ConfigDirectoryUnavailable;
            log::error!("failed to load settings config: {error}");

            return (
                Self::in_memory(Config::default()),
                Some(ConfigRecovery {
                    backup: None,
                    error,
                }),
            );
        };

        Self::load_or_recover(config_file_path(&config_directory))
    }

    /// Loads settings from an injected path.
    ///
    /// A missing file starts with [`Config::default`]. Existing files that cannot
    /// be read or decoded return an error and are left untouched.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, ConfigLoadError> {
        let path = path.into();
        let config = match load_config(&path) {
            Ok(Some(config)) => config,
            Ok(None) => Config::default(),
            Err(error) => return Err(error),
        };

        Ok(Self::with_path(config, Some(path)))
    }

    /// Creates an in-memory store for injected runtimes and tests.
    pub fn in_memory(config: Config) -> Self {
        Self::with_path(config, None)
    }

    /// Returns the current immutable snapshot.
    pub fn read(&self) -> Arc<Config> {
        self.config.clone()
    }

    /// Stores `config` and reports the change set only once the file holds it.
    ///
    /// Nothing the application is running with changes until the file holds the
    /// submitted settings, so a submission that cannot be stored is reported to
    /// the caller and leaves the current snapshot exactly as it was. That is what
    /// lets the user submit the same change again. A store without a file has
    /// nothing to store and never fails.
    ///
    /// An unchanged snapshot is neither stored nor reported, and its network
    /// generations stay as they are. That is deliberate — there is nothing to
    /// store — and it has a consequence worth knowing: this is not what repairs
    /// a file that fell behind the snapshot, which is what a failed
    /// [`Self::update`] leaves behind. That failure is logged where it happens.
    pub fn write(
        &mut self,
        config: Config,
    ) -> Result<Option<ConfigChangeSet>, ConfigPersistenceError> {
        if self.config.as_ref() == &config {
            return Ok(None);
        }

        let mut candidate = config;
        self.network_generations.stamp(&mut candidate);
        // The new snapshot is the receiver and the stored snapshot is `previous`:
        // `Config::changes_since(&self /* the new snapshot */, previous)`, as the
        // signature in `settings.rs` defines it.
        let changes = candidate.changes_since(self.config.as_ref());
        self.persist(&candidate)?;
        self.config = Arc::new(candidate);

        Ok(Some(changes))
    }

    /// Mutates a cloned candidate and publishes it, persisting it afterwards.
    ///
    /// Subsystems that own a part of the settings (the account, the avatar, the
    /// workspaces) change a value nobody can be asked to submit again, so the
    /// candidate is published first and a persistence failure is only logged. A
    /// change a user submits and can submit again belongs in [`Self::write`],
    /// which reports the failure instead of dropping it.
    ///
    /// Returns the value produced by `update` and the published change set, or
    /// `None` for an unchanged candidate.
    pub fn update<R>(
        &mut self,
        update: impl FnOnce(&mut Config) -> R,
    ) -> (R, Option<ConfigChangeSet>) {
        let mut candidate = self.config.as_ref().clone();
        let result = update(&mut candidate);
        if self.config.as_ref() == &candidate {
            return (result, None);
        }

        self.network_generations.stamp(&mut candidate);
        // The new snapshot is the receiver and the stored snapshot is `previous`:
        // `Config::changes_since(&self /* the new snapshot */, previous)`, as the
        // signature in `settings.rs` defines it.
        let changes = candidate.changes_since(self.config.as_ref());
        self.config = Arc::new(candidate);
        self.persist_current();

        (result, Some(changes))
    }

    /// Wraps an already decoded snapshot, stamping its runtime generations.
    fn with_path(mut config: Config, path: Option<PathBuf>) -> Self {
        let mut network_generations = NetworkGenerationTracker::new();
        network_generations.stamp(&mut config);

        Self {
            config: Arc::new(config),
            network_generations,
            path,
        }
    }

    /// Loads a config file, falling back to defaults when it is unusable.
    ///
    /// The damaged bytes are moved aside before the replacement is written, so a
    /// start that resets the settings never destroys what the user had.
    fn load_or_recover(path: PathBuf) -> (Self, Option<ConfigRecovery>) {
        match load_config(&path) {
            Ok(Some(config)) => (Self::with_path(config, Some(path)), None),
            Ok(None) => (Self::with_path(Config::default(), Some(path)), None),
            Err(error) => {
                log::error!("failed to load settings config: {error}");
                let backup = back_up_damaged_config(&path);
                let config = Config::default();
                if let Err(error) = persist_config(&path, &config) {
                    log::error!("failed to write default settings config: {error}");
                }

                (
                    Self::with_path(config, Some(path)),
                    Some(ConfigRecovery { backup, error }),
                )
            }
        }
    }

    /// Stores `config`, or succeeds when this store has no file to store it in.
    fn persist(&self, config: &Config) -> Result<(), ConfigPersistenceError> {
        match self.path.as_deref() {
            Some(path) => persist_config(path, config),
            None => Ok(()),
        }
    }

    /// Stores the current snapshot, logging a failure instead of reporting it.
    fn persist_current(&self) {
        if let Err(error) = self.persist(self.config.as_ref()) {
            log::error!("failed to persist settings config: {error}");
        }
    }
}

/// What was recovered from when a stored config file had to be discarded.
#[derive(Debug)]
pub struct ConfigRecovery {
    /// Where the damaged file was moved, or `None` when it could not be kept.
    pub backup: Option<PathBuf>,
    /// Why the file was unusable. Logged, never shown to the user.
    pub error: ConfigLoadError,
}

/// Moves an unusable config file aside so the next start does not read it again.
///
/// Losing the old file is worse than starting over, but not bad enough to refuse
/// to start, so a failure here is logged and reported as "nothing was kept".
fn back_up_damaged_config(path: &Path) -> Option<PathBuf> {
    let Some(backup) = available_backup_path(path, time::OffsetDateTime::now_utc()) else {
        log::error!("failed to find a free backup name for damaged settings config {path:?}");

        return None;
    };

    match fs::rename(path, &backup) {
        Ok(()) => Some(backup),
        Err(error) => {
            log::error!("failed to back up damaged settings config {path:?}: {error}");

            None
        }
    }
}

/// The first free `config.json.corrupt-<timestamp>[-N]` name next to `path`.
///
/// The instant is handed in rather than read here, because the name it produces is what two
/// backups taken in the same second collide on: a caller that asked the clock itself could not
/// be tested for that collision without waiting for two calls to share a second.
fn available_backup_path(path: &Path, instant: time::OffsetDateTime) -> Option<PathBuf> {
    let directory = path.parent()?;
    let file_name = path.file_name()?.to_str()?;
    let backup_name = format!("{file_name}.corrupt-{}", utc_timestamp(instant));
    let first = directory.join(&backup_name);
    if !first.exists() {
        return Some(first);
    }

    (2..=MAX_BACKUP_ATTEMPTS)
        .map(|attempt| directory.join(format!("{backup_name}-{attempt}")))
        .find(|candidate| !candidate.exists())
}

/// The UTC instant a backup was taken, as `YYYYMMDD-HHMMSS`.
fn utc_timestamp(instant: time::OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        instant.year(),
        u8::from(instant.month()),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second()
    )
}

fn load_config(path: &Path) -> Result<Option<Config>, ConfigLoadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigLoadError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| ConfigLoadError::Decode {
            path: path.to_path_buf(),
            source,
        })
}

fn persist_config(path: &Path, config: &Config) -> Result<(), ConfigPersistenceError> {
    let directory = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    fs::create_dir_all(directory).map_err(|source| ConfigPersistenceError::CreateDirectory {
        path: directory.to_path_buf(),
        source,
    })?;

    let mut temporary_file = NamedTempFile::new_in(directory).map_err(|source| {
        ConfigPersistenceError::CreateTemporaryFile {
            path: directory.to_path_buf(),
            source,
        }
    })?;

    serde_json::to_writer_pretty(temporary_file.as_file_mut(), config).map_err(|source| {
        ConfigPersistenceError::Serialize {
            path: temporary_file.path().to_path_buf(),
            source,
        }
    })?;
    temporary_file
        .as_file_mut()
        .write_all(b"\n")
        .map_err(|source| ConfigPersistenceError::Write {
            path: temporary_file.path().to_path_buf(),
            source,
        })?;
    temporary_file
        .as_file()
        .sync_all()
        .map_err(|source| ConfigPersistenceError::Sync {
            path: temporary_file.path().to_path_buf(),
            source,
        })?;

    temporary_file
        .persist(path)
        .map(|_| ())
        .map_err(|error| ConfigPersistenceError::Replace {
            path: path.to_path_buf(),
            source: error.error,
        })
}

#[derive(Debug, Error)]
pub enum ConfigLoadError {
    #[error("the operating system config directory is unavailable")]
    ConfigDirectoryUnavailable,
    #[error("failed to read config file {path:?}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode JSON config file {path:?}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Why a settings snapshot could not be stored.
#[derive(Debug, Error)]
pub enum ConfigPersistenceError {
    #[error("failed to create config directory {path:?}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create a temporary config file in {path:?}: {source}")]
    CreateTemporaryFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize config to temporary file {path:?}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to finish writing temporary config file {path:?}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to synchronize temporary config file {path:?}: {source}")]
    Sync {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to atomically replace config file {path:?}: {source}")]
    Replace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use app_config::{
        market_participants::{
            credentials::{BinanceCredentials, OkxCredentials},
            origins::{BinanceHttpOrigin, BinanceMarket, BinanceWebsocketOrigin},
            settings::{BinanceOrigins, VenueSettings},
        },
        network::{
            KytrixServiceCapability, NamedProxyError, ProxyAuthorizationConfig, ProxyConfig,
            ProxyRoute, ServiceTarget,
        },
        settings::{AccountConfig, AccountWalletKind, Config, Language},
    };
    use market_participants_model::identifier::VenueId;
    use smol_str::SmolStr;

    use super::{
        CONFIG_DIRECTORY_NAME, CONFIG_FILE_NAME, ConfigLoadError, ConfigPersistenceError,
        ConfigStore, all_network_targets, available_backup_path, back_up_damaged_config,
        config_file_path,
    };
    use crate::test_support::test_temp;

    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the venue should be canonical")
    }

    fn x_media_target() -> ServiceTarget {
        ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::XMedia,
        }
    }

    fn store_from_path(path: impl Into<PathBuf>) -> ConfigStore {
        ConfigStore::from_path(path).expect("config store should load")
    }

    #[test]
    fn persisted_config_should_reject_unknown_routing_enums_without_rewriting_the_file() {
        let cases = [
            serde_json::json!({
                "kind": "unknown",
                "capability": "news",
            }),
            serde_json::json!({
                "kind": "route",
                "id": 1,
            }),
            serde_json::json!({
                "kind": "exchange",
                "venue": "binance",
                "market": "spot",
                "capability": "candlesticks",
            }),
            serde_json::json!({
                "kind": "kytrix_service",
                "capability": "Chat",
            }),
            serde_json::json!({
                "kind": "venue",
                "target": {
                    "venue": "Kraken",
                    "scope": "core/spot",
                    "capability": "candlesticks",
                },
            }),
            serde_json::json!({
                "kind": "venue",
                "target": {
                    "venue": "binance",
                    "scope": "Core/Spot",
                    "capability": "candlesticks",
                },
            }),
            serde_json::json!({
                "kind": "venue",
                "target": {
                    "venue": "binance",
                    "scope": "core/spot",
                    "capability": "depth",
                },
            }),
            serde_json::json!({
                "kind": "venue",
                "target": {
                    "venue": "binance",
                    "scope": "core/spot",
                },
            }),
        ];
        let temporary_directory = test_temp("config");

        for (index, target) in cases.into_iter().enumerate() {
            let mut fixture =
                serde_json::to_value(Config::default()).expect("default config should serialize");
            fixture["proxy_applications"] = serde_json::json!([{
                "target": target,
                "route": { "kind": "named", "name": "local" },
            }]);
            assert!(serde_json::from_value::<Config>(fixture.clone()).is_err());
            let bytes = serde_json::to_vec_pretty(&fixture).expect("fixture should encode");
            let path = temporary_directory
                .path()
                .join(format!("invalid-{index}.json"));
            fs::write(&path, &bytes).expect("invalid fixture should be written");

            let result = ConfigStore::from_path(&path);

            assert!(matches!(result, Err(ConfigLoadError::Decode { .. })));
            assert_eq!(
                fs::read(&path).expect("invalid config should remain readable"),
                bytes
            );
        }
    }

    #[test]
    fn a_service_target_without_an_application_should_decode() {
        let mut fixture =
            serde_json::to_value(Config::default()).expect("default config should serialize");
        fixture["proxy_applications"] = serde_json::json!([{
            "target": { "kind": "kytrix_service", "capability": "news" },
            "route": { "kind": "named", "name": "local" },
        }]);

        let decoded = serde_json::from_value::<Config>(fixture)
            .expect("a service target decodes whether or not it has an override");

        assert_eq!(decoded.proxy_applications.len(), 1);
    }

    #[test]
    fn config_file_path_should_append_kytrix_config_json() {
        let base = Path::new("base");

        let path = config_file_path(base);

        assert_eq!(path, base.join("kytrix").join("config.json"));
    }

    #[test]
    fn route_generation_should_rotate_across_an_aba() {
        let target = venue("binance");
        let mut store = ConfigStore::in_memory(Config::default());
        let initial_generation = store.read().network_route_generation(&target);

        store.update(|config| {
            config.proxies.insert("route-b".into(), local_proxy());
            config.set_venue_proxy(&target, Some(ProxyRoute::named("route-b")));
        });
        store.update(|config| {
            config.set_venue_proxy(&target, Some(ProxyRoute::Direct));
            config.proxies.remove("route-b");
        });

        assert_eq!(
            store.read().network_config().proxy_route_for(&target),
            ProxyRoute::Direct
        );

        assert_ne!(
            store.read().network_route_generation(&target),
            initial_generation
        );
    }

    #[test]
    fn equal_write_should_preserve_generations_without_publishing() {
        let target = venue("binance");
        let mut store = ConfigStore::in_memory(Config::default());
        let initial_generation = store.read().network_route_generation(&target);

        let changes = store
            .write(Config::default())
            .expect("an unchanged snapshot is accepted without being stored");

        assert!(changes.is_none());
        assert_eq!(
            store.read().network_route_generation(&target),
            initial_generation
        );
    }

    #[test]
    fn equivalent_proxy_rename_should_preserve_route_generation() {
        let target = venue("binance");
        let proxy = local_proxy();
        let mut configured = Config::default();
        configured.proxies.insert("local".into(), proxy.clone());
        configured.set_venue_proxy(&target, Some(ProxyRoute::named("local")));
        let mut store = ConfigStore::in_memory(configured);
        let initial_generation = store.read().network_route_generation(&target);

        store.update(|config| {
            config
                .replace_proxy("local", "renamed".into(), proxy)
                .expect("proxy rename should succeed");
        });

        assert_eq!(
            store.read().network_route_generation(&target),
            initial_generation
        );
    }

    #[test]
    fn proxy_definition_invalidity_and_dangling_reference_should_rotate_generation() {
        let target = venue("binance");
        let mut configured = Config::default();
        configured.proxies.insert("local".into(), local_proxy());
        configured.set_venue_proxy(&target, Some(ProxyRoute::named("local")));
        let mut store = ConfigStore::in_memory(configured);
        let mut previous = store.read().network_route_generation(&target);

        store.update(|config| config.proxies.get_mut("local").unwrap().port = 2080);
        let changed = store.read().network_route_generation(&target);
        assert_ne!(changed, previous);
        previous = changed;

        store.update(|config| config.proxies.get_mut("local").unwrap().addr = "bad host".into());
        let invalid = store.read().network_route_generation(&target);
        assert_ne!(invalid, previous);
        assert!(matches!(
            store
                .read()
                .network_config()
                .resolve_named_proxy_entry("local"),
            Err(NamedProxyError::InvalidAddress { .. })
        ));
        previous = invalid;

        store.update(|config| {
            config.proxies.remove("local");
        });
        let dangling = store.read().network_route_generation(&target);
        assert_ne!(dangling, previous);
        assert!(matches!(
            store
                .read()
                .network_config()
                .resolve_named_proxy_entry("local"),
            Err(NamedProxyError::Missing { .. })
        ));
    }

    /// The default line is stamped apart from every venue, and the venues that
    /// name a line of their own keep their stamp when it moves.
    ///
    /// A venue that shared the default's stamp would move with it, so the stamps
    /// the venues hold across the default line's own rotations are what says the
    /// two are stamped apart.
    #[test]
    fn the_default_route_generation_should_follow_the_route_it_resolves() {
        let bound = venue("binance");
        let following = venue("okx");
        let mut configured = Config::default();
        configured.proxies.insert("local".into(), local_proxy());
        configured.proxy_default = ProxyRoute::named("local");
        configured.set_venue_proxy(&bound, Some(ProxyRoute::Direct));
        let mut store = ConfigStore::in_memory(configured);
        let initial = store.read().network_route_generation(&following);
        let bound_initial = store.read().network_route_generation(&bound);

        store.update(|config| config.proxies.get_mut("local").unwrap().port = 2080);
        let rotated = store.read().network_route_generation(&following);

        assert_ne!(
            rotated, initial,
            "the default route is fingerprinted from its own configuration, without a placeholder venue"
        );
        assert_eq!(
            store.read().network_route_generation(&bound),
            bound_initial,
            "a venue that names its own route does not follow the default's rotation"
        );

        store.update(|config| config.proxies.get_mut("local").unwrap().addr = "bad host".into());
        assert_ne!(store.read().network_route_generation(&following), rotated);

        store.update(|config| config.proxy_default = ProxyRoute::Direct);
        let direct = store.read().network_route_generation(&following);
        assert_ne!(direct, rotated);
        assert_eq!(
            store.read().network_route_generation(&bound),
            bound_initial,
            "and it keeps its own stamp however the default line changes"
        );
    }

    #[test]
    fn a_meaningful_write_should_return_one_change_set() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(path);
        let expected = configured_settings();
        let binance = venue("binance");
        let initial_generation = store.read().network_route_generation(&binance);

        let changes = store
            .write(expected.clone())
            .expect("the submitted settings should be stored")
            .expect("a changed snapshot should report its change set");
        assert!(!changes.is_empty());
        assert_eq!(store.read().as_ref(), &expected);
        let rotated = store.read().network_route_generation(&binance);
        assert!(
            rotated > initial_generation,
            "the stored snapshot stamps a new generation for the venue whose route it moved: \
             {rotated:?} should follow {initial_generation:?}"
        );

        assert!(
            store
                .write(expected)
                .expect("an unchanged snapshot should be accepted again")
                .is_none(),
            "an unchanged snapshot is not published again"
        );
    }

    /// A write rotates the generation of the venue whose line it moves and leaves
    /// every other venue's generation alone, so a submission that changes no route
    /// moves nothing.
    #[test]
    fn a_write_should_rotate_only_the_route_generation_it_moves() {
        let binance = venue("binance");
        let okx = venue("okx");
        let mut store = ConfigStore::in_memory(configured_settings());
        let binance_initial = store.read().network_route_generation(&binance);
        let okx_initial = store.read().network_route_generation(&okx);

        let mut rerouted = store.read().as_ref().clone();
        assert!(rerouted.set_venue_proxy(&binance, Some(ProxyRoute::System)));
        let changes = store
            .write(rerouted)
            .expect("the rerouted settings should be stored")
            .expect("a changed snapshot should report its change set");
        assert_eq!(changes.venues, [binance]);
        let binance_rotated = store.read().network_route_generation(&binance);
        assert_ne!(
            binance_rotated, binance_initial,
            "the venue whose line moved is stamped again"
        );
        assert_eq!(
            store.read().network_route_generation(&okx),
            okx_initial,
            "another venue's line did not move, so its generation stays"
        );

        let mut unrelated = store.read().as_ref().clone();
        unrelated.language = Language::En;
        let changes = store
            .write(unrelated)
            .expect("the unrelated settings should be stored")
            .expect("a changed snapshot should report its change set");
        assert!(changes.venues.is_empty());
        assert_eq!(
            store.read().network_route_generation(&binance),
            binance_rotated,
            "a write that moves no line rotates no generation"
        );
        assert_eq!(store.read().network_route_generation(&okx), okx_initial);
    }

    /// Every Kytrix service capability is a target the store stamps, whether or not anything
    /// has configured a line of its own for it: a capability missing from the list would never
    /// have its route generation computed, so a proxy change for it would go unnoticed.
    #[test]
    fn every_kytrix_service_capability_should_have_a_network_target() {
        let targets = all_network_targets(&Config::default(), []);

        assert_eq!(targets.len(), 13);
        assert!(
            targets.contains(&ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::Cmc,
            }),
            "the CMC capability should be a stamped target"
        );
        assert!(
            targets.contains(&ServiceTarget::KytrixService {
                capability: KytrixServiceCapability::Ais,
            }),
            "the AIS capability should be a stamped target"
        );
    }

    /// Within one domain a change set names the map's keys in order: the
    /// add/edit pass first, then the removal pass, both keeping the `BTreeMap`
    /// order rather than the order the caller applied them.
    #[test]
    fn a_change_set_should_name_each_domain_in_map_order() {
        let mut store = ConfigStore::in_memory(configured_settings());
        let x_media = x_media_target();
        let account = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::Account,
        };
        let download = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::UpdaterDownload,
        };

        let mut candidate = store.read().as_ref().clone();
        candidate
            .proxy_applications
            .insert(download.clone(), ProxyRoute::System);
        candidate
            .proxy_applications
            .insert(account.clone(), ProxyRoute::Direct);
        candidate
            .proxy_applications
            .insert(x_media.clone(), ProxyRoute::Direct);
        assert!(candidate.set_venue_proxy(&venue("okx"), Some(ProxyRoute::Direct)));
        assert!(candidate.set_venue_proxy(&venue("binance"), Some(ProxyRoute::System)));

        let changes = store
            .write(candidate)
            .expect("the reordered settings should be stored")
            .expect("a changed snapshot should report its change set");

        assert_eq!(
            changes.proxy_applications,
            [account, x_media, download],
            "service routes are named in target key order, not insertion order"
        );
        assert_eq!(
            changes.venues,
            [VenueId::Binance, VenueId::Okx],
            "venues are named in venue key order, not edit order"
        );
    }

    /// A domain's removal pass follows its added and edited pass, and both keep
    /// the map's order: keys removed here sort before the added ones and still
    /// come last.
    #[test]
    fn a_change_set_should_name_removed_keys_after_the_added_and_edited_ones() {
        let account = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::Account,
        };
        let news = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::News,
        };
        let x_media = x_media_target();
        let download = ServiceTarget::KytrixService {
            capability: KytrixServiceCapability::UpdaterDownload,
        };
        let mut configured = configured_settings();
        configured
            .proxy_applications
            .insert(account.clone(), ProxyRoute::System);
        configured
            .proxy_applications
            .insert(news.clone(), ProxyRoute::System);
        let mut store = ConfigStore::in_memory(configured);

        let mut candidate = store.read().as_ref().clone();
        candidate.proxy_applications.remove(&account);
        candidate.proxy_applications.remove(&news);
        candidate
            .proxy_applications
            .insert(x_media.clone(), ProxyRoute::Direct);
        candidate
            .proxy_applications
            .insert(download.clone(), ProxyRoute::System);
        candidate.venues.remove(&venue("binance"));
        candidate.venues.remove(&venue("hyperliquid"));
        assert!(candidate.set_venue_proxy(&venue("okx"), Some(ProxyRoute::Direct)));

        let changes = store
            .write(candidate)
            .expect("the reduced settings should be stored")
            .expect("a changed snapshot should report its change set");

        assert_eq!(
            changes.proxy_applications,
            [x_media, download, account, news],
            "the edited and added service routes come in key order, then the removed ones"
        );
        assert_eq!(
            changes.venues,
            [venue("okx"), venue("binance"), venue("hyperliquid")],
            "the edited venue comes first, then the removed ones in key order"
        );
    }

    #[test]
    fn no_op_write_should_not_persist_or_publish() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(&path);

        let changes = store
            .write(Config::default())
            .expect("an unchanged snapshot should be accepted");

        assert!(changes.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn no_op_update_should_not_persist_or_publish() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(&path);

        let (result, changes) = store.update(|_| 7);

        assert_eq!(result, 7);
        assert!(changes.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn config_should_round_trip_through_json_file() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory
            .path()
            .join(CONFIG_DIRECTORY_NAME)
            .join(CONFIG_FILE_NAME);
        let expected = configured_settings();
        let mut store = store_from_path(&path);

        let _ = store
            .write(expected.clone())
            .expect("the submitted settings should be stored");
        let loaded = store_from_path(path).read();

        assert_eq!(loaded.as_ref(), &expected);
    }

    #[test]
    fn manager_should_write_pretty_json_with_a_trailing_newline() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(&path);

        let _ = store
            .write(configured_settings())
            .expect("the submitted settings should be stored");
        let contents = fs::read_to_string(path).expect("config file should be readable");

        assert!(contents.lines().count() > 2 && contents.ends_with('\n'));
    }

    #[test]
    fn manager_should_use_defaults_when_config_file_is_missing() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join("missing.json");

        let loaded = store_from_path(path).read();

        assert_eq!(loaded.as_ref(), &Config::default());
    }

    #[test]
    fn manager_should_reject_invalid_json_without_changing_the_file() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        fs::write(&path, b"{not valid JSON").expect("invalid fixture should be written");

        let result = ConfigStore::from_path(&path);

        assert!(matches!(result, Err(ConfigLoadError::Decode { .. })));
        assert_eq!(
            fs::read(path).expect("invalid fixture should remain readable"),
            b"{not valid JSON"
        );
    }

    #[test]
    fn a_damaged_config_file_should_be_backed_up_and_replaced_with_defaults() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let damaged = b"{not valid JSON";
        fs::write(&path, damaged).expect("invalid fixture should be written");

        let (store, recovery) = ConfigStore::load_or_recover(path.clone());

        let recovery = recovery.expect("a damaged file should be reported as recovered");
        assert!(matches!(recovery.error, ConfigLoadError::Decode { .. }));
        let backup = recovery.backup.expect("the damaged file should be kept");
        assert_eq!(
            fs::read(&backup).expect("the backup should be readable"),
            damaged,
            "the backup keeps exactly what the user had"
        );
        assert_eq!(store.read().as_ref(), &Config::default());
        assert_eq!(
            ConfigStore::from_path(&path)
                .expect("the replacement should load")
                .read()
                .as_ref(),
            &Config::default(),
            "the original path holds a usable config again"
        );
    }

    /// A configuration written under the previous shape is reset whole.
    ///
    /// The file is valid JSON and even names settings this build understands — a
    /// language, an appearance, the workspaces and the widget selection a user
    /// had — but it does not name the venues this build speaks, so it is not this
    /// shape. It is kept aside as a `.corrupt-*` backup and the application starts
    /// from defaults: nothing is migrated, nothing is completed from the built-in
    /// table, and every setting the old file carried is gone with it.
    #[test]
    fn an_old_configuration_is_reset_whole_and_reported() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut old = serde_json::to_value(Config::default()).expect("config should serialize");
        {
            let old = old.as_object_mut().expect("config serializes as an object");
            // The previous shape: a market-participants section and no venue map at all.
            old.remove("venues");
            old.insert(
                "exchanges".to_owned(),
                serde_json::json!({
                    "endpoints": {
                        "binance-core-http": { "http_origin": "https://api.binance.com" },
                    },
                    "credential_profiles": {
                        "binance-main": {
                            "kind": "binance",
                            "ed_25519_api_key": "old-key",
                            "ed_25519_private_key": "old-private",
                            "ed_25519_passphrase": "",
                        },
                    },
                }),
            );
            old.insert("language".to_owned(), serde_json::json!("en"));
            old.insert("appearance".to_owned(), serde_json::json!("dark"));
        }
        let old_bytes = serde_json::to_vec_pretty(&old).expect("the old file should serialize");
        fs::write(&path, &old_bytes).expect("the old fixture should be written");

        let (store, recovery) = ConfigStore::load_or_recover(path.clone());

        let recovery = recovery.expect("an old file is reported as recovered");
        assert!(
            matches!(recovery.error, ConfigLoadError::Decode { .. }),
            "the old shape is refused at decode, not read partially"
        );
        let backup = recovery.backup.expect("the old file should be kept");
        assert_eq!(
            fs::read(&backup).expect("the backup should be readable"),
            old_bytes,
            "the backup keeps exactly what the user had"
        );

        // Every setting the file carried is gone: the reset is whole rather than
        // a migration of the parts this build still understands.
        assert_eq!(store.read().as_ref(), &Config::default());
        assert_eq!(
            (
                store.read().language,
                store.read().appearance,
                store.read().workspaces.clone(),
            ),
            (
                Config::default().language,
                Config::default().appearance,
                Config::default().workspaces,
            ),
            "language, appearance, workspaces and the widget selection reset together"
        );
        assert!(
            !path
                .parent()
                .expect("the config has a directory")
                .join("")
                .as_os_str()
                .is_empty(),
            "the replacement lives where the old file did"
        );
    }

    /// A venue edit submitted the way a settings interface submits one — a mutated
    /// `Config` handed to a write-back entry — reaches the file and moves the venue's
    /// outbound generation. The dialog submits a whole applied configuration through
    /// `write`; this drives `update`, which takes the edit as a closure.
    #[test]
    fn a_venue_edit_should_persist_and_rotate_the_venue_generation() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(path.clone());
        let binance = venue("binance");
        let initial = store.read().network_route_generation(&binance);
        let initial_origin = store
            .read()
            .venue(&binance)
            .expect("the venue is configured")
            .origins("spot")
            .expect("the venue publishes the scope its requests are addressed by")
            .http;

        let (_, changes) = store.update(|config| {
            assert!(config.set_binance_market_origins(
                &binance,
                BinanceMarket::Spot,
                &BinanceOrigins::new(
                    BinanceMarket::Spot,
                    BinanceHttpOrigin::Api3,
                    BinanceWebsocketOrigin::StreamAlt,
                ),
            ));
            config.set_venue_proxy(&binance, Some(ProxyRoute::System));
        });
        assert!(changes.is_some());

        let stored = store_from_path(path).read();
        assert_ne!(
            stored
                .venue(&binance)
                .expect("the venue is configured")
                .origins("spot")
                .expect("the venue publishes the scope its requests are addressed by")
                .http,
            initial_origin,
            "the edit reached the file"
        );
        assert_eq!(
            stored
                .venue(&binance)
                .expect("the venue is configured")
                .proxy(),
            Some(ProxyRoute::System)
        );
        assert_ne!(
            store.read().network_route_generation(&binance),
            initial,
            "and its outbound route moved the venue's generation"
        );
    }

    #[test]
    fn a_backup_name_should_carry_a_timestamp_and_should_not_overwrite_an_existing_backup() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let instant = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("the fixture instant should be representable");

        let first =
            available_backup_path(&path, instant).expect("a backup name should be available");
        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some("config.json.corrupt-20231114-221320"),
            "the backup name is the damaged file's, stamped with the UTC instant it was taken"
        );
        fs::write(&first, b"kept").expect("the first backup should be written");

        // The same instant is asked about again, which is the collision the rule is about: a
        // later instant would be a free name and would say nothing about the numbering.
        let second = available_backup_path(&path, instant)
            .expect("a second backup name should be available");

        assert_ne!(first, second);
        assert!(
            second
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-2")),
            "a name the same instant already took should be numbered: {second:?}"
        );
        assert_eq!(
            fs::read(&first).expect("the first backup should remain readable"),
            b"kept",
            "an existing backup is never overwritten"
        );
    }

    #[test]
    fn a_damaged_file_that_cannot_be_moved_aside_should_be_reported_without_a_backup() {
        let temporary_directory = test_temp("config");
        let blocking_file = temporary_directory.path().join("not-a-directory");
        fs::write(&blocking_file, b"block directory creation")
            .expect("blocking file should be written");
        let path = blocking_file.join(CONFIG_FILE_NAME);

        let backup = back_up_damaged_config(&path);

        assert!(
            backup.is_none(),
            "a file that cannot be moved aside is reported, not silently dropped"
        );
    }

    #[test]
    fn a_missing_config_file_should_not_report_a_recovery() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);

        let (store, recovery) = ConfigStore::load_or_recover(path.clone());

        assert!(recovery.is_none(), "a first start is not a recovery");
        assert_eq!(store.read().as_ref(), &Config::default());
        assert!(
            !path.exists(),
            "a first start writes nothing until something changes"
        );
    }

    #[test]
    fn manager_should_replace_existing_config_on_consecutive_writes() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(&path);
        let _ = store
            .write(configured_settings())
            .expect("the submitted settings should be stored");
        let _ = store
            .write(Config::default())
            .expect("the replacement settings should be stored");

        let loaded = store_from_path(path).read();

        assert_eq!(loaded.as_ref(), &Config::default());
    }

    #[test]
    fn write_should_report_a_persistence_failure_without_publishing() {
        let temporary_directory = test_temp("config");
        let blocking_file = temporary_directory.path().join("not-a-directory");
        fs::write(&blocking_file, b"block directory creation")
            .expect("blocking file should be written");
        let path = blocking_file.join(CONFIG_FILE_NAME);
        let mut store = store_from_path(path);
        let previous = store.read();

        let result = store.write(configured_settings());

        assert!(matches!(
            result,
            Err(ConfigPersistenceError::CreateDirectory { .. })
        ));
        assert!(
            Arc::ptr_eq(&store.read(), &previous),
            "a submission that was not stored leaves the snapshot the application uses alone"
        );
    }

    #[test]
    fn write_should_retry_storing_a_snapshot_an_earlier_failure_never_published() {
        let temporary_directory = test_temp("config");
        let directory = temporary_directory.path().join("settings");
        let path = directory.join(CONFIG_FILE_NAME);
        let mut store = store_from_path(&path);
        let expected = configured_settings();
        fs::write(&directory, b"block the config directory")
            .expect("blocking file should be written");

        assert!(
            matches!(
                store.write(expected.clone()),
                Err(ConfigPersistenceError::CreateDirectory { .. })
            ),
            "the blocking file should make storing the settings fail"
        );
        fs::remove_file(&directory).expect("the blocking file should be removable");
        let _ = store
            .write(expected.clone())
            .expect("the same snapshot is submitted again after the failure is gone");

        let contents = fs::read_to_string(&path).expect("the retry should have stored a file");
        assert_eq!(
            serde_json::from_str::<Config>(&contents).expect("the stored file should decode"),
            expected,
            "a rejected submission is not treated as already written"
        );
    }

    #[test]
    fn write_should_succeed_without_a_file() {
        let mut store = ConfigStore::in_memory(Config::default());
        let expected = configured_settings();

        let _ = store
            .write(expected.clone())
            .expect("a store without a file has nothing to store");

        assert_eq!(store.read().as_ref(), &expected);
    }

    #[test]
    fn write_should_store_the_new_config_in_the_file() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(&path);
        let expected = configured_settings();

        let _ = store
            .write(expected.clone())
            .expect("the submitted settings should be stored");

        let contents = fs::read_to_string(&path).expect("the config file should be readable");
        assert_eq!(
            serde_json::from_str::<Config>(&contents).expect("the config file should decode"),
            expected,
            "the file holds the submitted settings"
        );
    }

    #[test]
    fn update_should_keep_changes_in_memory_when_persistence_fails() {
        let temporary_directory = test_temp("config");
        let blocking_file = temporary_directory.path().join("not-a-directory");
        fs::write(&blocking_file, b"block directory creation")
            .expect("blocking file should be written");
        let path = blocking_file.join(CONFIG_FILE_NAME);
        let mut store = store_from_path(path);

        let (_, changes) = store.update(|config| {
            config.proxies.insert(
                SmolStr::new_static("local"),
                ProxyConfig {
                    addr: SmolStr::new_static("127.0.0.1"),
                    port: 1080,
                    authorization: ProxyAuthorizationConfig::None,
                    resolve_dns_at_remote: true,
                },
            );
        });

        assert!(changes.is_some());
        assert!(store.read().proxies.contains_key("local"));
    }

    #[test]
    fn update_should_preserve_an_existing_arc_snapshot() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(path);
        let previous_snapshot = store.read();

        let (result, changes) = store.update(|config| {
            config.proxies.insert(
                SmolStr::new_static("local"),
                ProxyConfig {
                    addr: SmolStr::new_static("127.0.0.1"),
                    port: 1080,
                    authorization: ProxyAuthorizationConfig::None,
                    resolve_dns_at_remote: true,
                },
            );
            7
        });
        let current_snapshot = store.read();

        assert!(changes.is_some());
        assert_eq!(
            (
                previous_snapshot.proxies.len(),
                current_snapshot.proxies.len(),
                result,
            ),
            (0, 1, 7)
        );
    }

    /// Every venue that names no line of its own is stamped on the default
    /// line's route, so moving that line rotates each of them.
    #[test]
    fn changing_proxy_default_should_rotate_generation_for_venues_without_their_own_route() {
        let following = venue("binance");
        let other = venue("okx");
        let mut store = ConfigStore::in_memory(Config::default());
        let initial_following = store.read().network_route_generation(&following);
        let initial_other = store.read().network_route_generation(&other);

        store.update(|config| config.proxy_default = ProxyRoute::System);

        assert_ne!(
            store.read().network_route_generation(&following),
            initial_following,
            "a configured venue that names no line follows the default"
        );
        assert_ne!(
            store.read().network_route_generation(&other),
            initial_other,
            "and so does every other venue without a line of its own"
        );
        assert_eq!(
            store.read().network_config().proxy_route_for(&following),
            ProxyRoute::System
        );
    }

    /// A credential set of one market is what that market's requests sign with, and the
    /// file keeps the set each configured venue holds.
    #[test]
    fn a_venues_credentials_should_survive_a_stored_file() {
        let temporary_directory = test_temp("config");
        let path = temporary_directory.path().join(CONFIG_FILE_NAME);
        let mut store = store_from_path(&path);
        let _ = store
            .write(configured_settings())
            .expect("the submitted settings should be stored");

        let loaded = store_from_path(path).read();
        let binance = venue("binance");

        assert_eq!(
            loaded
                .venue(&binance)
                .expect("the venue is configured")
                .validate_credentials(binance, "spot"),
            Ok(true),
            "the spot market's own set is complete"
        );
        assert_eq!(
            loaded
                .venue(&binance)
                .expect("the venue is configured")
                .validate_credentials(binance, "usd-m-perp"),
            Ok(true),
            "and so is the perpetual market's"
        );
        assert_eq!(
            loaded
                .venues
                .iter()
                .filter(|(venue, config)| config
                    .validate_credentials(**venue, config.primary_scope())
                    .is_ok_and(|configured| configured))
                .count(),
            2,
            "the two venues the fixture configured hold a complete set"
        );
        assert_eq!(
            loaded
                .venue(&venue("hyperliquid"))
                .expect("the venue is configured")
                .validate_credentials(venue("hyperliquid"), "core-perp"),
            Ok(false),
            "and a venue the fixture did not configure holds none"
        );
    }

    #[test]
    fn an_unrelated_setting_should_not_rotate_a_generation() {
        let target = venue("binance");
        let mut store = ConfigStore::in_memory(Config::default());
        let initial_generation = store.read().network_route_generation(&target);

        let (_, changes) = store.update(|config| config.language = Language::En);

        assert!(changes.is_some());
        assert_eq!(store.read().language, Language::En);
        assert_eq!(
            store.read().network_route_generation(&target),
            initial_generation
        );
    }

    fn local_proxy() -> ProxyConfig {
        ProxyConfig {
            addr: SmolStr::new_static("127.0.0.1"),
            port: 1080,
            authorization: ProxyAuthorizationConfig::None,
            resolve_dns_at_remote: true,
        }
    }

    fn configured_settings() -> Config {
        let mut config = Config::default();
        config.account = Some(AccountConfig {
            user_id: "11111111-1111-4111-8111-111111111111".into(),
            wallet_address: "0x0123456789abcdef".into(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config.avatar = Some("<svg>configured-avatar</svg>".to_owned());
        config.proxies.insert(
            SmolStr::new_static("local"),
            ProxyConfig {
                addr: SmolStr::new_static("127.0.0.1"),
                port: 1080,
                authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                    username: SmolStr::new_static("proxy-user"),
                    password: SmolStr::new_static("proxy-password"),
                },
                resolve_dns_at_remote: true,
            },
        );
        config
            .proxy_applications
            .extend([(x_media_target(), ProxyRoute::named("local"))]);
        config.set_venue_proxy(&venue("binance"), Some(ProxyRoute::named("local")));
        config.set_venue_proxy(&venue("okx"), Some(ProxyRoute::named("local")));
        {
            let VenueSettings::Binance(binance) = config
                .venues
                .get_mut(&venue("binance"))
                .expect("binance is a built-in venue")
            else {
                panic!("binance is configured as Binance");
            };
            binance.credentials = BinanceCredentials {
                spot_api_key: SmolStr::new_static("binance-spot-key"),
                spot_private_key: SmolStr::new_static("binance-spot-private"),
                spot_passphrase: SmolStr::new_static("binance-spot-passphrase"),
                usd_m_perp_api_key: SmolStr::new_static("binance-perp-key"),
                usd_m_perp_private_key: SmolStr::new_static("binance-perp-private"),
                usd_m_perp_passphrase: SmolStr::new_static("binance-perp-passphrase"),
            };
        }
        {
            let VenueSettings::Okx(okx) = config
                .venues
                .get_mut(&venue("okx"))
                .expect("okx is a built-in venue")
            else {
                panic!("okx is configured as OKX");
            };
            okx.credentials = OkxCredentials {
                api_key: SmolStr::new_static("okx-api-key"),
                secret_key: SmolStr::new_static("okx-secret-key"),
                passphrase: SmolStr::new_static("okx-passphrase"),
            };
        }

        config
    }
}
