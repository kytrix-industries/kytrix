//! The tests of the credential dialog: what it opens with, what it writes, and what it leaves
//! alone.

use std::fs;

use app_config::{
    market_participants::{
        credentials::{BinanceMarketCredentials, HyperliquidCredentials, OkxCredentials},
        origins::BinanceMarket,
        settings::VenueSettings,
    },
    settings::{Config, Language},
};
use gpui_kit::{TestAppContext, px};
use i18n::{language::codes::LanguageCode, t};
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;

use super::test_support::*;
use crate::pages::venues::credentials::{
    CANCEL_SELECTOR, CONFIRM_SELECTOR, CredentialsKind, DIALOG_SELECTOR, FOOTER_SELECTOR,
    TITLE_SELECTOR, WIDTH,
};

/// The dialog opens over what the settings hold, and confirming it writes the values the user
/// left: the set is one value, so every value of it is written together.
#[gpui_kit::test]
fn the_dialog_should_open_prefilled_and_write_the_whole_set(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;

    click(cx, &kind.edit_selector());

    for selector in [
        DIALOG_SELECTOR,
        TITLE_SELECTOR,
        FOOTER_SELECTOR,
        CONFIRM_SELECTOR,
        CANCEL_SELECTOR,
    ] {
        assert!(
            bounds(cx, selector).is_some(),
            "the dialog should draw {selector}"
        );
    }
    for field in ["api_key", "secret_key", "passphrase"] {
        let selector = credentials_field(kind, field);
        assert!(
            bounds(cx, &selector).is_some(),
            "the dialog should draw the field {field} is typed into"
        );
    }

    // The caret is put at the end of what the field holds, so what is typed is appended: the
    // stored value was prefilled rather than the field started empty.
    type_into(cx, &credentials_field(kind, "api_key"), "typed");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "a stored set should take the dialog away"
    );
    assert!(
        bounds(cx, "SETTINGS_PAGE").is_some(),
        "the settings the dialog was opened over should still be drawn"
    );
    let written = stored(cx);
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

/// A dialog the user walks away from is not a change of the settings.
#[gpui_kit::test]
fn a_cancelled_dialog_should_not_write_the_set(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;
    let before = stored(cx);

    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "api_key"), "changed");
    click(cx, CANCEL_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "cancelling should take the dialog away"
    );
    assert_eq!(
        stored(cx),
        before,
        "a cancelled dialog must not write the settings"
    );
    assert_eq!(
        current_page(&panel, cx),
        "market_participants.okx",
        "the settings should still be on the page the dialog was opened over"
    );
}

/// A submission with a required value left empty is rejected: the dialog stays open with what the
/// user typed, and the settings are not written.
///
/// Binance's passphrase is the one value a set is complete without, so a form holding the two
/// required values is a complete set even while its passphrase is empty.
#[gpui_kit::test]
fn an_incomplete_set_should_keep_the_dialog_open_and_write_nothing(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);
    select(&panel, "market_participants.binance", cx);
    let kind = CredentialsKind::BinanceUsdMPerp;

    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "api_key"), "perp-key");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_some(),
        "a set missing a required value should leave the dialog open"
    );
    assert_eq!(
        stored(cx)
            .venue(&VenueId::Binance)
            .and_then(|venue| match venue {
                VenueSettings::Binance(binance) => Some(binance),
                _ => None,
            })
            .and_then(|binance| binance.usd_m_perp_credentials.clone()),
        None,
        "an incomplete set must not be written"
    );

    type_into(cx, &credentials_field(kind, "private_key"), "perp-private");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "a set holding its required values should be written"
    );
    let written = stored(cx);
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

/// Clearing is asked about before it is written: walking away from the alert leaves the set, and
/// confirming it clears the set the row belongs to and no other.
#[gpui_kit::test]
fn clearing_a_set_should_ask_first_and_take_only_that_set(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.binance", cx);
    let kind = CredentialsKind::BinanceSpot;

    click(cx, &credentials_clear(kind));
    assert!(
        bounds(cx, "BASE_UI_ALERT").is_some(),
        "clearing should be answered with an alert rather than written at once"
    );
    click(cx, "BASE_UI_ALERT_CANCEL");
    let kept = stored(cx);
    let Some(VenueSettings::Binance(binance)) = kept.venue(&VenueId::Binance) else {
        panic!("binance is configured as Binance");
    };
    assert!(
        binance.spot_credentials.is_some(),
        "walking away from the alert should leave the set in the settings"
    );

    click(cx, &credentials_clear(kind));
    click(cx, "BASE_UI_ALERT_OK");

    let cleared = stored(cx);
    let Some(VenueSettings::Binance(binance)) = cleared.venue(&VenueId::Binance) else {
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
        bounds(cx, &credentials_clear(kind)).is_none(),
        "a row with nothing stored should not offer to clear it"
    );

    // The dialog the row opens again is an empty form: a set cleared is a set the settings no
    // longer hold, and a form prefilled with what was cleared would still hold its values.
    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "api_key"), "after-clear");
    click(cx, CONFIRM_SELECTOR);
    assert!(
        bounds(cx, DIALOG_SELECTOR).is_some(),
        "the cleared set should open as an empty form, which a required value still rejects"
    );
    click(cx, CANCEL_SELECTOR);
}

/// A set the settings do not hold has nothing to clear, and a set they do hold offers the button
/// that clears it.
#[gpui_kit::test]
fn the_clear_button_should_be_drawn_only_for_a_configured_set(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;

    assert!(
        bounds(cx, &kind.edit_selector()).is_some(),
        "the edit button is there whether or not a set is stored"
    );
    assert!(
        bounds(cx, &credentials_clear(kind)).is_none(),
        "a venue with nothing stored has nothing to clear"
    );

    edit_settings(
        &panel,
        |config| {
            config.set_okx_credentials(
                &VenueId::Okx,
                Some(OkxCredentials {
                    api_key: SmolStr::new_static("okx-api-key"),
                    secret_key: SmolStr::new_static("okx-secret-key"),
                    passphrase: SmolStr::new_static("okx-passphrase"),
                }),
            );
        },
        cx,
    );

    assert!(
        bounds(cx, &credentials_clear(kind)).is_some(),
        "a stored set is cleared from its row"
    );
}

/// A write that could not be stored keeps the dialog open with what the user typed; once the
/// settings can be stored, confirming it again writes the set.
#[gpui_kit::test]
fn a_failed_write_should_keep_the_dialog_and_what_was_typed(cx: &mut TestAppContext) {
    let (manager, blocking, _directory) = manager_behind_a_file(cx);
    let (panel, cx) = settings_over(manager, Language::En, cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;

    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "api_key"), "typed-key");
    type_into(cx, &credentials_field(kind, "secret_key"), "typed-secret");
    type_into(
        cx,
        &credentials_field(kind, "passphrase"),
        "typed-passphrase",
    );
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_some(),
        "a set that could not be stored should leave the dialog open"
    );
    assert!(
        bounds(cx, "BASE_UI_ALERT").is_some(),
        "a write that could not be stored should tell the user"
    );
    assert_eq!(
        stored(cx)
            .venue(&VenueId::Okx)
            .and_then(|venue| match venue {
                VenueSettings::Okx(okx) => okx.credentials.clone(),
                _ => None,
            }),
        None,
        "settings that could not be stored must not be published as if they had been"
    );

    click(cx, "BASE_UI_ALERT_OK");
    fs::remove_file(&blocking).expect("the blocking file should be removable");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "the retry should take the dialog away"
    );
    let stored = stored(cx);
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

/// A clear that could not be stored leaves the set where it was and reports why; nothing asks the
/// user to confirm it again on its own. Once the settings can be stored, clearing the same row
/// again is what writes it — and only it.
#[gpui_kit::test]
fn a_failed_clear_should_keep_the_set_until_the_retry_writes_it(cx: &mut TestAppContext) {
    let (mut manager, blocking, _directory) = manager_behind_a_file(cx);
    let fixture = configured_settings();
    cx.update(|cx| {
        manager.update(|config| *config = fixture, cx);
    });
    let (panel, cx) = settings_over(manager, Language::En, cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;

    click(cx, &credentials_clear(kind));
    click(cx, "BASE_UI_ALERT_OK");

    assert!(
        stored(cx)
            .venue(&VenueId::Okx)
            .and_then(|venue| match venue {
                VenueSettings::Okx(okx) => okx.credentials.clone(),
                _ => None,
            })
            .is_some(),
        "a clear that could not be stored must leave the set in the settings"
    );
    assert!(
        bounds(cx, "BASE_UI_ALERT").is_some(),
        "a write that could not be stored should tell the user"
    );
    assert!(
        bounds(cx, &credentials_clear(kind)).is_some(),
        "the row still holds a set, so it still offers to clear it"
    );

    click(cx, "BASE_UI_ALERT_OK");
    fs::remove_file(&blocking).expect("the blocking file should be removable");

    click(cx, &credentials_clear(kind));
    click(cx, "BASE_UI_ALERT_OK");

    let cleared = stored(cx);
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

/// A venue's credential row is a row of the card that holds the rest of that venue's settings:
/// the credentials are one setting of the venue rather than a group of their own.
#[gpui_kit::test]
fn a_credential_row_should_share_the_venues_card(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    select(&panel, "market_participants.okx", cx);
    let card = element(cx, "SETTINGS_CARD_market_participants.okx_0");
    let okx_credentials = credentials_row(CredentialsKind::Okx);
    for selector in [
        "SETTINGS_FIELD_market_participants.okx.region",
        okx_credentials.as_str(),
    ] {
        assert!(
            card.contains(&element(cx, selector).center()),
            "{selector} belongs to the card the venue's settings are drawn in"
        );
    }
    assert!(
        bounds(cx, "SETTINGS_CARD_market_participants.okx_1").is_none(),
        "the credentials are not a card of their own"
    );

    select(&panel, "market_participants.hyperliquid", cx);
    let card = element(cx, "SETTINGS_CARD_market_participants.hyperliquid_0");
    let hyperliquid_credentials = credentials_row(CredentialsKind::Hyperliquid);
    for selector in [
        "SETTINGS_FIELD_market_participants.hyperliquid.http",
        "SETTINGS_FIELD_market_participants.hyperliquid.websocket",
        hyperliquid_credentials.as_str(),
    ] {
        assert!(
            card.contains(&element(cx, selector).center()),
            "{selector} belongs to the card the venue's addresses are drawn in"
        );
    }
    assert!(
        bounds(cx, "SETTINGS_CARD_market_participants.hyperliquid_1").is_none(),
        "the credentials are not a card of their own"
    );
}

/// Every value of a set is required but Binance's passphrase, which its key may have been created
/// without.
#[test]
fn every_credential_value_should_be_required_except_binances_passphrase() {
    for kind in [
        CredentialsKind::BinanceSpot,
        CredentialsKind::BinanceUsdMPerp,
        CredentialsKind::Okx,
        CredentialsKind::Hyperliquid,
        CredentialsKind::Longbridge,
    ] {
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

/// A secret is drawn masked and an identity as it is: a private key or a secret must not be read
/// off the screen, and an API key, a main account address and an application key are identities
/// the operator checks against the venue's own page.
#[test]
fn a_secret_should_be_masked_and_an_identity_shown() {
    for kind in [
        CredentialsKind::BinanceSpot,
        CredentialsKind::BinanceUsdMPerp,
        CredentialsKind::Okx,
        CredentialsKind::Hyperliquid,
        CredentialsKind::Longbridge,
    ] {
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

/// The alert a clear is confirmed with names the set it is about, in both languages and for both
/// of Binance's markets: the title is the one thing that tells one venue's credentials from
/// another's — and one market's from its sibling's — and a message that dropped it would ask the
/// user to confirm a removal without saying which.
///
/// The message is read from the set itself rather than from the locale with a title written into
/// it: the set's own title is what the dialog is titled with, and a message the page could build
/// while the dialog said another thing would be a test of the locale rather than of the alert the
/// page opens.
#[gpui_kit::test]
fn the_clear_alert_should_name_the_credentials_it_clears(cx: &mut TestAppContext) {
    let (_panel, cx) = settings(Config::default(), cx);
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
        cx.update(|_, cx| cx.set_global(language));

        let (title, message, alert_title) = cx.read(|cx| {
            (
                t!(cx, kind.title_key()),
                kind.clear_message(cx),
                kind.clear_title(cx),
            )
        });
        assert_eq!(
            title.as_ref(),
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
            message.starts_with("<p align=\"center\">") && message.ends_with("</p>"),
            "the message of {language:?} for {kind:?} should be one centred paragraph: {message}"
        );
        assert!(
            !alert_title.is_empty(),
            "the alert of {language:?} for {kind:?} should have a title"
        );
    }
}

/// The dialog's title names the trading surface its set signs for, in both languages: Binance's
/// two markets are two dialogs, and a title that named only the venue would not say which one.
#[test]
fn the_dialog_titles_should_name_the_trading_surface() {
    let i18n = i18n::global::embedded::load().expect("embedded locales should load");
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
            i18n.try_translate(LanguageCode::En, kind.title_key())
                .is_some_and(|text| text.as_ref() == english),
            "{kind:?}'s title should be written in English"
        );
        assert!(
            i18n.try_translate(LanguageCode::ZhCn, kind.title_key())
                .is_some_and(|text| text.as_ref() == chinese),
            "{kind:?}'s title should be written in Chinese"
        );
    }
}

/// A form submitted without its required values is rejected in place: every empty value is
/// reported under the field it belongs to, and each report goes when the value it complains about
/// is filled in rather than when the first of them is.
///
/// The report is the field's own line of the dialog, drawn under the input: a test that only
/// asserted the dialog stayed open would pass even if nothing told the user what was missing.
#[gpui_kit::test]
fn a_rejected_submission_should_report_every_value_it_is_missing(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;

    click(cx, &kind.edit_selector());
    click(cx, CONFIRM_SELECTOR);

    for field in ["api_key", "secret_key", "passphrase"] {
        assert!(
            bounds(cx, &credentials_error(kind, field)).is_some(),
            "the value {field} was submitted empty and should be reported under its field"
        );
    }
    assert!(
        bounds(cx, DIALOG_SELECTOR).is_some(),
        "a rejected submission should leave the dialog where it can be completed"
    );

    // A field reports what it holds when Enter is pressed in it, which is what takes a report
    // away: what is left in the other fields is still empty, so their reports stay.
    type_into(cx, &credentials_field(kind, "api_key"), "typed-key");
    cx.simulate_keystrokes("enter");
    settle(cx);

    assert!(
        bounds(cx, &credentials_error(kind, "api_key")).is_none(),
        "the report under a value that was filled in should go"
    );
    assert!(
        bounds(cx, &credentials_error(kind, "secret_key")).is_some(),
        "the report under a value that is still empty should stay"
    );
    assert!(
        bounds(cx, &credentials_error(kind, "passphrase")).is_some(),
        "filling one value in should not take the other reports away"
    );

    type_into(cx, &credentials_field(kind, "secret_key"), "typed-secret");
    cx.simulate_keystrokes("enter");
    settle(cx);

    assert!(
        bounds(cx, &credentials_error(kind, "secret_key")).is_none(),
        "the report under the second value should go when it is filled in"
    );
    assert!(
        bounds(cx, &credentials_error(kind, "passphrase")).is_some(),
        "the last value is still empty until the submission takes it"
    );

    type_into(
        cx,
        &credentials_field(kind, "passphrase"),
        "typed-passphrase",
    );
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "a completed form should be submitted and take the dialog away"
    );
    let written = stored(cx);
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

/// Submitting a field without changing what it holds does not take away the report of what it is
/// missing: only a value that actually changed clears the report, so an empty field stays reported
/// however often it is left or submitted.
#[gpui_kit::test]
fn submitting_a_field_unchanged_should_keep_its_report(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;

    click(cx, &kind.edit_selector());
    click(cx, CONFIRM_SELECTOR);

    let field = credentials_field(kind, "api_key");
    click(cx, &field);
    cx.simulate_keystrokes("enter");
    settle(cx);

    assert!(
        bounds(cx, &credentials_error(kind, "api_key")).is_some(),
        "a field submitted without a new value should still report what it holds empty"
    );
}

/// Binance's passphrase is the one value a set is complete without, and the form says so twice:
/// an empty required value is reported under its field, and an empty passphrase is not.
#[gpui_kit::test]
fn binances_passphrase_should_not_be_reported_as_missing(cx: &mut TestAppContext) {
    let (panel, cx) = settings(Config::default(), cx);
    select(&panel, "market_participants.binance", cx);
    let kind = CredentialsKind::BinanceUsdMPerp;

    click(cx, &kind.edit_selector());
    click(cx, CONFIRM_SELECTOR);

    for field in ["api_key", "private_key"] {
        assert!(
            bounds(cx, &credentials_error(kind, field)).is_some(),
            "the required value {field} should be reported when it is empty"
        );
    }
    assert!(
        bounds(cx, &credentials_error(kind, "passphrase")).is_none(),
        "Binance's passphrase may be left out, so it is not reported as missing"
    );

    type_into(cx, &credentials_field(kind, "api_key"), "perp-key");
    type_into(cx, &credentials_field(kind, "private_key"), "perp-private");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "a set with its required values and no passphrase should be written"
    );
    let written = stored(cx);
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

/// The dialog ends the way the application's other dialogs end: a line across it, and the two
/// buttons under that line rather than in among the fields. The fields are one column in the
/// order the set declares them, above the foot, and the foot is the dialog's own bottom.
#[gpui_kit::test]
fn the_credentials_dialog_should_be_drawn_the_way_the_application_draws_its_dialogs(
    cx: &mut TestAppContext,
) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;
    click(cx, &kind.edit_selector());

    let dialog = element(cx, DIALOG_SELECTOR);
    let footer = element(cx, FOOTER_SELECTOR);
    let confirm = element(cx, CONFIRM_SELECTOR);
    let cancel = element(cx, CANCEL_SELECTOR);

    assert_eq!(
        dialog.size.width,
        px(WIDTH),
        "the credentials dialog is 480 px wide: it is {dialog:?}"
    );

    let fields = kind
        .fields()
        .iter()
        .map(|field| (field, element(cx, &kind.field_selector(field))))
        .collect::<Vec<_>>();
    for (field, bounds) in &fields {
        assert!(
            bounds.bottom() <= footer.top(),
            "the value {} belongs above the line the foot starts with: {bounds:?} over \
             {footer:?}",
            field.id
        );
    }
    for pair in fields.windows(2) {
        assert!(
            pair[0].1.top() < pair[1].1.top(),
            "the values should be drawn in the order the set declares them: {} is above {}",
            pair[0].0.id,
            pair[1].0.id
        );
    }
    assert_eq!(
        footer.bottom(),
        dialog.bottom(),
        "the foot should be the bottom of the dialog: {footer:?} in {dialog:?}"
    );
    assert!(
        footer.contains(&cancel.center()) && footer.contains(&confirm.center()),
        "both buttons belong to the foot rather than to the fields: {cancel:?} and {confirm:?} in \
         {footer:?}"
    );
    assert!(
        cancel.right() < confirm.left(),
        "the button that leaves the set alone is before the one that writes it: {cancel:?} \
         before {confirm:?}"
    );
}

/// A credential row says what is configured and offers the two things a row can do: the dialog
/// that edits the set, and — for a set the settings hold — the alert that clears it. The buttons
/// end the control column in that order, so the one that takes something away is never where the
/// one that changes it is expected to be.
#[gpui_kit::test]
fn a_credential_rows_buttons_should_end_the_control_column(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "market_participants.okx", cx);
    let kind = CredentialsKind::Okx;

    let row = element(cx, &credentials_row(kind));
    let control = element(cx, &format!("SETTINGS_CONTROL_{}", kind.row_id()));
    let edit = element(cx, &kind.edit_selector());
    let clear = element(cx, &credentials_clear(kind));

    assert!(
        row.contains(&edit.center()) && row.contains(&clear.center()),
        "both buttons belong to the row they act on: {row:?} holds {edit:?} and {clear:?}"
    );
    assert!(
        control.contains(&edit.center()) && control.contains(&clear.center()),
        "both buttons belong to the row's control column: {control:?} holds {edit:?} and \
         {clear:?}"
    );
    assert!(
        edit.right() <= clear.left(),
        "the edit button is before the one that takes the set away: {edit:?} before {clear:?}"
    );
    assert!(
        (clear.right() - control.right()).abs() < px(1.0),
        "the clear button should end the control column: {clear:?} in {control:?}"
    );

    // The edit button is the row's own: it is there whether or not a set is stored, because
    // writing one is what a row with nothing stored is for.
    edit_settings(
        &panel,
        |config| {
            config.set_okx_credentials(&VenueId::Okx, None);
        },
        cx,
    );

    assert!(
        bounds(cx, &kind.edit_selector()).is_some(),
        "the edit button is there whether or not a set is stored"
    );
    assert!(
        bounds(cx, &credentials_clear(kind)).is_none(),
        "a row with nothing stored has nothing to clear"
    );
}

/// The Hyperliquid dialog opens over the set the settings hold and writes the whole set: what the
/// user types is appended to the stored value, the value the user did not touch is written back as
/// it was, and a cancelled dialog writes nothing at all.
#[gpui_kit::test]
fn a_hyperliquid_dialog_should_open_prefilled_and_write_the_whole_set(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    let kind = CredentialsKind::Hyperliquid;
    select(&panel, &credentials_page(kind), cx);
    let before = stored(cx);

    click(cx, &kind.edit_selector());
    type_into(
        cx,
        &credentials_field(kind, "api_agent_private_key"),
        "typed",
    );
    click(cx, CANCEL_SELECTOR);

    assert_eq!(
        stored(cx),
        before,
        "a cancelled dialog must not write the set it was opened over"
    );

    click(cx, &kind.edit_selector());
    type_into(
        cx,
        &credentials_field(kind, "main_account_address"),
        "typed",
    );
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "a stored set should take the dialog away when it is confirmed"
    );
    let written = stored(cx);
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

/// A venue that signs its whole surface with one set is cleared by the alert its row opens: what
/// is cleared is that venue's set, and neither the other venues nor Binance's two markets are
/// touched by it.
#[gpui_kit::test]
fn clearing_a_venue_should_write_none_and_leave_the_other_venues_alone(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    let venues = [
        CredentialsKind::Okx,
        CredentialsKind::Hyperliquid,
        CredentialsKind::Longbridge,
    ];

    for (index, kind) in venues.iter().copied().enumerate() {
        select(&panel, &credentials_page(kind), cx);
        click(cx, &credentials_clear(kind));
        assert!(
            bounds(cx, "BASE_UI_ALERT").is_some(),
            "clearing {kind:?} should be put to the user before it is written"
        );
        click(cx, "BASE_UI_ALERT_OK");

        let config = stored(cx);
        match kind {
            CredentialsKind::Okx => {
                let Some(VenueSettings::Okx(okx)) = config.venue(&VenueId::Okx) else {
                    panic!("okx is configured as OKX");
                };
                assert_eq!(
                    okx.credentials, None,
                    "confirming the alert should clear the set OKX signs with"
                );
            }
            CredentialsKind::Hyperliquid => {
                let Some(VenueSettings::Hyperliquid(settings)) =
                    config.venue(&VenueId::Hyperliquid)
                else {
                    panic!("hyperliquid is configured as Hyperliquid");
                };
                assert_eq!(
                    settings.credentials, None,
                    "confirming the alert should clear the set Hyperliquid signs with"
                );
            }
            CredentialsKind::Longbridge => {
                let Some(VenueSettings::Longbridge(settings)) = config.venue(&VenueId::Longbridge)
                else {
                    panic!("longbridge is configured as Longbridge");
                };
                assert_eq!(
                    settings.credentials, None,
                    "confirming the alert should clear the set Longbridge signs with"
                );
            }
            CredentialsKind::BinanceSpot | CredentialsKind::BinanceUsdMPerp => {
                unreachable!("the loop only clears the venues that sign with one set")
            }
        }

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

/// A set the settings hold is a set the row can clear, even when a hand-edited file left every
/// value of it empty: the row says what is stored rather than what it holds, and the dialog is
/// where an incomplete set is completed. The form opens on the empty values the settings hold —
/// what is typed is the whole of what lands in them — and a submission that leaves a required
/// value out is rejected until the value is filled in.
#[gpui_kit::test]
fn a_stored_but_empty_set_should_be_completed_or_cleared(cx: &mut TestAppContext) {
    fn empty() -> Option<BinanceMarketCredentials> {
        Some(BinanceMarketCredentials {
            api_key: SmolStr::default(),
            private_key: SmolStr::default(),
            passphrase: SmolStr::default(),
        })
    }

    let (panel, cx) = settings(Config::default(), cx);
    let kind = CredentialsKind::BinanceSpot;

    edit_settings(
        &panel,
        |config| {
            config.set_binance_market_credentials(&VenueId::Binance, BinanceMarket::Spot, empty());
        },
        cx,
    );
    select(&panel, "market_participants.binance", cx);

    assert!(
        bounds(cx, &credentials_row(kind)).is_some(),
        "a set of empty values is still a set the row is drawn for"
    );
    assert!(
        bounds(cx, &credentials_clear(kind)).is_some(),
        "a set of empty values is still a set the row offers to clear"
    );

    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "api_key"), "only-key");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_some(),
        "the form opened on empty values, so one filled value still leaves the set incomplete"
    );
    assert_eq!(
        stored(cx)
            .venue(&VenueId::Binance)
            .and_then(|venue| match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }),
        empty(),
        "a rejected submission should write nothing, not even the value it was typed with"
    );

    type_into(cx, &credentials_field(kind, "private_key"), "only-private");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "a set completed in the dialog should be written"
    );
    assert_eq!(
        stored(cx)
            .venue(&VenueId::Binance)
            .and_then(|venue| match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }),
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("only-key"),
            private_key: SmolStr::new_static("only-private"),
            passphrase: SmolStr::default(),
        }),
        "what was typed is the whole of what the empty values became"
    );

    click(cx, &credentials_clear(kind));
    click(cx, "BASE_UI_ALERT_OK");

    assert_eq!(
        stored(cx)
            .venue(&VenueId::Binance)
            .and_then(|venue| match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }),
        None,
        "clearing a stored set should leave the settings holding none"
    );
}

/// A set the settings hold is prefilled with the values it does hold, even when a hand-edited file
/// left it incomplete: typing the value that is empty completes the set, and the values that were
/// already there are written back untouched. The row still offers to clear it, because what it asks
/// is whether a set is stored rather than whether every value of it is filled in.
#[gpui_kit::test]
fn a_stored_but_incomplete_set_should_open_prefilled(cx: &mut TestAppContext) {
    fn incomplete() -> Option<BinanceMarketCredentials> {
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("kept-key"),
            private_key: SmolStr::default(),
            passphrase: SmolStr::new_static("kept-passphrase"),
        })
    }

    let (panel, cx) = settings(Config::default(), cx);
    let kind = CredentialsKind::BinanceSpot;

    edit_settings(
        &panel,
        |config| {
            config.set_binance_market_credentials(
                &VenueId::Binance,
                BinanceMarket::Spot,
                incomplete(),
            );
        },
        cx,
    );
    select(&panel, "market_participants.binance", cx);

    assert!(
        bounds(cx, &credentials_row(kind)).is_some(),
        "a set a hand-edited file left partly empty is still a set the row is drawn for"
    );
    assert!(
        bounds(cx, &credentials_clear(kind)).is_some(),
        "the row offers to clear the set whether or not every value of it is filled in"
    );

    click(cx, &kind.edit_selector());
    type_into(cx, &credentials_field(kind, "private_key"), "typed-private");
    click(cx, CONFIRM_SELECTOR);

    assert!(
        bounds(cx, DIALOG_SELECTOR).is_none(),
        "the set becomes complete as soon as the one empty required value is filled in"
    );
    assert_eq!(
        stored(cx)
            .venue(&VenueId::Binance)
            .and_then(|venue| match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }),
        Some(BinanceMarketCredentials {
            api_key: SmolStr::new_static("kept-key"),
            private_key: SmolStr::new_static("typed-private"),
            passphrase: SmolStr::new_static("kept-passphrase"),
        }),
        "the values the incomplete set already held were prefilled and written back unchanged"
    );

    click(cx, &credentials_clear(kind));
    click(cx, "BASE_UI_ALERT_OK");

    assert_eq!(
        stored(cx)
            .venue(&VenueId::Binance)
            .and_then(|venue| match venue {
                VenueSettings::Binance(binance) => binance.spot_credentials.clone(),
                _ => None,
            }),
        None,
        "clearing the completed set should leave the settings holding none"
    );
}
