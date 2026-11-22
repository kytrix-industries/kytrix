//! The settings page: what the application is configured by, drawn in the page area of the main
//! window in place of the workspace.
//!
//! The page owns what outlives a frame: the page that is drawn and whether it is drawn in a scroll
//! region. Each page is native UI — its rows, cards and controls are written where the page is
//! written — and this module is the frame around them: the sidebar, the search, the routing between
//! pages, and the one place a failed write is reported.
//!
//! The page has the gallery's shape: a title and the search field in its header, and the settings
//! menu on the left with the detail page behind the selected entry on the right. There is no
//! footer: a control writes the change it makes at once, so there is nothing to confirm and nothing
//! to cancel, and the page is left through the workspace rows alone.
//!
//! A control writes the settings rather than a copy of them, through the session every view of this
//! page shares — see [`SettingsHost::write`]. A write that cannot be stored leaves the settings as
//! they were and is reported over the page, so what the user had is still there to try again with.
//!
//! # What follows the settings
//!
//! The page projects the appearance it owns: [`Self::sync`] resolves the stored preference against
//! the system and writes the theme, because a control that changed the preference has to see the
//! change on the frame that follows the write. The language is the composition root's projection
//! (it keeps the translation context and the language signal in step, the way it does for every
//! widget); the page follows the signal and rebuilds its text when it moves.
//!
//! A page is native UI, so a page's text is built when the page is: a language that moved on is
//! what rebuilds it (the same rule `crates/widgets/AGENTS.md` §15 states for static text). The
//! sidebar and the page behind it are rebuilt together, because a row and the page it names are one
//! thing.

use std::{cell::RefCell, rc::Rc};

use base_ui::{
    kit::{
        alert::AlertBuilder,
        dialog::{Dialog, DialogBuilder},
        input::{self, InputNode, InputSpec, InputState},
        list::{List, ListOptions},
        scroll_bar::ScrollBar,
    },
    style_builder::StyleBuilder,
    theme::{
        appearance::Appearance as ThemeAppearance, components::ControlSize, handle::ThemeHandle,
        role::Role,
    },
};
use i18n::{language::codes::LanguageCode, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event},
    interaction::ScrollHandle,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::{PaintSpec, StylePatch},
    text::TextStyle,
    window::WindowId,
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

/// The width of the field the settings are searched with.
const SEARCH_WIDTH: f32 = 240.0;

/// The width of the sidebar column.
const SIDEBAR_WIDTH: f32 = 232.0;

/// The height of one sidebar row.
const SIDEBAR_ROW_HEIGHT: f32 = 30.0;

/// The whole of the settings, as the main window's settings page.
pub struct SettingsPage {
    /// The collaborators every control writes through.
    host: SettingsHost,
    /// The window the page lives in.
    window: WindowId,
    /// The unique key prefix of this page instance (two panes have two).
    key: SmolStr,
    /// The node every part of the page is built under.
    root: NodeId,
    /// The column the sidebar is drawn in.
    sidebar_column: NodeId,
    /// The node the current page is built into.
    body: NodeId,
    /// The current page's id.
    current: SmolStr,
    /// The current page, as every sidebar row reads it for its selected state.
    current_signal: Signal<SmolStr>,
    /// The page a sidebar row asked for, waiting for the frame that moves to it.
    selection: Signal<Option<SmolStr>>,
    /// The query typed into the search field.
    query: SmolStr,
    /// What the query admits.
    filter: SearchFilter,
    /// The language the text this page holds was built in.
    language: LanguageCode,
    /// The query the sidebar was last built for.
    sidebar_query: Option<SmolStr>,
    /// The rows of the sidebar, in the order they are drawn.
    sidebar_entries: Rc<RefCell<Vec<SidebarEntry>>>,
    /// The list the sidebar rows are drawn by, when the sidebar is drawn at all.
    sidebar: Option<List>,
    /// The actions one sidebar row dispatches when it is activated.
    sidebar_actions: Vec<Action>,
    /// The page behind the current entry.
    page: Option<Box<dyn PageView>>,
    /// The scroll region the current page lives in, when it scrolls.
    page_scroll: Option<(ScrollHandle, NodeId, ScrollBar)>,
    /// The search field and the state it edits.
    search_input: Option<InputNode>,
    search_state: Option<Signal<InputState>>,
    /// The spec the search field was built with, which is what a programmatic write needs.
    #[cfg(test)]
    search_spec: std::sync::Arc<InputSpec>,
    /// The dialogs this page opened and has not released yet.
    alerts: Vec<Dialog>,
    /// How many reports this page has opened, so each one gets a fresh key.
    alert_count: u64,
    /// Whether the operating system reports a dark appearance.
    system_dark: bool,
}

impl SettingsPage {
    /// Builds the settings page under `parent` and opens the page the settings start on.
    ///
    /// `key` is the unique prefix of this page instance: two settings pages in one application (two
    /// panes) need two, because a kit key is an application-level action identity.
    ///
    /// # Errors
    /// Engine node-building errors, or a failure from a kit component the page builds.
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
                        .gap(16.0)
                        .padding_sides(16.0, 24.0, 12.0, 24.0)
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
                TextStyle::new(20.0).with_weight(700).with_color(style),
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
                        .width(SEARCH_WIDTH)
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
            placeholder: app.read(&search_placeholder).clone(),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: ControlSize::Medium,
            submit: None,
        });
        let mut search_input = {
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
        search_input.install(app);
        // The field is cleared the way the old search field was: the clear control is the
        // field's own, and clearing it is what brings the whole sidebar back.
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
                        .gap(12.0)
                        .padding_sides(0.0, 24.0, 16.0, 24.0)
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
                        .width(SIDEBAR_WIDTH)
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
                        .width(1.0)
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
            #[cfg(test)]
            search_spec: std::sync::Arc::clone(&spec),
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

    /// The page currently drawn, for the shell and for the tests that assert where routing left it.
    #[must_use]
    pub fn current_page(&self) -> &SmolStr {
        &self.current
    }

    /// The query the page is answering, for the tests of the search.
    #[must_use]
    pub fn query(&self) -> &SmolStr {
        &self.query
    }

    /// Types query into the search field, for the tests of what a query answers.
    #[cfg(test)]
    pub(crate) fn set_query(&mut self, app: &mut App, query: &str) {
        let Some(state) = self.search_state else {
            return;
        };
        let spec = std::sync::Arc::clone(&self.search_spec);
        app.update(&state, |state| {
            state.set_text(&spec, query);
        });
    }

    /// Says why the last write could not be stored, if one is waiting.
    ///
    /// The page reports its own failures; the shell asks for one only when it takes the page away
    /// before a frame reported it, so the reason is carried to what is drawn next rather than
    /// dropped with the page. The reason is taken out of the session, so it is reported once.
    pub fn take_write_failure(&mut self) -> Option<SmolStr> {
        self.host
            .take_failure()
            .map(|error| SmolStr::new(error.to_string()))
    }

    /// What the query admits, for the tests of the search's answer.
    #[cfg(test)]
    pub(crate) fn filter_for_test(&self) -> &SearchFilter {
        &self.filter
    }

    /// The rows the sidebar is drawing, for the tests of its labels.
    #[cfg(test)]
    pub(crate) fn sidebar_labels(&self) -> Vec<SmolStr> {
        self.sidebar_entries
            .borrow()
            .iter()
            .map(SidebarEntry::label)
            .collect()
    }

    /// Registers the actions one sidebar row dispatches.
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

    /// The action one sidebar row dispatches.
    fn sidebar_action(&self, page: &str) -> Action {
        Action::Custom(format_smolstr!("{}.sidebar.{page}", self.key))
    }

    /// Builds the sidebar rows the current filter admits, replacing the ones drawn before.
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
            ListOptions::fixed(SIDEBAR_ROW_HEIGHT).padding(0.0, 4.0),
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

    /// Releases the page that is drawn and builds the one the settings are on.
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

    /// The page the ids name, when the settings are on one.
    fn page(&self) -> Option<PageId> {
        registry::page(self.current.as_ref())
    }

    /// Moves the page to one entry of the sidebar.
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

    /// Answers what the search field holds.
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

    /// The language the interface is drawn in, as the settings name it.
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

    /// Projects the appearance the settings name, so a control that changed it sees the change on
    /// the frame that follows the write.
    ///
    /// The language is the composition root's projection: it moves the translation context and the
    /// language signal together, and the page follows the signal in [Self::adopt_language].
    fn project(&mut self, app: &mut App) {
        let preference = self.host.config(app, |config| config.appearance);
        let resolved = appearance::appearance_of(preference, self.system_dark);
        if self.host.theme().current(app) != resolved {
            self.host.theme().set_appearance(app, resolved);
        }
    }

    /// Reports the last write that could not be stored, if one is waiting.
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

    /// Drives the reports that are open and releases the ones that closed.
    ///
    /// A dialog is a kit component: it owns a fade-in / fade-out and the focus trap, so it has to
    /// be synchronised on every frame the way the page's other controls are. A dialog that is not
    /// synchronised never finishes closing, which would leave its modal scope over the page.
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

    /// Runs the per-frame half of the page: what the settings moved, and what the page draws.
    ///
    /// # Errors
    /// Engine node-building errors from a rebuild, or a kit component that refused a write.
    pub fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if app.window(self.window).is_none() {
            return Ok(());
        }

        self.report_write_failure(app);
        self.sync_alerts(app);
        self.project(app);

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
        if let Some(input) = &self.search_input {
            input.sync(app);
        }

        Ok(())
    }

    /// Offers one platform event to the controls of the page; returns whether one consumed it.
    pub fn dispatch(&mut self, app: &mut App, event: &Event) -> bool {
        if let Event::AppearanceChanged { dark } = event {
            self.system_dark = *dark;
        }
        let mut consumed = false;
        if let Some(input) = &self.search_input {
            consumed |= input.dispatch(app, event);
        }
        if let Some(page) = self.page.as_mut() {
            consumed |= page.dispatch(app, event);
        }
        consumed
    }

    /// Releases every component the page owns and removes the nodes it built.
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

    /// Removes every child of `parent`, which is how a rebuilt region is cleaned before it is built
    /// again.
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

/// One row of the settings sidebar.
#[derive(Clone, Debug)]
pub(crate) enum SidebarEntry {
    /// A group's name over the pages that follow it: text, because a group is not something a user
    /// opens.
    Heading(SmolStr),
    /// A page, which is the one kind of row that opens something.
    Page { id: SmolStr, label: SmolStr },
    /// A divider between one part of the sidebar and the next.
    Separator,
}

impl SidebarEntry {
    /// The text this row draws.
    #[cfg(test)]
    fn label(&self) -> SmolStr {
        match self {
            Self::Heading(label) => label.clone(),
            Self::Page { label, .. } => label.clone(),
            Self::Separator => SmolStr::default(),
        }
    }

    /// The page this row opens, or `None` for anything else.
    fn page_id(&self) -> Option<SmolStr> {
        match self {
            Self::Heading(_) | Self::Separator => None,
            Self::Page { id, .. } => Some(id.clone()),
        }
    }
}

/// Builds one sidebar row inside the virtual list that draws them.
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
                        .padding_sides(10.0, 6.0, 10.0, 6.0)
                        .build(),
                )
                .bind_style({
                    let theme = theme.clone();
                    move |cx| StylePatch {
                        border: Some(ui_core::paint::primitive::Border {
                            width: 1.0,
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
                        .gap(8.0)
                        .padding_sides(0.0, 8.0, 0.0, 8.0)
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

/// The sidebar as it is drawn, holding only what the filter leaves reachable.
///
/// A page is reached by its label or by a setting on it — see [`registry::page_is_reachable`] — and
/// a group with none of its pages left is not drawn at all, which is what keeps an unmatched part
/// of the settings out of the way instead of dimming it.
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
