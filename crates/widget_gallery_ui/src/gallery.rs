//! The widget gallery: the page in which a pane's widget is chosen.
//!
//! The gallery is a page rather than a window dialog: the workspace overlay embeds it
//! and owns what happens to a choice, while the gallery only reports one. It is built
//! from the registry rather than from a list of its own — the categories are
//! [`WidgetCategory::ALL`] and the widgets of a category are the registered widgets
//! that declare it and may be chosen — so a widget added to the catalogue appears here
//! without anything being written here. The one registration that is not listed is the
//! widget a pane falls back to: the framework builds it, the user never picks it, and
//! it is the pane's starting point rather than an answer to this page.
//!
//! # The page is engine nodes and kit components
//!
//! There is no render function and no rebuild-every-frame tree: [`WidgetGallery::mount`]
//! builds one persistent subtree, a route registered with the application's
//! [`FrameRouter`](widget::frame::FrameRouter) synchronizes it every frame, and only what
//! actually changed is rebuilt:
//!
//! - the search field and the decision buttons are kit components, owned by the page and
//!   released with it; the category filter in the sidebar is kit buttons too;
//! - the catalogue rows are rebuilt when the search, the category, the language or the
//!   catalogue itself changes — never otherwise;
//! - the widget selected in the catalogue is mounted as a live preview in the right-hand
//!   pane through [`GalleryCatalogue`]; selecting another widget releases the previous
//!   preview first, so a page moved through its catalogue holds at most one widget
//!   instance;
//! - the whole page is released in one call: preview, kit components, keyboard bindings,
//!   action handlers, nodes and the page's signal scope.
//!
//! # Where the services come from
//!
//! A widget may not build its own collaborators, and neither may this page: the
//! application's one [`WidgetServices`] aggregate is passed in. Its
//! [`Layer`](base_ui::kit::layer::Layer), [`FrameRouter`](widget::frame::FrameRouter) and
//! [`AnimationRouter`](base_ui::kit::overlay::AnimationRouter) are the composition root's
//! single instances — the preview widgets mounted here register their routes on the same
//! router the page does, and every kit component a preview builds resolves the same layer
//! and completion router. The page never constructs a second one.
//!
//! # i18n
//!
//! Every user-visible sentence is a catalogue key resolved with `t_smol!`. A language
//! switch does not re-report anything: the page notices the injected language signal in
//! its next sync and rebuilds the components whose text is baked in at build time (the
//! search field, the category buttons, the decision buttons and the rows) while the
//! preview keeps running — a widget resolves its own copy on the frame that draws it.
//!
//! # What the page holds across frames
//!
//! Only what the user has done in it: the widget selected, the search text and the
//! category filter. The page opens on the widget the pane is already showing, so opening
//! it on a pane that has one is opening it on the answer that pane already has.
//!
//! # What a selection means
//!
//! Selecting a row previews that widget and changes what a confirmation would carry; the
//! pane's widget is not touched until the confirmation leaves this page. Cancelling
//! therefore changes nothing, and the confirm control refuses to confirm while nothing is
//! selected.
//!
//! # Where the search narrows
//!
//! The field searches the whole catalogue rather than one category: a section a search
//! leaves empty is not drawn, and a search that matches nothing is drawn as a search that
//! found nothing rather than as a page with nothing on it. A widget matches when its
//! localized name or the description in the language the page is drawn in contains the
//! text, case-insensitively. The category filter narrows within that.
//!
//! # Where the keyboard goes
//!
//! The page takes the keyboard when it opens — the search field is focused, so typing
//! narrows the catalogue at once — and holds it for as long as it is open: Escape and
//! Enter are the page's own keys and Tab keeps the keyboard in the field. The bindings are
//! scoped to the page's own root node, so they only fire while the keyboard is inside the
//! page; the pane behind it keeps its own bindings. The page is also the only owner of its
//! window's text and IME events while it has the focus, which it hands to the search
//! field — the kit input has no binding face of its own. A pointer press in the catalogue
//! chooses a row rather than moving the keyboard, so what the reader types after a click
//! still narrows the list.
//!
//! # What the page reports
//!
//! A confirmation and a cancellation are [`WidgetGalleryEvent`]s, read back with
//! [`WidgetGallery::take_event`]. The page does not close itself and does not know what is
//! behind it: what a confirmation changes, and whether the page goes away, belongs to
//! whoever embedded it.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
    sync::atomic::{AtomicU64, Ordering},
};

use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::IconSpec,
        input::{self, InputNode, InputSpec, InputState},
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        separator::Separator,
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, InputEvent, KeyBinding, KeyCode, Modifiers},
    interaction::{ScrollConfig, ScrollHandle},
    math::{Point, Size},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{color::Color, primitive::Border, style::StylePatch},
    text::TextStyle,
    window::WindowId,
};
use widget::{
    any::AnyWidget, category::WidgetCategory, descriptor::WidgetDescriptor, identity::WidgetId,
    services::WidgetServices,
};

use crate::{catalogue::GalleryCatalogue, widget_icon};

/// The size of a widget's icon in the catalogue rows.
const ICON_SIZE: f32 = 20.0;

/// The height of one row: icon, name, description and the selection mark.
const ROW_HEIGHT: f32 = 56.0;

/// The vertical gap between two rows and the height of one category heading.
const ROW_GAP: f32 = 4.0;
const HEADING_HEIGHT: f32 = 20.0;

/// The gap between a category heading and its first row.
const HEADING_GAP: f32 = 8.0;

/// The gap between two categories.
const CATEGORY_GAP: f32 = 16.0;

/// The width of the field the gallery is searched with.
const SEARCH_WIDTH: f32 = 240.0;

/// The width of the sidebar that filters the catalogue by category.
const SIDEBAR_WIDTH: f32 = 168.0;

/// The width of the preview pane.
const PREVIEW_WIDTH: f32 = 320.0;

/// The padding around the catalogue's content column.
const CONTENT_PADDING: f32 = 16.0;

/// The width of the selection mark at the right of a row.
const MARK_SIZE: f32 = 14.0;

/// How many galleries this process has built, so two pages never share a component key.
///
/// kit keys are application-level action identities (`"<key>.click"`), so two pages that
/// used the same constant key would overwrite each other's handlers — the second page's
/// click would run the first page's callback, and releasing either would remove the
/// other's handler. This counter is what the page's key prefix comes from; the same page
/// keeps its prefix for its whole life, so rebuilds and releases pair up.
static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);

/// What a choice made in the gallery is.
///
/// It is the page's whole output: [`WidgetGalleryEvent::Confirm`] carries the widget the
/// user selected and [`WidgetGalleryEvent::Cancel`] is the user leaving without choosing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetGalleryEvent {
    /// The user confirmed the selection.
    Confirm(WidgetId),
    /// The user cancelled, by pressing the cancel control or Escape.
    Cancel,
}

/// The key prefix of one gallery page.
///
/// Every engine debug id and every kit action identity the page builds is derived from it,
/// which is what lets two pages live in one application without colliding.
#[derive(Clone, Debug)]
struct GalleryKeys {
    prefix: SmolStr,
}

impl GalleryKeys {
    fn new() -> Self {
        let page = NEXT_PAGE.fetch_add(1, Ordering::Relaxed);
        Self {
            prefix: format_smolstr!("widget_gallery.{page}"),
        }
    }

    /// The key of one control inside the page.
    fn control(&self, suffix: &str) -> SmolStr {
        format_smolstr!("{}.{suffix}", self.prefix)
    }

    /// The action identity of one element inside the page.
    fn action(&self, suffix: &str) -> Action {
        Action::Custom(self.control(suffix))
    }
}

/// The kit components whose text is baked in at build time.
///
/// A language switch releases them and builds them again in the new language
/// (`crates/widgets/AGENTS.md` §15.1); everything with a binding face (the header copy,
/// the preview title, the preview hint) is not here.
struct Chrome {
    input: InputNode,
    categories: Vec<(WidgetCategory, Button)>,
    cancel: Button,
    confirm: Button,
}

/// One catalogue row the page built: the node a click lands on and the widget it names.
struct Row {
    node: NodeId,
    widget: WidgetId,
}

/// What the row list was built against.
///
/// The page rebuilds the list only when this changes; a steady frame compares it and
/// returns, so an idle gallery asks for no nodes (I3).
#[derive(Clone, Debug, Eq, PartialEq)]
struct RowsBuilt {
    query: SmolStr,
    filter: Option<WidgetCategory>,
    language: LanguageCode,
}

/// Everything one mounted page owns.
struct GalleryCore {
    window: WindowId,
    keys: GalleryKeys,
    services: Rc<WidgetServices>,
    catalogue: Rc<dyn GalleryCatalogue>,
    theme: ThemeHandle,
    translate: Translate,
    scope: SignalScope,
    /// The page's root node; keyboard bindings are scoped to it.
    panel: NodeId,
    /// The stable containers whose children a language rebuild replaces.
    header: NodeId,
    sidebar: NodeId,
    footer: NodeId,
    /// The stable catalogue viewport and the node the rows are built into.
    content: NodeId,
    scroll: Option<ScrollHandle>,
    scrollbar: ScrollBar,
    /// The stable preview pane: the host for the mounted surface.
    preview_host: NodeId,
    /// The page's own state.
    selection: Signal<Option<WidgetId>>,
    input_state: Signal<InputState>,
    placeholder: Signal<SmolStr>,
    selected: Option<WidgetId>,
    query: SmolStr,
    filter: Option<WidgetCategory>,
    language: LanguageCode,
    entries: Vec<WidgetDescriptor>,
    built_rows: Option<RowsBuilt>,
    rows: Vec<Row>,
    rows_by_node: HashMap<NodeId, WidgetId>,
    chrome: Option<Chrome>,
    /// The mounted preview and the selection it was mounted for.
    preview: Option<AnyWidget>,
    preview_selection: Option<WidgetId>,
    /// What a host reads back.
    event: Option<WidgetGalleryEvent>,
    /// The page's frame route, unregistered on release.
    route: Option<widget::frame::RouteId>,
    /// The page's key actions, whose handlers are removed on release.
    actions: Vec<Action>,
    released: bool,
}

impl GalleryCore {
    /// Builds one page under `parent` and registers its frame route.
    fn mount(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: Rc<WidgetServices>,
        catalogue: Rc<dyn GalleryCatalogue>,
        current: Option<WidgetId>,
    ) -> Result<WidgetGallery, EngineError> {
        let theme = services.theme.clone();
        let translate = services.translate.clone();
        let language = *app.read(&services.language);
        let keys = GalleryKeys::new();
        let scope = app.signal_scope();
        let selection = app.signal_scoped(scope, None);
        let input_state = app.signal_scoped(scope, InputState::new(""));
        let placeholder = app.signal_scoped(
            scope,
            t_smol!(&translate, "widget_gallery.search_placeholder"),
        );

        let entries = catalogue
            .descriptors()
            .into_iter()
            .filter(WidgetDescriptor::selectable)
            .collect::<Vec<_>>();
        // The page opens on the widget the pane is showing, when it is still an answer the
        // catalogue offers. A pane showing the framework's fallback opens with nothing
        // selected: there is no answer of the user's to preselect.
        let selected = current.filter(|current| entries.iter().any(|entry| entry.id() == current));
        app.write(&selection, selected.clone());

        let panel = build_panel(app, window, &keys, &theme, parent)?;
        let left = build_container(
            app,
            window,
            panel,
            StyleBuilder::new()
                .flex_1()
                .flex_col()
                .min_width_0()
                .min_height_0()
                .build(),
        )?;
        let header = build_header(
            app,
            window,
            &keys,
            left,
            &theme,
            &translate,
            &services.language,
        )?;
        let main = build_container(
            app,
            window,
            left,
            StyleBuilder::new()
                .flex_1()
                .flex_row()
                .width_full()
                .min_height_0()
                .build(),
        )?;
        let sidebar = build_container(
            app,
            window,
            main,
            StyleBuilder::new()
                .flex_none()
                .flex_col()
                .width(SIDEBAR_WIDTH)
                .gap(ROW_GAP)
                .padding_sides(16.0, 8.0, 16.0, 0.0)
                .build(),
        )?;
        let body = build_container(
            app,
            window,
            main,
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .min_height_0()
                .relative()
                .overflow_hidden()
                .build(),
        )?;
        let viewport = NodeBuilder::boxed()
            .id(keys.control("viewport"))
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .width_full()
                    .relative()
                    .overflow_hidden()
                    .build(),
            )
            .build(tree(app, window), body)?;
        let content = NodeBuilder::boxed()
            .id(keys.control("content"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_none()
                    .width_full()
                    .padding(CONTENT_PADDING)
                    .gap(CATEGORY_GAP)
                    .build(),
            )
            .build(tree(app, window), viewport)?;
        let scroll = app
            .make_scroll(window, viewport, ScrollConfig::default())
            .ok();
        let scrollbar = ScrollBar::new(
            app,
            theme.clone(),
            keys.control("scrollbar"),
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        scrollbar.build(tree(app, window), body)?;
        scrollbar.attach(app, window, viewport);
        let footer = build_footer(app, window, &keys, left)?;
        let preview_host = build_preview(
            app,
            window,
            &keys,
            panel,
            &theme,
            &translate,
            &services.language,
            &selection,
            &entries,
        )?;

        let core = Rc::new(RefCell::new(Self {
            window,
            keys,
            services,
            catalogue,
            theme,
            translate,
            scope,
            panel,
            header,
            sidebar,
            footer,
            content,
            scroll,
            scrollbar,
            preview_host,
            selection,
            input_state,
            placeholder,
            selected,
            query: SmolStr::default(),
            filter: None,
            language,
            entries,
            built_rows: None,
            rows: Vec::new(),
            rows_by_node: HashMap::new(),
            chrome: None,
            preview: None,
            preview_selection: None,
            event: None,
            route: None,
            actions: Vec::new(),
            released: false,
        }));

        {
            let weak = Rc::downgrade(&core);
            let mut core = core.borrow_mut();
            core.actions = core.bind_actions(app, &weak)?;
            core.build_chrome(app, &weak)?;
            let entries = core.entries.clone();
            core.rebuild_rows(app, &entries)?;
            core.route = Some(register_route(&weak, &core.services));
            // The page opens with the caret in the field, so the reader narrows the
            // catalogue by typing rather than by finding the field first.
            let input = core.chrome.as_ref().map(|chrome| chrome.input.root);
            if let Some(input) = input {
                app.set_focus(window, Some(input));
            }
        }

        Ok(WidgetGallery { core })
    }

    /// Binds the page's own keys, scoped to its root node.
    ///
    /// The bindings only fire while the keyboard is inside the page's subtree; the pane
    /// behind the page keeps its own bindings. The returned actions are kept by the page so
    /// releasing it removes their handlers.
    fn bind_actions(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<Vec<Action>, EngineError> {
        let cancel = self.keys.action("cancel");
        let confirm = self.keys.action("confirm");
        let focus_search = self.keys.action("focus-search");
        let row = self.keys.action("row");

        {
            let handler = Weak::clone(weak);
            app.on_action(cancel.clone(), move |_ctx| {
                let Some(core) = handler.upgrade() else {
                    return;
                };
                let mut core = core.borrow_mut();
                core.emit(WidgetGalleryEvent::Cancel);
            });
            let handler = Weak::clone(weak);
            app.on_action(confirm.clone(), move |ctx| {
                let Some(core) = handler.upgrade() else {
                    return;
                };
                let selected = {
                    let core = core.borrow();
                    ctx.read(&core.selection).clone()
                };
                let Some(widget) = selected else {
                    return;
                };
                let mut core = core.borrow_mut();
                core.emit(WidgetGalleryEvent::Confirm(widget));
            });
            let handler = Weak::clone(weak);
            app.on_action(focus_search.clone(), move |ctx| {
                let Some(core) = handler.upgrade() else {
                    return;
                };
                let (window, input) = {
                    let core = core.borrow();
                    let Some(input) = core.chrome.as_ref().map(|chrome| chrome.input.root) else {
                        return;
                    };
                    (core.window, input)
                };
                ctx.app_mut().set_focus(window, Some(input));
            });
            let handler = Weak::clone(weak);
            app.on_action(row.clone(), move |ctx| {
                let Some(core) = handler.upgrade() else {
                    return;
                };
                let node = ctx.node();
                let (widget, window, input) = {
                    let core = core.borrow();
                    let widget = core.rows_by_node.get(&node).cloned();
                    let input = core.chrome.as_ref().map(|chrome| chrome.input.root);
                    (widget, core.window, input)
                };
                let Some(widget) = widget else {
                    return;
                };
                {
                    let mut core = core.borrow_mut();
                    core.select(ctx.app_mut(), widget);
                }
                // A pointer press chooses a row; it does not move the keyboard out of the
                // field, so what the reader types next still narrows the list.
                if let Some(input) = input {
                    ctx.app_mut().set_focus(window, Some(input));
                }
            });
        }

        let bindings = [
            (KeyCode::Escape, Modifiers::empty(), cancel.clone()),
            (KeyCode::Tab, Modifiers::empty(), focus_search.clone()),
            (KeyCode::Tab, Modifiers::SHIFT, focus_search.clone()),
        ];
        for (key, modifiers, action) in bindings {
            app.bind_key_scoped(
                self.window,
                self.panel,
                KeyBinding {
                    key,
                    modifiers,
                    action,
                },
            )?;
        }

        Ok(vec![cancel, confirm, focus_search, row])
    }

    /// Builds the language-dependent chrome: search field, category buttons, decisions.
    fn build_chrome(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();

        let header = self.header;
        let window = self.window;
        let search_box = NodeBuilder::boxed()
            .id(self.keys.control("search"))
            .style(StyleBuilder::new().flex_none().width(SEARCH_WIDTH).build())
            .build(tree(app, window), header)?;
        let spec = std::sync::Arc::new(InputSpec {
            name: self.keys.control("search_input"),
            placeholder: t_smol!(&translate, "widget_gallery.search_placeholder"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            size: base_ui::theme::components::ControlSize::Medium,
            submit: None,
        });
        let state = self.input_state;
        let placeholder = self.placeholder;
        let mut input = input::build(
            tree(app, window),
            search_box,
            spec,
            &theme,
            &state,
            &placeholder,
        );
        input.set_clearable(app, true);
        input.install(app);

        let sidebar = self.sidebar;
        let mut categories = Vec::with_capacity(WidgetCategory::ALL.len());
        for category in WidgetCategory::ALL {
            let key = self
                .keys
                .control(&format_smolstr!("category.{}", category_index(category)));
            let button = Button::new(app, theme.clone(), key)
                .ghost()
                .label(t_smol!(&translate, category.label()))
                .on_click({
                    let weak = weak.clone();
                    move |ctx| {
                        let window = ctx.window();
                        let Some(core) = weak.upgrade() else {
                            return;
                        };
                        let input = {
                            let mut core = core.borrow_mut();
                            core.toggle_filter(category);
                            core.chrome.as_ref().map(|chrome| chrome.input.root)
                        };
                        let _ = ctx.app_mut().request_repaint(window);
                        // Filtering is a press in the sidebar; the caret stays in the field
                        // so the reader can keep narrowing by typing.
                        if let Some(input) = input {
                            ctx.app_mut().set_focus(window, Some(input));
                        }
                    }
                });
            button.build(app, window, sidebar)?;
            categories.push((category, button));
        }

        let footer = self.footer;
        Separator::horizontal(theme.clone()).build(tree(app, window), footer)?;
        let footer_row = NodeBuilder::boxed()
            .id(self.keys.control("footer_row"))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .width_full()
                    .gap(8.0)
                    .padding_sides(12.0, 20.0, 16.0, 20.0)
                    .build(),
            )
            .build(tree(app, window), footer)?;
        let cancel = Button::new(app, theme.clone(), self.keys.control("cancel"))
            .secondary()
            .label(t_smol!(&translate, "common.cancel"))
            .on_click({
                let weak = weak.clone();
                move |_| {
                    let Some(core) = weak.upgrade() else {
                        return;
                    };
                    let mut core = core.borrow_mut();
                    core.emit(WidgetGalleryEvent::Cancel);
                }
            });
        cancel.build(app, window, footer_row)?;

        let confirm = Button::new(app, theme, self.keys.control("confirm"))
            .primary()
            .label(t_smol!(&translate, "common.confirm"))
            .disabled(app.read(&self.selection).is_none())
            .on_click({
                let weak = weak.clone();
                move |ctx| {
                    let Some(core) = weak.upgrade() else {
                        return;
                    };
                    let selected = {
                        let core = core.borrow();
                        ctx.read(&core.selection).clone()
                    };
                    let Some(widget) = selected else {
                        return;
                    };
                    let mut core = core.borrow_mut();
                    core.emit(WidgetGalleryEvent::Confirm(widget));
                }
            });
        confirm.build(app, window, footer_row)?;

        self.chrome = Some(Chrome {
            input,
            categories,
            cancel,
            confirm,
        });
        Ok(())
    }

    /// Replaces the chrome after a language switch.
    fn rebuild_chrome(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<(), EngineError> {
        let focused = app.focused(self.window);
        let had_search_focus = self
            .chrome
            .as_ref()
            .is_some_and(|chrome| focused == Some(chrome.input.root));
        if let Some(chrome) = self.chrome.take() {
            clear_children(app, self.window, self.header);
            clear_children(app, self.window, self.sidebar);
            clear_children(app, self.window, self.footer);
            chrome.input.release(app);
            chrome.cancel.release(app);
            chrome.confirm.release(app);
            for (_, button) in chrome.categories {
                button.release(app);
            }
        }
        self.build_chrome(app, weak)?;
        if had_search_focus
            && let Some(input) = self.chrome.as_ref().map(|chrome| chrome.input.root)
        {
            app.set_focus(self.window, Some(input));
        }
        Ok(())
    }

    /// Rebuilds the row list for the current query, filter and language.
    fn rebuild_rows(
        &mut self,
        app: &mut App,
        entries: &[WidgetDescriptor],
    ) -> Result<(), EngineError> {
        clear_children(app, self.window, self.content);
        self.rows.clear();
        self.rows_by_node.clear();

        let row_action = self.keys.action("row");
        let groups = groups(entries, self.query.as_str(), self.filter, &self.translate);
        let mut height = CONTENT_PADDING * 2.0;
        if groups.is_empty() {
            let empty = EmptyState::new(self.theme.clone(), self.keys.control("empty"))
                .text(t_smol!(&self.translate, "common.no_matches"));
            empty.build(tree(app, self.window), self.content)?;
        } else {
            for (index, (category, widgets)) in groups.iter().enumerate() {
                if index > 0 {
                    height += CATEGORY_GAP;
                }
                height += HEADING_HEIGHT + HEADING_GAP;
                height += widgets.len() as f32 * ROW_HEIGHT;
                height += widgets.len().saturating_sub(1) as f32 * ROW_GAP;

                let group = build_container(
                    app,
                    self.window,
                    self.content,
                    StyleBuilder::new()
                        .flex_col()
                        .width_full()
                        .gap(HEADING_GAP)
                        .build(),
                )?;
                let heading = NodeBuilder::boxed()
                    .id(self.keys.control(&format_smolstr!("heading.{index}")))
                    .style(
                        StyleBuilder::new()
                            .flex_row()
                            .items_center()
                            .width_full()
                            .height(HEADING_HEIGHT)
                            .build(),
                    )
                    .build(tree(app, self.window), group)?;
                static_label(
                    app,
                    self.window,
                    &self.theme,
                    heading,
                    t_smol!(&self.translate, category.label()),
                    TextStyle::new(12.0).with_weight(600),
                    Role::FgMuted,
                )?;
                let rows = build_container(
                    app,
                    self.window,
                    group,
                    StyleBuilder::new()
                        .flex_col()
                        .width_full()
                        .gap(ROW_GAP)
                        .build(),
                )?;
                for widget in widgets {
                    let row = build_row(
                        app,
                        self.window,
                        &self.keys,
                        &self.theme,
                        &self.translate,
                        rows,
                        widget,
                        &row_action,
                        self.selection,
                    )?;
                    self.rows_by_node.insert(row.node, row.widget.clone());
                    self.rows.push(row);
                }
            }
        }
        self.built_rows = Some(self.rows_built());
        self.update_scroll_content(app, height);
        Ok(())
    }

    fn rows_built(&self) -> RowsBuilt {
        RowsBuilt {
            query: self.query.clone(),
            filter: self.filter,
            language: self.language,
        }
    }

    /// Tells the scroll container how tall the catalogue became.
    fn update_scroll_content(&mut self, app: &mut App, height: f32) {
        if let Some(handle) = self.scroll
            && let Err(error) = app.set_scroll_content(&handle, Size::new(1.0, height))
        {
            log::warn!("the gallery could not size its catalogue: {error}");
        }
    }

    /// Puts the catalogue back to the top: what the reader was looking at was the list the
    /// old search left.
    fn reset_scroll(&mut self, app: &mut App) {
        if let Some(handle) = self.scroll
            && let Err(error) = app.scroll_to(&handle, Point::new(0.0, 0.0))
        {
            log::warn!("the gallery could not scroll its catalogue: {error}");
        }
    }

    /// Selects one widget of the catalogue.
    fn select(&mut self, app: &mut App, widget: WidgetId) {
        let current = { (*app.read(&self.selection)).clone() };
        if current.as_ref() == Some(&widget) {
            return;
        }
        self.selected = Some(widget.clone());
        app.write(&self.selection, Some(widget));
    }

    /// Narrows the catalogue to one category, or clears the filter when it is already set.
    fn toggle_filter(&mut self, category: WidgetCategory) {
        self.filter = if self.filter == Some(category) {
            None
        } else {
            Some(category)
        };
    }

    /// Records the page's one pending report.
    fn emit(&mut self, event: WidgetGalleryEvent) {
        self.event = Some(event);
    }

    /// The page's one frame hook: the part of the page that changes between frames.
    fn sync(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }

        self.drain_input_events(app);

        let language = *app.read(&self.services.language);
        if language != self.language {
            self.language = language;
            app.write(
                &self.placeholder,
                t_smol!(&self.translate, "widget_gallery.search_placeholder"),
            );
            self.rebuild_chrome(app, weak)?;
            let entries = self.entries.clone();
            self.rebuild_rows(app, &entries)?;
        }

        let text = app.read(&self.input_state).text().to_owned();
        let query = text.trim().to_lowercase();
        if query != self.query.as_str() {
            self.query = SmolStr::new(query);
            self.reset_scroll(app);
        }
        if self.built_rows.as_ref() != Some(&self.rows_built()) {
            let entries = self.entries.clone();
            self.rebuild_rows(app, &entries)?;
        }

        self.sync_chrome(app);
        self.sync_preview(app)?;
        Ok(())
    }

    /// Forwards this window's text, key and IME events to the search field.
    ///
    /// The kit input has no binding face: its owner drains the platform events and hands
    /// them to the field, which consumes the ones aimed at the focused input. Pointer,
    /// focus and size events stay in the queue — the engine already dispatched them.
    fn drain_input_events(&mut self, app: &mut App) {
        if !self.has_focus(app) {
            return;
        }
        let window = self.window;
        let events = app.drain_events_where(|event_window, event| {
            event_window == window
                && matches!(
                    event,
                    Event::Input(InputEvent::Text { .. } | InputEvent::Key { .. }) | Event::Ime(_)
                )
        });
        if events.is_empty() {
            return;
        }
        let search = self.chrome.as_ref().map(|chrome| chrome.input.root);
        let mut forward = Vec::with_capacity(events.len());
        for (_, event) in events {
            // Enter in the focused search field is the page's confirmation. The engine
            // spends Enter on the focused node's activation before scoped bindings are
            // consulted, so the page takes it from the event queue instead of from a
            // binding — the field's caret is in the page, and confirming is what Enter
            // means there.
            if let Event::Input(InputEvent::Key {
                code: KeyCode::Enter | KeyCode::NumpadEnter,
                state: ui_core::event::KeyState::Down,
                repeat: false,
                modifiers,
                ..
            }) = &event
                && modifiers.is_empty()
                && app.focused(self.window) == search
            {
                let selected = { (*app.read(&self.selection)).clone() };
                if let Some(widget) = selected {
                    self.emit(WidgetGalleryEvent::Confirm(widget));
                }
                continue;
            }
            forward.push(event);
        }
        if forward.is_empty() {
            return;
        }
        if let Some(chrome) = self.chrome.as_ref() {
            for event in &forward {
                let _ = chrome.input.dispatch(app, event);
            }
        }
    }

    /// Whether the keyboard is inside the page's subtree.
    fn has_focus(&self, app: &App) -> bool {
        let Some(focused) = app.focused(self.window) else {
            return false;
        };
        let Some(window) = app.window(self.window) else {
            return false;
        };
        let tree = window.tree();
        let mut cursor = Some(focused);
        while let Some(node) = cursor {
            if node == self.panel {
                return true;
            }
            cursor = tree.get(node).ok().and_then(|node| node.parent());
        }
        false
    }

    /// Keeps the components that only paint state in step with the page's state.
    fn sync_chrome(&mut self, app: &mut App) {
        let no_selection = app.read(&self.selection).is_none();
        let filter = self.filter;
        let scrollbar = self.scrollbar.clone();
        if let Some(chrome) = self.chrome.as_mut() {
            let _ = chrome.input.sync(app);
            let _ = chrome.confirm.set_disabled(app, no_selection);
            for (category, button) in &chrome.categories {
                let _ = button.set_selected(app, filter == Some(*category));
            }
        }
        let _ = scrollbar.sync(app);
    }

    /// Mounts the selected widget as the preview, releasing whatever was previewed before.
    fn sync_preview(&mut self, app: &mut App) -> Result<(), EngineError> {
        let selection = app.read(&self.selection).clone();
        if selection == self.preview_selection {
            return Ok(());
        }
        if let Some(previous) = self.preview.take() {
            previous.release(app);
        }
        self.preview_selection = selection.clone();
        let Some(widget) = selection else {
            return Ok(());
        };
        match self
            .catalogue
            .mount(&widget, app, self.window, self.preview_host, &self.services)
        {
            Ok(preview) => {
                self.preview = Some(preview);
                Ok(())
            }
            Err(error) => {
                log::error!("the gallery could not preview `{widget}`: {error}");
                Ok(())
            }
        }
    }

    /// Releases everything the page owns.
    fn release(&mut self, app: &mut App) -> usize {
        if self.released {
            return 0;
        }
        self.released = true;

        let mut released = 0;
        if let Some(preview) = self.preview.take() {
            released += preview.release(app);
        }
        released += self.scrollbar.release(app);
        if let Some(chrome) = self.chrome.take() {
            chrome.input.release(app);
            chrome.cancel.release(app);
            chrome.confirm.release(app);
            for (_, button) in chrome.categories {
                button.release(app);
            }
        }
        let _ = app.remove_node(self.window, self.panel);
        released += app.release_scope(self.scope);
        released
    }
}

/// Registers the page's frame route on the application's one router.
///
/// The route holds a weak reference: a page that is dropped without being released stops
/// being synchronized instead of keeping itself alive through the router.
fn register_route(
    weak: &Weak<RefCell<GalleryCore>>,
    services: &Rc<WidgetServices>,
) -> widget::frame::RouteId {
    let weak = weak.clone();
    services.frame_router.register(move |app| {
        let Some(core) = weak.upgrade() else {
            return Ok(());
        };
        core.borrow_mut().sync(app, &weak)
    })
}

/// A mounted page.
///
/// The host embeds [`WidgetGallery::panel`] where the page belongs, reads
/// [`WidgetGallery::take_event`] after frames, and calls [`WidgetGallery::release`] when
/// the page goes away. The page synchronizes itself: it registers a route on the
/// application's frame router when it is mounted, and removes it when it is released.
#[derive(Clone)]
pub struct WidgetGallery {
    core: Rc<RefCell<GalleryCore>>,
}

impl WidgetGallery {
    /// Builds the gallery under `parent` over the application's services and catalogue.
    ///
    /// `current` is the widget the pane has now, which is selected when the catalogue
    /// still offers it. The page registers itself on the services' frame router and takes
    /// the keyboard into its search field.
    ///
    /// # Errors
    ///
    /// The engine's node errors when the page cannot be built under `parent` (a window that
    /// is gone, a parent that is not in its tree).
    pub fn mount(
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        services: Rc<WidgetServices>,
        catalogue: Rc<dyn GalleryCatalogue>,
        current: Option<WidgetId>,
    ) -> Result<Self, EngineError> {
        GalleryCore::mount(app, window, parent, services, catalogue, current)
    }

    /// The page's root node: where the host embeds it.
    #[must_use]
    pub fn panel(&self) -> NodeId {
        self.core.borrow().panel
    }

    /// Takes the one report the page made, if any.
    pub fn take_event(&self) -> Option<WidgetGalleryEvent> {
        self.core.borrow_mut().event.take()
    }

    /// The widget a confirmation would carry.
    #[must_use]
    pub fn selection(&self) -> Option<WidgetId> {
        self.core.borrow().selected.clone()
    }

    /// Whether the page has been released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.core.borrow().released
    }

    /// Releases the preview, the kit components, the keyboard bindings, the nodes and the
    /// page's signal scope. Idempotent.
    pub fn release(&self, app: &mut App) -> usize {
        let mut core = self.core.borrow_mut();
        if core.released {
            return 0;
        }
        if let Some(route) = core.route.take() {
            core.services.frame_router.unregister(route);
        }
        let actions = std::mem::take(&mut core.actions);
        for action in &actions {
            app.off_action(action);
        }
        core.release(app)
    }
}

impl std::fmt::Debug for WidgetGallery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let core = self.core.borrow();
        formatter
            .debug_struct("WidgetGallery")
            .field("window", &core.window)
            .field("panel", &core.panel)
            .field("selected", &core.selected)
            .field("released", &core.released)
            .finish_non_exhaustive()
    }
}

// ── node builders ───────────────────────────────────────────────────────────────────────

/// The window's node tree.
fn tree(app: &mut App, window: WindowId) -> &mut NodeTree {
    app.window_mut(window)
        .expect("the gallery's window exists")
        .tree_mut()
}

/// Builds a container under `parent` with `style`.
fn build_container(
    app: &mut App,
    window: WindowId,
    parent: NodeId,
    style: ui_core::paint::style::Style,
) -> Result<NodeId, EngineError> {
    NodeBuilder::boxed()
        .style(style)
        .build(tree(app, window), parent)
}

/// Removes every child of `node`, leaving the node itself in place.
///
/// Removing a node also clears the scoped keyboard bindings and the action declarations it
/// carried, which is why the page rebuilds by clearing and re-adding rather than by keeping
/// dead nodes around.
fn clear_children(app: &mut App, window: WindowId, node: NodeId) {
    let children = app
        .window(window)
        .and_then(|window| window.tree().get(node).ok())
        .map(|node| node.children().to_vec())
        .unwrap_or_default();
    for child in children {
        let _ = app.remove_node(window, child);
    }
}

/// The page's root: the surface colour, a row of the catalogue column and the preview pane.
fn build_panel(
    app: &mut App,
    window: WindowId,
    keys: &GalleryKeys,
    theme: &ThemeHandle,
    parent: NodeId,
) -> Result<NodeId, EngineError> {
    let theme_style = theme.clone();
    NodeBuilder::boxed()
        .id(keys.control("panel"))
        .style(
            StyleBuilder::new()
                .flex_row()
                .size_full()
                .min_width_0()
                .min_height_0()
                .build(),
        )
        .bind_style(move |cx| StylePatch {
            background: Some(theme_style.role(cx, Role::SurfaceCanvas)),
            ..StylePatch::default()
        })
        .build(tree(app, window), parent)
}

/// The page's header: what it is for and the field it is searched with.
///
/// The search field itself is built with the rest of the language-dependent chrome; the
/// header node is stable and a language rebuild replaces its children.
fn build_header(
    app: &mut App,
    window: WindowId,
    keys: &GalleryKeys,
    parent: NodeId,
    theme: &ThemeHandle,
    translate: &Translate,
    language: &Signal<LanguageCode>,
) -> Result<NodeId, EngineError> {
    let header = NodeBuilder::boxed()
        .id(keys.control("header"))
        .style(
            StyleBuilder::new()
                .flex_none()
                .flex_row()
                .items_center()
                .width_full()
                .gap(16.0)
                .padding_sides(20.0, 20.0, 12.0, 20.0)
                .build(),
        )
        .build(tree(app, window), parent)?;
    let titles = build_container(
        app,
        window,
        header,
        StyleBuilder::new()
            .flex_1()
            .min_width_0()
            .flex_col()
            .gap(2.0)
            .build(),
    )?;
    bound_label(
        app,
        window,
        theme,
        titles,
        "widget_gallery.title",
        TextStyle::new(18.0).with_weight(700),
        Role::FgDefault,
        translate,
        *language,
    )?;
    bound_label(
        app,
        window,
        theme,
        titles,
        "widget_gallery.subtitle",
        TextStyle::new(12.0),
        Role::FgMuted,
        translate,
        *language,
    )?;
    Ok(header)
}

/// The footer container; its separator and buttons are built with the chrome.
fn build_footer(
    app: &mut App,
    window: WindowId,
    keys: &GalleryKeys,
    parent: NodeId,
) -> Result<NodeId, EngineError> {
    NodeBuilder::boxed()
        .id(keys.control("footer"))
        .style(
            StyleBuilder::new()
                .flex_none()
                .flex_col()
                .width_full()
                .build(),
        )
        .build(tree(app, window), parent)
}

/// The preview pane: the selected widget's name and the host its surface is mounted in.
#[allow(clippy::too_many_arguments)]
fn build_preview(
    app: &mut App,
    window: WindowId,
    keys: &GalleryKeys,
    parent: NodeId,
    theme: &ThemeHandle,
    translate: &Translate,
    language: &Signal<LanguageCode>,
    selection: &Signal<Option<WidgetId>>,
    entries: &[WidgetDescriptor],
) -> Result<NodeId, EngineError> {
    let theme_style = theme.clone();
    let column = NodeBuilder::boxed()
        .id(keys.control("preview"))
        .style(
            StyleBuilder::new()
                .flex_none()
                .flex_col()
                .width(PREVIEW_WIDTH)
                .min_height_0()
                .gap(8.0)
                .padding(16.0)
                .build(),
        )
        .bind_style(move |cx| StylePatch {
            background: Some(theme_style.role(cx, Role::SurfaceRaised)),
            ..StylePatch::default()
        })
        .build(tree(app, window), parent)?;

    let names: Vec<(WidgetId, &'static str)> = entries
        .iter()
        .map(|entry| (entry.id().clone(), entry.display_name()))
        .collect();
    let translate_style = translate.clone();
    let selection_text = *selection;
    let language_text = *language;
    let preview_theme = theme.clone();
    NodeBuilder::text("", TextStyle::new(13.0).with_weight(600))
        .id(keys.control("preview_title"))
        .bind_text(move |cx| {
            let _language = cx.read(&language_text);
            let selected = cx.read(&selection_text).clone();
            selected
                .and_then(|selected| {
                    names
                        .iter()
                        .find(|(id, _)| *id == selected)
                        .map(|(_, key)| t_smol!(&translate_style, key))
                })
                .unwrap_or_default()
        })
        .bind_style(move |cx| {
            let visible = cx.read(&selection_text).is_some();
            StylePatch {
                foreground: Some(preview_theme.role(cx, Role::FgDefault)),
                visible: Some(visible),
                ..StylePatch::default()
            }
        })
        .build(tree(app, window), column)?;
    let hint_translate = translate.clone();
    let hint_theme = theme.clone();
    let hint_selection = *selection;
    let hint_language = *language;
    NodeBuilder::text(
        t_smol!(translate, "widget_gallery.subtitle"),
        TextStyle::new(12.0),
    )
    .id(keys.control("preview_hint"))
    .bind_text(move |cx| {
        let _language = cx.read(&hint_language);
        t_smol!(&hint_translate, "widget_gallery.subtitle")
    })
    .bind_style(move |cx| {
        let visible = cx.read(&hint_selection).is_none();
        StylePatch {
            foreground: Some(hint_theme.role(cx, Role::FgMuted)),
            visible: Some(visible),
            ..StylePatch::default()
        }
    })
    .build(tree(app, window), column)?;
    let host = NodeBuilder::boxed()
        .id(keys.control("preview_host"))
        .style(
            StyleBuilder::new()
                .flex_1()
                .width_full()
                .min_height_0()
                .relative()
                .build(),
        )
        .build(tree(app, window), column)?;
    Ok(host)
}

/// One catalogue row: icon, name, description, and whether the row is the selection.
#[allow(clippy::too_many_arguments)]
fn build_row(
    app: &mut App,
    window: WindowId,
    keys: &GalleryKeys,
    theme: &ThemeHandle,
    translate: &Translate,
    parent: NodeId,
    descriptor: &WidgetDescriptor,
    row_action: &Action,
    selection: Signal<Option<WidgetId>>,
) -> Result<Row, EngineError> {
    let widget = descriptor.id().clone();
    let name = t_smol!(translate, descriptor.display_name());
    let description = descriptor
        .description()
        .map(|key| t_smol!(translate, key))
        .filter(|description| !description.is_empty());
    let icon = widget_icon::icon_for(descriptor);

    let selected_style = selection;
    let selected_row = widget.clone();
    let row_theme = theme.clone();
    let row = NodeBuilder::boxed()
        .id(keys.control(&format_smolstr!("row.{}", widget.as_str())))
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .width_full()
                .height(ROW_HEIGHT)
                .padding_x(12.0)
                .gap(12.0)
                .build(),
        )
        .paint(PaintBuilder::new().rounded(8.0).build())
        .on_click(row_action.clone())
        .a11y(A11yOverride {
            role: Some(A11yRole::Button),
            label: Some(name.clone()),
            ..A11yOverride::default()
        })
        .bind_style(move |cx| {
            let selected = { (*cx.read(&selected_style)).clone().as_ref() == Some(&selected_row) };
            let line = if selected {
                row_theme.role(cx, Role::BrandBase)
            } else {
                row_theme.role(cx, Role::LineDivider)
            };
            StylePatch {
                background: Some(if selected {
                    row_theme.role(cx, Role::SurfaceControl)
                } else {
                    Color::TRANSPARENT
                }),
                border: Some(Border {
                    width: 1.0,
                    color: line,
                }),
                ..StylePatch::default()
            }
        })
        .build(tree(app, window), parent)?;

    let icon = IconSpec::new(icon).with_size(ICON_SIZE).build_themed(
        app,
        window,
        row,
        theme,
        Role::FgMuted,
    );
    if let Err(error) = icon {
        log::warn!("the gallery could not draw a widget's icon: {error}");
    }

    let text_column = build_container(
        app,
        window,
        row,
        StyleBuilder::new()
            .flex_1()
            .min_width_0()
            .flex_col()
            .gap(2.0)
            .build(),
    )?;
    static_label(
        app,
        window,
        theme,
        text_column,
        name,
        TextStyle::new(13.0).with_weight(500),
        Role::FgDefault,
    )?;
    if let Some(description) = description {
        static_label(
            app,
            window,
            theme,
            text_column,
            description,
            TextStyle::new(11.0),
            Role::FgMuted,
        )?;
    }

    let mark_selected = selection;
    let mark_row = widget.clone();
    let mark_theme = theme.clone();
    NodeBuilder::boxed()
        .id(keys.control(&format_smolstr!("mark.{}", widget.as_str())))
        .style(
            StyleBuilder::new()
                .flex_none()
                .width(MARK_SIZE)
                .height(MARK_SIZE)
                .build(),
        )
        .paint(PaintBuilder::new().rounded(MARK_SIZE / 2.0).build())
        .bind_style(move |cx| {
            let selected = { (*cx.read(&mark_selected)).clone().as_ref() == Some(&mark_row) };
            let color = if selected {
                mark_theme.role(cx, Role::BrandBase)
            } else {
                mark_theme.role(cx, Role::LineBorder)
            };
            StylePatch {
                background: Some(if selected {
                    mark_theme.role(cx, Role::BrandBase)
                } else {
                    Color::TRANSPARENT
                }),
                border: Some(Border { width: 1.0, color }),
                ..StylePatch::default()
            }
        })
        .build(tree(app, window), row)?;

    Ok(Row { node: row, widget })
}

/// A text leaf whose colour follows the theme.
fn static_label(
    app: &mut App,
    window: WindowId,
    theme: &ThemeHandle,
    parent: NodeId,
    text: SmolStr,
    style: TextStyle,
    role: Role,
) -> Result<NodeId, EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(text, style)
        .bind_style(move |cx| StylePatch {
            foreground: Some(theme.role(cx, role)),
            ..StylePatch::default()
        })
        .build(tree(app, window), parent)
}

/// A text leaf whose copy is a catalogue key resolved on every draw.
#[allow(clippy::too_many_arguments)]
fn bound_label(
    app: &mut App,
    window: WindowId,
    theme: &ThemeHandle,
    parent: NodeId,
    key: &'static str,
    style: TextStyle,
    role: Role,
    translate: &Translate,
    language: Signal<LanguageCode>,
) -> Result<NodeId, EngineError> {
    let initial = t_smol!(translate, key);
    let theme = theme.clone();
    let translate = translate.clone();
    NodeBuilder::text(initial, style)
        .bind_text(move |cx| {
            let _language = cx.read(&language);
            t_smol!(&translate, key)
        })
        .bind_style(move |cx| StylePatch {
            foreground: Some(theme.role(cx, role)),
            ..StylePatch::default()
        })
        .build(tree(app, window), parent)
}

/// The groups the search and the filter leave, in [`WidgetCategory::ALL`] order.
fn groups<'a>(
    entries: &'a [WidgetDescriptor],
    query: &str,
    filter: Option<WidgetCategory>,
    translate: &Translate,
) -> Vec<(WidgetCategory, Vec<&'a WidgetDescriptor>)> {
    WidgetCategory::ALL
        .into_iter()
        .filter(|category| filter.is_none_or(|filter| filter == *category))
        .filter_map(|category| {
            let widgets = entries
                .iter()
                .filter(|entry| entry.category() == category && matches(entry, query, translate))
                .collect::<Vec<_>>();
            (!widgets.is_empty()).then_some((category, widgets))
        })
        .collect()
}

/// Whether one widget is named by the search.
///
/// The name and the description are both the localized ones, because both are what the
/// reader sees. They are compared case-insensitively, and a search of nothing names
/// everything.
fn matches(descriptor: &WidgetDescriptor, query: &str, translate: &Translate) -> bool {
    if query.is_empty() {
        return true;
    }
    t_smol!(translate, descriptor.display_name())
        .to_lowercase()
        .contains(query)
        || descriptor
            .description()
            .is_some_and(|key| t_smol!(translate, key).to_lowercase().contains(query))
}

/// The position of a category in the page's fixed order; part of a component key.
fn category_index(category: WidgetCategory) -> usize {
    WidgetCategory::ALL
        .iter()
        .position(|candidate| *candidate == category)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    // The page is driven through the framework's headless host, the same host a widget's
    // tests use: one window, the product catalogue, one composition root, and the frame
    // router ticked before every frame.

    use std::{rc::Rc, time::Instant};

    use app_config::widget::WidgetConfig;
    use base_ui::style_builder::StyleBuilder;
    use i18n::{language::codes::LanguageCode, translate::Translate};
    use smol_str::SmolStr;
    use ui_core::{
        error::EngineError,
        event::{Event, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect},
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{NodeKind, NodeTree},
        },
        text::TextStyle,
    };
    use widget::{
        any::AnyWidget,
        category::WidgetCategory,
        config::WidgetConfigChange,
        cx::WidgetCx,
        descriptor::WidgetDescriptor,
        identity::{WidgetId, WidgetInstanceId},
        status::{WidgetAction, WidgetStatus},
        test_support::WidgetTestApp,
        view::{ReloadHandle, WidgetView},
    };

    use super::{WidgetGallery, WidgetGalleryEvent};
    use crate::catalogue::{GalleryCatalogue, GalleryError};

    // ── the catalogue the page is tested over ──────────────────────────────────────────

    macro_rules! fake_widget {
        ($widget:ident, $id:literal, $name:literal, $description:literal, $icon:literal,
         $category:ident, $marker:literal) => {
            struct $widget;

            impl WidgetView for $widget {
                type Config = ();

                const ID: WidgetId = WidgetId::new_static($id);
                const DISPLAY_NAME: &'static str = $name;
                const CATEGORY: WidgetCategory = WidgetCategory::$category;
                const DESCRIPTION: Option<&'static str> = Some($description);
                const ICON: Option<&'static str> = Some($icon);

                fn new(
                    _instance_id: WidgetInstanceId,
                    _config: Self::Config,
                    _status: WidgetStatus,
                    _reload: ReloadHandle,
                    _config_change: WidgetConfigChange,
                    _recovery: WidgetAction,
                    _cx: &mut WidgetCx<'_>,
                ) -> Result<Self, EngineError> {
                    Ok(Self)
                }

                fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

                fn has_content(&self) -> bool {
                    true
                }

                fn body(
                    &mut self,
                    parent: NodeId,
                    cx: &mut WidgetCx<'_>,
                ) -> Result<(), EngineError> {
                    NodeBuilder::text($marker, TextStyle::new(12.0))
                        .id($marker)
                        .build(cx.tree_mut(), parent)?;
                    Ok(())
                }
            }
        };
    }

    fake_widget!(
        CandlestickFake,
        "kytrix.test.candlestick",
        "widget.names.candlestick",
        "widget.descriptions.candlestick",
        "widgets/candlestick.png",
        Market,
        "TEST_PREVIEW_CANDLESTICK"
    );
    fake_widget!(
        MarketTradeFake,
        "kytrix.test.market_trade",
        "widget.names.market_trade",
        "widget.descriptions.market_trade",
        "widgets/market_trade.png",
        Market,
        "TEST_PREVIEW_MARKET_TRADE"
    );
    fake_widget!(
        FinancialIndexFake,
        "kytrix.test.financial_index",
        "widget.names.financial_index",
        "widget.descriptions.financial_index",
        "widgets/indices.png",
        Statistics,
        "TEST_PREVIEW_FINANCIAL_INDEX"
    );
    fake_widget!(
        NewsFake,
        "kytrix.test.news",
        "widget.names.news",
        "widget.descriptions.news",
        "widgets/news.png",
        Monitoring,
        "TEST_PREVIEW_NEWS"
    );

    /// The widget a pane falls back to: registered, with metadata, but never offered.
    struct HiddenFake;

    impl WidgetView for HiddenFake {
        type Config = ();

        const ID: WidgetId = WidgetId::new_static("kytrix.test.hidden");
        const DISPLAY_NAME: &'static str = "widget.names.empty";
        const CATEGORY: WidgetCategory = WidgetCategory::Market;
        const SELECTABLE: bool = false;

        fn new(
            _instance_id: WidgetInstanceId,
            _config: Self::Config,
            _status: WidgetStatus,
            _reload: ReloadHandle,
            _config_change: WidgetConfigChange,
            _recovery: WidgetAction,
            _cx: &mut WidgetCx<'_>,
        ) -> Result<Self, EngineError> {
            Ok(Self)
        }

        fn load(&mut self, _cx: &mut WidgetCx<'_>) {}

        fn has_content(&self) -> bool {
            true
        }

        fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
            NodeBuilder::text("TEST_PREVIEW_HIDDEN", TextStyle::new(12.0))
                .id("TEST_PREVIEW_HIDDEN")
                .build(cx.tree_mut(), parent)?;
            Ok(())
        }
    }

    fn descriptor_of<W: WidgetView>() -> WidgetDescriptor {
        let mut descriptor = WidgetDescriptor::new(
            W::ID,
            W::DISPLAY_NAME,
            W::CATEGORY,
            <W::Config as WidgetConfig>::SCHEMA_VERSION,
        )
        .with_selectable(W::SELECTABLE);
        if let Some(description) = W::DESCRIPTION {
            descriptor = descriptor.with_description(description);
        }
        if let Some(icon) = W::ICON {
            descriptor = descriptor.with_icon(icon);
        }
        descriptor
    }

    /// The test catalogue: the same list every test, in registry order.
    struct TestCatalogue {
        entries: Vec<WidgetDescriptor>,
    }

    impl TestCatalogue {
        fn new() -> Self {
            Self {
                entries: vec![
                    descriptor_of::<CandlestickFake>(),
                    descriptor_of::<MarketTradeFake>(),
                    descriptor_of::<FinancialIndexFake>(),
                    descriptor_of::<NewsFake>(),
                    descriptor_of::<HiddenFake>(),
                ],
            }
        }
    }

    impl GalleryCatalogue for TestCatalogue {
        fn descriptors(&self) -> Vec<WidgetDescriptor> {
            self.entries.clone()
        }

        fn mount(
            &self,
            id: &WidgetId,
            app: &mut ui_core::app::App,
            window: ui_core::window::WindowId,
            parent: NodeId,
            services: &Rc<widget::services::WidgetServices>,
        ) -> Result<AnyWidget, GalleryError> {
            let instance = WidgetInstanceId::new();
            let change = WidgetConfigChange::default();
            let mounted = match id.as_str() {
                "kytrix.test.candlestick" => AnyWidget::mount::<CandlestickFake>(
                    app,
                    window,
                    parent,
                    services,
                    instance,
                    (),
                    change,
                ),
                "kytrix.test.market_trade" => AnyWidget::mount::<MarketTradeFake>(
                    app,
                    window,
                    parent,
                    services,
                    instance,
                    (),
                    change,
                ),
                "kytrix.test.financial_index" => AnyWidget::mount::<FinancialIndexFake>(
                    app,
                    window,
                    parent,
                    services,
                    instance,
                    (),
                    change,
                ),
                "kytrix.test.news" => AnyWidget::mount::<NewsFake>(
                    app,
                    window,
                    parent,
                    services,
                    instance,
                    (),
                    change,
                ),
                "kytrix.test.hidden" => AnyWidget::mount::<HiddenFake>(
                    app,
                    window,
                    parent,
                    services,
                    instance,
                    (),
                    change,
                ),
                other => return Err(GalleryError::NotRegistered(SmolStr::new(other))),
            };
            mounted.map_err(GalleryError::Engine)
        }
    }

    // ── harness ───────────────────────────────────────────────────────────────────────

    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::global::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn widget(id: &str) -> WidgetId {
        WidgetId::parse(id).expect("the test widget id is valid")
    }

    fn find_debug(tree: &NodeTree, parent: NodeId, debug: &str) -> Option<NodeId> {
        let mut stack = vec![parent];
        while let Some(current) = stack.pop() {
            if tree.debug_id(current) == Some(debug) {
                return Some(current);
            }
            if let Ok(node) = tree.get(current) {
                stack.extend(node.children().iter().rev().copied());
            }
        }
        None
    }

    fn bounds_of(app: &WidgetTestApp, node: NodeId) -> Rect {
        app.app()
            .window(app.window())
            .expect("the host window")
            .tree()
            .get(node)
            .expect("the node is alive")
            .layout()
            .bounds
    }

    fn click_node(app: &mut WidgetTestApp, node: NodeId) {
        let bounds = bounds_of(app, node);
        let point = Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        app.dispatch_event(Event::Input(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        )));
        app.dispatch_event(Event::Input(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        )));
    }

    /// One page over the test catalogue, hosted in one headless application.
    struct Harness {
        app: WidgetTestApp,
        gallery: WidgetGallery,
    }

    impl Harness {
        fn mount(current: Option<&str>) -> Self {
            Self::mount_with(current, TestCatalogue::new())
        }

        fn mount_with(current: Option<&str>, catalogue: TestCatalogue) -> Self {
            let mut app = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
            let window = app.window();
            let parent = app.root();
            let services = Rc::clone(app.services_rc());
            let catalogue: Rc<dyn GalleryCatalogue> = Rc::new(catalogue);
            let gallery = WidgetGallery::mount(
                app.app_mut(),
                window,
                parent,
                services,
                catalogue,
                current.map(widget),
            )
            .expect("the gallery mounts");
            let mut harness = Self { app, gallery };
            harness.settle();
            harness
        }

        /// Two frames: the page's route runs in the first, a preview mounted during it
        /// registers its route for the second.
        fn settle(&mut self) {
            self.app.drain();
            self.app.drain();
        }

        fn keys(&self) -> SmolStr {
            self.gallery.core.borrow().keys.prefix.clone()
        }

        fn debug(&self, suffix: &str) -> SmolStr {
            smol_str::format_smolstr!("{}.{suffix}", self.keys())
        }

        fn find(&self, suffix: &str) -> Option<NodeId> {
            find_debug(self.app.tree(), self.gallery.panel(), &self.debug(suffix))
        }

        fn must_find(&self, suffix: &str) -> NodeId {
            self.find(suffix)
                .unwrap_or_else(|| panic!("the page draws `{suffix}`"))
        }

        fn rows(&self) -> Vec<String> {
            self.gallery
                .core
                .borrow()
                .rows
                .iter()
                .map(|row| row.widget.as_str().to_owned())
                .collect()
        }

        fn selection(&self) -> Option<String> {
            self.gallery
                .selection()
                .map(|widget| widget.as_str().to_owned())
        }

        fn click(&mut self, suffix: &str) {
            let node = self.must_find(suffix);
            click_node(&mut self.app, node);
            self.settle();
        }

        fn click_node(&mut self, node: NodeId) {
            click_node(&mut self.app, node);
            self.settle();
        }

        fn click_row(&mut self, id: &str) {
            let suffix = smol_str::format_smolstr!("row.{id}");
            self.click(suffix.as_str());
        }

        fn category_button(&self, index: usize) -> NodeId {
            let core = self.gallery.core.borrow();
            let chrome = core.chrome.as_ref().expect("chrome");
            chrome.categories[index]
                .1
                .node()
                .expect("the category button is built")
        }

        fn confirm_button(&self) -> NodeId {
            let core = self.gallery.core.borrow();
            core.chrome
                .as_ref()
                .expect("chrome")
                .confirm
                .node()
                .expect("the confirm control is built")
        }

        fn cancel_button(&self) -> NodeId {
            let core = self.gallery.core.borrow();
            core.chrome
                .as_ref()
                .expect("chrome")
                .cancel
                .node()
                .expect("the cancel control is built")
        }

        fn input_root(&self) -> NodeId {
            self.gallery
                .core
                .borrow()
                .chrome
                .as_ref()
                .expect("chrome")
                .input
                .root
        }

        fn focus_search(&mut self) {
            let input = self.input_root();
            let window = self.app.window();
            self.app.app_mut().set_focus(window, Some(input));
        }

        fn press(&mut self, key: KeyCode, modifiers: Modifiers) {
            self.app
                .dispatch_event(Event::Input(InputEvent::key_down(key, modifiers)));
            self.settle();
        }

        fn type_text(&mut self, text: &str) {
            self.app.dispatch_event(Event::Input(InputEvent::Text {
                text: SmolStr::new(text),
                timestamp: Instant::now(),
            }));
            self.settle();
        }

        fn placeholder(&self) -> String {
            let signal = self.gallery.core.borrow().placeholder;
            (*self.app.app().read(&signal)).to_string()
        }

        fn query(&self) -> String {
            self.gallery.core.borrow().query.to_string()
        }

        fn preview(&self, marker: &str) -> bool {
            let host = self.gallery.core.borrow().preview_host;
            self.app.find_by_debug_prefix(host, marker).is_some()
        }

        fn take_event(&self) -> Option<WidgetGalleryEvent> {
            self.gallery.take_event()
        }

        /// Every text node under `parent`, in tree order.
        fn texts_under(&self, parent: NodeId) -> Vec<String> {
            let tree = self.app.tree();
            let mut texts = Vec::new();
            let mut stack = vec![parent];
            while let Some(node) = stack.pop() {
                if let Ok(current) = tree.get(node) {
                    if let NodeKind::Text(content) = current.kind() {
                        texts.push(content.text.to_string());
                    }
                    stack.extend(current.children().iter().rev().copied());
                }
            }
            texts
        }

        fn has_text_under(&self, parent: NodeId, needle: &str) -> bool {
            self.texts_under(parent)
                .iter()
                .any(|text| text.contains(needle))
        }

        /// Whether the row's subtree carries the kit icon (an image node).
        fn row_has_icon(&self, id: &str) -> bool {
            let row = self.must_find(&smol_str::format_smolstr!("row.{id}"));
            let tree = self.app.tree();
            let mut stack = vec![row];
            while let Some(node) = stack.pop() {
                if let Ok(current) = tree.get(node) {
                    if matches!(current.kind(), NodeKind::Image(_)) {
                        return true;
                    }
                    stack.extend(current.children().iter().copied());
                }
            }
            false
        }
    }

    // ── listing, grouping, opening state ──────────────────────────────────────────────

    /// The page offers each widget the catalogue says may be chosen, once, and nothing else.
    #[test]
    fn the_gallery_should_list_every_widget_that_may_be_chosen_once() {
        let harness = Harness::mount(None);

        assert_eq!(
            harness.rows(),
            [
                "kytrix.test.candlestick",
                "kytrix.test.market_trade",
                "kytrix.test.financial_index",
                "kytrix.test.news",
            ],
            "the catalogue's selectable entries are listed in registry order"
        );
        for id in [
            "kytrix.test.candlestick",
            "kytrix.test.market_trade",
            "kytrix.test.financial_index",
            "kytrix.test.news",
        ] {
            assert!(
                harness
                    .find(&smol_str::format_smolstr!("row.{id}"))
                    .is_some(),
                "{id} has a row"
            );
        }
        assert!(
            harness.find("row.kytrix.test.hidden").is_none(),
            "the pane's fallback is not an answer this page offers"
        );
    }

    /// A pane showing a widget opens the gallery on that widget, which is the answer the
    /// pane already has — and the page previews it.
    #[test]
    fn a_pane_with_a_widget_should_open_on_it() {
        let harness = Harness::mount(Some("kytrix.test.news"));

        assert_eq!(harness.selection().as_deref(), Some("kytrix.test.news"));
        assert!(
            harness.preview("TEST_PREVIEW_NEWS"),
            "the first frame mounts the widget the pane already shows"
        );
        assert!(
            harness.app.router().len() >= 2,
            "the page and its preview are both routed"
        );
    }

    /// The widget a pane falls back to is not an answer this page offers, so a pane showing
    /// it opens with nothing selected and nothing previewed.
    #[test]
    fn a_pane_showing_the_fallback_should_open_with_nothing_selected() {
        let harness = Harness::mount(Some("kytrix.test.hidden"));

        assert_eq!(harness.selection(), None);
        assert!(!harness.preview("TEST_PREVIEW_"));
        assert_eq!(harness.app.router().len(), 1, "only the page is routed");
    }

    /// The catalogue is drawn in the categories of `WidgetCategory::ALL`: a category the
    /// page has something to list gets a heading, its rows follow it, and a category the
    /// page has nothing for is not drawn.
    #[test]
    fn the_gallery_should_group_widgets_under_their_category() {
        let harness = Harness::mount(None);

        let market = harness.must_find("heading.0");
        let statistics = harness.must_find("heading.1");
        let monitoring = harness.must_find("heading.2");
        let tops = [
            bounds_of(&harness.app, market).origin.y,
            bounds_of(&harness.app, statistics).origin.y,
            bounds_of(&harness.app, monitoring).origin.y,
        ];
        assert!(
            tops[0] < tops[1] && tops[1] < tops[2],
            "the headings follow WidgetCategory::ALL: {tops:?}"
        );
        for (heading, label) in [
            (market, "Market"),
            (statistics, "Statistics"),
            (monitoring, "Monitoring"),
        ] {
            assert!(
                harness.has_text_under(heading, label),
                "the heading says {label}"
            );
        }
        // Market has two rows, Statistics one and Monitoring one; the page lists no
        // fourth category.
        assert_eq!(harness.rows().len(), 4);
    }

    /// A widget that declares no kit picture is still drawn with one: every row carries an
    /// icon node.
    #[test]
    fn every_listed_widget_should_get_a_kit_icon() {
        let harness = Harness::mount(None);

        for id in [
            "kytrix.test.candlestick",
            "kytrix.test.market_trade",
            "kytrix.test.financial_index",
            "kytrix.test.news",
        ] {
            assert!(harness.row_has_icon(id), "{id}'s row carries an icon");
        }
    }

    // ── search and category filter ────────────────────────────────────────────────────

    /// The field searches the name a reader sees, case-insensitively, and the sections it
    /// leaves empty are not drawn.
    #[test]
    fn typing_a_name_should_narrow_the_gallery() {
        let mut harness = Harness::mount(None);
        harness.focus_search();

        harness.type_text("CANDLE");

        assert_eq!(harness.rows(), ["kytrix.test.candlestick"]);
        assert!(harness.find("row.kytrix.test.news").is_none());
        assert!(
            harness.find("heading.1").is_none(),
            "a category the search leaves empty is not drawn"
        );
        assert_eq!(harness.query(), "candle");
    }

    /// The search matches the description a widget declares as well as its name.
    #[test]
    fn typing_a_description_should_narrow_the_gallery() {
        let mut harness = Harness::mount(None);
        harness.focus_search();

        harness.type_text("multi-asset");

        assert_eq!(
            harness.rows(),
            ["kytrix.test.candlestick"],
            "the query names the widget's description, not its displayed name"
        );
    }

    /// The name a search matches is translated: with the interface in Chinese the widget's
    /// Chinese name narrows the list.
    #[test]
    fn typing_a_localized_name_should_narrow_the_gallery() {
        let mut harness = Harness::mount(None);
        harness.app.switch_language(LanguageCode::ZhCn);
        harness.settle();
        harness.focus_search();

        harness.type_text("线");

        assert_eq!(
            harness.rows(),
            ["kytrix.test.candlestick"],
            "the Chinese name of the candlestick widget is matched"
        );
    }

    /// The description a search matches is translated with the interface: the Chinese
    /// sentence narrows the list in the Chinese interface, and the English one does not.
    #[test]
    fn typing_a_localized_description_should_narrow_the_gallery() {
        let mut harness = Harness::mount(None);
        harness.app.switch_language(LanguageCode::ZhCn);
        harness.settle();
        harness.focus_search();

        harness.type_text("多资产");
        assert_eq!(
            harness.rows(),
            ["kytrix.test.candlestick"],
            "the Chinese description of the candlestick widget is matched"
        );

        harness.press(KeyCode::KeyA, Modifiers::CONTROL);
        harness.press(KeyCode::Backspace, Modifiers::empty());
        harness.type_text("Live");
        assert!(
            harness.rows().is_empty(),
            "the English description is not searched in the Chinese interface"
        );
    }

    /// Choosing a row is a pointer press in the catalogue, not a move of the keyboard: the
    /// search field keeps the caret, so what the reader types next still narrows the list.
    #[test]
    fn typing_after_choosing_a_widget_should_still_narrow_the_gallery() {
        let mut harness = Harness::mount(None);

        harness.click_row("kytrix.test.news");
        assert_eq!(harness.selection().as_deref(), Some("kytrix.test.news"));
        assert_eq!(
            harness.app.app().focused(harness.app.window()),
            Some(harness.input_root()),
            "choosing a row leaves the keyboard in the search field"
        );

        harness.type_text("candle");
        assert_eq!(harness.rows(), ["kytrix.test.candlestick"]);
        assert_eq!(
            harness.selection().as_deref(),
            Some("kytrix.test.news"),
            "narrowing the list does not change what a confirmation would carry"
        );
    }

    /// The category filter narrows the catalogue to one group, and choosing it again
    /// clears the filter.
    #[test]
    fn the_sidebar_should_filter_the_catalogue_by_category() {
        let mut harness = Harness::mount(None);
        let statistics = harness.category_button(1);

        harness.click_node(statistics);
        assert_eq!(harness.rows(), ["kytrix.test.financial_index"]);
        assert_eq!(
            harness.gallery.core.borrow().filter,
            Some(WidgetCategory::Statistics)
        );

        let statistics = harness.category_button(1);
        harness.click_node(statistics);
        assert_eq!(
            harness.rows().len(),
            4,
            "choosing the filter again clears it"
        );
        assert_eq!(harness.gallery.core.borrow().filter, None);
    }

    /// A search that matches nothing is drawn as a search that found nothing rather than as
    /// a catalogue with nothing in it.
    #[test]
    fn a_search_that_matches_nothing_should_say_so() {
        let mut harness = Harness::mount(None);
        harness.focus_search();

        harness.type_text("nothing here");

        assert!(harness.rows().is_empty());
        let empty = self::find_debug(
            harness.app.tree(),
            harness.gallery.panel(),
            &smol_str::format_smolstr!("{}:empty", harness.debug("empty")),
        );
        assert!(empty.is_some(), "the page says the search matched nothing");
        assert!(harness.find("row.kytrix.test.candlestick").is_none());
    }

    /// Clearing the field restores the whole catalogue, because the page is drawn from what
    /// the field holds.
    #[test]
    fn clearing_the_search_should_restore_the_whole_gallery() {
        let mut harness = Harness::mount(None);
        harness.focus_search();
        harness.type_text("multi-asset");
        assert_eq!(harness.rows().len(), 1);

        harness.press(KeyCode::KeyA, Modifiers::CONTROL);
        harness.press(KeyCode::Backspace, Modifiers::empty());

        assert_eq!(harness.query(), "");
        assert_eq!(harness.rows().len(), 4);
    }

    // ── selection and the two decisions ───────────────────────────────────────────────

    /// Selecting a row replaces the selection and the preview: the page carries the one
    /// widget a confirmation would name, and it holds one widget instance at a time.
    #[test]
    fn choosing_another_widget_should_replace_the_selection_and_the_preview() {
        let mut harness = Harness::mount(None);

        harness.click_row("kytrix.test.market_trade");
        assert_eq!(
            harness.selection().as_deref(),
            Some("kytrix.test.market_trade")
        );
        assert!(harness.preview("TEST_PREVIEW_MARKET_TRADE"));

        harness.click_row("kytrix.test.candlestick");
        assert_eq!(
            harness.selection().as_deref(),
            Some("kytrix.test.candlestick"),
            "the newly chosen widget replaces the previous one"
        );
        assert!(harness.preview("TEST_PREVIEW_CANDLESTICK"));
        assert!(
            !harness.preview("TEST_PREVIEW_MARKET_TRADE"),
            "the previous preview was released with its nodes"
        );
    }

    /// The confirm control reports the widget the page has selected.
    #[test]
    fn the_confirm_control_should_confirm_the_selection() {
        let mut harness = Harness::mount(Some("kytrix.test.news"));
        let confirm = harness.confirm_button();

        harness.click_node(confirm);

        assert_eq!(
            harness.take_event(),
            Some(WidgetGalleryEvent::Confirm(widget("kytrix.test.news")))
        );
    }

    /// The confirm control refuses input while nothing is selected: the disabled paint does
    /// not stop the handler on its own, so the page's own guard is what refuses.
    #[test]
    fn the_confirm_control_should_refuse_while_nothing_is_selected() {
        let mut harness = Harness::mount(None);
        let confirm = harness.confirm_button();

        harness.click_node(confirm);
        assert_eq!(harness.take_event(), None);

        harness.press(KeyCode::Enter, Modifiers::empty());
        assert_eq!(
            harness.take_event(),
            None,
            "Enter with nothing selected confirms nothing either"
        );
    }

    /// The cancel control leaves without choosing anything.
    #[test]
    fn the_cancel_control_should_emit_cancel() {
        let mut harness = Harness::mount(Some("kytrix.test.news"));
        let cancel = harness.cancel_button();

        harness.click_node(cancel);

        assert_eq!(harness.take_event(), Some(WidgetGalleryEvent::Cancel));
    }

    /// Escape leaves the page, and Enter confirms, while the caret is in the search field —
    /// the page's own keys fire inside its subtree and only once per press.
    #[test]
    fn escape_should_cancel_and_enter_should_confirm_from_the_search_field() {
        let mut harness = Harness::mount(Some("kytrix.test.news"));
        harness.focus_search();

        harness.press(KeyCode::Escape, Modifiers::empty());
        assert_eq!(harness.take_event(), Some(WidgetGalleryEvent::Cancel));

        harness.focus_search();
        harness.press(KeyCode::Enter, Modifiers::empty());
        assert_eq!(
            harness.take_event(),
            Some(WidgetGalleryEvent::Confirm(widget("kytrix.test.news")))
        );
    }

    /// The search narrows what is listed, not what a confirmation would carry: a widget
    /// selected before the search is still the answer when the search hides its row.
    #[test]
    fn a_selection_the_search_hides_should_still_be_confirmable() {
        let mut harness = Harness::mount(Some("kytrix.test.news"));
        harness.focus_search();

        harness.type_text("multi-asset");
        assert_eq!(harness.rows(), ["kytrix.test.candlestick"]);
        assert!(harness.find("row.kytrix.test.news").is_none());

        harness.press(KeyCode::Enter, Modifiers::empty());
        assert_eq!(
            harness.take_event(),
            Some(WidgetGalleryEvent::Confirm(widget("kytrix.test.news"))),
            "the search narrows the list, not the answer a confirmation carries"
        );
    }

    /// Tab keeps the keyboard in the page: it is the field's tab cycle, not the window's.
    #[test]
    fn tab_should_keep_the_keyboard_in_the_search_field() {
        let mut harness = Harness::mount(None);
        harness.focus_search();
        let input = harness.input_root();

        for modifiers in [Modifiers::empty(), Modifiers::SHIFT] {
            harness.press(KeyCode::Tab, modifiers);
            assert_eq!(
                harness.app.app().focused(harness.app.window()),
                Some(input),
                "Tab should leave the keyboard on the page's field"
            );
            assert_eq!(harness.query(), "", "Tab is navigation, not typing");
        }
    }

    // ── language ──────────────────────────────────────────────────────────────────────

    /// A language switch redraws the page's copy and its placeholder, and keeps the widget
    /// it previews: the page is not re-created.
    #[test]
    fn switching_the_language_should_redraw_the_page() {
        let mut harness = Harness::mount(Some("kytrix.test.news"));
        let header = harness.must_find("header");
        assert!(harness.has_text_under(header, "Widgets"));
        assert_eq!(harness.placeholder(), "Search widgets");
        assert!(harness.preview("TEST_PREVIEW_NEWS"));
        let routes = harness.app.router().len();

        harness.app.switch_language(LanguageCode::ZhCn);
        harness.settle();

        let header = harness.must_find("header");
        assert!(harness.has_text_under(header, "部件"), "the title follows");
        assert_eq!(harness.placeholder(), "搜索部件");
        let row = harness.must_find("row.kytrix.test.news");
        assert!(
            harness.has_text_under(row, "新闻"),
            "the rows are rebuilt in the new language"
        );
        assert!(
            harness.preview("TEST_PREVIEW_NEWS"),
            "the preview keeps running"
        );
        assert_eq!(
            harness.app.router().len(),
            routes,
            "the language switch mounts no widget instance"
        );
    }

    /// A language switch keeps the search text and the caret: switching is not a reason to
    /// lose what the reader was doing.
    #[test]
    fn a_language_switch_should_keep_the_search_text_and_the_caret() {
        let mut harness = Harness::mount(None);
        harness.focus_search();
        harness.type_text("candle");
        assert_eq!(harness.query(), "candle");

        harness.app.switch_language(LanguageCode::ZhCn);
        harness.settle();

        let input = harness.input_root();
        let text = {
            let state = harness.gallery.core.borrow().input_state;
            harness.app.app().read(&state).text().to_owned()
        };
        assert_eq!(text, "candle", "the typed text survives the rebuild");
        assert_eq!(
            harness.app.app().focused(harness.app.window()),
            Some(input),
            "the field keeps the caret"
        );
    }

    // ── lifetime: release, two pages, no growth ───────────────────────────────────────

    /// Releasing the page releases the preview, the kit components, the nodes and the value
    /// slots; repeated mount/release rounds reuse the slots instead of growing the table.
    #[test]
    fn releasing_the_page_should_leave_nothing_behind() {
        let mut app = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let catalogue: Rc<dyn GalleryCatalogue> = Rc::new(TestCatalogue::new());
        let window = app.window();
        let parent = app.root();
        let baseline_nodes = app.node_count();
        let baseline = app.signal_slot_stats();

        let mut released_slots = None;
        for round in 0..8 {
            let services = Rc::clone(app.services_rc());
            let gallery = WidgetGallery::mount(
                app.app_mut(),
                window,
                parent,
                services,
                Rc::clone(&catalogue),
                Some(widget("kytrix.test.candlestick")),
            )
            .expect("the gallery mounts");
            app.drain();
            app.drain();
            assert_eq!(app.router().len(), 2, "the page and its preview are routed");
            assert!(
                app.signal_slot_stats().live > baseline.live,
                "the mounted page minted its own values"
            );

            assert!(gallery.release(app.app_mut()) > 0);
            app.drain();
            assert!(gallery.is_released());
            assert_eq!(app.router().len(), 0, "every route is unregistered");
            assert_eq!(
                app.node_count(),
                baseline_nodes,
                "the page's nodes are gone"
            );
            assert_eq!(
                app.signal_slot_stats().live,
                baseline.live,
                "release hands every value slot back"
            );
            assert_eq!(
                gallery.release(app.app_mut()),
                0,
                "a second release is a no-op"
            );

            let slots = app.signal_slot_stats();
            match released_slots {
                None => released_slots = Some(slots),
                Some(expected) => assert_eq!(
                    slots.slots, expected.slots,
                    "round {round} reused the slots instead of growing the table"
                ),
            }
        }
    }

    /// Moving through the catalogue mounts one preview at a time: the page's node count and
    /// value slots stay where the first preview left them.
    #[test]
    fn moving_through_the_catalogue_should_not_grow_the_page() {
        let mut harness = Harness::mount(Some("kytrix.test.candlestick"));
        let nodes = harness.app.node_count();
        let slots = harness.app.signal_slot_stats();

        for round in 0..8 {
            let id = if round % 2 == 0 {
                "kytrix.test.news"
            } else {
                "kytrix.test.candlestick"
            };
            harness.click_row(id);
            assert_eq!(
                harness.app.node_count(),
                nodes,
                "round {round} holds one preview at a time"
            );
            assert_eq!(harness.app.router().len(), 2);
        }

        let after = harness.app.signal_slot_stats();
        assert_eq!(after.slots, slots.slots, "no slot leak across switches");
        assert_eq!(after.live, slots.live);
    }

    /// Two pages in one application keep their own keys, their own previews and their own
    /// reports, and releasing one leaves the other working.
    #[test]
    fn two_pages_should_keep_their_own_keys_previews_and_reports() {
        let mut app = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let catalogue: Rc<dyn GalleryCatalogue> = Rc::new(TestCatalogue::new());
        let services = Rc::clone(app.services_rc());
        let window = app.window();
        let root = app.root();
        {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("the host window")
                .tree_mut();
            tree.set_style(root, StyleBuilder::new().flex_row().size_full().build())
                .expect("the root style");
        }
        let left = NodeBuilder::boxed()
            .id("TEST_PANE_LEFT")
            .style(StyleBuilder::new().width(380.0).height_full().build())
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("the left pane");
        let right = NodeBuilder::boxed()
            .id("TEST_PANE_RIGHT")
            .style(StyleBuilder::new().flex_1().height_full().build())
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("the right pane");
        let first = WidgetGallery::mount(
            app.app_mut(),
            window,
            left,
            Rc::clone(&services),
            Rc::clone(&catalogue),
            Some(widget("kytrix.test.candlestick")),
        )
        .expect("the first page mounts");
        let second = WidgetGallery::mount(
            app.app_mut(),
            window,
            right,
            Rc::clone(&services),
            catalogue,
            Some(widget("kytrix.test.news")),
        )
        .expect("the second page mounts");
        app.drain();
        app.drain();

        assert_ne!(first.panel(), second.panel());
        assert_ne!(
            app.tree().debug_id(first.panel()),
            app.tree().debug_id(second.panel()),
            "the two pages have different component keys"
        );
        assert!(
            app.find_by_debug_prefix(first.panel(), "TEST_PREVIEW_CANDLESTICK")
                .is_some()
        );
        assert!(
            app.find_by_debug_prefix(second.panel(), "TEST_PREVIEW_NEWS")
                .is_some()
        );
        assert_eq!(app.router().len(), 4, "two pages and two previews");

        let first_confirm = first
            .core
            .borrow()
            .chrome
            .as_ref()
            .expect("chrome")
            .confirm
            .node()
            .expect("the confirm control is built");
        click_node(&mut app, first_confirm);
        assert_eq!(
            first.take_event(),
            Some(WidgetGalleryEvent::Confirm(widget(
                "kytrix.test.candlestick"
            )))
        );
        assert_eq!(second.take_event(), None, "the other page reports nothing");

        assert!(first.release(app.app_mut()) > 0);
        app.drain();
        assert_eq!(
            app.router().len(),
            2,
            "only the second page's routes remain"
        );
        assert!(
            app.find_by_debug_prefix(second.panel(), "TEST_PREVIEW_NEWS")
                .is_some(),
            "the second page keeps its preview"
        );

        let second_confirm = second
            .core
            .borrow()
            .chrome
            .as_ref()
            .expect("chrome")
            .confirm
            .node()
            .expect("the confirm control is built");
        click_node(&mut app, second_confirm);
        assert_eq!(
            second.take_event(),
            Some(WidgetGalleryEvent::Confirm(widget("kytrix.test.news"))),
            "the page still on screen keeps its own handlers"
        );
        second.release(app.app_mut());
    }
}
