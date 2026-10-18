mod support;

use base_ui::{
    assets::Assets, button::Button, deque_sized_list::DequeSizedList,
    interactive::InteractiveVariantExt, style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowBounds,
    WindowOptions, div, px, size,
};
use smol_str::format_smolstr;

/// How many entries a list keeps before the oldest one is dropped.
const CAPACITY: usize = 30;
/// How many entries every list starts with, so no panel is empty.
const INITIAL_ENTRIES: usize = 3;
/// The height every row is drawn at, which is what the list lays every place out at: a row of a
/// different height would be drawn out of step with the ones around it.
const ROW_HEIGHT: f32 = 44.0;

const ADJECTIVES: [&str; 8] = [
    "Brisk", "Calm", "Dense", "Eager", "Fluent", "Gentle", "Lucid", "Swift",
];
const NOUNS: [&str; 8] = [
    "Ledger", "Order", "Quote", "Report", "Stream", "Symbol", "Ticket", "Wallet",
];
const HEADLINES: [&str; 6] = [
    "Order filled",
    "Quote updated",
    "Wallet connected",
    "Alert triggered",
    "Position closed",
    "Report published",
];

/// One entry of a panel that is pushed into. The same entry is pushed into all four of them.
#[derive(Clone, Copy)]
struct Entry {
    /// The order this entry was pushed in.
    index: usize,
}

impl Entry {
    fn new(index: usize) -> Self {
        Self { index }
    }
}

/// The label one row carries, which is its number and two words.
fn label(index: usize) -> SharedString {
    SharedString::new(format_smolstr!(
        "{:04} {} {}",
        index,
        ADJECTIVES[index % ADJECTIVES.len()],
        NOUNS[(index / ADJECTIVES.len()) % NOUNS.len()]
    ))
}

/// What every row of a list is drawn as.
///
/// One size for every row, because that is what the list is: the height of the first place it
/// draws is the height it lays every other place out at.
fn row_frame(
    selector: &'static str,
    index: usize,
    label: SharedString,
    content: Hsla,
    muted: Hsla,
) -> impl IntoElement {
    div()
        .debug_selector(move || String::from(selector))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .w_full()
        .min_w_0()
        .h(px(ROW_HEIGHT))
        .px_3()
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(content)
                .child(label),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(SharedString::new(format_smolstr!(
                    "{}",
                    HEADLINES[index % HEADLINES.len()]
                ))),
        )
}

/// One pushed entry, drawn by the closure its list was given.
fn entry_row(_index: usize, entry: &Entry, _window: &mut Window, cx: &mut App) -> AnyElement {
    let content = cx.theme().roles.fg.default.hsla;
    let muted = cx.theme().roles.fg.muted.hsla;

    row_frame(
        "TEST_DEQUE_SIZED_ENTRY",
        entry.index,
        label(entry.index),
        content,
        muted,
    )
    .into_any_element()
}

/// The choices a panel makes about how its list draws.
#[derive(Clone, Copy)]
struct Behaviour {
    reverse: bool,
    keep_on_newest: bool,
}

impl Behaviour {
    /// The four combinations the example lays out.
    const ALL: [Behaviour; 4] = [
        Behaviour {
            reverse: false,
            keep_on_newest: false,
        },
        Behaviour {
            reverse: false,
            keep_on_newest: true,
        },
        Behaviour {
            reverse: true,
            keep_on_newest: false,
        },
        Behaviour {
            reverse: true,
            keep_on_newest: true,
        },
    ];

    /// The choices this panel makes, as its heading.
    fn title(self) -> SharedString {
        SharedString::new(format_smolstr!(
            "{} · {}",
            if self.reverse {
                "newest first"
            } else {
                "newest last"
            },
            if self.keep_on_newest {
                "keep on newest"
            } else {
                "free scroll"
            }
        ))
    }

    /// Where a pushed entry lands, and what the list does with the view while it does.
    fn hint(self) -> &'static str {
        match (self.reverse, self.keep_on_newest) {
            (false, false) => {
                "A new entry lands below the fold, and the view stays where you leave it."
            }
            (false, true) => "A new entry lands at the end, and a reader at the end follows it.",
            (true, false) => "A new entry lands on top, and the view is left where you put it.",
            (true, true) => {
                "A new entry lands on top, which is where a reader who is on the newest entry is kept."
            }
        }
    }
}

struct DequeSizedListExample {
    /// One list per combination, all drawing the same entries.
    panels: [(Behaviour, Entity<DequeSizedList<Entry>>); 4],
    /// How many entries have been pushed, which is what the heading counts.
    pushed: usize,
}

impl DequeSizedListExample {
    fn new(cx: &mut Context<Self>) -> Self {
        let panels = std::array::from_fn(|index| {
            let behaviour = Behaviour::ALL[index];
            let list = cx.new(|_| {
                let mut list =
                    DequeSizedList::new(("deque-sized-example", index).into(), CAPACITY, entry_row);

                if behaviour.reverse {
                    list = list.render_reverse();
                }
                if behaviour.keep_on_newest {
                    list = list.keep_on_newest();
                }

                for entry in 0..INITIAL_ENTRIES {
                    list.push(Entry::new(entry));
                }

                list
            });

            (behaviour, list)
        });

        Self {
            panels,
            pushed: INITIAL_ENTRIES,
        }
    }

    /// What the example shows, and the controls that push into every list at once.
    fn heading(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = theme.roles.fg.default.hsla;
        let muted = theme.roles.fg.muted.hsla;

        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .w_full()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(content)
                            .child(SharedString::new(format_smolstr!(
                                "Four ways to draw the same feed · {CAPACITY} entries kept · {} pushed",
                                self.pushed
                            ))),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_normal()
                            .text_xs()
                            .text_color(muted)
                            .child("Every panel is pushed into at once, and each keeps the newest entries: one height for every row, which is what the list measures its first place for."),
                    ),
            )
            .child(
                Button::new("push-entry")
                    .filled()
                    .child("Push entry")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        // The entry is built once and handed to every list: what it looks like,
                        // and therefore how tall it is, is this view's decision.
                        let entry = Entry::new(this.pushed);
                        this.pushed += 1;
                        for (_, list) in &this.panels {
                            list.update(cx, |list, cx| {
                                list.push(entry);
                                cx.notify();
                            });
                        }
                        cx.notify();
                    })),
            )
    }

    /// One list, named after the way it draws.
    fn panel(
        &self,
        behaviour: Behaviour,
        list: &Entity<DequeSizedList<Entry>>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let content = theme.roles.fg.default.hsla;
        let muted = theme.roles.fg.muted.hsla;

        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(div().text_sm().text_color(content).child(behaviour.title()))
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_normal()
                            .text_xs()
                            .text_color(muted)
                            .child(behaviour.hint()),
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
                    .child(list.clone()),
            )
    }
}

impl Render for DequeSizedListExample {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .child(self.heading(cx))
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .children(
                        self.panels
                            .iter()
                            .map(|(behaviour, list)| self.panel(*behaviour, list, cx)),
                    ),
            )
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(DequeSizedListExample::new),
        )
        .expect("failed to open window");
    });
}
