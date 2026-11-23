use std::{error::Error as _, io, path::PathBuf};

use app_config::{
    network::{KytrixServiceCapability, ProxyConfig, ProxyRoute, ServiceTarget},
    settings::{AccountConfig, AccountWalletKind, Appearance, Language},
    translation::{DownloadSite, TranslationModel},
};
use app_config_provider::store::ConfigPersistenceError;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use crate::{session::SettingsChange, settings_tests::test_support::Harness};

#[test]
fn a_change_should_reach_the_settings_at_once() {
    let mut harness = Harness::new();

    harness
        .write(SettingsChange::edit(|config| {
            config.language = Language::ZhCn
        }))
        .expect("an in-memory session stores every settings change");

    assert_eq!(
        harness.config().language,
        Language::ZhCn,
        "the change should be the settings of the moment it was written"
    );
    assert_eq!(
        harness.writes().len(),
        1,
        "one write publishes one snapshot"
    );
}

#[test]
fn a_change_should_keep_what_was_written_while_the_page_was_open() {
    let mut harness = Harness::new();
    harness.update_config(|config| {
        config.avatar = Some("<svg>generated-while-open</svg>".to_owned());
        config.account = Some(AccountConfig {
            user_id: "22222222-2222-4222-8222-222222222222".to_owned(),
            wallet_address: "0xfeed".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
    });

    harness
        .write(SettingsChange::edit(|config| {
            config.appearance = Appearance::Dark
        }))
        .expect("an in-memory session stores every settings change");

    let stored = harness.config();
    assert_eq!(stored.appearance, Appearance::Dark);
    assert_eq!(
        stored.avatar.as_deref(),
        Some("<svg>generated-while-open</svg>"),
        "a change must not roll back the avatar another subsystem wrote"
    );
    assert_eq!(
        stored
            .account
            .as_ref()
            .map(|account| account.user_id.as_str()),
        Some("22222222-2222-4222-8222-222222222222"),
        "a change must not roll back the account another subsystem signed in"
    );
}

fn proxy(port: u16) -> ProxyConfig {
    ProxyConfig {
        addr: SmolStr::new_static("127.0.0.1"),
        port,
        ..ProxyConfig::default()
    }
}

fn service_target() -> ServiceTarget {
    ServiceTarget::KytrixService {
        capability: KytrixServiceCapability::News,
    }
}

#[test]
fn adding_a_proxy_should_store_it_at_once() {
    let mut harness = Harness::new();

    harness
        .write(SettingsChange::AddProxy {
            name: SmolStr::new_static("hk"),
            proxy: proxy(1080),
        })
        .expect("an in-memory session stores every settings change");

    assert_eq!(
        harness.config().proxies.get("hk").map(|proxy| proxy.port),
        Some(1080)
    );
}

#[test]
fn replacing_a_proxys_name_should_move_the_routes_bound_to_it() {
    let mut harness = Harness::new();
    harness.update_config(|config| {
        config
            .proxies
            .insert(SmolStr::new_static("hk"), proxy(1080));
        config.proxy_default = ProxyRoute::named("hk");
        config
            .proxy_applications
            .insert(service_target(), ProxyRoute::named("hk"));
        config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::named("hk")));
    });

    harness
        .write(SettingsChange::ReplaceProxy {
            original: SmolStr::new_static("hk"),
            name: SmolStr::new_static("osaka"),
            proxy: proxy(1081),
        })
        .expect("an in-memory session stores every settings change");

    let stored = harness.config();
    assert!(
        !stored.proxies.contains_key("hk") && stored.proxies.contains_key("osaka"),
        "the entry should have moved to its new name"
    );
    assert_eq!(stored.proxies["osaka"].port, 1081);
    assert_eq!(stored.proxy_default, ProxyRoute::named("osaka"));
    assert_eq!(
        stored.proxy_applications[&service_target()],
        ProxyRoute::named("osaka"),
        "a service route bound to the entry should follow the rename"
    );
    assert_eq!(
        stored.venue(&VenueId::Okx).and_then(|venue| venue.proxy()),
        Some(ProxyRoute::named("osaka")),
        "a venue bound to the entry should follow the rename"
    );
}

#[test]
fn removing_a_proxy_should_reset_the_routes_bound_to_it() {
    let mut harness = Harness::new();
    harness.update_config(|config| {
        config
            .proxies
            .insert(SmolStr::new_static("hk"), proxy(1080));
        config.proxy_default = ProxyRoute::named("hk");
        config
            .proxy_applications
            .insert(service_target(), ProxyRoute::named("hk"));
        config.set_venue_proxy(&VenueId::Okx, Some(ProxyRoute::named("hk")));
    });

    harness
        .write(SettingsChange::RemoveProxy {
            name: SmolStr::new_static("hk"),
        })
        .expect("an in-memory session stores every settings change");

    let stored = harness.config();
    assert!(stored.proxies.is_empty(), "the entry should be gone");
    assert_eq!(
        stored.proxy_default,
        ProxyRoute::Direct,
        "a default line naming the removed entry falls back to the direct line"
    );
    assert!(
        stored.proxy_applications.is_empty(),
        "every service route naming the removed entry should be reset"
    );
    assert!(
        stored
            .venue(&VenueId::Okx)
            .is_some_and(|venue| venue.proxy().is_none()),
        "a venue naming the removed entry should follow the default line again"
    );
}

#[test]
fn a_directory_change_should_clear_the_selection_it_invalidates() {
    let first = tempfile::tempdir().expect("a temporary directory should be created");
    let second = tempfile::tempdir().expect("a temporary directory should be created");
    let mut harness = Harness::new();
    harness.update_config(|config| {
        config.translation.model_directory = Some(first.path().to_path_buf());
        config.translation.selected_model = Some(TranslationModel::HyMt218BQ4Km);
    });
    let before = harness.config();

    harness
        .write_all([
            SettingsChange::TranslationModelDirectory {
                directory: second.path().to_path_buf(),
            },
            SettingsChange::TranslationModelSelected { model: None },
        ])
        .expect("an in-memory session stores every settings change");

    let after = harness.config();
    assert_eq!(
        after.translation.model_directory,
        Some(second.path().to_path_buf())
    );
    assert_eq!(after.translation.selected_model, None);
    assert_eq!(
        after.translation.download_site,
        DownloadSite::HuggingFace,
        "the rest of the translation settings should be left alone"
    );
    assert_ne!(
        before.translation.model_directory, after.translation.model_directory,
        "the two changes land in one published snapshot"
    );
    assert_eq!(
        harness.writes().len(),
        1,
        "the changes are stored as one configuration"
    );
}

#[test]
fn a_write_that_cannot_be_stored_should_be_kept_for_the_page_once() {
    let mut harness = Harness::new();
    let before = harness.config();
    harness.fail_next_write(crate::session::SettingsWriteError::new(
        ConfigPersistenceError::CreateDirectory {
            path: PathBuf::from("unwritable"),
            source: io::Error::other("the directory cannot be created"),
        },
    ));

    let error = harness
        .write(SettingsChange::edit(|config| {
            config.appearance = Appearance::Dark
        }))
        .expect_err("the session reports the store's failure");

    assert_eq!(
        harness.config(),
        before,
        "a write that could not be stored must leave the snapshot alone"
    );
    let message = error.to_string();
    assert!(
        message.contains("the directory cannot be created"),
        "the displayed reason should be the store's own: {message}"
    );
    assert!(
        error.source().is_some(),
        "the write error should expose the store's failure"
    );
    assert!(
        harness.store().take_failure().is_some(),
        "the page should be handed the reason of the failed write"
    );
    assert!(
        harness.store().take_failure().is_none(),
        "one failure should be reported once"
    );
}

#[test]
fn an_unchanged_change_should_be_accepted_without_a_report() {
    let mut harness = Harness::new();
    let before = harness.config();
    let language = before.language;

    harness
        .write(SettingsChange::edit(move |config| {
            config.language = language
        }))
        .expect("a change that stores nothing is accepted");
    harness
        .write_all([])
        .expect("a write of nothing is accepted");

    assert_eq!(
        harness.config(),
        before,
        "a change that stores what is stored already is not a write"
    );
    assert!(
        harness.store().take_failure().is_none(),
        "a change that stores nothing is nothing to report"
    );
}

#[test]
fn a_change_a_setter_refuses_should_be_accepted_without_a_report() {
    let mut harness = Harness::new();
    harness.update_config(|config| {
        config.venues.remove(&VenueId::Okx);
    });
    let before = harness.config();

    harness
        .write(SettingsChange::edit(|config| {
            config.set_okx_credentials(
                &VenueId::Okx,
                Some(
                    app_config::market_participants::credentials::OkxCredentials {
                        api_key: SmolStr::new_static("typed-key"),
                        secret_key: SmolStr::new_static("typed-secret"),
                        passphrase: SmolStr::new_static("typed-passphrase"),
                    },
                ),
            );
        }))
        .expect("a value the settings cannot hold is not a failure");

    assert_eq!(
        harness.config().venues.len(),
        before.venues.len(),
        "a refused value leaves the settings exactly as they were"
    );
    assert!(
        harness.store().take_failure().is_none(),
        "a refused value is the user's choice, not a failure to report"
    );
    assert_eq!(harness.writes().len(), 0, "a refused value stores nothing");
}
