use std::{cell::RefCell, rc::Rc};

use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event},
    interaction::ScrollHandle,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
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
        dialog::{Dialog, DialogBuilder},
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions},
        scroll_bar::ScrollBar,
    },
    theme::{
        appearance::Appearance as ThemeAppearance, components::ControlSize, handle::ThemeHandle,
        role::Role,
    },
};

use crate::{
    appearance,
    host::SettingsHost,
    layout,
    pages::{
        page::{PageBuild, PageView},
        registry::{self, PageId},
    },
    search::SearchFilter,
};

const SEARCH_WIDTH: f32 = 240.0;

const SIDEBAR_WIDTH: f32 = 232.0;

const SIDEBAR_ROW_HEIGHT: LogicalPx = px(30.0);

pub struct SettingsPage {
    host: SettingsHost,
    window: WindowId,
    key: SmolStr,
    root: NodeId,
    sidebar_column: NodeId,
    body: NodeId,
    current: SmolStr,
    current_signal: Signal<SmolStr>,
    selection: Signal<Option<SmolStr>>,
    query: SmolStr,
    filter: SearchFilter,
    language: LanguageCode,
    sidebar_query: Option<SmolStr>,
    sidebar_entries: Rc<RefCell<Vec<SidebarEntry>>>,
    sidebar: Option<List>,
    sidebar_actions: Vec<Action>,
    page: Option<Box<dyn PageView>>,
    page_scroll: Option<(ScrollHandle, NodeId, ScrollBar)>,
    search_input: Option<InputNode>,
    search_state: Option<Signal<InputState>>,
    alerts: Vec<Dialog>,
    alert_count: u64,
    system_dark: bool,
}

impl SettingsPage {
    pub fn mount(
        host: SettingsHost,
        key: &str,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<Self, EngineError> {
        let key = SmolStr::new(key);
        let theme = host.theme_handle();
        let current = SmolStr::new_static(registry::DEFAULT_PAGE);
        let current_signal = app.signal(current.clone());
        let selection = app.signal(Option::<SmolStr>::None);
        let language = host.language(app);
        let system_dark = theme.current(app) == ThemeAppearance::Dark;

        let root = layout::themed_box(
            app,
            window,
            parent,
            SmolStr::new_static("SETTINGS_PAGE"),
            StyleBuilder::new()
                .flex_col()
                .size_full()
                .min_width_0()
                .min_height_0()
                .build(),
            PaintSpec::default(),
            &theme,
            Some(Role::SurfaceCanvas),
            None,
        )?;

        let header = layout::build_node(app, window, root, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_HEADER"))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .flex_none()
                        .width_full()
                        .gap(px(16.0))
                        .padding_sides(px(16.0), px(24.0), px(12.0), px(24.0))
                        .build(),
                )
                .build(tree, parent)
        })?;
        let title_theme = theme.clone();
        layout::build_node(app, window, header, |tree, parent| {
            let style = title_theme
                .theme_for(ThemeAppearance::Dark)
                .role(Role::FgDefault);
            NodeBuilder::text(
                i18n::t_smol!(host.translate(), "settings_ui.title"),
                TextStyle::new(px(20.0)).with_weight(700).with_color(style),
            )
            .id(SmolStr::new_static("SETTINGS_TITLE"))
            .bind_style(move |cx| StylePatch {
                foreground: Some(title_theme.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree, parent)
        })?;
        let search_box = layout::build_node(app, window, header, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_SEARCH"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width(px(SEARCH_WIDTH))
                        .min_width_0()
                        .build(),
                )
                .build(tree, parent)
        })?;
        let search_state = app.signal(InputState::new(""));
        let search_placeholder = app.signal(i18n::t_smol!(
            host.translate(),
            "settings_ui.search.placeholder"
        ));
        let spec = std::sync::Arc::new(InputSpec {
            name: SmolStr::new_static("SETTINGS_SEARCH"),
            label: None,
            placeholder: app.read(&search_placeholder).clone(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Medium,
            submit: None,
        });
        let search_input = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            input::build(
                tree,
                search_box,
                std::sync::Arc::clone(&spec),
                &theme,
                &search_state,
                &search_placeholder,
            )
        };
        search_input.install(app)?;
        search_input.set_clearable(app, true);

        let body_row = layout::build_node(app, window, root, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_BODY"))
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .flex_1()
                        .width_full()
                        .min_width_0()
                        .min_height_0()
                        .gap(px(12.0))
                        .padding_sides(px(0.0), px(24.0), px(16.0), px(24.0))
                        .build(),
                )
                .build(tree, parent)
        })?;
        let sidebar_column = layout::build_node(app, window, body_row, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_SIDEBAR"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width(px(SIDEBAR_WIDTH))
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, parent)
        })?;
        layout::build_node(app, window, body_row, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_DIVIDER"))
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width(px(1.0))
                        .height_full()
                        .build(),
                )
                .bind_style(move |cx| StylePatch {
                    background: Some(theme.role(cx, Role::LineDivider)),
                    ..StylePatch::default()
                })
                .build(tree, parent)
        })?;
        let body = layout::build_node(app, window, body_row, |tree, parent| {
            NodeBuilder::boxed()
                .id(SmolStr::new_static("SETTINGS_CONTENT"))
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .min_width_0()
                        .min_height_0()
                        .build(),
                )
                .build(tree, parent)
        })?;

        let mut page = Self {
            host,
            window,
            key,
            root,
            sidebar_column,
            body,
            current,
            current_signal,
            selection,
            query: SmolStr::default(),
            filter: SearchFilter::All,
            language,
            sidebar_query: None,
            sidebar_entries: Rc::new(RefCell::new(Vec::new())),
            sidebar: None,
            sidebar_actions: Vec::new(),
            page: None,
            page_scroll: None,
            search_input: Some(search_input),
            search_state: Some(search_state),
            alerts: Vec::new(),
            alert_count: 0,
            system_dark,
        };
        page.filter = SearchFilter::new("", &registry::search_nodes(page.host.translate()));
        page.register_sidebar(app)?;
        page.rebuild_sidebar(app, window)?;
        page.open_page(app, window)?;

        Ok(page)
    }

    #[must_use]
    pub fn current_page(&self) -> &SmolStr {
        &self.current
    }

    #[must_use]
    pub fn query(&self) -> &SmolStr {
        &self.query
    }

    #[cfg(test)]
    pub(crate) fn set_query(&mut self, app: &mut App, query: &str) {
        let Some(input) = &self.search_input else {
            return;
        };
        input.update_value(app, query);
    }

    pub fn take_write_failure(&mut self) -> Option<SmolStr> {
        self.host
            .take_failure()
            .map(|error| SmolStr::new(error.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn filter_for_test(&self) -> &SearchFilter {
        &self.filter
    }

    #[cfg(test)]
    pub(crate) fn sidebar_labels(&self) -> Vec<SmolStr> {
        self.sidebar_entries
            .borrow()
            .iter()
            .map(SidebarEntry::label)
            .collect()
    }

    fn register_sidebar(&mut self, app: &mut App) -> Result<(), EngineError> {
        for page in registry::pages() {
            let id = page.id();
            let action = self.sidebar_action(&id);
            let selection = self.selection;
            app.on_action(action.clone(), move |ctx| {
                ctx.write(&selection, Some(id.clone()));
            });
            self.sidebar_actions.push(action);
        }
        Ok(())
    }

    fn sidebar_action(&self, page: &str) -> Action {
        Action::Custom(format_smolstr!("{}.sidebar.{page}", self.key))
    }

    fn rebuild_sidebar(&mut self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        if let Some(list) = self.sidebar.take() {
            list.release(app);
        }
        self.remove_children(app, self.sidebar_column);
        let filter = self.filter.clone();
        let translate = self.host.translate().clone();
        let entries = sidebar_entries(&filter, &translate);
        let count = entries.len();
        let entries = Rc::new(RefCell::new(entries));
        self.sidebar_entries = Rc::clone(&entries);
        self.sidebar_query = Some(self.query.clone());

        if self.filter.is_nothing() || count == 0 {
            let theme = self.host.theme_handle();
            let label = i18n::t_smol!(translate, "settings_ui.no_matches");
            layout::themed_text(
                app,
                window,
                self.sidebar_column,
                Some(SmolStr::new_static("SETTINGS_SIDEBAR_NO_MATCHES")),
                label,
                12.0,
                400,
                Role::FgMuted,
                &theme,
            )?;
            return Ok(());
        }

        let theme = self.host.theme_handle();
        let current = self.current_signal;
        let row_theme = theme.clone();
        let row_key = self.key.clone();
        let list = List::new(
            app,
            theme,
            format_smolstr!("{}.sidebar", self.key),
            ListOptions::fixed(SIDEBAR_ROW_HEIGHT).padding(px(0.0), px(4.0)),
            move |tree: &mut NodeTree, parent: NodeId, index: usize| {
                let entry = entries.borrow().get(index).cloned();
                build_sidebar_row(tree, parent, entry, &row_theme, current, &row_key)
            },
        );
        let selection = self.selection;
        let entries = Rc::clone(&self.sidebar_entries);
        list.on_activate(move |app: &mut App, index: usize| {
            let page = entries.borrow().get(index).and_then(SidebarEntry::page_id);
            if let Some(page) = page {
                let _ = app.write(&selection, Some(page));
            }
        });
        list.mount(app, window, self.sidebar_column)?;
        list.set_items(app, count)?;
        self.sidebar = Some(list);

        Ok(())
    }

    fn open_page(&mut self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        if let Some(mut page) = self.page.take() {
            page.release(app);
        }
        if let Some((_, _, bar)) = self.page_scroll.take() {
            bar.release(app);
        }
        self.remove_children(app, self.body);

        let Some(page_id) = self
            .page()
            .filter(|page| registry::page_is_reachable(*page, &self.filter, self.host.translate()))
        else {
            let theme = self.host.theme_handle();
            let text = i18n::t_smol!(self.host.translate(), "settings_ui.no_matches");
            layout::themed_text(
                app,
                window,
                self.body,
                Some(SmolStr::new_static("SETTINGS_PAGE_NO_MATCHES")),
                text,
                13.0,
                400,
                Role::FgMuted,
                &theme,
            )?;
            return Ok(());
        };

        let mut page = registry::page_view(page_id, &self.host, self.key.as_str());
        let scrolls = page.scrolls();
        let parent = if scrolls {
            let theme = self.host.theme_handle();
            let (handle, content, bar) = layout::scroll_region(
                app,
                window,
                self.body,
                &theme,
                format_smolstr!("{}.page", self.key).as_str(),
            )?;
            self.page_scroll = Some((handle, content, bar));
            content
        } else {
            self.body
        };
        let filter = self.filter.clone();
        let page_key = SmolStr::new(page.page_id());
        let cx = PageBuild {
            filter: &filter,
            key: self.key.as_str(),
            page: page_key,
        };
        page.build(app, window, parent, &cx)?;
        self.page = Some(page);

        Ok(())
    }

    fn page(&self) -> Option<PageId> {
        registry::page(self.current.as_ref())
    }

    fn move_to(&mut self, id: SmolStr, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        if self.current == id {
            return Ok(());
        }

        self.current = id;
        app.write(&self.current_signal, self.current.clone());
        self.sidebar_query = None;
        self.rebuild_sidebar(app, window)?;
        self.open_page(app, window)
    }

    fn adopt_query(&mut self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let query = self
            .search_state
            .map(|state| SmolStr::new(app.read(&state).text()))
            .unwrap_or_default();
        if query == self.query {
            return Ok(());
        }

        self.query = query;
        self.filter = SearchFilter::new(
            self.query.as_str(),
            &registry::search_nodes(self.host.translate()),
        );
        let reachable = self.page().is_some_and(|page| {
            registry::page_is_reachable(page, &self.filter, self.host.translate())
        });
        if !reachable
            && let Some(page) = registry::first_admitted_page(&self.filter, self.host.translate())
        {
            self.current = page.id();
            app.write(&self.current_signal, self.current.clone());
        }
        self.sidebar_query = None;
        self.rebuild_sidebar(app, window)?;
        self.open_page(app, window)
    }

    fn adopt_language(&mut self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let language = self.host.language(app);
        if language == self.language {
            return Ok(());
        }

        self.language = language;
        if let Some(input) = &self.search_input {
            let placeholder =
                i18n::t_smol!(self.host.translate(), "settings_ui.search.placeholder");
            input.update_placeholder(app, placeholder);
        }
        self.sidebar_query = None;
        self.rebuild_sidebar(app, window)?;
        self.open_page(app, window)
    }

    fn project(&mut self, app: &mut App) {
        let preference = self.host.config(app, |config| config.appearance);
        let resolved = appearance::appearance_of(preference, self.system_dark);
        if self.host.theme().current(app) != resolved {
            self.host.theme().set_appearance(app, resolved);
        }
    }

    fn report_write_failure(&mut self, app: &mut App) {
        let Some(error) = self.host.take_failure() else {
            return;
        };
        let detail = SmolStr::new(error.to_string());
        let translate = self.host.translate().clone();
        let title = i18n::t_smol!(translate, "settings_ui.save_failed.title");
        let message = i18n::t_smol!(
            translate,
            "settings_ui.save_failed.message",
            reason = detail.as_str()
        );
        let confirm = i18n::t_smol!(translate, "common.confirm");
        self.alert_count += 1;
        let dialog = DialogBuilder::alert(title.clone(), move |_dialog, _theme| {
            AlertBuilder::new()
                .ok(title.clone(), message.clone())
                .ok_text(confirm.clone())
        })
        .build(
            app,
            self.host.theme_handle(),
            self.host.layer(),
            self.host.animation(),
            format_smolstr!("{}.save-failed.{}", self.key, self.alert_count),
        );
        let _ = dialog.open(app, self.window);
        self.alerts.push(dialog);
    }

    fn sync_alerts(&mut self, app: &mut App) {
        self.alerts.retain(|dialog| {
            if dialog.is_open(app) {
                if let Err(error) = dialog.sync(app) {
                    log::warn!("synchronising a settings report failed: {error}");
                }
                true
            } else {
                dialog.release(app);
                false
            }
        });
    }

    pub fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if app.window(self.window).is_none() {
            return Ok(());
        }

        self.report_write_failure(app);
        self.sync_alerts(app);
        self.project(app);
        if let Some(input) = &self.search_input {
            input.sync(app);
        }

        let selection = app.read(&self.selection).clone();
        if let Some(selection) = selection {
            app.write(&self.selection, None);
            self.move_to(selection, app, self.window)?;
        }
        self.adopt_query(app, self.window)?;
        self.adopt_language(app, self.window)?;
        if self.sidebar_query.as_ref() != Some(&self.query) {
            self.rebuild_sidebar(app, self.window)?;
        }

        if let Some(page) = self.page.as_mut() {
            page.sync(app)?;
        }
        if let Some((handle, content, _)) = &self.page_scroll {
            layout::sync_scroll_content(app, self.window, handle, *content);
        }
        if let Some(list) = &self.sidebar {
            list.sync(app)?;
        }
        if let Some((_, _, bar)) = &self.page_scroll {
            bar.sync(app)?;
        }

        Ok(())
    }

    pub fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        if let Event::AppearanceChanged { dark } = event {
            self.system_dark = *dark;
        }
        let mut consumed = false;
        if let Some(page) = self.page.as_mut() {
            consumed |= page.dispatch(app, event);
        }
        consumed
    }

    pub fn release(&mut self, app: &mut App) {
        if let Some(mut page) = self.page.take() {
            page.release(app);
        }
        if let Some((_, _, bar)) = self.page_scroll.take() {
            bar.release(app);
        }
        if let Some(list) = self.sidebar.take() {
            list.release(app);
        }
        for action in self.sidebar_actions.drain(..) {
            app.off_action(&action);
        }
        if let Some(input) = self.search_input.take() {
            input.release(app);
        }
        for dialog in self.alerts.drain(..) {
            dialog.release(app);
        }
        if let Err(error) = app.remove_node(self.window, self.root) {
            log::warn!("removing the settings page failed: {error}");
        }
    }

    fn remove_children(&self, app: &mut App, parent: NodeId) {
        let children = app
            .window(self.window)
            .and_then(|target| target.tree().get(parent).ok())
            .map(|entry| entry.children().to_vec())
            .unwrap_or_default();
        for child in children {
            if let Err(error) = app.remove_node(self.window, child) {
                log::warn!("removing a rebuilt settings node failed: {error}");
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SidebarEntry {
    Heading(SmolStr),
    Page { id: SmolStr, label: SmolStr },
    Separator,
}

impl SidebarEntry {
    #[cfg(test)]
    fn label(&self) -> SmolStr {
        match self {
            Self::Heading(label) => label.clone(),
            Self::Page { label, .. } => label.clone(),
            Self::Separator => SmolStr::default(),
        }
    }

    fn page_id(&self) -> Option<SmolStr> {
        match self {
            Self::Heading(_) | Self::Separator => None,
            Self::Page { id, .. } => Some(id.clone()),
        }
    }
}

fn build_sidebar_row(
    tree: &mut NodeTree,
    parent: NodeId,
    entry: Option<SidebarEntry>,
    theme: &ThemeHandle,
    current: Signal<SmolStr>,
    key: &str,
) -> Result<(), EngineError> {
    match entry {
        Some(SidebarEntry::Heading(label)) => {
            layout::themed_text_in_tree(
                tree,
                parent,
                None,
                label,
                11.0,
                600,
                Role::FgMuted,
                theme,
            )?;
        }
        Some(SidebarEntry::Separator) => {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width_full()
                        .height(SIDEBAR_ROW_HEIGHT)
                        .padding_sides(px(10.0), px(6.0), px(10.0), px(6.0))
                        .build(),
                )
                .bind_style({
                    let theme = theme.clone();
                    move |cx| StylePatch {
                        border: Some(ui_core::paint::primitive::Border {
                            width: px(1.0),
                            color: theme.role(cx, Role::LineDivider),
                        }),
                        ..StylePatch::default()
                    }
                })
                .build(tree, parent)?;
        }
        Some(SidebarEntry::Page { id, label }) => {
            let row_theme = theme.clone();
            let action = Action::Custom(format_smolstr!("{key}.sidebar.{id}"));
            let row = NodeBuilder::boxed()
                .id(format_smolstr!("SETTINGS_ENTRY_{id}"))
                .on_click(action)
                .style(
                    StyleBuilder::new()
                        .flex_row()
                        .items_center()
                        .width_full()
                        .height(SIDEBAR_ROW_HEIGHT)
                        .min_width_0()
                        .gap(px(8.0))
                        .padding_sides(px(0.0), px(8.0), px(0.0), px(8.0))
                        .build(),
                )
                .tab_stop(true)
                .a11y(A11yOverride {
                    role: Some(A11yRole::Button),
                    label: Some(label.clone()),
                    ..A11yOverride::default()
                })
                .bind_style({
                    let selected = id;
                    move |cx| {
                        let is_selected = cx.read(&current).as_str() == selected.as_str();
                        StylePatch {
                            background: if is_selected {
                                Some(row_theme.role(cx, Role::Selection))
                            } else {
                                Some(row_theme.role(cx, Role::SurfaceCanvas))
                            },
                            ..StylePatch::default()
                        }
                    }
                })
                .build(tree, parent)?;
            layout::themed_text_in_tree(tree, row, None, label, 12.0, 500, Role::FgDefault, theme)?;
        }
        None => {
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .flex_none()
                        .width_full()
                        .height(SIDEBAR_ROW_HEIGHT)
                        .build(),
                )
                .build(tree, parent)?;
        }
    }

    Ok(())
}

fn sidebar_entries(filter: &SearchFilter, translate: &Translate) -> Vec<SidebarEntry> {
    let mut entries = Vec::new();

    for group in registry::groups() {
        let pages = group
            .pages
            .into_iter()
            .filter(|page| registry::page_is_reachable(*page, filter, translate))
            .collect::<Vec<_>>();
        if pages.is_empty() {
            continue;
        }

        entries.push(SidebarEntry::Heading(i18n::t_smol!(
            translate,
            group.label_key
        )));
        for page in pages {
            entries.push(SidebarEntry::Page {
                id: page.id(),
                label: page.title(translate),
            });
        }
    }

    let roots = registry::root_pages()
        .into_iter()
        .filter(|page| registry::page_is_reachable(*page, filter, translate))
        .collect::<Vec<_>>();
    if !roots.is_empty() {
        if !entries.is_empty() {
            entries.push(SidebarEntry::Separator);
        }
        for page in roots {
            entries.push(SidebarEntry::Page {
                id: page.id(),
                label: page.title(translate),
            });
        }
    }

    entries
}
