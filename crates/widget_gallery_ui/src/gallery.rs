use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
    sync::atomic::{AtomicU64, Ordering},
};

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, KeyBinding, KeyCode, Modifiers},
    interaction::{ScrollConfig, ScrollHandle},
    math::{LogicalPoint, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{
        color::Color,
        primitive::Border,
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::TextStyle,
    window::WindowId,
};
use ui_kit::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::IconSpec,
        input::{self, InputNode, InputSpec, InputState},
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        separator::Separator,
    },
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{
    any::AnyWidget, category::WidgetCategory, descriptor::WidgetDescriptor, identity::WidgetId,
    services::WidgetServices,
};

use crate::{catalogue::GalleryCatalogue, widget_icon};

const ICON_SIZE: f32 = 20.0;

const ROW_HEIGHT: f32 = 56.0;

const ROW_GAP: f32 = 4.0;
const HEADING_HEIGHT: f32 = 20.0;

const HEADING_GAP: f32 = 8.0;

const CATEGORY_GAP: f32 = 16.0;

const SEARCH_WIDTH: f32 = 240.0;

const SIDEBAR_WIDTH: f32 = 168.0;

const PREVIEW_WIDTH: f32 = 320.0;

const CONTENT_PADDING: f32 = 16.0;

const MARK_SIZE: f32 = 14.0;

static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetGalleryEvent {
    Confirm(WidgetId),
    Cancel,
}

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

    fn control(&self, suffix: &str) -> SmolStr {
        format_smolstr!("{}.{suffix}", self.prefix)
    }

    fn action(&self, suffix: &str) -> Action {
        Action::Custom(self.control(suffix))
    }
}

struct Chrome {
    input: InputNode,
    categories: Vec<(WidgetCategory, Button)>,
    cancel: Button,
    confirm: Button,
}

struct Row {
    node: NodeId,
    widget: WidgetId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RowsBuilt {
    query: SmolStr,
    filter: Option<WidgetCategory>,
    language: LanguageCode,
}

struct GalleryCore {
    window: WindowId,
    keys: GalleryKeys,
    services: Rc<WidgetServices>,
    catalogue: Rc<dyn GalleryCatalogue>,
    theme: ThemeHandle,
    translate: Translate,
    scope: SignalScope,
    panel: NodeId,
    header_chrome: NodeId,
    sidebar: NodeId,
    footer: NodeId,
    content: NodeId,
    scroll: Option<ScrollHandle>,
    scrollbar: ScrollBar,
    preview_host: NodeId,
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
    preview: Option<AnyWidget>,
    preview_selection: Option<WidgetId>,
    event: Option<WidgetGalleryEvent>,
    route: Option<widget::frame::RouteId>,
    actions: Vec<Action>,
    released: bool,
}

impl GalleryCore {
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
        let header_chrome = build_container(
            app,
            window,
            header,
            StyleBuilder::new()
                .flex_none()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .build(),
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
                .width(px(SIDEBAR_WIDTH))
                .gap(px(ROW_GAP))
                .padding_sides(px(16.0), px(8.0), px(16.0), px(0.0))
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
                    .padding(px(CONTENT_PADDING))
                    .gap(px(CATEGORY_GAP))
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
            header_chrome,
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
            let input = core.chrome.as_ref().map(|chrome| chrome.input.editable);
            if let Some(input) = input {
                app.set_focus(window, Some(input));
            }
        }

        Ok(WidgetGallery { core })
    }

    fn bind_actions(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<Vec<Action>, EngineError> {
        let cancel = self.keys.action("cancel");
        let confirm = self.keys.action("confirm");
        let focus_search = self.keys.action("focus-search");
        let row = self.keys.action("row");
        let search_submit = self.keys.action("search-submit");

        {
            let handler = Weak::clone(weak);
            app.on_action(search_submit.clone(), move |ctx| {
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
                    let Some(input) = core.chrome.as_ref().map(|chrome| chrome.input.editable)
                    else {
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
                    let input = core.chrome.as_ref().map(|chrome| chrome.input.editable);
                    (widget, core.window, input)
                };
                let Some(widget) = widget else {
                    return;
                };
                {
                    let mut core = core.borrow_mut();
                    core.select(ctx.app_mut(), widget);
                }
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

        Ok(vec![cancel, confirm, focus_search, row, search_submit])
    }

    fn build_chrome(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let translate = self.translate.clone();

        let header = self.header_chrome;
        let window = self.window;
        let search_box = NodeBuilder::boxed()
            .id(self.keys.control("search"))
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .width(px(SEARCH_WIDTH))
                    .build(),
            )
            .build(tree(app, window), header)?;
        let spec = std::sync::Arc::new(InputSpec {
            name: self.keys.control("search_input"),
            label: None,
            placeholder: t_smol!(&translate, "widget_gallery.search_placeholder"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ui_kit::theme::components::ControlSize::Medium,
            submit: Some(self.keys.action("search-submit")),
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
                            core.chrome.as_ref().map(|chrome| chrome.input.editable)
                        };
                        let _ = ctx.app_mut().request_repaint(window);
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
                    .gap(px(8.0))
                    .padding_sides(px(12.0), px(20.0), px(16.0), px(20.0))
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

    fn rebuild_chrome(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<(), EngineError> {
        let focused = app.focused(self.window);
        let had_search_focus = self
            .chrome
            .as_ref()
            .is_some_and(|chrome| focused == Some(chrome.input.editable));
        if let Some(chrome) = self.chrome.take() {
            clear_children(app, self.window, self.header_chrome);
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
            && let Some(input) = self.chrome.as_ref().map(|chrome| chrome.input.editable)
        {
            app.set_focus(self.window, Some(input));
        }
        Ok(())
    }

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
                        .gap(px(HEADING_GAP))
                        .build(),
                )?;
                let heading = NodeBuilder::boxed()
                    .id(self.keys.control(&format_smolstr!("heading.{index}")))
                    .style(
                        StyleBuilder::new()
                            .flex_row()
                            .items_center()
                            .width_full()
                            .height(px(HEADING_HEIGHT))
                            .build(),
                    )
                    .build(tree(app, self.window), group)?;
                static_label(
                    app,
                    self.window,
                    &self.theme,
                    heading,
                    t_smol!(&self.translate, category.label()),
                    TextStyle::new(px(12.0)).with_weight(600),
                    Role::FgMuted,
                )?;
                let rows = build_container(
                    app,
                    self.window,
                    group,
                    StyleBuilder::new()
                        .flex_col()
                        .width_full()
                        .gap(px(ROW_GAP))
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

    fn update_scroll_content(&mut self, app: &mut App, height: f32) {
        if let Some(handle) = self.scroll
            && let Err(error) =
                app.set_scroll_content(&handle, LogicalSize::new(px(1.0), px(height)))
        {
            log::warn!("the gallery could not size its catalogue: {error}");
        }
    }

    fn reset_scroll(&mut self, app: &mut App) {
        if let Some(handle) = self.scroll
            && let Err(error) = app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(0.0)))
        {
            log::warn!("the gallery could not scroll its catalogue: {error}");
        }
    }

    fn select(&mut self, app: &mut App, widget: WidgetId) {
        let current = { (*app.read(&self.selection)).clone() };
        if current.as_ref() == Some(&widget) {
            return;
        }
        self.selected = Some(widget.clone());
        app.write(&self.selection, Some(widget));
    }

    fn toggle_filter(&mut self, category: WidgetCategory) {
        self.filter = if self.filter == Some(category) {
            None
        } else {
            Some(category)
        };
    }

    fn emit(&mut self, event: WidgetGalleryEvent) {
        self.event = Some(event);
    }

    fn sync(
        &mut self,
        app: &mut App,
        weak: &Weak<RefCell<GalleryCore>>,
    ) -> Result<(), EngineError> {
        if self.released {
            return Ok(());
        }

        if let Some(chrome) = self.chrome.as_mut() {
            let _ = chrome.input.sync(app);
        }

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

    fn sync_chrome(&mut self, app: &mut App) {
        let no_selection = app.read(&self.selection).is_none();
        let filter = self.filter;
        let scrollbar = self.scrollbar.clone();
        if let Some(chrome) = self.chrome.as_mut() {
            let _ = chrome.confirm.set_disabled(app, no_selection);
            for (category, button) in &chrome.categories {
                let _ = button.set_selected(app, filter == Some(*category));
            }
        }
        let _ = scrollbar.sync(app);
    }

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

#[derive(Clone)]
pub struct WidgetGallery {
    core: Rc<RefCell<GalleryCore>>,
}

impl WidgetGallery {
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

    #[must_use]
    pub fn panel(&self) -> NodeId {
        self.core.borrow().panel
    }

    pub fn take_event(&self) -> Option<WidgetGalleryEvent> {
        self.core.borrow_mut().event.take()
    }

    #[must_use]
    pub fn selection(&self) -> Option<WidgetId> {
        self.core.borrow().selected.clone()
    }

    #[must_use]
    pub fn is_released(&self) -> bool {
        self.core.borrow().released
    }

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

fn tree(app: &mut App, window: WindowId) -> &mut NodeTree {
    app.window_mut(window)
        .expect("the gallery's window exists")
        .tree_mut()
}

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
                .gap(px(16.0))
                .padding_sides(px(20.0), px(20.0), px(12.0), px(20.0))
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
            .gap(px(2.0))
            .build(),
    )?;
    bound_label(
        app,
        window,
        theme,
        titles,
        "widget_gallery.title",
        TextStyle::new(px(18.0)).with_weight(700),
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
        TextStyle::new(px(12.0)),
        Role::FgMuted,
        translate,
        *language,
    )?;
    Ok(header)
}

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
                .width(px(PREVIEW_WIDTH))
                .min_height_0()
                .gap(px(8.0))
                .padding(px(16.0))
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
    NodeBuilder::text("", TextStyle::new(px(13.0)).with_weight(600))
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
        TextStyle::new(px(12.0)),
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
                .height(px(ROW_HEIGHT))
                .padding_x(px(12.0))
                .gap(px(12.0))
                .build(),
        )
        .paint(PaintBuilder::new().rounded(px(8.0)).build())
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
                    width: px(1.0),
                    color: line,
                }),
                ..StylePatch::default()
            }
        })
        .build(tree(app, window), parent)?;

    let icon = IconSpec::new(icon).with_size(px(ICON_SIZE)).build_themed(
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
            .gap(px(2.0))
            .build(),
    )?;
    static_label(
        app,
        window,
        theme,
        text_column,
        name,
        TextStyle::new(px(13.0)).with_weight(500),
        Role::FgDefault,
    )?;
    if let Some(description) = description {
        static_label(
            app,
            window,
            theme,
            text_column,
            description,
            TextStyle::new(px(11.0)),
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
                .width(px(MARK_SIZE))
                .height(px(MARK_SIZE))
                .build(),
        )
        .paint(PaintBuilder::new().rounded(px(MARK_SIZE / 2.0)).build())
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
                border: Some(Border {
                    width: px(1.0),
                    color,
                }),
                ..StylePatch::default()
            }
        })
        .build(tree(app, window), row)?;

    Ok(Row { node: row, widget })
}

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

fn category_index(category: WidgetCategory) -> usize {
    WidgetCategory::ALL
        .iter()
        .position(|candidate| *candidate == category)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {

    use std::{rc::Rc, time::Instant};

    use app_config::widget::WidgetConfig;
    use i18n::{language::codes::LanguageCode, translate::Translate};
    use smol_str::SmolStr;
    use ui_core::{
        error::EngineError,
        event::{Event, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, px},
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{NodeKind, NodeTree},
        },
        paint::style_builder::StyleBuilder,
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
                    NodeBuilder::text($marker, TextStyle::new(px(12.0)))
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
            NodeBuilder::text("TEST_PREVIEW_HIDDEN", TextStyle::new(px(12.0)))
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

    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
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

    fn bounds_of(app: &WidgetTestApp, node: NodeId) -> LogicalRect {
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
        let point = LogicalPoint::new(
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
                .editable
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

    #[test]
    fn a_pane_showing_the_fallback_should_open_with_nothing_selected() {
        let harness = Harness::mount(Some("kytrix.test.hidden"));

        assert_eq!(harness.selection(), None);
        assert!(!harness.preview("TEST_PREVIEW_"));
        assert_eq!(harness.app.router().len(), 1, "only the page is routed");
    }

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
        assert_eq!(harness.rows().len(), 4);
    }

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

    #[test]
    fn the_cancel_control_should_emit_cancel() {
        let mut harness = Harness::mount(Some("kytrix.test.news"));
        let cancel = harness.cancel_button();

        harness.click_node(cancel);

        assert_eq!(harness.take_event(), Some(WidgetGalleryEvent::Cancel));
    }

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
            .style(StyleBuilder::new().width(px(380.0)).height_full().build())
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
