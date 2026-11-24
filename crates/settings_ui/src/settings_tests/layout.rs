use market_participants_connector::venue::definition::supported_venues;
use smol_str::SmolStr;
use ui_core::math::{LogicalPoint, LogicalRect};

use crate::{pages::registry, settings_tests::test_support::Harness};

fn bounds(harness: &Harness, selector: &str) -> LogicalRect {
    let node = harness
        .find(selector)
        .unwrap_or_else(|| panic!("no node whose id starts with {selector}"));
    harness
        .bounds(node)
        .unwrap_or_else(|| panic!("{selector} has not been laid out"))
}

fn left(rect: LogicalRect) -> f32 {
    f32::from(rect.origin.x)
}

fn right(rect: LogicalRect) -> f32 {
    f32::from(rect.origin.x + rect.size.width)
}

fn top(rect: LogicalRect) -> f32 {
    f32::from(rect.origin.y)
}

fn bottom(rect: LogicalRect) -> f32 {
    f32::from(rect.origin.y + rect.size.height)
}

fn centre(rect: LogicalRect) -> LogicalPoint {
    LogicalPoint::new(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    )
}

fn aligned(first: f32, second: f32) -> bool {
    (first - second).abs() < 1.0
}

fn settled() -> Harness {
    let mut harness = Harness::new();
    harness.settle(3);
    harness
}

#[test]
fn the_settings_should_open_on_the_appearance_page() {
    let harness = settled();

    assert_eq!(harness.page().current_page(), registry::DEFAULT_PAGE);
    assert_eq!(harness.page().query(), "");
    assert!(
        harness.page().filter_for_test().is_all(),
        "nothing was typed, so nothing should be filtered out"
    );
    assert!(
        harness.find("SETTINGS_ENTRY_appearance").is_some(),
        "the sidebar should list the page the settings open on"
    );
}

#[test]
fn the_header_should_hold_the_search_above_the_columns() {
    let harness = settled();

    let page = bounds(&harness, "SETTINGS_PAGE");
    let header = bounds(&harness, "SETTINGS_HEADER");
    let title = bounds(&harness, "SETTINGS_TITLE");
    let search = bounds(&harness, "SETTINGS_SEARCH");
    let sidebar = bounds(&harness, "SETTINGS_SIDEBAR");
    let content = bounds(&harness, "SETTINGS_CONTENT");

    assert!(
        aligned(f32::from(search.size.width), 240.0),
        "the search field is the gallery header's 240 px: {search:?}"
    );
    assert!(
        left(search) > right(title),
        "the search field is at the end of the header, not over the title: \
         search {search:?}, title {title:?}"
    );
    assert!(
        top(search) >= top(header) && bottom(search) <= bottom(header),
        "the search field belongs to the header: search {search:?}, header {header:?}"
    );
    assert!(
        aligned(left(header), left(page)) && aligned(right(header), right(page)),
        "the header spans the page: header {header:?}, page {page:?}"
    );
    assert!(
        top(sidebar) >= bottom(header),
        "the columns start under the header: sidebar {sidebar:?}, header {header:?}"
    );
    assert!(
        right(sidebar) < right(content),
        "the menu column should leave the rest of the row to the page: \
         sidebar {sidebar:?}, content {content:?}"
    );
}

#[test]
fn the_page_should_not_draw_a_footer() {
    let harness = settled();

    for selector in ["SETTINGS_FOOTER", "SETTINGS_CANCEL", "SETTINGS_CONFIRM"] {
        assert!(
            harness.find(selector).is_none(),
            "the settings page has no {selector}: a change is stored as it is made"
        );
    }

    let page = bounds(&harness, "SETTINGS_PAGE");
    let body = bounds(&harness, "SETTINGS_BODY");
    assert!(
        aligned(bottom(body), bottom(page)),
        "the body should reach the foot of the page: body {body:?}, page {page:?}"
    );
}

#[test]
fn selecting_a_page_should_show_it() {
    let mut harness = settled();

    harness.select_page("market_participants.okx");

    assert_eq!(
        harness.page().current_page(),
        "market_participants.okx",
        "the row that was clicked should be the page that is drawn"
    );
}

#[test]
fn selecting_a_page_should_move_the_sidebar_with_it() {
    let mut harness = settled();

    harness.select_page("data-format");
    assert_eq!(harness.page().current_page(), "data-format");
    assert!(
        harness.find("SETTINGS_ENTRY_data-format").is_some(),
        "the sidebar should list the page it opened"
    );

    harness.click_node("SETTINGS_CONTROL_data-format.offset");
    harness.settle(1);

    assert_eq!(
        harness.page().current_page(),
        "data-format",
        "pressing a control of the page must not move the page off it"
    );
}

#[test]
fn clicking_a_sidebar_row_should_open_its_page() {
    let mut harness = settled();

    harness.click_node("SETTINGS_ENTRY_market_participants.okx");
    harness.settle(1);

    assert_eq!(
        harness.page().current_page(),
        "market_participants.okx",
        "the row that was clicked should be the page that is drawn"
    );
}

#[test]
fn the_ai_group_should_open_the_translation_page() {
    let mut harness = settled();

    assert_eq!(
        harness.labels().into_iter().take(6).collect::<Vec<_>>(),
        vec![
            SmolStr::new_static("General"),
            SmolStr::new_static("Appearance"),
            SmolStr::new_static("Data formatting"),
            SmolStr::new_static("AI"),
            SmolStr::new_static("Translation"),
            SmolStr::new_static("Proxies"),
        ],
        "the AI group follows the general settings and precedes the proxies"
    );

    harness.select_page("ai.translation");

    assert_eq!(harness.page().current_page(), "ai.translation");
}

#[test]
fn every_page_should_be_drawable() {
    let mut harness = settled();
    let pages = registry::pages()
        .into_iter()
        .map(|page| page.id())
        .collect::<Vec<_>>();

    for page in pages {
        harness.select_page(page.as_str());

        assert_eq!(
            harness.page().current_page().as_str(),
            page.as_str(),
            "selecting {page} should draw it rather than another"
        );
    }
}

#[test]
fn a_setting_should_be_a_row_with_its_control_at_the_card_edge() {
    let harness = settled();

    let card = bounds(&harness, "SETTINGS_CARD_appearance");
    let row = bounds(&harness, "SETTINGS_FIELD_appearance");
    let label = bounds(&harness, "SETTINGS_LABEL_appearance");
    let description = bounds(&harness, "SETTINGS_DESCRIPTION_appearance");
    let control = bounds(&harness, "SETTINGS_CONTROL_appearance");

    assert!(
        card.contains(centre(row)),
        "the row belongs to the card: row {row:?}, card {card:?}"
    );
    assert!(
        left(label) < left(control),
        "the control is drawn beside the name rather than under it: \
         name {label:?}, control {control:?}"
    );
    assert!(
        top(description) >= bottom(label),
        "the explanation is under the name: name {label:?}, explanation {description:?}"
    );
    assert!(
        left(description) < left(control),
        "the explanation belongs to the side the name is on: \
         explanation {description:?}, control {control:?}"
    );
    assert!(
        aligned(right(control), right(card) - 20.0),
        "the control column ends at the card's padding: control {control:?}, card {card:?}"
    );
}

#[test]
fn one_group_should_be_one_card_with_a_line_between_its_rows() {
    let harness = settled();

    let card = bounds(&harness, "SETTINGS_CARD_appearance");
    let first = bounds(&harness, "SETTINGS_FIELD_appearance");
    let second = bounds(&harness, "SETTINGS_FIELD_language");
    let divider = bounds(&harness, "SETTINGS_DIVIDER_appearance_1");

    assert!(
        card.contains(centre(first)) && card.contains(centre(second)),
        "both rows belong to the card: first {first:?}, second {second:?}, card {card:?}"
    );
    assert!(
        bottom(first) <= top(divider) + 1.0 && bottom(divider) <= top(second) + 1.0,
        "the line belongs between the rows: first {first:?}, line {divider:?}, second {second:?}"
    );
    assert!(
        aligned(left(divider), left(first) + 20.0),
        "the line should start where the row's padding ends: line {divider:?}, row {first:?}"
    );
    assert!(
        aligned(right(divider), right(first) - 20.0),
        "the line should end where the row's padding starts: line {divider:?}, row {first:?}"
    );
    assert!(
        harness.find("SETTINGS_DIVIDER_appearance_2").is_none(),
        "a card draws no line below its last row"
    );
}

#[test]
fn a_heading_that_repeats_its_only_row_should_not_be_drawn() {
    let mut harness = settled();

    harness.select_page("configure");
    harness.settle(1);

    assert!(
        harness.find("SETTINGS_SECTION_TITLE_configure_0").is_none(),
        "the default line's heading repeats its row"
    );
    assert!(
        harness.find("SETTINGS_SECTION_TITLE_configure_1").is_some(),
        "the venue group is named over its rows"
    );
    assert!(
        harness.find("SETTINGS_SECTION_TITLE_configure_2").is_some(),
        "the Kytrix group is named over its rows"
    );
}

#[test]
fn the_venue_group_should_be_one_card_for_every_venue() {
    let mut harness = settled();

    harness.select_page("configure");
    harness.settle(1);

    let card = bounds(&harness, "SETTINGS_CARD_configure_1");
    for (index, venue) in supported_venues().iter().enumerate() {
        let selector = format!("SETTINGS_FIELD_configure.{}", venue.as_str());
        let row = bounds(&harness, &selector);
        assert!(
            card.contains(centre(row)),
            "the row for {} belongs to the venue card: row {row:?}, card {card:?}",
            venue.as_str()
        );
        if index > 0 {
            let divider = format!("SETTINGS_DIVIDER_configure_1_{index}");
            assert!(
                harness.find(&divider).is_some(),
                "the venue card should separate row {index} from the row above it"
            );
        }
    }
}

#[test]
fn the_sidebar_and_the_pages_should_name_each_other() {
    let harness = settled();
    let declared = registry::pages()
        .into_iter()
        .map(|page| page.id())
        .collect::<Vec<_>>();

    for page in &declared {
        assert!(
            harness.find(&format!("SETTINGS_ENTRY_{page}")).is_some(),
            "the sidebar should draw the row of {page}"
        );
    }
    assert_eq!(
        harness.find_all("SETTINGS_ENTRY_").len(),
        declared.len(),
        "the sidebar should draw exactly the pages the registry declares"
    );
}

#[test]
fn the_page_content_should_scroll_inside_the_settings_page() {
    let harness = settled();

    let outer = bounds(&harness, "settings.test.page.viewport-outer");
    let viewport = bounds(&harness, "settings.test.page.viewport");
    let content = bounds(&harness, "SETTINGS_PAGE_CONTENT");
    let track = bounds(&harness, "settings.test.page.scrollbar:track");
    let row = bounds(&harness, "SETTINGS_FIELD_appearance");

    assert!(
        outer.contains(centre(viewport)) && viewport.contains(centre(content)),
        "the scroll content belongs to the viewport and the viewport to the page: \
         outer {outer:?}, viewport {viewport:?}, content {content:?}"
    );
    assert!(
        aligned(left(content), left(viewport))
            && aligned(
                f32::from(content.size.width),
                f32::from(viewport.size.width)
            ),
        "the content column fills the viewport: content {content:?}, viewport {viewport:?}"
    );
    assert!(
        content.contains(centre(row)),
        "the page's rows belong to its content column: row {row:?}, content {content:?}"
    );
    assert!(
        aligned(right(track), right(outer)) && top(track) >= top(outer),
        "the scroll bar is drawn along the page's trailing edge: track {track:?}, outer {outer:?}"
    );
}
