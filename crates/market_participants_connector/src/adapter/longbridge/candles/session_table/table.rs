use jiff::{Zoned, civil, tz::TimeZone};

pub(crate) const MARKET_ZONE: &str = "America/New_York";

pub(crate) const NORMAL_SESSION_TABLE: SessionTable = SessionTable::new(
    (4, 0, 9, 30),
    (9, 30, 16, 0),
    (16, 0, 20, 0),
    ((20, 0), (4, 0)),
);

pub(crate) const HALF_DAY_SESSION_TABLE: SessionTable = SessionTable::new(
    (4, 0, 9, 30),
    (9, 30, 13, 0),
    (13, 0, 17, 0),
    ((20, 0), (4, 0)),
);

const END_OF_DAY_MINUTE: i32 = 24 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MarketSession {
    Normal,
    Pre,
    Post,
    Overnight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowClose {
    SameDay(i8, i8),
    NextDay(i8, i8),
}

pub(crate) type SessionWindow = ((i8, i8), WindowClose);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatchedWindow {
    pub session: MarketSession,
    pub open: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionTable {
    pre: SessionWindow,
    normal: SessionWindow,
    post: SessionWindow,
    overnight: SessionWindow,
}

impl SessionTable {
    pub(crate) const fn new(
        pre: (i8, i8, i8, i8),
        normal: (i8, i8, i8, i8),
        post: (i8, i8, i8, i8),
        overnight: ((i8, i8), (i8, i8)),
    ) -> Self {
        Self {
            pre: ((pre.0, pre.1), WindowClose::SameDay(pre.2, pre.3)),
            normal: (
                (normal.0, normal.1),
                WindowClose::SameDay(normal.2, normal.3),
            ),
            post: ((post.0, post.1), WindowClose::SameDay(post.2, post.3)),
            overnight: (
                overnight.0,
                WindowClose::NextDay(overnight.1.0, overnight.1.1),
            ),
        }
    }

    #[cfg(test)]
    pub(crate) const fn session(&self, session: MarketSession) -> SessionWindow {
        match session {
            MarketSession::Pre => self.pre,
            MarketSession::Normal => self.normal,
            MarketSession::Post => self.post,
            MarketSession::Overnight => self.overnight,
        }
    }

    pub(crate) fn window_of(
        &self,
        local: civil::Time,
        date: civil::Date,
        zone: &TimeZone,
    ) -> Option<MatchedWindow> {
        let minute = i32::from(local.hour()) * 60 + i32::from(local.minute());
        let windows = [
            (MarketSession::Pre, self.pre),
            (MarketSession::Normal, self.normal),
            (MarketSession::Post, self.post),
            (MarketSession::Overnight, self.overnight),
        ];
        for (session, ((open_hour, open_minute), close)) in windows {
            let open = i32::from(open_hour) * 60 + i32::from(open_minute);
            let matched = match close {
                WindowClose::SameDay(close_hour, close_minute) => {
                    let close = i32::from(close_hour) * 60 + i32::from(close_minute);
                    (open..close.min(END_OF_DAY_MINUTE)).contains(&minute)
                }
                WindowClose::NextDay(close_hour, close_minute) => {
                    let close = i32::from(close_hour) * 60 + i32::from(close_minute);
                    (open..END_OF_DAY_MINUTE).contains(&minute) || (0..close).contains(&minute)
                }
            };
            if matched {
                let opens_on = match (close, minute < open) {
                    (WindowClose::NextDay(..), true) => date.yesterday().ok()?,
                    _ => date,
                };
                let open = opens_on
                    .at(open_hour, open_minute, 0, 0)
                    .to_zoned(zone.clone())
                    .ok()?;
                return Some(MatchedWindow {
                    session,
                    open: open.timestamp().as_millisecond(),
                });
            }
        }
        None
    }

    pub(crate) fn fixed_bucket(
        &self,
        local: Zoned,
        interval_millis: i64,
        zone: &TimeZone,
    ) -> Option<i64> {
        let matched = self.window_of(local.time(), local.date(), zone)?;
        let elapsed = (local.timestamp().as_millisecond() - matched.open).max(0);
        Some(matched.open + (elapsed / interval_millis) * interval_millis)
    }
}

impl MarketSession {
    pub(crate) const fn is_normal(self) -> bool {
        matches!(self, Self::Normal)
    }
}
