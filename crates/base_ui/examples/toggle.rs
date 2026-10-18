//! The whole of `Toggle`: two variants, three sizes, and the two states a caller can write down.
//!
//! A toggle asks for four orthogonal things — a variant, a size, whether it is checked, and
//! whether it is disabled — so the grid below is the whole component: one row per variant and
//! size, one column per combination of checked and enabled, twenty-four cells in all and no two
//! of them the same. A checkbox and a radio differ only in the two glyph pairs they swap, and a
//! size reaches the glyph, the label and the row height together, which is why the same four
//! states are worth showing three times.
//!
//! Every cell is given the same number of builder calls: a checked box is `.checked(true)` and an
//! unchecked one is `.checked(false)`, not an omission, and a disabled row differs from a live
//! one by one call. The live cells carry `on_change` and answer a click; the disabled ones answer
//! nothing, which is a built-in property of `Toggle` rather than something this example arranges.

mod support;

use std::collections::HashMap;

use base_ui::{
    assets::Assets,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
    toggle::Toggle,
};
use gpui_kit::{
    AppContext, Bounds, Context, ElementId, Entity, IntoElement, ParentElement, Render, Styled,
    Window, WindowBounds, WindowOptions, base::Disableable, div, px, size,
};

/// What the widest column has to hold: `disabled · checked` split over two lines at this size.
const COLUMN_WIDTH: f32 = 96.;

/// Room for the longest row caption, `Large · checkbox`, so that the cells line up in a column.
const CAPTION_WIDTH: f32 = 140.;

/// The two axes that are not states, in the order they are shown.
const ROWS: [Row; 6] = [
    Row {
        variant: Variant::Checkbox,
        size: Size::Small,
    },
    Row {
        variant: Variant::Radio,
        size: Size::Small,
    },
    Row {
        variant: Variant::Checkbox,
        size: Size::Medium,
    },
    Row {
        variant: Variant::Radio,
        size: Size::Medium,
    },
    Row {
        variant: Variant::Checkbox,
        size: Size::Large,
    },
    Row {
        variant: Variant::Radio,
        size: Size::Large,
    },
];

/// The two axes that are states, in the order they are shown.
const COLUMNS: [Column; 4] = [
    Column {
        checked: true,
        disabled: false,
    },
    Column {
        checked: false,
        disabled: false,
    },
    Column {
        checked: true,
        disabled: true,
    },
    Column {
        checked: false,
        disabled: true,
    },
];

/// Which glyph pair a toggle swaps, and the only thing that tells one variant from the other.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Variant {
    Checkbox,
    Radio,
}

impl Variant {
    /// A variant is the id prefix of a row as well as its glyphs, because it and the size are
    /// both axes of the grid.
    fn name(self) -> &'static str {
        match self {
            Variant::Checkbox => "checkbox",
            Variant::Radio => "radio",
        }
    }

    fn toggle(self, id: impl Into<ElementId>) -> Toggle {
        match self {
            Variant::Checkbox => Toggle::new(id).checkbox(),
            Variant::Radio => Toggle::new(id).radio(),
        }
    }
}

/// The three sizes, every one of them shown against both variants.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Size {
    Small,
    Medium,
    Large,
}

impl Size {
    fn unified(self) -> UnifiedSize {
        match self {
            Size::Small => UnifiedSize::Small,
            Size::Medium => UnifiedSize::Medium,
            Size::Large => UnifiedSize::Large,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Size::Small => "Small",
            Size::Medium => "Medium",
            Size::Large => "Large",
        }
    }
}

/// One cell of the grid: whether it is checked, and whether it can be changed at all.
#[derive(Copy, Clone)]
struct Column {
    checked: bool,
    disabled: bool,
}

impl Column {
    /// The heading, on two lines: a column is 96 pixels wide and the names are long.
    fn title(self) -> &'static str {
        match (self.disabled, self.checked) {
            (false, true) => "enabled\nchecked",
            (false, false) => "enabled\nunchecked",
            (true, true) => "disabled\nchecked",
            (true, false) => "disabled\nunchecked",
        }
    }
}

/// One row of the grid: a variant at one of the three sizes.
#[derive(Copy, Clone)]
struct Row {
    variant: Variant,
    size: Size,
}

impl Row {
    /// A caption that names the row and only the row: what a cell is belongs to the cell.
    fn caption(self) -> String {
        format!("{} · {}", self.size.title(), self.variant.name())
    }

    /// The id of one cell, and the key its state is stored under.
    ///
    /// It names all four axes because each cell is its own element: a row of four cells is four
    /// toggles, and nothing about them is shared but the row they sit in.
    fn id(self, column: Column) -> &'static str {
        match (self.variant, self.size, column.checked, column.disabled) {
            (Variant::Checkbox, Size::Small, true, false) => "small-checkbox-enabled-checked",
            (Variant::Checkbox, Size::Small, false, false) => "small-checkbox-enabled-unchecked",
            (Variant::Checkbox, Size::Small, true, true) => "small-checkbox-disabled-checked",
            (Variant::Checkbox, Size::Small, false, true) => "small-checkbox-disabled-unchecked",
            (Variant::Checkbox, Size::Medium, true, false) => "medium-checkbox-enabled-checked",
            (Variant::Checkbox, Size::Medium, false, false) => "medium-checkbox-enabled-unchecked",
            (Variant::Checkbox, Size::Medium, true, true) => "medium-checkbox-disabled-checked",
            (Variant::Checkbox, Size::Medium, false, true) => "medium-checkbox-disabled-unchecked",
            (Variant::Checkbox, Size::Large, true, false) => "large-checkbox-enabled-checked",
            (Variant::Checkbox, Size::Large, false, false) => "large-checkbox-enabled-unchecked",
            (Variant::Checkbox, Size::Large, true, true) => "large-checkbox-disabled-checked",
            (Variant::Checkbox, Size::Large, false, true) => "large-checkbox-disabled-unchecked",
            (Variant::Radio, Size::Small, true, false) => "small-radio-enabled-checked",
            (Variant::Radio, Size::Small, false, false) => "small-radio-enabled-unchecked",
            (Variant::Radio, Size::Small, true, true) => "small-radio-disabled-checked",
            (Variant::Radio, Size::Small, false, true) => "small-radio-disabled-unchecked",
            (Variant::Radio, Size::Medium, true, false) => "medium-radio-enabled-checked",
            (Variant::Radio, Size::Medium, false, false) => "medium-radio-enabled-unchecked",
            (Variant::Radio, Size::Medium, true, true) => "medium-radio-disabled-checked",
            (Variant::Radio, Size::Medium, false, true) => "medium-radio-disabled-unchecked",
            (Variant::Radio, Size::Large, true, false) => "large-radio-enabled-checked",
            (Variant::Radio, Size::Large, false, false) => "large-radio-enabled-unchecked",
            (Variant::Radio, Size::Large, true, true) => "large-radio-disabled-checked",
            (Variant::Radio, Size::Large, false, true) => "large-radio-disabled-unchecked",
        }
    }
}

struct ToggleExample {
    /// What each live cell is showing now. The grid's initial picture is written in once, at
    /// construction, so that a later frame only has to read what a click left behind.
    checked: Entity<HashMap<&'static str, bool>>,
}

impl ToggleExample {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            checked: cx.new(|_| {
                ROWS.iter()
                    .flat_map(|row| COLUMNS.map(|column| (row.id(column), column.checked)))
                    .collect()
            }),
        }
    }

    /// One toggle with every one of its four choices written down.
    fn toggle(&self, row: Row, column: Column, checked: bool) -> Toggle {
        let toggle = row
            .variant
            .toggle(row.id(column))
            .checked(checked)
            .with_unified_size(row.size.unified());

        if column.disabled {
            return toggle.disabled(true);
        }

        let checked = self.checked.clone();
        toggle.on_change(move |is_checked, _window, cx| {
            checked.update(cx, |checked, cx| {
                checked.insert(row.id(column), is_checked);
                cx.notify();
            });
        })
    }

    fn grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let content = cx.theme().roles.fg.default.hsla;
        let checked = self.checked.read(cx).clone();

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap_2()
                    .child(div().w(px(CAPTION_WIDTH)))
                    .children(COLUMNS.map(|column| {
                        div()
                            .w(px(COLUMN_WIDTH))
                            .line_height(px(15.))
                            .text_xs()
                            .text_color(content)
                            .child(column.title())
                    })),
            )
            .children(ROWS.map(|row| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(CAPTION_WIDTH))
                            .text_sm()
                            .text_color(content)
                            .child(row.caption()),
                    )
                    .children(COLUMNS.map(|column| {
                        div()
                            .w(px(COLUMN_WIDTH))
                            .flex()
                            .items_center()
                            .child(self.toggle(row, column, checked[row.id(column)]))
                    }))
            }))
    }
}

impl Render for ToggleExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().roles.surface.canvas.hsla;
        let content = cx.theme().roles.fg.default.hsla;

        div()
            .relative()
            .size_full()
            .bg(background)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(self.grid(cx))
            .child(
                div()
                    .text_sm()
                    .text_color(content)
                    .child("The first two columns are live: click a glyph and watch it change."),
            )
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(780.), px(400.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(ToggleExample::new),
        )
        .expect("failed to open window");
    });
}
