mod support;

use base_ui::{
    assets::Assets, button::Button, deque_unsized_list::DequeUnsizedList,
    interactive::InteractiveVariantExt, separator::Separator, style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Bounds, Context, Entity, IntoElement, ListAlignment,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowBounds,
    WindowOptions, div, px, size,
};
use smol_str::format_smolstr;

/// How many entries every list keeps before the oldest one is dropped.
const CAPACITY: usize = 30;
/// How far above and below a panel the lists measure the entries they cannot see. An entry is
/// a few lines of text, so a panel's worth of them is enough to keep one from appearing a
/// moment after it was scrolled to.
const LIST_OVERDRAW: f32 = 200.0;
/// How many entries every list starts with, so no panel is empty.
const INITIAL_ENTRIES: usize = 3;
/// The tallest body an entry carries, in lines.
const MAX_BODY_LINES: usize = 4;

const HEADLINES: [&str; 6] = [
    "Order filled",
    "Quote updated",
    "Wallet connected",
    "Alert triggered",
    "Position closed",
    "Report published",
];

const SENTENCES: [&str; 6] = [
    "The last price moved inside the band this strategy watches.",
    "Fills are aggregated per instrument before they reach this log.",
    "Nothing else needs a decision while the stream stays connected.",
    "This entry draws more lines than the one above it, and the list measures both.",
    "Every entry is pushed at once into all eight lists, which hold the newest thirty.",
    "Scroll to see whether the list follows, and where a new entry lands.",
];

/// One entry of the log. The same entry is pushed into all eight lists.
#[derive(Clone, Copy)]
struct Entry {
    /// The order this entry was pushed in.
    index: usize,
    headline: &'static str,
    /// How many lines of body this entry draws, which is what makes its height differ from
    /// its neighbours'.
    body_lines: usize,
}

impl Entry {
    fn new(index: usize) -> Self {
        Self {
            index,
            headline: HEADLINES[index % HEADLINES.len()],
            body_lines: index % MAX_BODY_LINES + 1,
        }
    }
}

/// One entry, drawn by the closure the list was given.
///
/// The list measures whatever this returns, so an entry one line tall and an entry four
/// lines tall are both just an entry: no height is passed in, and none is assumed. Nothing
/// here draws the line between two entries either — the list draws that itself.
fn entry_row(_index: usize, entry: &Entry, _window: &mut Window, cx: &mut App) -> AnyElement {
    let theme = cx.theme();
    let content = theme.roles.fg.default.hsla;
    let muted = theme.roles.fg.muted.hsla;

    div()
        .flex()
        .flex_col()
        .gap_2()
        .w_full()
        .min_w_0()
        .px_3()
        .py_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(content)
                        .child(entry.headline),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::new(format_smolstr!(
                            "#{} · {} lines",
                            entry.index + 1,
                            entry.body_lines
                        ))),
                ),
        )
        .children((0..entry.body_lines).map(|line| {
            div()
                .min_w_0()
                .whitespace_normal()
                .text_sm()
                .text_color(content)
                .child(SENTENCES[(entry.index + line) % SENTENCES.len()])
        }))
        .into_any_element()
}

/// The three choices a list makes about how it draws, one panel each.
#[derive(Clone, Copy)]
struct Behaviour {
    align: ListAlignment,
    reverse: bool,
    keep_on_newest: bool,
}

impl Behaviour {
    /// The eight combinations, in the order the example lays them out.
    const ALL: [Behaviour; 8] = [
        Behaviour {
            align: ListAlignment::Top,
            reverse: false,
            keep_on_newest: false,
        },
        Behaviour {
            align: ListAlignment::Top,
            reverse: false,
            keep_on_newest: true,
        },
        Behaviour {
            align: ListAlignment::Top,
            reverse: true,
            keep_on_newest: false,
        },
        Behaviour {
            align: ListAlignment::Top,
            reverse: true,
            keep_on_newest: true,
        },
        Behaviour {
            align: ListAlignment::Bottom,
            reverse: false,
            keep_on_newest: false,
        },
        Behaviour {
            align: ListAlignment::Bottom,
            reverse: false,
            keep_on_newest: true,
        },
        Behaviour {
            align: ListAlignment::Bottom,
            reverse: true,
            keep_on_newest: false,
        },
        Behaviour {
            align: ListAlignment::Bottom,
            reverse: true,
            keep_on_newest: true,
        },
    ];

    /// The choices this panel makes, as its heading.
    fn title(self) -> SharedString {
        SharedString::new(format_smolstr!(
            "{} · {} · {}",
            match self.align {
                ListAlignment::Top => "Top",
                ListAlignment::Bottom => "Bottom",
            },
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
        match (self.align, self.reverse, self.keep_on_newest) {
            (ListAlignment::Top, false, false) => {
                "A new entry lands below the fold, and the view stays where you leave it."
            }
            (ListAlignment::Top, false, true) => {
                "A new entry lands at the end, and the view returns to it."
            }
            (ListAlignment::Top, true, false) => {
                "A new entry lands on top, and what you are reading stays put."
            }
            (ListAlignment::Top, true, true) => {
                "A push keeps a reader who is on the newest entry there, and leaves the others alone."
            }
            (ListAlignment::Bottom, false, false) => {
                "The list hangs on its end, which is where a new entry lands."
            }
            (ListAlignment::Bottom, false, true) => {
                "The list hangs on its end, and returns there when you scroll back."
            }
            (ListAlignment::Bottom, true, false) => {
                "The end holds the oldest entry, so a new entry lands off screen."
            }
            (ListAlignment::Bottom, true, true) => {
                "A push keeps a reader who is on the newest entry there, and leaves the others alone."
            }
        }
    }
}

struct DequeUnsizedListExample {
    /// One list per combination, all drawing the same entries.
    lists: [(Behaviour, Entity<DequeUnsizedList<Entry>>); 8],
    /// How many entries have been pushed, which is what the heading counts.
    pushed: usize,
}

impl DequeUnsizedListExample {
    fn new(cx: &mut Context<Self>) -> Self {
        let lists = std::array::from_fn(|index| {
            let behaviour = Behaviour::ALL[index];
            let list = cx.new(|_| {
                let mut list = DequeUnsizedList::new(
                    ("deque-example", index).into(),
                    behaviour.align,
                    CAPACITY,
                    px(LIST_OVERDRAW),
                    entry_row,
                )
                .separator(|_, _| Separator::horizontal().into_any_element());

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
            lists,
            pushed: INITIAL_ENTRIES,
        }
    }

    /// What the example shows, and the button that pushes into all eight lists at once.
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
                                "Eight ways to draw the same list · {CAPACITY} entries kept · {} pushed",
                                self.pushed
                            ))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Every panel draws the entries it holds from the render closure, one line between two entries coming from the list itself."),
                    ),
            )
            .child(
                Button::new("push-entry")
                    .filled()
                    .child("Push entry")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        // The entry is built once and handed to every list: what it looks
                        // like, and therefore how tall it is, is this view's decision.
                        let entry = Entry::new(this.pushed);
                        this.pushed += 1;
                        for (_, list) in &this.lists {
                            list.update(cx, |list, cx| {
                                list.push(entry);
                                cx.notify();
                            });
                        }
                        cx.notify();
                    })),
            )
    }

    /// Four panels side by side, each an equal share of the width.
    fn panel_row(
        &self,
        panels: &[(Behaviour, Entity<DequeUnsizedList<Entry>>)],
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_row()
            .gap_4()
            .children(
                panels
                    .iter()
                    .map(|(behaviour, list)| self.panel(*behaviour, list, cx)),
            )
    }

    /// One list, named after the way it draws.
    fn panel(
        &self,
        behaviour: Behaviour,
        list: &Entity<DequeUnsizedList<Entry>>,
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

impl Render for DequeUnsizedListExample {
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
            .child(self.panel_row(&self.lists[..4], cx))
            .child(self.panel_row(&self.lists[4..], cx))
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(1360.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(DequeUnsizedListExample::new),
        )
        .expect("failed to open window");
    });
}
