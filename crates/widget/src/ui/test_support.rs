//! Shared setup for the widget framework's tests.

use std::{cell::Cell, rc::Rc};

use gpui_kit::{App, AppContext, Context, Entity, IntoElement, Render, Window};
use i18n::{language::codes::LanguageCode, translator::types::I18n};

use crate::{
    any::has_content_of,
    category::WidgetCategory,
    config::WidgetConfigChange,
    identity::{WidgetId, WidgetInstanceId},
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    surface::WidgetSurface,
    view::WidgetView,
};

/// The messages the framework itself resolves while building a surface, and the copy its
/// tests report as a widget's own.
///
/// The `test.*` keys are a fixture rather than messages of the product. `test.notice` is
/// markdown that names a control the user has to reach for, its icon and its label inside
/// one `<kbd>`, so a test can tell the component's drawing from a plain run of words — and
/// the two languages spell it differently, because a notice is resolved on the frame that
/// draws it rather than where it was reported. `sample.failure` is a widget's own typed
/// failure, and it is resolved the same way.
///
/// `common.market_not_loaded` is the product's own sentence, together with the three keys
/// it is written in terms of — a venue's name through `venue.binance` and `venue.okx`, and
/// the control it points at through `window.market_participants.title`. They are here because
/// they are what the framework supplies when it resolves a notice that names markets.
pub(crate) const MESSAGES_EN: &str = r#"
common:
  connecting: Connecting
  loading: Loading
  retry: Retry
  market_not_loaded: "<p align=\"center\">This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the <kbd>![](file://icons/phosphoricons/graph.svg) %{control}</kbd> section in the top left corner.\nThe market participants this widget supports are:\n%{markets}</p>"
sample:
  failure: route unavailable
test:
  notice: 'Load a market from the <kbd>![](file://icons/phosphoricons/graph.svg) Markets</kbd> button'
  waiting: Waiting for orders
venue:
  binance: Binance
  okx: OKX
window:
  market_participants:
    title: Markets
"#;

pub(crate) const MESSAGES_ZH: &str = r#"
common:
  connecting: 正在连接
  loading: 正在加载
  retry: 重试
  market_not_loaded: "<p align=\"center\">当前组件需要先加载受支持的市场参与者元数据才可以使用。\n请先从左上角的 <kbd>![](file://icons/phosphoricons/graph.svg) %{control}</kbd> 中加载至少一个受支持的市场参与者元数据。\n本组件支持的市场参与者有：\n%{markets}</p>"
sample:
  failure: 路线不可用
test:
  notice: 请先从左上角的 <kbd>![](file://icons/phosphoricons/graph.svg) 市场</kbd> 中加载市场
  waiting: 正在等待订单
venue:
  binance: 币安
  okx: 欧易
window:
  market_participants:
    title: 市场
"#;

/// Installs a minimal English and Chinese translator into `cx`, together with the
/// language it renders in.
///
/// Only the translator and the language are shared. The style is a global too, but
/// which globals a test needs is part of what that test is about, so each one
/// installs its own.
pub(crate) fn install_translator(cx: &mut App) {
    let i18n = I18n::builder()
        .locale_yaml(LanguageCode::En, MESSAGES_EN)
        .expect("test locale should parse")
        .locale_yaml(LanguageCode::ZhCn, MESSAGES_ZH)
        .expect("test locale should parse")
        .fallback(LanguageCode::En)
        .build()
        .expect("test translator should build");

    cx.set_global(i18n);
    cx.set_global(LanguageCode::En);
}

/// A widget in the shape the framework drives, for the tests that need the whole path
/// rather than one piece of it.
///
/// It reports a [`Notice`] rather than a sentence and draws its state through the
/// [`WidgetSurface`] the framework built around it, and it counts the states it writes: a
/// language change has to reach the surface without the widget being asked for anything,
/// and that count is how a test sees whether it did.
///
/// It has no content: a notice stands in for a body, so this is the shape that draws one.
pub(crate) struct WaitingWidget {
    /// The state cell the framework created for this widget. A widget keeps the cell it
    /// reports into, and this one is no exception.
    status: Entity<WidgetStatus>,
    /// The surface the framework presents this widget through.
    ///
    /// A widget does not build it — the registry does, after the widget exists — so it is
    /// absent for the instant between the two, and `render` is not reached in that instant.
    surface: Option<Entity<WidgetSurface>>,
    /// How many states this widget has written.
    writes: Rc<Cell<usize>>,
}

impl WaitingWidget {
    /// Creates the fixture the way the framework does: the widget, and then the surface the
    /// registry builds around it out of the status cell and the erased body.
    pub(crate) fn build<C: AppContext>(cx: &mut C) -> Entity<Self> {
        let view = cx.new(|cx| {
            Self::new(
                WidgetInstanceId::default(),
                (),
                cx.new(|_| WidgetStatus::new()),
                |_| {},
                WidgetConfigChange::default(),
                WidgetAction::new("common.retry", |_| {}),
                cx,
            )
        });
        let status = view.read_with(cx, |widget, _| widget.status.clone());
        let body = view.clone().into();
        let surface =
            cx.new(|cx| WidgetSurface::new(status, body, has_content_of::<WaitingWidget>(), cx));

        view.update(cx, |widget, _| widget.surface = Some(surface));

        view
    }

    /// The state this widget last wrote.
    pub(crate) fn state(&self, cx: &App) -> WidgetState {
        self.status.state(cx)
    }

    /// The surface the framework presents this widget through.
    pub(crate) fn surface(&self) -> &Entity<WidgetSurface> {
        self.surface
            .as_ref()
            .expect("the fixture is built with its surface")
    }

    /// How many states this widget has written.
    pub(crate) fn writes(&self) -> usize {
        self.writes.get()
    }
}

impl WidgetView for WaitingWidget {
    type Config = ();

    const ID: WidgetId = WidgetId::new_static("test.waiting");
    const DISPLAY_NAME: &'static str = "test.waiting";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        _recovery: WidgetAction,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            status,
            surface: None,
            writes: Rc::new(Cell::new(0)),
        }
    }

    /// The fixture waits for a market it cannot load: what its load path reports is the
    /// reference to the copy, and the copy itself is resolved where it is drawn.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.writes.set(self.writes.get() + 1);
        self.status
            .clone()
            .report(WidgetState::waiting(Notice::message("test.notice")), cx);
    }

    fn has_content(&self) -> bool {
        false
    }
}

impl Render for WaitingWidget {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.surface().clone()
    }
}
