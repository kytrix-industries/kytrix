# Kytrix v0.0.3

## Added

**Candlestick indicators**

Candlestick charts now support technical indicators. A new **Chart settings** button in the header, to the left of the adjustment and interval selectors, opens a settings panel on the right side of the chart and narrows the chart area.

The main chart supports MA and EMA, and the sub charts support VOL, MACD, RSI and KDJ. Several sub charts can be enabled at once and stack from top to bottom in the order VOL → MACD → RSI → KDJ. For MA, EMA and RSI, each line's period (1–500) and colour can be set, and lines can be added or removed individually; MACD has short, long and signal periods, with separate toggles and colours for DIF and DEA, and also draws a histogram; KDJ has a calculation period and two smoothing periods, with separate toggles and colours for K, D and J; VOL has no settings.

Indicator colours are set with a new colour picker: choose a swatch from the palette, adjust the HSLA sliders, or type a hexadecimal colour. Each indicator's enabled state, settings and colours are saved with the workspace; VOL and MA (5, 10, 20) are enabled by default. The top-left corner of the main chart and of each sub chart shows the name, parameters and current value of each line or indicator, and moving the crosshair reveals the value at the corresponding bar. When you enable an indicator that needs more history, the chart fetches the required history automatically.

**Global Shipping Chokepoint**

The new **Global Shipping Chokepoint** widget tracks vessels in the Strait of Hormuz using live AIS data. The basemap is drawn from OpenStreetMap coastlines, borders and roads, with separate colour schemes for the light and dark appearances, and names for 5 countries, 31 cities and 2 bodies of water (some city names appear as you zoom in).

The map supports drag-to-pan and wheel or pinch zoom (1×–4×). Vessels are coloured by 20 ship types, with the bow direction following their course; hovering over a vessel shows 16 fields, including its name, MMSI, IMO, call sign, flag, ship type, navigation status, speed, course, heading, destination, ETA, draught, dimensions, and the last update times for dynamic and static data. The header can filter by ship type; the legend keeps every type visible (types excluded by the filter are greyed out), and the vessel count reflects the current filter. If the data has not updated for more than 5 minutes, the widget reports the current picture as failed and offers a recovery action instead of continuing to present it as live data.

The widget requires sign-in, and the account must include an AIS vessel data subscription; without one, you can view subscription plans from the widget.

**Financial Calendar**

The new **Financial Calendar** widget lists upcoming global financial events by date. The table has four columns — Importance, Time, Country and Name — and events are grouped by day; importance is shown as dots (one to three for low, medium and high respectively), and the event's reference period (month, quarter and so on) appears beside its name.

The header can filter by importance, date and country, and reset all filters with one click. All times are shown in the **Data time zone** setting, and changing the time zone re-groups events by day immediately; all-day events are never shifted to an adjacent date. The calendar refreshes automatically every 10 minutes, with a countdown to the next refresh and a manual refresh button in the header. The widget requires sign-in.

**Pentagon Pizza Index**

The new **Pentagon Pizza Index** widget shows the latest observation of the index. The index is a five-level readiness scale, from Level 1 ("Maximum readiness") to Level 5 ("Lowest state of readiness"); the card is coloured by level and shows the level number and observation time. The refresh button in the header shows a countdown to the next automatic reading (every 10 minutes) and can also refresh immediately. The widget requires sign-in.

**CoinMarketCap Fear & Greed Index**

The new **CoinMarketCap Fear & Greed Index** widget uses a semicircular gauge to show the current index value and its level (Extreme Fear, Fear, Neutral, Greed or Extreme Greed), with historical values for yesterday, last week, last month and last year listed below, as well as the yearly high and low over the past 365 calendar days, the dates on which they occurred and the levels at the time. The widget refreshes automatically every 60 seconds and can also be refreshed manually. The widget requires sign-in.

## Changed

**Wallet monitoring split**

The former Wallet Monitor has been split into two independent widgets, Ethereum Wallet Monitor and Solana Wallet Monitor, each using the address format and monitoring API for its chain. The wallet address list no longer shows Active/Paused status or update times, and is sorted by when each address was added; the management dialog is now chain-specific.

**Settings navigation**

The settings window now has a navigation sidebar on the left and page content on the right: group headings no longer collapse, and pages are listed directly beneath them. Search rebuilds the sidebar as you type, keeping only matching pages and groups, and automatically switches to the first matching page when the current page no longer matches.

**Update dialog**

The app update entry point is now an icon-only button. The update dialog has been redesigned: release notes are shown in a wider panel with a persistent scrollbar, download status and progress are centred, and the old dialog title and hover tooltip have been removed.

**Widget names and categories**

Tweet Monitor has been renamed to X; ETF Flow has been renamed to Crypto Currency ETF and moved from the Market category to Statistics; Hyperliquid Wallet Monitor has been renamed to Hyperliquid Wallet.

**Memory use**

Loading the full market catalogues for several venues now retains about 57 MiB of heap instead of about 125 MiB, and the load peak falls from about 143 MiB to about 90 MiB. Refreshing an already-loaded venue no longer keeps a full copy of the old catalogue, so repeated refreshes no longer accumulate memory.

## Fixed

**Candlestick history**

When a chart follows the live edge, widening the window or zooming out until more than one page of candles is visible no longer leaves a blank strip on the left that cannot be filled. History is now prefetched continuously as the viewport needs it, and prefetching no longer evicts the latest candle you are viewing.

**macOS title bar**

On macOS, the entire height of the custom title bar can now be used to drag the window; previously only the upper part worked. Double-clicking the title bar maximises or restores the window, and clicking tabs or buttons no longer starts an accidental drag.

**Singular forms**

In the English interface, count-dependent text now uses the singular form when the value is 1, so wording such as "1 wallets" or "1 indices" no longer appears.
