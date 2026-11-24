use std::{cell::RefCell, convert::Infallible, rc::Rc, sync::Arc};

use ahash::{AHashMap, AHashSet};
use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use market_participants_connector::connector::MarketParticipantsConnector;
use market_participants_model::{
    identifier::{VenueId, WalletAddress},
    listing::route::VenueRoute,
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    error::EngineError,
    event::{Action, Event, InputEvent},
    math::{LogicalPoint, LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        style::{Cursor, StylePatch},
        style_builder::StyleBuilder,
    },
    text::{TextOverflow, TextStyle},
};
use ui_kit::{
    kit::{
        button::Button,
        icon::IconKind,
        list::{List, ListOptions, SelectionMode},
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::{TranslatedError, account_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    detail::DetailTables,
    dialog::{AddressDialogError, AddressNotice, DialogRequest, DialogView, WalletAddressDialog},
    model::{DetailSnapshot, DetailTab, TimelineSnapshot, WalletOverviewSnapshot},
    service::{
        ClientWalletSource, WalletProtocol, WalletRequest, WalletService, WalletStatus,
        WalletStreamError, WalletStreamKey, WalletStreamMessage, loaded_scopes, wallet_routes_of,
    },
    timeline::{self, TimelinePaint},
    watch::{self, WatchListError, deserialize_addresses},
};

const ACCOUNT_ROW_HEIGHT: LogicalPx = px(88.0);
const WALLET_ROW_HEIGHT: LogicalPx =
    px(ACCOUNT_ROW_HEIGHT.value() + timeline::HEIGHT.value() + 1.0);
const ROW_OVERSCAN: usize = 2;

const ROW_CONTROL_SIZE: LogicalPx = px(24.0);
const ADDRESS_TEXT_SIZE: UnifiedSize = UnifiedSize::Medium;

const META_TEXT_SIZE: UnifiedSize = UnifiedSize::Small;

const MAX_ROWS: usize = watch::MAX_WALLET_ADDRESSES;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct HyperliquidWalletConfig {
    #[serde(deserialize_with = "deserialize_addresses")]
    addresses: Vec<WalletAddress>,
}

impl HyperliquidWalletConfig {
    #[cfg(test)]
    pub(super) fn monitoring(addresses: Vec<WalletAddress>) -> Self {
        Self { addresses }
    }
}

impl WidgetConfig for HyperliquidWalletConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HyperliquidWalletFailure {
    Stream(WalletStreamError),
    Rpc(RpcCallError<Infallible>),
}

impl TranslatedError for HyperliquidWalletFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

fn is_waiting_for_a_catalog(failure: &HyperliquidWalletFailure) -> bool {
    matches!(
        failure,
        HyperliquidWalletFailure::Stream(WalletStreamError::Account(error))
            if account_is_catalog_not_loaded(error)
    )
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum ScopeStatus {
    #[default]
    Loading,
    Ready,
    Failed(Arc<HyperliquidWalletFailure>),
}

#[derive(Default)]
struct AccountRowState {
    snapshot: Option<Arc<WalletOverviewSnapshot>>,
    status: ScopeStatus,
    ready: bool,
}

#[derive(Default)]
struct TimelineRowState {
    snapshot: Option<Arc<TimelineSnapshot>>,
    status: ScopeStatus,
    ready: bool,
}

struct DetailEntry {
    snapshot: Option<Arc<DetailSnapshot>>,
    status: ScopeStatus,
    ready: bool,
}

#[derive(Clone, Debug)]
struct WalletRowData {
    address: WalletAddress,
    overview: Option<Arc<WalletOverviewSnapshot>>,
    overview_status: ScopeStatus,
    timeline: Option<Arc<TimelineSnapshot>>,
    timeline_status: ScopeStatus,
}

#[derive(Clone, Debug)]
struct WalletRows {
    rows: Arc<[WalletRowData]>,
}

impl PartialEq for WalletRows {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.rows, &other.rows)
    }
}

impl Eq for WalletRows {}

impl Default for WalletRows {
    fn default() -> Self {
        Self {
            rows: Arc::from([]),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Shown {
    Overview,
    Detail(WalletAddress, DetailTab),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingCommand {
    Scope,
    Overview(Vec<WalletAddress>),
    Timeline(WalletAddress),
    Detail(WalletAddress, DetailTab),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Intent {
    ManageAddresses,
    Back,
    RetryFailed,
    RetryWallet(WalletAddress),
    RetryTimeline(WalletAddress),
    RefreshTab,
    SelectTab(DetailTab),
    OpenWallet(usize),
}

trait WalletDirectory {
    fn wallet_routes(&self) -> Vec<VenueRoute>;

    fn is_loaded(&self, route: &VenueRoute) -> bool;
}

struct ConnectorDirectory(MarketParticipantsConnector);

impl WalletDirectory for ConnectorDirectory {
    fn wallet_routes(&self) -> Vec<VenueRoute> {
        wallet_routes_of(&loaded_scopes(self.0.venues()))
    }

    fn is_loaded(&self, route: &VenueRoute) -> bool {
        self.0.venues().scope(route).is_some()
    }
}

fn market_venues() -> Vec<VenueId> {
    market_participants_connector::venue::definition::venues_supporting(CapabilityId::Wallet)
}

#[derive(Clone, Copy, Debug, Default)]
struct RowSlots {
    copy: Option<NodeId>,
    retry: Option<NodeId>,
    timeline_retry: Option<NodeId>,
}

struct RowControls {
    address: WalletAddress,
    copy: Button,
    retry: Option<Button>,
    timeline_retry: Option<Button>,
    language: LanguageCode,
    overview_failed: bool,
    timeline_failed: bool,
}

struct HeaderControls {
    manage: Button,
    back: Button,
    retry: Button,
    refresh: Button,
    manage_wrapper: NodeId,
    back_wrapper: NodeId,
    retry_wrapper: NodeId,
    refresh_wrapper: NodeId,
    parent: NodeId,
    retry_built: (LanguageCode, usize),
}

pub struct HyperliquidWalletWidget {
    config: HyperliquidWalletConfig,
    config_change: WidgetConfigChange,
    status: WidgetStatus,
    recovery: WidgetAction,
    binding: Option<WidgetBinding<WalletProtocol>>,
    pending: Pending<PendingCommand>,
    directory: Option<Rc<dyn WalletDirectory>>,
    overview: AHashMap<WalletAddress, AccountRowState>,
    timelines: AHashMap<WalletAddress, TimelineRowState>,
    detail: AHashMap<(WalletAddress, DetailTab), DetailEntry>,
    selected: Option<WalletAddress>,
    selected_tab: DetailTab,
    awaiting_market: bool,
    markets: Arc<[VenueRoute]>,
    market_reloaded: bool,
    seen_generation: u64,
    generation: Signal<u64>,
    rows: Signal<WalletRows>,
    page: Signal<bool>,
    selected_address: Signal<Option<WalletAddress>>,
    tab: Signal<DetailTab>,
    failed: Signal<usize>,
    intent: Signal<Option<Intent>>,
    intents: Signal<u64>,
    seen_intents: u64,
    language: LanguageCode,
    language_signal: Signal<LanguageCode>,
    translate: Translate,
    body_root: Option<NodeId>,
    list: Option<List>,
    row_slots: Rc<RefCell<AHashMap<usize, RowSlots>>>,
    row_canvases: Rc<RefCell<AHashMap<usize, NodeId>>>,
    row_controls: AHashMap<usize, RowControls>,
    timeline_paints: AHashMap<usize, (NodeId, TimelinePaint)>,
    hover: Option<(usize, LogicalPoint)>,
    hovered_row: Option<usize>,
    row_actions: Vec<Action>,
    detail_page: Option<NodeId>,
    tab_strip: Option<NodeId>,
    tab_buttons: Vec<(DetailTab, Button)>,
    tables: Option<DetailTables>,
    header: Option<HeaderControls>,
    header_parent: Option<NodeId>,
    dialog: Option<WalletAddressDialog>,
    notice: Option<AddressNotice>,
    accepted_detail: Option<(WalletAddress, DetailTab, Arc<DetailSnapshot>)>,
    detail_request: Option<(WalletAddress, DetailTab)>,
}

impl WidgetView for HyperliquidWalletWidget {
    type Config = HyperliquidWalletConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.hyperliquid_wallet");
    const DISPLAY_NAME: &'static str = "widget.names.hyperliquid_wallet";
    const CATEGORY: WidgetCategory = WidgetCategory::Monitoring;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.hyperliquid_wallet");
    const ICON: Option<&'static str> = Some("widgets/hyperliquid_wallet.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        let Some(connector) = cx.services().market_participants.clone() else {
            return Ok(Self::assemble(
                None,
                None,
                config,
                status,
                recovery,
                config_change,
                cx,
            ));
        };
        let source = ClientWalletSource::new(&connector);
        let directory: Rc<dyn WalletDirectory> = Rc::new(ConnectorDirectory(connector));
        let binding = WidgetBinding::spawn(cx, WalletService::new(source));
        Ok(Self::assemble(
            Some(binding),
            Some(directory),
            config,
            status,
            recovery,
            config_change,
            cx,
        ))
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        if !self.market_ready() {
            self.wait_for_market(cx);
            return;
        }
        self.markets = self.needed_markets();
        self.awaiting_market = false;
        let market_reloaded = std::mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        if !retry {
            self.detail.clear();
            self.show_overview_signal(cx);
            self.accepted_detail = None;
            self.clear_detail_tables(cx);
            if let Err(error) = self.seed(cx) {
                log::error!("the hyperliquid wallet pane could not seed its rows: {error}");
            }
        }
        self.pending.clear();
        self.detail_request = None;
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        self.follow(retry, market_reloaded, cx);
    }

    fn has_content(&self) -> bool {
        if self.awaiting_market {
            return false;
        }
        match self.shown() {
            Shown::Overview => {
                !self.config.addresses.is_empty()
                    && self.overview.values().any(|row| row.snapshot.is_some())
            }
            Shown::Detail(_, _) => true,
        }
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if group != HeaderGroup::Left {
            return Ok(());
        }
        self.release_header(cx);
        self.header_parent = Some(parent);
        self.build_header_controls(cx, parent)
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let language_signal = self.language_signal;
        let page = self.page;
        let selected = self.selected_address;

        let root = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_BODY")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .relative()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);

        let overview = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .left(px(0.0))
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_style(move |read| StylePatch {
                visible: Some(!*read.read(&page)),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), root)?;
        let a11y_translate = translate.clone();
        let list_holder = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_LIST")
            .a11y(A11yOverride {
                role: Some(A11yRole::ScrollView),
                ..A11yOverride::default()
            })
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_a11y(move |read| {
                let _language = *read.read(&language_signal);
                A11yPatch {
                    label: Some(t_smol!(
                        a11y_translate,
                        "widget.hyperliquid_wallet.scroll_rows"
                    )),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), overview)?;
        self.list = Some(self.make_list(cx, list_holder, &theme)?);
        self.update_rows(cx)?;

        let detail = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .top(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .left(px(0.0))
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_style(move |read| StylePatch {
                visible: Some(*read.read(&page)),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), root)?;
        self.detail_page = Some(detail);
        let heading = NodeBuilder::boxed()
            .id("TEST_HYPERLIQUID_WALLET_DETAIL_HEADING")
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .width_full()
                    .min_width_0()
                    .padding_x(px(8.0))
                    .padding_y(px(4.0))
                    .build(),
            )
            .build(cx.tree_mut(), detail)?;
        NodeBuilder::text(SmolStr::default(), addressed_text_style())
            .bind_text(move |read| {
                let address = (*read.read(&selected)).clone();
                address.map_or_else(SmolStr::default, |address| SmolStr::new(address.as_str()))
            })
            .bind_style({
                let theme = theme.clone();
                move |read| StylePatch {
                    foreground: Some(theme.role(read, Role::FgDefault)),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), heading)?;

        let strip = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .flex_none()
                    .width_full()
                    .min_width_0()
                    .items_center()
                    .padding(px(4.0))
                    .build(),
            )
            .build(cx.tree_mut(), detail)?;
        self.tab_strip = Some(strip);
        self.build_tab_buttons(cx, strip, &theme, &translate)?;

        self.tables = Some(DetailTables::build(cx, detail, page)?);
        if let Some(tables) = &self.tables {
            tables.show(self.selected_tab, cx)?;
        }
        self.sync_selected_tables(cx)?;

        cx.request_frame()?;
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                self.market_reloaded = !self.markets.is_empty();
                self.load(cx);
            }
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
                Inbound::Rpc { request_id, result } => {
                    self.apply_rpc(request_id, result, cx);
                }
            }
        }

        let intents = *cx.read(&self.intents);
        if intents != self.seen_intents {
            self.seen_intents = intents;
            if let Some(intent) = cx.read(&self.intent).clone() {
                self.apply_intent(intent, cx)?;
            }
        }

        self.sync_dialog(cx)?;
        if let Some(list) = &self.list {
            list.sync(cx.app_mut())?;
        }
        self.sync_row_controls(cx)?;
        self.sync_timelines(cx)?;
        if let Some(tables) = &self.tables {
            tables.sync(cx)?;
        }
        self.sync_selected_tables(cx)?;
        self.sync_tab_buttons(cx);
        self.sync_header(cx);
        self.report(cx);
        self.rebuild_static_copy(cx)?;

        if self.has_content() && self.body_root.is_none() {
            cx.request_frame()?;
        }
        Ok(())
    }

    fn dispatch(&mut self, event: &Event, cx: &mut WidgetCx<'_>) -> bool {
        match event {
            Event::Input(InputEvent::PointerMove { position, .. })
            | Event::Input(InputEvent::PointerEnter { position, .. }) => {
                self.track_hover(*position, cx)
            }
            Event::Input(InputEvent::PointerLeave { .. }) => {
                self.track_hover(LogicalPoint::new(px(-1.0), px(-1.0)), cx)
            }
            _ => false,
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.binding = None;
        self.release_header(cx);
        if let Some(mut dialog) = self.dialog.take() {
            dialog.release(cx);
        }
        let controls = std::mem::take(&mut self.row_controls);
        for (_, controls) in controls {
            release_row_controls(controls, cx);
        }
        for action in std::mem::take(&mut self.row_actions) {
            cx.app_mut().off_action(&action);
        }
        for (_, button) in std::mem::take(&mut self.tab_buttons) {
            button.release(cx.app_mut());
        }
        if let Some(tables) = self.tables.take() {
            tables.release(cx);
        }
        if let Some(list) = self.list.take() {
            let _ = list.release(cx.app_mut());
        }
        if let Some(root) = self.body_root.take() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, root);
        }
        for (_, (_, mut paint)) in std::mem::take(&mut self.timeline_paints) {
            paint.release(cx);
        }
        self.row_slots.borrow_mut().clear();
        self.row_canvases.borrow_mut().clear();
        self.overview.clear();
        self.timelines.clear();
        self.detail.clear();
        self.accepted_detail = None;
        self.detail_request = None;
    }
}

impl HyperliquidWalletWidget {
    fn assemble(
        binding: Option<WidgetBinding<WalletProtocol>>,
        directory: Option<Rc<dyn WalletDirectory>>,
        config: HyperliquidWalletConfig,
        status: WidgetStatus,
        recovery: WidgetAction,
        config_change: WidgetConfigChange,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let rows = cx.signal(WalletRows::default());
        let page = cx.signal(false);
        let selected_address = cx.signal(None);
        let tab = cx.signal(DetailTab::Spot);
        let failed = cx.signal(0_usize);
        let intent = cx.signal(None);
        let intents = cx.signal(0_u64);
        let language_signal = cx.language();
        let language = *cx.read(&language_signal);
        let translate = cx.translate().clone();
        Self {
            config,
            config_change,
            status,
            recovery,
            binding,
            pending: Pending::new(),
            directory,
            overview: AHashMap::new(),
            timelines: AHashMap::new(),
            detail: AHashMap::new(),
            selected: None,
            selected_tab: DetailTab::Spot,
            awaiting_market: false,
            markets: Arc::default(),
            market_reloaded: false,
            seen_generation,
            generation,
            rows,
            page,
            selected_address,
            tab,
            failed,
            intent,
            intents,
            seen_intents: 0,
            language,
            language_signal,
            translate,
            body_root: None,
            list: None,
            row_slots: Rc::new(RefCell::new(AHashMap::new())),
            row_canvases: Rc::new(RefCell::new(AHashMap::new())),
            row_controls: AHashMap::new(),
            timeline_paints: AHashMap::new(),
            hover: None,
            hovered_row: None,
            row_actions: Vec::new(),
            detail_page: None,
            tab_strip: None,
            tab_buttons: Vec::new(),
            tables: None,
            header: None,
            header_parent: None,
            dialog: None,
            notice: None,
            accepted_detail: None,
            detail_request: None,
        }
    }

    fn make_list(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        theme: &ThemeHandle,
    ) -> Result<List, EngineError> {
        let base = cx.unique_key("hyperliquid_wallet.rows");
        self.row_actions.clear();
        for index in 0..MAX_ROWS {
            let action = Action::Custom(format_smolstr!("{base}.row.{index}.open"));
            let intent = self.intent;
            let intents = self.intents;
            cx.app_mut().on_action(action.clone(), move |ctx| {
                ctx.write(&intent, Some(Intent::OpenWallet(index)));
                ctx.update(&intents, |count| *count += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
            self.row_actions.push(action);
        }

        let context = RowContext {
            rows: self.rows,
            language: self.language_signal,
            translate: self.translate.clone(),
            theme: theme.clone(),
            actions: self.row_actions.clone(),
            slots: Rc::clone(&self.row_slots),
            canvases: Rc::clone(&self.row_canvases),
        };
        let key = cx.unique_key("hyperliquid_wallet.list");
        let list = List::new(
            cx.app_mut(),
            theme.clone(),
            key,
            ListOptions::fixed(WALLET_ROW_HEIGHT)
                .overscan(ROW_OVERSCAN)
                .padding(px(0.0), px(0.0))
                .selection(SelectionMode::None),
            move |tree, parent, index| build_wallet_row(tree, parent, index, &context),
        );
        let count = self.config.addresses.len();
        let window = cx.window();
        list.set_initial_count(cx.app_mut(), count);
        list.build(cx.tree_mut(), parent)?;
        list.attach(cx.app_mut(), window)?;
        Ok(list)
    }

    fn build_tab_buttons(
        &mut self,
        cx: &mut WidgetCx<'_>,
        strip: NodeId,
        theme: &ThemeHandle,
        translate: &Translate,
    ) -> Result<(), EngineError> {
        let window = cx.window();
        for tab in DetailTab::ALL {
            let index = tab.index();
            let key = cx.unique_key(&format_smolstr!("hyperliquid_wallet.tab.{index}"));
            let label = tab_label(tab, translate);
            let intent = self.intent;
            let intents = self.intents;
            let button = Button::new(cx.app_mut(), theme.clone(), key)
                .outline()
                .small()
                .label(label)
                .selected(tab == self.selected_tab)
                .on_click(move |ctx| {
                    ctx.write(&intent, Some(Intent::SelectTab(tab)));
                    ctx.update(&intents, |count| *count += 1);
                    let window = ctx.window();
                    let _ = ctx.app_mut().request_repaint(window);
                });
            button.build(cx.app_mut(), window, strip)?;
            self.tab_buttons.push((tab, button));
        }
        Ok(())
    }

    fn rebuild_tab_buttons(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(detail) = self.detail_page else {
            return Ok(());
        };
        if cx.tree().get(detail).is_err() {
            return Ok(());
        }
        if let Some(strip) = self.tab_strip
            && cx.tree().get(strip).is_err()
        {
            return Ok(());
        }
        for (_, button) in std::mem::take(&mut self.tab_buttons) {
            button.release(cx.app_mut());
        }
        if let Some(strip) = self.tab_strip.take() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, strip);
        }
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();
        let strip = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .flex_none()
                    .width_full()
                    .min_width_0()
                    .items_center()
                    .padding(px(4.0))
                    .build(),
            )
            .build(cx.tree_mut(), detail)?;
        self.tab_strip = Some(strip);
        self.build_tab_buttons(cx, strip, &theme, &translate)
    }

    fn apply_intent(&mut self, intent: Intent, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        match intent {
            Intent::ManageAddresses => {
                if self.awaiting_market && !self.config.addresses.is_empty() {
                    return Ok(());
                }
                self.open_dialog(cx)?;
            }
            Intent::Back => {
                self.show_overview_signal(cx);
                self.report(cx);
                cx.request_frame()?;
            }
            Intent::RetryFailed => self.retry_failed(cx),
            Intent::RetryWallet(address) => self.retry_wallet(address, cx),
            Intent::RetryTimeline(address) => self.retry_timeline(address, cx),
            Intent::RefreshTab => {
                if let Some(address) = self.selected.clone() {
                    self.load_detail(address, self.selected_tab, true, cx)?;
                }
            }
            Intent::SelectTab(tab) => {
                if let Some(address) = self.selected.clone() {
                    self.selected_tab = tab;
                    cx.write(&self.tab, tab);
                    if let Some(tables) = &self.tables {
                        tables.show(tab, cx)?;
                        if self
                            .detail
                            .get(&(address.clone(), tab))
                            .is_none_or(|entry| entry.snapshot.is_none())
                        {
                            tables.set_loading(tab, true, cx)?;
                        }
                    }
                    self.sync_selected_tables(cx)?;
                    self.load_detail(address, tab, false, cx)?;
                }
            }
            Intent::OpenWallet(index) => {
                let address = self.config.addresses.get(index).cloned();
                if let Some(address) = address {
                    self.open_wallet(address, cx)?;
                }
            }
        }
        Ok(())
    }

    fn open_wallet(
        &mut self,
        address: WalletAddress,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        if self.selected.as_ref() == Some(&address) {
            return Ok(());
        }
        self.accepted_detail = None;
        self.clear_detail_tables(cx);
        self.selected = Some(address.clone());
        self.selected_tab = DetailTab::Spot;
        cx.write(&self.selected_address, Some(address.clone()));
        cx.write(&self.tab, DetailTab::Spot);
        cx.write(&self.page, true);
        if let Some(tables) = &self.tables {
            tables.show(DetailTab::Spot, cx)?;
        }
        self.load_detail(address, DetailTab::Spot, false, cx)?;
        self.report(cx);
        cx.request_frame()?;
        Ok(())
    }

    fn show_overview_signal(&mut self, cx: &mut WidgetCx<'_>) {
        self.selected = None;
        self.selected_tab = DetailTab::Spot;
        cx.write(&self.selected_address, None);
        cx.write(&self.tab, DetailTab::Spot);
        cx.write(&self.page, false);
    }

    fn follow(&mut self, retry: bool, market_reloaded: bool, cx: &mut WidgetCx<'_>) {
        match self.selected.clone() {
            Some(address) => {
                if let Err(error) = self.load_detail(address, self.selected_tab, retry, cx) {
                    log::error!("the hyperliquid wallet pane could not read its tab: {error}");
                }
            }
            None => self.request_overview(retry, market_reloaded, cx),
        }
    }

    fn seed(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.overview = self
            .config
            .addresses
            .iter()
            .cloned()
            .map(|address| (address, AccountRowState::default()))
            .collect();
        self.timelines = self
            .config
            .addresses
            .iter()
            .cloned()
            .map(|address| (address, TimelineRowState::default()))
            .collect();
        if let Some(list) = &self.list {
            list.set_items(cx.app_mut(), self.config.addresses.len())?;
        }
        self.update_rows(cx)?;
        cx.request_frame()?;
        Ok(())
    }

    fn update_rows(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let rows: Arc<[WalletRowData]> = self
            .config
            .addresses
            .iter()
            .map(|address| {
                let account = self.overview.get(address);
                let timeline = self.timelines.get(address);
                WalletRowData {
                    address: address.clone(),
                    overview: account.and_then(|row| row.snapshot.clone()),
                    overview_status: account.map_or(ScopeStatus::Loading, |row| row.status.clone()),
                    timeline: timeline.and_then(|row| row.snapshot.clone()),
                    timeline_status: timeline
                        .map_or(ScopeStatus::Loading, |row| row.status.clone()),
                }
            })
            .collect::<Vec<_>>()
            .into();
        cx.write(&self.rows, WalletRows { rows });
        self.sync_failed_count(cx);
        Ok(())
    }

    fn sync_failed_count(&self, cx: &mut WidgetCx<'_>) {
        let failed = self
            .overview
            .values()
            .filter(|row| matches!(row.status, ScopeStatus::Failed(_)))
            .count();
        cx.write(&self.failed, failed);
    }

    fn request_overview(&mut self, retry: bool, market_reloaded: bool, cx: &mut WidgetCx<'_>) {
        let request = if market_reloaded {
            WalletRequest::Reload {
                addresses: self.config.addresses.clone(),
            }
        } else if retry {
            WalletRequest::Retry {
                addresses: self.failed_addresses(),
            }
        } else {
            WalletRequest::Load {
                addresses: self.config.addresses.clone(),
            }
        };
        self.call(cx, request, PendingCommand::Scope);
        self.report(cx);
    }

    fn call(&mut self, cx: &mut WidgetCx<'_>, request: WalletRequest, command: PendingCommand) {
        self.detail_request = None;
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let request_id = binding.call(cx, request);
        self.pending.track(request_id, command);
    }

    fn failed_addresses(&self) -> Vec<WalletAddress> {
        self.config
            .addresses
            .iter()
            .filter(|address| {
                self.overview
                    .get(address)
                    .is_some_and(|row| matches!(row.status, ScopeStatus::Failed(_)))
            })
            .cloned()
            .collect()
    }

    fn load_detail(
        &mut self,
        address: WalletAddress,
        tab: DetailTab,
        retry: bool,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let key = (address.clone(), tab);
        self.detail
            .entry(key.clone())
            .or_insert_with(|| DetailEntry {
                snapshot: None,
                status: ScopeStatus::Loading,
                ready: false,
            });
        let Some(entry) = self.detail.get_mut(&key) else {
            return Ok(());
        };
        if !retry && entry.snapshot.is_some() {
            self.report(cx);
            cx.request_frame()?;
            return Ok(());
        }
        let replace = retry || matches!(entry.status, ScopeStatus::Failed(_));
        entry.status = ScopeStatus::Loading;
        entry.ready = false;
        if let Some(tables) = &self.tables {
            tables.show(tab, cx)?;
            tables.set_loading(tab, true, cx)?;
        }
        self.update_rows(cx)?;
        self.report(cx);
        self.call(
            cx,
            if replace {
                WalletRequest::RetryDetail { address, tab }
            } else {
                WalletRequest::LoadDetail { address, tab }
            },
            PendingCommand::Detail(key.0.clone(), key.1),
        );
        self.detail_request = Some(key);
        cx.request_frame()?;
        Ok(())
    }

    fn retry_timeline(&mut self, address: WalletAddress, cx: &mut WidgetCx<'_>) {
        let Some(row) = self.timelines.get_mut(&address) else {
            return;
        };
        row.status = ScopeStatus::Loading;
        row.ready = false;
        let _ = self.update_rows(cx);
        self.report(cx);
        self.call(
            cx,
            WalletRequest::RetryTimeline {
                address: address.clone(),
            },
            PendingCommand::Timeline(address),
        );
        let _ = cx.request_frame();
    }

    fn retry_wallet(&mut self, address: WalletAddress, cx: &mut WidgetCx<'_>) {
        if let Some(row) = self.overview.get_mut(&address) {
            row.status = ScopeStatus::Loading;
            row.ready = false;
        }
        let _ = self.update_rows(cx);
        self.report(cx);
        self.call(
            cx,
            WalletRequest::Retry {
                addresses: vec![address.clone()],
            },
            PendingCommand::Overview(vec![address]),
        );
        let _ = cx.request_frame();
    }

    fn retry_failed(&mut self, cx: &mut WidgetCx<'_>) {
        let failed = self.failed_addresses();
        if failed.is_empty() {
            return;
        }
        for address in &failed {
            if let Some(row) = self.overview.get_mut(address) {
                row.status = ScopeStatus::Loading;
                row.ready = false;
            }
        }
        let _ = self.update_rows(cx);
        self.report(cx);
        self.call(
            cx,
            WalletRequest::Retry {
                addresses: failed.clone(),
            },
            PendingCommand::Overview(failed),
        );
        let _ = cx.request_frame();
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<WalletProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        let Some(pending) = self.pending.take(request_id) else {
            return;
        };
        let Err(error) = result else {
            return;
        };
        let failure = Arc::new(HyperliquidWalletFailure::Rpc(error));
        match pending {
            PendingCommand::Scope => {
                for row in self.overview.values_mut() {
                    if matches!(row.status, ScopeStatus::Loading) {
                        row.status = ScopeStatus::Failed(Arc::clone(&failure));
                    }
                }
                for row in self.timelines.values_mut() {
                    if matches!(row.status, ScopeStatus::Loading) {
                        row.status = ScopeStatus::Failed(Arc::clone(&failure));
                    }
                }
            }
            PendingCommand::Overview(addresses) => {
                for address in addresses {
                    if let Some(row) = self.overview.get_mut(&address) {
                        row.status = ScopeStatus::Failed(Arc::clone(&failure));
                    }
                }
            }
            PendingCommand::Timeline(address) => {
                if let Some(row) = self.timelines.get_mut(&address) {
                    row.status = ScopeStatus::Failed(Arc::clone(&failure));
                }
            }
            PendingCommand::Detail(address, tab) => {
                if self.detail_request.as_ref() == Some(&(address.clone(), tab)) {
                    self.detail_request = None;
                }
                if let Some(entry) = self.detail.get_mut(&(address, tab)) {
                    entry.status = ScopeStatus::Failed(failure);
                    entry.snapshot = None;
                }
                if let Some(tables) = &self.tables {
                    let _ = tables.discard(tab, cx);
                }
            }
        }
        let _ = self.update_rows(cx);
        self.report(cx);
        if let Err(error) = self.sync_selected_tables(cx) {
            log::error!("the hyperliquid wallet pane could not redraw its detail: {error}");
        }
    }

    fn apply_latest(
        &mut self,
        key: WalletStreamKey,
        message: Arc<WalletStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        match (key, message.as_ref()) {
            (WalletStreamKey::Overview { address }, WalletStreamMessage::Overview(snapshot)) => {
                if !self.follows(&address) {
                    return;
                }
                let Some(row) = self.overview.get_mut(&address) else {
                    return;
                };
                row.snapshot = Some(snapshot.clone());
                if row.ready {
                    row.status = ScopeStatus::Ready;
                }
                let _ = self.update_rows(cx);
                self.report(cx);
            }
            (WalletStreamKey::Timeline { address }, WalletStreamMessage::Timeline(snapshot)) => {
                if !self.follows(&address) {
                    return;
                }
                let Some(row) = self.timelines.get_mut(&address) else {
                    return;
                };
                let changed = !row
                    .snapshot
                    .as_ref()
                    .is_some_and(|held| Arc::ptr_eq(held, snapshot));
                row.snapshot = Some(snapshot.clone());
                if row.ready {
                    row.status = ScopeStatus::Ready;
                }
                if changed {
                    let _ = self.update_rows(cx);
                    let _ = cx.request_frame();
                }
            }
            (WalletStreamKey::Detail { address, tab }, WalletStreamMessage::Detail(snapshot)) => {
                if !self.follows(&address) {
                    return;
                }
                if self.detail_request.as_ref() == Some(&(address.clone(), tab)) {
                    self.detail_request = None;
                }
                let Some(entry) = self.detail.get_mut(&(address, tab)) else {
                    return;
                };
                entry.snapshot = Some(snapshot.clone());
                if entry.ready {
                    entry.status = ScopeStatus::Ready;
                }
                let _ = self.update_rows(cx);
                self.report(cx);
                if let Err(error) = self.sync_selected_tables(cx) {
                    log::error!("the hyperliquid wallet pane could not draw its detail: {error}");
                }
            }
            _ => {}
        }
    }

    fn apply_reliable(
        &mut self,
        key: WalletStreamKey,
        message: Arc<WalletStreamMessage>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        let WalletStreamMessage::Status(status) = message.as_ref() else {
            return;
        };
        let changed = match key {
            WalletStreamKey::Overview { address } => {
                if !self.follows(&address) {
                    return;
                }
                let Some(row) = self.overview.get_mut(&address) else {
                    return;
                };
                apply_status(&mut row.status, &mut row.ready, status)
            }
            WalletStreamKey::Timeline { address } => {
                if !self.follows(&address) {
                    return;
                }
                let Some(row) = self.timelines.get_mut(&address) else {
                    return;
                };
                apply_status(&mut row.status, &mut row.ready, status)
            }
            WalletStreamKey::Detail { address, tab } => {
                if !self.follows(&address) {
                    return;
                }
                if matches!(status, WalletStatus::Failed(_))
                    && self.detail_request.as_ref() == Some(&(address.clone(), tab))
                {
                    self.detail_request = None;
                }
                let Some(entry) = self.detail.get_mut(&(address, tab)) else {
                    return;
                };
                apply_status(&mut entry.status, &mut entry.ready, status)
            }
        };
        if changed {
            let _ = self.update_rows(cx);
            self.report(cx);
            if let Err(error) = self.sync_selected_tables(cx) {
                log::error!("the hyperliquid wallet pane could not draw its detail: {error}");
            }
        }
    }

    fn follows(&self, address: &WalletAddress) -> bool {
        self.config.addresses.contains(address)
    }

    fn shown(&self) -> Shown {
        match self.selected.clone() {
            Some(address) => Shown::Detail(address, self.selected_tab),
            None => Shown::Overview,
        }
    }

    fn market_ready(&self) -> bool {
        let Some(directory) = &self.directory else {
            return false;
        };
        let routes = directory.wallet_routes();
        !routes.is_empty() && routes.iter().all(|route| directory.is_loaded(route))
    }

    fn needed_markets(&self) -> Arc<[VenueRoute]> {
        self.directory
            .as_ref()
            .map(|directory| directory.wallet_routes().into())
            .unwrap_or_default()
    }

    fn report_notice(&self, cx: &mut WidgetCx<'_>) {
        let notice = if self.awaiting_market {
            Some(Notice::MarketNotLoaded {
                venues: vec![VenueId::Hyperliquid],
                supported: market_venues(),
            })
        } else if self.config.addresses.is_empty() {
            Some(Notice::message("widget.hyperliquid_wallet.empty_addresses"))
        } else {
            None
        };
        if let Some(notice) = notice {
            self.status
                .report(cx.app_mut(), WidgetState::waiting(notice));
        }
    }

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.markets = Arc::default();
        self.pending.clear();
        self.detail_request = None;
        self.detail.clear();
        self.accepted_detail = None;
        self.clear_detail_tables(cx);
        self.show_overview_signal(cx);
        let _ = self.seed(cx);
        self.report_notice(cx);
        let _ = cx.request_frame();
    }

    fn report(&mut self, cx: &mut WidgetCx<'_>) {
        if self.awaiting_market {
            self.report_notice(cx);
            return;
        }
        match self.shown() {
            Shown::Overview => {
                if self.config.addresses.is_empty() {
                    self.report_notice(cx);
                    return;
                }
                if self.has_content() {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                    return;
                }
                if let Some(failure) = self.every_wallet_failed() {
                    if is_waiting_for_a_catalog(failure.as_ref()) {
                        let _ = cx.request_frame();
                        self.wait_for_market(cx);
                        return;
                    }
                    let recovery = self.recovery.clone();
                    self.status
                        .report(cx.app_mut(), WidgetState::failed(failure, recovery));
                    return;
                }
                self.status.report(cx.app_mut(), WidgetState::Connecting);
            }
            Shown::Detail(address, tab) => {
                let entry = self.detail.get(&(address, tab));
                let failure = entry.and_then(|entry| match &entry.status {
                    ScopeStatus::Failed(failure) => Some(failure.clone()),
                    ScopeStatus::Loading | ScopeStatus::Ready => None,
                });
                if failure
                    .as_ref()
                    .is_some_and(|failure| is_waiting_for_a_catalog(failure.as_ref()))
                {
                    self.wait_for_market(cx);
                    return;
                }
                if let Some(failure) = failure {
                    let recovery = self.recovery.clone();
                    self.status
                        .report(cx.app_mut(), WidgetState::failed(failure, recovery));
                } else if entry.is_some_and(|entry| entry.snapshot.is_none()) {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                }
            }
        }
    }

    fn every_wallet_failed(&self) -> Option<Arc<HyperliquidWalletFailure>> {
        let mut failure = None;
        for address in &self.config.addresses {
            match self.overview.get(address) {
                Some(AccountRowState {
                    status: ScopeStatus::Failed(error),
                    ..
                }) => failure = failure.or_else(|| Some(error.clone())),
                _ => return None,
            }
        }
        failure
    }

    fn open_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if self.dialog.is_some() {
            return Ok(());
        }
        self.notice = None;
        self.dialog = Some(WalletAddressDialog::open(cx)?);
        cx.request_frame()?;
        Ok(())
    }

    fn sync_dialog(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(mut dialog) = self.dialog.take() else {
            return Ok(());
        };
        let addresses = self.config.addresses.clone();
        let notice = self.notice.clone();
        let view = DialogView {
            addresses: &addresses,
            limit: watch::MAX_WALLET_ADDRESSES,
            notice: notice.as_ref(),
        };
        let requests = dialog.sync(cx, view)?;
        let open = dialog.is_open(cx.app());
        for request in requests {
            match request {
                DialogRequest::Add(address) => {
                    let result =
                        watch::with(&self.config.addresses, address).map_err(|error| match error {
                            WatchListError::TooMany => AddressDialogError::Limit,
                            WatchListError::Duplicate { .. } => AddressDialogError::Duplicate,
                        });
                    match result {
                        Ok(addresses) => {
                            self.notice = None;
                            self.store_addresses(addresses, cx)?;
                        }
                        Err(error) => self.notice = Some(AddressNotice::Problem(error)),
                    }
                }
                DialogRequest::Delete(address) => {
                    let addresses = watch::without(&self.config.addresses, &address);
                    self.notice = None;
                    self.store_addresses(addresses, cx)?;
                }
            }
        }
        if open {
            self.dialog = Some(dialog);
        } else {
            dialog.release(cx);
            self.notice = None;
        }
        Ok(())
    }
    fn store_addresses(
        &mut self,
        addresses: Vec<WalletAddress>,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        self.config.addresses = addresses;
        self.config_change.publish(cx.app_mut());
        self.seed(cx)?;
        self.detail
            .retain(|(address, _), _| self.config.addresses.contains(address));
        if self
            .selected
            .as_ref()
            .is_none_or(|address| !self.config.addresses.contains(address))
        {
            self.show_overview_signal(cx);
        }
        self.pending.clear();
        self.detail_request = None;
        self.accepted_detail = None;
        self.clear_detail_tables(cx);
        if self.awaiting_market {
            self.report_notice(cx);
        } else if !self.market_ready() {
            self.wait_for_market(cx);
        } else {
            self.markets = self.needed_markets();
            self.request_overview(false, false, cx);
        }
        cx.request_frame()?;
        Ok(())
    }

    fn clear_detail_tables(&self, cx: &mut WidgetCx<'_>) {
        if let Some(tables) = &self.tables
            && let Err(error) = tables.clear(cx)
        {
            log::error!("the hyperliquid wallet pane could not clear its tables: {error}");
        }
    }

    fn sync_selected_tables(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(tables) = &self.tables else {
            return Ok(());
        };
        let Some(address) = self.selected.clone() else {
            tables.set_loading(self.selected_tab, false, cx)?;
            return Ok(());
        };
        let tab = self.selected_tab;
        tables.show(tab, cx)?;
        let entry = self.detail.get(&(address.clone(), tab));
        let snapshot = entry.and_then(|entry| entry.snapshot.clone());
        let reading = self
            .detail_request
            .as_ref()
            .is_some_and(|(held, held_tab)| held == &address && *held_tab == tab);
        let failed = entry.is_some_and(|entry| matches!(entry.status, ScopeStatus::Failed(_)));
        let waiting = reading || (snapshot.is_none() && !failed);
        match &snapshot {
            Some(snapshot) if !reading => {
                let already =
                    self.accepted_detail
                        .as_ref()
                        .is_some_and(|(held, held_tab, accepted)| {
                            held == &address && *held_tab == tab && Arc::ptr_eq(accepted, snapshot)
                        });
                if !already {
                    tables.accept(tab, Arc::clone(snapshot), cx)?;
                    self.accepted_detail = Some((address, tab, Arc::clone(snapshot)));
                }
                tables.set_loading(tab, false, cx)?;
            }
            _ => tables.set_loading(tab, waiting, cx)?,
        }
        Ok(())
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(header) = &self.header else {
            return;
        };
        let addresses = self.config.addresses.len();
        header
            .manage
            .set_disabled(cx.app_mut(), self.awaiting_market && addresses > 0);
        header.back.set_disabled(cx.app_mut(), false);
        header.retry.set_disabled(cx.app_mut(), false);
        let loading = self.selected.as_ref().is_some_and(|address| {
            let key = (address.clone(), self.selected_tab);
            self.detail_request.as_ref() == Some(&key)
                || self
                    .detail
                    .get(&key)
                    .is_none_or(|entry| entry.snapshot.is_none())
        });
        header.refresh.set_disabled(cx.app_mut(), loading);
        let failed = *cx.read(&self.failed);
        let built = (self.language, failed);
        let stale = self
            .header
            .as_ref()
            .is_some_and(|header| header.retry_built != built);
        if stale && let Err(error) = self.rebuild_retry_control(cx, failed) {
            log::error!("the hyperliquid wallet header could not name the failure count: {error}");
        }
    }

    fn sync_tab_buttons(&mut self, cx: &mut WidgetCx<'_>) {
        for (tab, button) in &self.tab_buttons {
            button.set_selected(cx.app_mut(), *tab == self.selected_tab);
        }
    }

    fn sync_timelines(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(list) = &self.list else {
            return Ok(());
        };
        let mounted = list.mounted_rows(cx.app());
        let mounted_indices: AHashSet<usize> = mounted.iter().map(|(index, _)| *index).collect();
        let rows = cx.read(&self.rows).rows.clone();
        let language = self.language_signal;
        for (index, _) in &mounted {
            let Some(canvas) = self.row_canvases.borrow().get(index).copied() else {
                continue;
            };
            if cx.tree().get(canvas).is_err() {
                continue;
            }
            let Some(row) = rows.get(*index) else {
                continue;
            };
            let Some(overview) = row.overview.as_ref() else {
                continue;
            };
            let precision = overview.precision.clone();
            let snapshot = self
                .timelines
                .get(&row.address)
                .and_then(|timeline| timeline.snapshot.clone());
            let hover = self
                .hover
                .filter(|(row, _)| row == index)
                .map(|(_, point)| point);
            let entry = self
                .timeline_paints
                .entry(*index)
                .or_insert_with(|| (canvas, TimelinePaint::new(canvas)));
            if entry.0 != canvas {
                *entry = (canvas, TimelinePaint::new(canvas));
            }
            entry
                .1
                .sync(cx, snapshot.as_ref(), &precision, language, hover)?;
        }
        let stale: Vec<usize> = self
            .timeline_paints
            .keys()
            .copied()
            .filter(|index| !mounted_indices.contains(index))
            .collect();
        for index in stale {
            if let Some((_, mut paint)) = self.timeline_paints.remove(&index) {
                paint.release(cx);
            }
        }
        Ok(())
    }

    fn sync_row_controls(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(list) = &self.list else {
            return Ok(());
        };
        let mounted = list.mounted_rows(cx.app());
        let mounted_indices: AHashSet<usize> = mounted.iter().map(|(index, _)| *index).collect();
        let language = self.language;
        let rows = cx.read(&self.rows).rows.clone();

        let stale: Vec<usize> = self
            .row_controls
            .keys()
            .copied()
            .filter(|index| !mounted_indices.contains(index))
            .collect();
        for index in stale {
            if let Some(controls) = self.row_controls.remove(&index) {
                release_row_controls(controls, cx);
            }
        }

        for (index, _) in mounted {
            let Some(row) = rows.get(index) else {
                continue;
            };
            let overview_failed = matches!(row.overview_status, ScopeStatus::Failed(_));
            let timeline_failed = matches!(row.timeline_status, ScopeStatus::Failed(_));
            let rebuild = self.row_controls.get(&index).is_none_or(|controls| {
                controls.language != language
                    || controls.overview_failed != overview_failed
                    || controls.timeline_failed != timeline_failed
                    || controls.address != row.address
            });
            if !rebuild {
                continue;
            }
            if let Some(controls) = self.row_controls.remove(&index) {
                release_row_controls(controls, cx);
            }
            let slots = self
                .row_slots
                .borrow()
                .get(&index)
                .copied()
                .unwrap_or_default();
            if let Some(controls) = self.build_row_controls(
                cx,
                index,
                &row.address,
                overview_failed,
                timeline_failed,
                slots,
            )? {
                self.row_controls.insert(index, controls);
            }
        }
        Ok(())
    }

    fn build_row_controls(
        &mut self,
        cx: &mut WidgetCx<'_>,
        index: usize,
        address: &WalletAddress,
        overview_failed: bool,
        timeline_failed: bool,
        slots: RowSlots,
    ) -> Result<Option<RowControls>, EngineError> {
        let language = self.language;
        let window = cx.window();
        let theme = cx.theme().clone();
        let translate = cx.translate().clone();

        let Some(copy_slot) = slots.copy.filter(|node| cx.tree().get(*node).is_ok()) else {
            return Ok(None);
        };
        let copy_label = SmolStr::new(address.as_str());
        let copy_address = address.as_str().to_owned();
        let key = cx.unique_key(&format_smolstr!("hyperliquid_wallet.row.{index}.copy"));
        let copy = Button::new(cx.app_mut(), theme.clone(), key)
            .ghost()
            .small()
            .icon(IconKind::CopySimpleBold)
            .a11y_label(copy_label.clone())
            .tooltip_text(copy_label)
            .on_click(move |ctx| {
                let _ = ctx.app_mut().set_clipboard_text(&copy_address);
            });
        copy.build(cx.app_mut(), window, copy_slot)?;

        let mut retry = None;
        if overview_failed
            && let Some(slot) = slots.retry.filter(|node| cx.tree().get(*node).is_ok())
        {
            let label = t_smol!(translate, "widget.hyperliquid_wallet.refresh");
            let intent = self.intent;
            let intents = self.intents;
            let target = address.clone();
            let key = cx.unique_key(&format_smolstr!("hyperliquid_wallet.row.{index}.retry"));
            let button = Button::new(cx.app_mut(), theme.clone(), key)
                .ghost()
                .small()
                .icon(IconKind::ArrowsClockwise)
                .a11y_label(label.clone())
                .tooltip_text(label)
                .on_click(move |ctx| {
                    ctx.write(&intent, Some(Intent::RetryWallet(target.clone())));
                    ctx.update(&intents, |count| *count += 1);
                    let window = ctx.window();
                    let _ = ctx.app_mut().request_repaint(window);
                });
            button.build(cx.app_mut(), window, slot)?;
            retry = Some(button);
        }

        let mut timeline_retry = None;
        if timeline_failed
            && let Some(slot) = slots
                .timeline_retry
                .filter(|node| cx.tree().get(*node).is_ok())
        {
            let label = t_smol!(translate, "widget.hyperliquid_wallet.refresh");
            let intent = self.intent;
            let intents = self.intents;
            let target = address.clone();
            let key = cx.unique_key(&format_smolstr!(
                "hyperliquid_wallet.row.{index}.timeline_retry"
            ));
            let button = Button::new(cx.app_mut(), theme, key)
                .ghost()
                .small()
                .icon(IconKind::ArrowsClockwise)
                .a11y_label(label.clone())
                .tooltip_text(label)
                .on_click(move |ctx| {
                    ctx.write(&intent, Some(Intent::RetryTimeline(target.clone())));
                    ctx.update(&intents, |count| *count += 1);
                    let window = ctx.window();
                    let _ = ctx.app_mut().request_repaint(window);
                });
            button.build(cx.app_mut(), window, slot)?;
            timeline_retry = Some(button);
        }

        Ok(Some(RowControls {
            address: address.clone(),
            copy,
            retry,
            timeline_retry,
            language,
            overview_failed,
            timeline_failed,
        }))
    }

    fn rebuild_static_copy(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let language = *cx.read(&self.language_signal);
        if language == self.language {
            return Ok(());
        }
        self.language = language;
        self.rebuild_tab_buttons(cx)?;
        self.release_header(cx);
        self.build_header_again(cx)?;
        let controls = std::mem::take(&mut self.row_controls);
        for (_, controls) in controls {
            release_row_controls(controls, cx);
        }
        Ok(())
    }

    fn build_header_again(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(parent) = self.header_parent else {
            return Ok(());
        };
        if cx.tree().get(parent).is_err() {
            self.header_parent = None;
            return Ok(());
        }
        self.build_header_controls(cx, parent)
    }

    fn build_header_controls(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
    ) -> Result<(), EngineError> {
        let language = self.language;
        let failed = *cx.read(&self.failed);
        let translate = cx.translate().clone();
        let manage_label = t_smol!(translate, "widget.hyperliquid_wallet.manage_addresses");
        let back_label = t_smol!(translate, "widget.hyperliquid_wallet.back");
        let refresh_label = t_smol!(translate, "widget.hyperliquid_wallet.refresh");
        let (manage, manage_wrapper) = self.build_header_control(
            cx,
            parent,
            "hyperliquid_wallet.header.manage",
            IconKind::Faders,
            manage_label,
            Intent::ManageAddresses,
            Visibility::Overview,
        )?;
        let (back, back_wrapper) = self.build_header_control(
            cx,
            parent,
            "hyperliquid_wallet.header.back",
            IconKind::CaretLeft,
            back_label,
            Intent::Back,
            Visibility::Detail,
        )?;
        let (refresh, refresh_wrapper) = self.build_header_control(
            cx,
            parent,
            "hyperliquid_wallet.header.refresh",
            IconKind::ArrowsClockwise,
            refresh_label,
            Intent::RefreshTab,
            Visibility::Detail,
        )?;
        let (retry, retry_wrapper) = self.build_retry_control(cx, parent, failed, &translate)?;
        self.header = Some(HeaderControls {
            manage,
            back,
            retry,
            refresh,
            manage_wrapper,
            back_wrapper,
            retry_wrapper,
            refresh_wrapper,
            parent,
            retry_built: (language, failed),
        });
        Ok(())
    }

    fn build_header_control(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        suffix: &str,
        icon: IconKind,
        label: SmolStr,
        intent: Intent,
        visible: Visibility,
    ) -> Result<(Button, NodeId), EngineError> {
        let page = self.page;
        let failed = self.failed;
        let theme = cx.theme().clone();
        let window = cx.window();
        let intents = self.intents;
        let intent_signal = self.intent;
        let key = cx.unique_key(suffix);
        let button = Button::new(cx.app_mut(), theme, key)
            .ghost()
            .small()
            .icon(icon)
            .a11y_label(label.clone())
            .tooltip_text(label)
            .on_click(move |ctx| {
                ctx.write(&intent_signal, Some(intent.clone()));
                ctx.update(&intents, |count| *count += 1);
                let window = ctx.window();
                let _ = ctx.app_mut().request_repaint(window);
            });
        let wrapper = NodeBuilder::boxed()
            .style(StyleBuilder::new().flex_none().build())
            .bind_style(move |read| StylePatch {
                visible: Some(match visible {
                    Visibility::Overview => !*read.read(&page),
                    Visibility::Detail => *read.read(&page),
                    Visibility::Failed => !*read.read(&page) && *read.read(&failed) > 0,
                }),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        button.build(cx.app_mut(), window, wrapper)?;
        Ok((button, wrapper))
    }

    fn build_retry_control(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        failed: usize,
        translate: &Translate,
    ) -> Result<(Button, NodeId), EngineError> {
        self.build_header_control(
            cx,
            parent,
            "hyperliquid_wallet.header.retry_failed",
            IconKind::ArrowsClockwise,
            retry_failed_label(translate, failed),
            Intent::RetryFailed,
            Visibility::Failed,
        )
    }

    fn rebuild_retry_control(
        &mut self,
        cx: &mut WidgetCx<'_>,
        failed: usize,
    ) -> Result<(), EngineError> {
        let Some(parent) = self.header.as_ref().map(|header| header.parent) else {
            return Ok(());
        };
        if cx.tree().get(parent).is_err() {
            self.release_header(cx);
            self.header_parent = None;
            return Ok(());
        }
        let Some(mut header) = self.header.take() else {
            return Ok(());
        };
        let window = cx.window();
        if cx.tree().get(header.retry_wrapper).is_ok() {
            let _ = cx.app_mut().remove_node(window, header.retry_wrapper);
        }
        header.retry.release(cx.app_mut());
        let translate = cx.translate().clone();
        let (retry, retry_wrapper) = self.build_retry_control(cx, parent, failed, &translate)?;
        header.retry = retry;
        header.retry_wrapper = retry_wrapper;
        header.retry_built = (self.language, failed);
        self.header = Some(header);
        Ok(())
    }

    fn release_header(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(header) = self.header.take() else {
            return;
        };
        let window = cx.window();
        for wrapper in [
            header.manage_wrapper,
            header.back_wrapper,
            header.retry_wrapper,
            header.refresh_wrapper,
        ] {
            if cx.tree().get(wrapper).is_ok() {
                let _ = cx.app_mut().remove_node(window, wrapper);
            }
        }
        header.manage.release(cx.app_mut());
        header.back.release(cx.app_mut());
        header.retry.release(cx.app_mut());
        header.refresh.release(cx.app_mut());
    }

    fn track_hover(&mut self, position: LogicalPoint, cx: &mut WidgetCx<'_>) -> bool {
        let next = self.hover_target(position, cx);
        if next == self.hover {
            return false;
        }
        let previous = self.hovered_row;
        self.hover = next;
        self.hovered_row = next.map(|(row, _)| row);
        if let Some(list) = &self.list {
            for (index, node) in list.mounted_rows(cx.app()) {
                if Some(index) == previous || Some(index) == self.hovered_row {
                    let _ = cx.request_node_repaint(node);
                }
            }
        }
        let _ = cx.request_frame();
        true
    }

    fn hover_target(
        &self,
        position: LogicalPoint,
        cx: &WidgetCx<'_>,
    ) -> Option<(usize, LogicalPoint)> {
        let list = self.list.as_ref()?;
        for (index, row_node) in list.mounted_rows(cx.app()) {
            let Ok(row) = cx.tree().get(row_node) else {
                continue;
            };
            let bounds = row.layout().bounds;
            if !bounds.contains(position) {
                continue;
            }
            let canvas = self.row_canvases.borrow().get(&index).copied()?;
            let Ok(canvas_node) = cx.tree().get(canvas) else {
                continue;
            };
            let canvas_bounds = canvas_node.layout().bounds;
            if canvas_bounds.size.width <= px(0.0) || canvas_bounds.size.height <= px(0.0) {
                return None;
            }
            return Some((
                index,
                LogicalPoint::new(
                    position.x - canvas_bounds.origin.x,
                    position.y - canvas_bounds.origin.y,
                ),
            ));
        }
        None
    }
}

#[derive(Clone, Copy)]
enum Visibility {
    Overview,
    Detail,
    Failed,
}

fn apply_status(status: &mut ScopeStatus, ready: &mut bool, reported: &WalletStatus) -> bool {
    match reported {
        WalletStatus::Connecting => {
            *ready = false;
            if matches!(status, ScopeStatus::Loading) {
                return false;
            }
            *status = ScopeStatus::Loading;
            true
        }
        WalletStatus::Ready => {
            *ready = true;
            if matches!(status, ScopeStatus::Ready) {
                return false;
            }
            *status = ScopeStatus::Ready;
            true
        }
        WalletStatus::Failed(error) => {
            *ready = false;
            *status =
                ScopeStatus::Failed(Arc::new(HyperliquidWalletFailure::Stream(error.clone())));
            true
        }
    }
}

fn retry_failed_label(translate: &Translate, failed: usize) -> SmolStr {
    if failed == 1 {
        t_smol!(
            translate,
            "widget.hyperliquid_wallet.retry_failed_one",
            value = failed
        )
    } else {
        t_smol!(
            translate,
            "widget.hyperliquid_wallet.retry_failed",
            value = failed
        )
    }
}

fn tab_label(tab: DetailTab, translate: &Translate) -> SmolStr {
    match tab {
        DetailTab::Spot => t_smol!(translate, "widget.hyperliquid_wallet.tab_spot"),
        DetailTab::Perpetual => t_smol!(translate, "widget.hyperliquid_wallet.tab_perpetual"),
        DetailTab::OpenOrders => t_smol!(translate, "widget.hyperliquid_wallet.tab_open_orders"),
        DetailTab::Trades => t_smol!(translate, "widget.hyperliquid_wallet.tab_trades"),
        DetailTab::HistoricalOrders => {
            t_smol!(translate, "widget.hyperliquid_wallet.tab_historical_orders")
        }
        DetailTab::Ledger => t_smol!(translate, "widget.hyperliquid_wallet.tab_ledger"),
    }
}

fn addressed_text_style() -> TextStyle {
    TextStyle {
        size: ADDRESS_TEXT_SIZE.text_size(),
        overflow: TextOverflow::Ellipsis,
        ..TextStyle::new(ADDRESS_TEXT_SIZE.text_size())
    }
}

fn release_row_controls(controls: RowControls, cx: &mut WidgetCx<'_>) {
    let window = cx.window();
    for button in [
        Some(&controls.copy),
        controls.retry.as_ref(),
        controls.timeline_retry.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(node) = button.node()
            && cx.tree().get(node).is_ok()
        {
            let _ = cx.app_mut().remove_node(window, node);
        }
    }
    controls.copy.release(cx.app_mut());
    if let Some(retry) = controls.retry {
        retry.release(cx.app_mut());
    }
    if let Some(retry) = controls.timeline_retry {
        retry.release(cx.app_mut());
    }
}

struct RowContext {
    rows: Signal<WalletRows>,
    language: Signal<LanguageCode>,
    translate: Translate,
    theme: ThemeHandle,
    actions: Vec<Action>,
    slots: Rc<RefCell<AHashMap<usize, RowSlots>>>,
    canvases: Rc<RefCell<AHashMap<usize, NodeId>>>,
}

fn build_wallet_row(
    tree: &mut NodeTree,
    parent: NodeId,
    index: usize,
    context: &RowContext,
) -> Result<(), EngineError> {
    let rows = context.rows;
    let language = context.language;
    let translate = context.translate.clone();
    let theme = context.theme.clone();
    let actions = &context.actions;
    let slots = &context.slots;
    let canvases = &context.canvases;
    let mut built_slots = RowSlots::default();
    let row_style = StyleBuilder::new()
        .flex_col()
        .width_full()
        .height(WALLET_ROW_HEIGHT)
        .min_width_0()
        .build();
    let row = NodeBuilder::boxed()
        .id(format_smolstr!("TEST_HYPERLIQUID_WALLET_ROW_{index}"))
        .style(row_style)
        .on_click(
            actions
                .get(index)
                .cloned()
                .unwrap_or_else(|| Action::Custom(SmolStr::new_static("hyperliquid_wallet.noop"))),
        )
        .bind_style(|_read| StylePatch {
            cursor: Some(Cursor::Pointer),
            ..StylePatch::default()
        })
        .build(tree, parent)?;

    if index > 0 {
        let divider_theme = theme.clone();
        NodeBuilder::boxed()
            .id(format_smolstr!(
                "TEST_HYPERLIQUID_WALLET_ROW_{index}_DIVIDER"
            ))
            .style(
                StyleBuilder::new()
                    .width_full()
                    .height(px(1.0))
                    .flex_none()
                    .build(),
            )
            .bind_style(move |read| StylePatch {
                background: Some(divider_theme.role(read, Role::LineDivider)),
                ..StylePatch::default()
            })
            .build(tree, row)?;
    }

    let account = NodeBuilder::boxed()
        .id(format_smolstr!(
            "TEST_HYPERLIQUID_WALLET_ROW_{index}_ACCOUNT"
        ))
        .style(
            StyleBuilder::new()
                .flex_col()
                .flex_none()
                .justify_center()
                .width_full()
                .height(ACCOUNT_ROW_HEIGHT)
                .min_width_0()
                .padding_sides(px(4.0), px(8.0), px(4.0), px(8.0))
                .gap(px(2.0))
                .build(),
        )
        .build(tree, row)?;

    let address_line = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .width_full()
                .min_width_0()
                .gap(px(4.0))
                .build(),
        )
        .build(tree, account)?;
    let text_theme = theme.clone();
    NodeBuilder::text(SmolStr::default(), addressed_text_style())
        .id(format_smolstr!(
            "TEST_HYPERLIQUID_WALLET_ROW_{index}_ADDRESS"
        ))
        .bind_text(move |read| {
            read.read(&rows)
                .rows
                .get(index)
                .map_or_else(SmolStr::default, |row| SmolStr::new(row.address.as_str()))
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(text_theme.role(read, Role::FgDefault)),
            ..StylePatch::default()
        })
        .build(tree, address_line)?;
    built_slots.copy = Some(
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .width(ROW_CONTROL_SIZE)
                    .height(ROW_CONTROL_SIZE)
                    .build(),
            )
            .build(tree, address_line)?,
    );
    built_slots.retry = Some(
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_none()
                    .width(ROW_CONTROL_SIZE)
                    .height(ROW_CONTROL_SIZE)
                    .build(),
            )
            .build(tree, address_line)?,
    );

    let spot_line = NodeBuilder::boxed()
        .style(StyleBuilder::new().width_full().min_width_0().build())
        .build(tree, account)?;
    let spot_theme = theme.clone();
    let spot_translate = translate.clone();
    let spot_language = language;
    NodeBuilder::text(SmolStr::default(), meta_text_style())
        .id(format_smolstr!("TEST_HYPERLIQUID_WALLET_ROW_{index}_SPOT"))
        .bind_text(move |read| {
            let _language = *read.read(&spot_language);
            let rows = read.read(&rows);
            let Some(row) = rows.rows.get(index) else {
                return SmolStr::default();
            };
            let value = row.overview.as_ref().and_then(|snapshot| {
                snapshot
                    .summary
                    .spot_value
                    .map(|value| snapshot.precision.format_money_quantity(value))
            });
            let value = match (value, &row.overview_status) {
                (Some(value), _) => value,
                (None, ScopeStatus::Loading) => {
                    t_smol!(spot_translate, "common.loading")
                }
                (None, _) => SmolStr::new_static("-"),
            };
            format_smolstr!(
                "{}: {value}",
                t_smol!(spot_translate, "widget.hyperliquid_wallet.spot_value")
            )
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(spot_theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(tree, spot_line)?;

    let perpetual_line = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .width_full()
                .min_width_0()
                .gap(px(4.0))
                .build(),
        )
        .build(tree, account)?;
    let perpetual_theme = theme.clone();
    let perpetual_translate = translate.clone();
    let perpetual_language = language;
    NodeBuilder::text(SmolStr::default(), meta_text_style())
        .id(format_smolstr!(
            "TEST_HYPERLIQUID_WALLET_ROW_{index}_PERPETUAL"
        ))
        .bind_text(move |read| {
            let _language = *read.read(&perpetual_language);
            let rows = read.read(&rows);
            let Some(row) = rows.rows.get(index) else {
                return SmolStr::default();
            };
            let value = row.overview.as_ref().and_then(|snapshot| {
                snapshot
                    .summary
                    .perpetual_value
                    .map(|value| snapshot.precision.format_money_quantity(value))
            });
            let value = match (value, &row.overview_status) {
                (Some(value), _) => value,
                (None, ScopeStatus::Loading) => t_smol!(perpetual_translate, "common.loading"),
                (None, _) => SmolStr::new_static("-"),
            };
            format_smolstr!(
                "{}: {value}",
                t_smol!(
                    perpetual_translate,
                    "widget.hyperliquid_wallet.perpetual_value"
                )
            )
        })
        .bind_style(move |read| StylePatch {
            foreground: Some(perpetual_theme.role(read, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(tree, perpetual_line)?;

    let pnl_theme = theme.clone();
    let pnl_translate = translate.clone();
    let pnl_language = language;
    NodeBuilder::text(SmolStr::default(), meta_text_style())
        .id(format_smolstr!("TEST_HYPERLIQUID_WALLET_ROW_{index}_PNL"))
        .bind_text(move |read| {
            let _language = *read.read(&pnl_language);
            let rows = read.read(&rows);
            let Some(row) = rows.rows.get(index) else {
                return SmolStr::default();
            };
            let pnl = row.overview.as_ref().and_then(|snapshot| {
                snapshot
                    .summary
                    .unrealized_pnl
                    .map(|value| snapshot.precision.format_profit_or_loss(value))
            });
            let value = if row.overview.is_some() {
                pnl.unwrap_or_else(|| SmolStr::new_static("-"))
            } else {
                SmolStr::new_static("-")
            };
            format_smolstr!(
                "{}: {value}",
                t_smol!(pnl_translate, "widget.hyperliquid_wallet.unrealized_pnl")
            )
        })
        .bind_style(move |read| {
            let rows = read.read(&rows);
            let role = rows
                .rows
                .get(index)
                .and_then(|row| row.overview.as_ref())
                .and_then(|snapshot| snapshot.summary.unrealized_pnl)
                .map_or(Role::FgMuted, |value| match value.inner().cmp(&0) {
                    std::cmp::Ordering::Less => Role::MarketDown,
                    std::cmp::Ordering::Equal => Role::MarketFlat,
                    std::cmp::Ordering::Greater => Role::MarketUp,
                });
            StylePatch {
                foreground: Some(pnl_theme.role(read, role)),
                ..StylePatch::default()
            }
        })
        .build(tree, perpetual_line)?;

    let title_theme = theme.clone();
    let title_translate = translate.clone();
    let title_language = language;
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(META_TEXT_SIZE.text_size()),
    )
    .id(format_smolstr!(
        "TEST_HYPERLIQUID_WALLET_ROW_{index}_TIMELINE_TITLE"
    ))
    .bind_text(move |read| {
        let _language = *read.read(&title_language);
        t_smol!(title_translate, "widget.hyperliquid_wallet.timeline_title")
    })
    .bind_style(move |read| StylePatch {
        foreground: Some(title_theme.role(read, Role::FgSubtle)),
        ..StylePatch::default()
    })
    .build(tree, account)?;

    let timeline_area = NodeBuilder::boxed()
        .id(format_smolstr!(
            "TEST_HYPERLIQUID_WALLET_ROW_{index}_TIMELINE_AREA"
        ))
        .style(
            StyleBuilder::new()
                .relative()
                .flex_none()
                .width_full()
                .height(timeline::HEIGHT)
                .min_width_0()
                .build(),
        )
        .build(tree, row)?;
    let canvas = timeline::build_canvas(tree, timeline_area, theme.clone())?;
    canvases.borrow_mut().insert(index, canvas);

    let state_translate = translate;
    let state_language = language;
    let state = NodeBuilder::boxed()
        .id(format_smolstr!(
            "TEST_HYPERLIQUID_WALLET_ROW_{index}_TIMELINE_STATE"
        ))
        .style(
            StyleBuilder::new()
                .absolute()
                .left(px(0.0))
                .top(px(0.0))
                .size_full()
                .items_center()
                .justify_center()
                .build(),
        )
        .bind_style(move |read| {
            let rows = read.read(&rows);
            let visible = rows.rows.get(index).is_some_and(|row| {
                row.timeline.is_none() || matches!(row.timeline_status, ScopeStatus::Failed(_))
            });
            StylePatch {
                visible: Some(visible),
                ..StylePatch::default()
            }
        })
        .build(tree, timeline_area)?;
    let message_theme = theme;
    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(META_TEXT_SIZE.text_size()),
    )
    .id(format_smolstr!(
        "TEST_HYPERLIQUID_WALLET_ROW_{index}_TIMELINE_STATE_TEXT"
    ))
    .bind_text(move |read| {
        let _language = *read.read(&state_language);
        let rows = read.read(&rows);
        let Some(row) = rows.rows.get(index) else {
            return SmolStr::default();
        };
        match &row.timeline_status {
            ScopeStatus::Failed(failure) => failure.translated_message(&state_translate),
            ScopeStatus::Loading | ScopeStatus::Ready => {
                t_smol!(
                    state_translate,
                    "widget.hyperliquid_wallet.timeline_loading"
                )
            }
        }
    })
    .bind_style(move |read| StylePatch {
        foreground: Some(message_theme.role(read, Role::FgMuted)),
        ..StylePatch::default()
    })
    .build(tree, state)?;

    built_slots.timeline_retry = Some(
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .absolute()
                    .right(px(8.0))
                    .top(px(8.0))
                    .width(ROW_CONTROL_SIZE)
                    .height(ROW_CONTROL_SIZE)
                    .build(),
            )
            .build(tree, timeline_area)?,
    );

    slots.borrow_mut().insert(index, built_slots);
    Ok(())
}

fn meta_text_style() -> TextStyle {
    TextStyle {
        size: META_TEXT_SIZE.text_size(),
        overflow: TextOverflow::Ellipsis,
        ..TextStyle::new(META_TEXT_SIZE.text_size())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use app_config::widget::VersionedWidgetConfig;
    use i18n::{language::codes::LanguageCode, translate::Translate};
    use market_participants_connector::{
        account::error::AccountCatalogError, catalog::error::CatalogError,
    };
    use serde_json::json;
    use ui_core::{event::ActionSource, math::LogicalRect, node::signal::SignalScope};
    use widget::test_support::WidgetTestApp;

    use super::*;
    use crate::{
        model::DetailTab,
        service::WalletStreamError,
        test_support::{
            TestSource, address, config, detail_key, empty_timeline, message, money, overview_key,
            overview_snapshot, perpetual_route, spot_route, status, timeline_key,
        },
    };

    fn translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(translate(LanguageCode::En))
    }

    #[derive(Default)]
    struct FixtureDirectory {
        routes: RefCell<Vec<VenueRoute>>,
        loaded: RefCell<Vec<VenueRoute>>,
    }

    impl FixtureDirectory {
        fn published() -> Rc<Self> {
            let directory = Rc::new(Self::default());
            *directory.routes.borrow_mut() = vec![perpetual_route(), spot_route()];
            directory
        }

        fn loaded() -> Rc<Self> {
            let directory = Self::published();
            let routes = directory.routes.borrow().clone();
            *directory.loaded.borrow_mut() = routes;
            directory
        }

        fn load_all(&self) {
            let routes = self.routes.borrow().clone();
            *self.loaded.borrow_mut() = routes;
        }

        fn unload_all(&self) {
            self.loaded.borrow_mut().clear();
        }
    }

    impl WalletDirectory for FixtureDirectory {
        fn wallet_routes(&self) -> Vec<VenueRoute> {
            self.routes.borrow().clone()
        }

        fn is_loaded(&self, route: &VenueRoute) -> bool {
            self.loaded.borrow().iter().any(|held| held == route)
        }
    }

    struct Pane {
        widget: HyperliquidWalletWidget,
        status: WidgetStatus,
        parent: NodeId,
        published: Rc<Cell<usize>>,
        scope: SignalScope,
    }

    fn pane_over(
        harness: &mut WidgetTestApp,
        config: HyperliquidWalletConfig,
        source: Option<TestSource>,
        directory: Rc<FixtureDirectory>,
    ) -> Pane {
        let scope = harness.app_mut().signal_scope();
        let status = WidgetStatus::new(harness.app_mut(), scope);
        let parent = sized_parent(harness, px(800.0), px(600.0));
        let published = Rc::new(Cell::new(0));
        let counted = Rc::clone(&published);
        let config_change = WidgetConfigChange::new(move |_| {
            counted.set(counted.get() + 1);
        });
        let directory_handle: Rc<dyn WalletDirectory> = directory;
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        let binding =
            source.map(|source| WidgetBinding::spawn(&mut cx, WalletService::new(source)));
        let widget = HyperliquidWalletWidget::assemble(
            binding,
            Some(directory_handle),
            config,
            status,
            WidgetAction::new("common.retry", |_| {}),
            config_change,
            &mut cx,
        );
        Pane {
            widget,
            status,
            parent,
            published,
            scope,
        }
    }

    fn pane_cx<R>(
        harness: &mut WidgetTestApp,
        scope: SignalScope,
        body: impl FnOnce(&mut WidgetCx<'_>) -> R,
    ) -> R {
        let services = Rc::clone(harness.services_rc());
        let window = harness.window();
        let mut cx = WidgetCx::new(harness.app_mut(), window, &services, scope);
        body(&mut cx)
    }

    fn quiet_pane(harness: &mut WidgetTestApp, addresses: Vec<WalletAddress>) -> Pane {
        let directory = FixtureDirectory::loaded();
        pane_over(harness, config(addresses), None, directory)
    }

    fn waiting_pane(harness: &mut WidgetTestApp, addresses: Vec<WalletAddress>) -> Pane {
        let directory = FixtureDirectory::published();
        pane_over(harness, config(addresses), None, directory)
    }

    fn current_state(harness: &WidgetTestApp, pane: &Pane) -> WidgetState {
        pane.status.state(harness.app())
    }

    fn load(harness: &mut WidgetTestApp, pane: &mut Pane) {
        pane_cx(harness, pane.scope, |cx| pane.widget.load(cx));
    }

    fn sync(harness: &mut WidgetTestApp, pane: &mut Pane) {
        pane_cx(harness, pane.scope, |cx| pane.widget.sync(cx)).expect("the widget syncs");
    }

    fn build_body(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = pane.parent;
        pane_cx(harness, pane.scope, |cx| pane.widget.body(parent, cx)).expect("the body builds");
    }

    fn build_header(harness: &mut WidgetTestApp, pane: &mut Pane) {
        let parent = harness.placeholder_parent();
        pane_cx(harness, pane.scope, |cx| {
            pane.widget.header(HeaderGroup::Left, parent, cx)
        })
        .expect("the header builds");
    }

    fn drain(harness: &mut WidgetTestApp) {
        harness.drain();
    }

    fn report_overview(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        address: &WalletAddress,
        snapshot: Arc<WalletOverviewSnapshot>,
    ) {
        let key = overview_key(address);
        let message = message(WalletStreamMessage::Overview(snapshot));
        pane_cx(harness, pane.scope, |cx| {
            pane.widget.apply_latest(key, message, cx)
        });
    }

    fn report_timeline(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        address: &WalletAddress,
        snapshot: Arc<TimelineSnapshot>,
    ) {
        let key = timeline_key(address);
        let message = message(WalletStreamMessage::Timeline(snapshot));
        pane_cx(harness, pane.scope, |cx| {
            pane.widget.apply_latest(key, message, cx)
        });
    }

    fn report_detail(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        address: &WalletAddress,
        tab: DetailTab,
        snapshot: Arc<DetailSnapshot>,
    ) {
        let key = detail_key(address, tab);
        let message = message(WalletStreamMessage::Detail(snapshot));
        pane_cx(harness, pane.scope, |cx| {
            pane.widget.apply_latest(key, message, cx)
        });
    }

    fn report_status(
        harness: &mut WidgetTestApp,
        pane: &mut Pane,
        key: WalletStreamKey,
        reported: WalletStatus,
    ) {
        let message = status(reported);
        pane_cx(harness, pane.scope, |cx| {
            pane.widget.apply_reliable(key, message, cx)
        });
    }

    fn overview_of() -> Arc<WalletOverviewSnapshot> {
        overview_snapshot(
            crate::test_support::spot_state(Vec::new()),
            crate::test_support::perpetual_state(Vec::new()),
            Vec::new(),
            money(),
        )
    }

    fn a_wallet() -> WalletAddress {
        address("1")
    }

    fn assert_every_message_resolves() {
        let translate = translate(LanguageCode::En);
        for key in [
            "widget.names.hyperliquid_wallet",
            "widget.hyperliquid_wallet.add",
            "widget.hyperliquid_wallet.age_days",
            "widget.hyperliquid_wallet.age_hours",
            "widget.hyperliquid_wallet.age_minutes",
            "widget.hyperliquid_wallet.age_seconds",
            "widget.hyperliquid_wallet.already_monitored",
            "widget.hyperliquid_wallet.asset",
            "widget.hyperliquid_wallet.asset_share",
            "widget.hyperliquid_wallet.back",
            "widget.hyperliquid_wallet.buy",
            "widget.hyperliquid_wallet.close_long",
            "widget.hyperliquid_wallet.close_short",
            "widget.hyperliquid_wallet.coin",
            "widget.hyperliquid_wallet.collateral",
            "widget.hyperliquid_wallet.detail_tabs",
            "widget.hyperliquid_wallet.empty_addresses",
            "widget.hyperliquid_wallet.empty_monitored",
            "widget.hyperliquid_wallet.entry",
            "widget.hyperliquid_wallet.error_codec",
            "widget.hyperliquid_wallet.error_dex_read",
            "widget.hyperliquid_wallet.error_listing_missing",
            "widget.hyperliquid_wallet.error_route_mismatch",
            "widget.hyperliquid_wallet.error_stream_closed",
            "widget.hyperliquid_wallet.error_subscription_buffer_full",
            "widget.hyperliquid_wallet.error_subscription_closed",
            "widget.hyperliquid_wallet.fee",
            "widget.hyperliquid_wallet.funding",
            "widget.hyperliquid_wallet.invalid_address",
            "widget.hyperliquid_wallet.kind",
            "widget.hyperliquid_wallet.limit_reached",
            "widget.hyperliquid_wallet.liquidation",
            "widget.hyperliquid_wallet.long",
            "widget.hyperliquid_wallet.manage_addresses",
            "widget.hyperliquid_wallet.margin",
            "widget.hyperliquid_wallet.mark",
            "widget.hyperliquid_wallet.monitored_count",
            "widget.hyperliquid_wallet.open_long",
            "widget.hyperliquid_wallet.open_short",
            "widget.hyperliquid_wallet.perpetual_value",
            "widget.hyperliquid_wallet.placeholder",
            "widget.hyperliquid_wallet.position",
            "widget.hyperliquid_wallet.position_value",
            "widget.hyperliquid_wallet.quantity",
            "widget.hyperliquid_wallet.realized_pnl",
            "widget.hyperliquid_wallet.reduce_only",
            "widget.hyperliquid_wallet.refresh",
            "widget.hyperliquid_wallet.remove_address",
            "widget.hyperliquid_wallet.retry_failed",
            "widget.hyperliquid_wallet.scroll_addresses",
            "widget.hyperliquid_wallet.scroll_rows",
            "widget.hyperliquid_wallet.sell",
            "widget.hyperliquid_wallet.short",
            "widget.hyperliquid_wallet.side",
            "widget.hyperliquid_wallet.spot",
            "widget.hyperliquid_wallet.spot_value",
            "widget.hyperliquid_wallet.status",
            "widget.hyperliquid_wallet.tab_historical_orders",
            "widget.hyperliquid_wallet.tab_ledger",
            "widget.hyperliquid_wallet.tab_open_orders",
            "widget.hyperliquid_wallet.tab_perpetual",
            "widget.hyperliquid_wallet.tab_spot",
            "widget.hyperliquid_wallet.tab_trades",
            "widget.hyperliquid_wallet.timeline_end",
            "widget.hyperliquid_wallet.timeline_loading",
            "widget.hyperliquid_wallet.timeline_marker",
            "widget.hyperliquid_wallet.timeline_start",
            "widget.hyperliquid_wallet.timeline_title",
            "widget.hyperliquid_wallet.trigger_price",
            "widget.hyperliquid_wallet.unknown",
            "widget.hyperliquid_wallet.unrealized_pnl",
            "widget.hyperliquid_wallet.value",
            "common.price",
            "common.time",
            "common.yes",
            "common.no",
        ] {
            assert!(!t_smol!(translate, key).is_empty(), "{key} has no copy");
        }
        for tab in DetailTab::ALL {
            assert!(
                !tab_label(tab, &translate).is_empty(),
                "{tab:?} has no label"
            );
        }
        for failure in [
            HyperliquidWalletFailure::Stream(WalletStreamError::SubscriptionClosed),
            HyperliquidWalletFailure::Stream(WalletStreamError::StreamClosed),
            HyperliquidWalletFailure::Stream(WalletStreamError::Data(
                crate::model::WalletDataError::Codec { operation: "test" },
            )),
            HyperliquidWalletFailure::Stream(WalletStreamError::Account(
                market_participants_connector::account::error::AccountError::Protocol {
                    operation: "test",
                },
            )),
            HyperliquidWalletFailure::Rpc(RpcCallError::ServiceClosed {
                request_id: RequestId::new(1),
            }),
        ] {
            assert!(!failure.translated_message(&translate).is_empty());
        }
    }

    #[test]
    fn every_message_this_pane_shows_should_resolve_to_text() {
        assert_every_message_resolves();
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(
            HyperliquidWalletWidget::ID,
            WidgetId::new_static("kytrix.hyperliquid_wallet")
        );
        assert_eq!(
            HyperliquidWalletWidget::DISPLAY_NAME,
            "widget.names.hyperliquid_wallet"
        );
        assert_eq!(
            HyperliquidWalletWidget::CATEGORY,
            WidgetCategory::Monitoring
        );
        assert!(HyperliquidWalletWidget::DESCRIPTION.is_some());
    }

    #[test]
    fn the_config_should_round_trip_through_its_envelope() {
        let configured = config(vec![a_wallet(), address("2")]);
        let stored = VersionedWidgetConfig::encode(&configured).expect("a config should encode");
        let decoded = stored.decode::<HyperliquidWalletConfig>();
        assert_eq!(
            decoded.addresses,
            vec![a_wallet(), address("2")],
            "the list is what the pane follows, in the order it was added"
        );
    }

    #[test]
    fn a_payload_this_build_cannot_use_should_decode_as_the_default() {
        let duplicated = json!({
            "addresses": [
                "0x1111111111111111111111111111111111111111",
                "0x1111111111111111111111111111111111111111"
            ]
        });
        let past_the_limit = json!({
            "addresses": (0..11)
                .map(|index| format!("0x{:040x}", index + 1))
                .collect::<Vec<_>>()
        });
        let the_old_watch_list = json!({
            "addresses": "0x1111111111111111111111111111111111111111"
        });
        let a_bare_address_list = json!(["0x1111111111111111111111111111111111111111"]);
        let addresses_that_are_not_a_list = json!({ "addresses": { "one": 1 } });
        for payload in [
            duplicated,
            past_the_limit,
            the_old_watch_list,
            a_bare_address_list,
            addresses_that_are_not_a_list,
        ] {
            assert!(
                serde_json::from_value::<HyperliquidWalletConfig>(payload.clone()).is_err(),
                "a payload this build cannot have written is refused: {payload}"
            );
            assert_eq!(
                serde_json::from_value::<HyperliquidWalletConfig>(payload.clone())
                    .unwrap_or_default(),
                HyperliquidWalletConfig::default(),
                "a refused payload decodes as the default"
            );
        }
    }

    #[test]
    fn an_unloaded_venue_should_leave_the_pane_waiting() {
        let mut harness = harness();
        let mut pane = waiting_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);

        let state = current_state(&harness, &pane);
        assert!(
            state.failure().is_none(),
            "not loaded is a wait, not a failure"
        );
        assert_eq!(
            state,
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: vec![VenueId::Hyperliquid],
                supported: market_participants_connector::venue::definition::venues_supporting(
                    CapabilityId::Wallet,
                ),
            })
        );
        assert!(!pane.widget.has_content());
    }

    #[test]
    fn an_unloaded_venue_should_be_reported_before_an_empty_address_list() {
        let mut harness = harness();
        let mut pane = waiting_pane(&mut harness, Vec::new());
        load(&mut harness, &mut pane);

        let state = current_state(&harness, &pane);
        assert!(
            matches!(state.notice(), Some(Notice::MarketNotLoaded { .. })),
            "the market is the pane's precondition, not its content"
        );
    }

    #[test]
    fn adding_a_wallet_while_the_market_is_not_loaded_should_keep_waiting() {
        let mut harness = harness();
        let mut pane = waiting_pane(&mut harness, Vec::new());
        load(&mut harness, &mut pane);

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .store_addresses(vec![a_wallet()], cx)
                .expect("the list stores")
        });

        let state = current_state(&harness, &pane);
        assert!(
            matches!(state.notice(), Some(Notice::MarketNotLoaded { .. })),
            "a list is not a market: the notice stays what the pane waits with"
        );
        assert!(pane.published.get() >= 1, "the new list is published");
    }

    #[test]
    fn a_pane_with_no_wallets_should_say_so_in_the_language_showing_now() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, Vec::new());
        load(&mut harness, &mut pane);

        let english = current_state(&harness, &pane);
        assert!(matches!(
            english.notice(),
            Some(Notice::Message("widget.hyperliquid_wallet.empty_addresses"))
        ));
        let english_sentence = english
            .notice()
            .expect("a notice")
            .resolve(&translate(LanguageCode::En));
        let chinese_sentence = english
            .notice()
            .expect("a notice")
            .resolve(&translate(LanguageCode::ZhCn));
        assert!(!english_sentence.is_empty());
        assert_ne!(english_sentence, chinese_sentence);
    }

    #[test]
    fn loading_the_venue_should_let_the_waiting_pane_read() {
        let mut harness = harness();
        let directory = FixtureDirectory::published();
        let mut pane = pane_over(
            &mut harness,
            config(vec![a_wallet()]),
            None,
            Rc::clone(&directory),
        );
        load(&mut harness, &mut pane);
        assert!(pane.widget.awaiting_market);

        directory.load_all();
        let generation = harness.services().catalog_generation;
        harness.app_mut().update(&generation, |value| *value += 1);
        sync(&mut harness, &mut pane);

        assert!(
            !pane.widget.awaiting_market,
            "the market loaded: the pane reads"
        );
        assert_eq!(pane.widget.markets.len(), 2, "both wallet routes are read");
        assert_eq!(
            current_state(&harness, &pane),
            WidgetState::Connecting,
            "nothing has answered yet"
        );
    }

    #[test]
    fn a_market_that_went_away_should_leave_the_pane_waiting_again() {
        let mut harness = harness();
        let directory = FixtureDirectory::loaded();
        let mut pane = pane_over(
            &mut harness,
            config(vec![a_wallet()]),
            None,
            Rc::clone(&directory),
        );
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        assert!(pane.widget.has_content());

        directory.unload_all();
        let generation = harness.services().catalog_generation;
        harness.app_mut().update(&generation, |value| *value += 1);
        sync(&mut harness, &mut pane);

        assert!(
            pane.widget.awaiting_market,
            "the market the pane read is gone"
        );
        assert!(
            !pane.widget.has_content(),
            "nothing on screen is backed by a market"
        );
        assert!(matches!(
            current_state(&harness, &pane).notice(),
            Some(Notice::MarketNotLoaded { .. })
        ));
    }

    #[test]
    fn a_venue_commit_should_not_revive_a_failed_pane() {
        let mut harness = harness();
        let directory = FixtureDirectory::loaded();
        let mut pane = pane_over(
            &mut harness,
            config(vec![a_wallet()]),
            None,
            Rc::clone(&directory),
        );
        load(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        assert!(current_state(&harness, &pane).is_failed());

        let generation = harness.services().catalog_generation;
        harness.app_mut().update(&generation, |value| *value += 1);
        sync(&mut harness, &mut pane);

        let state = current_state(&harness, &pane);
        assert!(
            state.is_failed(),
            "a generation does not revive a failed pane"
        );
    }

    #[test]
    fn a_late_reading_after_the_market_went_away_should_not_clear_the_notice() {
        let mut harness = harness();
        let directory = FixtureDirectory::published();
        let mut pane = pane_over(
            &mut harness,
            config(vec![a_wallet()]),
            None,
            Rc::clone(&directory),
        );
        load(&mut harness, &mut pane);

        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());

        let state = current_state(&harness, &pane);
        assert!(
            matches!(state.notice(), Some(Notice::MarketNotLoaded { .. })),
            "a reading that was in flight when the market went away is refused"
        );
        assert!(!pane.widget.has_content());
    }

    #[test]
    fn a_wallet_read_the_directory_has_not_loaded_should_leave_the_pane_waiting() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);

        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Failed(WalletStreamError::Account(
                market_participants_connector::account::error::AccountError::Catalog(
                    AccountCatalogError::Directory(Box::new(CatalogError::CatalogNotLoaded {
                        route: perpetual_route(),
                    })),
                ),
            )),
        );
        sync(&mut harness, &mut pane);

        let state = current_state(&harness, &pane);
        assert!(
            state.failure().is_none(),
            "a catalog that has not loaded is a wait, not a failure"
        );
        assert!(matches!(
            state.notice(),
            Some(Notice::MarketNotLoaded { .. })
        ));
        assert!(!pane.widget.has_content());
    }

    #[test]
    fn a_scope_with_no_wallets_should_say_what_it_is_waiting_for() {
        let mut harness = harness();
        let mut pane = waiting_pane(&mut harness, Vec::new());
        load(&mut harness, &mut pane);
        let state = current_state(&harness, &pane);
        assert!(
            matches!(state.notice(), Some(Notice::MarketNotLoaded { .. })),
            "the market comes before the list"
        );
    }

    #[test]
    fn a_wallet_reading_should_become_content_and_readiness_should_follow_it() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        assert!(!pane.widget.has_content());
        assert_eq!(current_state(&harness, &pane), WidgetState::Connecting);

        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        assert!(pane.widget.has_content());
        assert_eq!(
            current_state(&harness, &pane),
            WidgetState::Ready,
            "a reading is content, whether or not its stream has said it is live"
        );
        assert!(!pane.widget.overview[&a_wallet()].ready);

        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Ready,
        );
        assert_eq!(current_state(&harness, &pane), WidgetState::Ready);
    }

    #[test]
    fn a_wallet_that_failed_beside_a_working_one_should_not_fail_the_pane() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Ready,
        );
        report_status(
            &mut harness,
            &mut pane,
            overview_key(&address("2")),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );

        assert!(!current_state(&harness, &pane).is_failed());
        assert_eq!(current_state(&harness, &pane), WidgetState::Ready);
        assert_eq!(pane.widget.failed_addresses(), vec![address("2")]);
    }

    #[test]
    fn a_scope_whose_wallets_all_failed_should_report_the_failure() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );

        let state = current_state(&harness, &pane);
        assert!(state.is_failed());
        assert!(
            !state
                .failure()
                .expect("a failure")
                .text(&translate(LanguageCode::En))
                .is_empty()
        );
    }

    #[test]
    fn a_message_after_a_failure_should_be_refused() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        pane.status.report(
            harness.app_mut(),
            WidgetState::failed(
                Arc::new(HyperliquidWalletFailure::Stream(
                    WalletStreamError::StreamClosed,
                )),
                pane.widget.recovery.clone(),
            ),
        );

        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        assert!(current_state(&harness, &pane).is_failed());
        assert!(!pane.widget.has_content());
    }

    #[test]
    fn a_message_for_an_address_the_pane_no_longer_follows_should_be_refused() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .store_addresses(vec![address("2")], cx)
                .expect("the list stores")
        });

        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        assert!(!pane.widget.has_content());
        assert!(pane.widget.overview.get(&a_wallet()).is_none());
    }

    #[test]
    fn opening_a_wallet_should_show_its_detail_and_report_its_state() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });
        assert_eq!(
            pane.widget.shown(),
            Shown::Detail(a_wallet(), DetailTab::Spot)
        );
        assert!(pane.widget.has_content(), "an open wallet is content");
        assert_eq!(
            current_state(&harness, &pane),
            WidgetState::Loading,
            "the tab has not answered"
        );

        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Spot,
            crate::test_support::spot_snapshot(Vec::new(), money()),
        );
        assert_eq!(
            current_state(&harness, &pane),
            WidgetState::Ready,
            "a reading that arrived is not waited for"
        );
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Ready,
        );
        assert_eq!(current_state(&harness, &pane), WidgetState::Ready);
    }

    #[test]
    fn a_failed_tab_should_fail_the_pane_and_its_recovery_should_read_it_again() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );

        let state = current_state(&harness, &pane);
        assert!(state.is_failed());
        assert_eq!(
            state.failure().expect("the failure").action().label_key(),
            "common.retry"
        );

        pane.status.report(
            harness.app_mut(),
            WidgetState::waiting(Notice::message("common.loading")),
        );
        let retry = pane.widget.status.is_failed(harness.app());
        assert!(!retry);
    }

    #[test]
    fn another_tab_should_report_its_own_state() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });
        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Spot,
            crate::test_support::spot_snapshot(Vec::new(), money()),
        );
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Ready,
        );
        assert_eq!(current_state(&harness, &pane), WidgetState::Ready);

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Trades), cx)
                .expect("the tab opens")
        });
        assert_eq!(
            current_state(&harness, &pane),
            WidgetState::Loading,
            "another tab is another reading"
        );
    }

    #[test]
    fn a_tab_that_was_read_should_not_be_read_again() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });
        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Spot,
            crate::test_support::spot_snapshot(Vec::new(), money()),
        );
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Ready,
        );
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Spot), cx)
                .expect("the tab stays")
        });
        assert_eq!(current_state(&harness, &pane), WidgetState::Ready);
    }

    #[test]
    fn reconciling_a_scope_should_drop_the_wallets_that_left() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_overview(&mut harness, &mut pane, &address("2"), overview_of());

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .store_addresses(vec![a_wallet()], cx)
                .expect("the list stores")
        });

        assert_eq!(pane.widget.config.addresses, vec![a_wallet()]);
        assert!(pane.widget.overview.contains_key(&a_wallet()));
        assert!(!pane.widget.overview.contains_key(&address("2")));
        assert!(pane.published.get() >= 1);
    }

    #[test]
    fn a_wallet_that_left_the_scope_should_leave_the_detail_view() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });
        assert_eq!(
            pane.widget.shown(),
            Shown::Detail(a_wallet(), DetailTab::Spot)
        );

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .store_addresses(vec![address("2")], cx)
                .expect("the list stores")
        });
        assert_eq!(pane.widget.shown(), Shown::Overview);
    }

    #[test]
    fn the_answer_to_a_replaced_command_should_be_dropped() {
        let mut harness = harness();
        let directory = FixtureDirectory::loaded();
        let mut pane = pane_over(
            &mut harness,
            config(vec![a_wallet()]),
            None,
            Rc::clone(&directory),
        );
        load(&mut harness, &mut pane);
        let first = RequestId::new(1);
        let second = RequestId::new(2);
        pane.widget
            .pending
            .track(second, PendingCommand::Overview(vec![a_wallet()]));
        let _ = first;
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget.apply_rpc(
                RequestId::new(1),
                Err(RpcCallError::ServiceClosed {
                    request_id: RequestId::new(1),
                }),
                cx,
            )
        });
        assert_eq!(pane.widget.pending.dropped(), 1);
        assert!(!current_state(&harness, &pane).is_failed());
    }

    #[test]
    fn a_steady_pane_should_not_ask_for_a_frame() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Ready,
        );
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let requests = harness.frame_requests();
        sync(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady pane writes no signal and asks for no frame"
        );
    }

    #[test]
    fn the_header_should_follow_what_the_pane_is_showing() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        drain(&mut harness);

        let header = pane.widget.header.as_ref().expect("the header is built");
        assert!(harness.visible(header.manage_wrapper));
        assert!(!harness.visible(header.back_wrapper));
        assert!(!harness.visible(header.refresh_wrapper));

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        let header = pane.widget.header.as_ref().expect("the header is built");
        assert!(!harness.visible(header.manage_wrapper));
        assert!(harness.visible(header.back_wrapper));
        assert!(harness.visible(header.refresh_wrapper));
    }

    #[test]
    fn the_control_in_the_header_should_bring_a_reader_back_to_the_list() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_header(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });

        let header = pane.widget.header.as_ref().expect("the header is built");
        let action = header
            .back
            .action_identity()
            .expect("the back control acts");
        let node = header.back.node().expect("the back control is built");
        let window = harness.window();
        harness.app_mut().dispatch_action(
            window,
            node,
            action,
            ActionSource::Click {
                click_count: 1,
                button: ui_core::event::PointerButton::Primary,
            },
        );
        sync(&mut harness, &mut pane);
        assert_eq!(pane.widget.shown(), Shown::Overview);
    }

    #[test]
    fn the_address_control_should_refuse_input_while_the_market_is_not_loaded() {
        let mut harness = harness();
        let mut pane = waiting_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);

        let header = pane.widget.header.as_ref().expect("the header is built");
        let action = header
            .manage
            .action_identity()
            .expect("the manage control acts");
        let node = header.manage.node().expect("the manage control is built");
        let window = harness.window();
        harness.app_mut().dispatch_action(
            window,
            node,
            action,
            ActionSource::Click {
                click_count: 1,
                button: ui_core::event::PointerButton::Primary,
            },
        );
        sync(&mut harness, &mut pane);
        assert!(
            pane.widget.dialog.is_none(),
            "the dialog reads a market the pane does not have"
        );
    }

    #[test]
    fn a_two_wallet_pane_should_list_the_wallets_it_follows() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_overview(&mut harness, &mut pane, &address("2"), overview_of());
        build_body(&mut harness, &mut pane);
        drain(&mut harness);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let rows = harness.app().read(&pane.widget.rows).rows.clone();
        assert_eq!(rows.len(), 2, "one row per followed address");
        assert_eq!(rows[0].address, a_wallet());
        assert_eq!(rows[1].address, address("2"));
    }

    #[test]
    fn a_timeline_that_arrived_should_be_painted_for_its_row() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        drain(&mut harness);
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        report_timeline(&mut harness, &mut pane, &a_wallet(), empty_timeline());
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let canvas = pane
            .widget
            .row_canvases
            .borrow()
            .get(&0)
            .copied()
            .expect("the row builds a timeline canvas");
        assert!(harness.has_canvas(canvas));
        assert!(
            !harness.canvas_primitives(canvas).is_empty(),
            "a reading paints the timeline"
        );
    }

    #[test]
    fn the_timeline_of_a_wallet_should_be_retried_from_its_row() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_status(
            &mut harness,
            &mut pane,
            timeline_key(&a_wallet()),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        assert!(
            pane.widget.timelines[&a_wallet()].status
                == ScopeStatus::Failed(Arc::new(HyperliquidWalletFailure::Stream(
                    WalletStreamError::StreamClosed
                )))
        );

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::RetryTimeline(a_wallet()), cx)
                .expect("the timeline retries")
        });
        assert!(matches!(
            pane.widget.timelines[&a_wallet()].status,
            ScopeStatus::Loading
        ));
    }

    #[test]
    fn releasing_the_pane_should_reclaim_every_value_slot() {
        let mut harness = harness();
        let baseline = harness.signal_slot_stats();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        drain(&mut harness);

        pane_cx(&mut harness, pane.scope, |cx| pane.widget.release(cx));
        harness.app_mut().release_scope(pane.scope);
        assert!(pane.widget.body_root.is_none());
        assert!(pane.widget.list.is_none());
        assert!(pane.widget.header.is_none());
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "releasing the pane reclaims every value slot it minted"
        );
    }

    #[test]
    fn a_pane_should_persist_the_addresses_it_follows() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        assert_eq!(
            pane.widget
                .config()
                .expect("the pane persists its list")
                .addresses,
            vec![a_wallet()]
        );
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .store_addresses(vec![address("2")], cx)
                .expect("the list stores")
        });
        assert_eq!(pane.published.get(), 1, "a change is published");
    }

    #[test]
    fn two_panes_should_keep_their_own_header_handlers() {
        let mut harness = harness();
        let mut first = quiet_pane(&mut harness, vec![a_wallet()]);
        let mut second = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut first);
        load(&mut harness, &mut second);
        build_header(&mut harness, &mut first);
        build_header(&mut harness, &mut second);

        let first_action = first
            .widget
            .header
            .as_ref()
            .expect("the first header")
            .manage
            .action_identity()
            .expect("an action");
        let second_action = second
            .widget
            .header
            .as_ref()
            .expect("the second header")
            .manage
            .action_identity()
            .expect("an action");
        assert_ne!(first_action, second_action, "each pane has its own key");
        assert!(harness.app().has_action_handler(&first_action));
        assert!(harness.app().has_action_handler(&second_action));

        pane_cx(&mut harness, first.scope, |cx| first.widget.release(cx));
        assert!(harness.app().has_action_handler(&second_action));
    }

    fn dispatch(harness: &mut WidgetTestApp, action: Action, node: NodeId) {
        let window = harness.window();
        harness.app_mut().dispatch_action(
            window,
            node,
            action,
            ActionSource::Click {
                click_count: 1,
                button: ui_core::event::PointerButton::Primary,
            },
        );
    }

    fn open_wallet(harness: &mut WidgetTestApp, pane: &mut Pane) {
        pane_cx(harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens")
        });
    }

    #[test]
    fn an_open_wallet_should_arrive_as_a_page_over_the_list() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        drain(&mut harness);
        let overview = harness
            .find_by_debug_prefix(pane.parent, "TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE")
            .expect("the list page is built");
        let detail = harness
            .find_by_debug_prefix(pane.parent, "TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
            .expect("the detail page is built");
        assert!(harness.visible(overview));
        assert!(!harness.visible(detail));

        open_wallet(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert!(
            !harness.visible(overview),
            "the list is not drawn over the wallet"
        );
        assert!(harness.visible(detail), "the wallet is the page showing");
    }

    #[test]
    fn asking_twice_for_one_wallet_should_leave_one_page() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        open_wallet(&mut harness, &mut pane);
        open_wallet(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("opening the open wallet is not a new page")
        });
        assert_eq!(
            pane.widget.shown(),
            Shown::Detail(a_wallet(), DetailTab::Spot)
        );
        assert_eq!(pane.widget.detail.len(), 1, "one tab, one reading");
    }

    #[test]
    fn reopening_a_wallet_should_show_the_reading_the_pane_already_has() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        open_wallet(&mut harness, &mut pane);
        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Spot,
            crate::test_support::spot_snapshot(Vec::new(), money()),
        );
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Ready,
        );
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::Back, cx)
                .expect("the list comes back")
        });
        assert_eq!(pane.widget.shown(), Shown::Overview);

        open_wallet(&mut harness, &mut pane);
        assert_eq!(
            current_state(&harness, &pane),
            WidgetState::Ready,
            "the reading the pane already has is what a reopened wallet shows"
        );
        assert!(
            pane.widget.detail[&(a_wallet(), DetailTab::Spot)]
                .snapshot
                .is_some()
        );
    }

    #[test]
    fn clicking_a_tab_should_show_that_tab() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        open_wallet(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Trades), cx)
                .expect("the tab opens")
        });
        assert_eq!(pane.widget.selected_tab, DetailTab::Trades);
        assert_eq!(*harness.app().read(&pane.widget.tab), DetailTab::Trades);
        assert!(
            pane.widget
                .detail
                .contains_key(&(a_wallet(), DetailTab::Trades))
        );
    }

    #[test]
    fn refreshing_a_tab_should_keep_its_reading_while_it_is_read_again() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        open_wallet(&mut harness, &mut pane);
        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Spot,
            crate::test_support::spot_snapshot(Vec::new(), money()),
        );
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Ready,
        );

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::RefreshTab, cx)
                .expect("the tab is read again")
        });
        let entry = &pane.widget.detail[&(a_wallet(), DetailTab::Spot)];
        assert!(
            entry.snapshot.is_some(),
            "the table a reader is looking at stays while it is refreshed"
        );
        assert!(matches!(entry.status, ScopeStatus::Loading));
        assert_eq!(current_state(&harness, &pane), WidgetState::Ready);
    }

    #[test]
    fn the_pane_should_draw_its_wallets_and_their_detail() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_timeline(&mut harness, &mut pane, &a_wallet(), empty_timeline());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let row = harness
            .find_by_debug_prefix(pane.parent, "TEST_HYPERLIQUID_WALLET_ROW_0")
            .expect("a followed wallet is a row");
        assert!(harness.visible(row));
        let canvas = pane
            .widget
            .row_canvases
            .borrow()
            .get(&0)
            .copied()
            .expect("the row builds its timeline");
        assert!(harness.has_canvas(canvas));

        open_wallet(&mut harness, &mut pane);
        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Spot,
            crate::test_support::spot_snapshot(Vec::new(), money()),
        );
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert!(
            harness
                .find_by_debug_prefix(pane.parent, "TEST_HYPERLIQUID_WALLET_DETAIL_PAGE")
                .is_some_and(|detail| harness.visible(detail)),
            "the open wallet's detail is the page drawn"
        );
    }

    #[test]
    fn a_wallet_the_venue_answered_nothing_for_should_still_be_drawn() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Ready,
        );
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let rows = harness.app().read(&pane.widget.rows).rows.clone();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].overview.is_none(), "the venue sent no reading");
        assert!(
            harness
                .find_by_debug_prefix(pane.parent, "TEST_HYPERLIQUID_WALLET_ROW_0")
                .is_some(),
            "a wallet with no reading is still a wallet the pane follows"
        );
    }

    #[test]
    fn a_fill_that_arrived_while_the_scope_was_loading_should_still_be_drawn() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_timeline(&mut harness, &mut pane, &a_wallet(), empty_timeline());
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let canvas = pane
            .widget
            .row_canvases
            .borrow()
            .get(&0)
            .copied()
            .expect("a row");
        assert!(
            !harness.canvas_primitives(canvas).is_empty(),
            "a timeline that arrived early is drawn once the row has a precision"
        );
    }

    #[test]
    fn the_address_control_should_open_the_dialog() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);

        let header = pane.widget.header.as_ref().expect("the header is built");
        let action = header
            .manage
            .action_identity()
            .expect("the manage control acts");
        let node = header.manage.node().expect("the manage control is built");
        dispatch(&mut harness, action, node);
        sync(&mut harness, &mut pane);
        assert!(pane.widget.dialog.is_some(), "the control opens the dialog");

        let header = pane.widget.header.as_ref().expect("the header is built");
        let action = header
            .manage
            .action_identity()
            .expect("the manage control acts");
        let node = header.manage.node().expect("the manage control is built");
        dispatch(&mut harness, action, node);
        sync(&mut harness, &mut pane);
        assert!(pane.widget.dialog.is_some());
    }

    #[test]
    fn a_wallet_the_dialog_removed_should_leave_the_list() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_overview(&mut harness, &mut pane, &address("2"), overview_of());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert_eq!(
            pane.widget
                .list
                .as_ref()
                .expect("the list")
                .count(harness.app()),
            2
        );

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .store_addresses(vec![a_wallet()], cx)
                .expect("the list stores")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert_eq!(
            pane.widget
                .list
                .as_ref()
                .expect("the list")
                .count(harness.app()),
            1,
            "the list is the wallets the pane follows"
        );
        assert_eq!(harness.app().read(&pane.widget.rows).rows.len(), 1);
    }

    #[test]
    fn a_pane_with_no_market_should_not_open_its_dialog_for_wallets() {
        let mut harness = harness();
        let mut pane = waiting_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::ManageAddresses, cx)
                .expect("the intent is applied")
        });
        assert!(
            pane.widget.dialog.is_none(),
            "the dialog reads a market the pane does not have"
        );
    }

    fn row_text(harness: &WidgetTestApp, pane: &Pane, suffix: &str) -> NodeId {
        harness
            .find_by_debug_prefix(
                pane.parent,
                &format!("TEST_HYPERLIQUID_WALLET_ROW_0_{suffix}"),
            )
            .expect("the row text exists")
    }

    #[test]
    fn the_rows_should_follow_the_language() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_timeline(&mut harness, &mut pane, &a_wallet(), empty_timeline());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let spot = row_text(&harness, &pane, "SPOT");
        let title = row_text(&harness, &pane, "TIMELINE_TITLE");
        let english = harness.text_of(spot);
        let english_title = harness.text_of(title);
        assert!(english.starts_with("Spot value:"), "{english}");
        assert!(!english_title.is_empty());

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let chinese = harness.text_of(spot);
        let chinese_title = harness.text_of(title);
        assert_ne!(english, chinese, "a row's labels follow the language");
        assert_ne!(english_title, chinese_title);
        let zh_translate = translate(LanguageCode::ZhCn);
        let label = t_smol!(zh_translate, "widget.hyperliquid_wallet.spot_value");
        assert!(
            chinese.starts_with(label.as_str()),
            "the row reads the sentence in the language showing now: {chinese}"
        );
    }

    #[test]
    fn the_header_control_should_follow_the_language() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        build_header(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        let english = pane
            .widget
            .header
            .as_ref()
            .expect("the header")
            .manage
            .a11y_label_text()
            .expect("the control has a label")
            .to_owned();
        assert_eq!(english, "Manage monitored addresses");

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let chinese = pane
            .widget
            .header
            .as_ref()
            .expect("the header is rebuilt")
            .manage
            .a11y_label_text()
            .expect("the control has a label")
            .to_owned();
        assert_ne!(
            english, chinese,
            "the control is rebuilt in the language now showing"
        );
        assert!(
            harness.app().has_action_handler(
                &pane
                    .widget
                    .header
                    .as_ref()
                    .expect("the header")
                    .manage
                    .action_identity()
                    .expect("the rebuilt control still acts")
            ),
            "the rebuilt control is registered"
        );
    }

    #[test]
    fn closing_the_dialog_should_leave_the_pane_without_one() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::ManageAddresses, cx)
                .expect("the dialog opens")
        });
        sync(&mut harness, &mut pane);
        let dialog = pane.widget.dialog.as_ref().expect("the dialog is open");
        let kit = dialog.test_dialog().expect("the kit dialog");
        assert!(kit.is_open(harness.app()));
        kit.close_now(harness.app_mut());
        sync(&mut harness, &mut pane);
        assert!(
            pane.widget.dialog.is_none(),
            "the pane drops a closed dialog"
        );
    }

    #[test]
    fn releasing_the_pane_with_its_dialog_should_reclaim_its_slots() {
        let mut harness = harness();
        let baseline = harness.signal_slot_stats();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::ManageAddresses, cx)
                .expect("the dialog opens")
        });
        build_header(&mut harness, &mut pane);
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        pane_cx(&mut harness, pane.scope, |cx| pane.widget.release(cx));
        harness.app_mut().release_scope(pane.scope);
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "the dialog and the body release everything they minted"
        );
    }

    fn trades_reading() -> Arc<DetailSnapshot> {
        use market_participants_model::market::units::Side;

        let listing = crate::test_support::published_perpetual("ETH");
        let fill = crate::test_support::fill(
            &a_wallet(),
            &listing,
            Side::Buy,
            "1",
            "100",
            1_700_000_000_000,
            7,
        );
        Arc::new(
            DetailSnapshot::new(
                crate::model::DetailData::Trades(vec![fill]),
                crate::test_support::precision_for(crate::test_support::test_routes()),
            )
            .expect("a published listing should validate"),
        )
    }

    fn open_trades(harness: &mut WidgetTestApp, pane: &mut Pane) {
        load(harness, pane);
        report_overview(harness, pane, &a_wallet(), overview_of());
        open_wallet(harness, pane);
        pane_cx(harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Trades), cx)
                .expect("the tab opens")
        });
        report_detail(
            harness,
            pane,
            &a_wallet(),
            DetailTab::Trades,
            trades_reading(),
        );
        report_status(
            harness,
            pane,
            detail_key(&a_wallet(), DetailTab::Trades),
            WalletStatus::Ready,
        );
        build_body(harness, pane);
        sync(harness, pane);
        drain(harness);
    }

    #[test]
    fn an_empty_detail_reading_should_be_the_tables_empty_state() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        open_wallet(&mut harness, &mut pane);
        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Spot,
            crate::test_support::spot_snapshot(Vec::new(), money()),
        );
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let tables = pane.widget.tables.as_ref().expect("the tables are built");
        assert_eq!(
            tables.row_count(DetailTab::Spot),
            0,
            "a reading with no rows is content with an empty body, not a placeholder"
        );
        assert!(
            tables
                .cell_node(DetailTab::Spot, 0, 0, harness.app())
                .is_none(),
            "there is no row to draw"
        );
        assert_eq!(current_state(&harness, &pane), WidgetState::Ready);
    }

    #[test]
    fn moving_the_display_zone_should_schedule_a_redraw_of_the_detail() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        open_trades(&mut harness, &mut pane);
        let cell = {
            let tables = pane.widget.tables.as_ref().expect("the tables are built");
            assert_eq!(tables.row_count(DetailTab::Trades), 1);
            tables
                .cell_node(DetailTab::Trades, 0, 0, harness.app())
                .expect("the row's first cell is built")
        };

        let signal = harness.services().config;
        let mut config = (*harness.app().read(&signal)).clone();
        config.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");
        let requests = harness.frame_requests();
        assert!(
            harness.app_mut().write(&signal, config),
            "moving the zone is a settings write"
        );
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert!(
            harness.frame_requests() > requests,
            "the settings write schedules the frame that redraws the rows"
        );
        assert!(
            harness.tree().get(cell).is_ok(),
            "the cell is still the cell the reading built"
        );
        assert_eq!(
            pane.widget
                .tables
                .as_ref()
                .expect("the tables are built")
                .row_count(DetailTab::Trades),
            1,
            "the reading is not handed to the table again"
        );
    }

    #[test]
    fn a_shrunk_list_should_release_the_row_controls_it_no_longer_shows() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_overview(&mut harness, &mut pane, &address("2"), overview_of());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert_eq!(
            pane.widget.row_controls.len(),
            2,
            "both rows have their controls mounted"
        );
        let live = harness.signal_slot_stats().live;

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .store_addresses(vec![a_wallet()], cx)
                .expect("the list stores")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        assert_eq!(
            pane.widget.row_controls.len(),
            1,
            "the row that left the list released its controls"
        );
        assert!(
            harness.signal_slot_stats().live < live,
            "the controls' signal scopes are handed back"
        );
        assert!(
            pane.widget
                .row_controls
                .values()
                .all(|controls| controls.address == a_wallet()),
            "the control that stayed belongs to the wallet that stayed"
        );
    }

    #[test]
    fn clicking_a_wallet_should_open_its_page() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let row = harness
            .find_by_debug_prefix(pane.parent, "TEST_HYPERLIQUID_WALLET_ROW_0")
            .expect("a followed wallet is a row");
        let action = pane.widget.row_actions[0].clone();
        dispatch(&mut harness, action, row);
        sync(&mut harness, &mut pane);
        assert_eq!(
            pane.widget.shown(),
            Shown::Detail(a_wallet(), DetailTab::Spot)
        );
    }

    #[test]
    fn selecting_a_failed_tab_again_should_read_it_again() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        open_wallet(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        assert!(current_state(&harness, &pane).is_failed());

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Spot), cx)
                .expect("the failed tab is read again")
        });
        let entry = &pane.widget.detail[&(a_wallet(), DetailTab::Spot)];
        assert!(
            matches!(entry.status, ScopeStatus::Loading),
            "a failed tab is read again by its own control"
        );
        assert!(!current_state(&harness, &pane).is_failed());
    }

    #[test]
    fn a_refresh_of_the_market_the_pane_reads_should_start_over() {
        let mut harness = harness();
        let directory = FixtureDirectory::loaded();
        let mut pane = pane_over(
            &mut harness,
            config(vec![a_wallet()]),
            None,
            Rc::clone(&directory),
        );
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        assert_eq!(pane.widget.markets.len(), 2);

        let generation = harness.services().catalog_generation;
        harness.app_mut().update(&generation, |value| *value += 1);
        sync(&mut harness, &mut pane);

        assert_eq!(
            *harness.app().read(&generation),
            pane.widget.seen_generation,
            "the generation the pane acted on is the one it saw"
        );
        assert!(!pane.widget.awaiting_market);
        assert_eq!(
            pane.widget.markets.len(),
            2,
            "the market the pane reads was published again: the load runs again"
        );
        assert_eq!(current_state(&harness, &pane), WidgetState::Connecting);
    }

    #[test]
    fn the_tab_group_should_stay_while_a_tab_that_was_never_read_is_read() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        open_wallet(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Ledger), cx)
                .expect("the tab opens")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        assert_eq!(
            pane.widget.tab_buttons.len(),
            DetailTab::ALL.len(),
            "the tab group is built once and stays while a tab is read"
        );
        assert!(pane.widget.tab_strip.is_some());
        assert!(
            pane.widget
                .detail
                .contains_key(&(a_wallet(), DetailTab::Ledger))
        );
    }

    fn sized_parent(harness: &mut WidgetTestApp, width: LogicalPx, height: LogicalPx) -> NodeId {
        harness.with_cx(|cx| {
            let root = cx.tree().root();
            NodeBuilder::boxed()
                .id("TEST_HYPERLIQUID_WALLET_BODY_PARENT")
                .style(
                    StyleBuilder::new()
                        .width(width)
                        .height(height)
                        .flex_none()
                        .flex_col()
                        .build(),
                )
                .build(cx.tree_mut(), root)
                .expect("the parent is built")
        })
    }

    fn bounds_of(harness: &WidgetTestApp, node: NodeId) -> LogicalRect {
        harness
            .tree()
            .get(node)
            .unwrap_or_else(|_| panic!("the node {node:?} is in the tree"))
            .layout()
            .bounds
    }

    fn find(harness: &WidgetTestApp, parent: NodeId, prefix: &str) -> NodeId {
        harness
            .find_by_debug_prefix(parent, prefix)
            .unwrap_or_else(|| panic!("{prefix} is built under {parent:?}"))
    }

    fn assert_close(actual: LogicalPx, expected: LogicalPx, what: &str) {
        assert!(
            (actual - expected).abs() < px(0.01),
            "{what}: {actual} != {expected}"
        );
    }

    #[test]
    fn the_two_pages_should_fill_the_body_they_share() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        let parent = sized_parent(&mut harness, px(600.0), px(400.0));
        pane.parent = parent;
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let body = pane.widget.body_root.expect("the body is built");
        let body_bounds = bounds_of(&harness, body);
        assert_close(
            body_bounds.size.width,
            px(600.0),
            "the body is as wide as its slot",
        );
        assert_close(
            body_bounds.size.height,
            px(400.0),
            "the body is as tall as its slot",
        );

        let overview = find(&harness, parent, "TEST_HYPERLIQUID_WALLET_OVERVIEW_PAGE");
        let detail = find(&harness, parent, "TEST_HYPERLIQUID_WALLET_DETAIL_PAGE");
        for (page, name) in [(overview, "the list page"), (detail, "the detail page")] {
            let bounds = bounds_of(&harness, page);
            assert_eq!(
                bounds.origin, body_bounds.origin,
                "{name} starts where the body starts rather than below its sibling"
            );
            assert_eq!(
                bounds.size, body_bounds.size,
                "{name} is the whole body, not half of it"
            );
        }
        assert!(
            harness.visible(overview) && !harness.visible(detail),
            "the list is the page drawn, and the detail page is still laid out"
        );

        open_wallet(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        let shown = bounds_of(&harness, detail);
        assert_eq!(shown.origin, body_bounds.origin);
        assert_eq!(shown.size, body_bounds.size);
        assert!(
            harness.visible(detail) && !harness.visible(overview),
            "the wallet is the page drawn now"
        );
    }

    #[test]
    fn the_detail_page_should_be_its_heading_its_tabs_and_its_tables() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        let parent = sized_parent(&mut harness, px(600.0), px(400.0));
        pane.parent = parent;
        build_body(&mut harness, &mut pane);
        open_wallet(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let page = bounds_of(
            &harness,
            find(&harness, parent, "TEST_HYPERLIQUID_WALLET_DETAIL_PAGE"),
        );
        let heading = bounds_of(
            &harness,
            find(&harness, parent, "TEST_HYPERLIQUID_WALLET_DETAIL_HEADING"),
        );
        let strip = bounds_of(
            &harness,
            pane.widget.tab_strip.expect("the tab strip is built"),
        );
        let tables = pane.widget.tables.as_ref().expect("the tables are built");
        let area = tables.area_node();
        let area_bounds = bounds_of(&harness, area);

        assert_close(
            heading.origin.y,
            page.origin.y,
            "the heading starts the page",
        );
        assert_close(
            strip.origin.y,
            heading.max().y,
            "the tabs follow the heading",
        );
        assert_close(
            area_bounds.origin.y,
            strip.max().y,
            "the tables follow the tabs",
        );
        assert_close(
            area_bounds.max().y,
            page.max().y,
            "nothing is left under the tables",
        );
        assert!(
            area_bounds.size.height > px(300.0),
            "the tables fill the page under the tabs: {}",
            area_bounds.size.height
        );

        let tab = find(&harness, area, "TEST_HYPERLIQUID_WALLET_DETAIL_TAB_0");
        let tab_bounds = bounds_of(&harness, tab);
        assert_eq!(tab_bounds.origin, area_bounds.origin);
        assert_eq!(tab_bounds.size, area_bounds.size);
    }

    #[test]
    fn opening_another_wallet_should_empty_the_tables_of_the_one_before() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        open_trades(&mut harness, &mut pane);
        let tables = pane.widget.tables.as_ref().expect("the tables are built");
        assert_eq!(tables.row_count(DetailTab::Trades), 1);

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::Back, cx)
                .expect("the list is shown")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(1), cx)
                .expect("the other wallet opens")
        });
        sync(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Trades), cx)
                .expect("the tab opens")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let tables = pane.widget.tables.as_ref().expect("the tables are built");
        assert_eq!(
            tables.row_count(DetailTab::Trades),
            0,
            "the wallet that was left is not drawn under the wallet that was opened"
        );
        assert!(
            tables
                .cell_node(DetailTab::Trades, 0, 0, harness.app())
                .is_none(),
            "and no cell of the reading belongs to this wallet"
        );
        assert!(
            tables.overlay_visible(harness.app()),
            "the tab that has not been read is waited for under the spinner"
        );
        assert_eq!(current_state(&harness, &pane), WidgetState::Loading);
    }

    #[test]
    fn a_failed_tab_should_not_leave_the_last_wallets_rows_under_it() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        open_trades(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::Back, cx)
                .expect("the list is shown")
        });
        sync(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(1), cx)
                .expect("the other wallet opens")
        });
        sync(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Trades), cx)
                .expect("the tab opens")
        });
        sync(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&address("2"), DetailTab::Trades),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let tables = pane.widget.tables.as_ref().expect("the tables are built");
        assert_eq!(
            tables.row_count(DetailTab::Trades),
            0,
            "the refused tab draws no reading, least of all another wallet's"
        );
        assert!(
            !tables.overlay_visible(harness.app()),
            "a refusal is not a wait"
        );
        assert!(current_state(&harness, &pane).is_failed());
    }

    #[test]
    fn reopening_a_wallet_should_draw_the_reading_it_already_has() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        open_trades(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::Back, cx)
                .expect("the list is shown")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::OpenWallet(0), cx)
                .expect("the wallet opens again")
        });
        sync(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::SelectTab(DetailTab::Trades), cx)
                .expect("the tab opens")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let tables = pane.widget.tables.as_ref().expect("the tables are built");
        assert_eq!(
            tables.row_count(DetailTab::Trades),
            1,
            "the reading the wallet already has is handed to its table again"
        );
        assert!(
            !tables.overlay_visible(harness.app()),
            "a reading the pane kept is not a wait"
        );
    }

    #[test]
    fn leaving_the_detail_page_should_stop_the_ages_clock() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        open_trades(&mut harness, &mut pane);
        assert!(
            pane.widget
                .tables
                .as_ref()
                .expect("the tables are built")
                .age_clock_armed(),
            "the ages of the wallet on screen are moved once a second"
        );

        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::Back, cx)
                .expect("the list is shown")
        });
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert!(
            !pane
                .widget
                .tables
                .as_ref()
                .expect("the tables are built")
                .age_clock_armed(),
            "a list page has no ages to move"
        );

        let requests = harness.frame_requests();
        for _ in 0..3 {
            harness.advance(Duration::from_secs(1));
        }
        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady list asks for no frame just because a second passed"
        );
    }

    #[test]
    fn refreshing_a_tab_should_say_it_is_loading() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        open_trades(&mut harness, &mut pane);
        pane_cx(&mut harness, pane.scope, |cx| {
            pane.widget
                .apply_intent(Intent::RefreshTab, cx)
                .expect("the tab is read again")
        });
        sync(&mut harness, &mut pane);
        assert!(
            pane.widget
                .tables
                .as_ref()
                .expect("the tables are built")
                .overlay_visible(harness.app()),
            "a tab that is read again is waited for over the reading it shows"
        );
        assert_eq!(
            pane.widget
                .tables
                .as_ref()
                .expect("the tables are built")
                .row_count(DetailTab::Trades),
            1,
            "the reading on hand is not thrown away while it is read again"
        );

        report_detail(
            &mut harness,
            &mut pane,
            &a_wallet(),
            DetailTab::Trades,
            trades_reading(),
        );
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Trades),
            WalletStatus::Ready,
        );
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        let tables = pane.widget.tables.as_ref().expect("the tables are built");
        assert!(
            !tables.overlay_visible(harness.app()),
            "the reading that answers ends the wait"
        );
        assert_eq!(tables.row_count(DetailTab::Trades), 1);
    }

    #[test]
    fn the_retry_control_should_name_how_many_wallets_failed() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &address("2"), overview_of());
        build_header(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            overview_key(&a_wallet()),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        sync(&mut harness, &mut pane);
        let label = pane
            .widget
            .header
            .as_ref()
            .expect("the header is built")
            .retry
            .a11y_label_text()
            .expect("the control has a label")
            .to_owned();
        assert_eq!(
            label, "Retry 1 failed wallet",
            "one failed wallet is named in the singular"
        );

        report_status(
            &mut harness,
            &mut pane,
            overview_key(&address("2")),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        sync(&mut harness, &mut pane);
        let label = pane
            .widget
            .header
            .as_ref()
            .expect("the header is built")
            .retry
            .a11y_label_text()
            .expect("the control has a label")
            .to_owned();
        assert_eq!(
            label, "Retry 2 failed wallets",
            "the control is rebuilt with the count showing now"
        );
    }

    #[test]
    fn a_timeline_that_failed_should_say_so_after_the_totals_arrived() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let state = find(
            &harness,
            pane.parent,
            "TEST_HYPERLIQUID_WALLET_ROW_0_TIMELINE_STATE",
        );
        assert!(
            harness.visible(state),
            "a wallet whose totals arrived and whose timeline has not is still waited for"
        );
        let message = find(
            &harness,
            pane.parent,
            "TEST_HYPERLIQUID_WALLET_ROW_0_TIMELINE_STATE_TEXT",
        );
        assert_eq!(
            harness.text_of(message),
            translate(LanguageCode::En).translate("widget.hyperliquid_wallet.timeline_loading"),
            "the row says the timeline is being read"
        );

        report_status(
            &mut harness,
            &mut pane,
            timeline_key(&a_wallet()),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        assert!(
            harness.visible(state),
            "a failed timeline says so even though the totals arrived"
        );
        let expected = HyperliquidWalletFailure::Stream(WalletStreamError::StreamClosed)
            .translated_message(&translate(LanguageCode::En));
        assert_eq!(
            harness.text_of(message),
            expected.as_str(),
            "the row says why its timeline is not drawn"
        );
    }

    #[test]
    fn a_wallet_row_should_be_its_account_line_over_its_timeline() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet(), address("2")]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        report_overview(&mut harness, &mut pane, &address("2"), overview_of());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        let rows = pane
            .widget
            .list
            .as_ref()
            .expect("the list is built")
            .mounted_rows(harness.app());
        assert_eq!(rows.len(), 2, "both wallets are mounted");
        for (index, row) in rows {
            let row_bounds = bounds_of(&harness, row);
            assert_close(
                row_bounds.size.height,
                WALLET_ROW_HEIGHT,
                "a row is its account line and its timeline",
            );
            let account = find(
                &harness,
                row,
                &format!("TEST_HYPERLIQUID_WALLET_ROW_{index}_ACCOUNT"),
            );
            let account_bounds = bounds_of(&harness, account);
            assert_close(
                account_bounds.size.height,
                ACCOUNT_ROW_HEIGHT,
                "the divider does not squeeze the account line",
            );
            let timeline = find(
                &harness,
                row,
                &format!("TEST_HYPERLIQUID_WALLET_ROW_{index}_TIMELINE_AREA"),
            );
            let timeline_bounds = bounds_of(&harness, timeline);
            assert_close(
                timeline_bounds.size.height,
                timeline::HEIGHT,
                "the timeline keeps its height",
            );
            assert_close(
                timeline_bounds.origin.y,
                account_bounds.max().y,
                "the timeline follows the account line",
            );
            if index > 0 {
                let divider = find(
                    &harness,
                    row,
                    &format!("TEST_HYPERLIQUID_WALLET_ROW_{index}_DIVIDER"),
                );
                assert_close(
                    bounds_of(&harness, divider).size.height,
                    px(1.0),
                    "the divider is one line",
                );
                assert_close(
                    account_bounds.origin.y - row_bounds.origin.y,
                    px(1.0),
                    "the divider sits above the account line, inside the row",
                );
                assert_close(
                    timeline_bounds.max().y,
                    row_bounds.max().y,
                    "a row that carries a divider fills its height",
                );
            } else {
                assert_close(
                    timeline_bounds.max().y,
                    row_bounds.max().y - 1.0,
                    "the first row keeps the divider's line free, as the old one did",
                );
            }
        }
    }

    #[test]
    fn rebuilding_a_rows_controls_should_take_their_nodes_out_of_the_tree() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        build_body(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);
        let before = pane
            .widget
            .row_controls
            .get(&0)
            .expect("the row has its controls")
            .copy
            .node()
            .expect("the copy control is built");
        assert!(
            harness.tree().get(before).is_ok(),
            "the control is in the tree"
        );

        harness.switch_language(LanguageCode::ZhCn);
        sync(&mut harness, &mut pane);
        sync(&mut harness, &mut pane);
        drain(&mut harness);

        assert!(
            harness.tree().get(before).is_err(),
            "the control of the row that was rebuilt is taken out of the tree"
        );
        let after = pane
            .widget
            .row_controls
            .get(&0)
            .expect("the row has its controls again")
            .copy
            .node()
            .expect("the rebuilt control is built");
        assert_ne!(after, before, "the control is a node of its own");
        assert!(
            harness.tree().get(after).is_ok(),
            "the rebuilt control is in the tree"
        );
    }

    #[test]
    fn a_status_after_a_failure_should_be_refused() {
        let mut harness = harness();
        let mut pane = quiet_pane(&mut harness, vec![a_wallet()]);
        load(&mut harness, &mut pane);
        report_overview(&mut harness, &mut pane, &a_wallet(), overview_of());
        open_wallet(&mut harness, &mut pane);
        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Failed(WalletStreamError::StreamClosed),
        );
        assert!(current_state(&harness, &pane).is_failed());

        report_status(
            &mut harness,
            &mut pane,
            detail_key(&a_wallet(), DetailTab::Spot),
            WalletStatus::Ready,
        );
        assert!(
            current_state(&harness, &pane).is_failed(),
            "a late status is not a recovery"
        );
        assert!(matches!(
            pane.widget.detail[&(a_wallet(), DetailTab::Spot)].status,
            ScopeStatus::Failed(_)
        ));
    }
}
