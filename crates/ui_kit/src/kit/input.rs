use std::{
    cell::{Cell, RefCell},
    fmt,
    sync::Arc,
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::{Dimension, LengthPercentageAuto};
use ui_core::{
    a11y::A11yRole,
    app::App,
    event::Action,
    interaction::{ScrollAxis, ScrollConfig, ScrollHandle},
    math::{LogicalPx, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        color::Color,
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::{TextAlign, TextOverflow, TextStyle, TextWrap},
    text_edit::{EditableSpec, TextEditMetrics, TextEditSnapshot, TextSelection, sanitize_text},
    window::WindowId,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    kit::{
        button::Button,
        icon::IconKind,
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        size::UnifiedSizable,
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};

const CORNER_RADIUS: f32 = 4.0;

#[derive(Clone, Debug)]
pub struct InputSpec {
    pub name: SmolStr,
    pub label: Option<SmolStr>,
    pub placeholder: SmolStr,
    pub masked: bool,
    pub multiline: bool,
    pub readonly: bool,
    pub disabled: bool,
    pub max_length: Option<usize>,
    pub visible_lines: Option<u16>,
    pub size: ControlSize,
    pub submit: Option<Action>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    Empty,
    TooLong { max: usize },
    Invalid { i18n_key: SmolStr },
}

impl InputError {
    #[must_use]
    pub fn i18n_key(&self) -> &str {
        match self {
            Self::Empty => "input.error.empty",
            Self::TooLong { .. } => "input.error.tooLong",
            Self::Invalid { i18n_key } => i18n_key.as_str(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputState {
    text: Arc<str>,
    selection: TextSelection,
    composing: bool,
    focused: bool,
    can_undo: bool,
    can_redo: bool,
    error: Option<InputError>,
    generation: u64,
}

impl InputState {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let text: Arc<str> = Arc::from(text.into());
        let end = text.len();
        Self {
            text,
            selection: TextSelection::collapsed_at(end),
            composing: false,
            focused: false,
            can_undo: false,
            can_redo: false,
            error: None,
            generation: 0,
        }
    }

    fn mirrored(snapshot: &TextEditSnapshot, error: Option<InputError>) -> Self {
        Self {
            text: Arc::clone(&snapshot.text),
            selection: snapshot.selection,
            composing: snapshot.composing,
            focused: snapshot.focused,
            can_undo: snapshot.can_undo,
            can_redo: snapshot.can_redo,
            error,
            generation: snapshot.generation,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn selection(&self) -> TextSelection {
        self.selection
    }

    #[must_use]
    pub fn caret(&self) -> usize {
        self.selection.focus().byte
    }

    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.composing
    }

    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    #[must_use]
    pub fn error(&self) -> Option<&InputError> {
        self.error.as_ref()
    }

    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.can_undo
    }

    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.can_redo
    }

    pub fn validate(&mut self, spec: &InputSpec) -> Option<InputError> {
        let error = if self.text.is_empty() {
            Some(InputError::Empty)
        } else {
            spec.max_length
                .filter(|max| grapheme_count(&self.text) > *max)
                .map(|max| InputError::TooLong { max })
        };
        self.error = error.clone();
        error
    }

    pub fn set_error(&mut self, error: Option<InputError>) {
        self.error = error;
    }
}

#[derive(Clone, Copy, PartialEq)]
struct InputColors {
    caret: Color,
    selection: Color,
    placeholder: Color,
    composition: Color,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LayoutPlan {
    editable_padding_y: LogicalPx,
    root_height: Option<LogicalPx>,
    root_min_height: LogicalPx,
}

#[derive(Clone, Copy, PartialEq)]
enum FieldAxis {
    SingleLine,
    Multiline,
}

impl FieldAxis {
    fn of(spec: &InputSpec) -> Self {
        if spec.multiline {
            Self::Multiline
        } else {
            Self::SingleLine
        }
    }
}

#[derive(Clone)]
pub struct InputNode {
    pub root: NodeId,
    pub viewport: NodeId,
    pub editable: NodeId,
    pub scrollbar: RefCell<Option<ScrollBar>>,
    pub placeholder_state: Signal<SmolStr>,
    pub state: Signal<InputState>,
    pub spec: Arc<InputSpec>,
    clear: RefCell<Option<Button>>,
    theme: ThemeHandle,
    scroll_handle: Cell<Option<ScrollHandle>>,
    scroll_content: Cell<Option<LogicalSize>>,
    metrics: Cell<Option<TextEditMetrics>>,
    applied_layout: Cell<Option<LayoutPlan>>,
    applied_colors: Cell<Option<InputColors>>,
    applied_text_style: RefCell<Option<TextStyle>>,
    last_placeholder: RefCell<SmolStr>,
    invalid: Cell<bool>,
    submit_on_blur: Cell<bool>,
}

impl fmt::Debug for InputNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputNode")
            .field("name", &self.spec.name)
            .field("root", &self.root)
            .field("editable", &self.editable)
            .field("clearable", &self.clear.borrow().is_some())
            .field("submit_on_blur", &self.submit_on_blur.get())
            .finish()
    }
}

#[must_use]
pub fn build(
    tree: &mut NodeTree,
    parent: NodeId,
    spec: Arc<InputSpec>,
    theme: &ThemeHandle,
    state: &Signal<InputState>,
    placeholder_text: &Signal<SmolStr>,
) -> InputNode {
    let tokens = theme.tokens();
    let size = tokens.for_size(spec.size);
    let border_width = tokens.border_width;
    let disabled = spec.disabled;
    let multiline = spec.multiline;
    let name = spec.name.clone();
    let axis = FieldAxis::of(&spec);
    let padding_x = size.padding_x;
    let padding_y = if multiline {
        size.padding_x
    } else {
        LogicalPx::ZERO
    };
    let fallback_line = size.font_size;
    let root_min_height = if multiline {
        fallback_line
    } else {
        LogicalPx::ZERO
    };

    let mut root_style = StyleBuilder::new()
        .flex_row()
        .items_center()
        .relative()
        .width_full()
        .min_width_0()
        .padding_x(padding_x)
        .padding_y(padding_y)
        .border(border_width)
        .overflow_hidden()
        .build();
    match axis {
        FieldAxis::SingleLine => {
            root_style.flex(0.0, 0.0, Dimension::auto());
            root_style.height_length(size.height);
        }
        FieldAxis::Multiline => match spec.visible_lines {
            Some(lines) => {
                root_style.flex(0.0, 0.0, Dimension::auto());
                root_style.height_length(visible_lines_height(
                    f32::from(lines.max(1)),
                    fallback_line,
                    border_width,
                    padding_y,
                ));
            }
            None => {
                root_style.flex(1.0, 1.0, Dimension::length(0.0));
                root_style.height_percent(1.0);
                root_style.min_height_length(root_min_height);
            }
        },
    }
    let root_paint = PaintBuilder::new()
        .border(border_width, Color::TRANSPARENT)
        .rounded(px(CORNER_RADIUS))
        .build();

    let style_state = *state;
    let style_theme = theme.clone();
    let root = NodeBuilder::boxed()
        .style(root_style)
        .paint(root_paint)
        .id(format_smolstr!("{name}.root"))
        .tab_stop(false)
        .bind_style(move |cx| {
            let state = cx.read(&style_state);
            let colors = style_theme.components(cx).input;
            let border_color = if disabled {
                colors.border.disabled
            } else if state.error().is_some() {
                style_theme.role(cx, Role::StatusDangerBorder)
            } else if state.is_focused() {
                colors.border.focused
            } else {
                colors.border.normal
            };
            StylePatch {
                background: Some(if disabled {
                    colors.background.disabled
                } else {
                    colors.background.enabled
                }),
                border: Some(ui_core::paint::primitive::Border {
                    width: border_width,
                    color: border_color,
                }),
                cursor: Some(if disabled {
                    ui_core::paint::style::Cursor::NotAllowed
                } else {
                    ui_core::paint::style::Cursor::Text
                }),
                ..StylePatch::default()
            }
        })
        .build(tree, parent)
        .expect("input: root node mounts under a live parent");

    let viewport_style = match axis {
        FieldAxis::SingleLine => StyleBuilder::new()
            .flex_row()
            .flex_1()
            .items_center()
            .height_full()
            .min_width_0()
            .relative()
            .overflow_hidden()
            .build(),
        FieldAxis::Multiline => StyleBuilder::new()
            .flex_col()
            .flex_1()
            .height_full()
            .width_full()
            .min_width_0()
            .min_height_0()
            .relative()
            .overflow_hidden()
            .build(),
    };
    let viewport = NodeBuilder::boxed()
        .style(viewport_style)
        .id(format_smolstr!("{name}.viewport"))
        .tab_stop(false)
        .build(tree, root)
        .expect("input: viewport mounts under the fresh root");

    let mut editable_style = match axis {
        FieldAxis::SingleLine => StyleBuilder::new().flex_none().height_full().build(),
        FieldAxis::Multiline => {
            let mut style = StyleBuilder::new().width_full().build();
            style.flex(0.0, 0.0, Dimension::auto());
            style
        }
    };
    match axis {
        FieldAxis::SingleLine => {
            editable_style.min_width(LengthPercentageAuto::percent(1.0));
        }
        FieldAxis::Multiline => {
            editable_style.min_height(LengthPercentageAuto::percent(1.0));
        }
    }
    let editable_spec = EditableSpec {
        role: editable_role(&spec),
        label: spec.label.clone().unwrap_or_else(|| spec.name.clone()),
        placeholder: spec.placeholder.clone(),
        multiline,
        masked: spec.masked,
        readonly: spec.readonly,
        disabled: spec.disabled,
        max_length: spec.max_length,
        submit: spec.submit.clone(),
        text_style: text_style(&spec, &size),
        caret_color: Color::TRANSPARENT,
        selection_color: Color::TRANSPARENT,
        placeholder_color: Color::TRANSPARENT,
        composition_color: Color::TRANSPARENT,
    };
    let editable = NodeBuilder::editable_text(editable_spec)
        .style(editable_style)
        .id(format_smolstr!("{name}.text"))
        .bind_style(move |cx| {
            cx.read(&style_state);
            StylePatch {
                interactive: Some(!disabled),
                cursor: Some(if disabled {
                    ui_core::paint::style::Cursor::NotAllowed
                } else {
                    ui_core::paint::style::Cursor::Text
                }),
                ..StylePatch::default()
            }
        })
        .build(tree, viewport)
        .expect("input: editable text mounts under the fresh viewport");

    InputNode {
        root,
        viewport,
        editable,
        scrollbar: RefCell::new(None),
        placeholder_state: *placeholder_text,
        state: *state,
        spec,
        clear: RefCell::new(None),
        theme: theme.clone(),
        scroll_handle: Cell::new(None),
        scroll_content: Cell::new(None),
        metrics: Cell::new(None),
        applied_layout: Cell::new(None),
        applied_colors: Cell::new(None),
        applied_text_style: RefCell::new(None),
        last_placeholder: RefCell::new(SmolStr::default()),
        invalid: Cell::new(false),
        submit_on_blur: Cell::new(false),
    }
}

impl InputNode {
    pub fn install(&self, app: &mut App) {
        let Some(window) = self.root.window() else {
            return;
        };
        let initial = app.read(&self.state).text().to_owned();
        let _ = app.text_edit_set_value(window, self.editable, &initial);
        let initial_error = self.too_long_error(&initial);
        let placeholder = app.read(&self.placeholder_state).clone();
        let _ = app.text_edit_set_placeholder(window, self.editable, placeholder.as_str());
        *self.last_placeholder.borrow_mut() = placeholder;
        let _ = self.sync_theme(app, window);
        let _ = self.sync_invalid(app, window);
        if let Ok(snapshot) = app.text_edit_snapshot(window, self.editable) {
            let next = InputState::mirrored(&snapshot, initial_error);
            app.write(&self.state, next);
        }
        if !self.spec.disabled {
            self.install_scroll(app, window);
        }
        let _ = self.sync_layout(app, window);
    }

    pub fn release(&self, app: &mut App) -> usize {
        let mut released = 0;
        if let Some(scrollbar) = self.scrollbar.borrow_mut().take() {
            released += scrollbar.release(app);
        }
        if let Some(button) = self.clear.borrow_mut().take() {
            released += button.release(app);
        }
        released
    }

    #[must_use]
    pub fn value(&self, app: &App) -> SmolStr {
        let Some(window) = self.root.window() else {
            return SmolStr::default();
        };
        app.text_edit_value(window, self.editable)
            .unwrap_or_default()
    }

    pub fn update_value(&self, app: &mut App, value: &str) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        let Ok(changed) = app.text_edit_set_value(window, self.editable, value) else {
            return false;
        };
        self.refresh_mirror(app, window, self.too_long_error(value));
        if changed {
            *self.last_placeholder.borrow_mut() = app.read(&self.placeholder_state).clone();
            let _ = app.request_repaint(window);
        }
        changed
    }

    pub fn update_label(&self, app: &mut App, label: impl Into<SmolStr>) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        let Ok(changed) = app.text_edit_set_label(window, self.editable, label.into()) else {
            return false;
        };
        if changed {
            let _ = app.request_repaint(window);
        }
        changed
    }

    pub fn update_placeholder(&self, app: &mut App, placeholder: impl Into<SmolStr>) -> bool {
        let placeholder = placeholder.into();
        let changed = app.write(&self.placeholder_state, placeholder.clone());
        if changed {
            *self.last_placeholder.borrow_mut() = placeholder;
            if let Some(window) = self.root.window() {
                let value = self.last_placeholder.borrow().clone();
                let _ = app.text_edit_set_placeholder(window, self.editable, value.as_str());
                let _ = app.request_repaint(window);
            }
        }
        changed
    }

    pub fn clear(&self, app: &mut App) -> bool {
        self.update_value(app, "")
    }

    pub fn set_clearable(&mut self, app: &mut App, clearable: bool) -> bool {
        if self.clear.borrow().is_some() == clearable {
            return false;
        }
        let Some(window) = self.root.window() else {
            return false;
        };
        if !clearable {
            if let Some(button) = self.clear.borrow_mut().take() {
                if let Some(node) = button.node()
                    && app
                        .window(window)
                        .is_some_and(|target| target.tree().get(node).is_ok())
                {
                    let _ = app.remove_node(window, node);
                }
                button.release(app);
            }
            return true;
        }
        let node = self.clone();
        let key = format_smolstr!("{}.clear", self.spec.name);
        let button = Button::new(app, self.theme.clone(), key)
            .icon(IconKind::X)
            .a11y_label(SmolStr::new_static("Clear"))
            .ghost()
            .small()
            .on_click(move |ctx| {
                node.clear(ctx.app_mut());
            });
        if button.build(app, window, self.root).is_err() {
            return false;
        }
        *self.clear.borrow_mut() = Some(button);
        self.sync_clear_button(app);
        true
    }

    pub fn set_submit_on_blur(&self, submit_on_blur: bool) -> bool {
        self.submit_on_blur.replace(submit_on_blur) != submit_on_blur
    }

    pub fn sync(&self, app: &mut App) -> bool {
        let Some(window) = self.root.window() else {
            return false;
        };
        let Ok(snapshot) = app.text_edit_snapshot(window, self.editable) else {
            return false;
        };
        let previous = app.read(&self.state).clone();
        let mirror_dirty = snapshot.generation != previous.generation;
        let blur_submit = previous.focused
            && !snapshot.focused
            && self.submit_on_blur.get()
            && !self.spec.multiline
            && self.spec.submit.is_some();
        let focus_changed = previous.focused != snapshot.focused;
        let mut changed = false;
        if mirror_dirty {
            let mut next = InputState::mirrored(&snapshot, previous.error.clone());
            if snapshot.text != previous.text {
                next.error = self.too_long_error(snapshot.text.as_ref());
            }
            changed |= app.write(&self.state, next);
        }
        if focus_changed {
            if snapshot.focused {
                app.set_ime_enabled(window, !self.spec.readonly);
            } else {
                app.clear_ime_cursor_area(window);
                if app.focused_editable(window).is_none() {
                    app.set_ime_enabled(window, false);
                }
            }
            changed = true;
        }
        changed |= self.sync_theme(app, window);
        changed |= self.sync_placeholder(app, window);
        changed |= self.sync_layout(app, window);
        changed |= self.sync_invalid(app, window);
        changed |= self.sync_scroll(app, window);
        changed |= self.sync_clear_button(app);
        if blur_submit && let Some(submit) = &self.spec.submit {
            changed |= app.dispatch_action(
                window,
                self.root,
                submit.clone(),
                ui_core::event::ActionSource::Programmatic,
            );
        }
        changed
    }

    fn refresh_mirror(&self, app: &mut App, window: WindowId, error: Option<InputError>) {
        let Ok(snapshot) = app.text_edit_snapshot(window, self.editable) else {
            return;
        };
        app.write(&self.state, InputState::mirrored(&snapshot, error));
    }

    fn too_long_error(&self, text: &str) -> Option<InputError> {
        let max = self.spec.max_length?;
        let cleaned = sanitize_text(text, self.spec.multiline);
        (grapheme_count(cleaned.as_str()) > max).then_some(InputError::TooLong { max })
    }

    fn install_scroll(&self, app: &mut App, window: WindowId) {
        if FieldAxis::of(&self.spec) == FieldAxis::SingleLine {
            return;
        }
        let config = ScrollConfig {
            axis: ScrollAxis::Vertical,
            line_height: self.metrics.get().map_or_else(
                || self.theme.tokens().for_size(self.spec.size).font_size,
                |metrics| metrics.line_height,
            ),
        };
        let Ok(handle) = app.make_scroll(window, self.viewport, config) else {
            return;
        };
        self.scroll_handle.set(Some(handle));
        let scrollbar = ScrollBar::new(
            app,
            self.theme.clone(),
            format_smolstr!("{}.scrollbar", self.spec.name),
            scrollbar_axis(ScrollAxis::Vertical),
            ScrollbarOptions::default().hover(),
        );
        let built = app
            .window_mut(window)
            .is_some_and(|target| scrollbar.build(target.tree_mut(), self.root).is_ok());
        if !built {
            return;
        }
        scrollbar.attach(app, window, self.viewport);
        *self.scrollbar.borrow_mut() = Some(scrollbar);
    }

    fn sync_theme(&self, app: &mut App, window: WindowId) -> bool {
        let appearance = self.theme.current(app);
        let theme = self.theme.theme_for(appearance);
        let input = theme.components().input;
        let colors = InputColors {
            caret: theme.role(Role::FgDefault),
            selection: input.selection,
            placeholder: input.placeholder,
            composition: theme.role(Role::FgDefault),
        };
        let mut changed = false;
        if self.applied_colors.get() != Some(colors) {
            let _ = app.text_edit_set_caret_color(window, self.editable, colors.caret);
            let _ = app.text_edit_set_selection_color(window, self.editable, colors.selection);
            let _ = app.text_edit_set_placeholder_color(window, self.editable, colors.placeholder);
            let _ = app.text_edit_set_composition_color(window, self.editable, colors.composition);
            self.applied_colors.set(Some(colors));
            changed = true;
        }
        let mut style = self.text_style();
        style.color = if self.spec.disabled {
            input.foreground.disabled
        } else {
            input.foreground.enabled
        };
        let stale = self.applied_text_style.borrow().as_ref() != Some(&style);
        if stale
            && app
                .text_edit_set_text_style(window, self.editable, style.clone())
                .is_ok()
        {
            *self.applied_text_style.borrow_mut() = Some(style);
            changed = true;
        }
        changed
    }

    fn text_style(&self) -> TextStyle {
        let size = self.theme.tokens().for_size(self.spec.size);
        text_style(&self.spec, &size)
    }

    fn sync_placeholder(&self, app: &mut App, window: WindowId) -> bool {
        let placeholder = app.read(&self.placeholder_state).clone();
        if *self.last_placeholder.borrow() == placeholder {
            return false;
        }
        *self.last_placeholder.borrow_mut() = placeholder.clone();
        app.text_edit_set_placeholder(window, self.editable, placeholder.as_str())
            .unwrap_or(false)
    }

    fn sync_layout(&self, app: &mut App, window: WindowId) -> bool {
        if let Ok(metrics) = app.text_edit_metrics(window, self.editable) {
            self.metrics.set(Some(metrics));
        }
        let Some(metrics) = self.metrics.get() else {
            return false;
        };
        let plan = self.layout_plan(metrics);
        if self.applied_layout.get() == Some(plan) {
            return false;
        }
        let Some(target) = app.window_mut(window) else {
            return false;
        };
        let changed = apply_layout_plan(target.tree_mut(), self.root, self.editable, plan);
        if changed {
            self.applied_layout.set(Some(plan));
        }
        changed
    }

    fn layout_plan(&self, metrics: TextEditMetrics) -> LayoutPlan {
        let tokens = self.theme.tokens();
        let size = tokens.for_size(self.spec.size);
        let border = tokens.border_width;
        let line = metrics.line_height.max(px(1.0));
        match FieldAxis::of(&self.spec) {
            FieldAxis::SingleLine => {
                let content = (size.height - border * 2.0).max(LogicalPx::ZERO);
                LayoutPlan {
                    editable_padding_y: ((content - line) * 0.5).max(LogicalPx::ZERO),
                    root_height: None,
                    root_min_height: LogicalPx::ZERO,
                }
            }
            FieldAxis::Multiline => match self.spec.visible_lines {
                Some(lines) => LayoutPlan {
                    editable_padding_y: LogicalPx::ZERO,
                    root_height: Some(visible_lines_height(
                        f32::from(lines.max(1)),
                        line,
                        border,
                        size.padding_x,
                    )),
                    root_min_height: LogicalPx::ZERO,
                },
                None => LayoutPlan {
                    editable_padding_y: LogicalPx::ZERO,
                    root_height: None,
                    root_min_height: line,
                },
            },
        }
    }

    fn sync_invalid(&self, app: &mut App, window: WindowId) -> bool {
        let invalid = app.read(&self.state).error().is_some();
        if self.invalid.get() == invalid {
            return false;
        }
        self.invalid.set(invalid);
        app.text_edit_set_a11y_invalid(window, self.editable, invalid)
            .unwrap_or(false)
    }

    fn sync_scroll(&self, app: &mut App, window: WindowId) -> bool {
        let Some(handle) = self.scroll_handle.get() else {
            return false;
        };
        let mut changed = false;
        let content = {
            let Some(target) = app.window(window) else {
                return false;
            };
            let tree = target.tree();
            let (Ok(editable), Ok(viewport)) = (tree.get(self.editable), tree.get(self.viewport))
            else {
                return false;
            };
            LogicalSize::new(
                editable
                    .layout()
                    .bounds
                    .size
                    .width
                    .max(viewport.layout().bounds.size.width),
                editable
                    .layout()
                    .bounds
                    .size
                    .height
                    .max(viewport.layout().bounds.size.height),
            )
        };
        if self.scroll_content.get() != Some(content) {
            let _ = app.set_scroll_content(&handle, content);
            self.scroll_content.set(Some(content));
            changed = true;
        }
        if let Some(scrollbar) = self.scrollbar.borrow().as_ref() {
            changed |= scrollbar.sync(app).unwrap_or(false);
        }
        changed
    }

    fn sync_clear_button(&self, app: &mut App) -> bool {
        let Some(button) = self.clear.borrow().clone() else {
            return false;
        };
        let Some(node) = button.node() else {
            return false;
        };
        let mut changed = button.set_disabled(app, self.spec.disabled);
        let display = if app.read(&self.state).text().is_empty() {
            taffy::style::Display::None
        } else {
            taffy::style::Display::Flex
        };
        let Some(window) = node.window() else {
            return changed;
        };
        let Some(target) = app.window_mut(window) else {
            return changed;
        };
        let tree = target.tree_mut();
        let Ok(current) = tree.get(node) else {
            return changed;
        };
        let mut style = current.style().clone();
        if style.display_mode() != display {
            style.display(display);
            let _ = tree.set_style(node, style);
            changed = true;
        }
        changed
    }
}

fn apply_layout_plan(
    tree: &mut NodeTree,
    root: NodeId,
    editable: NodeId,
    plan: LayoutPlan,
) -> bool {
    let mut changed = false;
    if let Ok(node) = tree.get(root) {
        let mut style = node.style().clone();
        if let Some(height) = plan.root_height {
            style.height_length(height);
        }
        if plan.root_min_height.value() > 0.0 {
            style.min_height(LengthPercentageAuto::length(plan.root_min_height.value()));
        }
        let _ = tree.set_style(root, style);
        changed = true;
    }
    if let Ok(node) = tree.get(editable) {
        let mut style = node.style().clone();
        style.padding_y(plan.editable_padding_y);
        let _ = tree.set_style(editable, style);
        changed = true;
    }
    changed
}

fn visible_lines_height(
    lines: f32,
    line_height: LogicalPx,
    border: LogicalPx,
    padding: LogicalPx,
) -> LogicalPx {
    px(lines * line_height.value() + 2.0 * (border.value() + padding.value()))
}

fn editable_role(spec: &InputSpec) -> A11yRole {
    if spec.masked {
        A11yRole::PasswordInput
    } else if spec.multiline {
        A11yRole::MultilineTextInput
    } else {
        A11yRole::TextInput
    }
}

fn text_style(spec: &InputSpec, size: &crate::theme::components::SizeTokens) -> TextStyle {
    let wrap = if spec.multiline {
        TextWrap::Wrap
    } else {
        TextWrap::NoWrap
    };
    TextStyle::new(size.font_size)
        .with_wrap(wrap)
        .with_alignment(TextAlign::Start)
        .with_overflow(TextOverflow::Clip, 0)
}

fn scrollbar_axis(axis: ScrollAxis) -> ScrollbarAxis {
    match axis {
        ScrollAxis::Horizontal => ScrollbarAxis::Horizontal,
        ScrollAxis::Vertical => ScrollbarAxis::Vertical,
    }
}

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, sync::Arc};

    use smol_str::SmolStr;
    use ui_core::{
        a11y::{A11yRole, A11yTextSelection},
        app::App,
        event::{Action, ImeEvent, InputEvent, KeyCode, Modifiers, PointerButton},
        interaction::{ScrollConfig, ScrollHandle},
        math::{LogicalPoint, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeKind},
        paint::{
            color::{Color, ColorResolver},
            style_builder::StyleBuilder,
        },
        test_support::{
            app::TestApp,
            record::{PrimitiveKind, QuadRecord},
        },
        window::WindowId,
    };

    use super::{InputError, InputNode, InputSpec, InputState, build};
    use crate::theme::{
        appearance::Appearance, components::ControlSize, handle::ThemeHandle, role::Role,
    };

    fn spec() -> InputSpec {
        InputSpec {
            name: SmolStr::new_static("test"),
            label: None,
            placeholder: SmolStr::new_static("请输入"),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Medium,
            submit: None,
        }
    }

    fn multiline_spec(visible_lines: Option<u16>) -> InputSpec {
        InputSpec {
            multiline: true,
            visible_lines,
            size: ControlSize::Small,
            ..spec()
        }
    }

    fn masked_spec() -> InputSpec {
        InputSpec {
            masked: true,
            ..spec()
        }
    }

    fn max_spec(max: usize) -> InputSpec {
        InputSpec {
            max_length: Some(max),
            ..spec()
        }
    }

    struct Harness {
        app: TestApp,
        node: InputNode,
        state: Signal<InputState>,
        placeholder: Signal<SmolStr>,
        theme: ThemeHandle,
    }

    fn harness(spec: InputSpec, initial: &str) -> Harness {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let state = app.app_mut().signal(InputState::new(initial));
        let placeholder = app.app_mut().signal(spec.placeholder.clone());
        let spec = Arc::new(spec);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let node = build(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut(),
            root,
            Arc::clone(&spec),
            &theme,
            &state,
            &placeholder,
        );
        node.install(app.app_mut());
        for _ in 0..3 {
            let _ = node.sync(app.app_mut());
            app.drain().expect("稳定帧");
        }
        Harness {
            app,
            node,
            state,
            placeholder,
            theme,
        }
    }

    impl Harness {
        fn window(&self) -> WindowId {
            self.app.window_id()
        }

        fn drain(&mut self) {
            self.sync();
            self.app.drain().expect("帧");
        }

        fn sync(&mut self) -> bool {
            self.node.sync(self.app.app_mut())
        }

        fn state(&self) -> InputState {
            self.app.app().read(&self.state).clone()
        }

        fn text(&self) -> SmolStr {
            SmolStr::new(self.state().text())
        }

        fn engine_value(&self) -> SmolStr {
            self.node.value(self.app.app())
        }

        fn engine_display(&self) -> SmolStr {
            let window = self.window();
            let tree = self.app.app().window(window).expect("窗口存在").tree();
            match tree.get(self.node.editable).expect("可编辑节点").kind() {
                NodeKind::Text(content) => content.text.clone(),
                other => panic!("可编辑节点必须是文本节点：{other:?}"),
            }
        }

        fn focus(&mut self) {
            let window = self.window();
            assert!(
                self.app
                    .app_mut()
                    .set_focus(window, Some(self.node.editable)),
                "可编辑节点可聚焦"
            );
            self.drain();
        }

        fn blur(&mut self) {
            let window = self.window();
            self.app.app_mut().set_focus(window, None);
            self.drain();
        }

        fn type_text(&mut self, text: &str) {
            let _ = self.app.inject(InputEvent::Text {
                text: SmolStr::new(text),
                timestamp: std::time::Instant::now(),
            });
            self.drain();
        }

        fn key(&mut self, code: KeyCode, modifiers: Modifiers) {
            let _ = self.app.inject(InputEvent::key_down(code, modifiers));
            self.drain();
        }

        fn ime(&mut self, event: ImeEvent) {
            self.app.dispatch_event(ui_core::event::Event::Ime(event));
            self.drain();
        }

        fn root_bounds(&self) -> ui_core::math::LogicalRect {
            self.app
                .probe()
                .bounds(self.node.root)
                .expect("root 已布局")
        }

        fn editable_bounds(&self) -> ui_core::math::LogicalRect {
            self.app
                .probe()
                .bounds(self.node.editable)
                .expect("editable 已布局")
        }

        fn caret_rect(&self, byte: usize) -> ui_core::math::LogicalRect {
            let window = self.window();
            self.app
                .app()
                .window(window)
                .expect("窗口存在")
                .text_caret_rect(self.node.editable, byte)
                .expect("caret 几何")
        }

        fn metrics(&self) -> ui_core::text_edit::TextEditMetrics {
            let window = self.window();
            self.app
                .app()
                .text_edit_metrics(window, self.node.editable)
                .expect("布局已就绪")
        }

        fn snapshot(&self) -> ui_core::text_edit::TextEditSnapshot {
            let window = self.window();
            self.app
                .app()
                .text_edit_snapshot(window, self.node.editable)
                .expect("快照")
        }

        fn a11y_node(&mut self) -> ui_core::a11y::A11yNode {
            let snapshot = crate::kit::test_support::full_a11y(&mut self.app);
            snapshot
                .nodes()
                .iter()
                .find(|node| node.engine == self.node.editable)
                .cloned()
                .expect("a11y 树包含可编辑节点")
        }

        fn quads(&self) -> Vec<QuadRecord> {
            let Some(frame) = self.app.frame_record().frame.as_ref() else {
                return Vec::new();
            };
            frame
                .quads
                .iter()
                .filter(|quad| quad.node == self.node.editable)
                .copied()
                .collect()
        }

        fn primitives(&self, kind: PrimitiveKind) -> usize {
            let Some(frame) = self.app.frame_record().frame.as_ref() else {
                return 0;
            };
            frame
                .primitives
                .iter()
                .filter(|primitive| primitive.node == self.node.editable && primitive.kind == kind)
                .count()
        }

        fn clear_button_node(&self) -> NodeId {
            let button = self.node.clear.borrow().clone().expect("清空按钮已建");
            button.node().expect("清空按钮有节点")
        }

        fn display_of(&self, node: NodeId) -> taffy::style::Display {
            let window = self.window();
            self.app
                .app()
                .window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("节点存在")
                .style()
                .inner()
                .display
        }

        fn click_node(&mut self, node: NodeId) {
            let bounds = self.app.probe().bounds(node).expect("已布局");
            let point = LogicalPoint::new(
                bounds.origin.x + bounds.size.width * 0.5,
                bounds.origin.y + bounds.size.height * 0.5,
            );
            let _ = self.app.inject(InputEvent::pointer_down(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
        }

        fn style_patch(&self, node: NodeId) -> ui_core::paint::style::StylePatch {
            let window = self.window();
            self.app
                .app()
                .window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("节点存在")
                .style_patch()
                .clone()
        }
    }

    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    #[test]
    fn install_pushes_the_initial_value_into_the_engine_and_the_mirror() {
        let harness = harness(spec(), "hello");
        assert_eq!(harness.engine_value(), "hello");
        assert_eq!(harness.engine_display(), "hello");
        assert_eq!(harness.text(), "hello");
        assert_eq!(harness.state().caret(), 5);
        assert!(harness.state().selection().is_collapsed());
    }

    #[test]
    fn typing_and_backspace_flow_through_the_engine_into_the_mirror() {
        let mut harness = harness(spec(), "");
        harness.focus();
        harness.type_text("abc");
        assert_eq!(harness.engine_value(), "abc");
        assert_eq!(harness.text(), "abc");
        harness.key(KeyCode::Backspace, Modifiers::empty());
        assert_eq!(harness.engine_value(), "ab");
        assert_eq!(harness.text(), "ab");
        assert!(harness.state().can_undo(), "引擎编辑进入撤销历史");
    }

    #[test]
    fn engine_value_is_fresh_before_the_frame_mirrors_it() {
        let mut harness = harness(spec(), "");
        harness.focus();
        let _ = harness.app.inject(InputEvent::Text {
            text: SmolStr::new_static("xy"),
            timestamp: std::time::Instant::now(),
        });
        assert_eq!(harness.engine_value(), "xy", "引擎文本同步可读");
        assert_eq!(harness.text(), "", "镜像要等 sync 才写入");
        harness.drain();
        assert_eq!(harness.text(), "xy");
    }

    #[test]
    fn ime_commit_updates_the_engine_and_the_mirror() {
        let mut harness = harness(spec(), "");
        harness.focus();
        harness.ime(ImeEvent::Start);
        assert!(harness.state().is_composing());
        harness.ime(ImeEvent::Preedit {
            text: SmolStr::new_static("ni"),
            caret: None,
            clauses: Default::default(),
            replace: None,
        });
        assert_eq!(harness.engine_display(), "ni");
        harness.ime(ImeEvent::Commit {
            text: SmolStr::new_static("你"),
            replace: None,
        });
        assert_eq!(harness.engine_value(), "你");
        assert!(!harness.state().is_composing());
        assert!(harness.engine_display().as_str().contains('你'));
    }

    #[test]
    fn masked_input_mirrors_the_real_text_and_paints_bullets() {
        let mut harness = harness(masked_spec(), "");
        harness.node.update_value(harness.app.app_mut(), "secret");
        harness.drain();
        assert_eq!(harness.text(), "secret", "镜像保存真实文本");
        assert_eq!(harness.engine_value(), "secret");
        assert_eq!(harness.engine_display(), "••••••");
        assert_eq!(harness.a11y_node().value.as_deref(), Some("••••••"));
        assert_eq!(harness.a11y_node().role, A11yRole::PasswordInput);
    }

    #[test]
    fn max_length_counts_grapheme_clusters_not_bytes() {
        let mut harness = harness(
            InputSpec {
                max_length: Some(2),
                ..spec()
            },
            "",
        );
        harness.node.update_value(harness.app.app_mut(), "👨‍👩‍👧‍👦ab");
        harness.drain();
        assert_eq!(harness.text(), "👨‍👩‍👧‍👦a");
        assert_eq!(
            harness.state().error(),
            Some(&InputError::TooLong { max: 2 }),
            "程序化写入超长必须报 TooLong"
        );
    }

    #[test]
    fn disabled_input_is_not_focusable_and_ignores_typing() {
        let mut harness = harness(
            InputSpec {
                disabled: true,
                ..spec()
            },
            "locked",
        );
        let window = harness.window();
        assert!(
            !harness
                .app
                .app_mut()
                .set_focus(window, Some(harness.node.editable)),
            "禁用输入不可聚焦"
        );
        let _ = harness.app.inject(InputEvent::Text {
            text: SmolStr::new_static("x"),
            timestamp: std::time::Instant::now(),
        });
        harness.drain();
        assert_eq!(harness.engine_value(), "locked");
        assert_eq!(harness.text(), "locked");
    }

    #[test]
    fn readonly_input_takes_focus_but_refuses_edits() {
        let mut harness = harness(
            InputSpec {
                readonly: true,
                ..spec()
            },
            "fixed",
        );
        harness.focus();
        assert_eq!(
            harness.app.app().focused(harness.window()),
            Some(harness.node.editable)
        );
        harness.type_text("x");
        assert_eq!(harness.engine_value(), "fixed");
    }

    #[test]
    fn clear_button_appears_with_text_and_clears_the_engine() {
        let mut harness = harness(spec(), "");
        assert!(harness.node.set_clearable(harness.app.app_mut(), true));
        harness.drain();
        let button_node = harness.clear_button_node();
        assert_eq!(
            harness.display_of(button_node),
            taffy::style::Display::None,
            "空文本隐藏清空按钮"
        );
        harness.node.update_value(harness.app.app_mut(), "text");
        harness.drain();
        assert_eq!(harness.display_of(button_node), taffy::style::Display::Flex);
        harness.click_node(button_node);
        harness.drain();
        assert_eq!(harness.engine_value(), "", "清空按钮清空引擎文本");
        assert_eq!(harness.text(), "");
    }

    #[test]
    fn blur_dispatches_the_submit_action_once() {
        let hits = Rc::new(Cell::new(0));
        let submit = Action::Custom(SmolStr::new_static("test.submit"));
        let counter = Rc::clone(&hits);
        let mut harness = harness(
            InputSpec {
                submit: Some(submit.clone()),
                ..spec()
            },
            "",
        );
        harness
            .app
            .app_mut()
            .on_action(submit, move |_ctx| counter.set(counter.get() + 1));
        assert!(harness.node.set_submit_on_blur(true));
        harness.focus();
        harness.type_text("abc");
        harness.blur();
        assert_eq!(hits.get(), 1, "失焦派发一次 submit");
        harness.drain();
        assert_eq!(hits.get(), 1, "再同步不重复派发");
    }

    #[test]
    fn submit_on_blur_is_only_for_single_line_inputs() {
        let hits = Rc::new(Cell::new(0));
        let submit = Action::Custom(SmolStr::new_static("test.multi.submit"));
        let counter = Rc::clone(&hits);
        let mut harness = harness(
            InputSpec {
                submit: Some(submit.clone()),
                ..multiline_spec(None)
            },
            "",
        );
        harness
            .app
            .app_mut()
            .on_action(submit, move |_ctx| counter.set(counter.get() + 1));
        assert!(harness.node.set_submit_on_blur(true));
        harness.focus();
        harness.blur();
        assert_eq!(hits.get(), 0, "多行失焦不提交");
    }

    #[test]
    fn enter_submits_a_single_line_and_inserts_a_newline_in_a_multiline() {
        let hits = Rc::new(Cell::new(0));
        let submit = Action::Custom(SmolStr::new_static("test.enter.submit"));
        let counter = Rc::clone(&hits);
        let mut single = harness(
            InputSpec {
                submit: Some(submit.clone()),
                ..spec()
            },
            "",
        );
        single
            .app
            .app_mut()
            .on_action(submit, move |_ctx| counter.set(counter.get() + 1));
        single.focus();
        single.key(KeyCode::Enter, Modifiers::empty());
        assert_eq!(hits.get(), 1, "单行 Enter 提交");
        assert_eq!(single.engine_value(), "");

        let mut multi = harness(multiline_spec(None), "");
        multi.focus();
        multi.type_text("a");
        multi.key(KeyCode::Enter, Modifiers::empty());
        multi.type_text("b");
        assert_eq!(multi.engine_value(), "a\nb");
    }

    #[test]
    fn update_value_clears_undo_and_reports_too_long() {
        let mut harness = harness(
            InputSpec {
                max_length: Some(3),
                ..spec()
            },
            "",
        );
        harness.focus();
        harness.type_text("ab");
        assert!(harness.state().can_undo());
        assert!(harness.node.update_value(harness.app.app_mut(), "abcdef"));
        harness.drain();
        assert_eq!(harness.text(), "abc");
        assert!(!harness.state().can_undo(), "程序化写值清空撤销");
        assert_eq!(
            harness.state().error(),
            Some(&InputError::TooLong { max: 3 })
        );
        harness.node.update_value(harness.app.app_mut(), "ab");
        harness.drain();
        assert_eq!(harness.state().error(), None, "合法值清空错误态");
    }

    #[test]
    fn validate_reports_empty_and_too_long_with_stable_i18n_keys() {
        let mut state = InputState::new("");
        assert_eq!(state.validate(&spec()), Some(InputError::Empty));
        assert_eq!(
            state.error().map(InputError::i18n_key),
            Some("input.error.empty")
        );
        let mut too_long = InputState::new("abcd");
        let limited = InputSpec {
            max_length: Some(3),
            ..spec()
        };
        assert_eq!(
            too_long.validate(&limited),
            Some(InputError::TooLong { max: 3 })
        );
        assert_eq!(
            too_long.error().map(InputError::i18n_key),
            Some("input.error.tooLong")
        );
        too_long.set_error(Some(InputError::Invalid {
            i18n_key: SmolStr::new_static("input.error.duplicate"),
        }));
        assert_eq!(
            too_long.error().map(InputError::i18n_key),
            Some("input.error.duplicate")
        );
    }

    #[test]
    fn update_label_reaches_the_engine_a11y_label() {
        let mut harness = harness(spec(), "");
        assert_eq!(
            harness.a11y_node().label.as_ref().map(|label| label.text()),
            Some("test"),
            "the label starts as the field name"
        );
        assert!(harness.node.update_label(harness.app.app_mut(), "过滤框"));
        assert_eq!(
            harness.a11y_node().label.as_ref().map(|label| label.text()),
            Some("过滤框"),
            "the running label reaches the engine a11y tree"
        );
        assert!(
            !harness.node.update_label(harness.app.app_mut(), "过滤框"),
            "the same label is not reported as changed again"
        );
    }

    #[test]
    fn update_placeholder_reaches_the_engine_a11y_placeholder() {
        let mut harness = harness(spec(), "");
        harness.drain();
        assert_eq!(harness.a11y_node().placeholder.as_deref(), Some("请输入"));
        assert!(
            harness
                .node
                .update_placeholder(harness.app.app_mut(), "新的占位")
        );
        harness.drain();
        assert_eq!(harness.a11y_node().placeholder.as_deref(), Some("新的占位"));
        assert!(
            !harness
                .node
                .update_placeholder(harness.app.app_mut(), "新的占位"),
            "同值不写"
        );
    }

    #[test]
    fn a_dirty_error_survives_sync_until_the_text_changes() {
        let mut harness = harness(spec(), "abc");
        harness.drain();
        let mut state = harness.state();
        state.set_error(Some(InputError::Invalid {
            i18n_key: SmolStr::new_static("test.invalid"),
        }));
        harness.app.app_mut().write(&harness.state, state);
        harness.drain();
        assert!(harness.state().error().is_some(), "同步不得清掉业务错误");
        assert!(harness.a11y_node().invalid, "错误态透传到 a11y");
        harness.focus();
        harness.type_text("d");
        assert_eq!(harness.state().error(), None, "编辑后错误态清除");
        assert!(!harness.a11y_node().invalid);
    }

    #[test]
    fn single_line_height_comes_from_the_control_size() {
        for (size, expected) in [
            (ControlSize::Small, 20.0),
            (ControlSize::Medium, 30.0),
            (ControlSize::Large, 40.0),
        ] {
            let harness = harness(InputSpec { size, ..spec() }, "");
            let bounds = harness.root_bounds();
            assert!(
                (bounds.size.height.value() - expected).abs() < 0.01,
                "{size:?} 单行高度应为 {expected}，实际 {}",
                bounds.size.height.value()
            );
        }
    }

    #[test]
    fn visible_lines_sets_the_multiline_height_from_the_engine_metrics() {
        let harness = harness(multiline_spec(Some(3)), "");
        let metrics = harness.metrics();
        let tokens = harness.theme.tokens();
        let size = tokens.for_size(ControlSize::Small);
        let expected = 3.0 * metrics.line_height.value()
            + 2.0 * (tokens.border_width.value() + size.padding_x.value());
        let bounds = harness.root_bounds();
        assert!(
            (bounds.size.height.value() - expected).abs() < 0.5,
            "3 行高度={expected}，实际 {}",
            bounds.size.height.value()
        );
        let editable = harness.editable_bounds();
        let line = metrics.line_height.value();
        assert!(
            editable.size.height.value() >= 3.0 * line - 0.5,
            "文本节点不被 visible_lines 截断：{} vs {line}",
            editable.size.height.value()
        );
    }

    #[test]
    fn multiline_without_visible_lines_fills_the_host_height() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let state = app.app_mut().signal(InputState::new(""));
        let placeholder = app.app_mut().signal(SmolStr::new_static(""));
        let host = {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(320.0))
                        .height(px(120.0))
                        .build(),
                )
                .build(tree, root)
                .expect("宿主盒")
        };
        let node = {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            build(
                tree,
                host,
                Arc::new(multiline_spec(None)),
                &theme,
                &state,
                &placeholder,
            )
        };
        node.install(app.app_mut());
        app.drain().expect("首帧");
        let bounds = app.probe().bounds(node.root).expect("root 已布局");
        assert!(
            (bounds.size.height.value() - 120.0).abs() < 0.5,
            "无 visible_lines 时填满宿主高度：{}",
            bounds.size.height.value()
        );
    }

    #[test]
    fn placeholder_and_text_share_the_first_line_box_and_the_caret() {
        let mut harness = harness(spec(), "");
        harness.focus();
        let empty_caret = harness.caret_rect(0);
        assert_eq!(
            harness.engine_display(),
            "",
            "placeholder 不进入可编辑节点的显示文本"
        );
        assert!(
            harness.primitives(PrimitiveKind::GlyphRun) > 0,
            "placeholder 由可编辑节点自己绘制"
        );
        let empty_line = harness.metrics().line_height;
        assert!(
            (empty_caret.size.height.value() - empty_line.value()).abs() < 0.5,
            "空文本 caret 高度=行盒：{} vs {empty_line:?}",
            empty_caret.size.height.value()
        );
        harness.node.update_value(harness.app.app_mut(), "abc");
        harness.drain();
        let text_caret = harness.caret_rect(3);
        assert!(
            (text_caret.origin.y.value() - empty_caret.origin.y.value()).abs() < 0.5,
            "placeholder 与文本共用首行行盒：{} vs {}",
            empty_caret.origin.y.value(),
            text_caret.origin.y.value()
        );
        let root = harness.root_bounds();
        assert!(
            text_caret.origin.y >= root.origin.y
                && text_caret.origin.y + text_caret.size.height
                    <= root.origin.y + root.size.height + 0.5,
            "caret 在控件盒内：{text_caret:?} vs {root:?}"
        );
    }

    #[test]
    fn empty_text_caret_sits_inside_the_line_box_of_every_size() {
        for size in [ControlSize::Small, ControlSize::Medium, ControlSize::Large] {
            let mut harness = harness(InputSpec { size, ..spec() }, "");
            harness.focus();
            let caret = harness.caret_rect(0);
            let metrics = harness.metrics();
            let root = harness.root_bounds();
            assert!(
                (caret.size.height.value() - metrics.line_height.value()).abs() < 0.5,
                "{size:?} 空文本 caret 高度=行盒"
            );
            assert!(
                caret.origin.y >= root.origin.y - 0.5
                    && caret.origin.y + caret.size.height <= root.origin.y + root.size.height + 0.5,
                "{size:?} 空文本 caret 不得超出控件盒：{caret:?} vs {root:?}"
            );
        }
    }

    #[test]
    fn dragging_inside_a_scrolled_page_selects_the_expected_range() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let wrapper = {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(360.0))
                        .height(px(120.0))
                        .overflow_hidden()
                        .flex_col()
                        .build(),
                )
                .build(tree, root)
                .expect("滚动容器")
        };
        let handle: ScrollHandle = app
            .app_mut()
            .make_scroll(
                window,
                wrapper,
                ScrollConfig {
                    axis: ui_core::interaction::ScrollAxis::Vertical,
                    line_height: px(20.0),
                },
            )
            .expect("滚动容器注册");
        {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .height(px(20.0))
                        .flex_none()
                        .build(),
                )
                .build(tree, wrapper)
                .expect("前置留白");
        };
        let state = app.app_mut().signal(InputState::new("abcdefghij"));
        let placeholder = app.app_mut().signal(SmolStr::new_static(""));
        let node = {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            build(
                tree,
                wrapper,
                Arc::new(spec()),
                &theme,
                &state,
                &placeholder,
            )
        };
        node.install(app.app_mut());
        app.drain().expect("首帧");
        app.drain().expect("布局帧");
        app.drain().expect("高度帧");
        app.app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(360.0), px(400.0)))
            .expect("内容尺寸");
        app.app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(0.0), px(30.0)))
            .expect("滚动");
        app.drain().expect("滚动帧");

        let caret_at = |app: &App, byte: usize| {
            app.window(window)
                .expect("窗口存在")
                .text_caret_rect(node.editable, byte)
                .expect("caret 几何")
        };
        let start = caret_at(app.app(), 0);
        let end = caret_at(app.app(), 10);
        let from = LogicalPoint::new(
            start.origin.x + start.size.width * 0.5,
            start.origin.y + start.size.height * 0.5,
        );
        let to = LogicalPoint::new(
            end.origin.x + end.size.width * 0.5,
            end.origin.y + end.size.height * 0.5,
        );
        app.inject(InputEvent::pointer_down(
            from,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        app.move_pointer(to);
        app.inject(InputEvent::pointer_up(
            to,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        node.sync(app.app_mut());
        app.drain().expect("拖选帧");
        let state_now = app.app().read(&state).clone();
        assert_eq!(
            state_now.selection().range(),
            0..10,
            "滚动页面内拖选必须锚定点击处（BUG③）"
        );
        assert_eq!(state_now.selection().anchor().byte, 0);

        app.app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(0.0), px(0.0)))
            .expect("滚回顶部");
        app.drain().expect("归零帧");
        let start = caret_at(app.app(), 10);
        let end = caret_at(app.app(), 0);
        let from = LogicalPoint::new(
            start.origin.x + start.size.width * 0.5,
            start.origin.y + start.size.height * 0.5,
        );
        let to = LogicalPoint::new(
            end.origin.x + end.size.width * 0.5,
            end.origin.y + end.size.height * 0.5,
        );

        app.inject(InputEvent::pointer_down(
            from,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        app.move_pointer(to);
        app.inject(InputEvent::pointer_up(
            to,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        node.sync(app.app_mut());
        app.drain().expect("反向拖选帧");
        let state_now = app.app().read(&state).clone();
        assert_eq!(state_now.selection().range(), 0..10);
        assert_eq!(
            state_now.selection().anchor().byte,
            10,
            "反向拖选锚点在终点"
        );
    }

    #[test]
    fn control_characters_never_reach_the_engine_text() {
        let mut harness = harness(spec(), "");
        harness.focus();
        let _ = harness.app.inject(InputEvent::Text {
            text: SmolStr::new_static("\u{8}\u{1b}\u{1}"),
            timestamp: std::time::Instant::now(),
        });
        harness.drain();
        assert_eq!(harness.engine_value(), "", "控制字符不插入（BUG④）");
        harness.type_text("a\u{8}b");
        assert_eq!(harness.engine_value(), "ab", "控制字符被清洗");
        assert_eq!(harness.text(), "ab");
        assert!(
            harness
                .engine_display()
                .as_str()
                .chars()
                .all(|ch| !ch.is_control())
        );
    }

    #[test]
    fn releasing_a_single_line_input_reports_nothing_to_release() {
        let mut harness = harness(spec(), "abc");
        assert!(
            harness.node.scrollbar.borrow().is_none(),
            "a single line field draws no scrollbar"
        );
        assert_eq!(
            harness.node.release(harness.app.app_mut()),
            0,
            "there is nothing to detach"
        );
    }

    #[test]
    fn the_mirror_short_circuits_when_the_generation_does_not_change() {
        let mut harness = harness(spec(), "");
        harness.focus();
        assert!(!harness.sync(), "稳态 sync 不写信号");
        assert!(!harness.sync());
        harness.type_text("a");
        assert_eq!(harness.state().generation, harness.snapshot().generation);
        assert!(!harness.sync(), "同一世代不重复写镜像");
    }

    #[test]
    fn a11y_roles_follow_mask_and_line_count() {
        let mut single = harness(spec(), "abc");
        assert_eq!(single.a11y_node().role, A11yRole::TextInput);
        assert_eq!(single.a11y_node().value.as_deref(), Some("abc"));
        let mut multiline = harness(multiline_spec(None), "ab\ncd");
        assert_eq!(multiline.a11y_node().role, A11yRole::MultilineTextInput);
        let mut masked = harness(masked_spec(), "ab");
        assert_eq!(masked.a11y_node().role, A11yRole::PasswordInput);
        assert_eq!(masked.a11y_node().value.as_deref(), Some("••"));
    }

    #[test]
    fn a11y_selection_follows_the_engine_selection() {
        let mut harness = harness(spec(), "abcdef");
        harness.focus();
        harness.key(KeyCode::Home, Modifiers::empty());
        harness.key(KeyCode::ArrowRight, Modifiers::SHIFT);
        harness.key(KeyCode::ArrowRight, Modifiers::SHIFT);
        harness.drain();
        let node = harness.a11y_node();
        assert_eq!(
            node.text_selection,
            Some(A11yTextSelection {
                anchor: 0,
                focus: 2
            })
        );
    }

    #[test]
    fn switching_appearance_repaints_every_input_color() {
        let mut harness = harness(spec(), "abc");
        harness.focus();
        harness.key(KeyCode::KeyA, Modifiers::CONTROL);
        let light = harness.theme.theme_for(Appearance::Light);
        let caret = resolved(light.role(Role::FgDefault));
        let selection = resolved(light.components().input.selection);
        let quads = harness.quads();
        assert!(
            quads.iter().any(|quad| quad.fill.components == caret),
            "浅色 caret 颜色画进可编辑节点"
        );
        assert!(
            quads.iter().any(|quad| quad.fill.components == selection),
            "浅色选区颜色画进可编辑节点"
        );

        harness
            .theme
            .set_appearance(harness.app.app_mut(), Appearance::Dark);
        harness.drain();
        let dark = harness.theme.theme_for(Appearance::Dark);
        let dark_caret = resolved(dark.role(Role::FgDefault));
        let dark_selection = resolved(dark.components().input.selection);
        assert_ne!(dark_caret, caret);
        let quads = harness.quads();
        assert!(
            quads.iter().any(|quad| quad.fill.components == dark_caret),
            "深色 caret 颜色跟随主题"
        );
        assert!(
            quads
                .iter()
                .any(|quad| quad.fill.components == dark_selection),
            "深色选区颜色跟随主题"
        );
        let text_color = {
            let window = harness.window();
            let tree = harness.app.app().window(window).expect("窗口").tree();
            match tree.get(harness.node.editable).expect("editable").kind() {
                NodeKind::Text(content) => content.style.color,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            resolved(text_color),
            resolved(dark.components().input.foreground.enabled),
            "文本颜色跟随主题"
        );
        let patch = harness.style_patch(harness.node.root);
        assert_eq!(
            patch.background.map(resolved),
            Some(resolved(dark.components().input.background.enabled))
        );
        assert_eq!(
            patch.border.map(|border| resolved(border.color)),
            Some(resolved(dark.components().input.border.focused))
        );
    }

    #[test]
    fn the_root_takes_the_border_and_background_of_its_state() {
        let mut harness = harness(spec(), "");
        let normal_border = {
            let light = harness.theme.theme_for(Appearance::Light);
            resolved(light.components().input.border.normal)
        };
        let focus_border = {
            let light = harness.theme.theme_for(Appearance::Light);
            resolved(light.components().input.border.focused)
        };
        let normal = harness.style_patch(harness.node.root);
        assert_eq!(
            normal.border.map(|border| resolved(border.color)),
            Some(normal_border)
        );
        harness.focus();
        let focused = harness.style_patch(harness.node.root);
        assert_eq!(
            focused.border.map(|border| resolved(border.color)),
            Some(focus_border),
            "聚焦换边框色"
        );
    }

    #[test]
    fn the_editable_node_is_the_only_text_child_the_engine_owns() {
        let harness = harness(spec(), "");
        let window = harness.window();
        let tree = harness.app.app().window(window).expect("窗口").tree();
        let mut text_nodes = Vec::new();
        let mut stack = vec![harness.node.root];
        while let Some(node) = stack.pop() {
            let Ok(entry) = tree.get(node) else {
                continue;
            };
            if matches!(entry.kind(), NodeKind::Text(_)) {
                text_nodes.push(node);
            }
            stack.extend(entry.children().iter().copied());
        }
        assert_eq!(
            text_nodes,
            vec![harness.node.editable],
            "placeholder/caret 不再是独立节点（BUG① 结构性修复）"
        );
        assert!(
            tree.get(harness.node.editable)
                .expect("editable")
                .children()
                .is_empty()
        );
    }

    #[test]
    fn release_detaches_the_scrollbar_and_the_clear_button() {
        let mut harness = harness(multiline_spec(None), "abc");
        harness.node.set_clearable(harness.app.app_mut(), true);
        harness.drain();
        assert!(harness.node.scrollbar.borrow().is_some(), "滚动条已建");
        let released = harness.node.release(harness.app.app_mut());
        assert!(released > 0, "release 回收清空按钮与滚动条 action");
        assert!(harness.node.scrollbar.borrow().is_none());
        assert!(harness.node.clear.borrow().is_none());
        assert_eq!(
            harness.node.release(harness.app.app_mut()),
            0,
            "release 幂等"
        );
    }

    #[test]
    fn the_scrollbar_reports_overflow_only_when_the_content_is_taller() {
        let mut harness = harness(multiline_spec(Some(3)), "一\n二\n三");
        harness.drain();
        let fits = {
            let bar = harness.node.scrollbar.borrow().clone().expect("滚动条");
            bar.scrollable(harness.app.app())
        };
        assert!(!fits, "三行内容在可视三行内不溢出");
        harness
            .node
            .update_value(harness.app.app_mut(), "一\n二\n三\n四\n五");
        harness.drain();
        harness.drain();
        let overflows = {
            let bar = harness.node.scrollbar.borrow().clone().expect("滚动条");
            bar.scrollable(harness.app.app())
        };
        assert!(overflows, "五行内容超出可视三行");
    }

    #[test]
    fn the_placeholder_signal_is_exposed_for_dynamic_copy() {
        let harness = harness(spec(), "");
        assert_eq!(
            harness.app.app().read(&harness.placeholder).as_str(),
            harness
                .app
                .app()
                .read(&harness.node.placeholder_state)
                .as_str()
        );
    }

    #[test]
    fn an_error_changes_the_root_border_patch() {
        let mut harness = harness(spec(), "");
        let normal = {
            let light = harness.theme.theme_for(Appearance::Light);
            resolved(light.components().input.border.normal)
        };
        let rejected = {
            let light = harness.theme.theme_for(Appearance::Light);
            resolved(light.role(Role::StatusDangerBorder))
        };
        let border = harness.style_patch(harness.node.root).border;
        assert_eq!(border.map(|border| resolved(border.color)), Some(normal));
        let mut state = harness.state();
        state.set_error(Some(InputError::Empty));
        let signal = harness.node.state;
        harness.app.app_mut().write(&signal, state);
        harness.drain();
        let border = harness.style_patch(harness.node.root).border;
        assert_eq!(
            border.map(|border| resolved(border.color)),
            Some(rejected),
            "an error the field reports marks the border as rejected"
        );
    }

    #[test]
    fn switching_appearance_repaints_the_placeholder_color() {
        let mut harness = harness(spec(), "");
        let window = harness.window();
        let light_placeholder = {
            let light = harness.theme.theme_for(Appearance::Light);
            resolved(light.components().input.placeholder)
        };
        assert_eq!(
            resolved(
                harness
                    .app
                    .app()
                    .text_edit_placeholder_color(window, harness.node.editable)
                    .expect("the field keeps a placeholder color")
            ),
            light_placeholder,
            "the placeholder color reaches the engine from the light theme"
        );
        harness
            .theme
            .set_appearance(harness.app.app_mut(), Appearance::Dark);
        harness.drain();
        let dark_placeholder = {
            let dark = harness.theme.theme_for(Appearance::Dark);
            resolved(dark.components().input.placeholder)
        };
        assert_eq!(
            resolved(
                harness
                    .app
                    .app()
                    .text_edit_placeholder_color(window, harness.node.editable)
                    .expect("the field keeps a placeholder color")
            ),
            dark_placeholder,
            "the placeholder color follows the dark theme"
        );
        assert_ne!(
            light_placeholder, dark_placeholder,
            "the two appearances use different placeholder colors"
        );
    }

    #[test]
    fn update_value_reaches_readonly_and_disabled_fields() {
        let mut readonly = harness(
            InputSpec {
                readonly: true,
                ..spec()
            },
            "keep",
        );
        assert!(
            readonly
                .node
                .update_value(readonly.app.app_mut(), "programmatic")
        );
        assert_eq!(readonly.engine_value(), "programmatic");
        assert_eq!(readonly.text(), "programmatic");

        let mut disabled = harness(
            InputSpec {
                disabled: true,
                ..spec()
            },
            "keep",
        );
        assert!(
            disabled
                .node
                .update_value(disabled.app.app_mut(), "programmatic")
        );
        assert_eq!(disabled.engine_value(), "programmatic");
        assert_eq!(disabled.text(), "programmatic");
    }

    #[test]
    fn update_value_counts_the_sanitized_single_line_for_max_length() {
        let mut limited = harness(max_spec(3), "");
        assert!(limited.node.update_value(limited.app.app_mut(), "ab\tc"));
        assert_eq!(limited.engine_value(), "abc");
        assert_eq!(
            limited.state().error(),
            None,
            "the cleaned line is exactly the limit, not over it"
        );
        let mut over = harness(max_spec(2), "");
        assert!(over.node.update_value(over.app.app_mut(), "ab\tc"));
        assert_eq!(over.engine_value(), "ab");
        assert_eq!(
            over.state().error(),
            Some(&InputError::TooLong { max: 2 }),
            "a cleaned line over the limit reports TooLong"
        );
    }

    #[test]
    fn max_length_counts_grapheme_clusters_and_validation_agrees() {
        let mut harness = harness(max_spec(2), "");
        assert!(harness.node.update_value(harness.app.app_mut(), "👨‍👩‍👧‍👦a"));
        assert_eq!(
            harness.engine_value(),
            "👨‍👩‍👧‍👦a",
            "one family emoji is one grapheme, so both clusters fit"
        );
        assert_eq!(harness.state().error(), None);
        assert_eq!(harness.state().validate(&max_spec(2)), None);

        assert!(harness.node.update_value(harness.app.app_mut(), "abc"));
        assert_eq!(harness.engine_value(), "ab");
        assert_eq!(
            harness.state().error(),
            Some(&InputError::TooLong { max: 2 }),
            "the request that had to be clipped is reported"
        );
        assert_eq!(
            harness.state().validate(&max_spec(2)),
            None,
            "the mirrored text the engine keeps is within the limit"
        );
    }

    #[test]
    fn the_multiline_height_falls_back_on_the_first_frame_then_settles() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let state = app.app_mut().signal(InputState::new(""));
        let placeholder = app.app_mut().signal(SmolStr::new_static(""));
        let spec = multiline_spec(Some(3));
        let size = theme.tokens().for_size(spec.size);
        let border = theme.tokens().border_width.value();
        let fallback = 3.0 * size.font_size.value() + 2.0 * (border + size.padding_x.value());
        let node = {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            build(tree, root, Arc::new(spec), &theme, &state, &placeholder)
        };
        node.install(app.app_mut());
        let mut heights = Vec::with_capacity(5);
        for _ in 0..5 {
            let _ = node.sync(app.app_mut());
            app.drain().expect("帧");
            heights.push(
                app.probe()
                    .bounds(node.root)
                    .expect("root 已布局")
                    .size
                    .height
                    .value(),
            );
        }
        let settled = app
            .app()
            .text_edit_metrics(window, node.editable)
            .expect("指标就绪")
            .line_height
            .value();
        let expected = 3.0 * settled + 2.0 * (border + size.padding_x.value());
        assert!(
            (heights[0] - fallback).abs() < 0.5,
            "首帧按最小行高回退：{} vs {fallback}",
            heights[0]
        );
        assert!(
            (heights[1] - expected).abs() < 0.5,
            "第二帧换成引擎行高：{} vs {expected}",
            heights[1]
        );
        assert!(
            heights[2..]
                .iter()
                .all(|height| (height - expected).abs() < 0.5),
            "之后稳定不振荡：{heights:?}"
        );
    }

    #[test]
    fn ime_stays_on_when_the_focus_moves_back_to_an_earlier_synced_field() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let mut nodes: Vec<InputNode> = Vec::with_capacity(2);
        for name in ["first", "second"] {
            let state = app.app_mut().signal(InputState::new(""));
            let placeholder = app.app_mut().signal(SmolStr::new_static(""));
            let spec = InputSpec {
                name: SmolStr::new_static(name),
                ..spec()
            };
            let node = {
                let tree = app
                    .app_mut()
                    .window_mut(window)
                    .expect("窗口存在")
                    .tree_mut();
                build(tree, root, Arc::new(spec), &theme, &state, &placeholder)
            };
            node.install(app.app_mut());
            nodes.push(node);
        }
        let first = nodes[0].clone();
        let second = nodes[1].clone();
        let settle = |app: &mut TestApp| {
            for _ in 0..2 {
                let _ = first.sync(app.app_mut());
                let _ = second.sync(app.app_mut());
                app.drain().expect("帧");
            }
        };

        app.app_mut().set_focus(window, Some(first.editable));
        settle(&mut app);
        assert!(
            app.app().ime_enabled(window),
            "the field the focus landed on asks for the IME"
        );
        app.app_mut().set_focus(window, Some(second.editable));
        settle(&mut app);
        assert!(
            app.app().ime_enabled(window),
            "the field that took the focus next asks for the IME"
        );
        app.app_mut().set_focus(window, Some(first.editable));
        settle(&mut app);
        assert!(
            app.app().ime_enabled(window),
            "the frame that syncs the later field must leave the IME of the earlier field alone"
        );
        app.app_mut().set_focus(window, None);
        settle(&mut app);
        assert!(
            !app.app().ime_enabled(window),
            "with no editable field focused the window turns the IME off"
        );
    }

    #[test]
    fn ime_is_enabled_only_while_a_mutable_field_holds_the_focus() {
        let mut focused = harness(spec(), "");
        assert!(
            focused.app.app().ime_enabled(focused.window()),
            "the window starts with its IME on"
        );
        focused.focus();
        assert!(
            focused.app.app().ime_enabled(focused.window()),
            "a focused single line field asks for the IME"
        );
        focused.blur();
        assert!(
            !focused.app.app().ime_enabled(focused.window()),
            "the window stops asking for the IME once the field lets go"
        );

        let mut readonly = harness(
            InputSpec {
                readonly: true,
                ..spec()
            },
            "",
        );
        readonly.focus();
        assert!(
            !readonly.app.app().ime_enabled(readonly.window()),
            "a readonly field takes the focus without asking for the IME"
        );
    }

    #[test]
    fn releasing_a_disabled_input_reports_nothing_to_release() {
        let mut harness = harness(
            InputSpec {
                disabled: true,
                ..spec()
            },
            "abc",
        );
        assert_eq!(
            harness.node.release(harness.app.app_mut()),
            0,
            "a disabled field has nothing to detach"
        );
    }

    #[test]
    fn input_state_new_starts_with_a_collapsed_caret_at_the_end() {
        let state = InputState::new("hello");
        assert_eq!(state.text(), "hello");
        assert!(state.selection().is_collapsed());
        assert_eq!(state.caret(), 5);
        assert!(!state.is_focused());
        assert!(!state.is_composing());
        assert!(!state.can_undo());
        assert!(!state.can_redo());
        assert_eq!(state.error(), None);
        assert_eq!(state.generation, 0);
    }
}
