//! The form that adds a proxy to the catalog or changes one already in it.
//!
//! The form is a dialog of its own on the same window layer as the settings, drawn over them, and
//! it owns nothing but a draft of one entry: what it holds is not written to the settings when the
//! user confirms the form, so a cancelled edit leaves the catalog exactly as it was. What the
//! confirm button does is record the entry as a change of the settings session, which the
//! settings dialog writes when the user confirms that. What is recorded is validated first,
//! and a rejected form reports every problem at once rather than one field at a time.
//!
//! Dismissing the form takes it off the layer and leaves the settings it was opened over exactly
//! where they were, which is why nothing has to be re-mounted: the settings are never taken off
//! the layer, so the page they were showing, the search they were answered by and every field the
//! user had filled in are still there. The form is drawn over the settings and takes the mouse
//! from them, so a click aimed at the settings reaches the form while it is open.
//!
//! The name is the one field an edit may change, and renaming moves every route bound to the
//! old one: the catalog and its references are one value, so they are changed together.

use std::sync::Arc;

use app_config::{
    network::{ProxyAuthorizationConfig, ProxyConfig, normalize_proxy_host},
    settings::proxy_name_is_reserved,
};
use base_ui::{
    button::Button,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    separator::Separator,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
};
use gpui_kit::{
    App, AppContext as _, Context, DismissEvent, ElementId, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use i18n::t;
use smol_str::SmolStr;

use crate::{
    access::{FieldValue, ProxyDraft},
    draft::{SettingsDraft, SettingsEdit},
};

/// How wide the form is.
const WIDTH: f32 = 480.0;

/// The first index a user can choose the proxy line from, and the last.
const MIN_PORT: u16 = 1;
const MAX_PORT: u16 = u16::MAX;

/// What the confirm button is drawn under, which is what a test clicks it by.
pub(crate) const CONFIRM_SELECTOR: &str = "SETTINGS_PROXY_CONFIRM";

/// What the cancel button is drawn under, which is what a test clicks it by.
pub(crate) const CANCEL_SELECTOR: &str = "SETTINGS_PROXY_CANCEL";

/// What the row the form ends in is drawn under: the line across it and the two buttons under
/// that line, which is where the form says what it does rather than what it holds.
pub(crate) const FOOTER_SELECTOR: &str = "SETTINGS_PROXY_FOOTER";

/// What the control the entry's authorization is chosen in is drawn under, which is what a test
/// presses to open the choices.
pub(crate) const AUTHORIZATION_SELECTOR: &str = "SETTINGS_PROXY_AUTHORIZATION";

/// What the control the entry's DNS mode is chosen in is drawn under.
pub(crate) const DNS_MODE_SELECTOR: &str = "SETTINGS_PROXY_DNS_MODE";

/// How the form was opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProxyFormKind {
    /// Add a new entry.
    Add,
    /// Change the entry currently stored under `original`, which the name may move.
    Edit { original: SmolStr },
}

impl ProxyFormKind {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            Self::Add => t!(cx, "settings_ui.proxies.add"),
            Self::Edit { .. } => t!(cx, "settings_ui.proxies.edit"),
        }
    }

    fn debug_selector(&self) -> &'static str {
        match self {
            Self::Add => "SETTINGS_PROXY_ADD",
            Self::Edit { .. } => "SETTINGS_PROXY_EDIT",
        }
    }
}

/// Whether the form asks for credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Authorization {
    None,
    UsernameAndPassword,
}

impl Authorization {
    const ALL: [Self; 2] = [Self::None, Self::UsernameAndPassword];

    fn label(self, cx: &App) -> SharedString {
        match self {
            Self::None => t!(cx, "settings_ui.proxies.no_authorization"),
            Self::UsernameAndPassword => t!(cx, "settings_ui.proxies.username_password"),
        }
    }
}

/// Which side of a proxy connection resolves the names it is made to.
///
/// An entry stores this as one flag, so the two modes are the two readings of it: a name resolved
/// here is one the proxy is handed the address of, and a name resolved there is one the proxy
/// looks up itself. The form and the catalog both read an entry through this, so the mode a row
/// is listed under is the mode the form that wrote it showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DnsMode {
    /// This machine resolves the names, and the proxy is handed addresses.
    Local,
    /// The proxy resolves the names.
    Proxy,
}

impl DnsMode {
    const ALL: [Self; 2] = [Self::Local, Self::Proxy];

    pub(crate) fn label(self, cx: &App) -> SharedString {
        match self {
            Self::Local => t!(cx, "settings_ui.proxies.dns_mode_local"),
            Self::Proxy => t!(cx, "settings_ui.proxies.dns_mode_proxy"),
        }
    }

    /// The mode one stored entry is in.
    pub(crate) fn of(resolve_dns_at_remote: bool) -> Self {
        if resolve_dns_at_remote {
            Self::Proxy
        } else {
            Self::Local
        }
    }

    /// The flag one mode is stored as.
    fn resolve_dns_at_remote(self) -> bool {
        matches!(self, Self::Proxy)
    }
}

/// Every problem the form found with one submission.
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
}

/// The form that changes the proxy catalog.
pub(crate) struct ProxyEditor {
    kind: ProxyFormKind,
    draft: ProxyDraft,
    /// The settings session the entry is recorded in.
    session: Entity<SettingsDraft>,
    authorization: Authorization,
    errors: FormErrors,
    name: Entity<InputState>,
    addr: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    /// Held because a field reports what it holds when the caret leaves it, and the form has
    /// to hear that to know what was submitted.
    _inputs: Vec<Subscription>,
}

/// The form takes itself off the layer by emitting the dismissal its dialog listens for: the
/// form is what knows it is done, so it is what says so rather than depending on being the
/// dialog on top.
impl EventEmitter<DismissEvent> for ProxyEditor {}

impl ProxyEditor {
    /// Opens the form that adds an entry.
    pub fn show_add(session: Entity<SettingsDraft>, window: &mut Window, cx: &mut App) {
        Self::show(ProxyFormKind::Add, ProxyDraft::empty(), session, window, cx);
    }

    /// Opens the form that changes one entry.
    ///
    /// The entry is read from the session here rather than handed in, so a form opened from a
    /// row is always started from what the catalog holds instead of from what the row happened
    /// to be drawn with.
    pub fn show_edit(
        name: &SmolStr,
        session: Entity<SettingsDraft>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let stored = session.read(cx).config().proxies.get(name).cloned();
        let Some(proxy) = stored else {
            log::warn!("editing proxy {name} which is no longer in the catalog");
            return;
        };

        Self::show(
            ProxyFormKind::Edit {
                original: name.clone(),
            },
            ProxyDraft::from_proxy(name, &proxy),
            session,
            window,
            cx,
        );
    }

    fn show(
        kind: ProxyFormKind,
        draft: ProxyDraft,
        session: Entity<SettingsDraft>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let authorization = if draft.authenticated {
            Authorization::UsernameAndPassword
        } else {
            Authorization::None
        };
        let name = input(Field::Name, &draft.name, window, cx);
        let addr = input(Field::Addr, &draft.addr, window, cx);
        let port = input(Field::Port, &draft.port, window, cx);
        let username = input(Field::Username, &draft.username, window, cx);
        let password = input(Field::Password, &draft.password, window, cx);
        let editor = cx.new(|cx| {
            // Every field reports the value it holds when the caret leaves it, which is the
            // moment the draft takes it; the confirm button reads the fields back as well,
            // because clicking it does not take the caret away from what was typed last.
            let inputs = Field::ALL
                .into_iter()
                .zip([
                    name.clone(),
                    addr.clone(),
                    port.clone(),
                    username.clone(),
                    password.clone(),
                ])
                .map(|(field, entity)| {
                    cx.subscribe(
                        &entity,
                        move |editor: &mut Self, _, event: &InputStateEvent, cx| {
                            if let InputStateEvent::Blur(value) = event {
                                editor.write(field, value.clone(), cx);
                            }
                        },
                    )
                })
                .collect::<Vec<_>>();

            Self {
                kind,
                draft,
                session,
                authorization,
                errors: FormErrors::default(),
                name,
                addr,
                port,
                username,
                password,
                _inputs: inputs,
            }
        });

        window.dialog(cx, editor.clone());
        editor.update(cx, |_, cx| cx.notify());
    }

    /// Takes one field's value into the draft, which is what a blur reports.
    fn write(&mut self, field: Field, value: SharedString, cx: &mut Context<Self>) {
        let value = SmolStr::new(value.as_str());
        match field {
            Field::Name => self.draft.name = value,
            Field::Addr => self.draft.addr = value,
            Field::Port => self.draft.port = value,
            Field::Username => self.draft.username = value,
            Field::Password => self.draft.password = value,
        }
        self.errors = FormErrors::default();
        cx.notify();
    }

    /// Takes what every field currently holds into the draft.
    ///
    /// A field reports itself when the caret leaves it, and clicking the confirm button does
    /// not take the caret away from it: without this, the last thing typed before the button
    /// was pressed would be the one thing the form did not carry.
    fn commit_inputs(&mut self, cx: &mut Context<Self>) {
        // The fields are read while the draft is written, and the draft is written through
        // `self`, so what to write is collected before anything is written.
        let pending = Field::ALL
            .into_iter()
            .map(|field| (field, self.input(field).read(cx).value(cx)))
            .collect::<Vec<_>>();

        for (field, value) in pending {
            self.write(field, value, cx);
        }
    }

    /// The state of one field of the form.
    fn input(&self, field: Field) -> &Entity<InputState> {
        match field {
            Field::Name => &self.name,
            Field::Addr => &self.addr,
            Field::Port => &self.port,
            Field::Username => &self.username,
            Field::Password => &self.password,
        }
    }

    /// What one submission says, or every problem with it.
    ///
    /// A reserved name is rejected because the routing values are stored as names a user
    /// could otherwise type: `@direct` typed into the catalog would be a proxy that can
    /// never be chosen, resolved, or removed.
    fn validate(&self, cx: &App) -> Result<(SmolStr, ProxyConfig), FormErrors> {
        let mut errors = FormErrors::default();
        let name = self.draft.name.trim();
        let original = match &self.kind {
            ProxyFormKind::Add => None,
            ProxyFormKind::Edit { original } => Some(original.as_str()),
        };

        if name.is_empty() {
            errors.name = Some("settings_ui.proxies.name_required");
        } else if proxy_name_is_reserved(name) {
            errors.name = Some("settings_ui.proxies.name_reserved");
        } else if self
            .session
            .read(cx)
            .config()
            .proxies
            .keys()
            .any(|existing| existing.as_str() == name && original != Some(existing.as_str()))
        {
            errors.name = Some("settings_ui.proxies.name_exists");
        }

        let addr = normalize_proxy_host(&self.draft.addr);
        if addr.is_none() {
            errors.addr = Some("settings_ui.proxies.invalid_address");
        }
        let port = match self.draft.port.trim().parse::<u16>() {
            Ok(port) if (MIN_PORT..=MAX_PORT).contains(&port) => Some(port),
            _ => {
                errors.port = Some("settings_ui.proxies.invalid_port");
                None
            }
        };

        if self.authorization == Authorization::UsernameAndPassword {
            if self.draft.username.trim().is_empty() {
                errors.username = Some("settings_ui.proxies.username_required");
            }
            if self.draft.password.trim().is_empty() {
                errors.password = Some("settings_ui.proxies.password_required");
            }
        }

        let (Some(addr), Some(port)) = (addr, port) else {
            return Err(errors);
        };
        if errors.any() {
            return Err(errors);
        }

        let authorization = match self.authorization {
            Authorization::None => ProxyAuthorizationConfig::None,
            Authorization::UsernameAndPassword => ProxyAuthorizationConfig::UsernameAndPassword {
                username: SmolStr::new(self.draft.username.trim()),
                password: SmolStr::new(self.draft.password.trim()),
            },
        };

        Ok((
            SmolStr::new(name),
            ProxyConfig {
                addr,
                port,
                authorization,
                resolve_dns_at_remote: self.draft.resolve_dns_at_remote,
            },
        ))
    }

    /// Records the entry in the settings session, or reports what is wrong with the form.
    ///
    /// What the form holds is recorded as one change: an edit that renames an entry moves it
    /// and every route bound to it together, which is why it goes through the catalog's own
    /// replacement rather than through an insert and a delete. Nothing is written to the
    /// settings here — the session is what the user confirms.
    ///
    /// An entry that has been recorded takes the form off the layer, which uncovers the settings
    /// it was opened over: what is left for the user to confirm is theirs.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        self.commit_inputs(cx);
        let (name, proxy) = match self.validate(cx) {
            Ok(validated) => validated,
            Err(errors) => {
                self.errors = errors;
                cx.notify();
                return;
            }
        };

        let edit = match &self.kind {
            ProxyFormKind::Add => SettingsEdit::AddProxy { name, proxy },
            ProxyFormKind::Edit { original } => SettingsEdit::ReplaceProxy {
                original: original.clone(),
                name,
                proxy,
            },
        };
        self.session
            .update(cx, |session, cx| session.write(edit, cx));
        cx.emit(DismissEvent);
    }

    /// One text field, with its label and whatever is wrong with it.
    ///
    /// The error line is drawn whether or not there is anything wrong with the field, so a
    /// form that is rejected does not move everything below the field it complains about.
    fn field(
        &self,
        field: Field,
        input: &Entity<InputState>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let error = match field {
            Field::Name => self.errors.name,
            Field::Addr => self.errors.addr,
            Field::Port => self.errors.port,
            Field::Username => self.errors.username,
            Field::Password => self.errors.password,
        };
        let label = t!(cx, field.label_key());

        div()
            .debug_selector(move || field.debug_selector())
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
            .child(Input::new(input).medium())
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().roles.status.danger.base.hsla)
                    .children(error.map(|error| t!(cx, error))),
            )
    }
}

/// One editable field of the form.
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

    /// How the field is named, which is what its element and its state are identified by.
    ///
    /// Every field needs an identity of its own rather than one shared by the form: an element
    /// id is what the state of the control drawn under it is kept by across frames, so two
    /// fields sharing one are two controls the framework cannot tell apart.
    const fn id(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Addr => "addr",
            Self::Port => "port",
            Self::Username => "username",
            Self::Password => "password",
        }
    }

    /// What the field's element is drawn under, which is what a test clicks it by.
    fn debug_selector(self) -> String {
        format!("SETTINGS_PROXY_FIELD_{}", self.id())
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

/// A text field's state, built from what the draft holds.
///
/// The state is keyed under the field's own name, which is what keeps the control of one field
/// — the box it is drawn in and the animation of its border — from being the control of another:
/// the whole form shares one element-id prefix, so the name of the field is what tells them
/// apart.
fn input(
    field: Field,
    value: &FieldValue,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputStateBuilder::new(ElementId::NamedChild(
            Arc::new(ElementId::Name(SharedString::new_static(
                "settings-proxy-input",
            ))),
            SharedString::new_static(field.id()),
        ))
        .default_value(value.as_str())
        .build(window, cx)
    })
}

impl Render for ProxyEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = cx.entity().downgrade();
        let dns_editor = editor.clone();
        let authenticated = self.authorization == Authorization::UsernameAndPassword;
        let authorization = Authorization::ALL.into_iter().fold(
            Select::new("settings-proxy-authorization")
                .tonal()
                .placeholder(t!(cx, "settings_ui.proxies.no_authorization")),
            |select, choice| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    choice_id(choice),
                    choice.label(cx),
                )))
            },
        );
        let dns_mode = DnsMode::of(self.draft.resolve_dns_at_remote);
        let dns_mode_select = DnsMode::ALL.into_iter().fold(
            Select::new("settings-proxy-dns-mode").tonal(),
            |select, mode| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    dns_mode_id(mode),
                    mode.label(cx),
                )))
            },
        );

        div()
            .debug_selector({
                let selector = self.kind.debug_selector();
                move || selector.into()
            })
            .w(px(WIDTH))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_base()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .child(self.kind.title(cx)),
            )
            .child(self.field(Field::Name, &self.name, cx))
            .child(self.field(Field::Addr, &self.addr, cx))
            .child(self.field(Field::Port, &self.port, cx))
            .child(choice_row(
                t!(cx, "settings_ui.proxies.socks5_authorization"),
                AUTHORIZATION_SELECTOR,
                authorization
                    .selected_item(Some(choice_id(self.authorization)))
                    .on_change(move |choice, _, cx| {
                        let Some(choice) = choice.as_ref() else {
                            return;
                        };
                        let Some(editor) = editor.upgrade() else {
                            return;
                        };
                        let authorization = authorization_of(choice);
                        editor.update(cx, |editor, cx| {
                            editor.authorization = authorization;
                            editor.errors = FormErrors::default();
                            cx.notify();
                        });
                    }),
                cx,
            ))
            .when(authenticated, |this| {
                this.child(self.field(Field::Username, &self.username, cx))
                    .child(self.field(Field::Password, &self.password, cx))
            })
            .child(choice_row(
                t!(cx, "settings_ui.proxies.dns_mode"),
                DNS_MODE_SELECTOR,
                dns_mode_select
                    .selected_item(Some(dns_mode_id(dns_mode)))
                    .on_change(move |choice, _, cx| {
                        let Some(choice) = choice.as_ref() else {
                            return;
                        };
                        let Some(editor) = dns_editor.upgrade() else {
                            return;
                        };
                        let resolve_dns_at_remote = dns_mode_of(choice).resolve_dns_at_remote();
                        editor.update(cx, |editor, cx| {
                            editor.draft.resolve_dns_at_remote = resolve_dns_at_remote;
                            cx.notify();
                        });
                    }),
                cx,
            ))
            .child(
                // The foot of the form is the foot the application's other dialogs end in: a line
                // across the form, and the buttons under that line, which is what tells what the
                // form holds from what it does.
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
                                Button::new("settings-proxy-cancel")
                                    .ghost()
                                    .with_unified_size(UnifiedSize::Medium)
                                    .debug_selector(|| CANCEL_SELECTOR.into())
                                    .aria_label(t!(cx, "common.cancel"))
                                    .child(t!(cx, "common.cancel"))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("settings-proxy-confirm")
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

/// The identity one authorization choice is selected and reported by.
fn choice_id(choice: Authorization) -> SharedString {
    SharedString::new_static(match choice {
        Authorization::None => "none",
        Authorization::UsernameAndPassword => "username-password",
    })
}

fn authorization_of(id: &str) -> Authorization {
    match id {
        "username-password" => Authorization::UsernameAndPassword,
        _ => Authorization::None,
    }
}

/// The identity one DNS mode is selected and reported by.
fn dns_mode_id(mode: DnsMode) -> SharedString {
    SharedString::new_static(match mode {
        DnsMode::Local => "local",
        DnsMode::Proxy => "proxy",
    })
}

fn dns_mode_of(id: &str) -> DnsMode {
    match id {
        "proxy" => DnsMode::Proxy,
        _ => DnsMode::Local,
    }
}

/// One row of the form that is chosen from a menu: what the choice is called, and the control it
/// is made in.
///
/// The name is drawn above the control rather than inside it, so it is handed to the control as
/// well: a control read out without the row it was drawn in would be one nothing names.
///
/// The control is drawn in a box of its own, which is what a test presses to open the choices:
/// the row around it holds the name above it, so a point in the row is not necessarily a point in
/// the control.
fn choice_row(
    label: SharedString,
    selector: &'static str,
    select: Select,
    cx: &App,
) -> impl IntoElement {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().roles.fg.default.hsla)
                .child(label.clone()),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .debug_selector(move || selector.into())
                .child(select.aria_label(label)),
        )
}

#[cfg(test)]
mod tests {
    use super::{DnsMode, dns_mode_id, dns_mode_of};

    /// A mode is one reading of the flag an entry stores, and the choice a form reports is one of
    /// the choices it offered: a mode and its stored flag that disagreed would record the opposite
    /// of what the form showed, and an identity that did not come back would leave the control
    /// holding nothing.
    #[test]
    fn every_dns_mode_should_be_the_flag_it_is_stored_as() {
        for mode in DnsMode::ALL {
            assert_eq!(
                dns_mode_of(&dns_mode_id(mode)),
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
            dns_mode_id(DnsMode::Local),
            dns_mode_id(DnsMode::Proxy),
            "the two modes should be told apart by the control that offers them"
        );
    }
}
