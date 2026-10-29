//! GPU chart rendering for Terminal.
//!
//! - [`viewport`]: bar-index/price <-> pixel math, pan and zoom.
//! - [`lod`]: level-of-detail pyramids that bound per-frame work to the plot width.
//! - [`gpu`]: wgpu pipelines (candles, OHLC bars, volume, lines, areas, histograms, dots)
//!   and the egui paint-callback adapter.
//! - [`scale`]: axis ticks and labels. [`drawings`]: user drawing model and hit testing.

pub mod drawings;
pub mod gpu;
pub mod headless;
pub mod lod;
pub mod scale;
pub mod scene;
pub mod viewport;

pub use egui_wgpu;
pub use egui_wgpu::wgpu;
