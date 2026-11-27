use std::{io, path::PathBuf};

use app_config::{
    market_participants::{
        credentials::{BinanceMarketCredentials, HyperliquidCredentials, OkxCredentials},
        origins::BinanceMarket,
        settings::VenueSettings,
    },
    settings::Language,
};
use app_config_provider::store::ConfigPersistenceError;
use i18n::language::codes::LanguageCode;
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    event::{Action, KeyCode},
    math::px,
};
use ui_kit::kit::size::UnifiedSize;

use crate::{
    pages::venues::credentials::{
        CANCEL_SELECTOR, CONFIRM_SELECTOR, CredentialsKind, DIALOG_SELECTOR, FOOTER_SELECTOR,
    },
    session::SettingsWriteError,
    settings_tests::{
        test_support::{Harness, translate_in},
        venues::{
            alert_is_open, bounds, cancel_dialog, center, click_alert, confirm_dialog,
            credentials_key, dialog_key, edges, english_settings, field_error, field_id, field_of,
            open_credentials_dialog, open_page, type_into,
        },
    },
};

fn page_of(kind: CredentialsKind) -> &'static str {
    match kind {
        CredentialsKind::BinanceSpot | CredentialsKind::BinanceUsdMPerp => {
            "market_participants.binance"
        }
        CredentialsKind::Okx => "market_participants.okx",
        CredentialsKind::Hyperliquid => "market_participants.hyperliquid",
        CredentialsKind::Longbridge => "market_participants.longbridge",
    }
}

const KINDS: [CredentialsKind; 5] = [
    CredentialsKind::BinanceSpot,
    CredentialsKind::BinanceUsdMPerp,
    CredentialsKind::Okx,
    CredentialsKind::Hyperliquid,
    CredentialsKind::Longbridge,
];

fn write_failure() -> SettingsWriteError {
    SettingsWriteError::new(ConfigPersistenceError::CreateDirectory {
        path: PathBuf::from("blocked"),
        source: io::Error::other("the settings cannot be stored"),
    })
}

fn dialog_is_open(harness: &Harness, kind: CredentialsKind) -> bool {
    harness
        .find(&format!("{}:panel", dialog_key(kind)))
        .is_some()
        || harness.find(DIALOG_SELECTOR).is_some()
}

#[test]
fn the_dialog_should_open_prefilled_and_write_the_whole_set() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;
    open_credentials_dialog(&mut harness, kind);

    for selector in [
        DIALOG_SELECTOR,
        FOOTER_SELECTOR,
        CONFIRM_SELECTOR,
        CANCEL_SELECTOR,
    ] {
        assert!(
            harness.find(selector).is_some(),
            "the dialog should draw {selector}"
        );
    }
    for field in ["api_key", "secret_key", "passphrase"] {
        assert!(
            harness
                .find(kind.field_selector(field_of(kind, field)).as_ref())
                .is_some(),
            "the dialog should draw the field {field} is typed into"
        );
    }
    let title = harness
        .find(&format!("{}:title", dialog_key(kind)))
        .expect("the dialog draws its title");
    assert_eq!(
        harness.node_text(title),
        i18n::t_smol!(translate_in(LanguageCode::En), kind.title_key()),
        "the dialog is titled with the set it edits"
    );

    type_into(&mut harness, kind, "api_key", "typed");
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "a stored set should take the dialog away"
    );
    assert!(
        harness.field(CredentialsKind::Okx.row_id()).is_some(),
        "the settings the dialog was opened over should still be drawn"
    );
    let written = harness.config();
    let Some(VenueSettings::Okx(okx)) = written.venue(&VenueId::Okx) else {
        panic!("okx is configured as OKX");
    };
    assert_eq!(
        okx.credentials,
        Some(OkxCredentials {
            api_key: SmolStr::new_static("okx-api-keytyped"),
            secret_key: SmolStr::new_static("okx-secret-key"),
            passphrase: SmolStr::new_static("okx-passphrase"),
        }),
        "the value typed was appended to the stored one and the rest was written as it was"
    );
}

#[test]
fn a_cancelled_dialog_should_not_write_the_set() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;
    let before = harness.config();

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "changed");
    cancel_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "cancelling should take the dialog away"
    );
    assert_eq!(
        harness.config(),
        before,
        "a cancelled dialog must not write the settings"
    );
    assert_eq!(
        harness.page().current_page().as_str(),
        "market_participants.okx",
        "the settings should still be on the page the dialog was opened over"
    );
}

#[test]
fn an_incomplete_set_should_keep_the_dialog_open_and_write_nothing() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.binance");
    let kind = CredentialsKind::BinanceUsdMPerp;

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "perp-key");
    confirm_dialog(&mut harness);

    assert!(
        dialog_is_open(&harness, kind),
        "a set missing a required value should leave the dialog open"
    );
    let stored = harness.config();
    let VenueSettings::Binance(binance) = stored
        .venue(&VenueId::Binance)
        .expect("the default settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.usd_m_perp_credentials, None,
        "an incomplete set must not be written"
    );

    type_into(&mut harness, kind, "private_key", "perp-private");
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "a set holding its required values should be written"
    );
    let written = harness.config();
    let Some(VenueSettings::Binance(binance)) = written.venue(&VenueId::Binance) else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.usd_m_perp_credentials,
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("perp-key"),
            private_key: SmolStr::new_static("perp-private"),
            passphrase: SmolStr::default(),
        }),
        "the set is written without the passphrase its key was not created with"
    );
    assert_eq!(
        binance.spot_credentials, None,
        "the venue's other market was never configured and stays unconfigured"
    );
}

#[test]
fn clearing_a_set_should_ask_first_and_take_only_that_set() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.binance");
    let kind = CredentialsKind::BinanceSpot;

    harness.click_node(kind.clear_selector().as_ref());
    harness.settle(3);
    assert!(
        alert_is_open(&harness),
        "clearing should be answered with an alert rather than written at once"
    );
    click_alert(&mut harness, "cancel");

    let kept = harness.config();
    let VenueSettings::Binance(binance) = kept
        .venue(&VenueId::Binance)
        .expect("the fixture settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert!(
        binance.spot_credentials.is_some(),
        "walking away from the alert should leave the set in the settings"
    );

    harness.click_node(kind.clear_selector().as_ref());
    harness.settle(3);
    click_alert(&mut harness, "ok");

    let cleared = harness.config();
    let VenueSettings::Binance(binance) = cleared
        .venue(&VenueId::Binance)
        .expect("the fixture settings name every venue this build speaks")
    else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.spot_credentials, None,
        "confirming the alert should clear the set the row belongs to"
    );
    assert!(
        binance.usd_m_perp_credentials.is_some(),
        "and the venue's other market keeps its own set"
    );
    assert!(
        harness.find(kind.clear_selector().as_ref()).is_none(),
        "a row with nothing stored should not offer to clear it"
    );

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "after-clear");
    confirm_dialog(&mut harness);
    assert!(
        dialog_is_open(&harness, kind),
        "the cleared set should open as an empty form, which a required value still rejects"
    );
    cancel_dialog(&mut harness);
}

#[test]
fn the_clear_button_should_be_drawn_only_for_a_configured_set() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;

    assert!(
        harness.find(kind.edit_selector().as_ref()).is_some(),
        "the edit button is there whether or not a set is stored"
    );
    assert!(
        harness.find(kind.clear_selector().as_ref()).is_none(),
        "a venue with nothing stored has nothing to clear"
    );

    harness.update_config(|config| {
        config.set_okx_credentials(
            &VenueId::Okx,
            Some(OkxCredentials {
                api_key: SmolStr::new_static("okx-api-key"),
                secret_key: SmolStr::new_static("okx-secret-key"),
                passphrase: SmolStr::new_static("okx-passphrase"),
            }),
        );
    });
    harness.settle(2);

    assert!(
        harness.find(kind.clear_selector().as_ref()).is_some(),
        "a stored set is cleared from its row"
    );
}

#[test]
fn a_failed_write_should_keep_the_dialog_and_what_was_typed() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;
    harness.fail_next_write(write_failure());

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "typed-key");
    type_into(&mut harness, kind, "secret_key", "typed-secret");
    type_into(&mut harness, kind, "passphrase", "typed-passphrase");
    confirm_dialog(&mut harness);

    assert!(
        dialog_is_open(&harness, kind),
        "a set that could not be stored should leave the dialog open"
    );
    assert!(
        alert_is_open(&harness),
        "a write that could not be stored should tell the user"
    );
    let unchanged = harness.config();
    let Some(VenueSettings::Okx(okx)) = unchanged.venue(&VenueId::Okx) else {
        panic!("okx is configured as OKX");
    };
    assert_eq!(
        okx.credentials, None,
        "settings that could not be stored must not be published as if they had been"
    );

    harness.press(KeyCode::Escape);
    harness.settle(3);
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "the retry should take the dialog away"
    );
    let stored = harness.config();
    let Some(VenueSettings::Okx(okx)) = stored.venue(&VenueId::Okx) else {
        panic!("okx is configured as OKX");
    };
    assert_eq!(
        okx.credentials,
        Some(OkxCredentials {
            api_key: SmolStr::new_static("typed-key"),
            secret_key: SmolStr::new_static("typed-secret"),
            passphrase: SmolStr::new_static("typed-passphrase"),
        }),
        "the retry should write what the dialog kept"
    );
}

#[test]
fn a_failed_clear_should_keep_the_set_until_the_retry_writes_it() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;
    harness.fail_next_write(write_failure());

    harness.click_node(kind.clear_selector().as_ref());
    harness.settle(3);
    click_alert(&mut harness, "ok");

    assert!(
        kind.has_stored_set(&harness.config()),
        "a clear that could not be stored must leave the set in the settings"
    );
    assert!(
        alert_is_open(&harness),
        "a write that could not be stored should tell the user"
    );
    assert!(
        harness.find(kind.clear_selector().as_ref()).is_some(),
        "the row still holds a set, so it still offers to clear it"
    );

    harness.press(KeyCode::Escape);
    harness.settle(3);
    harness.advance(200);

    harness.click_node(kind.clear_selector().as_ref());
    harness.settle(3);
    click_alert(&mut harness, "ok");

    let cleared = harness.config();
    let Some(VenueSettings::Okx(okx)) = cleared.venue(&VenueId::Okx) else {
        panic!("okx is configured as OKX");
    };
    assert_eq!(
        okx.credentials, None,
        "the retry should write the clear it could not write before"
    );
    for other in [
        CredentialsKind::BinanceSpot,
        CredentialsKind::BinanceUsdMPerp,
        CredentialsKind::Hyperliquid,
        CredentialsKind::Longbridge,
    ] {
        assert!(
            other.has_stored_set(&cleared),
            "the retry should clear OKX's set and leave {other:?} configured"
        );
    }
}

#[test]
fn a_credential_row_should_share_the_venues_card() {
    let mut harness = Harness::with_config(english_settings());

    open_page(&mut harness, "market_participants.okx");
    let card = bounds(&harness, "SETTINGS_CARD_market_participants.okx_0");
    let row = bounds(
        &harness,
        &format!("SETTINGS_FIELD_{}", CredentialsKind::Okx.row_id()),
    );
    let region = bounds(&harness, "SETTINGS_FIELD_market_participants.okx.region");
    assert!(
        card.contains(center(region)),
        "the region belongs to the card the venue's settings are drawn in"
    );
    assert!(
        card.contains(center(row)),
        "the credential row belongs to the card the venue's settings are drawn in"
    );
    assert!(
        harness
            .find("SETTINGS_CARD_market_participants.okx_1")
            .is_none(),
        "the credentials are not a card of their own"
    );

    open_page(&mut harness, "market_participants.hyperliquid");
    let card = bounds(&harness, "SETTINGS_CARD_market_participants.hyperliquid_0");
    let row = bounds(
        &harness,
        &format!("SETTINGS_FIELD_{}", CredentialsKind::Hyperliquid.row_id()),
    );
    for selector in [
        "SETTINGS_FIELD_market_participants.hyperliquid.http",
        "SETTINGS_FIELD_market_participants.hyperliquid.websocket",
    ] {
        assert!(
            card.contains(center(bounds(&harness, selector))),
            "{selector} belongs to the card the venue's addresses are drawn in"
        );
    }
    assert!(
        card.contains(center(row)),
        "the credential row belongs to the card the venue's addresses are drawn in"
    );
    assert!(
        harness
            .find("SETTINGS_CARD_market_participants.hyperliquid_1")
            .is_none(),
        "the credentials are not a card of their own"
    );
}

#[test]
fn every_credential_value_should_be_required_except_binances_passphrase() {
    for kind in KINDS {
        for field in kind.fields() {
            let optional = kind.market().is_some() && field.id == "passphrase";
            assert_eq!(
                field.error_key.is_some(),
                !optional,
                "{kind:?}'s {} should be required unless it is Binance's optional passphrase",
                field.id
            );
        }
    }
}

#[test]
fn a_secret_should_be_masked_and_an_identity_shown() {
    for kind in KINDS {
        for field in kind.fields() {
            let masked = matches!(
                (kind, field.id),
                (
                    CredentialsKind::BinanceSpot | CredentialsKind::BinanceUsdMPerp,
                    "private_key" | "passphrase"
                ) | (CredentialsKind::Okx, "secret_key" | "passphrase")
                    | (CredentialsKind::Hyperliquid, "api_agent_private_key")
                    | (CredentialsKind::Longbridge, "app_secret" | "access_token")
            );
            assert_eq!(
                field.password,
                masked,
                "{kind:?}'s {} should be drawn {}",
                field.id,
                if masked { "masked" } else { "as it is" }
            );
        }
    }
}

#[test]
fn the_clear_alert_should_name_the_credentials_it_clears() {
    let cases = [
        (
            LanguageCode::En,
            CredentialsKind::BinanceSpot,
            "Binance Spot trading credentials",
        ),
        (
            LanguageCode::En,
            CredentialsKind::BinanceUsdMPerp,
            "Binance USDⓈ-M Futures trading credentials",
        ),
        (
            LanguageCode::ZhCn,
            CredentialsKind::BinanceSpot,
            "币安现货交易凭据",
        ),
        (
            LanguageCode::ZhCn,
            CredentialsKind::BinanceUsdMPerp,
            "币安U本位合约交易凭据",
        ),
    ];

    for (language, kind, expected_title) in cases {
        let translate = translate_in(language);
        let title = i18n::t_smol!(translate, kind.title_key());
        let message = kind.clear_message(&translate);
        let alert_title = kind.clear_title(&translate);
        assert_eq!(
            title.as_str(),
            expected_title,
            "{kind:?}'s title should be the one of its dialog"
        );
        assert!(
            message.contains(expected_title),
            "the message of {language:?} for {kind:?} should name the set it is about: {message}"
        );
        assert!(
            !message.contains("%{"),
            "every placeholder of {language:?} for {kind:?} should be filled in: {message}"
        );
        assert!(
            !alert_title.is_empty(),
            "the alert of {language:?} for {kind:?} should have a title"
        );
    }
}

#[test]
fn the_dialog_titles_should_name_the_trading_surface() {
    let messages = i18n::embedded::load().expect("embedded locales should load");
    let cases = [
        (
            CredentialsKind::BinanceSpot,
            "Binance Spot trading credentials",
            "币安现货交易凭据",
        ),
        (
            CredentialsKind::BinanceUsdMPerp,
            "Binance USDⓈ-M Futures trading credentials",
            "币安U本位合约交易凭据",
        ),
        (
            CredentialsKind::Okx,
            "OKX trading credentials",
            "欧易交易凭据",
        ),
        (
            CredentialsKind::Hyperliquid,
            "Hyperliquid trading credentials",
            "Hyperliquid交易凭据",
        ),
        (
            CredentialsKind::Longbridge,
            "Longbridge trading credentials",
            "长桥交易凭据",
        ),
    ];

    for (kind, english, chinese) in cases {
        assert!(
            messages
                .try_translate(LanguageCode::En, kind.title_key())
                .is_some_and(|text| text.as_str() == english),
            "{kind:?}'s title should be written in English"
        );
        assert!(
            messages
                .try_translate(LanguageCode::ZhCn, kind.title_key())
                .is_some_and(|text| text.as_str() == chinese),
            "{kind:?}'s title should be written in Chinese"
        );
    }
}

#[test]
fn a_rejected_submission_should_report_every_value_it_is_missing() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;

    open_credentials_dialog(&mut harness, kind);
    confirm_dialog(&mut harness);

    for field in ["api_key", "secret_key", "passphrase"] {
        assert!(
            !field_error(&harness, kind, field).is_empty(),
            "the value {field} was submitted empty and should be reported under its field"
        );
    }
    assert!(
        dialog_is_open(&harness, kind),
        "a rejected submission should leave the dialog where it can be completed"
    );

    type_into(&mut harness, kind, "api_key", "typed-key");
    harness.press(KeyCode::Enter);
    harness.settle(1);

    assert!(
        field_error(&harness, kind, "api_key").is_empty(),
        "the report under a value that was filled in should go"
    );
    assert!(
        !field_error(&harness, kind, "secret_key").is_empty(),
        "the report under a value that is still empty should stay"
    );
    assert!(
        !field_error(&harness, kind, "passphrase").is_empty(),
        "filling one value in should not take the other reports away"
    );

    type_into(&mut harness, kind, "secret_key", "typed-secret");
    harness.press(KeyCode::Enter);
    harness.settle(1);

    assert!(
        field_error(&harness, kind, "secret_key").is_empty(),
        "the report under the second value should go when it is filled in"
    );
    assert!(
        !field_error(&harness, kind, "passphrase").is_empty(),
        "the last value is still empty until the submission takes it"
    );

    type_into(&mut harness, kind, "passphrase", "typed-passphrase");
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "a completed form should be submitted and take the dialog away"
    );
    let written = harness.config();
    let Some(VenueSettings::Okx(okx)) = written.venue(&VenueId::Okx) else {
        panic!("okx is configured as OKX");
    };
    assert_eq!(
        okx.credentials,
        Some(OkxCredentials {
            api_key: SmolStr::new_static("typed-key"),
            secret_key: SmolStr::new_static("typed-secret"),
            passphrase: SmolStr::new_static("typed-passphrase"),
        }),
        "the completed form should be written"
    );
}

#[test]
fn submitting_a_field_unchanged_should_keep_its_report() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;

    open_credentials_dialog(&mut harness, kind);
    confirm_dialog(&mut harness);
    assert!(
        !field_error(&harness, kind, "api_key").is_empty(),
        "an empty required value should be reported"
    );

    let name = field_id(kind, "api_key");
    harness.click_node(&format!("{name}.root"));
    harness.settle(1);
    harness.press(KeyCode::Enter);
    harness.settle(2);

    assert!(
        !field_error(&harness, kind, "api_key").is_empty(),
        "a field submitted without a new value should still report what it holds empty"
    );
}

#[test]
fn binances_passphrase_should_not_be_reported_as_missing() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.binance");
    let kind = CredentialsKind::BinanceUsdMPerp;

    open_credentials_dialog(&mut harness, kind);
    confirm_dialog(&mut harness);

    for field in ["api_key", "private_key"] {
        assert!(
            !field_error(&harness, kind, field).is_empty(),
            "the required value {field} should be reported when it is empty"
        );
    }
    assert!(
        field_error(&harness, kind, "passphrase").is_empty(),
        "Binance's passphrase may be left out, so it is not reported as missing"
    );

    type_into(&mut harness, kind, "api_key", "perp-key");
    type_into(&mut harness, kind, "private_key", "perp-private");
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "a set with its required values and no passphrase should be written"
    );
    let written = harness.config();
    let Some(VenueSettings::Binance(binance)) = written.venue(&VenueId::Binance) else {
        panic!("binance is configured as Binance");
    };
    assert_eq!(
        binance.usd_m_perp_credentials,
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("perp-key"),
            private_key: SmolStr::new_static("perp-private"),
            passphrase: SmolStr::default(),
        }),
        "the passphrase the key was not created with is written as the empty value it was left as"
    );
}

#[test]
fn the_credentials_dialog_should_be_drawn_the_way_the_application_draws_its_dialogs() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;
    open_credentials_dialog(&mut harness, kind);

    let content = bounds(&harness, &format!("{}:content", dialog_key(kind)));
    let dialog = bounds(&harness, DIALOG_SELECTOR);
    let footer = bounds(&harness, FOOTER_SELECTOR);
    let confirm = bounds(&harness, CONFIRM_SELECTOR);
    let cancel = bounds(&harness, CANCEL_SELECTOR);

    assert!(
        (content.size.width - 480.0).abs() < px(1.0),
        "the credentials dialog is 480 px wide: it is {content:?}"
    );
    let fields = kind
        .fields()
        .iter()
        .map(|field| bounds(&harness, kind.field_selector(field).as_ref()))
        .collect::<Vec<_>>();
    for (field, field_bounds) in kind.fields().iter().zip(fields.iter()) {
        let (_, _, field_bottom, _) = edges(*field_bounds);
        let (footer_top, _, _, _) = edges(footer);
        assert!(
            field_bottom <= footer_top,
            "the value {} belongs above the line the foot starts with: {field_bounds:?} over \
             {footer:?}",
            field.id
        );
    }
    for pair in fields.windows(2) {
        let (first_top, _, _, _) = edges(pair[0]);
        let (second_top, _, _, _) = edges(pair[1]);
        assert!(
            first_top < second_top,
            "the values should be drawn in the order the set declares them: {pair:?}"
        );
    }
    let (_, _, footer_bottom, _) = edges(footer);
    let (_, _, dialog_bottom, _) = edges(dialog);
    assert!(
        (footer_bottom - dialog_bottom).abs() < 1.0,
        "the foot should be the bottom of the dialog: {footer:?} in {dialog:?}"
    );
    assert!(
        footer.contains(center(cancel)) && footer.contains(center(confirm)),
        "both buttons belong to the foot rather than to the fields: {cancel:?} and {confirm:?} in \
         {footer:?}"
    );
    let (_, cancel_right, _, _) = edges(cancel);
    let (_, _, _, confirm_left) = edges(confirm);
    assert!(
        cancel_right < confirm_left,
        "the button that leaves the set alone is before the one that writes it: {cancel:?} \
         before {confirm:?}"
    );
}

#[test]
fn a_credential_rows_buttons_should_end_the_control_column() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;

    let row = bounds(&harness, &format!("SETTINGS_FIELD_{}", kind.row_id()));
    let control = bounds(&harness, &format!("SETTINGS_CONTROL_{}", kind.row_id()));
    let edit = bounds(&harness, kind.edit_selector().as_ref());
    let clear = bounds(&harness, kind.clear_selector().as_ref());

    assert!(
        row.contains(center(edit)) && row.contains(center(clear)),
        "both buttons belong to the row they act on: {row:?} holds {edit:?} and {clear:?}"
    );
    assert!(
        control.contains(center(edit)) && control.contains(center(clear)),
        "both buttons belong to the row's control column: {control:?} holds {edit:?} and \
         {clear:?}"
    );
    let (_, edit_right, _, _) = edges(edit);
    let (_, _, _, clear_left) = edges(clear);
    assert!(
        edit_right <= clear_left,
        "the edit button is before the one that takes the set away: {edit:?} before {clear:?}"
    );
    let (_, clear_right, _, _) = edges(clear);
    let (_, control_right, _, _) = edges(control);
    assert!(
        (clear_right - control_right).abs() < 1.0,
        "the clear button should end the control column: {clear:?} in {control:?}"
    );
}

#[test]
fn a_credential_rows_buttons_should_be_medium_sized() {
    let mut harness = Harness::with_config(english_settings());

    for kind in KINDS {
        open_page(&mut harness, page_of(kind));
        for selector in [kind.edit_selector(), kind.clear_selector()] {
            let button = bounds(&harness, selector.as_ref());
            assert!(
                (button.size.height - UnifiedSize::Medium.h()).abs() < px(1.0),
                "{kind:?}'s button should be drawn at the medium size: {selector} is {button:?}"
            );
        }
    }
}

#[test]
fn a_hyperliquid_dialog_should_open_prefilled_and_write_the_whole_set() {
    let mut harness = Harness::with_config(english_settings());
    let kind = CredentialsKind::Hyperliquid;
    open_page(&mut harness, page_of(kind));
    let before = harness.config();

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_agent_private_key", "typed");
    cancel_dialog(&mut harness);

    assert_eq!(
        harness.config(),
        before,
        "a cancelled dialog must not write the set it was opened over"
    );

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "main_account_address", "typed");
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "a stored set should take the dialog away when it is confirmed"
    );
    let written = harness.config();
    let Some(VenueSettings::Hyperliquid(settings)) = written.venue(&VenueId::Hyperliquid) else {
        panic!("hyperliquid is configured as Hyperliquid");
    };
    assert_eq!(
        settings.credentials,
        Some(HyperliquidCredentials {
            main_account_address: SmolStr::new_static("0xhyperliquid-maintyped"),
            api_agent_private_key: SmolStr::new_static("hyperliquid-agent-private"),
        }),
        "the value typed was appended to the stored one and the other value was written back as \
         it was"
    );
}

#[test]
fn clearing_a_venue_should_write_none_and_leave_the_other_venues_alone() {
    let mut harness = Harness::with_config(english_settings());
    let venues = [
        CredentialsKind::Okx,
        CredentialsKind::Hyperliquid,
        CredentialsKind::Longbridge,
    ];

    for (index, kind) in venues.iter().copied().enumerate() {
        open_page(&mut harness, page_of(kind));
        harness.click_node(kind.clear_selector().as_ref());
        harness.settle(3);
        assert!(
            alert_is_open(&harness),
            "clearing {kind:?} should be put to the user before it is written"
        );
        click_alert(&mut harness, "ok");

        let config = harness.config();
        assert!(
            !kind.has_stored_set(&config),
            "confirming the alert should clear the set {kind:?} signs with"
        );
        for other in &venues[index + 1..] {
            assert!(
                other.has_stored_set(&config),
                "clearing {kind:?} should leave the venue {other:?}, which it never reached, \
                 configured"
            );
        }
        for done in &venues[..index] {
            assert!(
                !done.has_stored_set(&config),
                "clearing {kind:?} should not put back the set {done:?} had cleared"
            );
        }
        assert!(
            CredentialsKind::BinanceSpot.has_stored_set(&config)
                && CredentialsKind::BinanceUsdMPerp.has_stored_set(&config),
            "clearing {kind:?} should leave both of Binance's markets configured"
        );
    }
}

#[test]
fn a_stored_but_empty_set_should_be_completed_or_cleared() {
    fn empty() -> Option<BinanceMarketCredentials> {
        Some(BinanceMarketCredentials {
            api_key: SmolStr::default(),
            private_key: SmolStr::default(),
            passphrase: SmolStr::default(),
        })
    }

    let mut harness = Harness::new();
    let kind = CredentialsKind::BinanceSpot;
    harness.update_config(|config| {
        config.set_binance_market_credentials(&VenueId::Binance, BinanceMarket::Spot, empty());
    });
    open_page(&mut harness, page_of(kind));

    assert!(
        harness.field(kind.row_id()).is_some(),
        "a set of empty values is still a set the row is drawn for"
    );
    assert!(
        harness.find(kind.clear_selector().as_ref()).is_some(),
        "a set of empty values is still a set the row offers to clear"
    );

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "only-key");
    confirm_dialog(&mut harness);

    assert!(
        dialog_is_open(&harness, kind),
        "the form opened on empty values, so one filled value still leaves the set incomplete"
    );
    assert_eq!(
        harness.config().venue(&VenueId::Binance).and_then(|venue| {
            match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }
        }),
        empty(),
        "a rejected submission should write nothing, not even the value it was typed with"
    );

    type_into(&mut harness, kind, "private_key", "only-private");
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "a set completed in the dialog should be written"
    );
    assert_eq!(
        harness.config().venue(&VenueId::Binance).and_then(|venue| {
            match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }
        }),
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("only-key"),
            private_key: SmolStr::new_static("only-private"),
            passphrase: SmolStr::default(),
        }),
        "what was typed is the whole of what the empty values became"
    );

    harness.click_node(kind.clear_selector().as_ref());
    harness.settle(3);
    click_alert(&mut harness, "ok");

    assert_eq!(
        harness.config().venue(&VenueId::Binance).and_then(|venue| {
            match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }
        }),
        None,
        "clearing a stored set should leave the settings holding none"
    );
}

#[test]
fn a_stored_but_incomplete_set_should_open_prefilled() {
    fn incomplete() -> Option<BinanceMarketCredentials> {
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("kept-key"),
            private_key: SmolStr::default(),
            passphrase: SmolStr::new_static("kept-passphrase"),
        })
    }

    let mut harness = Harness::new();
    let kind = CredentialsKind::BinanceSpot;
    harness.update_config(|config| {
        config.set_binance_market_credentials(&VenueId::Binance, BinanceMarket::Spot, incomplete());
    });
    open_page(&mut harness, page_of(kind));

    assert!(
        harness.field(kind.row_id()).is_some(),
        "a set a hand-edited file left partly empty is still a set the row is drawn for"
    );
    assert!(
        harness.find(kind.clear_selector().as_ref()).is_some(),
        "the row offers to clear the set whether or not every value of it is filled in"
    );

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "private_key", "typed-private");
    confirm_dialog(&mut harness);

    assert!(
        !dialog_is_open(&harness, kind),
        "the set becomes complete as soon as the one empty required value is filled in"
    );
    assert_eq!(
        harness.config().venue(&VenueId::Binance).and_then(|venue| {
            match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }
        }),
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("kept-key"),
            private_key: SmolStr::new_static("typed-private"),
            passphrase: SmolStr::new_static("kept-passphrase"),
        }),
        "the values the incomplete set already held were prefilled and written back unchanged"
    );
}

#[test]
fn closing_the_dialog_should_release_its_fields() {
    let mut harness = Harness::new();
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;

    open_credentials_dialog(&mut harness, kind);
    let confirm = Action::Custom(format_smolstr!("{}:confirm.click", dialog_key(kind)));
    assert!(
        harness.app().has_action_handler(&confirm),
        "the open dialog's confirm button is a live control"
    );
    cancel_dialog(&mut harness);
    assert!(
        !harness.app().has_action_handler(&confirm),
        "the closed dialog's controls were released with it"
    );

    open_credentials_dialog(&mut harness, kind);
    type_into(&mut harness, kind, "api_key", "typed-key");
    type_into(&mut harness, kind, "secret_key", "typed-secret");
    type_into(&mut harness, kind, "passphrase", "typed-passphrase");
    confirm_dialog(&mut harness);

    let written = harness.config();
    let Some(VenueSettings::Okx(okx)) = written.venue(&VenueId::Okx) else {
        panic!("okx is configured as OKX");
    };
    assert_eq!(
        okx.credentials,
        Some(OkxCredentials {
            api_key: SmolStr::new_static("typed-key"),
            secret_key: SmolStr::new_static("typed-secret"),
            passphrase: SmolStr::new_static("typed-passphrase"),
        }),
        "the reopened dialog writes into the set it was opened over"
    );
}

#[test]
fn leaving_the_page_should_release_the_dialog() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;
    open_credentials_dialog(&mut harness, kind);

    let confirm = Action::Custom(format_smolstr!("{}:confirm.click", dialog_key(kind)));
    assert!(harness.app().has_action_handler(&confirm));

    harness.update_config(|config| config.language = Language::ZhCn);
    harness.settle(3);

    assert!(
        !harness.app().has_action_handler(&confirm),
        "the dialog's controls should be released with the page that opened them"
    );
    assert_eq!(
        harness.page().current_page().as_str(),
        "market_participants.okx",
        "the page the user was on is the page that is drawn"
    );
    assert!(
        harness
            .field("market_participants.okx.credentials")
            .is_some(),
        "the rebuilt page draws its rows"
    );
}

#[test]
fn a_language_switch_should_take_the_open_dialog_away() {
    let mut harness = Harness::with_config(english_settings());
    open_page(&mut harness, "market_participants.okx");
    let kind = CredentialsKind::Okx;
    open_credentials_dialog(&mut harness, kind);

    harness.update_config(|config| config.language = Language::ZhCn);
    harness.settle(3);

    assert!(
        !dialog_is_open(&harness, kind),
        "the page the dialog belonged to was rebuilt, so the dialog is not drawn"
    );
    assert_eq!(
        harness.page().current_page().as_str(),
        "market_participants.okx",
        "the settings stay on the page the dialog was opened over"
    );
    assert_eq!(
        harness.field_label("market_participants.okx.credentials"),
        i18n::t_smol!(
            translate_in(LanguageCode::ZhCn),
            "settings_ui.market_participants.credentials"
        ),
        "the rebuilt page draws its row in the new language"
    );
}

#[test]
fn every_set_should_be_drawn_as_one_row() {
    let mut harness = Harness::with_config(english_settings());

    for kind in KINDS {
        open_page(&mut harness, page_of(kind));
        assert!(
            harness.field(kind.row_id()).is_some(),
            "{kind:?} should be drawn as one row on its page"
        );
        for field in kind.fields() {
            assert!(
                harness
                    .field(&format!("{}.{}", kind.field_prefix(), field.id))
                    .is_none(),
                "{kind:?}'s {} should not be a row of its own",
                field.id
            );
        }
    }
}

#[test]
fn a_credentials_controls_should_be_named_by_the_row_they_belong_to() {
    let kind = CredentialsKind::Longbridge;
    assert_eq!(
        credentials_key(kind),
        SmolStr::new_static("settings.test.market_participants.longbridge.credentials"),
        "the row's controls are named under the settings page instance and the row's id"
    );
    assert_eq!(
        dialog_key(kind),
        SmolStr::new_static("settings.test.market_participants.longbridge.credentials.dialog"),
        "the dialog is named under the row it belongs to"
    );
}
