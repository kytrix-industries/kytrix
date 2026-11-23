use i18n::{t_smol, translate::Translate};
use kytrix_proto::types::v1 as proto_types;
use smol_str::SmolStr;
use thiserror::Error;
use widget::error::TranslatedError;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) enum PizzaLevel {
    FadeOut,
    DoubleTake,
    RoundHouse,
    FastPace,
    ExerciseTerm,
}

impl PizzaLevel {
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

    pub(super) const fn number(self) -> u8 {
        match self {
            Self::FadeOut => 5,
            Self::DoubleTake => 4,
            Self::RoundHouse => 3,
            Self::FastPace => 2,
            Self::ExerciseTerm => 1,
        }
    }

    pub(super) const fn name_key(self) -> &'static str {
        match self {
            Self::FadeOut => "widget.pentagon_pizza_index.level_5",
            Self::DoubleTake => "widget.pentagon_pizza_index.level_4",
            Self::RoundHouse => "widget.pentagon_pizza_index.level_3",
            Self::FastPace => "widget.pentagon_pizza_index.level_2",
            Self::ExerciseTerm => "widget.pentagon_pizza_index.level_1",
        }
    }

    pub(super) fn name(self, translate: &Translate) -> SmolStr {
        t_smol!(translate, self.name_key())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PizzaReading {
    level: PizzaLevel,
    observed_at: i64,
}

impl PizzaReading {
    pub(super) fn from_wire(
        index: proto_types::PentagonPizzaIndex,
    ) -> Result<Self, PizzaDataError> {
        if index.observed_at <= 0 {
            return Err(PizzaDataError::MissingObservation);
        }
        let level = u8::try_from(index.defcon_level)
            .ok()
            .and_then(PizzaLevel::from_number)
            .ok_or(PizzaDataError::UnknownLevel {
                level: index.defcon_level,
            })?;

        Ok(Self {
            level,
            observed_at: index.observed_at,
        })
    }

    pub(super) const fn level(self) -> PizzaLevel {
        self.level
    }

    pub(super) const fn observed_at(self) -> i64 {
        self.observed_at
    }

    #[cfg(test)]
    pub(super) const fn for_test(level: PizzaLevel, observed_at: i64) -> Self {
        Self { level, observed_at }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum PizzaDataError {
    #[error("the Pentagon Pizza Index has not observed a point yet")]
    MissingObservation,
    #[error("the Pentagon Pizza Index published the unknown level {level}")]
    UnknownLevel { level: u32 },
}

impl TranslatedError for PizzaDataError {
    fn translated_message(&self, translate: &Translate) -> SmolStr {
        match self {
            Self::MissingObservation => {
                t_smol!(translate, "error.pentagon_pizza_index.missing_observation")
            }
            Self::UnknownLevel { level } => {
                t_smol!(
                    translate,
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
