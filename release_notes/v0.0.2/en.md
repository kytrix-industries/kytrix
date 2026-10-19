# Kytrix v0.0.2

## Added

**Data time zone**

Market data is published in UTC, and until now that was the only clock it was ever shown on. Kytrix now has a **Data
time zone** setting: pick any whole-hour offset from UTC−12 to UTC+14, and every data timestamp is converted to it
before it reaches the screen.

The choice lives in Settings → General → **Data time zone**, and applies across the app — the candlestick time axis and
crosshair, Market Trades, the Force Orders Stream, News, Tweet Monitor, the wallet monitors, and the Open Interest,
Long/Short Ratio and Taker Buy/Sell Volume charts.

Changing it takes effect immediately, on the already-visible data, with no restart. The selection is saved with your
other settings; the default is UTC+0, which is how timestamps were displayed before.

**Account dashboard handoff**

When you are signed in, the avatar menu has a new **Account** entry above Sign out. It opens your account dashboard in
your system browser, signed in as the same account.

## Changed

**Settings navigation**

Appearance is no longer a loose entry at the top of the settings tree. Display preferences now sit under a **General**
group — Appearance and the new Data time zone together, first in the list. The **Market participants** group is expanded
when Settings opens, so venue credentials are visible without an extra click.
