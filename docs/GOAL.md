# Goal: terminal-rs, a Bloomberg-style trading terminal with congressional trades

Build a fast, keyboard-driven market terminal: TradingView-level charting with a Bloomberg Terminal look (black background, amber/green monospace, dense tiled panels, a command line like `AAPL <GO>`).

## Research (REA)
Use REA on tradingview.com (website mode) to study chart UX: interactions, drawing tools, indicators, layouts, hotkeys, data streaming. Notes go in docs/analysis/. Study behaviour only; never copy proprietary code or assets.

## Stack
Pure Rust, fully native, no web frontend: wgpu for GPU chart rendering plus a fast immediate-mode UI (egui or custom), tokio for data ingest, rusqlite storage. It must be extremely snappy: sub-second cold start, 120 fps pan and zoom over 1M+ bars, and nothing that blocks the UI thread. Targets macOS first, then Linux.

## Features
1. Command-line navigation, tiling multi-panel layout, watchlists, hotkeys.
2. Candles, volume, line and area charts; 1m to 1M timeframes; SMA, EMA, VWAP, RSI, MACD, Bollinger; trendline, horizontal and fib drawings; symbol compare.
3. Market data behind a pluggable provider trait. Start with a free source (Alpaca free tier or Yahoo history), keys in env vars, and sample data when there is no key.
4. Congress trades (key feature): ingest STOCK Act disclosures directly from official sources: Senate eFD (efdsearch.senate.gov) and House Clerk PTR filings (disclosures-clerk.house.gov), including PDF/XML parsing. Normalise politician, party, chamber, ticker, buy/sell, amount range, transaction date and filed date into SQLite, refreshed on a schedule.
   - Panels: latest trades feed; politician profile (history, estimated performance); per-ticker list of who in Congress traded it.
   - Chart overlay: B/S markers at transaction dates coloured by party, with a hover card and the disclosure lag.
5. Alerts when a tracked politician files a new trade.

## Quality
Tests for the parsers and indicators, frame-time benchmarks, and a README noting it was built from reverse-engineering research. Make Conventional Commits as you go.

## Progress
- [x] Command line, docking workspace (splits, tabs, drag and drop, pop-outs), saved layouts, link groups, rebindable hotkeys.
- [x] wgpu charts: candles/bars/line/area, 1m–1M, SMA/EMA/VWAP/RSI/MACD/Bollinger, drawings, compare, log scale.
- [x] Pluggable data providers (Yahoo, Binance/Coinbase, sample mode); multi-asset (equities, FX, crypto, rates, commodities, indices).
- [x] Congress ingest from Senate eFD and House Clerk (PDF/XML), feed, politician profiles, per-ticker panel, chart markers with hover card, tracking alerts, freshness indicator.
- [x] Native macOS look (unified toolbar, vibrancy sidebar on either side, SF fonts), options chains (OMON) with greeks.
- [x] Theme system: built-in themes (Terminal Dark/Light, Bloomberg, High Contrast, Solarized, Nord, Monokai), live editor in Settings, custom themes as TOML files with TOML/JSON import/export, `THEME` command, ⌘⌥T cycling, optional theme per layout.
- [x] Brokerage accounts through SnapTrade (read-only): setup screen, Keychain credential storage, Connection Portal, `PORT`/`ACCT`/`POS`/`ORD`/`ACTV` panes with live-repriced P&L.
- [x] Tests for parsers, indicators, layouts, prefs, themes and the SnapTrade client; frame-time benchmarks; README.
- [x] Collapsible drawing toolbar with grouped tools and flyouts: rays, extended and cross lines, parallel channel, fib extension, rectangle, long/short positions, measure, zoom; keep drawing, lock, hide, remove all.
