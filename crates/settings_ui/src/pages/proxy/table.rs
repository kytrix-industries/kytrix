use std::borrow::Cow;

use app_config::{
    network::{ProxyAuthorizationConfig, ProxyConfig},
    settings::Config,
};
use i18n::translate::Translate;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event},
    interaction::ActionCtx,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};
use ui_kit::{
    kit::{
        alert::AlertBuilder,
        button::Button,
        dialog::{Dialog, DialogBuilder},
        icon::IconKind,
        list::SelectionMode,
        markdown::{parse::parse_document, source::MarkdownAssets},
        table::{Column, ColumnAlign, Table, TableOptions},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, role::Role},
};

use crate::{
    host::SettingsHost,
    layout,
    pages::{
        page::{PageBuild, PageView},
        proxy::editor::{DnsMode, ProxyEditor},
    },
    session::SettingsChange,
};

const PAGE: &str = "server-management";

const COLUMN_COUNT: usize = 8;

const COLUMN_WEIGHTS: [f32; COLUMN_COUNT] = [0.14, 0.14, 0.06, 0.12, 0.12, 0.10, 0.10, 0.22];

const COLUMN_ALIGNS: [ColumnAlign; COLUMN_COUNT] = [
    ColumnAlign::Start,
    ColumnAlign::Start,
    ColumnAlign::End,
    ColumnAlign::Start,
    ColumnAlign::Start,
    ColumnAlign::Start,
    ColumnAlign::Start,
    ColumnAlign::Center,
];

const PASSWORD_MASK: &str = "••••••";

const ROW_GAP: f32 = 6.0;

type Rows = Vec<(SmolStr, ProxyConfig)>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProxyIntent {
    Add,
    Edit(SmolStr),
    Delete(SmolStr),
}

pub(crate) struct ProxyTablePage {
    host: SettingsHost,
    window: Option<WindowId>,
    prefix: Option<SmolStr>,
    rows: Option<Signal<Rows>>,
    request: Option<Signal<Option<ProxyIntent>>>,
    scope: Option<SignalScope>,
    table: Option<Table>,
    add_button: Option<Button>,
    row_actions: Vec<Action>,
    editor: Option<ProxyEditor>,
    alert: Option<Dialog>,
    applied: Option<Rows>,
    applied_rows: usize,
}

impl ProxyTablePage {
    pub(crate) fn new(host: SettingsHost, _key: &str) -> Self {
        Self {
            host,
            window: None,
            prefix: None,
            rows: None,
            request: None,
            scope: None,
            table: None,
            add_button: None,
            row_actions: Vec::new(),
            editor: None,
            alert: None,
            applied: None,
            applied_rows: 0,
        }
    }

    fn catalog(&self, app: &App) -> Rows {
        self.host.config(app, |config: &Config| {
            config
                .proxies
                .iter()
                .map(|(name, proxy)| (name.clone(), proxy.clone()))
                .collect()
        })
    }

    fn take_request(&self, app: &mut App) -> Option<ProxyIntent> {
        let signal = self.request?;
        let request = { app.read(&signal).clone() };
        if request.is_some() {
            app.write(&signal, None);
        }
        request
    }

    fn adopt(&mut self, app: &mut App, request: ProxyIntent) {
        if self.editor.is_some() || self.alert.is_some() {
            return;
        }
        let (Some(window), Some(prefix)) = (self.window, self.prefix.clone()) else {
            return;
        };

        match request {
            ProxyIntent::Add => {
                let key = format_smolstr!("{prefix}.add-form");
                match ProxyEditor::add(app, &self.host, window, key) {
                    Ok(editor) => self.editor = Some(editor),
                    Err(error) => log::warn!("opening the add-proxy form failed: {error}"),
                }
            }
            ProxyIntent::Edit(name) => {
                let key = format_smolstr!("{prefix}.edit-form");
                match ProxyEditor::edit(app, &self.host, window, key, &name) {
                    Ok(Some(editor)) => self.editor = Some(editor),
                    Ok(None) => {}
                    Err(error) => log::warn!("opening the edit-proxy form failed: {error}"),
                }
            }
            ProxyIntent::Delete(name) => self.open_delete(app, window, &prefix, name),
        }
    }

    fn open_delete(&mut self, app: &mut App, window: WindowId, prefix: &SmolStr, name: SmolStr) {
        let translate = self.host.translate().clone();
        let title = i18n::t_smol!(translate, "settings_ui.proxies.delete_title");
        let message = delete_message(&translate, &name);
        let confirm = i18n::t_smol!(translate, "common.confirm");
        let cancel = i18n::t_smol!(translate, "common.cancel");
        let close_label = i18n::t_smol!(translate, "common.close");
        let host = self.host.clone();
        let dialog = DialogBuilder::alert(title.clone(), move |dialog, _theme| {
            let dialog_for_ok = dialog.clone();
            let dialog_for_cancel = dialog.clone();
            let host = host.clone();
            let name = name.clone();
            AlertBuilder::new()
                .ok_cancel(title.clone(), message.clone())
                .ok_text(confirm.clone())
                .cancel_text(cancel.clone())
                .ok_action(move |_value: Option<SmolStr>, app: &mut App| {
                    let change = SettingsChange::RemoveProxy { name: name.clone() };
                    let _ = host.write(app, window, change);
                    dialog_for_ok.close(app);
                })
                .cancel_action(move |app: &mut App| {
                    dialog_for_cancel.close(app);
                })
        })
        .fade(false)
        .close_label(close_label)
        .build(
            app,
            self.host.theme_handle(),
            self.host.layer(),
            self.host.animation(),
            format_smolstr!("{prefix}.delete"),
        );
        match dialog.open(app, window) {
            Ok(_) => self.alert = Some(dialog),
            Err(error) => {
                log::warn!("opening the delete-proxy alert failed: {error}");
                dialog.release(app);
            }
        }
    }

    fn register_row_actions(&mut self, app: &mut App) {
        for action in self.row_actions.drain(..) {
            app.off_action(&action);
        }
        let (Some(rows), Some(prefix), Some(request), Some(window)) =
            (self.rows, self.prefix.clone(), self.request, self.window)
        else {
            return;
        };
        let count = app.read_with(&rows, |rows| rows.len());

        for index in 0..count {
            let action = Action::Custom(format_smolstr!("{prefix}.row.{index}.edit"));
            app.on_action(action.clone(), move |ctx: &mut ActionCtx<'_>| {
                open_row(ctx, rows, request, window, index, false);
            });
            self.row_actions.push(action);

            let action = Action::Custom(format_smolstr!("{prefix}.row.{index}.delete"));
            app.on_action(action.clone(), move |ctx: &mut ActionCtx<'_>| {
                open_row(ctx, rows, request, window, index, true);
            });
            self.row_actions.push(action);
        }
    }
}

fn open_row(
    ctx: &mut ActionCtx<'_>,
    rows: Signal<Rows>,
    request: Signal<Option<ProxyIntent>>,
    window: WindowId,
    index: usize,
    delete: bool,
) {
    let name = {
        let rows = ctx.read(&rows);
        rows.get(index).map(|(name, _profile)| name.clone())
    };
    let Some(name) = name else {
        return;
    };
    let intent = if delete {
        ProxyIntent::Delete(name)
    } else {
        ProxyIntent::Edit(name)
    };
    ctx.write(&request, Some(intent));
    let _ = ctx.app_mut().request_repaint(window);
}

impl PageView for ProxyTablePage {
    fn page_id(&self) -> &'static str {
        PAGE
    }

    fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        cx: &PageBuild<'_>,
    ) -> Result<(), EngineError> {
        self.window = Some(window);
        let theme = self.host.theme_handle();
        let translate = self.host.translate().clone();
        let prefix = SmolStr::new(cx.unique_key("proxy"));
        self.prefix = Some(prefix.clone());
        let scope = app.signal_scope();
        self.scope = Some(scope);
        let rows = app.signal_scoped(scope, Rows::default());
        self.rows = Some(rows);
        let request = app.signal_scoped(scope, Option::<ProxyIntent>::None);
        self.request = Some(request);

        let catalog = self.catalog(app);
        app.write(&rows, catalog.clone());
        self.applied = Some(catalog.clone());
        self.applied_rows = catalog.len();

        let page = layout::build_node(app, window, parent, |tree, parent| {
            NodeBuilder::boxed()
                .id("SETTINGS_PROXY_PAGE")
                .style(
                    StyleBuilder::new()
                        .flex_col()
                        .size_full()
                        .min_width_0()
                        .min_height_0()
                        .gap(8.0)
                        .build(),
                )
                .build(tree, parent)
        })?;

        let toolbar = layout::build_node(app, window, page, |tree, parent| {
            NodeBuilder::boxed()
                .id("SETTINGS_PROXY_ADD_ROW")
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .flex_none()
                        .width_full()
                        .build(),
                )
                .build(tree, parent)
        })?;
        let add_holder = layout::build_node(app, window, toolbar, |tree, parent| {
            NodeBuilder::boxed()
                .id("SETTINGS_PROXY_ADD_BUTTON")
                .style(StyleBuilder::new().flex_none().build())
                .build(tree, parent)
        })?;
        let add_label = i18n::t_smol!(translate, "settings_ui.proxies.add");
        let add_button = Button::new(app, theme.clone(), format_smolstr!("{prefix}.add"))
            .primary()
            .icon(IconKind::Plus)
            .label(add_label)
            .on_click(move |ctx: &mut ActionCtx<'_>| {
                ctx.write(&request, Some(ProxyIntent::Add));
                let _ = ctx.app_mut().request_repaint(window);
            });
        add_button.build(app, window, add_holder)?;
        self.add_button = Some(add_button);

        let frame = layout::themed_box(
            app,
            window,
            page,
            SmolStr::new_static("SETTINGS_PROXY_TABLE"),
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .width_full()
                .min_width_0()
                .min_height_0()
                .overflow_hidden()
                .relative()
                .build(),
            PaintBuilder::new().rounded(6.0).build(),
            &theme,
            Some(Role::SurfaceSunken),
            Some(Role::LineDivider),
        )?;

        let labels = CellLabels::new(&translate);
        let cell_theme = theme.clone();
        let action_prefix = format_smolstr!("{prefix}.row");
        let edit_label = i18n::t_smol!(translate, "settings_ui.proxies.edit");
        let delete_label = i18n::t_smol!(translate, "settings_ui.proxies.delete");
        let cells = move |tree: &mut NodeTree, parent: NodeId, row: usize, column: usize| {
            let cell = CellBuild {
                labels: &labels,
                theme: &cell_theme,
                action_prefix: &action_prefix,
                edit_label: &edit_label,
                delete_label: &delete_label,
            };
            build_cell(tree, parent, row, column, &cell, rows);
        };
        let table = Table::new(
            app,
            theme.clone(),
            format_smolstr!("{prefix}.table"),
            columns(&translate),
            TableOptions::fixed(theme.tokens().medium.height).selection(SelectionMode::None),
            cells,
            "",
        );
        table.set_initial_rows(app, catalog.len());
        table.mount(app, window, frame)?;
        self.table = Some(table);

        let empty = layout::build_node(app, window, frame, move |tree, parent| {
            NodeBuilder::boxed()
                .id("SETTINGS_PROXY_EMPTY")
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(0.0)
                        .right(0.0)
                        .top(0.0)
                        .bottom(0.0)
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .build(),
                )
                .bind_style(move |cx| StylePatch {
                    visible: Some(cx.read(&rows).is_empty()),
                    ..StylePatch::default()
                })
                .build(tree, parent)
        })?;
        let empty_label = i18n::t_smol!(translate, "settings_ui.proxies.empty");
        layout::build_node(app, window, empty, move |tree, parent| {
            let size = theme.tokens().medium.font_size;
            let text_theme = theme.clone();
            NodeBuilder::text(SmolStr::default(), TextStyle::new(size))
                .bind_text(move |cx| {
                    if cx.read(&rows).is_empty() {
                        empty_label.clone()
                    } else {
                        SmolStr::default()
                    }
                })
                .bind_style(move |cx| StylePatch {
                    foreground: Some(text_theme.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(tree, parent)
        })?;

        self.register_row_actions(app);

        Ok(())
    }

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if let Some(request) = self.take_request(app) {
            self.adopt(app, request);
        }

        let catalog = self.catalog(app);
        if self.applied.as_ref() != Some(&catalog) {
            let count = catalog.len();
            if let Some(rows) = self.rows {
                app.write(&rows, catalog.clone());
            }
            if count != self.applied_rows {
                if let Some(table) = &self.table {
                    table.set_rows(app, count)?;
                }
                self.applied_rows = count;
                self.register_row_actions(app);
            }
            self.applied = Some(catalog);
        }

        let mut close_editor = false;
        if let Some(editor) = self.editor.as_mut() {
            editor.sync(app)?;
            close_editor = !editor.is_open(app);
        }
        if close_editor && let Some(editor) = self.editor.take() {
            editor.release(app);
        }

        if let Some(alert) = self.alert.as_ref()
            && let Err(error) = alert.sync(app)
        {
            log::warn!("syncing the delete-proxy alert failed: {error}");
        }
        if self.alert.as_ref().is_some_and(|alert| !alert.is_open(app))
            && let Some(alert) = self.alert.take()
        {
            alert.release(app);
        }

        if let Some(table) = &self.table {
            table.sync(app)?;
        }

        Ok(())
    }

    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        if let Some(editor) = self.editor.as_mut() {
            return editor.dispatch(app, event);
        }
        false
    }

    fn release(&mut self, app: &mut App) {
        if let Some(editor) = self.editor.take() {
            editor.release(app);
        }
        if let Some(alert) = self.alert.take() {
            alert.release(app);
        }
        for action in self.row_actions.drain(..) {
            app.off_action(&action);
        }
        if let Some(table) = self.table.take() {
            table.release(app);
        }
        if let Some(button) = self.add_button.take() {
            button.release(app);
        }
        if let Some(scope) = self.scope.take() {
            app.release_scope(scope);
        }
    }

    fn scrolls(&self) -> bool {
        false
    }
}

struct CellLabels {
    no_authorization: SmolStr,
    username_password: SmolStr,
    dns_local: SmolStr,
    dns_proxy: SmolStr,
    mask: SmolStr,
}

impl CellLabels {
    fn new(translate: &Translate) -> Self {
        Self {
            no_authorization: i18n::t_smol!(translate, "settings_ui.proxies.no_authorization"),
            username_password: i18n::t_smol!(translate, "settings_ui.proxies.username_password"),
            dns_local: i18n::t_smol!(translate, DnsMode::Local.label_key()),
            dns_proxy: i18n::t_smol!(translate, DnsMode::Proxy.label_key()),
            mask: SmolStr::new_static(PASSWORD_MASK),
        }
    }
}

struct CellBuild<'a> {
    labels: &'a CellLabels,
    theme: &'a ThemeHandle,
    action_prefix: &'a SmolStr,
    edit_label: &'a SmolStr,
    delete_label: &'a SmolStr,
}

fn columns(translate: &Translate) -> Vec<Column> {
    let headers = [
        i18n::t_smol!(translate, "settings_ui.proxies.name"),
        i18n::t_smol!(translate, "settings_ui.proxies.address"),
        i18n::t_smol!(translate, "settings_ui.proxies.port"),
        i18n::t_smol!(translate, "settings_ui.proxies.authorization"),
        i18n::t_smol!(translate, "settings_ui.proxies.username"),
        i18n::t_smol!(translate, "settings_ui.proxies.password"),
        i18n::t_smol!(translate, "settings_ui.proxies.dns_mode"),
        SmolStr::default(),
    ];

    headers
        .into_iter()
        .zip(COLUMN_WEIGHTS)
        .zip(COLUMN_ALIGNS)
        .map(|((header, weight), align)| Column::flex(header, weight).align(align))
        .collect()
}

fn build_cell(
    tree: &mut NodeTree,
    parent: NodeId,
    row: usize,
    column: usize,
    cell: &CellBuild<'_>,
    rows: Signal<Rows>,
) {
    if let Err(error) = cell_node(tree, parent, row, column, cell, rows) {
        log::warn!("building a proxy catalog cell failed: {error}");
    }
}

fn cell_node(
    tree: &mut NodeTree,
    parent: NodeId,
    row: usize,
    column: usize,
    cell: &CellBuild<'_>,
    rows: Signal<Rows>,
) -> Result<(), EngineError> {
    if column + 1 == COLUMN_COUNT {
        return row_controls(tree, parent, row, cell);
    }

    let size = cell.theme.tokens().small.font_size;
    let text_theme = cell.theme.clone();
    let labels = CellLabels {
        no_authorization: cell.labels.no_authorization.clone(),
        username_password: cell.labels.username_password.clone(),
        dns_local: cell.labels.dns_local.clone(),
        dns_proxy: cell.labels.dns_proxy.clone(),
        mask: cell.labels.mask.clone(),
    };
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(size)
            .with_wrap(TextWrap::NoWrap)
            .with_overflow(TextOverflow::Ellipsis, 1),
    )
    .bind_text(move |cx| {
        let rows = cx.read(&rows);
        cell_text(&rows, row, column, &labels)
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(text_theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, parent)?;

    Ok(())
}

fn cell_text(rows: &Rows, row: usize, column: usize, labels: &CellLabels) -> SmolStr {
    let Some((name, proxy)) = rows.get(row) else {
        return SmolStr::default();
    };
    let authenticated = matches!(
        proxy.authorization,
        ProxyAuthorizationConfig::UsernameAndPassword { .. }
    );

    match column {
        0 => name.clone(),
        1 => proxy.addr.clone(),
        2 => format_smolstr!("{}", proxy.port),
        3 => {
            if authenticated {
                labels.username_password.clone()
            } else {
                labels.no_authorization.clone()
            }
        }
        4 => match &proxy.authorization {
            ProxyAuthorizationConfig::None => SmolStr::default(),
            ProxyAuthorizationConfig::UsernameAndPassword { username, .. } => username.clone(),
        },
        5 => match &proxy.authorization {
            ProxyAuthorizationConfig::None => SmolStr::default(),
            ProxyAuthorizationConfig::UsernameAndPassword { .. } => labels.mask.clone(),
        },
        6 => match DnsMode::of(proxy.resolve_dns_at_remote) {
            DnsMode::Local => labels.dns_local.clone(),
            DnsMode::Proxy => labels.dns_proxy.clone(),
        },
        _ => SmolStr::default(),
    }
}

fn row_controls(
    tree: &mut NodeTree,
    parent: NodeId,
    row: usize,
    cell: &CellBuild<'_>,
) -> Result<(), EngineError> {
    let controls = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .justify_center()
                .width_full()
                .gap(ROW_GAP)
                .build(),
        )
        .build(tree, parent)?;

    for (suffix, label) in [("edit", cell.edit_label), ("delete", cell.delete_label)] {
        let action = Action::Custom(format_smolstr!("{}.{row}.{suffix}", cell.action_prefix));
        let button = NodeBuilder::boxed()
            .id(format_smolstr!("SETTINGS_PROXY_ROW_{row}_{suffix}"))
            .style(StyleBuilder::new().flex_none().padding(4.0).build())
            .tab_stop(true)
            .on_click(action)
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(label.clone()),
                ..A11yOverride::default()
            })
            .build(tree, controls)?;
        layout::themed_text_in_tree(
            tree,
            button,
            None,
            label.clone(),
            12.0,
            500,
            Role::FgMuted,
            cell.theme,
        )?;
    }

    Ok(())
}

fn delete_message(translate: &Translate, name: &SmolStr) -> SmolStr {
    let rich = i18n::t_smol!(
        translate,
        "settings_ui.proxies.delete_message",
        name = name.as_str()
    );

    parse_document(rich.as_ref(), &NoMarkdownAssets).plain_text()
}

struct NoMarkdownAssets;

impl MarkdownAssets for NoMarkdownAssets {
    fn load(&self, _path: &str) -> Option<Cow<'static, [u8]>> {
        None
    }
}
