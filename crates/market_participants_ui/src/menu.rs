//! The market participants entry in the window header: the venue list this build speaks, and the
//! one action that loads each of them.
//!
//! # What it owns
//!
//! The list of venues, and what each of them is doing. The list is the *venue list* — what
//! `supported_venues()` answers — so it is the same whether or not anything is loaded, and
//! whether a venue is loaded is read from the loaded venues (`LoadedVenues`) every time the rows
//! are built rather than remembered. The activity is this menu's own memory: a load it started and
//! has not heard the end of, or the failure the last one reported. Nothing is persisted:
//! loading a venue is something this run did, not a setting.
//!
//! # What it does not own
//!
//! The venues themselves. Committing a venue's scopes, catalogs and metadata is
//! [`MarketParticipantsConnector::load_venue`], and this menu is its only caller: the user's press is
//! the one event that makes the program talk to a market. The load runs on the
//! application Tokio runtime and reports back through a GPUI task, because the executor a
//! view runs on has no reactor to drive a request.
//!
//! A failure is reported twice and never hidden: the row keeps saying the venue failed to
//! load, and the reason itself is opened once in an alert. A success says nothing — the row
//! already reads "refresh" because the venue is in the loaded venues now.

use ahash::AHashMap;
use base_ui::{
    brand::Brand,
    button::Button,
    dropdown::Dropdown,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    layer::LayerExt as _,
    menu::MenuItem,
    size::{UnifiedSizable, UnifiedSize},
    spinner::Spinner,
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity,
    Window,
    base::{Align, Disableable},
    div, px,
};
use i18n::t;
use market_participants_connector::{
    connector::{MarketParticipantsConnector, VenueLoadError},
    error::MarketParticipantError,
    venue::loaded::VenuesUpdated,
};
use market_participants_model::{display::lexical::scope_display_code, identifier::VenueId};
use smol_str::format_smolstr;
use tokio::task::JoinError;
use widget::{error::LocalizedError, venue::VenueDisplay};
use widget_tokio_runtime::global;

/// The element id of the header entry: the trigger, its panel and the state behind them.
const MENU_ID: &str = "market-participants-menu";

/// The sibling scopes every row is built under: they are told apart by the index the venue has
/// in the venue list, which is stable for as long as that list is.
const ROW_ID: &str = "market-participants-row";
const ACTION_ID: &str = "market-participants-action";
/// The mark at the left of a row's action, whatever it holds.
const MARK_ID: &str = "market-participants-mark";
const SPINNER_ID: &str = "market-participants-spinner";

/// How wide the rows of the panel are.
///
/// A [`Dropdown`] sizes its panel from its trigger, and this menu's trigger is a button
/// narrower than a venue line, so the width the panel takes is stated by its rows. It is a
/// fixed width because the panel is a column of venue lines with an action at the end of each
/// of them: a panel that resized as a load reported itself would move the buttons under the
/// pointer that opened it.
const MENU_WIDTH: f32 = 280.0;

/// The room the mark at the left of a row's action takes.
///
/// It is stated by this menu because neither thing the mark holds sizes itself: a [`Spinner`]
/// fills whatever box it is drawn in, and the connection icon is drawn at this size. One box
/// for both is what keeps a row's action where it is as the row's state changes.
const MARK_SIZE: f32 = 14.0;

/// How wide a row's action is.
///
/// The action is one control whatever a venue's state makes of it — the load a venue nothing
/// has loaded offers, the refresh a loaded one does — and it states its own width so that the
/// two labels are drawn in the same box: a control that resized as a load reported itself
/// would move the button under the pointer that pressed it.
const ACTION_WIDTH: f32 = 64.0;

/// What one venue's row is doing, as this menu remembers it.
///
/// There is no `Idle`: a venue nothing is happening to is not doing anything, which is what
/// having no entry here says.
#[derive(Clone, Debug)]
enum Activity {
    /// A load is in flight for this venue.
    Loading,
    /// The last load failed: the line the row keeps under the venue's name.
    ///
    /// The state is the whole of what is remembered. The sentence the line reads is
    /// read when the row is *drawn* rather than when the failure was reported,
    /// because it names the venue: a sentence resolved at report time would keep the
    /// language it was reported in, and the row it sits under would name the same
    /// venue in the language the interface is showing now. What the failure actually
    /// said is a fact about that moment and is stated in full by the alert that was
    /// raised when it happened.
    Failed,
}

/// One line of the panel: a venue this build speaks, and whether a load committed it.
struct VenueRow {
    venue: VenueId,
    /// Whether the venue is loaded, read from the loaded venues when the row was built.
    loaded: bool,
}

/// Why one row's action failed.
///
/// A load reports its own typed failure; a task that never reported at all is the other
/// answer, and it is what is left to say when the load said nothing.
#[derive(Debug, thiserror::Error)]
enum LoadFailure {
    #[error("{0}")]
    Venue(#[from] VenueLoadError),
    #[error("the task loading {venue} did not finish: {source}")]
    Interrupted {
        venue: VenueId,
        #[source]
        source: JoinError,
    },
}

impl LocalizedError for LoadFailure {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Venue(error) => match error {
                // Each variant of a load's failure is a sentence of its own: the stage that
                // failed is what the row has to report, and a scope-carrying stage names the
                // scope it failed on.
                VenueLoadError::Discovery { venue, source } => t!(
                    cx,
                    "error.venue_load.discovery",
                    market = venue.display_string(cx),
                    cause = source.localized_message(cx)
                ),
                VenueLoadError::DiscoveryUnverifiable { venue, dex } => t!(
                    cx,
                    "error.venue_load.discovery_unverifiable",
                    market = venue.display_string(cx),
                    dex = dex.as_str()
                ),
                VenueLoadError::DiscoveryLimitExceeded {
                    venue,
                    named,
                    limit,
                } => t!(
                    cx,
                    "error.venue_load.discovery_limit_exceeded",
                    market = venue.display_string(cx),
                    named = *named,
                    limit = *limit
                ),
                VenueLoadError::Registration { venue, .. } => t!(
                    cx,
                    "error.venue_load.registration",
                    market = venue.display_string(cx)
                ),
                // The metadata stage is a venue-level sentence in the error type's own words —
                // its `Display` names the venue and not the scope — so the row states the same
                // thing the typed failure does. The scope the variant carries is a caller's
                // answer to act on, not part of what the row says.
                VenueLoadError::Metadata { venue, source, .. } => t!(
                    cx,
                    "error.venue_load.metadata",
                    market = venue.display_string(cx),
                    cause = source.localized_message(cx)
                ),
                VenueLoadError::Catalog {
                    venue,
                    scope,
                    source,
                } => match source.as_ref() {
                    // A catalog that moved under its fetch is reported as the moved catalog it
                    // is — the venue's own stage answer — rather than as a read that failed:
                    // what the user has to do is load the market again.
                    MarketParticipantError::Catalog(error)
                        if matches!(
                            error.as_ref(),
                            market_participants_connector::catalog::error::CatalogError::RouteRevisionChanged { .. }
                        ) =>
                    {
                        t!(
                            cx,
                            "error.venue_load.revision_changed",
                            market = venue.display_string(cx),
                            scope = scope_display_code(scope)
                        )
                    }
                    cause => t!(
                        cx,
                        "error.venue_load.catalog",
                        market = venue.display_string(cx),
                        scope = scope_display_code(scope),
                        cause = cause.localized_message(cx)
                    ),
                },
                VenueLoadError::CatalogValidation {
                    venue,
                    scope,
                    error,
                } => t!(
                    cx,
                    "error.venue_load.catalog_validation",
                    market = venue.display_string(cx),
                    scope = scope_display_code(scope),
                    cause = error.localized_message(cx)
                ),
                VenueLoadError::RevisionChanged { venue, scope } => t!(
                    cx,
                    "error.venue_load.revision_changed",
                    market = venue.display_string(cx),
                    scope = scope_display_code(scope)
                ),
                // A cancelled load is the user's own doing, and the row says so without
                // dressing it up as a failure of the venue.
                VenueLoadError::Interrupted { venue } => {
                    t!(
                        cx,
                        "error.venue_load.interrupted",
                        market = venue.display_string(cx)
                    )
                }
                // A venue the definition table answers no definition for is one this build
                // draws no row for, so this is a load started against a venue the menu was
                // never able to offer: the sentence names the venue rather than reporting a
                // stage that never ran.
                VenueLoadError::UnsupportedVenue { venue } => t!(
                    cx,
                    "error.venue_load.unsupported_venue",
                    market = venue.display_string(cx)
                ),
            },
            // A load that did not finish has no venue-level reason to name: the line its row
            // shows is all this program can say about it.
            Self::Interrupted { venue, .. } => t!(
                cx,
                "window.market_participants.failed",
                market = venue.display_string(cx)
            ),
        }
    }
}

/// The market participants entry the window header mounts.
pub struct MarketParticipants {
    connector: MarketParticipantsConnector,
    /// The rows the panel draws, rebuilt from the static venue list and the loaded venues.
    rows: Vec<VenueRow>,
    /// What each venue is doing, if anything.
    activity: AHashMap<VenueId, Activity>,
    /// The load in flight, one per venue.
    ///
    /// One entry per venue rather than one for the menu, because venues are independent:
    /// a single slot would drop the load of the venue that was pressed first as soon
    /// as a second row was pressed, leaving that venue to be reported by nobody.
    tasks: AHashMap<VenueId, Task<()>>,
    /// Whether the panel is open, which the overlay writes and the trigger reads.
    open_state: Entity<bool>,
}

impl MarketParticipants {
    /// Builds the menu the application mounts, reading the market participants root from the globals
    /// the composition root installed.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let connector = cx.global::<MarketParticipantsConnector>().clone();
        let open_state = cx.new(|_| false);

        // Whether the panel is open lives in a state entity the overlay writes and the
        // trigger reads, so this menu follows it: a press that opens the panel has to reach
        // the frame that draws it, and an open panel has to show what the loaded venues hold
        // *now* rather than what they held the last time it was opened.
        let opened = open_state.clone();
        cx.observe(&opened, |menu, state, cx| {
            if *state.read(cx) {
                menu.rebuild_rows();
            }
            cx.notify();
        })
        .detach();

        // A commit changes what a row offers: the venue that just loaded offers a refresh where
        // it offered a load, and a load that failed leaves the row as it was. The panel follows
        // the commits rather than waiting for the next press of its trigger, so the row the user
        // pressed reports what their own action did.
        let events = connector.venues().events();
        cx.subscribe(&events, |menu, _, _: &VenuesUpdated, cx| {
            menu.rebuild_rows();
            cx.notify();
        })
        .detach();

        let mut menu = Self {
            connector,
            rows: Vec::new(),
            activity: AHashMap::with_capacity(4),
            tasks: AHashMap::with_capacity(4),
            open_state,
        };
        menu.rebuild_rows();
        menu
    }

    /// Re-reads the venue list and what is loaded.
    ///
    /// The static venue list is what the panel lists — every venue this build can load,
    /// whether or not it is loaded — and the loaded venues answer which of them are. It
    /// is the only routing view: a venue is loaded exactly when its whole scope set,
    /// catalog and metadata committed, so "in the list" and "fully usable" are the
    /// same statement. Neither answer is cached across a look: a load changes the loaded
    /// venues, and the row that offered the load reads the answer back rather than
    /// assuming it.
    fn rebuild_rows(&mut self) {
        self.rows = self
            .connector
            .supported_venues()
            .into_iter()
            .map(|venue| {
                let loaded = self.connector.venues().loaded(&venue).is_some();
                VenueRow { venue, loaded }
            })
            .collect();
    }

    /// Starts the load one row's action asks for.
    ///
    /// The request itself is what `load_venue` builds; this hands it to the application
    /// runtime, because the executor a view runs on has no reactor and a network future
    /// awaited there panics inside the market participant instead of being sent.
    ///
    /// A venue already loading is left alone. The row in flight attaches no action at all,
    /// and this is the same rule stated where the load is started: two presses delivered
    /// inside one frame both reach the handler the previous frame drew, and a second load
    /// would run beside the first one and report over it.
    ///
    /// A venue that would be unreadable without credentials is refused before the load is
    /// built: the load itself would succeed — its directory is public — and every pane that
    /// then read the market would fail on the credential, so what the user needs is to
    /// configure the venue rather than to watch a load finish. The question is the market participant
    /// root's, which is this menu's only source of configuration; a venue whose reads are
    /// public answers no and is offered exactly as it was.
    fn start_load(&mut self, venue: VenueId, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.activity.get(&venue), Some(Activity::Loading)) {
            return;
        }
        if self.connector.load_needs_credentials(venue) {
            let market = venue.display_string(cx);
            let title = t!(
                cx,
                "window.market_participants.credentials_required_title",
                market = market
            );
            let message = t!(
                cx,
                "window.market_participants.credentials_required_message",
                market = market
            );
            let confirm = t!(cx, "common.confirm");
            window.alert(cx, move |alert, _, _| {
                alert.ok(title, message).ok_text(confirm)
            });
            return;
        }
        self.activity.insert(venue, Activity::Loading);

        // The window the press came from is the window the report is made in: the alert
        // belongs where the user is looking, and the task that has to raise it has no window
        // of its own.
        let window_handle = window.window_handle();
        let load = global::handle(cx).run(self.connector.load_venue(venue));
        let reported = venue;
        let task = cx.spawn(async move |menu, cx| {
            let outcome = load.await;
            let _ = cx.update_window(window_handle, move |_, window, cx| {
                let _ = menu.update(cx, |menu, cx| {
                    menu.finish_load(&reported, outcome, window, cx)
                });
            });
        });
        self.tasks.insert(venue, task);
        cx.notify();
    }

    /// Accepts the outcome of one load.
    ///
    /// A success is silent: the row's label follows the loaded venues, which hold the venue
    /// now, and the user is looking at the row. A failure is reported once — in the alert — and
    /// stays on the row until the next load says otherwise. Whether the venue is loaded
    /// is not this menu's answer to give: a first connection that failed is not registered
    ///, so its row still offers to load, while a refresh that failed leaves
    /// everything that was loaded exactly as it was, so its row still offers to refresh.
    ///
    /// A load whose task was cancelled reports nothing, and it is not left in flight either:
    /// the row goes back to offering the action it was asked for rather than keeping a
    /// progress mark nothing will ever finish.
    fn finish_load(
        &mut self,
        venue: &VenueId,
        outcome: Result<Result<(), VenueLoadError>, JoinError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tasks.remove(venue);

        let failure = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(LoadFailure::Venue(error)),
            // The task the window was driving is gone: whatever happened is not this menu's
            // to report any more. The row it was started from is still there, though, and it
            // has to offer the venue's action again: `activity` is the only thing that says a
            // venue is loading and a loading row attaches no action at all, so a venue left
            // loading here is a venue no press can reach for the rest of the window's life.
            Err(error) if error.is_cancelled() => {
                self.activity.remove(venue);
                self.rebuild_rows();
                cx.notify();
                return;
            }
            Err(source) => Some(LoadFailure::Interrupted {
                venue: *venue,
                source,
            }),
        };

        match failure {
            Some(failure) => {
                log::warn!("loading {venue} failed: {failure}");
                self.activity.insert(*venue, Activity::Failed);

                let title = t!(
                    cx,
                    "window.market_participants.failed",
                    market = venue.display_string(cx)
                );
                let message = failure.localized_message(cx);
                let confirm = t!(cx, "common.confirm");
                window.alert(cx, move |alert, _, _| {
                    alert.ok(title, message).ok_text(confirm)
                });
            }
            None => {
                self.activity.remove(venue);
            }
        }

        self.rebuild_rows();
        cx.notify();
    }

    /// One row of the panel: the venue, what it is doing, and the action it offers.
    ///
    /// The action is the only part of a row that takes input, and what it is comes from
    /// `activity`: a venue whose load is in flight offers nothing to press. Greying the
    /// button is not enough — a disabled control still takes the pointer — so the handler is
    /// not attached while the load it would start is already running.
    ///
    /// The mark left of the action tells the two apart at a glance: a load in flight is shown
    /// by the progress mark and nothing else, while a row that is not loading shows the
    /// connection its action is about — a venue nothing has loaded offers to plug in, a loaded
    /// one is plugged in already.
    fn row(&self, menu: &WeakEntity<Self>, index: usize, row: &VenueRow, cx: &App) -> AnyElement {
        let venue = row.venue;
        let market = venue.display_string(cx);

        // The logo is the venue's own, which is a fact about the identity and not about
        // the name it is drawn under: matching the drawn name would blank the logo the
        // moment the name changed language.
        let brand_logo = match venue {
            VenueId::Binance => Brand::binance(),
            VenueId::Okx => Brand::okx(),
            VenueId::Hyperliquid => Brand::hyperliquid(),
            VenueId::Longbridge => Brand::longbridge(),
        }
        .size_4();

        let loading = matches!(self.activity.get(&venue), Some(Activity::Loading));
        let (label, state) = match row.loaded {
            true => (t!(cx, "window.market_participants.refresh"), "REFRESH"),
            false => (t!(cx, "window.market_participants.load"), "LOAD"),
        };
        let action_selector = format_smolstr!("TEST_MARKET_PARTICIPANTS_ACTION_{state}_{venue:?}");
        let mut action = Button::new((ACTION_ID, index))
            .filled()
            .small()
            .w(px(ACTION_WIDTH))
            .flex_none()
            .debug_selector(move || action_selector.to_string())
            .child(div().flex_1().text_center().child(label));
        if loading {
            action = action.disabled(true);
        } else {
            let menu = menu.clone();
            let pressed = venue;
            action = action.on_click(move |_, window, cx| {
                let _ = menu.update(cx, |menu, cx| menu.start_load(pressed, window, cx));
            });
        }

        let mark = match (loading, row.loaded) {
            (true, _) => {
                let selector = format_smolstr!("TEST_MARKET_PARTICIPANTS_LOADING_{venue:?}");
                div()
                    .id((MARK_ID, index))
                    .size(px(MARK_SIZE))
                    .aria_label(t!(
                        cx,
                        "window.market_participants.loading",
                        market = market
                    ))
                    .debug_selector(move || selector.to_string())
                    .child(Spinner::new((SPINNER_ID, index)))
                    .into_any_element()
            }
            (false, true) => {
                let selector = format_smolstr!("TEST_MARKET_PARTICIPANTS_LOADED_{venue:?}");
                div()
                    .id((MARK_ID, index))
                    .size(px(MARK_SIZE))
                    .aria_label(t!(cx, "window.market_participants.loaded", market = market))
                    .debug_selector(move || selector.to_string())
                    .child(Icon::new(IconKind::PlugsConnected).size(px(MARK_SIZE)))
                    .into_any_element()
            }
            (false, false) => {
                let selector = format_smolstr!("TEST_MARKET_PARTICIPANTS_UNLOADED_{venue:?}");
                div()
                    .id((MARK_ID, index))
                    .size(px(MARK_SIZE))
                    .aria_label(t!(
                        cx,
                        "window.market_participants.unloaded",
                        market = market
                    ))
                    .debug_selector(move || selector.to_string())
                    .child(Icon::new(IconKind::Plugs).size(px(MARK_SIZE)))
                    .into_any_element()
            }
        };

        let mut line = div().flex_1().min_w_0().flex().flex_col().child(
            div()
                .truncate()
                .text_size(UnifiedSize::Small.text_size())
                .child(market.clone()),
        );
        if let Some(Activity::Failed) = self.activity.get(&venue) {
            let failed_selector = format_smolstr!("TEST_MARKET_PARTICIPANTS_FAILED_{venue:?}");
            line = line.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().roles.status.danger.base.hsla)
                    .debug_selector(move || failed_selector.to_string())
                    .child(t!(cx, "window.market_participants.failed", market = market)),
            );
        }

        let row_selector = format_smolstr!("TEST_MARKET_PARTICIPANTS_ROW_{venue:?}");
        div()
            .id((ROW_ID, index))
            .debug_selector(move || row_selector.to_string())
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(brand_logo)
            .child(line)
            .child(mark)
            .child(action)
            .into_any_element()
    }
}

impl Render for MarketParticipants {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = t!(cx, "window.market_participants.title");
        let menu = cx.entity().downgrade();

        // The panel's rows are built here, once per frame the menu draws, and handed to the
        // dropdown: the menu they are entries of is built with it, and a row is a picture of
        // `rows` + `activity` as they are now rather than an element kept between frames.
        let mut dropdown = Dropdown::new(MENU_ID)
            .outline()
            .medium()
            // The header row is the window's drag area, so a control in it takes the pointer
            // explicitly or a press on it would move the window instead.
            .occlude()
            .aria_label(label.clone())
            .debug_selector(|| "TEST_MARKET_PARTICIPANTS_TRIGGER".into())
            .open_state(&self.open_state)
            .align(Align::End)
            .child(Icon::new(IconKind::Graph).size_4())
            .child(label);

        for (index, row) in self.rows.iter().enumerate() {
            // The panel takes its width from the trigger, which is a button narrower than a
            // venue line, so each row states the width the panel is to take.
            dropdown = dropdown.menu_item(MenuItem::Custom(
                div()
                    .w(px(MENU_WIDTH))
                    .child(self.row(&menu, index, row, cx))
                    .into_any_element(),
            ));
        }

        dropdown
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use app_config::{
        market_participants::{
            credentials::LongbridgeCredentials,
            fixture::{self, FixtureAddress},
            settings::VenueSettings,
        },
        network::ProxyRoute,
        settings::Config,
    };
    use app_config_provider::notification::ConfigNotification;
    use base_ui::{animation::ANIMATION_TICK, layer::Layer};
    use gpui_kit::{
        AppContext as _, Context, Entity, IntoElement, Modifiers, MouseButton, ParentElement as _,
        Render, SharedString, Styled as _, TestAppContext, VisualTestContext, Window, div, px,
        size,
    };
    use i18n::{language::codes::LanguageCode, t, translator::types::I18n};
    use market_participants_connector::{
        catalog::{
            batch::CatalogEntry,
            error::{CatalogError, SourceError},
            source::RouteInputRevision,
            store::Catalog,
        },
        connector::{
            MarketParticipantsConnector, MarketParticipantsConnectorInput, VenueLoadError,
        },
        error::MarketParticipantError,
        routing::{DescriptorError, RegistryError},
        runtime::MarketParticipantNetworkSnapshot,
    };
    use market_participants_model::{
        display::lexical::scope_display_code,
        identifier::{VenueId, VenueScopeId},
        listing::route::VenueRoute,
    };
    use network::transport::hub::{TransportHub, TransportPolicy};
    use smol_str::{SmolStr, format_smolstr};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::{TcpListener, TcpStream},
        sync::watch,
        task::JoinHandle,
    };
    use widget::{error::LocalizedError, venue::VenueDisplay};
    use widget_tokio_runtime::{global, runtime::Runtime};

    use super::{Activity, LoadFailure, MarketParticipants};

    const TRIGGER: &str = "TEST_MARKET_PARTICIPANTS_TRIGGER";
    /// The box the panel is drawn in. The panel belongs to the dropdown this menu is, so the
    /// box is named by the dropdown rather than by this menu.
    const PANEL: &str = "TEST_DROPDOWN_PANEL";
    const ALERT: &str = "BASE_UI_ALERT";
    const ALERT_OK: &str = "BASE_UI_ALERT_OK";

    const BINANCE_ROW: &str = "TEST_MARKET_PARTICIPANTS_ROW_Binance";
    const OKX_ROW: &str = "TEST_MARKET_PARTICIPANTS_ROW_Okx";
    const HYPERLIQUID_ROW: &str = "TEST_MARKET_PARTICIPANTS_ROW_Hyperliquid";
    const LONGBRIDGE_ROW: &str = "TEST_MARKET_PARTICIPANTS_ROW_Longbridge";

    const OKX_LOAD: &str = "TEST_MARKET_PARTICIPANTS_ACTION_LOAD_Okx";
    const OKX_REFRESH: &str = "TEST_MARKET_PARTICIPANTS_ACTION_REFRESH_Okx";
    const OKX_FAILED: &str = "TEST_MARKET_PARTICIPANTS_FAILED_Okx";
    const OKX_LOADING: &str = "TEST_MARKET_PARTICIPANTS_LOADING_Okx";

    const BINANCE_LOAD: &str = "TEST_MARKET_PARTICIPANTS_ACTION_LOAD_Binance";
    const BINANCE_FAILED: &str = "TEST_MARKET_PARTICIPANTS_FAILED_Binance";
    const BINANCE_LOADING: &str = "TEST_MARKET_PARTICIPANTS_LOADING_Binance";

    const LONGBRIDGE_LOAD: &str = "TEST_MARKET_PARTICIPANTS_ACTION_LOAD_Longbridge";
    const LONGBRIDGE_LOADING: &str = "TEST_MARKET_PARTICIPANTS_LOADING_Longbridge";

    const HYPERLIQUID_LOAD: &str = "TEST_MARKET_PARTICIPANTS_ACTION_LOAD_Hyperliquid";

    /// The venues whose origins point at the loopback fixture.
    ///
    /// A harness configures every venue this build speaks — the ones a test drives with the
    /// fixture's origin, and every other one with an origin nothing answers on — so a load a test
    /// starts cannot leave the process.
    const OKX_VENUES: &[VenueId] = &[VenueId::Okx];

    /// The Binance venue, which the two-venue case drives.
    const BINANCE_VENUES: &[VenueId] = &[VenueId::Binance];

    /// The three venues whose reads are public, and so the three the panel offers without a
    /// credentials gate.
    const PUBLIC_VENUES: &[VenueId] = &[VenueId::Binance, VenueId::Okx, VenueId::Hyperliquid];

    /// The origin every venue a test does not drive resolves to: a loopback port nothing listens
    /// on, so a load through it fails in this process rather than reaching a real venue.
    const UNREACHABLE: &str = "http://127.0.0.1:1/";

    /// What the fixture answers a request none of its answers covers.
    ///
    /// A body no catalog protocol decodes, so the load fails on the venue's own answer: a
    /// fixture that serves nothing stands for a venue that refuses the load, not for one that
    /// published nothing.
    const REFUSED: &str = r#""the fixture serves no answer for this request""#;

    /// How long a test waits for a load it started to report before it fails.
    const LOAD_TIMEOUT: Duration = Duration::from_secs(10);

    /// The catalog answers of the OKX routes, as the venue's own protocol spells them.
    ///
    /// The same wire shapes
    /// `crates/market_participants_connector/tests/display_names.rs` serves to the same
    /// adapter: the fixture is the wire, so a load that finishes against it finished
    /// against the venue's protocol rather than against a stub of the adapter's functions.
    const OKX_CATALOGS: &[FixtureAnswer] = &[
        FixtureAnswer {
            path: "/api/v5/public/instruments",
            parameter: "instType=SPOT",
            body: r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT","baseCcy":"BTC","quoteCcy":"USDT","tickSz":"0.1","lotSz":"0.000001","minSz":"0.00001","maxLmtSz":"1000","state":"live","listTime":"1500000000000"}]}"#,
        },
        FixtureAnswer {
            path: "/api/v5/public/instruments",
            parameter: "instType=SWAP",
            body: r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","baseCcy":"","quoteCcy":"","settleCcy":"USDT","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","maxLmtSz":"10000","ctVal":"0.01","ctType":"linear","uly":"BTC-USDT","instFamily":"BTC-USDT","state":"live"}]}"#,
        },
    ];

    /// One built-in venue identity.
    fn venue(name: &str) -> VenueId {
        VenueId::from_name(name).expect("the built-in venue identity is canonical")
    }

    fn okx() -> VenueId {
        venue("okx")
    }

    /// The other venue the two-venue case drives.
    fn binance() -> VenueId {
        venue("binance")
    }

    /// One built-in route of the venue the tests drive.
    fn okx_spot() -> VenueRoute {
        VenueRoute::new(
            okx(),
            VenueScopeId::new("spot").expect("the built-in scope is canonical"),
        )
    }

    /// The venue's other built-in route, which a load fetches after the first one.
    fn okx_swap() -> VenueRoute {
        VenueRoute::new(
            okx(),
            VenueScopeId::new("swap").expect("the built-in scope is canonical"),
        )
    }

    /// Where a harness's venue origins resolve.
    ///
    /// A venue is configured as a whole, so a harness points whole venues at its loopback
    /// fixture: the venues it names resolve to the fixture's own origin, and every other venue
    /// this build speaks resolves to a loopback port nothing answers on. No test in this module
    /// may leave the process.
    struct Origins {
        /// The loopback sockets a driven venue answers on, when a test drives one.
        fixture: Option<FixtureAddress>,
        /// The venues that resolve to `fixture`.
        loopback: Vec<VenueId>,
    }

    impl Origins {
        /// No venue resolves to a fixture: every venue this build speaks is reached through
        /// [`UNREACHABLE`], so a load fails in this process.
        const OFFLINE: Self = Self {
            fixture: None,
            loopback: Vec::new(),
        };

        /// The venues named resolve to a loopback venue; every other venue resolves to
        /// [`UNREACHABLE`].
        fn at(fixture: &str, loopback: &[VenueId]) -> Self {
            Self {
                fixture: Some(FixtureAddress::at(
                    &fixture.parse().expect("the fixture origin is a URL"),
                )),
                loopback: loopback.to_vec(),
            }
        }

        /// The sockets every venue of this build resolves to here.
        ///
        /// The driven venues answer on the fixture's pair, and every other venue is reached on
        /// [`UNREACHABLE`]. Both are loopback, which is what keeps a load in this process.
        fn every_venue(&self) -> Vec<(VenueId, FixtureAddress)> {
            market_participants_connector::venue::definition::supported_venues()
                .into_iter()
                .map(|venue| {
                    let address = self.address_of(&venue);
                    (venue, address)
                })
                .collect()
        }

        /// The sockets one venue resolves to here.
        fn address_of(&self, venue: &VenueId) -> FixtureAddress {
            match &self.fixture {
                Some(fixture) if self.loopback.contains(venue) => fixture.clone(),
                _ => FixtureAddress::at(&UNREACHABLE.parse().expect("the dead port is a URL")),
            }
        }
    }

    /// The market participants root a test drives, with every venue's origins resolving as `origins` says.
    ///
    /// The root's own work is handed to the process-wide test runtime rather than to the
    /// runtime the harness installs for the window, so a test can pull the runtime a *load* is
    /// driven on out from under the menu without taking the venue conversation with it.
    ///
    /// The configuration is what carries the fixture addresses, so the root dials them for as
    /// long as it lives: there is nothing to hold beside it. It also carries `credentials`, when
    /// a test supplies one, because that is the one thing the load gate answers from: a test
    /// that says "this venue is configured" says it through the same field the settings page
    /// writes, rather than through a seam beside the question.
    ///
    /// The credential values are placeholders. Nothing here asserts on them, and the venue they
    /// belong to is reached at a loopback socket, so no request of a test carries them anywhere.
    fn market_participants_root(
        loaded_venues: &[VenueId],
        origins: &Origins,
        credentials: Option<(VenueId, LongbridgeCredentials)>,
    ) -> MarketParticipantsConnector {
        offline_symbol_directory();
        let mut config = Config::default();
        config.venues = fixture::venues(origins.every_venue()).into_venues();
        for venue_config in config.venues.values_mut() {
            venue_config.set_proxy(Some(ProxyRoute::System));
        }
        if let Some((venue, credentials)) = credentials {
            let Some(VenueSettings::Longbridge(settings)) = config.venues.get_mut(&venue) else {
                panic!("{venue} is the venue whose credentials the fixture fills in");
            };
            settings.credentials = credentials;
        }
        let snapshot = Arc::new(MarketParticipantNetworkSnapshot::new(config.clone()));
        let notification = ConfigNotification::new(snapshot);
        // A fixture speaks plain HTTP on a loopback socket, which the transport has to be told
        // is allowed; which origin a request addresses is the venue configuration's answer, and
        // the fixture asserts that every venue this build speaks resolves to a loopback address.
        let policy = TransportPolicy {
            allow_plaintext: true,
            extra_root_certificates: Vec::new(),
        };
        let connector = MarketParticipantsConnector::new_with_catalog(
            MarketParticipantsConnectorInput::new(
                Arc::new(TransportHub::new(policy)),
                notification,
            )
            .with_runtime(Runtime::for_test()),
            Arc::new(Catalog::new()),
        )
        .expect("the market participants root builds from typed runtime inputs");
        connector
            .load_fixture(
                &market_participants_connector::test_support::definitions(loaded_venues),
                &[],
                &[],
            )
            .expect("the routes of the loaded venues load");
        assert_every_venue_resolves_in_process(&config);
        connector
    }

    /// Points every symbol-directory read of this module at the dead port, once.
    ///
    /// The venue whose directory the market participant publishes reads it from a host of its own rather
    /// than from the venue's origin, so the fixture addresses cannot reach it: without this seam
    /// a load of that venue would leave the process, which is the one thing no test in this
    /// module may do. The install is process-wide and held for the life of the process because
    /// the loads outlive the function that built their root.
    fn offline_symbol_directory()
    -> &'static market_participants_connector::test_support::SymbolDirectoryRootGuard {
        static OFFLINE: std::sync::OnceLock<
            market_participants_connector::test_support::SymbolDirectoryRootGuard,
        > = std::sync::OnceLock::new();
        OFFLINE.get_or_init(|| {
            market_participants_connector::test_support::set_symbol_directory_root(
                UNREACHABLE.parse().expect("the dead port is a URL"),
            )
        })
    }

    /// Asserts that every origin of every venue this build speaks is a loopback address.
    ///
    /// The build resolves a request through the venue configuration's own origins, so a venue
    /// this fixture did not configure is a load that could reach a real venue: what is asserted
    /// is the resolution rather than the ids of a list, so a venue added to the build later fails
    /// a test here rather than quietly turning one into a request somewhere on the network.
    fn assert_every_venue_resolves_in_process(config: &Config) {
        let mut checked = 0;
        for venue in market_participants_connector::venue::definition::supported_venues() {
            let venue_config = config.venue(&venue).unwrap_or_else(|| {
                panic!(
                    "this build speaks {}, which this fixture does not configure: a load through \
                     it would resolve to the build's built-in origin",
                    venue.as_str()
                )
            });

            for scope in venue_config.scope_ids() {
                let Some(pair) = venue_config.origins(scope) else {
                    continue;
                };
                for origin in [&pair.http, &pair.websocket] {
                    assert!(
                        is_loopback(origin),
                        "{} resolves to {origin}, which is not a loopback origin: a load through \
                         it would leave this process",
                        venue.as_str()
                    );
                    checked += 1;
                }
            }
        }

        assert!(
            checked > 0,
            "this build's venue list addresses at least one origin"
        );
    }

    /// Whether one origin addresses this machine.
    fn is_loopback(origin: &url::Url) -> bool {
        matches!(
            origin.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]") | Some("::1")
        )
    }

    /// One catalog answer the fixture serves: the request it belongs to, and the JSON the
    /// venue's own protocol defines for that request.
    struct FixtureAnswer {
        /// The path the request addresses.
        path: &'static str,
        /// The query parameter that tells this answer's request from its siblings'.
        parameter: &'static str,
        /// The body the venue answers with.
        body: &'static str,
    }

    impl FixtureAnswer {
        /// Whether this answer is the one `request` asks for.
        fn serves(&self, request: &FixtureRequest) -> bool {
            request.asks(self.path, self.parameter)
        }
    }

    /// One request the fixture was asked, read off the wire.
    struct FixtureRequest {
        /// The method the request was sent with.
        method: SmolStr,
        /// The path it addressed.
        path: SmolStr,
        /// The query it carried, without the `?`.
        query: SmolStr,
    }

    impl FixtureRequest {
        /// The request as it is recorded for an assertion.
        fn line(&self) -> SmolStr {
            format_smolstr!("{} {}?{}", self.method, self.path, self.query)
        }

        /// Whether the request addressed one path with one query parameter.
        fn asks(&self, path: &str, parameter: &str) -> bool {
            self.method == "GET"
                && self.path == path
                && self.query.split('&').any(|pair| pair == parameter)
        }
    }

    /// The one loopback venue a test's loads can talk to.
    ///
    /// It is the wire, not a stub of the adapter: what it answers is the venue's own protocol,
    /// and what it was asked is recorded, so a load that finished through it really finished
    /// against the venue's format. Every request is held until the test opens the fixture, so
    /// a load can be looked at while it is in flight instead of being stated.
    struct VenueFixture {
        /// The `http://` origin a harness points the venues it drives at.
        origin: String,
        /// What the fixture was asked, in the order the requests arrived.
        asked: Arc<Mutex<Vec<SmolStr>>>,
        /// Whether requests are answered or held.
        open: watch::Sender<bool>,
        server: JoinHandle<()>,
    }

    impl VenueFixture {
        /// Answers every request the fixture is holding, and every one that arrives after.
        fn release(&self) {
            self.open
                .send(true)
                .expect("the fixture is still serving its requests");
        }

        /// What the fixture was asked, in the order the requests arrived.
        fn asked(&self) -> Vec<SmolStr> {
            self.asked
                .lock()
                .expect("the fixture's requests are readable")
                .clone()
        }

        /// Stops serving: a fixture is a socket a load talks to, and a test that is finished
        /// with it does not leave the listener behind for the next one.
        fn shutdown(self) {
            self.server.abort();
        }
    }

    /// Serves the catalog protocols of the venues a test drives on a loopback port.
    ///
    /// A request no answer covers is answered with a body no catalog decodes, so a fixture
    /// serving nothing makes the loads that reach it fail on the venue's answer — and a test
    /// that meant to serve one finds out here rather than through an empty catalog.
    fn venue_fixture(answers: &'static [FixtureAnswer]) -> VenueFixture {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("the fixture binds a loopback port");
        listener
            .set_nonblocking(true)
            .expect("the fixture listener is non-blocking");
        let address = listener
            .local_addr()
            .expect("the fixture listener has an address");
        let asked: Arc<Mutex<Vec<SmolStr>>> = Arc::new(Mutex::new(Vec::new()));
        let (open, held) = watch::channel(false);
        let server = Runtime::for_test().run(serve(listener, answers, Arc::clone(&asked), held));
        VenueFixture {
            // The trailing slash is what a parsed origin states for a root path, and the
            // origin the venue configuration holds is asserted against this string as it is.
            origin: format!("http://{address}/"),
            asked,
            open,
            server,
        }
    }

    /// Accepts conversations and serves one held request at a time off each of them.
    async fn serve(
        listener: std::net::TcpListener,
        answers: &'static [FixtureAnswer],
        asked: Arc<Mutex<Vec<SmolStr>>>,
        open: watch::Receiver<bool>,
    ) {
        let listener =
            TcpListener::from_std(listener).expect("the fixture listener enters the runtime");
        while let Ok((mut socket, _)) = listener.accept().await {
            let asked = Arc::clone(&asked);
            let mut open = open.clone();
            tokio::spawn(async move {
                while let Some(request) = read_request(&mut socket).await {
                    asked
                        .lock()
                        .expect("the fixture's requests are writable")
                        .push(request.line());
                    // Held until the test opens the fixture: a request the fixture is holding is
                    // a load that is really in flight.
                    if open.wait_for(|open| *open).await.is_err() {
                        return;
                    }
                    let body = answers
                        .iter()
                        .find(|answer| answer.serves(&request))
                        .map_or(REFUSED, |answer| answer.body);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    /// Reads one request off a conversation.
    ///
    /// A catalog request is a `GET` whose answer never depends on a body, so what is read is
    /// the request line and the headers that carry it.
    async fn read_request(socket: &mut TcpStream) -> Option<FixtureRequest> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let head = loop {
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head]).into_owned();
        let mut fields = head.lines().next().unwrap_or_default().split(' ');
        let method = SmolStr::new(fields.next().unwrap_or_default());
        let target = fields.next().unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Some(FixtureRequest {
            method,
            path: SmolStr::new(path),
            query: SmolStr::new(query),
        })
    }

    /// The window a test drives: the header row with the menu at the end of it, over the
    /// layer every alert is opened on.
    struct Header {
        menu: Entity<MarketParticipants>,
    }

    impl Render for Header {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // The menu is mounted the way the header mounts it: a row that sizes the trigger
            // to its own button and puts it at the end of the row.
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_end()
                .child(self.menu.clone())
        }
    }

    struct Harness<'a> {
        menu: Entity<MarketParticipants>,
        cx: &'a mut VisualTestContext,
    }

    fn harness<'a>(
        cx: &'a mut TestAppContext,
        loaded_venues: &[VenueId],
        origins: &Origins,
    ) -> Harness<'a> {
        harness_over(cx, loaded_venues, origins, None)
    }

    /// [`harness`] over a configuration in which one venue holds credentials.
    ///
    /// The credentials are the settings page's own field, so a test that drives a configured
    /// venue drives what a user who filled the page in would have.
    fn harness_over<'a>(
        cx: &'a mut TestAppContext,
        loaded_venues: &[VenueId],
        origins: &Origins,
        credentials: Option<(VenueId, LongbridgeCredentials)>,
    ) -> Harness<'a> {
        // The loads the menu starts run on the application runtime, so the test's own
        // executor has to be allowed to wait for them rather than only for its own queue.
        cx.executor().allow_parking();
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("embedded locales should load");
            global::install(cx).expect("the test Tokio runtime should build");
            let root = market_participants_root(loaded_venues, origins, credentials);
            // A menu follows the venue commits the root announces, so the fixture attaches the
            // boundary the application's install path attaches.
            root.attach_events(cx);
            cx.set_global(root);
        });

        let menu = cx.update(|cx| cx.new(MarketParticipants::new));
        let rendered = menu.clone();
        let (_, cx) =
            cx.add_window_view(move |_, cx| Layer::new(cx.new(|_| Header { menu: rendered })));
        cx.simulate_resize(size(px(1280.0), px(960.0)));
        cx.run_until_parked();

        Harness { menu, cx }
    }

    /// Whether the thing `selector` names is drawn.
    fn shows(harness: &mut Harness, selector: &'static str) -> bool {
        harness.cx.debug_bounds(selector).is_some()
    }

    /// Lets a fade finish and paints what has settled: a popup in its first frames is painted
    /// at zero opacity, so what is drawn has to be given the frames it is waiting for before
    /// a test decides whether it is there.
    fn settle(harness: &mut Harness) {
        harness
            .cx
            .executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        harness.cx.update(|window, _| window.refresh());
        harness.cx.run_until_parked();
    }

    /// Runs the window until what a load did shows up in it.
    ///
    /// A load runs on the application runtime and reports through a GPUI task, and the test
    /// executor only runs what is already awake: waiting for a load means handing the thread
    /// back until the answer that woke it arrives, which is what this does. It waits with a
    /// deadline, so a load that never reports fails the test here instead of hanging it.
    fn wait_for(harness: &mut Harness, what: &str, settled: impl Fn(&mut Harness) -> bool) {
        let deadline = Instant::now() + LOAD_TIMEOUT;
        while !settled(harness) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            harness.cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Presses what `selector` names and paints what has settled.
    ///
    /// The press is delivered the way a window delivers it — the button goes down, the
    /// window draws the frame that follows, and only then does it come up — because a
    /// control that closes on the way down and opens on the way up behaves differently when
    /// the two are delivered without a frame between them. What a press *does* runs on the
    /// way up, so the load a row starts begins after that frame rather than inside it.
    fn press(harness: &mut Harness, selector: &'static str) {
        let target = harness
            .cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the harness draws what it is asked to press: {selector}"))
            .center();
        harness
            .cx
            .simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
        harness.cx.run_until_parked();
        harness
            .cx
            .simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
        harness.cx.run_until_parked();
        settle(harness);
    }

    /// Opens the panel by pressing the trigger.
    fn open_panel(harness: &mut Harness) {
        press(harness, TRIGGER);
        assert!(shows(harness, PANEL), "the trigger should open the panel");
    }

    /// Whether one venue is loaded, which is what a row's label is drawn from.
    fn registered(harness: &mut Harness, venue: &VenueId) -> bool {
        harness.cx.update(|_, cx| {
            cx.global::<MarketParticipantsConnector>()
                .venues()
                .loaded(venue)
                .is_some()
        })
    }

    /// The listings one route has published, which is what "the venue's catalog loaded" means.
    fn listings(harness: &mut Harness, route: &VenueRoute) -> Option<Arc<[CatalogEntry]>> {
        harness.cx.update(|_, cx| {
            cx.global::<MarketParticipantsConnector>()
                .catalog()
                .cached_listings(route)
        })
    }

    /// How many loads this menu is driving, one per venue.
    fn tasks_in_flight(harness: &mut Harness) -> usize {
        harness.cx.update(|_, cx| harness.menu.read(cx).tasks.len())
    }

    /// What one venue's row is saying, when it is saying anything.
    fn activity(harness: &mut Harness, venue: &VenueId) -> Option<Activity> {
        harness
            .cx
            .update(|_, cx| harness.menu.read(cx).activity.get(venue).cloned())
    }

    /// What one venue's row reports about its own load, restated the way the row states it.
    ///
    /// It *restates* it rather than reading it: the stored state is now only "this row
    /// failed", and the sentence is built at draw time from the row's own venue key. So
    /// this pins that the row is in that state — a failure recorded against the wrong
    /// venue, or one shared flag for the panel, still fails here — and it can show what
    /// the row would say, but it is not evidence about what the row draws. Nothing in
    /// this harness can read a drawn string.
    fn failure_text(harness: &mut Harness, venue: &VenueId) -> SmolStr {
        match activity(harness, venue) {
            Some(Activity::Failed) => {}
            other => panic!("{venue} should be reporting a failure, it is {other:?}"),
        }

        harness.cx.update(|_, cx| {
            SmolStr::new(
                t!(
                    cx,
                    "window.market_participants.failed",
                    market = venue.display_string(cx)
                )
                .as_ref(),
            )
        })
    }

    /// The sentence a load refused for want of credentials states, in the language the window
    /// shows, resolved the way the refusal resolves it.
    ///
    /// The alert a refusal opens is a private entity of the window layer, so this is the text it
    /// is built from — the venue's own display name bound to `market` — rather than a second
    /// spelling of it: what a test compares against it is comparing against what the alert says.
    fn refusal_text(harness: &mut Harness, venue: &VenueId) -> SharedString {
        harness.cx.update(|_, cx| {
            t!(
                cx,
                "window.market_participants.credentials_required_message",
                market = venue.display_string(cx)
            )
        })
    }

    /// Starts one venue's load where the row's action starts it, for the press a frame can
    /// deliver before the row it was drawn from is replaced.
    fn start_load(harness: &mut Harness, venue: &VenueId) {
        harness.cx.update(|window, cx| {
            harness
                .menu
                .update(cx, |menu, cx| menu.start_load(*venue, window, cx));
        });
        settle(harness);
    }

    /// Loads one venue the way the menu's action does — `MarketParticipantsConnector::load_venue` —
    /// and answers what it reported.
    fn load(harness: &mut Harness, venue: &VenueId) -> Result<(), VenueLoadError> {
        let (connector, runtime) = harness.cx.update(|_, cx| {
            (
                cx.global::<MarketParticipantsConnector>().clone(),
                global::handle(cx),
            )
        });
        // The load is the market participants root's own work, and this thread is not inside a runtime:
        // driving it here is what makes what it reported the test's to read.
        runtime.handle().block_on(connector.load_venue(*venue))
    }

    /// Whether one load failed by refusing to resolve a request in this process.
    ///
    /// The build resolves a request through the venue configuration's own origins, and this
    /// fixture points every one of them at a loopback address, so a failure that is *not* a
    /// resolution refusal is one that reached the wire — a transport error, a decode error or a
    /// venue's own answer. That is the typed answer to "did this load leave this process?".
    fn refused_to_resolve(outcome: &Result<(), VenueLoadError>) -> bool {
        match outcome {
            Err(VenueLoadError::Discovery { source, .. }) => {
                matches!(source.as_ref(), MarketParticipantError::Configuration(_))
            }
            Err(VenueLoadError::Catalog { source, .. }) => match source.as_ref() {
                MarketParticipantError::Catalog(error) => matches!(
                    error.as_ref(),
                    CatalogError::SourceFailed { error, .. }
                        if matches!(error.as_ref(), SourceError::Configuration { .. })
                ),
                _ => false,
            },
            _ => false,
        }
    }

    /// A load of one venue cannot leave this process: every origin every venue this build speaks
    /// is reached at is a loopback address, so what a load through one fails with is a failure of
    /// getting there rather than a venue's own answer.
    ///
    /// The configuration is what a request resolves through, so the guarantee is asserted on it:
    /// a venue this fixture did not configure is a load that could reach a real venue, wherever
    /// the machine happens to be.
    #[gpui_kit::test]
    fn an_offline_load_should_fail_on_an_origin_nothing_answers_on(cx: &mut TestAppContext) {
        let mut harness = harness(cx, &[], &Origins::OFFLINE);
        let venues = harness.cx.update(|_, cx| {
            cx.global::<MarketParticipantsConnector>()
                .supported_venues()
        });
        assert_eq!(venues.len(), 4, "this build speaks four venues");

        for venue in venues {
            let outcome = load(&mut harness, &venue);
            assert!(
                !refused_to_resolve(&outcome),
                "loading {venue} should have reached the origin it is configured with rather than \
                 refusing to resolve one: {outcome:?}"
            );
            assert!(
                format!("{outcome:?}").contains(UNREACHABLE),
                "loading {venue} should have addressed the origin its own configuration names, so \
                 what it failed with names that origin: {outcome:?}"
            );
        }
    }

    /// Every venue this build speaks is a row of the panel, and a venue it does not
    /// speak is not: the list is `supported_venues()`, whether or not anything is loaded.
    #[gpui_kit::test]
    fn the_panel_should_list_every_venue_this_build_speaks(cx: &mut TestAppContext) {
        let mut harness = harness(cx, &[], &Origins::OFFLINE);

        assert!(
            !shows(&mut harness, PANEL),
            "a panel nobody opened draws nothing"
        );
        open_panel(&mut harness);

        let loaded = harness.cx.update(|_, cx| {
            cx.global::<MarketParticipantsConnector>()
                .supported_venues()
        });
        assert_eq!(loaded.len(), 4, "this build speaks four venues");
        for row in [BINANCE_ROW, OKX_ROW, HYPERLIQUID_ROW, LONGBRIDGE_ROW] {
            assert!(shows(&mut harness, row), "the panel draws {row}");
        }
    }

    /// A market that cannot be read without credentials is refused before a load is started, and
    /// the user is told to configure it instead.
    ///
    /// What this observes is that no load was requested at all: the menu drives one task per
    /// venue, so a refusal that reached `load_venue` would have a task to show for itself and a
    /// row in flight — and the alert would then be the *failure* alert, which reports a load that
    /// ran. The row's own state is asserted beside it, because "nothing is loading" and "nothing
    /// failed" are the two halves of "the load was never started".
    #[gpui_kit::test]
    fn a_load_without_credentials_should_be_refused_with_an_alert(cx: &mut TestAppContext) {
        let mut harness = harness(cx, &[], &Origins::OFFLINE);
        open_panel(&mut harness);

        let longbridge = venue("longbridge");
        assert!(
            harness.cx.update(|_, cx| cx
                .global::<MarketParticipantsConnector>()
                .load_needs_credentials(longbridge)),
            "a fresh installation holds no Longbridge credentials"
        );

        press(&mut harness, LONGBRIDGE_LOAD);

        assert!(
            shows(&mut harness, ALERT),
            "the refusal is stated where the user is looking"
        );
        assert_eq!(
            tasks_in_flight(&mut harness),
            0,
            "the refused load was never requested, so no task of this menu is driving one"
        );
        assert!(
            activity(&mut harness, &longbridge).is_none(),
            "the venue is not loading and has not failed: nothing was started"
        );
        assert!(
            shows(&mut harness, LONGBRIDGE_LOAD) && !shows(&mut harness, LONGBRIDGE_LOADING),
            "the row still offers the load it offered before the press"
        );
        assert!(
            !registered(&mut harness, &longbridge),
            "a load that was never started registers nothing"
        );

        // What the alert says is the credentials sentence rather than the load-failure one, which
        // is what tells a user who has to configure a venue apart from one whose venue refused.
        let (title, message) = harness.cx.update(|_, cx| {
            (
                t!(
                    cx,
                    "window.market_participants.credentials_required_title",
                    market = longbridge.display_string(cx)
                ),
                t!(
                    cx,
                    "window.market_participants.credentials_required_message",
                    market = longbridge.display_string(cx)
                ),
            )
        });
        assert!(!title.is_empty() && !message.is_empty());
        assert!(
            title.contains("Longbridge") && message.contains("Longbridge"),
            "the refusal names the venue it is about: {title} / {message}"
        );
    }

    /// The credentials check is beside the re-entrancy guard rather than instead of it: a venue
    /// that is configured loads exactly as it did, through the same path and with no alert.
    #[gpui_kit::test]
    fn a_configured_market_should_load_through_the_same_path(cx: &mut TestAppContext) {
        let longbridge = venue("longbridge");
        let mut harness = harness_over(
            cx,
            &[],
            &Origins::OFFLINE,
            Some((
                longbridge,
                LongbridgeCredentials {
                    app_key: SmolStr::new_static("configured-app-key"),
                    app_secret: SmolStr::new_static("configured-app-secret"),
                    access_token: SmolStr::new_static("configured-access-token"),
                },
            )),
        );
        open_panel(&mut harness);

        assert!(
            !harness.cx.update(|_, cx| cx
                .global::<MarketParticipantsConnector>()
                .load_needs_credentials(longbridge)),
            "a complete credential set is what the gate asks for"
        );

        press(&mut harness, LONGBRIDGE_LOAD);

        assert!(
            shows(&mut harness, LONGBRIDGE_LOADING),
            "a configured venue starts the load its row offers"
        );
        assert_eq!(
            tasks_in_flight(&mut harness),
            1,
            "the load really was requested and is this menu's task"
        );
        assert!(
            !shows(&mut harness, ALERT),
            "a load that started is not the credentials refusal"
        );

        wait_for(&mut harness, "the load to report", |harness| {
            tasks_in_flight(harness) == 0
        });
        settle(&mut harness);

        assert!(
            activity(&mut harness, &longbridge).is_some(),
            "the load reported through the path a load of any other venue takes"
        );
        assert!(
            !registered(&mut harness, &longbridge),
            "the fixture holds its answer, so the venue did not commit"
        );
    }

    /// The three venues whose reads are public are offered exactly as they were: the credentials
    /// gate is a question each venue's own definition answers, and none of them declares it.
    ///
    /// What the gate would have made of one of them is asserted first, so a venue that is *not*
    /// refused here is refused nowhere: a press either starts a load — the fixture is asked for
    /// the venue's own catalog — or the menu states the credentials refusal, and never both.
    /// The fixture answers with a body no catalog decodes, so a load that ran fails on the venue's
    /// own answer rather than committing.
    #[gpui_kit::test]
    fn venues_whose_reads_are_public_should_never_meet_the_credentials_gate(
        cx: &mut TestAppContext,
    ) {
        let fixture = venue_fixture(&[]);
        fixture.release();
        let mut harness = harness(cx, &[], &Origins::at(&fixture.origin, PUBLIC_VENUES));
        open_panel(&mut harness);

        for row in [BINANCE_ROW, OKX_ROW, HYPERLIQUID_ROW] {
            assert!(shows(&mut harness, row), "the panel draws {row}");
        }

        for (venue, load, catalog) in [
            (binance(), BINANCE_LOAD, "/api/v3/exchangeInfo"),
            (okx(), OKX_LOAD, "/api/v5/public/instruments"),
            (venue("hyperliquid"), HYPERLIQUID_LOAD, "/info"),
        ] {
            assert!(
                !harness.cx.update(|_, cx| cx
                    .global::<MarketParticipantsConnector>()
                    .load_needs_credentials(venue)),
                "{venue} publishes its reads, so nothing about it has to be configured first"
            );
            let refused = refusal_text(&mut harness, &venue);
            let asked_before = fixture.asked().len();

            press(&mut harness, load);

            wait_for(&mut harness, "the load to report", |harness| {
                tasks_in_flight(harness) == 0 && shows(harness, ALERT)
            });
            settle(&mut harness);

            let asked = fixture.asked();
            assert!(
                asked[asked_before..]
                    .iter()
                    .any(|target| target.contains(catalog)),
                "{venue} started the load its row offers and asked for its own catalog: {:?}",
                &asked[asked_before..]
            );
            assert!(
                !refused.is_empty(),
                "the refusal {venue} never met is still a sentence of its own"
            );

            press(&mut harness, ALERT_OK);
            open_panel(&mut harness);
        }

        fixture.shutdown();
    }

    /// The alert a load refused for want of credentials raises is a sentence in every language
    /// the interface ships, and it says which market to configure and where.
    ///
    /// The keys are resolved here exactly as the refusal resolves them — the venue's own display
    /// name bound to `market` — because the alert itself is a private entity of the window layer:
    /// what this pins is the copy the refusal states, which is the same text the alert is built
    /// from.
    #[gpui_kit::test]
    fn the_credentials_alert_should_read_in_every_language(cx: &mut TestAppContext) {
        // The harness installs the catalogue the interface renders from, which is the one
        // `install` may do once per process; the language is what this test moves.
        let harness = harness(cx, &[], &Origins::OFFLINE);

        let longbridge = venue("longbridge");
        let mut seen = Vec::new();

        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            harness.cx.update(|_, cx| cx.set_global(language));
            let (market, title, message) = harness.cx.update(|_, cx| {
                (
                    longbridge.display_string(cx),
                    t!(
                        cx,
                        "window.market_participants.credentials_required_title",
                        market = longbridge.display_string(cx)
                    ),
                    t!(
                        cx,
                        "window.market_participants.credentials_required_message",
                        market = longbridge.display_string(cx)
                    ),
                )
            });

            assert!(
                !market.is_empty() && !title.is_empty() && !message.is_empty(),
                "the refusal reads as a sentence in {language}: {title} / {message}"
            );
            assert!(
                title.contains(market.as_ref()) && message.contains(market.as_ref()),
                "the refusal names the market in {language}: {title} / {message}"
            );
            assert!(
                message.contains("API Key"),
                "the refusal says which value to configure in {language}: {message}"
            );

            seen.push((title, message));
        }

        assert_ne!(
            seen[0], seen[1],
            "the two languages really state the refusal differently"
        );
    }

    /// Pressing a row's action leaves the panel open: the row reports its progress and its
    /// failures in the panel, so a press that closed it would hide what it started.
    #[gpui_kit::test]
    fn pressing_a_row_action_should_leave_the_panel_open(cx: &mut TestAppContext) {
        let mut harness = harness(cx, &[], &Origins::OFFLINE);
        open_panel(&mut harness);

        press(&mut harness, OKX_LOAD);
        // The press reached the row: the load it started failed and reported itself, so the
        // panel being open is the panel surviving a press rather than a press that missed.
        wait_for(&mut harness, "what the press started", |harness| {
            shows(harness, ALERT)
        });

        assert!(
            shows(&mut harness, PANEL),
            "the panel stays open while a row loads"
        );
        assert!(
            shows(&mut harness, OKX_ROW),
            "the row the press was made in is still drawn"
        );
        assert!(
            shows(&mut harness, ALERT),
            "what the press started reported itself"
        );
    }

    /// A load that succeeded is silent and flips its row: the row's label follows the loaded
    /// venues, which hold the venue now, so the action it offers is a refresh.
    ///
    /// What the press starts is the real connection action against a venue that answers on a
    /// loopback socket, and what the test reads is the state that load left: the row offers a
    /// refresh because the venue really is in the loaded venues with its scopes and catalogs,
    /// not because the test told the menu a load succeeded.
    #[gpui_kit::test]
    fn a_clicked_load_that_succeeds_should_offer_a_refresh(cx: &mut TestAppContext) {
        let fixture = venue_fixture(OKX_CATALOGS);
        let mut harness = harness(cx, &[], &Origins::at(&fixture.origin, OKX_VENUES));
        open_panel(&mut harness);
        assert!(
            shows(&mut harness, OKX_LOAD),
            "a venue nothing has loaded offers to load"
        );
        assert!(
            listings(&mut harness, &okx_spot()).is_none(),
            "nothing has loaded the venue's catalog yet"
        );

        press(&mut harness, OKX_LOAD);
        assert!(
            shows(&mut harness, OKX_LOADING),
            "the load the press started draws its progress"
        );

        // The load is really in flight and really waiting for the venue: the fixture is holding
        // the catalog request, and nothing has been published while it waits. (A load commits
        // nothing until it succeeds: the venue is in the loaded venues only once its scopes,
        // catalogs and metadata have all landed, and a load that fails leaves nothing — V4.)
        wait_for(&mut harness, "the venue's catalog request", |_| {
            fixture.asked().len() == 1
        });
        assert!(!shows(&mut harness, ALERT));
        assert!(
            listings(&mut harness, &okx_spot()).is_none(),
            "the catalog is not published before the venue answers"
        );

        fixture.release();
        wait_for(&mut harness, "the venue to load", |harness| {
            tasks_in_flight(harness) == 0
        });
        settle(&mut harness);

        assert!(
            registered(&mut harness, &okx()),
            "the load really registered the venue"
        );
        let spot = listings(&mut harness, &okx_spot())
            .expect("the load really published the venue's spot catalog");
        assert!(
            spot.iter().any(|entry| entry
                .listing
                .native_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.as_str() == "BTC-USDT")),
            "the catalog the venue answered with is what was published"
        );
        assert!(
            listings(&mut harness, &okx_swap()).is_some(),
            "the load fetched the venue's second catalog too"
        );
        assert!(
            shows(&mut harness, OKX_REFRESH),
            "a loaded venue offers to refresh"
        );
        assert!(
            !shows(&mut harness, OKX_LOAD),
            "and no longer offers to load"
        );
        assert!(!shows(&mut harness, OKX_LOADING));
        assert!(
            !shows(&mut harness, ALERT),
            "a load that succeeded reports nothing"
        );
        assert!(
            shows(&mut harness, PANEL),
            "the panel the load was started from stays open"
        );

        let asked = fixture.asked();
        assert_eq!(
            asked.len(),
            2,
            "the fixture answered exactly the two catalog routes of the venue: {asked:?}"
        );
        assert!(
            asked.iter().any(|target| target.contains("instType=SPOT")),
            "the spot catalog was fetched: {asked:?}"
        );
        assert!(
            asked.iter().any(|target| target.contains("instType=SWAP")),
            "the swap catalog was fetched: {asked:?}"
        );

        fixture.shutdown();
    }

    /// A first connection that fails is reported once, in the alert, and its row keeps
    /// offering to load: nothing is loaded, so the row's own answer is still
    /// "load", and pressing it again is the retry.
    #[gpui_kit::test]
    fn a_first_load_that_fails_should_be_reported_once(cx: &mut TestAppContext) {
        let mut harness = harness(cx, &[], &Origins::OFFLINE);
        open_panel(&mut harness);

        press(&mut harness, OKX_LOAD);
        wait_for(&mut harness, "the failed load", |harness| {
            shows(harness, ALERT)
        });

        assert!(
            shows(&mut harness, ALERT),
            "the failure is reported where the user is"
        );
        assert!(
            shows(&mut harness, OKX_FAILED),
            "the row keeps saying the venue failed to load"
        );
        assert!(
            shows(&mut harness, OKX_LOAD),
            "a venue that could not be loaded still offers to load"
        );
        assert!(
            !registered(&mut harness, &okx()),
            "a first connection that failed leaves nothing registered"
        );

        // One alert per failed action: reading it does not bring it back, because it was
        // opened once, when the load reported.
        press(&mut harness, ALERT_OK);

        assert!(
            !shows(&mut harness, ALERT),
            "the failure is reported once per action"
        );
    }

    /// A refresh that fails changes nothing that was loaded — including the row's own
    /// answer: the venue is still registered, so the row still offers to refresh.
    #[gpui_kit::test]
    fn a_refresh_that_fails_should_keep_offering_a_refresh(cx: &mut TestAppContext) {
        let mut harness = harness(cx, &[okx()], &Origins::OFFLINE);
        open_panel(&mut harness);
        assert!(
            shows(&mut harness, OKX_REFRESH),
            "a loaded venue offers to refresh"
        );

        press(&mut harness, OKX_REFRESH);
        wait_for(&mut harness, "the failed refresh", |harness| {
            shows(harness, ALERT)
        });

        assert!(shows(&mut harness, ALERT), "the failed refresh is reported");
        assert!(shows(&mut harness, OKX_FAILED), "the row reports it too");
        assert!(
            shows(&mut harness, OKX_REFRESH),
            "a refresh that failed leaves the venue loaded, so the row still offers one"
        );
        assert!(!shows(&mut harness, OKX_LOAD));
        assert!(
            registered(&mut harness, &okx()),
            "a refresh that failed changes nothing that was committed"
        );
    }

    /// A row whose load is in flight does not respond to a second press, while the same
    /// press on the same row does start a load once the row is idle.
    ///
    /// The load the row is in flight for is a real one, held open by the fixture, so what the
    /// second press meets is the state a press really leaves behind rather than one a test
    /// stated.
    #[gpui_kit::test]
    fn a_row_that_is_loading_should_ignore_a_second_press(cx: &mut TestAppContext) {
        let fixture = venue_fixture(&[]);
        let mut harness = harness(cx, &[], &Origins::at(&fixture.origin, OKX_VENUES));
        open_panel(&mut harness);

        press(&mut harness, OKX_LOAD);
        assert!(
            shows(&mut harness, OKX_LOADING),
            "a row in flight draws its progress"
        );
        assert_eq!(tasks_in_flight(&mut harness), 1);

        press(&mut harness, OKX_LOAD);

        assert!(
            !shows(&mut harness, ALERT),
            "a row in flight starts nothing, so nothing failed"
        );
        assert!(
            shows(&mut harness, OKX_LOADING) && !shows(&mut harness, OKX_FAILED),
            "the row is still loading"
        );
        assert_eq!(
            tasks_in_flight(&mut harness),
            1,
            "one load per venue: the press on a row in flight starts nothing"
        );

        // The action itself refuses a load for a venue that is already loading: that is the
        // same rule stated where a load is started, for the second press one frame can
        // deliver before the row it was drawn from is replaced.
        start_load(&mut harness, &okx());

        assert!(
            !shows(&mut harness, ALERT),
            "one load per venue: a second start is refused"
        );
        assert!(shows(&mut harness, OKX_LOADING));
        assert_eq!(tasks_in_flight(&mut harness), 1);

        // The load the row is in flight for is the one that press started, and once it reports
        // the row is idle again and offers the action it offered before — which is what the
        // press it ignored would have started a second time.
        fixture.release();
        wait_for(&mut harness, "the load to report", |harness| {
            tasks_in_flight(harness) == 0
        });
        assert!(
            shows(&mut harness, ALERT),
            "the load the press started reported"
        );
        assert!(shows(&mut harness, OKX_FAILED));
        assert!(!shows(&mut harness, OKX_LOADING));
        assert!(shows(&mut harness, OKX_LOAD));
        fixture.shutdown();
    }

    /// Venues are independent: two loads run beside each other, and each failure is reported
    /// against the row that asked for it.
    ///
    /// Both loads are really in flight at the same time and both are held there by the
    /// fixture, so what this pins is that the loads are held *per venue*: one slot for the
    /// menu would have taken the first venue's load away from it the moment the second row was
    /// pressed, and the row it was started from would have stayed loading for the rest of the
    /// window's life.
    #[gpui_kit::test]
    fn loads_of_two_venues_should_run_beside_each_other(cx: &mut TestAppContext) {
        let fixture = venue_fixture(&[]);
        let both_venues: Vec<VenueId> = BINANCE_VENUES.iter().chain(OKX_VENUES).copied().collect();
        let mut harness = harness(cx, &[], &Origins::at(&fixture.origin, &both_venues));
        open_panel(&mut harness);

        press(&mut harness, OKX_LOAD);
        press(&mut harness, BINANCE_LOAD);

        assert_eq!(
            tasks_in_flight(&mut harness),
            2,
            "each venue's load is held by a task of its own"
        );
        assert!(
            shows(&mut harness, OKX_LOADING) && shows(&mut harness, BINANCE_LOADING),
            "both rows are in flight"
        );
        assert!(
            !shows(&mut harness, ALERT),
            "nothing reported while the fixture holds both loads"
        );

        fixture.release();
        wait_for(&mut harness, "both loads to report", |harness| {
            tasks_in_flight(harness) == 0
        });
        settle(&mut harness);

        assert!(
            shows(&mut harness, OKX_FAILED) && shows(&mut harness, BINANCE_FAILED),
            "each row reports the load it asked for"
        );
        assert!(
            !shows(&mut harness, OKX_LOADING) && !shows(&mut harness, BINANCE_LOADING),
            "both loads reached a terminal state"
        );
        assert!(
            shows(&mut harness, OKX_LOAD) && shows(&mut harness, BINANCE_LOAD),
            "a first connection that failed leaves nothing registered"
        );
        assert!(!registered(&mut harness, &okx()) && !registered(&mut harness, &binance()));
        assert!(
            shows(&mut harness, ALERT),
            "the failure is reported where the user is"
        );

        // Each failure is the row's own. What a row *says* is no longer evidence of that:
        // the line is drawn from the row's own venue key, so a row cannot name another
        // venue's load however the failure was recorded. What is still evidence — and
        // what one shared "something failed" flag, or a failure recorded against the
        // wrong venue, would break — is that both rows carry a failure of their own and
        // that a venue nobody loaded carries none.
        let okx_said = failure_text(&mut harness, &okx());
        let binance_said = failure_text(&mut harness, &binance());
        assert!(
            activity(&mut harness, &venue("hyperliquid")).is_none(),
            "a venue nobody loaded reports nothing"
        );
        assert_ne!(
            okx_said, binance_said,
            "the two rows state their own venue rather than one shared sentence"
        );

        // Both loads really went to the loopback venue, each of them for its own catalog.
        let asked = fixture.asked();
        assert!(
            asked
                .iter()
                .any(|target| target.contains("/api/v3/exchangeInfo")),
            "the Binance load asked the fixture for its catalog: {asked:?}"
        );
        assert!(
            asked
                .iter()
                .any(|target| target.contains("/api/v5/public/instruments")),
            "the OKX load asked the fixture for its catalog: {asked:?}"
        );

        fixture.shutdown();
    }

    /// The control for the offline load above: a load that reaches an origin reports something
    /// else, whatever that is.
    ///
    /// Without this, "the failure is a failure of getting there" could hold for every load —
    /// including one that really reached a venue — and the assertion above would pin nothing: the
    /// fixture here answers with a body no catalog protocol decodes, so a load that arrives fails
    /// on the venue's own answer instead.
    #[gpui_kit::test]
    fn a_load_that_reaches_an_origin_should_not_fail_on_getting_there(cx: &mut TestAppContext) {
        let fixture = venue_fixture(&[]);
        fixture.release();
        let mut harness = harness(cx, &[], &Origins::at(&fixture.origin, OKX_VENUES));

        let outcome = load(&mut harness, &okx());

        assert!(
            matches!(outcome, Err(VenueLoadError::Catalog { .. })),
            "the load reached the origin it resolved and failed on the venue's answer: \
             {outcome:?}"
        );
        assert!(
            !refused_to_resolve(&outcome),
            "an origin that answers reports what the venue answered, not a refusal to resolve \
             one: {outcome:?}"
        );
        assert_eq!(
            fixture.asked().len(),
            1,
            "the request the load made is the one the fixture answered: {:?}",
            fixture.asked()
        );
        fixture.shutdown();
    }

    /// A load that was cancelled does not wedge its row: the venue goes back to offering the
    /// action it was asked for, and pressing it again starts a load.
    ///
    /// The task a load runs in can be gone without the load having reported anything — the
    /// runtime the window drives it on was shut down under it — and the row it was started from
    /// is the only thing that can put the venue back: `activity` is this menu's own memory, and
    /// a loading row attaches no action at all, so a venue left loading is a venue no press can
    /// ever reach again.
    #[gpui_kit::test]
    fn a_cancelled_load_should_not_wedge_its_row(cx: &mut TestAppContext) {
        let fixture = venue_fixture(&[]);
        let mut harness = harness(cx, &[okx()], &Origins::at(&fixture.origin, OKX_VENUES));
        open_panel(&mut harness);
        assert!(
            shows(&mut harness, OKX_REFRESH),
            "a loaded venue offers to refresh"
        );

        press(&mut harness, OKX_REFRESH);
        assert!(shows(&mut harness, OKX_LOADING));
        assert_eq!(tasks_in_flight(&mut harness), 1);
        wait_for(&mut harness, "the venue's catalog request", |_| {
            fixture.asked().len() == 1
        });

        // Pull the runtime the window drives the load on out from under it: the task is
        // cancelled, and what the menu is handed is the cancellation rather than a report.
        harness
            .cx
            .update(|_, cx| global::install(cx).expect("a second runtime builds"));
        wait_for(
            &mut harness,
            "the menu to hear the cancellation",
            |harness| tasks_in_flight(harness) == 0,
        );
        settle(&mut harness);

        assert!(
            !shows(&mut harness, OKX_LOADING),
            "a cancelled load does not keep its row loading"
        );
        assert!(
            shows(&mut harness, OKX_REFRESH) && !shows(&mut harness, OKX_FAILED),
            "a cancelled load is not a failure this menu reports"
        );
        assert!(!shows(&mut harness, ALERT), "and it opens no alert");
        assert!(
            activity(&mut harness, &okx()).is_none(),
            "the venue is not doing anything any more"
        );

        // The control: the row is pressable again, which is what a cancelled load took away —
        // a row the menu still believes is loading attaches no action at all.
        press(&mut harness, OKX_REFRESH);

        assert_eq!(
            tasks_in_flight(&mut harness),
            1,
            "the press reaches the action the row offers again"
        );
        assert!(shows(&mut harness, OKX_LOADING));
        fixture.shutdown();
    }

    /// A missing key translates to an empty string, which is how this menu would fail
    /// quietly: rows with no action, an alert that says nothing. Every key this crate reads
    /// is listed here and resolved in every locale the binary embeds, so a rename or a
    /// forgotten locale fails a test instead of blanking out the interface.
    #[gpui_kit::test]
    fn every_key_this_menu_reads_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("embedded locales should load");
        });

        let keys = [
            "common.confirm",
            "error.venue_load.catalog",
            "error.venue_load.catalog_validation",
            "error.venue_load.discovery",
            "error.venue_load.discovery_limit_exceeded",
            "error.venue_load.discovery_unverifiable",
            "error.venue_load.interrupted",
            "error.venue_load.metadata",
            "error.venue_load.registration",
            "error.venue_load.revision_changed",
            "error.venue_load.unsupported_venue",
            "window.market_participants.credentials_required_message",
            "window.market_participants.credentials_required_title",
            "window.market_participants.load",
            "window.market_participants.failed",
            "window.market_participants.loaded",
            "window.market_participants.loading",
            "window.market_participants.refresh",
            "window.market_participants.title",
            "window.market_participants.unloaded",
        ];

        cx.update(|cx| {
            let languages = cx.global::<I18n>().languages();
            assert_eq!(
                languages.len(),
                2,
                "this binary embeds the locales it renders in"
            );

            for language in languages {
                cx.set_global(language);

                for key in keys {
                    assert!(
                        !i18n::t!(cx, key).is_empty(),
                        "{key} has no copy in {language}"
                    );
                }

                // Every failure a load can report has to say something, and the one whose copy
                // is a sentence of its own has to say *that*: a catalog that moved under its
                // fetch is reported by asking the user to ask again, not by naming a route. Its
                // two route input revisions are public, so the variant is built here and put
                // through the same localization as the rest.
                let stale_catalog = LoadFailure::Venue(VenueLoadError::Catalog {
                    venue: okx(),
                    scope: okx_spot().scope,
                    source: Box::new(MarketParticipantError::Catalog(Box::new(
                        CatalogError::RouteRevisionChanged {
                            route: okx_spot(),
                            staged: Box::new(
                                RouteInputRevision::describe().catalog_descriptor(1).build(),
                            ),
                            current: Box::new(
                                RouteInputRevision::describe().catalog_descriptor(2).build(),
                            ),
                        },
                    ))),
                });
                let discovery = LoadFailure::Venue(VenueLoadError::Discovery {
                    venue: okx(),
                    source: Box::new(MarketParticipantError::RuntimeUnavailable),
                });
                let registration = LoadFailure::Venue(VenueLoadError::Registration {
                    venue: okx(),
                    source: Box::new(RegistryError::Descriptor(DescriptorError::DuplicateRoute {
                        route: okx_spot(),
                    })),
                });
                let route_catalog = LoadFailure::Venue(VenueLoadError::Catalog {
                    venue: okx(),
                    scope: okx_spot().scope,
                    source: Box::new(MarketParticipantError::Catalog(Box::new(
                        CatalogError::CatalogNotLoaded { route: okx_spot() },
                    ))),
                });
                // The refusal of a venue the definition table answers no definition for: a row
                // the menu never draws, but a failure its localization still has to state.
                let unsupported = LoadFailure::Venue(VenueLoadError::UnsupportedVenue {
                    venue: VenueId::Longbridge,
                });

                for failure in [&discovery, &registration, &route_catalog, &unsupported] {
                    assert!(
                        !failure.localized_message(cx).is_empty(),
                        "{failure:?} has no copy in {language}"
                    );
                }

                let stale = i18n::t!(
                    cx,
                    "error.venue_load.revision_changed",
                    market = okx().display_string(cx),
                    scope = scope_display_code(&okx_spot().scope)
                );
                assert!(
                    !stale.is_empty(),
                    "the moved catalog sentence has no copy in {language}"
                );
                assert_eq!(
                    stale_catalog.localized_message(cx),
                    stale,
                    "a catalog that moved under its fetch reads as its own sentence in {language}"
                );
                assert_ne!(
                    route_catalog.localized_message(cx),
                    stale,
                    "a catalog failure that carries a route is not reported as a moved catalog in \
                     {language}"
                );
            }
        });
    }
}
