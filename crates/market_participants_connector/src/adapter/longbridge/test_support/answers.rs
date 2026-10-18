//! What the fixture's answers are: the values a test states and the frames it reads
//! back.
//!
//! Every value here is a statement about the venue rather than about this build: the
//! one-time password the token endpoint hands out, whether an authentication is accepted,
//! which packages the profile grants, which candles a candlestick command returns and
//! which days the calendar names. A test configures them and the venue serves them, so an
//! assertion about what this build made of an answer is an assertion about the venue's own
//! input.

use bytes::Bytes;
use longbridge_proto::quote::SecurityQuote;

/// The error code the fixture's refused profile carries.
pub(crate) const PROFILE_REFUSAL_CODE: u64 = 4_000_000;
/// The error message the fixture's refused profile carries.
pub(crate) const PROFILE_REFUSAL_MESSAGE: &str = "the fixture refuses the profile";
/// The error code the fixture's refused subscription carries.
pub(crate) const SUBSCRIPTION_REFUSAL_CODE: u64 = 3_000_000;
/// The error message the fixture's refused subscription carries.
pub(crate) const SUBSCRIPTION_REFUSAL_MESSAGE: &str = "the fixture refuses the subscription";

/// What the venue answers the trading-day command with.
#[derive(Clone, Default)]
pub(crate) struct CalendarAnswer {
    /// The status the trading-day command is answered with.
    pub status: u8,
    /// The half days the venue states, as `YYYYMMDD`.
    pub half_days: Vec<String>,
}

/// What the venue's token endpoint answers.
///
/// The three payload values are what the venue's envelope carries inside its `data`;
/// the envelope's own two values are what it says around them. The live run measured
/// the envelope rather than the flat shape the design states (§14.1), so the fixture
/// answers the envelope, and the code it states is what a refusal is read from.
#[derive(Clone)]
pub(crate) struct TokenAnswer {
    /// The HTTP status the endpoint answers with.
    pub status: u16,
    /// The venue's own outcome code: zero when the request succeeded.
    pub code: i64,
    /// The venue's own sentence about that outcome.
    pub message: &'static str,
    /// Whether the envelope carries a payload at all.
    ///
    /// A granted answer carries one; a refused one states its code and message with
    /// `data: null`, which is the shape the live run measured for its own refusals. The
    /// flag is separate from the code so that a case can state the one shape §4.9
    /// forbids: a success that carries no payload.
    pub payload: bool,
    /// The one-time password the socket is to authenticate with.
    pub otp: &'static str,
    /// How many connections the account may hold open.
    pub limit: i32,
    /// How many it holds open right now.
    pub online: i32,
}

impl TokenAnswer {
    /// One token the account may use.
    pub fn granted(otp: &'static str) -> Self {
        Self {
            status: 200,
            code: 0,
            message: "success",
            payload: true,
            otp,
            limit: 4,
            online: 1,
        }
    }

    /// One token the venue refuses with its own code rather than with a status.
    ///
    /// The venue refused the live run's signing failures this way — `403` with
    /// `{"code":403201,"message":"signature invalid"}` — so a fixture states both, and
    /// the answer a case reads is the envelope's own code.
    pub fn refused(code: i64, message: &'static str, status: u16) -> Self {
        Self {
            status,
            code,
            message,
            payload: false,
            ..Self::granted("fixture-otp")
        }
    }

    /// The same envelope with no payload, which is the shape a refusal carries.
    pub fn without_payload(self) -> Self {
        Self {
            payload: false,
            ..self
        }
    }
}

/// What the venue answers an authentication with.
#[derive(Clone)]
pub(crate) struct AuthAnswer {
    /// The status the authentication frame is answered with.
    pub status: u8,
}

impl AuthAnswer {
    /// An authentication the venue accepts.
    pub fn accepted() -> Self {
        Self { status: 0 }
    }
}

/// What the venue's market-data profile answers.
#[derive(Clone)]
pub(crate) struct ProfileAnswer {
    /// One `(command, limit, burst)` row per command the venue paces.
    pub rate_limit: Vec<(u8, i32, i32)>,
    /// How many US market-data packages the account holds.
    pub us_packages: usize,
    /// The status the profile command is answered with.
    pub status: u8,
}

impl ProfileAnswer {
    /// A profile that grants US market data and paces nothing.
    pub fn granted() -> Self {
        Self {
            rate_limit: Vec::new(),
            us_packages: 1,
            status: 0,
        }
    }

    /// A profile that grants no US market data.
    pub fn without_us_market_data() -> Self {
        Self {
            us_packages: 0,
            ..Self::granted()
        }
    }
}

/// What the venue answers the real-time quote command with.
///
/// The answer is the venue's own `SecurityQuoteResponse`: one quote per symbol the
/// request asked about, and a status that is the venue's refusal of the request itself.
/// The default answer states neither — no quote and no refusal — which is what a case
/// about a request the venue answered nothing for asks the fixture for.
#[derive(Clone, Default)]
pub(crate) struct QuoteAnswer {
    /// The status the real-time quote command is answered with.
    pub status: u8,
    /// The quotes the venue returns, in the order it sends them.
    pub quotes: Vec<SecurityQuote>,
}

/// What the venue answers a candlestick command with.
///
/// One book serves both candlestick commands — the history (cmd 27) and a
/// subscription's opening screen (cmd 19) — because the venue answers both with the same
/// message. The refusal is the venue's own `Error` under a non-zero status, and the
/// subscription's own answer is a separate field: only a subscription can be refused
/// while its screen was granted.
#[derive(Clone, Default)]
pub(crate) struct CandleAnswer {
    /// The status a candlestick command is answered with.
    pub status: u8,
    /// The candles the venue returns, in the order it sends them.
    pub candles: Vec<FixtureCandle>,
    /// The status a subscribe command is answered with.
    pub subscribe_status: u8,
}

impl CandleAnswer {
    /// An answer that returns these candles and accepts the subscription.
    pub fn of(candles: Vec<FixtureCandle>) -> Self {
        Self {
            status: 0,
            candles,
            subscribe_status: 0,
        }
    }
}

/// One candle of the fixture's own tape, stated in the market's seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FixtureCandle {
    /// The candle's open, in whole cents.
    pub open: i64,
    /// Its high.
    pub high: i64,
    /// Its low.
    pub low: i64,
    /// Its close.
    pub close: i64,
    /// Its volume, in shares.
    pub volume: i64,
    /// Its opening instant, in Unix **seconds**.
    pub timestamp: i64,
}

impl FixtureCandle {
    /// One candle with a flat price, which is what a fixture about timestamps needs.
    pub fn flat(cents: i64, timestamp: i64) -> Self {
        Self {
            open: cents,
            high: cents,
            low: cents,
            close: cents,
            volume: 1,
            timestamp,
        }
    }
}

/// One frame the fixture received, read as the protocol states it.
#[derive(Clone, Debug)]
pub(crate) struct ReceivedFrame {
    /// The command the frame asks for.
    pub command: u8,
    /// The request id the frame carries.
    pub request_id: u32,
    /// The frame's body.
    pub body: Bytes,
}
