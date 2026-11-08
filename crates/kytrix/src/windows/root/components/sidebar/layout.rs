//! The sidebar's geometry, and the cycle the title-bar toggle walks its states through.
//!
//! The measurements live here rather than at the call sites because several things have to
//! agree on them: the panel, the rows inside it, the title bar whose height the panel and
//! the page leave room for, and the tests that say a row fits the panel it is drawn in.

use app_config::ui::SidebarMode;
use gpui_kit::{Pixels, px};

/// How wide the sidebar is while it draws its labels.
pub(crate) const SIDEBAR_FULL_WIDTH: Pixels = px(220.0);

/// How wide the sidebar is while it draws icons only.
///
/// It is the item's square with one padding on either side, so the rail leaves the same gap
/// left and right; the square also sits under the title bar's toggle, which is the same
/// square.
pub(crate) const SIDEBAR_ICON_WIDTH: Pixels = px(40.0);

/// The space between the sidebar's edge and the rows inside it.
pub(crate) const SIDEBAR_PADDING: Pixels = px(4.0);

/// The square every item of the sidebar takes: a row's height in either state, and the
/// width an item keeps while the panel draws icons only.
///
/// The title bar's toggle is the same square, so the two line up when the panel is docked
/// under it.
pub(crate) const SIDEBAR_ITEM_SIZE: Pixels = px(32.0);

/// The space between two items of the workspace list.
///
/// It is also the space between the list and the button that adds a workspace, so the
/// section keeps one vertical rhythm.
pub(crate) const SIDEBAR_ITEM_GAP: Pixels = px(4.0);

/// The horizontal padding inside a full row's ghost button.
///
/// The button lays out a transparent one-pixel border of its own, and the icon-only square
/// centres its mark at `(SIDEBAR_ITEM_SIZE - SIDEBAR_ICON_SIZE) / 2` = 8 px. Seven pixels
/// of padding inside that border put a full row's mark at the same eight from the button's
/// edge, so the mark stays where the pointer saw it when the rail expands.
pub(crate) const SIDEBAR_FULL_PADDING: Pixels = px(7.0);

/// How wide a workspace's name may grow before it is cut off with an ellipsis.
pub(crate) const SIDEBAR_NAME_MAX_WIDTH: Pixels = px(150.0);

/// How large the workspace mark and the settings gear of the foot are drawn.
pub(crate) const SIDEBAR_ICON_SIZE: Pixels = px(16.0);

/// The height of the title bar.
///
/// The window draws it over everything, so the sidebar and the page both start their content
/// this far down: the panel's background reaches the top of the window, its content does not.
pub(crate) const TITLE_BAR_HEIGHT: Pixels = px(40.0);

/// Where the sidebar toggle sits, measured from the window's left edge on macOS, where the
/// traffic lights the system draws in that corner end.
pub(crate) const TOGGLE_INSET_MACOS: Pixels = px(78.0);

/// Where the sidebar toggle sits on every platform that draws no traffic lights.
///
/// The toggle is a 32 px control centred in the 40 px bar, so its top margin is 4 px. The
/// inset is that same margin, which makes the button look equally far from the window's top
/// and left edges; it also lines the button up with the row column the panel's 4 px padding
/// starts.
pub(crate) const TOGGLE_INSET: Pixels = px(4.0);

/// The width the sidebar settles at in `mode`: none while it is hidden, the icon rail, or
/// the full panel.
///
/// The window animates toward it rather than cutting to it, so a state change moves the
/// panel and the page beside it together.
pub(crate) fn width(mode: SidebarMode) -> Pixels {
    match mode {
        SidebarMode::Hidden => px(0.0),
        SidebarMode::IconOnly => SIDEBAR_ICON_WIDTH,
        SidebarMode::Full => SIDEBAR_FULL_WIDTH,
    }
}

/// The mode the title bar's toggle advances to: hidden, icons only, labels, hidden again.
pub(crate) fn next_mode(mode: SidebarMode) -> SidebarMode {
    match mode {
        SidebarMode::Hidden => SidebarMode::IconOnly,
        SidebarMode::IconOnly => SidebarMode::Full,
        SidebarMode::Full => SidebarMode::Hidden,
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::px;

    use super::{
        SIDEBAR_FULL_PADDING, SIDEBAR_ICON_SIZE, SIDEBAR_ICON_WIDTH, SIDEBAR_ITEM_GAP,
        SIDEBAR_ITEM_SIZE, SIDEBAR_NAME_MAX_WIDTH, SIDEBAR_PADDING, SidebarMode, TITLE_BAR_HEIGHT,
        TOGGLE_INSET, TOGGLE_INSET_MACOS, next_mode, width,
    };

    /// One press of the toggle walks all three states and comes back to the one it started
    /// from, and each of them is drawn at its own width.
    #[test]
    fn the_toggle_should_cycle_hidden_to_icon_only_to_full_and_back() {
        let mut mode = SidebarMode::Hidden;

        for (next, settled) in [
            (SidebarMode::IconOnly, SIDEBAR_ICON_WIDTH),
            (SidebarMode::Full, super::SIDEBAR_FULL_WIDTH),
            (SidebarMode::Hidden, px(0.0)),
        ] {
            mode = next_mode(mode);

            assert_eq!(mode, next, "the toggle advances to the next state");
            assert_eq!(width(mode), settled, "{next:?} settles at its own width");
        }
    }

    /// The icon-only panel is the item's square and its two paddings, so the rail leaves the
    /// same gap on either side of the square the title bar's toggle draws.
    #[test]
    fn an_icon_only_panel_should_be_the_item_and_its_paddings() {
        assert_eq!(
            SIDEBAR_ICON_WIDTH,
            SIDEBAR_PADDING * 2.0 + SIDEBAR_ITEM_SIZE
        );
    }

    /// A full row's mark sits exactly where the icon-only square centres its mark: the
    /// transparent border of the row's ghost button counts toward the eight pixels, so the
    /// mark does not move when the rail expands.
    #[test]
    fn a_full_rows_mark_should_sit_where_the_icon_only_square_centres_its_mark() {
        let mark_inset = (SIDEBAR_ITEM_SIZE - SIDEBAR_ICON_SIZE) / 2.0;

        assert_eq!(SIDEBAR_FULL_PADDING + px(1.0), mark_inset);
    }

    /// The measurements the window and the panel share are the ones the design asks for.
    #[test]
    fn the_shared_measurements_should_be_the_designed_ones() {
        assert_eq!(super::SIDEBAR_FULL_WIDTH, px(220.0));
        assert_eq!(SIDEBAR_ICON_WIDTH, px(40.0));
        assert_eq!(SIDEBAR_NAME_MAX_WIDTH, px(150.0));
        assert_eq!(TITLE_BAR_HEIGHT, px(40.0));
        assert_eq!(SIDEBAR_ITEM_SIZE, px(32.0));
        assert_eq!(SIDEBAR_ITEM_GAP, px(4.0));
        assert_eq!(SIDEBAR_FULL_PADDING, px(7.0));
        assert_eq!(TOGGLE_INSET_MACOS, px(78.0));
        assert_eq!(TOGGLE_INSET, px(4.0));
    }

    /// The toggle's horizontal inset is the margin the bar leaves above it, so the button
    /// looks equally far from the window's top and left edges.
    #[test]
    fn the_toggle_inset_should_match_the_margin_above_the_toggle() {
        assert_eq!(TOGGLE_INSET, (TITLE_BAR_HEIGHT - SIDEBAR_ITEM_SIZE) / 2.0);
    }
}
