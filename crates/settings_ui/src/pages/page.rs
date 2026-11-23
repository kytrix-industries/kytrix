use ui_core::{app::App, error::EngineError, event::Event, node::id::NodeId, window::WindowId};

use crate::search::SearchFilter;

pub(crate) struct PageBuild<'a> {
    pub filter: &'a SearchFilter,
    pub key: &'a str,
    pub page: smol_str::SmolStr,
}

impl PageBuild<'_> {
    pub(crate) fn unique_key(&self, suffix: &str) -> smol_str::SmolStr {
        smol_str::format_smolstr!("{}.{}.{suffix}", self.key, self.page)
    }
}

pub(crate) trait PageView {
    fn page_id(&self) -> &'static str;

    fn build(
        &mut self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
        cx: &PageBuild<'_>,
    ) -> Result<(), EngineError>;

    fn sync(&mut self, app: &mut App) -> Result<(), EngineError>;

    fn dispatch(&mut self, app: &mut App, event: &Event) -> bool;

    fn release(&mut self, app: &mut App);

    fn scrolls(&self) -> bool {
        true
    }
}
