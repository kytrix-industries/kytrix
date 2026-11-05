//! The prediction-event selector.
//!
//! A prediction venue publishes events rather than a catalog, so this dialog is
//! not an instrument selector with another filter: it asks the venue for a page of
//! event cards, draws them as cards with their option probabilities, and reports
//! the event the user confirmed. The dialog is one venue at a time: switching the
//! venue resets the category, the search and the page.

use std::{future::Future, rc::Rc, time::Duration};

use app_config_provider::manager::ConfigManager;
use base_ui::{
    badge::Badge,
    button::Button,
    empty_state::EmptyState,
    input::{Input, InputState, InputStateBuilder, InputStateEvent},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    menu::{MenuItem, MenuItemAction},
    select::Select,
    size::UnifiedSizable,
    spinner::Spinner,
    style::theme::ThemeExt,
    tab_bar::TabBar,
};
use gpui_kit::{
    AnyElement, App, AppContext, Context, DismissEvent, ElementId, Entity, EventEmitter,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, base::Disableable, div,
    prelude::FluentBuilder, px,
};
use i18n::t;
use market_participants_connector::{
    connector::MarketParticipantsConnector,
    market::{
        error::MarketError,
        prediction::{DEFAULT_PREDICTION_PAGE_SIZE, PredictionEventsRequest},
        provider::MarketCapability,
    },
    venue::loaded::LoadedVenues,
};
use market_participants_model::{
    identifier::VenueId,
    prediction::{
        PredictionCategory, PredictionEventCard, PredictionEventRef, PredictionEventsPage,
        PredictionOptionCard,
    },
    routing::CapabilityId,
};
use smol_str::{SmolStr, format_smolstr};
use widget::{error::LocalizedError, time_zone::data_time_zone, venue::VenueDisplay};
use widget_tokio_runtime::global;

use crate::prediction_locale::polymarket_locale;

/// The number of cards one page draws.
const PAGE_SIZE: u16 = DEFAULT_PREDICTION_PAGE_SIZE;
/// The number of columns and rows the card grid draws.
const GRID_COLUMNS: u16 = 4;
const GRID_ROWS: u16 = 2;
/// How long the search box waits after a keystroke.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// The width of a probability bar at 100%.
const BAR_WIDTH: f32 = 96.0;
/// The size of the dialog content.
const DIALOG_WIDTH: f32 = 960.0;
const DIALOG_HEIGHT: f32 = 680.0;

/// One source of prediction-event pages.
///
/// It is a private seam rather than a public extension point: production answers
/// through [`MarketCapability`] and a test answers with a fake page.
pub trait PredictionEventSource: Clone + Send + Sync + 'static {
    /// Reads one page.
    fn page(
        &self,
        request: PredictionEventsRequest,
    ) -> impl Future<Output = Result<PredictionEventsPage, MarketError>> + Send;
}

impl PredictionEventSource for MarketCapability {
    async fn page(
        &self,
        request: PredictionEventsRequest,
    ) -> Result<PredictionEventsPage, MarketError> {
        self.prediction_events(request).fetch().await
    }
}

/// What happens when the user confirms an event, with the title its card showed.
type ConfirmHandler = Rc<dyn Fn(PredictionEventRef, SmolStr, &mut Window, &mut App) + 'static>;

/// Why one page could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PredictionSelectorError {
    Market(MarketError),
    Interrupted,
}

impl LocalizedError for PredictionSelectorError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::Market(error) => error.localized_message(cx),
            Self::Interrupted => t!(cx, "error.prediction_selector.interrupted"),
        }
    }
}

/// The event selector's content area.
pub struct PredictionEventSelector<S: PredictionEventSource> {
    source: S,
    venues: Vec<VenueId>,
    venue: VenueId,
    categories: Vec<PredictionCategory>,
    category: Option<SmolStr>,
    search: String,
    page: Option<PredictionEventsPage>,
    loading: bool,
    error: Option<PredictionSelectorError>,
    selected: Option<PredictionEventRef>,
    selected_title: Option<SmolStr>,
    cursor: Option<SmolStr>,
    cursor_stack: Vec<Option<SmolStr>>,
    page_number: u32,
    navigation_backup: Option<(Option<SmolStr>, Vec<Option<SmolStr>>, u32)>,
    generation: u64,
    search_generation: u64,
    search_input: Entity<InputState>,
    _search_subscription: Subscription,
    search_task: Option<Task<()>>,
    on_confirm: ConfirmHandler,
}

impl PredictionEventSelector<MarketCapability> {
    /// Presents the selector against the production market capability.
    ///
    /// Returns whether a dialog was presented: with no loaded venue that serves
    /// prediction events there is nothing to select from.
    pub fn show(
        current: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        on_confirm: ConfirmHandler,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let source = cx.global::<MarketParticipantsConnector>().market();
        Self::show_with_source(current, current_title, source, on_confirm, window, cx)
    }
}

impl<S: PredictionEventSource> PredictionEventSelector<S> {
    /// Presents the selector against an explicit source.
    pub fn show_with_source(
        current: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        source: S,
        on_confirm: ConfirmHandler,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let venues = loaded_prediction_venues(cx.global::<MarketParticipantsConnector>().venues());
        if venues.is_empty() {
            return false;
        }
        let search_input = cx.new(|cx| {
            InputStateBuilder::new("prediction-event-search")
                .placeholder(t!(cx, "prediction_selector.search_placeholder"))
                .build(window, cx)
        });
        let selector = cx.new(|cx| {
            Self::new(
                source,
                venues,
                current,
                current_title,
                search_input,
                on_confirm,
                cx,
            )
        });
        window.dialog(cx, selector.clone());
        selector.update(cx, |selector, cx| selector.reload(cx));
        true
    }

    fn new(
        source: S,
        venues: Vec<VenueId>,
        selected: Option<PredictionEventRef>,
        current_title: Option<SmolStr>,
        search_input: Entity<InputState>,
        on_confirm: ConfirmHandler,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected = selected.filter(|event| venues.contains(&event.route.venue));
        let selected_title = selected.as_ref().and(current_title);
        let venue = selected
            .as_ref()
            .map(|event| event.route.venue)
            .unwrap_or(venues[0]);
        let subscription = cx.subscribe(&search_input, |this, _, event: &InputStateEvent, cx| {
            if let InputStateEvent::Change(query) = event {
                this.search_changed(query.as_ref(), cx);
            }
        });
        Self {
            source,
            venues,
            venue,
            categories: Vec::new(),
            category: None,
            search: String::new(),
            page: None,
            loading: false,
            error: None,
            selected,
            selected_title,
            cursor: None,
            cursor_stack: Vec::new(),
            page_number: 1,
            navigation_backup: None,
            generation: 0,
            search_generation: 0,
            search_input,
            _search_subscription: subscription,
            search_task: None,
            on_confirm,
        }
    }

    fn request(&self, cx: &App) -> PredictionEventsRequest {
        let locale = cx
            .try_global::<ConfigManager>()
            .and_then(|manager| polymarket_locale(manager.read().language));
        PredictionEventsRequest::first(self.venue)
            .category(self.category.clone())
            .search((!self.search.trim().is_empty()).then(|| SmolStr::new(self.search.trim())))
            .cursor(self.cursor.clone())
            .page_size(PAGE_SIZE)
            .locale(locale)
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        let source = self.source.clone();
        let request = self.request(cx);
        // The market clients use Tokio's timers and I/O, so the read must run on
        // the application Tokio runtime rather than on GPUI's executor.
        let load = global::handle(cx).run(async move { source.page(request).await });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = match load.await {
                Ok(result) => result.map_err(PredictionSelectorError::Market),
                Err(error) if error.is_cancelled() => return,
                Err(_) => Err(PredictionSelectorError::Interrupted),
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.apply(result, cx);
                }
            });
        })
        .detach();
    }

    fn apply(
        &mut self,
        result: Result<PredictionEventsPage, PredictionSelectorError>,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match result {
            Ok(page) => {
                for category in &page.categories {
                    if let Some(known) = self
                        .categories
                        .iter_mut()
                        .find(|known| known.key == category.key)
                    {
                        known.label = category.label.clone();
                    } else {
                        self.categories.push(category.clone());
                    }
                }
                self.navigation_backup = None;
                self.page = Some(page);
                self.error = None;
            }
            Err(error) => {
                if let Some((cursor, cursor_stack, page_number)) = self.navigation_backup.take() {
                    self.cursor = cursor;
                    self.cursor_stack = cursor_stack;
                    self.page_number = page_number;
                }
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    fn search_changed(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.search == query {
            return;
        }
        self.search = query.to_owned();
        self.search_generation = self.search_generation.wrapping_add(1);
        self.navigation_backup = None;
        let generation = self.search_generation;
        // The page is stale from the first keystroke: show the loading state while
        // the debounce runs and invalidate any read that is still in flight.
        self.generation = self.generation.wrapping_add(1);
        self.loading = true;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        cx.notify();
        // Dropping the previous debounce task cancels the search it belonged to.
        self.search_task.take();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if this.search_generation == generation {
                    this.reload(cx);
                }
            });
        });
        self.search_task = Some(task);
    }

    fn select_venue(&mut self, venue: VenueId, window: &mut Window, cx: &mut Context<Self>) {
        if self.venue == venue {
            return;
        }
        self.venue = venue;
        self.selected = None;
        self.selected_title = None;
        self.categories.clear();
        self.category = None;
        self.search.clear();
        self.search_input
            .update(cx, |input, cx| input.update_value("", window, cx));
        // A debounce armed for the old venue must not reload the new one.
        self.cancel_search_debounce();
        self.navigation_backup = None;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        self.page = None;
        self.reload(cx);
    }

    fn select_category(&mut self, index: usize, cx: &mut Context<Self>) {
        let next = index
            .checked_sub(1)
            .and_then(|index| self.categories.get(index))
            .map(|category| category.key.clone());
        if self.category == next {
            return;
        }
        self.cancel_search_debounce();
        self.navigation_backup = None;
        self.category = next;
        self.cursor = None;
        self.cursor_stack.clear();
        self.page_number = 1;
        self.reload(cx);
    }

    fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(next) = self.page.as_ref().and_then(|page| page.next_cursor.clone()) else {
            return;
        };
        self.cancel_search_debounce();
        self.navigation_backup = Some((
            self.cursor.clone(),
            self.cursor_stack.clone(),
            self.page_number,
        ));
        self.cursor_stack.push(self.cursor.clone());
        self.cursor = Some(next);
        self.page_number = self.page_number.saturating_add(1);
        self.reload(cx);
    }

    fn previous_page(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(previous) = self.cursor_stack.last().cloned() else {
            return;
        };
        self.cancel_search_debounce();
        self.navigation_backup = Some((
            self.cursor.clone(),
            self.cursor_stack.clone(),
            self.page_number,
        ));
        self.cursor_stack.pop();
        self.cursor = previous;
        self.page_number = self.page_number.saturating_sub(1).max(1);
        self.reload(cx);
    }

    fn cancel_search_debounce(&mut self) {
        self.search_task.take();
        self.search_generation = self.search_generation.wrapping_add(1);
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let Some(title) = self.selected_title.clone() else {
            return;
        };
        let on_confirm = self.on_confirm.clone();
        on_confirm(selected, title, window, cx);
        cx.emit(DismissEvent);
    }

    fn venue_select(&self, cx: &Context<Self>) -> Select {
        let select = self.venues.iter().fold(
            Select::new(ElementId::Name(SharedString::new_static("PredictionVenue")))
                .placeholder(SharedString::new(self.venue.display_string(cx)))
                .selected_item(Some(SharedString::new(self.venue.as_str())))
                .medium(),
            |select, venue| {
                select.menu_item(MenuItem::Action(MenuItemAction::new(
                    SharedString::new(venue.as_str()),
                    SharedString::new(venue.display_string(cx)),
                )))
            },
        );
        select.on_change({
            let selector = cx.entity();
            let venues = self.venues.clone();
            move |value, window, app| {
                let Some(value) = value.as_deref() else {
                    return;
                };
                let venue = VenueId::from_name(value).ok().or_else(|| {
                    venues
                        .iter()
                        .copied()
                        .find(|venue| venue.display_string(app).as_ref() == value)
                });
                let Some(venue) = venue else {
                    return;
                };
                selector.update(app, |this, cx| this.select_venue(venue, window, cx));
            }
        })
    }

    fn category_tabs(&self, cx: &Context<Self>) -> TabBar {
        let selected = self
            .category
            .as_ref()
            .and_then(|category| {
                self.categories
                    .iter()
                    .position(|known| &known.key == category)
            })
            .map_or(0, |index| index + 1);
        let tabs = self.categories.iter().fold(
            TabBar::new("PredictionCategories").small().child(
                div()
                    .child(t!(cx, "prediction_selector.all"))
                    .into_any_element(),
            ),
            |tabs, category| {
                tabs.child(
                    div()
                        .child(SharedString::new(category.label.clone()))
                        .into_any_element(),
                )
            },
        );
        tabs.selected_index(selected).on_change({
            let selector = cx.entity();
            move |index, _, app| {
                selector.update(app, |this, cx| this.select_category(*index, cx));
            }
        })
    }

    fn render_card(
        &self,
        index: usize,
        card: &PredictionEventCard,
        cx: &Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_ref() == Some(&card.event);
        let selector = cx.entity().downgrade();
        let event = card.event.clone();
        let title = card.title.clone();
        let theme = cx.theme();
        let mut options = div().flex().flex_col().gap_1();
        for option in &card.options {
            options = options.child(option_row(option, cx));
        }
        let mut body = div()
            .id((
                ElementId::Name(SharedString::new_static("PredictionCard")),
                SharedString::new(card.event.native_event.as_str()),
            ))
            .debug_selector(move || format!("PREDICTION_EVENT_CARD_{index}"))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(if selected {
                theme.roles.brand.base.hsla
            } else {
                theme.roles.line.divider.hsla
            })
            .bg(theme.roles.surface.raised.hsla)
            .cursor_pointer()
            .on_click(move |_, _, app| {
                let _ = selector.update(app, |this, cx| {
                    this.selected = Some(event.clone());
                    this.selected_title = Some(title.clone());
                    cx.notify();
                });
            })
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(theme.roles.fg.default.hsla)
                    .line_height(gpui_kit::px(18.0))
                    .max_h(px(36.0))
                    .overflow_hidden()
                    .child(SharedString::new(card.title.clone())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Badge::new()
                            .info()
                            .small()
                            .child(SharedString::new(card.category.label.clone())),
                    )
                    .children(card.ends_at.and_then(|end| format_end(end, cx)).map(|end| {
                        div()
                            .text_xs()
                            .text_color(theme.roles.fg.muted.hsla)
                            .child(SharedString::new(end))
                    })),
            )
            .child(options);
        if card.remaining_markets > 0 {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "prediction_selector.more_markets",
                        count = card.remaining_markets
                    )),
            );
        } else if card.remaining_options > 0 {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.roles.fg.muted.hsla)
                    .child(t!(
                        cx,
                        "prediction_selector.more_options",
                        count = card.remaining_options
                    )),
            );
        }
        body.into_any_element()
    }

    fn render_body(&self, cx: &Context<Self>) -> AnyElement {
        if self.loading {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .debug_selector(|| "PREDICTION_EVENT_SPINNER".into())
                        .size_5()
                        .child(Spinner::new("PredictionSpinner")),
                )
                .into_any_element();
        }
        if let Some(error) = &self.error {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(EmptyState::new(
                    "PredictionError",
                    error.localized_message(cx),
                ))
                .child(
                    Button::new("PredictionRetry")
                        .tonal()
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
                        .child(t!(cx, "common.retry")),
                )
                .into_any_element();
        }
        let Some(page) = &self.page else {
            return div().flex_1().into_any_element();
        };
        if page.events.is_empty() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(EmptyState::new(
                    "PredictionEmpty",
                    t!(cx, "prediction_selector.no_matching_events"),
                ))
                .into_any_element();
        }
        let grid = div()
            .debug_selector(|| "PREDICTION_EVENTS_GRID".into())
            .size_full()
            .grid()
            .grid_cols(GRID_COLUMNS)
            .grid_rows(GRID_ROWS)
            .gap_2()
            .children(
                page.events
                    .iter()
                    .take(usize::from(PAGE_SIZE))
                    .enumerate()
                    .map(|(index, card)| self.render_card(index, card, cx)),
            );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(grid)
            .into_any_element()
    }

    fn footer(&self, cx: &Context<Self>) -> AnyElement {
        let previous = self.cursor_stack.is_empty() || self.loading;
        let next = self
            .page
            .as_ref()
            .is_none_or(|page| page.next_cursor.is_none() || self.loading);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("PredictionPrevious")
                    .ghost()
                    .small()
                    .disabled(previous)
                    .when(!previous, |button| {
                        button.on_click(cx.listener(|this, _, _, cx| this.previous_page(cx)))
                    })
                    .child(t!(cx, "prediction_selector.previous")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().roles.fg.muted.hsla)
                    .child(t!(cx, "prediction_selector.page", page = self.page_number)),
            )
            .child(
                Button::new("PredictionNext")
                    .ghost()
                    .small()
                    .disabled(next)
                    .when(!next, |button| {
                        button.on_click(cx.listener(|this, _, _, cx| this.next_page(cx)))
                    })
                    .child(t!(cx, "prediction_selector.next")),
            )
            .child(div().flex_1())
            .child(
                Button::new("PredictionCancel")
                    .tonal()
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                    .child(t!(cx, "common.cancel")),
            )
            .child(
                Button::new("PredictionConfirm")
                    .filled()
                    .disabled(self.selected.is_none() || self.selected_title.is_none())
                    .when(
                        self.selected.is_some() && self.selected_title.is_some(),
                        |button| {
                            button.on_click(
                                cx.listener(|this, _, window, cx| this.confirm(window, cx)),
                            )
                        },
                    )
                    .child(t!(cx, "common.confirm")),
            )
            .into_any_element()
    }
}

impl<S: PredictionEventSource> EventEmitter<DismissEvent> for PredictionEventSelector<S> {}

impl<S: PredictionEventSource> Render for PredictionEventSelector<S> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .debug_selector(|| "PREDICTION_EVENT_SELECTOR".into())
            .w(px(DIALOG_WIDTH))
            .h(px(DIALOG_HEIGHT))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.venue_select(cx))
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.search_input).clearable(true)),
                    ),
            )
            .child(self.category_tabs(cx))
            .child(
                div()
                    .debug_selector(|| "PREDICTION_EVENTS_BODY".into())
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .bg(theme.roles.surface.sunken.hsla)
                    .p_2()
                    .child(self.render_body(cx)),
            )
            .child(self.footer(cx))
    }
}

fn option_row(option: &PredictionOptionCard, cx: &App) -> AnyElement {
    let percentage = option
        .probability
        .map(|probability| (probability.ratio().inner() as f64 / 1e16).clamp(0.0, 100.0))
        .unwrap_or(0.0);
    let color = cx.theme().roles.chart.series[0].hsla;
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(cx.theme().roles.fg.default.hsla)
                .child(SharedString::new(option.label.clone())),
        )
        .child(
            div()
                .flex_none()
                .w(px(BAR_WIDTH))
                .h(px(4.0))
                .rounded_full()
                .bg(cx.theme().roles.surface.overlay.hsla)
                .child(
                    div()
                        .h_full()
                        .w(px(BAR_WIDTH * (percentage / 100.0) as f32))
                        .rounded_full()
                        .bg(color),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(34.0))
                .text_xs()
                .text_color(cx.theme().roles.fg.muted.hsla)
                .child(SharedString::new(format_smolstr!("{percentage:.0}%"))),
        )
        .into_any_element()
}

fn loaded_prediction_venues(venues: &LoadedVenues) -> Vec<VenueId> {
    let mut loaded: Vec<VenueId> = VenueId::ALL
        .into_iter()
        .filter(|venue| {
            venues.entries().iter().any(|(route, entry)| {
                route.venue == *venue
                    && entry
                        .capabilities
                        .iter()
                        .any(|support| support.capability == CapabilityId::PredictionEvents)
            })
        })
        .collect();
    loaded.sort();
    loaded
}

fn format_end(millis: i64, cx: &App) -> Option<String> {
    let seconds = millis.checked_div(1_000)?;
    let date = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    let date = date.to_offset(data_time_zone(cx));
    Some(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use app_config::settings::{Config, Language};
    use app_config_provider::manager::ConfigManager;
    use base_ui::{
        input::InputStateBuilder,
        layer::{Layer, LayerExt},
    };
    use gpui_kit::{
        App, AppContext as _, BorrowAppContext, Context, Entity, IntoElement, Render,
        TestAppContext, VisualTestContext, Window, div,
    };
    use i18n::global::embedded;
    use market_participants_connector::market::error::MarketError;
    use market_participants_model::{
        identifier::{IdentifierValue, VenueId, VenueScopeId},
        listing::route::VenueRoute,
        prediction::{
            PredictionCategory, PredictionEventCard, PredictionEventRef, PredictionEventsPage,
        },
    };
    use smol_str::SmolStr;
    use widget_tokio_runtime::global;

    use super::{PredictionEventSelector, PredictionEventSource};

    /// A page source that needs Tokio's timer, exactly like the HTTP clients do.
    #[derive(Clone, Default)]
    struct SlowSource {
        reads: Arc<AtomicUsize>,
        events: Arc<Vec<PredictionEventCard>>,
        next_cursor: Option<SmolStr>,
    }

    impl PredictionEventSource for SlowSource {
        async fn page(
            &self,
            _request: market_participants_connector::market::prediction::PredictionEventsRequest,
        ) -> Result<PredictionEventsPage, MarketError> {
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(PredictionEventsPage {
                events: self.events.as_ref().clone(),
                categories: Vec::new(),
                next_cursor: self.next_cursor.clone(),
                total: Some(self.events.len() as u32),
            })
        }
    }

    fn card(index: usize) -> PredictionEventCard {
        PredictionEventCard {
            event: PredictionEventRef {
                route: VenueRoute::new(
                    VenueId::Polymarket,
                    VenueScopeId::new("predict").expect("the test scope should be valid"),
                ),
                native_event: IdentifierValue::new(format!("event-{index}"))
                    .expect("the test event should be valid"),
            },
            title: SmolStr::new(format!("Event {index}")),
            category: PredictionCategory::new("politics", "Politics"),
            ends_at: None,
            volume: None,
            liquidity: None,
            options: Vec::new(),
            remaining_options: 0,
            market_count: 1,
            remaining_markets: 0,
        }
    }

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn install(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::base::init(cx);
            base_ui::style::theme::install(cx);
            embedded::install(cx).expect("the embedded locales should load");
            global::install(cx).expect("the test Tokio runtime should build");
            let manager = ConfigManager::in_memory(Config::default(), cx);
            cx.set_global(manager);
        });
    }

    fn selector(
        window: &mut VisualTestContext,
        source: SlowSource,
    ) -> Entity<PredictionEventSelector<SlowSource>> {
        window.update(|window, cx| {
            let search = cx
                .new(|cx| InputStateBuilder::new("prediction-event-search-test").build(window, cx));
            cx.new(|cx| {
                PredictionEventSelector::new(
                    source,
                    vec![VenueId::Polymarket],
                    None,
                    None,
                    search,
                    Rc::new(|_: PredictionEventRef, _: SmolStr, _: &mut Window, _: &mut App| {}),
                    cx,
                )
            })
        })
    }

    /// The event button names an event by its title, so the dialog hands the
    /// title back with the identity it confirmed.
    #[gpui_kit::test]
    fn confirming_an_event_should_pass_the_title(cx: &mut TestAppContext) {
        install(cx);
        let confirmed = Rc::new(RefCell::new(None));
        let captured = confirmed.clone();
        let source = SlowSource::default();
        let event = card(0).event;
        let title = card(0).title;
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = window.update(|window, cx| {
            let search = cx
                .new(|cx| InputStateBuilder::new("prediction-event-search-test").build(window, cx));
            cx.new(|cx| {
                PredictionEventSelector::new(
                    source,
                    vec![VenueId::Polymarket],
                    Some(event.clone()),
                    Some(title.clone()),
                    search,
                    Rc::new(
                        move |event: PredictionEventRef,
                              title: SmolStr,
                              _: &mut Window,
                              _: &mut App| {
                            *captured.borrow_mut() = Some((event, title));
                        },
                    ),
                    cx,
                )
            })
        });

        window.update(|window, cx| {
            selector.update(cx, |selector, cx| selector.confirm(window, cx));
        });

        assert_eq!(confirmed.borrow().clone(), Some((event, title)));
    }

    fn settle_until(window: &mut VisualTestContext, what: &str, ready: impl Fn(&App) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            window.run_until_parked();
            let handle = window.update(|_, cx| global::handle(cx).handle().clone());
            handle.block_on(async { tokio::task::yield_now().await });
            if window.update(|_, cx| ready(cx)) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what} should happen");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The read is started from a GPUI event, but the market clients use Tokio's
    /// reactor. A read that runs on GPUI's executor panics before the dialog can draw.
    #[gpui_kit::test]
    fn reading_a_page_should_run_on_the_application_tokio_runtime(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource::default();
        let window = cx.add_empty_window();
        let selector = selector(window, source.clone());

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.reload(cx)));
        settle_until(window, "the page to arrive", |cx| {
            !selector.read(cx).loading
        });

        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
        assert!(window.update(|_, cx| selector.read(cx).error.is_none()));
        assert_eq!(
            window.update(|_, cx| { selector.read(cx).page.as_ref().and_then(|page| page.total) }),
            Some(0)
        );
    }

    #[gpui_kit::test]
    fn each_request_reads_the_current_language(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource::default();
        let window = cx.add_empty_window();
        let selector = selector(window, source);

        let request = window.update(|_, cx| selector.read(cx).request(cx));
        assert_eq!(request.locale.as_deref(), Some("zh"));

        window.update(|_, cx| {
            cx.update_global::<ConfigManager, _>(|manager, cx| {
                manager.update(|config| config.language = Language::En, cx);
            });
        });
        let request = window.update(|_, cx| selector.read(cx).request(cx));
        assert_eq!(request.locale, None);
    }

    /// The selector must be drawable in the layer that opens it, with the page it
    /// read; this is the path that was invisible while the read panicked.
    #[gpui_kit::test]
    fn the_open_selector_should_draw_its_empty_page(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource::default();
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source);

        window.update(|window, cx| {
            window.dialog(cx, selector.clone());
            selector.update(cx, |selector, cx| selector.reload(cx));
        });
        settle_until(window, "the empty page to arrive", |cx| {
            !selector.read(cx).loading
        });

        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("PREDICTION_EVENT_SELECTOR").is_some(),
            "the selector dialog should be drawn with its empty page"
        );
    }

    /// A full page is two rows of four cards, and every space around them is the
    /// same width: the grid's padding matches the gap between cards.
    #[gpui_kit::test]
    fn a_full_page_should_draw_eight_cards_with_even_gaps(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource {
            events: Arc::new((0..8).map(card).collect()),
            ..Default::default()
        };
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source);

        window.update(|window, cx| {
            window.dialog(cx, selector.clone());
            selector.update(cx, |selector, cx| selector.reload(cx));
        });
        settle_until(window, "the full page to arrive", |cx| {
            !selector.read(cx).loading
        });

        window.update(|window, _| window.refresh());
        window.run_until_parked();
        let body = window
            .debug_bounds("PREDICTION_EVENTS_BODY")
            .expect("the grid's container should be drawn");
        let cards: Vec<_> = (0..8)
            .map(|index| {
                let selector = Box::leak(format!("PREDICTION_EVENT_CARD_{index}").into_boxed_str());
                window
                    .debug_bounds(selector)
                    .expect("every card of a full page should be drawn")
            })
            .collect();

        let gap_x = cards[1].left().as_f32() - cards[0].right().as_f32();
        let gap_y = cards[4].top().as_f32() - cards[0].bottom().as_f32();
        let spaces = [
            gap_x,
            gap_y,
            cards[0].left().as_f32() - body.left().as_f32() - 1.0,
            body.right().as_f32() - cards[3].right().as_f32() - 1.0,
            cards[0].top().as_f32() - body.top().as_f32() - 1.0,
            body.bottom().as_f32() - cards[7].bottom().as_f32() - 1.0,
        ];
        assert!(gap_x > 0.0, "the cards should not touch");
        for space in spaces {
            assert!(
                (space - gap_x).abs() <= 1.0,
                "every space around the cards should equal the gap: {spaces:?}"
            );
        }
    }

    /// The category tabs belong to the venue being shown. Switching venues must
    /// drop the old venue's tabs before the new venue's first page arrives.
    #[gpui_kit::test]
    fn switching_venue_should_clear_the_event_categories(cx: &mut TestAppContext) {
        install(cx);
        let window = cx.add_empty_window();
        let selector = selector(window, SlowSource::default());

        window.update(|window, cx| {
            selector.update(cx, |selector, _| {
                selector
                    .categories
                    .push(PredictionCategory::new("politics", "Politics"));
            });
            selector.update(cx, |selector, cx| {
                selector.select_venue(VenueId::Hyperliquid, window, cx)
            });
        });

        window.update(|_, cx| {
            assert!(selector.read(cx).categories.is_empty());
            assert!(selector.read(cx).category.is_none());
        });
    }

    /// Moving to another page keeps the old page in memory for the footer, but the
    /// body shows the spinner while the new page is being read.
    #[gpui_kit::test]
    fn paging_should_show_the_spinner_in_the_middle_of_the_container(cx: &mut TestAppContext) {
        install(cx);
        let source = SlowSource {
            events: Arc::new((0..8).map(card).collect()),
            next_cursor: Some(SmolStr::new("next")),
            ..Default::default()
        };
        let (_, window) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        let selector = selector(window, source);

        window.update(|window, cx| {
            window.dialog(cx, selector.clone());
            selector.update(cx, |selector, cx| selector.reload(cx));
        });
        settle_until(window, "the first page to arrive", |cx| {
            !selector.read(cx).loading
        });

        window.update(|window, _| window.refresh());
        window.run_until_parked();
        assert!(
            window.debug_bounds("PREDICTION_EVENTS_GRID").is_some(),
            "the first page should be drawn before paging"
        );

        window.update(|_, cx| selector.update(cx, |selector, cx| selector.next_page(cx)));
        window.run_until_parked();
        let spinner = window
            .debug_bounds("PREDICTION_EVENT_SPINNER")
            .expect("the loading spinner should be drawn while the next page is read");
        let body = window
            .debug_bounds("PREDICTION_EVENTS_BODY")
            .expect("the grid's container should be drawn");
        assert!(
            (spinner.center().x.as_f32() - body.center().x.as_f32()).abs() <= 1.0
                && (spinner.center().y.as_f32() - body.center().y.as_f32()).abs() <= 1.0,
            "the spinner should sit in the middle of the container"
        );

        settle_until(window, "the next page to arrive", |cx| {
            !selector.read(cx).loading
        });
    }
}
