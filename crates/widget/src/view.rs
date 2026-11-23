use app_config::widget::WidgetConfig;
use ui_core::{error::EngineError, event::Event, node::id::NodeId};

use super::{
    category::WidgetCategory,
    config::WidgetConfigChange,
    identity::{WidgetId, WidgetInstanceId},
    instrument_link::InstrumentLink,
    status::{WidgetAction, WidgetStatus},
};
use crate::cx::WidgetCx;

#[derive(Clone)]
pub struct ReloadHandle(std::rc::Rc<dyn Fn(&mut ui_core::app::App)>);

impl ReloadHandle {
    pub fn new(reload: impl Fn(&mut ui_core::app::App) + 'static) -> Self {
        Self(std::rc::Rc::new(reload))
    }

    pub fn reload(&self, app: &mut ui_core::app::App) {
        (self.0)(app);
    }
}

impl std::fmt::Debug for ReloadHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ReloadHandle")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderGroup {
    Left,
    Right,
}

pub trait WidgetView: Sized + 'static {
    type Config: WidgetConfig;

    const ID: WidgetId;
    const DISPLAY_NAME: &'static str;
    const CATEGORY: WidgetCategory;
    const DESCRIPTION: Option<&'static str> = None;
    const ICON: Option<&'static str> = None;
    const SELECTABLE: bool = true;

    #[allow(clippy::too_many_arguments)]
    fn new(
        instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError>;

    fn load(&mut self, cx: &mut WidgetCx<'_>);

    fn has_content(&self) -> bool;

    fn config(&self) -> Option<&Self::Config> {
        None
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError>;

    fn sync(&mut self, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        Ok(())
    }

    fn header(
        &mut self,
        _group: HeaderGroup,
        _parent: NodeId,
        _cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    fn dispatch(&mut self, _event: &Event, _cx: &mut WidgetCx<'_>) -> bool {
        false
    }

    fn release(&mut self, _cx: &mut WidgetCx<'_>) {}

    fn instrument_link(&self) -> Option<InstrumentLink> {
        None
    }
}
