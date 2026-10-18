//! The proxy catalog: one row per saved endpoint, with the controls that change it.
//!
//! A catalog with nothing in it has no rows to draw, so what it says then is one line across
//! the body — see [`TableDataSet::empty`] — rather than a row whose every column says it.
//!
//! The table is an entity of its own because that is what [`Table`] reads its rows from. It
//! holds one snapshot at a time and is told to look again when the settings session changes
//! it, which is what makes it follow an edit rather than showing what the catalog was when
//! the dialog opened.
//!
//! Opening a form is not done here. A table cell is drawn without a window, and a dialog
//! needs one, so a row records the form the user asked for and the dialog takes it: the dialog
//! opens the form on the window layer, which pushes it and draws it on the next frame. A request
//! is opened once, in the frame the dialog takes it — see [`ProxyTable::take_request`].

use app_config::network::{ProxyAuthorizationConfig, ProxyConfig};
use base_ui::{
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    table::{Table, TableDataSet},
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Empty, Entity, InteractiveElement, IntoElement, Length,
    ParentElement, Render, RenderOnce, SharedString, StatefulInteractiveElement, Styled,
    Subscription, WeakEntity, Window, base::Align, div, relative,
};
use i18n::t;
use smol_str::SmolStr;

use crate::{draft::SettingsDraft, editor::DnsMode};

/// Columns: name, address, port, authentication, DNS mode, actions.
const COLUMN_COUNT: usize = 6;

/// Column widths in header order.
///
/// Every column is a share of the panel, so the widths are the whole of it: the name and the
/// address are the widest two, because they are what a row is told apart and reached by.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.22)),
    Length::Definite(relative(0.22)),
    Length::Definite(relative(0.10)),
    Length::Definite(relative(0.18)),
    Length::Definite(relative(0.20)),
    Length::Definite(relative(0.08)),
];

/// What one row asks the dialog to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProxyRequest {
    /// Open the form that adds a new entry.
    Add,
    /// Open the form that edits `name`, started from what it currently holds.
    Edit { name: SmolStr },
    /// Remove `name` and everything bound to it, which the dialog has the user confirm before
    /// anything is recorded.
    Delete { name: SmolStr },
}

/// The proxy catalog, drawn as a table.
pub(crate) struct ProxyTable {
    rows: Vec<(SmolStr, ProxyConfig)>,
    /// The form a row asked for, which the dialog takes when it next draws.
    request: Option<ProxyRequest>,
    /// How many times the rows changed, so the table is drawn again rather than reusing
    /// what it drew before.
    revision: u64,
    /// The table's own handle, which its rows are drawn with: a cell is given no window,
    /// so a row cannot reach the entity it belongs to any other way.
    owner: WeakEntity<ProxyTable>,
    /// The session the rows are the catalog of.
    draft: Entity<SettingsDraft>,
    /// Held because the subscription is what keeps the rows of the table the rows of the
    /// session: dropping it would leave a catalog that never notices it changed.
    _draft_changes: Subscription,
}

impl ProxyTable {
    pub fn new(draft: Entity<SettingsDraft>, cx: &mut Context<Self>) -> Self {
        let changes = cx.observe(&draft, |table, _, cx| table.reload(cx));

        let mut table = Self {
            rows: Vec::new(),
            request: None,
            revision: 0,
            owner: cx.entity().downgrade(),
            draft,
            _draft_changes: changes,
        };
        table.reload(cx);

        table
    }

    /// Records the form the user asked for, which the dialog takes when it next draws and opens on
    /// the window layer.
    pub fn request(&mut self, request: ProxyRequest, cx: &mut Context<Self>) {
        self.request = Some(request);
        cx.notify();
    }

    /// Takes the form a row asked for, so it is opened once.
    pub fn take_request(&mut self) -> Option<ProxyRequest> {
        self.request.take()
    }

    /// Re-reads the catalog of the session and tells the table to draw it again when it
    /// changed.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let config = self.draft.read(cx).config();
        let rows = config
            .proxies
            .iter()
            .map(|(name, proxy)| (name.clone(), proxy.clone()))
            .collect::<Vec<_>>();
        drop(config);

        let changed = self.rows != rows;
        self.rows = rows;
        if changed {
            self.revision = self.revision.wrapping_add(1);
            cx.notify();
        }
    }
}

impl TableDataSet<COLUMN_COUNT> for ProxyTable {
    fn rows_count(&self) -> usize {
        // A catalog with nothing in it has no rows: what it has to say then is the one line
        // the body draws in place of them, not a row of columns that each say it.
        self.rows.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; COLUMN_COUNT] {
        [
            t!(cx, "settings_ui.proxies.name"),
            t!(cx, "settings_ui.proxies.address"),
            t!(cx, "settings_ui.proxies.port"),
            t!(cx, "settings_ui.proxies.authorization"),
            t!(cx, "settings_ui.proxies.dns_mode"),
            SharedString::new_static(""),
        ]
    }

    fn column_aligns(&self) -> [Align; COLUMN_COUNT] {
        [
            Align::Start,
            Align::Start,
            Align::End,
            Align::End,
            Align::End,
            Align::Center,
        ]
    }

    fn column_widths(&self) -> [Length; COLUMN_COUNT] {
        COLUMN_WIDTHS
    }

    fn cell(&self, row_index: usize, column_index: usize, _cx: &App) -> impl IntoElement {
        let Some((name, proxy)) = self.rows.get(row_index) else {
            return Empty.into_any_element();
        };

        ProxyCell {
            name: name.clone(),
            proxy: proxy.clone(),
            column: column_index,
            owner: self.owner.clone(),
        }
        .into_any_element()
    }

    /// A catalog that holds nothing says so once, across the body, rather than in every column.
    fn empty(&self, cx: &App) -> Option<AnyElement> {
        Some(
            div()
                .debug_selector(|| "SETTINGS_PROXY_EMPTY".into())
                .text_sm()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(t!(cx, "settings_ui.proxies.empty"))
                .into_any_element(),
        )
    }
}

/// One cell of one row, drawn when the table asks for it.
///
/// The text is resolved here rather than in [`ProxyTable::cell`], which is given no
/// application context to translate with.
#[derive(IntoElement)]
struct ProxyCell {
    name: SmolStr,
    proxy: ProxyConfig,
    column: usize,
    owner: WeakEntity<ProxyTable>,
}

impl RenderOnce for ProxyCell {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            name,
            proxy,
            column,
            owner,
        } = self;

        // The row controls ask the table for a form rather than opening one, because a cell
        // is drawn without a window and a dialog needs one.
        let actions = ProxyActions {
            name: name.clone(),
            owner,
        };

        match column {
            0 => SharedString::from(name.as_str()).into_any_element(),
            1 => SharedString::from(proxy.addr.as_str()).into_any_element(),
            2 => proxy.port.to_string().into_any_element(),
            3 => match &proxy.authorization {
                ProxyAuthorizationConfig::None => {
                    t!(cx, "settings_ui.proxies.no_authorization").into_any_element()
                }
                ProxyAuthorizationConfig::UsernameAndPassword { .. } => {
                    t!(cx, "settings_ui.proxies.username_password").into_any_element()
                }
            },
            4 => DnsMode::of(proxy.resolve_dns_at_remote)
                .label(cx)
                .into_any_element(),
            5 => actions.into_any_element(),
            _ => Empty.into_any_element(),
        }
    }
}

/// The controls of one row: edit and delete, both of which ask the table for a form.
///
/// The row is given the table's own weak handle because a cell is drawn without a window,
/// and an entity is only reachable through one.
#[derive(IntoElement)]
struct ProxyActions {
    name: SmolStr,
    owner: WeakEntity<ProxyTable>,
}

impl RenderOnce for ProxyActions {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let table = self.owner.clone();
        let edit_table = table.clone();
        let delete_table = table;
        let edit_name = self.name.clone();
        let delete_name = self.name.clone();
        let theme = cx.theme();

        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                Button::new(ElementId::NamedChild(
                    std::sync::Arc::new(ElementId::Name(SharedString::new_static(
                        "settings-proxy-edit",
                    ))),
                    SharedString::from(self.name.as_str()),
                ))
                .small()
                .ghost()
                .aria_label(t!(cx, "settings_ui.proxies.edit"))
                .with_tooltip(t!(cx, "settings_ui.proxies.edit"))
                .child(Icon::new(IconKind::NotePencil).size_3())
                .on_click(move |_, _, cx| {
                    let Some(table) = edit_table.upgrade() else {
                        return;
                    };
                    let name = edit_name.clone();
                    table.update(cx, |table, cx| {
                        table.request(ProxyRequest::Edit { name }, cx);
                    });
                }),
            )
            .child(
                Button::new(ElementId::NamedChild(
                    std::sync::Arc::new(ElementId::Name(SharedString::new_static(
                        "settings-proxy-delete",
                    ))),
                    SharedString::from(self.name.as_str()),
                ))
                .small()
                .ghost()
                .aria_label(t!(cx, "settings_ui.proxies.delete"))
                .with_tooltip(t!(cx, "settings_ui.proxies.delete"))
                .child(
                    Icon::new(IconKind::Trash)
                        .size_3()
                        .text_color(theme.roles.status.danger.base.hsla),
                )
                .on_click(move |_, _, cx| {
                    let Some(table) = delete_table.upgrade() else {
                        return;
                    };
                    let name = delete_name.clone();
                    table.update(cx, |table, cx| {
                        table.request(ProxyRequest::Delete { name }, cx);
                    });
                }),
            )
    }
}

impl Render for ProxyTable {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let table = cx.entity();

        div()
            .id("settings-proxy-page")
            .debug_selector(|| "SETTINGS_PROXY_PAGE".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                // The row above the catalog is the way to add to it and nothing else, so what it
                // holds is the right end of it.
                div().flex_none().flex().items_center().justify_end().child(
                    Button::new("settings-proxy-add")
                        .debug_selector(|| "SETTINGS_PROXY_ADD_BUTTON".into())
                        .filled()
                        .medium()
                        .aria_label(t!(cx, "settings_ui.proxies.add"))
                        .with_tooltip(t!(cx, "settings_ui.proxies.add"))
                        .child(Icon::new(IconKind::Plus).size_3())
                        .child(t!(cx, "settings_ui.proxies.add"))
                        .on_click(cx.listener(|table, _, _, cx| {
                            table.request(ProxyRequest::Add, cx);
                        })),
                ),
            )
            .child(
                div()
                    .debug_selector(|| "SETTINGS_PROXY_TABLE".into())
                    .w_full()
                    // The table is what the page leaves below that row, however tall the page is.
                    .flex_1()
                    .min_h_0()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    // §8.4: a table's rows are transparent over `surface.sunken`, and its
                    // header is `surface.raised`. The table lives on a dialog here, so the
                    // content face has to be laid down by the box that holds it.
                    .bg(theme.roles.surface.sunken.hsla)
                    .overflow_hidden()
                    .child(Table::new(
                        ElementId::NamedInteger(
                            SharedString::new_static("settings-proxy-table"),
                            self.revision,
                        ),
                        &table,
                    )),
            )
    }
}
