//! Shared construction of prediction-event outcome groups.
//!
//! Both prediction adapters publish the same canonical shape: one outcome group
//! per question, one member per selectable option, and one representative side per
//! option. The venue-specific part is the wire decode; the canonical identity and
//! the date parsing live here so the two adapters cannot derive different groups
//! from the same event.

use market_participants_model::{
    asset::AssetId,
    decimal::{Fraction, UnixMillis},
    error::ModelError,
    outcome::{
        OutcomeGroup, OutcomeGroupSeed, OutcomeLegRef, OutcomeMember, OutcomeSide, OutcomeTopology,
        ResolutionRule, SelectionCardinality, SettlementFractionRule,
    },
    settlement::Expiry,
};
use smol_str::SmolStr;

use crate::error::MarketParticipantError;

/// The expiry of an event that publishes no end instant.
///
/// A prediction event always has a resolution deadline in its own rules, but a
/// venue may omit the field from a list answer. The fallback is the end of the
/// century rather than "now": an event without a deadline is not an event that
/// expired, and a group identity derived from the current instant would move every
/// time it was read.
pub(crate) const FALLBACK_EXPIRY_MS: UnixMillis = 4_102_444_800_000;

/// Builds one canonical outcome group from a question and its options.
///
/// `members` carries each option's label and the labels of its sides. A binary
/// group has exactly one member with two sides; a mutually exclusive group has one
/// member per option, each with its own two sides.
pub(crate) fn build_group(
    question: SmolStr,
    expiry_at: Option<UnixMillis>,
    settlement_asset: AssetId,
    topology: OutcomeTopology,
    members: Vec<(SmolStr, Vec<SmolStr>)>,
) -> Result<OutcomeGroup, MarketParticipantError> {
    let question = truncate_question(question);
    let expiry_at = expiry_at.unwrap_or(FALLBACK_EXPIRY_MS).max(1);
    let expiry = Expiry::new(expiry_at, None, None).map_err(model_error)?;
    let seed = OutcomeGroupSeed {
        question,
        expiry,
        topology,
        settlement_asset,
        resolution: ResolutionRule::FixedAt { at: expiry_at },
        fraction_rule: SettlementFractionRule::AnyFraction,
    };
    let seed_id = seed.seed_id();
    let mut sides = Vec::new();
    let mut group_members = Vec::with_capacity(members.len());
    for (label, side_labels) in members {
        let mut member_sides = Vec::with_capacity(side_labels.len());
        for side_label in side_labels {
            let side = OutcomeSide::new(&seed_id, side_label).map_err(model_error)?;
            member_sides.push(side.id);
            sides.push(side);
        }
        group_members.push(OutcomeMember::new(&seed_id, label, member_sides).map_err(model_error)?);
    }
    OutcomeGroup::new(seed, sides, group_members).map_err(model_error)
}

/// Returns every tradable side of every member of one group.
///
/// A binary group has one member with two sides (`Yes`/`No`); a mutually
/// exclusive group has one side per member. The side label is the label a chart
/// draws, and the returned leg names exactly the side whose native asset the
/// adapter maps to it.
pub(crate) fn group_legs(group: &OutcomeGroup) -> Vec<(OutcomeLegRef, SmolStr)> {
    let mut legs = Vec::with_capacity(group.sides.len());
    for member in &group.members {
        for side_id in &member.sides {
            let Some(side) = group.sides.iter().find(|side| side.id == *side_id) else {
                continue;
            };
            legs.push((
                OutcomeLegRef {
                    group: group.id,
                    member: member.id,
                    side: *side_id,
                },
                side.label.clone(),
            ));
        }
    }
    legs
}

/// Builds a binary topology.
pub(crate) const fn binary() -> OutcomeTopology {
    OutcomeTopology::Binary
}

/// Builds a mutually exclusive topology with exactly one selected member.
pub(crate) const fn exactly_one() -> OutcomeTopology {
    OutcomeTopology::MutuallyExclusive(SelectionCardinality::ExactlyOne)
}

/// Truncates a question to the model's scalar-value limit.
///
/// The model refuses a longer question rather than truncating it, so a venue whose
/// title is longer than the limit would lose the whole event. A prediction event's
/// title is display text rather than identity, so the adapter states the bounded
/// form once, at the boundary where the venue's title is still authoritative.
pub(crate) fn truncate_question(question: SmolStr) -> SmolStr {
    let limit = market_participants_model::outcome::MAX_OUTCOME_QUESTION_SCALARS;
    if question.chars().count() <= limit {
        return question;
    }
    SmolStr::new(question.chars().take(limit).collect::<String>())
}

/// Parses a probability the venue published as a decimal string.
pub(crate) fn probability(value: &str) -> Option<Fraction> {
    value.parse().ok()
}

/// Parses a `YYYY-MM-DDTHH:MM:SSZ` instant.
pub(crate) fn parse_iso8601_millis(value: &str) -> Option<UnixMillis> {
    let bytes = value.as_bytes();
    if bytes.len() < 19
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = value.get(0..4)?.parse::<i32>().ok()?;
    let month = value.get(5..7)?.parse::<u8>().ok()?;
    let day = value.get(8..10)?.parse::<u8>().ok()?;
    let hour = value.get(11..13)?.parse::<u8>().ok()?;
    let minute = value.get(14..16)?.parse::<u8>().ok()?;
    let second = value.get(17..19)?.parse::<u8>().ok()?;
    civil_millis(year, month, day, hour, minute, second)
}

/// Parses a `YYYYMMDD-HHMM` instant, the compact form HIP-4 descriptions use.
pub(crate) fn parse_compact_millis(value: &str) -> Option<UnixMillis> {
    let bytes = value.as_bytes();
    if bytes.len() != 13 || bytes.get(8) != Some(&b'-') {
        return None;
    }
    let year = value.get(0..4)?.parse::<i32>().ok()?;
    let month = value.get(4..6)?.parse::<u8>().ok()?;
    let day = value.get(6..8)?.parse::<u8>().ok()?;
    let hour = value.get(9..11)?.parse::<u8>().ok()?;
    let minute = value.get(11..13)?.parse::<u8>().ok()?;
    civil_millis(year, month, day, hour, minute, 0)
}

/// Converts one UTC civil instant to Unix milliseconds.
fn civil_millis(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> Option<UnixMillis> {
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    seconds.checked_mul(1_000)
}

/// Days from 1970-01-01 to one civil date, by Howard Hinnant's algorithm.
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i32::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i32::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}

/// Converts a model failure to the protocol error that carries it.
pub(crate) fn model_error(error: ModelError) -> MarketParticipantError {
    MarketParticipantError::MarketModel(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::{parse_compact_millis, parse_iso8601_millis, truncate_question};

    #[test]
    fn an_iso_instant_is_parsed_as_utc_milliseconds() {
        assert_eq!(
            parse_iso8601_millis("2026-11-03T23:59:00Z"),
            Some(1_793_750_340_000)
        );
        assert_eq!(parse_iso8601_millis("not-a-date"), None);
    }

    #[test]
    fn a_compact_hip4_instant_is_parsed_as_utc_milliseconds() {
        assert_eq!(
            parse_compact_millis("20261103-2000"),
            Some(1_793_736_000_000)
        );
        assert_eq!(parse_compact_millis("2026-11-03"), None);
    }

    #[test]
    fn a_long_question_is_truncated_at_the_model_limit() {
        let question = "x".repeat(200);
        let truncated = truncate_question(question.into());
        assert_eq!(
            truncated.chars().count(),
            market_participants_model::outcome::MAX_OUTCOME_QUESTION_SCALARS
        );
    }
}
