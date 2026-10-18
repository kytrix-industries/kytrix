mod support;

use base_ui::{assets::Assets, size::UnifiedSizable, style::theme::ThemeExt, tab_bar::TabBar};
use gpui_kit::{
    App, AppContext, Bounds, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, WindowBounds, WindowOptions, div, px, size,
};

struct TabBarExample {
    tabs: Entity<Vec<SharedString>>,
    selected_tab: Entity<usize>,
    fixed_selected_tab: Entity<usize>,
}

impl TabBarExample {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            tabs: cx.new(|_| {
                vec![
                    "Nice".into(),
                    "GOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOD".into(),
                    "Happy".into(),
                ]
            }),
            selected_tab: cx.new(|_| 0),
            fixed_selected_tab: cx.new(|_| 0),
        }
    }

    fn on_close(&self) -> impl Fn(&usize, &mut Window, &mut App) + 'static {
        let tabs = self.tabs.clone();
        let selected_tab = self.selected_tab.clone();

        move |index, _window, cx| {
            let remaining = tabs.update(cx, |tabs, cx| {
                if *index < tabs.len() {
                    tabs.remove(*index);
                }
                cx.notify();
                tabs.len()
            });

            if *selected_tab.read(cx) >= remaining {
                selected_tab.write(cx, remaining.saturating_sub(1));
            }
        }
    }
}

impl Render for TabBarExample {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.tabs.read(cx).clone();
        let selected_tab = *self.selected_tab.read(cx);
        let fixed_selected_tab = *self.fixed_selected_tab.read(cx);
        let theme = cx.theme();

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_6()
            .child(
                div().w(px(560.)).child(
                    TabBar::new("tab-bar-small")
                        .small()
                        .indicator_fade()
                        .selected_index(selected_tab)
                        .on_change(on_change(&self.selected_tab))
                        .on_close(self.on_close())
                        .children(tabs.clone()),
                ),
            )
            .child(
                div().w(px(560.)).child(
                    TabBar::new("tab-bar-medium")
                        .medium()
                        .indicator_slide()
                        .selected_index(selected_tab)
                        .on_change(on_change(&self.selected_tab))
                        .on_close(self.on_close())
                        .children(tabs),
                ),
            )
            .child(
                div().w(px(560.)).child(
                    TabBar::new("tab-bar-large")
                        .large()
                        .indicator_slide()
                        .selected_index(fixed_selected_tab)
                        .on_change(on_change(&self.fixed_selected_tab))
                        .children(vec!["Tab1", "Tab2", "Tab3", "Tab4"]),
                ),
            )
            .child(
                div()
                    .text_sm()
                    .child(format!("Closable selected index: {selected_tab}")),
            )
            .child(
                div()
                    .text_sm()
                    .child(format!("Fixed selected index: {fixed_selected_tab}")),
            )
    }
}

fn on_change(selected: &Entity<usize>) -> impl Fn(&usize, &mut Window, &mut App) + 'static {
    let selected = selected.clone();

    move |index, _window, cx| selected.write(cx, *index)
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(720.), px(460.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(TabBarExample::new),
        )
        .expect("failed to open window");
    });
}
