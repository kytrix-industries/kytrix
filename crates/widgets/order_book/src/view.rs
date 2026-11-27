use std::{
    cell::{Cell, RefCell},
    convert::Infallible,
    mem,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use instrument_selector_ui::{
    host::SelectorHost,
    instrument_button::{InstrumentButton, InstrumentButtonHandle},
    instrument_selector::{
        InstrumentFilters, InstrumentKindFilter, InstrumentStatusFilter, LoadedScope, offers_route,
    },
};
use market_participants_connector::catalog::display::catalog::DisplaySnapshot;
use market_participants_model::{
    asset::AssetId,
    decimal::{Price, Tick},
    listing::route::VenueListingKey,
    market::{book::MarketSequence, selection::InstrumentSelection, units::QuantityUnit},
    routing::CapabilityId,
};
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yPatch,
    app::{App, TimerId},
    error::EngineError,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::{TextAlign, TextStyle},
    window::WindowId,
};
use ui_kit::{
    kit::{
        empty_state::EmptyState,
        icon::{IconKind, IconSpec},
        list::{List, ListOptions},
        motion::{DEPTH_ANIMATION_SETTLE_EPSILON, depth_animation_progress_after},
        select::{Select, SelectItem, SelectOptions},
        separator::Separator,
        size::UnifiedSize,
    },
    theme::{handle::ThemeHandle, role::Role},
};
use widget::{
    binding::{Inbound, Pending, WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    error::{TranslatedError, market_is_catalog_not_loaded},
    identity::{WidgetId, WidgetInstanceId},
    protocol::RequestId,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{HeaderGroup, ReloadHandle, WidgetView},
};

use super::{
    directory::{
        ConnectorDirectory, ListingSupport, MarketDirectory, listing_support, market_venues,
    },
    model::{
        GroupedBook, GroupedLevel, OrderBookMetadata, OrderBookSnapshot, PRICE_MULTIPLIERS,
        WINDOW_SIZES, default_window_size, interpolated_ratio, is_after, price_step_label,
        price_step_labels, window_sizes,
    },
    paint::{
        ASK_BARS_SELECTOR, BID_BARS_SELECTOR, Imbalance, SideCanvas, drawable_rows, row_stride,
    },
    service::{
        OrderBookProtocol, OrderBookRequest, OrderBookResponse, OrderBookService, OrderBookStatus,
        OrderBookStreamError, OrderBookStreamKey, OrderBookStreamMessage,
    },
};

const SNAPSHOT_DEPTH: u16 = 5_000;

const DEPTH_ANIMATION_TICK: Duration = Duration::from_millis(16);

const INSTRUMENT_KEY_SUFFIX: &str = "order_book.header.instrument";
const PRICE_STEP_KEY_SUFFIX: &str = "order_book.header.price_step";
const WINDOW_SIZE_KEY_SUFFIX: &str = "order_book.header.window_size";
const ASK_LIST_KEY_SUFFIX: &str = "order_book.ask";
const BID_LIST_KEY_SUFFIX: &str = "order_book.bid";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBookConfig {
    selection: Option<InstrumentSelection>,
    price_multiplier: u32,
    window_size: usize,
}

impl Default for OrderBookConfig {
    fn default() -> Self {
        Self {
            selection: None,
            price_multiplier: PRICE_MULTIPLIERS[0],
            window_size: WINDOW_SIZES[0],
        }
    }
}

impl WidgetConfig for OrderBookConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

impl OrderBookConfig {
    fn settle(&mut self, depth: Option<u16>) -> bool {
        let defaults = Self::default();
        let mut settled = false;
        if !PRICE_MULTIPLIERS.contains(&self.price_multiplier) {
            self.price_multiplier = defaults.price_multiplier;
            settled = true;
        }
        if let Some(depth) = depth
            && !window_sizes(Some(depth)).contains(&self.window_size)
        {
            self.window_size = default_window_size(Some(depth));
            settled = true;
        }

        settled
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OrderBookFailure {
    Stream(OrderBookStreamError),
    Rpc(widget::rpc::RpcCallError<Infallible>),
}

impl TranslatedError for OrderBookFailure {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::Stream(error) => error.translated_message(translate),
            Self::Rpc(error) => error.translated_message(translate),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    Start,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MidDirection {
    Up,
    Down,
    Flat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderRequest {
    PriceStep(u32),
    WindowSize(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OrderBookUnitLabels {
    price: SmolStr,
    quantity: SmolStr,
}

impl OrderBookUnitLabels {
    fn of(metadata: &OrderBookMetadata, display: &DisplaySnapshot, translate: &Translate) -> Self {
        Self {
            price: asset_display(display, &metadata.quote_asset, translate),
            quantity: quantity_label(&metadata.quantity_unit, display, translate),
        }
    }
}

fn asset_display(display: &DisplaySnapshot, asset: &AssetId, translate: &Translate) -> SmolStr {
    display.resolver().asset_code(asset).map_or_else(
        |_| t_smol!(translate, "error.display.unknown_asset"),
        |code| SmolStr::new(code.as_str()),
    )
}

fn quantity_label(
    unit: &QuantityUnit,
    display: &DisplaySnapshot,
    translate: &Translate,
) -> SmolStr {
    match unit {
        QuantityUnit::AssetUnits { asset } | QuantityUnit::Notional { asset } => {
            asset_display(display, asset, translate)
        }
        QuantityUnit::Contracts => t_smol!(translate, "widget.order_book.contracts_unit"),
        QuantityUnit::OutcomeTokens { .. } => {
            t_smol!(translate, "widget.order_book.outcome_tokens_unit")
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowSide {
    Ask,
    Bid,
}

impl RowSide {
    fn price_role(self) -> Role {
        match self {
            Self::Ask => Role::MarketDown,
            Self::Bid => Role::MarketUp,
        }
    }

    fn depth_role(self) -> Role {
        match self {
            Self::Ask => Role::MarketDownWeak,
            Self::Bid => Role::MarketUpWeak,
        }
    }
}

fn mid_direction_of(current: Option<Price>, previous: Option<Price>) -> MidDirection {
    match (current, previous) {
        (Some(current), Some(previous)) if current > previous => MidDirection::Up,
        (Some(current), Some(previous)) if current < previous => MidDirection::Down,
        _ => MidDirection::Flat,
    }
}

fn depth_is_active(progress: f32, span: f32) -> bool {
    span * (1.0 - progress) > DEPTH_ANIMATION_SETTLE_EPSILON
}

struct DepthTickerState {
    progress: Signal<f32>,
    span: Signal<f32>,
    ticking: Cell<bool>,
    alive: Cell<bool>,
    window: WindowId,
    scope: NodeId,
    timer: RefCell<Option<TimerId>>,
}

impl DepthTickerState {
    fn schedule(self: &Rc<Self>, app: &mut App) {
        let state = Rc::clone(self);
        match app.after_scoped(self.window, self.scope, DEPTH_ANIMATION_TICK, move |app| {
            tick_depth(app, state);
        }) {
            Ok(timer) => *self.timer.borrow_mut() = Some(timer),
            Err(error) => {
                log::warn!("the order book's depth animation could not schedule a tick: {error}");
                self.ticking.set(false);
            }
        }
    }
}

fn tick_depth(app: &mut App, state: Rc<DepthTickerState>) {
    if !state.alive.get() {
        return;
    }
    let progress = *app.read(&state.progress);
    let span = *app.read(&state.span);
    let next = depth_animation_progress_after(progress, span, DEPTH_ANIMATION_TICK);
    app.write(&state.progress, next);
    let _ = app.request_repaint(state.window);
    if !depth_is_active(next, span) {
        state.ticking.set(false);
        return;
    }
    state.schedule(app);
}

#[derive(Clone, Default)]
struct PublishedBook {
    book: Option<Arc<GroupedBook>>,
}

impl PartialEq for PublishedBook {
    fn eq(&self, other: &Self) -> bool {
        match (&self.book, &other.book) {
            (None, None) => true,
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl PublishedBook {
    fn mid_price_text(&self) -> Option<&SmolStr> {
        self.book.as_ref()?.mid_price_text.as_ref()
    }

    fn spread_text(&self) -> Option<&SmolStr> {
        self.book.as_ref()?.spread_text.as_ref()
    }

    fn imbalance_text(&self, side: RowSide) -> Option<&SmolStr> {
        let book = self.book.as_ref()?;
        Some(match side {
            RowSide::Ask => &book.ask_imbalance_text,
            RowSide::Bid => &book.bid_imbalance_text,
        })
    }

    fn level(&self, side: RowSide, row: usize) -> Option<&GroupedLevel> {
        let book = self.book.as_ref()?;
        match side {
            RowSide::Ask => book.asks.get(row),
            RowSide::Bid => book.bids.get(row),
        }
    }

    fn is_empty(&self) -> bool {
        self.book
            .as_ref()
            .is_some_and(|book| book.asks.is_empty() && book.bids.is_empty())
    }

    fn direction_against(&self, previous: Option<Price>) -> MidDirection {
        mid_direction_of(self.book.as_ref().and_then(|book| book.mid_price), previous)
    }
}

struct HeaderLeft {
    control: InstrumentButtonHandle,
    parent: NodeId,
    built: HeaderState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HeaderState {
    selection: Option<InstrumentSelection>,
    language: LanguageCode,
    generation: u64,
}

struct HeaderRight {
    slot: NodeId,
    root: NodeId,
    price_step: Select,
    window_size: Select,
    built: HeaderRightState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HeaderRightState {
    language: LanguageCode,
    depth: Option<u16>,
    price_tick: Option<Tick>,
    price_multiplier: u32,
    window_size: usize,
    disabled: bool,
}

pub struct OrderBookWidget {
    config: OrderBookConfig,
    binding: Option<WidgetBinding<OrderBookProtocol>>,
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    directory: Option<Rc<dyn MarketDirectory>>,
    body_root: Option<NodeId>,
    ask_area: Option<NodeId>,
    bid_area: Option<NodeId>,
    ask_list: Option<List>,
    bid_list: Option<List>,
    ask_bars: SideCanvas,
    bid_bars: SideCanvas,
    imbalance: Imbalance,
    book: Signal<PublishedBook>,
    unit_labels: Signal<OrderBookUnitLabels>,
    mid_direction: Signal<MidDirection>,
    depth_progress: Signal<f32>,
    depth_span: Signal<f32>,
    empty_text: Signal<SmolStr>,
    selection_requests: Signal<Option<InstrumentSelection>>,
    header_requests: Signal<Option<HeaderRequest>>,
    language: Signal<LanguageCode>,
    generation: Signal<u64>,
    translate: Translate,
    theme: ThemeHandle,
    snapshot: Option<OrderBookSnapshot>,
    grouped: Option<Arc<GroupedBook>>,
    ready: Option<MarketSequence>,
    metadata: Option<OrderBookMetadata>,
    price_step_labels: [SmolStr; PRICE_MULTIPLIERS.len()],
    drawable_rows: usize,
    previous_mid_price: Option<Price>,
    book_dirty: bool,
    awaiting_market: bool,
    market_reloaded: bool,
    seen_generation: u64,
    built_language: Option<LanguageCode>,
    depth_ticker: Option<Rc<DepthTickerState>>,
    header_left: Option<HeaderLeft>,
    header_right: Option<HeaderRight>,
}

impl WidgetView for OrderBookWidget {
    type Config = OrderBookConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.order_book");
    const DISPLAY_NAME: &'static str = "widget.names.order_book";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: Option<&'static str> = Some("widget.descriptions.order_book");
    const ICON: Option<&'static str> = Some("widgets/order_book.png");

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
                config,
                None,
                None,
                status,
                config_change,
                recovery,
                cx,
            ));
        };
        let service = OrderBookService::new(connector.market(), SNAPSHOT_DEPTH);
        let binding = WidgetBinding::spawn(cx, service);
        let directory: Rc<dyn MarketDirectory> = Rc::new(ConnectorDirectory::new(connector));
        Ok(Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            cx,
        ))
    }

    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    fn has_content(&self) -> bool {
        self.snapshot.is_some()
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(directory) = self.directory.clone() else {
            self.wait_for_market(cx);
            return;
        };
        if !self.market_ready(directory.as_ref()) {
            self.wait_for_market(cx);
            return;
        }
        self.awaiting_market = false;
        let market_reloaded = mem::take(&mut self.market_reloaded);
        let retry = self.status.is_failed(cx.app());
        if !retry && !market_reloaded {
            self.clear_book(cx);
        }
        self.start(retry, market_reloaded, cx);
    }

    fn body(&mut self, parent: NodeId, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let theme = self.theme.clone();
        let tokens = theme.tokens();
        let font_size = tokens.small.font_size;
        let label_height = tokens.small.height;
        let mid_height = tokens.medium.height + tokens.large.font_size;
        let imbalance_height = tokens.small.height;
        let stride = row_stride(font_size);

        let translate = self.translate.clone();
        let language = self.language;
        let root = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_LADDER")
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_1()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_a11y(move |read| {
                let _language = *read.read(&language);
                A11yPatch {
                    label: Some(t_smol!(translate, "widget.order_book.book")),
                    ..A11yPatch::default()
                }
            })
            .build(cx.tree_mut(), parent)?;
        self.body_root = Some(root);

        self.build_column_labels(cx, root, font_size, label_height)?;
        Separator::horizontal(theme.clone()).build(cx.tree_mut(), root)?;
        let ask_area = self.build_ladder_area(cx, root, RowSide::Ask, font_size, stride)?;
        self.ask_area = Some(ask_area);
        Separator::horizontal(theme.clone()).build(cx.tree_mut(), root)?;
        self.build_mid_price(cx, root, mid_height)?;
        Separator::horizontal(theme.clone()).build(cx.tree_mut(), root)?;
        let bid_area = self.build_ladder_area(cx, root, RowSide::Bid, font_size, stride)?;
        self.bid_area = Some(bid_area);
        self.build_imbalance(cx, root, imbalance_height, font_size)?;
        self.build_empty_state(cx, root)?;

        self.publish_book(cx)?;
        self.publish_unit_labels(cx);
        self.publish_empty_text(cx);
        self.ensure_depth_ticker(cx);
        Ok(())
    }

    fn sync(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let generation = *cx.read(&self.generation);
        if generation != self.seen_generation {
            self.seen_generation = generation;
            if !self.status.is_failed(cx.app()) {
                self.market_reloaded = true;
                self.load(cx);
            }
        }

        if let Some(binding) = self.binding.as_ref() {
            for inbound in binding.take() {
                self.apply(inbound, cx);
            }
        }

        let requested = (*cx.read(&self.selection_requests)).clone();
        if let Some(selection) = requested {
            cx.write(&self.selection_requests, None);
            self.select_selection(selection, cx);
        }
        let requested = *cx.read(&self.header_requests);
        if let Some(request) = requested {
            cx.write(&self.header_requests, None);
            match request {
                HeaderRequest::PriceStep(multiplier) => {
                    self.select_price_multiplier(multiplier, cx)
                }
                HeaderRequest::WindowSize(window_size) => {
                    self.select_window_size(window_size, cx);
                }
            }
        }

        let ladder_bounds = self.ask_area.and_then(|node| {
            cx.tree()
                .get(node)
                .ok()
                .map(|target| target.layout().bounds)
        });
        if let Some(bounds) = ladder_bounds
            && bounds.size.height > px(0.0)
        {
            let drawable = drawable_rows(bounds, self.theme.tokens().small.font_size);
            if drawable != self.drawable_rows {
                self.drawable_rows = drawable;
                self.regroup(cx);
            }
        }

        if self.book_dirty {
            self.book_dirty = false;
            self.publish_book(cx)?;
        }

        self.sync_bars(cx)?;
        self.publish_empty_text(cx);
        self.sync_lists(cx)?;
        self.sync_header(cx)?;

        Ok(())
    }

    fn header(
        &mut self,
        group: HeaderGroup,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        match group {
            HeaderGroup::Left => {
                self.release_header_left(cx, true);
                self.build_header_left(parent, cx)
            }
            HeaderGroup::Right => {
                self.release_header_right(cx, true);
                self.build_header_right(parent, cx)
            }
        }
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        self.release_header_left(cx, false);
        self.release_header_right(cx, false);
        if let Some(list) = self.ask_list.take() {
            list.release(cx.app_mut());
        }
        if let Some(list) = self.bid_list.take() {
            list.release(cx.app_mut());
        }
        self.stop_depth_animation(cx.app_mut());
        if let Some(mut binding) = self.binding.take() {
            binding.cancel();
        }
        self.body_root = None;
        self.ask_area = None;
        self.bid_area = None;
    }
}

impl OrderBookWidget {
    fn assemble(
        config: OrderBookConfig,
        binding: Option<WidgetBinding<OrderBookProtocol>>,
        directory: Option<Rc<dyn MarketDirectory>>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let book = cx.signal(PublishedBook::default());
        let unit_labels = cx.signal(OrderBookUnitLabels {
            price: SmolStr::new_static("--"),
            quantity: SmolStr::new_static("--"),
        });
        let mid_direction = cx.signal(MidDirection::Flat);
        let depth_progress = cx.signal(1.0_f32);
        let depth_span = cx.signal(0.0_f32);
        let empty_text = cx.signal(SmolStr::new_static(""));
        let selection_requests = cx.signal(None::<InstrumentSelection>);
        let header_requests = cx.signal(None::<HeaderRequest>);
        let language = cx.services().language;
        let generation = cx.services().catalog_generation;
        let seen_generation = *cx.read(&generation);
        let translate = cx.translate().clone();
        let theme = cx.theme().clone();

        Self {
            config,
            binding,
            pending: Pending::new(),
            status,
            recovery,
            config_change,
            directory,
            body_root: None,
            ask_area: None,
            bid_area: None,
            ask_list: None,
            bid_list: None,
            ask_bars: SideCanvas::new(),
            bid_bars: SideCanvas::new(),
            imbalance: Imbalance::new(),
            book,
            unit_labels,
            mid_direction,
            depth_progress,
            depth_span,
            empty_text,
            selection_requests,
            header_requests,
            language,
            generation,
            translate,
            theme,
            snapshot: None,
            grouped: None,
            ready: None,
            metadata: None,
            price_step_labels: price_step_labels(None),
            drawable_rows: WINDOW_SIZES[WINDOW_SIZES.len() - 1],
            previous_mid_price: None,
            book_dirty: false,
            awaiting_market: false,
            market_reloaded: false,
            seen_generation,
            built_language: None,
            depth_ticker: None,
            header_left: None,
            header_right: None,
        }
    }

    #[cfg(test)]
    fn with_source<S: super::service::OrderBookSource>(
        config: OrderBookConfig,
        source: S,
        directory: Rc<dyn MarketDirectory>,
        status: WidgetStatus,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        let service = OrderBookService::new(source, SNAPSHOT_DEPTH);
        let binding = WidgetBinding::spawn(cx, service);
        Self::assemble(
            config,
            Some(binding),
            Some(directory),
            status,
            config_change,
            recovery,
            cx,
        )
    }

    fn build_column_labels(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        font_size: LogicalPx,
        height: LogicalPx,
    ) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_COLUMNS")
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .flex_none()
                    .height(height)
                    .padding_x(px(8.0))
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let translate = self.translate.clone();
        let language = self.language;
        let labels = self.unit_labels;
        let theme = self.theme.clone();
        for (key, id) in [
            (
                "widget.order_book.price_with_unit",
                "TEST_ORDER_BOOK_COLUMN_PRICE",
            ),
            (
                "widget.order_book.amount_with_unit",
                "TEST_ORDER_BOOK_COLUMN_AMOUNT",
            ),
            (
                "widget.order_book.total_with_unit",
                "TEST_ORDER_BOOK_COLUMN_TOTAL",
            ),
        ] {
            let translate = translate.clone();
            let theme = theme.clone();
            NodeBuilder::text(SmolStr::default(), aligned_text(font_size))
                .id(id)
                .style(
                    StyleBuilder::new()
                        .flex_1()
                        .min_width_0()
                        .height_full()
                        .build(),
                )
                .bind_text(move |read| {
                    let _language = *read.read(&language);
                    let labels = read.read(&labels);
                    let unit = match key {
                        "widget.order_book.price_with_unit" => labels.price.clone(),
                        "widget.order_book.amount_with_unit" => labels.quantity.clone(),
                        _ => labels.quantity.clone(),
                    };
                    t_smol!(translate, key, unit = unit)
                })
                .bind_style(move |read| StylePatch {
                    foreground: Some(theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), row)?;
        }
        Ok(())
    }

    fn build_ladder_area(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        side: RowSide,
        font_size: LogicalPx,
        stride: LogicalPx,
    ) -> Result<NodeId, EngineError> {
        let area = NodeBuilder::boxed()
            .id(match side {
                RowSide::Ask => "TEST_ORDER_BOOK_ASKS",
                RowSide::Bid => "TEST_ORDER_BOOK_BIDS",
            })
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_1()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let theme = self.theme.clone();
        let selector = match side {
            RowSide::Ask => ASK_BARS_SELECTOR,
            RowSide::Bid => BID_BARS_SELECTOR,
        };
        match side {
            RowSide::Ask => self.ask_bars.build(cx, area, selector, theme.clone())?,
            RowSide::Bid => self.bid_bars.build(cx, area, selector, theme.clone())?,
        }

        let book = self.book;
        let row_theme = self.theme.clone();
        let key = cx.unique_key(match side {
            RowSide::Ask => ASK_LIST_KEY_SUFFIX,
            RowSide::Bid => BID_LIST_KEY_SUFFIX,
        });
        let list = List::new(
            cx.app_mut(),
            theme,
            key,
            ListOptions::fixed(stride),
            move |tree, parent, row| {
                build_row(tree, parent, row, side, book, row_theme.clone(), font_size)
            },
        );
        let count = match side {
            RowSide::Ask => self.grouped.as_ref().map_or(0, |book| book.asks.len()),
            RowSide::Bid => self.grouped.as_ref().map_or(0, |book| book.bids.len()),
        };
        list.set_initial_count(cx.app_mut(), count);
        let window = cx.window();
        list.mount(cx.app_mut(), window, area)?;
        match side {
            RowSide::Ask => self.ask_list = Some(list),
            RowSide::Bid => self.bid_list = Some(list),
        }
        Ok(area)
    }

    fn build_mid_price(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        height: LogicalPx,
    ) -> Result<(), EngineError> {
        let tokens = self.theme.tokens();
        let font_size = tokens.large.font_size;
        let small_size = tokens.small.font_size;
        let row = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_MID")
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .height(height)
                    .gap(px(8.0))
                    .padding_x(px(8.0))
                    .min_width_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;

        let direction = self.mid_direction;
        let window = cx.window();
        for (kind, role, shown) in [
            (IconKind::ArrowUp, Role::MarketUp, MidDirection::Up),
            (IconKind::ArrowDown, Role::MarketDown, MidDirection::Down),
        ] {
            let slot = NodeBuilder::boxed()
                .style(StyleBuilder::new().flex_none().build())
                .bind_style(move |read| StylePatch {
                    visible: Some(*read.read(&direction) == shown),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), row)?;
            let icon = IconSpec {
                kind,
                size: tokens.small.icon_size,
                ..IconSpec::new(kind)
            };
            if let Err(error) = icon.build_themed(cx.app_mut(), window, slot, &self.theme, role) {
                log::warn!("the order book's mid-price arrow could not be built: {error}");
            }
        }

        let book = self.book;
        NodeBuilder::text(SmolStr::default(), bold_text(font_size))
            .id("TEST_ORDER_BOOK_MID_PRICE")
            .style(StyleBuilder::new().flex_none().build())
            .bind_text(move |read| {
                let book = read.read(&book);
                book.mid_price_text()
                    .cloned()
                    .unwrap_or_else(|| SmolStr::new_static("--"))
            })
            .bind_style({
                let theme = self.theme.clone();
                let direction = self.mid_direction;
                move |read| {
                    let role = match *read.read(&direction) {
                        MidDirection::Up => Role::MarketUp,
                        MidDirection::Down => Role::MarketDown,
                        MidDirection::Flat => Role::MarketFlat,
                    };
                    StylePatch {
                        foreground: Some(theme.role(read, role)),
                        ..StylePatch::default()
                    }
                }
            })
            .build(cx.tree_mut(), row)?;

        let translate = self.translate.clone();
        let language = self.language;
        NodeBuilder::text(SmolStr::default(), TextStyle::new(small_size))
            .id("TEST_ORDER_BOOK_SPREAD")
            .style(StyleBuilder::new().flex_none().build())
            .bind_text(move |read| {
                let _language = *read.read(&language);
                let book = read.read(&book);
                book.spread_text().map_or_else(
                    || SmolStr::new_static("--"),
                    |spread| t_smol!(translate, "widget.order_book.spread", value = spread),
                )
            })
            .bind_style({
                let theme = self.theme.clone();
                move |read| StylePatch {
                    foreground: Some(theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                }
            })
            .build(cx.tree_mut(), row)?;
        Ok(())
    }

    fn build_imbalance(
        &mut self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        height: LogicalPx,
        font_size: LogicalPx,
    ) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_IMBALANCE_ROW")
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_none()
                    .height(height)
                    .min_width_0()
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let theme = self.theme.clone();
        self.imbalance.build(cx, row, theme.clone())?;
        let overlay = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .relative()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .size_full()
                    .padding_x(px(8.0))
                    .build(),
            )
            .build(cx.tree_mut(), row)?;

        let book = self.book;
        for (side, role, id) in [
            (
                RowSide::Ask,
                Role::MarketDown,
                "TEST_ORDER_BOOK_ASK_IMBALANCE",
            ),
            (
                RowSide::Bid,
                Role::MarketUp,
                "TEST_ORDER_BOOK_BID_IMBALANCE",
            ),
        ] {
            NodeBuilder::text(SmolStr::default(), bold_text(font_size))
                .id(id)
                .style(StyleBuilder::new().flex_none().build())
                .bind_text(move |read| {
                    let book = read.read(&book);
                    book.imbalance_text(side)
                        .cloned()
                        .unwrap_or_else(|| SmolStr::new_static("--"))
                })
                .bind_style({
                    let theme = theme.clone();
                    move |read| StylePatch {
                        foreground: Some(theme.role(read, role)),
                        ..StylePatch::default()
                    }
                })
                .build(cx.tree_mut(), overlay)?;
        }
        Ok(())
    }

    fn build_empty_state(&self, cx: &mut WidgetCx<'_>, parent: NodeId) -> Result<(), EngineError> {
        let book = self.book;
        let overlay = NodeBuilder::boxed()
            .id("TEST_ORDER_BOOK_EMPTY_OVERLAY")
            .style(
                StyleBuilder::new()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .size_full()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .build(),
            )
            .bind_style(move |read| StylePatch {
                visible: Some(read.read(&book).is_empty()),
                ..StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;
        EmptyState::new(self.theme.clone(), "TEST_ORDER_BOOK_EMPTY")
            .bind_text(self.empty_text)
            .build(cx.tree_mut(), overlay)?;
        Ok(())
    }

    fn publish_book(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let published = PublishedBook {
            book: self.grouped.clone(),
        };
        let (ask_count, bid_count) = self
            .grouped
            .as_ref()
            .map_or((0, 0), |book| (book.asks.len(), book.bids.len()));
        let direction = published.direction_against(self.previous_mid_price);
        let changed = cx.write(&self.book, published);
        cx.write(&self.mid_direction, direction);
        if let Some(list) = self.ask_list.as_ref() {
            list.set_count(cx.app_mut(), ask_count)?;
        }
        if let Some(list) = self.bid_list.as_ref() {
            list.set_count(cx.app_mut(), bid_count)?;
        }
        if changed {
            cx.request_frame()?;
        }
        Ok(())
    }

    fn publish_unit_labels(&mut self, cx: &mut WidgetCx<'_>) {
        let labels = self.unit_labels_now();
        cx.write(&self.unit_labels, labels);
    }

    fn publish_empty_text(&mut self, cx: &mut WidgetCx<'_>) {
        let language = *cx.read(&self.language);
        if self.built_language == Some(language) {
            return;
        }
        self.built_language = Some(language);
        cx.write(&self.empty_text, t_smol!(self.translate, "common.no_data"));
    }

    fn unit_labels_now(&self) -> OrderBookUnitLabels {
        let Some(metadata) = self.metadata.as_ref() else {
            return OrderBookUnitLabels {
                price: SmolStr::new_static("--"),
                quantity: SmolStr::new_static("--"),
            };
        };
        let Some(directory) = self.directory.as_ref() else {
            return OrderBookUnitLabels {
                price: SmolStr::new_static("--"),
                quantity: SmolStr::new_static("--"),
            };
        };
        OrderBookUnitLabels::of(metadata, &directory.display(), &self.translate)
    }

    fn sync_bars(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(book) = self.grouped.clone() else {
            return Ok(());
        };
        let progress = *cx.read(&self.depth_progress);
        let font_size = self.theme.tokens().small.font_size;
        let appearance = self.theme.current(cx.app());
        let resolved = self.theme.theme_for(appearance);
        let ask_color = resolved.role(RowSide::Ask.depth_role());
        let bid_color = resolved.role(RowSide::Bid.depth_role());
        let ask_levels = book.asks.clone();
        let ask_offset = self
            .ask_list
            .as_ref()
            .and_then(|list| list.offset(cx.app()))
            .unwrap_or(LogicalPx::ZERO);
        self.ask_bars.sync(
            cx,
            &ask_levels,
            progress,
            font_size,
            ask_offset,
            ask_color,
            true,
        )?;
        let bid_levels = book.bids.clone();
        let bid_offset = self
            .bid_list
            .as_ref()
            .and_then(|list| list.offset(cx.app()))
            .unwrap_or(LogicalPx::ZERO);
        self.bid_bars.sync(
            cx,
            &bid_levels,
            progress,
            font_size,
            bid_offset,
            bid_color,
            false,
        )?;
        let ask_ratio =
            interpolated_ratio(book.previous_imbalance, book.current_imbalance, progress);
        self.imbalance.sync(cx, ask_ratio, ask_color, bid_color)?;
        Ok(())
    }

    fn sync_lists(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(list) = self.ask_list.as_ref() {
            list.sync(cx.app_mut())?;
        }
        if let Some(list) = self.bid_list.as_ref() {
            list.sync(cx.app_mut())?;
        }
        Ok(())
    }

    fn market_ready(&self, directory: &dyn MarketDirectory) -> bool {
        if !directory.any_market_loaded() {
            return false;
        }

        match self.config.selection.as_ref() {
            Some(selection) => directory.is_loaded(&selection.listing.route),
            None => true,
        }
    }

    fn wait_for_market(&mut self, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = true;
        self.clear_book(cx);
        self.report_markets_not_loaded(cx);
    }

    fn report_markets_not_loaded(&self, cx: &mut WidgetCx<'_>) {
        let supported = market_venues();
        let venues = self
            .config
            .selection
            .as_ref()
            .map(|selection| vec![selection.listing.route.venue])
            .unwrap_or_else(|| supported.clone());

        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::MarketNotLoaded { venues, supported }),
        );
    }

    fn report_nothing_chosen(&self, cx: &mut WidgetCx<'_>) {
        self.status.report(
            cx.app_mut(),
            WidgetState::waiting(Notice::message(
                "widget.order_book.select_instrument_prompt",
            )),
        );
    }

    fn report_waiting_for_directory(
        &mut self,
        error: &OrderBookStreamError,
        cx: &mut WidgetCx<'_>,
    ) -> bool {
        let OrderBookStreamError::MarketParticipant(error) = error else {
            return false;
        };
        if !market_is_catalog_not_loaded(error) && !error.is_venue_not_loaded() {
            return false;
        }
        self.wait_for_market(cx);
        true
    }

    fn current_key(&self) -> Option<OrderBookStreamKey> {
        self.config.selection.as_ref().map(|selection| {
            OrderBookStreamKey::new(
                selection.listing.clone(),
                self.config.price_multiplier,
                self.config.window_size,
            )
        })
    }

    fn start(&mut self, retry: bool, market_reloaded: bool, cx: &mut WidgetCx<'_>) {
        self.awaiting_market = false;
        self.ready = None;
        let Some(directory) = self.directory.clone() else {
            self.nothing_chosen(cx);
            return;
        };
        let Some(selection) = self.config.selection.clone() else {
            self.clear_book(cx);
            self.report_nothing_chosen(cx);
            return;
        };
        let depth = directory.order_book_depth(&selection.listing.route);
        if self.config.settle(depth) {
            log::warn!(
                "the book's stored grouping or row limit is not offered here; it was reset to \
                 the default"
            );
            self.config_change.publish(cx.app_mut());
        }
        let routes = directory.loaded_scopes();
        if !offers_route(&routes, &selection.listing.route, CapabilityId::OrderBook) {
            self.withdraw(&selection.listing, "is no longer a loaded route", cx);
            return;
        }
        let offered = directory.contains(&selection.listing);
        let support = listing_support(&selection.listing, offered, &routes, directory.as_ref());
        if support == ListingSupport::Withdrawn {
            self.withdraw(&selection.listing, "is no longer offered by its route", cx);
            return;
        }
        self.status.report(cx.app_mut(), WidgetState::Connecting);
        if !retry && !market_reloaded {
            self.clear_book(cx);
        }
        let key = OrderBookStreamKey::new(
            selection.listing.clone(),
            self.config.price_multiplier,
            self.config.window_size,
        );
        let request = if retry || market_reloaded {
            OrderBookRequest::Retry(key)
        } else {
            OrderBookRequest::Load(key)
        };
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, request);
            self.pending.track(request_id, PendingCommand::Start);
        }
    }

    fn nothing_chosen(&mut self, cx: &mut WidgetCx<'_>) {
        self.clear_book(cx);
        self.report_nothing_chosen(cx);
    }

    fn withdraw(&mut self, listing: &VenueListingKey, cause: &str, cx: &mut WidgetCx<'_>) {
        log::warn!(
            "`{}` {cause}; the book went back to having nothing chosen",
            listing.listing_id
        );
        self.config.selection = None;
        self.config_change.publish(cx.app_mut());
        self.nothing_chosen(cx);
    }

    fn clear_book(&mut self, cx: &mut WidgetCx<'_>) {
        self.snapshot = None;
        self.grouped = None;
        self.metadata = None;
        self.price_step_labels = price_step_labels(None);
        self.previous_mid_price = None;
        self.ready = None;
        self.stop_depth_animation(cx.app_mut());
        cx.write(&self.depth_progress, 1.0);
        cx.write(&self.depth_span, 0.0);
        cx.write(
            &self.unit_labels,
            OrderBookUnitLabels {
                price: SmolStr::new_static("--"),
                quantity: SmolStr::new_static("--"),
            },
        );
        self.book_dirty = true;
    }

    fn apply(&mut self, inbound: Inbound<OrderBookProtocol>, cx: &mut WidgetCx<'_>) {
        match inbound {
            Inbound::Reliable { key, message } => self.apply_reliable(key, &message, cx),
            Inbound::Latest { key, message } => self.apply_latest(key, &message, cx),
            Inbound::Rpc { request_id, result } => self.apply_rpc(request_id, result, cx),
        }
    }

    fn apply_reliable(
        &mut self,
        key: OrderBookStreamKey,
        message: &OrderBookStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        let OrderBookStreamMessage::Status(status) = message else {
            return;
        };
        match status {
            OrderBookStatus::Connecting => {
                if self.snapshot.is_none() {
                    self.status.report(cx.app_mut(), WidgetState::Connecting);
                }
            }
            OrderBookStatus::LoadingSnapshot(metadata) => {
                if self.set_metadata(metadata.clone(), cx) {
                    self.book_dirty = true;
                }
                self.status.report(cx.app_mut(), WidgetState::Loading);
            }
            OrderBookStatus::Ready { revision } => {
                self.ready = Some(revision.clone());
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !is_after(revision, &snapshot.sequence))
                {
                    self.status.report(cx.app_mut(), WidgetState::Ready);
                } else {
                    self.status.report(cx.app_mut(), WidgetState::Loading);
                }
            }
            OrderBookStatus::Error(error) => {
                if self.report_waiting_for_directory(error, cx) {
                    return;
                }
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(
                        Arc::new(OrderBookFailure::Stream(error.clone())),
                        recovery,
                    ),
                );
            }
        }
    }

    fn apply_latest(
        &mut self,
        key: OrderBookStreamKey,
        message: &OrderBookStreamMessage,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.current_key().as_ref() != Some(&key)
            || self.status.is_failed(cx.app())
            || self.awaiting_market
        {
            return;
        }
        if let OrderBookStreamMessage::Snapshot(snapshot) = message {
            self.accept_snapshot(snapshot.clone(), cx);
        }
    }

    fn accept_snapshot(&mut self, snapshot: OrderBookSnapshot, cx: &mut WidgetCx<'_>) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| is_after(&current.sequence, &snapshot.sequence))
        {
            return;
        }
        let metadata_changed = self.set_metadata(snapshot.metadata.clone(), cx);
        let sequence = snapshot.sequence.clone();
        self.snapshot = Some(snapshot);
        if metadata_changed {
            self.book_dirty = true;
        }
        let regrouped = self.regroup(cx);
        let _ = regrouped;
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| !is_after(ready, &sequence))
        {
            self.status.report(cx.app_mut(), WidgetState::Ready);
        }
    }

    fn apply_rpc_result(
        &mut self,
        result: WidgetRpcResult<OrderBookProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if self.status.is_failed(cx.app()) || self.awaiting_market {
            return;
        }
        match result {
            Ok(OrderBookResponse::StreamStarted) => {}
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    cx.app_mut(),
                    WidgetState::failed(Arc::new(OrderBookFailure::Rpc(error)), recovery),
                );
            }
        }
    }

    fn apply_rpc(
        &mut self,
        request_id: RequestId,
        result: WidgetRpcResult<OrderBookProtocol>,
        cx: &mut WidgetCx<'_>,
    ) {
        if let Some(PendingCommand::Start) = self.pending.take(request_id) {
            self.apply_rpc_result(result, cx);
        }
    }

    fn regroup(&mut self, cx: &mut WidgetCx<'_>) -> bool {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let previous_mid_price = self.grouped.as_ref().and_then(|book| book.mid_price);
        let progress = *cx.read(&self.depth_progress);
        let grouped = GroupedBook::build(
            snapshot,
            self.grouped.as_deref(),
            progress,
            self.config.price_multiplier,
            self.effective_window_size(),
        );
        let changed = self
            .grouped
            .as_ref()
            .is_none_or(|current| !current.has_same_current_state(&grouped));
        if !changed {
            return false;
        }
        self.previous_mid_price = previous_mid_price;
        self.grouped = Some(Arc::new(grouped));
        self.book_dirty = true;
        self.restart_depth_animation(cx);
        true
    }

    fn effective_window_size(&self) -> usize {
        self.config.window_size.min(self.drawable_rows.max(1))
    }

    fn book_depth(&self) -> Option<u16> {
        self.metadata.as_ref().map(|metadata| metadata.depth)
    }

    fn display_price_tick(&self) -> Option<Tick> {
        self.grouped
            .as_ref()
            .map(|book| book.price_tick)
            .or_else(|| {
                self.metadata
                    .as_ref()
                    .map(|metadata| metadata.precision.price_tick)
            })
    }

    fn price_step_label(&self, multiplier: u32) -> SmolStr {
        PRICE_MULTIPLIERS
            .iter()
            .position(|candidate| *candidate == multiplier)
            .map_or_else(
                || price_step_label(self.display_price_tick(), multiplier),
                |index| self.price_step_labels[index].clone(),
            )
    }

    fn set_metadata(&mut self, metadata: OrderBookMetadata, cx: &mut WidgetCx<'_>) -> bool {
        if self.metadata.as_ref() == Some(&metadata) {
            return false;
        }
        self.price_step_labels = price_step_labels(Some(metadata.precision.price_tick));
        self.metadata = Some(metadata);
        self.publish_unit_labels(cx);
        true
    }

    fn restart_depth_animation(&mut self, cx: &mut WidgetCx<'_>) {
        let reduce_motion = *cx.read(&cx.services().reduce_motion);
        let span = self
            .grouped
            .as_ref()
            .map_or(0.0, |book| book.animation_span());
        if reduce_motion || !depth_is_active(0.0, span) {
            cx.write(&self.depth_progress, 1.0);
            cx.write(&self.depth_span, 0.0);
            self.stop_depth_animation(cx.app_mut());
            return;
        }
        cx.write(&self.depth_span, span);
        cx.write(&self.depth_progress, 0.0);
        self.ensure_depth_ticker(cx);
    }

    fn ensure_depth_ticker(&mut self, cx: &mut WidgetCx<'_>) {
        let Some(scope) = self.body_root else {
            return;
        };
        let progress = *cx.read(&self.depth_progress);
        let span = *cx.read(&self.depth_span);
        if !depth_is_active(progress, span) {
            return;
        }
        if *cx.read(&cx.services().reduce_motion) {
            return;
        }
        if self
            .depth_ticker
            .as_ref()
            .is_some_and(|state| state.alive.get() && state.ticking.get())
        {
            return;
        }
        let state = Rc::new(DepthTickerState {
            progress: self.depth_progress,
            span: self.depth_span,
            ticking: Cell::new(true),
            alive: Cell::new(true),
            window: cx.window(),
            scope,
            timer: RefCell::new(None),
        });
        state.schedule(cx.app_mut());
        self.depth_ticker = Some(state);
    }

    fn stop_depth_animation(&mut self, app: &mut App) {
        let Some(state) = self.depth_ticker.take() else {
            return;
        };
        state.alive.set(false);
        state.ticking.set(false);
        if let Some(timer) = state.timer.borrow_mut().take() {
            let _ = app.cancel_timer(&timer);
        }
    }

    fn instrument_filters(&self) -> InstrumentFilters {
        InstrumentFilters {
            kind_filter: InstrumentKindFilter::All,
            status_filter: InstrumentStatusFilter::Any,
            required_capability: CapabilityId::OrderBook,
            entry_predicate: None,
            routes: self.directory.as_ref().map_or_else(
                || Arc::from(Vec::<LoadedScope>::new()),
                |directory| directory.loaded_scopes(),
            ),
        }
    }

    fn selector_host(&self, cx: &WidgetCx<'_>) -> SelectorHost {
        let host = SelectorHost::new(Rc::clone(cx.services_rc()));
        match &self.directory {
            Some(directory) => host.with_catalog(Some(directory.catalog())),
            None => host,
        }
    }

    fn header_state(&self, cx: &WidgetCx<'_>) -> HeaderState {
        HeaderState {
            selection: self.config.selection.clone(),
            language: *cx.read(&self.language),
            generation: self.seen_generation,
        }
    }

    fn header_right_state(&self, cx: &WidgetCx<'_>) -> HeaderRightState {
        HeaderRightState {
            language: *cx.read(&self.language),
            depth: self.book_depth(),
            price_tick: self.display_price_tick(),
            price_multiplier: self.config.price_multiplier,
            window_size: self.config.window_size,
            disabled: self.awaiting_market || self.snapshot.is_none(),
        }
    }

    fn build_header_left(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let host = self.selector_host(cx);
        let filters = self.instrument_filters();
        let selection = self.config.selection.clone();
        let requests = self.selection_requests;
        let button = InstrumentButton::new(
            cx.unique_key(INSTRUMENT_KEY_SUFFIX),
            selection,
            filters,
            "widget.order_book.select_instrument",
            move |selection: InstrumentSelection, app: &mut App, window: WindowId| {
                hand_over_selection(requests, selection, app, window);
            },
        );
        let window = cx.window();
        let control = button.build(&host, cx.app_mut(), window, parent)?;
        self.header_left = Some(HeaderLeft {
            control,
            parent,
            built: self.header_state(cx),
        });
        Ok(())
    }

    fn build_header_right(
        &mut self,
        parent: NodeId,
        cx: &mut WidgetCx<'_>,
    ) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .build(),
            )
            .build(cx.tree_mut(), parent)?;
        let theme = self.theme.clone();
        let translate = self.translate.clone();
        let window = cx.window();
        let disabled = self.awaiting_market || self.snapshot.is_none();
        let requests = self.header_requests;

        let price_items = PRICE_MULTIPLIERS
            .iter()
            .map(|multiplier| {
                SelectItem::new(
                    format_smolstr!("{multiplier}"),
                    self.price_step_label(*multiplier),
                )
            })
            .collect::<Vec<_>>();
        let price_step = self.build_select(
            cx,
            row,
            SelectSpec {
                key_suffix: PRICE_STEP_KEY_SUFFIX,
                aria_key: "widget.order_book.price_grouping",
                items: price_items,
                chosen: format_smolstr!("{}", self.config.price_multiplier),
                placeholder: self.price_step_label(self.config.price_multiplier),
                kind: HeaderSelect::PriceStep,
            },
            requests,
            disabled,
            theme.clone(),
            window,
        )?;

        let window_items = window_sizes(self.book_depth())
            .iter()
            .map(|rows| {
                SelectItem::new(
                    format_smolstr!("{rows}"),
                    t_smol!(translate, "widget.order_book.rows", count = rows),
                )
            })
            .collect::<Vec<_>>();
        let window_size = self.build_select(
            cx,
            row,
            SelectSpec {
                key_suffix: WINDOW_SIZE_KEY_SUFFIX,
                aria_key: "widget.order_book.visible_rows",
                items: window_items,
                chosen: format_smolstr!("{}", self.config.window_size),
                placeholder: t_smol!(
                    translate,
                    "widget.order_book.rows",
                    count = self.config.window_size
                ),
                kind: HeaderSelect::WindowSize,
            },
            requests,
            disabled,
            theme,
            window,
        )?;

        self.header_right = Some(HeaderRight {
            slot: parent,
            root: row,
            price_step,
            window_size,
            built: self.header_right_state(cx),
        });
        Ok(())
    }

    fn build_select(
        &self,
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        spec: SelectSpec,
        requests: Signal<Option<HeaderRequest>>,
        disabled: bool,
        theme: ThemeHandle,
        window: WindowId,
    ) -> Result<Select, EngineError> {
        let options = SelectOptions::default()
            .size(UnifiedSize::Small)
            .label(t_smol!(self.translate, spec.aria_key))
            .placeholder(spec.placeholder);
        let key = cx.unique_key(spec.key_suffix);
        let kind = spec.kind;
        let select =
            Select::new(cx.app_mut(), theme, key, options).on_change(move |app, chosen| {
                let Some(id) = chosen.first() else {
                    return;
                };
                let request = match kind {
                    HeaderSelect::PriceStep => id.parse::<u32>().ok().map(HeaderRequest::PriceStep),
                    HeaderSelect::WindowSize => {
                        id.parse::<usize>().ok().map(HeaderRequest::WindowSize)
                    }
                };
                if let Some(request) = request {
                    hand_over_header_request(requests, request, app, window);
                }
            });
        select
            .set_items(cx.app_mut(), spec.items)
            .unwrap_or_else(|error| {
                log::warn!(
                    "the order book's picker `{}` could not set its items: {error}",
                    spec.key_suffix
                );
            });
        select.set_chosen(cx.app_mut(), [spec.chosen.as_str()], false);
        select.build(cx.app_mut(), window, parent)?;
        let _ = select.set_disabled(cx.app_mut(), disabled);
        Ok(select)
    }

    fn sync_header(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        if let Some(header) = self.header_left.as_ref() {
            let moved = header.built.language != *cx.read(&self.language)
                || header.built.generation != self.seen_generation
                || header.built.selection.as_ref() != self.config.selection.as_ref();
            if moved {
                let parent = header.parent;
                if cx.tree().get(parent).is_err() {
                    self.release_header_left(cx, false);
                } else {
                    self.release_header_left(cx, true);
                    self.build_header_left(parent, cx)?;
                }
            }
        }
        if let Some(header) = self.header_right.as_ref() {
            let moved = header.built != self.header_right_state(cx);
            if moved {
                let slot = header.slot;
                if cx.tree().get(slot).is_err() {
                    self.release_header_right(cx, false);
                } else {
                    self.release_header_right(cx, true);
                    self.build_header_right(slot, cx)?;
                }
            }
        }
        Ok(())
    }

    fn release_header_left(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header_left.take() else {
            return;
        };
        let node = header.control.node();
        if remove_node && cx.tree().get(node).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, node);
        }
        let _ = header.control.release(cx.app_mut());
    }

    fn release_header_right(&mut self, cx: &mut WidgetCx<'_>, remove_node: bool) {
        let Some(header) = self.header_right.take() else {
            return;
        };
        if remove_node && cx.tree().get(header.root).is_ok() {
            let window = cx.window();
            let _ = cx.app_mut().remove_node(window, header.root);
        }
        header.price_step.release(cx.app_mut());
        header.window_size.release(cx.app_mut());
    }

    fn select_selection(&mut self, selection: InstrumentSelection, cx: &mut WidgetCx<'_>) -> bool {
        if self.config.selection.as_ref() == Some(&selection) && !self.status.is_failed(cx.app()) {
            return false;
        }
        self.config.selection = Some(selection);
        self.config_change.publish(cx.app_mut());
        self.clear_book(cx);
        if self.awaiting_market {
            self.load(cx);
        } else {
            self.start(false, false, cx);
        }
        true
    }

    fn select_price_multiplier(&mut self, multiplier: u32, cx: &mut WidgetCx<'_>) {
        if self.config.price_multiplier == multiplier || !PRICE_MULTIPLIERS.contains(&multiplier) {
            return;
        }
        self.config.price_multiplier = multiplier;
        self.config_change.publish(cx.app_mut());
        self.restream(cx);
        self.regroup(cx);
    }

    fn select_window_size(&mut self, window_size: usize, cx: &mut WidgetCx<'_>) {
        if self.config.window_size == window_size
            || !window_sizes(self.book_depth()).contains(&window_size)
        {
            return;
        }
        self.config.window_size = window_size;
        self.config_change.publish(cx.app_mut());
        self.restream(cx);
        self.regroup(cx);
    }

    fn restream(&mut self, cx: &mut WidgetCx<'_>) {
        if self.status.is_failed(cx.app()) {
            return;
        }
        let Some(key) = self.current_key() else {
            return;
        };
        if let Some(binding) = self.binding.as_mut() {
            let request_id = binding.call(cx, OrderBookRequest::Load(key));
            self.pending.track(request_id, PendingCommand::Start);
        }
    }
}

#[derive(Clone, Copy)]
enum HeaderSelect {
    PriceStep,
    WindowSize,
}

struct SelectSpec {
    key_suffix: &'static str,
    aria_key: &'static str,
    items: Vec<SelectItem>,
    chosen: SmolStr,
    placeholder: SmolStr,
    kind: HeaderSelect,
}

fn hand_over_selection(
    requests: Signal<Option<InstrumentSelection>>,
    selection: InstrumentSelection,
    app: &mut App,
    window: WindowId,
) {
    app.write(&requests, Some(selection));
    let _ = app.request_repaint(window);
}

fn hand_over_header_request(
    requests: Signal<Option<HeaderRequest>>,
    request: HeaderRequest,
    app: &mut App,
    window: WindowId,
) {
    app.write(&requests, Some(request));
    let _ = app.request_repaint(window);
}

fn aligned_text(size: LogicalPx) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.alignment = TextAlign::Right;
    style
}

fn bold_text(size: LogicalPx) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.weight = 700;
    style
}

fn build_row(
    tree: &mut ui_core::node::tree::NodeTree,
    parent: NodeId,
    row: usize,
    side: RowSide,
    book: Signal<PublishedBook>,
    theme: ThemeHandle,
    font_size: LogicalPx,
) -> Result<(), EngineError> {
    let container = NodeBuilder::boxed()
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .size_full()
                .min_width_0()
                .build(),
        )
        .build(tree, parent)?;
    for column in 0..3usize {
        let mut builder = NodeBuilder::text(SmolStr::default(), aligned_text(font_size))
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .min_width_0()
                    .height_full()
                    .build(),
            )
            .bind_text(move |read| {
                let book = read.read(&book);
                book.level(side, row)
                    .map(|level| match column {
                        0 => level.price_text.clone(),
                        1 => level.quantity_text.clone(),
                        _ => level.cumulative_quantity_text.clone(),
                    })
                    .unwrap_or_default()
            });
        builder = if column == 0 {
            let theme = theme.clone();
            let role = side.price_role();
            builder.bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, role)),
                ..StylePatch::default()
            })
        } else {
            let theme = theme.clone();
            builder.bind_style(move |read| StylePatch {
                foreground: Some(theme.role(read, Role::FgDefault)),
                ..StylePatch::default()
            })
        };
        builder.build(tree, container)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
