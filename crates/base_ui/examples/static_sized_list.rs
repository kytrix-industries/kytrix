mod support;

use base_ui::{
    assets::Assets, button::Button, static_sized_list::StaticSizedList, style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions,
    base::Selectable, div, px, size,
};
use smol_str::format_smolstr;

/// How many rows the list is given.
const ROW_COUNT: usize = 1_000;

const ADJECTIVES: [&str; 8] = [
    "Brisk", "Calm", "Dense", "Eager", "Fluent", "Gentle", "Lucid", "Swift",
];
const NOUNS: [&str; 8] = [
    "Ledger", "Order", "Quote", "Report", "Stream", "Symbol", "Ticket", "Wallet",
];

struct StaticSizedListExample {
    /// The list, which is given every row it draws at once: what it draws is a set whose
    /// length is known before it is drawn.
    list: Entity<StaticSizedList<usize>>,
    /// Index of the chosen row, which the view holds rather than the list: a row is the
    /// caller's element, and what it is drawn as is read from the view when it is drawn.
    chosen: Entity<Option<usize>>,
}

impl StaticSizedListExample {
    fn new(cx: &mut Context<Self>) -> Self {
        let chosen = cx.new(|_| None);
        let list = cx.new({
            let chosen = chosen.clone();
            move |_| {
                let mut list = StaticSizedList::new(
                    "rows".into(),
                    move |index: usize, _: &usize, _window, cx| {
                        let is_chosen = *chosen.read(cx) == Some(index);

                        Button::new(("row", index))
                            .selected(is_chosen)
                            .on_click({
                                let chosen = chosen.clone();
                                move |_, _, cx| {
                                    // Choosing the chosen row again clears the choice.
                                    let next = (*chosen.read(cx) != Some(index)).then_some(index);
                                    chosen.update(cx, |chosen, cx| {
                                        *chosen = next;
                                        cx.notify();
                                    });
                                }
                            })
                            .child(
                                div()
                                    .debug_selector(|| "TEST_STATIC_SIZED_ROW".into())
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .w_full()
                                    .min_w_0()
                                    .child(div().min_w_0().truncate().child(row_label(index)))
                                    .child(div().flex_none().text_sm().child(SharedString::new(
                                        format_smolstr!(
                                            "{}",
                                            if is_chosen { "chosen" } else { "" }
                                        ),
                                    ))),
                            )
                            .into_any_element()
                    },
                );

                // Every row is one size, which is what this list is for: the height of the
                // first place it draws is the height it lays every other place out at.
                list.replace(0..ROW_COUNT);
                list
            }
        });

        Self { list, chosen }
    }
}

impl Render for StaticSizedListExample {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = *self.chosen.read(cx);
        let theme = cx.theme();

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.roles.fg.default.hsla)
                            .child(SharedString::new(format_smolstr!(
                                "{ROW_COUNT} rows · {}",
                                match chosen {
                                    Some(index) => format_smolstr!("chosen: {}", row_label(index)),
                                    None => format_smolstr!("nothing chosen"),
                                }
                            ))),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_normal()
                            .text_xs()
                            .text_color(theme.roles.fg.default.hsla)
                            .child("The list is given the whole set at once and draws what it was given, one height for every row."),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .overflow_hidden()
                    .child(self.list.clone()),
            )
    }
}

/// The label one row carries, which is its number and two words.
fn row_label(index: usize) -> SharedString {
    SharedString::new(format_smolstr!(
        "{:04} {} {}",
        index,
        ADJECTIVES[index % ADJECTIVES.len()],
        NOUNS[(index / ADJECTIVES.len()) % NOUNS.len()]
    ))
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(640.), px(560.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(StaticSizedListExample::new),
        )
        .expect("failed to open window");
    });
}
