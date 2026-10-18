mod support;

use base_ui::{
    assets::Assets,
    icon::IconKind,
    menu::{MenuItem, MenuItemAction, MenuItemIconAction},
    select::Select,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
    tooltip::TooltipExt,
};
use gpui_kit::{
    App, AppContext, Bounds, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, WindowBounds, WindowOptions, base::Disableable, div, px, size,
};

/// The entries every select in this example is offered.
const OPTIONS: [(&str, &str); 4] = [
    ("option-a", "Option A"),
    ("option-b", "Option B"),
    ("option-c", "Option C"),
    ("option-d", "Option D"),
];

struct SelectExample {
    selected: Entity<Option<SharedString>>,
}

impl SelectExample {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            selected: cx.new(|_| None),
        }
    }

    /// Records the entry that was selected, which is the id a select hands back.
    fn record(&self) -> impl Fn(&Option<SharedString>, &mut Window, &mut App) + 'static {
        let selected = self.selected.clone();

        move |selection, _window, cx| selected.write(cx, selection.clone())
    }

    /// A select over [`OPTIONS`], holding `selected` the way a select is held: the *id* of an
    /// entry, which the select looks up in its own entries to draw on its trigger.
    fn select(
        &self,
        id: &'static str,
        size: UnifiedSize,
        selected: Option<SharedString>,
    ) -> Select {
        OPTIONS
            .iter()
            .fold(
                Select::new(id)
                    .with_unified_size(size)
                    .selected_item(selected),
                |select, (item_id, label)| {
                    select.menu_item(MenuItem::Action(MenuItemAction::new(*item_id, *label)))
                },
            )
            .on_change(self.record())
            .with_tooltip(format!("Select ({size:?})"))
    }

    /// One select per size, over the same entries.
    fn size_row(&self, selected: Option<SharedString>) -> impl IntoElement {
        sized_row(
            [
                ("select-small", UnifiedSize::Small),
                ("select-medium", UnifiedSize::Medium),
                ("select-large", UnifiedSize::Large),
            ]
            .into_iter()
            .map(move |(id, size)| self.select(id, size, selected.clone())),
        )
    }

    /// The two ways a select can have nothing to choose from: offered no entries at all, and
    /// offered entries while being unable to open them.
    fn state_row(&self, selected: Option<SharedString>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .w(px(200.))
                    .child(Select::new("select-empty").selected_item(selected.clone())),
            )
            .child(
                div().w(px(200.)).child(
                    self.select("select-disabled", UnifiedSize::Medium, selected)
                        .disabled(true),
                ),
            )
    }

    /// A select over entries that carry an icon: its trigger draws the icon of the selected
    /// entry beside its label.
    fn icon_row(&self, selected: Option<SharedString>) -> impl IntoElement {
        div().w(px(200.)).child(
            [
                (IconKind::Square, "Square", "square"),
                (IconKind::Circle, "Circle", "circle"),
                (IconKind::Eye, "Eye", "eye"),
            ]
            .into_iter()
            .fold(
                Select::new("select-icon").selected_item(selected),
                |select, (icon_kind, label, id)| {
                    select.menu_item(MenuItem::IconAction(MenuItemIconAction::new(
                        id, icon_kind, label,
                    )))
                },
            )
            .on_change(self.record())
            .with_tooltip("Select with icons"),
        )
    }

    /// A select hands back the id of the entry that was clicked, so this is what it has to say:
    /// the label the entry under that id was drawn with.
    fn label_of(id: &SharedString) -> &'static str {
        OPTIONS
            .iter()
            .chain([
                &("square", "Square"),
                &("circle", "Circle"),
                &("eye", "Eye"),
            ])
            .find_map(|(item_id, label)| item_id.eq(&id.as_str()).then_some(*label))
            .unwrap_or("(an unknown entry)")
    }
}

impl Render for SelectExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected.read(cx).clone();
        let theme = cx.theme();

        let readout = match &selected {
            Some(selected) => format!("Selected: {} ({selected})", Self::label_of(selected)),
            None => "Selected: none".to_string(),
        };

        div()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_6()
            .child(self.size_row(selected.clone()))
            .child(self.icon_row(selected.clone()))
            .child(self.state_row(selected))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.roles.fg.default.hsla)
                    .child(readout),
            )
    }
}

/// Lays selects out in a row, each in a box of its own: a select fills what it is drawn in, so
/// this is also what gives every one of them the same width.
fn sized_row(selects: impl IntoIterator<Item = Select>) -> impl IntoElement {
    div().flex().flex_row().items_center().gap_4().children(
        selects
            .into_iter()
            .map(|select| div().w(px(200.)).child(select)),
    )
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(760.), px(360.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(SelectExample::new),
        )
        .expect("failed to open window");
    });
}
