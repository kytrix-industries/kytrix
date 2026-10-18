//! Every `Interactive` variant there is: four variants, each of them shown live and disabled.
//!
//! A variant is nothing but a set of paints — `filled` and `tonal` fill their box, `outline` and
//! `ghost` leave it alone and differ only in whether the border survives — so the eight cells
//! below are the whole component. `disabled` is the one state a caller can ask for; `normal`,
//! `hover` and `active` belong to the pointer, which is what the live column is for. `Interactive`
//! registers its own hover and press listeners, so pointing at a live cell and holding the button
//! down on it is the whole demonstration — the example asks for nothing extra.

mod support;

use base_ui::{
    assets::Assets,
    interactive::{Interactive, InteractiveVariantExt},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, Hsla, IntoElement, ParentElement, Render, Styled, Window,
    WindowBounds, WindowOptions, base::Disableable, div, px, size,
};

/// Room for the longest label, so both columns of cells line up under their headings.
const CELL_WIDTH: f32 = 128.;

/// The same room again for the variant names down the left.
const TITLE_WIDTH: f32 = 96.;

/// Between two columns of a row, and therefore also between a heading and its cells.
const COLUMN_GAP: f32 = 12.;

/// What the four rows and the headings come to, which is also how wide the note below is: the
/// note is the only thing here that is not a column, and it reads better level with the grid.
const GRID_WIDTH: f32 = TITLE_WIDTH + 2. * CELL_WIDTH + 2. * COLUMN_GAP;

struct InteractiveExample;

impl Render for InteractiveExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = theme.roles.fg.default.hsla;
        // The cells are drawn with the installed theme whatever the window style is, so the page
        // takes the theme's canvas rather than a colour of its own.
        let canvas = theme.roles.surface.canvas.hsla;

        div()
            .bg(canvas)
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(headings(content))
            .child(variant_row(
                "filled",
                "Filled",
                InteractiveVariantExt::filled,
                content,
            ))
            .child(variant_row(
                "tonal",
                "Tonal",
                InteractiveVariantExt::tonal,
                content,
            ))
            .child(variant_row(
                "outline",
                "Outline",
                InteractiveVariantExt::outline,
                content,
            ))
            .child(variant_row(
                "ghost",
                "Ghost",
                InteractiveVariantExt::ghost,
                content,
            ))
            .child(hint(content))
    }
}

/// One variant in both of the states it can be in without a pointer: live and disabled.
fn variant_row(
    name: &'static str,
    label: &'static str,
    variant: fn(Interactive) -> Interactive,
    color: Hsla,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(COLUMN_GAP))
        .child(caption(TITLE_WIDTH, name, color))
        .child(cell(format!("{name}-enabled"), label, variant, false))
        .child(cell(format!("{name}-disabled"), label, variant, true))
}

/// An `Interactive` with every choice it has written down: a variant, a label, and whether it is
/// disabled.
fn cell(
    id: String,
    label: &'static str,
    variant: fn(Interactive) -> Interactive,
    disabled: bool,
) -> Interactive {
    variant(Interactive::new(id))
        .w(px(CELL_WIDTH))
        .justify_center()
        .child(label)
        .disabled(disabled)
}

fn headings(color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(COLUMN_GAP))
        .child(div().w(px(TITLE_WIDTH)))
        .child(caption(CELL_WIDTH, "Enabled", color))
        .child(caption(CELL_WIDTH, "Disabled", color))
}

fn hint(color: Hsla) -> impl IntoElement {
    div()
        .w(px(GRID_WIDTH))
        .mt_2()
        .text_sm()
        .text_color(color)
        .child(
            "Hover a live cell, then hold the mouse button down on it: hover and active are the \
         two states the pointer owns, and the disabled column answers neither.",
        )
}

fn caption(width: f32, label: &'static str, color: Hsla) -> impl IntoElement {
    div().w(px(width)).text_sm().text_color(color).child(label)
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(560.), px(360.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| InteractiveExample),
        )
        .expect("failed to open window");
    });
}
