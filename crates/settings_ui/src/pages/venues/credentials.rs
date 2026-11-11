//! The trading credentials of the market participants, and the dialog that edits them.
//!
//! A credential set is one value of one venue's shape — or, for Binance, of one market's — and the
//! page draws it as one row. What the settings hold is a set or nothing at all: the row says
//! whether the venue (or market) has a stored set rather than showing the values it holds, and a
//! stored set is treated as configured even when a hand-edited file left one of its values empty.
//! The values are edited in a dialog of their own, opened from the row, because a private key and a
//! passphrase are not a value a page should lay out in the open. The dialog owns a draft of one
//! set: nothing is written until the user confirms it, a cancelled dialog leaves the settings
//! exactly as they were, and a confirmed one writes the set as one change. A required value that
//! was left empty rejects the submission, and the dialog stays open with what the user typed. A
//! stored set that is incomplete is therefore completed in the same dialog; what reports it as
//! invalid at runtime is the venue's own validation, not this page.
//!
//! Clearing is the one row action that takes something away, so it is put to the user before it is
//! written: the alert it is put to is opened from the row, and only what the alert confirms is
//! written. Clearing writes `None`, which leaves the other market of the same venue untouched.
//!
//! The kind of a credential set is the whole of what tells one set from another: which venue it
//! belongs to, which Binance market it signs for, the row it is drawn under, the fields it holds,
//! and how it is read, written and cleared. Every venue page is written with the same helpers
//! rather than with a copy of this dialog.

use ahash::AHashMap;
use app_config::{
    market_participants::{
        credentials::{
            BinanceMarketCredentials, HyperliquidCredentials, LongbridgeCredentials, OkxCredentials,
        },
        origins::BinanceMarket,
        settings::VenueSettings,
    },
    settings::Config,
};
use base_ui::{
    button::Button,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    separator::Separator,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, DismissEvent, ElementId, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, div, px,
};
use i18n::t;
use market_participants_model::identifier::VenueId;
use smol_str::SmolStr;
use widget::venue::VenueDisplay;

use crate::{
    layout::{self, PageArgs},
    search::{FieldSearch, words},
    session::{SettingsChange, SettingsSession},
};

/// How wide the dialog is.
pub(crate) const WIDTH: f32 = 480.0;

/// What the dialog is drawn under, which is what a test finds it by.
pub(crate) const DIALOG_SELECTOR: &str = "SETTINGS_CREDENTIALS_DIALOG";

/// What the dialog's title is drawn under.
pub(crate) const TITLE_SELECTOR: &str = "SETTINGS_CREDENTIALS_TITLE";

/// What the row the dialog ends in is drawn under: the line across it and the two buttons under
/// that line, which is where the dialog says what it does rather than what it holds.
pub(crate) const FOOTER_SELECTOR: &str = "SETTINGS_CREDENTIALS_FOOTER";

/// What the button that writes the set is drawn under, which is what a test clicks it by.
pub(crate) const CONFIRM_SELECTOR: &str = "SETTINGS_CREDENTIALS_CONFIRM";

/// What the button that leaves the set alone is drawn under.
pub(crate) const CANCEL_SELECTOR: &str = "SETTINGS_CREDENTIALS_CANCEL";

/// One value of one credential set.
///
/// The id is what the value is stored and identified by: it is the field the venue's API signs
/// with, and it is part of the element id every input of the dialog is keyed under. The error key
/// is the sentence a required value is rejected with, and no key for an optional one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CredentialField {
    pub id: &'static str,
    pub label_key: &'static str,
    pub placeholder_key: Option<&'static str>,
    pub password: bool,
    pub error_key: Option<&'static str>,
}

const BINANCE_FIELDS: [CredentialField; 3] = [
    CredentialField {
        id: "api_key",
        label_key: "settings_ui.market_participants.api_key",
        placeholder_key: Some("settings_ui.market_participants.enter_api_key"),
        password: false,
        error_key: Some("settings_ui.market_participants.api_key_required"),
    },
    CredentialField {
        id: "private_key",
        label_key: "settings_ui.market_participants.ed25519_private_key",
        placeholder_key: Some("settings_ui.market_participants.enter_private_key"),
        password: true,
        error_key: Some("settings_ui.market_participants.private_key_required"),
    },
    CredentialField {
        id: "passphrase",
        label_key: "settings_ui.market_participants.ed25519_passphrase",
        placeholder_key: Some("settings_ui.market_participants.enter_passphrase"),
        password: true,
        error_key: None,
    },
];

const OKX_FIELDS: [CredentialField; 3] = [
    CredentialField {
        id: "api_key",
        label_key: "settings_ui.market_participants.api_key",
        placeholder_key: Some("settings_ui.market_participants.enter_api_key"),
        password: false,
        error_key: Some("settings_ui.market_participants.api_key_required"),
    },
    CredentialField {
        id: "secret_key",
        label_key: "settings_ui.market_participants.secret_key",
        placeholder_key: Some("settings_ui.market_participants.enter_secret_key"),
        password: true,
        error_key: Some("settings_ui.market_participants.secret_key_required"),
    },
    CredentialField {
        id: "passphrase",
        label_key: "settings_ui.market_participants.passphrase",
        placeholder_key: Some("settings_ui.market_participants.enter_passphrase"),
        password: true,
        error_key: Some("settings_ui.market_participants.passphrase_required"),
    },
];

const HYPERLIQUID_FIELDS: [CredentialField; 2] = [
    CredentialField {
        id: "main_account_address",
        label_key: "settings_ui.market_participants.main_account_address",
        placeholder_key: None,
        password: false,
        error_key: Some("settings_ui.market_participants.main_account_address_required"),
    },
    CredentialField {
        id: "api_agent_private_key",
        label_key: "settings_ui.market_participants.api_agent_private_key",
        placeholder_key: Some("settings_ui.market_participants.enter_private_key"),
        password: true,
        error_key: Some("settings_ui.market_participants.api_agent_private_key_required"),
    },
];

const LONGBRIDGE_FIELDS: [CredentialField; 3] = [
    CredentialField {
        id: "app_key",
        label_key: "settings_ui.market_participants.app_key",
        placeholder_key: Some("settings_ui.market_participants.enter_app_key"),
        password: false,
        error_key: Some("settings_ui.market_participants.app_key_required"),
    },
    CredentialField {
        id: "app_secret",
        label_key: "settings_ui.market_participants.app_secret",
        placeholder_key: Some("settings_ui.market_participants.enter_app_secret"),
        password: true,
        error_key: Some("settings_ui.market_participants.app_secret_required"),
    },
    CredentialField {
        id: "access_token",
        label_key: "settings_ui.market_participants.access_token",
        placeholder_key: Some("settings_ui.market_participants.enter_access_token"),
        password: true,
        error_key: Some("settings_ui.market_participants.access_token_required"),
    },
];

/// One venue's — or one Binance market's — credential set as the settings page configures it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialsKind {
    BinanceSpot,
    BinanceUsdMPerp,
    Okx,
    Hyperliquid,
    Longbridge,
}

impl CredentialsKind {
    /// The credential set of one Binance market.
    pub(crate) const fn for_market(market: BinanceMarket) -> Self {
        match market {
            BinanceMarket::Spot => Self::BinanceSpot,
            BinanceMarket::UsdMPerp => Self::BinanceUsdMPerp,
        }
    }

    /// The venue this set signs for.
    pub(crate) const fn venue(self) -> VenueId {
        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => VenueId::Binance,
            Self::Okx => VenueId::Okx,
            Self::Hyperliquid => VenueId::Hyperliquid,
            Self::Longbridge => VenueId::Longbridge,
        }
    }

    /// The Binance market this set signs for, or nothing for a venue that signs its whole surface
    /// with one set.
    pub(crate) const fn market(self) -> Option<BinanceMarket> {
        match self {
            Self::BinanceSpot => Some(BinanceMarket::Spot),
            Self::BinanceUsdMPerp => Some(BinanceMarket::UsdMPerp),
            Self::Okx | Self::Hyperliquid | Self::Longbridge => None,
        }
    }

    /// The id the row is drawn and searched under.
    pub(crate) const fn row_id(self) -> &'static str {
        match self {
            Self::BinanceSpot => "market_participants.binance.spot.credentials",
            Self::BinanceUsdMPerp => "market_participants.binance.usd-m-perp.credentials",
            Self::Okx => "market_participants.okx.credentials",
            Self::Hyperliquid => "market_participants.hyperliquid.credentials",
            Self::Longbridge => "market_participants.longbridge.credentials",
        }
    }

    /// The prefix every field of the dialog is identified by.
    ///
    /// A field is a value of one set, so its id is the set's plus the field's own: the dialog of
    /// one market and the dialog of its sibling are drawn from the same code, and the only thing
    /// that tells their fields apart is which set they belong to.
    pub(crate) const fn field_prefix(self) -> &'static str {
        match self {
            Self::BinanceSpot => "market_participants.binance.spot",
            Self::BinanceUsdMPerp => "market_participants.binance.usd-m-perp",
            Self::Okx => "market_participants.okx",
            Self::Hyperliquid => "market_participants.hyperliquid",
            Self::Longbridge => "market_participants.longbridge",
        }
    }

    /// The name the dialog is titled with: it says which trading surface the set signs for, so a
    /// user editing one of Binance's two markets is told which one.
    pub(crate) const fn title_key(self) -> &'static str {
        match self {
            Self::BinanceSpot => "settings_ui.market_participants.binance_spot_credentials_title",
            Self::BinanceUsdMPerp => {
                "settings_ui.market_participants.binance_usd_m_perp_credentials_title"
            }
            Self::Okx => "settings_ui.market_participants.okx_credentials_title",
            Self::Hyperliquid => "settings_ui.market_participants.hyperliquid_credentials_title",
            Self::Longbridge => "settings_ui.market_participants.longbridge_credentials_title",
        }
    }

    /// The title the alert that clears a set is drawn with.
    ///
    /// The title and the message are read together from here rather than spelled out at the call
    /// site, so the alert the row opens is the sentence this set's own part of the page asks for.
    pub(crate) fn clear_title(self, cx: &App) -> SharedString {
        t!(
            cx,
            "settings_ui.market_participants.clear_credentials_title"
        )
    }

    /// The sentence the alert that clears a set asks with.
    ///
    /// The title of the set is written into it, so a user confirming a removal is told which set
    /// goes; it is the same name the dialog that edits the set is titled with.
    pub(crate) fn clear_message(self, cx: &App) -> SharedString {
        let title = t!(cx, self.title_key());

        t!(
            cx,
            "settings_ui.market_participants.clear_credentials_message",
            title = title.as_str()
        )
    }

    /// The values one set of this kind holds, in the order the dialog lays them out.
    pub(crate) const fn fields(self) -> &'static [CredentialField] {
        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => &BINANCE_FIELDS,
            Self::Okx => &OKX_FIELDS,
            Self::Hyperliquid => &HYPERLIQUID_FIELDS,
            Self::Longbridge => &LONGBRIDGE_FIELDS,
        }
    }

    /// The id one value of this set is drawn and keyed under.
    pub(crate) fn field_id(self, field: &CredentialField) -> SharedString {
        SharedString::from(format!("{}.{}", self.field_prefix(), field.id))
    }

    /// What the row's edit button is drawn under.
    pub(crate) fn edit_selector(self) -> String {
        format!("SETTINGS_EDIT_CREDENTIALS_{}", self.row_id())
    }

    /// What the row's clear button is drawn under.
    pub(crate) fn clear_selector(self) -> String {
        format!("SETTINGS_CLEAR_CREDENTIALS_{}", self.row_id())
    }

    /// What one field's line of the dialog is drawn under.
    pub(crate) fn field_selector(self, field: &CredentialField) -> String {
        format!("SETTINGS_CREDENTIALS_FIELD_{}", self.field_id(field))
    }

    /// What one field's error is drawn under, when the field has one.
    pub(crate) fn error_selector(self, field: &CredentialField) -> String {
        format!("SETTINGS_CREDENTIALS_ERROR_{}", self.field_id(field))
    }

    /// Whether the settings hold a set for this kind.
    ///
    /// The row clears what is stored, so the question is whether a set is there at all —
    /// `is_some` — rather than whether every value of it is filled in: a set a hand-edited
    /// file left incomplete is still a set the user can clear, and completing it is what
    /// the dialog is for.
    pub(crate) fn has_stored_set(self, config: &Config) -> bool {
        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => {
                let Some(market) = self.market() else {
                    return false;
                };
                match config.venue(&self.venue()) {
                    Some(VenueSettings::Binance(settings)) => {
                        settings.market_credentials(market).is_some()
                    }
                    _ => false,
                }
            }
            Self::Okx => matches!(
                config.venue(&self.venue()),
                Some(VenueSettings::Okx(settings)) if settings.credentials.is_some()
            ),
            Self::Hyperliquid => matches!(
                config.venue(&self.venue()),
                Some(VenueSettings::Hyperliquid(settings)) if settings.credentials.is_some()
            ),
            Self::Longbridge => matches!(
                config.venue(&self.venue()),
                Some(VenueSettings::Longbridge(settings)) if settings.credentials.is_some()
            ),
        }
    }

    /// The set the settings hold, as the dialog's draft: a set nothing is stored for is an empty
    /// form, and a stored one is prefilled with every value it holds.
    pub(crate) fn draft(self, config: &Config) -> CredentialsDraft {
        let mut draft = CredentialsDraft::empty(self);

        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => {
                let Some(market) = self.market() else {
                    return draft;
                };
                let Some(VenueSettings::Binance(settings)) = config.venue(&self.venue()) else {
                    return draft;
                };
                let Some(credentials) = settings.market_credentials(market) else {
                    return draft;
                };
                draft.set("api_key", credentials.api_key.clone());
                draft.set("private_key", credentials.private_key.clone());
                draft.set("passphrase", credentials.passphrase.clone());
            }
            Self::Okx => {
                let Some(VenueSettings::Okx(settings)) = config.venue(&self.venue()) else {
                    return draft;
                };
                let Some(credentials) = settings.credentials.as_ref() else {
                    return draft;
                };
                draft.set("api_key", credentials.api_key.clone());
                draft.set("secret_key", credentials.secret_key.clone());
                draft.set("passphrase", credentials.passphrase.clone());
            }
            Self::Hyperliquid => {
                let Some(VenueSettings::Hyperliquid(settings)) = config.venue(&self.venue()) else {
                    return draft;
                };
                let Some(credentials) = settings.credentials.as_ref() else {
                    return draft;
                };
                draft.set(
                    "main_account_address",
                    credentials.main_account_address.clone(),
                );
                draft.set(
                    "api_agent_private_key",
                    credentials.api_agent_private_key.clone(),
                );
            }
            Self::Longbridge => {
                let Some(VenueSettings::Longbridge(settings)) = config.venue(&self.venue()) else {
                    return draft;
                };
                let Some(credentials) = settings.credentials.as_ref() else {
                    return draft;
                };
                draft.set("app_key", credentials.app_key.clone());
                draft.set("app_secret", credentials.app_secret.clone());
                draft.set("access_token", credentials.access_token.clone());
            }
        }

        draft
    }

    /// Writes the set the draft holds, exactly as the user typed it.
    pub(crate) fn save(self, config: &mut Config, draft: &CredentialsDraft) {
        let venue = self.venue();
        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => {
                let Some(market) = self.market() else {
                    return;
                };
                let credentials = BinanceMarketCredentials {
                    api_key: SmolStr::new(draft.value("api_key")),
                    private_key: SmolStr::new(draft.value("private_key")),
                    passphrase: SmolStr::new(draft.value("passphrase")),
                };
                config.set_binance_market_credentials(&venue, market, Some(credentials));
            }
            Self::Okx => {
                let credentials = OkxCredentials {
                    api_key: SmolStr::new(draft.value("api_key")),
                    secret_key: SmolStr::new(draft.value("secret_key")),
                    passphrase: SmolStr::new(draft.value("passphrase")),
                };
                config.set_okx_credentials(&venue, Some(credentials));
            }
            Self::Hyperliquid => {
                let credentials = HyperliquidCredentials {
                    main_account_address: SmolStr::new(draft.value("main_account_address")),
                    api_agent_private_key: SmolStr::new(draft.value("api_agent_private_key")),
                };
                config.set_hyperliquid_credentials(&venue, Some(credentials));
            }
            Self::Longbridge => {
                let credentials = LongbridgeCredentials {
                    app_key: SmolStr::new(draft.value("app_key")),
                    app_secret: SmolStr::new(draft.value("app_secret")),
                    access_token: SmolStr::new(draft.value("access_token")),
                };
                config.set_longbridge_credentials(&venue, Some(credentials));
            }
        }
    }

    /// Clears the set, and only the set: Binance's other market keeps its own.
    pub(crate) fn clear(self, config: &mut Config) {
        let venue = self.venue();
        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => {
                let Some(market) = self.market() else {
                    return;
                };
                config.set_binance_market_credentials(&venue, market, None);
            }
            Self::Okx => {
                config.set_okx_credentials(&venue, None);
            }
            Self::Hyperliquid => {
                config.set_hyperliquid_credentials(&venue, None);
            }
            Self::Longbridge => {
                config.set_longbridge_credentials(&venue, None);
            }
        }
    }

    /// The search row of this set.
    ///
    /// What is searched is the row and what it is for, never the name of a field it holds: the
    /// fields are values of the row, and a user looking for a credential looks for the credentials
    /// rather than for the spelling one venue gives one of them.
    pub(crate) fn search_field(self, cx: &App) -> FieldSearch {
        let label = t!(cx, "settings_ui.market_participants.credentials");
        let description = t!(
            cx,
            "settings_ui.market_participants.credentials_description"
        );
        // The venue is searched by the name it is drawn with as well as by the identity it is
        // stored under: the identity is `binance` in every language, and a user looking for the
        // venue's credentials in Chinese types the name the page shows them.
        let venue_name = t!(cx, self.venue().name_key());
        let mut parts = Vec::with_capacity(10);
        parts.push(self.row_id());
        parts.push(label.as_ref());
        parts.push(description.as_ref());
        parts.push("market_participants");
        parts.push("credentials");
        parts.push("凭据");
        parts.push(self.venue().as_str());
        parts.push(venue_name.as_ref());
        let market_title = self
            .market()
            .map(|market| (market, t!(cx, market_title_key(market))));
        if let Some((market, title)) = &market_title {
            parts.push(market.scope());
            parts.push(title.as_ref());
        }

        FieldSearch {
            id: SharedString::from(self.row_id()),
            text: words(&parts),
        }
    }
}

/// The name one Binance market is drawn under, which the set's search row repeats.
const fn market_title_key(market: BinanceMarket) -> &'static str {
    match market {
        BinanceMarket::Spot => "settings_ui.market_participants.binance_spot",
        BinanceMarket::UsdMPerp => "settings_ui.market_participants.binance_usd_m_perp",
    }
}

/// One set's values as the dialog holds them, keyed by the field they belong to.
#[derive(Clone)]
pub(crate) struct CredentialsDraft {
    values: AHashMap<&'static str, SmolStr>,
}

impl CredentialsDraft {
    /// The form a set nothing is stored for opens on.
    fn empty(kind: CredentialsKind) -> Self {
        let fields = kind.fields();
        let mut values = AHashMap::with_capacity(fields.len());
        for field in fields {
            values.insert(field.id, SmolStr::default());
        }

        Self { values }
    }

    /// What one field holds; a field the draft does not name holds nothing.
    fn value(&self, id: &str) -> &str {
        self.values.get(id).map_or("", SmolStr::as_str)
    }

    /// Takes what one field holds.
    fn set(&mut self, id: &'static str, value: SmolStr) {
        self.values.insert(id, value);
    }
}

/// The row one credential set is drawn as.
///
/// The row says what is configured rather than showing it: the edit button is always there, and a
/// set the settings hold has a clear button beside it. Clearing is asked about before it is
/// written, because it is the one thing a row can do that takes something away.
pub(crate) fn credentials_row(args: &mut PageArgs<'_, '_>, kind: CredentialsKind) -> AnyElement {
    let label = t!(args.cx, "settings_ui.market_participants.credentials");
    let description = t!(
        args.cx,
        "settings_ui.market_participants.credentials_description"
    );
    let edit_label = t!(args.cx, "settings_ui.market_participants.edit_credentials");
    // The button says what it edits as well as what it does: Binance's two markets are two rows
    // of the same page, so a label that named only the action would tell them apart by nothing a
    // reader of the accessibility tree can hear.
    let title = t!(args.cx, kind.title_key());
    let edit_aria = t!(
        args.cx,
        "settings_ui.market_participants.edit_credentials_for",
        title = title.as_str()
    );
    let session = args.settings.session_entity();
    let mut controls = div().flex().items_center().justify_end().gap_2();

    controls = controls.child(
        Button::new(layout::element_id(
            "settings-credentials-edit",
            kind.row_id(),
        ))
        .medium()
        .tonal()
        .debug_selector({
            let selector = kind.edit_selector();
            move || selector
        })
        .aria_label(edit_aria)
        .child(edit_label)
        .on_click(move |_, window, cx| {
            CredentialsEditor::show(kind, session.clone(), window, cx);
        }),
    );

    if kind.has_stored_set(args.config) {
        let clear_label = t!(args.cx, "settings_ui.market_participants.clear_credentials");
        let clear_aria = t!(
            args.cx,
            "settings_ui.market_participants.clear_credentials_for",
            title = title.as_str()
        );
        let session = args.settings.session_entity();

        controls = controls.child(
            Button::new(layout::element_id(
                "settings-credentials-clear",
                kind.row_id(),
            ))
            .medium()
            .tonal()
            .debug_selector({
                let selector = kind.clear_selector();
                move || selector
            })
            .aria_label(clear_aria)
            .child(clear_label)
            .on_click(move |_, window, cx| {
                // Both the alert and its action are `'static`, so every sentence is resolved
                // here, where the application is still known to be in the language it is
                // drawn in.
                let title = kind.clear_title(cx);
                let message = kind.clear_message(cx);
                let confirm = t!(cx, "common.confirm");
                let cancel = t!(cx, "common.cancel");
                let session = session.clone();

                window.alert(cx, move |alert, _, _| {
                    alert
                        .ok_cancel(title, message)
                        .ok_text(confirm)
                        .cancel_text(cancel)
                        .ok_action(move |_, _, cx| {
                            session.update(cx, |session, cx| {
                                let change = SettingsChange::edit(move |config| kind.clear(config));
                                let _ = session.write(change, cx);
                            });
                        })
                });
            }),
        );
    }

    layout::row(
        kind.row_id(),
        label,
        description,
        controls.into_any_element(),
        layout::CONTROL_WIDTH,
        args.cx,
    )
}

/// The dialog that edits one credential set.
pub(crate) struct CredentialsEditor {
    kind: CredentialsKind,
    draft: CredentialsDraft,
    /// The settings session the set is written through.
    session: Entity<SettingsSession>,
    /// The reason each rejected field was rejected, if the current submission was rejected.
    errors: AHashMap<&'static str, &'static str>,
    /// The state of every value of the dialog, keyed by the field it belongs to.
    inputs: AHashMap<&'static str, Entity<InputState>>,
    /// Held because a field reports what it holds when the caret leaves it, and the dialog has to
    /// hear that to know what was submitted.
    _events: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for CredentialsEditor {}

impl CredentialsEditor {
    /// Opens the dialog over what the settings hold for this set.
    pub(crate) fn show(
        kind: CredentialsKind,
        session: Entity<SettingsSession>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let draft = {
            let config = session.read(cx).config(cx);
            kind.draft(config.as_ref())
        };
        let fields = kind.fields();
        let mut inputs = AHashMap::with_capacity(fields.len());
        for field in fields {
            let input = input(kind, field, draft.value(field.id), window, cx);
            inputs.insert(field.id, input);
        }
        let editor = cx.new(|cx| {
            // Every field reports the value it holds when the caret leaves it — and when Enter is
            // pressed — which is the moment the draft takes it; the confirm button reads the
            // fields back as well, because clicking it does not take the caret away from what was
            // typed last.
            let events = fields
                .iter()
                .map(|field| {
                    let id = field.id;
                    cx.subscribe(
                        &inputs[id],
                        move |editor: &mut Self, _, event: &InputStateEvent, cx| {
                            if let InputStateEvent::Blur(value)
                            | InputStateEvent::PressEnter(value) = event
                            {
                                editor.write(id, value.clone(), cx);
                            }
                        },
                    )
                })
                .collect::<Vec<_>>();

            Self {
                kind,
                draft,
                session,
                errors: AHashMap::with_capacity(fields.len()),
                inputs,
                _events: events,
            }
        });

        window.dialog(cx, editor.clone());
        editor.update(cx, |_, cx| cx.notify());
    }

    /// Takes one field's value into the draft, which is what a field reports when it is left.
    fn write(&mut self, id: &'static str, value: SharedString, cx: &mut Context<Self>) {
        let changed = self.draft.value(id) != value.as_str();
        self.draft.set(id, SmolStr::new(value.as_str()));
        // Only the field whose value actually changed stops being reported: leaving a field
        // without changing what it holds must not take away the report of what it is still
        // missing, and the other fields' reports stay until they are filled in as well.
        if changed {
            self.errors.remove(id);
        }
        cx.notify();
    }

    /// Takes what every field currently holds into the draft.
    ///
    /// A field reports itself when the caret leaves it, and clicking the confirm button does not
    /// take the caret away from it: without this, the last thing typed before the button was
    /// pressed would be the one thing the dialog did not carry.
    fn commit_inputs(&mut self, cx: &mut Context<Self>) {
        // The fields are read while the draft is written, and the draft is written through
        // `self`, so what to write is collected before anything is written.
        let pending = self
            .kind
            .fields()
            .iter()
            .map(|field| (field.id, self.inputs[field.id].read(cx).value(cx)))
            .collect::<Vec<_>>();

        for (id, value) in pending {
            self.write(id, value, cx);
        }
    }

    /// Every required value the submission left empty.
    fn validate(&self) -> AHashMap<&'static str, &'static str> {
        let mut errors = AHashMap::with_capacity(self.kind.fields().len());
        for field in self.kind.fields() {
            let Some(error) = field.error_key else {
                continue;
            };
            if self.draft.value(field.id).trim().is_empty() {
                errors.insert(field.id, error);
            }
        }

        errors
    }

    /// Writes the set into the settings, or reports every problem with the form.
    ///
    /// A set that has been stored takes the dialog off the layer. One that could not be stored
    /// leaves it open with what the user typed: the settings page reports the reason, and the
    /// dialog is where the set is submitted again from.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        self.commit_inputs(cx);
        let errors = self.validate();
        if !errors.is_empty() {
            self.errors = errors;
            cx.notify();
            return;
        }

        let kind = self.kind;
        let draft = self.draft.clone();
        let stored = self.session.update(cx, |session, cx| {
            session.write(
                SettingsChange::edit(move |config| kind.save(config, &draft)),
                cx,
            )
        });

        match stored {
            Ok(()) => cx.emit(DismissEvent),
            // The reason is the settings page's to report, and the dialog stays where the user can
            // submit it again.
            Err(_) => cx.notify(),
        }
    }

    /// One field of the dialog, with its label and whatever is wrong with it.
    ///
    /// The error line is drawn whether or not there is anything wrong with the field, so a form
    /// that is rejected does not move everything below the field it complains about. What the
    /// message is drawn under is only there when there is one: a test reading a selector that a
    /// filled-in field also answered with would not be reading the rejection.
    fn field(&self, field: &'static CredentialField, cx: &Context<Self>) -> impl IntoElement {
        let label = t!(cx, field.label_key);
        let error = self.errors.get(field.id).copied();
        let selector = self.kind.field_selector(field);
        let error_line = match error {
            Some(error) => {
                let selector = self.kind.error_selector(field);

                div()
                    .debug_selector(move || selector)
                    .child(t!(cx, error))
                    .into_any_element()
            }
            // A line that holds nothing is still a line: the space it is drawn with is what keeps
            // the height a message would have, so reporting one field does not move everything
            // below it.
            None => div()
                .child(SharedString::new_static(" "))
                .into_any_element(),
        };

        div()
            .debug_selector(move || selector)
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().roles.fg.default.hsla)
                    .child(label),
            )
            .child(Input::new(&self.inputs[field.id]).medium())
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().roles.status.danger.base.hsla)
                    .child(error_line),
            )
    }
}

/// A text field's state, built from what the draft holds.
///
/// The state is keyed under the field's own id, which is what keeps the box one value is typed
/// into from being the box of another: one dialog holds every value of one set, and the sets
/// share this code.
fn input(
    kind: CredentialsKind,
    field: &CredentialField,
    value: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    let id = kind.field_id(field);

    cx.new(|cx| {
        let mut builder = InputStateBuilder::new(ElementId::Name(id)).default_value(value);
        if field.password {
            builder = builder.password();
        }
        if let Some(placeholder) = field.placeholder_key {
            builder = builder.placeholder(t!(cx, placeholder));
        }

        builder.build(window, cx)
    })
}

impl Render for CredentialsEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .debug_selector(|| DIALOG_SELECTOR.into())
            .w(px(WIDTH))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .debug_selector(|| TITLE_SELECTOR.into())
                    .text_base()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .child(t!(cx, self.kind.title_key())),
            )
            .children(self.kind.fields().iter().map(|field| self.field(field, cx)))
            .child(
                // The foot of the dialog is the foot the application's other dialogs end in: a
                // line across it, and the buttons under that line.
                div()
                    .debug_selector(|| FOOTER_SELECTOR.into())
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Separator::horizontal())
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("settings-credentials-cancel")
                                    .ghost()
                                    .with_unified_size(UnifiedSize::Medium)
                                    .debug_selector(|| CANCEL_SELECTOR.into())
                                    .aria_label(t!(cx, "common.cancel"))
                                    .child(t!(cx, "common.cancel"))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("settings-credentials-confirm")
                                    .filled()
                                    .with_unified_size(UnifiedSize::Medium)
                                    .debug_selector(|| CONFIRM_SELECTOR.into())
                                    .aria_label(t!(cx, "common.confirm"))
                                    .child(t!(cx, "common.confirm"))
                                    .on_click(cx.listener(|editor, _, _, cx| {
                                        editor.confirm(cx);
                                    })),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use app_config::settings::Config;
    use gpui_kit::{SharedString, TestAppContext};
    use i18n::t;

    use super::{CredentialsKind, input};
    use crate::settings_tests::test_support::settings;

    /// The five sets the settings edit, each with every value it holds.
    const KINDS: [CredentialsKind; 5] = [
        CredentialsKind::BinanceSpot,
        CredentialsKind::BinanceUsdMPerp,
        CredentialsKind::Okx,
        CredentialsKind::Hyperliquid,
        CredentialsKind::Longbridge,
    ];

    /// The state the dialog builds for one value is the state its declaration describes: a secret
    /// is masked and an identity is not, the hint under an empty field is the sentence the field
    /// declares — or no hint at all — and what the field starts with is what was stored for it.
    ///
    /// The assertions are about the built field rather than about the declaration the field was
    /// built from: a declaration that said one thing while the builder was handed another would
    /// pass a test of the declaration and fail this one.
    #[gpui_kit::test]
    fn every_field_should_be_built_the_way_its_declaration_describes_it(cx: &mut TestAppContext) {
        let (_panel, cx) = settings(Config::default(), cx);
        let built = cx.update(|window, cx| {
            let mut built = Vec::new();
            for kind in KINDS {
                for field in kind.fields() {
                    let input = input(kind, field, "stored-value", window, cx);
                    built.push((kind, field, input));
                }
            }
            built
        });
        cx.read(|cx| {
            for (kind, field, input) in &built {
                let input = input.read(cx);
                assert_eq!(
                    input.is_masked(cx),
                    field.password,
                    "{kind:?}'s {} should be drawn {}",
                    field.id,
                    if field.password { "masked" } else { "as it is" }
                );
                assert_eq!(
                    input.placeholder(cx),
                    field.placeholder_key.map(|key| t!(cx, key)),
                    "{kind:?}'s {} should be hinted with the sentence its declaration names",
                    field.id
                );
                assert_eq!(
                    input.value(cx),
                    SharedString::new_static("stored-value"),
                    "{kind:?}'s {} should start from the value it was built with",
                    field.id
                );
            }
        });
    }
}
