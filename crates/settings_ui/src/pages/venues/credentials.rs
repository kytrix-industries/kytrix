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

use std::{
    cell::RefCell,
    rc::{Rc, Weak},
    sync::Arc,
};

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
    kit::{
        alert::AlertBuilder,
        button::Button,
        dialog::{Dialog, DialogBuilder},
        input::{self, InputNode, InputSpec, InputState},
    },
    style_builder::StyleBuilder,
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use i18n::translate::Translate;
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    paint::style::{PaintSpec, StylePatch},
    text::TextStyle,
    window::WindowId,
};
use widget::venue::VenueDisplay;

use crate::{
    host::SettingsHost,
    layout::{self, RowFn},
    search::{FieldSearch, words},
    session::SettingsChange,
};

/// How wide the dialog is.
const WIDTH: f32 = 480.0;

/// What the dialog's content is drawn under, which is what a test finds it by.
pub(crate) const DIALOG_SELECTOR: &str = "SETTINGS_CREDENTIALS_DIALOG";

/// What the row the dialog ends in is drawn under: the line across it and the two buttons under
/// that line, which is where the dialog says what it does rather than what it holds.
pub(crate) const FOOTER_SELECTOR: &str = "SETTINGS_CREDENTIALS_FOOTER";

/// What the button that writes the set is drawn under.
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
    pub(crate) fn clear_title(self, translate: &Translate) -> SmolStr {
        i18n::t_smol!(
            translate,
            "settings_ui.market_participants.clear_credentials_title"
        )
    }

    /// The sentence the alert that clears a set asks with.
    ///
    /// The title of the set is written into it, so a user confirming a removal is told which set
    /// goes; it is the same name the dialog that edits the set is titled with.
    pub(crate) fn clear_message(self, translate: &Translate) -> SmolStr {
        let title = i18n::t_smol!(translate, self.title_key());

        i18n::t_smol!(
            translate,
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
    pub(crate) fn field_id(self, field: &CredentialField) -> SmolStr {
        format_smolstr!("{}.{}", self.field_prefix(), field.id)
    }

    /// What the row's edit button is drawn under.
    pub(crate) fn edit_selector(self) -> SmolStr {
        format_smolstr!("SETTINGS_EDIT_CREDENTIALS_{}", self.row_id())
    }

    /// What the row's clear button is drawn under.
    pub(crate) fn clear_selector(self) -> SmolStr {
        format_smolstr!("SETTINGS_CLEAR_CREDENTIALS_{}", self.row_id())
    }

    /// What one field's line of the dialog is drawn under.
    pub(crate) fn field_selector(self, field: &CredentialField) -> SmolStr {
        format_smolstr!("SETTINGS_CREDENTIALS_FIELD_{}", self.field_id(field))
    }

    /// What one field's error is drawn under, when the field has one.
    pub(crate) fn error_selector(self, field: &CredentialField) -> SmolStr {
        format_smolstr!("SETTINGS_CREDENTIALS_ERROR_{}", self.field_id(field))
    }

    /// Whether the settings hold a set for this kind.
    ///
    /// The row clears what is stored, so the question is whether a set is there at all — `is_some`
    /// — rather than whether every value of it is filled in: a set a hand-edited file left
    /// incomplete is still a set the user can clear, and completing it is what the dialog is for.
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
    pub(crate) fn search_field(self, translate: &Translate) -> FieldSearch {
        let label = i18n::t_smol!(translate, "settings_ui.market_participants.credentials");
        let description = i18n::t_smol!(
            translate,
            "settings_ui.market_participants.credentials_description"
        );
        // The venue is searched by the name it is drawn with as well as by the identity it is
        // stored under: the identity is `binance` in every language, and a user looking for the
        // venue's credentials in Chinese types the name the page shows them.
        let venue_name = i18n::t_smol!(translate, self.venue().name_key());
        let market_title = self
            .market()
            .map(|market| (market, i18n::t_smol!(translate, market_title_key(market))));
        let mut parts = Vec::with_capacity(10);
        parts.push(self.row_id());
        parts.push(label.as_ref());
        parts.push(description.as_ref());
        parts.push("market_participants");
        parts.push("credentials");
        parts.push("凭据");
        parts.push(self.venue().as_str());
        parts.push(venue_name.as_ref());
        if let Some((market, title)) = &market_title {
            parts.push(market.scope());
            parts.push(title.as_ref());
        }
        let text = words(&parts);

        FieldSearch {
            id: SmolStr::new_static(self.row_id()),
            text,
        }
    }
}

/// The name one Binance market is drawn under, which the set's search row repeats.
pub(crate) const fn market_title_key(market: BinanceMarket) -> &'static str {
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
    pub(crate) fn empty(kind: CredentialsKind) -> Self {
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

/// The credential rows one venue page draws.
///
/// A venue draws one row per set it configures (Binance draws one per market), and every row owns
/// the dialog that edits it and the alert that clears it.
pub(crate) struct CredentialsArea {
    rows: Vec<CredentialsRow>,
}

impl CredentialsArea {
    /// The area of a page that draws no rows yet.
    pub(crate) fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// Builds the row of one set and takes ownership of what it draws with.
    ///
    /// # Errors
    /// Engine node-building errors from the row's controls.
    pub(crate) fn row(
        &mut self,
        app: &mut App,
        window: WindowId,
        host: &SettingsHost,
        base_key: SmolStr,
        kind: CredentialsKind,
        theme: &ThemeHandle,
    ) -> Result<RowFn, EngineError> {
        let (row, row_fn) = CredentialsRow::build(app, window, host, base_key, kind, theme)?;
        self.rows.push(row);

        Ok(row_fn)
    }

    /// Makes every row show what the settings hold for this frame.
    ///
    /// # Errors
    /// A kit component refused a write.
    pub(crate) fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        for row in &mut self.rows {
            row.sync(app)?;
        }

        Ok(())
    }

    /// Offers one platform event to the rows (their dialogs type into their own fields).
    pub(crate) fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let mut consumed = false;
        for row in &self.rows {
            consumed |= row.dispatch(app, event);
        }

        consumed
    }

    /// Releases every component the rows own.
    pub(crate) fn release(&mut self, app: &mut App) {
        for row in self.rows.drain(..) {
            row.release(app);
        }
    }
}

/// One credential row and the dialog that edits it.
struct CredentialsRow {
    kind: CredentialsKind,
    host: SettingsHost,
    window: WindowId,
    theme: ThemeHandle,
    /// The control column the buttons are drawn in, and the clear button when a set is stored.
    state: Rc<RefCell<RowState>>,
    edit: Button,
    editor: Rc<CredentialsEditor>,
    alert: Dialog,
    /// The key the clear button is named by when it is drawn.
    clear_key: SmolStr,
    /// What the clear button is labelled and named for, resolved in the language of the build.
    clear_label: SmolStr,
    clear_aria: SmolStr,
}

/// What one row currently draws below its control column.
#[derive(Default)]
struct RowState {
    /// The column the buttons are drawn in; filled when the row is built.
    controls: Option<NodeId>,
    /// The wrapper and button of the clear control, when a set is stored.
    clear: Option<(NodeId, Button)>,
}

impl CredentialsRow {
    /// Builds one row and returns it with the row builder the page puts in its card.
    fn build(
        app: &mut App,
        window: WindowId,
        host: &SettingsHost,
        base_key: SmolStr,
        kind: CredentialsKind,
        theme: &ThemeHandle,
    ) -> Result<(Self, RowFn), EngineError> {
        let translate = host.translate();
        let label = i18n::t_smol!(translate, "settings_ui.market_participants.credentials");
        let description = i18n::t_smol!(
            translate,
            "settings_ui.market_participants.credentials_description"
        );
        let edit_label = i18n::t_smol!(
            translate,
            "settings_ui.market_participants.edit_credentials"
        );
        // The button says what it edits as well as what it does: Binance's two markets are two rows
        // of the same page, so a label that named only the action would tell them apart by nothing
        // a reader of the accessibility tree can hear.
        let clear_label = i18n::t_smol!(
            translate,
            "settings_ui.market_participants.clear_credentials"
        );
        let title = i18n::t_smol!(translate, kind.title_key());
        let edit_aria = i18n::t_smol!(
            translate,
            "settings_ui.market_participants.edit_credentials_for",
            title = title.as_str()
        );
        let clear_aria = i18n::t_smol!(
            translate,
            "settings_ui.market_participants.clear_credentials_for",
            title = title.as_str()
        );
        let edit_key = format_smolstr!("{base_key}.edit");
        let clear_key = format_smolstr!("{base_key}.clear");
        let alert_key = format_smolstr!("{base_key}.clear-alert");
        let stored = host.config(app, |config| kind.has_stored_set(config));

        let editor = CredentialsEditor::new(app, host, window, kind, base_key.as_str());
        let alert = Self::build_clear_alert(app, host, window, kind, alert_key);
        let weak = Rc::downgrade(&editor);
        let edit = Button::new(app, theme.clone(), edit_key)
            .secondary()
            .label(edit_label)
            .a11y_label(edit_aria)
            .on_click(move |ctx| {
                if let Some(editor) = weak.upgrade() {
                    editor.open(ctx.app_mut());
                }
            });

        let state: Rc<RefCell<RowState>> = Rc::new(RefCell::new(RowState::default()));
        let state_for_row = Rc::clone(&state);
        let edit_for_row = edit.clone();
        let row_theme = theme.clone();
        let row_clear_key = clear_key.clone();
        let row_clear_label = clear_label.clone();
        let row_clear_aria = clear_aria.clone();
        let alert_for_row = alert.clone();
        let row_fn = layout::row(
            theme,
            kind.row_id(),
            label,
            description,
            move |app, window, parent| {
                let controls = layout::container(
                    app,
                    window,
                    parent,
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(8.0)
                        .width_full()
                        .build(),
                )?;
                let edit_wrapper =
                    Self::button_wrapper(app, window, controls, kind.edit_selector())?;
                edit_for_row.build(app, window, edit_wrapper)?;
                if stored {
                    let clear_wrapper =
                        Self::button_wrapper(app, window, controls, kind.clear_selector())?;
                    let clear = Button::new(app, row_theme, row_clear_key)
                        .secondary()
                        .label(row_clear_label)
                        .a11y_label(row_clear_aria)
                        .on_click(move |ctx| {
                            let window = ctx.window();
                            let _ = alert_for_row.open(ctx.app_mut(), window);
                        });
                    clear.build(app, window, clear_wrapper)?;
                    state_for_row.borrow_mut().clear = Some((clear_wrapper, clear));
                }
                state_for_row.borrow_mut().controls = Some(controls);

                Ok(())
            },
        );

        let row = Self {
            kind,
            host: host.clone(),
            window,
            theme: theme.clone(),
            state,
            edit,
            editor,
            alert,
            clear_key,
            clear_label,
            clear_aria,
        };

        Ok((row, row_fn))
    }

    /// Builds the alert that puts clearing one set to the user.
    fn build_clear_alert(
        app: &mut App,
        host: &SettingsHost,
        window: WindowId,
        kind: CredentialsKind,
        key: SmolStr,
    ) -> Dialog {
        let translate = host.translate();
        let title = kind.clear_title(translate);
        let message = kind.clear_message(translate);
        let confirm = i18n::t_smol!(translate, "common.confirm");
        let cancel = i18n::t_smol!(translate, "common.cancel");
        let write_host = host.clone();

        DialogBuilder::alert(title.clone(), move |dialog, _theme| {
            let ok_dialog = dialog.clone();
            let cancel_dialog = dialog.clone();
            let write_host = write_host.clone();
            AlertBuilder::new()
                .ok_cancel(title.clone(), message.clone())
                .ok_text(confirm.clone())
                .cancel_text(cancel.clone())
                .ok_action(move |_value, app| {
                    let _ = write_host.write(
                        app,
                        window,
                        SettingsChange::edit(move |config| kind.clear(config)),
                    );
                    ok_dialog.close(app);
                })
                .cancel_action(move |app| {
                    cancel_dialog.close(app);
                })
        })
        .build(
            app,
            host.theme_handle(),
            host.layer(),
            host.animation(),
            key,
        )
    }

    /// Builds one wrapper node what a row button is drawn in, which is the id a test clicks it by.
    fn button_wrapper(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        id: SmolStr,
    ) -> Result<NodeId, EngineError> {
        layout::build_node(app, window, parent, move |tree, parent| {
            NodeBuilder::boxed()
                .id(id)
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .build(),
                )
                .build(tree, parent)
        })
    }

    /// Makes the row show what the settings hold for this frame, including whether a set is stored
    /// at all (which is what the clear button is drawn for).
    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        let stored = self
            .host
            .config(app, |config| self.kind.has_stored_set(config));
        if stored != self.state.borrow().clear.is_some() {
            self.rebuild_clear(app, stored)?;
        }

        self.edit.sync(app)?;
        if let Some((_, clear)) = self.state.borrow().clear.as_ref() {
            clear.sync(app)?;
        }
        self.editor.sync(app)?;
        let _ = self.alert.sync(app);

        Ok(())
    }

    /// Draws or takes away the clear button, which is drawn only while a set is stored.
    fn rebuild_clear(&mut self, app: &mut App, stored: bool) -> Result<(), EngineError> {
        if let Some((node, button)) = self.state.borrow_mut().clear.take() {
            button.release(app);
            if let Err(error) = app.remove_node(self.window, node) {
                log::warn!("removing the clear-credentials button failed: {error}");
            }
        }
        if !stored {
            return Ok(());
        }

        let Some(controls) = self.state.borrow().controls else {
            return Ok(());
        };
        let wrapper = Self::button_wrapper(app, self.window, controls, self.kind.clear_selector())?;
        let alert = self.alert.clone();
        let clear = Button::new(app, self.theme.clone(), self.clear_key.clone())
            .secondary()
            .label(self.clear_label.clone())
            .a11y_label(self.clear_aria.clone())
            .on_click(move |ctx| {
                let window = ctx.window();
                let _ = alert.open(ctx.app_mut(), window);
            });
        clear.build(app, self.window, wrapper)?;
        self.state.borrow_mut().clear = Some((wrapper, clear));

        Ok(())
    }

    /// Offers one platform event to the row's dialog.
    fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.editor.dispatch(app, event)
    }

    /// Releases every component the row owns.
    fn release(self, app: &mut App) {
        self.edit.release(app);
        if let Some((node, clear)) = self.state.borrow_mut().clear.take() {
            clear.release(app);
            if let Err(error) = app.remove_node(self.window, node) {
                log::warn!("removing the clear-credentials button failed: {error}");
            }
        }
        self.editor.release(app);
        self.alert.release(app);
    }
}

/// The dialog that edits one credential set.
///
/// The dialog is built once, when the page is, and opened from the row it belongs to. Its content
/// is built per open, over the draft the settings of that moment hold; nothing is written until
/// the content's own confirm button writes the set — the submission validates first, and a set
/// that cannot be stored leaves the dialog open with what the user typed.
struct CredentialsEditor {
    inner: Rc<RefCell<EditorInner>>,
}

/// The one state the dialog's content and the row that opens it share.
struct EditorInner {
    kind: CredentialsKind,
    host: SettingsHost,
    window: WindowId,
    /// The dialog, once it is built; the content closure reaches the editor through a weak handle
    /// because the dialog owns that closure.
    dialog: Option<Dialog>,
    /// The key every component of the dialog is named by.
    key: SmolStr,
    /// The draft the content of the next open is built from.
    draft: CredentialsDraft,
    /// The content of the open, when there is one.
    form: Option<CredentialsForm>,
    theme: ThemeHandle,
    translate: Translate,
}

/// What one open of the dialog holds.
struct CredentialsForm {
    /// The scope every signal of the form belongs to, released with the form.
    scope: SignalScope,
    fields: Vec<FormField>,
    buttons: Vec<Button>,
}

/// One value of the form: its declaration, the input that edits it, and what was already reported
/// about it (which the value changing is what takes away).
struct FormField {
    field: &'static CredentialField,
    input: InputNode,
    error: Signal<Option<SmolStr>>,
    /// The text the input held when its error was last observed, so only a value that changed
    /// takes the report away.
    previous: RefCell<SmolStr>,
}

impl CredentialsEditor {
    /// Builds the (closed) dialog and returns the editor that opens it.
    fn new(
        app: &mut App,
        host: &SettingsHost,
        window: WindowId,
        kind: CredentialsKind,
        base_key: &str,
    ) -> Rc<Self> {
        let key = format_smolstr!("{base_key}.dialog");
        let title = i18n::t_smol!(host.translate(), kind.title_key());
        let editor = Rc::new(Self {
            inner: Rc::new(RefCell::new(EditorInner {
                kind,
                host: host.clone(),
                window,
                dialog: None,
                key: key.clone(),
                draft: CredentialsDraft::empty(kind),
                form: None,
                theme: host.theme_handle(),
                translate: host.translate().clone(),
            })),
        });
        let weak = Rc::downgrade(&editor);
        let dialog = DialogBuilder::new(title)
            .close_button(false)
            .width(WIDTH)
            .content(move |app, window, body| {
                if let Some(editor) = weak.upgrade() {
                    editor.build_form(app, window, body)?;
                }

                Ok(())
            })
            .build(
                app,
                host.theme_handle(),
                host.layer(),
                host.animation(),
                key,
            );
        editor.inner.borrow_mut().dialog = Some(dialog);

        editor
    }

    /// Opens the dialog over the set the settings hold now.
    fn open(&self, app: &mut App) {
        let (dialog, window, draft) = {
            let inner = self.inner.borrow();
            let draft = inner.kind.draft(&inner.host.config(app, Config::clone));
            (inner.dialog.clone(), inner.window, draft)
        };
        let Some(dialog) = dialog else {
            return;
        };
        if dialog.is_open(app) {
            return;
        }

        self.release_form(app);
        self.inner.borrow_mut().draft = draft;
        let _ = dialog.open(app, window);
    }

    /// Closes the dialog the way its cancel button does.
    fn close(&self, app: &mut App) {
        let dialog = self.inner.borrow().dialog.clone();
        if let Some(dialog) = dialog {
            dialog.close(app);
        }
    }

    /// Builds the content of one open: the values on top, the foot below.
    ///
    /// The content is built over `inner.draft`, which is the set the settings held when the dialog
    /// was opened, and every value is a field of its own with the line its rejection is drawn in.
    fn build_form(
        self: &Rc<Self>,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let (kind, key, theme, translate, draft) = {
            let inner = self.inner.borrow();
            (
                inner.kind,
                inner.key.clone(),
                inner.theme.clone(),
                inner.translate.clone(),
                inner.draft.clone(),
            )
        };
        let scope = app.signal_scope();
        let fields = kind.fields();
        let mut form_fields = Vec::with_capacity(fields.len());
        let mut buttons = Vec::with_capacity(2);

        let root = layout::build_node(app, window, parent, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static(DIALOG_SELECTOR))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .width_full()
                        .min_width_0()
                        .gap(12.0)
                        .build(),
                )
                .build(tree, parent)
        })?;

        for field in fields {
            let column = layout::build_node(app, window, root, move |tree, parent| {
                NodeBuilder::boxed()
                    .id(kind.field_selector(field))
                    .style(
                        StyleBuilder::new()
                            .flex_col()
                            .width_full()
                            .min_width_0()
                            .gap(4.0)
                            .build(),
                    )
                    .build(tree, parent)
            })?;
            let label = i18n::t_smol!(translate, field.label_key);
            layout::themed_text(
                app,
                window,
                column,
                None,
                label,
                12.0,
                500,
                Role::FgDefault,
                &theme,
            )?;
            let initial = SmolStr::new(draft.value(field.id));
            let state = app.signal_scoped(scope, InputState::new(initial.as_str()));
            let placeholder_text = field
                .placeholder_key
                .map_or_else(SmolStr::default, |key| i18n::t_smol!(translate, key));
            let placeholder = app.signal_scoped(scope, placeholder_text.clone());
            let spec = Arc::new(InputSpec {
                name: kind.field_id(field),
                placeholder: placeholder_text,
                masked: field.password,
                multiline: false,
                readonly: false,
                disabled: false,
                max_length: None,
                size: ControlSize::Medium,
                submit: None,
            });
            let input = {
                let tree = app
                    .window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut();
                input::build(tree, column, spec, &theme, &state, &placeholder)
            };
            input.install(app);
            let error: Signal<Option<SmolStr>> = app.signal_scoped(scope, None);
            let error_theme = theme.clone();
            layout::build_node(app, window, column, move |tree, parent| {
                NodeBuilder::text(SmolStr::default(), TextStyle::new(12.0))
                    .id(kind.error_selector(field))
                    .bind_text(move |cx| cx.read(&error).clone().unwrap_or_default())
                    .bind_style(move |cx| {
                        let shown = cx.read(&error).is_some();
                        StylePatch {
                            foreground: Some(error_theme.role(cx, Role::StatusDangerBase)),
                            visible: Some(shown),
                            ..StylePatch::default()
                        }
                    })
                    .build(tree, parent)
            })?;
            form_fields.push(FormField {
                field,
                input,
                error,
                previous: RefCell::new(initial),
            });
        }

        let footer = layout::build_node(app, window, root, move |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static(FOOTER_SELECTOR))
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .flex_none()
                        .width_full()
                        .gap(8.0)
                        .build(),
                )
                .build(tree, parent)
        })?;
        layout::themed_box(
            app,
            window,
            footer,
            format_smolstr!("{key}:divider"),
            StyleBuilder::new()
                .flex_none()
                .width_full()
                .height(1.0)
                .build(),
            PaintSpec::default(),
            &theme,
            Some(Role::LineDivider),
            None,
        )?;
        let row = layout::container(
            app,
            window,
            footer,
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .justify_end()
                .width_full()
                .gap(8.0)
                .build(),
        )?;
        let weak = Rc::downgrade(self);
        let cancel_label = i18n::t_smol!(translate, "common.cancel");
        let cancel_wrapper =
            Self::button_wrapper(app, window, row, SmolStr::new_static(CANCEL_SELECTOR))?;
        let cancel = Button::new(app, theme.clone(), format_smolstr!("{key}:cancel"))
            .ghost()
            .label(cancel_label)
            .on_click({
                let weak = Weak::clone(&weak);
                move |ctx| {
                    if let Some(editor) = weak.upgrade() {
                        editor.close(ctx.app_mut());
                    }
                }
            });
        cancel.build(app, window, cancel_wrapper)?;
        buttons.push(cancel);

        let confirm_label = i18n::t_smol!(translate, "common.confirm");
        let confirm_wrapper =
            Self::button_wrapper(app, window, row, SmolStr::new_static(CONFIRM_SELECTOR))?;
        let confirm = Button::new(app, theme.clone(), format_smolstr!("{key}:confirm"))
            .primary()
            .label(confirm_label)
            .on_click(move |ctx| {
                if let Some(editor) = weak.upgrade() {
                    editor.confirm(ctx.app_mut());
                }
            });
        confirm.build(app, window, confirm_wrapper)?;
        buttons.push(confirm);

        self.inner.borrow_mut().form = Some(CredentialsForm {
            scope,
            fields: form_fields,
            buttons,
        });

        Ok(())
    }

    /// Writes the set the form holds, or reports every value it is missing.
    ///
    /// The dialog stays open when anything is missing or the set cannot be stored: what the user
    /// typed is what they submit again with.
    fn confirm(&self, app: &mut App) {
        let (dialog, kind, host, window, translate) = {
            let inner = self.inner.borrow();
            (
                inner.dialog.clone(),
                inner.kind,
                inner.host.clone(),
                inner.window,
                inner.translate.clone(),
            )
        };
        let pending = {
            let inner = self.inner.borrow();
            let Some(form) = inner.form.as_ref() else {
                return;
            };
            form.fields
                .iter()
                .map(|field| (field.field, field.input.state, field.error))
                .collect::<Vec<_>>()
        };

        let mut draft = CredentialsDraft::empty(kind);
        let mut rejected = false;
        for (field, state, error) in pending {
            let value = SmolStr::new(app.read(&state).text());
            match field.error_key {
                Some(key) if value.trim().is_empty() => {
                    app.write(&error, Some(i18n::t_smol!(translate, key)));
                    rejected = true;
                }
                _ => {
                    app.write(&error, None);
                }
            }
            draft.set(field.id, value);
        }
        if rejected {
            return;
        }

        let stored = host.write(
            app,
            window,
            SettingsChange::edit(move |config| kind.save(config, &draft)),
        );
        // The reason the set could not be stored is the settings page's to report, and the dialog
        // stays where the user can submit it again.
        if stored.is_ok()
            && let Some(dialog) = dialog
        {
            dialog.close(app);
        }
    }

    /// Makes the dialog and its form follow the frame.
    fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        let dialog = self.inner.borrow().dialog.clone();
        let Some(dialog) = dialog else {
            return Ok(());
        };
        if !dialog.is_open(app) {
            self.release_form(app);
            return Ok(());
        }

        {
            let inner = self.inner.borrow();
            if let Some(form) = inner.form.as_ref() {
                for field in &form.fields {
                    field.input.sync(app);
                    let text =
                        app.read_with(&field.input.state, |state| SmolStr::new(state.text()));
                    if field.previous.borrow().as_str() != text.as_str() {
                        *field.previous.borrow_mut() = text;
                        if app.read(&field.error).is_some() {
                            app.write(&field.error, None);
                        }
                    }
                }
                for button in &form.buttons {
                    button.sync(app)?;
                }
            }
        }
        let _ = dialog.sync(app);
        if !dialog.is_open(app) {
            self.release_form(app);
        }

        Ok(())
    }

    /// Offers one platform event to the form's inputs.
    fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let inner = self.inner.borrow();
        let Some(form) = inner.form.as_ref() else {
            return false;
        };
        let mut consumed = false;
        for field in &form.fields {
            consumed |= field.input.dispatch(app, event);
        }

        consumed
    }

    /// Releases the form of the open, if there is one.
    fn release_form(&self, app: &mut App) {
        let form = self.inner.borrow_mut().form.take();
        let Some(form) = form else {
            return;
        };
        for field in &form.fields {
            field.input.release(app);
        }
        for button in &form.buttons {
            button.release(app);
        }
        app.release_scope(form.scope);
    }

    /// Builds one wrapper node a dialog button is drawn in, which is the id a test clicks it by.
    fn button_wrapper(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        id: SmolStr,
    ) -> Result<NodeId, EngineError> {
        layout::build_node(app, window, parent, move |tree, parent| {
            NodeBuilder::boxed()
                .id(id)
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .flex_row()
                        .items_center()
                        .build(),
                )
                .build(tree, parent)
        })
    }

    /// Releases the form and the dialog.
    fn release(&self, app: &mut App) {
        self.release_form(app);
        let dialog = self.inner.borrow_mut().dialog.take();
        if let Some(dialog) = dialog {
            dialog.release(app);
        }
    }
}
