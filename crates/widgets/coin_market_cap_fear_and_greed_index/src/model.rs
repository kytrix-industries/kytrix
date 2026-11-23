use kytrix_proto::types::v1 as proto_types;
use smol_str::SmolStr;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoinMarketCapFearAndGreedIndexRow {
    pub date: SmolStr,
    pub score: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoinMarketCapFearAndGreedIndexSnapshot {
    pub current: u8,
    pub yesterday: Option<CoinMarketCapFearAndGreedIndexRow>,
    pub last_week: Option<CoinMarketCapFearAndGreedIndexRow>,
    pub last_month: Option<CoinMarketCapFearAndGreedIndexRow>,
    pub last_year: Option<CoinMarketCapFearAndGreedIndexRow>,
    pub yearly_high: Option<CoinMarketCapFearAndGreedIndexRow>,
    pub yearly_low: Option<CoinMarketCapFearAndGreedIndexRow>,
    pub collected_at: i64,
}

impl CoinMarketCapFearAndGreedIndexSnapshot {
    pub(crate) const fn is_empty(&self) -> bool {
        self.collected_at == 0
    }

    pub(crate) fn same_display(&self, other: &Self) -> bool {
        self.is_empty() == other.is_empty()
            && self.current == other.current
            && self.yesterday == other.yesterday
            && self.last_week == other.last_week
            && self.last_month == other.last_month
            && self.last_year == other.last_year
            && self.yearly_high == other.yearly_high
            && self.yearly_low == other.yearly_low
    }

    pub(crate) fn parse(
        index: proto_types::CoinMarketCapFearAndGreedIndex,
    ) -> Result<Self, CoinMarketCapFearAndGreedIndexParseError> {
        Ok(Self {
            current: score(index.current)?,
            yesterday: index.yesterday.map(row).transpose()?,
            last_week: index.last_week.map(row).transpose()?,
            last_month: index.last_month.map(row).transpose()?,
            last_year: index.last_year.map(row).transpose()?,
            yearly_high: index.yearly_high.map(row).transpose()?,
            yearly_low: index.yearly_low.map(row).transpose()?,
            collected_at: index.timestamp,
        })
    }
}

fn row(
    point: proto_types::CoinMarketCapFearAndGreedIndexPoint,
) -> Result<CoinMarketCapFearAndGreedIndexRow, CoinMarketCapFearAndGreedIndexParseError> {
    Ok(CoinMarketCapFearAndGreedIndexRow {
        date: SmolStr::new(point.date),
        score: score(point.score)?,
    })
}

fn score(value: u32) -> Result<u8, CoinMarketCapFearAndGreedIndexParseError> {
    u8::try_from(value)
        .ok()
        .filter(|score| *score <= 100)
        .ok_or(CoinMarketCapFearAndGreedIndexParseError::ScoreOutOfRange { value })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    ExtremeFear,
    Fear,
    Neutral,
    Greed,
    ExtremeGreed,
}

impl Level {
    pub(crate) const fn of(score: u8) -> Self {
        match score {
            0..=19 => Self::ExtremeFear,
            20..=39 => Self::Fear,
            40..=59 => Self::Neutral,
            60..=79 => Self::Greed,
            _ => Self::ExtremeGreed,
        }
    }

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::ExtremeFear => "widget.coin_market_cap_fear_and_greed_index.level.extreme_fear",
            Self::Fear => "widget.coin_market_cap_fear_and_greed_index.level.fear",
            Self::Neutral => "widget.coin_market_cap_fear_and_greed_index.level.neutral",
            Self::Greed => "widget.coin_market_cap_fear_and_greed_index.level.greed",
            Self::ExtremeGreed => "widget.coin_market_cap_fear_and_greed_index.level.extreme_greed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum CoinMarketCapFearAndGreedIndexParseError {
    #[error("the fear and greed snapshot carried score {value}, above 100")]
    ScoreOutOfRange { value: u32 },
}

#[cfg(test)]
mod tests {
    use kytrix_proto::types::v1::{
        CoinMarketCapFearAndGreedIndex, CoinMarketCapFearAndGreedIndexPoint,
    };

    use super::*;

    fn point(date: &str, score: u32) -> CoinMarketCapFearAndGreedIndexPoint {
        CoinMarketCapFearAndGreedIndexPoint {
            date: date.to_owned(),
            score,
        }
    }

    fn collected() -> CoinMarketCapFearAndGreedIndex {
        CoinMarketCapFearAndGreedIndex {
            current: 46,
            yesterday: Some(point("2026-10-23", 45)),
            last_week: Some(point("2026-10-17", 40)),
            last_month: Some(point("2026-09-24", 28)),
            last_year: Some(point("2025-10-24", 72)),
            yearly_high: Some(point("2025-11-20", 88)),
            yearly_low: Some(point("2026-04-15", 12)),
            timestamp: 1_790_000_000_000,
        }
    }

    fn current_only(current: u32) -> CoinMarketCapFearAndGreedIndex {
        CoinMarketCapFearAndGreedIndex {
            current,
            timestamp: 1_790_000_000_000,
            ..CoinMarketCapFearAndGreedIndex::default()
        }
    }

    fn row_of(date: &str, score: u8) -> Option<CoinMarketCapFearAndGreedIndexRow> {
        Some(CoinMarketCapFearAndGreedIndexRow {
            date: SmolStr::new(date),
            score,
        })
    }

    #[test]
    fn a_complete_answer_should_map_every_field() {
        let snapshot = CoinMarketCapFearAndGreedIndexSnapshot::parse(collected())
            .expect("a complete answer should parse");

        assert_eq!(
            snapshot,
            CoinMarketCapFearAndGreedIndexSnapshot {
                current: 46,
                yesterday: row_of("2026-10-23", 45),
                last_week: row_of("2026-10-17", 40),
                last_month: row_of("2026-09-24", 28),
                last_year: row_of("2025-10-24", 72),
                yearly_high: row_of("2025-11-20", 88),
                yearly_low: row_of("2026-04-15", 12),
                collected_at: 1_790_000_000_000,
            }
        );
    }

    #[test]
    fn an_answer_without_history_should_parse_with_every_row_missing() {
        let snapshot = CoinMarketCapFearAndGreedIndexSnapshot::parse(current_only(47))
            .expect("an answer without history should parse");

        assert_eq!(snapshot.current, 47);
        assert_eq!(snapshot.yesterday, None);
        assert_eq!(snapshot.last_week, None);
        assert_eq!(snapshot.last_month, None);
        assert_eq!(snapshot.last_year, None);
        assert_eq!(snapshot.yearly_high, None);
        assert_eq!(snapshot.yearly_low, None);
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn a_score_above_the_scale_should_be_refused() {
        for value in [101_u32, 255, 1_000] {
            assert_eq!(
                CoinMarketCapFearAndGreedIndexSnapshot::parse(current_only(value))
                    .expect_err("a score above 100 is refused"),
                CoinMarketCapFearAndGreedIndexParseError::ScoreOutOfRange { value },
                "the failure has to name the value it refused"
            );

            let mut answer = collected();
            answer.yesterday = Some(point("2026-10-23", value));
            assert_eq!(
                CoinMarketCapFearAndGreedIndexSnapshot::parse(answer)
                    .expect_err("a point above 100 is refused"),
                CoinMarketCapFearAndGreedIndexParseError::ScoreOutOfRange { value }
            );
        }
    }

    #[test]
    fn the_ends_of_the_scale_should_parse_and_name_their_levels() {
        for (score, level) in [(0_u32, Level::ExtremeFear), (100, Level::ExtremeGreed)] {
            let snapshot = CoinMarketCapFearAndGreedIndexSnapshot::parse(current_only(score))
                .expect("an end of the scale should parse");
            let score = u8::try_from(score).expect("the ends fit a byte");

            assert_eq!(snapshot.current, score);
            assert_eq!(Level::of(score), level);
        }
    }

    #[test]
    fn every_band_boundary_should_belong_to_the_band_it_opens() {
        let boundaries = [
            (0, Level::ExtremeFear),
            (19, Level::ExtremeFear),
            (20, Level::Fear),
            (39, Level::Fear),
            (40, Level::Neutral),
            (59, Level::Neutral),
            (60, Level::Greed),
            (79, Level::Greed),
            (80, Level::ExtremeGreed),
            (100, Level::ExtremeGreed),
        ];

        for (score, level) in boundaries {
            assert_eq!(
                Level::of(score),
                level,
                "score {score} belongs to {level:?}"
            );
        }
    }

    #[test]
    fn every_level_should_name_its_own_message_key() {
        let levels = [
            Level::ExtremeFear,
            Level::Fear,
            Level::Neutral,
            Level::Greed,
            Level::ExtremeGreed,
        ];

        for level in levels {
            assert!(
                level
                    .key()
                    .starts_with("widget.coin_market_cap_fear_and_greed_index.level."),
                "{level:?} should name a level key: {}",
                level.key()
            );
        }

        let mut keys = levels.map(Level::key).to_vec();
        keys.sort_unstable();
        let distinct = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), distinct, "each level reads differently");
    }

    #[test]
    fn only_a_zero_collection_time_should_be_an_empty_snapshot() {
        let mut uncollected = collected();
        uncollected.timestamp = 0;
        assert!(
            CoinMarketCapFearAndGreedIndexSnapshot::parse(uncollected)
                .expect("an uncollected answer still parses")
                .is_empty(),
            "a collection time of zero means nothing has been collected yet"
        );

        let zero = CoinMarketCapFearAndGreedIndexSnapshot::parse(current_only(0))
            .expect("a current value of zero is a reading rather than a missing one");
        assert!(!zero.is_empty());
        assert_eq!(zero.current, 0);
    }

    #[test]
    fn two_answers_collected_at_different_times_should_draw_the_same() {
        let snapshot = CoinMarketCapFearAndGreedIndexSnapshot::parse(collected())
            .expect("a complete answer should parse");
        let mut polled_again = snapshot.clone();
        polled_again.collected_at += 60_000;

        assert!(
            snapshot.same_display(&polled_again),
            "the collection time is not on screen"
        );
        assert_ne!(
            snapshot, polled_again,
            "the answers are still different snapshots"
        );
    }

    #[test]
    fn a_changed_reading_or_row_should_be_a_different_display() {
        let reference = CoinMarketCapFearAndGreedIndexSnapshot::parse(collected())
            .expect("a complete answer should parse");
        let mut variants: Vec<(&str, CoinMarketCapFearAndGreedIndexSnapshot)> = Vec::new();

        let mut current = reference.clone();
        current.current = 47;
        variants.push(("current reading", current));

        let changes: [(&str, fn(&mut CoinMarketCapFearAndGreedIndexSnapshot)); 6] = [
            ("yesterday", |snapshot| {
                snapshot.yesterday = row_of("2026-10-22", 45)
            }),
            ("last week", |snapshot| {
                snapshot.last_week = row_of("2026-10-16", 40)
            }),
            ("last month", |snapshot| {
                snapshot.last_month = row_of("2026-09-23", 28)
            }),
            ("last year", |snapshot| {
                snapshot.last_year = row_of("2025-10-23", 72)
            }),
            ("yearly high", |snapshot| {
                snapshot.yearly_high = row_of("2025-11-19", 88)
            }),
            ("yearly low", |snapshot| {
                snapshot.yearly_low = row_of("2026-04-14", 12)
            }),
        ];
        for (name, change) in changes {
            let mut changed = reference.clone();
            change(&mut changed);
            variants.push((name, changed));
        }

        let mut missing = reference.clone();
        missing.yesterday = None;
        variants.push(("row without a reading", missing));

        for (name, changed) in variants {
            assert!(
                !reference.same_display(&changed),
                "a changed {name} is a different picture"
            );
            assert!(
                !changed.same_display(&reference),
                "the comparison is symmetric for {name}"
            );
        }
    }

    #[test]
    fn an_uncollected_answer_should_never_read_the_same_as_a_collected_one() {
        let uncollected = CoinMarketCapFearAndGreedIndexSnapshot::parse(
            CoinMarketCapFearAndGreedIndex::default(),
        )
        .expect("the service's cold answer should parse");
        assert!(uncollected.is_empty());

        let mut collected_with_the_same_rows = uncollected.clone();
        collected_with_the_same_rows.collected_at = 1_790_000_000_000;

        assert!(
            !uncollected.same_display(&collected_with_the_same_rows),
            "the empty state disappearing is a visible change"
        );
        assert!(
            !collected_with_the_same_rows.same_display(&uncollected),
            "and so is the empty state appearing"
        );
    }

    #[test]
    fn an_answer_without_yearly_extremes_should_still_be_a_snapshot_to_draw() {
        let mut answer = collected();
        answer.yearly_high = None;
        answer.yearly_low = None;
        let snapshot = CoinMarketCapFearAndGreedIndexSnapshot::parse(answer)
            .expect("a normal answer should parse");

        assert_eq!(snapshot.yearly_high, None);
        assert_eq!(snapshot.yearly_low, None);
        assert!(!snapshot.is_empty());
        assert_eq!(snapshot.current, 46);
        assert_eq!(snapshot.yesterday, row_of("2026-10-23", 45));
    }
}
