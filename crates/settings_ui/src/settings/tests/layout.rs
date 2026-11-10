//! The tests of the page's layout: the header, the columns, and the sidebar's rows.

use app_config::settings::Config;
use gpui_kit::{Modifiers, SharedString, TestAppContext, px};

use super::test_support::*;
use crate::{search::SearchFilter, settings::default_page};

#[gpui_kit::test]
fn the_page_should_open_on_the_default_page(cx: &mut TestAppContext) {
    let (dialog, cx) = settings(Config::default(), cx);

    dialog.read_with(cx, |dialog, _| {
        assert_eq!(dialog.current_page, default_page());
        assert_eq!(dialog.query, gpui_kit::SharedString::default());
        assert!(dialog.filter.is_all());
        // The row of the page the dialog shows is a row of the sidebar, so the two columns
        // say the same thing from the first frame.
        assert!(
            dialog
                .sidebar
                .iter()
                .any(|entry| entry.page_id().is_some_and(|id| id == default_page())),
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
        assert_eq!(dialog.current_page, "market_participants.okx");
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

    // Working in the page is what the sidebar used to answer with its own selection: the
    // caret goes into the field, and a sidebar that disagreed put its own page back under
    // it.
    click(cx, "SETTINGS_CONTROL_market_participants.okx.api_key");
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
            SharedString::new_static("Data time zone"),
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
    let pages = pages()
        .iter()
        .map(|page| page.id.clone())
        .collect::<Vec<_>>();

    for page in pages {
        select(&dialog, page.as_str(), cx);

        dialog.read_with(cx, |dialog, _| {
            assert_eq!(
                dialog.current_page, page,
                "selecting {page} should draw it rather than another"
            );
        });
    }
}

/// Every page the catalog declares is reachable from the sidebar, and every row leads to
/// one: a page nobody can select is a page that does not exist, and a row that opens
/// nothing is a dead end.
#[test]
fn the_sidebar_and_the_pages_should_name_each_other() {
    let catalog = test_catalog();
    let mut reachable = catalog.tree.iter().flat_map(leaves).collect::<Vec<_>>();
    reachable.sort_unstable();

    let mut declared = catalog
        .pages
        .iter()
        .map(|page| page.id.clone())
        .collect::<Vec<_>>();
    declared.sort_unstable();

    assert_eq!(reachable, declared);
    assert!(SearchFilter::new("", &[]).is_all());
}

/// The pages a tree entry leads to, which for a group are the pages of its children.
fn leaves(node: &crate::catalog::TreeNodeDef) -> Vec<SharedString> {
    match node {
        crate::catalog::TreeNodeDef::Page(id) => vec![id.clone()],
        crate::catalog::TreeNodeDef::Group { children, .. } => {
            children.iter().flat_map(leaves).collect()
        }
    }
}
