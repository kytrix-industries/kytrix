//! The indices pane: which indices it follows, and how it draws them.
//!
//! # What a pane is scoped to
//!
//! Two things: the account the service serves it for, and the indices the user
//! selected. Those two are the *scope*, and the scope is the stream key as well — so
//! the pane never has to decide whether a quote belongs to the selection on screen:
//! a message that arrives for a scope it has left is a message the framework does
//! not deliver, and one that arrives by hand is refused by comparing the key.
//!
//! # What it reports, and what it does not
//!
//! It reports through the framework's state cell and nothing else: a failure is a
//! failure, and the framework decides where that is shown and what the recovery
//! looks like. Two states are *not* failures: nobody signed in, and nothing selected.
//! Both are neutral text for a pane that has nothing to show yet, which is what
//! `WidgetState::waiting` is for.
//!
//! # Where the flash lives
//!
//! A cell flashes when its index moves. The flash is driven by the pane rather than
//! by an animation element, because the body is cached: a cached subtree is only
//! repainted when its own view asks for a frame, so the pane advances the flash and
//! requests the next frame while one is still visible. The platform's reduce-motion
//! setting turns the flash off entirely — it is movement, and a reader who asked for
//! less of it does not want a market's worth of it.
//!
//! # Two ways of drawing one answer
//!
//! The grid and the table are two views of the same rows, not two scopes: switching
//! between them re-draws what the pane already holds and re-reads nothing.

use std::{
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use ahash::{AHashSet, HashMap, HashMapExt};
use app_config::{
    settings::Config,
    widget::{WidgetConfig, WidgetConfigSchemaVersion},
};
use app_config_provider::manager::ConfigManager;
use base_ui::{
    button::Button,
    empty_state::EmptyState,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    join::Join,
    scroll_bar::Scrollbar,
    size::UnifiedSizable,
    style::theme::ThemeExt,
    table::{Table, TableDataSet},
    tooltip::TooltipExt,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, ElementId, Empty, Entity, FontWeight, Hsla,
    InteractiveElement, IntoElement, Length, ParentElement, Pixels, Point, Render, RenderOnce,
    ScrollHandle, SharedString, Size, StatefulInteractiveElement, Styled, Task, Window,
    base::{Align, Disableable, ScrollbarMode, Selectable},
    canvas, div, point,
    prelude::FluentBuilder,
    px, relative, size,
};
use i18n::{language::codes::LanguageCode, t};
use kytrix_proto::types as proto_types;
use kytrix_service_provider::service::{
    connector::KytrixServiceConnector,
    engine::{IndexTopic, KytrixServiceError},
};
use serde::{Deserialize, Deserializer, Serialize};
use smol_str::SmolStr;
use tokio::runtime::Handle;
use widget::{
    binding::{WidgetBinding, WidgetRpcResult},
    category::WidgetCategory,
    config::WidgetConfigChange,
    error::LocalizedError,
    identity::{WidgetId, WidgetInstanceId},
    rpc::RpcCallError,
    status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    view::{WidgetHeader, WidgetView},
};
use widget_tokio_runtime::global;

use super::{
    dialog::IndicesSelector,
    model::{
        IndexDirection, IndexQuote, IndicesDataError, SelectedIndex, format_percent, format_price,
        format_signed_cents, normalize_selection,
    },
    service::{
        IndicesProtocol, IndicesRequest, IndicesScope, IndicesService, IndicesServiceError,
        IndicesSource, IndicesStatus, IndicesStreamError, IndicesStreamMessage,
    },
};

/// How long a flash stays on the cell of an index that moved.
const FLASH_DURATION: Duration = Duration::from_millis(350);
/// The point of a flash at which it is at its strongest.
const FLASH_PEAK: Duration = Duration::from_millis(100);
/// How tall the flash bar across the top of a grid cell is.
const GRID_FLASH_HEIGHT: f32 = 6.0;
/// The narrowest and shortest a grid cell may be, which is also what the persisted
/// configuration is allowed to name.
const GRID_MIN_CELL_WIDTH: u16 = 160;
const GRID_MIN_CELL_HEIGHT: u16 = 88;
/// The room the pane keeps for the grid's own scrollbars.
const SCROLLBAR_THICKNESS: f32 = 8.0;
/// The table's columns, in header order.
const COLUMN_COUNT: usize = 4;
/// Column widths in header order, as a share of the pane.
const COLUMN_WIDTHS: [Length; COLUMN_COUNT] = [
    Length::Definite(relative(0.4)),
    Length::Definite(relative(0.2)),
    Length::Definite(relative(0.2)),
    Length::Definite(relative(0.2)),
];

/// Which of the two ways of drawing an answer a pane uses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum IndicesDisplayMode {
    #[default]
    Grid,
    Table,
}

impl IndicesDisplayMode {
    /// The glyph this mode is drawn as.
    ///
    /// The two are the halves of one switch, so they are drawn as a filled grid of
    /// squares and as a table: the shape of what the mode draws rather than an
    /// abstraction of it.
    const fn icon(self) -> IconKind {
        match self {
            Self::Grid => IconKind::SquaresFour,
            Self::Table => IconKind::Table,
        }
    }

    /// The element id of the control that selects this mode.
    const fn control_id(self) -> &'static str {
        match self {
            Self::Grid => "IndicesGridMode",
            Self::Table => "IndicesTableMode",
        }
    }
}

/// The indices one pane follows, and how it draws them.
///
/// The selection is the pane's scope, so it is decoded through the same validation
/// the service applies: a persisted selection is either a selection the service can
/// follow or a configuration error the framework reports.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IndicesConfig {
    selection: Vec<SelectedIndex>,
    mode: IndicesDisplayMode,
}

impl Default for IndicesConfig {
    fn default() -> Self {
        Self {
            selection: Vec::new(),
            mode: IndicesDisplayMode::Grid,
        }
    }
}

impl WidgetConfig for IndicesConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

impl<'de> Deserialize<'de> for IndicesConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            selection: Vec<SelectedIndex>,
            mode: IndicesDisplayMode,
        }

        let wire = Wire::deserialize(deserializer)?;

        Ok(Self {
            selection: normalize_selection(&wire.selection).map_err(serde::de::Error::custom)?,
            mode: wire.mode,
        })
    }
}

/// A failure a pane reports.
#[derive(Clone, Debug, PartialEq, Eq)]
enum IndicesFailure {
    Stream(IndicesStreamError),
    Rpc(RpcCallError<IndicesServiceError>),
    Selection(IndicesDataError),
}

impl LocalizedError for IndicesFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Stream(error) => error.localized_message(cx),
            Self::Rpc(error) => error.localized_message(cx),
            Self::Selection(error) => error.localized_message(cx),
        }
    }
}

/// One index as a frame draws it: what the pane follows, and what it holds for it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IndicesRow {
    index: SelectedIndex,
    quote: Option<IndexQuote>,
}

/// The flash one index left behind the last time its price moved.
#[derive(Clone, Copy, Debug)]
struct QuoteFlash {
    direction: IndexDirection,
    started_at: Instant,
}

impl QuoteFlash {
    fn is_active(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < FLASH_DURATION
    }

    fn opacity(self, now: Instant) -> f32 {
        flash_opacity(now.saturating_duration_since(self.started_at))
    }
}

/// The rows the table draws, which is where the shared table reads them from.
struct IndicesTable {
    rows: Arc<[IndicesRow]>,
    /// Whether the pane behind the rows is working, which is what a row without a
    /// quote says instead of a number.
    busy: bool,
}

impl IndicesTable {
    fn empty() -> Self {
        Self {
            rows: Arc::from([]),
            busy: false,
        }
    }
}

impl TableDataSet<COLUMN_COUNT> for IndicesTable {
    fn rows_count(&self) -> usize {
        self.rows.len()
    }

    fn headers(&self, cx: &App) -> [SharedString; COLUMN_COUNT] {
        [
            t!(cx, "widget.indices.index"),
            t!(cx, "widget.indices.latest"),
            t!(cx, "widget.indices.change"),
            t!(cx, "widget.indices.change_percent"),
        ]
    }

    fn column_aligns(&self) -> [Align; COLUMN_COUNT] {
        [Align::Start, Align::End, Align::End, Align::End]
    }

    fn column_widths(&self) -> [Length; COLUMN_COUNT] {
        COLUMN_WIDTHS
    }

    fn cell(&self, row_index: usize, column_index: usize, _cx: &App) -> impl IntoElement {
        let Some(row) = self.rows.get(row_index) else {
            return Empty.into_any_element();
        };

        IndicesCell {
            row: row.clone(),
            column: column_index,
            busy: self.busy,
        }
        .into_any_element()
    }

    /// A read window that carried no index is an empty table, which is a fact rather
    /// than a state: the table says so instead of leaving the body under its headers
    /// blank.
    fn empty(&self, cx: &App) -> Option<AnyElement> {
        Some(EmptyState::new("IndicesEmpty", t!(cx, "common.no_data")).into_any_element())
    }
}

/// One cell of one row, drawn when the table asks for it.
///
/// The shared table renders a cell without a window or an application context, so a
/// cell that needs a color or a translation is an element of its own, rendered later,
/// where both are available.
#[derive(IntoElement)]
struct IndicesCell {
    row: IndicesRow,
    column: usize,
    busy: bool,
}

impl RenderOnce for IndicesCell {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let quote = self.row.quote;
        let direction = quote.map_or(IndexDirection::Flat, IndexQuote::direction);
        let color = match direction {
            IndexDirection::Up => theme.roles.market.up.hsla,
            IndexDirection::Down => theme.roles.market.down.hsla,
            IndexDirection::Flat => theme.roles.market.flat.hsla,
        };

        match self.column {
            0 => name_cell(&self.row, self.busy, cx),
            1 => {
                let (text, color) = match quote {
                    Some(quote) => (format_price(quote.latest), color),
                    None => (pending_text(self.busy, cx), theme.roles.fg.muted.hsla),
                };
                div()
                    .truncate()
                    .text_color(color)
                    .child(text)
                    .into_any_element()
            }
            2 => number_cell(
                quote.map(|quote| format_signed_cents(quote.change())),
                color,
            ),
            3 => number_cell(
                quote.map(|quote| format_percent(quote.change_percent())),
                color,
            ),
            _ => Empty.into_any_element(),
        }
    }
}

/// One number of a row, or the dash a row without a quote has for it.
fn number_cell(text: Option<SharedString>, color: Hsla) -> AnyElement {
    div()
        .truncate()
        .text_color(color)
        .child(text.unwrap_or_else(|| SharedString::new_static("--")))
        .into_any_element()
}

/// The name of an index, and what its row has to say beside it.
///
/// A row that is reconnecting keeps the price it has, so its state is only named when
/// it has nothing to show; a market that is closed is a fact about the index rather
/// than about the pane, so it is named either way.
fn name_cell(row: &IndicesRow, busy: bool, cx: &App) -> AnyElement {
    let content = cx.theme().roles.fg.default.hsla;
    let note_color = cx.theme().roles.fg.muted.hsla;
    let note = match row.quote {
        Some(quote) if !quote.is_trading => Some(t!(cx, "widget.indices.market_closed")),
        Some(_) => None,
        None => Some(pending_text(busy, cx)),
    };

    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(content)
                .child(index_name(&row.index, cx)),
        )
        .children(note.map(|note| {
            div()
                .ml_2()
                .flex_none()
                .truncate()
                .text_xs()
                .text_color(note_color)
                .child(note)
        }))
        .into_any_element()
}

/// What a row says while it has no quote to show.
fn pending_text(busy: bool, cx: &App) -> SharedString {
    if busy {
        t!(cx, "common.connecting")
    } else {
        SharedString::new_static("--")
    }
}

/// An index's name in the language the interface is reading.
fn index_name(index: &SelectedIndex, cx: &App) -> SharedString {
    SharedString::new(index.name.get(interface_language(cx)))
}

/// The language the interface is reading, which is the one a manifest's names are
/// resolved in.
fn interface_language(cx: &App) -> LanguageCode {
    *cx.global::<LanguageCode>()
}

/// The rows a pane draws for one selection.
fn rows_of(
    selection: &[SelectedIndex],
    quotes: Option<&HashMap<IndexTopic, IndexQuote>>,
) -> Arc<[IndicesRow]> {
    let mut rows = Vec::with_capacity(selection.len());
    rows.extend(selection.iter().map(|index| IndicesRow {
        quote: quotes.and_then(|quotes| quotes.get(&index.topic)).copied(),
        index: index.clone(),
    }));
    rows.into()
}

/// How strong a flash is at one point of its life: it rises to its peak and falls back
/// to nothing, which is a triangle rather than a curve because a market does not move
/// smoothly either.
fn flash_opacity(elapsed: Duration) -> f32 {
    if elapsed >= FLASH_DURATION {
        return 0.0;
    }
    if elapsed <= FLASH_PEAK {
        elapsed.as_secs_f32() / FLASH_PEAK.as_secs_f32()
    } else {
        (FLASH_DURATION - elapsed).as_secs_f32() / (FLASH_DURATION - FLASH_PEAK).as_secs_f32()
    }
}

/// The label of one display mode.
fn mode_label(mode: IndicesDisplayMode, cx: &App) -> SharedString {
    match mode {
        IndicesDisplayMode::Grid => t!(cx, "widget.indices.grid"),
        IndicesDisplayMode::Table => t!(cx, "widget.indices.table"),
    }
}

/// One half of the mode switch.
///
/// It carries the mode's name as both its accessible name and its tooltip, because a
/// glyph on its own is a guess: the mark is the only thing distinguishing the two halves
/// on screen. Selecting one is what unselects the other — both read the pane's mode —
/// so there is nothing here to keep in step.
fn mode_button(
    mode: IndicesDisplayMode,
    current: IndicesDisplayMode,
    widget: &Entity<IndicesWidget>,
    cx: &App,
) -> Button {
    let label = mode_label(mode, cx);
    let widget = widget.clone();
    let selected = mode == current;

    Button::new(ElementId::Name(SharedString::new_static(mode.control_id())))
        .small()
        .selected(selected)
        .when_else(selected, |this| this.filled(), |this| this.ghost())
        .with_tooltip(label)
        .on_click(move |_, _, cx| {
            widget.update(cx, |this, cx| this.set_mode(mode, cx));
        })
        .child(Icon::new(mode.icon()).size_3())
}

/// The account a pane follows, as the settings hold it.
fn account_of(config: &Config) -> Option<SmolStr> {
    config
        .account
        .as_ref()
        .map(|account| SmolStr::new(account.user_id.as_str()))
}

/// Reads the manifest of the indices the service currently carries.
///
/// The pane's own question, kept apart from the service seam: the service subscribes and
/// polls, while the pane only needs to know whether a stored selection still exists. A
/// test answers it directly instead of standing up the service.
pub(super) trait ManifestSource {
    fn fetch_manifest(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<proto_types::IndexManifest, KytrixServiceError>> + Send>>;
}

/// Bridges the one-shot manifest lookup from GPUI's executor to the application's
/// Tokio runtime. The widget still awaits the result from its GPUI task, but the
/// connector future itself is always polled by Tokio, which is required by its
/// transport and timer operations.
struct RuntimeManifestSource {
    connector: KytrixServiceConnector,
    runtime: Handle,
}

impl ManifestSource for RuntimeManifestSource {
    fn fetch_manifest(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<proto_types::IndexManifest, KytrixServiceError>> + Send>>
    {
        let connector = self.connector.clone();
        let task = self
            .runtime
            .spawn(async move { IndicesSource::fetch_manifest(&connector).await });
        Box::pin(async move { task.await.unwrap_or(Err(KytrixServiceError::Closed)) })
    }
}

/// One pane: the rows it draws, the scope it follows, and its two ways of drawing.
pub struct IndicesWidget {
    config: IndicesConfig,
    /// Reads what the service currently carries.
    ///
    /// The pane asks it for the manifest before it subscribes, so a selected index the
    /// service no longer carries goes back to having nothing chosen.
    manifest: Rc<dyn ManifestSource>,
    binding: WidgetBinding<IndicesProtocol>,
    status: Entity<WidgetStatus>,
    recovery: WidgetAction,
    config_change: WidgetConfigChange,
    /// The account the pane reads for, which is half of every scope it opens.
    account_id: Option<SmolStr>,
    /// The quotes of the scope the pane is following, once some have arrived.
    quotes: Option<Arc<HashMap<IndexTopic, IndexQuote>>>,
    /// The scope those quotes were published for.
    ///
    /// It is also what makes the pane content: quotes for a selection the user has
    /// left are not this pane's to draw.
    accepted: Option<IndicesScope>,
    /// Whether the scope's subscription has reported itself ready.
    ready: bool,
    /// The rows a frame draws.
    rows: Arc<[IndicesRow]>,
    /// The table, which the pane keeps because the shared table reads its rows from
    /// an entity rather than from the widget's own state.
    table: Entity<IndicesTable>,
    /// Whether the table was last told a row without a quote is working.
    synced_busy: bool,
    /// The flash each index left behind the last time its price moved.
    flashes: HashMap<IndexTopic, QuoteFlash>,
    grid_scroll: ScrollHandle,
    _account_task: Task<()>,
}

impl WidgetView for IndicesWidget {
    type Config = IndicesConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.market.indices");
    const DISPLAY_NAME: &'static str = "widget.names.indices";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    const DESCRIPTION: &'static str = "Global market and commodity indices";
    const ICON: Option<&'static str> = Some("widgets/indices.png");

    fn new(
        _instance_id: WidgetInstanceId,
        config: Self::Config,
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = cx.global::<KytrixServiceConnector>().clone();
        let manifest: Rc<dyn ManifestSource> = Rc::new(RuntimeManifestSource {
            connector: source.clone(),
            runtime: global::handle(cx).handle().clone(),
        });
        Self::new_with_source(
            config,
            source,
            manifest,
            status,
            config_change,
            recovery,
            cx,
        )
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let retry = self.status.is_failed(cx);
        self.account_id = account_of(&cx.global::<ConfigManager>().read());
        self.start(retry, cx);
    }

    /// The options this widget persists.
    ///
    /// The framework asks for these to write the pane back, so this answers with the
    /// field itself rather than computing anything.
    fn config(&self) -> Option<&Self::Config> {
        Some(&self.config)
    }

    /// A pane is content once it holds quotes for the scope it is following.
    ///
    /// Holding quotes for a selection the user has left is not content: the pane is
    /// waiting for the new selection to arrive, and it says so.
    fn has_content(&self) -> bool {
        self.accepted.is_some() && self.accepted == self.current_scope()
    }

    /// The choose button on the left, and the two display modes on the right.
    fn header(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> WidgetHeader {
        WidgetHeader::empty()
            .left(self.choose_button(cx))
            .right(self.mode_switch(cx))
    }
}

impl IndicesWidget {
    /// Drops the selected indices the manifest no longer carries.
    ///
    /// Returns whether this answer still describes the selection the pane follows, which is
    /// what decides whether the subscription for it starts. A read that failed proves
    /// nothing — the service may simply not have answered — so the selection is kept and the
    /// subscription reports its own error, which the user can recover from. Losing every
    /// index goes back to "nothing chosen", which is the pane's own waiting state rather than
    /// an error.
    fn settle_selection(
        &mut self,
        asked: Vec<SelectedIndex>,
        manifest: Result<proto_types::IndexManifest, KytrixServiceError>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.account_id.is_none() || self.config.selection != asked {
            return false;
        }
        let Ok(manifest) = manifest else {
            return true;
        };
        let kept = manifest
            .indices
            .iter()
            .map(|definition| definition.code.as_str())
            .collect::<AHashSet<_>>();
        let settled = asked
            .iter()
            .filter(|index| kept.contains(index.topic.code()))
            .cloned()
            .collect::<Vec<_>>();
        if settled.len() == asked.len() {
            return true;
        }
        log::warn!(
            "{} selected index(es) are no longer in the manifest; the pane went back to \
             following {}",
            asked.len() - settled.len(),
            settled.len()
        );
        self.config.selection = settled;
        self.config_change.publish(cx);
        true
    }

    /// The seam a test can answer through: `new` only picks the production services.
    ///
    /// The subscription and the manifest read are separate seams because a test drives
    /// them separately: the fake session answers subscriptions, and the manifest is
    /// whatever the test says the service carries.
    fn new_with_source<S>(
        config: IndicesConfig,
        source: S,
        manifest: Rc<dyn ManifestSource>,
        status: Entity<WidgetStatus>,
        config_change: WidgetConfigChange,
        recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self
    where
        S: IndicesSource,
    {
        let binding = WidgetBinding::spawn_conditionally(
            global::handle(cx).handle(),
            IndicesService::new(source),
            cx,
            Self::apply_reliable,
            Self::apply_latest,
        );
        let table = cx.new(|_| IndicesTable::empty());
        // The account is the pane's scope, and the user can change it while the pane
        // is open: signing out has to take the indices with it, and signing in has to
        // read again.
        let (initial_account, mut changes) = {
            let manager = cx.global::<ConfigManager>();
            (account_of(&manager.read()), manager.subscribe())
        };
        let account_task = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let account = account_of(&changes.borrow_and_update());
                if this
                    .update(cx, |this, cx| this.reconcile_account(account, cx))
                    .is_err()
                {
                    return;
                }
            }
        });

        Self {
            config,
            manifest,
            binding,
            status,
            recovery,
            config_change,
            // The pane records the account it was built for, but it starts nothing:
            // the framework calls `load` once construction is done.
            account_id: initial_account,
            quotes: None,
            accepted: None,
            ready: false,
            rows: Arc::from([]),
            table,
            synced_busy: false,
            flashes: HashMap::new(),
            grid_scroll: ScrollHandle::new(),
            _account_task: account_task,
        }
    }

    /// The scope this pane follows, which is its account and its selection.
    fn current_scope(&self) -> Option<IndicesScope> {
        IndicesScope::new(self.account_id.as_ref(), &self.config.selection)
    }

    /// Follows the account the settings now hold.
    ///
    /// Another account is another scope, so the quotes the pane is showing are dropped
    /// rather than kept under a new name.
    fn reconcile_account(&mut self, account: Option<SmolStr>, cx: &mut Context<Self>) {
        if self.account_id == account {
            return;
        }
        self.account_id = account;
        self.start(false, cx);
    }

    /// Settles the stored selection against what the service carries, and follows it.
    ///
    /// A recovery keeps the quotes the user is still reading — blanking them to
    /// reconnect would flash a screen of numbers away — while an explicit change of
    /// scope starts over, because the pane cannot vouch for what it was showing.
    fn start(&mut self, retry: bool, cx: &mut Context<Self>) {
        self.ready = false;
        self.flashes.clear();
        if self.current_scope().is_none() {
            self.follow(retry, cx);
            return;
        }
        if !retry {
            self.clear_quotes(cx);
        }
        // A selected index the manifest no longer carries is settled before the pane
        // subscribes to it: subscribing first would report a failure about a scope the user
        // cannot return to anyway. The read is work this pane is doing, so it reports
        // connecting while it is in flight — the request that follows is what connects.
        self.status.report(WidgetState::Connecting, cx);
        cx.notify();
        let manifest = self.manifest.clone();
        let asked = self.config.selection.clone();
        cx.spawn(async move |this, cx| {
            let manifest = manifest.fetch_manifest().await;
            let _ = this.update(cx, |this, cx| {
                // An answer about a selection the user has left, or a pane that signed out
                // while it was being read, is not this pane's to act on: what replaced it
                // started a subscription of its own.
                if !this.settle_selection(asked, manifest, cx) {
                    return;
                }
                this.follow(retry, cx);
            });
        })
        .detach();
    }

    /// Follows the scope the config now holds, or says what the pane is waiting for.
    fn follow(&mut self, retry: bool, cx: &mut Context<Self>) {
        let Some(scope) = self.current_scope() else {
            self.clear_quotes(cx);
            // The key is written where the notice is reported rather than handed to the
            // framework through a helper, so what the pane says is readable here: with
            // nobody signed in the pane sends the reader to the account settings, and with
            // an account it asks for a selection.
            let notice = if self.account_id.is_none() {
                Notice::message("widget.indices.logged_out")
            } else {
                Notice::message("widget.indices.select_at_least_one")
            };
            self.status.report(WidgetState::waiting(notice), cx);
            // Nothing is followed, so nothing should stay subscribed on this pane's
            // behalf: an account that signed out must not keep a seat it is not using.
            self.binding.call(IndicesRequest::Stop, cx, |_, _, _| {});
            cx.notify();
            return;
        };
        self.status.report(WidgetState::Connecting, cx);
        self.binding.call(
            if retry {
                IndicesRequest::Retry(scope)
            } else {
                IndicesRequest::Load(scope)
            },
            cx,
            Self::apply_load_result,
        );
        cx.notify();
    }

    /// Drops the quotes of a scope the pane is leaving.
    fn clear_quotes(&mut self, cx: &mut Context<Self>) {
        self.quotes = None;
        self.accepted = None;
        self.ready = false;
        self.flashes.clear();
        self.sync_rows(cx);
    }

    fn apply_load_result(
        &mut self,
        result: WidgetRpcResult<IndicesProtocol>,
        cx: &mut Context<Self>,
    ) {
        // The answer says the service took the scope; what the subscription then
        // reports about itself arrives on the stream, which is where a failure to
        // subscribe is drawn from.
        if let Err(error) = result {
            let recovery = self.recovery.clone();
            self.status.report(
                WidgetState::failed(Arc::new(IndicesFailure::Rpc(error)), recovery),
                cx,
            );
        }
    }

    /// Applies a status the stream reported.
    ///
    /// Returns whether what a frame draws changed, which is what the framework turns
    /// into a repaint: a status is drawn by the framework around the body, but the rows
    /// say what they are still waiting for, and those are the widget's.
    fn apply_reliable(
        &mut self,
        key: IndicesScope,
        message: Arc<IndicesStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let IndicesStreamMessage::Status(status) = message.as_ref() else {
            return false;
        };

        match status {
            IndicesStatus::Connecting => {
                self.ready = false;
                self.status.report(WidgetState::Connecting, cx);
            }
            IndicesStatus::Ready => {
                self.ready = true;
                // Readiness is published at the protocol's own boundary: a ready
                // subscription is not the same thing as the quotes it describes having
                // arrived, and this is where the two meet.
                if self.has_content() {
                    self.status.report(WidgetState::Ready, cx);
                } else {
                    self.status.report(WidgetState::Loading, cx);
                }
            }
            IndicesStatus::Failed(error) => {
                self.ready = false;
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(IndicesFailure::Stream(error.clone())), recovery),
                    cx,
                );
            }
        }

        self.sync_rows(cx)
    }

    /// Applies the quotes the stream published.
    ///
    /// Returns whether what a frame draws changed, which for quotes is always: the
    /// service publishes them when one of them moved.
    fn apply_latest(
        &mut self,
        key: IndicesScope,
        message: Arc<IndicesStreamMessage>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_scope().as_ref() != Some(&key) || self.status.is_failed(cx) {
            return false;
        }
        let IndicesStreamMessage::Quotes(quotes) = message.as_ref() else {
            return false;
        };

        self.mark_flashes(quotes, cx);
        self.quotes = Some(quotes.clone());
        self.accepted = Some(key);
        if self.ready {
            self.status.report(WidgetState::Ready, cx);
        }
        self.sync_rows(cx);
        true
    }

    /// Notes the indices whose price moved, which is what a cell flashes for.
    ///
    /// The first quotes of a scope are where it starts rather than a move, so a pane
    /// that opens on a market does not flash its whole selection at once. A flash
    /// belongs to the grid, which is the only one of the two ways of drawing a row with
    /// room for it: a table that flashed would ask for frames nobody can see.
    fn mark_flashes(&mut self, quotes: &HashMap<IndexTopic, IndexQuote>, cx: &mut Context<Self>) {
        let Some(previous) = self.quotes.as_deref() else {
            return;
        };
        if cx.reduce_motion() || self.config.mode != IndicesDisplayMode::Grid {
            return;
        }
        let now = Instant::now();
        for (topic, quote) in quotes {
            let Some(previous) = previous.get(topic) else {
                continue;
            };
            let direction = match quote.latest.cmp(&previous.latest) {
                std::cmp::Ordering::Less => IndexDirection::Down,
                std::cmp::Ordering::Equal => continue,
                std::cmp::Ordering::Greater => IndexDirection::Up,
            };
            self.flashes.insert(
                topic.clone(),
                QuoteFlash {
                    direction,
                    started_at: now,
                },
            );
        }
    }

    /// Rebuilds the rows a frame draws, and tells the table about them.
    ///
    /// Returns whether anything drawn changed. The table is a child entity, so it is
    /// notified here rather than on every frame: the body is cached, and a table told
    /// about the same rows twice would repaint for nothing.
    fn sync_rows(&mut self, cx: &mut Context<Self>) -> bool {
        let busy = self.status.state(cx).is_busy();
        let rows = rows_of(&self.config.selection, self.quotes.as_deref());
        if busy == self.synced_busy && self.rows.as_ref() == rows.as_ref() {
            return false;
        }

        self.synced_busy = busy;
        self.rows = rows.clone();
        self.table.update(cx, |table, cx| {
            table.rows = rows;
            table.busy = busy;
            cx.notify();
        });
        true
    }

    /// Follows the indices the user confirmed in the selector.
    fn select_indices(&mut self, indices: Vec<SelectedIndex>, cx: &mut Context<Self>) {
        let selection = match normalize_selection(&indices) {
            Ok(selection) => selection,
            Err(error) => {
                let recovery = self.recovery.clone();
                self.status.report(
                    WidgetState::failed(Arc::new(IndicesFailure::Selection(error)), recovery),
                    cx,
                );
                return;
            }
        };
        if selection == self.config.selection {
            return;
        }
        self.config.selection = selection;
        self.config_change.publish(cx);
        self.start(false, cx);
    }

    fn set_mode(&mut self, mode: IndicesDisplayMode, cx: &mut Context<Self>) {
        if self.config.mode == mode {
            return;
        }
        // Both modes draw the rows the pane already holds, so this re-reads nothing and
        // carries no scroll position across: the two do not scroll alike.
        self.config.mode = mode;
        self.config_change.publish(cx);
        cx.notify();
    }

    /// Adds a frame to the flashes that are still visible, and drops the ones that are
    /// not.
    fn settle_flashes(&mut self, window: &mut Window) {
        let now = Instant::now();
        self.flashes.retain(|_, flash| flash.is_active(now));
        if !self.flashes.is_empty() {
            // The body is cached, and a cached subtree is only repainted when its own
            // view says so. A flash is exactly that case, so it asks for the next frame
            // here.
            window.request_animation_frame();
        }
    }

    /// The control that opens the selector.
    ///
    /// It is disabled without an account: a catalogue is served to an account, so a
    /// reader the service does not know has nothing to choose from — and a live control
    /// that can only fail reads as if the pane were broken.
    fn choose_button(&self, cx: &Context<Self>) -> AnyElement {
        let widget = cx.entity().downgrade();

        div()
            .debug_selector(|| "TEST_INDICES_CHOOSE".into())
            .flex_none()
            // A control in the header row has to take its own clicks: the row is the
            // pane's drag area as well.
            .occlude()
            .child(
                Button::new(ElementId::Name(SharedString::new_static("IndicesChoose")))
                    .filled()
                    .small()
                    .disabled(self.account_id.is_none())
                    .aria_label(t!(cx, "widget.indices.choose"))
                    .with_tooltip(t!(cx, "widget.indices.choose"))
                    .on_click(move |_, window, cx| {
                        let Some(current) = widget.upgrade() else {
                            return;
                        };
                        let selection = current.read(cx).config.selection.clone();
                        // What the selector is handed is weak, so a pane closed with its picker still up
                        // is released rather than held by it. Unlike the other panes' pickers, this one
                        // has no test that says so: `IndicesSelector::show` reads its service from a
                        // global that only a production `AuthSession` can install, so the path this
                        // callback is reached by cannot be driven here.
                        let widget_for_choice = widget.clone();
                        IndicesSelector::show(
                            &selection,
                            move |selection, _, cx| {
                                // The selector does not keep the pane alive: a pane gone by the time a
                                // selection is confirmed has nothing left to select into.
                                let _ = widget_for_choice.update(cx, |this, cx| {
                                    this.select_indices(selection, cx);
                                });
                            },
                            window,
                            cx,
                        );
                    })
                    .child(Icon::new(IconKind::Plus).size_3()),
            )
            .into_any_element()
    }

    /// The two halves of the mode switch, which the header joins into one control.
    ///
    /// They are exclusive by construction rather than by bookkeeping: both read the
    /// pane's own `mode`, and each is selected exactly when it is that mode, so picking
    /// one is what unselects the other.
    fn mode_controls(&self, cx: &Context<Self>) -> (Button, Button) {
        let widget = cx.entity();
        let current = self.config.mode;

        (
            mode_button(IndicesDisplayMode::Grid, current, &widget, cx),
            mode_button(IndicesDisplayMode::Table, current, &widget, cx),
        )
    }

    /// The two ways of drawing the answer, as one control with two halves.
    ///
    /// They are joined so they read as one switch rather than as two unrelated controls
    /// that happen to sit together.
    fn mode_switch(&self, cx: &Context<Self>) -> AnyElement {
        let (grid, table) = self.mode_controls(cx);

        div()
            .debug_selector(|| "TEST_INDICES_MODE".into())
            .flex_none()
            .occlude()
            .child(Join::default().button(grid).button(table))
            .into_any_element()
    }

    /// The table, which is the shared one: the rows are the pane's, and the table reads
    /// them from the child entity it was handed.
    fn table_body(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id(ElementId::Name(SharedString::new_static(
                "IndicesTableFrame",
            )))
            .debug_selector(|| "TEST_INDICES_TABLE".into())
            .aria_label(t!(cx, "widget.indices.table"))
            .size_full()
            .min_h_0()
            .child(Table::new(
                // The table keeps its own scroll position in state keyed by this id, so
                // it has to stay stable across frames.
                ElementId::Name(SharedString::new_static("IndicesTable")),
                &self.table,
            ))
            .into_any_element()
    }

    /// The grid: fixed-size cells in a viewport that scrolls both ways.
    fn grid_body(&self, cx: &Context<Self>) -> AnyElement {
        let horizontal = div()
            .absolute()
            .left_0()
            .right(px(SCROLLBAR_THICKNESS))
            .bottom_0()
            .h(px(SCROLLBAR_THICKNESS))
            .child(Scrollbar::horizontal(self.grid_scroll.clone()).mode(ScrollbarMode::Always));
        let vertical = div()
            .absolute()
            .top_0()
            .right_0()
            .bottom(px(SCROLLBAR_THICKNESS))
            .w(px(SCROLLBAR_THICKNESS))
            .child(Scrollbar::vertical(self.grid_scroll.clone()).mode(ScrollbarMode::Always));

        div()
            .relative()
            .size_full()
            .child(grid_viewport(
                self.rows.clone(),
                self.flashes.clone(),
                self.status.state(cx).is_busy(),
                px(f32::from(GRID_MIN_CELL_WIDTH)),
                px(f32::from(GRID_MIN_CELL_HEIGHT)),
                &self.grid_scroll,
            ))
            .child(horizontal)
            .child(vertical)
            .into_any_element()
    }
}

impl Render for IndicesWidget {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body is only embedded while the pane has content, so this is the ready
        // state and nothing else. Everything the pane is *doing* is drawn by the
        // framework around this.
        self.settle_flashes(window);
        match self.config.mode {
            IndicesDisplayMode::Grid => self.grid_body(cx),
            IndicesDisplayMode::Table => self.table_body(cx),
        }
    }
}

/// The grid's fixed-cell layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GridLayout {
    pub columns: usize,
    pub rows: usize,
    pub cell_width: Pixels,
    pub cell_height: Pixels,
    pub content_width: Pixels,
    pub content_height: Pixels,
}

/// Resolves fixed-size cells without using Taffy's grid layout: the viewport width
/// alone decides the column count, and the remaining cells wrap into rows.
pub(crate) fn grid_layout(
    viewport: Size<Pixels>,
    item_count: usize,
    cell_width: Pixels,
    cell_height: Pixels,
) -> GridLayout {
    let cell_width = cell_width.max(px(1.0));
    let cell_height = cell_height.max(px(1.0));
    if item_count == 0 {
        return GridLayout {
            columns: 0,
            rows: 0,
            cell_width,
            cell_height,
            content_width: Pixels::ZERO,
            content_height: Pixels::ZERO,
        };
    }

    let columns = ((viewport.width / cell_width).floor() as usize)
        .max(1)
        .min(item_count);
    let rows = item_count.div_ceil(columns);

    GridLayout {
        columns,
        rows,
        cell_width,
        cell_height,
        content_width: px(columns as f32 * cell_width.as_f32()),
        content_height: px(rows as f32 * cell_height.as_f32()),
    }
}

/// The scrollable viewport the grid's cells are laid out in.
///
/// The cells are placed by hand rather than by the layout engine, so the viewport is
/// built during prepaint, where the box it was given is known: the number of columns a
/// frame has room for is a property of that frame rather than of the pane.
fn grid_viewport(
    rows: Arc<[IndicesRow]>,
    flashes: HashMap<IndexTopic, QuoteFlash>,
    busy: bool,
    cell_width: Pixels,
    cell_height: Pixels,
    scroll_handle: &ScrollHandle,
) -> AnyElement {
    let viewport_scroll_handle = scroll_handle.clone();
    canvas(
        move |bounds, window, app| {
            let layout = grid_layout(bounds.size, rows.len(), cell_width, cell_height);
            let columns = layout.columns.max(1);
            let now = Instant::now();
            let cells = rows.iter().enumerate().map(|(index, row)| {
                let column = index % columns;
                let line = index / columns;
                grid_cell(
                    row,
                    flashes.get(&row.index.topic).copied(),
                    point(
                        px(column as f32 * layout.cell_width.as_f32()),
                        px(line as f32 * layout.cell_height.as_f32()),
                    ),
                    size(layout.cell_width, layout.cell_height),
                    busy,
                    now,
                    app,
                )
            });
            let content = div()
                .id(ElementId::Name(SharedString::new_static(
                    "IndicesGridContent",
                )))
                .debug_selector(|| "TEST_INDICES_GRID_CONTENT".into())
                .relative()
                .flex_none()
                .w(layout.content_width)
                .h(layout.content_height)
                .children(cells);
            let centered = div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .w(bounds.size.width.max(layout.content_width))
                .h(bounds.size.height.max(layout.content_height))
                .child(content);
            let mut viewport = div()
                .id(ElementId::Name(SharedString::new_static(
                    "IndicesGridViewport",
                )))
                .size_full()
                .min_w_0()
                .min_h_0()
                .overflow_scroll()
                .track_scroll(&viewport_scroll_handle)
                .child(centered)
                .into_any_element();
            viewport.prepaint_as_root(bounds.origin, bounds.size.into(), window, app);
            viewport
        },
        |_, mut viewport, window, app| viewport.paint(window, app),
    )
    .absolute()
    .inset_0()
    .size_full()
    .into_any_element()
}

/// One cell of the grid, at the place the layout gave it.
fn grid_cell(
    row: &IndicesRow,
    flash: Option<QuoteFlash>,
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    busy: bool,
    now: Instant,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.roles.fg.muted.hsla;
    let quote = row.quote;
    let direction = quote.map_or(IndexDirection::Flat, IndexQuote::direction);
    let text_color = match direction {
        IndexDirection::Up => theme.roles.market.up.hsla,
        IndexDirection::Down => theme.roles.market.down.hsla,
        IndexDirection::Flat => theme.roles.market.flat.hsla,
    };
    // A cell is one of the grid's siblings under its own content box, so what tells one cell from
    // another is the index it draws.
    let cell_id = ElementId::NamedChild(
        Arc::new(ElementId::Name(SharedString::new_static("IndicesGridCell"))),
        SharedString::new(row.index.topic.code()),
    );
    let details = match quote {
        Some(quote) => div()
            .w_full()
            .flex()
            .items_center()
            .text_xs()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_center()
                    .child(format_percent(quote.change_percent())),
            )
            .children((!quote.is_trading).then(|| {
                div()
                    .flex_none()
                    .px_1()
                    .truncate()
                    .text_color(muted)
                    .child(t!(cx, "widget.indices.market_closed"))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_center()
                    .child(format_signed_cents(quote.change())),
            )
            .into_any_element(),
        None => div()
            .w_full()
            .truncate()
            .text_center()
            .text_xs()
            .text_color(muted)
            .child(pending_text(busy, cx))
            .into_any_element(),
    };
    let flash_bar = div()
        .id(ElementId::from((cell_id.clone(), "flash")))
        .debug_selector(|| "TEST_INDICES_GRID_FLASH".into())
        .absolute()
        .top(px(-1.0))
        .left(px(-1.0))
        .right(px(-1.0))
        .h(px(GRID_FLASH_HEIGHT))
        .bg(flash.map_or_else(Hsla::default, |flash| {
            let color = match flash.direction {
                IndexDirection::Up => theme.roles.market.up.hsla,
                IndexDirection::Down => theme.roles.market.down.hsla,
                IndexDirection::Flat => theme.roles.market.flat.hsla,
            };
            color.opacity(flash.opacity(now))
        }));

    div()
        .id(cell_id)
        .debug_selector(|| "TEST_INDICES_GRID_CELL".into())
        .absolute()
        .left(origin.x)
        .top(origin.y)
        .w(cell.width)
        .h(cell.height)
        .min_w_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .border_1()
        .border_color(theme.roles.line.divider.hsla)
        // The cell is a tinted card, not a depth bar behind one: `market.*_surface` is the
        // chip recipe §7.5/§7.6 define for a market color written on its own tint, and it is
        // the pairing §7.8 holds to 4.5:1. The alpha weak washes are for a bar painted behind
        // the data, where the page still shows through.
        .bg(match direction {
            IndexDirection::Up => theme.roles.market.up_surface.hsla,
            IndexDirection::Down => theme.roles.market.down_surface.hsla,
            IndexDirection::Flat => theme.roles.surface.raised.hsla,
        })
        .text_color(text_color)
        .child(
            div()
                .w_full()
                .truncate()
                .text_center()
                .text_xs()
                .child(index_name(&row.index, cx)),
        )
        .child(
            div()
                .w_full()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .truncate()
                .text_lg()
                .font_weight(FontWeight::BOLD)
                .child(quote.map_or_else(
                    || SharedString::new_static("--"),
                    |quote| format_price(quote.latest),
                )),
        )
        .child(details)
        .child(flash_bar)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, sync::atomic::Ordering};

    use app_config::{
        settings::{AccountConfig, AccountWalletKind, Config},
        widget::{VersionedWidgetConfig, decode_or_default},
    };
    use gpui_kit::{
        AppContext, Bounds, Entity, IntoElement, TestAppContext, VisualTestContext,
        base::Selectable, point,
    };
    use kytrix_service_provider::service::engine::KytrixServiceError;
    use network::transport::error::TransportError;
    use widget::protocol::RequestId;

    use super::*;
    use crate::{
        model::{IndicesInvalidReason, LocalizedIndexName, MAX_NAME_CHARS, MAX_SELECTED_INDICES},
        service::IndicesStreamError,
        test_support::{ACCOUNT, FakeSource, event, selected, topic},
    };

    /// Settings holding `account`, or holding nobody when it is `None`.
    fn settings(account: Option<&str>) -> Config {
        let mut config = Config::default();
        config.account = account.map(|user_id| AccountConfig {
            user_id: user_id.to_owned(),
            wallet_address: "0x0000000000000000000000000000000000000001".to_owned(),
            wallet_kind: AccountWalletKind::Ethereum,
        });
        config
    }

    /// Installs everything a pane reads, with `account` signed in or nobody.
    fn install(cx: &mut App, account: Option<&str>) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("the embedded locales should load");
        global::install(cx).expect("the test Tokio runtime should build");
        cx.set_global(ConfigManager::in_memory(settings(account)));
    }

    fn config(selection: &[SelectedIndex]) -> IndicesConfig {
        IndicesConfig {
            selection: normalize_selection(selection)
                .expect("the fixture selection should be valid"),
            ..IndicesConfig::default()
        }
    }

    /// A manifest source that never answers, which is what "the service did not say"
    /// looks like: the pane leaves the selection alone, so most tests never need one.
    struct PendingManifest;

    impl ManifestSource for PendingManifest {
        fn fetch_manifest(
            &self,
        ) -> Pin<
            Box<dyn Future<Output = Result<proto_types::IndexManifest, KytrixServiceError>> + Send>,
        > {
            Box::pin(std::future::pending())
        }
    }

    /// A manifest carrying exactly these indices.
    ///
    /// It can be told about more, because a catalogue belongs to the service rather than to
    /// the user's next selection: a test that follows a second index says the service
    /// carries it too.
    struct Manifest(RefCell<Vec<IndexTopic>>);

    impl Manifest {
        /// A manifest of the indices these codes name.
        fn carrying(codes: &[&str]) -> Self {
            Self(RefCell::new(codes.iter().map(|code| topic(code)).collect()))
        }

        /// Adds the indices these codes name to what the catalogue carries.
        fn also_carrying(&self, codes: &[&str]) {
            self.0
                .borrow_mut()
                .extend(codes.iter().map(|code| topic(code)));
        }
    }

    impl ManifestSource for Manifest {
        fn fetch_manifest(
            &self,
        ) -> Pin<
            Box<dyn Future<Output = Result<proto_types::IndexManifest, KytrixServiceError>> + Send>,
        > {
            let indices = self
                .0
                .borrow()
                .iter()
                .map(|topic| proto_types::IndexDefinition {
                    request_code: format!(".{}", topic.code()),
                    code: topic.code().to_owned(),
                    category: "test".to_owned(),
                    name: None,
                })
                .collect();
            Box::pin(async move {
                Ok(proto_types::IndexManifest {
                    categories: Vec::new(),
                    indices,
                })
            })
        }
    }

    /// A catalogue carrying exactly the indices a pane was configured with.
    fn configured_manifest(config: &IndicesConfig) -> Rc<dyn ManifestSource> {
        Rc::new(Manifest(RefCell::new(
            config
                .selection
                .iter()
                .map(|index| index.topic.clone())
                .collect(),
        )))
    }

    /// A pane the framework would have built, driven by a fake session.
    ///
    /// The fake answers nothing unless a test queues it an answer: a source that
    /// answered on its own would race the tests, because its service publishes on a
    /// task of its own.
    fn pane_with_source<S>(
        source: S,
        config: IndicesConfig,
        cx: &mut Context<IndicesWidget>,
    ) -> IndicesWidget
    where
        S: IndicesSource,
    {
        let manifest = configured_manifest(&config);
        pane_with_manifest(source, manifest, config, cx)
    }

    /// The same, with a manifest the pane reads the selected indices against.
    fn pane_with_manifest<S>(
        source: S,
        manifest: Rc<dyn ManifestSource>,
        config: IndicesConfig,
        cx: &mut Context<IndicesWidget>,
    ) -> IndicesWidget
    where
        S: IndicesSource,
    {
        let status = cx.new(|_| WidgetStatus::new());
        let weak = cx.weak_entity();
        let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
            let _ = weak.update(cx, |pane, cx| pane.load(cx));
        });
        IndicesWidget::new_with_source(
            config,
            source,
            manifest,
            status,
            WidgetConfigChange::default(),
            recovery,
            cx,
        )
    }
    /// A pane created the way the framework creates one: state, then one load.
    fn pane(
        cx: &mut TestAppContext,
        source: FakeSource,
        config: IndicesConfig,
    ) -> Entity<IndicesWidget> {
        let manifest = configured_manifest(&config);
        loaded(cx, source, manifest, config)
    }

    /// A pane loaded over a catalogue a test arranges itself.
    fn loaded(
        cx: &mut TestAppContext,
        source: FakeSource,
        manifest: Rc<dyn ManifestSource>,
        config: IndicesConfig,
    ) -> Entity<IndicesWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            cx.new(|cx| pane_with_manifest(source, manifest, config, cx))
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    /// A pane nobody is signed in for.
    fn signed_out_pane(cx: &mut TestAppContext, source: FakeSource) -> Entity<IndicesWidget> {
        cx.executor().allow_parking();
        let pane = cx.update(|cx| {
            install(cx, None);
            cx.new(|cx| pane_with_source(source, config(&[selected("HSI")]), cx))
        });
        pane.update(cx, |pane, cx| pane.load(cx));
        pane
    }

    fn quote(code: &str, latest: u32) -> IndexQuote {
        IndexQuote::from_event(&event(topic(code), latest, i64::from(latest)))
    }

    /// Reports quotes as the stream would.
    ///
    /// The binding is cancelled first, which is what makes the reported message the
    /// only input the pane has.
    fn report_quotes(pane: &Entity<IndicesWidget>, quotes: &[(&str, u32)], cx: &mut App) -> bool {
        let scope = pane
            .read(cx)
            .current_scope()
            .expect("a loaded pane follows a scope");
        let mut published = HashMap::with_capacity(quotes.len());
        published.extend(
            quotes
                .iter()
                .map(|(code, latest)| (topic(code), quote(code, *latest))),
        );
        pane.update(cx, |pane, cx| {
            pane.binding.cancel();
            pane.apply_latest(
                scope,
                Arc::new(IndicesStreamMessage::Quotes(Arc::new(published))),
                cx,
            )
        })
    }

    /// Reports a status as the stream would.
    fn report_status(pane: &Entity<IndicesWidget>, status: IndicesStatus, cx: &mut App) -> bool {
        let scope = pane
            .read(cx)
            .current_scope()
            .expect("a loaded pane follows a scope");
        pane.update(cx, |pane, cx| {
            pane.apply_reliable(scope, Arc::new(IndicesStreamMessage::Status(status)), cx)
        })
    }

    fn failure_of(pane: &Entity<IndicesWidget>, cx: &App) -> SharedString {
        pane.read(cx)
            .status
            .state(cx)
            .failure()
            .expect("the pane should have reported a failure")
            .text(cx)
    }

    /// Lets the pane and its service run until what the test is waiting for happens.
    ///
    /// Two executors are involved: the pane sends its commands from GPUI's own, and the
    /// service answers on the application runtime. So both are given a turn — the pane
    /// first, because a command that has not been sent is a service that has nothing to
    /// answer.
    fn settle_until(cx: &mut TestAppContext, what: &str, ready: impl Fn(&App) -> bool) {
        crate::test_support::settle_until(cx, what, ready);
    }

    fn latest_of(pane: &Entity<IndicesWidget>, cx: &App) -> Vec<Option<u32>> {
        pane.read(cx)
            .rows
            .iter()
            .map(|row| row.quote.map(|quote| quote.latest))
            .collect()
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        assert_eq!(IndicesWidget::ID.as_str(), "kytrix.market.indices");
        assert_eq!(IndicesWidget::DISPLAY_NAME, "widget.names.indices");
        assert_eq!(IndicesConfig::SCHEMA_VERSION, WidgetConfigSchemaVersion::V1);
    }

    #[gpui_kit::test]
    fn a_signed_out_pane_should_say_what_it_is_waiting_for(cx: &mut TestAppContext) {
        let pane = signed_out_pane(cx, FakeSource::default());

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert_eq!(pane.current_scope(), None);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message("widget.indices.logged_out")),
                "a pane nobody signed in for is not a failure"
            );
            assert!(!pane.status.state(cx).is_failed());
            assert_eq!(
                pane.status.state(cx),
                WidgetState::waiting(Notice::message("widget.indices.logged_out")),
                "and it is waiting rather than working"
            );
        });
    }

    /// The notice is a reference into the catalogue, so a waiting pane says what it waits
    /// for in the language now showing — without a reload and without anyone re-reporting
    /// for it.
    #[gpui_kit::test]
    fn a_waiting_pane_should_say_what_it_waits_for_in_the_language_showing_now(
        cx: &mut TestAppContext,
    ) {
        let pane = signed_out_pane(cx, FakeSource::default());

        let english = cx.update(|cx| {
            let waiting = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("a pane nobody signed in for is waiting");
            assert_eq!(waiting, Notice::message("widget.indices.logged_out"));
            let english = waiting.resolve(cx);
            assert_eq!(english, t!(cx, "widget.indices.logged_out"));

            cx.set_global(LanguageCode::ZhCn);
            english
        });
        // The switch reaches the pane on the effect flush of the update that made it, and
        // nothing here reloads the pane or re-reports on its behalf.
        cx.update(|cx| {
            let waiting = pane
                .read(cx)
                .status
                .state(cx)
                .notice()
                .cloned()
                .expect("the pane is still waiting");
            assert_eq!(
                waiting,
                Notice::message("widget.indices.logged_out"),
                "the pane still stands in with the same reference"
            );
            let chinese = waiting.resolve(cx);
            assert_ne!(chinese, english, "the copy follows the language");
            assert_eq!(chinese, t!(cx, "widget.indices.logged_out"));
        });
    }

    #[gpui_kit::test]
    fn a_pane_with_nothing_selected_should_ask_for_a_selection(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let pane = pane(cx, source.clone(), IndicesConfig::default());

        cx.update(|cx| {
            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message("widget.indices.select_at_least_one"))
            );
        });
        assert!(
            source.subscribed().is_empty(),
            "a pane with nothing selected subscribes to nothing"
        );
    }

    #[gpui_kit::test]
    fn a_pane_should_follow_the_selection_it_was_given(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let control = source.subscribe();
        let pane = pane(
            cx,
            source.clone(),
            config(&[selected("HSI"), selected("SPX")]),
        );

        cx.update(|cx| {
            assert_eq!(pane.read(cx).status.state(cx), WidgetState::Connecting);
            assert!(!pane.read(cx).has_content());
        });
        settle_until(cx, "the pane to subscribe", |_| {
            source.subscribed().len() == 1
        });
        assert_eq!(
            source.subscribed(),
            vec![vec![topic("HSI"), topic("SPX")]],
            "the scope is the selection the pane was configured with"
        );
        assert_eq!(control.drops.load(Ordering::SeqCst), 0);
    }

    /// The whole path in one test: the scope the pane opens, the quotes the session
    /// publishes, and what the pane draws out of them.
    #[gpui_kit::test]
    fn a_pane_should_show_the_quotes_its_session_publishes(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let control = source.subscribe();
        let pane = pane(cx, source.clone(), config(&[selected("HSI")]));

        settle_until(cx, "the pane to subscribe", |_| {
            source.subscribed().len() == 1
        });
        cx.update(|cx| {
            assert!(
                !pane.read(cx).has_content(),
                "a subscription on its own is not content"
            );
        });

        let handle = cx.update(|cx| global::handle(cx).handle().clone());
        handle.block_on(async {
            control
                .events
                .send(Ok(event(topic("HSI"), 10_500, 7)))
                .await
                .expect("the quote should be published");
        });
        settle_until(cx, "the quotes to reach the pane", |cx| {
            pane.read(cx).has_content()
        });

        cx.update(|cx| {
            assert_eq!(latest_of(&pane, cx), [Some(10_500)]);
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "the session called the scope ready once its only index had quoted"
            );
        });
    }

    #[gpui_kit::test]
    fn quotes_should_become_content_and_readiness_should_follow_them(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI"), selected("SPX")]));

        cx.update(|cx| {
            assert!(
                !report_status(&pane, IndicesStatus::Ready, cx),
                "a row without a quote says it is working either way, so the body it is in \
                 has nothing to redraw for the pane having moved from connecting to loading"
            );
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Loading,
                "a ready subscription is not the quotes it describes"
            );
            assert!(!pane.read(cx).has_content());

            assert!(report_quotes(&pane, &[("HSI", 101)], cx));
            assert!(pane.read(cx).has_content());
            assert_eq!(
                pane.read(cx).status.state(cx),
                WidgetState::Ready,
                "the quotes a status described make the pane ready"
            );
            assert_eq!(
                pane.read(cx).table.read(cx).rows.len(),
                2,
                "the table draws a row per selected index"
            );
            assert_eq!(
                latest_of(&pane, cx),
                [Some(101), None],
                "an index that has not quoted keeps its place with nothing in it"
            );
        });
    }

    #[gpui_kit::test]
    fn a_price_that_moved_should_flash_and_the_first_quotes_should_not(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            report_quotes(&pane, &[("HSI", 101)], cx);
            assert!(
                pane.read(cx).flashes.is_empty(),
                "the first quotes of a scope are where it starts, not a move"
            );

            report_quotes(&pane, &[("HSI", 102)], cx);
            let flash = *pane
                .read(cx)
                .flashes
                .get(&topic("HSI"))
                .expect("a move should flash");
            assert_eq!(flash.direction, IndexDirection::Up);
            assert!(flash.is_active(Instant::now()));
        });
    }

    /// A flash that has run its course is dropped, which is what stops the pane asking
    /// for frames: an animation that never settles is a pane that never idles.
    #[gpui_kit::test]
    fn a_flash_should_be_forgotten_once_it_has_faded(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));
        let window = cx.add_empty_window();

        window.update(|_, cx| {
            report_quotes(&pane, &[("HSI", 101)], cx);
            report_quotes(&pane, &[("HSI", 102)], cx);
        });
        draw_pane(window, &pane);
        window.update(|_, cx| {
            assert_eq!(
                pane.read(cx).flashes.len(),
                1,
                "a flash still visible is kept"
            );
        });

        window.update(|_, cx| {
            pane.update(cx, |pane, _| {
                // Where the first frame of a flash is a third of a second later.
                pane.flashes.insert(
                    topic("HSI"),
                    QuoteFlash {
                        direction: IndexDirection::Up,
                        started_at: Instant::now() - FLASH_DURATION - Duration::from_millis(1),
                    },
                );
            });
        });
        draw_pane(window, &pane);
        window.update(|_, cx| {
            assert!(
                pane.read(cx).flashes.is_empty(),
                "a flash that has run its course asks for no more frames"
            );
        });
    }

    /// Draws a pane in a frame of its own, which is where its own frame work runs.
    fn draw_pane(window: &mut VisualTestContext, pane: &Entity<IndicesWidget>) {
        let pane = pane.clone();
        window.draw(
            point(Pixels::ZERO, Pixels::ZERO),
            size(px(400.0), px(400.0)),
            move |_, _| pane.into_any_element(),
        );
    }

    #[gpui_kit::test]
    fn reduce_motion_should_leave_no_flash(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            cx.set_reduce_motion(true);
            report_quotes(&pane, &[("HSI", 101)], cx);
            report_quotes(&pane, &[("HSI", 102)], cx);

            assert!(
                pane.read(cx).flashes.is_empty(),
                "a reader who asked for less movement is not shown a market's worth of it"
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_scope_should_offer_the_recovery_it_was_given(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            report_status(
                &pane,
                IndicesStatus::Failed(IndicesStreamError::Transport(TransportError::Closed)),
                cx,
            );

            let status = pane.read(cx).status.state(cx);
            let failure = status.failure().expect("a failed scope is reported");
            assert_eq!(
                failure.action().label(cx).as_ref(),
                "Retry",
                "every failure carries the framework's own way out"
            );
            assert!(!failure.text(cx).is_empty());
        });
    }

    #[gpui_kit::test]
    fn messages_after_a_failure_should_be_ignored(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            report_quotes(&pane, &[("HSI", 101)], cx);
            report_status(
                &pane,
                IndicesStatus::Failed(IndicesStreamError::Transport(TransportError::Closed)),
                cx,
            );

            assert!(
                !report_quotes(&pane, &[("HSI", 102)], cx),
                "a late quote is not a change"
            );
            assert!(!report_status(&pane, IndicesStatus::Ready, cx));
            assert!(pane.read(cx).status.is_failed(cx));
            assert_eq!(
                latest_of(&pane, cx),
                [Some(101)],
                "what was on screen stays on screen under the failure"
            );
        });
    }

    /// A refusal about the selection is the one refusal with something to say beyond
    /// "it failed": how many indices the account may follow.
    #[gpui_kit::test]
    fn a_refused_selection_should_say_how_many_indices_the_account_may_follow(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI"), selected("SPX")]));

        cx.update(|cx| {
            report_status(
                &pane,
                IndicesStatus::Failed(IndicesStreamError::IndexLimit { limit: Some(3) }),
                cx,
            );

            assert_eq!(
                failure_of(&pane, cx).as_ref(),
                t!(cx, "error.indices.index_limit", limit = 3).as_ref()
            );
        });
    }

    #[gpui_kit::test]
    fn a_refusal_without_a_published_limit_should_still_say_what_happened(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI"), selected("SPX")]));

        cx.update(|cx| {
            report_status(
                &pane,
                IndicesStatus::Failed(IndicesStreamError::IndexLimit { limit: None }),
                cx,
            );

            assert_eq!(
                failure_of(&pane, cx).as_ref(),
                t!(cx, "error.indices.index_limit_unknown").as_ref()
            );
        });
    }

    #[gpui_kit::test]
    fn changing_the_selection_should_follow_the_new_scope_and_drop_the_old_quotes(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let first = source.subscribe();
        let second = source.subscribe_after_drop(Some(first.drops.clone()));
        let manifest = Rc::new(Manifest::carrying(&["HSI"]));
        let pane = loaded(
            cx,
            source.clone(),
            manifest.clone(),
            config(&[selected("HSI")]),
        );

        settle_until(cx, "the pane to subscribe", |_| {
            source.subscribed().len() == 1
        });
        let handle = cx.update(|cx| global::handle(cx).handle().clone());
        handle.block_on(async {
            first
                .events
                .send(Ok(event(topic("HSI"), 101, 1)))
                .await
                .expect("the quote should be published");
        });
        settle_until(cx, "the quotes to reach the pane", |cx| {
            pane.read(cx).has_content()
        });

        // The service carries the index the user follows next as well.
        manifest.also_carrying(&["SPX"]);
        cx.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.select_indices(vec![selected("HSI"), selected("SPX")], cx);
            });
            assert!(
                !pane.read(cx).has_content(),
                "the quotes of the selection the user left are not this pane's to draw"
            );
            assert_eq!(
                pane.read(cx)
                    .rows
                    .iter()
                    .map(|row| row.index.topic.code())
                    .collect::<Vec<_>>(),
                ["HSI", "SPX"]
            );
        });
        settle_until(cx, "the pane to change its scope", |_| {
            source.subscribed().len() == 2
        });
        assert_eq!(
            source.subscribed(),
            vec![vec![topic("HSI")], vec![topic("HSI"), topic("SPX")]]
        );
        assert_eq!(second.drops.load(Ordering::SeqCst), 0);
    }

    #[gpui_kit::test]
    fn confirming_the_selection_a_pane_already_follows_should_change_nothing(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source.clone(), config(&[selected("HSI")]));

        settle_until(cx, "the pane to subscribe", |_| {
            source.subscribed().len() == 1
        });
        cx.update(|cx| {
            report_quotes(&pane, &[("HSI", 101)], cx);
            pane.update(cx, |pane, cx| {
                pane.select_indices(vec![selected("HSI")], cx);
            });

            assert!(
                pane.read(cx).has_content(),
                "the selection it already follows is not a scope it left"
            );
        });
        assert_eq!(source.subscribed(), vec![vec![topic("HSI")]]);
    }

    #[gpui_kit::test]
    fn signing_out_should_take_the_indices_with_it(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            report_quotes(&pane, &[("HSI", 101)], cx);
            pane.update(cx, |pane, cx| pane.reconcile_account(None, cx));

            let pane = pane.read(cx);
            assert!(!pane.has_content());
            assert_eq!(pane.current_scope(), None);
            assert_eq!(
                pane.status.state(cx).notice(),
                Some(&Notice::message("widget.indices.logged_out"))
            );
        });
        settle_until(cx, "the subscription to be released", |_| {
            control.drops.load(Ordering::SeqCst) == 1
        });
        assert_eq!(
            control.drops.load(Ordering::SeqCst),
            1,
            "a pane that follows nothing leaves nothing subscribed"
        );
    }

    #[gpui_kit::test]
    fn the_recovery_should_read_the_scope_again(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source.clone(), config(&[selected("HSI")]));

        settle_until(cx, "the pane to subscribe", |_| {
            source.subscribed().len() == 1
        });
        cx.update(|cx| {
            report_status(
                &pane,
                IndicesStatus::Failed(IndicesStreamError::Service(KytrixServiceError::Closed)),
                cx,
            );
            assert!(pane.read(cx).status.is_failed(cx));

            // The framework's recovery reruns `load`, which is the only thing that
            // starts a failed pane again.
            pane.update(cx, |pane, cx| pane.load(cx));
        });
        settle_until(cx, "the recovery to read again", |_| {
            source.subscribed().len() == 2
        });
        assert_eq!(
            source.subscribed(),
            vec![vec![topic("HSI")], vec![topic("HSI")]],
            "the recovery retries the scope the pane follows"
        );
    }

    #[gpui_kit::test]
    fn the_table_should_hold_the_rows_the_pane_draws(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            assert_eq!(pane.read(cx).table.read(cx).rows.len(), 1);
            report_status(&pane, IndicesStatus::Ready, cx);
            report_quotes(&pane, &[("HSI", 101)], cx);

            let table = pane.read(cx).table.clone();
            assert_eq!(table.read(cx).rows.len(), 1);
            assert_eq!(
                table.read(cx).rows[0].quote.map(|quote| quote.latest),
                Some(101)
            );
            assert!(
                !table.read(cx).busy,
                "a row with nothing left to wait for says so through the table as well"
            );
        });
    }

    /// A row without a quote is the only place the pane's own work is drawn, so both
    /// answers are pinned here: working, and nothing left to wait for.
    #[gpui_kit::test]
    fn a_row_without_a_quote_should_say_whether_the_pane_is_working(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));

            assert_eq!(pending_text(true, cx), t!(cx, "common.connecting"));
            assert_eq!(pending_text(false, cx), SharedString::new_static("--"));
        });
    }

    /// The table is told whether the pane is working, and that is a fact of its own:
    /// a failure leaves the rows exactly as they were, and they still have to stop
    /// saying the pane is connecting — nothing else tells the table.
    #[gpui_kit::test]
    fn a_failure_should_tell_the_table_the_pane_stopped_working(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI"), selected("SPX")]));

        cx.update(|cx| {
            assert!(
                pane.read(cx).table.read(cx).busy,
                "a connecting pane says so in the rows that have not quoted"
            );
            assert!(
                report_status(
                    &pane,
                    IndicesStatus::Failed(IndicesStreamError::Transport(TransportError::Closed)),
                    cx
                ),
                "the pane having stopped working is a change the rows have to hear about"
            );
            assert!(
                !pane.read(cx).table.read(cx).busy,
                "and the rows it did not touch say so"
            );
        });
    }

    #[gpui_kit::test]
    fn switching_the_display_mode_should_redraw_the_rows_the_pane_holds(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            report_quotes(&pane, &[("HSI", 101)], cx);
            pane.update(cx, |pane, cx| pane.set_mode(IndicesDisplayMode::Table, cx));

            assert_eq!(pane.read(cx).config.mode, IndicesDisplayMode::Table);
            assert!(pane.read(cx).has_content());
            assert_eq!(latest_of(&pane, cx), [Some(101)]);
        });
    }

    /// The two halves of the switch read one state, so picking one is what unselects the
    /// other: there is no pair of flags that could disagree.
    #[gpui_kit::test]
    fn the_mode_controls_should_be_exclusive(cx: &mut TestAppContext) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = pane(cx, source, config(&[selected("HSI")]));

        cx.update(|cx| {
            let (grid, table) = pane.update(cx, |pane, cx| pane.mode_controls(cx));
            assert!(grid.is_selected(), "a pane drawing a grid offers the grid");
            assert!(!table.is_selected(), "and not the table");

            pane.update(cx, |pane, cx| pane.set_mode(IndicesDisplayMode::Table, cx));
            let (grid, table) = pane.update(cx, |pane, cx| pane.mode_controls(cx));
            assert!(!grid.is_selected(), "picking one mode unselects the other");
            assert!(table.is_selected());
        });
    }

    #[test]
    fn the_default_config_should_be_a_grid_with_nothing_selected() {
        let config = IndicesConfig::default();

        assert_eq!(config.mode, IndicesDisplayMode::Grid);
        assert!(config.selection.is_empty());
    }

    #[test]
    fn a_config_should_reject_missing_fields() {
        assert!(serde_json::from_str::<IndicesConfig>("{}").is_err());
    }

    #[test]
    fn a_config_should_normalize_its_selection_and_reject_the_next_index() {
        let value = serde_json::to_value(selected("HSI")).expect("a selection should encode");
        let at_limit = serde_json::json!({
            "selection": vec![value.clone(); MAX_SELECTED_INDICES],
            "mode": "Grid",
        });
        let config: IndicesConfig =
            serde_json::from_value(at_limit).expect("the limit should decode");
        assert_eq!(config.selection.len(), 1, "duplicates should normalize");

        let over_limit = serde_json::json!({
            "selection": vec![value; MAX_SELECTED_INDICES + 1],
            "mode": "Grid",
        });
        assert!(serde_json::from_value::<IndicesConfig>(over_limit).is_err());
    }

    #[test]
    fn a_config_should_reject_names_the_manifest_could_not_carry() {
        let value = serde_json::json!({
            "selection": [{
                "topic": "HSI",
                "name": { "en": "x".repeat(MAX_NAME_CHARS + 1), "zh": "valid" }
            }],
            "mode": "Grid",
        });

        assert!(serde_json::from_value::<IndicesConfig>(value).is_err());
    }

    /// A selected index the service no longer carries is dropped, and the pane is told
    /// so it can be written back with the selection it is actually following.
    #[gpui_kit::test]
    fn a_selected_index_the_manifest_dropped_should_go_away(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            let pane = cx.new(|cx| {
                pane_with_manifest(
                    FakeSource::default(),
                    Rc::new(Manifest::carrying(&["SPX"])),
                    config(&[selected("SPX"), selected("HSI")]),
                    cx,
                )
            });

            pane.update(cx, |pane, cx| {
                pane.settle_selection(
                    pane.config.selection.clone(),
                    Ok(proto_types::IndexManifest {
                        categories: Vec::new(),
                        indices: vec![proto_types::IndexDefinition {
                            request_code: ".SPX".to_owned(),
                            code: "SPX".to_owned(),
                            category: "test".to_owned(),
                            name: None,
                        }],
                    }),
                    cx,
                );

                let kept = pane
                    .config
                    .selection
                    .iter()
                    .map(|index| index.topic.code().to_owned())
                    .collect::<Vec<_>>();
                assert_eq!(kept, ["SPX"], "HSI is not in the manifest any more");
            });
        });
    }

    /// A manifest read that failed proves nothing, so the selection is left alone and the
    /// subscription reports its own error.
    #[gpui_kit::test]
    fn a_manifest_read_that_failed_should_leave_the_selection_alone(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            let pane = cx.new(|cx| {
                pane_with_manifest(
                    FakeSource::default(),
                    Rc::new(PendingManifest),
                    config(&[selected("SPX")]),
                    cx,
                )
            });

            pane.update(cx, |pane, cx| {
                pane.settle_selection(
                    pane.config.selection.clone(),
                    Err(KytrixServiceError::Disconnected),
                    cx,
                );

                let kept = pane
                    .config
                    .selection
                    .iter()
                    .map(|index| index.topic.code().to_owned())
                    .collect::<Vec<_>>();
                assert_eq!(
                    kept,
                    ["SPX"],
                    "the service not answering is not the service saying no"
                );
            });
        });
    }

    /// What the service carries is settled before the pane subscribes: the scope it asks for
    /// is the one it settled on, not the one it was handed.
    #[gpui_kit::test]
    fn a_selected_index_the_manifest_dropped_should_never_be_subscribed_to(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let _control = source.subscribe();
        let pane = loaded(
            cx,
            source.clone(),
            Rc::new(Manifest::carrying(&["SPX"])),
            config(&[selected("SPX"), selected("HSI")]),
        );

        settle_until(cx, "the pane to subscribe", |_| {
            source.subscribed().len() == 1
        });
        cx.update(|cx| {
            assert_eq!(
                pane.read(cx)
                    .config
                    .selection
                    .iter()
                    .map(|index| index.topic.code().to_owned())
                    .collect::<Vec<_>>(),
                ["SPX"]
            );
        });
        assert_eq!(
            source.subscribed(),
            vec![vec![topic("SPX")]],
            "the pane subscribed to HSI, which the service no longer carries"
        );
    }

    /// A selection the service carries none of goes back to "nothing chosen", and nothing is
    /// subscribed on its behalf — not even an empty scope.
    #[gpui_kit::test]
    fn a_selection_the_manifest_dropped_entirely_should_subscribe_to_nothing(
        cx: &mut TestAppContext,
    ) {
        let source = FakeSource::default();
        let pane = loaded(
            cx,
            source.clone(),
            Rc::new(Manifest::carrying(&[])),
            config(&[selected("HSI")]),
        );

        settle_until(cx, "the pane to settle its selection", |cx| {
            pane.read(cx)
                .status
                .state(cx)
                .notice()
                .is_some_and(|notice| {
                    notice == &Notice::message("widget.indices.select_at_least_one")
                })
        });
        cx.update(|cx| {
            assert!(pane.read(cx).config.selection.is_empty());
            assert!(
                !pane.read(cx).status.state(cx).is_failed(),
                "a catalogue without the index is not a failure: the pane has nothing chosen"
            );
        });
        assert!(
            source.subscribed().is_empty(),
            "the pane subscribed to an index the service no longer carries"
        );
    }

    /// A missing config is the default, and a present one decodes.
    #[test]
    fn a_missing_config_should_use_the_default_and_a_present_one_should_decode() {
        assert_eq!(
            decode_or_default::<IndicesConfig>(None),
            IndicesConfig::default()
        );

        let stored = VersionedWidgetConfig::encode(&IndicesConfig {
            mode: IndicesDisplayMode::Table,
            ..IndicesConfig::default()
        })
        .expect("the config should encode");
        assert_eq!(
            stored.decode::<IndicesConfig>().mode,
            IndicesDisplayMode::Table
        );
    }

    #[test]
    fn a_flash_should_rise_to_its_peak_and_fall_back_to_nothing() {
        assert_eq!(
            (
                flash_opacity(Duration::ZERO),
                flash_opacity(FLASH_PEAK),
                flash_opacity(FLASH_DURATION),
            ),
            (0.0, 1.0, 0.0)
        );
        assert_eq!(flash_opacity(FLASH_DURATION + Duration::from_secs(1)), 0.0);
    }

    #[test]
    fn a_grid_layout_should_use_every_column_the_width_fits() {
        assert_eq!(
            grid_layout(size(px(800.0), px(300.0)), 5, px(160.0), px(88.0)),
            GridLayout {
                columns: 5,
                rows: 1,
                cell_width: px(160.0),
                cell_height: px(88.0),
                content_width: px(800.0),
                content_height: px(88.0),
            }
        );
    }

    #[test]
    fn a_grid_layout_should_ignore_the_height_when_counting_columns() {
        assert_eq!(
            grid_layout(size(px(500.0), px(100.0)), 10, px(160.0), px(60.0)),
            GridLayout {
                columns: 3,
                rows: 4,
                cell_width: px(160.0),
                cell_height: px(60.0),
                content_width: px(480.0),
                content_height: px(240.0),
            }
        );
    }

    #[test]
    fn a_grid_layout_should_floor_the_width_without_resizing_cells() {
        assert_eq!(
            grid_layout(size(px(479.0), px(176.0)), 5, px(160.0), px(88.0)),
            GridLayout {
                columns: 2,
                rows: 3,
                cell_width: px(160.0),
                cell_height: px(88.0),
                content_width: px(320.0),
                content_height: px(264.0),
            }
        );
    }

    #[test]
    fn an_empty_grid_should_have_no_content() {
        assert_eq!(
            grid_layout(size(px(640.0), px(400.0)), 0, px(160.0), px(88.0)),
            GridLayout {
                columns: 0,
                rows: 0,
                cell_width: px(160.0),
                cell_height: px(88.0),
                content_width: Pixels::ZERO,
                content_height: Pixels::ZERO,
            }
        );
    }

    #[test]
    fn a_grid_should_keep_one_cell_in_no_space_at_all() {
        assert_eq!(
            grid_layout(size(Pixels::ZERO, Pixels::ZERO), 1, px(160.0), px(60.0)),
            GridLayout {
                columns: 1,
                rows: 1,
                cell_width: px(160.0),
                cell_height: px(60.0),
                content_width: px(160.0),
                content_height: px(60.0),
            }
        );
    }

    /// A view that draws nothing but the grid, so what the grid is worth drawing can
    /// be asserted without a pane's own state around it.
    struct GridTestView {
        rows: Arc<[IndicesRow]>,
        flashes: HashMap<IndexTopic, QuoteFlash>,
        scroll: ScrollHandle,
        cell_width: Pixels,
        cell_height: Pixels,
    }

    impl Render for GridTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .overflow_hidden()
                .child(grid_viewport(
                    self.rows.clone(),
                    self.flashes.clone(),
                    false,
                    self.cell_width,
                    self.cell_height,
                    &self.scroll,
                ))
        }
    }

    fn grid_rows(item_count: usize) -> Arc<[IndicesRow]> {
        let selection = (0..item_count)
            .map(|index| selected(&format!("INDEX_{index}")))
            .collect::<Vec<_>>();
        rows_of(&selection, None)
    }

    fn test_grid(
        cx: &mut TestAppContext,
        scroll: &ScrollHandle,
        item_count: usize,
        cell_width: Pixels,
        cell_height: Pixels,
    ) -> Entity<GridTestView> {
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));
            let scroll = scroll.clone();
            cx.new(|_| GridTestView {
                rows: grid_rows(item_count),
                flashes: HashMap::new(),
                scroll,
                cell_width,
                cell_height,
            })
        })
    }

    /// Draws the grid in a frame of its own, with every cache missed, so the bounds it answers with
    /// are this frame's paint actions rather than the ones an earlier frame left behind
    /// (AGENTS.md §10): the grid is drawn inside the body, which is the one cached layer.
    fn draw_grid(
        cx: &mut VisualTestContext,
        viewport: Size<Pixels>,
        view: &Entity<GridTestView>,
    ) -> Bounds<Pixels> {
        cx.update(|window, _| window.refresh());
        let view = view.clone();
        cx.draw(point(Pixels::ZERO, Pixels::ZERO), viewport, |_, _| {
            view.into_any_element()
        });
        cx.debug_bounds("TEST_INDICES_GRID_CONTENT")
            .expect("the grid content should be drawn")
    }

    #[gpui_kit::test]
    fn the_grid_should_lay_out_each_frame_from_its_own_bounds(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let scroll = ScrollHandle::new();
        let view = test_grid(cx, &scroll, 5, px(160.0), px(88.0));

        let wide = draw_grid(cx, size(px(800.0), px(300.0)), &view);
        let narrow = draw_grid(cx, size(px(320.0), px(263.0)), &view);

        assert_eq!(
            (wide.size, narrow.size),
            (size(px(800.0), px(88.0)), size(px(320.0), px(264.0)))
        );
    }

    #[gpui_kit::test]
    fn the_grid_should_center_content_smaller_than_its_viewport(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let scroll = ScrollHandle::new();
        let view = test_grid(cx, &scroll, 2, px(160.0), px(88.0));

        let content = draw_grid(cx, size(px(500.0), px(300.0)), &view);

        assert_eq!(
            (content.origin, scroll.max_offset()),
            (
                point(px(90.0), px(106.0)),
                point(Pixels::ZERO, Pixels::ZERO)
            )
        );
    }

    #[gpui_kit::test]
    fn the_grid_should_start_scrolling_below_one_cells_width(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let scroll = ScrollHandle::new();
        let view = test_grid(cx, &scroll, 5, px(160.0), px(88.0));

        draw_grid(cx, size(px(160.0), px(500.0)), &view);
        let exact = scroll.max_offset().x;
        draw_grid(cx, size(px(159.0), px(500.0)), &view);

        assert_eq!((exact, scroll.max_offset().x), (Pixels::ZERO, px(1.0)));
    }

    #[gpui_kit::test]
    fn the_grid_should_start_scrolling_below_its_content_height(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let scroll = ScrollHandle::new();
        let view = test_grid(cx, &scroll, 5, px(160.0), px(88.0));

        draw_grid(cx, size(px(320.0), px(264.0)), &view);
        let exact = scroll.max_offset().y;
        draw_grid(cx, size(px(320.0), px(263.0)), &view);

        assert_eq!((exact, scroll.max_offset().y), (Pixels::ZERO, px(1.0)));
    }

    #[gpui_kit::test]
    fn a_flash_should_cover_the_top_of_its_cell(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let scroll = ScrollHandle::new();
        let view = test_grid(cx, &scroll, 1, px(160.0), px(88.0));
        view.update(cx, |view, _| {
            view.flashes.insert(
                topic("INDEX_0"),
                QuoteFlash {
                    direction: IndexDirection::Up,
                    started_at: Instant::now(),
                },
            );
        });

        draw_grid(cx, size(px(160.0), px(88.0)), &view);
        let cell = cx
            .debug_bounds("TEST_INDICES_GRID_CELL")
            .expect("the cell should be drawn");
        let flash = cx
            .debug_bounds("TEST_INDICES_GRID_FLASH")
            .expect("the flash should be drawn");

        assert_eq!(
            (cell.size, flash),
            (
                size(px(160.0), px(88.0)),
                Bounds::new(cell.origin, size(px(160.0), px(GRID_FLASH_HEIGHT)))
            )
        );
    }

    #[test]
    fn a_name_should_be_read_in_the_language_the_interface_is_reading() {
        let name = LocalizedIndexName {
            en: SmolStr::new_static("Hang Seng Index"),
            zh: SmolStr::new_static("恒生指数"),
        };

        assert_eq!(name.get(LanguageCode::En), "Hang Seng Index");
        assert_eq!(name.get(LanguageCode::ZhCn), "恒生指数");
        assert_eq!(
            name.get(LanguageCode::Ja),
            "Hang Seng Index",
            "a manifest carries two names, so every other locale reads English"
        );
    }

    /// Every message this pane can show, in one place.
    ///
    /// A key that is missing from a locale translates to an empty string rather than to
    /// an error, so a gap in the interface is what a typo looks like: this is the test
    /// that turns it into a failure.
    #[gpui_kit::test]
    fn every_message_this_pane_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(cx, Some(ACCOUNT));

            for key in [
                "widget.names.indices",
                "widget.indices.change",
                "widget.indices.change_percent",
                "widget.indices.choose",
                "widget.indices.grid",
                "widget.indices.index",
                "widget.indices.latest",
                "widget.indices.logged_out",
                "widget.indices.market_closed",
                "widget.indices.select_at_least_one",
                "widget.indices.selector.empty",
                "widget.indices.selector.load_failed",
                "widget.indices.selector.loading",
                "widget.indices.table",
            ] {
                assert!(!t!(cx, key).is_empty(), "{key} has no copy");
            }

            let failures = [
                IndicesFailure::Stream(IndicesStreamError::Service(KytrixServiceError::Closed)),
                IndicesFailure::Stream(IndicesStreamError::Transport(TransportError::Closed)),
                IndicesFailure::Stream(IndicesStreamError::IndexLimit { limit: Some(3) }),
                IndicesFailure::Stream(IndicesStreamError::IndexLimit { limit: None }),
                IndicesFailure::Rpc(RpcCallError::Service {
                    request_id: RequestId::new(1),
                    source: IndicesServiceError::Data(IndicesDataError::SelectionCapacity {
                        limit: MAX_SELECTED_INDICES,
                    }),
                }),
            ];
            for failure in failures {
                assert!(
                    !failure.localized_message(cx).is_empty(),
                    "{failure:?} has no copy"
                );
            }

            let reasons = [
                IndicesInvalidReason::InvalidCode,
                IndicesInvalidReason::DuplicateCategory {
                    code: SmolStr::new_static("asia"),
                },
                IndicesInvalidReason::DuplicateIndex {
                    code: SmolStr::new_static("HSI"),
                },
                IndicesInvalidReason::UnknownCategory {
                    index: SmolStr::new_static("HSI"),
                    category: SmolStr::new_static("missing"),
                },
                IndicesInvalidReason::MissingName,
                IndicesInvalidReason::NameFormat,
            ];
            for reason in reasons {
                let error = IndicesDataError::Invalid(reason);
                assert!(
                    !error.localized_message(cx).is_empty(),
                    "{error:?} has no copy"
                );
            }
            assert!(
                !IndicesDataError::ManifestCapacity { limit: 4_096 }
                    .localized_message(cx)
                    .is_empty()
            );
        });
    }
}
