use gpui_kit::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div,
};
use market_participants_ui::menu::MarketParticipants;

use crate::windows::root::components::workspace::{WorkspaceSwitcher, Workspaces};

pub struct Market {
    market_participants: Entity<MarketParticipants>,
    workspaces: Entity<Workspaces>,
}

impl Market {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Market {
            market_participants: cx.new(MarketParticipants::new),
            workspaces: cx.new(Workspaces::load),
        }
    }

    pub fn toolbar(&self) -> impl IntoElement {
        div()
            .flex()
            .gap_1()
            .child(self.market_participants.clone())
            .child(WorkspaceSwitcher::new(self.workspaces.clone()))
    }
}

impl Render for Market {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.workspaces.clone()
    }
}
