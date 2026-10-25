//! The Pentagon Pizza Index as one pane reads it: the five-rung ladder the service
//! publishes, the reading a round produced, and why a round produced none.
//!
//! Everything here is pure. The only things it borrows from the rest of the
//! application are the translator its failures are written with and the typed
//! error trait the framework shows them through.
//!
//! # The ladder
//!
//! The index does not publish a number with a colour; it publishes a rung of a
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
//! service's own numbering is what a reader has to see.

use gpui_kit::{App, SharedString};
use i18n::t;
use kytrix_proto::types as proto_types;
use thiserror::Error;
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
    /// The level the service published, or nothing when it published one this
    /// pane does not know.
    ///
    /// The scale is closed: a reading outside `1..=5` is refused here rather than
    /// clamped, because a sixth rung is a reading this pane cannot colour or name
    /// and a clamped one would report a severity the service did not publish.
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

    /// The number the service publishes for this rung, which is what a reader sees.
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

/// One reading: the rung, and the moment it was observed at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PizzaReading {
    level: PizzaLevel,
    /// The Unix millisecond the index observed this reading at.
    observed_at: i64,
}

impl PizzaReading {
    /// The reading one service answer states, or why it states none.
    ///
    /// The answer carries the rung in a `uint32` because the protocol has no enum
    /// for it, so the scale is what decides whether the number is a level at all:
    /// a value above a byte cannot be a rung any more than a zero can.
    ///
    /// The moment is checked first, because a zero one is what a collector that has
    /// not run yet answers with, and no reading was ever observed at the start of
    /// 1970: reporting the rung of that answer would name a level nothing published.
    pub(super) fn from_wire(
        index: proto_types::PentagonPizzaIndex,
    ) -> Result<Self, PizzaDataError> {
        if index.observed_at <= 0 {
            return Err(PizzaDataError::MissingObservation);
        }
        let level = u8::try_from(index.overall_index)
            .ok()
            .and_then(PizzaLevel::from_number)
            .ok_or(PizzaDataError::UnknownLevel {
                level: index.overall_index,
            })?;

        Ok(Self {
            level,
            observed_at: index.observed_at,
        })
    }

    pub(super) const fn level(self) -> PizzaLevel {
        self.level
    }

    /// The Unix millisecond this reading was observed at.
    ///
    /// It is kept as the instant the index stamped rather than as a formatted
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

/// Why a published answer could not be read as a reading.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum PizzaDataError {
    /// The answer carries no observation time, which is what a collector that has
    /// not run yet answers with.
    #[error("the Pentagon Pizza Index has not observed a point yet")]
    MissingObservation,
    #[error("the Pentagon Pizza Index published the unknown level {level}")]
    UnknownLevel { level: u32 },
}

impl LocalizedError for PizzaDataError {
    fn localized_message(&self, cx: &App) -> SharedString {
        match self {
            Self::MissingObservation => t!(cx, "error.pentagon_pizza_index.missing_observation"),
            // The level is the service's own number and the pane has no name for
            // it: a sentence showing it is the only honest thing to say, and it is
            // still a sentence about what arrived rather than about the service's
            // formatting.
            Self::UnknownLevel { level } => {
                t!(
                    cx,
                    "error.pentagon_pizza_index.unknown_level",
                    level = level
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{OBSERVED_AT, answer};

    /// The answer the pane is built for is one the typed DTO reads, and the two
    /// fields it is drawn from are the ones the answer actually carries.
    #[test]
    fn a_live_answer_should_read_as_the_reading_it_publishes() {
        let reading = PizzaReading::from_wire(answer(5)).expect("the live answer is a reading");

        assert_eq!(reading.level(), PizzaLevel::FadeOut);
        assert_eq!(
            reading.observed_at(),
            OBSERVED_AT,
            "the moment the index observed the point at"
        );
    }

    /// A collector that has not run yet answers with a message nothing was filled
    /// in, and reporting its rung would name a level nothing published.
    #[test]
    fn an_answer_without_an_observation_should_not_be_read_as_a_reading() {
        for observed_at in [0_i64, -1, i64::MIN] {
            let mut unanswered = answer(5);
            unanswered.observed_at = observed_at;

            assert_eq!(
                PizzaReading::from_wire(unanswered),
                Err(PizzaDataError::MissingObservation),
                "no point was ever observed at {observed_at}"
            );
        }
    }

    /// The scale is closed, so a level outside it is refused rather than clamped
    /// into a neighbour: a clamped reading would report a severity the index never
    /// published.
    #[test]
    fn a_level_outside_the_ladder_should_be_refused_rather_than_clamped() {
        for level in [0_u32, 6, 7, 256, u32::MAX] {
            assert_eq!(
                PizzaReading::from_wire(answer(level)),
                Err(PizzaDataError::UnknownLevel { level }),
                "{level} is not a rung of the ladder"
            );
        }
    }

    /// Every rung reads back as the number it was published under, which is the
    /// one property the pane's number and the service's number share.
    #[test]
    fn every_rung_should_round_trip_through_its_published_number() {
        for number in 1..=5_u8 {
            let level = PizzaLevel::from_number(number).expect("1 to 5 are all rungs");

            assert_eq!(level.number(), number, "level {number}");
            assert_eq!(
                PizzaReading::from_wire(answer(u32::from(number)))
                    .expect("every rung is a reading")
                    .level(),
                level
            );
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
            "the calmest rung is the highest number"
        );
    }
}
