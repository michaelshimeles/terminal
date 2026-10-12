# terminal-rs

A fast, keyboard-driven market terminal in pure Rust: TradingView-class GPU charting with a
Bloomberg Terminal look (dense tiled panes, monospace numbers, a command line like `AAPL <GO>`)
across **stocks, ETFs, indices, FX, crypto, US Treasuries, futures/commodities and options**,
plus **congressional trade tracking ingested directly from the official STOCK Act disclosure
systems** and **your own brokerage accounts through [SnapTrade](https://snaptrade.com)**
(read-only). Native compact UI, themes from Bloomberg amber to Solarized, and a fully dockable
workspace with saved layouts.

![terminal-rs: AAPL chart with congress trade markers and RSI, the Congress trades feed, world markets and a multi-asset watchlist](docs/img/overview.png)

> **Built from reverse-engineering research.** Chart UX was studied behaviourally on
> tradingview.com with [REA](https://github.com/morluto/rea) in website mode (passive captures of
> the public, logged-out chart page), then given a Bloomberg Terminal look. Findings, evidence IDs
> and their design implications are in [`docs/analysis/`](docs/analysis/README.md). Only behaviour
> was studied: no TradingView or Bloomberg code, scripts, icons, fonts or other assets are used or
> included here. The disclosure parsers were likewise built by examining the public Senate eFD and
> House Clerk sites and their filings.

## Screenshots

| | |
|---|---|
| ![Bloomberg theme: NVDA chart with congress markers and Nancy Pelosi's trade history with estimated returns](docs/img/bloomberg-politician.png) | ![Options chain (OMON) for AAPL with bid/ask, IV and delta per strike beside the chart](docs/img/options-omon.png) |
| **Bloomberg theme** — `NVDA`, `POL PELOSI`: party-coloured B/S markers and a politician profile with estimated return, hit rate and disclosure lag | **Options** — `AAPL OMON`: every expiry, calls and puts by strike, IV and greeks, spot line |
| ![Terminal Light theme with the latest congressional trades feed](docs/img/light-congress.png) | ![Nord theme, sidebar on the right, dragging a Watchlist tab onto the chart pane's dock compass](docs/img/docking-nord.png) |
| **Terminal Light** — the latest STOCK Act filings with party, chamber, owner and amount filters | **Docking** (Nord theme, sidebar on the right) — drag a tab onto the compass to tab or split |
| ![Live theme editor: Monokai applied to the chart while its colours are shown in Settings](docs/img/theme-editor.png) | ![Theme gallery in Settings › Appearance](docs/img/theme-gallery.png) ![Congress marker hover card with disclosure lag](docs/img/hover-card.png) |
| **Theme editor** — edit any colour, font or density live; save, duplicate, import/export TOML or JSON | **Theme gallery** and the **marker hover card** (politician, amount, traded/filed, disclosure lag) |
| ![Portfolio summary: total value, unrealised P&L, cash, buying power, allocation bar, top holdings and accounts beside an AAPL chart](docs/img/portfolio.png) | ![Positions table with quantity, live last price, average cost, value and unrealised P&L](docs/img/positions.png) |
| **Portfolio** — `PORT`: value, unrealised P&L, cash and buying power, allocation, top holdings, accounts (simulated sample shown) | **Positions** — `POS`: live terminal quotes (cyan) reprice holdings; click a row to chart it |

Screenshots are from the Linux build under Xvfb with live data (October 2026); on macOS the same UI
gets the native title bar with traffic lights, window vibrancy behind the sidebar and SF fonts.

## Features

- **Multi-asset with Bloomberg-style market keys** — `AAPL <EQUITY>`, `SPY <EQUITY>`,
  `SPX <INDEX>`, `BTCUSD <CRNCY>`, `EURUSD <CRNCY>`, `US10Y <GOVT>`, `CL1 <CMDTY>` (keys display
  as `AAPL US Equity`, `BTCUSD Curncy`, `US10Y Govt`, `CL1 Comdty`). The yellow key is optional for
  known tickers. A built-in catalog covers ~120 instruments (regional indices, equity index futures,
  energy/metals/grains, the full Treasury curve, majors and crosses, top crypto, popular ETFs and
  large caps); anything else resolves by convention.
- **Command line and search** — `<SECURITY> [<FUNCTION>] <GO>` (Enter is `<GO>`); typing anywhere
  starts a command, with a live interpretation underneath (`5` → "Interval 5 minutes") and symbol
  matches across all asset classes (catalog instantly, plus Yahoo search after a short pause);
  ↑/↓, Tab, Enter; history with ↑/↓. `SECF gold cmdty` opens a full search panel.
- **Charts** — GPU-rendered candles, OHLC bars, line and area; 1m, 5m, 15m, 30m, 1H, 4H, 1D, 1W,
  1M plus quick ranges (1D … ALL); volume; SMA, EMA, VWAP, Bollinger overlays; RSI and MACD
  sub-panes; `CMP <SYM>` compare; log scale; crosshair with OHLC legend; smooth pan and zoom over
  a million bars. Live crypto bars from the Binance websocket (Coinbase fallback).
- **Drawings** — a collapsible drawing toolbar on each chart's left edge (the tab on the plot's
  edge, `TOOLS` or Alt+D) with grouped tools and flyouts: trendline (Alt+T), ray, extended line,
  horizontal line (Alt+H), horizontal ray (Alt+J), vertical line (Alt+V), cross line (Alt+C),
  parallel channel; fib retracement (Alt+F) and trend-based fib extension; long and short positions
  (entry, target, stop, with % to each and the reward-to-risk ratio); rectangle; measure (price
  change, %, bars and time between two points) and zoom to an area. Drawings are stored in
  (time, price) so they survive zoom and timeframe changes; magnet (Alt+M) snaps to OHLC; keep
  drawing, lock and hide all drawings, remove all; click to select, Del to remove; ⌘Z / ⌘⇧Z (or
  `UNDO` / `REDO`) undo and redo drawing changes per chart. The toolbar state and toggles save with
  the layout.
- **Options** (`AAPL OMON`, ⌘⇧O) — full chains from Yahoo for US equities, ETFs and indices
  (SPX via ^SPX): every expiration, calls and puts by strike with bid/ask/last, change, volume,
  open interest and IV, plus Black-Scholes greeks (Δ, Γ, Θ/day, vega) from each contract's IV, the
  dividend yield and the 3-month T-bill rate; in-the-money shading, spot line, ±10/±20/all
  strikes; offline mode shows a flagged simulated chain.
- **Markets** — `WEI` world markets (Americas/EMEA/Asia-Pacific indices, futures, Treasuries, FX,
  crypto and commodities with sparklines), `GC` US Treasury yield curve (now vs 1M and 1Y ago,
  2s10s / 3m10y / 5s30s), **news** (`AAPL CN`; Yahoo headlines, CoinDesk for crypto) and
  **order book** depth (`BTCUSD CRNCY OB`; live Binance book, Coinbase fallback; markets without a
  free public book show a clearly marked simulation).
- **Market data behind a trait** (`tr_data::MarketData`), routed per asset class to free sources:

  | Asset class | History | Quotes | Live |
  |---|---|---|---|
  | Crypto | Binance public REST (`data-api.binance.vision`, then `api.binance.com`), Coinbase Exchange fallback | Binance 24h ticker | Binance kline websocket, Coinbase ticker fallback |
  | US Treasuries | Treasury.gov daily par yield curve CSV (FRED fallback); Yahoo `^IRX/^FVX/^TNX/^TYX` intraday | Treasury.gov | — |
  | FX, indices, futures | Yahoo Finance (`EURUSD=X`, `^GSPC`, `CL=F`) | Yahoo spark (batched) | — |
  | Stocks, ETFs | Alpaca when `APCA_API_KEY_ID`/`APCA_API_SECRET_KEY` are set, else Yahoo | Yahoo spark | — |

  No keys are required. `TR_PROVIDER=sample` runs fully offline on deterministic, class-aware sample
  data (BTCUSD near 65,000, EURUSD near 1.09, US10Y near 4.2%) **with simulated live ticks**, and
  the live router falls back to sample data when a source is unreachable. Bars are cached in SQLite
  so charts paint from disk on start. Older history pages in as you scroll left.
- **Congress trades** (key feature)
  - Ingests Periodic Transaction Reports from the **Senate eFD** (efdsearch.senate.gov: agreement
    + CSRF session, DataTables search API, HTML transaction tables) and the **House Clerk**
    (disclosures-clerk.house.gov: yearly `{year}FD.zip` XML index, PTR PDFs parsed with pure-Rust
    text extraction). Scanned paper filings are detected and recorded as such.
  - Normalises politician, party, chamber, state, ticker, buy/sell, amount range, transaction date
    and filed date into SQLite. Party comes from the official rosters (clerk.house.gov
    `MemberData.xml`, senate.gov `senators_cfm.xml`); politicians are keyed by bioguide ID, which
    also repairs typos in the source index.
  - Refreshed on a schedule (every 30 minutes by default, `TR_CONGRESS_REFRESH_MIN`, `0` disables;
    `REFRESH`/F6 forces one), incrementally — only unseen filings are fetched.
  - Panels: latest trades feed with party/chamber/side filters (`CONG`); per-ticker "who in
    Congress traded it" (`NVDA CONG`, or right-click any ticker); politician profile with trade
    history and **estimated performance** (amount-weighted directional return, hit rate, average
    disclosure lag) (`POL PELOSI`); directory (`POL`).
  - **Chart overlay**: B/S markers at transaction dates coloured by party (blue D, red R, purple
    I); hover for a card with politician, amount, owner, traded/filed dates and the disclosure lag.
  - **Alerts**: `TRACK <name>` (or ☆ in the directory/profile); when a tracked politician files a
    new PTR you get a toast, an `ALERTS n` badge and an entry in `ALERTS`.

- **Dockable workspace and saved layouts**
  - Any panel (chart, watchlist, news, order book, world markets, yield curve, options, Congress
    feed, politicians, alerts, search, settings, help) docks anywhere: drag a tab (or a pane's
    header) onto another pane and drop on the compass (centre adds a tab, arrows split), with a
    ghost preview and a labelled landing highlight, or drag to a window edge. Resize with
    dividers (double-click to equalise), tabs in every pane, grid presets (`LAYOUT 1|2|2H|3|4|6`
    or the toolbar), and pop any pane out into its own native window (close it to dock back).
    Add panels from each pane's **+** menu, `NEW <PANEL>` / `SPLIT [DOWN] <PANEL>` or ⌘1…⌘0.
  - **Link groups**: give panes a colour (`LINK BLUE`, or the tab menu) and one security change
    drives every linked chart, order book, news, options and per-ticker Congress pane; clicking a
    row in a linked watchlist or world-markets pane loads it into the group.
  - **Saved layouts**: save, name, rename, duplicate, delete and switch (`LAYOUT <name>`, the
    sidebar list, ⌥1…⌥9); the active layout autosaves and is restored on launch. Each layout is a
    JSON file in `~/Library/Application Support/terminal-rs/layouts` (`$XDG_DATA_HOME` on Linux)
    and can be imported or exported (`LAYOUT EXPORT`/`IMPORT`, or Settings › Layouts). Per-pane
    settings (chart type, indicators, drawings, filters, depth, link, tab title) and, optionally,
    the theme (`LAYOUT THEME ON`) save with the layout.
- **Themes** — built-in Terminal Dark, Terminal Light, Bloomberg (amber on black, monospace,
  compact), High Contrast, Solarized Dark/Light, Nord and Monokai. A theme sets every colour
  (backgrounds, panels, text, accent, up/down, candles, grid, series), the interface and number
  fonts and size, and density (compact, regular, comfortable). Pick one from the gallery in
  Settings › Appearance, the View menu, `THEME <name>` (prefix match), `THEME NEXT|PREV|LIST` or
  ⌘⌥T; `THEME SYSTEM` follows the OS with a chosen dark and light theme. Settings › Themes is a
  live editor: editing a built-in makes a copy (built-ins stay read-only); duplicate, rename,
  delete, import or export as TOML or JSON. Custom themes are TOML files in the `themes` folder
  next to `layouts`; a partial file inherits everything else from Terminal Dark or Light.
- **Native, compact UI** — a macOS unified compact toolbar with the traffic lights inset, a
  translucent source-list sidebar with window vibrancy (`NSVisualEffectView` via `objc2`, in
  `crates/macos`) that can sit on either side (`SIDEBAR LEFT|RIGHT`, ⌘⌥S; the toolbar toggle moves
  and mirrors with it, right-click it for the View menu), SF Pro for chrome and SF Mono for
  numbers, segmented controls, spring animations, hover states and context menus. The System
  theme follows the OS appearance and accent colour; charts render at native retina scale with
  4x MSAA.
- **Brokerage accounts (SnapTrade, read-only)** — connect your brokers once through SnapTrade's
  Connection Portal (opened in your browser) and the terminal shows real accounts and balances
  (`ACCT`), positions with unrealised P&L (`POS`), orders from the last 30 days (`ORD`), activity
  such as trades, dividends, interest and transfers (`ACTV`) and a portfolio summary with
  allocation (`PORT`). Holdings are repriced with the terminal's own live quotes; clicking a
  position, order or activity row charts it, and right-click offers the option chain, news,
  Congress trades and watchlist. `AAPL POS` / `AAPL ORD` filter to one security, the account picker
  to one account. Stocks, ETFs, funds, crypto and options (OCC symbols shown as `AAPL 350C`) are
  supported. Nothing can place, change or cancel orders, and the portal requests read-only
  access. Without credentials the panes show a setup screen; offline (`TR_PROVIDER=sample` or
  `TR_BROKER=sample`) they show a clearly labelled simulated portfolio. See
  [Brokerage setup](#brokerage-setup-snaptrade).
- **Hotkeys and preferences** — every hotkey is rebindable from a native shortcut table in
  Settings › Hotkeys and Help (click a binding, press the new keys); defaults for new charts and
  order books; everything persists in `prefs.json`.

## Running

```sh
cargo run --release                  # Yahoo data, congress ingest in the background
TR_PROVIDER=sample cargo run --release   # offline
APCA_API_KEY_ID=… APCA_API_SECRET_KEY=… cargo run --release   # Alpaca
```

Data lives in `~/Library/Application Support/terminal-rs` (macOS) or
`$XDG_DATA_HOME/terminal-rs` (Linux), or `TR_DATA_DIR`. The first congress ingest covers the
current and previous year (a few minutes; it runs in the background while you work).
`cargo run -p tr-congress --example ingest -- congress.sqlite3` runs one ingest from the CLI.

Appearance: `TR_THEME=light|dark|<theme name>` overrides the saved theme, `TR_MSAA=1|2|4|8` sets
chart anti-aliasing (default 4 on macOS), `TR_VIBRANCY=0` disables the translucent sidebar, and
⌃⌘S (Ctrl+Shift+S elsewhere) toggles the sidebar. `scripts/bundle-macos.sh` builds a retina-capable
`terminal-rs.app`; `scripts/check-macos-cross.sh` compiles and lints the whole workspace for
aarch64-apple-darwin from Linux (zig as the C cross-compiler; linking needs the macOS SDK).

## Brokerage setup (SnapTrade)

1. Create API keys (a client ID and consumer key) in the
   [SnapTrade dashboard](https://dashboard.snaptrade.com). Personal keys work too.
2. In the terminal, type `PORT SETUP` (or open Settings › Brokerage) and enter the keys.
3. Click **Register User**. This creates a SnapTrade user for this installation and saves its user
   secret, which SnapTrade only returns once. With personal keys, leave the user fields empty.
4. Click **Connect Brokerage…** (or type `PORT CONNECT`). SnapTrade's Connection Portal opens in
   your browser with read-only access; sign in to your broker there. New accounts appear within
   a minute, and **Test Connection** confirms what is linked.

**Where credentials live.** On macOS the client ID, consumer key, user ID and user secret are
stored together in the **Keychain** (generic password, service `terminal-rs`, account
`snaptrade`). Elsewhere they go in `<data dir>/secrets/snaptrade.json` with owner-only permissions
(0600 in a 0700 directory). They are never written to the repository, layouts, prefs or logs;
request errors are scrubbed of the URL (which carries the user secret), and the settings screen
masks secrets. **Forget Credentials** removes them. `SNAPTRADE_CLIENT_ID`,
`SNAPTRADE_CONSUMER_KEY`, `SNAPTRADE_USER_ID` and `SNAPTRADE_USER_SECRET` override the saved
values field by field (useful for CI or a shell profile), and `TR_SECRET_STORE=file` uses the
file store on macOS.

**How it talks to SnapTrade.** Requests go to `https://api.snaptrade.com` and are signed as the
SnapTrade docs specify: `clientId` and a Unix `timestamp` in the query, and a `Signature` header
holding a base64 HMAC-SHA256 (keyed with the consumer key) of the canonical JSON
`{"content","path","query"}`. Portfolios refresh every two minutes while a portfolio pane is
visible (every 20 s for five minutes after opening the portal); `PORT REFRESH` reloads on demand.
Brokerage data can be delayed or cached by SnapTrade depending on your plan, which is why holdings
are repriced with the terminal's live quotes where a market key exists.

## Keyboard shortcuts and commands

Press **F1** (or type `HELP`) for the full, searchable list; every hotkey can be rebound in
Settings › Hotkeys. On Linux and Windows ⌘ is Ctrl and ⌥ is Alt.

| Keys | Action |
|---|---|
| F1 · F2 · F3 · F4 · F5 · F6 · F12 | help · Congress feed · politicians · alerts · watchlist · refresh filings · frame timing |
| F7 · F8 · F9 | brokerage portfolio · positions · orders |
| Tab · ⇧Tab | next · previous pane |
| ⌘1 … ⌘0 · ⌘⇧O | new chart, watchlist, news, order book, world markets, yield curve, Congress, politicians, alerts, search pane · options chain |
| ⌘D · ⌘⇧D · ⌘T · ⌘W · ⌘⇧P | split right · split down · new tab · close pane · pop out |
| ⌘S · ⌥1 … ⌥9 | save layout · switch to saved layout 1–9 |
| ⌃⌘S (Ctrl+Shift+S) · ⌘⌥S | toggle sidebar · move sidebar to the other side |
| ⌘⌥T · ⌘, | cycle theme · settings |
| ⌥T · ⌥H · ⌥J · ⌥V · ⌥C · ⌥F · ⌥M | draw trendline · horizontal · horizontal ray · vertical · cross line · fib · magnet |
| ⌥D · ⌘Z · ⌘⇧Z | show or hide the drawing toolbar · undo · redo drawing changes |
| ⌥R · ⌥L · Esc · Del | reset chart view · log scale · cancel · delete selected drawing |
| wheel · drag · drag axes · double-click | zoom · pan · stretch · reset |

| Command | Action |
|---|---|
| `AAPL` · `AAPL US EQUITY <GO>` | load a security in the focused chart |
| `BTCUSD <CRNCY>` · `EURUSD <CRNCY>` · `US10Y <GOVT>` · `CL1 <CMDTY>` · `SPX <INDEX>` | other asset classes |
| `5` `1H` `D` `1W` `1M` · `YTD` `5Y` `ALL` | interval · quick range |
| `RSI` `MACD` `BB` `SMA 50` `EMA 20` `VWAP` · `CLEAR` | toggle indicators |
| `CMP MSFT` · `LOG` · `CANDLE` `BAR` `LINE` `AREA` | compare · log scale · chart type |
| `TOOLS` · `TL` `RAY` `EXTLINE` `HL` `HRAY` `VL` `CROSS` `CHANNEL` · `FIB` `FIBEXT` · `RECT` · `LONG` `SHORT` · `MEASURE` · `CLEAR DRAW` · `UNDO` `REDO` | drawing toolbar · drawing tools · remove all drawings · undo, redo |
| `AAPL OMON` · `AAPL CN` · `BTCUSD CRNCY OB` · `WEI` · `GC` · `SECF <query>` | options · news · order book · world markets · yield curve · search |
| `CONG` · `NVDA CONG` · `POL PELOSI` · `TRACK PELOSI` · `ALERTS` · `REFRESH` | Congress feed · who traded it · profile · track · alerts · refresh |
| `NEW BOOK` · `SPLIT DOWN NEWS` · `POP` · `LINK GREEN` · `TITLE Desk` · `CLOSE` | add, split, pop out, link, rename, close panes |
| `LAYOUT 4` · `LAYOUT SAVE Macro` · `LAYOUT Macro` · `LAYOUT RENAME Rates` · `LAYOUT DUP` · `LAYOUT DEL` · `LAYOUT EXPORT` | grid preset · saved layouts |
| `THEME BLOOMBERG` · `THEME NEXT` · `THEME SYSTEM` · `THEME EDIT` · `THEME COMPACT` · `LAYOUT THEME ON` | themes |
| `PORT` · `ACCT` · `POS` · `ORD` · `ACTV` · `AAPL POS` · `AAPL ORD` | brokerage portfolio · accounts and balances · positions · orders · activity · one security |
| `PORT REFRESH` · `PORT CONNECT` · `PORT SETUP` (`BROKER`) | reload accounts · connect a broker in the browser · SnapTrade keys |
| `SIDEBAR RIGHT` · `SIDEBAR` · `PREFS` · `HELP` | sidebar side or visibility · settings · help |
| right-click chart / watchlist / feed row / sidebar toggle | context menu (chart type, indicators, draw, add to watchlist, who traded it, View menu) |

## Architecture

```
app/            eframe shell: command line, docked panes, chart widget, panels
  dock.rs       docking tree (splits + tab groups): insert/move/remove, layout, resize, presets
  layouts.rs    saved layout documents (JSON) and the layouts folder store
  prefs.rs      appearance, keymap and panel defaults (prefs.json)
  themes.rs     theme model, built-in themes, TOML/JSON theme files and the themes folder
  app/          workspace (drag and drop, pop-out windows, link groups), panels, settings, chrome
  worker.rs     tokio runtime: all network, SQLite, pyramid and indicator work (UI never blocks)
crates/core     Bar/Series, timeframes, market keys + security catalog, indicators, sample data
crates/data     MarketData trait; per-class router (Binance/Coinbase, Treasury.gov/FRED, Yahoo,
                Alpaca), websocket live feeds, sample provider with simulated ticks; bar cache
crates/chart    wgpu renderer, LOD pyramids, viewport math, axes, drawings, egui paint callback
crates/congress Senate eFD + House Clerk parsers, rosters, SQLite store, refresh, alerts, performance
crates/broker   SnapTrade client (request signing, accounts, balances, positions, orders, activity),
                portfolio model and P&L, Keychain / owner-only credential store, simulated sample
docs/analysis   REA research notes on TradingView chart UX
```

**Rendering.** Bars are uploaded once per data version into a storage buffer as a level-of-detail
pyramid (level *k* merges 2^k bars), and geometry is generated in the vertex shader (one instance
per bar or line segment). Each frame picks the level that keeps instances at ≥ 2 px, so the work per
frame is bounded by the plot width, not the series length: a 1M-bar chart fully zoomed out draws
about 800 instances. Pan and zoom only rewrite a few hundred bytes of uniforms; bar indices are split
into integer and fraction for f32 precision at millions of bars. Axes, legend, crosshair, drawings and
markers are cheap egui shapes.

**Threading.** The UI thread sends requests to the worker and drains responses each frame; history
fetches, pyramid building (≈ 200 ms for 1M bars), indicators, SQLite queries and the congress ingest
all run on tokio tasks. The worker owns each chart's bar series, so initial history, paging and live
websocket ticks merge into one copy; live updates are coalesced to at most four GPU re-uploads a
second.

## Performance

Measured on Linux with Mesa **llvmpipe (a CPU software rasterizer)** — a real GPU is much faster:

- **Cold start**: window and GPU ready in **143 ms**; charts paint from the SQLite cache as soon as
  the worker answers.
- **1M-bar pan/zoom** (`cargo run --release -p tr-chart --example framebench`, 1600×900, candles +
  volume + 2 overlays, 300 frames per phase):

  | phase | CPU per frame p50 / p99 | until GPU done p50 / p99 |
  |---|---|---|
  | zoom out to all 1M bars | 0.07 / 0.20 ms | 3.09 / 5.27 ms |
  | pan across history | 0.06 / 0.18 ms | 1.40 / 3.93 ms |
  | zoom in to 8 px/bar | 0.05 / 0.19 ms | 1.34 / 2.63 ms |

  All within the 8.33 ms budget for 120 fps even on the software rasterizer. Criterion versions:
  `cargo bench -p tr-chart --bench frame`.
- **In the app** (F12 readout, interactive wheel-zoom + drag): UI CPU ≈ 0.2 ms per frame, 118 fps.
  To try a million bars interactively:
  `TR_PROVIDER=sample TR_SAMPLE_BARS=1000000 TR_HISTORY_LIMIT=1000000 TR_FPS=1 cargo run --release`, then `1 <GO>`.
  With all 1M one-minute bars visible on llvmpipe under Xvfb the app measured UI CPU 0.36 ms and
  76 fps for the whole window (software rasterization and presentation bound; the chart pass itself
  is the benchmark above).
- **Indicators over 1M bars** (`cargo bench -p tr-core`): SMA 1.8 ms, EMA 2.2 ms, VWAP 7.0 ms,
  Bollinger 29 ms; RSI and MACD are single-pass (≈ 12 ms and 26 ms including first-touch of their
  output memory).

## Tests

```sh
cargo test --workspace                       # unit + fixture tests (parsers, indicators, chart, app)
cargo test -p tr-data -- --ignored           # live tests: every asset class, quotes, curve, search, websocket
cargo bench -p tr-core && cargo bench -p tr-chart
```

- Parsers are tested against real filings captured from the official sources
  (`crates/congress/tests/fixtures`): multi-page House PDFs with partial sales and page-break rows,
  spouse/joint owners, wrapped and open-ended amounts, CUSIP vs ticker, exchanges, private holdings,
  a scanned paper filing, Senate search JSON (electronic, paper and amended reports) and PTR tables.
  An end-to-end refresh test covers discovery, roster joins, idempotency and alerts.
- Data-source parsers are tested against captured responses: Binance klines/24h ticker/websocket
  kline frames, Coinbase candles/stats/websocket ticker, Treasury.gov curve CSV, FRED CSV, Yahoo
  chart/spark/search. Market-key parsing, catalog search and symbol mapping per source are unit
  tested; opt-in live tests hit the real sources for every asset class.
- Indicators are checked against reference values (Wilder's RSI example, hand-computed EMA, MACD,
  population-SD Bollinger, session VWAP).
- Layouts: the dock tree (insert, split, tab, move, remove/normalise, resize clamping, presets,
  JSON round trip and repair of hand-edited files), layout documents (every panel kind and its
  settings, links, popped windows, older formats), the store (save, list, rename, duplicate,
  delete, import, export, safe file names), migration from pre-docking settings, prefs and
  keymap parsing/rebinding.
- Themes: every built-in round-trips through TOML and JSON, partial theme files inherit from the
  dark/light base, the themes folder store (save, rename, delete, name collisions), copy-on-write
  names and migration of older appearance preferences.
- Brokerage: request signatures match values computed independently with Python's `hmac` over the
  documented canonical JSON; an end-to-end test runs the client against a local mock SnapTrade
  server that verifies every request's signature, timestamp and auth parameters and serves
  fixtures shaped like the API's responses (stocks, ETFs, crypto, options, cash equivalents,
  string and numeric decimals, a failing account); P&L for longs, shorts and option multipliers;
  market-key mapping; OCC symbols; credential storage permissions and redaction; errors never
  echo the user secret; every portfolio view renders.
- The chart renders real frames offscreen on whatever adapter is present and checks pixels.

## Platform notes and limitations

- macOS is the primary target (wgpu Metal backend); Linux works with Vulkan or GL. This repository
  was developed and run on Linux (X11, software Vulkan). For macOS, the GPU and UI stack (`tr-core`,
  `tr-chart`, `tr-macos`, eframe/winit) is cross-checked for `aarch64-apple-darwin`, and CI
  (`.github/workflows/ci.yml`) builds, tests, lints and launches the app with screenshots on macOS
  arm64 and Intel runners.
- Scanned paper filings (some House PTRs and Senate `/paper/` reports) are recorded and alerted but
  not OCR'd, so their trades are not itemised.
- Estimated performance uses disclosed value bands (midpoints), so it is an estimate of trade-level
  skill, not a reconstruction of anyone's portfolio.
- Yahoo's chart endpoint is unofficial and rate-limited; use Alpaca for a supported free source.
  Senate eFD terms prohibit using the data for commercial purposes or credit decisions; the terminal
  accepts the eFD agreement on your behalf for personal research use.
