use bytes::Bytes;
use longbridge_proto::quote::SecurityQuote;

pub(crate) const PROFILE_REFUSAL_CODE: u64 = 4_000_000;
pub(crate) const PROFILE_REFUSAL_MESSAGE: &str = "the fixture refuses the profile";
pub(crate) const SUBSCRIPTION_REFUSAL_CODE: u64 = 3_000_000;
pub(crate) const SUBSCRIPTION_REFUSAL_MESSAGE: &str = "the fixture refuses the subscription";

#[derive(Clone, Default)]
pub(crate) struct CalendarAnswer {
    pub status: u8,
    pub half_days: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct TokenAnswer {
    pub status: u16,
    pub code: i64,
    pub message: &'static str,
    pub payload: bool,
    pub otp: &'static str,
    pub limit: i32,
    pub online: i32,
}

impl TokenAnswer {
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

    pub fn refused(code: i64, message: &'static str, status: u16) -> Self {
        Self {
            status,
            code,
            message,
            payload: false,
            ..Self::granted("fixture-otp")
        }
    }

    pub fn without_payload(self) -> Self {
        Self {
            payload: false,
            ..self
        }
    }
}

#[derive(Clone)]
pub(crate) struct AuthAnswer {
    pub status: u8,
}

impl AuthAnswer {
    pub fn accepted() -> Self {
        Self { status: 0 }
    }
}

#[derive(Clone)]
pub(crate) struct ProfileAnswer {
    pub rate_limit: Vec<(u8, i32, i32)>,
    pub us_packages: usize,
    pub status: u8,
}

impl ProfileAnswer {
    pub fn granted() -> Self {
        Self {
            rate_limit: Vec::new(),
            us_packages: 1,
            status: 0,
        }
    }

    pub fn without_us_market_data() -> Self {
        Self {
            us_packages: 0,
            ..Self::granted()
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct QuoteAnswer {
    pub status: u8,
    pub quotes: Vec<SecurityQuote>,
}

#[derive(Clone, Default)]
pub(crate) struct CandleAnswer {
    pub status: u8,
    pub candles: Vec<FixtureCandle>,
    pub subscribe_status: u8,
}

impl CandleAnswer {
    pub fn of(candles: Vec<FixtureCandle>) -> Self {
        Self {
            status: 0,
            candles,
            subscribe_status: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FixtureCandle {
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    pub volume: i64,
    pub timestamp: i64,
}

impl FixtureCandle {
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

#[derive(Clone, Debug)]
pub(crate) struct ReceivedFrame {
    pub command: u8,
    pub request_id: u32,
    pub body: Bytes,
}
