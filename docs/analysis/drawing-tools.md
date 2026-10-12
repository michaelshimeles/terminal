# Drawing tools

## Observed (`ev_16ea0e50`, `ev_5888cfc1` accessibility tree)

Left toolbar groups (accessible names): Cursors (pressed by default), Trend tools, Gann and Fibonacci
tools, Patterns, Forecasting and measurement tools, Geometric shapes / Brush, Annotation tools (Text),
Icons, Measure, Zoom in, Magnets, Keep drawing, Lock drawings and indicators, Hide all drawings,
Remove objects.

**Trend tools** flyout: Trendline `Alt+T`, Ray, Info line, Extended line, Trend angle,
Horizontal line `Alt+H`, Horizontal ray `Alt+J`, Vertical line `Alt+V`, Crossline `Alt+C`;
Channels: Parallel channel, Regression trend, Flat top/bottom, Disjoint channel; Pitchforks: Pitchfork,
Schiff, Modified Schiff, Inside pitchfork.

**Gann and Fibonacci** flyout: Fib retracement `Alt+F`, Trend-based fib extension, Fib channel,
Fib time zone, Fib speed resistance fan, Trend-based fib time, Fib circles, Fib spiral, Fib speed
resistance arcs, Fib wedge, Pitchfan; Gann box, Gann square fixed, Gann square, Gann fan.

Magnet tooltip text: "Magnet mode snaps drawings placed near price bars to the closest OHLC value".

## Documented

- Two-point tools (trendline, fib) are created by click–click or press–drag–release; a horizontal line is
  one click. Esc cancels an in-progress drawing. After placing, the tool reverts to the cursor unless
  "Keep drawing" is on.
- Selected drawings show endpoint handles that can be dragged; dragging the body moves the whole drawing;
  Delete/Backspace removes the selection. Shift constrains angles.
- Fib retracement default levels: 0, 0.236, 0.382, 0.5, 0.618, 0.786, 1 (plus extensions 1.618, 2.618… off
  or optional), each level labelled with its ratio and price.

## Unknown

- `Alt+H` without a pointer position placed nothing visible headless; whether it anchors to the last price
  or the cursor could not be confirmed (Documented: cursor price).

## terminal-rs implications

- MVP tool set: trendline (`Alt+T`), horizontal line (`Alt+H`), vertical line (`Alt+V`),
  fib retracement (`Alt+F`). Same hotkeys, click–click creation, Esc cancels, Delete removes the selected
  drawing, handles to edit endpoints.
- Drawings are stored in **data coordinates (time, price)**, never pixels, so they survive pan/zoom and
  timeframe changes; persisted per symbol in SQLite.
- Magnet toggle on `Alt+M` (bare letters are reserved for the command line)
  snaps to the nearest O/H/L/C of the bar under the cursor.
- Toolbar (implemented after the MVP): a collapsible strip on the chart's left edge mirroring the
  observed grouping — cursor; trend tools (trendline, ray, extended line, horizontal line and ray,
  vertical, cross line, parallel channel); Fibonacci (retracement, trend-based extension);
  projection (long/short position); shapes (rectangle); then measure and zoom; then magnet, keep
  drawing, lock, hide and remove all. A group button shows the tool last picked from its flyout
  (corner chevron or right-click), as observed. Closed, the toolbar is a slim tab on the plot edge.
  Glyphs are drawn in-app as simple line icons; no TradingView icons are used. Three-point tools
  (channel, extension, position) store the third anchor alongside the first two.
