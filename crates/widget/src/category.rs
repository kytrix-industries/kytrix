//! The group of the widget gallery a widget is listed under.

/// The group a widget belongs to.
///
/// A widget declares its own category, so a selection surface is built from the
/// registry rather than from a table beside it: a widget that is registered is a
/// widget that is listed, under the group it named.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WidgetCategory {
    /// Live market data.
    Market,
    /// Statistics aggregated from market data.
    Statistics,
    /// Alerts and monitored feeds.
    Monitoring,
}

impl WidgetCategory {
    /// Every category, in the order a selection surface lists them.
    pub const ALL: [Self; 3] = [Self::Market, Self::Statistics, Self::Monitoring];

    /// Returns the message key of this category's label.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Market => "widget.categories.market",
            Self::Statistics => "widget.categories.statistics",
            Self::Monitoring => "widget.categories.monitoring",
        }
    }
}

#[cfg(test)]
mod tests {
    use ahash::HashSet;

    use super::WidgetCategory;

    /// A label is a message key. A copy-paste mistake between two of them would draw
    /// one category in place of another — or draw it with no label at all, since a key
    /// that is missing from every locale translates to nothing.
    #[test]
    fn every_category_should_name_itself_once() {
        let labels = WidgetCategory::ALL.map(|category| category.label());

        assert_eq!(
            labels.into_iter().collect::<HashSet<_>>().len(),
            WidgetCategory::ALL.len()
        );
        for label in labels {
            assert!(
                label.starts_with("widget.categories."),
                "`{label}` is not a category key"
            );
        }
    }
}
