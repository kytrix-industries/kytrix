use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::engine::KytrixServiceError;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::{TextAlign, TextStyle},
};
use ui_kit::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::{IconError, IconKind, IconSpec},
        size::{UnifiedSizable, UnifiedSize},
        tooltip::{Tooltip, TooltipContent, TooltipOptions},
    },
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::TranslatedError,
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use crate::{
    gauge::{Gauge, level_color},
    model::{
        CoinMarketCapFearAndGreedIndexParseError, CoinMarketCapFearAndGreedIndexRow,
        CoinMarketCapFearAndGreedIndexSnapshot, Level,
    },
    service::{
        CoinMarketCapFearAndGreedIndexFetchError, CoinMarketCapFearAndGreedIndexProtocol,
        CoinMarketCapFearAndGreedIndexRequest, CoinMarketCapFearAndGreedIndexService,
        CoinMarketCapFearAndGreedIndexServiceError, CoinMarketCapFearAndGreedIndexSource,
        CoinMarketCapFearAndGreedIndexStreamKey, CoinMarketCapFearAndGreedIndexStreamMessage,
    },
};

const MISSING: &str = "—";

const LEVEL_MARKER_SIZE: f32 = 8.0;

const VALUE_COLUMN_WIDTH: f32 = 28.0;

const SEMIBOLD: u16 = 600;

const ROW_TEXT_SIZE: UnifiedSize = UnifiedSize::Medium;

const SMALL_TEXT_SIZE: UnifiedSize = UnifiedSize::Small;

const ICON_SIZE: UnifiedSize = UnifiedSize::Medium;

const ROW_COUNT: usize = 6;

const HISTORY_ROWS: usize = 4;

const SECTION_COUNT: usize = 2;

const BODY_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_BODY";

const EMPTY_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_EMPTY";

const REFRESH_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_REFRESH";

const HEADER_KEY_SUFFIX: &str = "coin_market_cap_fear_and_greed_index.header.refresh";

const ROW_YESTERDAY: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_YESTERDAY";
const ROW_LAST_WEEK: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_LAST_WEEK";
const ROW_LAST_MONTH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_LAST_MONTH";
const ROW_LAST_YEAR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_LAST_YEAR";
const ROW_YEARLY_HIGH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_YEARLY_HIGH";
const ROW_YEARLY_LOW: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_YEARLY_LOW";

const MISSING_YESTERDAY: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_YESTERDAY";
const MISSING_LAST_WEEK: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_LAST_WEEK";
const MISSING_LAST_MONTH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_LAST_MONTH";
const MISSING_LAST_YEAR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_LAST_YEAR";
const MISSING_YEARLY_HIGH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_YEARLY_HIGH";
const MISSING_YEARLY_LOW: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_YEARLY_LOW";

const LEVEL_MARKER_YESTERDAY: &str =
    "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_LEVEL_MARKER_YESTERDAY";
const LEVEL_MARKER_LAST_WEEK: &str =
    "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_LEVEL_MARKER_LAST_WEEK";
const LEVEL_MARKER_LAST_MONTH: &str =
    "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_LEVEL_MARKER_LAST_MONTH";
const LEVEL_MARKER_LAST_YEAR: &str =
    "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_LEVEL_MARKER_LAST_YEAR";
const LEVEL_MARKER_YEARLY_HIGH: &str =
    "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_LEVEL_MARKER_YEARLY_HIGH";
const LEVEL_MARKER_YEARLY_LOW: &str =
    "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_LEVEL_MARKER_YEARLY_LOW";

#[derive(Clone, Copy, Debug)]
struct RowDescriptor {
    row: &'static str,
    marker: &'static str,
    missing: &'static str,
    label: &'static str,
    dated: bool,
}

const ROWS: [RowDescriptor; ROW_COUNT] = [
    RowDescriptor {
        row: ROW_YESTERDAY,
        marker: LEVEL_MARKER_YESTERDAY,
        missing: MISSING_YESTERDAY,
        label: "widget.coin_market_cap_fear_and_greed_index.yesterday",
        dated: false,
    },
    RowDescriptor {
        row: ROW_LAST_WEEK,
        marker: LEVEL_MARKER_LAST_WEEK,
        missing: MISSING_LAST_WEEK,
        label: "widget.coin_market_cap_fear_and_greed_index.last_week",
        dated: false,
    },
    RowDescriptor {
        row: ROW_LAST_MONTH,
        marker: LEVEL_MARKER_LAST_MONTH,
        missing: MISSING_LAST_MONTH,
        label: "widget.coin_market_cap_fear_and_greed_index.last_month",
        dated: false,
    },
    RowDescriptor {
        row: ROW_LAST_YEAR,
        marker: LEVEL_MARKER_LAST_YEAR,
        missing: MISSING_LAST_YEAR,
        label: "widget.coin_market_cap_fear_and_greed_index.last_year",
        dated: false,
    },
    RowDescriptor {
        row: ROW_YEARLY_HIGH,
        marker: LEVEL_MARKER_YEARLY_HIGH,
        missing: MISSING_YEARLY_HIGH,
        label: "widget.coin_market_cap_fear_and_greed_index.yearly_high",
        dated: true,
    },
    RowDescriptor {
        row: ROW_YEARLY_LOW,
        marker: LEVEL_MARKER_YEARLY_LOW,
        missing: MISSING_YEARLY_LOW,
        label: "widget.coin_market_cap_fear_and_greed_index.yearly_low",
        dated: true,
    },
];

#[derive(Clone, Copy, Debug)]
struct SectionDescriptor {
    title: &'static str,
    info: &'static str,
    key: &'static str,
}

const SECTIONS: [SectionDescriptor; SECTION_COUNT] = [
    SectionDescriptor {
        title: "widget.coin_market_cap_fear_and_greed_index.title",
        info: "widget.coin_market_cap_fear_and_greed_index.info_history",
        key: "coin_market_cap_fear_and_greed_index.info.history",
    },
    SectionDescriptor {
        title: "widget.coin_market_cap_fear_and_greed_index.yearly_extremes",
        info: "widget.coin_market_cap_fear_and_greed_index.info_yearly",
        key: "coin_market_cap_fear_and_greed_index.info.yearly",
    },
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoinMarketCapFearAndGreedIndexConfig;

impl WidgetConfig for CoinMarketCapFearAndGreedIndexConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CoinMarketCapFearAndGreedIndexFailure {
    Fetch(CoinMarketCapFearAndGreedIndexFetchError),
    Rpc(RpcCallError<CoinMarketCapFearAndGreedIndexServiceError>),
}

impl TranslatedError for CoinMarketCapFearAndGreedIndexFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Fetch(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for CoinMarketCapFearAndGreedIndexFetchError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Service(error) => error.translated_message(translate),
            Self::Parse(error) => error.translated_message(translate),
        }
    }
}

impl TranslatedError for CoinMarketCapFearAndGreedIndexParseError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::ScoreOutOfRange { value } => t_smol!(
                translate,
                "error.coin_market_cap_fear_and_greed_index.score_out_of_range",
                value = value
            ),
        }
    }
}

impl TranslatedError for CoinMarketCapFearAndGreedIndexServiceError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::StreamClosed | Self::StreamBusy => t_smol!(
                translate,
                "error.coin_market_cap_fear_and_greed_index.stream_unavailable"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowsKey([bool; ROW_COUNT]);

impl RowsKey {
    fn of(snapshot: &CoinMarketCapFearAndGreedIndexSnapshot) -> Self {
        Self(std::array::from_fn(|index| {
            row_at(snapshot, index).is_some()
        }))
    }
}

struct HeaderControl {
    button: Button,
    node: NodeId,
    #[cfg(test)]
    action: ui_core::event::Action,
    language: LanguageCode,
    disabled: bool,
}

struct Card {
    root: NodeId,
    history_rows: NodeId,
    extreme_rows: NodeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Load,
    Refresh,
}

pub struct CoinMarketCapFearAndGreedIndexWidget {
    binding: Option<WidgetBinding<CoinMarketCapFearAndGreedIndexProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    gauge: Gauge,
    snapshot: Option<Arc<CoinMarketCapFearAndGreedIndexSnapshot>>,
    items: Signal<Option<Arc<CoinMarketCapFearAndGreedIndexSnapshot>>>,
    language: Signal<LanguageCode>,
    refresh_pending: bool,
    account_id: Option<SmolStr>,
    refresh_requests: Signal<u64>,
    seen_refresh_requests: u64,
    header: Option<HeaderControl>,
    body_root: Option<NodeId>,
    card: Option<Card>,
    rows_built: Option<RowsKey>,
    empty: Option<NodeId>,
    empty_language: Option<LanguageCode>,
    tooltips: [Option<Tooltip>; SECTION_COUNT],
    tooltips_language: Option<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
}

impl WidgetView for CoinMarketCapFearAndGreedIndexWidget {
    type Config = CoinMarketCapFearAndGreedIndexConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.coin_market_cap_fear_and_greed_index");
    const DISPLAY_NAME: &'static str = "widget.names.coin_market_cap_fear_and_greed_index";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> =
        Some("widget.descriptions.coin_market_cap_fear_and_greed_index");
    const ICON: Option<&'static str> = Some("widgets/coin_market_cap_fear_and_greed_index.png");
    const SELECTABLE: bool = true;

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let source = cx.services().kytrix_service().cloned();

        Ok(Self::assemble(source, status, recovery, cx))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the fear and greed pane could not start: {error}");
        }
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let root = NodeBuilder::boxed()
            .id(BODY_SELECTOR)
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);
        self.sync_body_content(cx)?;

        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx)?;
        }

        let messages = self
            .binding
            .as_ref()
            .map(WidgetBinding::take)
            .unwrap_or_default();
        for message in messages {
            match message {
                Inbound::Reliable { key, message } => {
                    self.apply_reliable(key, message, cx);
                }
                Inbound::Latest { key, message } => {
                    self.apply_latest(key, message, cx);
                }
                Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
            }
        }

        let requests = *cx.read(&self.refresh_requests);
        if requests != self.seen_refresh_requests {
            self.seen_refresh_requests = requests;
            self.refresh(cx)?;
        }

        self.sync_body_content(cx)?;
        self.sync_gauge(cx)?;
        self.sync_tooltips(cx)?;
        self.sync_header(cx)?;

        if self.snapshot.is_some() && self.body_root.is_none() {
            cx.request_frame()?;
        }

        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if group != HeaderGroup::Right {
            return Ok(());
        }
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        self.release_header(cx, false);
        self.release_tooltips(cx);
        self.gauge.release(cx.app_mut());
        if let Some(card) = self.card.take() {
            let _ = remove_own_node(cx, card.root);
        }
        self.rows_built = None;
        self.empty = None;
        self.empty_language = None;
        if let Some(root) = self.body_root.take() {
            let _ = remove_own_node(cx, root);
        }
        self.snapshot = None;
    }
}

impl CoinMarketCapFearAndGreedIndexWidget {
    fn assemble<S: CoinMarketCapFearAndGreedIndexSource>(
        source: Option<S>,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let binding = source.map(|source| {
            WidgetBinding::spawn(cx, CoinMarketCapFearAndGreedIndexService::new(source))
        });
        let items = cx.signal(None::<Arc<CoinMarketCapFearAndGreedIndexSnapshot>>);
        let refresh_requests = cx.signal(0_u64);
        let language = cx.services().language;
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();
        let account_id = cx.account();

        Self {
            binding,
            pending: Pending::new(),
            status,
            recovery,
            gauge: Gauge::new(),
            snapshot: None,
            items,
            language,
            refresh_pending: false,
            account_id,
            refresh_requests,
            seen_refresh_requests: 0,
            header: None,
            body_root: None,
            card: None,
            rows_built: None,
            empty: None,
            empty_language: None,
            tooltips: [None, None],
            tooltips_language: None,
            translate,
            theme,
        }
    }

    #[cfg(test)]
    fn new_with_source<S: CoinMarketCapFearAndGreedIndexSource>(
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), status, recovery, cx)
    }

    fn current_key(&self) -> Option<CoinMarketCapFearAndGreedIndexStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| CoinMarketCapFearAndGreedIndexStreamKey { account_id })
    }

    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.refresh_pending || self.status.state(app) != WidgetState::Ready
    }

    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.refresh_pending = false;
        self.pending.clear();
        let Some(key) = self.current_key() else {
            self.clear_snapshot(cx)?;
            self.status.report(
                cx.app_mut(),
                WidgetState::waiting(Notice::message(
                    "widget.coin_market_cap_fear_and_greed_index.logged_out",
                )),
            );
            cx.request_frame()?;
            return Ok(());
        };
        if !retry {
            self.clear_snapshot(cx)?;
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        let Some(binding) = self.binding.as_mut() else {
            self.report_failure(
                CoinMarketCapFearAndGreedIndexFailure::Fetch(
                    CoinMarketCapFearAndGreedIndexFetchError::Service(KytrixServiceError::Closed),
                ),
                cx,
            );
            cx.request_frame()?;
            return Ok(());
        };
        let request_id = binding.call(
            cx,
            if retry {
                CoinMarketCapFearAndGreedIndexRequest::Retry(key)
            } else {
                CoinMarketCapFearAndGreedIndexRequest::Load(key)
            },
        );
        self.pending.track(request_id, PendingCommand::Load);
        cx.request_frame()?;

        Ok(())
    }

    fn clear_snapshot(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.snapshot = None;
        cx.write(&self.items, None);
        cx.request_frame()?;

        Ok(())
    }

    fn refresh(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.refresh_is_disabled(cx.app()) {
            return Ok(());
        }
        let Some(binding) = self.binding.as_mut() else {
            return Ok(());
        };
        self.refresh_pending = true;
        let request_id = binding.call(cx, CoinMarketCapFearAndGreedIndexRequest::Refresh);
        self.pending.track(request_id, PendingCommand::Refresh);
        cx.request_frame()?;

        Ok(())
    }

    fn report_failure(
        &self,
        failure: CoinMarketCapFearAndGreedIndexFailure,
        cx: &mut WidgetCx<'_>,
    ) {
        let recovery = self.recovery.clone();
        self.status.report(
            cx.app_mut(),
            WidgetState::failed(Arc::new(failure), recovery),
        );
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<CoinMarketCapFearAndGreedIndexProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        let Some(command) = self.pending.take(request_id) else {
            log::debug!(
                "the fear and greed pane ignored reply {request_id}: no command is waiting for it"
            );
            return;
        };
        if self.status.is_failed(cx.app()) {
            return;
        }
        let Err(error) = result else {
            return;
        };
        if command == PendingCommand::Refresh {
            self.refresh_pending = false;
        }
        self.report_failure(CoinMarketCapFearAndGreedIndexFailure::Rpc(error), cx);
    }

    fn apply_reliable(
        &mut self,
        key: CoinMarketCapFearAndGreedIndexStreamKey,
        message: Arc<CoinMarketCapFearAndGreedIndexStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let CoinMarketCapFearAndGreedIndexStreamMessage::Failed { error } = message.as_ref() else {
            return false;
        };
        self.refresh_pending = false;
        self.report_failure(
            CoinMarketCapFearAndGreedIndexFailure::Fetch(error.clone()),
            cx,
        );

        false
    }

    fn apply_latest(
        &mut self,
        key: CoinMarketCapFearAndGreedIndexStreamKey,
        message: Arc<CoinMarketCapFearAndGreedIndexStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        if self.current_key().as_ref() != Some(&key) || self.status.is_failed(cx.app()) {
            return false;
        }
        let CoinMarketCapFearAndGreedIndexStreamMessage::Snapshot(snapshot) = message.as_ref()
        else {
            return false;
        };
        let no_op = self
            .snapshot
            .as_ref()
            .is_some_and(|held| held.same_display(snapshot))
            && !self.refresh_pending
            && self.status.state(cx.app()) == WidgetState::Ready;
        if no_op {
            return false;
        }
        self.snapshot = Some(snapshot.clone());
        cx.write(&self.items, self.snapshot.clone());
        self.refresh_pending = false;
        self.status.report(cx.app_mut(), WidgetState::Ready);

        true
    }

    fn sync_body_content(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(root) = self.body_root else {
            return Ok(());
        };
        let Some(snapshot) = self.snapshot.clone() else {
            return Ok(());
        };
        self.sync_empty_state(cx)?;
        if snapshot.is_empty() {
            self.remove_card(cx)?;
            return Ok(());
        }
        if self.card.is_none() {
            self.build_card(cx, root, &snapshot)?;
        }
        self.sync_rows(cx, &snapshot)?;

        Ok(())
    }

    fn build_card(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        snapshot: &CoinMarketCapFearAndGreedIndexSnapshot,
    ) -> Result<(), EngineError> {
        let card = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .gap(8.0)
                    .padding(12.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.gauge.build(cx, card)?;
        let history_rows = self.build_section(cx, card, 0)?;
        let extreme_rows = self.build_section(cx, card, 1)?;
        self.card = Some(Card {
            root: card,
            history_rows,
            extreme_rows,
        });
        self.build_rows(cx, history_rows, 0..HISTORY_ROWS)?;
        self.build_rows(cx, extreme_rows, HISTORY_ROWS..ROW_COUNT)?;
        self.rows_built = Some(RowsKey::of(snapshot));

        Ok(())
    }

    fn build_section(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        index: usize,
    ) -> Result<NodeId, EngineError> {
        let descriptor = SECTIONS[index];
        let row = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let language = self.language;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            text_style(SMALL_TEXT_SIZE.text_size(), SEMIBOLD, None),
        )
        .bind_text(move |read| {
            let _language = read.read(&language);
            t_smol!(translate, descriptor.title)
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), row)?;

        let theme = self.theme.clone();
        let window = cx.window();
        let icon = IconSpec::new(IconKind::WarningCircle)
            .with_size(ICON_SIZE.icon_size())
            .build_themed(cx.app_mut(), window, row, &theme, Role::FgSubtle)
            .map_err(|error| icon_error(error, row))?;
        let key = cx.unique_key(descriptor.key);
        let layer = cx.layer().clone();
        let animation = cx.animation().clone();
        let content = t_smol!(self.translate, descriptor.info);
        let tooltip = Tooltip::new(
            cx.app_mut(),
            self.theme.clone(),
            layer,
            animation,
            key,
            TooltipOptions::default(),
        );
        tooltip.set_content(Some(TooltipContent::new(content)));
        tooltip.attach(cx.app_mut(), window, icon);
        self.tooltips[index] = Some(tooltip);

        let rows = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .gap(4.0)
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        Ok(rows)
    }

    fn build_rows(
        &self,
        cx: &mut WidgetCx<'_>,
        container: NodeId,
        range: std::ops::Range<usize>,
    ) -> Result<(), EngineError> {
        for index in range {
            self.build_row(cx, container, index)?;
        }

        Ok(())
    }

    fn build_row(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        index: usize,
    ) -> Result<(), EngineError> {
        let descriptor = ROWS[index];
        let present = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| row_at(snapshot, index).is_some());
        let row = NodeBuilder::boxed()
            .id(descriptor.row)
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(8.0)
                    .width_full()
                    .min_width_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let language = self.language;
        let items = self.items;
        let translate = self.translate.clone();
        let theme = self.theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            text_style(ROW_TEXT_SIZE.text_size(), 400, None),
        )
        .style(StyleBuilder::new().flex_1().min_width_0().build())
        .bind_text(move |read| {
            let _language = read.read(&language);
            let snapshot = read.read(&items);
            row_label(descriptor, snapshot.as_deref(), index, &translate)
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), row)?;

        let right = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(8.0)
                    .flex_none()
                    .build(),
            )
            .build(cx.tree_mut(), row)?;

        if present {
            self.build_level_marker(cx, right, index)?;
        }

        let items = self.items;
        let language = self.language;
        let theme = self.theme.clone();
        let mut value = NodeBuilder::text(
            SmolStr::default(),
            text_style(ROW_TEXT_SIZE.text_size(), SEMIBOLD, Some(TextAlign::End)),
        )
        .style(
            StyleBuilder::new()
                .width(VALUE_COLUMN_WIDTH)
                .flex_none()
                .build(),
        )
        .bind_text(move |read| {
            let _language = read.read(&language);
            let snapshot = read.read(&items);
            value_text(
                snapshot
                    .as_deref()
                    .and_then(|snapshot| row_at(snapshot, index)),
            )
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        });
        if !present {
            value = value.id(descriptor.missing);
        }
        value.build(cx.tree_mut(), right)?;

        Ok(())
    }

    fn build_level_marker(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        index: usize,
    ) -> Result<(), EngineError> {
        let descriptor = ROWS[index];
        let marker = NodeBuilder::boxed()
            .id(descriptor.marker)
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(4.0)
                    .flex_none()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let appearance = self.theme.current(cx.app());
        let initial = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| row_at(snapshot, index))
            .map_or_else(
                || self.theme.theme_for(appearance).role(Role::FgDefault),
                |row| level_color(self.theme.theme_for(appearance), Level::of(row.score)),
            );
        let items = self.items;
        let theme = self.theme.clone();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .size(LEVEL_MARKER_SIZE, LEVEL_MARKER_SIZE)
                    .flex_none()
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .background(initial)
                    .rounded(LEVEL_MARKER_SIZE / 2.0)
                    .build(),
            )
            .bind_style(move |read| {
                let snapshot = read.read(&items);
                let level = snapshot
                    .as_deref()
                    .and_then(|snapshot| row_at(snapshot, index))
                    .map(|row| Level::of(row.score));
                match level {
                    Some(level) => StylePatch {
                        background: Some(level_color(theme.theme(read), level)),
                        ..StylePatch::default()
                    },
                    None => StylePatch::default(),
                }
            })
            .build(cx.tree_mut(), marker)?;

        let language = self.language;
        let translate = self.translate.clone();
        let items = self.items;
        let theme = self.theme.clone();
        NodeBuilder::text(
            SmolStr::default(),
            text_style(SMALL_TEXT_SIZE.text_size(), 400, None),
        )
        .bind_text(move |read| {
            let _language = read.read(&language);
            let snapshot = read.read(&items);
            level_name(
                snapshot
                    .as_deref()
                    .and_then(|snapshot| row_at(snapshot, index)),
                &translate,
            )
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), marker)?;

        Ok(())
    }

    fn sync_rows(
        &mut self,
        cx: &mut WidgetCx<'_>,
        snapshot: &CoinMarketCapFearAndGreedIndexSnapshot,
    ) -> Result<(), EngineError> {
        let Some(card) = self.card.as_ref() else {
            return Ok(());
        };
        let key = RowsKey::of(snapshot);
        if self.rows_built == Some(key) {
            return Ok(());
        }
        let (history_rows, extreme_rows) = (card.history_rows, card.extreme_rows);
        replace_children(cx, history_rows)?;
        replace_children(cx, extreme_rows)?;
        self.build_rows(cx, history_rows, 0..HISTORY_ROWS)?;
        self.build_rows(cx, extreme_rows, HISTORY_ROWS..ROW_COUNT)?;
        self.rows_built = Some(key);
        cx.request_frame()?;

        Ok(())
    }

    fn remove_card(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(card) = self.card.take() else {
            return Ok(());
        };
        self.rows_built = None;
        self.release_tooltips(cx);
        self.gauge.release(cx.app_mut());
        remove_own_node(cx, card.root)
    }

    fn sync_empty_state(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let empty = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.is_empty());
        if !empty {
            if let Some(node) = self.empty.take() {
                remove_own_node(cx, node)?;
                self.empty_language = None;
            }
            return Ok(());
        }

        let language = *cx.read(&self.language);
        let stale = self
            .empty
            .is_none_or(|_| self.empty_language != Some(language));
        if !stale {
            return Ok(());
        }
        if let Some(node) = self.empty.take() {
            remove_own_node(cx, node)?;
        }
        let Some(root) = self.body_root else {
            return Ok(());
        };

        let overlay = NodeBuilder::boxed()
            .id(EMPTY_SELECTOR)
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(0.0)
                    .right(0.0)
                    .bottom(0.0)
                    .left(0.0)
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .build(cx.tree_mut(), root)?;
        let text = t_smol!(self.translate, "common.no_data");
        EmptyState::new(self.theme.clone(), EMPTY_SELECTOR)
            .text(text)
            .build(cx.tree_mut(), overlay)?;
        self.empty = Some(overlay);
        self.empty_language = Some(language);

        Ok(())
    }

    fn sync_gauge(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(snapshot) = self.snapshot.clone() else {
            return Ok(());
        };
        if snapshot.is_empty() || self.card.is_none() {
            return Ok(());
        }
        let level = Level::of(snapshot.current);
        let value = snapshot.current;
        let level_name = t_smol!(self.translate, level.key());
        self.gauge.sync(cx, value, &level_name)
    }

    fn sync_tooltips(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        if self.tooltips_language != Some(language) {
            let translate = self.translate.clone();
            for (index, tooltip) in self.tooltips.iter().enumerate() {
                if let Some(tooltip) = tooltip {
                    tooltip.set_content(Some(TooltipContent::new(t_smol!(
                        translate,
                        SECTIONS[index].info
                    ))));
                }
            }
            self.tooltips_language = Some(language);
        }
        for tooltip in self.tooltips.iter().flatten() {
            let _ = tooltip.sync(cx.app_mut());
        }

        Ok(())
    }

    fn release_tooltips(&mut self, cx: &mut WidgetCx<'_>) {
        for tooltip in self.tooltips.iter_mut() {
            if let Some(tooltip) = tooltip.take() {
                tooltip.release(cx.app_mut());
            }
        }
        self.tooltips_language = None;
    }

    fn build_header(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language);
        let label = t_smol!(
            self.translate,
            "widget.coin_market_cap_fear_and_greed_index.refresh"
        );
        let key = cx.unique_key(HEADER_KEY_SUFFIX);
        let theme = self.theme.clone();
        let requests = self.refresh_requests;
        let disabled = self.refresh_is_disabled(cx.app());
        let window = cx.window();
        let node = NodeBuilder::boxed()
            .id(REFRESH_SELECTOR)
            .style(StyleBuilder::new().flex_none().build())
            .build(cx.tree_mut(), parent)?;
        let button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(IconKind::ArrowsClockwise)
            .icon_size(ICON_SIZE.icon_size())
            .a11y_label(label)
            .disabled(disabled)
            .on_click(move |ctx| {
                ctx.update(&requests, |count| *count += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        button.build(cx.app_mut(), window, node)?;
        self.header = Some(HeaderControl {
            #[cfg(test)]
            action: button
                .action_identity()
                .expect("a click callback registered the control's action"),
            button,
            node,
            language,
            disabled,
        });

        Ok(())
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let disabled = self.refresh_is_disabled(cx.app());
        let Some(header) = self.header.as_mut() else {
            return Ok(());
        };
        header.button.sync(cx.app_mut())?;
        if disabled != header.disabled {
            let _ = header.button.set_disabled(cx.app_mut(), disabled);
            header.disabled = disabled;
        }
        let language = *cx.read(&self.language);
        if header.language == language {
            return Ok(());
        }
        let node = header.node;
        let parent = cx
            .tree()
            .get(node)
            .ok()
            .and_then(ui_core::node::tree::Node::parent);
        self.release_header(cx, true);
        match parent {
            Some(parent) if cx.tree().get(parent).is_ok() => self.build_header(parent, cx),
            _ => Ok(()),
        }
    }

    fn release_header(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header.take() else {
            return;
        };
        if remove_node && cx.tree().get(header.node).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, header.node);
        }
        let _ = header.button.release(cx.app_mut());
    }
}

fn text_style(size: f32, weight: u16, alignment: Option<TextAlign>) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.weight = weight;
    if let Some(alignment) = alignment {
        style.alignment = alignment;
    }

    style
}

fn row_at(
    snapshot: &CoinMarketCapFearAndGreedIndexSnapshot,
    index: usize,
) -> Option<&CoinMarketCapFearAndGreedIndexRow> {
    match index {
        0 => snapshot.yesterday.as_ref(),
        1 => snapshot.last_week.as_ref(),
        2 => snapshot.last_month.as_ref(),
        3 => snapshot.last_year.as_ref(),
        4 => snapshot.yearly_high.as_ref(),
        5 => snapshot.yearly_low.as_ref(),
        _ => None,
    }
}

fn row_label(
    descriptor: RowDescriptor,
    snapshot: Option<&CoinMarketCapFearAndGreedIndexSnapshot>,
    index: usize,
    translate: &Translate,
) -> SmolStr {
    let row = snapshot.and_then(|snapshot| row_at(snapshot, index));
    if descriptor.dated {
        t_smol!(translate, descriptor.label, date = date_text(row))
    } else {
        t_smol!(translate, descriptor.label)
    }
}

fn level_name(row: Option<&CoinMarketCapFearAndGreedIndexRow>, translate: &Translate) -> SmolStr {
    row.map_or_else(SmolStr::default, |row| {
        t_smol!(translate, Level::of(row.score).key())
    })
}

fn value_text(row: Option<&CoinMarketCapFearAndGreedIndexRow>) -> SmolStr {
    row.map_or_else(
        || SmolStr::new_static(MISSING),
        |row| format_smolstr!("{}", row.score),
    )
}

fn date_text(row: Option<&CoinMarketCapFearAndGreedIndexRow>) -> SmolStr {
    row.map_or_else(|| SmolStr::new_static(MISSING), |row| row.date.clone())
}

fn replace_children(cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<(), EngineError> {
    let children = cx
        .tree()
        .get(parent)
        .map(|target| target.children().to_vec())
        .unwrap_or_default();
    for child in children {
        remove_own_node(cx, child)?;
    }

    Ok(())
}

fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

fn icon_error(error: IconError, parent: NodeId) -> EngineError {
    match error {
        IconError::Decode(error) | IconError::Node(error) => error,
        IconError::UnknownWindow { window } => EngineError::UnknownWindow { window },
        IconError::InvalidSize { .. } | IconError::UnknownKind { .. } => {
            EngineError::StaleNode { node: parent }
        }
    }
}

#[cfg(test)]
mod tests;
