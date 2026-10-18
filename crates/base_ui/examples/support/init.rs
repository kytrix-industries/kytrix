use gpui_kit::App;

pub fn init(cx: &mut App) {
    gpui_kit::base::init(cx);
    base_ui::style::theme::install(cx);
}
