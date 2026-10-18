mod support;

use base_ui::{
    assets::Assets,
    icon::IconKind,
    menu::{Menu, MenuItem, MenuItemAction, MenuItemIconAction},
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, WindowBounds, WindowOptions, base::Disableable, div, px, size,
};

struct MenuExample {
    last_action: Entity<Option<SharedString>>,
}

impl MenuExample {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            last_action: cx.new(|_| None),
        }
    }

    fn menu(&self, id: &'static str, size: UnifiedSize) -> Menu {
        Menu::new(id)
            .with_unified_size(size)
            .item(MenuItem::Action(MenuItemAction::new("new", "New File")))
            .item(MenuItem::IconAction(MenuItemIconAction::new(
                "open",
                IconKind::Square,
                "Open File",
            )))
            .item(MenuItem::IconAction(MenuItemIconAction::new(
                "recent",
                IconKind::CaretDown,
                "Open Recent",
            )))
            .item(MenuItem::Separator)
            .item(MenuItem::Action(MenuItemAction::new("save", "Save")))
            .item(MenuItem::Action(
                MenuItemAction::new("export", "Export").disabled(true),
            ))
            .on_item_click({
                let last_action = self.last_action.clone();
                move |_event, item_id, _window, cx| last_action.write(cx, Some(item_id))
            })
    }
}

impl Render for MenuExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let last_action = self.last_action.read(cx).clone();
        let theme = cx.theme();

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_4()
                    .child(
                        div()
                            .w(px(240.))
                            .h(px(210.))
                            .child(self.menu("menu-small", UnifiedSize::Small)),
                    )
                    .child(
                        div()
                            .w(px(240.))
                            .h(px(230.))
                            .child(self.menu("menu-medium", UnifiedSize::Medium)),
                    )
                    .child(
                        div()
                            .w(px(240.))
                            .h(px(260.))
                            .child(self.menu("menu-large", UnifiedSize::Large)),
                    ),
            )
            .child(div().text_sm().child(match &last_action {
                Some(last_action) => format!("Last clicked: {last_action}"),
                None => "Last clicked: none".to_string(),
            }))
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(880.), px(460.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(MenuExample::new),
        )
        .expect("failed to open window");
    });
}
