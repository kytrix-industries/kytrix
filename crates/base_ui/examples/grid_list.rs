mod support;

use base_ui::{
    assets::Assets,
    grid_list::GridList,
    icon::{Icon, IconKind},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window, WindowBounds,
    WindowOptions, div, px, size,
};

/// How many columns the example lays its icons out over.
const COLUMNS: u16 = 5;

/// The width the grid is drawn at, unless the window is too narrow for it: the grid is as wide
/// as it is given rather than as wide as its contents, so a column is what is left of that
/// width divided by the column count.
const GRID_WIDTH: f32 = 560.;

/// The height one icon and the name under it is drawn at.
///
/// Every item is the same height, because a row of a grid is as tall as its tallest item.
const ITEM_HEIGHT: f32 = 78.0;

/// Every icon the component can draw, which is what fills the grid.
const ICON_KINDS: [IconKind; 38] = [
    IconKind::X,
    IconKind::XBold,
    IconKind::Circle,
    IconKind::CircleX,
    IconKind::RadioButton,
    IconKind::Square,
    IconKind::SquareBold,
    IconKind::CheckSquare,
    IconKind::Check,
    IconKind::CheckCircle,
    IconKind::CircleNotch,
    IconKind::CaretDown,
    IconKind::CaretLeft,
    IconKind::ArrowUp,
    IconKind::ArrowDown,
    IconKind::Eye,
    IconKind::EyeClosed,
    IconKind::Minus,
    IconKind::MinusBold,
    IconKind::CopySimpleBold,
    IconKind::Plus,
    IconKind::NotePencil,
    IconKind::Trash,
    IconKind::PuzzlePiece,
    IconKind::SquareHalf,
    IconKind::SquaresFour,
    IconKind::Table,
    IconKind::ColumnsPlusLeft,
    IconKind::ColumnsPlusRight,
    IconKind::RowsPlusTop,
    IconKind::RowsPlusBottom,
    IconKind::ArrowsClockwise,
    IconKind::Repeat,
    IconKind::MagnifyingGlass,
    IconKind::At,
    IconKind::Play,
    IconKind::GearSix,
    IconKind::WarningCircle,
];

/// One grid cell: the icon centered, its name under it.
///
/// The cell is filled, because a cell is as wide as its column and an item drawn narrower than
/// one would sit at its start rather than under its own icon.
fn item(
    kind: IconKind,
    name: &'static str,
    content: gpui_kit::Hsla,
    muted: gpui_kit::Hsla,
) -> impl IntoElement {
    div()
        .w_full()
        .h(px(ITEM_HEIGHT))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .overflow_hidden()
        .child(Icon::new(kind).size_8().text_color(content))
        .child(
            div()
                .max_w_full()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(muted)
                .child(name),
        )
}

struct GridListExample;

impl Render for GridListExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = theme.roles.fg.default.hsla;
        let muted = theme.roles.fg.muted.hsla;

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .gap_4()
            .p_6()
            .child(
                div()
                    .text_sm()
                    .text_color(content)
                    .child("GridList · 5 columns · the grid scrolls"),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(GRID_WIDTH))
                    .flex_1()
                    .min_h_0()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .overflow_hidden()
                    .child(
                        GridList::new("grid-list-example", COLUMNS).children(
                            ICON_KINDS
                                .into_iter()
                                .map(|kind| item(kind, icon_name(kind), content, muted)),
                        ),
                    ),
            )
    }
}

/// The name an icon is labelled with, which is the path it is drawn from without its
/// directory and extension.
fn icon_name(kind: IconKind) -> &'static str {
    kind.path()
        .rsplit('/')
        .next()
        .expect("an icon path has a file name")
        .trim_end_matches(".svg")
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(760.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| GridListExample),
        )
        .expect("failed to open window");
    });
}
