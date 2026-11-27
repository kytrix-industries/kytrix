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
use i18n::translate::Translate;
use market_participants_model::identifier::VenueId;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    math::px,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    paint::{
        style::{PaintSpec, StylePatch},
        style_builder::StyleBuilder,
    },
    text::TextStyle,
    window::WindowId,
};
use ui_kit::{
    kit::{
        alert::AlertBuilder,
        button::Button,
        dialog::{Dialog, DialogBuilder},
        input::{self, InputNode, InputSpec, InputState},
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};
use widget::venue::VenueDisplay;

use crate::{
    host::SettingsHost,
    layout::{self, RowFn},
    search::{FieldSearch, words},
    session::SettingsChange,
};

const WIDTH: f32 = 480.0;

pub(crate) const DIALOG_SELECTOR: &str = "SETTINGS_CREDENTIALS_DIALOG";

pub(crate) const FOOTER_SELECTOR: &str = "SETTINGS_CREDENTIALS_FOOTER";

pub(crate) const CONFIRM_SELECTOR: &str = "SETTINGS_CREDENTIALS_CONFIRM";

pub(crate) const CANCEL_SELECTOR: &str = "SETTINGS_CREDENTIALS_CANCEL";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialsKind {
    BinanceSpot,
    BinanceUsdMPerp,
    Okx,
    Hyperliquid,
    Longbridge,
}

impl CredentialsKind {
    pub(crate) const fn for_market(market: BinanceMarket) -> Self {
        match market {
            BinanceMarket::Spot => Self::BinanceSpot,
            BinanceMarket::UsdMPerp => Self::BinanceUsdMPerp,
        }
    }

    pub(crate) const fn venue(self) -> VenueId {
        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => VenueId::Binance,
            Self::Okx => VenueId::Okx,
            Self::Hyperliquid => VenueId::Hyperliquid,
            Self::Longbridge => VenueId::Longbridge,
        }
    }

    pub(crate) const fn market(self) -> Option<BinanceMarket> {
        match self {
            Self::BinanceSpot => Some(BinanceMarket::Spot),
            Self::BinanceUsdMPerp => Some(BinanceMarket::UsdMPerp),
            Self::Okx | Self::Hyperliquid | Self::Longbridge => None,
        }
    }

    pub(crate) const fn row_id(self) -> &'static str {
        match self {
            Self::BinanceSpot => "market_participants.binance.spot.credentials",
            Self::BinanceUsdMPerp => "market_participants.binance.usd-m-perp.credentials",
            Self::Okx => "market_participants.okx.credentials",
            Self::Hyperliquid => "market_participants.hyperliquid.credentials",
            Self::Longbridge => "market_participants.longbridge.credentials",
        }
    }

    pub(crate) const fn field_prefix(self) -> &'static str {
        match self {
            Self::BinanceSpot => "market_participants.binance.spot",
            Self::BinanceUsdMPerp => "market_participants.binance.usd-m-perp",
            Self::Okx => "market_participants.okx",
            Self::Hyperliquid => "market_participants.hyperliquid",
            Self::Longbridge => "market_participants.longbridge",
        }
    }

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

    pub(crate) fn clear_title(self, translate: &Translate) -> SmolStr {
        i18n::t_smol!(
            translate,
            "settings_ui.market_participants.clear_credentials_title"
        )
    }

    pub(crate) fn clear_message(self, translate: &Translate) -> SmolStr {
        let title = i18n::t_smol!(translate, self.title_key());

        i18n::t_smol!(
            translate,
            "settings_ui.market_participants.clear_credentials_message",
            title = title.as_str()
        )
    }

    pub(crate) const fn fields(self) -> &'static [CredentialField] {
        match self {
            Self::BinanceSpot | Self::BinanceUsdMPerp => &BINANCE_FIELDS,
            Self::Okx => &OKX_FIELDS,
            Self::Hyperliquid => &HYPERLIQUID_FIELDS,
            Self::Longbridge => &LONGBRIDGE_FIELDS,
        }
    }

    pub(crate) fn field_id(self, field: &CredentialField) -> SmolStr {
        format_smolstr!("{}.{}", self.field_prefix(), field.id)
    }

    pub(crate) fn edit_selector(self) -> SmolStr {
        format_smolstr!("SETTINGS_EDIT_CREDENTIALS_{}", self.row_id())
    }

    pub(crate) fn clear_selector(self) -> SmolStr {
        format_smolstr!("SETTINGS_CLEAR_CREDENTIALS_{}", self.row_id())
    }

    pub(crate) fn field_selector(self, field: &CredentialField) -> SmolStr {
        format_smolstr!("SETTINGS_CREDENTIALS_FIELD_{}", self.field_id(field))
    }

    pub(crate) fn error_selector(self, field: &CredentialField) -> SmolStr {
        format_smolstr!("SETTINGS_CREDENTIALS_ERROR_{}", self.field_id(field))
    }

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

    pub(crate) fn search_field(self, translate: &Translate) -> FieldSearch {
        let label = i18n::t_smol!(translate, "settings_ui.market_participants.credentials");
        let description = i18n::t_smol!(
            translate,
            "settings_ui.market_participants.credentials_description"
        );
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

pub(crate) const fn market_title_key(market: BinanceMarket) -> &'static str {
    match market {
        BinanceMarket::Spot => "settings_ui.market_participants.binance_spot",
        BinanceMarket::UsdMPerp => "settings_ui.market_participants.binance_usd_m_perp",
    }
}

#[derive(Clone)]
pub(crate) struct CredentialsDraft {
    values: AHashMap<&'static str, SmolStr>,
}

impl CredentialsDraft {
    pub(crate) fn empty(kind: CredentialsKind) -> Self {
        let fields = kind.fields();
        let mut values = AHashMap::with_capacity(fields.len());
        for field in fields {
            values.insert(field.id, SmolStr::default());
        }

        Self { values }
    }

    fn value(&self, id: &str) -> &str {
        self.values.get(id).map_or("", SmolStr::as_str)
    }

    fn set(&mut self, id: &'static str, value: SmolStr) {
        self.values.insert(id, value);
    }
}

pub(crate) struct CredentialsArea {
    rows: Vec<CredentialsRow>,
}

impl CredentialsArea {
    pub(crate) fn new() -> Self {
        Self { rows: Vec::new() }
    }

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

    pub(crate) fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        for row in &mut self.rows {
            row.sync(app)?;
        }

        Ok(())
    }

    pub(crate) fn release(&mut self, app: &mut App) {
        for row in self.rows.drain(..) {
            row.release(app);
        }
    }
}

struct CredentialsRow {
    kind: CredentialsKind,
    host: SettingsHost,
    window: WindowId,
    theme: ThemeHandle,
    state: Rc<RefCell<RowState>>,
    edit: Button,
    editor: Rc<CredentialsEditor>,
    alert: Dialog,
    clear_key: SmolStr,
    clear_label: SmolStr,
    clear_aria: SmolStr,
}

#[derive(Default)]
struct RowState {
    controls: Option<NodeId>,
    clear: Option<(NodeId, Button)>,
}

impl CredentialsRow {
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
                        .gap(px(8.0))
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
        .build(app, host.theme_handle(), key)
    }

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

struct CredentialsEditor {
    inner: Rc<RefCell<EditorInner>>,
}

struct EditorInner {
    kind: CredentialsKind,
    host: SettingsHost,
    window: WindowId,
    dialog: Option<Dialog>,
    key: SmolStr,
    draft: CredentialsDraft,
    form: Option<CredentialsForm>,
    theme: ThemeHandle,
    translate: Translate,
}

struct CredentialsForm {
    scope: SignalScope,
    fields: Vec<FormField>,
    buttons: Vec<Button>,
}

struct FormField {
    field: &'static CredentialField,
    input: InputNode,
    error: Signal<Option<SmolStr>>,
    previous: RefCell<SmolStr>,
}

impl CredentialsEditor {
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
            .build(app, host.theme_handle(), key);
        editor.inner.borrow_mut().dialog = Some(dialog);

        editor
    }

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

    fn close(&self, app: &mut App) {
        let dialog = self.inner.borrow().dialog.clone();
        if let Some(dialog) = dialog {
            dialog.close(app);
        }
    }

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
                        .gap(px(12.0))
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
                            .gap(px(4.0))
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
                label: None,
                placeholder: placeholder_text,
                masked: field.password,
                multiline: false,
                readonly: false,
                disabled: false,
                max_length: None,
                visible_lines: None,
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
            input.install(app)?;
            let error: Signal<Option<SmolStr>> = app.signal_scoped(scope, None);
            let error_theme = theme.clone();
            layout::build_node(app, window, column, move |tree, parent| {
                NodeBuilder::text(SmolStr::default(), TextStyle::new(px(12.0)))
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
                        .gap(px(8.0))
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
                .height(px(1.0))
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
                .gap(px(8.0))
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
                .map(|field| (field.field, field.input.clone(), field.error))
                .collect::<Vec<_>>()
        };

        let mut draft = CredentialsDraft::empty(kind);
        let mut rejected = false;
        for (field, input, error) in pending {
            let value = input.value(app);
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
        if stored.is_ok()
            && let Some(dialog) = dialog
        {
            dialog.close(app);
        }
    }

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

    fn release(&self, app: &mut App) {
        self.release_form(app);
        let dialog = self.inner.borrow_mut().dialog.take();
        if let Some(dialog) = dialog {
            dialog.release(app);
        }
    }
}
