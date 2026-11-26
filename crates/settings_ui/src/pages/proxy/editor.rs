use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use app_config::{
    network::{ProxyAuthorizationConfig, ProxyConfig, normalize_proxy_host},
    settings::proxy_name_is_reserved,
};
use i18n::translate::Translate;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    interaction::ActionCtx,
    math::{LogicalPx, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    paint::{
        style::{Style, StylePatch},
        style_builder::StyleBuilder,
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};
use ui_kit::{
    kit::{
        button::Button,
        dialog::{Dialog, DialogBuilder, DialogError},
        input::{self, InputNode, InputSpec, InputState},
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};

use crate::{host::SettingsHost, layout, session::SettingsChange};

const FORM_WIDTH: LogicalPx = px(480.0);

const LABEL_SIZE: f32 = 13.0;

const ERROR_SIZE: f32 = 11.0;

const ERROR_LINE_HEIGHT: f32 = 16.0;

const MIN_PORT: u16 = 1;
const MAX_PORT: u16 = u16::MAX;

pub(crate) const ADD_SELECTOR: &str = "SETTINGS_PROXY_ADD";

pub(crate) const EDIT_SELECTOR: &str = "SETTINGS_PROXY_EDIT";

pub(crate) const CONFIRM_SELECTOR: &str = "SETTINGS_PROXY_CONFIRM";

pub(crate) const CANCEL_SELECTOR: &str = "SETTINGS_PROXY_CANCEL";

pub(crate) const FOOTER_SELECTOR: &str = "SETTINGS_PROXY_FOOTER";

pub(crate) const AUTHORIZATION_SELECTOR: &str = "SETTINGS_PROXY_AUTHORIZATION";

pub(crate) const DNS_MODE_SELECTOR: &str = "SETTINGS_PROXY_DNS_MODE";

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProxyFormKind {
    Add,
    Edit { original: SmolStr },
}

impl ProxyFormKind {
    fn title_key(&self) -> &'static str {
        match self {
            Self::Add => "settings_ui.proxies.add",
            Self::Edit { .. } => "settings_ui.proxies.edit",
        }
    }

    fn selector(&self) -> &'static str {
        match self {
            Self::Add => ADD_SELECTOR,
            Self::Edit { .. } => EDIT_SELECTOR,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProxyDraft {
    name: SmolStr,
    addr: SmolStr,
    port: SmolStr,
    username: SmolStr,
    password: SmolStr,
    authenticated: bool,
    resolve_dns_at_remote: bool,
}

impl ProxyDraft {
    fn empty() -> Self {
        Self {
            name: SmolStr::default(),
            addr: SmolStr::default(),
            port: SmolStr::default(),
            username: SmolStr::default(),
            password: SmolStr::default(),
            authenticated: false,
            resolve_dns_at_remote: false,
        }
    }

    fn from_proxy(name: &SmolStr, proxy: &ProxyConfig) -> Self {
        let (username, password, authenticated) = match &proxy.authorization {
            ProxyAuthorizationConfig::None => (SmolStr::default(), SmolStr::default(), false),
            ProxyAuthorizationConfig::UsernameAndPassword { username, password } => {
                (username.clone(), password.clone(), true)
            }
        };

        Self {
            name: name.clone(),
            addr: proxy.addr.clone(),
            port: format_smolstr!("{}", proxy.port),
            username,
            password,
            authenticated,
            resolve_dns_at_remote: proxy.resolve_dns_at_remote,
        }
    }

    fn text(&self, field: Field) -> &SmolStr {
        match field {
            Field::Name => &self.name,
            Field::Addr => &self.addr,
            Field::Port => &self.port,
            Field::Username => &self.username,
            Field::Password => &self.password,
        }
    }

    fn set_text(&mut self, field: Field, value: SmolStr) {
        match field {
            Field::Name => self.name = value,
            Field::Addr => self.addr = value,
            Field::Port => self.port = value,
            Field::Username => self.username = value,
            Field::Password => self.password = value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Authorization {
    None,
    UsernameAndPassword,
}

impl Authorization {
    const ALL: [Self; 2] = [Self::None, Self::UsernameAndPassword];

    const fn label_key(self) -> &'static str {
        match self {
            Self::None => "settings_ui.proxies.no_authorization",
            Self::UsernameAndPassword => "settings_ui.proxies.username_password",
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UsernameAndPassword => "username-password",
        }
    }

    fn of_id(id: &str) -> Self {
        match id {
            "username-password" => Self::UsernameAndPassword,
            _ => Self::None,
        }
    }

    fn of_draft(authenticated: bool) -> Self {
        if authenticated {
            Self::UsernameAndPassword
        } else {
            Self::None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DnsMode {
    Local,
    Proxy,
}

impl DnsMode {
    pub(crate) const ALL: [Self; 2] = [Self::Local, Self::Proxy];

    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::Local => "settings_ui.proxies.dns_mode_local",
            Self::Proxy => "settings_ui.proxies.dns_mode_proxy",
        }
    }

    pub(crate) fn of(resolve_dns_at_remote: bool) -> Self {
        if resolve_dns_at_remote {
            Self::Proxy
        } else {
            Self::Local
        }
    }

    const fn resolve_dns_at_remote(self) -> bool {
        matches!(self, Self::Proxy)
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Proxy => "proxy",
        }
    }

    fn of_id(id: &str) -> Self {
        match id {
            "proxy" => Self::Proxy,
            _ => Self::Local,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FormErrors {
    name: Option<&'static str>,
    addr: Option<&'static str>,
    port: Option<&'static str>,
    username: Option<&'static str>,
    password: Option<&'static str>,
}

impl FormErrors {
    fn any(&self) -> bool {
        self.name.is_some()
            || self.addr.is_some()
            || self.port.is_some()
            || self.username.is_some()
            || self.password.is_some()
    }

    fn of(&self, field: Field) -> Option<&'static str> {
        match field {
            Field::Name => self.name,
            Field::Addr => self.addr,
            Field::Port => self.port,
            Field::Username => self.username,
            Field::Password => self.password,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Name,
    Addr,
    Port,
    Username,
    Password,
}

impl Field {
    const ALL: [Self; 5] = [
        Self::Name,
        Self::Addr,
        Self::Port,
        Self::Username,
        Self::Password,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Addr => 1,
            Self::Port => 2,
            Self::Username => 3,
            Self::Password => 4,
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Addr => "addr",
            Self::Port => "port",
            Self::Username => "username",
            Self::Password => "password",
        }
    }

    const fn label_key(self) -> &'static str {
        match self {
            Self::Name => "settings_ui.proxies.name",
            Self::Addr => "settings_ui.proxies.socks5_address",
            Self::Port => "settings_ui.proxies.socks5_port",
            Self::Username => "settings_ui.proxies.username",
            Self::Password => "settings_ui.proxies.password",
        }
    }
}

fn validate(
    draft: &ProxyDraft,
    kind: &ProxyFormKind,
    catalog: &BTreeMap<SmolStr, ProxyConfig>,
) -> Result<(SmolStr, ProxyConfig), FormErrors> {
    let mut errors = FormErrors::default();
    let name = draft.name.trim();
    let original = match kind {
        ProxyFormKind::Add => None,
        ProxyFormKind::Edit { original } => Some(original.as_str()),
    };

    if name.is_empty() {
        errors.name = Some("settings_ui.proxies.name_required");
    } else if proxy_name_is_reserved(name) {
        errors.name = Some("settings_ui.proxies.name_reserved");
    } else if catalog
        .keys()
        .any(|existing| existing.as_str() == name && original != Some(existing.as_str()))
    {
        errors.name = Some("settings_ui.proxies.name_exists");
    }

    let addr = normalize_proxy_host(&draft.addr);
    if addr.is_none() {
        errors.addr = Some("settings_ui.proxies.invalid_address");
    }
    let port = match draft.port.trim().parse::<u16>() {
        Ok(port) if (MIN_PORT..=MAX_PORT).contains(&port) => Some(port),
        _ => {
            errors.port = Some("settings_ui.proxies.invalid_port");
            None
        }
    };

    if draft.authenticated {
        if draft.username.trim().is_empty() {
            errors.username = Some("settings_ui.proxies.username_required");
        }
        if draft.password.trim().is_empty() {
            errors.password = Some("settings_ui.proxies.password_required");
        }
    }

    let (Some(addr), Some(port)) = (addr, port) else {
        return Err(errors);
    };
    if errors.any() {
        return Err(errors);
    }

    let authorization = if draft.authenticated {
        ProxyAuthorizationConfig::UsernameAndPassword {
            username: SmolStr::new(draft.username.trim()),
            password: SmolStr::new(draft.password.trim()),
        }
    } else {
        ProxyAuthorizationConfig::None
    };

    Ok((
        SmolStr::new(name),
        ProxyConfig {
            addr,
            port,
            authorization,
            resolve_dns_at_remote: draft.resolve_dns_at_remote,
        },
    ))
}

struct FormControls {
    inputs: Vec<InputNode>,
    authorization: Option<Select>,
    dns_mode: Option<Select>,
    confirm: Option<Button>,
    cancel: Option<Button>,
    errors: [Signal<SmolStr>; 5],
    authenticated: Signal<bool>,
}

impl FormControls {
    fn new(app: &mut App, scope: SignalScope, authenticated: bool) -> Self {
        Self {
            inputs: Vec::with_capacity(Field::ALL.len()),
            authorization: None,
            dns_mode: None,
            confirm: None,
            cancel: None,
            errors: Field::ALL.map(|_| app.signal_scoped(scope, SmolStr::default())),
            authenticated: app.signal_scoped(scope, authenticated),
        }
    }
}

pub(crate) struct ProxyEditor {
    dialog: Dialog,
    draft: Rc<RefCell<ProxyDraft>>,
    controls: Rc<RefCell<FormControls>>,
    handle: Rc<RefCell<Option<Dialog>>>,
    scope: SignalScope,
}

impl ProxyEditor {
    pub(crate) fn add(
        app: &mut App,
        host: &SettingsHost,
        window: WindowId,
        key: SmolStr,
    ) -> Result<Self, DialogError> {
        Self::open(
            app,
            host,
            window,
            key,
            ProxyFormKind::Add,
            ProxyDraft::empty(),
        )
    }

    pub(crate) fn edit(
        app: &mut App,
        host: &SettingsHost,
        window: WindowId,
        key: SmolStr,
        name: &SmolStr,
    ) -> Result<Option<Self>, DialogError> {
        let stored = host.config(app, |config| config.proxies.get(name).cloned());
        let Some(proxy) = stored else {
            log::warn!("editing proxy {name} which is no longer in the catalog");
            return Ok(None);
        };

        Self::open(
            app,
            host,
            window,
            key,
            ProxyFormKind::Edit {
                original: name.clone(),
            },
            ProxyDraft::from_proxy(name, &proxy),
        )
        .map(Some)
    }

    fn open(
        app: &mut App,
        host: &SettingsHost,
        window: WindowId,
        key: SmolStr,
        kind: ProxyFormKind,
        draft: ProxyDraft,
    ) -> Result<Self, DialogError> {
        let theme = host.theme_handle();
        let translate = host.translate().clone();
        let scope = app.signal_scope();
        let controls = Rc::new(RefCell::new(FormControls::new(
            app,
            scope,
            draft.authenticated,
        )));
        let draft = Rc::new(RefCell::new(draft));
        let handle: Rc<RefCell<Option<Dialog>>> = Rc::new(RefCell::new(None));

        let title = i18n::t_smol!(translate, kind.title_key());
        let close_label = i18n::t_smol!(translate, "common.close");
        let content = {
            let controls = Rc::clone(&controls);
            let draft = Rc::clone(&draft);
            let handle = Rc::clone(&handle);
            let host = host.clone();
            let theme = theme.clone();
            let key = key.clone();
            move |app: &mut App, window: WindowId, body: NodeId| {
                let form = FormBuild {
                    host: &host,
                    theme: &theme,
                    translate: &translate,
                    controls: &controls,
                    draft: &draft,
                    handle: &handle,
                    kind: &kind,
                    key: &key,
                    scope,
                    window,
                };
                form.build(app, body)
            }
        };

        let dialog = DialogBuilder::new(title)
            .content(content)
            .width(FORM_WIDTH.value())
            .fade(false)
            .close_label(close_label)
            .build(
                app,
                theme,
                host.layer(),
                host.animation(),
                format_smolstr!("{key}.editor"),
            );
        dialog.open(app, window)?;
        *handle.borrow_mut() = Some(dialog.clone());

        Ok(Self {
            dialog,
            draft,
            controls,
            handle,
            scope,
        })
    }

    pub(crate) fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if let Err(error) = self.dialog.sync(app) {
            log::warn!("syncing the proxy form failed: {error}");
        }
        if !self.dialog.is_open(app) {
            return Ok(());
        }

        {
            let controls = self.controls.borrow();
            for input in &controls.inputs {
                input.sync(app);
            }
        }
        self.follow_fields(app);
        let controls = self.controls.borrow();
        if let Some(select) = &controls.authorization {
            select.sync(app)?;
        }
        if let Some(select) = &controls.dns_mode {
            select.sync(app)?;
        }
        for button in [controls.confirm.as_ref(), controls.cancel.as_ref()]
            .into_iter()
            .flatten()
        {
            button.sync(app)?;
        }

        Ok(())
    }

    fn follow_fields(&self, app: &mut App) {
        let errors = self.controls.borrow().errors;
        let mut draft = self.draft.borrow_mut();
        for field in Field::ALL {
            let Some(input) = self.controls.borrow().inputs.get(field.index()).cloned() else {
                continue;
            };
            let value = app.read_with(&input.state, |state| SmolStr::new(state.text()));
            if draft.text(field) != &value {
                draft.set_text(field, value);
                app.write(&errors[field.index()], SmolStr::default());
            }
        }
    }

    pub(crate) fn is_open(&self, app: &App) -> bool {
        self.dialog.is_open(app)
    }

    pub(crate) fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        let controls = self.controls.borrow();
        let mut consumed = false;
        if let Some(select) = &controls.authorization {
            consumed |= select.dispatch(app, event);
        }
        if let Some(select) = &controls.dns_mode {
            consumed |= select.dispatch(app, event);
        }
        consumed
    }

    pub(crate) fn release(self, app: &mut App) {
        {
            let mut controls = self.controls.borrow_mut();
            for input in controls.inputs.drain(..) {
                input.release(app);
            }
            if let Some(select) = controls.authorization.take() {
                select.release(app);
            }
            if let Some(select) = controls.dns_mode.take() {
                select.release(app);
            }
            if let Some(button) = controls.confirm.take() {
                button.release(app);
            }
            if let Some(button) = controls.cancel.take() {
                button.release(app);
            }
        }
        self.dialog.release(app);
        self.handle.borrow_mut().take();
        app.release_scope(self.scope);
    }
}

struct FormBuild<'a> {
    host: &'a SettingsHost,
    theme: &'a ThemeHandle,
    translate: &'a Translate,
    controls: &'a Rc<RefCell<FormControls>>,
    draft: &'a Rc<RefCell<ProxyDraft>>,
    handle: &'a Rc<RefCell<Option<Dialog>>>,
    kind: &'a ProxyFormKind,
    key: &'a SmolStr,
    scope: SignalScope,
    window: WindowId,
}

impl FormBuild<'_> {
    fn build(&self, app: &mut App, body: NodeId) -> Result<(), EngineError> {
        let form = layout_box(
            app,
            self.window,
            body,
            self.kind.selector(),
            StyleBuilder::new()
                .flex_col()
                .width_full()
                .min_width_0()
                .gap(px(12.0))
                .build(),
        )?;

        let authenticated = self.controls.borrow().authenticated;
        let mut inputs = Vec::with_capacity(Field::ALL.len());
        inputs.push(self.field(app, form, Field::Name, None)?);
        inputs.push(self.field(app, form, Field::Addr, None)?);
        inputs.push(self.field(app, form, Field::Port, None)?);

        self.authorization_select(app, form, authenticated)?;

        inputs.push(self.field(app, form, Field::Username, Some(authenticated))?);
        inputs.push(self.field(app, form, Field::Password, Some(authenticated))?);
        self.controls.borrow_mut().inputs = inputs;

        let dns_mode = DnsMode::of(self.draft.borrow().resolve_dns_at_remote);
        self.dns_mode_select(app, form, dns_mode)?;

        self.footer(app, form)?;

        Ok(())
    }

    fn field(
        &self,
        app: &mut App,
        parent: NodeId,
        field: Field,
        visible: Option<Signal<bool>>,
    ) -> Result<InputNode, EngineError> {
        let value = self.draft.borrow().text(field).clone();
        let error = self.controls.borrow().errors[field.index()];
        let label = i18n::t_smol!(self.translate, field.label_key());
        let wrapper = layout_builder(
            app,
            self.window,
            parent,
            format_smolstr!("SETTINGS_PROXY_FIELD_{}", field.id()),
            StyleBuilder::new()
                .flex_col()
                .width_full()
                .min_width_0()
                .gap(px(4.0))
                .build(),
            visible,
        )?;
        layout::themed_text(
            app,
            self.window,
            wrapper,
            None,
            label,
            LABEL_SIZE,
            500,
            Role::FgDefault,
            self.theme,
        )?;

        let state = app.signal_scoped(self.scope, InputState::new(value.as_str()));
        let placeholder = app.signal_scoped(self.scope, SmolStr::default());
        let spec = std::sync::Arc::new(InputSpec {
            name: format_smolstr!("{}.field.{}", self.key, field.id()),
            label: None,
            placeholder: SmolStr::default(),
            masked: field == Field::Password,
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
                .window_mut(self.window)
                .ok_or(EngineError::UnknownWindow {
                    window: self.window,
                })?
                .tree_mut();
            input::build(tree, wrapper, spec, self.theme, &state, &placeholder)
        };
        input.install(app)?;

        let theme = self.theme.clone();
        layout::build_node(app, self.window, wrapper, move |tree, parent| {
            NodeBuilder::text(
                SmolStr::default(),
                TextStyle::new(px(ERROR_SIZE)).with_wrap(TextWrap::Wrap),
            )
            .style(StyleBuilder::new().height(px(ERROR_LINE_HEIGHT)).build())
            .bind_text(move |cx| cx.read(&error).clone())
            .bind_style(move |cx| {
                let visible = { !cx.read(&error).is_empty() };
                StylePatch {
                    visible: Some(visible),
                    foreground: Some(theme.role(cx, Role::StatusDangerBase)),
                    ..StylePatch::default()
                }
            })
            .build(tree, parent)
        })?;

        Ok(input)
    }

    fn authorization_select(
        &self,
        app: &mut App,
        parent: NodeId,
        authenticated: Signal<bool>,
    ) -> Result<Select, EngineError> {
        let current = Authorization::of_draft(self.draft.borrow().authenticated);
        let label = i18n::t_smol!(self.translate, "settings_ui.proxies.socks5_authorization");
        let items = Authorization::ALL
            .into_iter()
            .map(|choice| {
                SelectItem::new(
                    choice.id(),
                    i18n::t_smol!(self.translate, choice.label_key()),
                )
            })
            .collect::<Vec<_>>();
        let select = Select::new(
            app,
            self.theme.clone(),
            self.host.layer(),
            self.host.animation(),
            format_smolstr!("{}.authorization", self.key),
            SelectOptions::default()
                .width(FORM_WIDTH)
                .label(label.clone()),
        );
        select.set_items(app, items)?;
        select.set_chosen(app, [current.id()], false);
        let draft = Rc::clone(self.draft);
        let window = self.window;
        let select = select.on_change(move |app: &mut App, chosen: Vec<SmolStr>| {
            let Some(id) = chosen.first() else {
                return;
            };
            let authorization = Authorization::of_id(id);
            draft.borrow_mut().authenticated = authorization == Authorization::UsernameAndPassword;
            app.write(
                &authenticated,
                authorization == Authorization::UsernameAndPassword,
            );
            let _ = app.request_repaint(window);
        });
        self.choice_row(app, parent, AUTHORIZATION_SELECTOR, label, &select)?;
        self.controls.borrow_mut().authorization = Some(select.clone());
        Ok(select)
    }

    fn dns_mode_select(
        &self,
        app: &mut App,
        parent: NodeId,
        current: DnsMode,
    ) -> Result<Select, EngineError> {
        let label = i18n::t_smol!(self.translate, "settings_ui.proxies.dns_mode");
        let items = DnsMode::ALL
            .into_iter()
            .map(|mode| SelectItem::new(mode.id(), i18n::t_smol!(self.translate, mode.label_key())))
            .collect::<Vec<_>>();
        let select = Select::new(
            app,
            self.theme.clone(),
            self.host.layer(),
            self.host.animation(),
            format_smolstr!("{}.dns-mode", self.key),
            SelectOptions::default()
                .width(FORM_WIDTH)
                .label(label.clone()),
        );
        select.set_items(app, items)?;
        select.set_chosen(app, [current.id()], false);
        let draft = Rc::clone(self.draft);
        let window = self.window;
        let select = select.on_change(move |app: &mut App, chosen: Vec<SmolStr>| {
            let Some(id) = chosen.first() else {
                return;
            };
            draft.borrow_mut().resolve_dns_at_remote = DnsMode::of_id(id).resolve_dns_at_remote();
            let _ = app.request_repaint(window);
        });
        self.choice_row(app, parent, DNS_MODE_SELECTOR, label, &select)?;
        self.controls.borrow_mut().dns_mode = Some(select.clone());
        Ok(select)
    }

    fn choice_row(
        &self,
        app: &mut App,
        parent: NodeId,
        selector: &'static str,
        label: SmolStr,
        select: &Select,
    ) -> Result<(), EngineError> {
        let row = layout_box(
            app,
            self.window,
            parent,
            selector,
            StyleBuilder::new()
                .flex_col()
                .width_full()
                .min_width_0()
                .gap(px(4.0))
                .build(),
        )?;
        layout::themed_text(
            app,
            self.window,
            row,
            None,
            label,
            LABEL_SIZE,
            500,
            Role::FgDefault,
            self.theme,
        )?;
        select.mount(app, self.window, row)?;
        Ok(())
    }

    fn footer(&self, app: &mut App, parent: NodeId) -> Result<(), EngineError> {
        let footer = layout_box(
            app,
            self.window,
            parent,
            FOOTER_SELECTOR,
            StyleBuilder::new()
                .flex_col()
                .width_full()
                .flex_none()
                .gap(px(8.0))
                .build(),
        )?;
        let divider_theme = self.theme.clone();
        layout::build_node(app, self.window, footer, move |tree, parent| {
            Separator::horizontal(divider_theme).build(tree, parent)?;
            Ok(parent)
        })?;
        let row = layout_box(
            app,
            self.window,
            footer,
            "SETTINGS_PROXY_FOOTER_BUTTONS",
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .justify_end()
                .width_full()
                .flex_none()
                .gap(px(8.0))
                .build(),
        )?;

        let cancel_label = i18n::t_smol!(self.translate, "common.cancel");
        let cancel = layout_box(
            app,
            self.window,
            row,
            CANCEL_SELECTOR,
            StyleBuilder::new().flex_none().build(),
        )?;
        let handle = Rc::clone(self.handle);
        let cancel_button = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("{}.cancel", self.key),
        )
        .secondary()
        .label(cancel_label)
        .on_click(move |ctx: &mut ActionCtx<'_>| {
            if let Some(dialog) = handle.borrow().clone() {
                dialog.close(ctx.app_mut());
            }
        });
        cancel_button.build(app, self.window, cancel)?;

        let confirm_label = i18n::t_smol!(self.translate, "common.confirm");
        let confirm_holder = layout_box(
            app,
            self.window,
            row,
            CONFIRM_SELECTOR,
            StyleBuilder::new().flex_none().build(),
        )?;
        let host = self.host.clone();
        let window = self.window;
        let kind = self.kind.clone();
        let draft = Rc::clone(self.draft);
        let controls = Rc::clone(self.controls);
        let handle = Rc::clone(self.handle);
        let confirm_button = Button::new(
            app,
            self.theme.clone(),
            format_smolstr!("{}.confirm", self.key),
        )
        .primary()
        .label(confirm_label)
        .on_click(move |ctx: &mut ActionCtx<'_>| {
            confirm(
                ctx.app_mut(),
                &host,
                window,
                &kind,
                &draft,
                &controls,
                &handle,
            );
        });
        confirm_button.build(app, self.window, confirm_holder)?;

        let mut controls = self.controls.borrow_mut();
        controls.cancel = Some(cancel_button);
        controls.confirm = Some(confirm_button);
        Ok(())
    }
}

fn confirm(
    app: &mut App,
    host: &SettingsHost,
    window: WindowId,
    kind: &ProxyFormKind,
    draft: &Rc<RefCell<ProxyDraft>>,
    controls: &Rc<RefCell<FormControls>>,
    handle: &Rc<RefCell<Option<Dialog>>>,
) {
    let errors = controls.borrow().errors;
    {
        let mut draft = draft.borrow_mut();
        for field in Field::ALL {
            let Some(input) = controls.borrow().inputs.get(field.index()).cloned() else {
                continue;
            };
            let value = input.value(app);
            draft.set_text(field, value);
        }
    }
    let draft = draft.borrow().clone();
    let catalog = host.config(app, |config| config.proxies.clone());

    let (name, proxy) = match validate(&draft, kind, &catalog) {
        Ok(validated) => validated,
        Err(rejected) => {
            let translate = host.translate().clone();
            for field in Field::ALL {
                let message = rejected
                    .of(field)
                    .map(|key| i18n::t_smol!(translate, key))
                    .unwrap_or_default();
                app.write(&errors[field.index()], message);
            }
            let _ = app.request_repaint(window);
            return;
        }
    };

    let change = match kind {
        ProxyFormKind::Add => SettingsChange::AddProxy { name, proxy },
        ProxyFormKind::Edit { original } => SettingsChange::ReplaceProxy {
            original: original.clone(),
            name,
            proxy,
        },
    };
    if host.write(app, window, change).is_ok()
        && let Some(dialog) = handle.borrow().clone()
    {
        dialog.close(app);
    }
    let _ = app.request_repaint(window);
}

fn layout_box(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: impl Into<SmolStr>,
    style: Style,
) -> Result<NodeId, EngineError> {
    let id = id.into();
    layout::build_node(app, window, parent, move |tree, parent| {
        NodeBuilder::boxed().id(id).style(style).build(tree, parent)
    })
}

fn layout_builder(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    id: SmolStr,
    style: Style,
    visible: Option<Signal<bool>>,
) -> Result<NodeId, EngineError> {
    layout::build_node(app, window, parent, move |tree, parent| {
        let builder = NodeBuilder::boxed().id(id).style(style);
        let builder = match visible {
            Some(visible) => builder.bind_style(move |cx| StylePatch {
                visible: Some(*cx.read(&visible)),
                ..StylePatch::default()
            }),
            None => builder,
        };
        builder.build(tree, parent)
    })
}

#[cfg(test)]
mod tests {
    use app_config::{
        network::{ProxyAuthorizationConfig, ProxyConfig},
        settings::Config,
    };
    use smol_str::{SmolStr, format_smolstr};

    use super::{Authorization, DnsMode, Field, ProxyDraft, ProxyFormKind, validate};

    fn proxy() -> ProxyConfig {
        ProxyConfig {
            addr: SmolStr::new_static("127.0.0.1"),
            port: 1080,
            authorization: ProxyAuthorizationConfig::UsernameAndPassword {
                username: SmolStr::new_static("user"),
                password: SmolStr::new_static("secret"),
            },
            resolve_dns_at_remote: false,
        }
    }

    #[test]
    fn a_draft_should_hold_the_entry_the_form_edits() {
        let draft = ProxyDraft::from_proxy(&SmolStr::new_static("hk"), &proxy());

        assert_eq!(draft.name, "hk");
        assert_eq!(draft.addr, "127.0.0.1");
        assert_eq!(draft.port, "1080");
        assert_eq!(draft.username, "user");
        assert_eq!(draft.password, "secret");
        assert!(draft.authenticated);
        assert!(!draft.resolve_dns_at_remote);
    }

    #[test]
    fn every_dns_mode_should_be_the_flag_it_is_stored_as() {
        for mode in DnsMode::ALL {
            assert_eq!(
                DnsMode::of_id(mode.id()),
                mode,
                "the identity of {mode:?} should name it back"
            );
            assert_eq!(
                DnsMode::of(mode.resolve_dns_at_remote()),
                mode,
                "{mode:?} should survive the flag it is stored as"
            );
        }

        assert!(
            !DnsMode::Local.resolve_dns_at_remote(),
            "an entry resolved here is one the proxy is handed addresses for"
        );
        assert!(
            DnsMode::Proxy.resolve_dns_at_remote(),
            "an entry resolved there is one the proxy looks up itself"
        );
        assert_ne!(
            DnsMode::Local.id(),
            DnsMode::Proxy.id(),
            "the two modes should be told apart by the control that offers them"
        );
    }

    #[test]
    fn every_authorization_should_be_the_choice_it_is_reported_as() {
        for choice in Authorization::ALL {
            assert_eq!(Authorization::of_id(choice.id()), choice);
        }
        assert_eq!(
            Authorization::of_draft(true),
            Authorization::UsernameAndPassword
        );
        assert_eq!(Authorization::of_draft(false), Authorization::None);
    }

    #[test]
    fn a_form_with_nothing_in_it_should_report_every_required_field() {
        let config = Config::default();

        let errors = validate(&ProxyDraft::empty(), &ProxyFormKind::Add, &config.proxies)
            .expect_err("an empty form is not an entry");

        assert_eq!(errors.name, Some("settings_ui.proxies.name_required"));
        assert_eq!(errors.addr, Some("settings_ui.proxies.invalid_address"));
        assert_eq!(errors.port, Some("settings_ui.proxies.invalid_port"));
    }

    #[test]
    fn a_name_should_be_new_and_not_a_routing_token() {
        let mut config = Config::default();
        config.proxies.insert(SmolStr::new_static("hk"), proxy());

        let mut draft = ProxyDraft::empty();
        draft.addr = SmolStr::new_static("127.0.0.1");
        draft.port = SmolStr::new_static("1080");

        draft.name = SmolStr::new_static("@direct");
        assert_eq!(
            validate(&draft, &ProxyFormKind::Add, &config.proxies)
                .expect_err("a reserved name is rejected")
                .name,
            Some("settings_ui.proxies.name_reserved")
        );

        draft.name = SmolStr::new_static("hk");
        assert_eq!(
            validate(&draft, &ProxyFormKind::Add, &config.proxies)
                .expect_err("a taken name is rejected")
                .name,
            Some("settings_ui.proxies.name_exists")
        );

        let editing = ProxyFormKind::Edit {
            original: SmolStr::new_static("hk"),
        };
        assert!(
            validate(&draft, &editing, &config.proxies).is_ok(),
            "an edit keeps the name it is stored under"
        );
    }

    #[test]
    fn credentials_should_be_required_only_while_the_form_asks_for_them() {
        let config = Config::default();
        let mut draft = ProxyDraft::empty();
        draft.name = SmolStr::new_static("osaka");
        draft.addr = SmolStr::new_static("127.0.0.1");
        draft.port = SmolStr::new_static("1080");

        assert!(
            validate(&draft, &ProxyFormKind::Add, &config.proxies).is_ok(),
            "an entry without authentication needs no credentials"
        );

        draft.authenticated = true;
        let errors = validate(&draft, &ProxyFormKind::Add, &config.proxies)
            .expect_err("an authenticated entry needs credentials");
        assert_eq!(
            errors.username,
            Some("settings_ui.proxies.username_required")
        );
        assert_eq!(
            errors.password,
            Some("settings_ui.proxies.password_required")
        );
    }

    #[test]
    fn a_confirmed_entry_should_be_normalized_the_way_the_catalog_stores_it() {
        let config = Config::default();
        let mut draft = ProxyDraft::empty();
        draft.name = SmolStr::new_static("  osaka  ");
        draft.addr = SmolStr::new_static("  ::1 ");
        draft.port = SmolStr::new_static(" 1080 ");

        let (name, proxy) =
            validate(&draft, &ProxyFormKind::Add, &config.proxies).expect("a filled form");

        assert_eq!(name, "osaka");
        assert_eq!(proxy.addr, "[::1]");
        assert_eq!(proxy.port, 1080);
        assert_eq!(proxy.authorization, ProxyAuthorizationConfig::None);
    }

    #[test]
    fn a_port_should_be_an_integer_from_one_to_the_largest_port() {
        let config = Config::default();
        let mut draft = ProxyDraft::empty();
        draft.name = SmolStr::new_static("osaka");
        draft.addr = SmolStr::new_static("127.0.0.1");

        for rejected in ["0", "65536", "-1", "1080.0", "http"] {
            draft.port = SmolStr::new(rejected);
            assert_eq!(
                validate(&draft, &ProxyFormKind::Add, &config.proxies)
                    .expect_err("a port outside the range is rejected")
                    .port,
                Some("settings_ui.proxies.invalid_port"),
                "{rejected} is not a port"
            );
        }

        for accepted in ["1", "65535"] {
            draft.port = SmolStr::new(accepted);
            assert!(
                validate(&draft, &ProxyFormKind::Add, &config.proxies).is_ok(),
                "{accepted} is a port"
            );
        }
    }

    #[test]
    fn every_field_should_be_read_and_written_through_its_index() {
        let mut draft = ProxyDraft::empty();

        for field in Field::ALL {
            draft.set_text(field, format_smolstr!("{}", field.index()));
        }
        for field in Field::ALL {
            assert_eq!(draft.text(field), &format_smolstr!("{}", field.index()));
        }
    }
}
