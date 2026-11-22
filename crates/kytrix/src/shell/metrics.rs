//! The shell's geometry, in one place.
//!
//! The measurements are shared by several views and their tests: the sidebar's three
//! widths, the item square the title bar's controls and the sidebar's rows share, the
//! title bar's height (which the sidebar and the page both leave room for), and the pane
//! split bounds. They live here rather than at the call sites so the views cannot drift
//! apart.

use app_config::ui::SidebarMode;

/// The height of the title bar.
///
/// The window draws it over everything, so the sidebar and the page both start their
/// content this far down.
pub(crate) const TITLE_BAR_HEIGHT: f32 = 40.0;

/// The gap between two title-bar controls.
pub(crate) const CONTROL_GAP: f32 = 4.0;

/// Where the title-bar controls start, measured from the window's left/top edge.
pub(crate) const BAR_INSET: f32 = 4.0;

/// Where the sidebar toggle sits on macOS, where the system's traffic lights take the
/// window's top-left corner.
pub(crate) const BAR_INSET_MACOS: f32 = 78.0;

/// The right inset of the title bar on macOS (the traffic lights are on the left; the
/// account control keeps its own margin on the right).
pub(crate) const BAR_RIGHT_INSET_MACOS: f32 = 12.0;

/// How wide the sidebar is while it draws its labels.
pub(crate) const SIDEBAR_FULL_WIDTH: f32 = 220.0;

/// How wide the sidebar is while it draws icons only: the item square and one padding on
/// either side.
pub(crate) const SIDEBAR_ICON_WIDTH: f32 = 40.0;

/// The space between the sidebar's edge and its rows.
pub(crate) const SIDEBAR_PADDING: f32 = 4.0;

/// The space between two sidebar rows.
pub(crate) const SIDEBAR_ITEM_GAP: f32 = 4.0;

/// The page's padding around its content.
pub(crate) const PAGE_PADDING: f32 = 4.0;

/// The minimum width a pane is laid out at before a split bar stops moving.
pub(crate) const PANE_MIN_WIDTH: f32 = 120.0;

/// The compact width a pane collapses to when the workspace is narrower than the minimum.
pub(crate) const PANE_COMPACT_WIDTH: f32 = 88.0;

/// The width (or height) of a split bar.
pub(crate) const SPLIT_BAR_SIZE: f32 = 4.0;

/// The width the sidebar settles at in `mode`.
pub(crate) const fn width(mode: SidebarMode) -> f32 {
    match mode {
        SidebarMode::Hidden => 0.0,
        SidebarMode::IconOnly => SIDEBAR_ICON_WIDTH,
        SidebarMode::Full => SIDEBAR_FULL_WIDTH,
    }
}

/// The mode the title bar's toggle advances to: hidden, icons only, labels, hidden again.
pub(crate) const fn next_mode(mode: SidebarMode) -> SidebarMode {
    match mode {
        SidebarMode::Hidden => SidebarMode::IconOnly,
        SidebarMode::IconOnly => SidebarMode::Full,
        SidebarMode::Full => SidebarMode::Hidden,
    }
}

/// Whether the sidebar draws labels in `mode`.
pub(crate) const fn draws_labels(mode: SidebarMode) -> bool {
    matches!(mode, SidebarMode::Full)
}

/// The left inset the title bar's first control needs on this platform.
pub(crate) const fn bar_inset() -> f32 {
    if cfg!(target_os = "macos") {
        BAR_INSET_MACOS
    } else {
        BAR_INSET
    }
}

/// The right inset the title bar's last control keeps on this platform.
pub(crate) const fn bar_right_inset() -> f32 {
    if cfg!(target_os = "macos") {
        BAR_RIGHT_INSET_MACOS
    } else {
        BAR_INSET
    }
}

/// Whether the platform draws the window's own chrome (the system draws its traffic lights
/// on macOS; the shell draws the title bar's buttons everywhere else).
pub(crate) const fn draws_system_buttons() -> bool {
    !cfg!(target_os = "macos")
}

#[cfg(test)]
mod tests {
    use app_config::ui::SidebarMode;

    use super::*;

    /// The toggle walks all three states and comes back, and each state has its own width.
    #[test]
    fn the_toggle_should_cycle_hidden_to_icon_only_to_full_and_back() {
        let mut mode = SidebarMode::Hidden;
        for (next, settled) in [
            (SidebarMode::IconOnly, SIDEBAR_ICON_WIDTH),
            (SidebarMode::Full, SIDEBAR_FULL_WIDTH),
            (SidebarMode::Hidden, 0.0),
        ] {
            mode = next_mode(mode);
            assert_eq!(mode, next);
            assert_eq!(width(mode), settled);
        }
    }

    /// Only the full state draws labels; both collapsed states are icon-only.
    #[test]
    fn only_the_full_sidebar_should_draw_labels() {
        assert!(draws_labels(SidebarMode::Full));
        assert!(!draws_labels(SidebarMode::IconOnly));
        assert!(!draws_labels(SidebarMode::Hidden));
    }
}
