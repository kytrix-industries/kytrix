use std::{cell::RefCell, rc::Rc};

use gpui_kit::{base::*, prelude::FluentBuilder, *};
use smallvec::SmallVec;

use crate::{
    animation::ANIMATION_TICK,
    icon::{Icon, IconKind},
    size::{UnifiedSizable, UnifiedSize},
    style::{color::ResolvedColor, theme::ThemeExt},
};

#[derive(Copy, Clone, Eq, PartialEq)]
enum IndicatorAnimation {
    Fade,
    Slide,
}

#[derive(IntoElement)]
pub struct TabBar {
    id: ElementId,
    unified_size: UnifiedSize,
    indicator_animation: IndicatorAnimation,
    selected_index: usize,
    on_change: Option<Rc<dyn Fn(&usize, &mut Window, &mut App)>>,
    on_close: Option<Rc<dyn Fn(&usize, &mut Window, &mut App)>>,
    children: SmallVec<[AnyElement; 4]>,
}

impl TabBar {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            unified_size: UnifiedSize::Medium,
            indicator_animation: IndicatorAnimation::Fade,
            selected_index: 0,
            on_change: None,
            on_close: None,
            children: SmallVec::new(),
        }
    }

    pub fn indicator_fade(mut self) -> Self {
        self.indicator_animation = IndicatorAnimation::Fade;
        self
    }

    pub fn indicator_slide(mut self) -> Self {
        self.indicator_animation = IndicatorAnimation::Slide;
        self
    }

    pub fn selected_index(mut self, index: usize) -> Self {
        self.selected_index = index;
        self
    }

    pub fn on_change(
        mut self,
        on_change: impl Fn(&usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Rc::new(on_change));
        self
    }

    pub fn on_close(mut self, on_close: impl Fn(&usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(on_close));
        self
    }
}

impl UnifiedSizable for TabBar {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.unified_size = size;
        self
    }
}

impl ParentElement for TabBar {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for TabBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let TabBar {
            id,
            unified_size,
            indicator_animation,
            selected_index,
            on_change,
            on_close,
            children,
        } = self;

        let children_size = children.len();

        let tab_bounds_state = window.use_keyed_state((id.clone(), "tab_bounds"), cx, |_, _| {
            Rc::new(RefCell::new(vec![Bounds::default(); children.len()]))
        });

        tab_bounds_state
            .as_mut(cx)
            .borrow_mut()
            .resize(children.len(), Bounds::default());

        // A tab bar is a recessed track with one raised tab in it: the selected tab is drawn on
        // `surface.overlay` and everything else on the sunken track, which is what makes the
        // selection read as the thing being looked at rather than as a second tint.
        let theme = cx.theme();
        let track = theme.roles.surface.sunken.hsla;
        let track_text = theme.roles.fg.muted.hsla;

        gpui_kit::base::Tabs::new(id.clone())
            .bg(track)
            // `TabsList text-muted-foreground`. Every tab states its own colour, so this is what
            // anything written on the track but not in a tab is written in.
            .text_color(track_text)
            .rounded_sm()
            .w_full()
            .p_1()
            .child(
                div()
                    .relative()
                    .when(
                        matches!(indicator_animation, IndicatorAnimation::Slide)
                            && children_size > 0,
                        |this| {
                            this.child(TabSlideIndicator {
                                id: (id.clone(), "indicator").into(),
                                selected_index,
                                tab_bounds_state: tab_bounds_state.clone(),
                            })
                        },
                    )
                    .child(div().relative().flex().gap_1().children(
                        children.into_iter().enumerate().map({
                            |(index, child)| Tab {
                                id: (id.clone(), index.to_string()).into(),
                                size: unified_size,
                                indicator_animation,
                                index,
                                selected_index,
                                tab_bounds_state: tab_bounds_state.clone(),
                                content: child,
                                on_change: on_change.clone(),
                                on_close: on_close.clone(),
                            }
                        }),
                    )),
            )
    }
}

#[derive(IntoElement)]
struct Tab {
    id: ElementId,
    size: UnifiedSize,
    indicator_animation: IndicatorAnimation,
    index: usize,
    selected_index: usize,
    tab_bounds_state: Entity<Rc<RefCell<Vec<Bounds<Pixels>>>>>,
    content: AnyElement,
    on_change: Option<Rc<dyn Fn(&usize, &mut Window, &mut App)>>,
    on_close: Option<Rc<dyn Fn(&usize, &mut Window, &mut App)>>,
}

impl RenderOnce for Tab {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Tab {
            id,
            size,
            indicator_animation,
            index,
            selected_index,
            tab_bounds_state,
            content,
            on_change,
            on_close,
        } = self;

        let selected = index == selected_index;
        let children_size = tab_bounds_state.read(cx).borrow().len();

        let hover_state = window.use_keyed_state((id.clone(), "hover"), cx, |_, _| false);
        let hover = *hover_state.read(cx);

        let theme = cx.theme();
        let (background, foreground) = match (selected, hover) {
            (false, false) => (ResolvedColor::transparent(), theme.roles.fg.muted),
            (false, true) => (theme.roles.veil.hover, theme.roles.fg.muted),
            (true, _) => (theme.roles.surface.overlay, theme.roles.fg.default),
        };

        let bg = if matches!(indicator_animation, IndicatorAnimation::Slide) {
            // The indicator is drawn where the fill would be, so the tab that slides has no
            // fill of its own: the paint that draws nothing rather than a colour faded to
            // nothing.
            ResolvedColor::transparent()
        } else {
            transition(
                (id.clone(), "bg"),
                background,
                Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
                window,
                cx,
            )
        };

        let fg = transition(
            (id.clone(), "fg"),
            foreground,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let content = div()
            .h(size.h())
            .flex()
            .items_center()
            .child(
                div()
                    .h_full()
                    .text_size(size.text_size())
                    .px(size.px())
                    .max_w_40()
                    .child(content)
                    .content_center()
                    .truncate(),
            )
            .when_some(
                on_close.clone().filter(|_| children_size > 1),
                |this, on_close| {
                    this.child(div().flex_none().pr(size.px()).child(TabCloseButton {
                        id: (id.clone(), "close").into(),
                        size,
                        index,
                        on_close,
                    }))
                },
            );

        gpui_kit::base::Tab::new(id)
            .selected(selected)
            .bg(bg.hsla)
            .text_color(fg.hsla)
            .rounded_sm()
            .on_hover(move |hovered, _, cx| hover_state.write(cx, *hovered))
            .child(content)
            .on_prepaint({
                move |bounds, _, cx| {
                    tab_bounds_state.update(cx, |this, _cx| {
                        this.borrow_mut()[index] = bounds;
                    })
                }
            })
            .when_some(on_change, |this, on_change| {
                this.on_click(move |_, window, cx| on_change(&index, window, cx))
            })
    }
}

#[derive(IntoElement)]
struct TabSlideIndicator {
    id: ElementId,
    selected_index: usize,
    tab_bounds_state: Entity<Rc<RefCell<Vec<Bounds<Pixels>>>>>,
}

impl RenderOnce for TabSlideIndicator {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let TabSlideIndicator {
            id,
            selected_index,
            tab_bounds_state,
        } = self;

        let (current_tab_bounds, first_tab_bounds) = tab_bounds_state.read_with(cx, |this, _| {
            (this.borrow()[selected_index], this.borrow()[0])
        });

        let left = spring(
            (id.clone(), "left"),
            current_tab_bounds.left() - first_tab_bounds.left(),
            Spring::new(ANIMATION_TICK),
            window,
            cx,
        );

        let width = spring(
            (id, "width"),
            current_tab_bounds.size.width,
            Spring::new(ANIMATION_TICK),
            window,
            cx,
        );

        let indicator = cx.theme().roles.surface.overlay.hsla;

        div()
            .absolute()
            .rounded_sm()
            .left(left)
            .w(width)
            .h(current_tab_bounds.size.height)
            .bg(indicator)
    }
}

#[derive(IntoElement)]
struct TabCloseButton {
    id: ElementId,
    size: UnifiedSize,
    index: usize,
    on_close: Rc<dyn Fn(&usize, &mut Window, &mut App)>,
}

impl RenderOnce for TabCloseButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let TabCloseButton {
            id,
            size,
            index,
            on_close,
        } = self;

        let hover_state = window.use_keyed_state((id.clone(), "hover"), cx, |_, _| false);
        let hover = *hover_state.read(cx);

        let mouse_down_state = window.use_keyed_state((id.clone(), "mouse_down"), cx, |_, _| false);
        let mouse_down = *mouse_down_state.read(cx);

        let theme = cx.theme();
        let (background, foreground) = match (mouse_down, hover) {
            (false, false) => (ResolvedColor::transparent(), theme.roles.fg.muted),
            (false, true) => (theme.roles.veil.hover, theme.roles.fg.default),
            (true, _) => (theme.roles.veil.active, theme.roles.fg.default),
        };

        let bg = transition(
            (id.clone(), "bg"),
            background,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        let fg = transition(
            (id.clone(), "fg"),
            foreground,
            Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
            window,
            cx,
        );

        div()
            .id(id)
            .when(matches!(size, UnifiedSize::Small), |this| this.size_3())
            .when(matches!(size, UnifiedSize::Medium), |this| this.size_4())
            .when(matches!(size, UnifiedSize::Large), |this| this.size_5())
            .bg(bg.hsla)
            .rounded_sm()
            .child(Icon::new(IconKind::X).size_full().text_color(fg.hsla))
            .on_hover(move |hovered, _, cx| hover_state.write(cx, *hovered))
            .on_mouse_down(MouseButton::Left, {
                let mouse_down_state = mouse_down_state.clone();
                move |_, _, cx| mouse_down_state.write(cx, true)
            })
            .on_mouse_up(MouseButton::Left, {
                let mouse_down_state = mouse_down_state.clone();
                move |_, _, cx| mouse_down_state.write(cx, false)
            })
            .on_mouse_up_out(MouseButton::Left, {
                let mouse_down_state = mouse_down_state;
                move |_, _, cx| mouse_down_state.write(cx, false)
            })
            .on_click({
                move |_, window, cx| {
                    cx.stop_propagation();
                    on_close(&index, window, cx)
                }
            })
    }
}
