use gpui_kit::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div,
};
use market_participants_ui::menu::MarketParticipants;

use crate::windows::root::components::workspace::Workspaces;

/// The market page: the market participants control over the active workspace's stage.
///
/// The workspaces are the window's, not the page's: the sidebar lists them and this page
/// shows the stage of whichever one is active, so one entity is what both of them read.
pub struct Market {
    market_participants: Entity<MarketParticipants>,
    workspaces: Entity<Workspaces>,
}

impl Market {
    pub(crate) fn new(cx: &mut Context<Self>, workspaces: Entity<Workspaces>) -> Self {
        Market {
            market_participants: cx.new(MarketParticipants::new),
            workspaces,
        }
    }
}

impl Render for Market {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_1()
            // The toolbar is as tall as the control in it: it is the page's first line, and
            // the stage below takes everything else.
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.market_participants.clone()),
            )
            .child(div().flex_1().min_h_0().child(self.workspaces.clone()))
    }
}
