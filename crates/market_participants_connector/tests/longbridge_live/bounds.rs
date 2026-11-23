use std::time::Duration;

pub const CASE_TIMEOUT: Duration = Duration::from_secs(180);
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub const SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(60);
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
pub const PUSH_WINDOW: Duration = Duration::from_secs(30);
pub const MAX_READ: Duration = Duration::from_secs(3);
pub const RECONNECT_TIMEOUT: Duration = Duration::from_secs(45);
pub const PING_OBSERVATION: Duration = Duration::from_secs(20);

pub const REQUEST_TIMEOUT_MILLIS: u16 = 30_000;
pub const AUTH_REQUEST_ID: u32 = 0;
pub const FIRST_SCREEN_COUNT: i32 = 1_000;
pub const TRADE_SESSION_ALL: i32 = 100;
