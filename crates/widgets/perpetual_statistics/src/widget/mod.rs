//! The five perpetual-statistics widgets.
//!
//! A widget is one chart of one series: which series is the only thing that
//! differs between them, so each file here states its identity and hands the rest
//! to [`display`], which owns the scope, the poll, the header and the body.
//!
//! The split is deliberate. `widget::display` is where a change to *how* these
//! panes behave belongs — the instrument picker, the interval, the countdown, the
//! refresh — and each chart's own file is where a change to *which series* it is
//! belongs: its id, its name, and the endpoint that answers it.

pub mod display;
pub mod long_short_account_ratio;
pub mod open_interest;
pub mod taker_volume;
pub mod top_trader_account_ratio;
pub mod top_trader_position_ratio;
