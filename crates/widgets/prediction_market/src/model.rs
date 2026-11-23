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

pub(crate) const MAX_LINES: usize = 12;

pub(crate) const MAX_POINTS: usize = 4_096;

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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PredictionMarketSeries {
    pub leg: OutcomeLegRef,
    pub label: SmolStr,
    pub color: usize,
    pub points: Arc<[PredictionPricePoint]>,
    pub latest: Option<Fraction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredictionMarketChoice {
    pub market: PredictionMarketRef,
    pub label: SmolStr,
    pub group: PredictionMarketGroup,
}

fn push_live_point(points: &mut Vec<PredictionPricePoint>, point: PredictionPricePoint) {
    points.push(point);
    if points.len() > MAX_POINTS {
        points.drain(..points.len() - MAX_POINTS);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PredictionMarketSnapshot {
    pub revision: u64,
    pub range: PredictionRange,
    pub title: SmolStr,
    pub market: PredictionMarketRef,
    pub legs: Arc<[PredictionLeg]>,
    pub default_leg: Option<OutcomeLegRef>,
    pub markets: Arc<[PredictionMarketChoice]>,
    pub series: Arc<[PredictionMarketSeries]>,
    pub omitted: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredictionOrderBookTarget {
    pub event: PredictionEventRef,
    pub legs: Vec<PredictionLeg>,
    pub leg: OutcomeLegRef,
}

impl PredictionMarketSnapshot {
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
        let mut lines: Vec<PredictionMarketSeries> = series
            .into_iter()
            .map(|mut series| {
                let label = labels
                    .get(&series.leg)
                    .cloned()
                    .unwrap_or_else(|| SmolStr::new_static("Option"));
                let latest = series.points.last().map(|point| point.probability);
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

    pub(crate) fn latest_timestamp(&self) -> Option<UnixMillis> {
        self.series
            .iter()
            .flat_map(|line| line.points.iter())
            .map(|point| point.timestamp)
            .max()
    }

    pub(crate) fn market_groups(&self) -> Vec<&PredictionMarketGroup> {
        let mut groups: Vec<&PredictionMarketGroup> = Vec::with_capacity(8);
        for market in self.markets.iter() {
            if !groups.iter().any(|group| group.key == market.group.key) {
                groups.push(&market.group);
            }
        }
        groups
    }

    pub(crate) fn choice(&self, market: &PredictionMarketRef) -> Option<&PredictionMarketChoice> {
        self.markets.iter().find(|choice| &choice.market == market)
    }

    pub(crate) fn selected_choice(
        &self,
        selected: Option<&PredictionMarketRef>,
    ) -> Option<&PredictionMarketChoice> {
        selected
            .and_then(|market| self.choice(market))
            .or_else(|| self.choice(&self.market))
    }

    pub(crate) fn markets_in_group<'a>(
        &'a self,
        group: &'a PredictionMarketGroup,
    ) -> impl Iterator<Item = &'a PredictionMarketChoice> + 'a {
        self.markets
            .iter()
            .filter(move |market| market.group.key == group.key)
    }

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
