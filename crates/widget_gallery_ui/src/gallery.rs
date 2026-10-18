//! The widget gallery: the dialog in which a pane's widget is chosen.
//!
//! The gallery is built from the registry rather than from a list of its own: the
//! categories are [`WidgetCategory::ALL`] and the widgets of a category are the
//! registered widgets that declare it and may be chosen. A widget is therefore
//! listed unless it says otherwise, and a widget added to the catalogue appears
//! here without anything being written here. The one entry that is not listed is
//! the widget a pane falls back to: the framework builds it, the user never picks
//! it, and it is the pane's own starting point rather than an answer to this
//! dialog.
//!
//! What the dialog holds across frames is only what the user has done in it: the
//! category being shown and the widget the grid has selected. Both open on the
//! widget the pane is already showing, so opening the gallery on a pane that has one
//! is opening it on the answer that pane already has.
//!
//! The column of categories is a tree of the dialog's own, so which category is the
//! current one is the row the tree has selected rather than something the rows are
//! built with: a row is a category and its label, and a click on one arrives as the
//! selection the dialog reads back through [`category_of_row`].

use std::rc::Rc;

use base_ui::{
    button::Button,
    grid_list::GridList,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
    layer::LayerExt,
    separator::Separator,
    style::theme::ThemeExt as _,
    tree::Tree,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, DismissEvent, ElementId, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement, Pixels, Render, RenderOnce, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled as _, Subscription, WeakEntity, Window,
    base::{Disableable, Selectable as _, TreeItem, TreeState},
    div, img, point, px,
};
use i18n::{language::codes::LanguageCode, t};
use widget::{category::WidgetCategory, identity::WidgetId};
use widget_registry::registry::WidgetRegistry;

/// The size of the panel the dialog draws.
const WIDTH: Pixels = px(800.0);
const HEIGHT: Pixels = px(560.0);

/// The width of the column of categories.
const CATEGORY_WIDTH: Pixels = px(160.0);

/// How many widgets the grid draws per row. A tile is as wide as its column, so the
/// grid is four tiles wide because it is drawn four columns wide.
const GRID_COLUMNS: u16 = 4;

/// The height of one widget in the grid, which holds an icon and the widget's name
/// under it.
const TILE_HEIGHT: Pixels = px(140.0);

/// The size of a widget's icon.
const ICON_SIZE: Pixels = px(64.0);

/// Debug selectors of the dialog's own parts.
const PANEL_SELECTOR: &str = "WIDGET_GALLERY";
const CATEGORIES_SELECTOR: &str = "WIDGET_GALLERY_CATEGORIES";
const CONFIRM_SELECTOR: &str = "WIDGET_GALLERY_CONFIRM";
const CANCEL_SELECTOR: &str = "WIDGET_GALLERY_CANCEL";
const DECLARED_ICON_SELECTOR: &str = "WIDGET_GALLERY_ICON_DECLARED";
const FALLBACK_ICON_SELECTOR: &str = "WIDGET_GALLERY_ICON_FALLBACK";

/// The name the tree knows the row of one category by.
///
/// The tree names its rows, and a row the dialog is told about is read back through this: a name
/// has to be known before the frame it is drawn in, so each category's is written out rather than
/// built from the category's own name.
fn category_row_name(category: WidgetCategory) -> &'static str {
    match category {
        WidgetCategory::Market => "widget-gallery-category-market",
        WidgetCategory::Statistics => "widget-gallery-category-statistics",
        WidgetCategory::Monitoring => "widget-gallery-category-monitoring",
    }
}

/// The category a row of the column stands for, from the name the tree reports it under.
fn category_of_row(name: &SharedString) -> Option<WidgetCategory> {
    WidgetCategory::ALL
        .into_iter()
        .find(|category| name.as_str() == category_row_name(*category))
}

/// The column of categories as the tree draws it: one row per category, labelled in the language
/// the window is showing.
fn category_rows(cx: &App) -> Vec<TreeItem> {
    WidgetCategory::ALL
        .into_iter()
        .map(|category| TreeItem::new(category_row_name(category), t!(cx, category.label())))
        .collect()
}

/// Opens the widget gallery as a dialog on the window layer.
///
/// `current` is the widget the pane is showing, which the gallery opens selected;
/// `choose` is handed the widget that was confirmed, before the dialog closes.
pub fn show_widget_gallery(
    current: Option<WidgetId>,
    choose: impl Fn(&WidgetId, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let gallery = cx.new(|cx| WidgetGallery::new(current, choose, cx));

    window.dialog(cx, gallery);
}

/// The dialog: the categories on the left, the widgets one of them holds on the
/// right, and the two decisions below them.
struct WidgetGallery {
    /// The widget the pane is showing, which the gallery opens selected.
    current: Option<WidgetId>,
    /// Every widget this dialog offers, in registry order, with the category it
    /// declares.
    widgets: Vec<GalleryWidget>,
    /// The category whose widgets the grid shows.
    category: WidgetCategory,
    /// The widget the grid has selected, which is what the confirm control adds.
    selection: Option<WidgetId>,
    /// The column of categories, which the tree draws and keeps the selection of.
    ///
    /// Which category is the current one is the row the tree has selected; what this dialog holds
    /// is the category behind that row, and the two are kept in step through
    /// [`category_of_row`].
    categories: Entity<TreeState>,
    /// The language the rows of the column were labelled in.
    ///
    /// A row holds its label, so rows of a language the window has left are rows that have to be
    /// built again; what the grid draws is built afresh on every frame and follows on its own.
    language: LanguageCode,
    /// Where the grid is scrolled to, which the grid scrolls through and which every
    /// category it shows starts from.
    grid_scroll: ScrollHandle,
    /// What a confirmed choice is handed to.
    choose: Rc<dyn Fn(&WidgetId, &mut Window, &mut App)>,
    /// Held so a tile can act on the dialog that drew it.
    gallery: WeakEntity<Self>,
    /// Held because the subscription is what follows the column: dropping it would leave a row
    /// that is clicked and never shown.
    _category_selection: Subscription,
}

/// One widget as the gallery lists it.
struct GalleryWidget {
    id: WidgetId,
    category: WidgetCategory,
    /// Message key of the widget's name.
    display_name: &'static str,
    /// The asset path the widget declared, if it declared one.
    icon: Option<&'static str>,
}

impl WidgetGallery {
    fn new(
        current: Option<WidgetId>,
        choose: impl Fn(&WidgetId, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let widgets = cx
            .global::<WidgetRegistry>()
            .iter()
            .filter(|registration| registration.descriptor().selectable())
            .map(|registration| {
                let descriptor = registration.descriptor();

                GalleryWidget {
                    id: descriptor.id().clone(),
                    category: descriptor.category(),
                    display_name: descriptor.display_name(),
                    icon: descriptor.icon(),
                }
            })
            .collect::<Vec<_>>();
        // The dialog opens on the widget the pane is showing: the category is the one
        // that widget declares, and the widget itself is what the grid has selected.
        // A pane with nothing in it opens on the first category instead, with nothing
        // selected, because there is no answer to open on.
        let shown = current
            .as_ref()
            .and_then(|current| widgets.iter().find(|widget| &widget.id == current));
        let category = shown.map_or(WidgetCategory::ALL[0], |widget| widget.category);
        let selection = shown.map(|widget| widget.id.clone());
        let categories = cx.new(|cx| TreeState::new(cx).items(category_rows(cx)));
        categories.update(cx, |categories, cx| {
            let selected = SharedString::new_static(category_row_name(category));
            categories.set_selected_index(categories.index_of(&selected), cx);
        });
        // The tree moves its own selection — a row is selected by clicking it, and the arrow keys
        // move it while the tree has focus — so what the dialog shows follows the row the tree
        // reports rather than the row a click happened to land on.
        let category_selection = cx.observe(&categories, |gallery, categories, cx| {
            let selected = categories
                .read(cx)
                .selected_item()
                .map(|item| item.id.clone());
            if let Some(category) = selected.as_ref().and_then(category_of_row) {
                gallery.show_category(category, cx);
            }
        });
        Self {
            current,
            widgets,
            category,
            selection,
            categories,
            language: *cx.global::<LanguageCode>(),
            grid_scroll: ScrollHandle::new(),
            choose: Rc::new(choose),
            gallery: cx.weak_entity(),
            _category_selection: category_selection,
        }
    }

    /// Shows the widgets of another category.
    ///
    /// What was selected in the category the user left is not what the confirm
    /// control would add any more, so nothing is selected here — with one exception:
    /// the widget the pane is already showing is selected again when its own category
    /// comes back, because coming back to it is asking for the answer it already has.
    fn show_category(&mut self, category: WidgetCategory, cx: &mut Context<Self>) {
        if self.category == category {
            return;
        }

        self.category = category;
        let restored = self
            .current
            .as_ref()
            .filter(|current| self.category_of(current) == Some(category))
            .cloned();
        self.selection = restored;
        self.grid_scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// Rebuilds the rows of the column in the language the window is showing now.
    ///
    /// A row holds the label it was built with, so the column is the one part of the dialog that
    /// does not follow a language by being drawn again: it is built again here, with the row of
    /// the category being shown selected, because the tree is given its rows afresh.
    fn adopt_language(&mut self, cx: &mut Context<Self>) {
        let language = *cx.global::<LanguageCode>();
        if language == self.language {
            return;
        }

        self.language = language;
        let selected = SharedString::new_static(category_row_name(self.category));
        self.categories.update(cx, |categories, cx| {
            categories.set_items(category_rows(cx), cx);
            categories.set_selected_index(categories.index_of(&selected), cx);
        });
    }

    /// Selects one widget of the category being shown.
    fn select(&mut self, widget_id: &WidgetId, cx: &mut Context<Self>) {
        if self.selection.as_ref() == Some(widget_id) {
            return;
        }

        self.selection = Some(widget_id.clone());
        cx.notify();
    }

    /// The category one registered widget declares.
    fn category_of(&self, widget_id: &WidgetId) -> Option<WidgetCategory> {
        self.widgets
            .iter()
            .find(|widget| &widget.id == widget_id)
            .map(|widget| widget.category)
    }

    /// The widgets of the category being shown, as a grid four wide.
    ///
    /// The grid is what scrolls, and it draws the scrollbar that reaches the rows which do
    /// not fit: a category can outgrow the panel, and a catalogue only grows. It is drawn
    /// through this dialog's handle for where it is scrolled to, so a category the user turns
    /// to starts from the top. The column of categories beside it is fixed instead, because
    /// the three of them always fit.
    fn grid(&self, cx: &App) -> AnyElement {
        let tiles = self
            .widgets
            .iter()
            .filter(|widget| widget.category == self.category)
            .map(|widget| {
                let selected = self.selection.as_ref() == Some(&widget.id);

                self.tile(widget, selected, cx)
            });

        GridList::new(
            ElementId::Name(SharedString::new_static("widget-gallery-grid")),
            GRID_COLUMNS,
        )
        .scroll_handle(self.grid_scroll.clone())
        .flex_1()
        .min_w_0()
        .min_h_0()
        .children(tiles)
        .into_any_element()
    }

    /// One widget in the grid: its icon over its name, as the button that selects it.
    ///
    /// A tile fills the column it is drawn in rather than being drawn at a width of its own,
    /// so the four of a row are as wide as the grid divides the panel.
    fn tile(&self, widget: &GalleryWidget, selected: bool, cx: &App) -> AnyElement {
        let gallery = self.gallery.clone();
        let widget_id = widget.id.clone();

        div()
            .w_full()
            .h(TILE_HEIGHT)
            .p_1()
            .child(
                Button::new(ElementId::Name(SharedString::from(widget.id.as_str())))
                    .ghost()
                    .selected(selected)
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .items_center()
                    .justify_center()
                    .child(WidgetIcon::new(widget.icon))
                    .child(
                        div()
                            .text_xs()
                            .text_center()
                            .child(t!(cx, widget.display_name)),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = gallery.update(cx, |gallery, cx| gallery.select(&widget_id, cx));
                    }),
            )
            .into_any_element()
    }

    /// The two decisions: leave without choosing, or add the widget that is selected.
    ///
    /// The confirm control refuses input while nothing is selected: a category the
    /// user is only browsing holds no answer, and a control that added nothing would
    /// close the dialog as if it had chosen something.
    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .gap_2()
            .child(
                Button::new(ElementId::Name(SharedString::new_static(
                    "widget-gallery-cancel",
                )))
                .tonal()
                .debug_selector(|| String::from(CANCEL_SELECTOR))
                .aria_label(t!(cx, "common.cancel"))
                .child(t!(cx, "common.cancel"))
                .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
            .child(
                Button::new(ElementId::Name(SharedString::new_static(
                    "widget-gallery-confirm",
                )))
                .filled()
                .debug_selector(|| String::from(CONFIRM_SELECTOR))
                .aria_label(t!(cx, "common.confirm"))
                .disabled(self.selection.is_none())
                .child(t!(cx, "common.confirm"))
                .on_click(cx.listener(|gallery, _, window, cx| {
                    // The choice is handed over before the gallery asks to close: the emit is
                    // deferred to the end of the effect cycle, so the callback still runs and the
                    // dialog closes behind it.
                    let Some(widget_id) = gallery.selection.clone() else {
                        return;
                    };
                    (gallery.choose)(&widget_id, window, cx);
                    cx.emit(DismissEvent);
                })),
            )
            .into_any_element()
    }
}

/// The gallery closes its dialog by emitting this, which is the signal the dialog listens for:
/// saying it is done comes from the gallery's own context, not from the window.
impl EventEmitter<DismissEvent> for WidgetGallery {}

impl Render for WidgetGallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.adopt_language(cx);

        let text = cx.theme().roles.fg.default.hsla;
        let grid = self.grid(cx);
        let footer = self.footer(cx);

        div()
            .debug_selector(|| String::from(PANEL_SELECTOR))
            .w(WIDTH)
            .h(HEIGHT)
            .flex()
            .flex_col()
            .gap_2()
            .text_color(text)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .debug_selector(|| String::from(CATEGORIES_SELECTOR))
                            .w(CATEGORY_WIDTH)
                            .flex_none()
                            .min_h_0()
                            .child(Tree::new(
                                "widget-gallery-categories".into(),
                                self.categories.clone(),
                            )),
                    )
                    .child(Separator::vertical())
                    .child(grid),
            )
            .child(Separator::horizontal())
            .child(footer)
    }
}

/// The icon of one widget in the grid.
///
/// A widget declares an asset path rather than a picture, so the path is resolved
/// where the icon is drawn. The declared icon is a picture and is drawn through the
/// image pipeline — the same one the application's other artwork goes through — so
/// any format that pipeline decodes is a valid declaration. A widget that declares
/// no icon gets the gallery's own, so a widget that has not been given a picture yet
/// is still a picture in the grid rather than a gap beside its name.
#[derive(IntoElement)]
struct WidgetIcon {
    icon: Option<&'static str>,
}

impl WidgetIcon {
    fn new(icon: Option<&'static str>) -> Self {
        Self { icon }
    }
}

impl RenderOnce for WidgetIcon {
    fn render(self, _window: &mut Window, _: &mut App) -> impl IntoElement {
        let fallback = self.icon.is_none();
        let icon = match self.icon {
            Some(path) => img(path).size(ICON_SIZE).into_any_element(),
            None => Icon::new(IconKind::PuzzlePiece)
                .size(ICON_SIZE)
                .into_any_element(),
        };

        div()
            .debug_selector(move || {
                String::from(if fallback {
                    FALLBACK_ICON_SELECTOR
                } else {
                    DECLARED_ICON_SELECTOR
                })
            })
            .flex_none()
            .child(icon)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, ffi::OsStr, path::Path, rc::Rc, time::Duration};

    use base_ui::{
        animation::ANIMATION_TICK,
        layer::{Layer, LayerExt as _},
    };
    use gpui_kit::{
        App, AppContext as _, AssetSource as _, Context, DismissEvent, Entity, EventEmitter, Img,
        InteractiveElement as _, IntoElement, Modifiers, ParentElement, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px, size, test::TestWindowExt as _,
    };
    use widget::{category::WidgetCategory, identity::WidgetId};
    use widget_registry::registry::WidgetRegistry;

    use super::{
        CANCEL_SELECTOR, CONFIRM_SELECTOR, DECLARED_ICON_SELECTOR, FALLBACK_ICON_SELECTOR,
        PANEL_SELECTOR, WidgetGallery, WidgetIcon, category_of_row, show_widget_gallery,
    };

    /// The dialog a test draws over the gallery.
    const OVER_SELECTOR: &str = "WIDGET_GALLERY_TEST_OVER";

    /// The state a test drives the dialog through, and the window it is drawn in.
    struct GalleryHarness<'a> {
        gallery: Entity<WidgetGallery>,
        cx: &'a mut VisualTestContext,
    }

    impl GalleryHarness<'_> {
        fn category(&self) -> WidgetCategory {
            self.cx.read(|cx| self.gallery.read(cx).category)
        }

        fn selection(&self) -> Option<String> {
            self.cx.read(|cx| {
                self.gallery
                    .read(cx)
                    .selection
                    .as_ref()
                    .map(|id| id.as_str().to_owned())
            })
        }

        /// The category the column has selected, which is the row a click on the tree moves.
        fn selected_row(&self) -> Option<WidgetCategory> {
            self.cx.read(|cx| {
                self.gallery
                    .read(cx)
                    .categories
                    .read(cx)
                    .selected_item()
                    .and_then(|item| category_of_row(&item.id))
            })
        }
    }

    /// A window holding nothing but the gallery, drawn for `current`.
    fn gallery<'a>(cx: &'a mut TestAppContext, current: Option<&str>) -> GalleryHarness<'a> {
        cx.update(install);
        gallery_over(cx, current)
    }

    /// The same over the state [`install`] already put in place: what a test that read
    /// the registry before it built the window needs.
    fn gallery_over<'a>(cx: &'a mut TestAppContext, current: Option<&str>) -> GalleryHarness<'a> {
        let current = current.map(widget);
        let gallery =
            cx.update(move |cx| cx.new(|cx| WidgetGallery::new(current, |_, _, _| {}, cx)));
        let root = gallery.clone();
        let (_, cx) = cx.add_window_view(|_, _| GalleryRoot { gallery: root });
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.run_until_parked();

        GalleryHarness { gallery, cx }
    }

    /// Installs what the gallery is built on: the theme, the translator and the
    /// catalogue.
    ///
    /// Each is an application-wide global an application installs exactly once, so a
    /// test that reads the catalogue before it builds the window installs them once and
    /// draws with [`gallery_over`] rather than installing them again.
    fn install(cx: &mut App) {
        base_ui::style::theme::install(cx);
        i18n::global::embedded::install(cx).expect("embedded locales should load");
        widget_registry::catalogue::install(cx).expect("the catalogue should install");
    }

    fn widget(id: &str) -> WidgetId {
        WidgetId::parse(id).expect("the test widget id should be valid")
    }

    /// Every widget the gallery offers, in registry order, with the category it
    /// declares.
    fn listed_in_registry(harness: &GalleryHarness) -> Vec<(WidgetId, WidgetCategory)> {
        harness.cx.read(|cx| {
            cx.global::<WidgetRegistry>()
                .iter()
                .filter(|registration| registration.descriptor().selectable())
                .map(|registration| {
                    let descriptor = registration.descriptor();

                    (descriptor.id().clone(), descriptor.category())
                })
                .collect()
        })
    }

    /// Lets the dialog's own fade finish and repaints, so what is drawn is what has
    /// settled: a dialog in its first frames is painted at zero opacity and cannot be
    /// told from one that stays.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(ANIMATION_TICK + Duration::from_millis(50));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    struct GalleryRoot {
        gallery: Entity<WidgetGallery>,
    }

    impl Render for GalleryRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.gallery.clone())
        }
    }

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// A dialog drawn over the gallery, so that a test can tell a dialog the gallery closed from
    /// the dialog it was opened under.
    struct Over;

    impl EventEmitter<DismissEvent> for Over {}

    impl Render for Over {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| String::from(OVER_SELECTOR))
                .w(px(200.0))
                .h(px(100.0))
        }
    }

    /// A pane with nothing in it has no answer to open on, so the gallery opens on the
    /// first category with nothing selected.
    #[gpui_kit::test]
    fn a_pane_without_a_widget_should_open_on_the_first_category(cx: &mut TestAppContext) {
        let harness = gallery(cx, None);

        assert_eq!(harness.category(), WidgetCategory::ALL[0]);
        assert_eq!(harness.selection(), None);
        assert_eq!(
            harness.selected_row(),
            Some(WidgetCategory::ALL[0]),
            "the column should open with the category it shows selected"
        );
    }

    /// The widget a pane falls back to is not an answer this dialog offers, so a pane
    /// showing it opens exactly like a pane showing nothing: the first category, with
    /// nothing selected.
    #[gpui_kit::test]
    fn a_pane_showing_the_fallback_widget_should_open_with_nothing_selected(
        cx: &mut TestAppContext,
    ) {
        let harness = gallery(cx, Some("kytrix.empty"));

        assert_eq!(harness.category(), WidgetCategory::ALL[0]);
        assert_eq!(harness.selection(), None);
    }

    /// The column of categories is the tree, so a category is chosen by clicking its row, and
    /// what the dialog shows follows the row the tree reports.
    #[gpui_kit::test]
    fn clicking_a_category_row_should_show_the_widgets_it_holds(cx: &mut TestAppContext) {
        let harness = gallery(cx, None);
        let statistics = WidgetCategory::ALL[1];

        // The rows of the tree are the categories in their own order, and a row is clicked by
        // the position it is drawn at.
        harness.cx.update(|window, cx| window.click(1usize, cx));

        assert_eq!(harness.category(), statistics);
        assert_eq!(harness.selected_row(), Some(statistics));
        assert_eq!(
            harness.selection(),
            None,
            "a category the user only turned to holds no answer to confirm"
        );
    }

    /// The column opens on the category of the widget the pane shows, which is the row the tree
    /// has selected: the two columns say the same thing from the first frame.
    #[gpui_kit::test]
    fn the_column_should_open_on_the_category_of_the_widget_the_pane_shows(
        cx: &mut TestAppContext,
    ) {
        cx.update(install);
        let (id, category) = cx.update(|cx| {
            let registry = cx.global::<WidgetRegistry>();
            let descriptor = registry
                .iter()
                .map(|registration| registration.descriptor())
                .find(|descriptor| descriptor.category() != WidgetCategory::ALL[0])
                .expect("the catalogue should register a widget outside the first category");

            (descriptor.id().clone(), descriptor.category())
        });
        let harness = gallery_over(cx, Some(id.as_str()));

        assert_eq!(
            harness.category(),
            category,
            "the gallery should open on the category of the widget the pane shows"
        );
        assert_eq!(
            harness.selected_row(),
            Some(category),
            "the column should open with the category it is showing selected"
        );
    }

    /// The gallery lists what may be chosen, under the category each widget
    /// declares: a widget that is registered is a widget that can be picked, unless
    /// it is built by the framework rather than chosen — and none is listed twice.
    #[gpui_kit::test]
    fn the_gallery_should_list_every_widget_that_may_be_chosen_once(cx: &mut TestAppContext) {
        let harness = gallery(cx, None);
        let listed = harness.cx.read(|cx| {
            harness
                .gallery
                .read(cx)
                .widgets
                .iter()
                .map(|widget| (widget.id.clone(), widget.category))
                .collect::<Vec<_>>()
        });

        assert_eq!(
            listed,
            listed_in_registry(&harness),
            "the gallery lists the registry, in its order"
        );
        assert!(
            !listed.iter().any(|(id, _)| id.as_str() == "kytrix.empty"),
            "the pane's fallback is not an answer this dialog offers"
        );
    }

    /// The entry opens the gallery as the window's own dialog, and closing it takes it
    /// off the window again.
    #[gpui_kit::test]
    fn the_entry_should_open_the_gallery_as_a_dialog(cx: &mut TestAppContext) {
        cx.update(install);
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_none(),
            "nothing is open yet"
        );
        cx.update(|window, cx| show_widget_gallery(None, |_, _, _| {}, window, cx));
        settle(cx);
        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_some(),
            "the gallery should be the window's dialog"
        );

        let cancel = cx
            .debug_bounds(CANCEL_SELECTOR)
            .expect("the dialog should offer a way out of it");
        cx.simulate_click(cancel.center(), Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_none(),
            "closing the dialog should take the gallery off the window"
        );
    }

    /// The gallery is what closes the dialog it is drawn in: the control that leaves it raises the
    /// signal its dialog subscribes to, and that signal reaches the dialog holding the gallery
    /// wherever it sits on the layer — which is what dismissing through the window cannot do.
    #[gpui_kit::test]
    fn the_gallery_should_close_its_own_dialog(cx: &mut TestAppContext) {
        cx.update(install);
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.run_until_parked();

        let gallery = cx.update(|_, cx| cx.new(|cx| WidgetGallery::new(None, |_, _, _| {}, cx)));
        cx.update(|window, cx| window.dialog(cx, gallery.clone()));
        settle(cx);

        let cancel = cx
            .debug_bounds(CANCEL_SELECTOR)
            .expect("the dialog should offer a way out of it");
        cx.simulate_click(cancel.center(), Modifiers::none());
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_none(),
            "the gallery's own control should have taken its dialog off the window once the exit \
             transition was over"
        );

        // The gallery is opened again, this time under another dialog: a dialog takes the mouse
        // from the one below it, so the control underneath cannot be clicked, and what still
        // closes the gallery's own dialog is the signal the gallery itself raises.
        let over = cx.update(|_, cx| cx.new(|_| Over));
        cx.update(|window, cx| {
            window.dialog(cx, gallery.clone());
            window.dialog(cx, over);
        });
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_some(),
            "the gallery should be open again"
        );
        assert!(
            cx.debug_bounds(OVER_SELECTOR).is_some(),
            "the dialog opened over it should be drawn"
        );

        cx.update(|_, cx| gallery.update(cx, |_, cx| cx.emit(DismissEvent)));
        settle(cx);

        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_none(),
            "a gallery that is not the dialog on top should still close its own dialog"
        );
        assert!(
            cx.debug_bounds(OVER_SELECTOR).is_some(),
            "the dialog opened over the gallery should be left where it is"
        );
    }

    /// The confirm control is what a choice is made through: the widget the grid has selected is
    /// handed to the callback the gallery was opened with — with the window the dialog is drawn in
    /// — and the dialog only closes behind it.
    #[gpui_kit::test]
    fn the_confirm_control_should_hand_the_choice_over_before_the_dialog_closes(
        cx: &mut TestAppContext,
    ) {
        cx.update(install);
        let chosen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view(|_, cx| Layer::new(cx.new(|_| Empty)));
        cx.simulate_resize(size(px(1000.0), px(800.0)));
        cx.run_until_parked();

        let widget_id = cx.read(|cx| {
            cx.global::<WidgetRegistry>()
                .iter()
                .next()
                .expect("the catalogue should register a widget")
                .descriptor()
                .id()
                .clone()
        });
        let gallery = {
            let chosen = chosen.clone();
            cx.update(|_, cx| {
                cx.new(|cx| {
                    WidgetGallery::new(
                        None,
                        move |id: &WidgetId, _, _| chosen.borrow_mut().push(id.as_str().to_owned()),
                        cx,
                    )
                })
            })
        };
        cx.update(|window, cx| window.dialog(cx, gallery.clone()));
        // What the grid has selected is what the confirm control adds, and a control with nothing
        // to add refuses input.
        cx.update(|_, cx| gallery.update(cx, |gallery, cx| gallery.select(&widget_id, cx)));
        settle(cx);

        let confirm = cx
            .debug_bounds(CONFIRM_SELECTOR)
            .expect("the dialog should offer a way to confirm");
        cx.simulate_click(confirm.center(), Modifiers::none());
        settle(cx);

        assert_eq!(
            chosen.borrow().first().map(String::as_str),
            Some(widget_id.as_str()),
            "the callback should have been handed the widget the grid had selected"
        );
        assert!(
            cx.debug_bounds(PANEL_SELECTOR).is_none(),
            "confirming should have closed the dialog once the exit transition was over"
        );
    }

    /// A widget that declares no icon is drawn with the gallery's own, so a widget
    /// that has not been given a picture yet is still a picture in the grid.
    #[gpui_kit::test]
    fn an_icon_should_be_the_declared_one_and_the_galleries_own_without_one(
        cx: &mut TestAppContext,
    ) {
        cx.update(base_ui::style::theme::install);
        struct Icons;

        impl Render for Icons {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .flex()
                    .child(WidgetIcon::new(None))
                    .child(WidgetIcon::new(Some("widgets/candlestick.png")))
            }
        }

        let (_, cx) = cx.add_window_view(|_, _| Icons);
        cx.run_until_parked();

        assert!(cx.debug_bounds(FALLBACK_ICON_SELECTOR).is_some());
        assert!(cx.debug_bounds(DECLARED_ICON_SELECTOR).is_some());
    }

    /// Every widget the gallery lists brings a picture of its own, and that picture is one
    /// the application carries and the image pipeline decodes.
    ///
    /// The gallery draws the path a widget declares, so a path that names nothing —
    /// or a picture in a format the image pipeline cannot read, which draws nothing
    /// at all — is a widget that appears in the grid without an icon and without
    /// anything saying so. A widget the gallery does not offer is not drawn there at
    /// all, so it is not checked here either.
    #[gpui_kit::test]
    fn every_listed_widget_should_declare_an_icon_the_gallery_can_draw(cx: &mut TestAppContext) {
        cx.update(install);
        let declared = cx.update(|cx| {
            cx.global::<WidgetRegistry>()
                .iter()
                .filter(|registration| registration.descriptor().selectable())
                .map(|registration| {
                    (
                        registration.descriptor().id().clone(),
                        registration.descriptor().icon(),
                    )
                })
                .collect::<Vec<_>>()
        });

        for (id, icon) in declared {
            let path = icon.unwrap_or_else(|| panic!("{id} should declare an icon"));
            let format = Path::new(path)
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default();

            assert!(
                Img::extensions().contains(&format),
                "{id} declares {path}, which the image pipeline does not decode"
            );
            assert!(
                base_ui::assets::Assets
                    .load(path)
                    .expect("the asset source should answer")
                    .is_some(),
                "{id} declares {path}, which the application does not carry"
            );
        }
    }
}
