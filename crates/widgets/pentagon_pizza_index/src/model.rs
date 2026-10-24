//! The Pentagon Pizza Index as one pane reads it: the five-rung ladder the feed
//! publishes, the reading a round produced, and why a round produced none.
//!
//! Everything here is pure. The only things it borrows from the rest of the
//! application are the translator its failures are written with and the typed
//! error trait the framework shows them through.
//!
//! # The ladder
//!
//! The feed does not publish a number with a colour; it publishes a rung of a
//! five-level readiness scale, and the colour is the rung's. The scale is the one
//! the site itself draws (its own frontend names the five levels `FADE OUT`,
//! `DOUBLE TAKE`, `ROUND HOUSE`, `FAST PACE` and `EXERCISE TERM` for levels 5 to
//! 1), so the model keeps the level as a closed enum rather than as an integer
//! with a range check at every use: a level outside the scale is a reading this
//! pane refuses, not one it clamps into a neighbour.
//!
//! # Why the rank and the number are two things
//!
//! The number counts *down* as the alert rises — level 1 is the most serious —
//! while everything that compares two readings, narrows a colour scale or asks
//! "is this worse" wants the opposite order. [`PizzaLevel`] therefore derives its
//! ordering from the variants' declaration order, which runs from calm to
//! maximum, and offers [`PizzaLevel::number`] separately for the one place the
//! feed's own numbering is what a reader has to see.

use gpui_kit::{App, SharedString};
use i18n::t;
use serde::Deserialize;
use smol_str::SmolStr;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use widget::error::LocalizedError;

/// One rung of the readiness ladder, ordered from the calmest to the most severe.
///
/// The variants are in ascending severity while the numbers they stand for are in
/// descending order, which is why the number is a method rather than a
/// discriminant: an enum whose discriminants decided its ordering would sort its
/// calmest rung last.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) enum PizzaLevel {
    /// Level 5: the lowest state of readiness, and the scale's quietest rung.
    FadeOut,
    /// Level 4: an increased intelligence watch.
    DoubleTake,
    /// Level 3: an increase in force readiness.
    RoundHouse,
    /// Level 2: the step below maximum readiness.
    FastPace,
    /// Level 1: maximum readiness.
    ExerciseTerm,
}

impl PizzaLevel {
    /// The level the feed published, or nothing when it published one this pane
    /// does not know.
    ///
    /// The scale is closed: a reading outside `1..=5` is refused here rather than
    /// clamped, because a sixth rung is a reading this pane cannot colour or name
    /// and a clamped one would report a severity the feed did not publish.
    pub(super) const fn from_number(number: u8) -> Option<Self> {
        match number {
            5 => Some(Self::FadeOut),
            4 => Some(Self::DoubleTake),
            3 => Some(Self::RoundHouse),
            2 => Some(Self::FastPace),
            1 => Some(Self::ExerciseTerm),
            _ => None,
        }
    }

    /// The number the feed publishes for this rung, which is what a reader sees.
    pub(super) const fn number(self) -> u8 {
        match self {
            Self::FadeOut => 5,
            Self::DoubleTake => 4,
            Self::RoundHouse => 3,
            Self::FastPace => 2,
            Self::ExerciseTerm => 1,
        }
    }

    /// The message key this rung's own name is read from.
    ///
    /// The site names its levels after code words (`FADE OUT`, `EXERCISE TERM`),
    /// which read as jargon in either language and mean nothing to a reader who
    /// has not seen them before. What a locale spells instead is the readiness the
    /// rung states, which is what the pane is actually reporting.
    pub(super) const fn name_key(self) -> &'static str {
        match self {
            Self::FadeOut => "widget.pentagon_pizza_index.level_5",
            Self::DoubleTake => "widget.pentagon_pizza_index.level_4",
            Self::RoundHouse => "widget.pentagon_pizza_index.level_3",
            Self::FastPace => "widget.pentagon_pizza_index.level_2",
            Self::ExerciseTerm => "widget.pentagon_pizza_index.level_1",
        }
    }

    /// The rung's own name in the language showing now.
    pub(super) fn name(self, cx: &App) -> SharedString {
        t!(cx, self.name_key())
    }
}

/// One reading: the rung, and the moment the feed stamped it with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PizzaReading {
    level: PizzaLevel,
    /// The Unix millisecond the feed observed this reading at.
    observed_at: i64,
}

impl PizzaReading {
    /// The reading one published point states, or why it states none.
    pub(super) fn from_wire(point: WirePoint) -> Result<Self, PizzaDataError> {
        let level = PizzaLevel::from_number(point.level)
            .ok_or(PizzaDataError::UnknownLevel { level: point.level })?;

        Ok(Self {
            level,
            observed_at: parse_timestamp(&point.timestamp)?,
        })
    }

    pub(super) const fn level(self) -> PizzaLevel {
        self.level
    }

    /// The Unix millisecond this reading was observed at.
    ///
    /// It is kept as the instant the feed stamped rather than as a formatted
    /// string: what the pane draws is resolved by the frame that draws it, in the
    /// display time zone showing then, so caching the text would freeze a clock
    /// into the reading.
    pub(super) const fn observed_at(self) -> i64 {
        self.observed_at
    }

    /// One reading, for a test that reports one by hand.
    #[cfg(test)]
    pub(super) const fn for_test(level: PizzaLevel, observed_at: i64) -> Self {
        Self { level, observed_at }
    }
}

/// One point of the feed, as it is published.
///
/// It is the payload of `https://www.pizzint.watch/api/defcon/latest`:
/// `{"success":true,"data":{"timestamp":"…","defcon_level":5,"defcon_score":0}}`.
/// The score is deliberately not read: the pane reports the rung and the moment,
/// and a field nothing draws is a field that will disagree with the code that
/// stopped reading it.
#[derive(Clone, Debug, Deserialize)]
pub(super) struct WirePoint {
    timestamp: SmolStr,
    #[serde(rename = "defcon_level")]
    level: u8,
}

/// The envelope every point is published in.
///
/// `success` is the feed's own verdict on the read, and a `false` one carries no
/// point at all: treating that as a missing point rather than as an empty reading
/// is what keeps a refused read from being drawn as "level 5, calm".
#[derive(Clone, Debug, Deserialize)]
pub(super) struct WireEnvelope {
    success: bool,
    data: Option<WirePoint>,
}

impl WireEnvelope {
    /// The reading this answer carries, or why it carries none.
    pub(super) fn into_reading(self) -> Result<PizzaReading, PizzaDataError> {
        if !self.success {
            return Err(PizzaDataError::Refused);
        }
        let point = self.data.ok_or(PizzaDataError::MissingPoint)?;

        PizzaReading::from_wire(point)
    }
}

/// Why a published answer could not be read as a reading.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum PizzaDataError {
    #[error("the Pentagon Pizza Index refused the read")]
    Refused,
    #[error("the Pentagon Pizza Index answered without a reading")]
    MissingPoint,
    #[error("the Pentagon Pizza Index published the unknown level {level}")]
    UnknownLevel { level: u8 },
    #[error("the Pentagon Pizza Index published an unreadable timestamp")]
    Timestamp,
    #[error("the Pentagon Pizza Index answer is not the document this pane reads")]
    Unreadable,
}

impl LocalizedError for PizzaDataError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            // The level is the feed's own number and the pane has no name for it:
            // a sentence showing it is the only honest thing to say, and it is
            // still a sentence about what arrived rather than about the feed's
            // formatting.
            Self::Refused => t!(cx, "error.pentagon_pizza_index.refused"),
            Self::MissingPoint => t!(cx, "error.pentagon_pizza_index.missing_point"),
            Self::UnknownLevel { level } => {
                t!(
                    cx,
                    "error.pentagon_pizza_index.unknown_level",
                    level = level
                )
            }
            Self::Timestamp => t!(cx, "error.pentagon_pizza_index.timestamp"),
            Self::Unreadable => t!(cx, "error.pentagon_pizza_index.unreadable"),
        }
    }
}

/// The Unix millisecond one RFC 3339 timestamp names.
///
/// The feed stamps its points in UTC (`2026-10-24T06:00:00.000Z`), so the parse is
/// of an instant rather than of a wall clock: the offset it carries is applied by
/// the parser and is not something a reader sees.
fn parse_timestamp(timestamp: &str) -> Result<i64, PizzaDataError> {
    OffsetDateTime::parse(timestamp, &Rfc3339)
        .ok()
        .map(|time| time.unix_timestamp_nanos())
        .and_then(|nanos| i64::try_from(nanos / 1_000_000).ok())
        .ok_or(PizzaDataError::Timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One answer of the feed, exactly as the live endpoint publishes it.
    const LIVE_ANSWER: &str = r#"{"success":true,"data":{"timestamp":"2026-10-24T06:00:00.000Z","defcon_level":5,"defcon_score":0}}"#;

    fn envelope(json: &str) -> WireEnvelope {
        serde_json::from_str(json).expect("the fixture is a wire envelope")
    }

    /// The payload the pane is built for is one the typed DTO reads, and the two
    /// fields it is drawn from are the ones the answer actually carries.
    #[test]
    fn a_live_answer_should_read_as_the_reading_it_publishes() {
        let reading = envelope(LIVE_ANSWER)
            .into_reading()
            .expect("the live answer is a reading");

        assert_eq!(reading.level(), PizzaLevel::FadeOut);
        assert_eq!(
            reading.observed_at(),
            1_792_821_600_000,
            "2026-10-24T06:00:00Z as Unix milliseconds"
        );
    }

    /// The scale is closed, so a level outside it is refused rather than clamped
    /// into a neighbour: a clamped reading would report a severity the feed never
    /// published.
    #[test]
    fn a_level_outside_the_ladder_should_be_refused_rather_than_clamped() {
        for level in [0_u8, 6, 7, u8::MAX] {
            let json = format!(
                r#"{{"success":true,"data":{{"timestamp":"2026-10-24T06:00:00.000Z","defcon_level":{level}}}}}"#
            );

            assert_eq!(
                envelope(&json).into_reading(),
                Err(PizzaDataError::UnknownLevel { level }),
                "{level} is not a rung of the ladder"
            );
        }
    }

    /// Every rung reads back as the number it was published under, which is the
    /// one property the pane's number and the feed's number share.
    #[test]
    fn every_rung_should_round_trip_through_its_published_number() {
        for number in 1..=5_u8 {
            let level = PizzaLevel::from_number(number).expect("1 to 5 are all rungs");

            assert_eq!(level.number(), number, "level {number}");
        }
        assert_eq!(PizzaLevel::from_number(0), None);
        assert_eq!(PizzaLevel::from_number(6), None);
    }

    /// The ladder is ordered from calm to maximum even though the numbers run the
    /// other way, which is what lets everything that compares two readings mean
    /// "worse" by "greater".
    #[test]
    fn the_ladder_should_be_ordered_from_calm_to_maximum() {
        assert!(PizzaLevel::FadeOut < PizzaLevel::DoubleTake);
        assert!(PizzaLevel::DoubleTake < PizzaLevel::RoundHouse);
        assert!(PizzaLevel::RoundHouse < PizzaLevel::FastPace);
        assert!(PizzaLevel::FastPace < PizzaLevel::ExerciseTerm);
        assert_eq!(
            PizzaLevel::FadeOut.number(),
            5,
            "the calmest rung is the feed's highest number"
        );
    }

    /// A refused read carries no point, and drawing one as level 5 would be
    /// reporting calm in answer to a request that failed.
    #[test]
    fn a_refused_read_should_not_be_read_as_a_reading() {
        assert_eq!(
            envelope(r#"{"success":false,"error":"rate limited"}"#).into_reading(),
            Err(PizzaDataError::Refused)
        );
        assert_eq!(
            envelope(r#"{"success":true}"#).into_reading(),
            Err(PizzaDataError::MissingPoint)
        );
    }

    /// A timestamp that is not one cannot be turned into the instant the pane
    /// draws, and guessing one would put a made-up time under a real reading.
    #[test]
    fn an_unreadable_timestamp_should_fail_the_read() {
        for timestamp in [
            "",
            "yesterday",
            "2026-10-24 06:00:00",
            "2026-13-24T06:00:00Z",
        ] {
            let json = format!(
                r#"{{"success":true,"data":{{"timestamp":"{timestamp}","defcon_level":3}}}}"#
            );

            assert_eq!(
                envelope(&json).into_reading(),
                Err(PizzaDataError::Timestamp),
                "{timestamp:?} is not an instant"
            );
        }
    }

    /// The offset a timestamp carries is applied rather than ignored: a point
    /// stamped at `+01:00` is the instant it names, not the wall clock it spells.
    #[test]
    fn a_timestamp_should_be_read_as_the_instant_its_offset_names() {
        let utc = envelope(
            r#"{"success":true,"data":{"timestamp":"2026-10-24T06:00:00.000Z","defcon_level":3}}"#,
        )
        .into_reading()
        .expect("a UTC point is a reading");
        let shifted = envelope(
            r#"{"success":true,"data":{"timestamp":"2026-10-24T07:00:00.000+01:00","defcon_level":3}}"#,
        )
        .into_reading()
        .expect("an offset point is a reading");

        assert_eq!(utc.observed_at(), shifted.observed_at());
    }
}
