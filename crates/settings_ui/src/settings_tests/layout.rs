//! The tests of the page's layout: the header, the columns, and the sidebar's rows.

use app_config::settings::Config;
use gpui_kit::{Modifiers, SharedString, TestAppContext, px};
use market_participants_connector::venue::definition::supported_venues;

use super::test_support::*;
use crate::{pages::registry, search::SearchFilter};

#[gpui_kit::test]
fn the_page_should_open_on_the_default_page(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);

    dialog.read_with(cx, |dialog, _| {
        assert_eq!(dialog.current_page_for_test(), registry::DEFAULT_PAGE);
        assert_eq!(dialog.query_for_test(), &gpui_kit::SharedString::default());
        assert!(dialog.filter_for_test().is_all());
        // The row of the page the dialog shows is a row of the sidebar, so the two columns
        // say the same thing from the first frame.
        assert!(
            dialog.sidebar_for_test().iter().any(|entry| entry
                .page_id()
                .is_some_and(|id| id == registry::DEFAULT_PAGE)),
            "the sidebar should list the page the dialog opens on"
        );
    });
}

/// The header is the gallery's: the title on the left and the search field at the end of
/// the row, above the two columns. The field still narrows the sidebar; what moved is where
/// it is drawn.
#[gpui_kit::test]
fn the_header_should_hold_the_search_above_the_columns(cx: &mut TestAppContext) {
    let (_, cx) = settings(Config::default(), cx);

    let page = cx
        .debug_bounds("SETTINGS_PAGE")
        .expect("the settings page should be drawn");
    let column = cx
        .debug_bounds("SETTINGS_COLUMN")
        .expect("the page's content column should be drawn");
    let header = cx
        .debug_bounds("SETTINGS_HEADER")
        .expect("the header should be drawn");
    let title = cx
        .debug_bounds("SETTINGS_TITLE")
        .expect("the header should draw its title");
    let search = cx
        .debug_bounds("SETTINGS_SEARCH")
        .expect("the header should draw its search field");
    let sidebar = cx
        .debug_bounds("SETTINGS_SIDEBAR")
        .expect("the sidebar should be drawn");

    assert_eq!(
        column.size.width,
        px(1000.0).min(page.size.width),
        "the content column is 1000 px, or the page's whole width when the window is narrower"
    );
    assert!(
        (column.left() - (page.left() + (page.size.width - column.size.width) / 2.0)).abs()
            < px(1.0),
        "the content column should be centred in the page: column {column:?}, page {page:?}"
    );
    assert_eq!(
        search.size.width,
        px(240.0),
        "the search field is the gallery header's 240 px"
    );
    assert!(
        search.left() > title.right(),
        "the search field is at the end of the header, not over the title: \
         search {search:?}, title {title:?}"
    );
    assert!(
        search.top() >= header.top() && search.bottom() <= header.bottom(),
        "the search field belongs to the header: search {search:?}, header {header:?}"
    );
    assert!(
        sidebar.top() >= header.bottom(),
        "the columns start under the header: sidebar {sidebar:?}, header {header:?}"
    );
    assert!(
        sidebar.right() < column.right(),
        "the menu column should leave the rest of the content column to the page"
    );
}

/// The settings page has no footer: there is nothing to confirm and nothing to cancel, so
/// the body is the page's last block.
#[gpui_kit::test]
fn the_page_should_not_draw_a_footer(cx: &mut TestAppContext) {
    let (_, cx) = settings(Config::default(), cx);

    let page = cx
        .debug_bounds("SETTINGS_PAGE")
        .expect("the settings page should be drawn");
    let column = cx
        .debug_bounds("SETTINGS_COLUMN")
        .expect("the page's content column should be drawn");

    for selector in ["SETTINGS_FOOTER", "SETTINGS_CANCEL", "SETTINGS_CONFIRM"] {
        assert!(
            cx.debug_bounds(selector).is_none(),
            "the settings page has no {selector}: a change is stored as it is made"
        );
    }
    assert_eq!(
        column.bottom(),
        page.bottom(),
        "the body should reach the foot of the page: column {column:?}, page {page:?}"
    );
}

#[gpui_kit::test]
fn selecting_a_page_should_show_it_and_reset_its_scroll(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(configured_settings(), cx);
    cx.run_until_parked();

    select(&dialog, "market_participants.okx", cx);

    dialog.read_with(cx, |dialog, _| {
        assert_eq!(dialog.current_page_for_test(), "market_participants.okx");
    });
}

/// The page drawn and the row that names it are one thing: the rows are drawn from the page
/// the page is on, so a page moved on its own would leave the row behind it, and a row
/// that moved the page back would take the field the user is typing into off the screen.
#[gpui_kit::test]
fn selecting_a_page_should_move_the_sidebar_with_it(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    select(&panel, "market_participants.okx", cx);

    assert_eq!(
        current_page(&panel, cx),
        "market_participants.okx",
        "the page selected and the page drawn are one thing"
    );

    // Working in the page is what the sidebar used to answer with its own selection: a control of
    // the page is pressed, and a sidebar that disagreed put its own page back under it.
    click(cx, "SETTINGS_CONTROL_market_participants.okx.region");
    assert_eq!(
        current_page(&panel, cx),
        "market_participants.okx",
        "the sidebar must not be able to move the page back off the page it is on"
    );
}

/// A row is what opens the page it names: the rows are the only way into a page besides
/// the search, so a row that does not reach its page is a page with no way in.
#[gpui_kit::test]
fn clicking_a_sidebar_row_should_open_its_page(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    let row = cx
        .debug_bounds("SETTINGS_ENTRY_market_participants.okx")
        .expect("the sidebar should list the OKX page");
    cx.simulate_click(row.center(), Modifiers::none());
    settle(cx);

    assert_eq!(
        current_page(&panel, cx),
        "market_participants.okx",
        "the row that was clicked should be the page that is drawn"
    );
}

/// The AI group sits between the general settings and the proxies, and its one page is the
/// translation page.
#[gpui_kit::test]
fn the_ai_group_should_open_the_translation_page(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    assert_eq!(
        labels(&panel, cx).into_iter().take(6).collect::<Vec<_>>(),
        [
            SharedString::new_static("General"),
            SharedString::new_static("Appearance"),
            SharedString::new_static("Data formatting"),
            SharedString::new_static("AI"),
            SharedString::new_static("Translation"),
            SharedString::new_static("Proxies"),
        ],
        "the AI group follows the general settings and precedes the proxies"
    );

    click(cx, "SETTINGS_ENTRY_ai.translation");

    assert_eq!(current_page(&panel, cx), "ai.translation");
}

/// Every page is drawn, one after another.
///
/// What this is for is the frames rather than the pixels: a page is built while the
/// dialog is being rendered, so a field that reached back into the dialog — or a view
/// that read it while it drew itself — would be a panic rather than a wrong color, and
/// this is what would find it.
#[gpui_kit::test]
fn every_page_should_be_drawable(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(configured_settings(), cx);
    let pages = registry::pages()
        .into_iter()
        .map(|page| page.id())
        .collect::<Vec<_>>();

    for page in pages {
        select(&dialog, page.as_str(), cx);

        dialog.read_with(cx, |dialog, _| {
            assert_eq!(
                dialog.current_page_for_test().as_ref(),
                page.as_ref(),
                "selecting {page} should draw it rather than another"
            );
        });
    }
}

/// A setting is one row: its name and explanation on the left, its control on the right, and the
/// row belongs to the card its group is drawn as.
#[gpui_kit::test]
fn a_setting_should_be_a_row_with_its_control_at_the_card_edge(cx: &mut TestAppContext) {
    let (_, cx) = settings(Config::default(), cx);

    let card = cx
        .debug_bounds("SETTINGS_CARD_appearance_0")
        .expect("the appearance group should be drawn as a card");
    let row = cx
        .debug_bounds("SETTINGS_FIELD_appearance")
        .expect("the appearance row should be drawn");
    let label = cx
        .debug_bounds("SETTINGS_LABEL_appearance")
        .expect("the row should draw its name");
    let description = cx
        .debug_bounds("SETTINGS_DESCRIPTION_appearance")
        .expect("the row should draw its explanation");
    let control = cx
        .debug_bounds("SETTINGS_CONTROL_appearance")
        .expect("the row should draw its control");

    assert!(card.contains(&row.center()), "the row belongs to the card");
    assert!(
        label.left() < control.left(),
        "the control is drawn beside the name rather than under it: name {label:?}, \
         control {control:?}"
    );
    assert!(
        description.top() >= label.bottom(),
        "the explanation is under the name: name {label:?}, explanation {description:?}"
    );
    assert!(
        description.left() < control.left(),
        "the explanation belongs to the side the name is on: explanation {description:?}, \
         control {control:?}"
    );
    assert!(
        (control.right() - (card.right() - px(20.0))).abs() < px(1.0),
        "the control column ends at the card's padding: control {control:?}, card {card:?}"
    );
}

/// The rows of one group share one card, and the card draws a line between them.
#[gpui_kit::test]
fn one_group_should_be_one_card_with_a_line_between_its_rows(cx: &mut TestAppContext) {
    let (_, cx) = settings(Config::default(), cx);

    let card = cx
        .debug_bounds("SETTINGS_CARD_appearance_0")
        .expect("the appearance group should be drawn as a card");
    let first = cx
        .debug_bounds("SETTINGS_FIELD_appearance")
        .expect("the first row should be drawn");
    let second = cx
        .debug_bounds("SETTINGS_FIELD_language")
        .expect("the second row should be drawn");
    let divider = cx
        .debug_bounds("SETTINGS_DIVIDER_appearance_0_1")
        .expect("the card should separate its rows");

    assert!(card.contains(&first.center()));
    assert!(card.contains(&second.center()));
    assert!(
        first.bottom() <= divider.top() && divider.bottom() <= second.top(),
        "the line belongs between the rows: first {first:?}, line {divider:?}, second {second:?}"
    );
    assert!(
        (divider.left() - (first.left() + px(20.0))).abs() < px(1.0),
        "the line should start where the row's padding ends: line {divider:?}, row {first:?}"
    );
    assert!(
        (divider.right() - (first.right() - px(20.0))).abs() < px(1.0),
        "the line should end where the row's padding starts: line {divider:?}, row {first:?}"
    );
    assert!(
        cx.debug_bounds("SETTINGS_DIVIDER_appearance_0_2").is_none(),
        "a card draws no line below its last row"
    );
}

/// A group whose heading repeats the only row it holds is drawn without that heading: the row
/// already names the setting, and two titles for one setting is one too many.
#[gpui_kit::test]
fn a_heading_that_repeats_its_only_row_should_not_be_drawn(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);

    select(&panel, "configure", cx);
    assert!(
        cx.debug_bounds("SETTINGS_SECTION_TITLE_configure_0")
            .is_none(),
        "the default line's heading repeats its row"
    );
    assert!(
        cx.debug_bounds("SETTINGS_SECTION_TITLE_configure_1")
            .is_some(),
        "the venue group is named over its rows"
    );

    select(&panel, "market_participants.okx", cx);
    assert!(
        cx.debug_bounds("SETTINGS_SECTION_TITLE_market_participants.okx_0")
            .is_none(),
        "the region's heading repeats its row"
    );
    assert!(
        cx.debug_bounds("SETTINGS_CARD_market_participants.okx_0")
            .is_some(),
        "the region and the credential set are rows of one card"
    );
    assert!(
        cx.debug_bounds("SETTINGS_CARD_market_participants.okx_1")
            .is_none(),
        "the credential set is a row of that card rather than a group of its own"
    );
}

/// The lines of the venues share one card: a card per venue would be five cards under the same
/// heading, and the heading already says what the rows have in common.
#[gpui_kit::test]
fn the_venue_group_should_be_one_card_for_every_venue(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    select(&panel, "configure", cx);

    let card = cx
        .debug_bounds("SETTINGS_CARD_configure_1")
        .expect("the venue group should be drawn as one card");
    let venues = supported_venues();
    for (index, venue) in venues.iter().enumerate() {
        let selector =
            Box::leak(format!("SETTINGS_FIELD_configure.{}", venue.as_str()).into_boxed_str());
        let row = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the venue card should draw {selector}"));
        assert!(
            card.contains(&row.center()),
            "the row for {} belongs to the venue card",
            venue.as_str()
        );
        if index > 0 {
            let divider = format!("SETTINGS_DIVIDER_configure_1_{index}");
            assert!(
                cx.debug_bounds(Box::leak(divider.into_boxed_str()))
                    .is_some(),
                "the venue card should separate row {index} from the row above it"
            );
        }
    }
}

/// Every page the registry offers is drawn as a sidebar row, and every row leads to one: a page
/// nobody can select is a page that does not exist, and a row that opens nothing is a dead end.
#[gpui_kit::test]
fn the_sidebar_and_the_pages_should_name_each_other(cx: &mut TestAppContext) {
    let (panel, cx) = settings(configured_settings(), cx);
    let drawn = panel.read_with(cx, |panel, _| {
        panel
            .sidebar_for_test()
            .iter()
            .filter_map(|entry| entry.page_id().cloned())
            .collect::<Vec<_>>()
    });
    let declared = registry::pages()
        .into_iter()
        .map(|page| page.id())
        .collect::<Vec<_>>();

    assert_eq!(drawn, declared);
    assert!(SearchFilter::new("", &[]).is_all());
}
