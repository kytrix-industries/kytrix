use app_config::ui::SidebarMode;
use ui_core::math::{LogicalPx, px};

pub(crate) const TITLE_BAR_HEIGHT: LogicalPx = px(40.0);

pub(crate) const CONTROL_GAP: LogicalPx = px(4.0);

pub(crate) const BAR_INSET: LogicalPx = px(4.0);

pub(crate) const BAR_INSET_MACOS: LogicalPx = px(78.0);

pub(crate) const BAR_RIGHT_INSET_MACOS: LogicalPx = px(12.0);

pub(crate) const SIDEBAR_FULL_WIDTH: LogicalPx = px(220.0);

pub(crate) const SIDEBAR_ICON_WIDTH: LogicalPx = px(40.0);

pub(crate) const SIDEBAR_PADDING: LogicalPx = px(4.0);

pub(crate) const SIDEBAR_ITEM_GAP: LogicalPx = px(4.0);

pub(crate) const PAGE_PADDING: LogicalPx = px(4.0);

pub(crate) const PANE_MIN_WIDTH: LogicalPx = px(120.0);

pub(crate) const PANE_COMPACT_WIDTH: LogicalPx = px(88.0);

pub(crate) const SPLIT_BAR_SIZE: LogicalPx = px(4.0);

pub(crate) const fn width(mode: SidebarMode) -> LogicalPx {
    match mode {
        SidebarMode::Hidden => LogicalPx::ZERO,
        SidebarMode::IconOnly => SIDEBAR_ICON_WIDTH,
        SidebarMode::Full => SIDEBAR_FULL_WIDTH,
    }
}

pub(crate) const fn next_mode(mode: SidebarMode) -> SidebarMode {
    match mode {
        SidebarMode::Hidden => SidebarMode::IconOnly,
        SidebarMode::IconOnly => SidebarMode::Full,
        SidebarMode::Full => SidebarMode::Hidden,
    }
}

pub(crate) const fn draws_labels(mode: SidebarMode) -> bool {
    matches!(mode, SidebarMode::Full)
}

pub(crate) const fn bar_inset() -> LogicalPx {
    if cfg!(target_os = "macos") {
        BAR_INSET_MACOS
    } else {
        BAR_INSET
    }
}

pub(crate) const fn bar_right_inset() -> LogicalPx {
    if cfg!(target_os = "macos") {
        BAR_RIGHT_INSET_MACOS
    } else {
        BAR_INSET
    }
}

pub(crate) const fn draws_system_buttons() -> bool {
    !cfg!(target_os = "macos")
}

#[cfg(test)]
mod tests {
    use app_config::ui::SidebarMode;

    use super::*;

    #[test]
    fn the_toggle_should_cycle_hidden_to_icon_only_to_full_and_back() {
        let mut mode = SidebarMode::Hidden;
        for (next, settled) in [
            (SidebarMode::IconOnly, SIDEBAR_ICON_WIDTH),
            (SidebarMode::Full, SIDEBAR_FULL_WIDTH),
            (SidebarMode::Hidden, LogicalPx::ZERO),
        ] {
            mode = next_mode(mode);
            assert_eq!(mode, next);
            assert_eq!(width(mode), settled);
        }
    }

    #[test]
    fn only_the_full_sidebar_should_draw_labels() {
        assert!(draws_labels(SidebarMode::Full));
        assert!(!draws_labels(SidebarMode::IconOnly));
        assert!(!draws_labels(SidebarMode::Hidden));
    }
}
