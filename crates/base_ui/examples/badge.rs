//! The whole of `Badge`: three shapes, and the three sizes each of them comes in.
//!
//! A badge asks for two things — how the color is worn (tonal, solid or outlined) and how big it
//! is — and they do not interact: a size reaches the text, the padding and the height together,
//! and a shape picks which role the badge is painted in. The grid below is every combination of
//! the two, so the entire component is nine badges, no two alike.
//!
//! Every cell spells out both choices, even where one of them is the default a badge already
//! has: the point of the grid is that a reader can see which call produced which cell. The color
//! itself is not on the grid: a badge with no status color is neutral, and a status badge is the
//! same three shapes with one of the four status roles.

mod support;

use base_ui::{assets::Assets, badge::Badge, size::UnifiedSizable, style::theme::ThemeExt};
use gpui_kit::{
    AppContext, Bounds, Context, Hsla, IntoElement, ParentElement, Render, Styled, Window,
    WindowBounds, WindowOptions, div, px, size,
};

/// Wide enough for the longest badge, `Outline` at the large size, with room either side of it.
const CELL_WIDTH: f32 = 160.;

/// Room for the size names down the left, so the cells line up under their headings.
const CAPTION_WIDTH: f32 = 72.;

/// The three shapes, in the order they are shown, each applied to a badge.
const SHAPES: [(fn(Badge) -> Badge, &str); 3] = [
    (Badge::tonal, "Tonal"),
    (Badge::solid, "Solid"),
    (Badge::outline, "Outline"),
];

/// The three sizes, each of them shown against every shape.
const SIZES: [(fn(Badge) -> Badge, &str); 3] = [
    (Badge::small, "Small"),
    (Badge::medium, "Medium"),
    (Badge::large, "Large"),
];

struct BadgeExample;

impl Render for BadgeExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let roles = &cx.theme().roles;

        div()
            .relative()
            .size_full()
            .bg(roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(headings(roles.fg.default.hsla))
            .children(SIZES.map(|(size, title)| row(size, title, roles.fg.default.hsla)))
    }
}

/// One row per size, with a badge for every shape in it.
fn row(size: fn(Badge) -> Badge, title: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(CAPTION_WIDTH))
                .text_sm()
                .text_color(color)
                .child(title),
        )
        .children(SHAPES.map(|(shape, title)| {
            div()
                .w(px(CELL_WIDTH))
                .flex()
                .justify_center()
                .child(shape(size(Badge::new())).child(title))
        }))
}

/// The shapes named once, above the cells they head.
fn headings(color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(div().w(px(CAPTION_WIDTH)))
        .children(SHAPES.map(|(_, title)| {
            div()
                .w(px(CELL_WIDTH))
                .text_sm()
                .text_color(color)
                .child(title)
        }))
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(640.), px(320.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| BadgeExample),
        )
        .expect("failed to open window");
    });
}
