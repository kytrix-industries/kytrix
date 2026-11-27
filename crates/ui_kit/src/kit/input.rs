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
    error::EngineError,
    event::Action,
    interaction::{ScrollAxes, ScrollConfig, ScrollHandle},
    math::{LogicalPoint, LogicalPx, LogicalSize, px},
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
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions, detach_proxy},
        size::UnifiedSizable,
    },
    theme::{components::ControlSize, handle::ThemeHandle, role::Role},
};

const CORNER_RADIUS: f32 = 4.0;

const SCROLLBAR_GUTTER: f32 = 6.0;

const SCROLLBAR_FOCUS_OPACITY: f32 = 0.5;

const SINGLE_LINE_TRAILING_INSET: f32 = 5.0;

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
    clear_proxy: Cell<bool>,
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
                    padding_x,
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
        FieldAxis::SingleLine => StyleBuilder::new()
            .flex_none()
            .height_full()
            .padding_sides(px(0.0), px(SINGLE_LINE_TRAILING_INSET), px(0.0), px(0.0))
            .build(),
        FieldAxis::Multiline => {
            let mut style = StyleBuilder::new()
                .margin_sides(px(0.0), px(SCROLLBAR_GUTTER), px(0.0), px(0.0))
                .build();
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
    tree.set_press_focus_target(root, editable)
        .expect("input: the press host targets the editable it just built");

    InputNode {
        root,
        viewport,
        editable,
        scrollbar: RefCell::new(None),
        placeholder_state: *placeholder_text,
        state: *state,
        spec,
        clear: RefCell::new(None),
        clear_proxy: Cell::new(false),
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
    pub fn install(&self, app: &mut App) -> Result<(), EngineError> {
        let Some(window) = self.root.window() else {
            return Err(EngineError::StaleNode { node: self.root });
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
            self.install_scroll(app, window)?;
        }
        let _ = self.sync_layout(app, window);
        Ok(())
    }

    pub fn release(&self, app: &mut App) -> usize {
        let mut released = 0;
        if let Some(scrollbar) = self.scrollbar.borrow_mut().take() {
            released += scrollbar.release(app);
        }
        if let Some(button) = self.clear.borrow_mut().take() {
            self.detach_clear_proxy(app, &button);
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
        let mirrored = self.refresh_mirror(app, window, self.too_long_error(value));
        if changed {
            self.reset_horizontal_scroll(app);
        }
        if changed || mirrored {
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

    pub fn set_clearable(&self, app: &mut App, clearable: bool) -> bool {
        if self.clear.borrow().is_some() == clearable {
            return false;
        }
        let Some(window) = self.root.window() else {
            return false;
        };
        if !clearable {
            if let Some(button) = self.clear.borrow_mut().take() {
                self.detach_clear_proxy(app, &button);
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
        changed |= self.sync_scroll(app, window, snapshot.focused);
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

    fn refresh_mirror(&self, app: &mut App, window: WindowId, error: Option<InputError>) -> bool {
        let Ok(snapshot) = app.text_edit_snapshot(window, self.editable) else {
            return false;
        };
        app.write(&self.state, InputState::mirrored(&snapshot, error))
    }

    fn too_long_error(&self, text: &str) -> Option<InputError> {
        let max = self.spec.max_length?;
        let cleaned = sanitize_text(text, self.spec.multiline);
        (grapheme_count(cleaned.as_str()) > max).then_some(InputError::TooLong { max })
    }

    fn install_scroll(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let tokens = self.theme.tokens();
        let size = tokens.for_size(self.spec.size);
        let axis = FieldAxis::of(&self.spec);
        let config = match axis {
            FieldAxis::SingleLine => ScrollConfig {
                axes: ScrollAxes::Horizontal,
                line_height: size.font_size,
            },
            FieldAxis::Multiline => ScrollConfig {
                axes: ScrollAxes::Vertical,
                line_height: self
                    .metrics
                    .get()
                    .map_or(size.font_size, |metrics| metrics.line_height),
            },
        };
        if axis == FieldAxis::SingleLine {
            let handle = app.make_scroll(window, self.viewport, config)?;
            self.scroll_handle.set(Some(handle));
            return Ok(());
        }
        let scrollbar = ScrollBar::new(
            app,
            self.theme.clone(),
            format_smolstr!("{}.scrollbar", self.spec.name),
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default()
                .hover()
                .inset(size.padding_x)
                .focus_opacity(SCROLLBAR_FOCUS_OPACITY)
                .tab_stop(false),
        );
        {
            let Some(target) = app.window_mut(window) else {
                return Err(EngineError::UnknownWindow { window });
            };
            scrollbar.build(target.tree_mut(), self.root)?;
        }
        let handle = match app.make_scroll(window, self.viewport, config) {
            Ok(handle) => handle,
            Err(error) => {
                discard_scrollbar(app, window, &scrollbar);
                return Err(error);
            }
        };
        self.scroll_handle.set(Some(handle));
        if let Err(error) = scrollbar.attach(app, window, self.viewport) {
            discard_scrollbar(app, window, &scrollbar);
            let _ = app.clear_scroll(&handle);
            self.scroll_handle.set(None);
            return Err(error);
        }
        *self.scrollbar.borrow_mut() = Some(scrollbar);
        Ok(())
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

    fn sync_scroll(&self, app: &mut App, window: WindowId, focused: bool) -> bool {
        let Some(handle) = self.scroll_handle.get() else {
            return false;
        };
        let mut changed = false;
        if let Some(metrics) = self.metrics.get() {
            let _ = app.set_scroll_line_height(&handle, metrics.line_height);
        }
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
            changed |= scrollbar.set_focused(app, focused);
            changed |= scrollbar.sync(app).unwrap_or(false);
        }
        changed
    }

    fn reset_horizontal_scroll(&self, app: &mut App) {
        let Some(handle) = self.scroll_handle.get() else {
            return;
        };
        let Some(state) = app.scroll_state(&handle) else {
            return;
        };
        if state.axes != ScrollAxes::Horizontal || state.offset.x == LogicalPx::ZERO {
            return;
        }
        let _ = app.scroll_to(&handle, LogicalPoint::new(px(0.0), state.offset.y));
    }

    fn sync_clear_button(&self, app: &mut App) -> bool {
        let Some(button) = self.clear.borrow().clone() else {
            return false;
        };
        let Some(node) = button.node() else {
            return false;
        };
        self.attach_clear_proxy(app, node);
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

    fn attach_clear_proxy(&self, app: &mut App, node: NodeId) {
        if self.clear_proxy.get() {
            return;
        }
        let Some(window) = node.window() else {
            return;
        };
        if app.scroll_handle(window, self.viewport).is_none() {
            return;
        }
        if app
            .set_scroll_proxy(window, node, Some(self.viewport))
            .is_ok()
        {
            self.clear_proxy.set(true);
        }
    }

    fn detach_clear_proxy(&self, app: &mut App, button: &Button) {
        if !self.clear_proxy.get() {
            return;
        }
        self.clear_proxy.set(false);
        let Some(node) = button.node() else {
            return;
        };
        let Some(window) = node.window() else {
            return;
        };
        detach_proxy(app, window, node);
    }
}

fn discard_scrollbar(app: &mut App, window: WindowId, scrollbar: &ScrollBar) {
    scrollbar.release(app);
    if let Some(track) = scrollbar.nodes().0 {
        let _ = app.remove_node(window, track);
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
    padding_x: LogicalPx,
) -> LogicalPx {
    px(lines * line_height.value() + 2.0 * (border.value() + padding_x.value()))
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
        TextWrap::BreakWord
    } else {
        TextWrap::NoWrap
    };
    TextStyle::new(size.font_size)
        .with_wrap(wrap)
        .with_alignment(TextAlign::Start)
        .with_overflow(TextOverflow::Clip, 0)
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
        error::EngineError,
        event::{Action, ImeEvent, InputEvent, KeyCode, Modifiers, PointerButton},
        interaction::{ScrollAxes, ScrollConfig, ScrollHandle},
        math::{LogicalPoint, LogicalPx, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeKind},
        paint::{
            color::{Color, ColorResolver},
            style::Style,
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
        node.install(app.app_mut()).expect("install");
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

        fn click_point(&mut self, point: LogicalPoint) {
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
            self.drain();
        }

        fn focused(&self) -> Option<NodeId> {
            self.app.app().focused(self.window())
        }

        fn frame_index(&self) -> ui_core::window::FrameIndex {
            self.app
                .app()
                .window(self.window())
                .expect("窗口存在")
                .frame_index()
        }

        fn ime_cursor_area(&self) -> Option<ui_core::math::LogicalRect> {
            self.app
                .app()
                .ime_cursor_area(self.window())
                .expect("窗口存在")
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
    fn clicking_the_control_chrome_focuses_the_editable_without_blurring_it() {
        let hits = Rc::new(Cell::new(0));
        let submit = Action::Custom(SmolStr::new_static("test.chrome.submit"));
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
        harness.drain();
        assert_eq!(harness.focused(), None, "前置：控件未聚焦");

        let root = harness.root_bounds();
        let center_y = root.origin.y + root.size.height * 0.5;
        let border = LogicalPoint::new(root.origin.x + px(0.5), center_y);
        let padding = LogicalPoint::new(root.origin.x + px(3.0), center_y);

        harness.click_point(padding);
        assert_eq!(
            harness.focused(),
            Some(harness.node.editable),
            "点击内边距必须让 editable 获得焦点"
        );
        assert_eq!(hits.get(), 0, "聚焦不得触发 submit_on_blur");
        assert!(harness.state().is_focused(), "镜像同步到聚焦态");
        assert!(harness.app.app().ime_enabled(harness.window()));
        let ime_area = harness.ime_cursor_area();
        assert!(ime_area.is_some(), "聚焦后可编辑节点必须交付 IME 光标矩形");
        let caret = harness.state().caret();
        let settled = harness.frame_index();

        harness.click_point(border);
        assert_eq!(
            harness.focused(),
            Some(harness.node.editable),
            "点击边框必须保持 editable 焦点"
        );
        assert_eq!(hits.get(), 0, "边框点击不得触发 submit_on_blur");
        assert!(
            harness.app.app().ime_enabled(harness.window()),
            "边框点击不得关闭 IME"
        );
        assert_eq!(
            harness.ime_cursor_area(),
            ime_area,
            "边框点击不得重开/清空 IME 光标矩形"
        );
        assert_eq!(
            harness.state().caret(),
            caret,
            "边框点击按点落 caret：与上一落点相同则不改变光标位置"
        );
        assert_eq!(
            harness.frame_index().get(),
            settled.get() + 1,
            "边框点击只允许一次落点重绘（blur→focus 的中间态会多排帧）"
        );
        harness.app.drain().expect("稳定帧");
        assert_eq!(
            harness.frame_index().get(),
            settled.get() + 1,
            "边框点击后必须回到静止，不得持续出帧"
        );

        let editable = harness.editable_bounds();
        harness.click_point(LogicalPoint::new(
            editable.origin.x + editable.size.width * 0.5,
            editable.origin.y + editable.size.height * 0.5,
        ));
        assert_eq!(
            harness.focused(),
            Some(harness.node.editable),
            "点击文本区仍聚焦 editable"
        );
        assert_eq!(hits.get(), 0);

        harness.click_point(LogicalPoint::new(
            root.origin.x + root.size.width * 0.5,
            root.max().y + px(20.0),
        ));
        assert_eq!(harness.focused(), None, "点击控件外必须照旧失焦");
        assert_eq!(hits.get(), 1, "真正的失焦只提交一次");
    }

    #[test]
    fn pressing_the_control_chrome_lands_the_caret_at_the_pressed_point() {
        let mut harness = harness(spec(), "abcdef");
        harness.drain();
        harness.focus();
        assert_eq!(harness.state().caret(), 6, "前置：caret 在文本末尾");
        let root = harness.root_bounds();
        let point = LogicalPoint::new(
            root.origin.x + px(3.0),
            root.origin.y + root.size.height * 0.5,
        );
        harness.click_point(point);
        assert_eq!(
            harness.focused(),
            Some(harness.node.editable),
            "点击内边距必须让 editable 获得焦点"
        );
        assert_eq!(
            harness.state().caret(),
            0,
            "按点落 caret：左侧内边距落在文本起点"
        );
    }

    #[test]
    fn clicking_the_multiline_padding_and_gutter_focuses_the_editable() {
        let mut harness = harness(multiline_spec(Some(3)), "one\ntwo\nthree\nfour");
        harness.drain();
        let root = harness.root_bounds();
        let editable = harness.editable_bounds();
        assert!(
            editable.max().x + px(3.0) < root.max().x,
            "前置：文本右侧必须有滚动条 gutter"
        );
        let gutter = LogicalPoint::new(
            editable.max().x + px(3.0),
            editable.origin.y + editable.size.height * 0.5,
        );
        harness.click_point(gutter);
        assert_eq!(
            harness.focused(),
            Some(harness.node.editable),
            "点击滚动条 gutter 必须聚焦 editable"
        );
        let top_padding = LogicalPoint::new(
            root.origin.x + root.size.width * 0.5,
            root.origin.y + px(3.0),
        );
        harness.click_point(top_padding);
        assert_eq!(
            harness.focused(),
            Some(harness.node.editable),
            "点击多行顶部内边距必须保持 editable 焦点"
        );
    }

    fn tab_cycle_around_input(harness: &mut Harness) -> Vec<NodeId> {
        let window = harness.window();
        let tree_root = harness
            .app
            .app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .root();
        let before = NodeBuilder::boxed()
            .on_click(Action::custom("test.tab.before"))
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(window)
                    .expect("窗口存在")
                    .tree_mut(),
                tree_root,
            )
            .expect("前置按钮");
        let after = NodeBuilder::boxed()
            .on_click(Action::custom("test.tab.after"))
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(window)
                    .expect("窗口存在")
                    .tree_mut(),
                tree_root,
            )
            .expect("后置按钮");
        harness.drain();
        assert_eq!(harness.focused(), None, "前置：没有焦点");
        let sequence: Vec<NodeId> = (0..4)
            .map(|_| {
                harness
                    .app
                    .app_mut()
                    .focus_next(window)
                    .expect("Tab 必须落在候选节点上")
            })
            .collect();
        assert_eq!(
            sequence,
            vec![harness.node.editable, before, after, harness.node.editable],
            "输入控件只有 editable 一个 Tab 停靠点（root/viewport/滚动条不进入序列）"
        );
        sequence
    }

    #[test]
    fn the_input_contributes_exactly_one_tab_stop() {
        let mut single = harness(spec(), "");
        tab_cycle_around_input(&mut single);
        let mut multiline = harness(multiline_spec(Some(3)), "");
        tab_cycle_around_input(&mut multiline);
        assert!(
            multiline.node.scrollbar.borrow().is_some(),
            "前置：多行输入带 overlay 滚动条（其节点也不得进入 Tab 序）"
        );
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
    fn error_states_expose_stable_i18n_keys() {
        let mut state = InputState::new("");
        state.set_error(Some(InputError::Empty));
        assert_eq!(
            state.error().map(InputError::i18n_key),
            Some("input.error.empty")
        );
        state.set_error(Some(InputError::TooLong { max: 3 }));
        assert_eq!(
            state.error().map(InputError::i18n_key),
            Some("input.error.tooLong")
        );
        state.set_error(Some(InputError::Invalid {
            i18n_key: SmolStr::new_static("input.error.duplicate"),
        }));
        assert_eq!(
            state.error().map(InputError::i18n_key),
            Some("input.error.duplicate")
        );
        state.set_error(None);
        assert!(state.error().is_none());
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
    fn an_external_placeholder_write_survives_an_update_value_in_the_same_frame() {
        let mut harness = harness(spec(), "");
        harness.drain();
        let external = SmolStr::new_static("外部写入的占位");
        assert!(
            harness
                .app
                .app_mut()
                .write(&harness.placeholder, external.clone())
        );
        assert!(harness.node.update_value(harness.app.app_mut(), "abc"));
        harness.drain();
        assert_eq!(
            harness.a11y_node().placeholder.as_deref(),
            Some(external.as_str()),
            "宿主直接写 placeholder 信号后，同帧 update_value 不得吞掉同步"
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
        node.install(app.app_mut()).expect("install");
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
                    axes: ui_core::interaction::ScrollAxes::Vertical,
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
                        .height(px(40.0))
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
        node.install(app.app_mut()).expect("install");
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
        let visible = app.probe().bounds(wrapper).expect("滚动容器已布局");
        assert!(
            from.y >= visible.origin.y
                && to.y >= visible.origin.y
                && from.y <= visible.max().y
                && to.y <= visible.max().y,
            "前置：注入点必须是滚动容器可见区内的窗口坐标（from={from:?} to={to:?} visible={visible:?}）"
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
        let steady = harness.app.app().signal_write_calls();
        assert!(!harness.sync(), "稳态 sync 不写信号");
        assert!(!harness.sync());
        assert_eq!(
            harness.app.app().signal_write_calls(),
            steady,
            "generation 不变时 sync 不得调用 App::write"
        );
        harness.type_text("a");
        assert_eq!(harness.state().generation, harness.snapshot().generation);
        let steady = harness.app.app().signal_write_calls();
        assert!(!harness.sync(), "同一世代不重复写镜像");
        assert_eq!(
            harness.app.app().signal_write_calls(),
            steady,
            "同一世代不得调用 App::write"
        );
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
        let pixels = harness.app.rendered().is_some();
        if !pixels {
            eprintln!(
                "跳过输入框 caret/选区的像素断言：本会话没有可用的呈现后端（无 Metal/Vulkan 设备）"
            );
        }
        let light = harness.theme.theme_for(Appearance::Light);
        let caret = resolved(light.role(Role::FgDefault));
        let selection = resolved(light.components().input.selection);
        let quads = harness.quads();
        if pixels {
            assert!(
                quads.iter().any(|quad| quad.fill.components == caret),
                "浅色 caret 颜色画进可编辑节点"
            );
            assert!(
                quads.iter().any(|quad| quad.fill.components == selection),
                "浅色选区颜色画进可编辑节点"
            );
        }

        harness
            .theme
            .set_appearance(harness.app.app_mut(), Appearance::Dark);
        harness.drain();
        let dark = harness.theme.theme_for(Appearance::Dark);
        let dark_caret = resolved(dark.role(Role::FgDefault));
        let dark_selection = resolved(dark.components().input.selection);
        assert_ne!(dark_caret, caret);
        let quads = harness.quads();
        if pixels {
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
        }
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
    fn the_placeholder_signal_the_caller_holds_reaches_the_engine() {
        let mut harness = harness(spec(), "");
        harness.drain();
        assert_eq!(
            harness.a11y_node().placeholder.as_deref(),
            Some("请输入"),
            "前置：初始占位已经到达引擎"
        );
        let external = SmolStr::new_static("宿主写入的占位");
        assert!(
            harness
                .app
                .app_mut()
                .write(&harness.placeholder, external.clone()),
            "宿主持有的是同一个 placeholder 信号"
        );
        harness.drain();
        assert_eq!(
            harness.a11y_node().placeholder.as_deref(),
            Some(external.as_str()),
            "宿主直接写 placeholder 信号后，下一帧必须到达引擎"
        );
        assert_eq!(
            harness
                .app
                .app()
                .read(&harness.node.placeholder_state)
                .as_str(),
            external.as_str(),
            "kit 镜像的正是宿主写入的那个信号"
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
    fn an_error_only_update_value_requests_a_frame() {
        let mut harness = harness(max_spec(3), "");
        harness.drain();
        harness.drain();
        assert!(harness.node.update_value(harness.app.app_mut(), "abc"));
        harness.drain();
        harness.drain();
        assert_eq!(harness.state().error(), None, "前置：三个字符正好在上限内");
        let idle = harness.frame_index();

        assert!(
            !harness.node.update_value(harness.app.app_mut(), "abcd"),
            "引擎文本已被截成上限，不算改变"
        );
        assert_eq!(
            harness.state().error(),
            Some(&InputError::TooLong { max: 3 }),
            "只有错误态变化"
        );
        let mut frames = 0;
        for _ in 0..3 {
            harness.app.drain().expect("帧");
            frames = harness.frame_index().get() - idle.get();
            if frames > 0 {
                break;
            }
        }
        assert_eq!(frames, 1, "只改错误态也必须恰排一帧");
        let rejected = {
            let light = harness.theme.theme_for(Appearance::Light);
            resolved(light.role(Role::StatusDangerBorder))
        };
        assert_eq!(
            harness
                .style_patch(harness.node.root)
                .border
                .map(|border| resolved(border.color)),
            Some(rejected),
            "错误边框必须在同一帧到达控件"
        );
    }

    #[test]
    fn max_length_counts_grapheme_clusters_for_typed_and_programmatic_text() {
        let mut harness = harness(max_spec(2), "");
        assert!(harness.node.update_value(harness.app.app_mut(), "👨‍👩‍👧‍👦a"));
        assert_eq!(
            harness.engine_value(),
            "👨‍👩‍👧‍👦a",
            "one family emoji is one grapheme, so both clusters fit"
        );
        assert_eq!(harness.state().error(), None);

        assert!(harness.node.update_value(harness.app.app_mut(), "abc"));
        assert_eq!(harness.engine_value(), "ab");
        assert_eq!(
            harness.state().error(),
            Some(&InputError::TooLong { max: 2 }),
            "the request that had to be clipped is reported"
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
        node.install(app.app_mut()).expect("install");
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
            node.install(app.app_mut()).expect("install");
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
    fn moving_the_focus_to_a_focusable_non_editable_node_turns_the_ime_off() {
        let mut harness = harness(spec(), "abc");
        harness.focus();
        let window = harness.window();
        assert!(
            harness.app.app().ime_enabled(window),
            "前置：可编辑节点上的焦点打开了 IME"
        );
        assert!(harness.ime_cursor_area().is_some());
        let root = harness
            .app
            .app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .root();
        let button = NodeBuilder::boxed()
            .on_click(Action::custom("test.ime.non-editable"))
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(window)
                    .expect("窗口存在")
                    .tree_mut(),
                root,
            )
            .expect("可聚焦按钮");
        assert!(
            harness.app.app_mut().set_focus(window, Some(button)),
            "非可编辑但可聚焦的节点可以拿走焦点"
        );
        harness.drain();
        assert_eq!(harness.focused(), Some(button), "焦点落在非可编辑节点上");
        assert!(
            !harness.app.app().ime_enabled(window),
            "焦点移到可聚焦但非可编辑节点必须关闭窗口 IME"
        );
        assert_eq!(harness.ime_cursor_area(), None, "失焦必须清掉 IME 光标矩形");

        assert!(
            harness
                .app
                .app_mut()
                .set_focus(window, Some(harness.node.editable))
        );
        harness.drain();
        assert!(
            harness.app.app().ime_enabled(window),
            "焦点回到可编辑节点必须重新打开 IME"
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

    fn settle(harness: &mut Harness) {
        for _ in 0..10 {
            harness.app.advance(std::time::Duration::from_millis(20));
            harness.drain();
        }
    }

    fn node_bounds(harness: &Harness, node: NodeId) -> ui_core::math::LogicalRect {
        harness.app.probe().bounds(node).expect("节点已布局")
    }

    fn vertical_handle(harness: &Harness) -> ScrollHandle {
        let window = harness.window();
        harness
            .app
            .app()
            .scroll_handle(window, harness.node.viewport)
            .expect("多行 viewport 有滚动槽")
    }

    #[test]
    fn visible_lines_sizes_the_field_and_the_overlay_scrollbar_together() {
        let mut harness = harness(multiline_spec(Some(3)), "one\ntwo\nthree\nfour\nfive\nsix");
        harness.drain();
        harness.drain();
        let metrics = harness.metrics();
        let tokens = harness.theme.tokens();
        let size = tokens.for_size(ControlSize::Small);
        let expected_height = 3.0 * metrics.line_height.value()
            + 2.0 * (tokens.border_width.value() + size.padding_x.value());
        assert!(
            (harness.root_bounds().size.height.value() - expected_height).abs() < 0.5,
            "3 行高度={expected_height}，实际 {}",
            harness.root_bounds().size.height.value()
        );

        let viewport = node_bounds(&harness, harness.node.viewport);
        let editable = harness.editable_bounds();
        assert!(
            (editable.size.width.value() - (viewport.size.width.value() - 6.0)).abs() < 0.5,
            "文本节点宽度 = viewport - 6px gutter：{} vs {}",
            editable.size.width.value(),
            viewport.size.width.value()
        );

        let bar = harness
            .node
            .scrollbar
            .borrow()
            .clone()
            .expect("多行溢出时存在滚动条");
        assert!(bar.scrollable(harness.app.app()), "内容 6 行 > 可视 3 行");
        let (track, _) = bar.nodes();
        let track = node_bounds(&harness, track.expect("轨道"));
        assert!((track.size.width.value() - 4.0).abs() < 0.01, "4px overlay");
        assert!(
            (track.size.height.value() - viewport.size.height.value()).abs() < 0.5,
            "轨道高度 = viewport：{} vs {}",
            track.size.height.value(),
            viewport.size.height.value()
        );
        assert!(
            (track.origin.y - viewport.origin.y).value().abs() < 0.5
                && (track.max().x - viewport.max().x).value().abs() < 0.5,
            "轨道贴 viewport 右侧内边缘：{track:?} vs {viewport:?}"
        );

        let state = harness
            .app
            .app()
            .scroll_state(&vertical_handle(&harness))
            .expect("滚动状态");
        let thumb = bar.thumb_rect(harness.app.app()).expect("thumb");
        let expected_thumb =
            track.size.height * (state.viewport.height.value() / state.content.height.value());
        assert!(
            (thumb.size.height - expected_thumb).value().abs() < 0.5,
            "thumb 高度 = viewport/content × track：{:?} vs {expected_thumb:?}",
            thumb.size.height
        );
        assert!(state.content.height > state.viewport.height);
    }

    #[test]
    fn focusing_an_overflowing_multiline_keeps_the_scrollbar_semi_visible() {
        let mut harness = harness(multiline_spec(Some(3)), "one\ntwo\nthree\nfour\nfive\nsix");
        harness.drain();
        settle(&mut harness);
        let bar = harness.node.scrollbar.borrow().clone().expect("滚动条");
        assert_eq!(bar.opacity(harness.app.app()), 0.0, "未聚焦未悬停时隐藏");

        harness.focus();
        settle(&mut harness);
        let focused = bar.opacity(harness.app.app());
        assert!(
            (focused - 0.5).abs() < 0.01,
            "聚焦时常驻低不透明度（0.5），实际 {focused}"
        );

        harness.blur();
        settle(&mut harness);
        assert_eq!(bar.opacity(harness.app.app()), 0.0, "失焦后淡出");

        let (track, _) = bar.nodes();
        let track = node_bounds(&harness, track.expect("轨道"));
        let hover = LogicalPoint::new(
            track.origin.x + track.size.width * 0.5,
            track.origin.y + track.size.height * 0.5,
        );
        let _ = harness.app.inject(ui_core::event::InputEvent::pointer_move(
            hover,
            ui_core::event::Modifiers::empty(),
        ));
        settle(&mut harness);
        assert!(
            bar.opacity(harness.app.app()) >= 0.99,
            "hover 时淡入到 1.0，实际 {}",
            bar.opacity(harness.app.app())
        );
    }

    #[test]
    fn a_one_line_field_keeps_its_overlay_thumb_draggable() {
        let content = (0..40)
            .map(|line| format!("line{line}\n"))
            .collect::<String>();
        let mut harness = harness(multiline_spec(Some(1)), content.as_str());
        harness.drain();
        harness.drain();
        let bar = harness
            .node
            .scrollbar
            .borrow()
            .clone()
            .expect("多行溢出时存在滚动条");
        assert!(bar.scrollable(harness.app.app()), "前置：内容远超一行");
        let (track_node, _) = bar.nodes();
        let track = node_bounds(&harness, track_node.expect("轨道"));
        assert!(
            track.size.height.value() < 24.0,
            "前置：visible_lines=1 的极短轨道，实际 {}",
            track.size.height.value()
        );
        let thumb = bar.thumb_rect(harness.app.app()).expect("thumb");
        assert!(
            thumb.size.height >= px(2.0),
            "极短轨道的最小滑块不得塌成零高度：{}",
            thumb.size.height.value()
        );
        assert!(
            track.size.height - thumb.size.height >= px(2.0),
            "极短轨道必须留下拖拽行程：track {} thumb {}",
            track.size.height.value(),
            thumb.size.height.value()
        );

        let handle = vertical_handle(&harness);
        let origin = LogicalPoint::new(
            track.origin.x + track.size.width * 0.5,
            track.origin.y + thumb.origin.y + thumb.size.height * 0.5,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            origin,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let promoted = LogicalPoint::new(origin.x, origin.y + 10.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(promoted, Modifiers::empty()));
        let moved = LogicalPoint::new(origin.x, origin.y + 20.0);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(moved, Modifiers::empty()));
        let _ = harness.app.inject(InputEvent::pointer_up(
            moved,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let offset = harness
            .app
            .app()
            .scroll_state(&handle)
            .expect("滚动状态")
            .offset
            .y;
        assert!(
            offset > px(0.0),
            "拖拽 visible_lines=1 的滑块必须改变滚动偏移：{offset}"
        );
    }

    #[test]
    fn the_overlay_scrollbar_never_covers_the_glyphs() {
        let mut harness = harness(multiline_spec(Some(3)), &"word ".repeat(40));
        harness.app.invalidate_all_lanes();
        harness.drain();
        let editable = harness.editable_bounds();
        let bar = harness.node.scrollbar.borrow().clone().expect("滚动条");
        let (track, _) = bar.nodes();
        let track = node_bounds(&harness, track.expect("轨道"));
        let glyph_right = harness
            .app
            .rendered()
            .expect("已提交")
            .primitives
            .iter()
            .filter(|primitive| {
                primitive.node == harness.node.editable && primitive.kind == PrimitiveKind::GlyphRun
            })
            .map(|primitive| primitive.bounds.max().x)
            .fold(px(0.0), LogicalPx::max);
        assert!(
            editable.max().x <= track.origin.x + 0.5,
            "文本节点（含 gutter）不得伸入滚动条：{editable:?} vs {track:?}"
        );
        assert!(
            glyph_right <= track.origin.x + 0.5,
            "字形不得被滚动条覆盖：{glyph_right:?} vs {track:?}"
        );
    }

    #[test]
    fn a_single_line_field_scrolls_horizontally_without_a_scrollbar() {
        let mut harness = harness(spec(), "");
        harness
            .node
            .update_value(harness.app.app_mut(), "0123456789".repeat(40).as_str());
        harness.drain();
        assert!(
            harness.node.scrollbar.borrow().is_none(),
            "单行不建 ScrollBar（结构不变）"
        );
        let window = harness.window();
        let handle = harness
            .app
            .app()
            .scroll_handle(window, harness.node.viewport)
            .expect("单行 viewport 有水平滚动槽");
        let state = harness.app.app().scroll_state(&handle).expect("滚动状态");
        assert_eq!(state.axes, ScrollAxes::Horizontal);
        let tree = harness.app.app().window(window).expect("窗口").tree();
        assert_eq!(
            tree.get(harness.node.root).expect("root").children().len(),
            1,
            "单行 root 只有 viewport，不新增 overlay 结构"
        );
    }

    #[test]
    fn update_value_resets_the_single_line_horizontal_offset() {
        let mut harness = harness(spec(), "");
        let long = "0123456789".repeat(60);
        harness.node.update_value(harness.app.app_mut(), &long);
        harness.drain();
        harness.focus();
        harness.drain();
        let window = harness.window();
        let handle = harness
            .app
            .app()
            .scroll_handle(window, harness.node.viewport)
            .expect("水平滚动槽");
        let revealed = harness
            .app
            .app()
            .scroll_state(&handle)
            .expect("滚动状态")
            .offset
            .x;
        assert!(
            revealed > px(0.0),
            "末尾 caret 必须横向 reveal：{revealed:?}"
        );

        harness.node.update_value(harness.app.app_mut(), "");
        harness.drain();
        assert_eq!(
            harness
                .app
                .app()
                .scroll_state(&handle)
                .expect("滚动状态")
                .offset
                .x,
            px(0.0),
            "清空值后水平偏移归零"
        );
        assert_eq!(harness.engine_value(), "");
        assert_eq!(harness.text(), "");
    }

    fn viewport_handle(harness: &Harness) -> ScrollHandle {
        let window = harness.window();
        harness
            .app
            .app()
            .scroll_handle(window, harness.node.viewport)
            .expect("viewport 有滚动槽")
    }

    fn assert_newest_character_revealed(harness: &Harness) {
        let handle = viewport_handle(harness);
        let state = harness.app.app().scroll_state(&handle).expect("滚动状态");
        let byte = harness.engine_display().len();
        let caret = harness.caret_rect(byte);
        let viewport = node_bounds(harness, harness.node.viewport);
        assert!(
            caret.origin.x >= viewport.origin.x && caret.max().x <= viewport.max().x,
            "最新输入的字符的 caret 必须完整落在视口内（display byte={byte}）：{caret:?} vs {viewport:?}"
        );
        let editable_width = harness.editable_bounds().size.width;
        assert!(
            (state.content.width - editable_width).abs() <= px(0.01),
            "滚动槽内容宽必须等于已布局 editable 宽度（display byte={byte}）：{state:?} vs {editable_width}"
        );
        let scroll_min = state.content.width - state.viewport.width;
        assert!(
            (state.offset.x - scroll_min).abs() <= px(0.01),
            "单行 reveal 必须同帧把 offset 推到内容右端（display byte={byte}）：{state:?} vs {scroll_min}"
        );
    }

    #[test]
    fn typing_into_an_overflowing_single_line_field_reveals_the_newest_character_in_one_frame() {
        let initial = "0".repeat(40);
        let mut harness = harness_at_width(120.0, spec(), initial.as_str());
        harness.focus();
        let settled = harness
            .app
            .app()
            .scroll_state(&viewport_handle(&harness))
            .expect("滚动状态");
        assert!(
            settled.offset.x > px(0.0),
            "前置：聚焦后单行内容必须已溢出并 reveal（offset={:?}）",
            settled.offset.x
        );
        for step in 0..20 {
            harness.type_text("1");
            assert_eq!(
                harness.engine_value().len(),
                initial.len() + 1 + step,
                "键入必须落到引擎文本"
            );
            assert_newest_character_revealed(&harness);
        }
    }

    #[test]
    fn typing_wide_cjk_characters_reveals_the_newest_character_in_one_frame() {
        let initial = "0".repeat(40);
        let mut harness = harness_at_width(120.0, spec(), initial.as_str());
        harness.focus();
        for step in 0..20 {
            harness.type_text("。");
            assert_eq!(
                harness.engine_value().len(),
                initial.len() + 3 * (step + 1),
                "CJK 键入必须落到引擎文本"
            );
            assert_newest_character_revealed(&harness);
        }
    }

    #[test]
    fn typing_into_an_overflowing_masked_field_reveals_the_newest_character_in_one_frame() {
        let initial = "0".repeat(40);
        let mut harness = harness_at_width(120.0, masked_spec(), initial.as_str());
        harness.focus();
        for step in 0..20 {
            harness.type_text("a");
            assert_eq!(
                harness.engine_value().len(),
                initial.len() + 1 + step,
                "掩码键入必须落到引擎真实文本"
            );
            assert_newest_character_revealed(&harness);
        }
    }

    #[test]
    fn typing_at_the_bottom_of_a_visible_lines_field_keeps_the_caret_visible_in_one_frame() {
        let mut harness = harness_at_width(160.0, multiline_spec(Some(3)), "one\ntwo\nthree");
        harness.focus();
        let start = harness
            .app
            .app()
            .scroll_state(&viewport_handle(&harness))
            .expect("滚动状态");
        assert!(
            (start.content.height - start.viewport.height).abs() <= px(0.5),
            "前置：初始文本恰好填满 3 行视口（{start:?}）"
        );
        for _ in 0..40 {
            harness.type_text("。");
            let byte = harness.engine_display().len();
            let caret = harness.caret_rect(byte);
            let viewport = node_bounds(&harness, harness.node.viewport);
            assert!(
                caret.origin.x >= viewport.origin.x
                    && caret.max().x <= viewport.max().x
                    && caret.origin.y >= viewport.origin.y
                    && caret.max().y <= viewport.max().y,
                "底部继续输入必须同帧把 caret 留在视口内（display byte={byte}）：{caret:?} vs {viewport:?}"
            );
        }
        let state = harness
            .app
            .app()
            .scroll_state(&viewport_handle(&harness))
            .expect("滚动状态");
        assert!(
            state.content.height > start.content.height && state.offset.y > px(0.0),
            "结论：连续输入必须让多行内容变高并触发纵向 reveal（{state:?} vs {start:?}）"
        );
    }

    #[test]
    fn dragging_a_multiline_scrolls_by_the_engine_line_height() {
        let mut harness = harness(multiline_spec(Some(3)), "one\ntwo\nthree\nfour\nfive\nsix");
        harness.drain();
        harness.drain();
        let metrics = harness.metrics();
        let tokens = harness.theme.tokens();
        let font_size = tokens.for_size(ControlSize::Small).font_size;
        assert!(
            metrics.line_height > font_size,
            "前置：真实行高必须大于字号（否则回退不可判别）：{} vs {font_size:?}",
            metrics.line_height
        );
        let window = harness.window();
        let handle = harness
            .app
            .app()
            .scroll_handle(window, harness.node.viewport)
            .expect("多行滚动槽");
        let viewport = node_bounds(&harness, harness.node.viewport);
        let start = LogicalPoint::new(viewport.origin.x + px(10.0), viewport.origin.y + px(5.0));
        let _ = harness.app.inject(InputEvent::pointer_down(
            start,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let far = LogicalPoint::new(start.x, viewport.max().y + px(40.0));
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(far, Modifiers::empty()));
        let offset = harness
            .app
            .app()
            .scroll_state(&handle)
            .expect("滚动状态")
            .offset
            .y;
        assert!(
            (offset - metrics.line_height * 2.0).value().abs() <= 0.5,
            "拖拽自动滚动步长上限必须是 2×引擎行高（{}），实际 {offset:?}（字号回退会是 {:?}）",
            metrics.line_height * 2.0,
            font_size * 2.0
        );
    }

    fn harness_at_width(width: f32, spec: InputSpec, initial: &str) -> Harness {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let state = app.app_mut().signal(InputState::new(initial));
        let placeholder = app.app_mut().signal(spec.placeholder.clone());
        let spec = Arc::new(spec);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let host = {
            let tree = app
                .app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut();
            NodeBuilder::boxed()
                .style(StyleBuilder::new().width(px(width)).build())
                .build(tree, root)
                .expect("宿主盒")
        };
        let node = build(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .tree_mut(),
            host,
            Arc::clone(&spec),
            &theme,
            &state,
            &placeholder,
        );
        node.install(app.app_mut()).expect("install");
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

    fn editable_glyph_right(harness: &Harness) -> LogicalPx {
        harness
            .app
            .rendered()
            .expect("已提交")
            .primitives
            .iter()
            .filter(|primitive| {
                primitive.node == harness.node.editable && primitive.kind == PrimitiveKind::GlyphRun
            })
            .map(|primitive| primitive.bounds.max().x)
            .fold(px(0.0), LogicalPx::max)
    }

    #[test]
    fn cjk_punctuation_typed_into_a_multiline_field_wraps_without_horizontal_overflow() {
        let mut harness = harness_at_width(160.0, multiline_spec(Some(3)), "");
        harness.focus();
        let window = harness.window();
        let handle = harness
            .app
            .app()
            .scroll_handle(window, harness.node.viewport)
            .expect("多行滚动槽");
        for step in 0..40 {
            harness.type_text("。");
            let state = harness.app.app().scroll_state(&handle).expect("滚动状态");
            assert_eq!(
                state.offset.x,
                px(0.0),
                "多行输入不得横向滚动（第 {step} 个字符）：{state:?}"
            );
            let metrics = harness.metrics();
            assert!(
                (metrics.content_height - metrics.line_height * metrics.line_count as f32).abs()
                    <= px(0.5),
                "内容高必须等于行高之和（第 {step} 个字符）：{metrics:?}"
            );
            let editable = harness.editable_bounds();
            let glyph_right = editable_glyph_right(&harness);
            assert!(
                glyph_right <= editable.max().x + 1.0,
                "字形不得越过文本盒（第 {step} 个字符）：{glyph_right} vs {editable:?}"
            );
        }
        let metrics = harness.metrics();
        assert!(
            metrics.line_count >= 3,
            "40 个句号在 160px 宿主内必须折成至少 3 行：{metrics:?}"
        );
        assert!(
            metrics.content_height > metrics.line_height * 3.0,
            "内容必须超过 visible_lines=3 的视口高度：{metrics:?}"
        );
        let bar = harness.node.scrollbar.borrow().clone().expect("多行滚动条");
        assert!(
            bar.scrollable(harness.app.app()),
            "内容超过 3 行后滚动条必须可滚动"
        );
    }

    fn page_harness(spec: InputSpec, initial: &str) -> (TestApp, InputNode, ScrollHandle) {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let page = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(px(400.0))
                        .height(px(300.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("page")
        };
        let page_handle = app
            .app_mut()
            .make_scroll(window, page, ScrollConfig::default())
            .expect("page scroll");
        app.app_mut()
            .set_scroll_content(&page_handle, LogicalSize::new(px(400.0), px(1_200.0)))
            .expect("page content");
        let state = app.app_mut().signal(InputState::new(initial));
        let placeholder = app.app_mut().signal(spec.placeholder.clone());
        let spec = Arc::new(spec);
        let node = build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            page,
            spec,
            &theme,
            &state,
            &placeholder,
        );
        node.install(app.app_mut()).expect("install");
        for _ in 0..3 {
            let _ = node.sync(app.app_mut());
            let _ = app.drain().expect("frame");
        }
        (app, node, page_handle)
    }

    fn long_multiline_text() -> String {
        let mut text = String::new();
        for index in 0..24 {
            text.push_str(&format!("第 {index} 行内容\n"));
        }
        text
    }

    fn wheel_down(app: &mut TestApp, point: LogicalPoint) {
        let _ = app.inject(InputEvent::scroll_lines(
            point,
            LogicalPoint::new(px(0.0), px(-1.0)),
            Modifiers::empty(),
        ));
    }

    fn descendant_of(app: &TestApp, node: NodeId, ancestor: NodeId) -> bool {
        let window = app.window_id();
        let tree = app.app().window(window).expect("window").tree();
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            if current == ancestor {
                return true;
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        false
    }

    fn point_center(app: &TestApp, node: NodeId) -> LogicalPoint {
        let bounds = app.probe().bounds(node).expect("bounds");
        LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn scroll_proxy(app: &TestApp, node: NodeId) -> Option<NodeId> {
        let window = app.window_id();
        app.app()
            .window(window)
            .expect("window")
            .tree()
            .scroll_proxy(node)
    }

    #[test]
    fn multiline_input_wheel_over_bar_scrolls_the_input_not_the_page() {
        let text = long_multiline_text();
        let (mut app, node, page) = page_harness(multiline_spec(Some(2)), text.as_str());
        assert!(node.scrollbar.borrow().is_some(), "多行输入必须有滚动条");
        let window = app.window_id();
        let handle = app
            .app()
            .scroll_handle(window, node.viewport)
            .expect("input scroll slot");
        let before = app.app().scroll_state(&handle).expect("state").offset.y;
        let bar = node.scrollbar.borrow().clone().expect("bar");
        let (track, _) = bar.nodes();
        let track = track.expect("track");
        let point = point_center(&app, track);
        assert_eq!(
            app.probe().hit(point).map(|hit| hit.node),
            Some(track),
            "滚轮落点必须命中轨道"
        );
        wheel_down(&mut app, point);
        let after = app.app().scroll_state(&handle).expect("state").offset.y;
        assert!(
            after > before,
            "滚轮在轨道上必须滚输入自身：{before} -> {after}"
        );
        assert_eq!(
            app.app().scroll_state(&page).expect("page state").offset,
            LogicalPoint::ZERO,
            "不得滚外层页面"
        );
        let _ = app.drain();
    }

    #[test]
    fn input_clear_button_wheel_scrolls_the_input_not_the_page() {
        let text = long_multiline_text();
        let (mut app, node, page) = page_harness(multiline_spec(Some(2)), text.as_str());
        assert!(node.set_clearable(app.app_mut(), true), "清空按钮已创建");
        for _ in 0..2 {
            let _ = node.sync(app.app_mut());
            let _ = app.drain().expect("frame");
        }
        let button = node.clear.borrow().clone().expect("clear button");
        let button_node = button.node().expect("clear button node");
        assert_eq!(
            scroll_proxy(&app, button_node),
            Some(node.viewport),
            "清空按钮代理到输入视口"
        );
        let window = app.window_id();
        let handle = app
            .app()
            .scroll_handle(window, node.viewport)
            .expect("input scroll slot");
        let before = app.app().scroll_state(&handle).expect("state").offset.y;
        let point = point_center(&app, button_node);
        let hit = app.probe().hit(point).map(|hit| hit.node);
        assert!(
            hit.is_some_and(|hit| descendant_of(&app, hit, button_node)),
            "滚轮落点必须在清空按钮内：{hit:?}"
        );
        wheel_down(&mut app, point);
        let after = app.app().scroll_state(&handle).expect("state").offset.y;
        assert!(
            after > before,
            "清空按钮上的滚轮必须滚输入自身：{before} -> {after}"
        );
        assert_eq!(
            app.app().scroll_state(&page).expect("page state").offset,
            LogicalPoint::ZERO,
            "不得滚外层页面"
        );
        assert!(node.set_clearable(app.app_mut(), false), "清空按钮已移除");
        let _ = app.drain();
    }

    #[test]
    fn clear_button_proxy_is_cleared_when_the_button_is_removed_or_released() {
        let text = long_multiline_text();
        let (mut app, node, _page) = page_harness(multiline_spec(Some(2)), text.as_str());
        assert!(node.set_clearable(app.app_mut(), true), "清空按钮已创建");
        for _ in 0..2 {
            let _ = node.sync(app.app_mut());
            let _ = app.drain().expect("frame");
        }
        let button_node = node
            .clear
            .borrow()
            .clone()
            .expect("clear button")
            .node()
            .expect("clear button node");
        assert_eq!(
            scroll_proxy(&app, button_node),
            Some(node.viewport),
            "清空按钮代理到输入视口"
        );
        assert!(node.set_clearable(app.app_mut(), false), "清空按钮已移除");
        let _ = app.drain().expect("frame");
        {
            let window = app.window_id();
            let tree = app.app().window(window).expect("window").tree();
            assert!(tree.get(button_node).is_err(), "按钮节点随之卸载");
        }
        assert_eq!(
            scroll_proxy(&app, button_node),
            None,
            "节点卸载后读代理不 panic 且为 None"
        );
        assert!(node.set_clearable(app.app_mut(), true), "清空按钮已重建");
        for _ in 0..2 {
            let _ = node.sync(app.app_mut());
            let _ = app.drain().expect("frame");
        }
        let button_node = node
            .clear
            .borrow()
            .clone()
            .expect("clear button")
            .node()
            .expect("clear button node");
        assert_eq!(scroll_proxy(&app, button_node), Some(node.viewport));
        assert!(node.release(app.app_mut()) > 0, "release 回收组件");
        assert_eq!(
            scroll_proxy(&app, button_node),
            None,
            "release 后按钮代理被清除"
        );
        assert_eq!(node.release(app.app_mut()), 0, "release 幂等");
        let _ = app.drain().expect("frame");
    }

    #[test]
    fn install_reports_missing_install_targets_and_keeps_no_scrollbar() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let state = app.app_mut().signal(InputState::new("abc"));
        let placeholder = app.app_mut().signal(SmolStr::new_static("占位"));
        let spec = Arc::new(multiline_spec(Some(2)));
        let node = build(
            app.app_mut().window_mut(window).expect("window").tree_mut(),
            root,
            spec,
            &theme,
            &state,
            &placeholder,
        );
        let viewport = node.viewport;
        let root_children = |app: &TestApp| {
            app.app()
                .window(window)
                .expect("window")
                .tree()
                .get(root)
                .expect("root")
                .children()
                .len()
        };
        let children_before = root_children(&app);
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style(viewport, Style::default())
            .expect("视口样式");
        let error = node
            .install(app.app_mut())
            .expect_err("非滚动视口必须让 install 报错");
        assert!(
            matches!(error, EngineError::NotScrollable { node } if node == viewport),
            "缺失滚动槽必须报 NotScrollable：{error:?}"
        );
        assert!(
            node.scrollbar.borrow().is_none(),
            "make_scroll 失败不得保存（注册）滚动条"
        );
        assert!(
            node.scroll_handle.get().is_none(),
            "make_scroll 失败后不得保留滚动句柄"
        );
        assert_eq!(
            root_children(&app),
            children_before,
            "build 已建出的滚动条节点必须在 make_scroll 失败时回收"
        );
        {
            let tree = app.app().window(window).expect("window").tree();
            assert!(tree.get(viewport).is_ok(), "视口节点仍在");
        }
        assert!(
            app.app().scroll_handle(window, viewport).is_none(),
            "install 失败后不得残留滚动槽或返回滚动句柄"
        );
        assert!(
            app.app()
                .window(window)
                .expect("window")
                .tree()
                .scroll_proxy(node.root)
                .is_none(),
            "失败不得留下代理"
        );
        app.app_mut()
            .remove_node(window, viewport)
            .expect("remove viewport");
        let children_before_missing_viewport = root_children(&app);
        let error = node
            .install(app.app_mut())
            .expect_err("视口卸载后 install 必须报错");
        assert!(
            matches!(error, EngineError::StaleNode { node } if node == viewport),
            "缺失可滚动视口必须报 StaleNode：{error:?}"
        );
        assert!(
            node.scrollbar.borrow().is_none(),
            "失败不得保存（注册）滚动条"
        );
        assert!(
            node.scroll_handle.get().is_none(),
            "视口卸载后 install 失败不得保留滚动句柄"
        );
        assert_eq!(
            root_children(&app),
            children_before_missing_viewport,
            "build 已建出的滚动条节点必须在 make_scroll 失败时回收"
        );
        assert!(
            app.app()
                .window(window)
                .expect("window")
                .tree()
                .scroll_proxy(node.root)
                .is_none(),
            "失败不得留下代理"
        );
        app.app_mut()
            .remove_node(window, node.root)
            .expect("remove root");
        let expected_root = node.root;
        let error = node
            .install(app.app_mut())
            .expect_err("根节点卸载后必须报错");
        assert!(
            matches!(error, EngineError::StaleNode { node } if node == expected_root),
            "build 失败必须报 StaleNode：{error:?}"
        );
        assert!(
            node.scrollbar.borrow().is_none(),
            "build 失败不得保存滚动条"
        );
    }
}
