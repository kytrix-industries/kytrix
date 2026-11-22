//! The CMC fear and greed pane: the account it is scoped to, the reading it draws,
//! and the controls it offers.
//!
//! # What this pane is
//!
//! The index has a single current value and six history points, and the service
//! reads the whole snapshot every 60 seconds — so the pane never asks for a part of
//! the answer, it asks for *the* answer, and everything on screen is a view of the
//! one snapshot it holds. The gauge and the rows are therefore not scopes of their
//! own: a snapshot that draws something else redraws them, and a snapshot that would
//! draw the same pixels nothing at all. The collection time is not one of those
//! pixels, so a poll of an index that has not moved is not a redraw.
//!
//! # What it does with a failure
//!
//! A refused read is a failure the user sees: the pane reports it through the
//! framework, which puts a banner over the reading that is still on screen. The
//! service ends its schedule at that point, so the framework's recovery is what
//! starts it again — a service that is briefly unreachable does not turn into a
//! request every minute until someone notices.
//!
//! # Why a reply is matched by request identity
//!
//! One binding carries one command at a time: a second `call` replaces and aborts the
//! first. Abort only stops an answer that has not been handed to the widget yet, so an
//! answer can already be queued when the replacement happens — and the one place they
//! are delivered, `sync`, replaces a command before it drains exactly when the account
//! changes. The pane therefore records the framework `RequestId` every command was
//! issued under ([`Pending`]) and applies an answer only to the identity it belongs
//! to. An identity the table does not know answers a command this pane replaced or
//! gave up on, and is dropped and counted instead of being applied to the command now
//! waiting: the mis-attribution that would let the failure of a signed-out account's
//! `Load` fail the pane the next account is reading (AGENTS §4.4, §8.2).
//!
//! # Who may see it
//!
//! The service serves this snapshot over an authenticated connection, so a pane
//! with no account behind it fetches nothing and says so. That is a *waiting* state
//! rather than a failure: nothing has gone wrong, the pane is just not for an
//! anonymous reader.
//!
//! The account is read from the injected settings snapshot on every frame rather
//! than followed through an event subscription (AGENTS §8.1): signing in or out is a
//! settings write, and the framework's surface root reads the same snapshot, so the
//! frame that presents the new account is the frame this pane's `sync` sees it in.
//! Nothing is retried on a timer either.
//!
//! # An empty snapshot is not an error
//!
//! Before its collector has run, the service answers with a snapshot whose
//! collection time is zero. That is a normal cold start rather than a broken
//! service, so the pane draws 「暂无数据」 in the body and reports readiness: a
//! spinner over a healthy empty answer would say the pane is still working, and a
//! failure would ask the user to recover from something that has not failed.
//!
//! # The body is built once and kept up to date
//!
//! [`WidgetView::body`] builds the tree once: the gauge's canvas, two section
//! headings with their info notes, and two groups of rows. Everything dynamic is a
//! `bind_text` / `bind_style` closure over the pane's own signals (the snapshot, the
//! language, the theme), so a new reading redraws the rows without rebuilding them.
//! Two things are structural and are therefore replaced rather than re-bound:
//!
//! - **the empty stand-in** (`common.no_data`) is built and taken down as the
//!   snapshot moves between collected and uncollected, and rebuilt when the language
//!   moves (its copy is a build-time `SmolStr`, AGENTS §15.1);
//! - **the rows** change shape when a reading disappears or appears — a missing row
//!   has no level dot and no level name — so the row nodes are rebuilt when the set
//!   of present readings moves. A changed score or a language switch is not a
//!   rebuild: those are bindings.
//!
//! # The header control is a signal, not a re-entry
//!
//! The refresh button's handler runs with an application context and no widget
//! context, so it cannot call into the pane: it writes the pane's request signal and
//! asks for a frame, and the next `sync` performs the refresh (AGENTS §4.2). The
//! button is drawn disabled for the states that cannot refresh, and the action entry
//! guards itself as well — a disabled kit control only draws that state, it does not
//! stop an event.
//!
//! # The canvas is drawn by the gauge module
//!
//! The reading is self-drawn ([`crate::gauge`]): a canvas node whose frame is
//! prepared from the node's measured box and rebuilt only when the box, the reading,
//! the language or the theme colours move. The canvas content does not follow the
//! engine's group opacity (spec-delta E13 R1); this pane has no group fade.
//!
//! # Time zones
//!
//! A row's date stays the UTC calendar day the service generated rather than becoming
//! a calendar value, so the display time zone does not move it: reading it in another
//! zone would name a day the service did not. The pane therefore reads no time zone
//! at all, and the test `the_display_time_zone_should_not_move_a_service_date` pins
//! that (there is nothing here for `cx.data_time_zone()` to format).

use std::sync::Arc;

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        icon::{IconError, IconKind, IconSpec},
        size::{UnifiedSizable, UnifiedSize},
        tooltip::{Tooltip, TooltipContent, TooltipOptions},
    },
    style_builder::{PaintBuilder, StyleBuilder},
    theme::{handle::ThemeHandle, role::Role},
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use kytrix_service_provider::service::engine::KytrixServiceError;
use serde::{Deserialize, Serialize};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal},
    paint::style::StylePatch,
    text::{TextAlign, TextStyle},
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

/// The dash a row without a reading draws.
///
/// It is not `common.no_data` and not a waiting state: the row's point does not
/// exist, while the pane may well have everything else to show.
const MISSING: &str = "—";

/// The diameter of the dot that marks a row's level.
///
/// The dot carries the level's colour; the level's name beside it stays in the
/// default text colour, because the scale's yellow is a fill and not readable as a
/// label on a light surface.
const LEVEL_MARKER_SIZE: f32 = 8.0;

/// The width of a row's value column, so the level names beside it line up.
///
/// The values are `0..=100`, so the column is wide enough for three digits. It is
/// fixed rather than a minimum: a one- or two-digit value must not move the level's
/// name, which is what makes the level names read as one column down the card.
const VALUE_COLUMN_WIDTH: f32 = 28.0;

/// The weight a value and a section heading are drawn at.
const SEMIBOLD: u16 = 600;

/// The text size of a row's label and value.
///
/// The theme has no body/typography token yet (AGENTS §15.4), so the pane takes the
/// kit size ladder: the reference's `text_sm` (14 px) is the ladder's regular size.
const ROW_TEXT_SIZE: UnifiedSize = UnifiedSize::Medium;

/// The text size of a level's name and of a section heading.
///
/// The reference drew both at `text_xs` (12 px), which is the ladder's compact size.
const SMALL_TEXT_SIZE: UnifiedSize = UnifiedSize::Small;

/// The size step of the pane's two decorative icons: the section headings' info icon
/// and the header's refresh icon.
///
/// The reference drew both with `Icon::size_3()` (12 px), which is *not* the icon step
/// of the control they sit in: a `Small` heading or button draws its icon at 8 px. The
/// 12 px icon is the ladder's medium step, and giving it explicitly is what keeps the
/// two icons the size the reference drew them (kit P2-9 `Button::icon_size`).
const ICON_SIZE: UnifiedSize = UnifiedSize::Medium;

/// How many rows the pane draws.
const ROW_COUNT: usize = 6;

/// How many of those rows are the history rows (the rest are the yearly extremes).
const HISTORY_ROWS: usize = 4;

/// How many section headings the card draws.
const SECTION_COUNT: usize = 2;

/// The debug id of the pane's body root.
const BODY_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_BODY";

/// The debug id of the stand-in an uncollected answer draws.
const EMPTY_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_EMPTY";

/// The debug id of the header's refresh control.
const REFRESH_SELECTOR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_REFRESH";

/// The widget-local name of the header control's kit key.
///
/// A kit key is **application-level** — the button derives its click action from it
/// (`"<key>.click"`) — so the pane does not spell it out: [`WidgetCx::unique_key`]
/// stamps this suffix with the pane's instance scope, and two panes cannot share a
/// handler (AGENTS §9, hard constraint 11).
const HEADER_KEY_SUFFIX: &str = "coin_market_cap_fear_and_greed_index.header.refresh";

/// The debug selectors of the six rows.
const ROW_YESTERDAY: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_YESTERDAY";
const ROW_LAST_WEEK: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_LAST_WEEK";
const ROW_LAST_MONTH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_LAST_MONTH";
const ROW_LAST_YEAR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_LAST_YEAR";
const ROW_YEARLY_HIGH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_YEARLY_HIGH";
const ROW_YEARLY_LOW: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_ROW_YEARLY_LOW";

/// The debug selectors of the dash each row draws when it has no reading.
///
/// A painted frame's text is not readable from a test, so the selector a value
/// carries is what says a dash was drawn — and which row drew it. Every row has its
/// own, so a test can tell an absent row that still drew its dash from one that
/// silently stopped drawing it.
const MISSING_YESTERDAY: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_YESTERDAY";
const MISSING_LAST_WEEK: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_LAST_WEEK";
const MISSING_LAST_MONTH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_LAST_MONTH";
const MISSING_LAST_YEAR: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_LAST_YEAR";
const MISSING_YEARLY_HIGH: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_YEARLY_HIGH";
const MISSING_YEARLY_LOW: &str = "TEST_COIN_MARKET_CAP_FEAR_AND_GREED_INDEX_MISSING_YEARLY_LOW";

/// The debug selectors of the dots that mark a row's level.
///
/// A painted dot is not readable from a test, so the selector says whether the
/// marker was drawn and which row drew it.
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

/// One row of the card, and the catalogue copy that names it.
#[derive(Clone, Copy, Debug)]
struct RowDescriptor {
    /// The row's debug id.
    row: &'static str,
    /// The debug id of the dot-and-name group, drawn only when the row has a reading.
    marker: &'static str,
    /// The debug id of the value node when the row has no reading.
    missing: &'static str,
    /// The catalogue key the row's label is read from.
    label: &'static str,
    /// Whether the label names the day the reading happened on (the yearly rows do).
    dated: bool,
}

/// The six rows, in the order the card draws them.
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

/// One section heading, and the note its info icon explains.
#[derive(Clone, Copy, Debug)]
struct SectionDescriptor {
    /// The catalogue key of the heading.
    title: &'static str,
    /// The catalogue key of the note the info icon shows.
    info: &'static str,
    /// The widget-local name of the tooltip's kit key.
    key: &'static str,
}

/// The card's two section headings.
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

/// The pane has nothing to persist, so its config carries the schema version alone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoinMarketCapFearAndGreedIndexConfig;

impl WidgetConfig for CoinMarketCapFearAndGreedIndexConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// A failure a pane reports that is not already a stream error.
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

/// Which of the six rows currently carry a reading.
///
/// The row *nodes* differ by it — a row without a reading has no level dot and no
/// level name — so a change is what rebuilds them. A changed score is not: that is a
/// binding on nodes that stay where they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowsKey([bool; ROW_COUNT]);

impl RowsKey {
    fn of(snapshot: &CoinMarketCapFearAndGreedIndexSnapshot) -> Self {
        Self(std::array::from_fn(|index| {
            row_at(snapshot, index).is_some()
        }))
    }
}

/// The header's refresh control, kept so its state can be synchronized and it can be
/// released.
struct HeaderControl {
    button: Button,
    /// The wrapper node the pane's header group holds.
    node: NodeId,
    /// The action identity the button registered (`"<key>.click"`), so a test can
    /// dispatch the click the engine would.
    #[cfg(test)]
    action: ui_core::event::Action,
    /// The language the control's accessible name was built in.
    language: LanguageCode,
    /// Whether the control is currently drawn disabled.
    disabled: bool,
}

/// The card the body draws for a collected answer.
struct Card {
    /// The card container.
    root: NodeId,
    /// The container the four history rows live in. It outlives a row rebuild, so
    /// the card's child order (and therefore the layout order) is fixed.
    history_rows: NodeId,
    /// The container the two yearly rows live in.
    extreme_rows: NodeId,
}

/// The commands one pane can have in flight on its one binding.
///
/// The identity [`WidgetBinding::call`] returned is what says whose answer arrived: an
/// answer this table does not know belongs to a command the pane replaced or gave up
/// on (see the module documentation). The kind recorded is what tells the pane whether
/// the answer ends the window a click opened — only a refresh does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingCommand {
    /// The read that starts or restarts the stream: `Load` or `Retry`.
    Load,
    /// The read the refresh control brought forward.
    Refresh,
}

/// One fear and greed pane: the account it follows, the reading it draws, and its
/// refresh control.
pub struct CoinMarketCapFearAndGreedIndexWidget {
    /// The executor bridge, absent on a host that installed no Kytrix service.
    binding: Option<WidgetBinding<CoinMarketCapFearAndGreedIndexProtocol>>,
    /// The command in flight, keyed by the identity the binding returned.
    pending: Pending<PendingCommand>,
    status: WidgetStatus,
    recovery: WidgetAction,
    gauge: Gauge,
    /// The answer the body is drawn from, or nothing before the first one arrives.
    snapshot: Option<Arc<CoinMarketCapFearAndGreedIndexSnapshot>>,
    /// The reading the row bindings read. A row closure has no application context,
    /// so the snapshot is a signal the nodes watch rather than a field they borrow.
    items: Signal<Option<Arc<CoinMarketCapFearAndGreedIndexSnapshot>>>,
    /// The selected language, read where text and static copy are built.
    language: Signal<LanguageCode>,
    /// Whether a refresh this pane sent is still waiting for the reading it asked
    /// for. It closes the window between the click and the answer, during which the
    /// service would refuse a second command.
    refresh_pending: bool,
    /// The account this pane is for, or `None` while nobody is signed in.
    account_id: Option<SmolStr>,
    /// Click requests from the header control: a button handler owns no widget
    /// context, so it asks the next `sync` for the refresh instead.
    refresh_requests: Signal<u64>,
    seen_refresh_requests: u64,
    /// The header control, once the pane has built its header row.
    header: Option<HeaderControl>,
    /// The root of the body this pane built.
    body_root: Option<NodeId>,
    /// The card, once the body has built one.
    card: Option<Card>,
    /// Which rows the card currently draws (see [`RowsKey`]).
    rows_built: Option<RowsKey>,
    /// The stand-in an uncollected answer draws, with the language it was built in.
    empty: Option<NodeId>,
    empty_language: Option<LanguageCode>,
    /// The info notes beside the two section headings.
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
        // A retry keeps the reading the user is still looking at — blanking it to
        // reconnect would flash a screen of data away — while an explicit load
        // starts over, because the pane cannot vouch for what it was showing.
        let retry = self.status.is_failed(cx.app());
        self.account_id = cx.account();
        if let Err(error) = self.start(retry, cx) {
            log::error!("the fear and greed pane could not start: {error}");
        }
    }

    /// A pane is content once an answer has arrived for the account it follows.
    ///
    /// Having the last answer is not the same thing: it may have been served to an
    /// account that has since signed out.
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
        // The settings are the pane's scope: signing in or out is another read, and
        // a settings write reaches this hook on the frame it schedules.
        let account = cx.account();
        if account != self.account_id {
            self.account_id = account;
            self.start(false, cx)?;
        }

        // The binding delivers messages instead of calling back into the widget; this
        // is the one place they are applied, where the widget has a context again.
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

        // A click on the header control is applied here rather than in its handler,
        // which owns no widget context (see the module documentation).
        let requests = *cx.read(&self.refresh_requests);
        if requests != self.seen_refresh_requests {
            self.seen_refresh_requests = requests;
            self.refresh(cx)?;
        }

        self.sync_body_content(cx)?;
        self.sync_gauge(cx)?;
        self.sync_tooltips(cx)?;
        self.sync_header(cx)?;

        // The body is built by the framework from the answer `has_content` gives, and
        // it asks that question before this hook runs: content that first appeared
        // here is one frame away, so ask for that frame.
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
        // The pane is building its row; the control this widget built into the
        // previous one went with it. Only the component is the widget's to take down.
        self.release_header(cx, true);
        self.build_header(parent, cx)
    }

    fn release(&mut self, cx: &mut WidgetCx<'_>) {
        // Dropping the bridge aborts its forwarding tasks and cancels the service.
        self.binding = None;
        // The header's nodes belong to the pane, which removes its row: only the
        // component's signal scope and action handler are this widget's to release.
        self.release_header(cx, false);
        self.release_tooltips(cx);
        // The meshes live in the process-wide geometry store, so nothing else would
        // give them back.
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
    /// The seam the framework and the tests build a pane through.
    ///
    /// The pane has no opinion about where its readings come from, so the source is
    /// an argument: the production constructor answers it with the injected Kytrix
    /// service connection, and a test with a fake source.
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
        // The pane records the account it was built for, but it starts nothing: the
        // framework calls `load` once construction is done.
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

    /// A pane over a source the caller answers, for the crate's own tests.
    #[cfg(test)]
    fn new_with_source<S: CoinMarketCapFearAndGreedIndexSource>(
        source: S,
        status: WidgetStatus,
        recovery: WidgetAction,
        cx: &mut WidgetCx<'_>,
    ) -> Self {
        Self::assemble(Some(source), status, recovery, cx)
    }

    /// The account this pane would accept an answer from.
    fn current_key(&self) -> Option<CoinMarketCapFearAndGreedIndexStreamKey> {
        self.account_id
            .clone()
            .map(|account_id| CoinMarketCapFearAndGreedIndexStreamKey { account_id })
    }

    /// Whether the refresh control has nothing to do.
    ///
    /// It is live exactly when the pane is ready and no refresh of its own is still
    /// waiting: a pane that is connecting has nothing to bring forward, a failed one
    /// is recovered through the framework's own control above the reading, and a
    /// pending refresh would be refused by the service as busy.
    fn refresh_is_disabled(&self, app: &App) -> bool {
        self.refresh_pending || self.status.state(app) != WidgetState::Ready
    }

    /// Starts reading, or restarts for an account the user is retrying.
    fn start(&mut self, retry: bool, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        // The command that was in flight belongs to the stream this call is
        // replacing, so its answer is not something to wait for any more. Forgetting
        // it here is what keeps that answer from being applied to the command about
        // to be issued: a `Load` for the account that just signed out fails while the
        // pane is reading for the one that replaced it.
        self.refresh_pending = false;
        self.pending.clear();
        let Some(key) = self.current_key() else {
            // There is nothing to read for a reader the service does not know, and
            // nothing has gone wrong, so the pane says what it is waiting for.
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
            // A host without the service connection can answer nothing: the pane is
            // for a reader whose connection is missing, and that is a failure.
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

    /// Drops the reading the pane can no longer vouch for.
    fn clear_snapshot(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        self.snapshot = None;
        cx.write(&self.items, None);
        cx.request_frame()?;

        Ok(())
    }

    /// Reads the account's answer again without waiting for the next interval.
    ///
    /// Only a ready pane gets here, and the control that calls it is drawn disabled
    /// for the other states — the handler still fires (a kit control's `disabled`
    /// only draws the state), so this guard is the one that stops it.
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

    /// Reports one failure, with the recovery the framework offered this widget.
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

    /// Applies the answer to a load, a retry or a refresh.
    ///
    /// The answer is matched to the command it answers by the framework identity the
    /// binding returned when that command was issued. An identity the table does not
    /// know answers a command this pane replaced or gave up on — the account changed,
    /// the pane signed out — so it is dropped and counted rather than applied to the
    /// command now waiting (see the module documentation).
    ///
    /// A failure is terminal, so an answer that arrives after one is dropped: the
    /// recovery reports `Connecting` before it asks for anything, which is what
    /// makes the next answer one this pane still wants.
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
            // An accepted command. The reading it asked for arrives on the stream, and
            // that is what ends the wait — a refresh's wait included.
            return;
        };
        if command == PendingCommand::Refresh {
            // A refused refresh is not one to keep waiting for: the window the click
            // opened ends with the answer to the command that opened it.
            self.refresh_pending = false;
        }
        self.report_failure(CoinMarketCapFearAndGreedIndexFailure::Rpc(error), cx);
    }

    /// Applies the failure the stream reported.
    ///
    /// Returns whether anything a frame draws changed; a failure is drawn by the
    /// framework around the body, so reporting it is enough.
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

    /// Applies the snapshot the stream published.
    ///
    /// Returns whether anything a frame draws changed: the body is kept, and an
    /// answer that folds into the same pixels is not worth a frame. The comparison is
    /// the snapshot's [`same_display`](CoinMarketCapFearAndGreedIndexSnapshot::same_display)
    /// rather than the whole answer, because the collection time is never drawn.
    ///
    /// Two things still count as a change even when a frame would draw the same: an
    /// answer ends a refresh the header is waiting for (the control re-enables), and
    /// a pane a retry left in `Connecting` has to report `Ready` on the answer even
    /// though the reading it kept draws the same. Both of those are the pane's own
    /// state rather than the drawing, so they are checked before the snapshot is
    /// discarded as a no-op.
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
            // The answer neither moves the drawing nor ends anything the pane is
            // waiting for, so the allocation it already holds is the one the body
            // keeps drawing from.
            return false;
        }
        self.snapshot = Some(snapshot.clone());
        cx.write(&self.items, self.snapshot.clone());
        self.refresh_pending = false;
        self.status.report(cx.app_mut(), WidgetState::Ready);

        true
    }

    /// Keeps the body in step with the reading: the card for a collected answer, the
    /// `common.no_data` stand-in for an uncollected one.
    fn sync_body_content(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(root) = self.body_root else {
            return Ok(());
        };
        let Some(snapshot) = self.snapshot.clone() else {
            // The framework draws a placeholder over the body while there is no
            // content; the nodes stay as they are for the answer that replaces it.
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

    /// Builds the card: the gauge, the two section headings with their notes, and the
    /// two row groups.
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

    /// Builds one section heading over the container its rows will live in.
    ///
    /// The container is the card's fixed child: a row rebuild replaces the nodes
    /// inside it, never the container, so the card's order — gauge, heading, history,
    /// heading, yearly — cannot depend on a rebuild.
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

        // The note is a kit tooltip on a decorative icon, exactly as the reference
        // drew it: the icon is the trigger, so the heading itself stays a label.
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

    /// Builds every row of one group into its container.
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

    /// Builds one row of the card: its label, the level it sat in, and its value.
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

        // What the row is. A yearly row names the day its reading happened on, which
        // is part of the sentence rather than a second line: the binding resolves the
        // whole label, so a missing day is a dash inside one catalogue message.
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
            // The dash is the whole value of a row without a reading: the selector is
            // what tells a test (and a debugging session) which row drew it.
            value = value.id(descriptor.missing);
        }
        value.build(cx.tree_mut(), right)?;

        Ok(())
    }

    /// Builds the dot and the level's name a row with a reading carries.
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

        // The dot's colour is a binding over the reading and the theme; the static
        // paint only has to give the first style pass something valid to draw.
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

    /// Rebuilds the row nodes when the set of present readings changed.
    ///
    /// The row containers are the card's fixed children and stay: the nodes inside
    /// them are replaced, so the card's order — gauge, heading, history, heading,
    /// yearly — never depends on a rebuild.
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
        // The nodes that were on screen are gone and the ones built in their place
        // have not been drawn yet; nothing else asks for that frame.
        cx.request_frame()?;

        Ok(())
    }

    /// Takes the card down, releasing the components it owns first.
    fn remove_card(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let Some(card) = self.card.take() else {
            return Ok(());
        };
        self.rows_built = None;
        self.release_tooltips(cx);
        self.gauge.release(cx.app_mut());
        remove_own_node(cx, card.root)
    }

    /// Shows `common.no_data` over an uncollected answer.
    ///
    /// The stand-in is the widget's, not the framework's: an uncollected answer is
    /// *content*, so this is the body saying there is nothing in it rather than a
    /// placeholder saying there is no body. Its copy is static, so it is built for the
    /// language showing now and rebuilt when that moves (AGENTS §15.1).
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

    /// Draws the reading the pane now holds.
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

    /// Keeps the two info notes in the language showing now, and ticks them.
    ///
    /// A tooltip's content is a resolved sentence, so a language switch replaces it
    /// (the component closes the old panel and opens the new one). Its `sync` drives
    /// the hover / delay state machine and the overlay's own frame work.
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

    /// Releases the two tooltips (their overlays and their signal scopes).
    fn release_tooltips(&mut self, cx: &mut WidgetCx<'_>) {
        for tooltip in self.tooltips.iter_mut() {
            if let Some(tooltip) = tooltip.take() {
                tooltip.release(cx.app_mut());
            }
        }
        self.tooltips_language = None;
    }

    /// Builds the pane's refresh control into the header group's container.
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
            // A disabled button is only *drawn* disabled, so the handler stays
            // attached and the guard that matters is the one in `refresh`, at the
            // action's entry.
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

    /// Keeps the header control in step with the state it is drawn for.
    ///
    /// The button's accessible name is a resolved sentence, so a language switch
    /// rebuilds the control; a state change only toggles the disabled drawing.
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
        // The pane may have rebuilt its header row since: the group is not this
        // widget's node, so a stale control already went with the row.
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

    /// Removes the control's wrapper (when the widget owns the teardown) and releases
    /// the component's scope and action handler.
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

/// The text style one line of this pane's text is drawn with.
fn text_style(size: f32, weight: u16, alignment: Option<TextAlign>) -> TextStyle {
    let mut style = TextStyle::new(size);
    style.weight = weight;
    if let Some(alignment) = alignment {
        style.alignment = alignment;
    }

    style
}

/// The row whose reading `index` names, if the answer carries it.
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

/// The label of one row, in the language showing on the frame that draws it.
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

/// The level's name beside a row's dot, or nothing for a row without a reading.
fn level_name(row: Option<&CoinMarketCapFearAndGreedIndexRow>, translate: &Translate) -> SmolStr {
    row.map_or_else(SmolStr::default, |row| {
        t_smol!(translate, Level::of(row.score).key())
    })
}

/// The value of a row, or the dash that says there is none.
fn value_text(row: Option<&CoinMarketCapFearAndGreedIndexRow>) -> SmolStr {
    row.map_or_else(
        || SmolStr::new_static(MISSING),
        |row| format_smolstr!("{}", row.score),
    )
}

/// The day a row happened on, or the dash that says there is no reading.
fn date_text(row: Option<&CoinMarketCapFearAndGreedIndexRow>) -> SmolStr {
    row.map_or_else(|| SmolStr::new_static(MISSING), |row| row.date.clone())
}

/// Removes every child of `parent`, keeping `parent` itself in place.
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

/// Removes a node this pane built, if it is still in the tree.
///
/// Every block this pane replaces goes through this step: the nodes first, then the
/// components that own scopes on them. A node that is already gone is not a failure —
/// the framework removes the surface's subtree after `release`, and a header row the
/// pane rebuilt took the control with it — so asking the engine to remove it again
/// would fail the rebuild (`StaleNode`) and leave the pane without the replacement.
fn remove_own_node(cx: &mut WidgetCx<'_>, node: NodeId) -> Result<(), EngineError> {
    if cx.tree().get(node).is_err() {
        return Ok(());
    }
    let window = cx.window();
    cx.app_mut().remove_node(window, node).map(|_| ())
}

/// The engine error an icon mount reports.
///
/// `IconSpec::build_themed` reports a refused size, an unknown kind and a wrong node
/// kind beside the two engine errors; this pane passes a size from the theme's ladder
/// and a constant kind, so those cannot happen, and an impossible one is reported as
/// a stale node of the row the icon was being mounted in rather than panicking.
fn icon_error(error: IconError, parent: NodeId) -> EngineError {
    match error {
        IconError::Decode(error) | IconError::Node(error) => error,
        IconError::UnknownWindow { window } => EngineError::UnknownWindow { window },
        IconError::InvalidSize { .. }
        | IconError::UnknownKind { .. }
        | IconError::NotImageNode { .. } => EngineError::StaleNode { node: parent },
    }
}

#[cfg(test)]
mod tests;
