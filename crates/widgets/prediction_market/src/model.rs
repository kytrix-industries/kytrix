//! The probability chart one market draws.
//!
//! A prediction event is a set of markets, and one market is a set of outcome
//! sides. The chart follows one market at a time, keeps one line per side, and
//! caps the lines at [`MAX_LINES`]. Everything here is pure data: the drawing
//! code never asks the connector for a label or a probability.

use std::{collections::BTreeMap, sync::Arc};

use market_participants_model::{
    decimal::{Fraction, UnixMillis},
    outcome::OutcomeLegRef,
    prediction::{
        PredictionEventRef, PredictionEventView, PredictionLeg, PredictionMarket,
        PredictionMarketGroup, PredictionMarketRef, PredictionPricePoint, PredictionRange,
        PredictionSeries,
    },
};
use smol_str::SmolStr;

/// The most lines one chart draws.
pub(crate) const MAX_LINES: usize = 12;

/// The most points one line keeps.
pub(crate) const MAX_POINTS: usize = 4_096;

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

/// One line of a prediction market chart.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PredictionMarketSeries {
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
pub(crate) struct PredictionMarketSnapshot {
    pub revision: u64,
    /// The range the reading was asked over.
    pub range: PredictionRange,
    pub title: SmolStr,
    pub market: PredictionMarketRef,
    /// Every side of the drawn market, in the order the venue listed them.
    ///
    /// The order is the venue's, because it is what a subscription to the
    /// market's topic is identified by: a book asked for with these legs shares
    /// the connection lease the chart already holds.
    pub legs: Arc<[PredictionLeg]>,
    /// The side the venue calls the market's first outcome.
    pub default_leg: Option<OutcomeLegRef>,
    /// The markets a reader can switch to, in the order the venue listed them.
    pub markets: Arc<[PredictionMarketChoice]>,
    pub series: Arc<[PredictionMarketSeries]>,
    /// How many sides the selected market has beyond the lines drawn.
    pub omitted: usize,
}

/// The book one chart reading points at: the market's sides and the one of them
/// the panel draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredictionOrderBookTarget {
    pub event: PredictionEventRef,
    /// Every leg of the drawn market, in venue order.
    pub legs: Vec<PredictionLeg>,
    pub leg: OutcomeLegRef,
}

impl PredictionMarketSnapshot {
    /// Builds a chart from one range, one event view, the selected market and
    /// its series.
    pub(crate) fn new(
        revision: u64,
        range: PredictionRange,
        event: PredictionEventView,
        market: PredictionMarket,
        series: Vec<PredictionSeries>,
    ) -> Self {
        let selected = market.market.clone();
        let legs: Arc<[PredictionLeg]> = market.legs.clone().into();
        let default_leg = market.default_leg.clone();
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
        // The sides stay in the order the venue published them: the legend and the
        // colours are read off this list, so sorting it by the latest value would
        // shuffle both on every tick and again on every reload.
        let mut lines: Vec<PredictionMarketSeries> = series
            .into_iter()
            .map(|mut series| {
                let label = labels
                    .get(&series.leg)
                    .cloned()
                    .unwrap_or_else(|| SmolStr::new_static("Option"));
                let latest = series.points.last().map(|point| point.probability);
                // History is the one read that can exceed the live point budget;
                // keep the newest points so a line never grows without bound.
                if series.points.len() > MAX_POINTS {
                    series.points.drain(..series.points.len() - MAX_POINTS);
                }
                PredictionMarketSeries {
                    leg: series.leg,
                    label,
                    color: 0,
                    points: series.points.into(),
                    latest,
                }
            })
            .collect();
        let omitted = lines.len().saturating_sub(MAX_LINES);
        lines.truncate(MAX_LINES);
        for (index, line) in lines.iter_mut().enumerate() {
            line.color = index;
        }
        Self {
            revision,
            range,
            title: event.title,
            market: market.market,
            legs,
            default_leg,
            markets,
            series: lines.into(),
            omitted,
        }
    }

    /// The book the panel shows: the market's default leg, or the first
    /// charted leg when the venue named no default.
    ///
    /// The leg has to be one of the market's own: a venue that named a side the
    /// market no longer publishes leaves the panel to the first line the chart
    /// actually drew. A market with no side at all has no book to ask for.
    pub(crate) fn book_target(&self) -> Option<PredictionOrderBookTarget> {
        if self.legs.is_empty() {
            return None;
        }
        let published = |leg: &OutcomeLegRef| self.legs.iter().any(|side| &side.leg == leg);
        let leg = self.default_leg.clone().filter(published).or_else(|| {
            self.series
                .iter()
                .map(|line| line.leg.clone())
                .find(published)
        })?;
        Some(PredictionOrderBookTarget {
            event: self.market.event.clone(),
            legs: self.legs.iter().cloned().collect(),
            leg,
        })
    }

    /// The newest point any line holds, for the endpoint pulse.
    pub(crate) fn latest_timestamp(&self) -> Option<UnixMillis> {
        self.series
            .iter()
            .flat_map(|line| line.points.iter())
            .map(|point| point.timestamp)
            .max()
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
        let series: Vec<PredictionMarketSeries> = self
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
mod tests {
    use market_participants_model::{
        decimal::Fraction,
        prediction::{PredictionPricePoint, PredictionRange, PredictionSeries},
    };

    use super::{MAX_LINES, MAX_POINTS, PredictionMarketSnapshot, push_live_point};
    use crate::test_support::{charted_market, event_ref, event_with, market};

    fn ids(snapshot: &PredictionMarketSnapshot) -> Vec<&str> {
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
    fn thirteen_sides_keep_twelve_lines_in_venue_order_and_report_the_rest() {
        let event = event_ref();
        let mut markets = Vec::with_capacity(13);
        let mut legs = Vec::with_capacity(13);
        let mut series = Vec::with_capacity(13);
        for index in 0..13 {
            let (market, market_legs) = charted_market(&event, &format!("charted-{index}"));
            let leg = market_legs[0].clone();
            legs.push(leg.clone());
            series.push(PredictionSeries {
                leg,
                points: vec![PredictionPricePoint {
                    timestamp: 1_000 + index as i64,
                    probability: Fraction::ONE,
                }],
            });
            markets.push(market);
        }
        // Every side is a leg of the drawn market, so each line has its venue label.
        let mut drawn = markets[0].clone();
        drawn.legs = markets
            .iter()
            .map(|market| market.legs[0].clone())
            .collect();
        let view = event_with(markets);

        let snapshot = PredictionMarketSnapshot::new(1, PredictionRange::All, view, drawn, series);

        assert_eq!(snapshot.series.len(), MAX_LINES);
        assert_eq!(
            snapshot
                .series
                .iter()
                .map(|line| line.leg.clone())
                .collect::<Vec<_>>(),
            legs.iter().take(MAX_LINES).cloned().collect::<Vec<_>>(),
            "the twelve kept lines are the venue's first twelve, in its order"
        );
        assert_eq!(
            snapshot
                .series
                .iter()
                .map(|line| line.color)
                .collect::<Vec<_>>(),
            (0..MAX_LINES).collect::<Vec<_>>(),
            "a colour is the kept line's position in that order"
        );
        assert_eq!(
            snapshot.omitted, 1,
            "the thirteenth side is reported, not drawn"
        );
    }

    #[test]
    fn the_point_cap_is_four_thousand_and_ninety_six() {
        assert_eq!(MAX_POINTS, 4_096);
    }

    #[test]
    fn a_live_line_keeps_only_the_points_inside_its_cap() {
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
    fn a_history_line_keeps_only_the_points_inside_its_cap() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);
        let points: Vec<PredictionPricePoint> = (0..=i64::try_from(MAX_POINTS)
            .expect("the cap fits"))
            .map(|timestamp| PredictionPricePoint {
                timestamp,
                probability: Fraction::ZERO,
            })
            .collect();
        let series = vec![PredictionSeries {
            leg: legs[0].clone(),
            points,
        }];

        let snapshot = PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);
        let line = snapshot
            .series
            .iter()
            .find(|line| line.leg == legs[0])
            .expect("the charted line is present");

        assert_eq!(line.points.len(), MAX_POINTS);
        assert_eq!(line.points.first().map(|point| point.timestamp), Some(1));
    }

    #[test]
    fn the_snapshot_echoes_the_range_it_was_read_over() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);
        let series = vec![PredictionSeries {
            leg: legs[0].clone(),
            points: vec![PredictionPricePoint {
                timestamp: 1_000,
                probability: Fraction::ONE,
            }],
        }];

        let snapshot =
            PredictionMarketSnapshot::new(7, PredictionRange::Week1, view, market, series);

        assert_eq!(snapshot.range, PredictionRange::Week1);
        assert_eq!(
            snapshot.with_live(Vec::new(), 8).range,
            PredictionRange::Week1,
            "a live tick keeps the range the reading was asked over"
        );
    }

    #[test]
    fn the_latest_timestamp_is_the_newest_point_across_every_line() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);
        let series: Vec<PredictionSeries> = legs
            .iter()
            .enumerate()
            .map(|(index, leg)| PredictionSeries {
                leg: leg.clone(),
                points: vec![PredictionPricePoint {
                    timestamp: 1_000 + index as i64,
                    probability: Fraction::ONE,
                }],
            })
            .collect();

        let snapshot = PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);

        assert_eq!(
            snapshot.latest_timestamp(),
            Some(1_000 + legs.len() as i64 - 1)
        );
    }

    #[test]
    fn an_empty_snapshot_has_no_latest_timestamp() {
        let event = event_ref();
        let (market, _) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);

        let snapshot =
            PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, Vec::new());

        assert_eq!(snapshot.latest_timestamp(), None);
    }

    #[test]
    fn the_lines_keep_the_order_the_venue_published_them_in() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        // The first side is the weakest, so a value sort would move it last.
        let series = vec![
            PredictionSeries {
                leg: legs[0].clone(),
                points: vec![PredictionPricePoint {
                    timestamp: 1_000,
                    probability: Fraction::ZERO,
                }],
            },
            PredictionSeries {
                leg: legs[1].clone(),
                points: vec![PredictionPricePoint {
                    timestamp: 1_000,
                    probability: Fraction::ONE,
                }],
            },
        ];
        let view = event_with(vec![market.clone()]);

        let snapshot = PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);

        assert_eq!(
            snapshot
                .series
                .iter()
                .map(|line| line.leg.clone())
                .collect::<Vec<_>>(),
            legs,
            "the legend and the colours read this order, so it has to be the venue's"
        );
        assert_eq!(
            snapshot
                .series
                .iter()
                .map(|line| line.color)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "a colour is the position of its line in that order"
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
        let mut snapshot =
            PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);
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
        let mut snapshot =
            PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);
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
        let snapshot = PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            view,
            markets[0].clone(),
            Vec::new(),
        );

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
        let snapshot = PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            view,
            markets[1].clone(),
            Vec::new(),
        );

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
        let snapshot = PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            view,
            markets[0].clone(),
            Vec::new(),
        );

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
        let snapshot = PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            view,
            markets[0].clone(),
            Vec::new(),
        );

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
        let snapshot = PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            view,
            markets[1].clone(),
            Vec::new(),
        );

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

    #[test]
    fn the_book_target_is_the_markets_default_leg() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);
        let series = vec![PredictionSeries {
            leg: legs[1].clone(),
            points: Vec::new(),
        }];

        let snapshot = PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);
        let target = snapshot.book_target().expect("the market has a book");

        assert_eq!(target.leg, legs[0], "the venue named the first side");
        assert_eq!(target.event, event);
        assert_eq!(
            target
                .legs
                .iter()
                .map(|leg| leg.leg.clone())
                .collect::<Vec<_>>(),
            legs,
            "the request carries every side of the market, in venue order"
        );
    }

    #[test]
    fn a_market_without_a_default_falls_back_to_the_first_charted_leg() {
        let event = event_ref();
        let (mut market, legs) = charted_market(&event, "charted");
        market.default_leg = None;
        let view = event_with(vec![market.clone()]);
        // The chart drew the second side first, so that is the side to read.
        let series = vec![
            PredictionSeries {
                leg: legs[1].clone(),
                points: Vec::new(),
            },
            PredictionSeries {
                leg: legs[0].clone(),
                points: Vec::new(),
            },
        ];

        let snapshot = PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);

        assert_eq!(
            snapshot.book_target().map(|target| target.leg),
            Some(legs[1].clone()),
            "with no default, the first line the chart drew is the book"
        );
    }

    #[test]
    fn a_default_leg_the_market_no_longer_publishes_falls_back_to_a_drawn_one() {
        let event = event_ref();
        let (mut market, legs) = charted_market(&event, "charted");
        let (other, other_legs) = charted_market(&event, "other");
        // The venue named a side of a different market, which this one has no
        // side for.
        market.default_leg = Some(other_legs[0].clone());
        let view = event_with(vec![market.clone(), other]);
        let series = vec![PredictionSeries {
            leg: legs[0].clone(),
            points: Vec::new(),
        }];

        let snapshot = PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, series);

        assert_eq!(
            snapshot.book_target().map(|target| target.leg),
            Some(legs[0].clone()),
            "a side this market does not publish cannot be read"
        );
    }

    #[test]
    fn a_market_with_no_legs_has_no_book_target() {
        let event = event_ref();
        let markets = vec![market(&event, "empty", "markets", true)];
        let view = event_with(markets.clone());

        let snapshot = PredictionMarketSnapshot::new(
            1,
            PredictionRange::All,
            view,
            markets[0].clone(),
            Vec::new(),
        );

        assert_eq!(snapshot.book_target(), None);
    }

    #[test]
    fn the_snapshot_keeps_the_markets_legs_in_venue_order() {
        let event = event_ref();
        let (market, legs) = charted_market(&event, "charted");
        let view = event_with(vec![market.clone()]);

        let snapshot =
            PredictionMarketSnapshot::new(1, PredictionRange::All, view, market, Vec::new());

        assert_eq!(
            snapshot
                .legs
                .iter()
                .map(|leg| leg.leg.clone())
                .collect::<Vec<_>>(),
            legs
        );
        assert_eq!(snapshot.default_leg, Some(legs[0].clone()));
        assert_eq!(
            snapshot
                .legs
                .iter()
                .map(|leg| leg.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Yes", "No"],
            "the labels the panel names the default side with are the venue's"
        );
    }
}
