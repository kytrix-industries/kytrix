mod support;

use base_ui::{
    assets::Assets,
    style::theme::ThemeExt,
    table::{Table, TableDataSet},
};
use gpui_kit::{
    AppContext, Bounds, Context, IntoElement, Length, ParentElement, Render, SharedString, Styled,
    Window, WindowBounds, WindowOptions, base::Align, div, px, relative, size,
};

struct DemoDataSet;

impl TableDataSet<5> for DemoDataSet {
    fn rows_count(&self) -> usize {
        100
    }

    fn headers(&self, _cx: &gpui_kit::App) -> [SharedString; 5] {
        [
            "Column1".into(),
            "Column2".into(),
            "Column3".into(),
            "Column4".into(),
            "Column5".into(),
        ]
    }

    fn column_aligns(&self) -> [Align; 5] {
        [
            Align::Start,
            Align::Center,
            Align::Center,
            Align::Center,
            Align::End,
        ]
    }

    fn column_widths(&self) -> [Length; 5] {
        [
            relative(0.2).into(),
            relative(0.1).into(),
            relative(0.1).into(),
            relative(0.3).into(),
            relative(0.3).into(),
        ]
    }

    fn cell(&self, row_index: usize, column_index: usize, _cx: &gpui_kit::App) -> impl IntoElement {
        SharedString::new(format!("Cell {row_index}-{column_index}"))
    }
}

struct TableExample;

impl Render for TableExample {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dataset = window.use_keyed_state("table-dataset", cx, |_, _| DemoDataSet);
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
                    .h(px(280.))
                    .child(Table::new("table", &dataset)),
            )
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(960.), px(680.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| TableExample),
        )
        .expect("failed to open window");
    });
}
