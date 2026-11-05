//! The chart's geometry, asserted directly: the plot inside the box, the two
//! domains, the ticks, and the decimation that folds a series onto the plot.

use gpui_kit::{Bounds, Pixels, point, px, size};
use i18n::{global::embedded, language::codes::LanguageCode};
use market_participants_model::prediction::PredictionRange;
use time::{Date, Month, OffsetDateTime, UtcOffset};

use super::{
    geometry::{
        CLOCK_TICK_SPACING, ChartLayout, LEFT_INSET, RIGHT_GUTTER, Sample, TOP_STRIP, TimeDomain,
        TimeLabelKind, TimeTicks, ValueAxis, decimate, lead_index, percent, time_domain,
        time_ticks, visible_max_percent, x_for_time, y_for_percent,
    },
    text::format_axis_labels,
};

fn plot() -> Bounds<Pixels> {
    Bounds::new(point(px(8.0), px(18.0)), size(px(880.0), px(200.0)))
}

/// The layout of the reference chart's box: 928 x 248 gives an 880 x 200 plot.
fn layout() -> ChartLayout {
    ChartLayout::new(Bounds::new(
        point(px(0.0), px(0.0)),
        size(px(928.0), px(248.0)),
    ))
}

fn sample(timestamp: i64, percent: f64) -> Sample {
    Sample { timestamp, percent }
}

fn millis(year: i32, month: Month, day: u8) -> i64 {
    Date::from_calendar_date(year, month, day)
        .expect("the fixture date is valid")
        .midnight()
        .assume_utc()
        .unix_timestamp()
        * 1_000
}

/// The label of every tick the axis chose, formatted the way the chart formats them.
fn labels(ticks: &TimeTicks, zone: UtcOffset) -> Vec<String> {
    let i18n = embedded::load().expect("the embedded locales should load");
    format_axis_labels(
        &ticks.timestamps,
        ticks.label,
        zone,
        &i18n,
        LanguageCode::En,
    )
    .into_iter()
    .map(|label| label.to_string())
    .collect()
}

/// Asserts that no two non-empty labels in a list read the same.
fn assert_unique(labels: &[String]) {
    let mut non_empty: Vec<String> = labels
        .iter()
        .filter(|label| !label.is_empty())
        .cloned()
        .collect();
    let expected = non_empty.len();
    non_empty.sort();
    non_empty.dedup();
    assert_eq!(non_empty.len(), expected, "the labels repeat: {labels:?}");
}

#[test]
fn the_layout_leaves_the_measured_strips_around_the_plot() {
    let layout = layout();

    assert_eq!(layout.plot.origin, point(px(LEFT_INSET), px(TOP_STRIP)));
    assert_eq!(layout.plot.size, size(px(880.0), px(200.0)));
    assert_eq!(layout.plot.right(), px(928.0 - RIGHT_GUTTER));
    assert_eq!(layout.plot.bottom(), px(248.0 - 30.0));
    assert_eq!(layout.top_strip().size.height, px(TOP_STRIP));
    assert_eq!(layout.bottom_band().size.height, px(30.0));
}

#[test]
fn a_box_too_small_for_the_strips_has_an_empty_plot() {
    let layout = ChartLayout::new(Bounds::new(
        point(px(0.0), px(0.0)),
        size(px(20.0), px(20.0)),
    ));

    assert_eq!(layout.plot.size.width, Pixels::ZERO);
    assert_eq!(layout.plot.size.height, Pixels::ZERO);
}

#[test]
fn the_whole_life_domain_is_the_data_extent() {
    assert_eq!(
        time_domain(1_000, 5_000, PredictionRange::All),
        TimeDomain {
            start: 1_000,
            end: 5_000
        }
    );
}

#[test]
fn a_bounded_range_keeps_its_own_span_before_the_newest_sample() {
    assert_eq!(
        time_domain(0, 7_200_000, PredictionRange::Hour1),
        TimeDomain {
            start: 3_600_000,
            end: 7_200_000
        },
        "a market older than the range is trimmed to the range"
    );
}

#[test]
fn a_bounded_range_shrinks_to_a_market_younger_than_it() {
    assert_eq!(
        time_domain(7_000_000, 7_200_000, PredictionRange::Hour1),
        TimeDomain {
            start: 7_000_000,
            end: 7_200_000
        }
    );
}

#[test]
fn a_single_sample_gets_a_symmetric_window() {
    let domain = time_domain(1_000, 1_000, PredictionRange::All);

    assert_eq!(domain.start, 1_000 - 30 * 60 * 1_000);
    assert_eq!(domain.end, 1_000 + 30 * 60 * 1_000);
    assert!(domain.span() > 0);
}

#[test]
fn the_x_mapping_clamps_to_the_plot() {
    let domain = TimeDomain {
        start: 1_000,
        end: 11_000,
    };

    assert_eq!(x_for_time(domain, plot(), 1_000), plot().origin.x);
    assert_eq!(x_for_time(domain, plot(), 11_000), plot().right());
    assert_eq!(
        x_for_time(domain, plot(), 6_000),
        plot().origin.x + plot().size.width / 2.0
    );
    assert_eq!(x_for_time(domain, plot(), 0), plot().origin.x);
    assert_eq!(x_for_time(domain, plot(), 20_000), plot().right());
}

#[test]
fn a_probability_is_read_in_percentage_points() {
    use market_participants_model::decimal::{Fraction, Ratio};

    let half =
        Fraction::new(Ratio::from_inner(500_000_000_000_000_000)).expect("a half is a fraction");
    assert!((percent(half) - 50.0).abs() < f64::EPSILON);
    assert!((percent(Fraction::ONE) - 100.0).abs() < f64::EPSILON);
    assert!((percent(Fraction::ZERO) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn the_value_axis_rounds_up_to_five_and_ticks_by_ten() {
    let thirty_five = ValueAxis::new(31.5);
    assert_eq!(thirty_five.maximum, 35.0);
    assert_eq!(thirty_five.ticks, vec![0.0, 10.0, 20.0, 30.0]);

    let twenty_five = ValueAxis::new(20.2);
    assert_eq!(twenty_five.maximum, 25.0);
    assert_eq!(twenty_five.ticks, vec![0.0, 10.0, 20.0]);
}

#[test]
fn the_value_axis_never_shrinks_below_ten() {
    let quiet = ValueAxis::new(0.0);
    assert_eq!(quiet.maximum, 10.0);
    assert_eq!(quiet.ticks, vec![0.0, 10.0]);

    let low = ValueAxis::new(4.9);
    assert_eq!(low.maximum, 10.0);

    let exact = ValueAxis::new(50.0);
    assert_eq!(exact.maximum, 50.0);
    assert_eq!(exact.ticks, vec![0.0, 10.0, 20.0, 30.0, 40.0, 50.0]);
}

#[test]
fn the_y_mapping_puts_zero_at_the_bottom_and_the_top_at_the_top() {
    let axis = ValueAxis::new(31.5);

    assert_eq!(y_for_percent(0.0, &axis, plot()), plot().bottom());
    assert_eq!(y_for_percent(35.0, &axis, plot()), plot().origin.y);
    assert_eq!(
        y_for_percent(17.5, &axis, plot()),
        plot().origin.y + plot().size.height / 2.0
    );
    assert_eq!(
        y_for_percent(90.0, &axis, plot()),
        plot().origin.y,
        "a value past the axis is clamped rather than drawn outside the plot"
    );
}

#[test]
fn the_lead_is_the_last_sample_before_the_window() {
    let samples = [
        sample(0, 10.0),
        sample(1_000, 20.0),
        sample(2_000, 30.0),
        sample(3_000, 40.0),
    ];

    assert_eq!(lead_index(&samples, 2_500), Some(2));
    assert_eq!(lead_index(&samples, 0), Some(0));
    assert_eq!(lead_index(&samples, -10), None);
    assert_eq!(lead_index(&samples, 5_000), Some(3));
}

#[test]
fn the_largest_probability_counts_the_lead_the_line_enters_from() {
    let samples = [sample(0, 90.0), sample(2_000, 30.0)];
    let domain = TimeDomain {
        start: 1_500,
        end: 2_000,
    };

    assert_eq!(
        visible_max_percent(&samples, domain),
        90.0,
        "the lead is drawn, so its value is part of the axis"
    );
}

#[test]
fn decimation_keeps_at_most_one_point_per_pixel_column() {
    let samples: Vec<Sample> = (0..=1_000)
        .map(|index| sample(index * 10, 10.0 + (index % 7) as f64))
        .collect();
    let domain = TimeDomain {
        start: 0,
        end: 10_000,
    };
    let folded = decimate(&samples, domain, &ValueAxis::new(90.0), plot());

    assert!(
        folded.screen.len() <= 880 + 2,
        "one point per pixel column plus the lead and the first visible point: {} points",
        folded.screen.len()
    );
    assert!(
        folded.screen.windows(2).all(|pair| pair[0].x <= pair[1].x),
        "the folded points keep their order"
    );
    assert!(folded.last.is_some());
}

#[test]
fn decimation_keeps_the_lead_and_the_newest_visible_point() {
    let samples = [
        sample(-1_000, 10.0),
        sample(0, 20.0),
        sample(500, 30.0),
        sample(1_000, 40.0),
    ];
    let domain = TimeDomain {
        start: 0,
        end: 1_000,
    };
    let folded = decimate(&samples, domain, &ValueAxis::new(40.0), plot());

    assert_eq!(
        folded.screen.first().map(|point| point.x),
        Some(plot().origin.x),
        "the line enters at the plot's left edge"
    );
    assert_eq!(
        folded.screen.last().map(|point| point.x),
        Some(plot().right()),
        "the newest sample is at the plot's right edge"
    );
    assert_eq!(folded.last.map(|point| point.x), Some(plot().right()));
}

#[test]
fn decimation_keeps_a_single_sample_as_one_point() {
    let samples = [sample(1_000, 26.5)];
    let domain = time_domain(1_000, 1_000, PredictionRange::All);
    let folded = decimate(&samples, domain, &ValueAxis::new(26.5), plot());

    assert_eq!(folded.screen.len(), 1);
    assert!(folded.last.is_some());
}

#[test]
fn a_subpixel_probability_step_should_not_be_drawn_as_a_second_line() {
    // A 0.05 point dip over a 900 px plot is 0.45 px: real history, but below the
    // resolution of the line. It used to paint a parallel antialiased run beside the
    // flat line, which reads as a duplicated stroke.
    let plot = Bounds::new(point(px(8.0), px(18.0)), size(px(880.0), px(900.0)));
    let samples = [
        sample(0, 99.95),
        sample(1_000, 99.95),
        sample(2_000, 99.90),
        sample(3_000, 99.95),
    ];
    let domain = TimeDomain {
        start: 0,
        end: 3_000,
    };
    let folded = decimate(&samples, domain, &ValueAxis::new(100.0), plot);
    let ys: Vec<Pixels> = folded.screen.iter().map(|point| point.y).collect();

    assert_eq!(ys.len(), 4, "the samples still keep their columns");
    assert!(
        ys.windows(2).all(|pair| pair[0] == pair[1]),
        "the subpixel dip keeps the line's y: {ys:?}"
    );
    assert_eq!(
        folded.last.map(|point| point.y),
        ys.first().copied(),
        "the endpoint sits on the line it ends"
    );
}

#[test]
fn a_probability_step_of_a_pixel_or_more_is_still_drawn() {
    let plot = Bounds::new(point(px(8.0), px(18.0)), size(px(880.0), px(900.0)));
    let samples = [sample(0, 99.95), sample(1_000, 99.80)];
    let domain = TimeDomain {
        start: 0,
        end: 1_000,
    };
    let folded = decimate(&samples, domain, &ValueAxis::new(100.0), plot);

    assert!(
        folded.screen[0].y != folded.screen[1].y,
        "a visible step is not flattened"
    );
}

#[test]
fn a_short_window_picks_a_tick_step_that_fits() {
    let domain = TimeDomain {
        start: 0,
        end: 3_600_000,
    };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Clock);
    assert_eq!(
        ticks.timestamps.len(),
        13,
        "a five-minute step over an hour"
    );
    assert_eq!(ticks.timestamps.first().copied(), Some(0));
    assert!(
        ticks
            .timestamps
            .windows(2)
            .all(|pair| pair[1] - pair[0] == 300_000),
        "the labels are evenly spaced"
    );
    assert_unique(&labels(&ticks, UtcOffset::UTC));
}

#[test]
fn a_day_picks_a_three_hour_step_and_blanks_its_repeated_endpoint() {
    let start = millis(2026, Month::January, 5) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 86_400_000,
    };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Clock);
    assert_eq!(
        ticks.timestamps.len(),
        9,
        "the gridlines keep the closing tick even when its label repeats"
    );
    assert!(
        ticks
            .timestamps
            .windows(2)
            .all(|pair| pair[1] - pair[0] == 10_800_000)
    );
    let labels = labels(&ticks, UtcOffset::UTC);
    assert_unique(&labels);
    assert_eq!(
        labels.iter().filter(|label| label.is_empty()).count(),
        1,
        "the closing clock label repeats the opening one and is blanked: {labels:?}"
    );
}

#[test]
fn a_wide_plot_steps_away_from_hourly_labels_over_a_day() {
    let start = millis(2026, Month::January, 5) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 86_400_000,
    };
    let ticks = time_ticks(domain, px(1600.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Clock);
    assert!(
        ticks.timestamps.len() <= 12,
        "a day's clock axis is capped at twelve labels: {:?}",
        ticks.timestamps
    );
    assert!(
        ticks
            .timestamps
            .windows(2)
            .all(|pair| pair[1] - pair[0] == 10_800_000),
        "the cap forces a step of at least two hours instead of an hourly one that wraps"
    );
    let labels = labels(&ticks, UtcOffset::UTC);
    assert_unique(&labels);
}

#[test]
fn a_twelve_hour_window_still_uses_hourly_labels() {
    let start = millis(2026, Month::January, 5) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 12 * 3_600_000,
    };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Clock);
    assert!(
        ticks
            .timestamps
            .windows(2)
            .all(|pair| pair[1] - pair[0] == 3_600_000),
        "a window of twelve hours or less keeps the normal hourly tier"
    );
    assert_unique(&labels(&ticks, UtcOffset::UTC));
}

#[test]
fn a_four_hundred_pixel_week_still_gets_daily_labels() {
    let start = millis(2026, Month::January, 5) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 7 * 86_400_000,
    };
    let ticks = time_ticks(domain, px(400.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Day);
    assert_eq!(
        ticks.timestamps.len(),
        7,
        "one label per day at four hundred pixels: {:?}",
        ticks.timestamps
    );
    assert_unique(&labels(&ticks, UtcOffset::UTC));
}

#[test]
fn a_narrow_plot_still_gets_at_least_two_ticks() {
    let domain = TimeDomain {
        start: 0,
        end: 3_600_000,
    };
    let ticks = time_ticks(domain, px(CLOCK_TICK_SPACING), UtcOffset::UTC).timestamps;

    assert!(
        ticks.len() >= 2,
        "a narrow axis keeps two labels: {ticks:?}"
    );
    assert!(ticks.len() <= 2);
}

#[test]
fn a_days_ticks_are_aligned_to_the_local_clock() {
    let zone = UtcOffset::from_hms(5, 30, 0).expect("the fixture offset is valid");
    let domain = TimeDomain {
        start: 0,
        end: 86_400_000,
    };
    let ticks = time_ticks(domain, px(880.0), zone).timestamps;
    assert!(!ticks.is_empty());

    for tick in ticks {
        let local = OffsetDateTime::from_unix_timestamp_nanos(i128::from(tick) * 1_000_000)
            .expect("the tick is a valid instant")
            .to_offset(zone);
        assert_eq!(
            tick.rem_euclid(3_600_000),
            1_800_000,
            "a +05:30 zone puts hour steps at :30 UTC"
        );
        assert_eq!(local.minute(), 0);
        assert_eq!(local.hour() % 3, 0);
    }
}

#[test]
fn long_windows_are_labelled_at_month_starts() {
    let start = millis(2026, Month::January, 1);
    let end = start + 400 * 86_400_000;
    let domain = TimeDomain { start, end };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Month);
    assert!(
        ticks.timestamps.len() >= 12,
        "400 days are labelled by month: {:?}",
        ticks.timestamps
    );
    for tick in ticks.timestamps {
        let local = OffsetDateTime::from_unix_timestamp_nanos(i128::from(tick) * 1_000_000)
            .expect("the tick is a valid instant");
        assert_eq!(local.day(), 1, "a month tick starts on the first");
        assert_eq!(local.hour(), 0);
        assert_eq!(local.minute(), 0);
    }
}

#[test]
fn a_multi_year_window_picks_a_year_step() {
    let start = millis(2000, Month::January, 1);
    let end = millis(2026, Month::January, 1);
    let domain = TimeDomain { start, end };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::MonthYear);
    assert!(ticks.timestamps.len() >= 2);
    assert!(
        ticks.timestamps.len() <= 16,
        "one label per 72 pixels: {:?}",
        ticks.timestamps
    );
    for tick in ticks.timestamps {
        let local = OffsetDateTime::from_unix_timestamp_nanos(i128::from(tick) * 1_000_000)
            .expect("the tick is a valid instant");
        assert_eq!(local.month(), Month::January);
        assert_eq!(local.day(), 1);
    }
}

#[test]
fn a_century_axis_strides_years_without_walking_the_finer_steps() {
    let start = millis(1900, Month::January, 1);
    let end = millis(2000, Month::January, 1);
    let ticks = time_ticks(TimeDomain { start, end }, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::MonthYear);
    assert!(
        (2..=16).contains(&ticks.timestamps.len()),
        "a century still fits one label per 72 pixels: {:?}",
        ticks.timestamps
    );
    let labels = labels(&ticks, UtcOffset::UTC);
    assert_unique(&labels);
    for tick in ticks.timestamps {
        let local = OffsetDateTime::from_unix_timestamp_nanos(i128::from(tick) * 1_000_000)
            .expect("the tick is a valid instant");
        assert_eq!(local.month(), Month::January);
        assert_eq!(local.day(), 1);
    }
}

#[test]
fn a_week_is_labelled_by_distinct_days_at_the_reference_width() {
    let start = millis(2026, Month::January, 5) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 7 * 86_400_000,
    };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Day);
    assert_eq!(
        ticks.timestamps.len(),
        7,
        "one label a day: {:?}",
        ticks.timestamps
    );
    let labels = labels(&ticks, UtcOffset::UTC);
    assert_unique(&labels);
    assert_eq!(labels.first().map(String::as_str), Some("Jan 6"));
    assert_eq!(labels.last().map(String::as_str), Some("Jan 12"));
}

#[test]
fn a_month_is_labelled_by_unique_dates_rather_than_months() {
    let start = millis(2026, Month::January, 1) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 30 * 86_400_000,
    };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Day);
    assert!(
        (10..=11).contains(&ticks.timestamps.len()),
        "a three-day step over a month: {:?}",
        ticks.timestamps
    );
    let labels = labels(&ticks, UtcOffset::UTC);
    assert_unique(&labels);
    assert!(
        labels.iter().all(|label| label.contains(' ')),
        "the month tier labels dates, not month names: {labels:?}"
    );
}

#[test]
fn a_two_month_all_range_is_labelled_by_unique_weeks() {
    let start = millis(2026, Month::January, 1) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 60 * 86_400_000,
    };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Day);
    assert!(
        (8..=9).contains(&ticks.timestamps.len()),
        "a weekly step over sixty days: {:?}",
        ticks.timestamps
    );
    assert_unique(&labels(&ticks, UtcOffset::UTC));
}

#[test]
fn a_two_day_all_range_is_labelled_by_unique_days() {
    let start = millis(2026, Month::January, 5) + 12 * 3_600_000;
    let domain = TimeDomain {
        start,
        end: start + 2 * 86_400_000,
    };
    let ticks = time_ticks(domain, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Day);
    assert_eq!(ticks.timestamps.len(), 2);
    assert_unique(&labels(&ticks, UtcOffset::UTC));
}

#[test]
fn a_fifteen_month_window_uses_bare_month_labels_like_the_reference() {
    let start = millis(2024, Month::August, 1);
    let end = millis(2025, Month::November, 1);
    let ticks = time_ticks(TimeDomain { start, end }, px(880.0), UtcOffset::UTC);

    assert_eq!(ticks.label, TimeLabelKind::Month);
    assert_eq!(
        ticks.timestamps.len(),
        16,
        "the reference's sixteen month starts: {:?}",
        ticks.timestamps
    );
    let labels = labels(&ticks, UtcOffset::UTC);
    assert_eq!(labels.first().map(String::as_str), Some("Aug"));
    assert_eq!(labels.last().map(String::as_str), Some("Nov"));
    assert!(
        labels.iter().all(|label| label.len() == 3),
        "a fifteen-month window keeps the year off its labels: {labels:?}"
    );
    for tick in ticks.timestamps {
        let local = OffsetDateTime::from_unix_timestamp_nanos(i128::from(tick) * 1_000_000)
            .expect("the tick is a valid instant");
        assert_eq!(local.day(), 1, "a month tick starts on the first");
        assert_eq!(local.hour(), 0);
    }
}

#[test]
fn a_two_year_window_puts_the_year_in_its_month_labels() {
    let start = millis(2025, Month::January, 1);
    let end = millis(2027, Month::January, 1);
    let ticks = time_ticks(TimeDomain { start, end }, px(880.0), UtcOffset::UTC);

    assert_eq!(
        ticks.label,
        TimeLabelKind::MonthYear,
        "a two-year window needs its year to keep its months apart"
    );
    assert_eq!(
        ticks.timestamps.len(),
        9,
        "a quarterly step over two years: {:?}",
        ticks.timestamps
    );
    let labels = labels(&ticks, UtcOffset::UTC);
    assert_unique(&labels);
    assert!(
        labels.iter().all(|label| label.contains(" 202")),
        "every month label is year-disambiguated: {labels:?}"
    );
    assert_eq!(labels.first().map(String::as_str), Some("Jan 2025"));
    assert_eq!(labels.last().map(String::as_str), Some("Jan 2027"));
}
