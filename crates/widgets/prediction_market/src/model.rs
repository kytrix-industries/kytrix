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
mod model_tests;
