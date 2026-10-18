use gpui_kit::{
    App, AppContext, Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window,
    base::{
        Easing, Transition,
        input::{InputEditorStyle, InputEvent, MaskPattern},
        transition,
    },
    div,
    prelude::FluentBuilder,
};

use crate::{
    animation::ANIMATION_TICK,
    icon::{Icon, IconKind},
    size::{UnifiedSizable, UnifiedSize, UnifiedSizeStyle},
    style::theme::{Theme, ThemeExt},
};

#[derive(Copy, Clone, Eq, PartialEq)]
enum InputMaskType {
    Password,
    Currency { fraction: usize },
}

pub struct InputStateBuilder {
    id: ElementId,
    default_value: Option<SharedString>,
    placeholder: Option<SharedString>,
    input_mask_type: Option<InputMaskType>,
}

impl InputStateBuilder {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            default_value: None,
            placeholder: None,
            input_mask_type: None,
        }
    }

    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    pub fn default_value(mut self, default_value: impl Into<SharedString>) -> Self {
        self.default_value = Some(default_value.into());
        self
    }

    pub fn password(mut self) -> Self {
        self.input_mask_type = Some(InputMaskType::Password);
        self
    }

    pub fn currency(mut self, fraction: usize) -> Self {
        self.input_mask_type = Some(InputMaskType::Currency { fraction });
        self
    }

    pub fn build(self, window: &mut Window, cx: &mut Context<InputState>) -> InputState {
        let InputStateBuilder {
            id,
            default_value,
            placeholder,
            input_mask_type,
        } = self;

        let base = cx.new(|cx| {
            let mut base = gpui_kit::base::input::InputState::new(window, cx);

            if let Some(default_value) = default_value {
                base = base.default_value(default_value);
            }

            if let Some(placeholder) = placeholder {
                base = base.placeholder(placeholder);
            }

            if let Some(input_mask_type) = input_mask_type {
                match input_mask_type {
                    InputMaskType::Password => {
                        base = base.masked(true);
                    }
                    InputMaskType::Currency { fraction } => {
                        base = base.mask_pattern(MaskPattern::Number {
                            separator: Some(','),
                            fraction: Some(fraction),
                        });
                    }
                }
            }

            update_editor_style(&mut base, cx);

            base
        });

        cx.observe_global::<Theme>(|this, cx| {
            this.base.update(cx, |base, cx| {
                update_editor_style(base, cx);
            })
        })
        .detach();

        cx.subscribe(&base, |_this, base, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                cx.emit(InputStateEvent::Change(base.read(cx).unmask_value()));
            }
            InputEvent::PressEnter { .. } => {
                cx.emit(InputStateEvent::PressEnter(base.read(cx).unmask_value()));
            }
            // A field that commits what was typed rather than what was typed last needs
            // the moment the caret leaves it, which is what a blur is: the value travels
            // with the event so a subscriber does not have to read the state back.
            InputEvent::Blur => {
                cx.emit(InputStateEvent::Blur(base.read(cx).unmask_value()));
            }
            InputEvent::Focus => {}
        })
        .detach();

        InputState {
            id,
            // focus_handle: base.focus_handle(cx),
            base,
            input_mask_type,
        }
    }
}

/// What an [`InputState`] reports to whoever is watching the field.
///
/// Every variant carries the value the field held at that moment, so a subscriber reacts
/// to what was committed rather than to what the field says by the time it runs.
pub enum InputStateEvent {
    /// The text changed while it was being typed.
    Change(SharedString),
    /// Enter was pressed.
    PressEnter(SharedString),
    /// The caret left the field.
    Blur(SharedString),
}

pub struct InputState {
    id: ElementId,
    base: Entity<gpui_kit::base::input::InputState>,
    input_mask_type: Option<InputMaskType>,
}

impl InputState {
    /// The focus handle of the engine state behind this field.
    ///
    /// Hand it to [`Window::focus`] to put the caret in the field from outside
    /// the component, which is how a container that owns several fields moves
    /// focus between them.
    pub fn engine_focus_handle(&self, cx: &App) -> FocusHandle {
        self.base.read(cx).focus_handle(cx)
    }

    pub fn value(&self, cx: &App) -> SharedString {
        self.base.read(cx).value()
    }

    pub fn update_value(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.base
            .update(cx, |this, cx| this.set_value(value, window, cx))
    }

    pub fn update_masked(&mut self, masked: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.base
            .update(cx, |this, cx| this.set_masked(masked, window, cx))
    }

    pub fn update_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.base
            .update(cx, |this, cx| this.set_disabled(disabled, cx))
    }

    pub fn update_placeholder(
        &mut self,
        placeholder: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.base
            .update(cx, |this, cx| this.set_placeholder(placeholder, window, cx))
    }
}

impl EventEmitter<InputStateEvent> for InputState {}

impl Render for InputState {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.base.clone()
    }
}

#[derive(IntoElement)]
pub struct Input {
    input_state: Entity<InputState>,
    unified_size: UnifiedSize,
    prefix_icon: Option<IconKind>,
    clearable: bool,
}

impl Input {
    pub fn new(input_state: &Entity<InputState>) -> Self {
        Self {
            input_state: input_state.clone(),
            unified_size: UnifiedSize::Medium,
            prefix_icon: None,
            clearable: false,
        }
    }

    pub fn prefix_icon(mut self, icon: IconKind) -> Self {
        self.prefix_icon = Some(icon);
        self
    }

    pub fn clearable(mut self, clearable: bool) -> Self {
        self.clearable = clearable;
        self
    }
}

impl UnifiedSizable for Input {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = size;
        self
    }
}

impl Focusable for Input {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input_state.read(cx).base.focus_handle(cx)
    }
}

impl RenderOnce for Input {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Input {
            input_state,
            unified_size,
            prefix_icon,
            clearable,
        } = self;

        let current_input_state = input_state.read(cx);
        let id = current_input_state.id.clone();
        let is_password = matches!(
            current_input_state.input_mask_type,
            Some(InputMaskType::Password)
        );
        let base_input_state = current_input_state.base.read(cx);
        let has_value = !base_input_state.unmask_value().is_empty();
        let input_presentation = base_input_state.presentation();
        let masked = input_presentation.is_masked();
        let disabled = input_presentation.is_disabled();
        let focus_handle = base_input_state.focus_handle(cx);

        let theme = cx.theme();

        let background = if disabled {
            theme.components.input.background.disabled
        } else {
            theme.components.input.background.enabled
        };

        let foreground = if disabled {
            theme.components.input.foreground.disabled
        } else {
            theme.components.input.foreground.enabled
        };

        let border = match (disabled, focus_handle.is_focused(window)) {
            (true, _) => theme.components.input.border.disabled,
            (false, true) => theme.components.input.border.focused,
            (false, false) => theme.components.input.border.normal,
        };

        let placeholder = theme.components.input.placeholder;

        let border = transition(
            (id.clone(), "border"),
            border,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        div()
            .id((id.clone(), "input-container"))
            // The box the field is drawn in, which is what a test looks at to see how wide the
            // field it was given is.
            .debug_selector(|| "BASE_UI_INPUT".into())
            .bg(background.hsla)
            .text_color(foreground.hsla)
            .border_1()
            .border_color(border.hsla)
            .rounded_sm()
            .unified_sized(unified_size)
            .flex()
            .items_center()
            .when_some(prefix_icon, |this, prefix_icon| {
                this.child(
                    Icon::new(prefix_icon)
                        .size(unified_size.icon_size())
                        .text_color(placeholder.hsla),
                )
            })
            .child(input_state.clone())
            .when(is_password, |this| {
                this.child(PasswordVisibleToggleButton {
                    id: (id.clone(), "password-toggle").into(),
                    unified_size,
                    masked,
                    input_state: input_state.clone(),
                    disabled,
                })
            })
            .when(has_value && clearable, |this| {
                this.child(ClearButton {
                    id: (id.clone(), "clear").into(),
                    unified_size,
                    input_state: input_state.clone(),
                    disabled,
                })
            })
        // when window close but still has focus on some element, it leaked
        // .on_mouse_down_out(move |_, window, cx| window.blur(cx))
    }
}

#[derive(IntoElement)]
struct ClearButton {
    id: ElementId,
    unified_size: UnifiedSize,
    input_state: Entity<InputState>,
    disabled: bool,
}

impl RenderOnce for ClearButton {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            id,
            unified_size,
            input_state,
            disabled,
        } = self;

        div()
            .id(id)
            .cursor_pointer()
            .child(Icon::new(IconKind::X).size(unified_size.icon_size()))
            .when(!disabled, |this| {
                this.on_click(move |_, window, cx| {
                    input_state.update(cx, |this, cx| {
                        this.base.update(cx, |base, cx| base.clean(window, cx));
                        cx.notify();
                    });
                })
            })
    }
}

#[derive(IntoElement)]
struct PasswordVisibleToggleButton {
    id: ElementId,
    unified_size: UnifiedSize,
    masked: bool,
    input_state: Entity<InputState>,
    disabled: bool,
}

impl RenderOnce for PasswordVisibleToggleButton {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            id,
            unified_size,
            masked,
            input_state,
            disabled,
        } = self;

        div()
            .id(id)
            .cursor_pointer()
            .child(
                Icon::new(if masked {
                    IconKind::Eye
                } else {
                    IconKind::EyeClosed
                })
                .size(unified_size.icon_size()),
            )
            .when(!disabled, |this| {
                this.on_click({
                    move |_, window, cx| {
                        input_state.update(cx, |this, cx| {
                            this.base.update(cx, |base, cx| {
                                base.toggle_masked(window, cx);
                            });
                        });
                    }
                })
            })
    }
}

fn update_editor_style(base: &mut gpui_kit::base::input::InputState, cx: &mut App) {
    let theme = cx.theme();
    base.set_editor_style(InputEditorStyle {
        foreground: theme.components.input.foreground.enabled.hsla,
        muted_foreground: theme.components.input.placeholder.hsla,
        selection: theme.components.input.selection.hsla,
        ..Default::default()
    });
}
