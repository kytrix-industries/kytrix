use std::{cell::RefCell, fmt, rc::Rc};

use market_participants_model::{
    listing::route::VenueListingKey, market::selection::InstrumentSelection,
};
use ui_core::app::App;

type ApplyHandler = Rc<dyn Fn(InstrumentSelection, &mut App) -> bool + 'static>;
type ChangeHandler = Rc<dyn Fn(InstrumentSelection, &mut App) + 'static>;

#[derive(Clone)]
pub struct InstrumentLink {
    selection: Rc<RefCell<InstrumentSelection>>,
    apply: ApplyHandler,
    on_change: Rc<RefCell<Option<ChangeHandler>>>,
}

impl fmt::Debug for InstrumentLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstrumentLink")
            .field("selection", &self.selection())
            .finish_non_exhaustive()
    }
}

impl InstrumentLink {
    pub fn new(
        selection: InstrumentSelection,
        apply: impl Fn(InstrumentSelection, &mut App) -> bool + 'static,
    ) -> Self {
        Self {
            selection: Rc::new(RefCell::new(selection)),
            apply: Rc::new(apply),
            on_change: Rc::new(RefCell::new(None)),
        }
    }

    pub fn selection(&self) -> InstrumentSelection {
        self.selection.borrow().clone()
    }

    pub fn listing(&self) -> VenueListingKey {
        self.selection.borrow().listing.clone()
    }

    pub fn apply_remote(&self, selection: InstrumentSelection, cx: &mut App) -> bool {
        if !(self.apply)(selection.clone(), cx) {
            return false;
        }
        *self.selection.borrow_mut() = selection;
        true
    }

    pub fn local_changed(&self, selection: InstrumentSelection, cx: &mut App) {
        *self.selection.borrow_mut() = selection.clone();
        let handler = self.on_change.borrow().clone();
        if let Some(handler) = handler {
            handler(selection, cx);
        }
    }

    pub fn set_change_handler(&self, handler: impl Fn(InstrumentSelection, &mut App) + 'static) {
        *self.on_change.borrow_mut() = Some(Rc::new(handler));
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use market_participants_model::{
        identifier::{VenueId, VenueScopeId},
        listing::{
            route::{VenueListingKey, VenueRoute},
            venue::VenueListingId,
        },
        market::selection::InstrumentSelection,
    };
    use serde_json::{Map, Value};
    use ui_core::app::App;

    use super::*;

    fn app() -> App {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("the test runtime builds");
        App::new(runtime.handle().clone())
    }

    fn route(scope: &str) -> VenueRoute {
        VenueRoute::new(
            VenueId::Binance,
            VenueScopeId::new(scope).expect("the fixture scope is canonical"),
        )
    }

    fn listing_id(digit: char) -> VenueListingId {
        let value = format!("listing-id/v1/{}", digit.to_string().repeat(64));
        VenueListingId::from_identity_value(&value).expect("the fixture listing id is canonical")
    }

    fn listing(scope: &str, digit: char) -> VenueListingKey {
        VenueListingKey::new(route(scope), listing_id(digit))
    }

    fn selection(scope: &str, digit: char) -> InstrumentSelection {
        InstrumentSelection {
            listing: listing(scope, digit),
        }
    }

    #[test]
    fn rejected_remote_selection_does_not_replace_value() {
        let link = InstrumentLink::new(selection("spot", 'a'), |_, _| false);
        let mut app = app();

        assert!(!link.apply_remote(selection("spot", 'b'), &mut app));
        assert_eq!(link.selection(), selection("spot", 'a'));
    }

    #[test]
    fn local_changes_broadcast_once_and_remote_changes_do_not_broadcast() {
        let applies = Rc::new(Cell::new(0));
        let changes = Rc::new(Cell::new(0));
        let link = InstrumentLink::new(selection("spot", 'a'), {
            let applies = applies.clone();
            move |_, _| {
                applies.set(applies.get() + 1);
                true
            }
        });
        link.set_change_handler({
            let changes = changes.clone();
            move |_, _| changes.set(changes.get() + 1)
        });

        let mut app = app();
        link.local_changed(selection("spot", 'b'), &mut app);
        assert!(link.apply_remote(selection("spot", 'c'), &mut app));

        assert_eq!((changes.get(), applies.get()), (1, 1));
        assert_eq!(link.selection(), selection("spot", 'c'));
    }

    #[test]
    fn a_link_should_propagate_the_exact_listing_key_it_was_given() {
        let link = InstrumentLink::new(selection("usd-m-perp", 'a'), |_, _| true);
        let mut app = app();

        assert!(link.apply_remote(selection("spot", 'b'), &mut app));

        assert_eq!(link.listing(), listing("spot", 'b'));
        assert_eq!(link.selection().listing.route, route("spot"));
    }

    #[test]
    fn two_listings_of_one_route_should_stay_distinct_selections() {
        let first = selection("spot", 'a');
        let second = selection("spot", 'b');
        let link = InstrumentLink::new(first.clone(), |_, _| true);
        let mut app = app();

        assert!(link.apply_remote(second.clone(), &mut app));

        assert_eq!(link.selection(), second);
        assert_ne!(link.selection(), first);
        assert_eq!(link.selection().listing.route, first.listing.route);
    }

    fn payload(members: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        let mut map = Map::with_capacity(2);
        for (name, value) in members {
            map.insert(name.to_owned(), value);
        }

        Value::Object(map)
    }

    #[test]
    fn a_selection_should_be_wire_shaped_as_the_listing_it_names() {
        let selection = selection("usd-m-perp", 'd');
        let encoded = serde_json::to_value(&selection).expect("a selection serializes");
        let listing = encoded
            .get("listing")
            .expect("the listing key is the only field of a selection");

        assert!(
            listing.get("route").is_some() && listing.get("listing_id").is_some(),
            "the listing key carries the route and the listing id: {encoded}"
        );
        assert_eq!(
            serde_json::from_value::<InstrumentSelection>(encoded.clone())
                .expect("what was written decodes"),
            selection
        );

        let route = serde_json::to_value(route("spot")).expect("a route serializes");
        let instrument = Value::String(
            "v1/0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        );
        let legacy = payload([("route", route), ("instrument", instrument)]);
        assert!(
            serde_json::from_value::<InstrumentSelection>(legacy).is_err(),
            "a selection that names an instrument rather than a listing is not a selection"
        );
        assert!(
            serde_json::from_value::<InstrumentSelection>(payload([])).is_err(),
            "a listing key is required rather than defaulted"
        );
    }
}
