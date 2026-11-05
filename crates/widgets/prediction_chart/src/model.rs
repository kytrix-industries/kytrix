//! The probability window one chart draws.
//!
//! A prediction event is a set of markets, and one market is a set of outcome
//! sides. The chart follows one market at a time, keeps one line per side, and
//! caps the lines at [`MAX_LINES`]. Everything here is pure data: the drawing
//! code never asks the connector for a label or a probability.

use std::{collections::BTreeMap, sync::Arc};

use market_participants_model::{
    decimal::Fraction,
    outcome::OutcomeLegRef,
    prediction::{
        PredictionEventView, PredictionMarket, PredictionMarketGroup, PredictionMarketRef,
        PredictionPricePoint, PredictionSeries,
    },
};
use smol_str::SmolStr;

/// The most lines one chart draws.
pub(crate) const MAX_LINES: usize = 12;

/// The most points one line keeps.
pub(crate) const MAX_POINTS: usize = 2_000;

/// The markets a reader can switch to.
///
/// A live event still publishes the markets it has already settled; those are not
/// choices a reader can act on, so they are hidden. The market being drawn stays in
/// the list even after it closes, and an event with nothing open keeps every market
/// so a finished event's history remains chartable.
fn visible_markets<'a>(
    event: &'a PredictionEventView,
    selected: &'a PredictionMarketRef,
) -> impl Iterator<Item = &'a PredictionMarket> + 'a {
    let has_open = event.markets.iter().any(|market| market.open);
    event
        .markets
        .iter()
        .filter(move |market| !has_open || market.open || &market.market == selected)
}

/// One line of a prediction chart.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PredictionChartSeries {
    pub leg: OutcomeLegRef,
    pub label: SmolStr,
    /// The index of the chart colour this line is drawn in.
    pub color: usize,
    pub points: Arc<[PredictionPricePoint]>,
    pub latest: Option<Fraction>,
}

/// One market the header can switch to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredictionMarketChoice {
    pub market: PredictionMarketRef,
    pub label: SmolStr,
    pub group: PredictionMarketGroup,
}

/// Appends one live point and keeps the line inside its point budget.
fn push_live_point(points: &mut Vec<PredictionPricePoint>, point: PredictionPricePoint) {
    points.push(point);
    if points.len() > MAX_POINTS {
        points.drain(..points.len() - MAX_POINTS);
    }
}

/// One complete chart reading.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PredictionChartSnapshot {
    pub revision: u64,
    pub title: SmolStr,
    pub market: PredictionMarketRef,
    /// The markets a reader can switch to, in the order the venue listed them.
    pub markets: Arc<[PredictionMarketChoice]>,
    pub series: Arc<[PredictionChartSeries]>,
    /// How many sides the selected market has beyond the lines drawn.
    pub omitted: usize,
}

impl PredictionChartSnapshot {
    /// Builds a chart from one event view, the selected market and its series.
    pub(crate) fn new(
        revision: u64,
        event: PredictionEventView,
        market: PredictionMarket,
        series: Vec<PredictionSeries>,
    ) -> Self {
        let selected = market.market.clone();
        let labels: BTreeMap<OutcomeLegRef, SmolStr> = market
            .legs
            .iter()
            .map(|leg| (leg.leg.clone(), leg.label.clone()))
            .collect();
        let markets = visible_markets(&event, &selected)
            .map(|market| PredictionMarketChoice {
                market: market.market.clone(),
                label: market.label.clone(),
                group: market.group.clone(),
            })
            .collect();
        let mut lines: Vec<PredictionChartSeries> = series
            .into_iter()
            .map(|series| {
                let label = labels
                    .get(&series.leg)
                    .cloned()
                    .unwrap_or_else(|| SmolStr::new_static("Option"));
                let latest = series.points.last().map(|point| point.probability);
                PredictionChartSeries {
                    leg: series.leg,
                    label,
                    color: 0,
                    points: series.points.into(),
                    latest,
                }
            })
            .collect();
        lines.sort_by(|left, right| {
            right
                .latest
                .cmp(&left.latest)
                .then_with(|| left.label.cmp(&right.label))
        });
        let omitted = lines.len().saturating_sub(MAX_LINES);
        lines.truncate(MAX_LINES);
        for (index, line) in lines.iter_mut().enumerate() {
            line.color = index;
        }
        Self {
            revision,
            title: event.title,
            market: market.market,
            markets,
            series: lines.into(),
            omitted,
        }
    }

    /// The market groups in the order the event lists them.
    pub(crate) fn market_groups(&self) -> Vec<&PredictionMarketGroup> {
        let mut groups: Vec<&PredictionMarketGroup> = Vec::with_capacity(8);
        for market in self.markets.iter() {
            if !groups.iter().any(|group| group.key == market.group.key) {
                groups.push(&market.group);
            }
        }
        groups
    }

    /// The choice for one market, if the header offers it.
    pub(crate) fn choice(&self, market: &PredictionMarketRef) -> Option<&PredictionMarketChoice> {
        self.markets.iter().find(|choice| &choice.market == market)
    }

    /// The choice the header shows for a requested market.
    ///
    /// A switch inside the event names a market the snapshot already lists, so the
    /// header moves before the new reading arrives. A market the snapshot does not
    /// offer — the service fell back to another one — leaves the header on the
    /// market the snapshot draws.
    pub(crate) fn selected_choice(
        &self,
        selected: Option<&PredictionMarketRef>,
    ) -> Option<&PredictionMarketChoice> {
        selected
            .and_then(|market| self.choice(market))
            .or_else(|| self.choice(&self.market))
    }

    /// The markets of one group, in the order the event lists them.
    pub(crate) fn markets_in_group<'a>(
        &'a self,
        group: &'a PredictionMarketGroup,
    ) -> impl Iterator<Item = &'a PredictionMarketChoice> + 'a {
        self.markets
            .iter()
            .filter(move |market| market.group.key == group.key)
    }

    /// Appends one live reading to each line it names.
    ///
    /// A live tick is a new observation even when it carries the same timestamp
    /// as the point before it, so it is appended rather than folded into that
    /// point. Lines keep the order they had when the chart loaded: re-sorting on
    /// every tick would make the legend and the colours jump.
    pub(crate) fn with_live(mut self, latest: Vec<PredictionSeries>, revision: u64) -> Self {
        let mut by_leg: BTreeMap<OutcomeLegRef, PredictionPricePoint> = latest
            .into_iter()
            .filter_map(|series| {
                series
                    .points
                    .into_iter()
                    .next_back()
                    .map(|point| (series.leg, point))
            })
            .collect();
        let series: Vec<PredictionChartSeries> = self
            .series
            .iter()
            .cloned()
            .map(|mut line| {
                let Some(point) = by_leg.remove(&line.leg) else {
                    return line;
                };
                if line
                    .points
                    .last()
                    .is_some_and(|last| point.timestamp < last.timestamp)
                {
                    return line;
                }
                let mut points: Vec<PredictionPricePoint> = line.points.to_vec();
                push_live_point(&mut points, point);
                line.points = points.into();
                line.latest = line.points.last().map(|point| point.probability);
                line
            })
            .collect();
        self.revision = revision;
        self.series = series.into();
        self
    }
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use market_participants_model::{
        decimal::Fraction,
        prediction::{PredictionPricePoint, PredictionSeries},
    };

    use super::{MAX_LINES, MAX_POINTS, PredictionChartSnapshot, push_live_point};
    use crate::test_support::{charted_market, event_ref, event_with, market};

    fn ids(snapshot: &PredictionChartSnapshot) -> Vec<&str> {
        snapshot
            .markets
            .iter()
            .map(|market| market.market.native_market.as_str())
            .collect()
    }

    #[test]
    fn the_line_cap_is_the_agreed_twelve() {
        assert_eq!(MAX_LINES, 12);
    }

    #[test]
    fn a_live_line_keeps_only_its_newest_two_thousand_points() {
        let mut points = Vec::new();
        for timestamp in 0..=i64::try_from(MAX_POINTS).expect("the cap fits in i64") {
            push_live_point(
                &mut points,
                PredictionPricePoint {
                    timestamp,
                    probability: Fraction::ZERO,
                },
            );
        }

        assert_eq!(points.len(), MAX_POINTS);
        assert_eq!(points.first().map(|point| point.timestamp), Some(1));
        assert_eq!(
            points.last().map(|point| point.timestamp),
            Some(i64::try_from(MAX_POINTS).expect("the cap fits in i64"))
        );
    }

    #[test]
    fn a_live_tick_appends_a_point_and_keeps_the_lines_order() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let series: Vec<PredictionSeries> = legs
            .iter()
            .enumerate()
            .map(|(index, leg)| PredictionSeries {
                leg: leg.clone(),
                points: vec![PredictionPricePoint {
                    timestamp: 1_000,
                    probability: Fraction::new(
                        market_participants_model::decimal::Ratio::from_inner(
                            u128::from(index as u64) * 100_000_000_000_000_000,
                        ),
                    )
                    .expect("the fixture probability is a fraction"),
                }],
            })
            .collect();
        let view = event_with(vec![market.clone()]);
        let mut snapshot = PredictionChartSnapshot::new(1, view, market, series);
        let colors_before: Vec<usize> = snapshot.series.iter().map(|line| line.color).collect();
        let live: Vec<PredictionSeries> = legs
            .iter()
            .map(|leg| PredictionSeries {
                leg: leg.clone(),
                points: vec![PredictionPricePoint {
                    timestamp: 1_001,
                    probability: Fraction::ONE,
                }],
            })
            .collect();

        snapshot = snapshot.with_live(live, 2);

        assert_eq!(snapshot.revision, 2);
        let colors_after: Vec<usize> = snapshot.series.iter().map(|line| line.color).collect();
        assert_eq!(colors_after, colors_before, "a tick must not reorder lines");
        for line in snapshot.series.iter() {
            assert_eq!(
                line.points.last().map(|point| point.timestamp),
                Some(1_001),
                "every line takes the tick that names it"
            );
            assert_eq!(line.latest, Some(Fraction::ONE));
        }
    }

    #[test]
    fn a_live_tick_older_than_the_line_is_dropped() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);
        let series: Vec<PredictionSeries> = legs
            .iter()
            .map(|leg| PredictionSeries {
                leg: leg.clone(),
                points: vec![PredictionPricePoint {
                    timestamp: 1_000,
                    probability: Fraction::ONE,
                }],
            })
            .collect();
        let mut snapshot = PredictionChartSnapshot::new(1, view, market, series);
        let stale: Vec<PredictionSeries> = legs
            .iter()
            .map(|leg| PredictionSeries {
                leg: leg.clone(),
                points: vec![PredictionPricePoint {
                    timestamp: 999,
                    probability: Fraction::ZERO,
                }],
            })
            .collect();

        snapshot = snapshot.with_live(stale, 2);

        for line in snapshot.series.iter() {
            assert_eq!(
                line.points.len(),
                1,
                "a tick older than the last point is not a new observation"
            );
            assert_eq!(line.latest, Some(Fraction::ONE));
        }
    }

    #[test]
    fn a_live_event_hides_closed_markets() {
        let event = event_ref();
        let markets = vec![
            market(&event, "open-a", "group-a", true),
            market(&event, "closed-b", "group-b", false),
            market(&event, "open-c", "group-a", true),
        ];
        let view = event_with(markets.clone());
        let snapshot = PredictionChartSnapshot::new(1, view, markets[0].clone(), Vec::new());

        assert_eq!(ids(&snapshot), vec!["open-a", "open-c"]);
    }

    #[test]
    fn a_drawn_closed_market_stays_in_the_list() {
        let event = event_ref();
        let markets = vec![
            market(&event, "open-a", "group-a", true),
            market(&event, "closed-b", "group-b", false),
        ];
        let view = event_with(markets.clone());
        let snapshot = PredictionChartSnapshot::new(1, view, markets[1].clone(), Vec::new());

        assert_eq!(ids(&snapshot), vec!["open-a", "closed-b"]);
    }

    #[test]
    fn a_finished_event_keeps_every_market() {
        let event = event_ref();
        let markets = vec![
            market(&event, "closed-a", "group-a", false),
            market(&event, "closed-b", "group-b", false),
        ];
        let view = event_with(markets.clone());
        let snapshot = PredictionChartSnapshot::new(1, view, markets[0].clone(), Vec::new());

        assert_eq!(ids(&snapshot), vec!["closed-a", "closed-b"]);
    }

    #[test]
    fn the_header_follows_a_requested_market_until_the_snapshot_replaces_it() {
        let event = event_ref();
        let markets = vec![
            market(&event, "open-a", "group-a", true),
            market(&event, "open-b", "group-b", true),
            market(&event, "closed-c", "group-c", false),
        ];
        let view = event_with(markets.clone());
        let snapshot = PredictionChartSnapshot::new(1, view, markets[0].clone(), Vec::new());

        assert_eq!(
            snapshot
                .selected_choice(Some(&markets[1].market))
                .map(|choice| choice.market.native_market.as_str()),
            Some("open-b")
        );
        assert_eq!(
            snapshot
                .selected_choice(Some(&markets[2].market))
                .map(|choice| choice.market.native_market.as_str()),
            Some("open-a"),
            "a market the snapshot does not offer leaves the header on the drawn one"
        );
        assert_eq!(
            snapshot
                .selected_choice(None)
                .map(|choice| choice.market.native_market.as_str()),
            Some("open-a")
        );
    }

    #[test]
    fn market_groups_follow_the_listing_order_and_name_the_drawn_group() {
        let event = event_ref();
        let markets = vec![
            market(&event, "spread-a", "spreads", true),
            market(&event, "moneyline", "moneyline", true),
            market(&event, "spread-b", "spreads", true),
        ];
        let view = event_with(markets.clone());
        let snapshot = PredictionChartSnapshot::new(1, view, markets[1].clone(), Vec::new());

        let groups = snapshot.market_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key.as_str(), "spreads");
        assert_eq!(groups[1].key.as_str(), "moneyline");
        assert_eq!(
            snapshot
                .choice(&markets[1].market)
                .map(|choice| choice.group.key.as_str()),
            Some("moneyline")
        );
        assert_eq!(snapshot.markets_in_group(groups[0]).count(), 2);
    }
}
