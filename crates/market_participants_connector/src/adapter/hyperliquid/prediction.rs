//! HIP-4 event market data.
//!
//! The venue publishes outcome metadata and a template table rather than a
//! catalog. A question is a set of named outcomes that settle together; a
//! standalone outcome is one binary or scalar question. Both are decoded from the
//! metadata the venue load committed, so an event view never reaches the venue.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

use market_participants_model::{
    decimal::{Fraction, UnixMillis},
    identifier::{IdentifierValue, VenueId, VenueScopeId},
    listing::{continuity::ListingContinuityKey, route::VenueRoute, venue::HyperliquidOutcomeId},
    prediction::{
        PredictionCategory, PredictionEventCard, PredictionEventRef, PredictionEventView,
        PredictionEventsPage, PredictionLeg, PredictionMarket, PredictionMarketGroup,
        PredictionMarketRef, PredictionNativeLeg, PredictionOptionCard, PredictionPricePoint,
        PredictionSeries,
    },
};
use smol_str::{SmolStr, format_smolstr};

use super::{
    HyperliquidCandle, HyperliquidCandleSnapshotRequest, HyperliquidInfoRequest,
    HyperliquidOutcomeQuestion, HyperliquidOutcomeSpec, HyperliquidOutcomeTemplate,
    adapter::implementation::{post_hyperliquid_info, scope_metadata},
    catalog::{outcome_asset_id, outcome_coin, outcome_continuity, outcome_encoding},
    types::HyperliquidAllMids,
};
use crate::{
    adapter::{
        ExecutionContext,
        prediction::{
            binary, build_group, exactly_one, group_legs, parse_compact_millis,
            parse_iso8601_millis, truncate_question,
        },
    },
    error::MarketParticipantError,
    market_participants::model::{
        PredictionEventQuery, PredictionEventsQuery, PredictionSeriesQuery,
    },
    venue::metadata::{OutcomeRouteMeta, ScopeMetadata},
};

/// The most events one page can carry.
const PAGE_LIMIT: usize = 500;

/// The most options one card draws.
const CARD_OPTIONS: usize = 6;

/// The most candles the venue answers one snapshot with.
///
/// The venue applies this cap itself; it is not encoded in the API document this
/// adapter reads, so the number is a documented approximation of the venue's
/// behaviour rather than a verified contract. It only bounds how far back the
/// whole-life range asks from.
const HYPERLIQUID_CANDLE_LIMIT: i64 = 5_000;

/// Reads one page of HIP-4 events from the loaded metadata.
pub(crate) async fn events(
    context: &ExecutionContext,
    query: &PredictionEventsQuery,
) -> Result<PredictionEventsPage, MarketParticipantError> {
    let meta = outcome_meta(context, &query.route)?;
    let mids = all_mids(context).await?;
    let mut cards = Vec::with_capacity(meta.questions.len() + meta.outcomes.len());
    for question in &meta.questions {
        if let Some(card) = question_card(meta, question, &mids)? {
            cards.push(card);
        }
    }
    for outcome in &meta.outcomes {
        if meta.questions.iter().any(|question| {
            question.fallback_outcome == outcome.outcome
                || question.named_outcomes.contains(&outcome.outcome)
        }) {
            continue;
        }
        if let Some(card) = outcome_card(meta, outcome, &mids)? {
            cards.push(card);
        }
    }
    cards.retain(|card| {
        query
            .category
            .as_ref()
            .is_none_or(|category| &card.category.key == category)
            && query.search.as_ref().is_none_or(|search| {
                let needle = search.trim().to_lowercase();
                needle.is_empty()
                    || card.title.to_lowercase().contains(&needle)
                    || card
                        .options
                        .iter()
                        .any(|option| option.label.to_lowercase().contains(&needle))
            })
    });
    cards.sort_by(|left, right| {
        right
            .volume
            .cmp(&left.volume)
            .then_with(|| left.title.cmp(&right.title))
    });
    let total = u32::try_from(cards.len()).unwrap_or(u32::MAX);
    let categories = categories_of(&cards);
    let offset = query
        .cursor
        .as_deref()
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(cards.len());
    let size = usize::from(query.page_size).clamp(1, PAGE_LIMIT);
    let end = offset.saturating_add(size).min(cards.len());
    Ok(PredictionEventsPage {
        events: cards[offset..end].to_vec(),
        categories,
        next_cursor: (end < cards.len()).then(|| SmolStr::new(format_smolstr!("{end}"))),
        total: Some(total),
    })
}

/// Reads one event from the loaded metadata, without a network request.
pub(crate) fn event(
    context: &ExecutionContext,
    query: &PredictionEventQuery,
) -> Result<PredictionEventView, MarketParticipantError> {
    let meta = outcome_meta(context, &query.event.route)?;
    let native = query.event.native_event.as_str();
    if let Some(id) = native.strip_prefix("q:") {
        let question = meta
            .questions
            .iter()
            .find(|question| question.question.to_string() == id)
            .ok_or_else(|| not_found(native))?;
        return question_view(meta, question).map(|view| view.0);
    }
    if let Some(id) = native.strip_prefix("o:") {
        let outcome = meta
            .outcomes
            .iter()
            .find(|outcome| outcome.outcome.to_string() == id)
            .ok_or_else(|| not_found(native))?;
        return outcome_view(meta, outcome).map(|view| view.0);
    }
    Err(not_found(native))
}

/// Reads probability series for the requested legs.
pub(crate) async fn series(
    context: &ExecutionContext,
    query: &PredictionSeriesQuery,
) -> Result<Vec<PredictionSeries>, MarketParticipantError> {
    let range = query.range;
    history(
        context,
        &query.legs,
        range.span_millis(),
        range.fidelity_minutes(),
    )
    .await
}

/// One of the venue's candle intervals: the name the venue knows it by and its
/// width.
struct CandleInterval {
    name: SmolStr,
    millis: i64,
}

/// Maps a requested bucket width to one of the venue's candle intervals.
fn candle_interval(fidelity_minutes: u16) -> CandleInterval {
    let (name, millis) = match fidelity_minutes {
        0..=1 => ("1m", 60_000),
        2..=3 => ("3m", 180_000),
        4..=5 => ("5m", 300_000),
        6..=15 => ("15m", 900_000),
        16..=30 => ("30m", 1_800_000),
        31..=60 => ("1h", 3_600_000),
        61..=120 => ("2h", 7_200_000),
        121..=240 => ("4h", 14_400_000),
        241..=480 => ("8h", 28_800_000),
        481..=720 => ("12h", 43_200_000),
        721..=1_440 => ("1d", 86_400_000),
        1_441..=4_320 => ("3d", 259_200_000),
        _ => ("1w", 604_800_000),
    };
    CandleInterval {
        name: SmolStr::new_static(name),
        millis,
    }
}

async fn history(
    context: &ExecutionContext,
    legs: &[PredictionLeg],
    span: Option<i64>,
    fidelity_minutes: u16,
) -> Result<Vec<PredictionSeries>, MarketParticipantError> {
    let end = now_millis()?;
    let interval = candle_interval(fidelity_minutes);
    // The venue caps an answer at [`HYPERLIQUID_CANDLE_LIMIT`] candles, so a
    // range with no lower bound asks for exactly that many and no further.
    let start = match span {
        Some(span) => end.saturating_sub(span.max(1)),
        None => end.saturating_sub(HYPERLIQUID_CANDLE_LIMIT.saturating_mul(interval.millis)),
    };
    let mut series = Vec::with_capacity(legs.len());
    for leg in legs {
        let Some(encoding) = encoding(leg) else {
            series.push(PredictionSeries {
                leg: leg.leg.clone(),
                points: Vec::new(),
            });
            continue;
        };
        let request = HyperliquidInfoRequest::CandleSnapshot {
            req: HyperliquidCandleSnapshotRequest {
                coin: outcome_coin(encoding),
                interval: interval.name.clone(),
                start_time: start,
                end_time: end,
            },
        };
        let rows: Vec<HyperliquidCandle> = post_hyperliquid_info(
            &context.http_client,
            context.http_origin(),
            &context.route().venue,
            &request,
        )
        .await?;
        let points = rows
            .into_iter()
            .filter_map(|row| {
                let probability = row.c.parse::<Fraction>().ok()?;
                (row.t >= 0).then_some(PredictionPricePoint {
                    timestamp: row.t,
                    probability,
                })
            })
            .collect();
        series.push(PredictionSeries {
            leg: leg.leg.clone(),
            points,
        });
    }
    Ok(series)
}

fn question_card(
    meta: &OutcomeRouteMeta,
    question: &HyperliquidOutcomeQuestion,
    mids: &HyperliquidAllMids,
) -> Result<Option<PredictionEventCard>, MarketParticipantError> {
    let (view, options) = question_view(meta, question)?;
    Ok(Some(card_from(view, options, mids)))
}

fn outcome_card(
    meta: &OutcomeRouteMeta,
    outcome: &HyperliquidOutcomeSpec,
    mids: &HyperliquidAllMids,
) -> Result<Option<PredictionEventCard>, MarketParticipantError> {
    let (view, options) = outcome_view(meta, outcome)?;
    Ok(Some(card_from(view, options, mids)))
}

fn card_from(
    view: PredictionEventView,
    options: Vec<PredictionOptionCard>,
    mids: &HyperliquidAllMids,
) -> PredictionEventCard {
    let encodings: BTreeMap<_, _> = view
        .markets
        .iter()
        .flat_map(|market| market.legs.iter())
        .filter_map(|leg| match &leg.native {
            PredictionNativeLeg::HyperliquidOutcome { encoding, .. } => {
                Some((leg.leg.clone(), *encoding))
            }
            PredictionNativeLeg::Polymarket { .. } => None,
        })
        .collect();
    let mut options: Vec<PredictionOptionCard> = options
        .into_iter()
        .map(|mut option| {
            if option.probability.is_none() {
                option.probability = encodings
                    .get(&option.leg)
                    .and_then(|encoding| probability_of(mids, *encoding));
            }
            option
        })
        .collect();
    options.sort_by_key(|option| std::cmp::Reverse(option.probability));
    let remaining = options.len().saturating_sub(CARD_OPTIONS);
    options.truncate(CARD_OPTIONS);
    PredictionEventCard {
        event: view.event,
        title: view.title,
        category: view.category,
        ends_at: view.ends_at,
        volume: None,
        liquidity: None,
        options,
        remaining_options: u16::try_from(remaining).unwrap_or(u16::MAX),
        market_count: 1,
        remaining_markets: 0,
    }
}

fn question_view(
    meta: &OutcomeRouteMeta,
    question: &HyperliquidOutcomeQuestion,
) -> Result<(PredictionEventView, Vec<PredictionOptionCard>), MarketParticipantError> {
    let pairs = description_pairs(&question.description);
    let (title, category) = render_question(meta, question, &pairs);
    let ends_at = deadline_of(&pairs);
    let settled: BTreeSet<u64> = question.settled_named_outcomes.iter().copied().collect();
    let mut outcome_ids: Vec<u64> = question
        .named_outcomes
        .iter()
        .copied()
        .filter(|outcome| !settled.contains(outcome))
        .collect();
    if !settled.contains(&question.fallback_outcome)
        && !outcome_ids.contains(&question.fallback_outcome)
    {
        outcome_ids.push(question.fallback_outcome);
    }
    let quote_token = outcome_ids
        .iter()
        .find_map(|id| meta.outcomes.iter().find(|outcome| outcome.outcome == *id))
        .and_then(|outcome| outcome.quote_token.as_deref());
    let settlement = settlement_asset(quote_token)?;
    let thresholds: Vec<SmolStr> = pairs
        .get("priceThresholds")
        .map(|thresholds| thresholds.split(',').map(SmolStr::new).collect())
        .unwrap_or_default();
    let bucket_asset = pairs
        .get("underlying")
        .or_else(|| pairs.get("perp"))
        .map(|asset| asset.as_str());
    let mut members = Vec::with_capacity(outcome_ids.len());
    let mut native_sides = BTreeMap::new();
    let mut used_labels: BTreeMap<SmolStr, usize> = BTreeMap::new();
    for id in &outcome_ids {
        let Some(outcome) = meta.outcomes.iter().find(|outcome| outcome.outcome == *id) else {
            continue;
        };
        let mut label = if *id == question.fallback_outcome {
            SmolStr::new_static("Other")
        } else if let Some(index) = question
            .named_outcomes
            .iter()
            .position(|candidate| candidate == id)
        {
            bucket_asset
                .and_then(|asset| bucket_label(asset, &thresholds, index))
                .unwrap_or_else(|| render_outcome_name(meta, outcome, &pairs))
        } else {
            render_outcome_name(meta, outcome, &pairs)
        };
        let occurrence = used_labels.entry(label.clone()).or_default();
        *occurrence += 1;
        if *occurrence > 1 {
            // A member identity is derived from its label, so two outcomes with
            // the same rendered name would make the group invalid. Keep both
            // options and make the repeated name readable and stable.
            label = SmolStr::new(format_smolstr!("{label} ({occurrence})"));
        }
        native_sides.insert(
            label.clone(),
            (HyperliquidOutcomeId(outcome.outcome), 0_u64),
        );
        members.push((label.clone(), vec![label]));
    }
    if members.is_empty() {
        return Err(not_found(&question.question.to_string()));
    }
    let group = build_group(title.clone(), ends_at, settlement, exactly_one(), members)?;
    let event = PredictionEventRef {
        route: route_of(meta)?,
        native_event: IdentifierValue::new(format_smolstr!("q:{}", question.question))
            .map_err(|error| protocol("decode a HIP-4 question id", error))?,
    };
    let market = PredictionMarketRef {
        event: event.clone(),
        native_market: event.native_event.clone(),
    };
    let (legs, options) = legs_of(meta, &group, &event, &market, &native_sides)?;
    let default_leg = legs.first().map(|leg| leg.leg.clone());
    let market = PredictionMarket {
        market,
        question: title.clone(),
        label: title.clone(),
        group: PredictionMarketGroup::new("outcomes", "Outcomes"),
        headline: true,
        open: true,
        default_leg,
        legs,
    };
    Ok((
        PredictionEventView {
            event,
            title,
            category,
            ends_at,
            markets: vec![market],
        },
        options,
    ))
}

fn outcome_view(
    meta: &OutcomeRouteMeta,
    outcome: &HyperliquidOutcomeSpec,
) -> Result<(PredictionEventView, Vec<PredictionOptionCard>), MarketParticipantError> {
    let pairs = description_pairs(&outcome.description);
    let title = render_outcome_name(meta, outcome, &pairs);
    let category =
        category_from_description(&pairs).unwrap_or_else(|| category_of(meta, &outcome.name));
    let ends_at = deadline_of(&pairs);
    let sides: Vec<SmolStr> = if outcome.side_specs.len() == 2 {
        outcome
            .side_specs
            .iter()
            .map(|side| render_side_name(meta, &side.name, &pairs))
            .collect()
    } else {
        vec![SmolStr::new_static("Yes"), SmolStr::new_static("No")]
    };
    let native_sides: BTreeMap<_, _> = sides
        .iter()
        .enumerate()
        .map(|(index, label)| {
            (
                label.clone(),
                (HyperliquidOutcomeId(outcome.outcome), index as u64),
            )
        })
        .collect();
    let default_label = sides.first().cloned();
    let group = build_group(
        title.clone(),
        ends_at,
        settlement_asset(outcome.quote_token.as_deref())?,
        binary(),
        vec![(SmolStr::new_static("Yes"), sides)],
    )?;
    let event = PredictionEventRef {
        route: route_of(meta)?,
        native_event: IdentifierValue::new(format_smolstr!("o:{}", outcome.outcome))
            .map_err(|error| protocol("decode a HIP-4 outcome id", error))?,
    };
    let market = PredictionMarketRef {
        event: event.clone(),
        native_market: event.native_event.clone(),
    };
    let (legs, options) = legs_of(meta, &group, &event, &market, &native_sides)?;
    let default_leg = default_label.and_then(|label| {
        legs.iter()
            .find(|leg| leg.label == label)
            .map(|leg| leg.leg.clone())
    });
    let market = PredictionMarket {
        market,
        question: title.clone(),
        label: title.clone(),
        group: PredictionMarketGroup::new("outcomes", "Outcomes"),
        headline: true,
        open: true,
        default_leg,
        legs,
    };
    Ok((
        PredictionEventView {
            event,
            title,
            category,
            ends_at,
            markets: vec![market],
        },
        options,
    ))
}

fn legs_of(
    meta: &OutcomeRouteMeta,
    group: &market_participants_model::outcome::OutcomeGroup,
    event: &PredictionEventRef,
    market: &PredictionMarketRef,
    native_sides: &BTreeMap<SmolStr, (HyperliquidOutcomeId, u64)>,
) -> Result<(Vec<PredictionLeg>, Vec<PredictionOptionCard>), MarketParticipantError> {
    let scope = scope_of(meta)?;
    let mut legs = Vec::new();
    let mut options = Vec::new();
    for (leg, label) in group_legs(group) {
        let Some((outcome_id, side)) = native_sides.get(&label) else {
            continue;
        };
        let encoding = outcome_encoding(outcome_id.0, *side)?;
        let asset_id = outcome_asset_id(encoding)?;
        let listing = ListingContinuityKey {
            route: event.route.clone(),
            terms: outcome_continuity(&scope, *outcome_id, asset_id),
        }
        .listing_key()
        .map_err(|error| MarketParticipantError::MarketModel(Box::new(error)))?;
        legs.push(PredictionLeg {
            leg: leg.clone(),
            label: label.clone(),
            probability: None,
            listing,
            native: PredictionNativeLeg::HyperliquidOutcome {
                outcome: *outcome_id,
                side: leg.side,
                encoding,
                coin: outcome_coin(encoding),
                asset_id,
            },
        });
        options.push(PredictionOptionCard {
            market: market.clone(),
            leg,
            label,
            probability: None,
        });
    }
    Ok((legs, options))
}

fn scope_of(meta: &OutcomeRouteMeta) -> Result<VenueScopeId, MarketParticipantError> {
    match meta.deployer.as_deref() {
        Some(name) => super::catalog::outcome_venue_scope(name),
        None => VenueScopeId::new("outcome-core")
            .map_err(|error| protocol("build the outcome-core scope", error)),
    }
}

fn route_of(meta: &OutcomeRouteMeta) -> Result<VenueRoute, MarketParticipantError> {
    Ok(VenueRoute::new(VenueId::Hyperliquid, scope_of(meta)?))
}

fn outcome_meta<'a>(
    context: &'a ExecutionContext,
    route: &VenueRoute,
) -> Result<&'a OutcomeRouteMeta, MarketParticipantError> {
    let metadata: &ScopeMetadata = scope_metadata(context, route)?;
    if let ScopeMetadata::Outcome(meta) = metadata {
        return Ok(meta);
    }
    Err(MarketParticipantError::UnsupportedCapability {
        route: route.clone(),
        capability: market_participants_model::routing::CapabilityId::PredictionEvents,
    })
}

async fn all_mids(
    context: &ExecutionContext,
) -> Result<HyperliquidAllMids, MarketParticipantError> {
    post_hyperliquid_info(
        &context.http_client,
        context.http_origin(),
        &context.route().venue,
        &HyperliquidInfoRequest::AllMids,
    )
    .await
}

fn encoding(leg: &PredictionLeg) -> Option<u64> {
    match &leg.native {
        PredictionNativeLeg::HyperliquidOutcome { encoding, .. } => Some(*encoding),
        PredictionNativeLeg::Polymarket { .. } => None,
    }
}

fn probability_of(mids: &HyperliquidAllMids, encoding: u64) -> Option<Fraction> {
    let value = mids.get(&outcome_coin(encoding))?;
    value.parse().ok()
}

fn categories_of(cards: &[PredictionEventCard]) -> Vec<PredictionCategory> {
    let mut categories = BTreeMap::new();
    for card in cards {
        categories
            .entry(card.category.key.clone())
            .or_insert_with(|| card.category.clone());
    }
    categories.into_values().collect()
}

fn settlement_asset(
    quote_token: Option<&str>,
) -> Result<market_participants_model::asset::AssetId, MarketParticipantError> {
    // HIP-4 settles in the outcome's quote token. The live core markets all use
    // USDC, which is also the fallback when the venue omits the field.
    super::catalog::token_asset(quote_token.unwrap_or("USDC")).map(|terms| terms.asset_id())
}

fn description_pairs(description: &str) -> BTreeMap<SmolStr, SmolStr> {
    description
        .split('|')
        .filter_map(|segment| segment.split_once(':'))
        .map(|(key, value)| (SmolStr::new(key), SmolStr::new(value)))
        .collect()
}

fn render_question(
    meta: &OutcomeRouteMeta,
    question: &HyperliquidOutcomeQuestion,
    pairs: &BTreeMap<SmolStr, SmolStr>,
) -> (SmolStr, PredictionCategory) {
    let template = template_of(meta, &question.name);
    let title = description_title(pairs)
        .or_else(|| {
            template
                .map(|template| render_template(&template.name, pairs))
                .filter(|title| !title.is_empty())
        })
        .unwrap_or_else(|| question.name.clone());
    (
        truncate_question(title),
        category_from_description(pairs).unwrap_or_else(|| category_of(meta, &question.name)),
    )
}

fn render_outcome_name(
    meta: &OutcomeRouteMeta,
    outcome: &HyperliquidOutcomeSpec,
    pairs: &BTreeMap<SmolStr, SmolStr>,
) -> SmolStr {
    if is_fallback_outcome(outcome) {
        return SmolStr::new_static("Other");
    }
    // An outcome's own description carries placeholders the question does not,
    // such as `participant` for a team in a contest. The question's pairs remain
    // the base so a template can still read a shared field from it.
    let mut values = pairs.clone();
    let own = description_pairs(&outcome.description);
    values.extend(own.clone());
    let template = template_of(meta, &outcome.name);
    // Only the outcome's own description decides a machine-readable title; the
    // question's `class` describes the question, not each of its members.
    let name = description_title(&own)
        .or_else(|| {
            template
                .map(|template| render_template(&template.name, &values))
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| {
            SmolStr::new(
                outcome
                    .name
                    .strip_prefix("template:")
                    .unwrap_or(outcome.name.as_str()),
            )
        });
    truncate_question(name)
}

/// Whether an outcome is the fallback member of a multi-outcome question.
fn is_fallback_outcome(outcome: &HyperliquidOutcomeSpec) -> bool {
    outcome.name == "template fallback" || outcome.description == "other"
}

/// Renders the machine-readable description the core recurring markets use.
///
/// The core `outcomeMeta` entries have no template name: a price-binary outcome
/// is called `Recurring` and its description carries the asset and target. Using
/// only the name would collapse every core market into one title and, because a
/// group identity is derived from that title, into one group identity.
fn description_title(pairs: &BTreeMap<SmolStr, SmolStr>) -> Option<SmolStr> {
    let class = pairs.get("class")?.as_str();
    let asset = pairs
        .get("underlying")
        .or_else(|| pairs.get("perp"))
        .filter(|asset| !asset.is_empty());
    match class {
        "priceBinary" => {
            let asset = asset?;
            let target = pairs
                .get("targetPrice")
                .or_else(|| pairs.get("target"))
                .filter(|target| !target.is_empty());
            Some(match target {
                Some(target) => format_smolstr!("{asset} above {target}"),
                None => format_smolstr!("{asset} price"),
            })
        }
        "priceBucket" => {
            let asset = asset?;
            let thresholds = pairs
                .get("priceThresholds")
                .filter(|thresholds| !thresholds.is_empty());
            Some(match thresholds {
                Some(thresholds) => format_smolstr!("{asset} buckets {thresholds}"),
                None => format_smolstr!("{asset} price range"),
            })
        }
        _ => None,
    }
}

/// Names one price-bucket member from the question's ordered thresholds.
fn bucket_label(asset: &str, thresholds: &[SmolStr], index: usize) -> Option<SmolStr> {
    let upper = thresholds.get(index);
    let lower = index.checked_sub(1).and_then(|index| thresholds.get(index));
    match (lower, upper) {
        (None, Some(upper)) => Some(format_smolstr!("{asset} < {upper}")),
        (Some(lower), Some(upper)) => Some(format_smolstr!("{asset} {lower}-{upper}")),
        (Some(lower), None) => Some(format_smolstr!("{asset} > {lower}")),
        (None, None) => None,
    }
}

fn category_from_description(pairs: &BTreeMap<SmolStr, SmolStr>) -> Option<PredictionCategory> {
    match pairs.get("class")?.as_str() {
        "priceBinary" | "priceBucket" => Some(PredictionCategory::new("price", "Price")),
        _ => None,
    }
}

fn render_side_name(
    meta: &OutcomeRouteMeta,
    name: &SmolStr,
    pairs: &BTreeMap<SmolStr, SmolStr>,
) -> SmolStr {
    let name = name.strip_prefix("template:").unwrap_or(name);
    let template = template_of(meta, name);
    template
        .map(|template| render_template(&template.name, pairs))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| render_template(name, pairs))
}

fn template_of<'a>(
    meta: &'a OutcomeRouteMeta,
    name: &str,
) -> Option<&'a HyperliquidOutcomeTemplate> {
    let id = name.strip_prefix("template:").unwrap_or(name);
    meta.templates.iter().find(|template| template.id == id)
}

fn render_template(template: &str, pairs: &BTreeMap<SmolStr, SmolStr>) -> SmolStr {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            rendered.push_str(&rest[open..]);
            return SmolStr::new(rendered);
        };
        let key = &rest[open + 1..open + close];
        if let Some(value) = pairs.get(key) {
            rendered.push_str(value);
        } else {
            rendered.push_str(&rest[open..=open + close]);
        }
        rest = &rest[open + close + 1..];
    }
    rendered.push_str(rest);
    SmolStr::new(rendered)
}

fn category_of(meta: &OutcomeRouteMeta, template_name: &str) -> PredictionCategory {
    let category = template_of(meta, template_name)
        .and_then(|template| {
            let metadata = template.description.rsplit("metadata=").next()?;
            metadata
                .split('|')
                .find_map(|segment| segment.strip_prefix("category:"))
        })
        .unwrap_or("other");
    match category {
        "price" => PredictionCategory::new("price", "Price"),
        "sports" => PredictionCategory::new("sports", "Sports"),
        "economics" => PredictionCategory::new("economics", "Economics"),
        "culture" => PredictionCategory::new("culture", "Culture"),
        "technology" => PredictionCategory::new("technology", "Technology"),
        other => PredictionCategory::new("other", SmolStr::new(other)),
    }
}

fn deadline_of(pairs: &BTreeMap<SmolStr, SmolStr>) -> Option<UnixMillis> {
    const KEYS: [&str; 8] = [
        "resolutionDeadline",
        "listingDeadline",
        "scheduledStart",
        "expiry",
        "time",
        "dateTime",
        "decisionDeadline",
        "scheduledDecision",
    ];
    for key in KEYS {
        let Some(value) = pairs.get(key) else {
            continue;
        };
        if let Some(instant) = parse_compact_millis(value) {
            return Some(instant);
        }
        if let Some(instant) = parse_iso8601_millis(value) {
            return Some(instant);
        }
    }
    None
}

fn now_millis() -> Result<UnixMillis, MarketParticipantError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or_else(|| protocol("read the clock", "the clock is before the epoch"))
}

fn not_found(native: &str) -> MarketParticipantError {
    protocol(
        "find a HIP-4 event",
        format_smolstr!("{native} is not published"),
    )
}

fn protocol(operation: &'static str, reason: impl std::fmt::Display) -> MarketParticipantError {
    MarketParticipantError::ProtocolError {
        operation: SmolStr::new_static(operation),
        reason: format_smolstr!("{reason}"),
        raw_data: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use market_participants_model::listing::venue::HyperliquidOutcomeId;
    use smol_str::SmolStr;

    use super::{
        HyperliquidOutcomeQuestion, HyperliquidOutcomeSpec, PredictionNativeLeg, deadline_of,
        description_pairs, outcome_view, question_view, render_template,
    };
    use crate::{
        adapter::hyperliquid::{
            HyperliquidOutcomeSideSpec, HyperliquidOutcomeTemplate, HyperliquidOutcomeTemplateRole,
        },
        venue::metadata::OutcomeRouteMeta,
    };

    fn outcome(id: u64, name: &'static str) -> HyperliquidOutcomeSpec {
        HyperliquidOutcomeSpec {
            outcome: id,
            name: SmolStr::new_static(name),
            description: SmolStr::new_static(""),
            side_specs: vec![
                HyperliquidOutcomeSideSpec {
                    name: SmolStr::new_static("Yes"),
                },
                HyperliquidOutcomeSideSpec {
                    name: SmolStr::new_static("No"),
                },
            ],
            quote_token: Some(SmolStr::new_static("USDC")),
            venue: Some(SmolStr::new_static("test")),
        }
    }

    #[test]
    fn a_question_leg_keeps_the_outcome_it_was_built_from() {
        let meta = OutcomeRouteMeta {
            deployer: Some(SmolStr::new_static("test")),
            outcomes: vec![
                outcome(11, "Alpha"),
                outcome(22, "Beta"),
                outcome(33, "Other"),
            ],
            questions: Vec::new(),
            templates: Vec::new(),
        };
        let question = HyperliquidOutcomeQuestion {
            question: 7,
            name: SmolStr::new_static("Question"),
            description: SmolStr::new_static(""),
            fallback_outcome: 33,
            named_outcomes: vec![22, 11],
            settled_named_outcomes: Vec::new(),
        };
        let (view, _) = question_view(&meta, &question).expect("the question decodes");

        assert_eq!(view.markets[0].legs.len(), 3);
        for leg in &view.markets[0].legs {
            let expected = match leg.label.as_str() {
                "Alpha" => 11,
                "Beta" => 22,
                "Other" => 33,
                other => panic!("unexpected option {other}"),
            };
            match &leg.native {
                PredictionNativeLeg::HyperliquidOutcome {
                    outcome, encoding, ..
                } => {
                    assert_eq!(*outcome, HyperliquidOutcomeId(expected));
                    assert_eq!(*encoding, expected * 10);
                }
                PredictionNativeLeg::Polymarket { .. } => {
                    panic!("a HIP-4 leg must stay a Hyperliquid outcome")
                }
            }
        }
    }

    #[test]
    fn a_binary_outcome_maps_yes_and_no_to_their_own_encodings() {
        let meta = OutcomeRouteMeta {
            deployer: Some(SmolStr::new_static("test")),
            outcomes: vec![outcome(11, "Binary")],
            questions: Vec::new(),
            templates: Vec::new(),
        };
        let binary = outcome(11, "Binary");
        let (view, options) = outcome_view(&meta, &binary).expect("the outcome decodes");

        assert_eq!(view.markets[0].legs.len(), 2);
        let yes = view.markets[0]
            .legs
            .iter()
            .find(|leg| leg.label == "Yes")
            .expect("the Yes leg exists");
        let no = view.markets[0]
            .legs
            .iter()
            .find(|leg| leg.label == "No")
            .expect("the No leg exists");
        assert!(matches!(
            &yes.native,
            PredictionNativeLeg::HyperliquidOutcome { encoding: 110, .. }
        ));
        assert!(matches!(
            &no.native,
            PredictionNativeLeg::HyperliquidOutcome { encoding: 111, .. }
        ));
        assert_eq!(options.len(), 2);
    }

    /// A participant template reads the outcome's own description, not only the
    /// question's; otherwise every team would render as the literal `{participant}`.
    #[test]
    fn a_question_outcome_reads_its_own_description_for_template_placeholders() {
        let mut alpha = outcome(11, "template:sportsContestParticipant2");
        alpha.description = SmolStr::new_static("participant:Alpha");
        let mut beta = outcome(22, "template:sportsContestParticipant2");
        beta.description = SmolStr::new_static("participant:Beta");
        let meta = OutcomeRouteMeta {
            deployer: Some(SmolStr::new_static("test")),
            outcomes: vec![alpha, beta, outcome(33, "Other")],
            questions: Vec::new(),
            templates: vec![
                HyperliquidOutcomeTemplate {
                    id: SmolStr::new_static("sportsContestResult"),
                    role: HyperliquidOutcomeTemplateRole::Kind(SmolStr::new_static(
                        "questionOutcome",
                    )),
                    name: SmolStr::new_static("{participantA} vs {participantB}"),
                    description: SmolStr::new_static(""),
                },
                HyperliquidOutcomeTemplate {
                    id: SmolStr::new_static("sportsContestParticipant2"),
                    role: HyperliquidOutcomeTemplateRole::Kind(SmolStr::new_static(
                        "questionOutcome",
                    )),
                    name: SmolStr::new_static("{participant}"),
                    description: SmolStr::new_static(""),
                },
            ],
        };
        let question = HyperliquidOutcomeQuestion {
            question: 7,
            name: SmolStr::new_static("template:sportsContestResult"),
            description: SmolStr::new_static(
                "participantA:Alpha|participantB:Beta|resolutionDeadline:20261110-2000",
            ),
            fallback_outcome: 33,
            named_outcomes: vec![11, 22],
            settled_named_outcomes: Vec::new(),
        };

        let (view, _) = question_view(&meta, &question).expect("the question decodes");
        let labels: Vec<&str> = view.markets[0]
            .legs
            .iter()
            .map(|leg| leg.label.as_str())
            .collect();

        assert!(labels.contains(&"Alpha"));
        assert!(labels.contains(&"Beta"));
    }

    /// A venue can publish two outcomes whose rendered names collide. Their member
    /// identities are label-derived, so the group must disambiguate them rather
    /// than reject the whole event.
    #[test]
    fn repeated_outcome_names_stay_distinct_and_valid() {
        let meta = OutcomeRouteMeta {
            deployer: Some(SmolStr::new_static("test")),
            outcomes: vec![
                outcome(11, "Recurring Named Outcome"),
                outcome(22, "Recurring Named Outcome"),
                outcome(33, "Recurring Named Outcome"),
            ],
            questions: Vec::new(),
            templates: Vec::new(),
        };
        let question = HyperliquidOutcomeQuestion {
            question: 7,
            name: SmolStr::new_static("Recurring"),
            description: SmolStr::new_static(""),
            fallback_outcome: 33,
            named_outcomes: vec![11, 22],
            settled_named_outcomes: Vec::new(),
        };

        let (view, _) = question_view(&meta, &question).expect("the question decodes");
        let labels: std::collections::BTreeSet<&str> = view.markets[0]
            .legs
            .iter()
            .map(|leg| leg.label.as_str())
            .collect();

        assert_eq!(view.markets[0].legs.len(), 3);
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn a_description_is_split_into_key_value_pairs() {
        let pairs = description_pairs("perp:ETH|threshold:2114|time:20261106-0800");
        assert_eq!(pairs.get("perp").map(SmolStr::as_str), Some("ETH"));
        assert_eq!(pairs.get("threshold").map(SmolStr::as_str), Some("2114"));
    }

    #[test]
    fn a_core_price_binary_description_names_the_market() {
        let mut market = outcome(11, "Recurring");
        market.description = SmolStr::new_static(
            "class:priceBinary|underlying:BTC|expiry:20261106-0600|targetPrice:80371|period:1d",
        );
        let meta = OutcomeRouteMeta {
            deployer: None,
            outcomes: vec![market.clone()],
            questions: Vec::new(),
            templates: Vec::new(),
        };

        let (view, _) = outcome_view(&meta, &market).expect("the core outcome decodes");

        assert_eq!(view.title, "BTC above 80371");
        assert_eq!(view.category.key, "price");
        assert_eq!(
            view.ends_at,
            deadline_of(&description_pairs(&market.description))
        );
    }

    #[test]
    fn a_price_bucket_question_description_names_the_question() {
        let meta = OutcomeRouteMeta {
            deployer: None,
            outcomes: vec![
                outcome(11, "Recurring Named Outcome"),
                outcome(22, "Recurring Named Outcome 2"),
                outcome(33, "Other"),
            ],
            questions: Vec::new(),
            templates: Vec::new(),
        };
        let question = HyperliquidOutcomeQuestion {
            question: 7,
            name: SmolStr::new_static("Recurring"),
            description: SmolStr::new_static(
                "class:priceBucket|underlying:BTC|priceThresholds:80000,90000|expiry:20261106-0600",
            ),
            fallback_outcome: 33,
            named_outcomes: vec![11, 22],
            settled_named_outcomes: Vec::new(),
        };

        let (view, _) = question_view(&meta, &question).expect("the price bucket decodes");

        assert_eq!(view.title, "BTC buckets 80000,90000");
        assert_eq!(view.category.key, "price");
        let labels: Vec<&str> = view.markets[0]
            .legs
            .iter()
            .map(|leg| leg.label.as_str())
            .collect();
        assert!(labels.contains(&"BTC < 80000"));
        assert!(labels.contains(&"BTC 80000-90000"));
        assert!(labels.contains(&"Other"));
    }

    #[test]
    fn a_template_fallback_is_named_other() {
        let mut fallback = outcome(33, "template fallback");
        fallback.description = SmolStr::new_static("other");
        let meta = OutcomeRouteMeta {
            deployer: None,
            outcomes: vec![outcome(11, "Alpha"), fallback],
            questions: Vec::new(),
            templates: Vec::new(),
        };
        let question = HyperliquidOutcomeQuestion {
            question: 7,
            name: SmolStr::new_static("Question"),
            description: SmolStr::new_static(""),
            fallback_outcome: 33,
            named_outcomes: vec![11],
            settled_named_outcomes: Vec::new(),
        };

        let (view, _) = question_view(&meta, &question).expect("the fallback decodes");

        assert!(view.markets[0].legs.iter().any(|leg| leg.label == "Other"));
        assert!(
            !view.markets[0]
                .legs
                .iter()
                .any(|leg| leg.label == "template fallback")
        );
    }

    #[test]
    fn a_settled_named_outcome_is_not_offered() {
        let meta = OutcomeRouteMeta {
            deployer: None,
            outcomes: vec![
                outcome(11, "Alpha"),
                outcome(22, "Beta"),
                outcome(33, "Other"),
            ],
            questions: Vec::new(),
            templates: Vec::new(),
        };
        let question = HyperliquidOutcomeQuestion {
            question: 7,
            name: SmolStr::new_static("Question"),
            description: SmolStr::new_static(""),
            fallback_outcome: 33,
            named_outcomes: vec![11, 22],
            settled_named_outcomes: vec![11],
        };

        let (view, _) = question_view(&meta, &question).expect("the settled question decodes");

        assert!(!view.markets[0].legs.iter().any(|leg| leg.label == "Alpha"));
        assert!(view.markets[0].legs.iter().any(|leg| leg.label == "Beta"));
    }

    #[test]
    fn a_listing_deadline_is_read_from_the_description() {
        let pairs = description_pairs("listingDeadline:20270501-0359");
        assert!(deadline_of(&pairs).is_some());
    }

    #[test]
    fn a_history_fidelity_selects_a_matching_candle_interval() {
        let hour = super::candle_interval(60);
        assert_eq!(hour.name, "1h");
        assert_eq!(hour.millis, 3_600_000);
        assert_eq!(super::candle_interval(5).name, "5m");
        assert_eq!(super::candle_interval(1_440).name, "1d");
        assert_eq!(super::candle_interval(720).name, "12h");
    }

    #[test]
    fn the_whole_life_range_reaches_back_one_venue_candle_cap() {
        // The whole-life range has no lower bound of its own, so its reach is
        // the venue's candle cap at the interval the fidelity selects.
        assert_eq!(
            super::HYPERLIQUID_CANDLE_LIMIT * super::candle_interval(720).millis,
            5_000 * 12 * 3_600_000
        );
    }

    #[test]
    fn a_template_placeholder_is_replaced() {
        let mut pairs = BTreeMap::new();
        pairs.insert(SmolStr::new_static("perp"), SmolStr::new_static("ETH"));
        pairs.insert(
            SmolStr::new_static("threshold"),
            SmolStr::new_static("2114"),
        );
        assert_eq!(
            render_template("{perp} above {threshold}?", &pairs),
            "ETH above 2114?"
        );
    }

    #[test]
    fn a_missing_placeholder_is_left_visible_rather_than_emptied() {
        let pairs = BTreeMap::new();
        assert_eq!(render_template("{perp} above", &pairs), "{perp} above");
    }

    #[test]
    fn a_compact_deadline_is_read_from_the_description() {
        let pairs = description_pairs("resolutionDeadline:20261110-2000");
        assert_eq!(deadline_of(&pairs), Some(1_794_340_800_000));
    }
}
